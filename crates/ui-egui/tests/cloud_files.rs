//! Open and Save against cloud storage, driven through the real interface with
//! a service that lives inside the test.
//!
//! The service answers on the calling thread. A job posted during a frame is
//! therefore in the inbox before the next one. Everything else is the shipping
//! code: the Home page's Cloud section, the Save As dialog, the title bar, the
//! conflict modal and `Session`'s own incremental save.

use std::sync::{Arc, Mutex};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use pdfcraft_ui_egui::PdfCraftApp;
use pdfcraft_ui_egui::cloud::{self, Account, Capability, CloudApp, CloudBackend, Destination, Entry, EntryKind, Event, Fault, Job, Post};
use serde_json::{Value as Json, json};

type Shell = Harness<'static, PdfCraftApp>;

// ---------------------------------------------------------------- a document

/// A PDF of `n` pages, each with its number on it. The same generator the
/// control-channel tests use.
fn fixture(n: usize) -> Vec<u8> {
    let mut objs: Vec<String> = vec!["<< /Type /Catalog /Pages 2 0 R >>".into()];
    let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 4 + 2 * i)).collect();
    objs.push(format!("<< /Type /Pages /Kids [{}] /Count {n} /MediaBox [0 0 200 300] >>", kids.join(" ")));
    objs.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into());
    for i in 0..n {
        objs.push(format!("<< /Type /Page /Parent 2 0 R /Contents {} 0 R /Resources << /Font << /F1 3 0 R >> >> >>", 5 + 2 * i));
        let body = format!("BT /F1 24 Tf 20 150 Td (Page {}) Tj ET", i + 1);
        objs.push(format!("<< /Length {} >>\nstream\n{body}\nendstream", body.len()));
    }
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for o in offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).as_bytes());
    out
}

/// A one-pixel PNG, for the file a provider mislabels as a PDF.
const PNG: [u8; 69] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
    0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53, 0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x08, 0xd7, 0x63, 0xf8, 0xcf, 0xc0, 0x00,
    0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xdd, 0x8d, 0xb0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

/// How many pages the open document has, as the window sees it.
fn pages(h: &Shell) -> usize {
    h.state().active_ids().and_then(|(_, id)| h.state().session.get(id)).map_or(0, |doc| doc.info.pages.len())
}

fn dirty(h: &Shell) -> bool {
    h.state().active_ids().and_then(|(_, id)| h.state().session.get(id)).is_some_and(|doc| doc.dirty)
}

/// How many pages a stored copy has, read back through the engine.
fn stored_pages(bytes: &[u8]) -> usize {
    let mut session = pdfcraft_engine::Session::new();
    let prepared = session.prepare_open("stored.pdf", None, Arc::new(bytes.to_vec()), None).expect("the stored copy opens");
    let id = session.commit_open(prepared);
    session.get(id).map_or(0, |doc| doc.info.pages.len())
}

// ---------------------------------------------------------------- a service in memory

#[derive(Default)]
struct State {
    /// Every file and folder, by its own path. A folder is listed in its parent
    /// like any other entry.
    files: Vec<(Entry, Vec<u8>)>,
    /// Incoming shares. One has no folder of this user's to sit in.
    shared: Vec<(Entry, Vec<u8>)>,
    capabilities: Vec<Capability>,
    linked: bool,
    /// What the service was asked to do. A test can then prove a save went out
    /// once and was not retried.
    writes: Vec<(String, Option<String>, usize)>,
    creates: Vec<(String, String, usize)>,
    reads: Vec<String>,
    conversations: Vec<String>,
}

impl State {
    fn account(&self) -> Result<Account, Fault> {
        if !self.linked {
            return Err(Fault::NotLinked);
        }
        Ok(Account { display: "Ada Lovelace".to_owned(), capabilities: self.capabilities.clone(), provider_linked: true })
    }

    fn find(&self, id: &str) -> Option<&(Entry, Vec<u8>)> {
        self.files.iter().chain(self.shared.iter()).find(|(e, _)| e.id == id)
    }

    /// Somebody else saves over a file. That is what makes the next conditional
    /// write fail.
    fn overwrite(&mut self, id: &str, bytes: Vec<u8>) {
        if let Some(slot) = self.files.iter_mut().chain(self.shared.iter_mut()).find(|(e, _)| e.id == id) {
            slot.0.etag = Some(format!("{}-next", slot.0.etag.clone().unwrap_or_default()));
            slot.1 = bytes;
        }
    }

