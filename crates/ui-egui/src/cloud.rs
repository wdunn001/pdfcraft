//! PdfCraft's half of Open and Save against cloud storage.
//!
//! The pages themselves are `craft_ui::cloud`. Every Craft app offers the same
//! ones. What is here is what only PdfCraft can answer. A document is a PDF.
//! `.pdf` and `application/pdf` are what that means. The Linux desktop entry,
//! the macOS `CFBundleDocumentTypes` and the Windows file associations say so
//! already. Bytes become a document through [`PdfCraftApp::open_bytes`] and go
//! back through `Session::save_bytes`. The words are the ones
//! [`crate::i18n`] has.
//!
//! PdfCraft holds no provider credential and speaks no provider protocol. Every
//! call goes to the CollabCraft service. That service is the single place a
//! provider is configured and permissions are decided. Replacing one provider
//! with another changes nothing here.
//!
//! Nothing in this file opens a socket either. The transport half is a
//! [`CloudBackend`] the host application supplies (see `apps/pdfcraft/cloud`).
//! Every CollabCraft call blocks. A blocking read of a hundred megabyte scan
//! inside a frame would freeze the window for seconds. So the pages state a
//! [`Job`]. The host runs it on a worker thread. The answer arrives on a
//! channel that [`pump`] drains on a later frame.

use std::collections::HashMap;
use std::sync::Arc;

// Re-exported so a host application can name these types without depending on
// the crates they come from. A test wants the same.
pub use craft_cloud::{Capability, Entry, EntryKind};
pub use craft_ui::cloud::{
    Account, Busy, CloudApp, CloudBackend, CloudFiles, Conflict, CopyFormat, Destination, DocumentKind, Event, Fault, Folder, Job, Link, Origin,
    Post, Source, UNAVAILABLE, available, browse, can_converse, conflict_modal, destination_name, has_target, link, load_shared, open_entry,
    open_page, origin, parent_of, prefers_cloud, progress, pump, reload_active, save, save_as, save_page, share_button, shares, sign_in, sign_out,
    start, who, work_together,
};
use egui::{Color32, Painter, Rect, Ui, Vec2};
use pdfcraft_engine::DocId;
use serde_json::Value as Json;

use crate::theme::{self, Tokens};
use crate::{PdfCraftApp, icons};

/// Extensions PdfCraft opens as documents. Its desktop file picker offers the
/// same single one for an open (`files::files_picker`). The packaged
/// application registers itself for that one alone.
///
/// Images and text files are deliberately absent. PdfCraft converts those into
/// a new, unsaved PDF (Create a PDF). What ends up open is then a document of
/// its own, with nothing to save back over.
const OPENS: [&str; 1] = ["pdf"];

/// Content types PdfCraft opens, matched against the start of what the provider
/// said. Read beside the extension because neither is reliable alone. A WebDAV
/// server hands back `application/octet-stream` for a perfectly good PDF. A
/// file exported by a web application often has a correct content type and no
/// extension at all.
const MIMES: [&str; 1] = ["application/pdf"];

/// What a PdfCraft document is, as a cloud file list needs to know.
pub const DOCUMENT: DocumentKind = DocumentKind { extensions: &OPENS, mimes: &MIMES, noun: "document", default_extension: "pdf" };

/// The Save a Copy formats the Share menu offers under the share itself.
///
/// PdfCraft's own. The identifier of each is the command that carries it out.
/// The labels are the ones the Export a PDF tool group already uses. The same
/// format is therefore called the same thing in both places.
const COPIES: [(&str, &str); 4] = [
    ("export.docx", "Microsoft Word (.docx)"),
    ("export.pptx", "Presentation (.pptx)"),
    ("export.html", "HTML web page"),
    ("export.text", "Text (plain)"),
];

/// The bytes each save in flight encoded.
///
/// Only PdfCraft knows what a version of a document is. The pages hold none of
/// these. `Session::mark_saved` rebases editing on exactly the bytes that
/// reached the server. The next save then appends only the edits made since. An
/// edit made while the bytes were on the wire stays unsaved.
pub type Sent = HashMap<u64, Arc<Vec<u8>>>;

/// Whether PdfCraft can open this entry.
pub fn opens(entry: &Entry) -> bool {
    craft_ui::cloud::opens(entry, DOCUMENT)
}

