//! The frontend-agnostic application core (issue #108): the state behind the
//! folder pane, message list and reading pane, and the logic that keeps it in
//! step with what the accounts' IMAP sessions report.
//!
//! An [`AppCore`] knows no UI type. It is built with a [`Waker`] (how a
//! session tells the frontend there is something to pump) and a tokio
//! [`Handle`] (where actors run, so a frontend need not own the runtime). The
//! frontend calls [`AppCore::pump`] when woken, redraws from the public state,
//! and applies the returned [`Changes`].

mod progress;
mod pump;
mod state;

use std::collections::BTreeSet;
use std::time::Instant;

use tokio::runtime::Handle;
use tokio::sync::mpsc;

use crate::auth::Auth;
use crate::config::AccountConfig;
use crate::db::DbCommand;
use crate::imap::{ImapCommand, MailHeader};
use crate::progress::ProgressKind;
use crate::render::Attachment;
use crate::session::{
    AccountEvent, AccountId, AccountSession, DEFAULT_WATCH_MAILBOX, Hooks, NotifyFn, SessionParams,
};
use crate::waker::Waker;

pub use state::{AccountView, Banner, BulkAction, ConnState, ProgressView};

/// What the frontend has to do after the core changed, beyond redrawing from
/// the state it reads.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Changes {
    /// HTML the reading pane must show now (the last one wins).
    pub reading_pane: Option<String>,
    /// Accounts added through the Add account form that have just connected
    /// for the first time: the frontend saves each (see
    /// [`AppCore::take_pending_persist`]), tells the listener and closes the
    /// form.
    pub persist_accounts: Vec<AccountId>,
}

/// See the module doc.
pub struct AppCore {
    runtime: Handle,
    /// Handed (cloned) to each new [`AccountSession`], whose forwarder tags
    /// what it forwards with the account id.
    events_tx: mpsc::Sender<AccountEvent>,
    events_rx: mpsc::Receiver<AccountEvent>,
    hooks: Hooks,
    changes: Changes,

    /// Where cache bookkeeping is sent.
    pub db_tx: mpsc::Sender<DbCommand>,
    /// Every connected (or connecting) account, in the order they were
    /// added. Each holds its own `ImapActor`, `IDLE` watch and new-mail
    /// watermark (see `session.rs`); removing one drops all of them.
    pub accounts: Vec<AccountView>,
    /// The account the message list and reading pane show. `None` until the
    /// first account has connected, and again once the last one is gone.
    pub active: Option<AccountId>,
    /// Transient, non-error status text ("Connecting...", "Page 3 of 9").
    pub status: String,
    /// Active error banners (B9), newest last. See [`Banner`].
    pub banners: Vec<Banner>,
    /// Monotonic source for `Banner::id`, so a dismiss click can target the
    /// exact banner clicked even if another one is added/removed the same
    /// frame -- mirrors `next_req_id`'s reasoning.
    next_banner_id: u64,

    /// The mailbox open in the message list, within the `active` account.
    pub selected_mailbox: String,
    pub headers: Vec<MailHeader>,
    pub selected_uid: Option<u32>,
    /// Multi-select (B8): every UID selected via shift/ctrl-click, in
    /// addition to `selected_uid` (the one whose body is actually shown --
    /// always the most recently *plain*-clicked message, or the sole member
    /// of a multi-selection made by ctrl/shift-clicking from scratch).
    /// Bulk actions (archive/delete/mark read or unread) act on this set
    /// when it's non-empty, falling back to `selected_uid` alone otherwise.
    pub selected_uids: BTreeSet<u32>,
    /// Anchor for shift-click range selection: the last *plain* (no
    /// modifier) click, or the single UID a ctrl-click started a fresh
    /// selection from.
    pub select_anchor: Option<u32>,
    /// Set when a message is opened, cleared once its `\Seen` flag has been
    /// sent (or the user navigates away first) -- B8's "mark as read with a
    /// delay" so briefly passing over a message in the list doesn't mark it
    /// read.
    pub pending_mark_seen: Option<(u32, Instant)>,
    pub current_page: u32,
    pub total_pages: u32,
    /// Attachments for the currently-open message (B6), if fetched directly
    /// from IMAP. Cleared whenever a different message is opened. A message
    /// opened from a cached search result never populates this -- the cache
    /// only stores rendered HTML, not the raw bytes attachments come from;
    /// see PLAN.md §B6.
    pub current_attachments: Vec<Attachment>,
    /// The currently-open message's rendered HTML, kept only so
    /// Reply/Reply All/Forward (B7) can quote it -- see `compose.rs`. Empty
    /// when no message is loaded.
    pub current_message_html: String,