    fn answer(&mut self, job: Job) -> Vec<Event> {
        // Signing out needs no session. It answers nothing either. The window
        // clears its own state before the job is ever sent.
        if matches!(job, Job::SignOut) {
            self.linked = false;
            return Vec::new();
        }
        if !self.linked && !matches!(job, Job::LinkStart | Job::LinkPoll(_)) {
            return vec![match job {
                Job::Account => Event::Account(Err(Fault::NotLinked)),
                Job::List(path) => Event::Listed { path, entries: Err(Fault::NotLinked) },
                Job::SharedWithMe => Event::Shared(Err(Fault::NotLinked)),
                Job::Read { entry, into, .. } => Event::Read { entry, into, bytes: Err(Fault::NotLinked) },
                Job::Write { uid, .. } | Job::Create { uid, .. } => Event::Saved { uid, entry: Err(Fault::NotLinked) },
                Job::LinkStart | Job::LinkPoll(_) => Event::Linked(Ok(false)),
                Job::Conversation(_) => Event::Conversation(Err(Fault::NotLinked)),
                // Handled above, before this arm is reached.
                Job::SignOut => return Vec::new(),
            }];
        }
        match job {
            Job::SignOut => Vec::new(),
            Job::Conversation(id) => {
                self.conversations.push(id);
                if !self.capabilities.contains(&Capability::Conversation) {
                    let why = "this provider has nowhere to talk".to_owned();
                    return vec![Event::Conversation(Err(Fault::Refused(why)))];
                }
                let room = craft_cloud::Conversation { url: "https://cloud.example/call/room".to_owned(), name: "Talk".to_owned() };
                vec![Event::Conversation(Ok(room))]
            }
            Job::Account => vec![Event::Account(self.account())],
            Job::LinkStart => {
                vec![Event::Browser { url: "https://cloud.example/link/abc".to_owned(), handle: Some("abc".to_owned()) }]
            }
            Job::LinkPoll(_) => vec![Event::Linked(Ok(self.linked))],
            Job::List(path) => {
                let entries = self.files.iter().filter(|(e, _)| parent(&e.path) == path).map(|(e, _)| e.clone()).collect();
                vec![Event::Listed { path, entries: Ok(entries) }]
            }
            Job::SharedWithMe => {
                if !self.capabilities.contains(&Capability::SharedWithMe) {
                    return vec![Event::Shared(Err(Fault::Refused("this provider has no incoming shares".to_owned())))];
                }
                vec![Event::Shared(Ok(self.shared.iter().map(|(e, _)| e.clone()).collect()))]
            }
            Job::Read { entry, into } => {
                self.reads.push(entry.id.clone());
                let bytes = match self.find(&entry.id) {
                    Some((_, bytes)) => Ok(bytes.clone()),
                    None => Err(Fault::Refused("no such file".to_owned())),
                };
                vec![Event::Read { entry, into, bytes }]
            }
            Job::Write { uid, entry, bytes } => {
                self.writes.push((entry.id.clone(), entry.etag.clone(), bytes.len()));
                let stored = self.find(&entry.id).map(|(e, _)| e.etag.clone());
                let answer = match stored {
                    None => Err(Fault::Refused("no such file".to_owned())),
                    // The condition the save was made under. A stored copy that
                    // moved on refuses it. That is the whole point.
                    Some(stored) if stored != entry.etag => Err(Fault::Conflict),
                    Some(_) => {
                        let slot = self.files.iter_mut().chain(self.shared.iter_mut()).find(|(e, _)| e.id == entry.id);
                        match slot {
                            Some(slot) => {
                                slot.0.etag = Some(format!("{}+", slot.0.etag.clone().unwrap_or_default()));
                                slot.0.size = Some(bytes.len() as u64);
                                slot.1 = bytes;
                                Ok(slot.0.clone())
                            }
                            None => Err(Fault::Refused("no such file".to_owned())),
                        }
                    }
                };
                vec![Event::Saved { uid, entry: answer }]
            }
            Job::Create { uid, folder, name, bytes } => {
                self.creates.push((folder.clone(), name.clone(), bytes.len()));
                let path = format!("{}/{name}", folder.trim_end_matches('/'));
                let entry = file_entry(&path, None);
                self.files.push((entry.clone(), bytes));
                vec![Event::Saved { uid, entry: Ok(entry) }]
            }
        }
    }
}

fn parent(path: &str) -> String {
    match path.trim_end_matches('/').rfind('/') {
        Some(0) | None => String::new(),
        Some(cut) => path[..cut].to_owned(),
    }
}