/// A typed name as a document file name. Any folder separators are dropped and
/// a missing extension becomes `.pdf`.
pub fn document_name(typed: &str) -> String {
    craft_ui::cloud::document_name(typed, DOCUMENT)
}

/// Whether PdfCraft can write the bytes a destination is asking for.
///
/// PdfCraft writes a PDF. A destination whose name says PDF gets one. So does
/// one with a bare name whose content type says PDF. So does one with a bare
/// name and nothing said about it at all. A destination that names another
/// format is refused by [`CloudApp::encode`].
///
/// The file list offers nothing but PDFs. A typed name without a known
/// extension becomes one. What reaches the refusal is a provider that renamed a
/// document under the user, or a caller that passed its own destination.
fn writes(into: Destination<'_>) -> bool {
    let extension = into.extension();
    if !extension.is_empty() {
        return OPENS.contains(&extension.as_str());
    }
    // A bare name leaves the content type to read. A provider that said
    // nothing has said nothing against a PDF either.
    into.mime.is_none_or(|mime| {
        let mime = mime.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
        MIMES.iter().any(|known| mime.starts_with(known))
    })
}

/// The whole of the cloud state, for the control channel and for tests.
pub fn status(app: &PdfCraftApp) -> Json {
    craft_ui::cloud::report(app)
}

/// The Share menu's export half, as the control it is drawn in wants it.
pub fn copies() -> Vec<CopyFormat> {
    COPIES.iter().map(|(id, label)| CopyFormat::new(*id, crate::i18n::t(label))).collect()
}

/// Finish whatever is being typed, then save the open document back over the
/// entry it came from.
///
/// The commit is separate from [`save`] because the pages only ask for it on
/// their own Save button. A save started from the File menu has to ask too: a
/// form field being typed into is part of what is saved (#166).
pub fn save_active(app: &mut PdfCraftApp) -> Result<Json, String> {
    if !app.ready_to_save() {
        // Both halves of `ready_to_save` say why themselves.
        return Ok(Json::Null);
    }
    save(app)
}

/// The host part of the service address, for the account menu's first line.
///
/// The service is deployment-specific. This is therefore read from the
/// environment and compiled in nowhere. Somebody with an account on two
/// deployments needs to know which one a document is about to land in.
fn service_host() -> String {
    ["PDFCRAFT_CLOUD_URL", "CRAFT_CLOUD_URL", "COLLABCRAFT_URL"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok())
        .map(|value| value.trim().trim_end_matches('/').to_owned())
        .find(|value| !value.is_empty())
        .map(|value| value.trim_start_matches("https://").trim_start_matches("http://").to_owned())
        .unwrap_or_default()
}

impl CloudApp for PdfCraftApp {
    fn cloud(&self) -> &CloudFiles {
        &self.cloud
    }

    fn cloud_mut(&mut self) -> &mut CloudFiles {
        &mut self.cloud
    }

    fn kind(&self) -> DocumentKind {
        DOCUMENT
    }

    fn active(&self) -> Option<u64> {
        self.active_ids().map(|(_, id)| id.0)
    }

    fn title(&self, uid: u64) -> String {
        self.session.get(DocId(uid)).map(|doc| doc.display_name()).unwrap_or_default()
    }

    fn open_bytes(&mut self, name: &str, bytes: Vec<u8>) -> Result<Option<u64>, String> {
        // Bytes that are an image or a text file become a new, unsaved PDF
        // (`create_ui::open_converted`). What ends up open is a document of its
        // own. It is therefore given no cloud origin. It gets no way to save
        // back over the file that was read. The list offers none of those
        // anyway. A provider that mislabels one still cannot overwrite it.
        let converted =
            matches!(pdfcraft_engine::source_kind(name, &bytes), Some(pdfcraft_engine::SourceKind::Image | pdfcraft_engine::SourceKind::Text));
        let before = self.views.len();
        PdfCraftApp::open_bytes(self, name, None, bytes)?;
        // Protected bytes are not a document until a password arrives. The
        // prompt carries them on. Nothing is said about them yet.
        if converted || self.password_prompt.is_some() || self.views.len() == before {
            return Ok(None);
        }
        Ok(self.active())
    }

