//! `esmail-win32`: esMail's native Win32 frontend. The window lives in `app`;
//! on other targets this is an empty program so the workspace still builds
//! there.

// Release builds are a GUI app, so launching it does not open a console window.
// Debug builds keep the console: `--screenshot` and the examples print their
// timing report to stderr.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(windows)]
mod app;

#[cfg(windows)]
fn main() {
    app::main();
}

#[cfg(not(windows))]
fn main() {}
