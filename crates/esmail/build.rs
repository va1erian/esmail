//! Attaches the application icon and version information to `esmail.exe`, so
//! Explorer, the taskbar, Alt+Tab and "Apps & features" show esMail's icon and
//! the right name instead of a generic executable.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icon.ico");

    // `CARGO_CFG_TARGET_OS` is the *target*; `cfg!(windows)` would describe the
    // machine building this script, which differs when cross-compiling.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut resource = winresource::WindowsResource::new();
    resource
        .set_icon("assets/icon.ico")
        .set("ProductName", "esMail")
        .set("FileDescription", "esMail - IMAP mail client")
        .set("OriginalFilename", "esmail.exe");
    if let Err(e) = resource.compile() {
        // Without the resource the program still works, just with a generic
        // icon; do not make that a build failure on a machine without the
        // Windows SDK's resource compiler.
        println!("cargo:warning=could not embed the Windows icon and version info: {e}");
    }
}