fn file_entry(path: &str, mime: Option<&str>) -> Entry {
    Entry {
        id: path.to_owned(),
        room: format!("room:{path}"),
        name: path.rsplit('/').next().unwrap_or(path).to_owned(),
        path: path.to_owned(),
        kind: EntryKind::File,
        size: Some(1024),
        modified: Some("2026-10-01T09:00:00Z".to_owned()),
        mime: mime.map(str::to_owned),
        etag: Some("v1".to_owned()),
        writable: true,
        shared_by: None,
    }
}

fn folder_entry(path: &str) -> Entry {
    Entry { kind: EntryKind::Folder, size: None, etag: None, mime: None, ..file_entry(path, None) }
}

/// The worker half, answering on the calling thread. A job posted during a
/// frame is therefore answered before the next one.
struct Service(Arc<Mutex<State>>);

impl CloudBackend for Service {
    fn submit(&self, job: Job, post: Post) {
        let events = match self.0.lock() {
            Ok(mut state) => state.answer(job),
            Err(poisoned) => poisoned.into_inner().answer(job),
        };
        for event in events {
            post.send(event);
        }
    }
}

/// A sharing service that answers out of a fixed set of people.
///
/// The share dialog is a craft-ui control with its own tests. This exists so the
/// window has somewhere to share to. That is what decides whether the title bar
/// draws a split button at all.
struct Sharing;

impl craft_ui::share::ShareBackend for Sharing {
    fn submit(&self, request: craft_ui::share::Request, reply: craft_ui::share::Replier) {
        use craft_cloud::{Contact, Permission, Share, ShareTarget};
        use craft_ui::share::{Answer, Request};
        let person = |id: &str, name: &str, email: &str| Contact {
            id: id.to_owned(),
            target: ShareTarget::User(id.to_owned()),
            name: name.to_owned(),
            email: Some(email.to_owned()),
            avatar: None,
            is_group: false,
        };
        match request {
            Request::Capabilities => reply.answer(Answer::Capabilities {
                capabilities: vec![
                    Capability::Sharing,
                    Capability::Contacts,
                    Capability::LinkShares,
                    Capability::LinkPassword,
                    Capability::ShareNotify,
                ],
                linked: true,
            }),
            Request::Lookup { query, .. } => {
                let everybody = [
                    person("dana", "Dana Okafor", "dana@example.org"),
                    person("priya", "Priya Raman", "priya@example.org"),
                    person("tomas", "Tom\u{e1}s Alvarez", "tomas@example.org"),
                ];
                let want = query.to_lowercase();
                reply.answer(Answer::Lookup(everybody.into_iter().filter(|c| c.name.to_lowercase().contains(&want)).collect()));
            }
            Request::Access { .. } => reply.answer(Answer::Access(vec![
                Share {
                    id: "s1".to_owned(),
                    target: ShareTarget::User("dana".to_owned()),
                    display: Some("Dana Okafor".to_owned()),
                    permission: Permission::Edit,
                    url: None,
                    note: None,
                    created: None,
                },
                Share {
                    id: "s2".to_owned(),
                    target: ShareTarget::Link,
                    display: None,
                    permission: Permission::View,
                    url: Some("https://cloud.example/s/abc123".to_owned()),
                    note: None,
                    created: None,
                },
            ])),
            other => reply.refuse(format!("{other:?} is not offered here")),
        }
    }
}

// ---------------------------------------------------------------- the window

fn harness(state: State) -> (Shell, Arc<Mutex<State>>) {
    build(state, false)
}

/// The same window with a sharing service attached. That is what makes the
/// title bar draw a Share split button.
fn sharing(state: State) -> (Shell, Arc<Mutex<State>>) {
    build(state, true)
}

fn build(state: State, shares: bool) -> (Shell, Arc<Mutex<State>>) {
    let store = Arc::new(Mutex::new(state));
    let service = store.clone();
    let mut h = Harness::builder().with_size(egui::vec2(1500.0, 1200.0)).build_eframe(move |_cc| {
        let mut app = PdfCraftApp::new();
        let _ = app.set_option("language", "en");
        // Background work runs on the frame that asks for it.
        app.run_inline = true;
        app.cloud.connect(Service(service));
        if shares {
            app.share.connect(Sharing);
        }
        app
    });
    // Fonts install on frame 1 and apply on frame 2.
    h.run_steps(6);
    (h, store)
}

