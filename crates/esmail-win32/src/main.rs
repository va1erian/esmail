//! `esmail-win32`: esMail's native Win32 frontend (read-only prototype). The
//! window lives in `app`; on other targets this is an empty program so the
//! workspace still builds there.

#[cfg(windows)]
mod app;

#[cfg(windows)]
fn main() {
    app::main();
}

#[cfg(not(windows))]
fn main() {}
