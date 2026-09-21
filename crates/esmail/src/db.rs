//! Local SQLite cache: message metadata, cached bodies, and a full-text
//! index, keyed by `(account_id, mailbox, uid)`.
//!
//! B3 of PLAN.md. What's here: the relational schema (`mailboxes`,
//! `messages`, `bodies`), an LRU cap on cached bodies, and the pure
//! `sync_decision` this needs to eventually drive incremental sync. What's
//! **not** here yet: anything that actually issues the incremental IMAP
//! fetch a `FetchFrom` decision calls for — `imap.rs` reports the
//! UIDVALIDITY/UIDNEXT it already reads off `session.examine()`, `db.rs`
//! records it and computes the decision, but nothing acts on `FetchFrom` by
//! requesting more messages yet. `BulkDownload` still pulls the whole
//! mailbox every time. See PLAN.md §B3 for why that part waited.

use rusqlite::{params, Connection};
use tokio::sync::mpsc;
use crate::imap::MailHeader;

/// Cap on rows in `bodies` across all accounts/mailboxes; the oldest
/// (by `cached_at`) are evicted once a write pushes past it.
const MAX_CACHED_BODIES: i64 = 2000;

pub enum DbCommand {
    IndexMail {
        account_id: String,
        mailbox: String,
        header: MailHeader,
        body: String,
    },
    /// Metadata-only counterpart to `IndexMail` (B3): upserts `messages` for
    /// each header without a body to cache, since these come from
    /// `ImapCommand::FetchHeadersFrom`'s envelope-only fetch -- the response
    /// to a `SyncPlan::FetchFrom`/`Resync` decision, acted on for the first
    /// time in B3. Deliberately does not touch `bodies`/`messages_fts`: a
    /// row with no body cached should not become findable-by-body-text
    /// (nor should it clobber an existing cached body/FTS row with an empty
    /// one) until something actually fetches and indexes that message's
    /// body -- today only `IndexMail`, i.e. `BulkDownload`. See PLAN.md §B3
    /// for the still-open gap that opening a single message via `FetchBody`
    /// doesn't index it either.
    IndexHeaders {
        account_id: String,
        mailbox: String,
        headers: Vec<MailHeader>,
    },
    /// Full-text search. `account_id: None` searches every account; either
    /// way each hit says which account and mailbox it came from.
    Search {
        account_id: Option<String>,
        query: String,
        mailbox: Option<String>,
    },
    FetchMail {
        account_id: String,
        mailbox: String,
        uid: u32,
    },
    /// Report what the server said about a mailbox on the most recent
    /// `EXAMINE`/`SELECT` (`imap.rs` already reads `uid_validity`/`uid_next`
    /// off the `Mailbox` it gets back from `session.examine()` — this just
    /// forwards it). Answered with a [`DbEvent::SyncPlan`].
    ReportMailboxState {
        account_id: String,
        mailbox: String,
        uid_validity: u32,
        uid_next: u32,
    },
    /// Mirror an IMAP `STORE`'s resulting flags into the local cache (B8) —
    /// sent once `ImapEvent::FlagsUpdated` confirms the server accepted a
    /// `\Seen`/`\Flagged`/`\Deleted` change, so a page rendered from the
    /// cache (or a later `search`) reflects it without waiting for the next
    /// full header re-fetch. Best-effort: silently a no-op if this
    /// `(account_id, mailbox, uid)` was never cached (nothing to update).
    UpdateFlags {
        account_id: String,
        mailbox: String,
        uid: u32,
        flags: Vec<String>,
    },
    /// Drop a message from the local cache (B8) — sent once
    /// `ImapEvent::Moved` confirms a move-to-Trash/Archive succeeded, so the
    /// cache doesn't keep showing a message under a mailbox it no longer
    /// lives in.
    RemoveMessage {
        account_id: String,
        mailbox: String,
        uid: u32,
    },
}

pub enum DbEvent {
    SearchResult { hits: Vec<SearchHit> },
    MailFetched { header: MailHeader, body: String },
    /// `FetchMail` (a cached search-result open) found no cached body --
    /// typically because `MAX_CACHED_BODIES`'s LRU cap evicted it since it
    /// was indexed, which is routine on a large mailbox. Carries `uid` (not
    /// just a generic `Error`) so `main.rs` can tell this apart from an
    /// unrelated DB error and fall back to a live `FetchBody` instead of
    /// leaving "Loading message..." on screen forever -- see the root-cause
    /// writeup on `main.rs`'s `DbEvent::MailFetchFailed` arm.
    MailFetchFailed { uid: u32, error: String },
    SyncPlan { account_id: String, mailbox: String, plan: SyncPlan },
    Error(String),
}

