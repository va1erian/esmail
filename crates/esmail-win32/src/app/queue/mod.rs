//! Drafts and Outbox, from the main window's side: opening the two windows,
//! keeping their lists current, and doing what they ask.
//!
//! Both lists are tables of the cache database shared with the egui app, so a
//! draft saved there shows here and a message queued here is retried there. The
//! windows only show rows; the main window owns the messages behind them.

mod window;

use esmail::db::OutboxItem;
use esmail_win32::core_glue::{draft_rows, outbox_rows};
use xui_win32::prelude::*;

use super::{App, Msg, placement};
use window::{Init, QueueMsg};

/// Which list a window shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueKind {
    /// Messages being written, saved automatically.
    Drafts,
    /// Messages that failed to send and are tried again.
    Outbox,
}

/// What a queue window asks the main window for.
pub enum Request {
    /// Continue writing the draft, or edit the outbox message, with this row id.
    Open(i64),
    /// Try sending the outbox message with this row id now.
    Retry(i64),
    /// Discard the message with this row id (the window has already asked).
    Delete(i64),
    /// The window is gone.
    Closed,
}

/// The two windows while they are open, and the outbox rows they were made from.
#[derive(Default)]
pub struct Queues {
    drafts: Option<WindowHandle<QueueMsg>>,
    outbox: Option<WindowHandle<QueueMsg>>,
    outbox_items: Vec<OutboxItem>,
}

impl Queues {
    fn window(&self, kind: QueueKind) -> Option<&WindowHandle<QueueMsg>> {
        match kind {
            QueueKind::Drafts => self.drafts.as_ref(),
            QueueKind::Outbox => self.outbox.as_ref(),
        }
        .filter(|window| window.is_alive())
    }

    /// Tells whichever queue windows are open which theme to wear.
    pub fn set_theme(&self, theme: Theme, follow_system: bool) {
        for window in [&self.drafts, &self.outbox].into_iter().flatten() {
            let _ = window.send(QueueMsg::SetTheme(theme, follow_system));
        }
    }

    /// Some open window, for a screenshot.
    pub fn any_window(&self) -> Option<&WindowHandle<QueueMsg>> {
        self.drafts.as_ref().or(self.outbox.as_ref())
    }
}

impl App {
    /// File > Drafts... and File > Outbox...: opens the window (or brings the
    /// open one forward) and asks the cache for its rows.
    pub(super) fn show_queue(&mut self, ui: &Ui<Msg>, kind: QueueKind) {
        if let Some(window) = self.queues.window(kind) {
            placement::bring_forward(window.hwnd());
        } else {
            let init = Init { kind, host: ui.proxy(), follow_system_theme: self.theme == esmail_win32::core_glue::ThemeChoice::System, acrylic: self.acrylic };
            match window::open(ui, init) {
                Ok(handle) => match kind {
                    QueueKind::Drafts => self.queues.drafts = Some(handle),
                    QueueKind::Outbox => self.queues.outbox = Some(handle),
                },
                Err(error) => return self.banner(&format!("Could not open the window: {error}")),
            }
        }
        self.refresh_queue(kind);
    }

    /// Asks the cache for `kind`'s rows again, if its window is open.
    pub(super) fn refresh_queue(&self, kind: QueueKind) {
        if self.queues.window(kind).is_some() {
            match kind {
                QueueKind::Drafts => self.core.cache().list_drafts(),
                QueueKind::Outbox => self.core.cache().list_outbox(),
            }
        }
    }

    /// The cache listed the drafts.
    pub(super) fn drafts_listed(&mut self, drafts: &[esmail::db::DraftSummary]) {
        self.queue_pending = false;
        if let Some(window) = self.queues.window(QueueKind::Drafts) {
            let _ = window.send(QueueMsg::Show(draft_rows(drafts)));
        }
    }

    /// The cache listed the outbox.
    pub(super) fn outbox_listed(&mut self, items: Vec<OutboxItem>) {
        self.queue_pending = false;
        if let Some(window) = self.queues.window(QueueKind::Outbox) {
            let _ = window.send(QueueMsg::Show(outbox_rows(&items, self.core.accounts())));
        }
        self.queues.outbox_items = items;
    }

    /// A draft the cache loaded: continues it in a compose window that keeps
    /// saving into the same row.
    pub(super) fn draft_loaded(&mut self, ui: &Ui<Msg>, id: i64, mut state: esmail::compose::ComposeState) {
        state.draft_id = Some(id);
        self.start_compose(ui, state, true);
        if let Some(window) = self.queues.window(QueueKind::Drafts) {
            window.close();
        }
    }

    /// A queue window asked for something.
    pub(super) fn queue_request(&mut self, ui: &Ui<Msg>, kind: QueueKind, request: Request) {
        match (kind, request) {
            (QueueKind::Drafts, Request::Open(id)) => self.core.cache().load_draft(id),
            (QueueKind::Drafts, Request::Delete(id)) => {
                self.core.cache().delete_draft(id);
                self.refresh_queue(kind);
            }
            (QueueKind::Outbox, Request::Open(id)) => self.edit_queued(ui, id),
            (QueueKind::Outbox, Request::Retry(id)) => self.retry_queued(id),
            (QueueKind::Outbox, Request::Delete(id)) => {
                self.core.cache().delete_outbox(id);
                self.refresh_queue(kind);
            }
            (_, Request::Retry(_)) => {}
            (QueueKind::Drafts, Request::Closed) => self.queues.drafts = None,
            (QueueKind::Outbox, Request::Closed) => self.queues.outbox = None,
        }
    }

    /// Takes an outbox message back into a compose window to fix and resend. It
    /// leaves the outbox first, so the retry timer cannot send the old copy too.
    fn edit_queued(&mut self, ui: &Ui<Msg>, id: i64) {
        let Some(position) = self.queues.outbox_items.iter().position(|item| item.id == id) else { return };
        let item = self.queues.outbox_items.remove(position);
        self.core.cache().delete_outbox(id);
        self.start_compose(ui, item.compose.with_account(Some(item.account_id)), true);
        self.refresh_queue(QueueKind::Outbox);
    }

    /// Sends an outbox message now instead of when its back-off ends. Its
    /// outcome updates the row (sent: gone; failed: backed off, error shown).
    fn retry_queued(&mut self, id: i64) {
        let Some(item) = self.queues.outbox_items.iter().find(|item| item.id == id).cloned() else { return };
        self.set_status("Sending...");
        self.outbox_due(vec![item]);
    }
}
