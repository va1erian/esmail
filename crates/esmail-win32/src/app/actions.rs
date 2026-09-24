//! Mail actions on the selected messages: flag, mark read or unread, archive,
//! delete. Each is a command to the account's IMAP actor; the list and the
//! folder counts change when the server confirms it, and a failure lands in the
//! status bar.
//!
//! The selected rows may be the open folder's or a search's, so every action
//! works from `(folder, header)` pairs and sends to each message's own folder.

use esmail::imap::{FLAG_FLAGGED, FLAG_SEEN, ImapCommand, MailHeader, SpecialUse};
use esmail_win32::core_glue::FolderRef;

use super::App;

/// Where Delete moves a message when the account names no Trash folder.
const TRASH_MAILBOX: &str = "Trash";
/// Where Archive moves a message when the account names no Archive folder.
const ARCHIVE_MAILBOX: &str = "Archive";

/// A message and the folder it lives in.
type Target = (FolderRef, MailHeader);

impl App {
    /// The selected messages.
    fn targets(&self) -> Vec<Target> {
        self.list.selection().into_iter().filter_map(|row| self.message_at(row)).collect()
    }

    /// Flags every selected message, or unflags them when all are flagged.
    pub(super) fn toggle_flag(&mut self) {
        let targets = self.targets();
        self.toggle_flag_of(&targets);
    }

    /// Toggles the flag of a row (its star was clicked, or Space was pressed on
    /// it): of the whole selection when the row is part of it, else of the row.
    pub(super) fn toggle_flag_at(&mut self, row: usize) {
        if self.list.selection().contains(&row) {
            return self.toggle_flag();
        }
        let target: Vec<Target> = self.message_at(row).into_iter().collect();
        self.toggle_flag_of(&target);
    }

    fn toggle_flag_of(&mut self, targets: &[Target]) {
        let flag = !targets.iter().all(|(_, header)| header.is_flagged());
        for (folder, header) in targets.iter().filter(|(_, header)| header.is_flagged() != flag) {
            self.set_flag(folder, header.uid, FLAG_FLAGGED, flag);
        }
    }

    /// Marks every selected message read or unread.
    pub(super) fn set_seen(&mut self, seen: bool) {
        for (folder, header) in self.targets().iter().filter(|(_, header)| header.is_seen() != seen) {
            self.set_flag(folder, header.uid, FLAG_SEEN, seen);
        }
    }

    fn set_flag(&mut self, folder: &FolderRef, uid: u32, flag: &str, on: bool) {
        let flags = vec![flag.to_string()];
        if on { self.store_flags(folder.clone(), uid, flags, Vec::new()) } else { self.store_flags(folder.clone(), uid, Vec::new(), flags) }
    }

    pub(super) fn store_flags(&mut self, folder: FolderRef, uid: u32, add: Vec<String>, remove: Vec<String>) {
        let req_id = self.action_ids.begin();
        if self.core.send(folder.account, ImapCommand::StoreFlags { mailbox: folder.mailbox, uid, add, remove, req_id }) {
            self.pending_actions += 1;
        } else {
            self.account_unavailable(folder.account);
        }
    }

    pub(super) fn archive(&mut self) {
        self.move_selected(SpecialUse::Archive, ARCHIVE_MAILBOX);
    }

    pub(super) fn delete(&mut self) {
        self.move_selected(SpecialUse::Trash, TRASH_MAILBOX);
    }

    fn move_selected(&mut self, kind: SpecialUse, default: &str) {
        let mut already_there = None;
        for (folder, header) in self.targets() {
            let dest = self.folders.borrow().special_folder(folder.account, kind, default);
            if dest == folder.mailbox {
                already_there = Some(dest);
                continue;
            }
            let req_id = self.action_ids.begin();
            let command = ImapCommand::MoveMessage { mailbox: folder.mailbox, uid: header.uid, dest, req_id };
            if self.core.send(folder.account, command) {
                self.pending_actions += 1;
            } else {
                self.account_unavailable(folder.account);
                return;
            }
        }
        if let Some(dest) = already_there {
            self.set_status(&format!("Some of these messages are already in {dest}"));
        }
    }

    /// A flag or move command settled (success or failure): the bars stop
    /// disabling the actions that were in flight.
    pub(super) fn action_finished(&mut self) {
        self.pending_actions = self.pending_actions.saturating_sub(1);
    }

