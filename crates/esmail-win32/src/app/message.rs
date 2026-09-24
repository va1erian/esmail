//! The selected message: fetching its body, showing it, and marking it read
//! once it has stayed open for a moment.

use std::time::{Duration, Instant};

use esmail::imap::{FLAG_SEEN, ImapCommand};
use esmail::render::Attachment;
use win32ui::prelude::*;

use super::{App, Msg};

/// `(account, mailbox, uid)`: which message a body fetch is for.
pub type BodyKey = (usize, String, u32);

/// How long a message stays open before it is marked read, so arrowing past it
/// does not (the egui app uses the same delay).
const MARK_SEEN_DELAY: Duration = Duration::from_millis(1200);
/// How often the pending mark-as-read is checked.
const SEEN_POLL_MILLIS: u32 = 300;

/// The message waiting to be marked read, and the timer that checks on it. The
/// timer runs only while a message is waiting.
#[derive(Default)]
pub struct SeenTimer {
    pending: Option<(u32, Instant)>,
    timer: Option<TimerId>,
}

impl SeenTimer {
    /// Whether `id` is this timer's tick.
    pub fn owns(&self, id: TimerId) -> bool {
        self.timer == Some(id)
    }

    fn schedule(&mut self, ui: &Ui<Msg>, uid: u32) {
        self.pending = Some((uid, Instant::now()));
        if self.timer.is_none() {
            self.timer = ui.set_timer(SEEN_POLL_MILLIS).ok();
        }
    }

    /// Forgets the pending message and stops the timer.
    pub fn cancel(&mut self, ui: &Ui<Msg>) {
        self.pending = None;
        if let Some(timer) = self.timer.take() {
            ui.kill_timer(timer);
        }
    }
}

impl App {
    pub(super) fn select(&mut self, ui: &Ui<Msg>, rows: &[usize]) {
        let [row] = rows else {
            self.bodies.cancel();
            self.seen.cancel(ui);
            return;
        };
        let Some((folder, header)) = self.message_at(*row) else { return };
        let key = (folder.account, folder.mailbox.clone(), header.uid);
        self.selected_in = Some(folder);
        self.set_status("Loading message...");
        self.seen.cancel(ui);
        if !header.is_seen() {
            self.seen.schedule(ui, header.uid);
        }
        self.selected = Some(header);
        self.message_shown = false;
        if let Some((key, id)) = self.bodies.want(key) {
            self.fetch_body(key, id);
        }
        if let Some(focus) = self.list.focus_row() {
            self.list.ensure_visible(focus);
        }
    }

    /// Whether the message `(account, mailbox, uid)` is the one on screen.
    pub(super) fn selected_is(&self, account: usize, mailbox: &str, uid: u32) -> bool {
        self.selected.as_ref().is_some_and(|header| header.uid == uid)
            && self.selected_in.as_ref().is_some_and(|folder| folder.account == account && folder.mailbox == mailbox)
    }

    fn fetch_body(&mut self, (account, mailbox, uid): BodyKey, req_id: u64) {
        if !self.core.send(account, ImapCommand::FetchBody { mailbox, uid, req_id }) {
            self.banner("this account is not connected");
        }
    }

    /// A body (or its failure) came back: show it if it is still the message
    /// the user wants, and start the next wanted fetch.
    pub(super) fn body_arrived(&mut self, account: usize, uid: u32, req_id: u64, outcome: std::result::Result<(String, Vec<Attachment>), String>) {
        let Some(mailbox) = self.selected_in.as_ref().map(|folder| folder.mailbox.clone()) else { return };
        let finished = self.bodies.finished(&(account, mailbox.clone(), uid), req_id);
        if finished.show {
            if let Some(header) = self.selected.clone() {
                match outcome {
                    Ok((html, attachments)) => {
                        self.core.cache().index_mail(account, &mailbox, header.clone(), html.clone(), attachments.clone());
                        self.reader.show_message(header, html, attachments);
                        self.set_status("Ready");
                    }
                    Err(error) => {
                        let text = format!("Could not load this message: {error}");
                        self.reader.show_notice(&text);
                        self.set_status(&format!("Error: {text}"));
                    }
                }
                self.message_shown = true;
            }
        }
        if let Some((key, id)) = finished.next {
            self.fetch_body(key, id);
        }
    }

    /// The timer tick: sends `\Seen` for the pending message once it has been
    /// open long enough, provided it is still the selected one.
    pub(super) fn mark_seen_when_due(&mut self, ui: &Ui<Msg>) {
        let Some((uid, since)) = self.seen.pending else { return self.seen.cancel(ui) };
        if self.selected.as_ref().map(|header| header.uid) != Some(uid) {
            return self.seen.cancel(ui);
        }
        if since.elapsed() >= MARK_SEEN_DELAY {
            self.seen.cancel(ui);
            if let Some(folder) = self.selected_in.clone() {
                self.store_flags(folder, uid, vec![FLAG_SEEN.to_string()], Vec::new());
            }
        }
    }
}
