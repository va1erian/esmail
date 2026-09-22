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
use crate::compose::{ComposeId, ComposeState};
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
    /// Forget everything cached for an account -- its messages, bodies, search
    /// index and sync state -- when the account is removed. Without this its
    /// mail would stay on disk, keep turning up in search across accounts, and
    /// a later re-add would trust a stale sync state.
    RemoveAccount { account_id: String },

    /// Durably record a message that failed to send, for the background
    /// retry queue: a row here survives an app restart, and stays until
    /// `MarkOutboxSent` removes it. `id: None` inserts a new row (the first
    /// failure from a given compose window); `Some(id)` overwrites that same
    /// row with fresh content and resets its backoff, so a second failed
    /// Send from the same still-open window updates in place instead of
    /// piling up a duplicate. `compose_id` is never interpreted here -- it
    /// rides along so `DbEvent::OutboxEnqueued` can hand it straight back to
    /// `main.rs`, which is what actually needs it (to know which window's
    /// `window_outbox_id` entry to update).
    EnqueueOutbox { id: Option<i64>, compose_id: ComposeId, account_id: String, compose: ComposeState },
    /// Rows whose `next_attempt_at` has passed -- due for a (re)send.
    /// Answered with `DbEvent::OutboxDue`.
    DueOutbox,
    /// The send for this outbox row succeeded: delete it.
    MarkOutboxSent { id: i64 },
    /// The send for this outbox row failed: bump its attempt count and push
    /// `next_attempt_at` out with exponential backoff (`backoff_seconds`),
    /// so `DueOutbox` stops returning it until then.
    MarkOutboxFailed { id: i64, error: String },
    /// Forget an outbox row without ever sending it (the user deleted it
    /// from the Outbox window).
    DeleteOutbox { id: i64 },
    /// Every outbox row, oldest first. Answered with `DbEvent::OutboxList`.
    ListOutbox,

    /// Create or update an autosaved draft. `id: None` inserts a new row
    /// (its freshly assigned id comes back in `DbEvent::DraftSaved`);
    /// `Some(id)` overwrites that row in place. `compose_id` is passed
    /// straight back on `DbEvent::DraftSaved`, same reason as
    /// `EnqueueOutbox`'s.
    SaveDraft { id: Option<i64>, compose_id: ComposeId, account_id: Option<String>, compose: ComposeState },
    /// Every saved draft, most-recently-updated first. Answered with
    /// `DbEvent::DraftList`.
    ListDrafts,
    /// Load one draft back into a compose window. Answered with
    /// `DbEvent::DraftLoaded`.
    LoadDraft { id: i64 },
    /// Forget a draft (sent, or deleted from the Drafts window).
    DeleteDraft { id: i64 },
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
    /// `EnqueueOutbox` succeeded: the row's id (freshly assigned, or the one
    /// that was passed in and got overwritten) and the `compose_id` that was
    /// passed through, so `main.rs` can record which window this row
    /// belongs to.
    OutboxEnqueued { id: i64, compose_id: ComposeId },
    OutboxDue { items: Vec<OutboxItem> },
    OutboxList { items: Vec<OutboxItem> },
    /// `SaveDraft` succeeded: the row's id (freshly assigned, or the one
    /// that was passed in and got overwritten), and the `compose_id` passed
    /// through so `main.rs` knows which window to attach it to.
    DraftSaved { id: i64, compose_id: ComposeId },
    DraftList { items: Vec<DraftSummary> },
    DraftLoaded { id: i64, compose: ComposeState },
    Error(String),
}

/// One row of the retry queue, as `DueOutbox`/`ListOutbox` return it.
#[derive(Debug, Clone)]
pub struct OutboxItem {
    pub id: i64,
    pub account_id: String,
    pub compose: ComposeState,
    pub attempts: i64,
    pub last_error: Option<String>,
}

