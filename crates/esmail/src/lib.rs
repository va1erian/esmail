//! Library half of the `esmail` crate: `main.rs` (the binary) builds the
//! eframe/egui app on top of these modules. Split out as a library purely so
//! `tests/imap_smtp_integration.rs` (an external test crate) can drive
//! `ImapActor`/`SmtpActor` directly against `mail-mock-server` — nothing
//! about the app's own structure or module boundaries changes.

pub mod auth;
pub mod compose;
pub mod config;
pub mod css;
pub mod db;
pub mod emoji;
pub mod idle_watch;
pub mod imap;
pub mod notify;
pub mod oauth;
pub mod render;
pub mod screenshot;
pub mod search_query;
pub mod secrets;
pub mod smtp;
/// Tray icon + Windows toast notifications (B10). Windows-only: see
/// notify.rs's module doc for why the pure detection logic lives separately
/// and builds everywhere.
#[cfg(target_os = "windows")]
pub mod tray;
