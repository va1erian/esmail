//! The main window's side of composing: opening compose windows, and running
//! what they ask for (send, save a draft, discard) against the core.
//!
//! A send that fails is recorded in the outbox and reported to its window, which
//! stays open with the text intact; the outbox is polled so a message whose
//! window is gone is still retried. A send that succeeds saves a copy to the
//! account's Sent folder, closes the window and deletes the draft.

use std::collections::HashMap;
use std::rc::Rc;

use esmail::compose::{ComposeId, ComposeState};
use esmail::contacts::Contacts;
use esmail::imap::{ImapCommand, SpecialUse};
use esmail::smtp::SmtpEvent;
use esmail_win32::core_glue::compose::{Kind, has_content, initial_state};
use esmail_win32::core_glue::{Delivered, Deliveries, Failure};
use xui_win32::prelude::*;

use super::compose::{self, ComposeMsg, Init, Request};
use super::queue::QueueKind;
use super::{App, Msg};

/// Where a sent message is filed when the account names no Sent folder.
const SENT_MAILBOX: &str = "Sent";
/// How often the outbox is asked for messages that are due another attempt.
pub const OUTBOX_POLL_MILLIS: u32 = 60_000;

/// One open (or just closed) compose window.
struct Entry {
    /// `None` once the window is gone.
    window: Option<WindowHandle<ComposeMsg>>,
    /// The `drafts` row the message was saved to.
    draft: Option<i64>,
    /// Saves whose row id has not come back yet.
    saves_pending: u32,
    /// The last of those was asked for by the user, so the window says it.
    announce_save: bool,
    /// The message is sent or discarded: a draft row that shows up late is
    /// deleted rather than kept.
    settled: bool,
}

/// The compose windows and the messages on their way out.
#[derive(Default)]
pub struct Composes {
    entries: HashMap<ComposeId, Entry>,
    deliveries: Deliveries,
}

impl Composes {
    /// Whether any compose window is still open.
    pub fn any_open(&self) -> bool {
        self.entries.values().any(|entry| entry.window.as_ref().is_some_and(WindowHandle::is_alive))
    }

    /// Some open window, for a screenshot.
    pub fn any_window(&self) -> Option<&WindowHandle<ComposeMsg>> {
        self.entries.values().find_map(|entry| entry.window.as_ref())
    }

    /// Tells every open window which theme to wear.
    pub fn set_theme(&self, theme: Theme, follow_system: bool) {
        for window in self.entries.values().filter_map(|entry| entry.window.as_ref()) {
            let _ = window.send(ComposeMsg::SetTheme(theme, follow_system));
        }
    }

    fn tell(&self, id: ComposeId, msg: ComposeMsg) {
        if let Some(window) = self.entries.get(&id).and_then(|entry| entry.window.as_ref()) {
            let _ = window.send(msg);
        }
    }

    /// Drops the entry of a closed window once nothing can still arrive for it.
    fn forget_if_done(&mut self, id: ComposeId) {
        if self.entries.get(&id).is_some_and(|entry| entry.window.is_none() && entry.saves_pending == 0) {
            self.entries.remove(&id);
        }
    }
}

impl App {
    /// Opens a compose window of `kind`: a reply or forward is derived from the
    /// message on screen, sent from the account it was received on.
    pub(super) fn open_compose(&mut self, ui: &Ui<Msg>, kind: Kind) {
        let original = self.reader.current_message();
        if kind.needs_original() && original.is_none() {
            return self.set_status("Open a message first.");
        }
        let account = self.selected_in.as_ref().or(self.open.as_ref().map(|open| open.folder())).map_or(0, |folder| folder.account);
        let Some(config) = self.core.accounts().get(account) else { return self.banner("No account to send from.") };
        let state = initial_state(kind, original, config);
        self.start_compose(ui, state, kind != Kind::New && kind != Kind::Forward);
    }

    /// Opens a window for `state` (a message to write, or a draft to continue).
    pub(super) fn start_compose(&mut self, ui: &Ui<Msg>, state: ComposeState, body_first: bool) {
        let accounts = self.core.accounts().iter().map(|a| (a.id.clone(), format!("{} <{}>", a.display_name, a.username))).collect();
        let id = self.composes.deliveries.next_id();
        let init = Init {
            id,
            state,
            accounts,
            contacts: Rc::new(self.contacts()),
            host: ui.proxy(),
            body_first,
            follow_system_theme: self.theme == esmail_win32::core_glue::ThemeChoice::System,
            acrylic: self.acrylic,
        };
        match compose::open(ui, init) {
            Ok(window) => {
                self.composes.entries.insert(id, Entry { window: Some(window), draft: None, saves_pending: 0, announce_save: false, settled: false });
            }
            Err(error) => self.banner(&format!("Could not open a compose window: {error}")),
        }
    }

    /// The addresses a window can suggest: everyone in the open folder and the
    /// search results, and the accounts themselves.
    fn contacts(&self) -> Contacts {
        let own = self.core.accounts().iter().map(|account| account.username.as_str());
        let listed = self.open.as_ref().map(|open| open.headers()).unwrap_or_default().iter();
        Contacts::from_headers(listed, own)
    }

    /// A compose window asked for something.
    pub(super) fn compose_request(&mut self, id: ComposeId, request: Request) {
        match request {
            Request::Send(state) => self.send_message(id, state),
            Request::SaveDraft { state, explicit } => self.save_draft(id, state, explicit),
            Request::Discard => self.discard_message(id),
            Request::Closed => {
                if let Some(entry) = self.composes.entries.get_mut(&id) {
                    entry.window = None;
                }
                self.composes.forget_if_done(id);
            }
        }
    }

