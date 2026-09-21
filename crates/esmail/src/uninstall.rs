//! `esmail --purge-data`: remove everything esMail has stored for the current
//! user. The Windows uninstaller runs this (when the user asks for it) before
//! it deletes the program files, so the knowledge of *where* things live stays
//! in one place, next to the code that puts them there.
//!
//! What "everything" means:
//!
//! * the saved passwords and OAuth refresh tokens in the OS credential store,
//!   one set per account in `config.toml` (read before that file is deleted);
//! * the config and data directories (`config.toml`, the mail cache);
//! * the attachment scratch directory in the temp folder;
//! * on Windows, the notification identity registered under `HKCU`
//!   ([`crate::shell::unregister_notification_identity`]).

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::config::{AccountConfig, Config};
use crate::paths;

/// The secret kinds `secrets.rs` stores per account.
const SECRET_KINDS: [&str; 3] = ["imap", "smtp", "oauth"];

/// The files esMail creates in a directory it does not exclusively own (one
/// relocated with `ESMAIL_CONFIG_DIR` / `ESMAIL_DATA_DIR`, which could be any
/// folder, so it is never deleted wholesale).
const DB_SIDE_FILES: [&str; 4] = ["mails.db", "mails.db-wal", "mails.db-shm", "mails.db-journal"];

/// Where things are, decoupled from the environment so the deletion logic
/// can be tested against a scratch directory.
struct Layout {
    config_dir: Option<PathBuf>,
    /// The config directory is esMail's alone (not an override).
    config_exclusive: bool,
    data_dir: Option<PathBuf>,
    data_exclusive: bool,
    scratch_dir: PathBuf,
}

impl Layout {
    fn current() -> Self {
        Self {
            config_dir: paths::config_dir(),
            config_exclusive: paths::config_dir_is_default(),
            data_dir: paths::data_dir(),
            data_exclusive: paths::data_dir_is_default(),
            scratch_dir: paths::attachments_dir(),
        }
    }
}

/// Remove all of esMail's per-user state. Returns a description of each thing
/// that could not be removed (empty on complete success); a missing item is
/// not a failure.
pub fn purge_user_data() -> Vec<String> {
    let accounts = Config::load().accounts;
    let mut problems = purge(&Layout::current(), &accounts, &mut |id, kind| crate::secrets::delete_password(id, kind));
    if let Err(e) = crate::shell::unregister_notification_identity() {
        problems.push(format!("notification registration: {e}"));
    }
    problems
}

fn purge(layout: &Layout, accounts: &[AccountConfig], delete_secret: &mut dyn FnMut(&str, &str)) -> Vec<String> {
    let mut problems = Vec::new();

    for account in accounts {
        for kind in SECRET_KINDS {
            delete_secret(&account.id, kind);
        }
    }

    if let Some(dir) = &layout.config_dir {
        if layout.config_exclusive {
            remove_owned_dir(dir, &mut problems);
        } else {
            remove_files(dir, &[paths::CONFIG_FILE_NAME], &mut problems);
        }
    }
    if let Some(dir) = &layout.data_dir {
        if layout.data_exclusive {
            remove_owned_dir(dir, &mut problems);
        } else {
            remove_files(dir, &[DB_SIDE_FILES.as_slice(), &[paths::TOAST_ICON_FILE_NAME]].concat(), &mut problems);
        }
    }
    remove_dir_all_if_present(&layout.scratch_dir, &mut problems);
    problems
}

/// Delete `dir` and the now-empty `esmail` folder above it that
/// `directories` nests it in (`%APPDATA%\esmail\config` -> `%APPDATA%\esmail`).
fn remove_owned_dir(dir: &Path, problems: &mut Vec<String>) {
    remove_dir_all_if_present(dir, problems);
    if let Some(parent) = dir.parent() {
        if parent.file_name().is_some_and(|name| name.eq_ignore_ascii_case("esmail")) {
            // Only succeeds if it is empty; anything else in there is not ours.
            let _ = fs::remove_dir(parent);
        }
    }
}

fn remove_dir_all_if_present(dir: &Path, problems: &mut Vec<String>) {
    match fs::remove_dir_all(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => problems.push(format!("{}: {e}", dir.display())),
    }
}

