//! Clicks in the reading pane: web and mail links, and the attachment links of
//! the header block. What a link may do is decided by `core_glue::links`.

use esmail::compose::ComposeState;
use esmail::render::Attachment;
use esmail::view_model::safe_attachment_filename;
use esmail_win32::core_glue::compose::{Kind, initial_state};
use esmail_win32::core_glue::files;
use esmail_win32::core_glue::links::{LinkAction, classify};
use xui_win32::Ui;

use super::{App, Msg};

impl App {
    pub(super) fn link_clicked(&mut self, ui: &Ui<Msg>, href: String) {
        match classify(&href) {
            LinkAction::Browser(url) => match opener::open_browser(&url) {
                Ok(()) => self.set_status(&format!("Opened {url} in the browser")),
                Err(error) => self.banner(&format!("Could not open {url}: {error}")),
            },
            LinkAction::Mail { to, subject } => self.compose_to(ui, to, subject),
            LinkAction::SaveAttachment(n) => self.save_attachment(ui, n),
            LinkAction::OpenAttachment(n) => self.open_attachment(ui, n),
            LinkAction::SaveAllAttachments => self.save_all_attachments(ui),
            LinkAction::Reconnect(account) => self.reconnect_account(ui, account),
            LinkAction::Refuse(reason) => self.set_status(&reason),
        }
    }

    /// A `mailto:` link: a new message to `to`, from the account on screen.
    fn compose_to(&mut self, ui: &Ui<Msg>, to: String, subject: Option<String>) {
        let account = self.selected_in.as_ref().map_or(0, |folder| folder.account);
        let Some(config) = self.core.accounts().get(account) else { return };
        let state = ComposeState { to, subject: subject.unwrap_or_default(), ..initial_state(Kind::New, None, config) };
        self.start_compose(ui, state, true);
    }

    fn attachment(&self, n: usize) -> Option<Attachment> {
        self.reader.current_attachments().get(n).cloned()
    }

    fn save_attachment(&mut self, ui: &Ui<Msg>, n: usize) {
        let Some(attachment) = self.attachment(n) else { return };
        let Some(path) = rfd::FileDialog::new().set_file_name(safe_attachment_filename(&attachment.filename)).save_file() else { return };
        self.set_status(&format!("Saving {}...", attachment.filename));
        let proxy = ui.proxy();
        std::thread::spawn(move || {
            let result = files::save_attachment(&path, &attachment).map(|()| format!("Saved {}", path.display()));
            let _ = proxy.send(Msg::AttachmentDone(result));
        });
    }

    /// Writes the attachment to a temporary file and hands it to the system.
    fn open_attachment(&mut self, ui: &Ui<Msg>, n: usize) {
        let Some(attachment) = self.attachment(n) else { return };
        self.set_status(&format!("Opening {}...", attachment.filename));
        let proxy = ui.proxy();
        std::thread::spawn(move || {
            let result = files::write_temp(&attachment)
                .and_then(|path| opener::open(&path).map_err(|e| format!("Cannot open {}: {e}", attachment.filename)))
                .map(|()| format!("Opened {}", attachment.filename));
            let _ = proxy.send(Msg::AttachmentDone(result));
        });
    }

    fn save_all_attachments(&mut self, ui: &Ui<Msg>) {
        let attachments = self.reader.current_attachments().to_vec();
        let Some(dir) = rfd::FileDialog::new().set_title("Save all attachments to").pick_folder() else { return };
        self.set_status("Saving attachments...");
        let proxy = ui.proxy();
        std::thread::spawn(move || {
            let result = files::save_all(&dir, &attachments).map(|written| format!("Saved {} attachments to {}", written.len(), dir.display()));
            let _ = proxy.send(Msg::AttachmentDone(result));
        });
    }

    /// A save or open finished, on its thread.
    /// View > Load remote images.
    pub(super) fn set_remote_images(&mut self, ui: &Ui<Msg>, allow: bool) {
        self.remote_images = allow;
        self.settings.remote_images = allow;
        self.save_settings();
        self.reader.set_remote_images(allow || self.current_sender_trusted());
        self.refresh_menu(ui);
    }

    pub(super) fn attachment_done(&mut self, result: std::result::Result<String, String>) {
        match result {
            Ok(text) => self.set_status(&text),
            Err(error) => self.banner(&error),
        }
    }
}
