//! Attaches the Common Controls v6 + per-monitor-v2 DPI manifest, the
//! application icon and version information to the esmail-win32 binaries on
//! Windows, so the tree view gets the modern themed look and Explorer, the
//! taskbar and Alt+Tab show esMail's icon and name. On other hosts this is a
//! no-op.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=esmail-win32.manifest");
    println!("cargo:rerun-if-changed=../esmail/assets/icon.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut resource = winresource::WindowsResource::new();
    resource
        .set_icon("../esmail/assets/icon.ico")
        .set_manifest_file("esmail-win32.manifest")
        .set("ProductName", "esMail")
        .set("FileDescription", "esMail - IMAP mail client (native)")
        .set("OriginalFilename", "esmail-win32.exe");
    resource.compile().expect("embed the esmail-win32 icon, version info and manifest");
}
