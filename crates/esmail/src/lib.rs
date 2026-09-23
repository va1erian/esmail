//! Library half of the `esmail` crate: `main.rs` (the binary) builds the
//! eframe/egui app on top of these modules. Split out as a library purely so
//! `tests/imap_smtp_integration.rs` (an external test crate) can drive
//! `ImapActor`/`SmtpActor` directly against `mail-mock-server` — nothing
//! about the app's own structure or module boundaries changes.

pub mod auth;
pub mod compose;
pub mod config;
pub mod contacts;
pub mod css;
pub mod db;
pub mod emoji;
pub mod icons;
pub mod idle_watch;
pub mod imap;
pub mod notify;
pub mod oauth;
pub mod paths;
pub mod render;
pub mod screenshot;
pub mod search_query;
pub mod secrets;
pub mod session;
pub mod shell;
pub mod smtp;
pub mod uninstall;
pub mod watcher;
/// Tray icon and new-mail toasts (B10): the OS-specific implementation is
/// chosen inside `platform`, see its module doc.
pub mod platform;
