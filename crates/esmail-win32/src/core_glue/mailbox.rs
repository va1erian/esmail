//! The folder whose messages the list shows, and how its pages accumulate.
//!
//! IMAP hands out headers 50 at a time, newest first. [`OpenFolder`] appends
//! each page as it arrives, asks for the next one only when the list says the
//! user is nearing the end, and drops a page that belongs to a folder (or a
//! refresh) the user already left.

use std::sync::Arc;

use esmail::imap::MailHeader;
use esmail::view_model::RowModel;

use super::{FolderRef, Latest};

/// A page to request: which one, and the id its reply will carry back.
#[derive(Debug, PartialEq, Eq)]
pub struct PageRequest {
    /// 1-based page number, as `ImapCommand::FetchHeaders` takes it.
    pub page: u32,
    /// Echoed on the reply; see [`OpenFolder::apply_page`].
    pub id: u64,
}

/// The messages loaded so far for one folder.
pub struct OpenFolder {
    folder: FolderRef,
    headers: Vec<MailHeader>,
    rows: Vec<RowModel>,
    next_page: u32,
    total_pages: Option<u32>,
    in_flight: bool,
    requests: Latest,
}

impl OpenFolder {
    /// An empty folder view; call [`next_request`](Self::next_request) to load
    /// its first page.
    pub fn new(folder: FolderRef) -> Self {
        Self { folder, headers: Vec::new(), rows: Vec::new(), next_page: 1, total_pages: None, in_flight: false, requests: Latest::default() }
    }

    /// The folder being shown.
    pub fn folder(&self) -> &FolderRef {
        &self.folder
    }

    /// The next page to fetch, or `None` while one is in flight or the whole
    /// folder is loaded.
    pub fn next_request(&mut self) -> Option<PageRequest> {
        if self.in_flight || self.total_pages.is_some_and(|total| self.next_page > total) {
            return None;
        }
        self.in_flight = true;
        Some(PageRequest { page: self.next_page, id: self.requests.begin() })
    }

    /// Appends a reply's headers. Returns how many rows were added, or `None`
    /// when the reply is stale.
    pub fn apply_page(&mut self, id: u64, page: u32, total_pages: u32, headers: Vec<MailHeader>) -> Option<usize> {
        if !self.requests.is_current(id) {
            return None;
        }
        self.in_flight = false;
        self.next_page = page + 1;
        self.total_pages = Some(total_pages);
        let added = headers.len();
        self.rows.extend(headers.iter().map(RowModel::from_header));
        self.headers.extend(headers);
        Some(added)
    }

    /// The outstanding page request failed; allow it to be asked for again.
    pub fn page_failed(&mut self) {
        self.in_flight = false;
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

    fn headers(uids: std::ops::Range<u32>) -> Vec<MailHeader> {
        uids.map(|uid| MailHeader {
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
        assert_eq!(open.apply_page(first.id, 1, 2, headers(100..150)), Some(50));
        let second = open.next_request().unwrap();
        assert_eq!(second.page, 2);
        assert_eq!(open.apply_page(second.id, 2, 2, headers(50..60)), Some(10));
        assert_eq!(open.loaded(), 60);
        assert_eq!(open.header(50).unwrap().uid, 50);
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
        assert_eq!(open.apply_page(stale.id, 1, 1, headers(1..3)), None);
        assert_eq!(open.loaded(), 0);
        assert_eq!(open.apply_page(fresh.id, 1, 1, headers(1..3)), Some(2));
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
        open.apply_page(first.id, 1, 0, Vec::new());
        assert!(open.is_complete());
    }
}