/// A linked account with two folders of files and one incoming share.
fn furnished() -> State {
    let mut state = State {
        linked: true,
        capabilities: vec![Capability::Files, Capability::SharedWithMe, Capability::Sharing, Capability::Etags],
        ..State::default()
    };
    state.files.push((folder_entry("/Reports"), Vec::new()));
    state.files.push((file_entry("/Budget.pdf", Some("application/pdf")), fixture(3)));
    state.files.push((file_entry("/notes.xlsx", Some("application/vnd.ms-excel")), b"PK\x03\x04".to_vec()));
    // No extension at all. The content type says what it is.
    state.files.push((file_entry("/exported", Some("application/pdf")), fixture(1)));
    state.files.push((file_entry("/Reports/Q3.pdf", None), fixture(2)));
    let mut team = file_entry("/shared/Team.pdf", None);
    team.shared_by = Some("Dana".to_owned());
    state.shared.push((team, fixture(4)));
    state
}

/// The Home tab. That is where the Cloud section lives.
fn open_page(h: &mut Shell) {
    h.state_mut().active = None;
    h.run_steps(4);
}

/// Steps the window, collecting every address it asked a browser to open.
fn steps(h: &mut Shell, count: usize) -> Vec<String> {
    let mut urls = Vec::new();
    for _ in 0..count {
        h.step();
        for command in &h.output().platform_output.commands {
            if let egui::OutputCommand::OpenUrl(open) = command {
                urls.push(open.url.clone());
            }
        }
    }
    urls
}

fn click(h: &mut Shell, label: &str) -> Vec<String> {
    h.get_by_role_and_label(egui::accesskit::Role::Button, label).click();
    steps(h, 4)
}

fn status(h: &Shell) -> Json {
    cloud::status(h.state())
}

// ---------------------------------------------------------------- the tests

#[test]
fn the_open_page_lists_my_files_and_what_was_shared_with_me() {
    let (mut h, _store) = harness(furnished());
    open_page(&mut h);
    assert!(h.query_by_label("My files").is_some(), "the Open page browses your own cloud files");
    assert!(h.query_by_label("Shared with me").is_some(), "and what other people shared with you, beside it");
    assert!(h.query_by_label("Budget.pdf").is_some());
    assert!(h.query_by_label("Reports").is_some(), "a folder is offered so the user can walk into it");
    assert!(h.query_by_label("exported").is_some(), "no extension, and a content type that says PDF");
    assert!(h.query_by_label("notes.xlsx").is_none(), "PdfCraft cannot open a spreadsheet, so it is not offered");
    assert!(h.query_by_label("Team.pdf").is_some());
    let shared = status(&h)["shared"].clone();
    assert_eq!(shared[0]["name"], "Team.pdf");
    assert_eq!(shared[0]["sharedBy"], "Dana", "each incoming share says who shared it");
    // Walking into a folder lists it.
    click(&mut h, "Reports");
    assert_eq!(status(&h)["folder"], "/Reports");
    assert!(h.query_by_label("Q3.pdf").is_some());
    click(&mut h, "Up");
    assert_eq!(status(&h)["folder"], "");
}

#[test]
fn shared_with_me_is_hidden_when_the_provider_cannot_do_it() {
    let mut state = furnished();
    state.capabilities = vec![Capability::Files];
    let (mut h, _store) = harness(state);
    open_page(&mut h);
    assert!(h.query_by_label("My files").is_some());
    assert!(h.query_by_label("Shared with me").is_none(), "a feature the provider does not have is hidden, not shown empty");
    assert!(h.query_by_label("Team.pdf").is_none());
}

#[test]
fn opening_a_cloud_document_loads_its_pages_and_remembers_where_it_came_from() {
    let (mut h, store) = harness(furnished());
    open_page(&mut h);
    click(&mut h, "Budget.pdf");
    assert_eq!(pages(&h), 3, "the bytes went through the ordinary open path");
    assert!(h.state().active.is_some(), "a document that opened shows its pages, not the Home tab");
    let origin = status(&h)["origin"].clone();
    assert_eq!(origin["id"], "/Budget.pdf", "the opaque handle is kept for the save");
    assert_eq!(origin["etag"], "v1", "so is the version it was read at");
    assert_eq!(origin["room"], "room:/Budget.pdf", "and the collaboration identity, which is not the handle");
    assert_eq!(store.lock().unwrap().reads, vec!["/Budget.pdf".to_owned()]);
    let path = h.state().active_ids().and_then(|(_, id)| h.state().session.get(id)).and_then(|doc| doc.path.clone());
    assert_eq!(path, None, "a cloud document has no file on this computer");
}