/// A draft's list-view summary -- everything the Drafts window shows without
/// needing the full `ComposeState` (attachments and all) for every row.
#[derive(Debug, Clone)]
pub struct DraftSummary {
    pub id: i64,
    pub subject: String,
    pub to: String,
    pub updated_at: i64,
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
            let conn = match open_cache() {
                Ok(c) => c,
                Err(e) => {
                    let _ = event_tx.blocking_send(DbEvent::Error(e));
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
                DbCommand::RemoveAccount { account_id } => {
                    if let Err(e) = remove_account(&self.conn, &account_id) {
                        let _ = self.event_tx.blocking_send(DbEvent::Error(e.to_string()));
                    }
                }
                DbCommand::EnqueueOutbox { id, compose_id, account_id, compose } => {
                    match enqueue_outbox(&self.conn, id, &account_id, &compose) {
                        Ok(id) => {
                            let _ = self.event_tx.blocking_send(DbEvent::OutboxEnqueued { id, compose_id });
                        }
                        Err(e) => {
                            let _ = self.event_tx.blocking_send(DbEvent::Error(e.to_string()));
                        }
                    }
                }
                DbCommand::DueOutbox => match due_outbox(&self.conn, now_unix()) {
                    Ok(items) => {
                        let _ = self.event_tx.blocking_send(DbEvent::OutboxDue { items });
                    }
                    Err(e) => {
                        let _ = self.event_tx.blocking_send(DbEvent::Error(e.to_string()));
                    }
                },
                DbCommand::ListOutbox => match list_outbox(&self.conn) {
                    Ok(items) => {
                        let _ = self.event_tx.blocking_send(DbEvent::OutboxList { items });
                    }
                    Err(e) => {
                        let _ = self.event_tx.blocking_send(DbEvent::Error(e.to_string()));
                    }
                },
                DbCommand::MarkOutboxSent { id } => {
                    if let Err(e) = delete_outbox(&self.conn, id) {
                        let _ = self.event_tx.blocking_send(DbEvent::Error(e.to_string()));
                    }
                }
                DbCommand::MarkOutboxFailed { id, error } => {
                    if let Err(e) = mark_outbox_failed(&self.conn, id, &error, now_unix()) {
                        let _ = self.event_tx.blocking_send(DbEvent::Error(e.to_string()));
                    }
                }
                DbCommand::DeleteOutbox { id } => {
                    if let Err(e) = delete_outbox(&self.conn, id) {
                        let _ = self.event_tx.blocking_send(DbEvent::Error(e.to_string()));
                    }
                }
                DbCommand::SaveDraft { id, compose_id, account_id, compose } => {
                    match save_draft(&self.conn, id, account_id.as_deref(), &compose) {
                        Ok(id) => {
                            let _ = self.event_tx.blocking_send(DbEvent::DraftSaved { id, compose_id });
                        }
                        Err(e) => {
                            let _ = self.event_tx.blocking_send(DbEvent::Error(e.to_string()));
                        }
                    }
                }
                DbCommand::ListDrafts => match list_drafts(&self.conn) {
                    Ok(items) => {
                        let _ = self.event_tx.blocking_send(DbEvent::DraftList { items });
                    }
                    Err(e) => {
                        let _ = self.event_tx.blocking_send(DbEvent::Error(e.to_string()));
                    }
                },
                DbCommand::LoadDraft { id } => match load_draft(&self.conn, id) {
                    Ok(compose) => {
                        let _ = self.event_tx.blocking_send(DbEvent::DraftLoaded { id, compose });
                    }
                    Err(e) => {
                        let _ = self.event_tx.blocking_send(DbEvent::Error(e.to_string()));
                    }
                },
                DbCommand::DeleteDraft { id } => {
                    if let Err(e) = delete_draft(&self.conn, id) {
                        let _ = self.event_tx.blocking_send(DbEvent::Error(e.to_string()));
                    }
                }
            }
        }
    }
}

/// Open the cache in the per-user data directory (see [`crate::paths`]),
/// creating the directory on first run. It used to be `mails.db` in the
/// working directory, which scattered a cache next to whatever folder the app
/// happened to be launched from (the Start menu launches it from
/// `System32`, where it cannot even write).
fn open_cache() -> Result<Connection, String> {
    let path = crate::paths::db_file().ok_or("no per-user data directory is available")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    }
    Connection::open(&path).map_err(|e| format!("could not open {}: {e}", path.display()))
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

        CREATE TABLE IF NOT EXISTS outbox (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            account_id      TEXT NOT NULL,
            compose_json    TEXT NOT NULL,
            created_at      INTEGER NOT NULL,
            attempts        INTEGER NOT NULL DEFAULT 0,
            next_attempt_at INTEGER NOT NULL,
            last_error      TEXT
        );

        CREATE TABLE IF NOT EXISTS drafts (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            account_id   TEXT,
            compose_json TEXT NOT NULL,
            updated_at   INTEGER NOT NULL
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
/// Delete every row cached for `account_id`, from every table.
fn remove_account(conn: &Connection, account_id: &str) -> rusqlite::Result<()> {
    for table in ["messages", "bodies", "messages_fts", "mailboxes"] {
        conn.execute(&format!("DELETE FROM {table} WHERE account_id = ?1"), params![account_id])?;
    }
    Ok(())
}

fn remove_message(conn: &Connection, account_id: &str, mailbox: &str, uid: u32) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM messages WHERE account_id = ?1 AND mailbox = ?2 AND uid = ?3", params![account_id, mailbox, uid])?;
    conn.execute("DELETE FROM bodies WHERE account_id = ?1 AND mailbox = ?2 AND uid = ?3", params![account_id, mailbox, uid])?;
    conn.execute("DELETE FROM messages_fts WHERE account_id = ?1 AND mailbox = ?2 AND uid = ?3", params![account_id, mailbox, uid])?;
    Ok(())
}

