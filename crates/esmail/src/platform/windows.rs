//! The Windows implementation of [`super`]: a system tray icon + menu (so the
//! window can be minimized to the tray instead of exiting the process) and
//! toast notifications for new mail, called through the WinRT toast API.
//! See PLAN.md §B10.
//!
//! This file owns all the Windows glue; the decision logic it is driven by
//! (when to poll, whether an update is "new mail", what a toast should say,
//! the toast XML and click arguments) lives in `notify.rs` and is unit tested
//! there without needing any of what is in this file.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows::Data::Xml::Dom::XmlDocument;
use windows::Foundation::TypedEventHandler;
use windows::UI::Notifications::{ToastActivatedEventArgs, ToastNotification, ToastNotificationManager};
use windows::System::Threading::{ThreadPool, WorkItemHandler};
use windows::Win32::System::Threading::{
    GetCurrentProcess, PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
    ProcessPowerThrottling, SetProcessInformation,
};
use windows::core::{HSTRING, IInspectable, Interface};

use crate::{icons, shell};

/// Owns the tray icon and its menu for the lifetime of the app. Dropping
/// this removes the icon from the tray, so it's held in `EsMailApp` for as
/// long as the process runs.
pub struct TrayState {
    // Never read directly again after construction, but must stay alive:
    // dropping a `TrayIcon` removes it from the shell's notification area.
    tray_icon: TrayIcon,
    /// Whether the icon shown is the light-glyph variant (for a dark taskbar),
    /// and when the system theme was last looked at; see `refresh_icon`.
    light_glyph: bool,
    theme_checked: Instant,
    show_id: MenuId,
    quit_id: MenuId,
    /// Last unread total put in the tooltip; see `set_unread`.
    unread: Option<u32>,
}

/// What the user asked for via the tray icon or its menu, decoded from the
/// raw `tray-icon`/`muda` event types so the caller (`main.rs`) doesn't need
/// to know those crates' event shapes.
pub enum TrayAction {
    /// Bring the main window back (left-click on the icon, or "Show esMail"
    /// in the menu).
    Show,
    /// "Quit" was clicked: actually exit the process, as opposed to the
    /// window-close path, which only hides it. See `EsMailApp`'s handling
    /// of `ViewportEvent`/`exit_requested` in main.rs.
    Quit,
}

impl TrayState {
    /// Build the tray icon and its menu. Fails (rather than panicking) if
    /// the shell's tray API is unavailable for some reason -- the caller
    /// treats that as "no tray this session" and leaves window-close
    /// behaving normally, rather than stranding the user with a hidden
    /// window and no way to bring it back.
    pub fn new() -> anyhow::Result<Self> {
        let light_glyph = shell::taskbar_is_dark();
        let menu = Menu::new();
        let show_item = MenuItem::new("Show esMail", true, None);
        let quit_item = MenuItem::new("Quit", true, None);
        let show_id = show_item.id().clone();
        let quit_id = quit_item.id().clone();
        menu.append(&show_item)?;
        menu.append(&quit_item)?;

        let tray_icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("esMail")
            .with_icon(themed_icon(light_glyph)?)
            .build()?;

        Ok(Self { tray_icon, light_glyph, theme_checked: Instant::now(), show_id, quit_id, unread: None })
    }

    /// Swap the icon if the taskbar went from light to dark (or back) since it
    /// was drawn. Looks at the system setting at most every couple of seconds,
    /// so it is fine to call every frame.
    pub fn refresh_icon(&mut self) {
        const CHECK_EVERY: Duration = Duration::from_secs(2);
        if self.theme_checked.elapsed() < CHECK_EVERY {
            return;
        }
        self.theme_checked = Instant::now();
        let light_glyph = shell::taskbar_is_dark();
        if light_glyph == self.light_glyph {
            return;
        }
        match themed_icon(light_glyph) {
            Ok(icon) => {
                if self.tray_icon.set_icon(Some(icon)).is_ok() {
                    self.light_glyph = light_glyph;
                }
            }
            Err(e) => log::warn!("could not redraw the tray icon: {e}"),
        }
    }

