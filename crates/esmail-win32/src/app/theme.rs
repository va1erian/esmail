//! The window's theme: the user's choice, and following the system's.
//!
//! win32ui re-themes the widgets and the frame by itself when the system theme
//! changes (`Ui::follow_system_theme`), but it has no notification for the app,
//! and the reading pane's HTML has to be rendered again in the new palette. So
//! while the system theme is followed, a slow timer compares the window's theme
//! with the palette on screen.

use win32ui::prelude::*;

use super::args::ThemeChoice;
use super::{App, Msg, chrome};

/// How often the followed system theme is compared with the reading pane.
pub(super) const POLL_MILLIS: u32 = 500;

impl App {
    /// View > Theme, and the start-up theme.
    pub(super) fn choose_theme(&mut self, ui: &mut Ui<Msg>, choice: ThemeChoice) {
        self.theme = choice;
        let following = choice == ThemeChoice::System;
        ui.follow_system_theme(following);
        ui.set_theme(chrome::theme(choice));
        self.reader.set_palette(super::palette_for(&ui.theme()));
        ui.set_menu_bar(chrome::menu_bar(self.theme, self.original_colours, self.remote_images));
        self.composes.set_theme(ui.theme(), following);
        self.accounts.set_theme(ui.theme(), following);
        match (following, self.theme_poll) {
            (true, None) => self.theme_poll = ui.set_timer(POLL_MILLIS).ok(),
            (false, Some(timer)) => {
                ui.kill_timer(timer);
                self.theme_poll = None;
            }
            _ => {}
        }
    }

    /// The poll tick: the system theme changed underneath the window.
    pub(super) fn sync_reader_theme(&mut self, ui: &Ui<Msg>) {
        let theme = ui.theme();
        if self.reader.is_dark() != theme.is_dark {
            self.reader.set_palette(super::palette_for(&theme));
        }
    }
}
