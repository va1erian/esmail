//! Entry point for the `view` example; the body lives in `app.rs` so the file
//! still has a `main` on non-Windows targets, where the library is empty.

#[cfg(windows)]
mod app;

#[cfg(windows)]
fn main() {
    app::main();
}

#[cfg(not(windows))]
fn main() {}
