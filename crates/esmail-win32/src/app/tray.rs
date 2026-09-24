//! The tray icon: Open / Compose / Quit, a tooltip with the unread total, and
//! closing the window to it.
//!
//! The icon's own hidden window lives on the UI thread, so its click and menu
//! events arrive in this thread's message loop. They are forwarded to the
//! window as messages from `tray-icon`'s event handlers: nothing polls.

use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use win32ui::prelude::*;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{IsIconic, SW_HIDE, SW_RESTORE, SW_SHOW, ShowWindow};

use esmail_win32::core_glue::compose::Kind;
use esmail_win32::core_glue::FolderRef;
use esmail_win32::core_glue::resident::{account_index, tray_tooltip};

use super::instance::Launch;
use super::{App, Msg};

/// The icon in the notification area. Dropping it removes the icon.
pub struct Tray {
    icon: TrayIcon,
    unread: u32,
}

impl Tray {
    /// Shows the icon. Fails (and the window then quits when closed, rather than
    /// hiding with no way back) when the shell has no notification area.
    pub fn new(proxy: Proxy<Msg>) -> anyhow::Result<Tray> {
        let art = esmail::icons::tray_icon(esmail::shell::taskbar_is_dark()).ok_or_else(|| anyhow::anyhow!("the embedded tray icon does not decode"))?;
        let open = MenuItem::new("&Open esMail", true, None);
        let compose = MenuItem::new("&Compose", true, None);
        let quit = MenuItem::new("&Quit", true, None);
        let menu = Menu::new();
        menu.append_items(&[&open, &compose, &PredefinedMenuItem::separator(), &quit])?;
        let (open_id, compose_id, quit_id) = (open.id().clone(), compose.id().clone(), quit.id().clone());

        let menu_proxy = proxy.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let launch = if event.id == open_id {
                Launch::Show
            } else if event.id == compose_id {
                Launch::Compose
            } else if event.id == quit_id {
                Launch::Quit
            } else {
                return;
            };
            let _ = menu_proxy.send(Msg::Launch(launch));
        }));
        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
            // Act on the release, like a button, so one click is one request.
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                let _ = proxy.send(Msg::Launch(Launch::Show));
            }
        }));

        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip(tray_tooltip(0))
            .with_icon(Icon::from_rgba(art.pixels, art.width, art.height)?)
            .build()?;
        Ok(Tray { icon, unread: 0 })
    }

    /// Puts the unread total in the tooltip; the shell is only touched when it changed.
    pub fn set_unread(&mut self, unread: u32) {
        if self.unread == unread {
            return;
        }
        self.unread = unread;
        if let Err(error) = self.icon.set_tooltip(Some(tray_tooltip(unread))) {
            log::warn!("could not update the tray tooltip: {error}");
        }
    }
}

fn raw(ui: &Ui<Msg>) -> HWND {
    HWND(ui.hwnd().raw() as *mut core::ffi::c_void)
}

impl App {
    /// What the tray, a toast click or a second launch asked for.
    pub(super) fn launch(&mut self, ui: &mut Ui<Msg>, launch: Launch) {
        match launch {
            Launch::Quit => self.quit(ui),
            Launch::Show => self.show_window(ui),
            Launch::Compose => {
                self.show_window(ui);
                self.open_compose(ui, Kind::New);
            }
            Launch::OpenAccount(id) => {
                self.show_window(ui);
                match account_index(self.core.accounts(), &id) {
                    Some(account) => self.open_folder(ui, FolderRef { account, mailbox: "INBOX".to_string() }),
                    None => log::warn!("a toast named the unknown account {id}"),
                }
            }
        }
    }

    /// The close box: hides to the tray when there is one and the user wants
    /// that, else quits (after the compose windows have been dealt with).
    pub(super) fn close(&mut self, ui: &mut Ui<Msg>) {
        if self.tray.is_some() && self.settings.close_to_tray {
            self.save_window_state(ui);
            self.hide_window(ui);
        } else {
            self.close_and_quit(ui);
        }
    }

    /// Brings the window back from the tray or the taskbar and to the front.
    fn show_window(&mut self, ui: &mut Ui<Msg>) {
        let hwnd = raw(ui);
        // SAFETY: `hwnd` is this app's own live top-level window.
        unsafe {
            let _ = ShowWindow(hwnd, if IsIconic(hwnd).as_bool() { SW_RESTORE } else { SW_SHOW });
        }
        ui.set_foreground();
        if self.hidden {
            self.hidden = false;
            self.resume_theme_poll(ui);
        }
    }

    /// Hides the window and stops everything that only exists to keep it
    /// current, so a window in the tray wakes nothing.
    fn hide_window(&mut self, ui: &mut Ui<Msg>) {
        // SAFETY: as in `show_window`.
        unsafe {
            let _ = ShowWindow(raw(ui), SW_HIDE);
        }
        self.hidden = true;
        self.pause_theme_poll(ui);
    }

    /// Mirrors the inbox unread total into the tooltip and the window title.
    pub(super) fn sync_unread(&mut self, ui: &Ui<Msg>) {
        let unread = self.folders.borrow().inbox_unread();
        if let Some(tray) = self.tray.as_mut() {
            tray.set_unread(unread);
        }
        if unread != self.title_unread {
            self.title_unread = unread;
            ui.set_title(&if unread == 0 { "esMail".to_string() } else { format!("esMail ({unread})") });
        }
    }
}
