//! Keeping watch over a set of accounts with no UI at all: the core of the
//! background listener (see `docs/BACKGROUND-LISTENER.md`).
//!
//! A [`Watcher`] owns one [`AccountSession`] per account -- so each account
//! gets its own connection, `IDLE` watch, new-mail watermark and toast, exactly
//! as in the GUI -- and drains the events those sessions produce. What it keeps
//! from them is deliberately small: whether each account is connected, and how
//! many unread messages its watched mailbox holds. Nothing here knows about
//! egui, the tray or the toast API; the caller passes a [`NotifyFn`] for toasts
//! and decides what to do with each [`Change`].
//!
//! **The caller must keep calling [`Watcher::next_change`].** The sessions'
//! forwarder tasks block when their event channel is full, so a watcher nobody
//! polls stops noticing new mail.

use std::collections::BTreeMap;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::auth::{self, Auth};
use crate::config::{AccountConfig, Config};
use crate::imap::{ImapCommand, ImapEvent};
use crate::session::{AccountEvent, AccountId, AccountSession, DEFAULT_WATCH_MAILBOX, Hooks, NotifyFn, SessionParams};

/// Where an account's connection stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountState {
    /// Connecting, or reconnecting after the connection dropped.
    Connecting,
    /// Connected, but the first poll of the watched mailbox has not happened
    /// yet. That poll only sets the baseline, so mail arriving before it is
    /// not announced (see `notify::update_watermark`).
    Connected,
    /// Connected and baselined: mail arriving from now on is announced.
    Watching,
    /// The connection could not be made (bad password, unreachable server, ...).
    /// The session keeps retrying on its own; this says why it is not up yet.
    Failed(String),
}

impl AccountState {
    /// Whether the connection is up (as opposed to connecting or failed).
    pub fn is_up(&self) -> bool {
        matches!(self, Self::Connected | Self::Watching)
    }
}

/// Something worth telling the caller about, returned by [`Watcher::next_change`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    State { account: AccountId, state: AccountState },
    /// The watched mailbox's unread count changed. `total` is the sum over all
    /// accounts, which is what the tray tooltip shows.
    Unread { account: AccountId, unread: u32, total: u32 },
}

struct Watched {
    config: AccountConfig,
    watch_mailbox: String,
    session: AccountSession,
    state: AccountState,
    unread: u32,
}

/// See the module doc.
pub struct Watcher {
    accounts: BTreeMap<AccountId, Watched>,
    events_tx: mpsc::Sender<AccountEvent>,
    events_rx: mpsc::Receiver<AccountEvent>,
    hooks: Hooks,
}

impl Watcher {
    /// A watcher with no accounts yet. `notify` is called with
    /// `(account id, title, body)` for each batch of new mail.
    pub fn new(notify: NotifyFn) -> Self {
        let (events_tx, events_rx) = mpsc::channel(64);
        Self { accounts: BTreeMap::new(), events_tx, events_rx, hooks: Hooks { notify, repaint: Arc::new(|| {}) } }
    }

    /// Make the watched accounts exactly `desired`: sessions for accounts that
    /// are gone are dropped (a real logout), new ones are started, and one
    /// whose [`AccountConfig`] changed is restarted. An account whose config is
    /// unchanged keeps its session -- including when only its credential
    /// changed; use [`Watcher::restart_account`] for that.
    pub fn apply_accounts(&mut self, desired: Vec<(AccountConfig, Auth)>) {
        let wanted: Vec<&str> = desired.iter().map(|(config, _)| config.id.as_str()).collect();
        self.accounts.retain(|id, _| wanted.contains(&id.as_str()));
        for (config, auth) in desired {
            let unchanged = self.accounts.get(&config.id).is_some_and(|w| w.config == config);
            if !unchanged {
                self.restart_account(config, auth);
            }
        }
    }