// ── outbox (retry queue) ─────────────────────────────────────────────────

/// Turn a rusqlite error into the same `String`-error shape the rest of this
/// module's helpers use, for the one step here (`serde_json`) that isn't a
/// `rusqlite::Error` to begin with. Mapped straight to `rusqlite::Error::ToSqlConversionFailure`
/// so every outbox/draft helper can keep returning a plain `rusqlite::Result`.
fn to_sql_err(e: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(e))
}

/// Insert a new outbox row (`id: None`) or overwrite an existing one
/// (`Some`) with fresh content and a reset backoff -- a second failed Send
/// from the same still-open window updates in place rather than piling up a
/// duplicate row for the same logical message.
fn enqueue_outbox(conn: &Connection, id: Option<i64>, account_id: &str, compose: &ComposeState) -> rusqlite::Result<i64> {
    let compose_json = serde_json::to_string(compose).map_err(to_sql_err)?;
    let now = now_unix();
    match id {
        Some(id) => {
            conn.execute(
                "UPDATE outbox SET account_id = ?1, compose_json = ?2, attempts = 0, next_attempt_at = ?3, last_error = NULL WHERE id = ?4",
                params![account_id, compose_json, now, id],
            )?;
            Ok(id)
        }
        None => {
            conn.execute(
                "INSERT INTO outbox (account_id, compose_json, created_at, attempts, next_attempt_at)
                 VALUES (?1, ?2, ?3, 0, ?3)",
                params![account_id, compose_json, now],
            )?;
            Ok(conn.last_insert_rowid())
        }
    }
}

fn row_to_outbox_item(row: &rusqlite::Row) -> rusqlite::Result<OutboxItem> {
    let compose_json: String = row.get(1)?;
    let compose: ComposeState = serde_json::from_str(&compose_json).map_err(to_sql_err)?;
    Ok(OutboxItem {
        id: row.get(0)?,
        account_id: row.get(2)?,
        attempts: row.get(3)?,
        last_error: row.get(4)?,
        compose,
    })
}

fn due_outbox(conn: &Connection, now: i64) -> rusqlite::Result<Vec<OutboxItem>> {
    let mut stmt = conn.prepare(
        "SELECT id, compose_json, account_id, attempts, last_error FROM outbox
         WHERE next_attempt_at <= ?1 ORDER BY id ASC",
    )?;
    let rows = stmt.query_map(params![now], row_to_outbox_item)?;
    rows.collect()
}

fn list_outbox(conn: &Connection) -> rusqlite::Result<Vec<OutboxItem>> {
    let mut stmt = conn.prepare("SELECT id, compose_json, account_id, attempts, last_error FROM outbox ORDER BY id ASC")?;
    let rows = stmt.query_map([], row_to_outbox_item)?;
    rows.collect()
}

fn delete_outbox(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM outbox WHERE id = ?1", params![id])?;
    Ok(())
}

/// Exponential backoff for the retry queue: 30s, 1m, 2m, 4m, ... capped at
/// one hour, so a server that's down for a while isn't hammered but a brief
/// blip is retried reasonably soon. `attempts` is the count *after* this
/// failure (i.e. always >= 1).
fn backoff_seconds(attempts: i64) -> i64 {
    let exponent = (attempts - 1).clamp(0, 7) as u32;
    (30i64 * 2i64.pow(exponent)).min(3600)
}