#[test]
fn save_writes_back_under_the_version_it_was_opened_at() {
    let (mut h, store) = harness(furnished());
    open_page(&mut h);
    click(&mut h, "Budget.pdf");
    let id = h.state().active_ids().expect("a document is open").1;
    h.state_mut().session.apply(id, pdfcraft_engine::Edit::DeletePages { pages: vec![2] }).unwrap();
    h.run_steps(2);
    assert!(dirty(&h));
    h.state_mut().execute("file.save");
    h.run_steps(4);
    let state = store.lock().unwrap();
    assert_eq!(state.writes.len(), 1, "one save, one write");
    assert_eq!(state.writes[0].0, "/Budget.pdf");
    assert_eq!(state.writes[0].1, Some("v1".to_owned()), "the etag it was read at is the condition");
    assert!(state.writes[0].2 > 0);
    drop(state);
    assert!(!dirty(&h), "the version that reached the service is the saved one");
    assert_eq!(status(&h)["origin"]["etag"], "v1+", "and the document moves on to the version the service returned");
    // The stored copy really changed.
    let stored = store.lock().unwrap().find("/Budget.pdf").unwrap().1.clone();
    assert_eq!(stored_pages(&stored), 2);
}

#[test]
fn a_save_that_would_overwrite_somebody_else_is_refused_and_offers_a_reload() {
    let (mut h, store) = harness(furnished());
    open_page(&mut h);
    click(&mut h, "Budget.pdf");
    let id = h.state().active_ids().expect("a document is open").1;
    h.state_mut().session.apply(id, pdfcraft_engine::Edit::DeletePages { pages: vec![2] }).unwrap();
    h.run_steps(2);
    // Somebody else saves first.
    store.lock().unwrap().overwrite("/Budget.pdf", fixture(7));
    h.state_mut().execute("file.save");
    h.run_steps(4);
    assert_eq!(store.lock().unwrap().writes.len(), 1, "a refused save is never retried");
    let stored = store.lock().unwrap().find("/Budget.pdf").unwrap().1.clone();
    assert_eq!(stored_pages(&stored), 7, "their work is still there");
    assert_eq!(status(&h)["conflict"]["name"], "Budget.pdf");
    assert!(h.query_by_label("This document was not saved").is_some(), "the user is told, and asked what to do");
    assert_eq!(pages(&h), 2, "their own edit is still open");
    assert!(dirty(&h), "and still unsaved");
    // Reloading is explicit. Only then are the local edits replaced.
    click(&mut h, "Reload from the cloud");
    assert_eq!(pages(&h), 7);
    assert!(!dirty(&h));
    assert_eq!(h.state().views.len(), 1, "the reload replaced the document, it did not add one");
    assert_eq!(status(&h)["conflict"], Json::Null);
    assert_eq!(store.lock().unwrap().writes.len(), 1, "and still nothing was written");
}

#[test]
fn save_as_offers_the_cloud_account_beside_this_computer() {
    let (mut h, store) = harness(furnished());
    h.state_mut().open_bytes("Local.pdf", None, fixture(2)).unwrap();
    h.run_steps(2);
    h.state_mut().execute("file.save_as");
    h.run_steps(6);
    assert!(h.query_by_label("This computer").is_some(), "the local destination is still there");
    assert!(h.query_by_label("Ada Lovelace").is_some(), "and the cloud account beside it");
    click(&mut h, "Ada Lovelace");
    // Choosing the cloud grows the modal. A centred modal takes a few more
    // frames to settle at its new place. A click aimed before then lands where
    // the row used to be.
    h.run_steps(10);
    click(&mut h, "Reports");
    assert_eq!(status(&h)["folder"], "/Reports", "the folder that was walked into");
    h.state_mut().cloud.save_name = "Plan".to_owned();
    h.run_steps(2);
    click(&mut h, "Save to the cloud");
    let state = store.lock().unwrap();
    assert_eq!(state.creates.len(), 1);
    assert_eq!(state.creates[0].0, "/Reports", "into the folder that was picked");
    assert_eq!(state.creates[0].1, "Plan.pdf", "a name typed without an extension is still a PDF");
    drop(state);
    assert!(!dirty(&h), "the new document belongs to the cloud copy now");
    assert_eq!(status(&h)["origin"]["id"], "/Reports/Plan.pdf");
    assert_eq!(h.state().dialog, None, "a save that started takes the dialog away");
    // Save now goes back to that entry, with no further questions.
    let id = h.state().active_ids().expect("a document is open").1;
    h.state_mut().session.apply(id, pdfcraft_engine::Edit::DeletePages { pages: vec![1] }).unwrap();
    h.state_mut().execute("file.save");
    h.run_steps(4);
    assert_eq!(store.lock().unwrap().writes[0].0, "/Reports/Plan.pdf");
}

