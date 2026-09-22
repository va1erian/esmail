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
struct Shared {
    state: ComposeState,
    /// What the window opened with; `state != initial` means there is
    /// something to lose ([`ComposeWindow::is_dirty`]).
    initial: ComposeState,
    /// `(account id, label)` for the From selector, refreshed by the app
    /// every frame it draws the main window.
    accounts: Vec<(String, String)>,
    /// The last thing that went wrong (a failed send, an unreadable
    /// attachment); shown in red until the next Send.
    error: Option<String>,
    /// A send is in flight: the form is locked and Send is disabled.
    sending: bool,
    /// Send was clicked; the app takes this in `logic()`.
    send_requested: bool,
    /// The user chose to be done with the message (Discard, or confirmed
    /// closing the OS window): the app drops the window.
    finished: bool,
    /// The "discard this unsent message?" question is showing in place of the
    /// buttons.
    confirm_discard: bool,
    focus: Option<Focus>,
}

/// Cheap to clone (an id plus an `Arc`): a copy lives in `EsMailApp`'s
/// `compose_registry` so the SMTP forwarder task can reach a specific
/// window's state directly from its own background thread -- see
/// [`Self::mark_sent_and_hide`] for why that matters.
#[derive(Clone)]
pub(super) struct ComposeWindow {
    id: ComposeId,
    shared: Arc<Mutex<Shared>>,
}

impl ComposeWindow {
    pub(super) fn new(id: ComposeId, state: ComposeState, focus: Focus) -> Self {
        let shared = Shared {
            initial: state.clone(),
            state,
            accounts: Vec::new(),
            error: None,
            sending: false,
            send_requested: false,
            finished: false,
            confirm_discard: false,
            focus: Some(focus),
        };
        Self { id, shared: Arc::new(Mutex::new(shared)) }
    }

    pub(super) fn id(&self) -> ComposeId {
        self.id
    }

