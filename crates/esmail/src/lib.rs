//! The esMail core: IMAP/SMTP actors, the SQLite cache, HTML sanitizing and
//! the shared models, with **no UI toolkit**. The native Windows frontend
//! (`crates/esmail-win32`) uses it today; the egui frontend lives in its own
//! repository (`va1erian/esmail-egui`) and depends on this crate. The actors
//! are also driven directly by `crates/esmail/tests` against
//! `mail-mock-server`.

pub mod app;
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
pub mod ipc;
pub mod notify;
pub mod oauth;
pub mod paths;
pub mod progress;
pub mod render;
pub mod search_query;
pub mod secrets;
pub mod session;
pub mod shell;
pub mod shortcuts;
pub mod smtp;
pub mod uninstall;
pub mod view_model;
pub mod waker;
pub mod watcher;
/// Tray icon and new-mail toasts (B10): the OS-specific implementation is
/// chosen inside `platform`, see its module doc.
pub mod platform;