#[test]
fn no_account_yet_is_an_offer_to_link_and_never_an_error() {
    let mut state = furnished();
    state.linked = false;
    let (mut h, store) = harness(state);
    open_page(&mut h);
    assert!(h.state().toast.is_none(), "not having linked an account yet is not a failure");
    assert_eq!(status(&h)["link"], "offered");
    assert_eq!(status(&h)["accountError"], Json::Null);
    let opened = click(&mut h, "Link a cloud account");
    assert_eq!(opened, vec!["https://cloud.example/link/abc".to_owned()], "the browser is opened for them");
    assert_eq!(status(&h)["link"]["waiting"], "https://cloud.example/link/abc");
    // They finish in the browser. The next poll finds the account there.
    store.lock().unwrap().linked = true;
    h.state_mut().cloud.link =
        Some(cloud::Link::Waiting { url: "https://cloud.example/link/abc".to_owned(), handle: Some("abc".to_owned()), next_poll: 0.0 });
    h.run_steps(8);
    assert_eq!(status(&h)["link"], Json::Null);
    assert_eq!(status(&h)["account"]["display"], "Ada Lovelace");
    assert!(h.query_by_label("Budget.pdf").is_some(), "and the files are listed without being asked again");
}

#[test]
fn a_read_only_share_is_never_saved_over() {
    let mut state = furnished();
    if let Some(slot) = state.shared.first_mut() {
        slot.0.writable = false;
    }
    let (mut h, store) = harness(state);
    open_page(&mut h);
    click(&mut h, "Team.pdf");
    assert_eq!(pages(&h), 4);
    // Save has nowhere of its own to go. It asks, exactly as it does for a
    // document that has never been saved.
    assert!(!cloud::has_target(h.state()), "a share you may only read is not a save target");
    let refused = cloud::save(h.state_mut()).unwrap_err();
    assert!(refused.contains("reading only"), "and saying why names the fix: {refused}");
    assert!(store.lock().unwrap().writes.is_empty(), "nothing was written back over somebody else's file");
}

