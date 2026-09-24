//! The local mail cache, seen from a window: what it holds is shown before the
//! network answers, what the network reports is written back to it, and
//! searches run against it.
//!
//! Reads go through a read-only connection on the runtime's blocking pool, so
//! they neither wait for the cache actor's queue nor touch the UI thread.
//! Writes and searches are commands to a [`DbActor`]; its replies, and the
//! reads' results, come back as [`CacheEvent`]s that [`Cache::pump`] moves out
//! of a channel without blocking, exactly like the IMAP events.

use std::sync::mpsc as std_mpsc;

use esmail::compose::ComposeId;
use esmail::db::{CacheReader, DbActor, DbCommand, DbEvent, OutboxItem, SearchHit};
use esmail::imap::MailHeader;
use esmail::render::Attachment;
use esmail::search_query::ParsedQuery;
use esmail::waker::Waker;
use tokio::runtime::Handle;
use tokio::sync::mpsc;

/// How many commands the cache actor queues before a write is dropped.
const COMMAND_QUEUE: usize = 256;

/// Something the cache reported.
#[derive(Debug)]
pub enum CacheEvent {
    /// The folders that have cached messages for `account`.
    Folders {
        /// The account's index.
        account: usize,
        /// Their full IMAP names.
        mailboxes: Vec<String>,
    },
    /// The newest cached messages of a folder, newest first. Empty when the
    /// folder has nothing cached.
    Headers {
        /// The account's index.
        account: usize,
        /// The folder's full IMAP name.
        mailbox: String,
        /// The cached headers.
        headers: Vec<MailHeader>,
    },
    /// The answer to a [`Cache::search`]. Searches are answered in the order
    /// they were sent, but a reply does not say which query it is for.
    Search(Vec<SearchHit>),
    /// A draft was written: the row it lives in, and the compose window it
    /// belongs to.
    DraftSaved {
        /// The `drafts` row.
        id: i64,
        /// The window that asked.
        compose_id: ComposeId,
    },
    /// A message that failed to send was recorded in the outbox.
    OutboxEnqueued {
        /// The `outbox` row.
        id: i64,
        /// The window (or retry) the message came from.
        compose_id: ComposeId,
    },
    /// The answer to [`Cache::due_outbox`]: messages to send again.
    OutboxDue(Vec<OutboxItem>),
    /// A cache read or write failed. The cache only speeds things up, so the
    /// app reports this without stopping.
    Failed(String),
}

/// The cache: reads, writes and search, all off the UI thread.
pub struct Cache {
    runtime: Handle,
    /// The `id` of each account, in the order the app numbers them.
    account_ids: Vec<String>,
    commands: mpsc::Sender<DbCommand>,
    events_tx: std_mpsc::Sender<CacheEvent>,
    events: std_mpsc::Receiver<CacheEvent>,
    waker: Waker,
}

impl Cache {
    /// Starts the cache actor on `runtime`. `waker` is called whenever an
    /// event is waiting.
    pub fn start(runtime: &Handle, account_ids: Vec<String>, waker: Waker) -> Cache {
        let (commands, command_rx) = mpsc::channel(COMMAND_QUEUE);
        let (db_events_tx, mut db_events) = mpsc::channel(32);
        let (events_tx, events) = std_mpsc::channel();
        DbActor::spawn(runtime, command_rx, db_events_tx);
        let forward_tx = events_tx.clone();
        let forward_waker = waker.clone();
        runtime.spawn(async move {
            while let Some(event) = db_events.recv().await {
                let event = match event {
                    DbEvent::SearchResult { hits } => CacheEvent::Search(hits),
                    DbEvent::DraftSaved { id, compose_id } => CacheEvent::DraftSaved { id, compose_id },
                    DbEvent::OutboxEnqueued { id, compose_id } => CacheEvent::OutboxEnqueued { id, compose_id },
                    DbEvent::OutboxDue { items } => CacheEvent::OutboxDue(items),
                    DbEvent::Error(message) => CacheEvent::Failed(message),
                    _ => continue,
                };
                let _ = forward_tx.send(event);
                forward_waker();
            }
        });
        Cache { runtime: runtime.clone(), account_ids, commands, events_tx, events, waker }
    }

