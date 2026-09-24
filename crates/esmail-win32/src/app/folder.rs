//! The open folder: loading its pages, refreshing it, and applying the result
//! to the list.

use std::sync::Arc;

use esmail::imap::{ImapCommand, MailHeader};
use esmail_win32::core_glue::mailbox::{Applied, Edit, OpenFolder};
use esmail_win32::core_glue::FolderRef;
use win32ui::{ControlExt, Ui};

use super::{App, Msg};

impl App {
    pub(super) fn open_folder(&mut self, ui: &Ui<Msg>, folder: FolderRef) {
        self.bodies.cancel();
        self.selected = None;
        self.seen.cancel(ui);
        ui.set_title(&format!("{} - esMail", folder.mailbox));
        self.set_status(&format!("Loading {}...", folder.mailbox));
        self.list.set_rows(Arc::from([]));
        self.reader.show_notice("Select a message to read it.");
        self.open = Some(OpenFolder::new(folder));
        self.request_page();
    }

    pub(super) fn request_page(&mut self) {
        let Some(open) = self.open.as_mut() else { return };
        let Some(request) = open.next_request() else { return };
        let folder = open.folder().clone();
        let sent = self.core.send(folder.account, ImapCommand::FetchHeaders { mailbox: folder.mailbox, page: request.page, req_id: request.id });
        if !sent {
            open.page_failed();
            self.banner("this account is not connected");
        }
    }

    /// Re-reads the newest page and every folder's unread count (F5). Falls
    /// back to opening the folder afresh when its first page never arrived.
    pub(super) fn refresh(&mut self, ui: &Ui<Msg>) {
        let Some(folder) = self.open.as_ref().map(|open| open.folder().clone()) else { return };
        self.request_unread_counts(folder.account, None);
        if !self.request_refresh() {
            self.open_folder(ui, folder);
        }
    }

    /// Asks for the newest page to merge into the list. Returns whether there
    /// was a loaded list to merge into.
    pub(super) fn request_refresh(&mut self) -> bool {
        let Some(open) = self.open.as_mut() else { return false };
        let Some(id) = open.refresh_request() else { return false };
        let folder = open.folder().clone();
        if !self.core.send(folder.account, ImapCommand::FetchHeaders { mailbox: folder.mailbox, page: 1, req_id: id }) {
            self.banner("this account is not connected");
        }
        true
    }

    pub(super) fn apply_headers(&mut self, account: usize, mailbox: &str, req_id: u64, page: u32, total_pages: u32, headers: Vec<MailHeader>) {
        let Some(open) = self.open.as_mut().filter(|o| o.folder().account == account && o.folder().mailbox == mailbox) else { return };
        let Some(applied) = open.apply_reply(req_id, page, total_pages, headers) else { return };
        let rows = open.rows();
        match applied {
            Applied::Page { added } if page == 1 => {
                self.list.set_rows(rows);
                if added > 0 {
                    self.select_first_requested();
                }
            }
            Applied::Page { .. } => self.list.extend_rows(rows),
            Applied::Refreshed(edits) if edits.is_empty() => self.list.update_rows(rows),
            Applied::Refreshed(edits) => {
                for edit in edits {
                    match edit {
                        Edit::Removed { at } => self.list.remove_rows(rows.clone(), at, 1),
                        Edit::Inserted { at } => self.list.insert_rows(rows.clone(), at, 1),
                    }
                }
            }
        }
        let Some(open) = self.open.as_ref() else { return };
        let more = if open.is_complete() { "" } else { " (scroll for older)" };
        let text = format!("{mailbox}: {} messages{more}", open.loaded());
        let gone = self.selected.as_ref().is_some_and(|header| open.row_of(header.uid).is_none());
        self.set_status(&text);
        if gone {
            self.selected = None;
            self.bodies.cancel();
            self.reader.show_notice("This message is no longer in the folder.");
        }
    }

    /// `--select ROW` (for screenshots): opens that row once the first page is in.
    fn select_first_requested(&mut self) {
        if let Some(row) = self.select_after_load.take() {
            self.list.set_selection(&[row]);
            self.list.focus();
        }
    }
}
