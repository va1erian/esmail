//! Where esMail keeps things on disk.
//!
//! Three kinds of files, each in the place the platform expects it:
//!
//! * **Configuration** (`config.toml`): small, hand-editable, worth
//!   roaming with the user. `%APPDATA%\esmail\config` on Windows,
//!   `~/.config/esmail` on Linux.
//! * **Data** (`mails.db`, the local mail cache): large, rebuildable from the
//!   servers, and not something to sync between machines.
//!   `%LOCALAPPDATA%\esmail\data` on Windows, `~/.local/share/esmail` on Linux.
//! * **Scratch** (attachments opened with "Open"): the OS temp directory.
//!
//! Setting `ESMAIL_CONFIG_DIR` / `ESMAIL_DATA_DIR` relocates the first two.
//! That is for portable installs and for running a development build without
//! touching the real profile; the installer's uninstall step never sets them,
//! so it always finds the real one.

use std::path::PathBuf;

const CONFIG_DIR_VAR: &str = "ESMAIL_CONFIG_DIR";
const DATA_DIR_VAR: &str = "ESMAIL_DATA_DIR";

/// File names inside the config and data directories. `uninstall` deletes
/// exactly these (and nothing else) from an overridden directory it does not
/// own.
pub const CONFIG_FILE_NAME: &str = "config.toml";
pub const DB_FILE_NAME: &str = "mails.db";
/// The notification icon written next to the cache (see `shell`).
pub const TOAST_ICON_FILE_NAME: &str = "toast-icon.png";

fn project_dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("", "", "esmail")
}

fn override_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// Directory holding `config.toml`.
pub fn config_dir() -> Option<PathBuf> {
    override_dir(CONFIG_DIR_VAR).or_else(|| project_dirs().map(|d| d.config_dir().to_path_buf()))
}

/// Directory holding the mail cache and other regenerable state.
pub fn data_dir() -> Option<PathBuf> {
    override_dir(DATA_DIR_VAR).or_else(|| project_dirs().map(|d| d.data_local_dir().to_path_buf()))
}

/// `config.toml`.
pub fn config_file() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join(CONFIG_FILE_NAME))
}

/// The SQLite mail cache.
pub fn db_file() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join(DB_FILE_NAME))
}

/// Where attachments are written so the OS can open them.
pub fn attachments_dir() -> PathBuf {
    std::env::temp_dir().join("esmail-attachments")
}

/// `true` if the config directory is the platform default rather than an
/// `ESMAIL_CONFIG_DIR` override -- i.e. a folder that is esMail's alone and
/// can be deleted wholesale.
pub fn config_dir_is_default() -> bool {
    override_dir(CONFIG_DIR_VAR).is_none()
}

/// Same as [`config_dir_is_default`], for the data directory.
pub fn data_dir_is_default() -> bool {
    override_dir(DATA_DIR_VAR).is_none()
}

/// Delete leftovers from earlier runs in the attachment scratch directory.
/// Best-effort: a file still open in another program simply stays.
pub fn clean_attachments_dir() {
    let dir = attachments_dir();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let _ = if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_and_data_are_different_directories() {
        // Without overrides (the test environment sets none) they must not
        // collapse into one: the cache would then roam with the config.
        if override_dir(CONFIG_DIR_VAR).is_none() && override_dir(DATA_DIR_VAR).is_none() {
            assert_ne!(config_dir(), data_dir());
        }
    }

    #[test]
    fn db_lives_in_the_data_dir_not_the_working_directory() {
        let db = db_file().expect("a data directory");
        assert!(db.is_absolute(), "{}", db.display());
        assert_eq!(db.parent().map(PathBuf::from), data_dir());
    }

    #[test]
    fn config_file_is_config_toml_in_the_config_dir() {
        let file = config_file().expect("a config directory");
        assert_eq!(file.file_name().unwrap(), "config.toml");
        assert_eq!(file.parent().map(PathBuf::from), config_dir());
    }
}
