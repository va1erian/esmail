//! Attachment files: reading one to send, and writing received ones to disk.
//!
//! These block on the file system, so callers run them off the UI thread.

use std::path::{Path, PathBuf};

use esmail::render::Attachment;
use esmail::view_model::safe_attachment_filename;

/// Largest file offered as an attachment. Most servers refuse messages past
/// 25 MB, and base64 adds a third on top of the file.
pub const MAX_ATTACHMENT_BYTES: u64 = 18 * 1024 * 1024;

/// Reads `path` as an attachment: its file name and its bytes.
pub fn read_attachment(path: &Path) -> Result<(String, Vec<u8>), String> {
    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).ok_or_else(|| format!("{} is not a file", path.display()))?;
    let size = std::fs::metadata(path).map_err(|e| format!("Cannot read {name}: {e}"))?.len();
    if size > MAX_ATTACHMENT_BYTES {
        return Err(format!("{name} is too large to attach ({} MB; the limit is {} MB).", size >> 20, MAX_ATTACHMENT_BYTES >> 20));
    }
    let data = std::fs::read(path).map_err(|e| format!("Cannot read {name}: {e}"))?;
    Ok((name, data))
}

/// Writes one received attachment to exactly `path`.
pub fn save_attachment(path: &Path, attachment: &Attachment) -> Result<(), String> {
    std::fs::write(path, &attachment.data).map_err(|e| format!("Cannot write {}: {e}", path.display()))
}

/// Writes every attachment into `dir`, never overwriting a file: a name taken
/// already (or by an earlier attachment) gets " (2)", " (3)"... before its
/// extension. Returns the paths written.
pub fn save_all(dir: &Path, attachments: &[Attachment]) -> Result<Vec<PathBuf>, String> {
    let mut written = Vec::new();
    for attachment in attachments {
        let path = free_name(dir, &safe_attachment_filename(&attachment.filename));
        save_attachment(&path, attachment)?;
        written.push(path);
    }
    Ok(written)
}

/// Writes `attachment` into a fresh temporary folder, under its sender-chosen
/// name (stripped of any path), and returns the file for the OS to open.
pub fn write_temp(attachment: &Attachment) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(format!("esmail-open-{}-{}", std::process::id(), unique()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
    let path = dir.join(safe_attachment_filename(&attachment.filename));
    save_attachment(&path, attachment)?;
    Ok(path)
}

fn unique() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())
}

fn free_name(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, format!(".{extension}")),
        _ => (name, String::new()),
    };
    (2..).map(|n| dir.join(format!("{stem} ({n}){extension}"))).find(|path| !path.exists()).expect("an unbounded range has a free name")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attachment(name: &str, data: &[u8]) -> Attachment {
        Attachment { filename: name.into(), mime_type: "application/octet-stream".into(), data: data.to_vec() }
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("esmail-files-test-{tag}-{}", unique()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_file_round_trips_through_an_attachment() {
        let dir = scratch("round");
        let path = dir.join("notes.txt");
        std::fs::write(&path, b"hello").unwrap();
        assert_eq!(read_attachment(&path).unwrap(), ("notes.txt".to_string(), b"hello".to_vec()));
        let out = dir.join("copy.txt");
        save_attachment(&out, &attachment("notes.txt", b"hello")).unwrap();
        assert_eq!(std::fs::read(out).unwrap(), b"hello");
    }

    #[test]
    fn a_missing_file_is_reported_by_name() {
        let error = read_attachment(&scratch("missing").join("nope.bin")).unwrap_err();
        assert!(error.contains("nope.bin"), "{error}");
    }

    #[test]
    fn save_all_never_overwrites_and_strips_paths() {
        let dir = scratch("all");
        std::fs::write(dir.join("a.txt"), b"old").unwrap();
        let written = save_all(&dir, &[attachment("a.txt", b"1"), attachment("a.txt", b"2"), attachment(r"..\..\evil.dll", b"3")]).unwrap();
        let names: Vec<_> = written.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["a (2).txt", "a (3).txt", "evil.dll"]);
        assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"old");
        assert!(written.iter().all(|p| p.starts_with(&dir)));
    }

    #[test]
    fn the_temp_copy_keeps_only_the_file_name() {
        let path = write_temp(&attachment("../../x/report.pdf", b"pdf")).unwrap();
        assert_eq!(path.file_name().unwrap(), "report.pdf");
        assert_eq!(std::fs::read(&path).unwrap(), b"pdf");
    }
}
