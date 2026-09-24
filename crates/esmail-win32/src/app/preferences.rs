//! The choices kept between runs (`win32-settings.toml`) and the View menu that
//! shows them.

use esmail::imap::MailHeader;
use esmail_win32::core_glue::{sender_trusted, set_sender_trusted};
use win32ui::prelude::*;

use super::chrome::{self, ViewState};
use super::{App, Msg};

impl App {
    /// Rebuilds the menu bar to show the current View choices.
    pub(super) fn refresh_menu(&self, ui: &Ui<Msg>) {
        ui.set_menu_bar(chrome::menu_bar(ViewState {
            theme: self.theme,
            original_colours: self.original_colours,
            remote_images: self.remote_images,
            close_to_tray: self.tray.is_some().then_some(self.settings.close_to_tray),
        }));
    }

    /// Writes the settings out. A failure is logged, not shown: the choice
    /// still applies to this run.
    pub(super) fn save_settings(&self) {
        let Some(path) = self.settings_path.as_deref() else { return };
        if let Err(error) = self.settings.save(path) {
            log::warn!("could not save the settings to {}: {error}", path.display());
        }
    }

    /// Whether the message on screen is from a sender whose images always load.
    pub(super) fn current_sender_trusted(&self) -> bool {
        self.reader.current_message().is_some_and(|(header, _)| sender_trusted(&self.config, header))
    }

    /// Fetches remote images for `header` if the View toggle or its sender's
    /// trust says so. Call before showing the message.
    pub(super) fn allow_images_for(&mut self, header: &MailHeader) {
        self.reader.set_remote_images(self.remote_images || sender_trusted(&self.config, header));
    }

    /// "Always load from <sender>" (or, with `false`, stop doing so), for the
    /// message on screen. A `--profile` run only reads a copy of the config, so
    /// there the choice lasts for the session.
    pub(super) fn trust_sender(&mut self, trusted: bool) {
        let Some((header, _)) = self.reader.current_message() else { return };
        let header = header.clone();
        if set_sender_trusted(&mut self.config, &header, trusted) && self.editable {
            self.config_saver.save(&self.config);
        }
        self.allow_images_for(&header);
    }

    /// Stop on the remote-images banner: forgets the sender's trust when that is
    /// why images load, else switches View > Load remote images off.
    pub(super) fn stop_remote_images(&mut self, ui: &Ui<Msg>) {
        if self.current_sender_trusted() {
            self.trust_sender(false);
        } else {
            self.set_remote_images(ui, false);
        }
    }

    /// View > Close to tray.
    pub(super) fn set_close_to_tray(&mut self, ui: &Ui<Msg>, on: bool) {
        self.settings.close_to_tray = on;
        self.save_settings();
        self.refresh_menu(ui);
    }
}