/// What should happen to bring a mailbox's local cache up to date, given what
/// the server just reported vs. what was last stored for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncPlan {
    /// UIDVALIDITY is unchanged from last time and UIDNEXT didn't move: the
    /// cache already has everything the server has.
    UpToDate,
    /// UIDVALIDITY is unchanged; UIDs in `first..last_known_uidnext` (both
    /// exclusive of `first_new_uid`) may exist on the server but not locally.
    /// `first_new_uid` is the first UID worth fetching.
    FetchFrom { first_new_uid: u32 },
    /// UIDVALIDITY changed since we last saw this mailbox: the server has
    /// reassigned UIDs, so every UID this cache has for it means nothing
    /// anymore. The mailbox's cached messages and bodies were wiped as part
    /// of computing this plan; start a full resync from UID 1.
    Resync,
}

pub struct DbActor {
    cmd_rx: mpsc::Receiver<DbCommand>,
    event_tx: mpsc::Sender<DbEvent>,
    conn: Connection,
}

impl DbActor {
    pub fn spawn(
        cmd_rx: mpsc::Receiver<DbCommand>,
        event_tx: mpsc::Sender<DbEvent>,
    ) {
        tokio::task::spawn_blocking(move || {
            let conn = match Connection::open("mails.db") {
                Ok(c) => c,
                Err(e) => {
                    let _ = event_tx.blocking_send(DbEvent::Error(e.to_string()));
                    return;
                }
            };

            if let Err(e) = init_schema(&conn) {
                let _ = event_tx.blocking_send(DbEvent::Error(e.to_string()));
                return;
            }

            let mut actor = DbActor {
                cmd_rx,
                event_tx,
                conn,
            };

            actor.run();
        });
    }

    fn run(&mut self) {
        while let Some(cmd) = self.cmd_rx.blocking_recv() {
            match cmd {
                DbCommand::IndexMail { account_id, mailbox, header, body } => {
                    if let Err(e) = index_mail(&self.conn, &account_id, &mailbox, &header, &body) {
                        let _ = self.event_tx.blocking_send(DbEvent::Error(e.to_string()));
                    }
                }
                DbCommand::IndexHeaders { account_id, mailbox, headers } => {
                    if let Err(e) = index_headers(&self.conn, &account_id, &mailbox, &headers) {
                        let _ = self.event_tx.blocking_send(DbEvent::Error(e.to_string()));
                    }
                }
                DbCommand::Search { account_id, query, mailbox } => {
                    match search(&self.conn, account_id.as_deref(), &query, mailbox.as_deref()) {
                        Ok(hits) => {
                            let _ = self.event_tx.blocking_send(DbEvent::SearchResult { hits });
                        }
                        Err(e) => {
                            let _ = self.event_tx.blocking_send(DbEvent::Error(e.to_string()));
                        }
                    }
                }
                DbCommand::FetchMail { account_id, mailbox, uid } => {
                    match fetch_mail(&self.conn, &account_id, &mailbox, uid) {
                        Ok((header, body)) => {
                            let _ = self.event_tx.blocking_send(DbEvent::MailFetched { header, body });
                        }
                        Err(e) => {
                            let _ = self.event_tx.blocking_send(DbEvent::MailFetchFailed { uid, error: e.to_string() });
                        }
                    }
                }
                DbCommand::ReportMailboxState { account_id, mailbox, uid_validity, uid_next } => {
                    match report_mailbox_state(&self.conn, &account_id, &mailbox, uid_validity, uid_next) {
                        Ok(plan) => {
                            let _ = self.event_tx.blocking_send(DbEvent::SyncPlan { account_id, mailbox, plan });
                        }
                        Err(e) => {
                            let _ = self.event_tx.blocking_send(DbEvent::Error(e.to_string()));
                        }
                    }
                }
                DbCommand::UpdateFlags { account_id, mailbox, uid, flags } => {
                    if let Err(e) = update_flags(&self.conn, &account_id, &mailbox, uid, &flags) {
                        let _ = self.event_tx.blocking_send(DbEvent::Error(e.to_string()));
                    }
                }
                DbCommand::RemoveMessage { account_id, mailbox, uid } => {
                    if let Err(e) = remove_message(&self.conn, &account_id, &mailbox, uid) {
                        let _ = self.event_tx.blocking_send(DbEvent::Error(e.to_string()));
                    }
                }
            }
        }
    }
}

fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS mailboxes (
            account_id      TEXT NOT NULL,
            mailbox         TEXT NOT NULL,
            uid_validity    INTEGER NOT NULL,
            uid_next        INTEGER NOT NULL,
            highest_modseq  INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (account_id, mailbox)
        );

        CREATE TABLE IF NOT EXISTS messages (
            account_id  TEXT NOT NULL,
            mailbox     TEXT NOT NULL,
            uid         INTEGER NOT NULL,
            subject     TEXT NOT NULL,
            from_addr   TEXT NOT NULL,
            to_addr     TEXT NOT NULL,
            date        TEXT NOT NULL,
            message_id  TEXT NOT NULL DEFAULT '',
            size        INTEGER NOT NULL DEFAULT 0,
            flags       TEXT NOT NULL DEFAULT '',
            thread_key  TEXT,
            PRIMARY KEY (account_id, mailbox, uid)
        );

        CREATE TABLE IF NOT EXISTS bodies (
            account_id  TEXT NOT NULL,
            mailbox     TEXT NOT NULL,
            uid         INTEGER NOT NULL,
            body        TEXT NOT NULL,
            cached_at   INTEGER NOT NULL,
            PRIMARY KEY (account_id, mailbox, uid)
        );

        CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
            account_id UNINDEXED,
            mailbox UNINDEXED,
            uid UNINDEXED,
            subject,
            from_addr,
            to_addr,
            body
        );
        ",
    )?;
    add_message_id_column_if_missing(conn)?;
    add_flags_column_if_missing(conn)
}

/// `messages.message_id` (B7) was added after `messages` itself (B3).
/// `CREATE TABLE IF NOT EXISTS` only creates a table that doesn't exist yet
/// at all — it does nothing to a `messages` table an earlier build of this
/// app already created without the column, which is exactly the local
/// `mails.db` this session's own B3-B6 testing left behind. Without this,
/// every `INSERT INTO messages (..., message_id, ...)` in `index_mail` would
/// fail against that file with "table messages has no column named
/// message_id" the first time a message was indexed.
fn add_message_id_column_if_missing(conn: &Connection) -> rusqlite::Result<()> {
    match conn.execute("ALTER TABLE messages ADD COLUMN message_id TEXT NOT NULL DEFAULT ''", []) {
        Ok(_) => Ok(()),
        // SQLite has no "ALTER TABLE ... ADD COLUMN IF NOT EXISTS"; detect
        // the column already being there by its own error text instead.
        Err(rusqlite::Error::SqliteFailure(_, Some(msg))) if msg.contains("duplicate column name") => Ok(()),
        Err(e) => Err(e),
    }
}

/// `messages.flags` (B8) was added after `messages` itself (B3), same
/// situation as `message_id` above: `CREATE TABLE IF NOT EXISTS` does
/// nothing to a `messages` table an earlier build already created without
/// this column (e.g. one from between B7 and B8, which has `message_id`
/// but not `flags`). Without this, `index_headers`/`index_mail`'s
/// `INSERT INTO messages (..., flags)` and `search`/`fetch_mail`'s
/// `SELECT ... m.flags` would fail with "table messages has no column
/// named flags" against such a file.
fn add_flags_column_if_missing(conn: &Connection) -> rusqlite::Result<()> {
    match conn.execute("ALTER TABLE messages ADD COLUMN flags TEXT NOT NULL DEFAULT ''", []) {
        Ok(_) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(_, Some(msg))) if msg.contains("duplicate column name") => Ok(()),
        Err(e) => Err(e),
    }
}

/// Insert or update several messages' metadata only, with no body to cache
/// (B3) -- see [`DbCommand::IndexHeaders`]'s doc for why this leaves
/// `bodies`/`messages_fts` untouched. `size` is left at whatever it already
/// was (0 for a never-seen row, via `messages`' own column default) rather
/// than being reset to 0 on every re-sync of an already-known message.
fn index_headers(
    conn: &Connection,
    account_id: &str,
    mailbox: &str,
    headers: &[MailHeader],
) -> rusqlite::Result<()> {
    for header in headers {
        conn.execute(
            "INSERT INTO messages (account_id, mailbox, uid, subject, from_addr, to_addr, date, message_id, size, flags)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, ?9)
             ON CONFLICT (account_id, mailbox, uid) DO UPDATE SET
                subject = excluded.subject,
                from_addr = excluded.from_addr,
                to_addr = excluded.to_addr,
                date = excluded.date,
                message_id = excluded.message_id,
                flags = excluded.flags",
            params![account_id, mailbox, header.uid, header.subject, header.from, header.to, header.date, header.message_id, flags_column(header)],
        )?;
    }
    Ok(())
}

