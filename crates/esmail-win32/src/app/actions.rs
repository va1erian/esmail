//! Mail actions on the selected messages: flag, mark read or unread, archive,
//! delete. Each is a command to the account's IMAP actor; the list and the
//! folder counts change when the server confirms it, and a failure lands in the
//! status bar.

use esmail::imap::{FLAG_FLAGGED, FLAG_SEEN, ImapCommand, MailHeader, SpecialUse};

use super::App;

/// Where Delete moves a message when the account names no Trash folder.
const TRASH_MAILBOX: &str = "Trash";
/// Where Archive moves a message when the account names no Archive folder.
const ARCHIVE_MAILBOX: &str = "Archive";

impl App {
    /// The selected messages' headers.
    fn targets(&self) -> Vec<MailHeader> {
        let Some(open) = self.open.as_ref() else { return Vec::new() };
        self.list.selection().into_iter().filter_map(|row| open.header(row).cloned()).collect()
    }

    /// Flags every selected message, or unflags them when all are flagged.
    pub(super) fn toggle_flag(&mut self) {
        let targets = self.targets();
        let flag = !targets.iter().all(MailHeader::is_flagged);
        for header in targets.iter().filter(|header| header.is_flagged() != flag) {
            self.set_flag(header.uid, FLAG_FLAGGED, flag);
        }
    }

    /// Marks every selected message read or unread.
    pub(super) fn set_seen(&mut self, seen: bool) {
        for header in self.targets().iter().filter(|header| header.is_seen() != seen) {
            self.set_flag(header.uid, FLAG_SEEN, seen);
        }
    }

    fn set_flag(&mut self, uid: u32, flag: &str, on: bool) {
        let flags = vec![flag.to_string()];
        if on { self.store_flags(uid, flags, Vec::new()) } else { self.store_flags(uid, Vec::new(), flags) }
    }

    pub(super) fn store_flags(&mut self, uid: u32, add: Vec<String>, remove: Vec<String>) {
        let Some(folder) = self.open.as_ref().map(|open| open.folder().clone()) else { return };
        let req_id = self.action_ids.begin();
        if !self.core.send(folder.account, ImapCommand::StoreFlags { mailbox: folder.mailbox, uid, add, remove, req_id }) {
            self.banner("this account is not connected");
        }
    }

    pub(super) fn archive(&mut self) {
        self.move_selected(SpecialUse::Archive, ARCHIVE_MAILBOX);
    }

    pub(super) fn delete(&mut self) {
        self.move_selected(SpecialUse::Trash, TRASH_MAILBOX);
    }

    fn move_selected(&mut self, kind: SpecialUse, default: &str) {
        let Some(folder) = self.open.as_ref().map(|open| open.folder().clone()) else { return };
        let dest = self.folders.borrow().special_folder(folder.account, kind, default);
        if dest == folder.mailbox {
            self.set_status(&format!("These messages are already in {dest}"));
            return;
        }
        for header in self.targets() {
            let req_id = self.action_ids.begin();
            let command = ImapCommand::MoveMessage { mailbox: folder.mailbox.clone(), uid: header.uid, dest: dest.clone(), req_id };
            if !self.core.send(folder.account, command) {
                self.banner("this account is not connected");
                return;
            }
        }
    }

    /// The server confirmed new flags: show them, and refresh the folder's
    /// unread count if the message changed between read and unread.
    pub(super) fn flags_updated(&mut self, account: usize, mailbox: &str, uid: u32, flags: Vec<String>) {
        let Some(open) = self.open.as_mut().filter(|o| o.folder().account == account && o.folder().mailbox == mailbox) else { return };
        let Some(was_seen) = open.row_of(uid).and_then(|row| open.header(row)).map(MailHeader::is_seen) else { return };
        let Some(row) = open.set_flags(uid, flags) else { return };
        let updated = open.header(row).cloned();
        self.list.update_rows(open.rows());
        let now_seen = updated.as_ref().is_some_and(MailHeader::is_seen);
        if let (Some(selected), Some(updated)) = (self.selected.as_mut().filter(|header| header.uid == uid), updated) {
            selected.flags = updated.flags;
        }
        if was_seen != now_seen {
            self.request_unread_counts(account, Some(&[mailbox]));
        }
    }

    /// The server moved a message out of the open folder: drop its row, and
    /// when it was the open message, move on to its neighbour.
    pub(super) fn moved(&mut self, account: usize, mailbox: &str, uid: u32, dest: &str) {
        self.set_status(&format!("Moved to {dest}"));
        self.request_unread_counts(account, Some(&[mailbox, dest]));
        let Some(open) = self.open.as_mut().filter(|o| o.folder().account == account && o.folder().mailbox == mailbox) else { return };
        let Some(row) = open.remove(uid) else { return };
        self.list.remove_rows(open.rows(), row, 1);
        if self.selected.as_ref().is_none_or(|header| header.uid != uid) {
            return;
        }
        self.selected = None;
        self.bodies.cancel();
        match open.loaded() {
            0 => self.reader.show_notice("Select a message to read it."),
            len => self.list.set_selection(&[row.min(len - 1)]),
        }
    }
}
