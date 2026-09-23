use std::time::Duration;

use tokio::sync::mpsc;
use tokio::net::TcpStream;
use tokio_native_tls::TlsStream;
use tokio_native_tls::native_tls::TlsConnector;
use secrecy::ExposeSecret;
use futures::StreamExt;
use anyhow::anyhow;

use crate::auth::{Auth, XOAuth2};
use crate::oauth::SignInExpired;
use crate::progress::{Progress, ProgressKind};

/// How many times [`ImapActor::ensure_connected`] retries a lost connection
/// before giving up and reporting the error to the UI.
const MAX_RECONNECT_ATTEMPTS: u32 = 5;
/// Backoff between reconnect attempts: 1s, 2s, 4s, 8s, capped at 16s.
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(16);

/// Credentials kept around so a dropped connection can be retried without the
/// user re-entering their password. Held only in memory, never persisted —
/// see `crate::secrets` for the on-disk (keyring) copy. For an OAuth account
/// `auth` is a shared token source, so each reconnect gets a currently valid
/// access token rather than replaying the one from the first login.
#[derive(Clone)]
struct Credentials {
    host: String,
    port: u16,
    username: String,
    auth: Auth,
}

#[derive(Debug, Clone)]
pub struct MailHeader {
    pub uid: u32,
    pub subject: String,
    pub from: String,
    pub to: String,
    pub date: String,
    /// The `Message-ID` header, e.g. `<abc123@example.com>`, angle brackets
    /// included (that's how `In-Reply-To`/`References` expect it). Empty
    /// when the server's ENVELOPE didn't include one — rare, but legal.
    pub message_id: String,
    /// Raw IMAP flags (e.g. `"\Seen"`, `"\Flagged"`, `"\Deleted"`) as of the
    /// most recent fetch (B8) -- every header/envelope fetch now asks for
    /// `FLAGS` alongside `ENVELOPE`, since the header list needs it for
    /// unread/star rendering. Empty for a header built somewhere that never
    /// had flags to report (e.g. `compose.rs`'s reply/forward derivation,
    /// which fabricates a `MailHeader` from the message being replied to).
    pub flags: Vec<String>,
}

impl MailHeader {
    pub fn is_seen(&self) -> bool {
        self.flags.iter().any(|f| f.eq_ignore_ascii_case("\\Seen"))
    }

    pub fn is_flagged(&self) -> bool {
        self.flags.iter().any(|f| f.eq_ignore_ascii_case("\\Flagged"))
    }

    /// The sender's bare address, lowercased -- the key "always load images
    /// from this sender" trusts on. `None` when `from` has no address in it.
    pub fn sender_address(&self) -> Option<String> {
        address_of(&self.from)
    }

    /// What the message list shows as the sender: the display name when the
    /// message has one, else the bare address.
    pub fn sender_name(&self) -> String {
        display_name_of(&self.from)
    }

    /// The message's `Date` as `("YYYY-MM-DD", "HH:MM")` in the local time
    /// zone, for the message list. `None` when the header is missing or not
    /// a date `mailparse` can read.
    ///
    /// This is the `Date:` header the sender's client wrote (what the
    /// `ENVELOPE` carries), not the server's `INTERNALDATE` -- the moment it
    /// was actually received. The two are usually minutes apart.
    pub fn local_date_time(&self) -> Option<(String, String)> {
        format_date_time(&self.date, &chrono::Local)
    }
}

/// [`MailHeader::local_date_time`] with the zone to render in as a parameter,
/// so it can be tested without depending on the machine's own zone.
fn format_date_time<Tz>(raw: &str, zone: &Tz) -> Option<(String, String)>
where
    Tz: chrono::TimeZone,
    Tz::Offset: std::fmt::Display,
{
    // `dateparse` is lenient: empty or garbage input comes back as `Ok(0)`,
    // which would print as 1970-01-01 rather than as "no date".
    let timestamp = mailparse::dateparse(raw).ok().filter(|&t| t > 0)?;
    let when = chrono::DateTime::from_timestamp(timestamp, 0)?.with_timezone(zone);
    Some((when.format("%Y-%m-%d").to_string(), when.format("%H:%M").to_string()))
}

/// The address out of a `parse_envelope_header`-style `"Name <user@host>"` /
/// `"user@host"` string, lowercased. Splits on the last `<...>` rather than
/// parsing RFC 5322, since the string is one this module formatted itself.
fn address_of(from: &str) -> Option<String> {
    let from = from.trim();
    let addr = match (from.rfind('<'), from.rfind('>')) {
        (Some(open), Some(close)) if open < close => &from[open + 1..close],
        _ => from,
    };
    let addr = addr.trim();
    // Both halves must be there: `parse_envelope_header` writes `Name <@>` for
    // an envelope address with no mailbox or host (group syntax, a malformed
    // `From`), and "@" must never become something that can be trusted.
    let (local, domain) = addr.split_once('@')?;
    (!local.is_empty() && !domain.is_empty()).then(|| addr.to_ascii_lowercase())
}

/// The `Name` out of `"Name <user@host>"`, without any surrounding quotes;
/// the whole string when there is no name part (or it is empty).
fn display_name_of(from: &str) -> String {
    let from = from.trim();
    if let Some(open) = from.rfind('<') {
        let name = from[..open].trim().trim_matches('"').trim();
        if !name.is_empty() {
            return name.to_string();
        }
        let inner = from[open + 1..].trim_end_matches('>').trim();
        if !inner.is_empty() {
            return inner.to_string();
        }
    }
    from.to_string()
}

/// Well-known IMAP flag names as `main.rs`/`imap.rs` construct `STORE`
/// queries with them -- kept as constants rather than repeated string
/// literals so a typo in one call site doesn't silently create a flag no
/// server recognizes.
pub const FLAG_SEEN: &str = "\\Seen";
pub const FLAG_FLAGGED: &str = "\\Flagged";
pub const FLAG_DELETED: &str = "\\Deleted";

/// One mailbox as reported by `LIST` (B8): its full name, the server's
/// hierarchy delimiter (so `main.rs` can split `"Work/Invoices"` into a
/// tree), and a best-effort special-use classification for sorting
/// well-known folders first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxInfo {
    pub name: String,
    pub delimiter: Option<String>,
    pub special_use: Option<SpecialUse>,
    /// `\Noselect` from `LIST`'s response attributes: a real mailbox name
    /// the server returned, but one that can't itself be `SELECT`/`EXAMINE`d
    /// -- purely a hierarchy container for its children (Gmail's `[Gmail]`
    /// is the canonical example). `mailbox_tree` treats this the same as a
    /// name that was never `LIST`ed at all (`MailboxNode::full_name` stays
    /// `None`), so the tree UI already renders it as a plain, unclickable
    /// label rather than sending a doomed `SELECT` when clicked.
    pub noselect: bool,
}

/// RFC 6154 special-use roles this cares about sorting specially, plus a
/// synthetic `Inbox` variant (RFC 6154 has no `\Inbox` attribute -- `INBOX`
/// is special by name, not by attribute, in every real server) so the tree
/// builder has one enum to sort all of "the folders every account has" by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SpecialUse {
    Inbox,
    Sent,
    Drafts,
    Trash,
    Archive,
    Junk,
}

impl SpecialUse {
    /// From a `LIST` response's name attributes, when the server advertises
    /// RFC 6154 special-use.
    fn from_attribute(attr: &async_imap::imap_proto::types::NameAttribute<'_>) -> Option<Self> {
        use async_imap::imap_proto::types::NameAttribute;
        match attr {
            NameAttribute::Sent => Some(SpecialUse::Sent),
            NameAttribute::Drafts => Some(SpecialUse::Drafts),
            NameAttribute::Trash => Some(SpecialUse::Trash),
            NameAttribute::Archive => Some(SpecialUse::Archive),
            NameAttribute::Junk => Some(SpecialUse::Junk),
            _ => None,
        }
    }

    /// Name-based fallback for a server that doesn't advertise RFC 6154
    /// special-use attributes (many don't) -- this is what lets
    /// `main.rs::special_use_mailbox` (issue #9) still find a Sent/Trash/
    /// Archive folder on such a server, provided it's conventionally named.
    /// Case-insensitive exact match only, not a substring search, so a
    /// mailbox that merely contains "sent" in a longer name (a filter
    /// folder called "Sent to Boss", say) isn't misclassified.
    fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "inbox" => Some(SpecialUse::Inbox),
            "sent" | "sent items" | "sent mail" => Some(SpecialUse::Sent),
            "drafts" => Some(SpecialUse::Drafts),
            "trash" | "deleted items" => Some(SpecialUse::Trash),
            "archive" => Some(SpecialUse::Archive),
            "junk" | "spam" | "junk e-mail" => Some(SpecialUse::Junk),
            _ => None,
        }
    }
}

/// One node of the tree `mailbox_tree` builds from `LIST`'s flat output
/// (B8), by splitting each mailbox's full name on the server's delimiter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxNode {
    /// This node's own path segment, e.g. `"Invoices"` for `"Work/Invoices"`.
    pub label: String,
    /// The full name to send back to the server (`EXAMINE`/`SELECT`/etc.) --
    /// `None` for a hierarchy node that exists only because a deeper mailbox
    /// implies it (e.g. `LIST` returned `"Work/Invoices"` but never
    /// `"Work"` itself; real servers usually also return the intermediate
    /// node, but nothing here assumes they do).
    pub full_name: Option<String>,
    pub special_use: Option<SpecialUse>,
    pub children: Vec<MailboxNode>,
}

