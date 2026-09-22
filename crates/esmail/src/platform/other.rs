//! The implementation of [`super`] for platforms without tray or toast
//! support here (B10 is Windows-only, see PLAN.md §B10): no tray icon, and a
//! new-mail "toast" is a log line. The same items as `windows.rs`, so nothing
//! outside `platform` has to know which one it got.

/// What the user asked for through the tray icon or its menu.
pub enum TrayAction {
    /// Bring the main window back.
    Show,
    /// Actually exit, as opposed to closing the window.
    Quit,
}

/// There is no tray icon here: [`TrayState::new`] always fails, and the app
/// runs as if the tray were unavailable (closing the window exits).
pub struct TrayState;

impl TrayState {
    pub fn new() -> anyhow::Result<Self> {
        Err(anyhow::anyhow!("no system tray support on this platform"))
    }

    pub fn poll_actions(&self) -> Vec<TrayAction> {
        Vec::new()
    }

    pub fn set_unread(&mut self, _total: u32) {}

    pub fn refresh_icon(&mut self) {}
}

/// Log the notification; there is no toast to show.
pub fn show_new_mail_toast(account_id: &str, title: &str, body: &str) {
    log::info!("new mail ({account_id}): {title} -- {body} (desktop notifications are Windows-only, see PLAN.md §B10)");
}

/// Nothing to record: there are no toasts to attribute.
pub fn use_own_notification_identity(_registered: bool) {}

/// Toasts are never shown here, so they are never clicked.
pub fn set_toast_click_handler(_handler: impl Fn(String) + Send + Sync + 'static) {}

/// The background-repaint throttle this works around (see `windows.rs`) is
/// Windows-specific; nothing to opt out of elsewhere.
pub fn disable_background_throttling() {}
