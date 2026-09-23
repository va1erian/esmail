//! Entry point for the `message_list` example; the body lives in `app.rs` so
//! the file still has a `main` on non-Windows targets, where the library is
//! empty.

#[cfg(windows)]
mod app;
#[cfg(windows)]
mod bench;

#[cfg(windows)]
fn main() {
    app::main();
}

#[cfg(not(windows))]
fn main() {}
