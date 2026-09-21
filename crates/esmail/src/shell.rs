//! Integration with the Windows shell, plus the single-instance lock. On other
//! platforms the Windows-only functions are harmless no-ops, so `main.rs` calls
//! them without `cfg` attributes. Everything here is safe Rust: the Win32 calls
//! go through the `windows` and `windows-registry` crates' safe wrappers.
//!
//! * **Notification identity.** A per-user registry entry under
//!   `HKCUSoftwareClassesAppUserModelId` that gives the toast
//!   AppUserModelID a display name and icon, so toasts read "esMail" instead of
//!   "Windows PowerShell", with or without the installer.
//! * **Single instance.** esMail lives in the tray, so launching it again (from
//!   the Start menu, say) must bring the existing window back rather than start
//!   a second process fighting over the same cache. The first instance holds an
//!   exclusive lock on a file in the data directory; a later launch fails to
//!   take it, drops a request file next to it (`show`, or `quit` for the
//!   installer) and exits. The running instance polls for that file.
//! * **Taskbar theme** query, so the tray icon can be light or dark.

#![forbid(unsafe_code)]

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::PathBuf;
use std::sync::OnceLock;

/// The AppUserModelID toasts are shown under.
pub const APP_USER_MODEL_ID: &str = "io.github.va1erian.esmail";
#[cfg(windows)]
const DISPLAY_NAME: &str = "esMail";

const LOCK_FILE: &str = "esmail.lock";

/// What a later launch of esMail can ask the running one to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// Come to the front (an ordinary second launch).
    Show,
    /// Exit cleanly (`esmail --quit`, which the installer uses before it
    /// replaces or removes the program files).
    Quit,
}

impl Request {
    const ALL: [Request; 2] = [Request::Quit, Request::Show];

    fn file_name(self) -> &'static str {
        match self {
            Request::Show => "show.request",
            Request::Quit => "quit.request",
        }
    }
}

/// Whether this process is the one that owns the tray icon and the cache.
#[derive(Debug, PartialEq, Eq)]
pub enum Instance {
    First,
    AlreadyRunning,
}

/// The lock, held for the life of the process (the OS releases it on exit,
/// however that happens).
static LOCK: OnceLock<File> = OnceLock::new();

fn request_path(request: Request) -> Option<PathBuf> {
    crate::paths::data_dir().map(|dir| dir.join(request.file_name()))
}

/// Try to become the single running instance. Never blocks startup: if the
/// lock file cannot be created at all (no data directory, read-only disk),
/// this process simply counts as the first.
pub fn acquire_single_instance() -> Instance {
    let Some(dir) = crate::paths::data_dir() else { return Instance::First };
    acquire_in(&dir)
}

fn acquire_in(dir: &std::path::Path) -> Instance {
    if fs::create_dir_all(dir).is_err() {
        return Instance::First;
    }
    let Ok(file) = OpenOptions::new().create(true).write(true).truncate(false).open(dir.join(LOCK_FILE)) else {
        return Instance::First;
    };
    match file.try_lock() {
        Ok(()) => {
            // Requests left behind by an instance that died before reading
            // them must not be obeyed by this one (a stale `quit` would close
            // it the moment it starts).
            for request in Request::ALL {
                let _ = fs::remove_file(dir.join(request.file_name()));
            }
            let _ = LOCK.set(file);
            Instance::First
        }
        Err(fs::TryLockError::WouldBlock) => Instance::AlreadyRunning,
        Err(fs::TryLockError::Error(_)) => Instance::First,
    }
}

/// Ask the running instance to do `request`. Call after
/// [`acquire_single_instance`] returned [`Instance::AlreadyRunning`].
pub fn send_request(request: Request) -> io::Result<()> {
    let path = request_path(request).ok_or_else(|| io::Error::other("no data directory"))?;
    fs::write(path, b"")
}

/// The request a later launch left for this instance, if any, consuming it.
/// Cheap enough to call from the UI loop.
pub fn take_request() -> Option<Request> {
    Request::ALL.into_iter().find(|request| request_path(*request).is_some_and(|path| fs::remove_file(path).is_ok()))
}

