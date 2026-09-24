//! Embeds the Common Controls v6 + per-monitor-v2 DPI manifest into the
//! example binaries, so the `TreeView` in `examples/message_list` gets the modern
//! themed look (and the common controls initialise against comctl32 v6). The
//! library itself is left untouched; on non-Windows hosts this is a no-op.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    println!("cargo:rerun-if-changed=esmail-win32.rc");
    println!("cargo:rerun-if-changed=esmail-win32.manifest");

    embed_resource::compile_for_everything("esmail-win32.rc", embed_resource::NONE)
        .manifest_optional()
        .expect("compile the esmail-win32 manifest");
}