#[test]
fn a_local_document_still_saves_to_disk_with_a_cloud_service_wired() {
    let dir = std::env::temp_dir().join(format!("pdfcraft-cloud-local-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("Local.pdf");
    let (mut h, store) = harness(furnished());
    h.state_mut().open_bytes("Local.pdf", None, fixture(3)).unwrap();
    h.state_mut().save_override = Some(path.to_string_lossy().into_owned());
    h.run_steps(2);
    h.state_mut().execute("file.save");
    h.run_steps(4);
    assert!(path.exists(), "the local Save path is untouched");
    assert!(store.lock().unwrap().writes.is_empty());
    assert_eq!(stored_pages(&std::fs::read(&path).unwrap()), 3);
    // And Save keeps going to that file afterwards.
    let id = h.state().active_ids().expect("a document is open").1;
    h.state_mut().session.apply(id, pdfcraft_engine::Edit::DeletePages { pages: vec![2] }).unwrap();
    h.state_mut().execute("file.save");
    h.run_steps(4);
    assert!(store.lock().unwrap().writes.is_empty());
    assert_eq!(stored_pages(&std::fs::read(&path).unwrap()), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn names_and_paths_are_handled_as_a_user_types_them() {
    assert_eq!(cloud::document_name(" Plan "), "Plan.pdf");
    assert_eq!(cloud::document_name("Plan.pdf"), "Plan.pdf");
    assert_eq!(cloud::document_name("Plan.PDF"), "Plan.PDF", "an extension already known is left as typed");
    assert_eq!(cloud::document_name("Plan.v2"), "Plan.v2.pdf", "an unknown suffix is part of the name");
    assert_eq!(cloud::document_name("/a/b/Plan"), "Plan.pdf", "a pasted path cannot become a folder");
    assert_eq!(cloud::document_name("   "), "");
    assert_eq!(cloud::parent_of(""), None);
    assert_eq!(cloud::parent_of("/Reports"), Some(String::new()));
    assert_eq!(cloud::parent_of("/Reports/2026"), Some("/Reports".to_owned()));
}

#[test]
fn working_together_opens_where_the_provider_says_to_go() {
    let mut state = furnished();
    state.capabilities.push(Capability::Conversation);
    let (mut h, store) = harness(state);
    open_page(&mut h);
    click(&mut h, "Budget.pdf");

    assert!(cloud::can_converse(h.state()), "the capability was reported");
    cloud::work_together(h.state_mut());
    let opened = steps(&mut h, 4);

    assert_eq!(opened, vec!["https://cloud.example/call/room".to_owned()]);
    assert_eq!(store.lock().unwrap().conversations, vec!["/Budget.pdf".to_owned()], "about the document that is open");
    assert_eq!(status(&h)["status"], json!("Opened in Talk"));
}

#[test]
fn a_document_that_was_never_in_the_cloud_is_asked_to_be_saved_there_first() {
    let mut state = furnished();
    state.capabilities.push(Capability::Conversation);
    let (mut h, store) = harness(state);
    h.state_mut().open_bytes("Local.pdf", None, fixture(1)).unwrap();
    h.run_steps(2);

    cloud::work_together(h.state_mut());
    let opened = steps(&mut h, 4);

    assert!(opened.is_empty(), "nowhere to go, so nothing opened");
    assert!(store.lock().unwrap().conversations.is_empty(), "and the provider was never asked");
    assert_eq!(status(&h)["status"], json!("Save this document to the cloud first"));
}

#[test]
fn a_provider_with_nowhere_to_talk_never_offers_it() {
    // furnished() reports no conversation capability.
    let (h, _store) = harness(furnished());
    assert!(!cloud::can_converse(h.state()), "the control has nothing to draw");
}

/// Bytes that are not a PDF become a new, unsaved document (Create a PDF). That
/// document belongs to nothing in the cloud. A save can never land on top of
/// the file that was read.
#[test]
fn a_file_that_is_not_a_pdf_is_never_given_an_origin_to_save_back_over() {
    let mut state = furnished();
    state.files.push((file_entry("/Scan.pdf", Some("application/pdf")), PNG.to_vec()));
    let (mut h, store) = harness(state);
    open_page(&mut h);
    click(&mut h, "Scan.pdf");
    assert_eq!(pages(&h), 1, "the image became a one-page document");
    assert_eq!(status(&h)["origin"], Json::Null, "which is not the file that was read");
    assert!(!cloud::has_target(h.state()));
    assert!(cloud::save(h.state_mut()).is_err(), "Save has to ask where, as it does for any new document");
    assert!(store.lock().unwrap().writes.is_empty());
}

#[test]
fn the_title_bar_shows_who_is_signed_in_and_offers_share() {
    let mut state = furnished();
    state.capabilities.push(Capability::Conversation);
    let (mut h, _store) = sharing(state);
    open_page(&mut h);
    click(&mut h, "Budget.pdf");
    assert!(h.query_by_role_and_label(egui::accesskit::Role::Button, "Share").is_some(), "the primary half shares");
    h.get_by_role_and_label(egui::accesskit::Role::Button, "More share options").click();
    h.run_steps(4);
    assert!(h.query_by_label("Work together").is_some(), "the chevron carries the variations");
    assert!(h.query_by_label("Microsoft Word (.docx)").is_some(), "and PdfCraft's own Save a Copy formats");
}

/// PdfCraft writes a PDF. A destination that says it is something else is
/// refused. A save can therefore never put PDF bytes under a name that
/// promises another format.
#[test]
fn bytes_are_refused_for_a_destination_that_is_not_a_pdf() {
    let (mut h, _store) = harness(furnished());
    open_page(&mut h);
    click(&mut h, "Budget.pdf");
    let uid = h.state().active_ids().expect("a document is open").1.0;
    let app = h.state_mut();

    assert!(app.encode(uid, Destination::new("Budget.pdf")).is_ok());
    assert!(app.encode(uid, Destination::new("BUDGET.PDF")).is_ok(), "a shouted name is the same name");
    assert!(app.encode(uid, Destination { name: "exported", mime: Some("application/pdf") }).is_ok(), "a bare name whose content type says PDF");
    assert!(app.encode(uid, Destination::new("exported")).is_ok(), "a bare name with nothing said against it");

    let refused = app.encode(uid, Destination::new("figures.csv")).unwrap_err();
    assert!(refused.contains(".csv"), "the refusal names the format asked for: {refused}");
    assert!(refused.contains("Export a PDF"), "and names the fix: {refused}");
    let refused = app.encode(uid, Destination { name: "figures", mime: Some("text/csv") }).unwrap_err();
    assert!(refused.contains("Export a PDF"), "a bare name whose content type says otherwise: {refused}");
}

/// A build with no service configured. That is what the other test files get.
#[test]
fn without_a_service_nothing_cloud_shaped_is_offered() {
    let dir = std::env::temp_dir().join(format!("pdfcraft-cloud-none-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("Local.pdf");
    // No backend is connected. `CloudFiles` has nothing to reach.
    let mut h = Harness::builder().with_size(egui::vec2(1500.0, 1200.0)).build_eframe(move |_cc| {
        let mut app = PdfCraftApp::new();
        let _ = app.set_option("language", "en");
        app.run_inline = true;
        app
    });
    h.run_steps(6);
    assert!(!cloud::available(h.state()));
    assert_eq!(status(&h)["available"], false);
    assert!(h.query_by_label("Cloud").is_none(), "Home offers no Cloud section");
    assert!(h.query_by_label("My files").is_none());
    assert!(h.query_by_label("Shared with me").is_none());
    assert!(!cloud::shares(h.state_mut()), "and the title bar draws no Share split button");
    // Save As writes the file, as it always has. It puts up no dialog of ours.
    h.state_mut().open_bytes("Local.pdf", None, fixture(1)).unwrap();
    h.state_mut().save_override = Some(path.to_string_lossy().into_owned());
    h.run_steps(2);
    h.state_mut().execute("file.save_as");
    h.run_steps(4);
    assert_eq!(h.state().dialog, None);
    assert_eq!(stored_pages(&std::fs::read(&path).unwrap()), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Writes one PNG per state to the directory in `PDFCRAFT_CLOUD_RENDERS`.
///
/// Skipped without it. The renders are review material for a human being. The
/// behaviour they illustrate is asserted by the tests above.
#[test]
fn renders_for_review() {
    let Ok(into) = std::env::var("PDFCRAFT_CLOUD_RENDERS") else {
        eprintln!("set PDFCRAFT_CLOUD_RENDERS to a directory to write renders");
        return;
    };
    let into = std::path::PathBuf::from(into);
    std::fs::create_dir_all(&into).expect("the render directory must be writable");

    let save = |h: &mut Shell, name: &str| {
        h.run_steps(8);
        let image = h.render().expect("an offscreen renderer is required to write renders");
        let path = into.join(format!("pdfcraft-{name}.png"));
        image.save(&path).expect("the render directory must be writable");
        eprintln!("wrote {}", path.display());
    };

    // One provider that does everything, built twice. One window has a document
    // open. Another is browsing. They cannot be the same window.
    let talkative = || {
        let mut state = furnished();
        state.capabilities.push(Capability::Conversation);
        state
    };

    // The file list, where every document carries an overflow menu.
    let (mut h, _store) = sharing(talkative());
    open_page(&mut h);
    save(&mut h, "open-page");

    // The same page with one of those menus open.
    h.get_all_by_role_and_label(egui::accesskit::Role::Button, "More options").next().expect("a row offers its menu").click();
    save(&mut h, "open-page-overflow");

    // The title bar. It carries the account, who else is here and Share as a
    // split button, with the chevron pressed.
    let (mut h, _store) = sharing(talkative());
    open_page(&mut h);
    click(&mut h, "Budget.pdf");
    let peers = ["Dana Okafor", "Priya Raman", "Tom\u{e1}s Alvarez", "Wen Li", "Sam Iredale", "Rosa Beltr\u{e1}n"];
    h.state_mut().presence.set(peers.iter().enumerate().map(|(n, who)| craft_ui::presence::Peer::new(format!("peer-{n}"), *who)).collect());
    save(&mut h, "title-bar-presence");
    h.get_by_role_and_label(egui::accesskit::Role::Button, "More share options").click();
    save(&mut h, "share-split-menu");

    // And the dialog the primary half opens, with real access on it.
    h.get_by_role_and_label(egui::accesskit::Role::Button, "Share").click();
    save(&mut h, "share-dialog");

    // Save As, where the cloud account sits beside this computer.
    let (mut h, _store) = sharing(talkative());
    h.state_mut().open_bytes("Local.pdf", None, fixture(2)).expect("the fixture opens");
    h.run_steps(2);
    h.state_mut().execute("file.save_as");
    h.run_steps(6);
    click(&mut h, "Ada Lovelace");
    save(&mut h, "save-as-page");

    // The modal a refused save puts up.
    let (mut h, store) = sharing(talkative());
    open_page(&mut h);
    click(&mut h, "Budget.pdf");
    let id = h.state().active_ids().expect("a document is open").1;
    h.state_mut().session.apply(id, pdfcraft_engine::Edit::DeletePages { pages: vec![2] }).expect("the page is deleted");
    store.lock().unwrap().overwrite("/Budget.pdf", fixture(7));
    h.state_mut().execute("file.save");
    save(&mut h, "conflict-modal");
}
