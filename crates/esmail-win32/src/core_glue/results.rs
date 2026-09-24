//! The messages a search found, which replace the folder's list while a search
//! is active.
//!
//! A hit can live in any account and folder, so unlike [`OpenFolder`] each row
//! remembers where its message is.
//!
//! [`OpenFolder`]: super::mailbox::OpenFolder

use std::sync::Arc;

use esmail::db::SearchHit;
use esmail::imap::MailHeader;
use esmail::view_model::RowModel;

use super::FolderRef;

/// What a search found, in the order the cache ranked it.
pub struct SearchResults {
    hits: Vec<(FolderRef, MailHeader)>,
    rows: Vec<RowModel>,
}

impl SearchResults {
    /// The hits whose account is one of `account_ids` (a hit of an account that
    /// was since removed cannot be opened).
    pub fn new(hits: Vec<SearchHit>, account_ids: &[String]) -> SearchResults {
        let hits: Vec<(FolderRef, MailHeader)> = hits
            .into_iter()
            .filter_map(|hit| {
                let account = account_ids.iter().position(|id| *id == hit.account_id)?;
                Some((FolderRef { account, mailbox: hit.mailbox }, hit.header))
            })
            .collect();
        let rows = hits.iter().map(|(_, header)| RowModel::from_header(header)).collect();
        SearchResults { hits, rows }
    }

    /// How many messages were found.
    pub fn len(&self) -> usize {
        self.hits.len()
    }

    /// Whether nothing was found.
    pub fn is_empty(&self) -> bool {
        self.hits.is_empty()
    }

    /// The rows for the list widget.
    pub fn rows(&self) -> Arc<[RowModel]> {
        self.rows.as_slice().into()
    }

    /// The folder and header behind list row `row`.
    pub fn get(&self, row: usize) -> Option<(&FolderRef, &MailHeader)> {
        self.hits.get(row).map(|(folder, header)| (folder, header))
    }

    fn position(&self, account: usize, mailbox: &str, uid: u32) -> Option<usize> {
        self.hits.iter().position(|(folder, header)| folder.account == account && folder.mailbox == mailbox && header.uid == uid)
    }

    /// Whether a message among the hits is marked read.
    pub fn seen(&self, account: usize, mailbox: &str, uid: u32) -> Option<bool> {
        self.position(account, mailbox, uid).map(|at| self.hits[at].1.is_seen())
    }

    /// Drops a message that left its folder. Returns the row it had.
    pub fn remove(&mut self, account: usize, mailbox: &str, uid: u32) -> Option<usize> {
        let at = self.position(account, mailbox, uid)?;
        self.hits.remove(at);
        self.rows.remove(at);
        Some(at)
    }

    /// Records the server's flags for a message, if it is among the hits.
    /// Returns whether a row changed.
    pub fn set_flags(&mut self, account: usize, mailbox: &str, uid: u32, flags: Vec<String>) -> bool {
        let Some(at) = self.position(account, mailbox, uid) else { return false };
        self.hits[at].1.flags = flags;
        self.rows[at] = RowModel::from_header(&self.hits[at].1);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(account_id: &str, mailbox: &str, uid: u32) -> SearchHit {
        SearchHit {
            account_id: account_id.to_string(),
            mailbox: mailbox.to_string(),
            header: MailHeader {
                uid,
                subject: format!("s{uid}"),
                from: String::new(),
                to: String::new(),
                date: String::new(),
                message_id: String::new(),
                flags: Vec::new(),
            },
        }
    }

    fn results() -> SearchResults {
        let ids = ["a".to_string(), "b".to_string()];
        SearchResults::new(vec![hit("b", "INBOX", 7), hit("gone", "INBOX", 1), hit("a", "Sent", 7)], &ids)
    }

    #[test]
    fn a_hit_knows_its_account_and_folder_and_a_removed_account_is_dropped() {
        let results = results();
        assert_eq!(results.len(), 2);
        let (folder, header) = results.get(0).unwrap();
        assert_eq!((folder.account, folder.mailbox.as_str(), header.uid), (1, "INBOX", 7));
        let (folder, _) = results.get(1).unwrap();
        assert_eq!((folder.account, folder.mailbox.as_str()), (0, "Sent"));
        assert_eq!(results.rows().len(), 2);
    }

    #[test]
    fn flags_are_matched_on_account_folder_and_uid_together() {
        let mut results = results();
        assert!(!results.set_flags(0, "INBOX", 7, vec!["\\Flagged".into()]));
        assert!(results.set_flags(0, "Sent", 7, vec!["\\Flagged".into()]));
        assert!(results.rows()[1].flagged);
        assert!(!results.rows()[0].flagged);
    }

    #[test]
    fn a_message_that_left_its_folder_is_dropped_from_the_hits() {
        let mut results = results();
        assert_eq!(results.remove(0, "Sent", 7), Some(1));
        assert_eq!((results.len(), results.rows().len()), (1, 1));
        assert_eq!(results.remove(0, "Sent", 7), None);
    }

    #[test]
    fn seen_reports_only_for_a_hit() {
        let results = results();
        assert_eq!(results.seen(1, "INBOX", 7), Some(false));
        assert_eq!(results.seen(1, "INBOX", 8), None);
    }
}