    /// Monotonic source for `ImapCommand::FetchHeaders`/`FetchBody` request
    /// ids. Only the reply matching `current_headers_req`/`current_body_req`
    /// is applied; an older one arriving late (e.g. a slow page-2 fetch
    /// answered after the user already moved to page 3) is dropped instead of
    /// clobbering newer state.
    next_req_id: u64,
    pub current_headers_req: u64,
    pub current_body_req: u64,

    pub search_query: String,
    pub search_results: Option<Vec<MailHeader>>,
    /// Where each entry of `search_results` lives (account, mailbox), index
    /// for index. A search can span accounts, and a UID means nothing without
    /// them. Empty exactly when `search_results` is `None`.
    pub search_origins: Vec<(AccountId, String)>,
    /// Search every account instead of only the active one.
    pub search_all_accounts: bool,
    /// Opening a search hit from another mailbox moved `active` /
    /// `selected_mailbox` there without reloading `headers`; when the search
    /// is cleared the message list has to be fetched again.
    pub headers_stale: bool,
    /// The one operation the bottom status bar is showing, if any. See
    /// [`ProgressView`].
    pub progress: Option<ProgressView>,
    /// The bulk flag/move action in flight, if any. See [`BulkAction`].
    pub bulk_action: Option<BulkAction>,
}

impl AppCore {
    /// A core with no accounts. Sessions run on `runtime` and call `waker`
    /// whenever they queue an event for [`Self::pump`]; `notify` shows a
    /// new-mail toast; `db_tx` is where cache bookkeeping is sent.
    pub fn new(runtime: Handle, waker: Waker, notify: NotifyFn, db_tx: mpsc::Sender<DbCommand>) -> Self {
        // Every account's events arrive on this one channel, tagged with the
        // account id by that account's `AccountSession` forwarder.
        let (events_tx, events_rx) = mpsc::channel(64);
        Self {
            runtime,
            events_tx,
            events_rx,
            hooks: Hooks { notify, repaint: waker },
            db_tx,
            changes: Changes::default(),
            accounts: Vec::new(),
            active: None,
            status: "Ready".to_string(),
            banners: Vec::new(),
            next_banner_id: 0,
            selected_mailbox: "INBOX".to_string(),
            headers: Vec::new(),
            selected_uid: None,
            selected_uids: BTreeSet::new(),
            select_anchor: None,
            pending_mark_seen: None,
            current_page: 1,
            total_pages: 1,
            current_attachments: Vec::new(),
            current_message_html: String::new(),
            next_req_id: 0,
            current_headers_req: 0,
            current_body_req: 0,
            search_query: String::new(),
            search_results: None,
            search_origins: Vec::new(),
            search_all_accounts: false,
            headers_stale: false,
            progress: None,
            bulk_action: None,
        }
    }

    /// The runtime the core's actors run on, for a frontend that needs to
    /// spawn its own tasks alongside them.
    pub fn runtime(&self) -> &Handle {
        &self.runtime
    }

    /// What changed since the last call, for the frontend to apply. Also
    /// returned by [`Self::pump`]; call this after an action that may have
    /// changed something outside a pump (`activate`, `disconnect_account`).
    pub fn take_changes(&mut self) -> Changes {
        std::mem::take(&mut self.changes)
    }

