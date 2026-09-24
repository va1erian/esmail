//! The folder whose messages the list shows, and how its pages accumulate.
//!
//! IMAP hands out headers 50 at a time, newest first. [`OpenFolder`] appends
//! each page as it arrives, asks for the next one only when the list says the
//! user is nearing the end, and drops a page that belongs to a folder (or a
//! refresh) the user already left. A refresh re-reads the newest page and
//! merges it into what is loaded, so new mail appears at the top without the
//! list losing its place.

use std::sync::Arc;

use esmail::imap::MailHeader;
use esmail::view_model::RowModel;

use super::{FolderRef, Latest};

/// A page to request: which one, and the id its reply will carry back.
#[derive(Debug, PartialEq, Eq)]
pub struct PageRequest {
    /// 1-based page number, as `ImapCommand::FetchHeaders` takes it.
    pub page: u32,
    /// Echoed on the reply; see [`OpenFolder::apply_reply`].
    pub id: u64,
}

/// One change a refresh made to the loaded rows. Apply the removals (listed
/// last row first, in the old numbering) and then the insertions (first row
/// first, in the new numbering) to keep a widget's selection and scroll
/// position on their messages.
#[derive(Debug, PartialEq, Eq)]
pub enum Edit {
    /// The message that was at row `at` is gone from the server.
    Removed {
        /// The row it occupied.
        at: usize,
    },
    /// A message arrived and is now at row `at`.
    Inserted {
        /// The row it occupies.
        at: usize,
    },
}

/// What a reply did to the loaded rows.
#[derive(Debug, PartialEq, Eq)]
pub enum Applied {
    /// A page of older messages joined the end.
    Page {
        /// How many rows were appended.
        added: usize,
    },
    /// A refresh merged in the newest page. Rows whose flags changed were
    /// updated in place, so an empty list of edits can still mean a change.
    Refreshed(Vec<Edit>),
}

/// The messages loaded so far for one folder.
pub struct OpenFolder {
    folder: FolderRef,
    headers: Vec<MailHeader>,
    rows: Vec<RowModel>,
    next_page: u32,
    total_pages: Option<u32>,
    ids: Latest,
    page_request: Option<u64>,
    refresh_request: Option<u64>,
}

impl OpenFolder {
    /// An empty folder view; call [`next_request`](Self::next_request) to load
    /// its first page.
    pub fn new(folder: FolderRef) -> Self {
        Self {
            folder,
            headers: Vec::new(),
            rows: Vec::new(),
            next_page: 1,
            total_pages: None,
            ids: Latest::default(),
            page_request: None,
            refresh_request: None,
        }
    }

    /// The folder being shown.
    pub fn folder(&self) -> &FolderRef {
        &self.folder
    }

    /// The next page to fetch, or `None` while one is in flight or the whole
    /// folder is loaded.
    pub fn next_request(&mut self) -> Option<PageRequest> {
        if self.page_request.is_some() || self.is_complete() {
            return None;
        }
        let id = self.ids.begin();
        self.page_request = Some(id);
        Some(PageRequest { page: self.next_page, id })
    }

    /// The id for a request that re-reads the newest page, or `None` until the
    /// first page has loaded (there is nothing to merge into yet). A refresh
    /// asked for while another is in flight supersedes it.
    pub fn refresh_request(&mut self) -> Option<u64> {
        if self.next_page == 1 {
            return None;
        }
        let id = self.ids.begin();
        self.refresh_request = Some(id);
        Some(id)
    }

    /// Applies a `FetchHeaders` reply, or returns `None` when it is stale.
    pub fn apply_reply(&mut self, id: u64, page: u32, total_pages: u32, headers: Vec<MailHeader>) -> Option<Applied> {
        if self.page_request == Some(id) {
            self.page_request = None;
            self.next_page = page + 1;
            self.total_pages = Some(total_pages);
            return Some(Applied::Page { added: self.append(headers) });
        }
        if self.refresh_request == Some(id) {
            self.refresh_request = None;
            self.total_pages = Some(total_pages);
            return Some(Applied::Refreshed(self.merge_newest(headers)));
        }
        None
    }

    /// The outstanding page request failed; allow it to be asked for again.
    pub fn page_failed(&mut self) {
        self.page_request = None;
    }

    /// Appends a page, skipping messages a refresh already pulled in (new mail
    /// pushes the last rows of the previous page onto the next one).
    fn append(&mut self, headers: Vec<MailHeader>) -> usize {
        let oldest = self.headers.last().map_or(u32::MAX, |header| header.uid);
        let fresh: Vec<MailHeader> = headers.into_iter().filter(|header| header.uid < oldest).collect();
        let added = fresh.len();
        self.rows.extend(fresh.iter().map(RowModel::from_header));
        self.headers.extend(fresh);
        added
    }