    /// Start (or replace) the session for `config`. Replacing drops the old
    /// session first, which stops its connections and watch.
    pub fn restart_account(&mut self, config: AccountConfig, auth: Auth) {
        self.accounts.remove(&config.id);
        let watch_mailbox = config.watch_mailbox.clone().unwrap_or_else(|| DEFAULT_WATCH_MAILBOX.to_string());
        let session = AccountSession::spawn(
            SessionParams {
                id: config.id.clone(),
                label: config.display_name.clone(),
                host: config.imap_host.clone(),
                port: config.imap_port,
                username: config.username.clone(),
                auth,
                watch_mailbox: watch_mailbox.clone(),
            },
            self.events_tx.clone(),
            self.hooks.clone(),
        );
        self.accounts.insert(
            config.id.clone(),
            Watched { config, watch_mailbox, session, state: AccountState::Connecting, unread: 0 },
        );
    }

    /// Ids of the accounts being watched, in order.
    pub fn account_ids(&self) -> Vec<&str> {
        self.accounts.keys().map(String::as_str).collect()
    }

    pub fn state(&self, account: &str) -> Option<&AccountState> {
        self.accounts.get(account).map(|w| &w.state)
    }

    /// Unread messages in the watched mailboxes, summed over all accounts.
    pub fn total_unread(&self) -> u32 {
        self.accounts.values().map(|w| w.unread).sum()
    }

    /// Wait for the next [`Change`], handling whatever the sessions report in
    /// the meantime. Never returns if there are no accounts (the watcher itself
    /// holds a sender, so the channel cannot close), so `select!` it with the
    /// caller's other event sources.
    pub async fn next_change(&mut self) -> Change {
        loop {
            let Some((account, event)) = self.events_rx.recv().await else {
                // Unreachable while `self.events_tx` exists; wait forever
                // rather than spin if that ever changes.
                std::future::pending::<()>().await;
                continue;
            };
            if let Some(change) = self.handle(account, event) {
                return change;
            }
        }
    }

    fn handle(&mut self, account: AccountId, event: ImapEvent) -> Option<Change> {
        // An event from a session that has since been replaced or removed.
        let watched = self.accounts.get_mut(&account)?;
        match event {
            ImapEvent::Connected => {
                watched.state = AccountState::Connected;
                Self::request_unread(watched);
                Some(Change::State { account, state: AccountState::Connected })
            }
            ImapEvent::Disconnected => {
                watched.state = AccountState::Connecting;
                Some(Change::State { account, state: AccountState::Connecting })
            }
            // An error while connected is a failed request, not a lost
            // connection; only one before the first connection says why the
            // account is not up.
            ImapEvent::Error(reason) if !watched.state.is_up() => {
                let state = AccountState::Failed(reason);
                watched.state = state.clone();
                Some(Change::State { account, state })
            }
            // The session forwarder polls the watched mailbox on every `IDLE`
            // push and on a timer, and toasts any new mail; the unread count
            // is refreshed on the same rhythm.
            ImapEvent::MailboxPolled { mailbox, .. } if mailbox == watched.watch_mailbox => {
                Self::request_unread(watched);
                if watched.state == AccountState::Connected {
                    watched.state = AccountState::Watching;
                    return Some(Change::State { account, state: AccountState::Watching });
                }
                None
            }
            ImapEvent::UnreadCounts(counts) => {
                let unread = *counts.get(&watched.watch_mailbox)?;
                if unread == watched.unread {
                    return None;
                }
                watched.unread = unread;
                let total = self.total_unread();
                Some(Change::Unread { account, unread, total })
            }
            _ => None,
        }
    }

    fn request_unread(watched: &Watched) {
        // Best effort: if the actor's queue is full the next poll asks again.
        let _ = watched
            .session
            .imap_tx()
            .try_send(ImapCommand::FetchUnreadCounts { mailboxes: vec![watched.watch_mailbox.clone()] });
    }
}

