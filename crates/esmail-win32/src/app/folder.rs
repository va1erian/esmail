//! The open folder: showing what the local cache holds at once, loading its
//! pages, refreshing it, and applying the result to the list.

use std::sync::Arc;

use esmail::imap::{ImapCommand, MailHeader};
use esmail::view_model::RowModel;
use esmail_win32::core_glue::mailbox::{Applied, Edit, OpenFolder};
use esmail_win32::core_glue::FolderRef;
use win32ui::{ControlExt, HasText, Ui};

use super::{App, Msg};

/// How many of a folder's newest cached messages are shown before the server
/// answers. More load as the user scrolls, from the server.
const CACHED_ROWS: usize = 1000;

impl App {
    pub(super) fn open_folder(&mut self, ui: &Ui<Msg>, folder: FolderRef) {
        self.bodies.cancel();
        self.selected = None;
        self.seen.cancel(ui);
        self.search.reset(ui);
        self.search_edit.set_text("");
        ui.set_title(&self.window_title(&folder));
        self.set_status(&format!("Loading {}...", folder.mailbox));
        self.list.set_rows(Arc::from([]));
        self.reader.show_notice("Select a message to read it.");
        self.core.cache().load_folder(folder.account, folder.mailbox.clone(), CACHED_ROWS);
        self.open = Some(OpenFolder::new(folder));
        self.request_page();
    }

    /// "INBOX - esMail", with the account named when there are several.
    fn window_title(&self, folder: &FolderRef) -> String {
        match self.core.accounts() {
            [_] | [] => format!("{} - esMail", folder.mailbox),
            accounts => format!("{} - {} - esMail", folder.mailbox, accounts[folder.account].display_name),
        }
    }

    /// The cache answered with a folder's newest messages: show them, unless the
    /// server got there first or the user has moved on.
    pub(super) fn seed_from_cache(&mut self, account: usize, mailbox: &str, headers: Vec<MailHeader>) {
        let Some(open) = self.open.as_mut().filter(|o| o.folder().account == account && o.folder().mailbox == mailbox) else { return };
        if !open.seed(headers) {
            return;
        }
        let rows = open.rows();
        if !self.search.active() {
            self.show_rows(rows, "cache");
        }
        self.folder_status();
        self.select_first_requested();
    }

    pub(super) fn request_page(&mut self) {
        let Some(open) = self.open.as_mut() else { return };
        let Some(request) = open.next_request() else { return };
        let folder = open.folder().clone();
        let sent = self.core.send(folder.account, ImapCommand::FetchHeaders { mailbox: folder.mailbox, page: request.page, req_id: request.id });
        if !sent {
            open.page_failed();
            self.account_unavailable(folder.account);
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
            self.account_unavailable(folder.account);
        }
        true
    }

    pub(super) fn apply_headers(&mut self, account: usize, mailbox: &str, req_id: u64, page: u32, total_pages: u32, headers: Vec<MailHeader>) {
        self.core.cache().index_headers(account, mailbox, &headers);
        let Some(open) = self.open.as_mut().filter(|o| o.folder().account == account && o.folder().mailbox == mailbox) else { return };
        let Some(applied) = open.apply_reply(req_id, page, total_pages, headers) else { return };
        let rows = open.rows();
        let keep_paging = matches!(applied, Applied::Page { added: 0 }) && !open.is_complete();
        // While a search's results are on screen the folder's model still
        // follows the server; the list shows it again when the search ends.
        if !self.search.active() {
            self.show_applied(applied, rows, page);
        }
        self.folder_status();
        let Some(open) = self.open.as_ref() else { return };
        let gone = self.selected.as_ref().is_some_and(|header| self.selected_in.as_ref() == Some(open.folder()) && open.row_of(header.uid).is_none());
        if gone {
            self.selected = None;
            self.bodies.cancel();
            self.reader.show_notice("This message is no longer in the folder.");
        }
        if keep_paging {
            // A page that only repeated cached rows adds nothing to scroll to,
            // so no scroll event would ask for the next one.
            self.request_page();
        }
    }

    fn show_applied(&mut self, applied: Applied, rows: Arc<[RowModel]>, page: u32) {
        match applied {
            Applied::Page { added } if page == 1 => {
                self.show_rows(rows, "server");
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
    }

    /// Replaces the list's rows, noting where the first ones came from.
    fn show_rows(&mut self, rows: Arc<[RowModel]>, source: &'static str) {
        self.startup.note_rows_from(source, rows.len());
        self.list.set_rows(rows);
    }

    /// The status line for the open folder.
    pub(super) fn folder_status(&self) {
        let Some(open) = self.open.as_ref() else { return };
        let mailbox = &open.folder().mailbox;
        let text = if open.is_cached_only() {
            format!("{mailbox}: {} cached messages, checking the server...", open.loaded())
        } else {
            let more = if open.is_complete() { "" } else { " (scroll for older)" };
            format!("{mailbox}: {} messages{more}", open.loaded())
        };
        self.set_status(&text);
    }

    /// `--select ROW` (for screenshots): opens that row once the first page is in.
    fn select_first_requested(&mut self) {
        if let Some(row) = self.select_after_load.take() {
            self.list.set_selection(&[row]);
            self.list.focus();
        }
    }
}
