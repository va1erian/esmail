//! The frontend-independent layer between esMail's mail core and this crate's
//! window: it owns the async runtime and the per-account IMAP sessions, and
//! turns "something arrived" into a plain drain the UI thread runs on demand.
//!
//! It drives the `esmail` library's actors directly (as the egui `main.rs`
//! does, read path only) and names no UI type, so it can be swapped for the
//! shared `AppCore` once that lands.
//!
//! # Threading
//!
//! Sessions run on the runtime's worker threads and push [`AccountEvent`]s into
//! one channel, then call the [`Waker`]. The waker only has to make the UI
//! thread call [`Core::pump`] soon (the window posts itself a message); `pump`
//! never blocks, so nothing here runs on the UI thread except moving events
//! out of the channel.

pub mod account_form;
pub mod account_setup;
mod cache;
mod deliveries;
pub mod images;
pub mod links;
pub mod files;
mod folders;
mod loads;
pub mod mailbox;
pub mod reading;
mod outbox;
pub mod compose;
pub mod resident;
mod results;
mod sending;
mod settings;
mod window_state;

use esmail::auth;
use esmail::compose::{ComposeId, ComposeState};
use esmail::config::{AccountConfig, Config};
use esmail::imap::{ImapCommand, ImapEvent};
use esmail::session::{AccountEvent, AccountSession, DEFAULT_WATCH_MAILBOX, Hooks, NotifyFn, SessionParams};
use esmail::smtp::SmtpEvent;
use esmail::waker::Waker;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

pub use cache::{Cache, CacheEvent};
pub use deliveries::{Deliveries, Delivered, Failure, Retry};
pub use folders::{FolderRef, FolderTree, Node, NodeId};
pub use loads::{BodyLoads, Finished, Latest};
pub use results::SearchResults;
pub use sending::Sender;
pub use settings::{Settings, ThemeChoice};
pub use window_state::WindowState;

/// Set to give an account a password when the OS keyring has none (used to
/// point the app at `mail-mock-server` without touching the real keyring).
const PASSWORD_FALLBACK_VAR: &str = "ESMAIL_PASSWORD";

/// How many events one [`Core::pump`] takes at most, so a burst cannot starve
/// input; the rest is picked up by the next wake.
const PUMP_BATCH: usize = 256;

/// An account that could not be started, with a message fit for a banner.
#[derive(Debug, PartialEq, Eq)]
pub struct StartupIssue {
    /// Index of the account in [`Core::accounts`].
    pub account: usize,
    /// What went wrong.
    pub message: String,
}

/// The runtime and the account sessions.
pub struct Core {
    sessions: Vec<Option<AccountSession>>,
    /// Each account's IMAP credentials, for the SMTP side to reuse (a Google
    /// account sends with the token source its session reads mail with).
    auths: Vec<Option<auth::Auth>>,
    sender: Sender,
    accounts: Vec<AccountConfig>,
    events: mpsc::Receiver<AccountEvent>,
    cache: Cache,
    /// Kept alive for the sessions and never touched again.
    _runtime: Runtime,
    waker: Waker,
}

impl Core {
    /// Starts a session for every account in `config` that has credentials.
    /// Accounts that cannot start are reported, not fatal. `notify` is called (on
    /// the runtime's threads) with each account's new-mail toast text.
    pub fn start(config: &Config, waker: Waker, notify: NotifyFn) -> std::io::Result<(Core, Vec<StartupIssue>)> {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_name("esmail-core").enable_all().build()?;
        let (event_tx, events) = mpsc::channel(256);
        let mut issues = Vec::new();
        let mut sessions = Vec::new();
        let mut auths = Vec::new();
        {
            let _enter = runtime.enter();
            for (index, account) in config.accounts.iter().enumerate() {
                match credentials(config, account) {
                    Ok(auth) => {
                        auths.push(Some(auth.clone()));
                        let params = SessionParams {
                            id: account.id.clone(),
                            label: account.display_name.clone(),
                            host: account.imap_host.clone(),
                            port: account.imap_port,
                            username: account.username.clone(),
                            auth,
                            watch_mailbox: account.watch_mailbox.clone().unwrap_or_else(|| DEFAULT_WATCH_MAILBOX.to_string()),
                        };
                        let hooks = Hooks { notify: notify.clone(), repaint: waker.clone() };
                        sessions.push(Some(AccountSession::spawn(runtime.handle(), params, event_tx.clone(), hooks)));
                    }
                    Err(message) => {
                        issues.push(StartupIssue { account: index, message });
                        auths.push(None);
                        sessions.push(None);
                    }
                }
            }
        }
        let cache = Cache::start(runtime.handle(), config.accounts.iter().map(|a| a.id.clone()).collect(), waker.clone());
        let sender = Sender::start(runtime.handle(), waker.clone());
        let core = Core { sessions, auths, sender, accounts: config.accounts.clone(), events, cache, _runtime: runtime, waker };
        Ok((core, issues))
    }