/// Turns `LIST`'s flat mailbox names into a tree by splitting each on its
/// reported delimiter, sorting special-use folders (`INBOX` first, then
/// `Sent`/`Drafts`/`Archive`/`Junk`/`Trash` in that fixed order) ahead of
/// everything else, which sorts alphabetically. Pure and unit-tested without
/// any server — see the tests below.
pub fn mailbox_tree(mailboxes: &[MailboxInfo]) -> Vec<MailboxNode> {
    let mut roots: Vec<MailboxNode> = Vec::new();

    for mb in mailboxes {
        let delimiter = mb.delimiter.as_deref().filter(|d| !d.is_empty());
        let segments: Vec<&str> = match delimiter {
            Some(d) => mb.name.split(d).filter(|s| !s.is_empty()).collect(),
            None => vec![mb.name.as_str()],
        };
        insert_path(&mut roots, &segments, &mb.name, mb.special_use, mb.noselect);
    }

    sort_tree(&mut roots);
    roots
}

fn insert_path(nodes: &mut Vec<MailboxNode>, segments: &[&str], full_name: &str, special_use: Option<SpecialUse>, noselect: bool) {
    let Some((first, rest)) = segments.split_first() else { return };
    let is_leaf = rest.is_empty();

    let existing = nodes.iter_mut().find(|n| n.label == *first);
    let node = match existing {
        Some(n) => n,
        None => {
            nodes.push(MailboxNode { label: first.to_string(), full_name: None, special_use: None, children: Vec::new() });
            nodes.last_mut().expect("just pushed")
        }
    };

    if is_leaf {
        // A `\Noselect` mailbox (e.g. Gmail's `[Gmail]`) is a real name the
        // server returned, but not one `SELECT`/`EXAMINE` will accept --
        // leave `full_name` unset so the tree UI treats it exactly like a
        // hierarchy node that was never `LIST`ed at all (a plain,
        // unclickable label) instead of sending a doomed `SELECT`.
        if !noselect {
            node.full_name = Some(full_name.to_string());
        }
        node.special_use = special_use;
    } else {
        insert_path(&mut node.children, rest, full_name, special_use, noselect);
    }
}

fn sort_tree(nodes: &mut [MailboxNode]) {
    nodes.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
    for node in nodes.iter_mut() {
        sort_tree(&mut node.children);
    }
}

/// One row of a `mailbox_tree` flattened for a plain top-down list UI
/// (`main.rs`'s mailbox panel isn't a real recursive tree widget --
/// egui/immediate-mode makes an owned, borrow-free flat list far simpler to
/// render with indentation than juggling `&mut self` through recursive
/// closures).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxRow {
    pub depth: usize,
    pub label: String,
    /// `None` for a hierarchy node with no mailbox of its own (see
    /// `MailboxNode::full_name`) -- not selectable/clickable.
    pub full_name: Option<String>,
    pub special_use: Option<SpecialUse>,
    /// Stable identity of this node in the tree (its labels from the root,
    /// joined with `/`), which -- unlike `full_name` -- every row has. What
    /// the collapsed-folders set is keyed on.
    pub key: String,
    /// Whether the row's node has children, i.e. can be collapsed.
    pub has_children: bool,
}

/// Depth-first flatten of [`mailbox_tree`]'s output, in the same order the
/// tree is already sorted in.
pub fn flatten_tree(nodes: &[MailboxNode]) -> Vec<MailboxRow> {
    let mut rows = Vec::new();
    flatten_into(nodes, 0, "", &mut rows);
    rows
}

fn flatten_into(nodes: &[MailboxNode], depth: usize, parent_key: &str, rows: &mut Vec<MailboxRow>) {
    for node in nodes {
        let key = if parent_key.is_empty() { node.label.clone() } else { format!("{parent_key}/{}", node.label) };
        rows.push(MailboxRow {
            depth,
            label: node.label.clone(),
            full_name: node.full_name.clone(),
            special_use: node.special_use,
            key: key.clone(),
            has_children: !node.children.is_empty(),
        });
        flatten_into(&node.children, depth + 1, &key, rows);
    }
}

/// Indices into `rows` of the rows to draw once every node whose `key` is in
/// `collapsed` has hidden its descendants. The collapsed node's own row stays.
pub fn visible_rows(rows: &[MailboxRow], collapsed: &std::collections::BTreeSet<String>) -> Vec<usize> {
    let mut visible = Vec::with_capacity(rows.len());
    // Depth of the collapsed ancestor whose descendants are being skipped.
    let mut hidden_below: Option<usize> = None;
    for (index, row) in rows.iter().enumerate() {
        if let Some(depth) = hidden_below {
            if row.depth > depth {
                continue;
            }
            hidden_below = None;
        }
        visible.push(index);
        if row.has_children && collapsed.contains(&row.key) {
            hidden_below = Some(row.depth);
        }
    }
    visible
}

/// The rows below `rows[index]` in the tree (its descendants, in order).
pub fn descendants(rows: &[MailboxRow], index: usize) -> &[MailboxRow] {
    let depth = rows[index].depth;
    let rest = &rows[index + 1..];
    let len = rest.iter().position(|r| r.depth <= depth).unwrap_or(rest.len());
    &rest[..len]
}

/// `(special-use rank, label)`, so every `SpecialUse` variant sorts ahead of
/// `None` (folders with no special role), and ties within each group sort
/// alphabetically.
fn sort_key(node: &MailboxNode) -> (u8, String) {
    let rank = match node.special_use {
        Some(SpecialUse::Inbox) => 0,
        Some(SpecialUse::Sent) => 1,
        Some(SpecialUse::Drafts) => 2,
        Some(SpecialUse::Archive) => 3,
        Some(SpecialUse::Junk) => 4,
        Some(SpecialUse::Trash) => 5,
        None => 6,
    };
    (rank, node.label.to_ascii_lowercase())
}

/// UIDVALIDITY/UIDNEXT as of the most recent `EXAMINE`/`SELECT`, read off
/// values `async_imap` already parses from the server's untagged response —
/// getting this costs nothing beyond what `fetch_headers`/`fetch_body`
/// already do. Feeds `db.rs`'s incremental-sync bookkeeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MailboxState {
    pub uid_validity: u32,
    pub uid_next: u32,
}

pub enum ImapCommand {
    Connect {
        host: String,
        port: u16,
        username: String,
        auth: Auth,
    },
    FetchMailboxes,
    /// `req_id` is echoed on the resulting [`ImapEvent::Headers`] (or
    /// `Error`, which does not carry it — see its doc) so the UI can drop a
    /// reply that arrives after a newer request superseded it, instead of the
    /// old mailbox-name string compare which could not tell two requests for
    /// the *same* mailbox apart (e.g. hitting "Refresh" twice quickly).
    FetchHeaders { mailbox: String, page: u32, req_id: u64 },
    /// See `FetchHeaders`; echoed on [`ImapEvent::Body`].
    FetchBody { mailbox: String, uid: u32, req_id: u64 },
    BulkDownload { mailbox: String },
    /// Save one message's full raw RFC822 source to `path` as an `.eml`
    /// file, so a specific real-world message can be kept as a test case
    /// (it can be opened again without an account via `ESMAIL_PREVIEW`, see
    /// `main.rs`). Handled on the body worker's own connection, like
    /// `FetchBody`, and answered with [`ImapEvent::Exported`] or
    /// [`ImapEvent::ExportFailed`].
    ExportMessage { mailbox: String, uid: u32, path: std::path::PathBuf },
    /// Lightweight new-mail poll (B10): re-`EXAMINE`s `mailbox` to read the
    /// fresh UIDVALIDITY/UIDNEXT off the untagged response -- the same free
    /// ride `fetch_headers` already takes, just without the ENVELOPE fetch
    /// that pulls the actual header list. Silently dropped if there is no
    /// live session: unlike every other command here, this deliberately
    /// does *not* call `ensure_connected` and retry with backoff -- a
    /// background poll on a timer should never itself trigger a reconnect
    /// storm while the user is offline. The next real user action still
    /// reconnects normally.
    PollMailbox { mailbox: String },
    /// Fetch just the envelopes for UIDs `first_uid..` in `mailbox`, to
    /// build a new-mail notification (B10). Unlike `FetchHeaders`, this is
    /// not paged and not `req_id`-tracked: it never feeds the visible
    /// message list, only `notify::build_notification`. Same "no
    /// `ensure_connected`" reasoning as `PollMailbox` -- this only ever
    /// follows a `PollMailbox` that just proved there's a live session.
    FetchNewHeaders { mailbox: String, first_uid: u32 },
    /// Fetch envelopes for UIDs `first_uid..` in `mailbox`, to feed `db.rs`'s
    /// incremental cache sync (B3) -- a `DbEvent::SyncPlan::FetchFrom` names
    /// exactly this UID range as what the cache is missing. Deliberately a
    /// distinct command/event pair from `FetchNewHeaders`/`NewHeaders`
    /// despite doing the identical fetch: those feed B10's new-mail toast
    /// (the per-account forwarder in session.rs builds a notification from every
    /// `NewHeaders` it sees), and this fires far more often -- on every
    /// `FetchHeaders` that turns up UIDs the cache hasn't seen yet, which
    /// includes the user's own routine "open INBOX"/"hit refresh". Routing
    /// both through one event would toast the user for their own actions.
    /// Unlike `FetchNewHeaders`, this *does* call `ensure_connected`: it's a
    /// direct follow-up to a `FetchHeaders` that just proved the session
    /// live moments ago (not an independent background-timer poll), so
    /// there's no "don't start a reconnect storm while offline" concern to
    /// preserve.
    FetchHeadersFrom { mailbox: String, first_uid: u32 },
    /// Save `raw` (a full RFC822 message) into `mailbox` via IMAP `APPEND`
    /// (B7) -- `main.rs` sends this after a successful SMTP send, with
    /// `raw` the exact bytes `smtp.rs` handed to the transport, to save a
    /// copy in the account's Sent folder the way every other mail client
    /// does (SMTP servers don't do this themselves; sending and saving are
    /// two separate steps a client is responsible for both of). `mailbox`
    /// is the caller's choice, not discovered here -- `main.rs` hardcodes
    /// `"Sent"` rather than looking up the `\Sent` special-use flag with a
    /// name-based fallback, which PLAN.md §B7 still lists as a real,
    /// separate gap.
    Append { mailbox: String, raw: Vec<u8> },
    /// Add/remove flags on one message (B8) -- `\Seen` on open (with a
    /// mark-as-read delay), the star/flag toggle (`\Flagged`), and
    /// mark-unread (`-\Seen`) all go through this one command. `req_id`
    /// mirrors `FetchHeaders`/`FetchBody`'s staleness-guard pattern: a
    /// mark-as-read timer that fires after the user already moved to a
    /// different message shouldn't apply to the new one.
    StoreFlags { mailbox: String, uid: u32, add: Vec<String>, remove: Vec<String>, req_id: u64 },
    /// Move one message to `dest` (B8's delete-to-Trash and archive). Tries
    /// the real `MOVE` extension first; if the server doesn't support it (or
    /// the attempt otherwise fails), falls back to `COPY` + `STORE
    /// +FLAGS.SILENT \Deleted` + `EXPUNGE`, mechanically what `MOVE` is
    /// defined to do anyway (see `async_imap::Session::mv`'s own doc). See
    /// `ImapActor::move_message`.
    MoveMessage { mailbox: String, uid: u32, dest: String, req_id: u64 },
    /// `STATUS <mailbox> (UNSEEN)` for each name in `mailboxes` (B8), to show
    /// an unread count next to each mailbox in the tree. Issued as one
    /// command per mailbox on `ImapActor`'s own session (not paged/batched)
    /// -- real servers commonly support pipelining `STATUS`, but this client
    /// doesn't attempt it; see `ImapActor::fetch_unread_counts`'s doc for why
    /// that's an acceptable, if not optimal, trade for the size of an
    /// account's mailbox list.
    FetchUnreadCounts { mailboxes: Vec<String> },
}