/// The accounts in `config` paired with their saved credentials, ready for
/// [`Watcher::apply_accounts`], and a `(display name, reason)` for each account
/// whose credential could not be read from the keyring.
pub fn load_accounts(config: &Config) -> (Vec<(AccountConfig, Auth)>, Vec<(String, String)>) {
    let mut accounts = Vec::new();
    let mut problems = Vec::new();
    for account in &config.accounts {
        match auth::saved_auth(config, account) {
            Ok(auth) => accounts.push((account.clone(), auth)),
            Err(reason) => problems.push((account.display_name.clone(), reason)),
        }
    }
    (accounts, problems)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn silent() -> NotifyFn {
        Arc::new(|_, _, _| {})
    }

    /// An account on a port nothing listens on: it never connects, which is
    /// all the bookkeeping tests need.
    fn dead_account(id: &str) -> (AccountConfig, Auth) {
        let mut config = AccountConfig::new(id.to_string(), "localhost".to_string(), 1, id.to_string());
        config.id = id.to_string();
        (config, Auth::password("pw".to_string()))
    }

    #[tokio::test]
    async fn apply_accounts_starts_and_stops_sessions() {
        let mut watcher = Watcher::new(silent());
        assert!(watcher.account_ids().is_empty());

        watcher.apply_accounts(vec![dead_account("a"), dead_account("b")]);
        assert_eq!(watcher.account_ids(), ["a", "b"]);

        watcher.apply_accounts(vec![dead_account("b"), dead_account("c")]);
        assert_eq!(watcher.account_ids(), ["b", "c"], "a is dropped, c is added, b is kept");

        watcher.apply_accounts(Vec::new());
        assert!(watcher.account_ids().is_empty());
    }

    #[tokio::test]
    async fn an_unchanged_account_keeps_its_session_and_a_changed_one_is_replaced() {
        let mut watcher = Watcher::new(silent());
        watcher.apply_accounts(vec![dead_account("a")]);
        // Pretend the session had got somewhere: a restart would reset it.
        watcher.accounts.get_mut("a").unwrap().state = AccountState::Connected;

        watcher.apply_accounts(vec![dead_account("a")]);
        assert_eq!(watcher.state("a"), Some(&AccountState::Connected), "same config: session kept");

        let (mut changed, auth) = dead_account("a");
        changed.imap_port = 2;
        watcher.apply_accounts(vec![(changed, auth)]);
        assert_eq!(watcher.state("a"), Some(&AccountState::Connecting), "new config: session restarted");
    }

    #[tokio::test]
    async fn an_unreachable_server_is_reported_as_failed() {
        let mut watcher = Watcher::new(silent());
        watcher.apply_accounts(vec![dead_account("a")]);
        let change = tokio::time::timeout(Duration::from_secs(30), watcher.next_change())
            .await
            .expect("a connection failure is reported");
        match change {
            Change::State { account, state: AccountState::Failed(reason) } => {
                assert_eq!(account, "a");
                assert!(!reason.is_empty());
            }
            other => panic!("expected a failure, got {other:?}"),
        }
        assert!(matches!(watcher.state("a"), Some(AccountState::Failed(_))));
    }

    #[tokio::test]
    async fn events_of_a_removed_account_are_ignored() {
        let mut watcher = Watcher::new(silent());
        watcher.apply_accounts(vec![dead_account("a")]);
        watcher.apply_accounts(Vec::new());
        assert_eq!(watcher.handle("a".to_string(), ImapEvent::Connected), None);
        assert_eq!(watcher.total_unread(), 0);
    }

    #[test]
    fn load_accounts_reports_credentials_it_cannot_read() {
        let mut config = Config::default();
        // A password account whose keyring entry does not exist.
        config.accounts.push(AccountConfig::new(
            "No Secret".to_string(),
            "watcher-test-no-such-host.invalid".to_string(),
            993,
            "nobody-watcher-test".to_string(),
        ));
        let (accounts, problems) = load_accounts(&config);
        assert!(accounts.is_empty());
        assert_eq!(problems, [("No Secret".to_string(), "no saved password".to_string())]);
    }
}
