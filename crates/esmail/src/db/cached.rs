//! Reading the cache back for a frontend that shows it before the network
//! answers, and indexing headers so what it caches is findable by subject and
//! sender.
//!
//! [`CacheReader`] is a read-only connection of its own, so a frontend can load
//! the newest cached messages of a folder on any thread without going through
//! the [`DbActor`](super::DbActor)'s command queue (which may be busy writing).

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use super::{index_headers, parse_flags_column};
use crate::imap::MailHeader;

/// A read-only view of the mail cache.
pub struct CacheReader {
    conn: Connection,
}

impl CacheReader {
    /// Opens the cache read-only. `Ok(None)` when there is no cache yet (a
    /// first run) or it has no `messages` table, so callers show an empty
    /// folder rather than an error.
    pub fn open() -> Result<Option<CacheReader>, String> {
        let Some(path) = crate::paths::db_file().filter(|path| path.exists()) else { return Ok(None) };
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
            .map_err(|e| format!("could not open {}: {e}", path.display()))?;
        let has_messages = conn
            .query_row("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'messages'", [], |_| Ok(()))
            .optional()
            .map_err(|e| e.to_string())?
            .is_some();
        Ok(has_messages.then_some(CacheReader { conn }))
    }

    /// The names of the mailboxes that have cached messages for `account_id`.
    pub fn mailboxes(&self, account_id: &str) -> Result<Vec<String>, String> {
        mailboxes(&self.conn, account_id).map_err(|e| e.to_string())
    }

    /// The `limit` newest cached messages of `mailbox`, newest (highest uid)
    /// first, the order the server pages them in.
    pub fn newest(&self, account_id: &str, mailbox: &str, limit: usize) -> Result<Vec<MailHeader>, String> {
        newest(&self.conn, account_id, mailbox, limit).map_err(|e| e.to_string())
    }
}

fn mailboxes(conn: &Connection, account_id: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT DISTINCT mailbox FROM messages WHERE account_id = ?1 ORDER BY mailbox")?;
    stmt.query_map(params![account_id], |row| row.get(0))?.collect()
}

fn newest(conn: &Connection, account_id: &str, mailbox: &str, limit: usize) -> rusqlite::Result<Vec<MailHeader>> {
    let mut stmt = conn.prepare_cached(
        "SELECT uid, subject, from_addr, to_addr, date, message_id, flags
         FROM messages WHERE account_id = ?1 AND mailbox = ?2
         ORDER BY uid DESC LIMIT ?3",
    )?;
    stmt.query_map(params![account_id, mailbox, limit as i64], |row| {
        let flags: String = row.get(6)?;
        Ok(MailHeader {
            uid: row.get(0)?,
            subject: row.get(1)?,
            from: row.get(2)?,
            to: row.get(3)?,
            date: row.get(4)?,
            message_id: row.get(5)?,
            flags: parse_flags_column(&flags),
        })
    })?
    .collect()
}

/// [`index_headers`] plus, for a message not cached before, an FTS row with no
/// body, so it turns up in a search by subject, sender or recipient. A message
/// that is already known keeps whatever FTS row (with or without a body) it
/// has; the row is replaced when `index_mail` caches its body.
pub(super) fn index_headers_searchable(conn: &Connection, account_id: &str, mailbox: &str, headers: &[MailHeader]) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    for header in headers {
        let known = tx
            .query_row(
                "SELECT 1 FROM messages WHERE account_id = ?1 AND mailbox = ?2 AND uid = ?3",
                params![account_id, mailbox, header.uid],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        index_headers(&tx, account_id, mailbox, std::slice::from_ref(header))?;
        if !known {
            tx.execute(
                "INSERT INTO messages_fts (account_id, mailbox, uid, subject, from_addr, to_addr, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6, '')",
                params![account_id, mailbox, header.uid, header.subject, header.from, header.to],
            )?;
        }
    }
    tx.commit()
}

#[cfg(test)]
mod tests {
    use super::super::{init_schema, search};
    use super::*;
    use crate::search_query::ParsedQuery;

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn
    }

    fn header(uid: u32, subject: &str) -> MailHeader {
        MailHeader {
            uid,
            subject: subject.to_string(),
            from: "alice@example.com".to_string(),
            to: "bob@example.com".to_string(),
            date: "2026-01-01".to_string(),
            message_id: format!("<{uid}@example.com>"),
            flags: vec!["\\Seen".to_string()],
        }
    }

    #[test]
    fn the_newest_messages_come_back_highest_uid_first_and_capped() {
        let conn = conn();
        let headers: Vec<MailHeader> = (1..=10).map(|uid| header(uid, "hello")).collect();
        index_headers_searchable(&conn, "acc", "INBOX", &headers).unwrap();
        index_headers_searchable(&conn, "acc", "Other", &[header(99, "elsewhere")]).unwrap();

        let newest = newest(&conn, "acc", "INBOX", 3).unwrap();
        assert_eq!(newest.iter().map(|h| h.uid).collect::<Vec<_>>(), [10, 9, 8]);
        assert_eq!((newest[0].subject.as_str(), newest[0].flags.as_slice()), ("hello", ["\\Seen".to_string()].as_slice()));
    }

    #[test]
    fn mailboxes_lists_each_cached_folder_of_the_account_once() {
        let conn = conn();
        index_headers_searchable(&conn, "acc", "INBOX", &[header(1, "a"), header(2, "b")]).unwrap();
        index_headers_searchable(&conn, "acc", "Sent", &[header(1, "c")]).unwrap();
        index_headers_searchable(&conn, "other", "Junk", &[header(1, "d")]).unwrap();
        assert_eq!(mailboxes(&conn, "acc").unwrap(), ["INBOX", "Sent"]);
        assert!(mailboxes(&conn, "nobody").unwrap().is_empty());
    }

    #[test]
    fn a_header_only_message_is_found_by_its_subject() {
        let conn = conn();
        index_headers_searchable(&conn, "acc", "INBOX", &[header(1, "quarterly invoice"), header(2, "lunch")]).unwrap();
        let hits = search(&conn, None, &ParsedQuery::parse("invoice"), None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].header.uid, 1);
    }

    #[test]
    fn re_indexing_a_known_message_does_not_duplicate_its_search_row() {
        let conn = conn();
        index_headers_searchable(&conn, "acc", "INBOX", &[header(1, "invoice")]).unwrap();
        index_headers_searchable(&conn, "acc", "INBOX", &[header(1, "invoice")]).unwrap();
        let hits = search(&conn, None, &ParsedQuery::parse("invoice"), None).unwrap();
        assert_eq!(hits.len(), 1);
    }
}