/// Delete the named files in `dir`, then `dir` itself if that left it empty.
fn remove_files(dir: &Path, names: &[&str], problems: &mut Vec<String>) {
    for name in names {
        let path = dir.join(name);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => problems.push(format!("{}: {e}", path.display())),
        }
    }
    let _ = fs::remove_dir(dir);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("esmail-uninstall-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn account(id: &str) -> AccountConfig {
        let mut account = AccountConfig::new(id.to_string(), "imap.example.com".to_string(), 993, id.to_string());
        account.id = id.to_string();
        account
    }

    #[test]
    fn deletes_every_secret_kind_of_every_account() {
        let root = scratch("secrets");
        let layout = Layout {
            config_dir: None,
            config_exclusive: true,
            data_dir: None,
            data_exclusive: true,
            scratch_dir: root.join("scratch"),
        };
        let mut deleted = Vec::new();
        let problems = purge(&layout, &[account("a"), account("b")], &mut |id, kind| deleted.push(format!("{id}:{kind}")));
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(deleted, ["a:imap", "a:smtp", "a:oauth", "b:imap", "b:smtp", "b:oauth"]);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn removes_owned_directories_and_the_empty_esmail_folder_above() {
        let root = scratch("owned");
        let config = root.join("Roaming").join("esmail").join("config");
        let data = root.join("Local").join("esmail").join("data");
        for dir in [&config, &data] {
            fs::create_dir_all(dir).unwrap();
            fs::write(dir.join("anything.bin"), b"x").unwrap();
        }
        let scratch_dir = root.join("temp").join("esmail-attachments");
        fs::create_dir_all(&scratch_dir).unwrap();
        fs::write(scratch_dir.join("a.pdf"), b"x").unwrap();

        let layout = Layout {
            config_dir: Some(config.clone()),
            config_exclusive: true,
            data_dir: Some(data.clone()),
            data_exclusive: true,
            scratch_dir: scratch_dir.clone(),
        };
        assert!(purge(&layout, &[], &mut |_, _| {}).is_empty());
        assert!(!root.join("Roaming").join("esmail").exists());
        assert!(!root.join("Local").join("esmail").exists());
        assert!(!scratch_dir.exists());
        // Only the esmail folders go, never their parents.
        assert!(root.join("Roaming").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn leaves_foreign_files_in_a_relocated_directory_alone() {
        let root = scratch("override");
        let dir = root.join("my-stuff");
        fs::create_dir_all(&dir).unwrap();
        for name in ["config.toml", "mails.db", "mails.db-wal", "notes.txt"] {
            fs::write(dir.join(name), b"x").unwrap();
        }
        let layout = Layout {
            config_dir: Some(dir.clone()),
            config_exclusive: false,
            data_dir: Some(dir.clone()),
            data_exclusive: false,
            scratch_dir: root.join("scratch"),
        };
        assert!(purge(&layout, &[], &mut |_, _| {}).is_empty());
        assert!(!dir.join("config.toml").exists());
        assert!(!dir.join("mails.db").exists());
        assert!(!dir.join("mails.db-wal").exists());
        assert!(dir.join("notes.txt").exists(), "a file esMail did not create must survive");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_relocated_directory_that_becomes_empty_is_removed() {
        let root = scratch("override-empty");
        let dir = root.join("cfg");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.toml"), b"x").unwrap();
        let layout = Layout {
            config_dir: Some(dir.clone()),
            config_exclusive: false,
            data_dir: None,
            data_exclusive: true,
            scratch_dir: root.join("scratch"),
        };
        assert!(purge(&layout, &[], &mut |_, _| {}).is_empty());
        assert!(!dir.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn nothing_to_remove_is_success() {
        let root = scratch("nothing");
        let layout = Layout {
            config_dir: Some(root.join("missing-config")),
            config_exclusive: true,
            data_dir: Some(root.join("missing-data")),
            data_exclusive: false,
            scratch_dir: root.join("missing-scratch"),
        };
        assert!(purge(&layout, &[], &mut |_, _| {}).is_empty());
        let _ = fs::remove_dir_all(root);
    }
}