    /// The configured accounts, in the order every account index refers to.
    pub fn accounts(&self) -> &[AccountConfig] {
        &self.accounts
    }

    /// The local cache: shown before the network answers, kept up to date from
    /// what the network reports, and searched.
    pub fn cache(&self) -> &Cache {
        &self.cache
    }

    /// Queues `state` to be sent from `account` (an index into
    /// [`accounts`](Self::accounts)); the outcome arrives through
    /// [`pump_sent`](Self::pump_sent) under `id`.
    pub fn send_mail(&self, id: ComposeId, account: usize, state: ComposeState) -> Result<(), String> {
        let config = self.accounts.get(account).ok_or("Choose an account to send from.")?;
        let smtp = sending::smtp_account(config, self.auths.get(account).and_then(Option::as_ref))?;
        sending::check_sendable(&smtp, &state)?;
        self.sender.send(id, smtp, state)
    }

    /// Moves the send outcomes that arrived since the last call out of the
    /// channel. Never blocks.
    pub fn pump_sent(&self) -> Vec<SmtpEvent> {
        self.sender.pump()
    }

    /// Queues `command` for `account`'s IMAP actor. Returns `false` when the
    /// account has no session or its queue is full.
    pub fn send(&self, account: usize, command: ImapCommand) -> bool {
        match self.sessions.get(account).and_then(Option::as_ref) {
            Some(session) => session.imap_tx().try_send(command).is_ok(),
            None => false,
        }
    }

    /// Moves the events that arrived since the last call out of the channel.
    /// Never blocks. If the batch limit was hit the waker is called again so
    /// the remainder gets its own turn.
    pub fn pump(&mut self) -> Vec<(usize, ImapEvent)> {
        let mut drained = Vec::new();
        while drained.len() < PUMP_BATCH {
            let Ok((id, event)) = self.events.try_recv() else { return drained };
            if let Some(index) = self.accounts.iter().position(|a| a.id == id) {
                drained.push((index, event));
            }
        }
        (self.waker)();
        drained
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        // Sessions must end before the runtime that runs their tasks does.
        self.sessions.clear();
    }
}

/// The configuration to open: the user's normal `config.toml`, or with a
/// profile the `profiles/<name>/config.toml` next to it. Read-only: nothing is
/// migrated or written back.
pub fn load_config(profile: Option<&str>) -> Result<Config, String> {
    let Some(name) = profile else { return Ok(Config::load()) };
    if name.is_empty() || name.contains(['/', '\\', '.']) {
        return Err(format!("invalid profile name {name:?}"));
    }
    let path = esmail::paths::config_dir().ok_or("no config directory")?.join("profiles").join(name).join(esmail::paths::CONFIG_FILE_NAME);
    let text = std::fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    toml::from_str(&text).map_err(|e| format!("cannot parse {}: {e}", path.display()))
}

/// The saved credentials for `account`, or the fallback variable's password.
fn credentials(config: &Config, account: &AccountConfig) -> Result<auth::Auth, String> {
    auth::saved_auth(config, account).or_else(|missing| match std::env::var(PASSWORD_FALLBACK_VAR) {
        Ok(password) if !password.is_empty() => Ok(auth::Auth::password(password)),
        _ => Err(missing),
    })
}