    /// Merges the newest page. Messages below the page's oldest uid were not
    /// re-read, so they are left alone; within its range, messages missing
    /// from the reply are gone from the server.
    fn merge_newest(&mut self, fresh: Vec<MailHeader>) -> Vec<Edit> {
        let floor = fresh.last().map_or(0, |header| header.uid);
        let mut edits = Vec::new();
        for at in (0..self.headers.len()).rev() {
            let uid = self.headers[at].uid;
            if uid >= floor && !fresh.iter().any(|header| header.uid == uid) {
                edits.push(Edit::Removed { at });
                self.headers.remove(at);
                self.rows.remove(at);
            }
        }
        for header in fresh {
            match self.row_of(header.uid) {
                Some(at) => {
                    self.rows[at] = RowModel::from_header(&header);
                    self.headers[at] = header;
                }
                None => {
                    let at = self.headers.iter().position(|old| old.uid < header.uid).unwrap_or(self.headers.len());
                    self.rows.insert(at, RowModel::from_header(&header));
                    self.headers.insert(at, header);
                    edits.push(Edit::Inserted { at });
                }
            }
        }
        edits
    }

    /// The row of message `uid`, if it is loaded.
    pub fn row_of(&self, uid: u32) -> Option<usize> {
        self.headers.iter().position(|header| header.uid == uid)
    }

    /// Records the server's flags for `uid`. Returns its row, or `None` when
    /// the message is not loaded.
    pub fn set_flags(&mut self, uid: u32, flags: Vec<String>) -> Option<usize> {
        let at = self.row_of(uid)?;
        self.headers[at].flags = flags;
        self.rows[at] = RowModel::from_header(&self.headers[at]);
        Some(at)
    }

    /// Drops `uid` (it was moved out of the folder). Returns the row it had.
    pub fn remove(&mut self, uid: u32) -> Option<usize> {
        let at = self.row_of(uid)?;
        self.headers.remove(at);
        self.rows.remove(at);
        Some(at)
    }

    /// The header behind list row `row`.
    pub fn header(&self, row: usize) -> Option<&MailHeader> {
        self.headers.get(row)
    }

    /// The rows for the list widget.
    pub fn rows(&self) -> Arc<[RowModel]> {
        self.rows.as_slice().into()
    }

    /// How many messages are loaded.
    pub fn loaded(&self) -> usize {
        self.headers.len()
    }

