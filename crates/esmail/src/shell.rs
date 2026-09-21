//! Integration with the Windows shell. On every other platform each function
//! here is a harmless no-op, so `main.rs` calls them without `cfg` attributes.
//!
//! * **Process identity.** The AppUserModelID ties together the taskbar
//!   button, the Start-menu shortcut the installer creates (which carries the
//!   same ID) and toast notifications. Without it, notifications are filed
//!   under "Windows PowerShell" and the taskbar cannot group the window with
//!   its pinned shortcut.
//! * **Notification identity.** A per-user registry entry that gives that ID a
//!   display name and icon, so toasts read "esMail" even for a portable copy
//!   that was never installed.
//! * **Single instance.** esMail lives in the tray, so launching it again (from
//!   the Start menu, say) must bring the existing window back rather than start
//!   a second process fighting over the same cache. The second launch pokes the
//!   first through a named event and exits.
//! * **Theme and DPI queries** for the tray icon.

use std::io;

/// Shared with `installer/esmail.iss` (`AppUserModelID`); a unit test checks
/// the two agree.
pub const APP_USER_MODEL_ID: &str = "io.github.va1erian.esmail";
/// Shared with `installer/esmail.iss` (`AppMutex`), which uses it to refuse to
/// install or uninstall over a running copy.
pub const SINGLE_INSTANCE_MUTEX: &str = "esMail.SingleInstance";
const SHOW_WINDOW_EVENT: &str = "esMail.ShowWindow";
const DISPLAY_NAME: &str = "esMail";

/// Whether this process is the one that owns the tray icon and the cache.
pub enum Instance {
    First(FirstInstance),
    /// Another esMail is already running and has been asked to show itself.
    AlreadyRunning,
}

/// Held for the life of the process; see [`FirstInstance::on_show_requested`].
pub struct FirstInstance {
    #[cfg(windows)]
    show_event: Option<imp::Handle>,
}

impl FirstInstance {
    /// Call `callback` (from a background thread) each time a later launch of
    /// esMail asks this one to come to the front.
    pub fn on_show_requested(&self, callback: impl Fn() + Send + 'static) {
        #[cfg(windows)]
        if let Some(event) = self.show_event {
            imp::spawn_show_listener(event, callback);
        }
        #[cfg(not(windows))]
        let _ = callback;
    }
}

/// Claim the single-instance lock, or -- if another copy holds it -- ask that
/// copy to show its window.
pub fn acquire_single_instance() -> Instance {
    #[cfg(windows)]
    {
        imp::acquire_single_instance()
    }
    #[cfg(not(windows))]
    {
        Instance::First(FirstInstance {})
    }
}

