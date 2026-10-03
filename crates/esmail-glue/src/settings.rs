//! The choices the window remembers between runs, kept in a small file next to
//! `config.toml` (not in it, which the other frontends share and rewrite).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use esmail::config::collapsed_folder_key;
use serde::{Deserialize, Serialize};

/// The file name inside the config directory.
const FILE_NAME: &str = "win32-settings.toml";

/// How the window is themed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    /// Always light.
    Light,
    /// Always dark.
    Dark,
    /// Follow the Windows app mode and accent colour, live (the default).
    #[default]
    System,
}

impl ThemeChoice {
    /// The name shown on the theme button.
    pub fn label(self) -> &'static str {
        match self {
            ThemeChoice::Light => "Light",
            ThemeChoice::Dark => "Dark",
            ThemeChoice::System => "System",
        }
    }

    /// The next choice in the cycle the theme button walks.
    pub fn next(self) -> ThemeChoice {
        match self {
            ThemeChoice::Light => ThemeChoice::Dark,
            ThemeChoice::Dark => ThemeChoice::System,
            ThemeChoice::System => ThemeChoice::Light,
        }
    }
}

/// The saved View choices and the tray behaviour.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// View > Theme.
    pub theme: ThemeChoice,
    /// View > Load remote images.
    pub remote_images: bool,
    /// Closing the window hides it to the tray instead of quitting.
    pub close_to_tray: bool,
    /// View > Original colours: draw mail as authored even on the dark theme.
    pub original_colours: bool,
    /// Folder-tree nodes the user collapsed, keyed like the shared config's
    /// `collapsed_folders` (per account, see `esmail::config`).
    #[serde(default)]
    pub collapsed_folders: BTreeSet<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { theme: ThemeChoice::System, remote_images: false, close_to_tray: true, original_colours: false, collapsed_folders: BTreeSet::new() }
    }
}

impl Settings {
    /// The settings file's path, or `None` without a config directory.
    pub fn path() -> Option<PathBuf> {
        esmail::paths::config_dir().map(|dir| dir.join(FILE_NAME))
    }

    /// Records whether the folder `key` of `account_id` is collapsed. Returns
    /// whether that changed the saved set.
    pub fn set_folder_collapsed(&mut self, account_id: &str, key: &str, collapsed: bool) -> bool {
        let entry = collapsed_folder_key(account_id, key);
        if collapsed { self.collapsed_folders.insert(entry) } else { self.collapsed_folders.remove(&entry) }
    }

    /// Reads the settings at `path`. A missing or malformed file yields the
    /// defaults: a broken settings file must never keep the window from opening.
    pub fn load(path: &Path) -> Settings {
        let Ok(text) = std::fs::read_to_string(path) else { return Settings::default() };
        toml::from_str(&text).unwrap_or_default()
    }

    /// Writes the settings to `path`, through a temporary file so a crash cannot
    /// leave half a file behind.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        let temp = path.with_extension("toml.tmp");
        std::fs::write(&temp, text)?;
        std::fs::rename(&temp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("esmail-settings-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(FILE_NAME)
    }

    #[test]
    fn saved_settings_read_back_the_same() {
        let path = temp_file("roundtrip");
        let settings = Settings { theme: ThemeChoice::Dark, remote_images: true, close_to_tray: false, original_colours: true, ..Settings::default() };
        settings.save(&path).unwrap();
        assert_eq!(Settings::load(&path), settings);
    }

    #[test]
    fn a_missing_or_malformed_file_is_the_defaults() {
        assert_eq!(Settings::load(&temp_file("missing")), Settings::default());
        let path = temp_file("malformed");
        std::fs::write(&path, "theme = [").unwrap();
        assert_eq!(Settings::load(&path), Settings::default());
    }

    #[test]
    fn an_older_file_with_fewer_keys_keeps_the_defaults_for_the_rest() {
        let path = temp_file("partial");
        std::fs::write(&path, "theme = \"light\"\n").unwrap();
        assert_eq!(Settings::load(&path), Settings { theme: ThemeChoice::Light, ..Settings::default() });
    }

    #[test]
    fn the_theme_button_cycles_through_all_three() {
        assert_eq!(ThemeChoice::Light.next().next().next(), ThemeChoice::Light);
    }

    #[test]
    fn collapsing_a_folder_is_per_account_and_reads_back() {
        let path = temp_file("folds");
        let mut settings = Settings::default();
        assert!(settings.set_folder_collapsed("work@example.com", "INBOX/Projects", true));
        // The same path under another account is a different entry.
        assert!(settings.set_folder_collapsed("home@example.com", "INBOX/Projects", true));
        settings.save(&path).unwrap();
        let loaded = Settings::load(&path);
        assert_eq!(loaded.collapsed_folders.len(), 2);
        assert!(!settings.set_folder_collapsed("work@example.com", "INBOX/Projects", true), "already collapsed");
        assert!(settings.set_folder_collapsed("work@example.com", "INBOX/Projects", false), "expanding removes it");
    }
}