    fn show_in_reading_pane(&mut self, html: String) {
        self.changes.reading_pane = Some(html);
    }

    /// Start a session for `account` with `auth`, replacing one already open
    /// for the same account: the old session is dropped first, which really
    /// stops its connections and watcher. `pending_persist` is `Some` for an
    /// account added through the form, which is only saved once its
    /// connection succeeds.
    ///
    /// The session's new-mail watch (B10) runs as a plain tokio task, not
    /// anything driven by the frontend's frame loop, so it keeps running -- and
    /// can keep showing toasts -- for as long as the process is alive,
    /// independent of whether the main window is visible.
    pub fn open_session(&mut self, account: &AccountConfig, auth: Auth, pending_persist: Option<(AccountConfig, Auth)>) {
        self.accounts.retain(|v| v.id() != account.id);
        let session = AccountSession::spawn(
            &self.runtime,
            SessionParams {
                id: account.id.clone(),
                label: account.display_name.clone(),
                host: account.imap_host.clone(),
                port: account.imap_port,
                username: account.username.clone(),
                auth: auth.clone(),
                watch_mailbox: account.watch_mailbox.clone().unwrap_or_else(|| DEFAULT_WATCH_MAILBOX.to_string()),
            },
            self.events_tx.clone(),
            self.hooks.clone(),
        );
        self.accounts.push(AccountView {
            session,
            state: ConnState::Connecting,
            mailbox_rows: Vec::new(),
            unread_counts: std::collections::HashMap::new(),
            auth,
            pending_persist,
        });
    }

    /// The account and credential an account added through the form should be
    /// saved with, once (`None` afterwards, and for a saved account).
    pub fn take_pending_persist(&mut self, account: &str) -> Option<(AccountConfig, Auth)> {
        self.view_mut(account)?.pending_persist.take()
    }

    /// Add a new error banner (B9). Callers pass a complete, already-worded
    /// message; this just assigns it an id and appends it -- dismissal is a
    /// separate frontend action that removes it by id.
    pub fn push_banner(&mut self, message: String) {
        self.next_banner_id += 1;
        self.banners.push(Banner { id: self.next_banner_id, message });
    }

    /// [`Self::push_banner`] for an error that belongs to one account. Names
    /// the account once there is more than one, so "IMAP error: login
    /// failed" says which of them; with a single account the text is what it
    /// always was.
    pub fn push_account_banner(&mut self, account: &str, message: String) {
        if self.accounts.len() > 1 {
            let label = self.account_label(account);
            self.push_banner(format!("{label}: {message}"));
        } else {
            self.push_banner(message);
        }
    }

    pub fn view(&self, account: &str) -> Option<&AccountView> {
        self.accounts.iter().find(|v| v.id() == account)
    }

    pub fn view_mut(&mut self, account: &str) -> Option<&mut AccountView> {
        self.accounts.iter_mut().find(|v| v.id() == account)
    }

    /// The account's display name, or its id if it is not (or no longer) a
    /// live session.
    pub fn account_label(&self, account: &str) -> String {
        self.view(account).map_or_else(|| account.to_string(), |v| v.label().to_string())
    }

    /// Every selectable mailbox of `account` (the ones `FetchUnreadCounts`
    /// can `STATUS`), from its current mailbox tree.
    pub fn mailbox_names(&self, account: &str) -> Vec<String> {
        self.view(account)
            .map(|v| v.mailbox_rows.iter().filter_map(|r| r.full_name.clone()).collect())
            .unwrap_or_default()
    }

    /// Send a command to one account's actor. Dropped if that account has
    /// been removed -- there is nothing left to answer it.
    pub fn send_imap_to(&self, account: &str, cmd: ImapCommand) {
        if let Some(view) = self.view(account) {
            let _ = view.session.imap_tx().try_send(cmd);
        }
    }