    /// Show the unread total (summed over every account by the caller) in the
    /// icon's tooltip. Only touches the shell when the number changed, since
    /// this is called every frame.
    pub fn set_unread(&mut self, total: u32) {
        if self.unread == Some(total) {
            return;
        }
        self.unread = Some(total);
        let tooltip = if total == 0 { "esMail".to_string() } else { format!("esMail \u{2014} {total} unread") };
        if let Err(e) = self.tray_icon.set_tooltip(Some(tooltip)) {
            log::warn!("could not update the tray tooltip: {e}");
        }
    }

    /// Drain every tray-icon-click and menu-click event queued since the
    /// last call. `tray-icon`/`muda` deliver events via global channels
    /// (`TrayIconEvent::receiver()`/`MenuEvent::receiver()`), not anything
    /// owned by this struct, so calling this from nowhere is harmless and
    /// calling it from two places would just split the events between
    /// callers -- only `EsMailApp::logic` does, once per invocation.
    pub fn poll_actions(&self) -> Vec<TrayAction> {
        let mut actions = Vec::new();

        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            // Left click fires both a `Down` and an `Up` event; only react
            // to `Up` (mirroring how a normal button click registers on
            // release) so one click doesn't queue two `Show` actions.
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                actions.push(TrayAction::Show);
            }
        }

        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == self.show_id {
                actions.push(TrayAction::Show);
            } else if event.id == self.quit_id {
                actions.push(TrayAction::Quit);
            }
        }

        actions
    }
}

/// The tray artwork (see `icons.rs`), in the glyph colour that shows up on the
/// current taskbar.
fn themed_icon(light_glyph: bool) -> anyhow::Result<Icon> {
    let art = icons::tray_icon(light_glyph)
        .ok_or_else(|| anyhow::anyhow!("the embedded tray icon does not decode"))?;
    Icon::from_rgba(art.pixels, art.width, art.height).map_err(|e| anyhow::anyhow!("tray icon: {e}"))
}

/// Opts this process out of Windows' "EXECUTION_SPEED" power throttling,
/// which the OS applies to a process none of whose windows currently has
/// focus -- coalescing its timers and delaying delivery of already-scheduled
/// window redraws by anywhere from a few seconds to, observed while chasing
/// #34's stuck "Sending…" spinner, well over a minute. That throttle is what
/// made a compose window's own repaint requests (and the periodic tray-tick
/// chain in `main.rs`'s `handle_tray`) go unheard once a second esMail
/// window took it out of focus, even though every request was correctly
/// targeted at the root viewport and returned immediately.
///
/// Called once at startup (see `main.rs`). Best-effort: if it fails (an
/// unsupported Windows version, say), esMail is simply subject to the normal
/// throttle again, same as before this existed -- the periodic heartbeat
/// thread in `main.rs` still bounds the worst case, just to a a longer one.
pub fn disable_background_throttling() {
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: 1,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        StateMask: 0, // 0 = do not throttle, for every bit named in ControlMask.
    };
    // Safety: `state` is a plain, fully-initialized `#[repr(C)]` struct kept
    // alive for the whole call, and its size matches what `ProcessInformation`
    // expects for `ProcessPowerThrottling` (see the Win32 docs for
    // `SetProcessInformation`/`PROCESS_POWER_THROTTLING_STATE`).
    let result = unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            std::ptr::from_ref(&state).cast(),
            u32::try_from(size_of::<PROCESS_POWER_THROTTLING_STATE>()).expect("struct size fits u32"),
        )
    };
    if let Err(e) = result {
        log::warn!("could not opt out of background power throttling: {e}");
    }
}

/// Windows PowerShell's AppUserModelID, which Windows always knows. Toasts
/// shown under it are attributed to "Windows PowerShell"; it is only the
/// fallback for when esMail's own identity could not be registered.
const FALLBACK_APP_ID: &str = r"{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\WindowsPowerShell\v1.0\powershell.exe";

/// Set once `shell::register_notification_identity` has succeeded. Windows
/// shows nothing at all for a toast whose AppUserModelID it has never heard
/// of, so until then (or if registering failed) toasts use [`FALLBACK_APP_ID`].
static OWN_IDENTITY_REGISTERED: AtomicBool = AtomicBool::new(false);