#[derive(Debug)]
pub enum ImapEvent {
    Connected,
    /// The connection was lost (or a command needed a reconnect). Followed by
    /// either a `Connected` once [`ImapActor::ensure_connected`]'s retry loop
    /// succeeds, or an `Error` once it exhausts its attempts.
    Disconnected,
    Error(String),
    /// B8: each entry carries the delimiter/special-use info `main.rs`'s
    /// `mailbox_tree` needs, not just a bare name -- see `MailboxInfo`.
    Mailboxes(Vec<MailboxInfo>),
    Headers { mailbox: String, headers: Vec<MailHeader>, page: u32, total_pages: u32, req_id: u64, mailbox_state: MailboxState },
    Body { uid: u32, html: String, attachments: Vec<crate::render::Attachment>, req_id: u64 },
    /// `FetchBody` failed -- a separate variant from `Error` for the same
    /// reason `FlagsUpdateFailed`/`MoveFailed` are: without `uid`/`req_id`
    /// attribution, `main.rs` cannot tell a failed body fetch from any other
    /// unrelated error, so it had no way to resolve the "Loading message..."
    /// placeholder `open_message` sets -- the placeholder stayed up forever
    /// on any failure (a dropped connection mid-fetch, a reconnect that
    /// exhausted its retries, a message that no longer exists on the
    /// server), even though a banner correctly reported the error. This is
    /// what closes that gap: `main.rs` matches `req_id`/`uid` the same way
    /// it does for `Body` and replaces the placeholder with a real message
    /// instead of leaving it stuck.
    BodyFailed { uid: u32, req_id: u64, error: String },
    /// Progress for a middle-to-long operation this actor (or its body
    /// worker) owns, so the UI's status bar can say what is happening
    /// (issue #79). One operation is shown at a time; a later report for a
    /// different `kind` simply replaces the previous one. Terminal events
    /// (`Exported`, `Appended`, ...) tell the UI to clear it.
    Progress { kind: ProgressKind, progress: Progress },
    /// Reply to `ExportMessage`: the message's raw source was written to `path`.
    Exported { path: std::path::PathBuf },
    /// `ExportMessage` failed (fetching the message or writing the file).
    ExportFailed { error: String },
    MailData { mailbox: String, header: MailHeader, body: String, attachments: Vec<crate::render::Attachment> },
    /// Reply to `PollMailbox` (B10).
    MailboxPolled { mailbox: String, state: MailboxState },
    /// Reply to `FetchNewHeaders` (B10).
    NewHeaders { mailbox: String, headers: Vec<MailHeader> },
    /// Reply to `FetchHeadersFrom` (B3).
    HeadersFrom { mailbox: String, headers: Vec<MailHeader> },
    /// Reply to `Append` (B7): `raw` was saved into `mailbox`.
    Appended { mailbox: String },
    /// `Append` failed. A separate variant from `Error` for the same reason
    /// `PollFailed` is: by the time this fires, the send it followed has
    /// already succeeded and the UI has already shown "Message sent" --
    /// routing this through `Error` would silently overwrite that with a
    /// misleading "Error: ..." for a step the user never asked to watch.
    AppendFailed { mailbox: String, error: String },
    /// `PollMailbox`/`FetchNewHeaders` failed. Deliberately a separate
    /// variant from `Error` rather than reusing it: those two commands are
    /// background/best-effort (see their docs), and `main.rs` logs this
    /// instead of overwriting `self.status` with it, so a transient blip on
    /// a 60-second timer never stomps on whatever the user is actually
    /// looking at.
    PollFailed(String),
    /// Reply to `StoreFlags` (B8): the message's resulting flag list.
    FlagsUpdated { mailbox: String, uid: u32, flags: Vec<String>, req_id: u64 },
    /// `StoreFlags` failed. Separate from `Error` so a failed flag toggle
    /// (e.g. clicking the star while offline) can be reported/rolled back
    /// against the specific message it was for, the same reasoning
    /// `AppendFailed`/`PollFailed` already document for their own commands.
    FlagsUpdateFailed { mailbox: String, uid: u32, error: String, req_id: u64 },
    /// Reply to `MoveMessage` (B8): `uid` no longer exists in `mailbox`; it
    /// now lives in `dest`.
    Moved { mailbox: String, uid: u32, dest: String, req_id: u64 },
    /// `MoveMessage` failed (including its COPY+STORE+EXPUNGE fallback).
    MoveFailed { mailbox: String, uid: u32, error: String, req_id: u64 },
    /// Reply to `FetchUnreadCounts` (B8): one entry per mailbox that
    /// answered `STATUS` successfully -- a mailbox that failed (e.g.
    /// deleted between `FetchMailboxes` and this call) is simply missing
    /// from the map rather than failing the whole batch.
    UnreadCounts(std::collections::HashMap<String, u32>),
}

pub struct ImapActor {
    cmd_rx: mpsc::Receiver<ImapCommand>,
    event_tx: mpsc::Sender<ImapEvent>,
    session: Option<async_imap::Session<TlsStream<TcpStream>>>,
    /// Set on the first successful [`ImapActor::connect`]; reused by
    /// [`ImapActor::ensure_connected`] to reconnect without the user retyping
    /// their password.
    credentials: Option<Credentials>,
    /// Where `FetchBody`/`BulkDownload` get routed once connected -- see
    /// `spawn_body_worker`'s doc for why those two specifically live on a
    /// second connection instead of this actor's own `session`. `None`
    /// before the first successful `Connect`.
    worker_tx: Option<mpsc::Sender<WorkerCommand>>,
}

impl ImapActor {
    pub fn spawn(
        cmd_rx: mpsc::Receiver<ImapCommand>,
        event_tx: mpsc::Sender<ImapEvent>,
    ) {
        let mut actor = ImapActor {
            cmd_rx,
            event_tx,
            session: None,
            credentials: None,
            worker_tx: None,
        };

        tokio::spawn(async move {
            actor.run().await;
        });
    }