    /// Export the open message's raw source as an `.eml`, chosen by the user.
    pub(super) fn export_selected(&mut self) {
        let (Some(folder), Some(header)) = (self.selected_in.clone(), self.selected.clone()) else {
            self.set_status("Select a message to export first");
            return;
        };
        let name = format!("{}.eml", esmail::view_model::safe_attachment_filename(&header.subject));
        let Some(path) = rfd::FileDialog::new().set_title("Export message").set_file_name(name).save_file() else { return };
        self.set_status(&format!("Exporting {}...", path.display()));
        if self.core.send(folder.account, ImapCommand::ExportMessage { mailbox: folder.mailbox, uid: header.uid, path }) {
            self.pending_actions += 1;
        } else {
            self.account_unavailable(folder.account);
        }
    }

    /// The server wrote the exported message to `path`.
    pub(super) fn export_done(&mut self, path: std::path::PathBuf) {
        self.action_finished();
        self.set_status(&format!("Saved {}", path.display()));
    }

    /// The export failed: fetching the message or writing the file.
    pub(super) fn export_failed(&mut self, error: &str) {
        self.action_finished();
        self.banner(&format!("Could not export the message: {error}"));
    }

    /// The server confirmed new flags: show them, and refresh the folder's
    /// unread count if the message changed between read and unread.
    pub(super) fn flags_updated(&mut self, account: usize, mailbox: &str, uid: u32, flags: Vec<String>) {
        self.core.cache().update_flags(account, mailbox, uid, flags.clone());
        let was_seen = self.set_flags_everywhere(account, mailbox, uid, flags);
        let Some((was_seen, now_seen)) = was_seen else { return };
        if was_seen != now_seen {
            self.request_unread_counts(account, Some(&[mailbox]));
        }
    }

    /// Records `flags` in the open folder, in the search results and in the open
    /// message, repainting whichever the list shows. Returns whether the message
    /// was read before and after, or `None` when none of them holds it.
    fn set_flags_everywhere(&mut self, account: usize, mailbox: &str, uid: u32, flags: Vec<String>) -> Option<(bool, bool)> {
        let searching = self.search.active();
        let mut seen = None;
        if let Some(open) = self.open.as_mut().filter(|o| o.folder().account == account && o.folder().mailbox == mailbox) {
            let was_seen = open.row_of(uid).and_then(|row| open.header(row)).map(MailHeader::is_seen);
            if let (Some(was_seen), Some(row)) = (was_seen, open.set_flags(uid, flags.clone())) {
                seen = Some((was_seen, open.header(row).is_some_and(MailHeader::is_seen)));
                if !searching {
                    self.list.update_rows(open.rows());
                }
            }
        }
        if let Some(results) = self.search.results_mut() {
            let was_seen = results.seen(account, mailbox, uid);
            if results.set_flags(account, mailbox, uid, flags.clone()) {
                seen = seen.or(was_seen.map(|was| (was, flags.iter().any(|flag| flag == FLAG_SEEN))));
                self.list.update_rows(results.rows());
            }
        }
        if self.selected_is(account, mailbox, uid) {
            if let Some(selected) = self.selected.as_mut() {
                selected.flags = flags;
            }
        }
        seen
    }

    /// The server moved a message out of its folder: drop its row, and when it
    /// was the open message, move on to its neighbour.
    pub(super) fn moved(&mut self, account: usize, mailbox: &str, uid: u32, dest: &str) {
        self.set_status(&format!("Moved to {dest}"));
        self.request_unread_counts(account, Some(&[mailbox, dest]));
        self.core.cache().remove_message(account, mailbox, uid);
        let was_selected = self.selected_is(account, mailbox, uid);
        let folder_row = self.open.as_mut().filter(|o| o.folder().account == account && o.folder().mailbox == mailbox).and_then(|open| open.remove(uid));
        let result_row = self.search.results_mut().and_then(|results| results.remove(account, mailbox, uid));
        let (row, rows, len) = match (self.search.results(), folder_row, result_row) {
            (Some(results), _, Some(row)) => (row, results.rows(), results.len()),
            (None, Some(row), _) => match self.open.as_ref() {
                Some(open) => (row, open.rows(), open.loaded()),
                None => return,
            },
            _ => return,
        };
        self.list.remove_rows(rows, row, 1);
        if !was_selected {
            return;
        }
        self.selected = None;
        self.bodies.cancel();
        match len {
            0 => self.reader.show_notice("Select a message to read it."),
            len => self.list.set_selection(&[row.min(len - 1)]),
        }
    }
}