/// Give the AppUserModelID a display name and icon for toast notifications.
/// Cheap and idempotent; called at every start so a moved or upgraded install
/// keeps working.
pub fn register_notification_identity() -> io::Result<()> {
    #[cfg(windows)]
    {
        imp::register_notification_identity()
    }
    #[cfg(not(windows))]
    {
        Ok(())
    }
}

/// Undo [`register_notification_identity`] (for `--purge-data`).
pub fn unregister_notification_identity() -> io::Result<()> {
    #[cfg(windows)]
    {
        imp::unregister_notification_identity()
    }
    #[cfg(not(windows))]
    {
        Ok(())
    }
}

/// `true` when the taskbar (and so the tray) is dark, i.e. wants a light icon.
pub fn taskbar_is_dark() -> bool {
    #[cfg(windows)]
    {
        imp::taskbar_is_dark()
    }
    #[cfg(not(windows))]
    {
        true
    }
}

#[cfg(windows)]
mod imp {
    use std::io;

    use windows_registry::CURRENT_USER;

    use super::{APP_USER_MODEL_ID, DISPLAY_NAME};

    fn to_io(e: windows::core::Error) -> io::Error {
        io::Error::other(e)
    }

    fn identity_key() -> String {
        format!("Software\\Classes\\AppUserModelId\\{APP_USER_MODEL_ID}")
    }

    pub fn register_notification_identity() -> io::Result<()> {
        let key = CURRENT_USER.create(identity_key()).map_err(to_io)?;
        key.set_string("DisplayName", DISPLAY_NAME).map_err(to_io)?;
        // The toast icon must be an image file on disk; the .exe's icon
        // resource does not qualify, so write the artwork out once.
        if let Some(icon) = crate::paths::data_dir().map(|dir| dir.join(crate::paths::TOAST_ICON_FILE_NAME)) {
            let current = std::fs::read(&icon).ok();
            if current.as_deref() != Some(crate::icons::WINDOW_ICON_PNG) {
                if let Some(dir) = icon.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::write(&icon, crate::icons::WINDOW_ICON_PNG)?;
            }
            key.set_string("IconUri", icon.to_string_lossy().as_ref()).map_err(to_io)?;
        }
        Ok(())
    }

    pub fn unregister_notification_identity() -> io::Result<()> {
        // Nothing registered (a fresh profile, or already purged) is success.
        if CURRENT_USER.open(identity_key()).is_err() {
            return Ok(());
        }
        CURRENT_USER.remove_tree(identity_key()).map_err(to_io)
    }

    pub fn taskbar_is_dark() -> bool {
        let light = CURRENT_USER
            .open("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize")
            .and_then(|key| key.get_u32("SystemUsesLightTheme"));
        // Windows before 1903 has no light taskbar and no such value.
        !matches!(light, Ok(v) if v != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("esmail-shell-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_second_lock_on_the_same_directory_is_refused() {
        let dir = scratch("lock");
        // `acquire_in` stores its lock in a process-wide static, so hold the
        // first one by hand to keep this test independent of it.
        fs::create_dir_all(&dir).unwrap();
        let first = OpenOptions::new().create(true).write(true).truncate(false).open(dir.join(LOCK_FILE)).unwrap();
        first.try_lock().unwrap();
        assert_eq!(acquire_in(&dir), Instance::AlreadyRunning);
        drop(first);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_free_lock_is_taken_and_stale_requests_are_discarded() {
        let dir = scratch("stale");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(Request::Quit.file_name()), b"").unwrap();
        fs::write(dir.join(Request::Show.file_name()), b"").unwrap();
        assert_eq!(acquire_in(&dir), Instance::First);
        assert!(!dir.join(Request::Quit.file_name()).exists(), "a stale quit would close the new instance");
        assert!(!dir.join(Request::Show.file_name()).exists());
        let _ = fs::remove_dir_all(dir);
    }
}