/// Insert or update one message's metadata, cached body, and FTS row. Safe to
/// call repeatedly for the same `(account_id, mailbox, uid)` — every table
/// upserts rather than duplicating.
fn index_mail(
    conn: &Connection,
    account_id: &str,
    mailbox: &str,
    header: &MailHeader,
    body: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO messages (account_id, mailbox, uid, subject, from_addr, to_addr, date, message_id, size, flags)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT (account_id, mailbox, uid) DO UPDATE SET
            subject = excluded.subject,
            from_addr = excluded.from_addr,
            to_addr = excluded.to_addr,
            date = excluded.date,
            message_id = excluded.message_id,
            size = excluded.size,
            flags = excluded.flags",
        params![
            account_id,
            mailbox,
            header.uid,
            header.subject,
            header.from,
            header.to,
            header.date,
            header.message_id,
            body.len() as i64,
            flags_column(header),
        ],
    )?;

    let cached_at = now_unix();
    conn.execute(
        "INSERT INTO bodies (account_id, mailbox, uid, body, cached_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (account_id, mailbox, uid) DO UPDATE SET
            body = excluded.body,
            cached_at = excluded.cached_at",
        params![account_id, mailbox, header.uid, body, cached_at],
    )?;

    // The FTS table has no natural key to upsert on, so replace-by-delete.
    conn.execute(
        "DELETE FROM messages_fts WHERE account_id = ?1 AND mailbox = ?2 AND uid = ?3",
        params![account_id, mailbox, header.uid],
    )?;
    conn.execute(
        "INSERT INTO messages_fts (account_id, mailbox, uid, subject, from_addr, to_addr, body)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![account_id, mailbox, header.uid, header.subject, header.from, header.to, body],
    )?;

    evict_lru_bodies(conn, MAX_CACHED_BODIES)?;
    Ok(())
}

/// Delete the oldest-cached rows in `bodies` until at most `max_rows` remain.
/// `messages`/`messages_fts` are untouched — this only trims the (larger,
/// re-fetchable) cached RFC822 bodies, not the metadata used to render the
/// header list or find things in search.
fn evict_lru_bodies(conn: &Connection, max_rows: i64) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM bodies WHERE rowid IN (
            SELECT rowid FROM bodies ORDER BY cached_at ASC
            LIMIT MAX(0, (SELECT COUNT(*) FROM bodies) - ?1)
        )",
        params![max_rows],
    )?;
    Ok(())
}

/// One full-text search result, with where it lives: results can now span
/// accounts and mailboxes, and a `MailHeader` (a UID and some envelope
/// fields) does not say which.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub account_id: String,
    pub mailbox: String,
    pub header: MailHeader,
}

/// `account_id: None` searches every account's cache (the FTS index is keyed
/// per account, so this is one query rather than one per account).
fn search(
    conn: &Connection,
    account_id: Option<&str>,
    query: &str,
    mailbox: Option<&str>,
) -> rusqlite::Result<Vec<SearchHit>> {
    // `account_id` and `mailbox` are bound as parameters (not spliced into
    // the SQL string) -- the previous version of this query built the WHERE
    // clause with `format!("... mailbox = '{}'", mb)`, which let a mailbox
    // name containing a `'` alter the query. IMAP mailbox names are server-
    // controlled, so this was reachable from an untrusted source.
    let mut stmt = conn.prepare(
        "SELECT m.uid, m.subject, m.from_addr, m.to_addr, m.date, m.message_id, m.flags,
                m.account_id, m.mailbox
         FROM messages_fts f
         JOIN messages m ON m.account_id = f.account_id
            AND m.mailbox = f.mailbox AND m.uid = f.uid
         WHERE (?1 IS NULL OR f.account_id = ?1)
            AND (?2 IS NULL OR f.mailbox = ?2)
            AND messages_fts MATCH ?3
         ORDER BY f.rank",
    )?;
    let rows = stmt.query_map(params![account_id, mailbox, query], |row| {
        let flags_str: String = row.get(6)?;
        Ok(SearchHit {
            account_id: row.get(7)?,
            mailbox: row.get(8)?,
            header: MailHeader {
                uid: row.get(0)?,
                subject: row.get(1)?,
                from: row.get(2)?,
                to: row.get(3)?,
                date: row.get(4)?,
                message_id: row.get(5)?,
                flags: parse_flags_column(&flags_str),
            },
        })
    })?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row?);
    }
    Ok(results)
}

fn fetch_mail(
    conn: &Connection,
    account_id: &str,
    mailbox: &str,
    uid: u32,
) -> rusqlite::Result<(MailHeader, String)> {
    conn.query_row(
        "SELECT m.subject, m.from_addr, m.to_addr, m.date, m.message_id, b.body, m.flags
         FROM messages m JOIN bodies b
            ON b.account_id = m.account_id AND b.mailbox = m.mailbox AND b.uid = m.uid
         WHERE m.account_id = ?1 AND m.mailbox = ?2 AND m.uid = ?3",
        params![account_id, mailbox, uid],
        |row| {
            let flags_str: String = row.get(6)?;
            Ok((
                MailHeader {
                    uid,
                    subject: row.get(0)?,
                    from: row.get(1)?,
                    to: row.get(2)?,
                    date: row.get(3)?,
                    message_id: row.get(4)?,
                    flags: parse_flags_column(&flags_str),
                },
                row.get(5)?,
            ))
        },
    )
}

