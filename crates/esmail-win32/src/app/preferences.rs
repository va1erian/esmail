//! The choices kept between runs (`win32-settings.toml`) and the View menu that
//! shows them.

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

    /// View > Close to tray.
    pub(super) fn set_close_to_tray(&mut self, ui: &Ui<Msg>, on: bool) {
        self.settings.close_to_tray = on;
        self.save_settings();
        self.refresh_menu(ui);
    }
}
