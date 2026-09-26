//! New-mail toasts. Detection and wording are the library's (the account
//! sessions' IDLE watch and `esmail::notify`); this only decides whether to
//! show the toast and routes a click back to the window.

use std::sync::Arc;

use esmail::platform;
use esmail::session::NotifyFn;
use xui_win32::Proxy;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

use super::instance::Launch;
use super::Msg;

/// Gives toasts esMail's own name and icon, and sends a click on one to the
/// window. The name is a per-user registry key the egui frontend writes too (see
/// `esmail::shell`); a run with a relocated data directory (a test profile) does
/// not write it, since the key would then point at a directory that goes away.
pub fn install(proxy: Proxy<Msg>) {
    if esmail::paths::data_dir_is_default() {
        match esmail::shell::register_notification_identity() {
            Ok(()) => platform::use_own_notification_identity(true),
            Err(error) => log::warn!("could not register the notification identity: {error}"),
        }
    }
    platform::set_toast_click_handler(move |account| {
        let _ = proxy.send(Msg::Launch(Launch::OpenAccount(account)));
    });
}

/// The hook the account sessions call with new mail: a toast, unless the window
/// is the one being looked at. Runs on the runtime's threads.
pub fn hook(window: usize) -> NotifyFn {
    Arc::new(move |account, title, body| {
        // SAFETY: `GetForegroundWindow` takes no arguments and only reads.
        let foreground = unsafe { GetForegroundWindow() };
        if foreground != HWND(window as *mut core::ffi::c_void) {
            platform::show_new_mail_toast(account, title, body);
        }
    })
}
