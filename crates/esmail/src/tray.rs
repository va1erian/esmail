//! Windows-only: system tray icon + menu (so the window can be minimized to
//! the tray instead of exiting the process) and toast notifications for new
//! mail. See PLAN.md §B10.
//!
//! This module owns all the platform glue; the decision logic it's driven
//! by (when to poll, whether an update is "new mail", what a toast should
//! say) lives in `notify.rs` and is unit tested there without needing any
//! of what's in this file.

use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

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
/// against -- `winrt_notification::Toast::POWERSHELL_APP_ID` is that crate's
/// own documented workaround for exactly this case (see its doc comment).
/// The toast still shows; Windows just attributes it to "Windows
/// PowerShell" (wrong icon, wrong name in Focus Assist settings) until
/// esMail ships an installer that registers a real AUMID + shortcut. Called
/// out as a known limitation in PLAN.md §B10, not silently accepted.
const APP_ID: &str = winrt_notification::Toast::POWERSHELL_APP_ID;

/// Show a new-mail toast. `title`/`body` are expected to already be
/// sanitized (see `notify::build_notification`) -- this function does not
/// re-sanitize, it only relies on `winrt-notification`'s own XML escaping
/// (`title()`/`text1()` run content through `xml::escape::escape_str_attribute`,
/// verified by reading that crate's source) to keep the content from
/// breaking the toast's markup.
///
/// Errors are logged, not propagated: a failed notification is not a reason
/// to disrupt anything else the app is doing, matching the "log, don't
/// crash the UI over it" treatment other best-effort I/O gets elsewhere
/// (e.g. `save_attachment` in main.rs).
pub fn show_new_mail_toast(title: &str, body: &str) {
    let result = winrt_notification::Toast::new(APP_ID)
        .title(title)
        .text1(body)
        .sound(Some(winrt_notification::Sound::Mail))
        .duration(winrt_notification::Duration::Short)
        .show();
    if let Err(e) = result {
        log::warn!("could not show new-mail toast: {e:?}");
    }
}