fn mark_outbox_failed(conn: &Connection, id: i64, error: &str, now: i64) -> rusqlite::Result<()> {
    let attempts_before: i64 = conn.query_row("SELECT attempts FROM outbox WHERE id = ?1", params![id], |r| r.get(0))?;
    let attempts = attempts_before + 1;
    conn.execute(
        "UPDATE outbox SET attempts = ?1, last_error = ?2, next_attempt_at = ?3 WHERE id = ?4",
        params![attempts, error, now + backoff_seconds(attempts), id],
    )?;
    Ok(())
}

// ── drafts ────────────────────────────────────────────────────────────────

/// Insert a new draft (`id: None`) or overwrite an existing one (`Some`),
/// returning the row's id either way.
fn save_draft(conn: &Connection, id: Option<i64>, account_id: Option<&str>, compose: &ComposeState) -> rusqlite::Result<i64> {
    let compose_json = serde_json::to_string(compose).map_err(to_sql_err)?;
    let now = now_unix();
    match id {
        Some(id) => {
            conn.execute(
                "UPDATE drafts SET account_id = ?1, compose_json = ?2, updated_at = ?3 WHERE id = ?4",
                params![account_id, compose_json, now, id],
            )?;
            Ok(id)
        }
        None => {
            conn.execute(
                "INSERT INTO drafts (account_id, compose_json, updated_at) VALUES (?1, ?2, ?3)",
                params![account_id, compose_json, now],
            )?;
            Ok(conn.last_insert_rowid())
        }
    }
}

fn list_drafts(conn: &Connection) -> rusqlite::Result<Vec<DraftSummary>> {
    let mut stmt = conn.prepare("SELECT id, compose_json, updated_at FROM drafts ORDER BY updated_at DESC")?;
    let rows = stmt.query_map([], |row| {
        let compose_json: String = row.get(1)?;
        let compose: ComposeState = serde_json::from_str(&compose_json).map_err(to_sql_err)?;
        Ok(DraftSummary {
            id: row.get(0)?,
            updated_at: row.get(2)?,
            subject: compose.subject,
            to: compose.to,
        })
    })?;
    rows.collect()
}

fn load_draft(conn: &Connection, id: i64) -> rusqlite::Result<ComposeState> {
    let compose_json: String = conn.query_row("SELECT compose_json FROM drafts WHERE id = ?1", params![id], |r| r.get(0))?;
    serde_json::from_str(&compose_json).map_err(to_sql_err)
}

