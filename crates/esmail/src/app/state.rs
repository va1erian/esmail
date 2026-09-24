//! The plain data [`super::AppCore`] is made of.

use std::collections::{BTreeSet, HashMap};

use crate::auth::Auth;
use crate::config::AccountConfig;
use crate::imap::MailboxRow;
use crate::progress::{Progress, ProgressKind};
use crate::session::AccountSession;

/// A dismissable error notice (B9), replacing the old pattern of clobbering
/// `AppCore::status` with `format!("Error: {e}")`/`format!("DB Error: {e}")`
/// — which lost whatever the status string was showing before (e.g. "Page 3
/// of 9") the moment an unrelated background error arrived, and gave the
/// user no way to see more than the single most recent one. `status` itself
/// stays for transient, non-error progress text ("Connecting...", "Page 3 of
/// 9") — this only replaces the error half of that one field's job.
pub struct Banner {
    pub id: u64,
    pub message: String,
}

/// The one middle-to-long operation shown in the bottom status bar, if any --
/// issue #79's single progress slot. A report for a new operation replaces
/// whatever was there, and a terminal event clears it only while it is still
/// the kind being shown, so a superseded report can't blank the current one.
pub struct ProgressView {
    pub kind: ProgressKind,
    pub progress: Progress,
}

/// A bulk action over a selection (mark read/unread, star/unstar, archive,
/// delete) in flight. Only one runs at a time -- the buttons that start one
/// are disabled while this is `Some` (issue #79's one-at-a-time decision) --
/// and [`super::AppCore::advance_bulk_action`] turns each per-message reply
/// into an update of the status bar's `done/total`.
pub struct BulkAction {
    pub kind: ProgressKind,
    pub total: u32,
    pub pending: BTreeSet<u32>,
}

/// Where one account's connection stands, for the folder pane.
#[derive(Debug, Clone, PartialEq)]
pub enum ConnState {
    /// The first connect is still in flight.
    Connecting,
    Connected,
    /// The connection dropped; the actor is reconnecting on its own.
    Disconnected,
    /// The first connect failed (bad password, unreachable server). The
    /// session is kept so the failure shows next to the account, and so
    /// "Reconnect" has something to replace.
    Failed(String),
}

/// One signed-in account and the state that is per account: the session
/// (actor + watcher, see `session.rs`), its mailbox tree and its unread
/// counts. Dropping it is a real logout.
pub struct AccountView {
    pub session: AccountSession,
    pub state: ConnState,
    /// The mailbox tree (B8), flattened for the folder pane -- see
    /// `imap::flatten_tree`'s doc for why a flat, owned `Vec` rather than a
    /// real recursive tree widget.
    pub mailbox_rows: Vec<MailboxRow>,
    /// `STATUS (UNSEEN)` per mailbox (B8), refreshed whenever `Mailboxes`
    /// arrives and after a flag/move changes what's unread. A mailbox
    /// missing from this map (rather than present with `0`) means its count
    /// hasn't been fetched yet, not that it's read.
    pub unread_counts: HashMap<String, u32>,
    /// What the session signs in with -- a password, or the account's own
    /// Google token source. Kept so SMTP sends reuse the very same OAuth
    /// source (one cached access token per account, not one per connection).
    pub auth: Auth,
    /// For an account added through the form: the config entry and credential
    /// to save once the connection actually succeeds (not on every click,
    /// and never for a password the server rejected). `None` for an account
    /// that came from the saved list.
    pub pending_persist: Option<(AccountConfig, Auth)>,
}

impl AccountView {
    pub fn id(&self) -> &str {
        self.session.id()
    }

    pub fn label(&self) -> &str {
        self.session.label()
    }

    pub fn total_unread(&self) -> u32 {
        self.mailbox_rows
            .iter()
            .filter_map(|r| r.full_name.as_ref())
            .filter(|name| name.eq_ignore_ascii_case("INBOX"))
            .filter_map(|name| self.unread_counts.get(name))
            .sum()
    }
}
