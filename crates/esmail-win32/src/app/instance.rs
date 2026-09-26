//! One window per data directory: a second launch hands over to the first.
//!
//! The lock is `esmail::shell`'s, the same one the egui frontend takes, so the
//! two frontends cannot run over one cache either. A second launch leaves its
//! request in files (`show.request`, and `compose.request` for `--compose`) and
//! signals a named event; the first instance has a thread blocked on that event
//! and forwards what it finds to the window. Nothing polls, so a window hidden
//! in the tray costs no wakeups.

use std::ffi::c_void;
use std::io;

use esmail::shell::{self, Instance, Request};
use esmail_win32::core_glue::resident;
use xui_win32::Proxy;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading::{CreateEventW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, SetEvent, WaitForSingleObject};
use windows::Win32::UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow};
use windows::core::HSTRING;

use super::Msg;

/// What the tray, a toast click or a second launch asks the window to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Launch {
    /// Come to the front.
    Show,
    /// Come to the front and open this account's inbox.
    OpenAccount(String),
    /// Come to the front with a new message open.
    Compose,
    /// Exit, whatever the close box would do.
    Quit,
}

/// The event a second launch signals, named after the data directory so
/// isolated profiles do not wake each other.
fn event_name(dir: &std::path::Path) -> HSTRING {
    let hash = dir.to_string_lossy().bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3));
    HSTRING::from(format!("Local\\esmail-win32-wake-{hash:016x}"))
}

/// The event this instance waits on, created as soon as the lock is held so a
/// second launch never finds the lock without it.
pub struct Waiting(HANDLE);

/// How the single-instance claim came out.
pub enum Claim {
    /// This is the first instance. It has to forward later launches, unless
    /// there is no data directory to lock (then nothing can reach it).
    First(Option<Waiting>),
    /// Another instance holds the lock and was asked to come forward.
    HandedOver,
}

/// Takes the single-instance lock, or hands over to the instance that has it
/// (with `compose` when this launch asked for a new message).
pub fn claim(compose: bool) -> io::Result<Claim> {
    let Some(dir) = esmail::paths::data_dir() else { return Ok(Claim::First(None)) };
    if shell::acquire_single_instance() == Instance::First {
        resident::discard_compose_request(&dir);
        // SAFETY: plain FFI with a valid name; the handle lives as long as the process.
        let event = unsafe { CreateEventW(None, false, false, &event_name(&dir)) }.map_err(io::Error::other)?;
        return Ok(Claim::First(Some(Waiting(event))));
    }
    if compose {
        resident::send_compose_request(&dir)?;
    }
    shell::send_request(&Request::Show)?;
    // SAFETY: as above; the event is only opened to be signalled, then closed.
    unsafe {
        // The first instance may bring its window forward for us.
        let _ = AllowSetForegroundWindow(ASFW_ANY);
        let event = OpenEventW(EVENT_MODIFY_STATE, false, &event_name(&dir)).map_err(io::Error::other)?;
        let signalled = SetEvent(event);
        let _ = CloseHandle(event);
        signalled.map_err(io::Error::other)?;
    }
    Ok(Claim::HandedOver)
}

/// `--quit`: ask a running instance to exit. Used by the installer before it
/// replaces or removes the program files. Either frontend may hold the lock;
/// the egui one polls the request file, this one waits on its own event, so
/// both are woken.
pub fn request_quit() {
    if shell::acquire_single_instance() != Instance::AlreadyRunning {
        return;
    }
    let _ = shell::send_request(&Request::Quit);
    let Some(dir) = esmail::paths::data_dir() else { return };
    // SAFETY: plain FFI; the event is only opened to be signalled, then closed,
    // and it is absent when the running instance is the egui frontend.
    unsafe {
        if let Ok(event) = OpenEventW(EVENT_MODIFY_STATE, false, &event_name(&dir)) {
            let _ = SetEvent(event);
            let _ = CloseHandle(event);
        }
    }
}

impl Waiting {
    /// Forwards what later launches leave to the window: everything already left
    /// (a launch that raced the startup), then whatever each wake brings.
    pub fn forward(self, proxy: Proxy<Msg>) {
        let event = self.0.0 as usize;
        std::thread::Builder::new()
            .name("esmail-instance".into())
            .spawn(move || {
                let event = HANDLE(event as *mut c_void);
                while drain(&proxy) {
                    // SAFETY: `event` is the handle `claim` created, open for the process's life.
                    unsafe { WaitForSingleObject(event, INFINITE) };
                }
            })
            .expect("spawn the instance thread");
    }
}

/// Sends every pending request to the window. `false` once the window is gone.
fn drain(proxy: &Proxy<Msg>) -> bool {
    let Some(dir) = esmail::paths::data_dir() else { return true };
    let mut launches = Vec::new();
    while let Some(request) = shell::take_request() {
        launches.push(match request {
            Request::Show => Launch::Show,
            Request::OpenAccount(id) => Launch::OpenAccount(id),
            Request::Quit => Launch::Quit,
        });
    }
    if resident::take_compose_request(&dir) {
        launches.push(Launch::Compose);
    }
    launches.into_iter().all(|launch| proxy.send(Msg::Launch(launch)).is_ok())
}