    async fn run(&mut self) {
        while let Some(cmd) = self.cmd_rx.recv().await {
            match cmd {
                ImapCommand::Connect { host, port, username, auth } => {
                    match self.connect(&host, port, &username, &auth).await {
                        Ok(_) => {
                            // Remembered so `ensure_connected` can reconnect
                            // without the user retyping their password.
                            let creds = Credentials { host, port, username, auth };
                            self.credentials = Some(creds.clone());
                            // One worker per successful `Connect`, not one
                            // per process: a second `Connect` (e.g. logging
                            // into a different account without restarting)
                            // should get a fresh worker on the new
                            // credentials rather than silently keep feeding
                            // `FetchBody`/`BulkDownload` to a worker still
                            // logged into the old account. The old worker's
                            // task simply ends once its `cmd_rx` (the
                            // `Sender` half we're about to drop here) closes.
                            let (worker_tx, worker_rx) = mpsc::channel(8);
                            spawn_body_worker(worker_rx, self.event_tx.clone(), creds);
                            self.worker_tx = Some(worker_tx);
                            let _ = self.event_tx.send(ImapEvent::Connected).await;
                        }
                        Err(e) => {
                            let _ = self.event_tx.send(ImapEvent::Error(e.to_string())).await;
                        }
                    }
                }
                ImapCommand::FetchMailboxes => {
                    if let Err(e) = self.ensure_connected().await {
                        let _ = self.event_tx.send(ImapEvent::Error(e.to_string())).await;
                        continue;
                    }
                    let session = self.session.as_mut().expect("ensure_connected just verified this");
                    match Self::fetch_mailboxes(session).await {
                        Ok(mbs) => {
                            let _ = self.event_tx.send(ImapEvent::Mailboxes(mbs)).await;
                        }
                        Err(e) => {
                            self.session = None;
                            let _ = self.event_tx.send(ImapEvent::Error(e.to_string())).await;
                        }
                    }
                }
                ImapCommand::FetchHeaders { mailbox, page, req_id } => {
                    if let Err(e) = self.ensure_connected().await {
                        let _ = self.event_tx.send(ImapEvent::Error(e.to_string())).await;
                        continue;
                    }
                    let session = self.session.as_mut().expect("ensure_connected just verified this");
                    match Self::fetch_headers(session, &mailbox, page).await {
                        Ok((headers, total_pages, mailbox_state)) => {
                            let _ = self.event_tx.send(ImapEvent::Headers { mailbox, headers, page, total_pages, req_id, mailbox_state }).await;
                        }
                        Err(e) => {
                            self.session = None;
                            let _ = self.event_tx.send(ImapEvent::Error(e.to_string())).await;
                        }
                    }
                }
                ImapCommand::FetchBody { mailbox, uid, req_id } => {
                    // Routed to the body worker's own connection (see
                    // `spawn_body_worker`) rather than handled here, so a
                    // slow body fetch can't stall `FetchHeaders`/
                    // `FetchMailboxes` waiting behind it in this actor's own
                    // command queue.
                    let Some(worker) = &self.worker_tx else {
                        let _ = self.event_tx.send(ImapEvent::BodyFailed { uid, req_id, error: "not connected".to_string() }).await;
                        continue;
                    };
                    let _ = worker.send(WorkerCommand::FetchBody { mailbox, uid, req_id }).await;
                }
                ImapCommand::BulkDownload { mailbox } => {
                    // Same reasoning as `FetchBody` above -- and doubly so
                    // here, since a bulk download is the single slowest,
                    // longest-running thing this actor ever does.
                    let Some(worker) = &self.worker_tx else {
                        let _ = self.event_tx.send(ImapEvent::Error("not connected".to_string())).await;
                        continue;
                    };
                    let _ = worker.send(WorkerCommand::BulkDownload { mailbox }).await;
                }
                ImapCommand::ExportMessage { mailbox, uid, path } => {
                    // Same routing as `FetchBody`: a full-message download
                    // has no business stalling this actor's own session.
                    let Some(worker) = &self.worker_tx else {
                        let _ = self.event_tx.send(ImapEvent::ExportFailed { error: "not connected".to_string() }).await;
                        continue;
                    };
                    let _ = worker.send(WorkerCommand::ExportMessage { mailbox, uid, path }).await;
                }
                ImapCommand::PollMailbox { mailbox } => {
                    // No `ensure_connected` here on purpose -- see the
                    // command's doc. Nothing to poll if there's no session.
                    let Some(session) = self.session.as_mut() else {
                        continue;
                    };
                    match session.examine(&mailbox).await {
                        Ok(mb) => {
                            let state = MailboxState {
                                uid_validity: mb.uid_validity.unwrap_or(0),
                                uid_next: mb.uid_next.unwrap_or(0),
                            };
                            let _ = self.event_tx.send(ImapEvent::MailboxPolled { mailbox, state }).await;
                        }
                        Err(e) => {
                            self.session = None;
                            let _ = self.event_tx.send(ImapEvent::PollFailed(e.to_string())).await;
                        }
                    }
                }
                ImapCommand::FetchNewHeaders { mailbox, first_uid } => {
                    let Some(session) = self.session.as_mut() else {
                        continue;
                    };
                    match Self::fetch_new_headers(session, &mailbox, first_uid).await {
                        Ok(headers) => {
                            let _ = self.event_tx.send(ImapEvent::NewHeaders { mailbox, headers }).await;
                        }
                        Err(e) => {
                            self.session = None;
                            let _ = self.event_tx.send(ImapEvent::PollFailed(e.to_string())).await;
                        }
                    }
                }
                ImapCommand::FetchHeadersFrom { mailbox, first_uid } => {
                    if let Err(e) = self.ensure_connected().await {
                        let _ = self.event_tx.send(ImapEvent::Error(e.to_string())).await;
                        continue;
                    }
                    let session = self.session.as_mut().expect("ensure_connected just verified this");
                    match Self::fetch_new_headers(session, &mailbox, first_uid).await {
                        Ok(headers) => {
                            let _ = self.event_tx.send(ImapEvent::HeadersFrom { mailbox, headers }).await;
                        }
                        Err(e) => {
                            self.session = None;
                            let _ = self.event_tx.send(ImapEvent::Error(e.to_string())).await;
                        }
                    }
                }
                ImapCommand::Append { mailbox, raw } => {
                    if let Err(e) = self.ensure_connected().await {
                        let _ = self.event_tx.send(ImapEvent::AppendFailed { mailbox, error: e.to_string() }).await;
                        continue;
                    }
                    let session = self.session.as_mut().expect("ensure_connected just verified this");
                    // The body upload can take a while for a message with
                    // large attachments and looks identical to a hang, so say
                    // what is happening before it starts (issue #79).
                    let _ = self
                        .event_tx
                        .send(ImapEvent::Progress { kind: ProgressKind::Append, progress: Progress::Indeterminate })
                        .await;
                    match session.append(&mailbox, &raw).await {
                        Ok(()) => {
                            let _ = self.event_tx.send(ImapEvent::Appended { mailbox }).await;
                        }
                        Err(e) => {
                            self.session = None;
                            let _ = self.event_tx.send(ImapEvent::AppendFailed { mailbox, error: e.to_string() }).await;
                        }
                    }
                }
                ImapCommand::StoreFlags { mailbox, uid, add, remove, req_id } => {
                    if let Err(e) = self.ensure_connected().await {
                        let _ = self.event_tx.send(ImapEvent::FlagsUpdateFailed { mailbox, uid, error: e.to_string(), req_id }).await;
                        continue;
                    }
                    let session = self.session.as_mut().expect("ensure_connected just verified this");
                    match Self::store_flags(session, &mailbox, uid, &add, &remove).await {
                        Ok(flags) => {
                            let _ = self.event_tx.send(ImapEvent::FlagsUpdated { mailbox, uid, flags, req_id }).await;
                        }
                        Err(e) => {
                            self.session = None;
                            let _ = self.event_tx.send(ImapEvent::FlagsUpdateFailed { mailbox, uid, error: e.to_string(), req_id }).await;
                        }
                    }
                }
                ImapCommand::MoveMessage { mailbox, uid, dest, req_id } => {
                    if let Err(e) = self.ensure_connected().await {
                        let _ = self.event_tx.send(ImapEvent::MoveFailed { mailbox, uid, error: e.to_string(), req_id }).await;
                        continue;
                    }
                    let session = self.session.as_mut().expect("ensure_connected just verified this");
                    match Self::move_message(session, &mailbox, uid, &dest).await {
                        Ok(()) => {
                            let _ = self.event_tx.send(ImapEvent::Moved { mailbox, uid, dest, req_id }).await;
                        }
                        Err(e) => {
                            self.session = None;
                            let _ = self.event_tx.send(ImapEvent::MoveFailed { mailbox, uid, error: e.to_string(), req_id }).await;
                        }
                    }
                }
                ImapCommand::FetchUnreadCounts { mailboxes } => {
                    if self.ensure_connected().await.is_err() {
                        // Best-effort/background, same reasoning as
                        // `PollMailbox` -- an unread-count refresh failing
                        // silently is preferable to it triggering a
                        // reconnect storm or an error banner over a UI
                        // element nobody explicitly asked to refresh.
                        continue;
                    }
                    let session = self.session.as_mut().expect("ensure_connected just verified this");
                    let counts = Self::fetch_unread_counts(session, &mailboxes).await;
                    let _ = self.event_tx.send(ImapEvent::UnreadCounts(counts)).await;
                }
            }
        }
    }

