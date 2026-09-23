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
//!   take it, drops a request file next to it (`show`, `open-account` for
//!   `--open-account`, or `quit` for the installer) and exits. The running
//!   instance polls for that file. The background listener raises the GUI the
//!   same way when its tray's "Show esMail" is clicked.
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Come to the front (an ordinary second launch, or the listener's tray).
    Show,
    /// Come to the front and select this account (`esmail --open-account`).
    OpenAccount(String),
    /// Exit cleanly (`esmail --quit`, which the installer uses before it
    /// replaces or removes the program files).
    Quit,
}

/// Every request file. The open-account one carries the account id as its
/// contents; the others are empty.
const REQUEST_FILES: [&str; 3] = ["quit.request", "show.request", "open-account.request"];

impl Request {
    fn file_name(&self) -> &'static str {
        match self {
            Request::Show => "show.request",
            Request::OpenAccount(_) => "open-account.request",
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

fn request_path(request: &Request) -> Option<PathBuf> {
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
            for file in REQUEST_FILES {
                let _ = fs::remove_file(dir.join(file));
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
pub fn send_request(request: &Request) -> io::Result<()> {
    let path = request_path(request).ok_or_else(|| io::Error::other("no data directory"))?;
    let payload = match request {
        Request::OpenAccount(account) => account.as_bytes(),
        Request::Show | Request::Quit => &[],
    };
    fs::write(path, payload)
}

/// The request a later launch left for this instance, if any, consuming it.
/// Cheap enough to call from the UI loop.
pub fn take_request() -> Option<Request> {
    crate::paths::data_dir().and_then(|dir| take_request_in(&dir))
}

fn take_request_in(dir: &std::path::Path) -> Option<Request> {
    for file in REQUEST_FILES {
        let path = dir.join(file);
        let Ok(payload) = fs::read_to_string(&path) else { continue };
        if fs::remove_file(&path).is_err() {
            continue;
        }
        return Some(match file {
            "quit.request" => Request::Quit,
            "open-account.request" => Request::OpenAccount(payload.trim().to_string()),
            _ => Request::Show,
        });
    }
    None
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
        for file in REQUEST_FILES {
            fs::write(dir.join(file), b"").unwrap();
        }
        assert_eq!(acquire_in(&dir), Instance::First);
        for file in REQUEST_FILES {
            assert!(!dir.join(file).exists(), "a stale {file} must not survive into the new instance");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn an_open_account_request_carries_its_account_id() {
        let dir = scratch("open-account");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(Request::OpenAccount(String::new()).file_name()), b"alice@example.com\n").unwrap();
        assert_eq!(take_request_in(&dir), Some(Request::OpenAccount("alice@example.com".into())));
        assert!(!dir.join("open-account.request").exists(), "the request is consumed once");
        let _ = fs::remove_dir_all(dir);
    }
}
