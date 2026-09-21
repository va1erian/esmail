//! The operating-system specific parts, behind one interface: a system tray
//! icon with a menu, and new-mail toast notifications that report clicks.
//!
//! The implementation is chosen at compile time and the rest of the app never
//! names a platform: [`windows.rs`](self) on Windows, [`other.rs`](self)
//! (which does as little as is honest) everywhere else. Adding another
//! platform is one more file and one more `cfg` line here -- `main.rs` needs no
//! changes, since it only ever sees the items re-exported below:
//!
//! - [`TrayState::new`] -- build the tray icon. Fails on a platform without
//!   one (or when the shell's tray is unavailable), which the caller treats as
//!   "no tray this session": closing the window then exits normally instead of
//!   hiding it with no way back.
//! - [`TrayState::poll_actions`] -- drain what the user asked for through the
//!   icon or its menu ([`TrayAction`]).
//! - [`TrayState::set_unread`] -- show the unread total on the icon.
//! - [`show_new_mail_toast`] -- show a toast for an account's new mail.
//! - [`set_toast_click_handler`] -- what to do with the account id when a
//!   toast is clicked. The handler may run on any thread.
//!
//! The decision logic these are driven by (whether it is new mail, what the
//! text says, the toast XML) is platform independent and lives in
//! `notify.rs`.

#[cfg(windows)]
#[path = "windows.rs"]
mod imp;

#[cfg(not(windows))]
#[path = "other.rs"]
mod imp;

pub use imp::{TrayAction, TrayState, set_toast_click_handler, show_new_mail_toast, use_own_notification_identity};