fn delete_draft(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM drafts WHERE id = ?1", params![id])?;
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
    fn remove_account_forgets_that_account_only() {
        let conn = test_conn();
        index_mail(&conn, "gone", "INBOX", &test_header(1), "needle").unwrap();
        index_mail(&conn, "kept", "INBOX", &test_header(1), "needle").unwrap();
        report_mailbox_state(&conn, "gone", "INBOX", 7, 2).unwrap();
        report_mailbox_state(&conn, "kept", "INBOX", 7, 2).unwrap();

        remove_account(&conn, "gone").unwrap();

        let hits = search(&conn, None, "needle", None).unwrap();
        assert_eq!(hits.len(), 1, "only the other account's mail is left to find");
        assert_eq!(hits[0].account_id, "kept");
        assert!(fetch_mail(&conn, "gone", "INBOX", 1).is_err(), "its cached body is gone too");
        // Its sync state is gone as well, so a re-add starts from scratch rather
        // than trusting an empty cache; the other account's is untouched.
        assert_ne!(report_mailbox_state(&conn, "gone", "INBOX", 7, 2).unwrap(), SyncPlan::UpToDate);
        assert_eq!(report_mailbox_state(&conn, "kept", "INBOX", 7, 2).unwrap(), SyncPlan::UpToDate);
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

    // ── outbox ────────────────────────────────────────────────────────────────

    fn test_compose(to: &str) -> ComposeState {
        ComposeState { to: to.to_string(), subject: "Hi".to_string(), body: "Body".to_string(), ..Default::default() }
    }

    #[test]
    fn enqueued_mail_is_immediately_due() {
        let conn = test_conn();
        enqueue_outbox(&conn, None, "acc", &test_compose("bob@example.com")).unwrap();

        let due = due_outbox(&conn, now_unix()).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].account_id, "acc");
        assert_eq!(due[0].compose.to, "bob@example.com");
        assert_eq!(due[0].attempts, 0);
    }

    #[test]
    fn marking_sent_removes_the_row() {
        let conn = test_conn();
        let id = enqueue_outbox(&conn, None, "acc", &test_compose("bob@example.com")).unwrap();

        delete_outbox(&conn, id).unwrap();

        assert!(due_outbox(&conn, now_unix()).unwrap().is_empty());
    }

    #[test]
    fn enqueuing_with_an_existing_id_overwrites_rather_than_duplicating() {
        let conn = test_conn();
        let id = enqueue_outbox(&conn, None, "acc", &test_compose("bob@example.com")).unwrap();
        mark_outbox_failed(&conn, id, "connection refused", now_unix()).unwrap();

        let same_id = enqueue_outbox(&conn, Some(id), "acc", &test_compose("carol@example.com")).unwrap();

        assert_eq!(same_id, id);
        let items = list_outbox(&conn).unwrap();
        assert_eq!(items.len(), 1, "must not have created a second row");
        assert_eq!(items[0].compose.to, "carol@example.com");
        assert_eq!(items[0].attempts, 0, "a fresh attempt resets the backoff");
        assert_eq!(items[0].last_error, None);
    }

    #[test]
    fn a_failed_send_is_not_due_again_until_its_backoff_elapses() {
        let conn = test_conn();
        let id = enqueue_outbox(&conn, None, "acc", &test_compose("bob@example.com")).unwrap();
        let now = now_unix();

        mark_outbox_failed(&conn, id, "connection refused", now).unwrap();

        assert!(due_outbox(&conn, now).unwrap().is_empty(), "should not retry immediately");
        let later = now + backoff_seconds(1) + 1;
        let due = due_outbox(&conn, later).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].attempts, 1);
        assert_eq!(due[0].last_error.as_deref(), Some("connection refused"));
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        assert_eq!(backoff_seconds(1), 30);
        assert_eq!(backoff_seconds(2), 60);
        assert_eq!(backoff_seconds(3), 120);
        assert_eq!(backoff_seconds(20), 3600);
    }

    #[test]
    fn list_outbox_returns_every_row() {
        let conn = test_conn();
        enqueue_outbox(&conn, None, "acc1", &test_compose("a@example.com")).unwrap();
        enqueue_outbox(&conn, None, "acc2", &test_compose("b@example.com")).unwrap();

        let items = list_outbox(&conn).unwrap();
        assert_eq!(items.len(), 2);
    }

    // ── drafts ────────────────────────────────────────────────────────────────

    #[test]
    fn saving_a_new_draft_then_loading_it_round_trips() {
        let conn = test_conn();
        let id = save_draft(&conn, None, Some("acc"), &test_compose("bob@example.com")).unwrap();

        let loaded = load_draft(&conn, id).unwrap();
        assert_eq!(loaded.to, "bob@example.com");
        assert_eq!(loaded.subject, "Hi");
    }

    #[test]
    fn saving_with_an_existing_id_overwrites_rather_than_duplicating() {
        let conn = test_conn();
        let id = save_draft(&conn, None, Some("acc"), &test_compose("bob@example.com")).unwrap();
        save_draft(&conn, Some(id), Some("acc"), &test_compose("carol@example.com")).unwrap();

        let drafts = list_drafts(&conn).unwrap();
        assert_eq!(drafts.len(), 1);
        assert_eq!(load_draft(&conn, id).unwrap().to, "carol@example.com");
    }

    #[test]
    fn list_drafts_orders_most_recently_updated_first() {
        let conn = test_conn();
        let first = save_draft(&conn, None, Some("acc"), &test_compose("a@example.com")).unwrap();
        let second = save_draft(&conn, None, Some("acc"), &test_compose("b@example.com")).unwrap();
        // Both saved in the same second would otherwise tie on `updated_at`
        // -- back-date the first so the ordering assertion below isn't a
        // race against the clock's own resolution.
        conn.execute("UPDATE drafts SET updated_at = updated_at - 10 WHERE id = ?1", params![first]).unwrap();

        let drafts = list_drafts(&conn).unwrap();
        assert_eq!(drafts[0].id, second);
        assert_eq!(drafts[1].id, first);
    }

    #[test]
    fn deleting_a_draft_removes_it() {
        let conn = test_conn();
        let id = save_draft(&conn, None, Some("acc"), &test_compose("bob@example.com")).unwrap();

        delete_draft(&conn, id).unwrap();

        assert!(list_drafts(&conn).unwrap().is_empty());
    }
}