/// Record whether registering esMail's own notification identity worked; if it
/// did, toasts are attributed to "esMail" with its icon.
pub fn use_own_notification_identity(registered: bool) {
    OWN_IDENTITY_REGISTERED.store(registered, Ordering::Relaxed);
}

fn app_id() -> &'static str {
    if OWN_IDENTITY_REGISTERED.load(Ordering::Relaxed) { shell::APP_USER_MODEL_ID } else { FALLBACK_APP_ID }
}

/// How many shown toasts to keep referenced. A toast's `Activated` handler is
/// tied to the `ToastNotification` object; keeping the recent ones alive means
/// a click on any toast still in the notification centre finds its handler.
const KEEP_TOASTS: usize = 32;

/// Called when a toast is clicked, with the account id it was shown for. Set
/// once at startup by [`set_toast_click_handler`].
type ClickHandler = Box<dyn Fn(String) + Send + Sync>;
static CLICK_HANDLER: OnceLock<ClickHandler> = OnceLock::new();
static SHOWN_TOASTS: Mutex<VecDeque<ToastNotification>> = Mutex::new(VecDeque::new());

/// Register what happens when a new-mail toast is clicked. The handler runs on
/// a WinRT thread-pool thread, not the UI thread, so it must only hand the
/// account id over (a channel send, a repaint request). Only the first call
/// has any effect.
pub fn set_toast_click_handler(handler: impl Fn(String) + Send + Sync + 'static) {
    let _ = CLICK_HANDLER.set(Box::new(handler));
}

/// Show a new-mail toast for `account_id`. `title`/`body` are expected to
/// already be sanitized (see `notify::build_account_notification`); the XML
/// they go into is escaped by `notify::toast_xml`.
///
/// Clicking the toast calls the handler from [`set_toast_click_handler`] with
/// `account_id`, so the app can open that account's mailbox.
///
/// Errors are logged, not propagated: a failed notification is not a reason
/// to disrupt anything else the app is doing, matching the "log, don't
/// crash the UI over it" treatment other best-effort I/O gets elsewhere
/// (e.g. `save_attachment` in main.rs).
pub fn show_new_mail_toast(account_id: &str, title: &str, body: &str) {
    // Run on the WinRT thread pool, whose threads are already in the COM
    // multithreaded apartment the toast API needs. The callers are tokio worker
    // threads that nothing else initializes, and joining an apartment by hand
    // (`RoInitialize`) is an `unsafe` call; this needs none. The work item is
    // fire-and-forget: it logs its own failure.
    let (account_id, title, body) = (account_id.to_owned(), title.to_owned(), body.to_owned());
    let queued = ThreadPool::RunAsync(&WorkItemHandler::new(move |_| {
        if let Err(e) = try_show_new_mail_toast(&account_id, &title, &body) {
            log::warn!("could not show new-mail toast: {e}");
        }
        Ok(())
    }));
    if let Err(e) = queued {
        log::warn!("could not queue the new-mail toast: {e}");
    }
}

fn try_show_new_mail_toast(account_id: &str, title: &str, body: &str) -> windows::core::Result<()> {
    let xml = XmlDocument::new()?;
    xml.LoadXml(&HSTRING::from(crate::notify::toast_xml(title, body, account_id)))?;
    let toast = ToastNotification::CreateToastNotification(&xml)?;

    toast.Activated(&TypedEventHandler::new(|_toast, args: windows::core::Ref<'_, IInspectable>| {
        // The launch arguments ride on the activation event of a toast that
        // was clicked (as opposed to one of its buttons, which we have none of).
        let arguments = args
            .as_ref()
            .and_then(|args| args.cast::<ToastActivatedEventArgs>().ok())
            .and_then(|args| args.Arguments().ok())
            .map(|arguments| arguments.to_string());
        match (arguments.as_deref().and_then(crate::notify::account_from_launch_arguments), CLICK_HANDLER.get()) {
            (Some(account), Some(handler)) => handler(account),
            _ => log::debug!("a toast was clicked but names no account, or no handler is set"),
        }
        Ok(())
    }))?;

    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(app_id()))?.Show(&toast)?;

    if let Ok(mut shown) = SHOWN_TOASTS.lock() {
        shown.push_back(toast);
        while shown.len() > KEEP_TOASTS {
            shown.pop_front();
        }
    }
    Ok(())
}