    /// Reconnect using the last credentials that worked, with exponential
    /// backoff, if the session was dropped (by `Connect` never having
    /// succeeded, or by a prior command failing and clearing `self.session`).
    /// A no-op — and free — when already connected.
    ///
    /// This only manages *this* actor's own session -- the one
    /// `FetchMailboxes`/`FetchHeaders`/`PollMailbox`/`FetchNewHeaders` use.
    /// It deliberately does not touch `self.worker_tx`: the body worker
    /// (`spawn_body_worker`, B2's session-pool split) keeps its own
    /// independent connection and reconnect loop, so a primary-session drop
    /// and reconnect here has no effect on it, and vice versa. IDLE (B11)
    /// is a third, still-separate connection (`idle_watch.rs`), managed
    /// entirely outside this actor.
    async fn ensure_connected(&mut self) -> anyhow::Result<()> {
        if self.session.is_some() {
            return Ok(());
        }
        let creds = self
            .credentials
            .clone()
            .ok_or_else(|| anyhow!("not connected yet"))?;

        let _ = self.event_tx.send(ImapEvent::Disconnected).await;

        let mut delay = Duration::from_secs(1);
        let mut last_err = None;
        for attempt in 1..=MAX_RECONNECT_ATTEMPTS {
            match self
                .connect(&creds.host, creds.port, &creds.username, &creds.auth)
                .await
            {
                Ok(()) => {
                    let _ = self.event_tx.send(ImapEvent::Connected).await;
                    return Ok(());
                }
                Err(e) => {
                    log::warn!("reconnect attempt {attempt}/{MAX_RECONNECT_ATTEMPTS} failed: {e}");
                    // Retrying cannot fix a revoked sign-in; only the user can.
                    let permanent = e.is::<SignInExpired>();
                    last_err = Some(e);
                    if permanent {
                        break;
                    }
                    if attempt < MAX_RECONNECT_ATTEMPTS {
                        tokio::time::sleep(delay).await;
                        delay = (delay * 2).min(MAX_RECONNECT_DELAY);
                    }
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("reconnect failed")))
    }

    async fn connect(&mut self, host: &str, port: u16, username: &str, auth: &Auth) -> anyhow::Result<()> {
        self.session = Some(connect_session(host, port, username, auth).await?);
        Ok(())
    }

    async fn fetch_mailboxes(session: &mut async_imap::Session<TlsStream<TcpStream>>) -> anyhow::Result<Vec<MailboxInfo>> {
        let mut mailboxes = Vec::new();
        let mut fetches = session.list(Some(""), Some("*")).await?;
        while let Some(name) = fetches.next().await {
            if let Ok(name) = name {
                let special_use = name
                    .attributes()
                    .iter()
                    .find_map(SpecialUse::from_attribute)
                    .or_else(|| SpecialUse::from_name(name.name()));
                let noselect = name
                    .attributes()
                    .iter()
                    .any(|a| matches!(a, async_imap::imap_proto::types::NameAttribute::NoSelect));
                mailboxes.push(MailboxInfo {
                    name: name.name().to_string(),
                    delimiter: name.delimiter().map(|d| d.to_string()),
                    special_use,
                    noselect,
                });
            }
        }
        Ok(mailboxes)
    }

    fn decode_rfc2047(bytes: &[u8]) -> String {
        let mut raw = b"X: ".to_vec();
        raw.extend_from_slice(bytes);
        raw.push(b'\n');
        if let Ok((header, _)) = mailparse::parse_header(&raw) {
            header.get_value()
        } else {
            String::from_utf8_lossy(bytes).to_string()
        }
    }

    /// Turn one fetched UID + its IMAP `ENVELOPE` into a [`MailHeader`].
    /// Factored out of `fetch_headers`/`bulk_download`, which each built
    /// this by hand before B10 needed a third copy for `fetch_new_headers` --
    /// three near-identical copies was the point at which "just duplicate it
    /// again" stopped being the lower-risk option.
    fn parse_envelope_header(uid: u32, envelope: &async_imap::imap_proto::Envelope<'_>, flags: Vec<String>) -> MailHeader {
        let subject = envelope.subject.as_ref().map(|s| Self::decode_rfc2047(s)).unwrap_or_default();

        // Every address in the field, comma-joined (`compose.rs`'s Reply-All
        // splits this back apart) — not just the first, which used to be all
        // that survived here and left Reply-All unable to see any Cc/To
        // recipient past the first.
        let format_address = |addrs: Option<&[async_imap::imap_proto::Address<'_>]>| -> String {
            addrs
                .map(|list| {
                    list.iter()
                        .map(|addr| {
                            let name = addr.name.as_ref().map(|n| Self::decode_rfc2047(n));
                            let mailbox = addr.mailbox.as_ref().map(|m| String::from_utf8_lossy(m).to_string()).unwrap_or_default();
                            let host = addr.host.as_ref().map(|h| String::from_utf8_lossy(h).to_string()).unwrap_or_default();
                            match name {
                                Some(n) => format!("{} <{}@{}>", n, mailbox, host),
                                None => format!("{}@{}", mailbox, host),
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default()
        };

        let from = format_address(envelope.from.as_deref());
        let to = format_address(envelope.to.as_deref());
        let date = envelope.date.as_ref().map(|d| String::from_utf8_lossy(d).to_string()).unwrap_or_default();
        let message_id = envelope.message_id.as_ref().map(|m| String::from_utf8_lossy(m).to_string()).unwrap_or_default();

        MailHeader { uid, subject, from, to, date, message_id, flags }
    }

    /// A `Fetch`'s IMAP flags (e.g. `\Seen`, `\Flagged`) as the raw strings
    /// `MailHeader::flags`/`STORE` queries use -- `flag_to_str` handles the
    /// mapping since `async_imap::types::Flag` has no `Display`.
    fn fetch_flags(msg: &async_imap::types::Fetch) -> Vec<String> {
        msg.flags().map(|f| flag_to_str(&f)).collect()
    }

    async fn fetch_headers(session: &mut async_imap::Session<TlsStream<TcpStream>>, mailbox_name: &str, page: u32) -> anyhow::Result<(Vec<MailHeader>, u32, MailboxState)> {
        let mailbox = session.examine(mailbox_name).await?;
        // `examine` already gets these off the server's untagged response —
        // no extra round trip. `db.rs`'s incremental-sync bookkeeping
        // (`DbCommand::ReportMailboxState`) rides along on every header
        // fetch for free.
        let mailbox_state = MailboxState {
            uid_validity: mailbox.uid_validity.unwrap_or(0),
            uid_next: mailbox.uid_next.unwrap_or(0),
        };

        let total = mailbox.exists;
        if total == 0 {
            return Ok((Vec::new(), 0, mailbox_state));
        }

        let per_page = 50;
        let total_pages = (total + per_page - 1) / per_page;
        let page = page.min(total_pages).max(1);

        let end = total.saturating_sub((page - 1) * per_page);
        let start = end.saturating_sub(per_page - 1).max(1);

        let query = format!("{}:{}", start, end);
        let fetches = session.fetch(query, "(UID ENVELOPE FLAGS)").await?;
        let messages = fetches.collect::<Vec<_>>().await;

        let mut headers = Vec::new();
        for msg in messages {
            let msg = msg?;
            let uid = msg.uid.ok_or_else(|| anyhow!("No UID"))?;
            let envelope = msg.envelope().ok_or_else(|| anyhow!("No envelope"))?;
            let flags = Self::fetch_flags(&msg);
            headers.push(Self::parse_envelope_header(uid, envelope, flags));
        }

        headers.reverse(); // Newest first
        Ok((headers, total_pages, mailbox_state))
    }

    /// Fetch one message's full raw RFC822 source.
    async fn fetch_raw(session: &mut async_imap::Session<TlsStream<TcpStream>>, mailbox_name: &str, uid: u32) -> anyhow::Result<Vec<u8>> {
        session.examine(mailbox_name).await?;
        let query = format!("{}", uid);
        let mut fetches = session.uid_fetch(query, "RFC822").await?;

        if let Some(msg) = fetches.next().await {
            let msg = msg?;
            let body = msg.body().ok_or_else(|| anyhow::anyhow!("No body"))?;
            return Ok(body.to_vec());
        }

        Err(anyhow!("Message not found or no body"))
    }

    async fn fetch_body(session: &mut async_imap::Session<TlsStream<TcpStream>>, mailbox_name: &str, uid: u32) -> anyhow::Result<(String, Vec<crate::render::Attachment>)> {
        let raw = Self::fetch_raw(session, mailbox_name, uid).await?;
        let html = crate::render::render_message(&raw);
        let attachments = crate::render::extract_attachments(&raw);
        Ok((html, attachments))
    }

    /// Fetch envelopes for every UID from `first_uid` onward (B10). Same
    /// `EXAMINE` + `UID FETCH ... (UID ENVELOPE)` shape as `fetch_headers`,
    /// minus the paging -- this always wants everything from `first_uid` to
    /// the end, since it exists to describe exactly the messages a
    /// `notify::WatermarkUpdate::NewMail` just reported as new.
    async fn fetch_new_headers(
        session: &mut async_imap::Session<TlsStream<TcpStream>>,
        mailbox_name: &str,
        first_uid: u32,
    ) -> anyhow::Result<Vec<MailHeader>> {
        session.examine(mailbox_name).await?;
        let query = format!("{}:*", first_uid);
        let fetches = session.uid_fetch(query, "(UID ENVELOPE FLAGS)").await?;
        let messages = fetches.collect::<Vec<_>>().await;

        let mut headers = Vec::new();
        for msg in messages {
            let msg = msg?;
            let uid = msg.uid.ok_or_else(|| anyhow!("No UID"))?;
            let envelope = msg.envelope().ok_or_else(|| anyhow!("No envelope"))?;
            let flags = Self::fetch_flags(&msg);
            headers.push(Self::parse_envelope_header(uid, envelope, flags));
        }
        Ok(headers)
    }

    async fn bulk_download(
        session: &mut async_imap::Session<TlsStream<TcpStream>>,
        mailbox_name: &str,
        event_tx: &mpsc::Sender<ImapEvent>,
    ) -> anyhow::Result<()> {
        let mailbox = session.examine(mailbox_name).await?;
        let total = mailbox.exists;
        if total == 0 {
            return Ok(());
        }

        let _ = event_tx
            .send(ImapEvent::Progress { kind: ProgressKind::Index, progress: Progress::Counted { current: 0, total } })
            .await;

        // Fetch all UIDs and Envelopes first to get metadata
        let query = format!("1:{}", total);
        let fetches = session.fetch(query, "(UID ENVELOPE FLAGS)").await?;
        let messages = fetches.collect::<Vec<_>>().await;

        for (i, msg) in messages.into_iter().enumerate() {
            let msg = msg?;
            let uid = msg.uid.ok_or_else(|| anyhow!("No UID"))?;
            let envelope = msg.envelope().ok_or_else(|| anyhow!("No envelope"))?;
            let flags = Self::fetch_flags(&msg);
            let header = Self::parse_envelope_header(uid, envelope, flags);

            // Now fetch body for this UID
            let body_query = format!("{}", uid);
            let mut body_fetches = session.uid_fetch(body_query, "RFC822").await?;
            let mut body = String::new();
            let mut attachments = Vec::new();
            if let Some(body_msg) = body_fetches.next().await {
                let body_msg = body_msg?;
                if let Some(bytes) = body_msg.body() {
                    body = crate::render::render_message(bytes);
                    attachments = crate::render::extract_attachments(bytes);
                }
            }

            let _ = event_tx.send(ImapEvent::MailData {
                mailbox: mailbox_name.to_string(),
                header,
                body,
                attachments,
            }).await;

            let _ = event_tx
                .send(ImapEvent::Progress {
                    kind: ProgressKind::Index,
                    progress: Progress::Counted { current: (i + 1) as u32, total },
                })
                .await;
        }

        Ok(())
    }

    /// Add/remove flags on one message (B8): `SELECT`s `mailbox_name` (not
    /// `EXAMINE` -- `STORE` needs write access) then issues one `UID STORE`
    /// per non-empty side (`+FLAGS`/`-FLAGS`), returning the resulting flag
    /// list from whichever `STORE` ran last. Two round trips when both `add`
    /// and `remove` are non-empty rather than one combined command -- IMAP
    /// has no single `STORE` verb that both adds and removes different flags
    /// in the same call, only replaces the whole set (`FLAGS`, which risks
    /// clobbering a flag set by something else between the read and the
    /// write) or adds/removes one set at a time.
    async fn store_flags(
        session: &mut async_imap::Session<TlsStream<TcpStream>>,
        mailbox_name: &str,
        uid: u32,
        add: &[String],
        remove: &[String],
    ) -> anyhow::Result<Vec<String>> {
        session.select(mailbox_name).await?;
        let mut last_flags: Option<Vec<String>> = None;

        if !add.is_empty() {
            let query = format!("+FLAGS ({})", add.join(" "));
            let mut stream = session.uid_store(uid.to_string(), query).await?;
            while let Some(msg) = stream.next().await {
                last_flags = Some(Self::fetch_flags(&msg?));
            }
        }
        if !remove.is_empty() {
            let query = format!("-FLAGS ({})", remove.join(" "));
            let mut stream = session.uid_store(uid.to_string(), query).await?;
            while let Some(msg) = stream.next().await {
                last_flags = Some(Self::fetch_flags(&msg?));
            }
        }

        last_flags.ok_or_else(|| anyhow!("STORE completed but the server reported no resulting flags for UID {uid}"))
    }

    /// Move one message to `dest` (B8's delete-to-Trash/archive): `SELECT`s
    /// `mailbox_name`, tries the real `MOVE` extension
    /// (`async_imap::Session::uid_mv`), and if that fails for any reason
    /// (server doesn't support it, or the attempt itself errors) falls back
    /// to `COPY` + `STORE +FLAGS.SILENT \Deleted` + `EXPUNGE` -- the same
    /// three steps `MOVE` is defined to be equivalent to (see `uid_mv`'s own
    /// doc comment). The fallback's `EXPUNGE` is a bare, mailbox-wide
    /// `EXPUNGE` rather than `UID EXPUNGE <uid>` (which needs the `UIDPLUS`
    /// extension this client doesn't check for) -- safe here because nothing
    /// else in this client's own flow marks a message `\Deleted` without
    /// immediately expunging it, so the only `\Deleted` message in the
    /// mailbox at that point is this one.
    async fn move_message(
        session: &mut async_imap::Session<TlsStream<TcpStream>>,
        mailbox_name: &str,
        uid: u32,
        dest: &str,
    ) -> anyhow::Result<()> {
        session.select(mailbox_name).await?;

        if session.uid_mv(uid.to_string(), dest).await.is_ok() {
            return Ok(());
        }

        // Fallback: COPY, mark \Deleted, EXPUNGE.
        session.uid_copy(uid.to_string(), dest).await?;
        let mut stream = session.uid_store(uid.to_string(), "+FLAGS.SILENT (\\Deleted)").await?;
        while let Some(msg) = stream.next().await {
            msg?;
        }
        drop(stream);
        session.expunge().await?.collect::<Vec<_>>().await;
        Ok(())
    }

    /// `STATUS (UNSEEN)` for each of `mailboxes` (B8), sequentially on this
    /// one session -- issued right after `FetchMailboxes`, whose reply names
    /// every mailbox to ask about. A mailbox whose `STATUS` errors (e.g. it
    /// was renamed/removed between the two calls) is silently skipped rather
    /// than failing the whole batch, since one stale/missing count shouldn't
    /// hide every other mailbox's. Sequential, one round trip per mailbox,
    /// rather than pipelined: acceptable for the handful of mailboxes a
    /// typical account has, and pipelining `STATUS` would need queuing
    /// several commands ahead of their responses, which this client's
    /// request/response session wrapper isn't set up to do anywhere else
    /// either.
    async fn fetch_unread_counts(
        session: &mut async_imap::Session<TlsStream<TcpStream>>,
        mailboxes: &[String],
    ) -> std::collections::HashMap<String, u32> {
        let mut counts = std::collections::HashMap::new();
        for mailbox in mailboxes {
            if let Ok(status) = session.status(mailbox, "(UNSEEN)").await {
                counts.insert(mailbox.clone(), status.unseen.unwrap_or(0));
            }
        }
        counts
    }
}

/// Maps an `async_imap`/`imap-proto` [`async_imap::types::Flag`] to the raw
/// IMAP flag string it came from (e.g. `Flag::Seen` -> `"\Seen"`). Needed
/// because that type has no `Display` impl; the reverse direction (building
/// a `STORE` query from a flag string) needs no such mapping since `STORE`'s
/// argument is just a string already.
fn flag_to_str(flag: &async_imap::types::Flag<'_>) -> String {
    use async_imap::types::Flag;
    match flag {
        Flag::Seen => "\\Seen".to_string(),
        Flag::Answered => "\\Answered".to_string(),
        Flag::Flagged => "\\Flagged".to_string(),
        Flag::Deleted => "\\Deleted".to_string(),
        Flag::Draft => "\\Draft".to_string(),
        Flag::Recent => "\\Recent".to_string(),
        Flag::MayCreate => "\\*".to_string(),
        Flag::Custom(s) => s.to_string(),
    }
}

/// The TLS-connect-then-log-in sequence, shared by [`ImapActor::connect`]
/// and [`spawn_body_worker`]'s own independent connection -- previously
/// inlined once in each of `ImapActor`'s two (now three, counting the
/// worker) call sites before B2's session-pool split gave it a second
/// caller. A password account logs in with `LOGIN`; an OAuth one with
/// `AUTHENTICATE XOAUTH2` and a freshly obtained access token.
pub(crate) async fn connect_session(
    host: &str,
    port: u16,
    username: &str,
    auth: &Auth,
) -> anyhow::Result<async_imap::Session<TlsStream<TcpStream>>> {
    // Fetched before the connection is opened so a token-refresh failure
    // (the sign-in was revoked, say) is reported as itself rather than as a
    // half-open connection that then errors out.
    let secret = auth.secret().await?;

    let tls_connector = TlsConnector::builder().build()?;
    let tokio_tls_connector = tokio_native_tls::TlsConnector::from(tls_connector);

    let stream = TcpStream::connect((host, port)).await?;
    let tls_stream = tokio_tls_connector.connect(host, stream).await?;
    let mut client = async_imap::Client::new(tls_stream);
    let _ = client.read_response().await;

    let session = match auth {
        Auth::Password(_) => client.login(username, secret.expose_secret()).await.map_err(|(e, _)| e)?,
        Auth::OAuth(_) => client
            .authenticate("XOAUTH2", XOAuth2::new(username, &secret))
            .await
            .map_err(|(e, _)| e)?,
    };
    Ok(session)
}

/// Commands [`ImapActor`] hands off to [`spawn_body_worker`]'s dedicated
/// connection rather than handling on its own `session`.
enum WorkerCommand {
    FetchBody { mailbox: String, uid: u32, req_id: u64 },
    BulkDownload { mailbox: String },
    ExportMessage { mailbox: String, uid: u32, path: std::path::PathBuf },
}

/// B2's session-pool split: a second, independent IMAP connection that
/// handles only [`ImapCommand::FetchBody`]/[`ImapCommand::BulkDownload`],
/// so opening a message (or running a bulk download) never blocks
/// `ImapActor`'s own session -- the one `FetchHeaders`/`FetchMailboxes` use
/// to keep the header list and mailbox tree responsive. This was the one
/// piece of B2 (PLAN.md's own wording: "one long-lived control session ...
/// plus a worker session for fetches") that stayed undone through B7 for
/// lack of anything to verify a live-IMAP-protocol rework against;
/// `mail-mock-server` (added since) is what unblocks it now, the same way
/// it unblocked B11's `IDLE` support.
///
/// Deliberately its own small connect/reconnect loop rather than sharing
/// `ImapActor::ensure_connected`: that method emits `ImapEvent::Connected`/
/// `Disconnected`, which the UI uses to gate the whole "are we logged in"
/// state (`AccountView::state`) and the account forwarder's poll
/// gate. A worker reconnect blipping that global state on every dropped
/// body fetch would be misleading -- the *account* is still connected as
/// far as the user should see, only this one background connection needed
/// to retry. So failures here are reported only on the specific request
/// that hit them (`ImapEvent::Error`), and a successful reconnect is
/// silent, exactly like a request that never needed to reconnect at all.
fn spawn_body_worker(
    mut cmd_rx: mpsc::Receiver<WorkerCommand>,
    event_tx: mpsc::Sender<ImapEvent>,
    credentials: Credentials,
) {
    tokio::spawn(async move {
        let mut session: Option<async_imap::Session<TlsStream<TcpStream>>> = None;

        while let Some(cmd) = cmd_rx.recv().await {
            if session.is_none() {
                match ensure_worker_connected(&credentials).await {
                    Ok(s) => session = Some(s),
                    Err(e) => {
                        // Report against the specific request that hit this,
                        // not a generic `Error`, so a `FetchBody` whose
                        // connection attempt failed still gets a reply
                        // `main.rs` can match against `current_body_req` and
                        // use to clear "Loading message..." -- see
                        // `ImapEvent::BodyFailed`'s doc. `BulkDownload` has
                        // no single uid/req_id to attribute to, so it keeps
                        // the generic `Error`.
                        match cmd {
                            WorkerCommand::FetchBody { uid, req_id, .. } => {
                                let _ = event_tx.send(ImapEvent::BodyFailed {
                                    uid,
                                    req_id,
                                    error: format!("could not open a connection for this request: {e}"),
                                }).await;
                            }
                            WorkerCommand::BulkDownload { .. } => {
                                let _ = event_tx.send(ImapEvent::Error(format!("could not open a connection for this request: {e}"))).await;
                            }
                            WorkerCommand::ExportMessage { .. } => {
                                let _ = event_tx.send(ImapEvent::ExportFailed {
                                    error: format!("could not open a connection for this request: {e}"),
                                }).await;
                            }
                        }
                        continue;
                    }
                }
            }
            let sess = session.as_mut().expect("just verified Some above");

            match cmd {
                WorkerCommand::FetchBody { mailbox, uid, req_id } => {
                    match ImapActor::fetch_body(sess, &mailbox, uid).await {
                        Ok((html, attachments)) => {
                            let _ = event_tx.send(ImapEvent::Body { uid, html, attachments, req_id }).await;
                        }
                        Err(e) => {
                            session = None; // let the next command's ensure_worker_connected retry
                            let _ = event_tx.send(ImapEvent::BodyFailed { uid, req_id, error: e.to_string() }).await;
                        }
                    }
                }
                WorkerCommand::ExportMessage { mailbox, uid, path } => {
                    match ImapActor::fetch_raw(sess, &mailbox, uid).await {
                        Ok(raw) => match tokio::fs::write(&path, raw).await {
                            Ok(()) => {
                                let _ = event_tx.send(ImapEvent::Exported { path }).await;
                            }
                            // A local file error says nothing about the
                            // IMAP session, so keep it.
                            Err(e) => {
                                let _ = event_tx.send(ImapEvent::ExportFailed {
                                    error: format!("could not write {}: {e}", path.display()),
                                }).await;
                            }
                        },
                        Err(e) => {
                            session = None;
                            let _ = event_tx.send(ImapEvent::ExportFailed { error: format!("{e:#}") }).await;
                        }
                    }
                }
                WorkerCommand::BulkDownload { mailbox } => {
                    if let Err(e) = ImapActor::bulk_download(sess, &mailbox, &event_tx).await {
                        session = None;
                        let _ = event_tx.send(ImapEvent::Error(e.to_string())).await;
                    }
                }
            }
        }
    });
}

/// Connect-with-backoff for [`spawn_body_worker`], mirroring
/// [`ImapActor::ensure_connected`]'s retry shape (same attempt count and
/// delay curve, see `MAX_RECONNECT_ATTEMPTS`/`MAX_RECONNECT_DELAY`) but
/// returning the session instead of storing it on `self`, and never
/// sending `Connected`/`Disconnected` -- see `spawn_body_worker`'s doc for
/// why.
async fn ensure_worker_connected(credentials: &Credentials) -> anyhow::Result<async_imap::Session<TlsStream<TcpStream>>> {
    let mut delay = Duration::from_secs(1);
    let mut last_err = None;
    for attempt in 1..=MAX_RECONNECT_ATTEMPTS {
        match connect_session(&credentials.host, credentials.port, &credentials.username, &credentials.auth).await {
            Ok(session) => return Ok(session),
            Err(e) => {
                log::warn!("body worker reconnect attempt {attempt}/{MAX_RECONNECT_ATTEMPTS} failed: {e}");
                // See `ImapActor::ensure_connected`.
                let permanent = e.is::<SignInExpired>();
                last_err = Some(e);
                if permanent {
                    break;
                }
                if attempt < MAX_RECONNECT_ATTEMPTS {
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(MAX_RECONNECT_DELAY);
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("reconnect failed")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mb(name: &str, delimiter: &str, special_use: Option<SpecialUse>) -> MailboxInfo {
        MailboxInfo { name: name.to_string(), delimiter: Some(delimiter.to_string()), special_use, noselect: false }
    }

    fn mb_noselect(name: &str, delimiter: &str) -> MailboxInfo {
        MailboxInfo { name: name.to_string(), delimiter: Some(delimiter.to_string()), special_use: None, noselect: true }
    }

    // ── MailHeader::is_seen / is_flagged ─────────────────────────────────────

    #[test]
    fn is_seen_and_is_flagged_read_the_flags_list() {
        let mut h = MailHeader {
            uid: 1,
            subject: String::new(),
            from: String::new(),
            to: String::new(),
            date: String::new(),
            message_id: String::new(),
            flags: vec![],
        };
        assert!(!h.is_seen());
        assert!(!h.is_flagged());

        h.flags = vec!["\\Seen".to_string(), "\\Flagged".to_string()];
        assert!(h.is_seen());
        assert!(h.is_flagged());
    }

    // ── SpecialUse::from_name ─────────────────────────────────────────────────

    #[test]
    fn special_use_from_name_recognizes_well_known_folders_case_insensitively() {
        assert_eq!(SpecialUse::from_name("INBOX"), Some(SpecialUse::Inbox));
        assert_eq!(SpecialUse::from_name("sent"), Some(SpecialUse::Sent));
        assert_eq!(SpecialUse::from_name("Trash"), Some(SpecialUse::Trash));
        assert_eq!(SpecialUse::from_name("Work"), None);
    }

    #[test]
    fn special_use_from_name_does_not_match_a_substring() {
        // A folder that merely contains "sent" in a longer name must not be
        // misclassified as the Sent special-use folder.
        assert_eq!(SpecialUse::from_name("Sent to Boss"), None);
    }

    // ── mailbox_tree ──────────────────────────────────────────────────────────

    #[test]
    fn mailbox_tree_puts_flat_mailboxes_at_the_root() {
        let mailboxes = vec![mb("INBOX", "/", Some(SpecialUse::Inbox)), mb("Archive", "/", Some(SpecialUse::Archive))];
        let tree = mailbox_tree(&mailboxes);
        let labels: Vec<&str> = tree.iter().map(|n| n.label.as_str()).collect();
        // INBOX ranks ahead of Archive regardless of alphabetical order.
        assert_eq!(labels, vec!["INBOX", "Archive"]);
    }

    #[test]
    fn mailbox_tree_splits_on_the_delimiter_into_a_hierarchy() {
        let mailboxes = vec![mb("Work/Invoices", "/", None), mb("Work/Receipts", "/", None)];
        let tree = mailbox_tree(&mailboxes);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].label, "Work");
        assert_eq!(tree[0].full_name, None, "Work itself was never listed, only its children");
        let child_labels: Vec<&str> = tree[0].children.iter().map(|n| n.label.as_str()).collect();
        assert_eq!(child_labels, vec!["Invoices", "Receipts"]);
        assert_eq!(tree[0].children[0].full_name.as_deref(), Some("Work/Invoices"));
    }

    #[test]
    fn mailbox_tree_sorts_special_use_folders_first_in_a_fixed_order() {
        let mailboxes = vec![
            mb("Zzz", "/", None),
            mb("Trash", "/", Some(SpecialUse::Trash)),
            mb("INBOX", "/", Some(SpecialUse::Inbox)),
            mb("Aaa", "/", None),
            mb("Sent", "/", Some(SpecialUse::Sent)),
        ];
        let tree = mailbox_tree(&mailboxes);
        let labels: Vec<&str> = tree.iter().map(|n| n.label.as_str()).collect();
        assert_eq!(labels, vec!["INBOX", "Sent", "Trash", "Aaa", "Zzz"]);
    }

    #[test]
    fn mailbox_tree_treats_an_empty_delimiter_as_no_hierarchy() {
        // A server with no hierarchy (delimiter NIL, `MailboxInfo::delimiter`
        // as `None`) must not be split at all, even if a mailbox name
        // happens to contain a `/`.
        let mailboxes = vec![MailboxInfo { name: "A/B".to_string(), delimiter: None, special_use: None, noselect: false }];
        let tree = mailbox_tree(&mailboxes);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].label, "A/B");
        assert_eq!(tree[0].full_name.as_deref(), Some("A/B"));
        assert!(tree[0].children.is_empty());
    }

    #[test]
    fn mailbox_tree_treats_a_noselect_mailbox_like_one_never_listed_at_all() {
        // Regression test for issue #10: Gmail advertises `[Gmail]` itself
        // via LIST, but marked `\Noselect` -- SELECT/EXAMINE-ing it fails
        // server-side. It must render as a plain hierarchy label (full_name
        // unset), the same as a name that was never LISTed, not as a
        // clickable mailbox that then throws a NONEXISTENT error on click.
        let mailboxes = vec![mb_noselect("[Gmail]", "/"), mb("[Gmail]/Sent Mail", "/", Some(SpecialUse::Sent))];
        let tree = mailbox_tree(&mailboxes);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].label, "[Gmail]");
        assert_eq!(tree[0].full_name, None, "a \\Noselect mailbox must not be selectable");
        assert_eq!(tree[0].children[0].full_name.as_deref(), Some("[Gmail]/Sent Mail"));
    }

    #[test]
    fn flatten_tree_preserves_depth_and_order() {
        let mailboxes = vec![mb("INBOX", "/", Some(SpecialUse::Inbox)), mb("Work/Invoices", "/", None)];
        let rows = flatten_tree(&mailbox_tree(&mailboxes));
        assert_eq!(rows.len(), 3); // INBOX, Work, Work/Invoices
        assert_eq!((rows[0].depth, rows[0].label.as_str(), rows[0].full_name.as_deref()), (0, "INBOX", Some("INBOX")));
        assert_eq!((rows[1].depth, rows[1].label.as_str(), rows[1].full_name.as_deref()), (0, "Work", None));
        assert_eq!((rows[2].depth, rows[2].label.as_str(), rows[2].full_name.as_deref()), (1, "Invoices", Some("Work/Invoices")));
    }

    fn keys(rows: &[MailboxRow], indices: &[usize]) -> Vec<String> {
        indices.iter().map(|&i| rows[i].key.clone()).collect()
    }

    fn collapsed(keys: &[&str]) -> std::collections::BTreeSet<String> {
        keys.iter().map(|k| k.to_string()).collect()
    }

    #[test]
    fn flatten_tree_gives_every_row_a_path_key_and_a_has_children_flag() {
        let mailboxes = vec![mb("INBOX", "/", Some(SpecialUse::Inbox)), mb("Work/Invoices", "/", None)];
        let rows = flatten_tree(&mailbox_tree(&mailboxes));
        let got: Vec<_> = rows.iter().map(|r| (r.key.as_str(), r.has_children)).collect();
        assert_eq!(got, vec![("INBOX", false), ("Work", true), ("Work/Invoices", false)]);
    }

    #[test]
    fn visible_rows_shows_everything_when_nothing_is_collapsed() {
        let rows = flatten_tree(&mailbox_tree(&[mb("A/B/C", "/", None), mb("A/D", "/", None)]));
        assert_eq!(visible_rows(&rows, &collapsed(&[])), (0..rows.len()).collect::<Vec<_>>());
    }

    #[test]
    fn visible_rows_hides_only_the_descendants_of_a_collapsed_node() {
        let rows = flatten_tree(&mailbox_tree(&[mb("A/B/C", "/", None), mb("A/D", "/", None), mb("E", "/", None)]));
        // Rows: A, A/B, A/B/C, A/D, E
        let shown = visible_rows(&rows, &collapsed(&["A/B"]));
        assert_eq!(keys(&rows, &shown), vec!["A", "A/B", "A/D", "E"]);
        let shown = visible_rows(&rows, &collapsed(&["A"]));
        assert_eq!(keys(&rows, &shown), vec!["A", "E"]);
    }

    #[test]
    fn visible_rows_ignores_the_collapsed_flag_on_a_node_without_children() {
        let rows = flatten_tree(&mailbox_tree(&[mb("A", "/", None), mb("B", "/", None)]));
        assert_eq!(visible_rows(&rows, &collapsed(&["A"])).len(), 2);
    }

    #[test]
    fn descendants_covers_exactly_the_subtree_below_a_row() {
        let rows = flatten_tree(&mailbox_tree(&[mb("A/B/C", "/", None), mb("A/D", "/", None), mb("E", "/", None)]));
        let below = |i: usize| descendants(&rows, i).iter().map(|r| r.key.as_str()).collect::<Vec<_>>();
        assert_eq!(below(0), vec!["A/B", "A/B/C", "A/D"]);
        assert_eq!(below(1), vec!["A/B/C"]);
        assert!(below(2).is_empty());
        assert!(below(4).is_empty()); // last row
    }

    // ── sender_address / sender_name ──────────────────────────────────────────

    fn header_from(from: &str) -> MailHeader {
        MailHeader { uid: 1, subject: String::new(), from: from.to_string(), to: String::new(), date: String::new(), message_id: String::new(), flags: vec![] }
    }

    #[test]
    fn sender_address_takes_the_angle_bracketed_address_lowercased() {
        assert_eq!(header_from("Alice Doe <Alice@Example.COM>").sender_address().as_deref(), Some("alice@example.com"));
    }

    #[test]
    fn sender_address_accepts_a_bare_address() {
        assert_eq!(header_from("bob@example.com").sender_address().as_deref(), Some("bob@example.com"));
    }

    #[test]
    fn sender_address_is_the_last_bracket_pair_when_the_name_contains_one() {
        assert_eq!(header_from("Eve <fake@a.com> <real@b.com>").sender_address().as_deref(), Some("real@b.com"));
    }

    #[test]
    fn sender_address_is_none_without_an_address() {
        assert_eq!(header_from("").sender_address(), None);
        assert_eq!(header_from("Nobody").sender_address(), None);
    }

    #[test]
    fn sender_address_is_none_when_the_local_part_or_domain_is_missing() {
        // What `parse_envelope_header` produces for an envelope address with
        // no mailbox/host.
        for from in ["@", "Undisclosed <@>", "<@>", "alice@", "@example.com", "Name <@example.com>"] {
            assert_eq!(header_from(from).sender_address(), None, "{from}");
        }
    }

    #[test]
    fn sender_name_prefers_the_display_name_and_strips_quotes() {
        assert_eq!(header_from("Alice Doe <a@example.com>").sender_name(), "Alice Doe");
        assert_eq!(header_from("\"Doe, Alice\" <a@example.com>").sender_name(), "Doe, Alice");
    }

    #[test]
    fn sender_name_falls_back_to_the_address() {
        assert_eq!(header_from("a@example.com").sender_name(), "a@example.com");
        assert_eq!(header_from(" <a@example.com>").sender_name(), "a@example.com");
    }

    // ── local_date_time ───────────────────────────────────────────────────────

    fn utc() -> chrono::FixedOffset {
        chrono::FixedOffset::east_opt(0).unwrap()
    }

    fn parts(raw: &str, zone: chrono::FixedOffset) -> Option<(String, String)> {
        format_date_time(raw, &zone)
    }

    #[test]
    fn date_time_is_formatted_as_year_month_day_and_hour_minute() {
        let got = parts("Mon, 21 Sep 2026 09:05:33 +0000", utc());
        assert_eq!(got, Some(("2026-09-21".to_string(), "09:05".to_string())));
    }

    #[test]
    fn date_time_is_converted_from_the_senders_offset_to_the_target_zone() {
        // 23:30 in UTC-5 is 04:30 the next day in UTC and 05:30 in UTC+1.
        let raw = "Mon, 21 Sep 2026 23:30:00 -0500";
        assert_eq!(parts(raw, utc()), Some(("2026-09-22".to_string(), "04:30".to_string())));
        let paris = chrono::FixedOffset::east_opt(3600).unwrap();
        assert_eq!(parts(raw, paris), Some(("2026-09-22".to_string(), "05:30".to_string())));
    }

    #[test]
    fn date_time_is_none_for_a_missing_or_unreadable_date() {
        assert_eq!(parts("", utc()), None);
        assert_eq!(parts("last tuesday", utc()), None);
    }

    // ── flag_to_str ───────────────────────────────────────────────────────────

    #[test]
    fn flag_to_str_maps_system_flags() {
        use async_imap::types::Flag;
        assert_eq!(flag_to_str(&Flag::Seen), "\\Seen");
        assert_eq!(flag_to_str(&Flag::Flagged), "\\Flagged");
        assert_eq!(flag_to_str(&Flag::Deleted), "\\Deleted");
    }

    #[test]
    fn flag_to_str_passes_a_custom_flag_through_verbatim() {
        use async_imap::types::Flag;
        assert_eq!(flag_to_str(&Flag::Custom("$MyLabel".into())), "$MyLabel");
    }
}