    /// Send a command to the active account's actor (a no-op with no active
    /// account). Everything the message list and reading pane do goes
    /// through here, since they only ever show the active account.
    pub fn send_imap(&self, cmd: ImapCommand) {
        if let Some(account) = &self.active {
            self.send_imap_to(account, cmd);
        }
    }

    /// A fresh request id for `FetchHeaders`/`FetchBody`, mechanically
    /// distinct from the last one handed out.
    pub fn next_req_id(&mut self) -> u64 {
        self.next_req_id += 1;
        self.next_req_id
    }

    /// Send `FetchHeaders`, recording its request id as the only one whose
    /// reply [`Self::pump`] will still accept.
    pub fn fetch_headers(&mut self, mailbox: String, page: u32) {
        let req_id = self.next_req_id();
        self.current_headers_req = req_id;
        self.send_imap(ImapCommand::FetchHeaders { mailbox, page, req_id });
    }

    /// Send `FetchBody`, recording its request id the same way `fetch_headers` does.
    pub fn fetch_body(&mut self, mailbox: String, uid: u32) {
        let req_id = self.next_req_id();
        self.current_body_req = req_id;
        self.send_imap(ImapCommand::FetchBody { mailbox, uid, req_id });
    }

    /// Make `account` the one the message list and reading pane show, open
    /// `mailbox` in it, and start fetching its first page. Everything that
    /// belonged to the previous account's list -- selection, search results,
    /// the open message -- is dropped, since none of it is meaningful in the
    /// new one.
    pub fn activate(&mut self, account: &str, mailbox: String) {
        let switching = self.active.as_deref() != Some(account);
        self.active = Some(account.to_string());
        self.headers_stale = false;
        if switching {
            self.search_query.clear();
            self.search_results = None;
            self.search_origins.clear();
            self.headers.clear();
            self.show_in_reading_pane(String::new());
            self.current_message_html.clear();
            self.current_attachments.clear();
        }
        self.selected_mailbox = mailbox.clone();
        self.selected_uid = None;
        self.selected_uids.clear();
        self.select_anchor = None;
        self.pending_mark_seen = None;
        self.current_page = 1;
        self.total_pages = 1;
        self.fetch_headers(mailbox, 1);
    }

    /// Real Logout / Remove account: drop the account's session, which stops
    /// its actor, its body worker and its `IDLE` watch (see
    /// `AccountSession`'s `Drop`), and leaves every other account alone. If
    /// it was the active one, the next remaining connected account takes
    /// over.
    pub fn disconnect_account(&mut self, account: &str) {
        let label = self.account_label(account);
        self.accounts.retain(|v| v.id() != account);
        // "All accounts" only exists as a choice with more than one.
        if self.accounts.len() < 2 {
            self.search_all_accounts = false;
        }
        if self.active.as_deref() == Some(account) {
            self.active = None;
            // A bulk action's replies are dropped once its account is gone
            // (see `pump`'s removal guard), so without this an in-flight one
            // would leave the status bar and the disabled buttons stuck
            // forever.
            self.bulk_action = None;
            self.clear_progress(ProgressKind::Flags);
            self.clear_progress(ProgressKind::Move);
            self.clear_progress(ProgressKind::Index);
            self.headers.clear();
            self.search_results = None;
            self.search_origins.clear();
            self.headers_stale = false;
            self.selected_uid = None;
            self.selected_uids.clear();
            self.select_anchor = None;
            self.pending_mark_seen = None;
            self.current_message_html.clear();
            self.current_attachments.clear();
            self.show_in_reading_pane(String::new());
            let next = self
                .accounts
                .iter()
                .find(|v| v.state == ConnState::Connected)
                .map(|v| v.id().to_string());
            if let Some(next) = next {
                self.activate(&next, "INBOX".to_string());
            }
        }
        self.status = format!("Logged out of {label}");
    }
}