    fn send_message(&mut self, id: ComposeId, state: ComposeState) {
        let account = state.account_id.as_ref().and_then(|wanted| self.core.accounts().iter().position(|a| &a.id == wanted));
        let queued = match account {
            Some(account) => self.core.send_mail(id, account, state.clone()).map(|()| account),
            None => Err("Choose an account to send from.".to_string()),
        };
        match queued {
            Ok(account) => {
                self.composes.deliveries.begin(id, account, state);
                self.composes.tell(id, ComposeMsg::Sending);
                self.set_status("Sending...");
            }
            Err(error) => self.composes.tell(id, ComposeMsg::Failed(error)),
        }
    }

    fn save_draft(&mut self, id: ComposeId, mut state: ComposeState, explicit: bool) {
        let Some(entry) = self.composes.entries.get_mut(&id) else { return };
        if !has_content(&state) {
            return;
        }
        state.draft_id = entry.draft;
        entry.saves_pending += 1;
        entry.announce_save = explicit;
        self.core.cache().save_draft(id, state);
    }

    fn discard_message(&mut self, id: ComposeId) {
        if let Some(row) = self.composes.deliveries.forget(id) {
            self.core.cache().delete_outbox(row);
        }
        self.settle(id);
    }

    /// The message is finished with: its draft, saved or still being saved, goes.
    fn settle(&mut self, id: ComposeId) {
        let Some(entry) = self.composes.entries.get_mut(&id) else { return };
        entry.settled = true;
        if let Some(draft) = entry.draft.take() {
            self.core.cache().delete_draft(draft);
        }
    }

    /// The cache wrote a draft.
    pub(super) fn draft_saved(&mut self, row: i64, id: ComposeId) {
        let Some(entry) = self.composes.entries.get_mut(&id) else { return };
        entry.saves_pending = entry.saves_pending.saturating_sub(1);
        if entry.settled {
            self.core.cache().delete_draft(row);
        } else {
            entry.draft = Some(row);
            if entry.saves_pending == 0 && std::mem::take(&mut entry.announce_save) {
                self.composes.tell(id, ComposeMsg::DraftSaved);
            }
        }
        self.composes.forget_if_done(id);
    }

    /// The cache recorded a failed message in the outbox.
    pub(super) fn outbox_enqueued(&mut self, row: i64, id: ComposeId) {
        self.composes.deliveries.outbox_enqueued(id, row);
    }

    /// The outbox has messages due another attempt.
    pub(super) fn outbox_due(&mut self, due: Vec<esmail::db::OutboxItem>) {
        let open = |id: ComposeId| self.composes.entries.get(&id).is_some_and(|entry| entry.window.is_some());
        let accounts = self.core.accounts();
        let (retries, unsendable) = self.composes.deliveries.retries(due, open, |wanted| accounts.iter().position(|a| a.id == wanted));
        for item in unsendable {
            self.core.cache().mark_outbox_failed(item.id, "This account is no longer configured.".to_string());
        }
        for retry in retries {
            if let Err(error) = self.core.send_mail(retry.id, retry.account, retry.state) {
                self.outcome_failed(retry.id, error);
            }
        }
    }

    /// Applies what the SMTP actor reported.
    pub(super) fn smtp_events(&mut self) {
        for event in self.core.pump_sent() {
            match event {
                SmtpEvent::Sent { id, raw } => self.outcome_sent(id, raw),
                SmtpEvent::Error { id, error } => self.outcome_failed(id, error),
            }
        }
    }

    fn outcome_sent(&mut self, id: ComposeId, raw: Vec<u8>) {
        let Some(Delivered { account, outbox_row }) = self.composes.deliveries.delivered(id) else { return };
        if let Some(row) = outbox_row {
            self.core.cache().mark_outbox_sent(row);
        }
        let mailbox = self.folders.borrow().special_folder(account, SpecialUse::Sent, SENT_MAILBOX);
        if !self.core.send(account, ImapCommand::Append { mailbox, raw }) {
            self.banner("The message was sent but could not be copied to the Sent folder.");
        }
        self.settle(id);
        self.composes.tell(id, ComposeMsg::Sent);
        self.set_status("Message sent");
        self.refresh_queue(QueueKind::Outbox);
    }

    fn outcome_failed(&mut self, id: ComposeId, error: String) {
        match self.composes.deliveries.failed(id) {
            Some(Failure::Backoff { row }) => self.core.cache().mark_outbox_failed(row, error.clone()),
            Some(Failure::Enqueue { account, state }) => {
                if let Some(config) = self.core.accounts().get(account) {
                    self.core.cache().enqueue_outbox(id, config.id.clone(), state);
                }
            }
            None => return,
        }
        let open = self.composes.entries.get(&id).is_some_and(|entry| entry.window.is_some());
        if open {
            self.composes.tell(id, ComposeMsg::Failed(error));
        } else {
            self.banner(&format!("A message could not be sent and stays in the outbox for another try: {error}"));
        }
        self.refresh_queue(QueueKind::Outbox);
    }
}

impl App {
    /// The main window's close box. Compose windows belong to it, so it waits
    /// for them: each asks about its own unsaved changes when closed.
    pub(super) fn close_and_quit(&mut self, ui: &mut Ui<Msg>) {
        if self.composes.any_open() {
            let _ = TaskDialog::new("Close the message windows first")
                .content("Each unfinished message asks whether to keep it as a draft.")
                .buttons([("OK", ())])
                .icon(TaskDialogIcon::Information)
                .show(ui);
            return;
        }
        self.quit(ui);
    }

    /// Ends the program now, leaving any compose window to its autosaved draft.
    pub(super) fn quit(&mut self, ui: &mut Ui<Msg>) {
        self.save_window_state(ui);
        ui.quit();
    }
}