    pub(super) fn viewport_id(&self) -> egui::ViewportId {
        egui::ViewportId::from_hash_of(("esmail-compose", self.id))
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
    pub(super) fn set_error_and_wake(&self, ctx: &egui::Context, error: String) {
        self.set_error(error);
        ctx.request_repaint_of(self.viewport_id());
    }

    /// Declares the window for this frame. Must be called every frame the
    /// main window draws, or egui closes the OS window (see the module docs).
    pub(super) fn show(&self, ctx: &egui::Context, accounts: Vec<(String, String)>, icon: Option<Arc<egui::IconData>>) {
        let title = {
            let mut s = self.lock();
            s.accounts = accounts;
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
        ctx.show_viewport_deferred(self.viewport_id(), builder, move |ui, _class| {
            let mut s = shared.lock().unwrap_or_else(PoisonError::into_inner);
            let before = (s.send_requested, s.finished);
            draw(ui, &mut s);
            if (s.send_requested, s.finished) != before {
                // Only the main viewport's `logic()` acts on these.
                ui.ctx().request_repaint_of(egui::ViewportId::ROOT);
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
fn discard_clicked(s: &mut Shared) {
    if s.sending {
        s.confirm_discard = true;
    } else {
        s.finished = true;
    }
}

fn draw(ui: &mut egui::Ui, s: &mut Shared) {
    let ctx = ui.ctx().clone();

    // Something would be lost by closing right now: unsaved typing, or a
    // send that's still in flight (there's no cancelling it once issued --
    // see `smtp.rs` -- so walking away from it silently would either lose
    // the message from view while it still gets sent, or, if it fails,
    // leave the error with nobody to show it to). An unedited Reply/Forward
    // sent as-is (`state == initial`) is exactly the case `state` alone
    // would miss.
    let unsent = s.state != s.initial || s.sending;

    // The OS window's own close button (or Alt+F4). Unsent text or an
    // in-flight send is asked about first; otherwise the app is told to
    // drop the window.
    if ctx.input(|i| i.viewport().close_requested()) {
        if unsent && !s.finished {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            s.confirm_discard = true;
        } else {
            s.finished = true;
        }
    }

    // Ctrl+Enter sends. Consumed up front so the text field does not also
    // insert a line break.
    let ctrl_enter = ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Enter));
    let mut send = ctrl_enter && !s.sending && !s.confirm_discard;

    egui::Panel::bottom("compose_actions").show(ui, |ui| {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if s.confirm_discard {
                if s.sending {
                    ui.label("A send is still in progress and can't be stopped -- discard this window anyway?");
                } else {
                    ui.label("Discard this unsent message?");
                }
                if ui.button("Discard").clicked() {
                    s.finished = true;
                }
                if ui.button("Keep editing").clicked() {
                    s.confirm_discard = false;
                }
            } else {
                if ui.add_enabled(!s.sending, egui::Button::new("Send")).on_hover_text("Ctrl+Enter").clicked() {
                    send = true;
                }
                if ui.button("Discard").clicked() {
                    discard_clicked(s);
                }
                if s.sending {
                    ui.spinner();
                    ui.weak("Sending…");
                } else if let Some(error) = &s.error {
                    ui.label(egui::RichText::new(error).color(egui::Color32::RED));
                }
            }
        });
        ui.add_space(4.0);
    });

    egui::CentralPanel::default().show(ui, |ui| {
        let Shared { state, accounts, error, sending, focus, .. } = &mut *s;
        ui.add_enabled_ui(!*sending, |ui| {
            egui::Grid::new("compose_grid").num_columns(2).show(ui, |ui| {
                // Which account this is sent from -- and whose Sent folder
                // gets the copy. Chosen when the window opened (the account
                // of the message being replied to, else the active one).
                ui.label("From:");
                let selected = state
                    .account_id
                    .as_deref()
                    .and_then(|id| accounts.iter().find(|(a, _)| a == id))
                    .map_or("(choose an account)", |(_, label)| label.as_str());
                egui::ComboBox::from_id_salt("compose_from").selected_text(selected).show_ui(ui, |ui| {
                    for (id, label) in accounts.iter() {
                        ui.selectable_value(&mut state.account_id, Some(id.clone()), label);
                    }
                });
                ui.end_row();

                ui.label("To:");
                let to = ui.add(egui::TextEdit::singleline(&mut state.to).desired_width(f32::INFINITY));
                if matches!(focus, Some(Focus::To)) {
                    to.request_focus();
                    *focus = None;
                }
                ui.end_row();

                ui.label("Cc:");
                ui.add(egui::TextEdit::singleline(&mut state.cc).desired_width(f32::INFINITY));
                ui.end_row();

                ui.label("Bcc:");
                ui.add(egui::TextEdit::singleline(&mut state.bcc).desired_width(f32::INFINITY));
                ui.end_row();

                ui.label("Subject:");
                ui.add(egui::TextEdit::singleline(&mut state.subject).desired_width(f32::INFINITY));
                ui.end_row();
            });

            ui.separator();

            let mut remove = None;
            ui.horizontal_wrapped(|ui| {
                if ui.button("Attach file…").clicked() {
                    // Not parented to this window: egui hands a viewport's
                    // callback no native window handle to parent it to.
                    if let Some(path) = rfd::FileDialog::new().pick_file() {
                        match std::fs::read(&path) {
                            Ok(data) => {
                                let filename = path
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| "attachment".to_string());
                                state.attachments.push((filename, data));
                            }
                            Err(e) => {
                                *error = Some(format!("Could not read {}: {e}", path.display()));
                            }
                        }
                    }
                }
                for (i, (filename, data)) in state.attachments.iter().enumerate() {
                    ui.label(format!("{filename} ({})", format_size(data.len())));
                    if ui.small_button("✕").on_hover_text("Remove").clicked() {
                        remove = Some(i);
                    }
                }
            });
            if let Some(i) = remove {
                state.attachments.remove(i);
            }

            // The body takes whatever room is left and scrolls past it, so it
            // follows the window as it is resized.
            let room = ui.available_size();
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                let body = ui.add(
                    egui::TextEdit::multiline(&mut state.body)
                        .desired_width(f32::INFINITY)
                        .min_size(room),
                );
                if matches!(focus, Some(Focus::Body)) {
                    body.request_focus();
                    *focus = None;
                }
            });
        });
    });

    if send {
        s.send_requested = true;
        s.error = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window() -> ComposeWindow {
        ComposeWindow::new(1, ComposeState { subject: "Hi".to_string(), ..Default::default() }, Focus::To)
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
        let a = ComposeWindow::new(1, ComposeState::default(), Focus::To);
        let b = ComposeWindow::new(2, ComposeState::default(), Focus::To);
        assert_ne!(a.viewport_id(), b.viewport_id());
        assert_eq!(a.viewport_id(), ComposeWindow::new(1, ComposeState::default(), Focus::To).viewport_id());
    }
}
