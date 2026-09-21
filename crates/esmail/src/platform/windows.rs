//! The Windows implementation of [`super`]: a system tray icon + menu (so the
//! window can be minimized to the tray instead of exiting the process) and
//! toast notifications for new mail, called through the WinRT toast API.
//! See PLAN.md §B10.
//!
//! This file owns all the Windows glue; the decision logic it is driven by
//! (when to poll, whether an update is "new mail", what a toast should say,
//! the toast XML and click arguments) lives in `notify.rs` and is unit tested
//! there without needing any of what is in this file.

use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows::Data::Xml::Dom::XmlDocument;
use windows::Foundation::TypedEventHandler;
use windows::UI::Notifications::{ToastActivatedEventArgs, ToastNotification, ToastNotificationManager};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
use windows::core::{HSTRING, IInspectable, Interface};

/// Owns the tray icon and its menu for the lifetime of the app. Dropping
/// this removes the icon from the tray, so it's held in `EsMailApp` for as
/// long as the process runs.
pub struct TrayState {
    // Never read directly again after construction, but must stay alive:
    // dropping a `TrayIcon` removes it from the shell's notification area.
    _tray_icon: TrayIcon,
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
            .with_icon(placeholder_icon()?)
            .build()?;

        Ok(Self { _tray_icon: tray_icon, show_id, quit_id, unread: None })
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
        if let Err(e) = self._tray_icon.set_tooltip(Some(tooltip)) {
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

/// A plain solid-colour 16x16 icon, generated at run time rather than
/// shipped as a separate asset file. esMail has no branding yet, and a
/// baked-in pixel buffer keeps the tray icon self-contained in source
/// instead of adding a binary asset nothing else in the build would use.
fn placeholder_icon() -> anyhow::Result<Icon> {
    const SIZE: u32 = 16;
    const RGBA: [u8; 4] = [0x2b, 0x6c, 0xb0, 0xff]; // opaque, unremarkable blue
    let pixels: Vec<u8> = RGBA
        .iter()
        .copied()
        .cycle()
        .take((SIZE * SIZE * 4) as usize)
        .collect();
    Icon::from_rgba(pixels, SIZE, SIZE).map_err(|e| anyhow::anyhow!("tray icon: {e}"))
}

/// The AppUserModelID toast notifications are shown under. esMail has no
/// installer and therefore no Start-menu shortcut to register a real one
/// against, so this borrows Windows PowerShell's, the same workaround
/// `winrt-notification` documented for exactly this case. The toast still
/// shows; Windows just attributes it to "Windows PowerShell" (wrong icon,
/// wrong name in Focus Assist settings) until esMail ships an installer that
/// registers a real AUMID + shortcut. Called out as a known limitation in
/// PLAN.md §B10, not silently accepted.
const APP_ID: &str = r"{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\WindowsPowerShell\v1.0\powershell.exe";

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

thread_local! {
    /// Whether this thread has joined a COM apartment yet. The threads that
    /// show toasts are tokio worker threads, which nothing else initializes.
    static COM_READY: Cell<bool> = const { Cell::new(false) };
}

/// Join the multithreaded apartment on this thread, once. Failing because the
/// thread already joined a different apartment is fine: the WinRT calls that
/// follow work either way.
fn ensure_com() {
    COM_READY.with(|ready| {
        if !ready.get() {
            // SAFETY: a plain COM initialisation call with no pointers.
            let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
            ready.set(true);
        }
    });
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
    if let Err(e) = try_show_new_mail_toast(account_id, title, body) {
        log::warn!("could not show new-mail toast: {e}");
    }
}

fn try_show_new_mail_toast(account_id: &str, title: &str, body: &str) -> windows::core::Result<()> {
    ensure_com();
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

    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_ID))?.Show(&toast)?;

    if let Ok(mut shown) = SHOWN_TOASTS.lock() {
        shown.push_back(toast);
        while shown.len() > KEEP_TOASTS {
            shown.pop_front();
        }
    }
    Ok(())
}