    fn encode(&mut self, uid: u64, into: Destination<'_>) -> Result<Vec<u8>, String> {
        // A document opened as one format has to go back as that format. PDF
        // bytes written under another name would hand the user a file nothing
        // can read, under a name that says otherwise.
        if !writes(into) {
            let what = if into.extension().is_empty() { into.name.to_owned() } else { format!(".{}", into.extension()) };
            let sentence = self.tr("PdfCraft saves a document as a PDF. It cannot save over {what}. Export a PDF writes the other formats.");
            return Err(crate::i18n::fmt(&sentence, &[("what", &what)]));
        }
        let id = DocId(uid);
        let bytes = self.session.save_bytes(id).map_err(|e| e.to_string())?;
        self.cloud_sent.insert(uid, bytes.clone());
        Ok((*bytes).clone())
    }

    fn close(&mut self, uid: u64) {
        if let Some(index) = self.views.iter().position(|view| view.id == DocId(uid)) {
            self.close_tab(index);
        }
    }

    fn mark_saved(&mut self, uid: u64) {
        let Some(bytes) = self.cloud_sent.remove(&uid) else { return };
        let id = DocId(uid);
        if let Err(e) = self.session.mark_saved(id, bytes, None) {
            self.notify_fmt("Saved, but reopening failed: {e}", &[("e", &e.to_string())]);
            return;
        }
        self.forget_recovery(id);
        if let Some(doc) = self.session.get(id)
            && let Some(view) = self.views.iter_mut().find(|view| view.id == id)
        {
            view.document_changed(&doc.info);
        }
    }

    fn drop_pending(&mut self, uid: u64) {
        self.cloud_sent.remove(&uid);
    }

    fn ready_to_save(&mut self) -> bool {
        let Some((_, id)) = self.active_ids() else { return false };
        // What is typed in a form field is part of what is saved (#166). The
        // preference to bake Fill & Sign marks in applies to a cloud save as it
        // does to a local one. Both say why they refused themselves.
        self.commit_typing_in(id) && self.flatten_fill_sign_for_save(id)
    }

    fn sharing(&mut self) -> &mut craft_ui::share::ShareDialog {
        &mut self.share
    }

    fn presence(&mut self) -> &mut craft_ui::presence::PresenceBar {
        &mut self.presence
    }

    fn paint_icon(&self, painter: &Painter, rect: Rect, kind: EntryKind, colour: Color32) {
        let name = if kind == EntryKind::Folder { "folder" } else { "file-text" };
        let size = rect.width().min(rect.height());
        let square = Rect::from_center_size(rect.center(), Vec2::splat(size));
        let image = icons::image(name, size, colour);
        // A file row hands over a painter. `Image::paint_at` wants a `Ui`.
        // The SVG is therefore loaded and drawn here. A texture still being
        // rasterized draws nothing this frame and arrives on the next one. The
        // image widget behaves the same way.
        if let Ok(egui::load::TexturePoll::Ready { texture }) = image.load_for_size(painter.ctx(), square.size()) {
            egui::widgets::paint_texture_at(painter, square, image.image_options(), &texture);
        }
    }

    fn open_url(&self, url: &str) {
        // `PdfCraftApp::open_url` also records the address for tests and
        // automation. Recording needs `&mut self`. The pages hand over
        // `&self`. What is left here is the half that reaches the browser.
        if let Some(ctx) = &self.ctx {
            ctx.open_url(egui::OpenUrl::new_tab(url));
        }
    }

    fn say(&mut self, text: String) {
        self.notify(text);
    }

    fn toast(&mut self, text: String) {
        self.notify(text);
    }

    fn go_to_save_as(&mut self) {
        self.save_as_to_cloud = Some(true);
        self.dialog = Some(crate::Dialog::SaveAs);
    }

    fn app_name(&self) -> &str {
        "PdfCraft"
    }

    fn tr(&self, text: &str) -> String {
        crate::i18n::t(text).to_owned()
    }

    fn service(&self) -> String {
        service_host()
    }

    fn save_note(&self) -> Option<String> {
        Some(self.tr("The document is saved as a PDF in the folder above."))
    }

    fn style(&self, ui: &Ui) -> craft_ui::cloud::CloudStyle {
        let t = Tokens::get(ui.ctx());
        craft_ui::cloud::CloudStyle {
            surface: t.card,
            border: t.border,
            text: t.text,
            dim: t.text_muted,
            hover: t.hover,
            soft: t.accent_soft,
            family: egui::FontFamily::Proportional,
            bold: theme::semibold(13.0).family,
        }
    }
}