/// Tell Windows which application this process is (see the module docs).
pub fn set_process_identity() {
    #[cfg(windows)]
    imp::set_process_identity();
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

/// The size in pixels of a small icon at the current DPI (16 at 100%).
pub fn small_icon_px() -> u32 {
    #[cfg(windows)]
    {
        imp::small_icon_px()
    }
    #[cfg(not(windows))]
    {
        16
    }
}

#[cfg(windows)]
mod imp {
    use std::io;
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, GetLastError, HANDLE, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_DWORD, RegDeleteTreeW, RegGetValueW, RegSetKeyValueW,
    };
    use windows_sys::Win32::System::Threading::{
        CreateEventW, CreateMutexW, INFINITE, SetEvent, WaitForSingleObject,
    };
    use windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
    use windows_sys::Win32::UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow, GetSystemMetrics, SM_CXSMICON};

    use super::{APP_USER_MODEL_ID, DISPLAY_NAME, FirstInstance, Instance, SHOW_WINDOW_EVENT, SINGLE_INSTANCE_MUTEX};

    /// A kernel handle that lives until the process exits. `HANDLE` is a raw
    /// pointer (not `Send`); this wrapper is, because a handle value is just an
    /// index into the process's handle table.
    #[derive(Clone, Copy)]
    pub struct Handle(HANDLE);
    unsafe impl Send for Handle {}

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn acquire_single_instance() -> Instance {
        let event_name = wide(SHOW_WINDOW_EVENT);
        // The event first, then the mutex: whoever finds the mutex taken can
        // then rely on the event existing.
        let event = unsafe { CreateEventW(null(), 0, 0, event_name.as_ptr()) };

        let mutex_name = wide(SINGLE_INSTANCE_MUTEX);
        let mutex = unsafe { CreateMutexW(null(), 0, mutex_name.as_ptr()) };
        let already_running = !mutex.is_null() && unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;

        if already_running {
            // Let the running copy take the foreground: this process was just
            // started by the user and so may hand that right on.
            unsafe {
                AllowSetForegroundWindow(ASFW_ANY);
                SetEvent(event);
            }
            return Instance::AlreadyRunning;
        }
        // The mutex handle is deliberately never closed: it is released when
        // the process exits, which is exactly the lifetime of the lock.
        Instance::First(FirstInstance { show_event: (!event.is_null()).then_some(Handle(event)) })
    }

    pub fn spawn_show_listener(event: Handle, callback: impl Fn() + Send + 'static) {
        std::thread::Builder::new()
            .name("esmail-show-listener".into())
            .spawn(move || {
                let event = event;
                while unsafe { WaitForSingleObject(event.0, INFINITE) } == WAIT_OBJECT_0 {
                    callback();
                }
            })
            .ok();
    }

    pub fn set_process_identity() {
        let id = wide(APP_USER_MODEL_ID);
        unsafe {
            SetCurrentProcessExplicitAppUserModelID(id.as_ptr());
        }
    }

    fn registry_key() -> Vec<u16> {
        wide(&format!("Software\\Classes\\AppUserModelId\\{APP_USER_MODEL_ID}"))
    }

    fn set_string(subkey: &[u16], name: &str, value: &str) -> io::Result<()> {
        let name = wide(name);
        let value = wide(value);
        let status = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                name.as_ptr(),
                REG_SZ,
                value.as_ptr().cast(),
                (value.len() * 2) as u32,
            )
        };
        if status == ERROR_SUCCESS { Ok(()) } else { Err(io::Error::from_raw_os_error(status as i32)) }
    }

    pub fn register_notification_identity() -> io::Result<()> {
        let key = registry_key();
        set_string(&key, "DisplayName", DISPLAY_NAME)?;
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
            set_string(&key, "IconUri", &icon.to_string_lossy())?;
        }
        Ok(())
    }

    pub fn unregister_notification_identity() -> io::Result<()> {
        let key = registry_key();
        let status = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, key.as_ptr()) };
        if status == ERROR_SUCCESS || status == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(status as i32))
        }
    }

    pub fn taskbar_is_dark() -> bool {
        let key = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
        let name = wide("SystemUsesLightTheme");
        let mut value: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_DWORD,
                null_mut(),
                (&mut value as *mut u32).cast(),
                &mut size,
            )
        };
        // Windows before 1903 has no light taskbar and no such value.
        status != ERROR_SUCCESS || value == 0
    }

    pub fn small_icon_px() -> u32 {
        let px = unsafe { GetSystemMetrics(SM_CXSMICON) };
        if px > 0 { px as u32 } else { 16 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installer_uses_the_same_identifiers() {
        let script = include_str!("../../../installer/esmail.iss");
        assert!(script.contains(&format!("AppUserModelID: \"{APP_USER_MODEL_ID}\"")), "AppUserModelID differs");
        assert!(script.contains(&format!("AppMutex={SINGLE_INSTANCE_MUTEX}")), "AppMutex differs");
    }

    #[cfg(windows)]
    #[test]
    fn small_icon_size_is_sane() {
        assert!((16..=64).contains(&small_icon_px()));
    }
}