/// `messages.flags` (B8) stores a space-separated list of raw IMAP flags
/// (e.g. `"\Seen \Flagged"`) -- splitting on whitespace round-trips cleanly
/// since no legal IMAP flag atom itself contains a space.
fn parse_flags_column(s: &str) -> Vec<String> {
    s.split_whitespace().map(|f| f.to_string()).collect()
}

fn flags_column(header: &MailHeader) -> String {
    header.flags.join(" ")
}

fn report_mailbox_state(
    conn: &Connection,
    account_id: &str,
    mailbox: &str,
    uid_validity: u32,
    uid_next: u32,
) -> rusqlite::Result<SyncPlan> {
    let previous: Option<(u32, u32)> = conn
        .query_row(
            "SELECT uid_validity, uid_next FROM mailboxes WHERE account_id = ?1 AND mailbox = ?2",
            params![account_id, mailbox],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();

    let plan = sync_decision(previous, uid_validity, uid_next);

    if plan == SyncPlan::Resync {
        conn.execute(
            "DELETE FROM messages WHERE account_id = ?1 AND mailbox = ?2",
            params![account_id, mailbox],
        )?;
        conn.execute(
            "DELETE FROM bodies WHERE account_id = ?1 AND mailbox = ?2",
            params![account_id, mailbox],
        )?;
        conn.execute(
            "DELETE FROM messages_fts WHERE account_id = ?1 AND mailbox = ?2",
            params![account_id, mailbox],
        )?;
    }

    conn.execute(
        "INSERT INTO mailboxes (account_id, mailbox, uid_validity, uid_next)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (account_id, mailbox) DO UPDATE SET
            uid_validity = excluded.uid_validity,
            uid_next = excluded.uid_next",
        params![account_id, mailbox, uid_validity, uid_next],
    )?;

    Ok(plan)
}

/// Overwrite the cached `flags` for one message (B8), if it's cached at all.
/// A no-op (not an error) when the row doesn't exist — the message may never
/// have been indexed (see `DbCommand::IndexHeaders`'s doc on what does and
/// doesn't populate `messages`).
fn update_flags(conn: &Connection, account_id: &str, mailbox: &str, uid: u32, flags: &[String]) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE messages SET flags = ?1 WHERE account_id = ?2 AND mailbox = ?3 AND uid = ?4",
        params![flags.join(" "), account_id, mailbox, uid],
    )?;
    Ok(())
}

/// Drop a moved-away message from every table that might hold it (B8).
fn remove_message(conn: &Connection, account_id: &str, mailbox: &str, uid: u32) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM messages WHERE account_id = ?1 AND mailbox = ?2 AND uid = ?3", params![account_id, mailbox, uid])?;
    conn.execute("DELETE FROM bodies WHERE account_id = ?1 AND mailbox = ?2 AND uid = ?3", params![account_id, mailbox, uid])?;
    conn.execute("DELETE FROM messages_fts WHERE account_id = ?1 AND mailbox = ?2 AND uid = ?3", params![account_id, mailbox, uid])?;
    Ok(())
}