    /// Whether every page has been loaded.
    pub fn is_complete(&self) -> bool {
        self.total_pages.is_some_and(|total| self.next_page > total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder() -> OpenFolder {
        OpenFolder::new(FolderRef { account: 0, mailbox: "INBOX".into() })
    }

    /// Headers for `uids`, newest first as a server sends them.
    fn headers(uids: std::ops::Range<u32>) -> Vec<MailHeader> {
        uids.rev()
            .map(|uid| MailHeader {
                uid,
                subject: format!("s{uid}"),
                from: String::new(),
                to: String::new(),
                date: String::new(),
                message_id: String::new(),
                flags: Vec::new(),
            })
            .collect()
    }

    /// A folder whose first page holds `uids` and which has a second page.
    fn loaded(uids: std::ops::Range<u32>) -> OpenFolder {
        let mut open = folder();
        let first = open.next_request().unwrap();
        open.apply_reply(first.id, 1, 2, headers(uids));
        open
    }

    fn uids(open: &OpenFolder) -> Vec<u32> {
        (0..open.loaded()).map(|row| open.header(row).unwrap().uid).collect()
    }

    #[test]
    fn the_first_request_is_page_one() {
        assert_eq!(folder().next_request(), Some(PageRequest { page: 1, id: 1 }));
    }

    #[test]
    fn no_second_request_while_one_is_in_flight() {
        let mut open = folder();
        open.next_request().unwrap();
        assert_eq!(open.next_request(), None);
    }

    #[test]
    fn pages_append_in_order_and_the_next_request_follows() {
        let mut open = folder();
        let first = open.next_request().unwrap();
        assert_eq!(open.apply_reply(first.id, 1, 2, headers(100..150)), Some(Applied::Page { added: 50 }));
        let second = open.next_request().unwrap();
        assert_eq!(second.page, 2);
        assert_eq!(open.apply_reply(second.id, 2, 2, headers(50..60)), Some(Applied::Page { added: 10 }));
        assert_eq!(open.loaded(), 60);
        assert_eq!(open.header(50).unwrap().uid, 59);
        assert_eq!(open.rows().len(), 60);
        assert!(open.is_complete());
        assert_eq!(open.next_request(), None);
    }

    #[test]
    fn a_reply_that_is_not_the_latest_request_is_dropped() {
        let mut open = folder();
        let stale = open.next_request().unwrap();
        open.page_failed();
        let fresh = open.next_request().unwrap();
        assert_eq!(open.apply_reply(stale.id, 1, 1, headers(1..3)), None);
        assert_eq!(open.loaded(), 0);
        assert_eq!(open.apply_reply(fresh.id, 1, 1, headers(1..3)), Some(Applied::Page { added: 2 }));
    }

    #[test]
    fn a_failed_page_can_be_requested_again() {
        let mut open = folder();
        open.next_request().unwrap();
        open.page_failed();
        assert_eq!(open.next_request().map(|r| r.page), Some(1));
    }

    #[test]
    fn an_empty_folder_is_complete_after_its_first_page() {
        let mut open = folder();
        let first = open.next_request().unwrap();
        open.apply_reply(first.id, 1, 0, Vec::new());
        assert!(open.is_complete());
    }

    #[test]
    fn nothing_can_be_refreshed_before_the_first_page() {
        assert_eq!(folder().refresh_request(), None);
    }

    #[test]
    fn a_refresh_puts_new_mail_at_the_top_and_reports_where() {
        let mut open = loaded(10..20);
        let id = open.refresh_request().unwrap();
        let applied = open.apply_reply(id, 1, 2, headers(12..22));
        assert_eq!(applied, Some(Applied::Refreshed(vec![Edit::Inserted { at: 0 }, Edit::Inserted { at: 1 }])));
        assert_eq!(uids(&open), [21, 20, 19, 18, 17, 16, 15, 14, 13, 12, 11, 10]);
        assert_eq!(open.rows().len(), 12);
    }

    #[test]
    fn a_refresh_updates_flags_in_place_without_edits() {
        let mut open = loaded(10..13);
        let id = open.refresh_request().unwrap();
        let mut fresh = headers(10..13);
        fresh[0].flags = vec!["\\Seen".into()];
        assert_eq!(open.apply_reply(id, 1, 1, fresh), Some(Applied::Refreshed(Vec::new())));
        assert!(open.header(0).unwrap().is_seen());
        assert!(open.rows()[0].seen);
    }

    #[test]
    fn a_refresh_drops_messages_the_server_no_longer_has_within_the_page() {
        let mut open = loaded(10..15);
        let id = open.refresh_request().unwrap();
        let mut fresh = headers(10..15);
        fresh.remove(1);
        assert_eq!(open.apply_reply(id, 1, 1, fresh), Some(Applied::Refreshed(vec![Edit::Removed { at: 1 }])));
        assert_eq!(uids(&open), [14, 12, 11, 10]);
    }

    #[test]
    fn a_refresh_leaves_older_loaded_pages_alone() {
        let mut open = loaded(100..150);
        let second = open.next_request().unwrap();
        open.apply_reply(second.id, 2, 2, headers(50..60));
        let id = open.refresh_request().unwrap();
        let applied = open.apply_reply(id, 1, 2, headers(101..151));
        assert_eq!(applied, Some(Applied::Refreshed(vec![Edit::Inserted { at: 0 }])));
        assert_eq!(open.loaded(), 61);
        assert_eq!(open.header(51).unwrap().uid, 59);
    }

    #[test]
    fn a_later_page_skips_messages_a_refresh_already_holds() {
        let mut open = loaded(100..150);
        let id = open.refresh_request().unwrap();
        open.apply_reply(id, 1, 2, headers(102..152));
        let second = open.next_request().unwrap();
        assert_eq!(open.apply_reply(second.id, 2, 2, headers(52..102)), Some(Applied::Page { added: 48 }));
        let all = uids(&open);
        assert!(all.windows(2).all(|pair| pair[0] > pair[1]), "newest first, no duplicates");
    }

    #[test]
    fn a_refresh_reply_that_was_superseded_is_dropped() {
        let mut open = loaded(10..12);
        let old = open.refresh_request().unwrap();
        let new = open.refresh_request().unwrap();
        assert_eq!(open.apply_reply(old, 1, 1, headers(10..12)), None);
        assert!(open.apply_reply(new, 1, 1, headers(10..12)).is_some());
    }

    #[test]
    fn flags_and_removal_address_a_message_by_uid() {
        let mut open = loaded(10..14);
        assert_eq!(open.set_flags(12, vec!["\\Flagged".into()]), Some(1));
        assert!(open.rows()[1].flagged);
        assert_eq!(open.remove(13), Some(0));
        assert_eq!(uids(&open), [12, 11, 10]);
        assert_eq!(open.remove(99), None);
    }
}
