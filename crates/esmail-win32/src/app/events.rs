//! What the accounts' IMAP sessions report, applied to the window.

use esmail::imap::{ImapCommand, ImapEvent};
use esmail_win32::core_glue::FolderRef;

use win32ui::Ui;

use super::{App, Msg, native_tree};

impl App {
    /// Applies everything the core has queued. Never blocks.
    pub(super) fn drain(&mut self, ui: &Ui<Msg>) {
        let mut tree_changed = false;
        for (account, event) in self.core.pump() {
            tree_changed |= self.handle(ui, account, event);
        }
        if tree_changed {
            native_tree::sync(&self.tree, &self.folders.borrow());
        }
    }

    /// Applies one event. Returns whether the folder tree needs syncing.
    fn handle(&mut self, ui: &Ui<Msg>, account: usize, event: ImapEvent) -> bool {
        match event {
            ImapEvent::Connected => {
                self.set_status("Connected");
                self.core.send(account, ImapCommand::FetchMailboxes);
                if self.open.as_ref().is_some_and(|open| open.folder().account == account) {
                    self.request_refresh();
                }
            }
            ImapEvent::Disconnected => self.set_status("Disconnected, reconnecting..."),
            ImapEvent::Error(error) => {
                if let Some(open) = self.open.as_mut() {
                    open.page_failed();
                }
                self.banner(&error);
            }
            ImapEvent::Mailboxes(mailboxes) => {
                self.folders.borrow_mut().set_mailboxes(account, &mailboxes);
                self.request_unread_counts(account, None);
                self.open_first_folder(ui, account);
                return true;
            }
            ImapEvent::UnreadCounts(counts) => {
                self.folders.borrow_mut().set_unread(account, counts);
                return true;
            }
            ImapEvent::NewHeaders { mailbox, .. } => {
                self.request_unread_counts(account, None);
                if self.open.as_ref().is_some_and(|open| open.folder().account == account && open.folder().mailbox == mailbox) {
                    self.request_refresh();
                }
            }
            ImapEvent::Headers { mailbox, headers, page, total_pages, req_id, .. } => {
                self.apply_headers(account, &mailbox, req_id, page, total_pages, headers);
            }
            ImapEvent::Body { uid, html, attachments, req_id } => self.body_arrived(account, uid, req_id, Ok((html, attachments))),
            ImapEvent::BodyFailed { uid, req_id, error } => self.body_arrived(account, uid, req_id, std::result::Result::Err(error)),
            ImapEvent::FlagsUpdated { mailbox, uid, flags, .. } => self.flags_updated(account, &mailbox, uid, flags),
            ImapEvent::FlagsUpdateFailed { uid, error, .. } => self.banner(&format!("Could not update message {uid}: {error}")),
            ImapEvent::Moved { mailbox, uid, dest, .. } => self.moved(account, &mailbox, uid, &dest),
            ImapEvent::MoveFailed { uid, error, .. } => self.banner(&format!("Could not move message {uid}: {error}")),
            _ => {}
        }
        false
    }

    /// Asks for unread counts: of `only`, or of every folder of `account`. A
    /// single-folder reply leaves the other counts alone.
    pub(super) fn request_unread_counts(&self, account: usize, only: Option<&[&str]>) {
        let mailboxes = match only {
            Some(names) => names.iter().map(|name| name.to_string()).collect(),
            None => self.folders.borrow().mailbox_names(account),
        };
        if !mailboxes.is_empty() {
            self.core.send(account, ImapCommand::FetchUnreadCounts { mailboxes });
        }
    }

    fn open_first_folder(&mut self, ui: &Ui<Msg>, account: usize) {
        if self.open.is_some() {
            return;
        }
        let wanted = self.wanted_folder.clone().unwrap_or_else(|| "INBOX".to_string());
        let mailbox = self.folders.borrow().mailbox_names(account).into_iter().find(|name| name.eq_ignore_ascii_case(&wanted));
        if let Some(mailbox) = mailbox {
            self.open_folder(ui, FolderRef { account, mailbox });
        }
    }
}
