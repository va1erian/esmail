//! Compose windows: one native window per message.
//!
//! Each [`ComposeWindow`] is an egui *deferred* viewport (a real OS window
//! with its own taskbar entry, title bar and place on any monitor), drawn
//! from state it shares with the app behind an `Arc<Mutex<..>>`. Deferred
//! rather than immediate on purpose: an immediate viewport is drawn inside
//! the main window's frame, and eframe runs no frame for the main window
//! while it is minimized, hidden in the tray or covered by another window --
//! which would freeze every compose window along with it. A deferred one
//! repaints on its own.
//!
//! The window never touches the rest of the app. Its buttons set flags in the
//! shared state (`send_requested`, `finished`) and wake the main viewport,
//! whose `logic()` picks them up and issues the actual SMTP send.
//!
//! The result comes back the other way without waiting on the main window,
//! on purpose: on Windows, a window with no focus -- which, once a compose
//! window is open, includes the main window unless the user deliberately
//! clicks back onto it -- can have its already-scheduled repaints delayed by
//! Windows' background power throttling for a long time (`main.rs`'s
//! `platform::disable_background_throttling` turns this off for the whole
//! process, but that alone wasn't enough in practice to make it prompt).
//! Rather than have a successful send depend on the user going back to the
//! main window, `EsMailApp`'s SMTP forwarder task calls
//! [`ComposeWindow::mark_sent_and_hide`] / [`ComposeWindow::set_error_and_wake`]
//! *directly* from its own background thread, using the copy of this window
//! kept in `EsMailApp::compose_registry` for exactly this. Those hide the OS
//! window (or show the error) on this window's own next pass -- independent
//! of the main window, and normally immediate, since this is the window the
//! Send click just landed in. The main window's `logic()` still does the
//! bookkeeping (dropping the entry from `EsMailApp::compose_windows`, which
//! is what actually destroys the OS window: egui destroys a deferred
//! viewport the parent stops showing) whenever it next runs, since that part
//! isn't user-visible and so doesn't need to be prompt.

use super::*;
use esmail::contacts::Contacts;
use esmail::waker::Waker;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// Where the caret goes when a window first opens.
#[derive(Clone, Copy)]
pub(super) enum Focus {
    /// A new message: the recipient.
    To,
    /// A reply or forward: the text, ready to type above the quote.
    Body,
}

/// The state the window and the app share.
pub(super) struct Shared {
    pub(super) state: ComposeState,
    /// What the window opened with; `state != initial` means there is
    /// something to lose ([`ComposeWindow::is_dirty`]).
    pub(super) initial: ComposeState,
    /// `(account id, label)` for the From selector, refreshed by the app
    /// every frame it draws the main window.
    pub(super) accounts: Vec<(String, String)>,
    /// Recipient autocomplete candidates, refreshed by the app the same way
    /// `accounts` is (see [`Contacts`]).
    pub(super) contacts: Arc<Contacts>,
    /// Autocomplete state for whichever recipient field has focus; only one
    /// can, so there is one of these rather than one per field.
    pub(super) suggest: compose_ui::SuggestState,
    /// The last thing that went wrong (a failed send, an unreadable
    /// attachment); shown in red until the next Send.
    pub(super) error: Option<String>,
    /// A send is in flight: the form is locked and Send is disabled.
    pub(super) sending: bool,
    /// Send was clicked; the app takes this in `logic()`.
    pub(super) send_requested: bool,
    /// The user chose to be done with the message (Discard, or confirmed
    /// closing the OS window): the app drops the window.
    pub(super) finished: bool,
    /// The "discard this unsent message?" question is showing in place of the
    /// buttons.
    pub(super) confirm_discard: bool,
    pub(super) focus: Option<Focus>,
}

/// Cheap to clone (an id plus an `Arc`): a copy lives in `EsMailApp`'s
/// `compose_registry` so the SMTP forwarder task can reach a specific
/// window's state directly from its own background thread -- see
/// [`Self::mark_sent_and_hide`] for why that matters.
#[derive(Clone)]
pub(super) struct ComposeWindow {
    id: ComposeId,
    shared: Arc<Mutex<Shared>>,
    /// Wakes the main window, the only one that acts on `send_requested` and
    /// `finished`.
    wake_app: Waker,
    /// Wakes this window's own viewport. Built by whoever owns the egui
    /// context so this type needs no viewport addressing of its own: the
    /// core's `Waker` is deliberately viewport-agnostic.
    wake_window: Waker,
}