    /// Moves the events that arrived since the last call out of the channel.
    /// Never blocks.
    pub fn pump(&self) -> Vec<CacheEvent> {
        self.events.try_iter().collect()
    }

    /// Reads which folders of `account` have cached messages.
    pub fn load_mailboxes(&self, account: usize) {
        let Some(id) = self.account_ids.get(account).cloned() else { return };
        self.read(move |reader| reader.mailboxes(&id).map(|mailboxes| CacheEvent::Folders { account, mailboxes }));
    }

    /// Reads the `limit` newest cached messages of a folder.
    pub fn load_folder(&self, account: usize, mailbox: String, limit: usize) {
        let Some(id) = self.account_ids.get(account).cloned() else { return };
        self.read(move |reader| {
            let headers = reader.newest(&id, &mailbox, limit)?;
            Ok(CacheEvent::Headers { account, mailbox, headers })
        });
    }

    /// Runs `read` on the blocking pool with a read-only connection and reports
    /// its event, or its failure. A missing cache is not a failure: there is
    /// just nothing to show yet.
    fn read(&self, read: impl FnOnce(&CacheReader) -> Result<CacheEvent, String> + Send + 'static) {
        let events = self.events_tx.clone();
        let waker = self.waker.clone();
        self.runtime.spawn_blocking(move || {
            let event = match CacheReader::open().and_then(|reader| reader.map(|reader| read(&reader)).transpose()) {
                Ok(Some(event)) => event,
                Ok(None) => return,
                Err(message) => CacheEvent::Failed(message),
            };
            let _ = events.send(event);
            waker();
        });
    }

    /// Caches headers the server listed, so they are shown at the next start
    /// and found by search.
    pub fn index_headers(&self, account: usize, mailbox: &str, headers: &[MailHeader]) {
        if let Some(account_id) = self.account_ids.get(account) {
            self.command(DbCommand::IndexHeadersSearchable { account_id: account_id.clone(), mailbox: mailbox.to_string(), headers: headers.to_vec() });
        }
    }

    /// Caches a message body (and makes the message searchable by it).
    pub fn index_mail(&self, account: usize, mailbox: &str, header: MailHeader, body: String, attachments: Vec<Attachment>) {
        if let Some(account_id) = self.account_ids.get(account) {
            self.command(DbCommand::IndexMail { account_id: account_id.clone(), mailbox: mailbox.to_string(), header, body, attachments });
        }
    }

    /// Records the server's flags for a message.
    pub fn update_flags(&self, account: usize, mailbox: &str, uid: u32, flags: Vec<String>) {
        if let Some(account_id) = self.account_ids.get(account) {
            self.command(DbCommand::UpdateFlags { account_id: account_id.clone(), mailbox: mailbox.to_string(), uid, flags });
        }
    }

    /// Forgets a message that left its folder.
    pub fn remove_message(&self, account: usize, mailbox: &str, uid: u32) {
        if let Some(account_id) = self.account_ids.get(account) {
            self.command(DbCommand::RemoveMessage { account_id: account_id.clone(), mailbox: mailbox.to_string(), uid });
        }
    }

    /// Searches every account's cache. The answer arrives as
    /// [`CacheEvent::Search`].
    pub fn search(&self, query: ParsedQuery) {
        self.command(DbCommand::Search { account_id: None, query, mailbox: None });
    }

    /// Queues a command. A full queue means the cache is far behind; dropping
    /// a write only costs a stale cache, so it is not an error.
    pub(super) fn command(&self, command: DbCommand) {
        let _ = self.commands.try_send(command);
    }
}