/// The pure decision behind [`report_mailbox_state`]: given what was stored
/// last time (`None` the first time this mailbox is ever seen) and what the
/// server just reported, decide what the cache needs.
fn sync_decision(
    previous: Option<(u32, u32)>,
    server_uid_validity: u32,
    server_uid_next: u32,
) -> SyncPlan {
    match previous {
        None => {
            // Never seen this mailbox before: everything up to uid_next - 1
            // is "new" from the cache's point of view.
            if server_uid_next <= 1 {
                SyncPlan::UpToDate
            } else {
                SyncPlan::FetchFrom { first_new_uid: 1 }
            }
        }
        Some((prev_validity, _)) if prev_validity != server_uid_validity => SyncPlan::Resync,
        Some((_, prev_uid_next)) if server_uid_next > prev_uid_next => {
            SyncPlan::FetchFrom { first_new_uid: prev_uid_next }
        }
        Some(_) => SyncPlan::UpToDate,
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_header(uid: u32) -> MailHeader {
        MailHeader {
            uid,
            subject: format!("Subject {uid}"),
            from: "alice@example.com".to_string(),
            to: "bob@example.com".to_string(),
            date: "2026-01-01".to_string(),
            message_id: format!("<msg{uid}@example.com>"),
            flags: Vec::new(),
        }
    }

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn init_schema_is_idempotent() {
        // Regression guard for the ALTER TABLE migration: running init twice
        // (e.g. every app startup against the same mails.db) must not error
        // the second time just because the column is already there.
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        init_schema(&conn).unwrap();
    }

    #[test]
    fn init_schema_adds_message_id_to_a_pre_b7_messages_table() {
        // Simulates a mails.db left over from before B7 added the column:
        // a `messages` table that init_schema's CREATE TABLE IF NOT EXISTS
        // alone would never touch, since the table already exists.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE messages (
                account_id TEXT NOT NULL, mailbox TEXT NOT NULL, uid INTEGER NOT NULL,
                subject TEXT NOT NULL, from_addr TEXT NOT NULL, to_addr TEXT NOT NULL,
                date TEXT NOT NULL, size INTEGER NOT NULL DEFAULT 0,
                flags TEXT NOT NULL DEFAULT '', thread_key TEXT,
                PRIMARY KEY (account_id, mailbox, uid)
            )",
        )
        .unwrap();

        init_schema(&conn).unwrap();
        index_mail(&conn, "acc", "INBOX", &test_header(1), "body").unwrap();

        let (header, _) = fetch_mail(&conn, "acc", "INBOX", 1).unwrap();
        assert_eq!(header.message_id, "<msg1@example.com>");
    }

    #[test]
    fn init_schema_adds_flags_to_a_pre_b8_messages_table() {
        // Simulates a mails.db left over from between B7 and B8: has
        // message_id (B7) but not flags (B8) -- the exact gap
        // add_flags_column_if_missing exists to close, same shape as the
        // message_id regression test above.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE messages (
                account_id TEXT NOT NULL, mailbox TEXT NOT NULL, uid INTEGER NOT NULL,
                subject TEXT NOT NULL, from_addr TEXT NOT NULL, to_addr TEXT NOT NULL,
                date TEXT NOT NULL, message_id TEXT NOT NULL DEFAULT '',
                size INTEGER NOT NULL DEFAULT 0, thread_key TEXT,
                PRIMARY KEY (account_id, mailbox, uid)
            )",
        )
        .unwrap();

        init_schema(&conn).unwrap();
        let mut header = test_header(1);
        header.flags = vec!["\\Seen".to_string()];
        index_mail(&conn, "acc", "INBOX", &header, "body").unwrap();

        let (fetched, _) = fetch_mail(&conn, "acc", "INBOX", 1).unwrap();
        assert_eq!(fetched.flags, vec!["\\Seen".to_string()]);
    }

    // ── index_mail / fetch_mail ──────────────────────────────────────────────

    #[test]
    fn index_then_fetch_round_trips() {
        let conn = test_conn();
        index_mail(&conn, "acc", "INBOX", &test_header(1), "<p>hello</p>").unwrap();

        let (header, body) = fetch_mail(&conn, "acc", "INBOX", 1).unwrap();
        assert_eq!(header.uid, 1);
        assert_eq!(header.subject, "Subject 1");
        assert_eq!(body, "<p>hello</p>");
    }

    #[test]
    fn fetch_mail_errors_when_metadata_is_cached_but_the_body_was_evicted() {
        // Reproduces the scenario behind GitHub issue #13 ("Loading
        // message..." stuck forever): `messages` has a row (from
        // `index_headers`, or an `index_mail` whose body later fell out of
        // `MAX_CACHED_BODIES`'s LRU cap -- routine on a mailbox bigger than
        // the cap) but `bodies` doesn't, since `fetch_mail`'s query is an
        // INNER JOIN across the two tables. This must return `Err` (mapped
        // to `DbEvent::MailFetchFailed` in `run`, carrying the uid so
        // `main.rs` can fall back to a live `FetchBody` instead of getting
        // stuck) rather than panicking or silently returning nothing.
        let conn = test_conn();
        index_headers(&conn, "acc", "INBOX", &[test_header(1)]).unwrap();

        assert!(fetch_mail(&conn, "acc", "INBOX", 1).is_err());
    }

    #[test]
    fn indexing_the_same_uid_twice_updates_rather_than_duplicates() {
        let conn = test_conn();
        index_mail(&conn, "acc", "INBOX", &test_header(1), "<p>v1</p>").unwrap();
        let mut updated = test_header(1);
        updated.subject = "Updated subject".to_string();
        index_mail(&conn, "acc", "INBOX", &updated, "<p>v2</p>").unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);

        let (header, body) = fetch_mail(&conn, "acc", "INBOX", 1).unwrap();
        assert_eq!(header.subject, "Updated subject");
        assert_eq!(body, "<p>v2</p>");
    }

    #[test]
    fn accounts_and_mailboxes_do_not_collide_on_the_same_uid() {
        let conn = test_conn();
        index_mail(&conn, "acc1", "INBOX", &test_header(1), "acc1 inbox").unwrap();
        index_mail(&conn, "acc2", "INBOX", &test_header(1), "acc2 inbox").unwrap();
        index_mail(&conn, "acc1", "Archive", &test_header(1), "acc1 archive").unwrap();

        assert_eq!(fetch_mail(&conn, "acc1", "INBOX", 1).unwrap().1, "acc1 inbox");
        assert_eq!(fetch_mail(&conn, "acc2", "INBOX", 1).unwrap().1, "acc2 inbox");
        assert_eq!(fetch_mail(&conn, "acc1", "Archive", 1).unwrap().1, "acc1 archive");
    }

    // ── index_headers ─────────────────────────────────────────────────────────

    #[test]
    fn index_headers_populates_messages_metadata_without_a_body() {
        let conn = test_conn();
        index_headers(&conn, "acc", "INBOX", &[test_header(1), test_header(2)]).unwrap();

        let subject: String = conn
            .query_row("SELECT subject FROM messages WHERE account_id = 'acc' AND mailbox = 'INBOX' AND uid = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(subject, "Subject 1");

        // No body was ever supplied -- `bodies` and `messages_fts` must stay
        // untouched, not get a row with an empty body that would make an
        // unfetched message spuriously "findable" or blank out a real
        // cached body a later index_mail wrote.
        let bodies: i64 = conn.query_row("SELECT COUNT(*) FROM bodies", [], |r| r.get(0)).unwrap();
        assert_eq!(bodies, 0);
        let fts: i64 = conn.query_row("SELECT COUNT(*) FROM messages_fts", [], |r| r.get(0)).unwrap();
        assert_eq!(fts, 0);
    }

    #[test]
    fn index_headers_does_not_clobber_an_already_cached_body_or_its_size() {
        // A mailbox re-sync (SyncPlan::FetchFrom) can report a UID that was
        // already fully indexed earlier via index_mail (e.g. the server's
        // UIDNEXT moved because of messages in a range that includes one
        // this cache already has the body for). index_headers must not
        // regress that row back to bodyless.
        let conn = test_conn();
        index_mail(&conn, "acc", "INBOX", &test_header(1), "<p>already cached</p>").unwrap();

        index_headers(&conn, "acc", "INBOX", &[test_header(1)]).unwrap();

        let (_, body) = fetch_mail(&conn, "acc", "INBOX", 1).unwrap();
        assert_eq!(body, "<p>already cached</p>");
    }

    // ── flags (B8) ────────────────────────────────────────────────────────────

    #[test]
    fn index_mail_round_trips_flags() {
        let conn = test_conn();
        let mut header = test_header(1);
        header.flags = vec!["\\Seen".to_string(), "\\Flagged".to_string()];
        index_mail(&conn, "acc", "INBOX", &header, "body").unwrap();

        let (fetched, _) = fetch_mail(&conn, "acc", "INBOX", 1).unwrap();
        assert_eq!(fetched.flags, vec!["\\Seen", "\\Flagged"]);
    }

    #[test]
    fn update_flags_overwrites_a_cached_messages_flags() {
        let conn = test_conn();
        index_mail(&conn, "acc", "INBOX", &test_header(1), "body").unwrap();

        update_flags(&conn, "acc", "INBOX", 1, &["\\Seen".to_string()]).unwrap();

        let (header, _) = fetch_mail(&conn, "acc", "INBOX", 1).unwrap();
        assert_eq!(header.flags, vec!["\\Seen"]);
    }

    #[test]
    fn update_flags_on_an_uncached_message_is_a_harmless_no_op() {
        let conn = test_conn();
        // No index_mail/index_headers call for uid 1 -- nothing cached.
        update_flags(&conn, "acc", "INBOX", 1, &["\\Seen".to_string()]).unwrap();
    }

    #[test]
    fn remove_message_deletes_from_every_table() {
        let conn = test_conn();
        index_mail(&conn, "acc", "INBOX", &test_header(1), "body text").unwrap();

        remove_message(&conn, "acc", "INBOX", 1).unwrap();

        assert!(fetch_mail(&conn, "acc", "INBOX", 1).is_err());
        let fts: i64 = conn.query_row("SELECT COUNT(*) FROM messages_fts", [], |r| r.get(0)).unwrap();
        assert_eq!(fts, 0);
    }

    // ── search ────────────────────────────────────────────────────────────────

    #[test]
    fn search_finds_a_matching_subject() {
        let conn = test_conn();
        index_mail(&conn, "acc", "INBOX", &test_header(1), "irrelevant body").unwrap();
        let results = search(&conn, Some("acc"), "\"Subject 1\"", None).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].header.uid, 1);
    }

    #[test]
    fn search_is_scoped_to_the_given_account() {
        let conn = test_conn();
        index_mail(&conn, "acc1", "INBOX", &test_header(1), "body").unwrap();
        index_mail(&conn, "acc2", "INBOX", &test_header(2), "body").unwrap();
        let results = search(&conn, Some("acc1"), "body", None).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].header.uid, 1);
    }

    #[test]
    fn search_without_an_account_spans_all_of_them_and_says_where_each_hit_lives() {
        let conn = test_conn();
        index_mail(&conn, "acc1", "INBOX", &test_header(1), "needle").unwrap();
        index_mail(&conn, "acc2", "Archive", &test_header(1), "needle").unwrap();
        index_mail(&conn, "acc3", "INBOX", &test_header(2), "haystack").unwrap();
        let mut hits: Vec<(String, String, u32)> = search(&conn, None, "needle", None)
            .unwrap()
            .into_iter()
            .map(|h| (h.account_id, h.mailbox, h.header.uid))
            .collect();
        hits.sort();
        // The same UID in two accounts stays two distinct hits.
        assert_eq!(
            hits,
            vec![("acc1".to_string(), "INBOX".to_string(), 1), ("acc2".to_string(), "Archive".to_string(), 1)]
        );
    }

    #[test]
    fn search_mailbox_filter_does_not_allow_sql_injection() {
        // Regression test for the format!()-built WHERE clause this replaced:
        // a mailbox name containing a quote must be treated as a literal
        // value, not splice into the query.
        let conn = test_conn();
        index_mail(&conn, "acc", "INBOX", &test_header(1), "body").unwrap();
        let malicious_mailbox = "INBOX' OR '1'='1";
        // Must not error, and must not match anything (no mailbox has that
        // literal name), rather than the old code's behavior of the quote
        // breaking out of the string and the OR making every row match.
        let results = search(&conn, Some("acc"), "body", Some(malicious_mailbox)).unwrap();
        assert_eq!(results.len(), 0);
    }

    // ── evict_lru_bodies ──────────────────────────────────────────────────────

    #[test]
    fn evict_lru_bodies_keeps_only_the_newest_rows() {
        let conn = test_conn();
        for uid in 1..=5 {
            conn.execute(
                "INSERT INTO bodies (account_id, mailbox, uid, body, cached_at) VALUES ('acc', 'INBOX', ?1, 'x', ?1)",
                params![uid],
            ).unwrap();
        }
        evict_lru_bodies(&conn, 3).unwrap();

        let mut stmt = conn.prepare("SELECT uid FROM bodies ORDER BY uid").unwrap();
        let uids: Vec<i64> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(uids, vec![3, 4, 5]);
    }

    #[test]
    fn evict_lru_bodies_is_a_no_op_under_the_cap() {
        let conn = test_conn();
        conn.execute(
            "INSERT INTO bodies (account_id, mailbox, uid, body, cached_at) VALUES ('acc', 'INBOX', 1, 'x', 1)",
            [],
        ).unwrap();
        evict_lru_bodies(&conn, 100).unwrap();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM bodies", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 1);
    }

    // ── sync_decision ─────────────────────────────────────────────────────────

    #[test]
    fn first_time_seeing_a_nonempty_mailbox_fetches_from_uid_1() {
        assert_eq!(
            sync_decision(None, 100, 50),
            SyncPlan::FetchFrom { first_new_uid: 1 }
        );
    }

    #[test]
    fn first_time_seeing_an_empty_mailbox_is_up_to_date() {
        // uid_next of 1 means no message has ever been assigned a UID yet.
        assert_eq!(sync_decision(None, 100, 1), SyncPlan::UpToDate);
    }

    #[test]
    fn unchanged_uid_validity_and_uid_next_is_up_to_date() {
        assert_eq!(sync_decision(Some((100, 50)), 100, 50), SyncPlan::UpToDate);
    }

    #[test]
    fn new_mail_since_last_sync_fetches_from_the_old_uid_next() {
        assert_eq!(
            sync_decision(Some((100, 50)), 100, 80),
            SyncPlan::FetchFrom { first_new_uid: 50 }
        );
    }

    #[test]
    fn changed_uid_validity_forces_a_full_resync_regardless_of_uid_next() {
        assert_eq!(sync_decision(Some((100, 50)), 200, 50), SyncPlan::Resync);
        assert_eq!(sync_decision(Some((100, 50)), 200, 5), SyncPlan::Resync);
    }

    #[test]
    fn report_mailbox_state_wipes_cached_messages_on_uid_validity_change() {
        let conn = test_conn();
        index_mail(&conn, "acc", "INBOX", &test_header(1), "body").unwrap();
        report_mailbox_state(&conn, "acc", "INBOX", 100, 50).unwrap();

        let plan = report_mailbox_state(&conn, "acc", "INBOX", 200, 50).unwrap();
        assert_eq!(plan, SyncPlan::Resync);

        let count: i64 = conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 0);
        let body_count: i64 = conn.query_row("SELECT COUNT(*) FROM bodies", [], |r| r.get(0)).unwrap();
        assert_eq!(body_count, 0);
    }

    #[test]
    fn report_mailbox_state_persists_what_it_saw_for_next_time() {
        let conn = test_conn();
        report_mailbox_state(&conn, "acc", "INBOX", 100, 50).unwrap();
        let plan = report_mailbox_state(&conn, "acc", "INBOX", 100, 50).unwrap();
        assert_eq!(plan, SyncPlan::UpToDate);
    }
}