impl ComposeWindow {
    pub(super) fn new(id: ComposeId, state: ComposeState, focus: Focus, wake_app: Waker, wake_window: Waker) -> Self {
        let shared = Shared {
            initial: state.clone(),
            state,
            accounts: Vec::new(),
            contacts: Arc::new(Contacts::default()),
            suggest: compose_ui::SuggestState::default(),
            error: None,
            sending: false,
            send_requested: false,
            finished: false,
            confirm_discard: false,
            focus: Some(focus),
        };
        Self { id, shared: Arc::new(Mutex::new(shared)), wake_app, wake_window }
    }

    pub(super) fn id(&self) -> ComposeId {
        self.id
    }

    pub(super) fn viewport_id_of(id: ComposeId) -> egui::ViewportId {
        egui::ViewportId::from_hash_of(("esmail-compose", id))
    }

    pub(super) fn viewport_id(&self) -> egui::ViewportId {
        Self::viewport_id_of(self.id)
    }

    /// Wakes this window's own viewport.
    pub(super) fn wake(&self) {
        (self.wake_window)();
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        // The state is plain data, so a panic elsewhere while it was held
        // leaves nothing half-updated worth refusing to read.
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether closing this window now would throw away typing -- or a send
    /// that hasn't come back yet, which is just as much a reason to ask
    /// first (there is no cancelling it once it's in flight; see `smtp.rs`).
    /// An unedited Reply/Forward mid-send (`state == initial`) is exactly
    /// the case this exists for: `state` alone would say there is nothing
    /// to lose.
    pub(super) fn is_dirty(&self) -> bool {
        let s = self.lock();
        s.state != s.initial || s.sending
    }

    /// The message, if Send was clicked since the last call.
    pub(super) fn take_send_request(&self) -> Option<ComposeState> {
        let mut s = self.lock();
        if std::mem::take(&mut s.send_requested) {
            Some(s.state.clone())
        } else {
            None
        }
    }

    /// The user is done with the message (Discard, or a confirmed close).
    pub(super) fn is_finished(&self) -> bool {
        self.lock().finished
    }

    /// The account the message is to be sent from.
    pub(super) fn account_id(&self) -> Option<String> {
        self.lock().state.account_id.clone()
    }

    /// A read-only copy of the form's current contents, for autosave --
    /// unlike [`Self::take_send_request`], this doesn't consume anything and
    /// can be called on any frame regardless of whether Send was clicked.
    pub(super) fn snapshot(&self) -> ComposeState {
        self.lock().state.clone()
    }

    /// Records which `drafts` row this window autosaves into from now on.
    /// Updates `initial` to match, so this bookkeeping-only change doesn't
    /// itself make [`Self::is_dirty`] true.
    pub(super) fn set_draft_id(&self, id: i64) {
        let mut s = self.lock();
        s.state.draft_id = Some(id);
        s.initial.draft_id = Some(id);
    }

    /// Locks or unlocks the form for a send in flight; starting one clears the
    /// previous error.
    pub(super) fn set_sending(&self, sending: bool) {
        let mut s = self.lock();
        s.sending = sending;
        if sending {
            s.error = None;
        }
    }

    /// Reports a problem in this window (and only this one) and unlocks it.
    pub(super) fn set_error(&self, error: String) {
        let mut s = self.lock();
        s.sending = false;
        s.error = Some(error);
    }

    /// Marks the message sent and hides the OS window immediately, called
    /// directly from the SMTP forwarder's background thread rather than
    /// waiting for the main window's `logic()` to notice (see the module
    /// docs on why that wait is unbounded on Windows). `Visible(false)`
    /// takes effect the next time *this* viewport's own pass runs, which
    /// happens independently of the main window -- normally right away,
    /// since it is the window the Send click just landed in. `finished` is
    /// still set so `EsMailApp::process_compose_windows` does the actual
    /// bookkeeping (dropping it from `compose_windows`) whenever it next
    /// runs; that part isn't user-visible so it doesn't need to be prompt.
    pub(super) fn mark_sent_and_hide(&self, ctx: &egui::Context) {
        self.lock().finished = true;
        ctx.send_viewport_cmd_to(self.viewport_id(), egui::ViewportCommand::Visible(false));
    }

    /// [`Self::set_error`], plus waking this window's own viewport directly
    /// rather than relying on the main window's `logic()` to do it -- same
    /// reasoning as [`Self::mark_sent_and_hide`].
    pub(super) fn set_error_and_wake(&self, error: String) {
        self.set_error(error);
        self.wake();
    }

    /// Declares the window for this frame. Must be called every frame the
    /// main window draws, or egui closes the OS window (see the module docs).
    pub(super) fn show(
        &self,
        ctx: &egui::Context,
        accounts: Vec<(String, String)>,
        contacts: Arc<Contacts>,
        icon: Option<Arc<egui::IconData>>,
    ) {
        let title = {
            let mut s = self.lock();
            s.accounts = accounts;
            s.contacts = contacts;
            if s.state.subject.trim().is_empty() {
                "New message — esMail".to_string()
            } else {
                format!("{} — esMail", s.state.subject.trim())
            }
        };
        let mut builder = egui::ViewportBuilder::default()
            .with_title(title)
            .with_inner_size([680.0, 560.0])
            .with_min_inner_size([420.0, 320.0]);
        if let Some(icon) = icon {
            builder = builder.with_icon(icon);
        }
        let shared = self.shared.clone();
        let wake_app = Arc::clone(&self.wake_app);
        ctx.show_viewport_deferred(self.viewport_id(), builder, move |ui, _class| {
            let mut s = shared.lock().unwrap_or_else(PoisonError::into_inner);
            let before = (s.send_requested, s.finished);
            compose_ui::draw(ui, &mut s);
            if (s.send_requested, s.finished) != before {
                // Only the main viewport's `logic()` acts on these.
                wake_app();
            }
        });
    }
}

/// What clicking the (non-confirmation) Discard button does. A separate,
/// directly testable function since it isn't otherwise reachable without
/// driving a real click through egui.
///
/// Discard stays a one-click, no-questions-asked action for typed-but-unsent
/// text -- that's the point of a dedicated Discard button. A send already in
/// flight is different: it can't be cancelled (see `smtp.rs`), so clicking
/// past it without asking is how a message the user just discarded still got
/// sent and filed to Sent behind their back (#34 review) -- that case goes
/// through the same confirmation as the close button.
pub(super) fn discard_clicked(s: &mut Shared) {
    if s.sending {
        s.confirm_discard = true;
    } else {
        s.finished = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_window(id: ComposeId, state: ComposeState) -> ComposeWindow {
        ComposeWindow::new(id, state, Focus::To, esmail::waker::noop(), esmail::waker::noop())
    }

    fn window() -> ComposeWindow {
        test_window(1, ComposeState { subject: "Hi".to_string(), ..Default::default() })
    }

    #[test]
    fn a_fresh_window_has_nothing_to_lose() {
        assert!(!window().is_dirty());
    }

    #[test]
    fn typing_makes_it_dirty() {
        let w = window();
        w.lock().state.body.push_str("hello");
        assert!(w.is_dirty());
    }

    #[test]
    fn a_send_in_flight_counts_as_dirty_even_with_nothing_typed() {
        // An unedited Reply/Forward sent as-is (#34 review): `state` alone
        // never changes, but there is still something to lose -- there is
        // no cancelling the send, so walking away from it shouldn't be free.
        let w = window();
        assert!(!w.is_dirty());
        w.set_sending(true);
        assert!(w.is_dirty());
        w.set_sending(false);
        assert!(!w.is_dirty());
    }

    #[test]
    fn discard_while_sending_asks_first_instead_of_finishing_immediately() {
        // #34 review: clicking Discard on a window whose send is still in
        // flight used to finish (and so close) the window on the spot, with
        // no way to stop the queued send from still landing in Sent behind
        // the user's back. It must go through the same confirmation the
        // close button already used for unsent typing.
        let w = window();
        w.set_sending(true);
        discard_clicked(&mut w.lock());
        assert!(!w.is_finished(), "must not finish on the spot while a send is in flight");
        assert!(w.lock().confirm_discard, "must ask before discarding a send still in flight");
    }

    #[test]
    fn discard_with_nothing_in_flight_still_finishes_on_the_spot() {
        // The point of a dedicated Discard button: no confirmation needed
        // when there is no in-flight send to lose.
        let w = window();
        w.lock().state.body.push_str("hello");
        discard_clicked(&mut w.lock());
        assert!(w.is_finished());
        assert!(!w.lock().confirm_discard);
    }

    #[test]
    fn a_send_request_is_taken_once() {
        let w = window();
        assert!(w.take_send_request().is_none());
        w.lock().send_requested = true;
        assert_eq!(w.take_send_request().map(|c| c.subject), Some("Hi".to_string()));
        assert!(w.take_send_request().is_none());
    }

    #[test]
    fn an_error_unlocks_the_form_and_a_new_send_clears_it() {
        let w = window();
        w.set_sending(true);
        w.set_error("nope".to_string());
        assert!(!w.lock().sending);
        assert_eq!(w.lock().error.as_deref(), Some("nope"));
        w.set_sending(true);
        assert!(w.lock().error.is_none());
    }

    #[test]
    fn windows_have_distinct_viewport_ids() {
        let a = test_window(1, ComposeState::default());
        let b = test_window(2, ComposeState::default());
        assert_ne!(a.viewport_id(), b.viewport_id());
        assert_eq!(a.viewport_id(), test_window(1, ComposeState::default()).viewport_id());
    }
}
