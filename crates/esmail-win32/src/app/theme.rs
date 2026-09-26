//! The window's theme: the user's choice, and following the system's.
//!
//! xui-win32 re-themes the widgets and the frame by itself when the system theme
//! changes (`Ui::follow_system_theme`), but it has no notification for the app,
//! and the reading pane's HTML has to be rendered again in the new palette. So
//! while the system theme is followed, a slow timer compares the window's theme
//! with the palette on screen.

use xui_win32::prelude::*;

use esmail_win32::core_glue::ThemeChoice;
use super::settings::SettingsMsg;
use super::{App, Msg, chrome};

/// How often the followed system theme is compared with the reading pane.
pub(super) const POLL_MILLIS: u32 = 500;

impl App {
    /// View > Theme, and the start-up theme.
    pub(super) fn choose_theme(&mut self, ui: &mut Ui<Msg>, choice: ThemeChoice) {
        self.theme = choice;
        self.settings.theme = choice;
        self.save_settings();
        let following = choice == ThemeChoice::System;
        ui.follow_system_theme(following);
        ui.set_theme(chrome::theme(choice));
        self.reader.set_palette(super::palette_for(&ui.theme()));
        self.refresh_menu(ui);
        self.composes.set_theme(ui.theme(), following);
        self.accounts.set_theme(ui.theme(), following);
        self.queues.set_theme(ui.theme(), following);
        if let Some(window) = self.settings_window.as_ref().filter(|window| window.is_alive()) {
            let _ = window.send(SettingsMsg::SetTheme(ui.theme(), following));
        }
        if following {
            self.resume_theme_poll(ui);
        } else {
            self.pause_theme_poll(ui);
        }
    }

    /// Starts watching the followed system theme, unless the window is in the tray.
    pub(super) fn resume_theme_poll(&mut self, ui: &mut Ui<Msg>) {
        if self.theme != ThemeChoice::System || self.hidden {
            return;
        }
        if self.theme_poll.is_none() {
            self.theme_poll = ui.set_timer(POLL_MILLIS).ok();
        }
        self.sync_reader_theme(ui);
    }

    /// Stops the timer (a window in the tray must wake nothing).
    pub(super) fn pause_theme_poll(&mut self, ui: &Ui<Msg>) {
        if let Some(timer) = self.theme_poll.take() {
            ui.kill_timer(timer);
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
