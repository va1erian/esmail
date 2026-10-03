//! Where the window was and how its panes were split, kept in a small file
//! next to `config.toml` (not in it, which the other frontends share and
//! rewrite).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The file name inside the config directory.
const FILE_NAME: &str = "win32-window.toml";

/// The window placement and split positions to restore at the next start.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    /// The restored (not maximized) outer rectangle in screen pixels: left,
    /// top, right, bottom.
    #[serde(default)]
    pub bounds: Option<[i32; 4]>,
    /// Whether the window was maximized.
    #[serde(default)]
    pub maximized: bool,
    /// The folder pane's width, in device-independent pixels.
    #[serde(default)]
    pub folders_width: Option<f32>,
    /// The message list's width, in device-independent pixels.
    #[serde(default)]
    pub list_width: Option<f32>,
}

impl WindowState {
    /// The state file's path, or `None` without a config directory.
    pub fn path() -> Option<PathBuf> {
        esmail::paths::config_dir().map(|dir| dir.join(FILE_NAME))
    }

    /// Reads the state at `path`. A missing, unreadable or malformed file, and
    /// any value that could not be a real window, yield the defaults: a broken
    /// state file must never keep the window from opening.
    pub fn load(path: &Path) -> WindowState {
        let Ok(text) = std::fs::read_to_string(path) else { return WindowState::default() };
        let Ok(state) = toml::from_str::<WindowState>(&text) else { return WindowState::default() };
        state.sanitized()
    }

    /// Writes the state to `path`, through a temporary file so a crash cannot
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

    /// Drops values that are not usable: an empty or inverted rectangle, a
    /// non-positive or non-finite width.
    fn sanitized(mut self) -> WindowState {
        let usable_width = |width: &Option<f32>| width.is_none_or(|w| w.is_finite() && w > 0.0);
        if self.bounds.is_some_and(|[left, top, right, bottom]| right <= left || bottom <= top) {
            self.bounds = None;
        }
        if !usable_width(&self.folders_width) {
            self.folders_width = None;
        }
        if !usable_width(&self.list_width) {
            self.list_width = None;
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("esmail-window-state-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(FILE_NAME)
    }

    #[test]
    fn a_saved_state_reads_back_the_same() {
        let path = temp_file("roundtrip");
        let state = WindowState { bounds: Some([10, 20, 1210, 780]), maximized: true, folders_width: Some(250.5), list_width: Some(400.0) };
        state.save(&path).unwrap();
        assert_eq!(WindowState::load(&path), state);
    }

    #[test]
    fn a_missing_file_is_the_defaults() {
        assert_eq!(WindowState::load(&temp_file("missing")), WindowState::default());
    }

    #[test]
    fn a_malformed_file_is_the_defaults() {
        let path = temp_file("malformed");
        std::fs::write(&path, "bounds = [1, 2").unwrap();
        assert_eq!(WindowState::load(&path), WindowState::default());
    }

    #[test]
    fn values_that_could_not_be_a_window_are_dropped() {
        let path = temp_file("nonsense");
        std::fs::write(&path, "bounds = [50, 50, 10, 10]\nfolders_width = -3.0\nlist_width = 300.0\n").unwrap();
        let state = WindowState::load(&path);
        assert_eq!((state.bounds, state.folders_width, state.list_width), (None, None, Some(300.0)));
    }

    #[test]
    fn saving_creates_the_directory() {
        let path = temp_file("nested").parent().unwrap().join("deeper").join(FILE_NAME);
        WindowState::default().save(&path).unwrap();
        assert!(path.exists());
    }
}
