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

mod folders;
mod loads;
pub mod mailbox;
pub mod reading;

use esmail::auth;
use esmail::config::{AccountConfig, Config};
use esmail::imap::{ImapCommand, ImapEvent};
use esmail::session::{AccountEvent, AccountSession, DEFAULT_WATCH_MAILBOX, Hooks, SessionParams};
use esmail::waker::Waker;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

pub use folders::{FolderRef, FolderTree, Node, NodeId};
pub use loads::{BodyLoads, Finished, Latest};

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
    accounts: Vec<AccountConfig>,
    events: mpsc::Receiver<AccountEvent>,
    /// Kept alive for the sessions and never touched again.
    _runtime: Runtime,
    waker: Waker,
}

impl Core {
    /// Starts a session for every account in `config` that has credentials.
    /// Accounts that cannot start are reported, not fatal.
    pub fn start(config: &Config, waker: Waker) -> std::io::Result<(Core, Vec<StartupIssue>)> {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_name("esmail-core").enable_all().build()?;
        let (event_tx, events) = mpsc::channel(256);
        let mut issues = Vec::new();
        let mut sessions = Vec::new();
        {
            let _enter = runtime.enter();
            for (index, account) in config.accounts.iter().enumerate() {
                match credentials(config, account) {
                    Ok(auth) => {
                        let params = SessionParams {
                            id: account.id.clone(),
                            label: account.display_name.clone(),
                            host: account.imap_host.clone(),
                            port: account.imap_port,
                            username: account.username.clone(),
                            auth,
                            watch_mailbox: account.watch_mailbox.clone().unwrap_or_else(|| DEFAULT_WATCH_MAILBOX.to_string()),
                        };
                        let hooks = Hooks { notify: std::sync::Arc::new(|_, _, _| {}), repaint: waker.clone() };
                        sessions.push(Some(AccountSession::spawn(params, event_tx.clone(), hooks)));
                    }
                    Err(message) => {
                        issues.push(StartupIssue { account: index, message });
                        sessions.push(None);
                    }
                }
            }
        }
        let core = Core { sessions, accounts: config.accounts.clone(), events, _runtime: runtime, waker };
        Ok((core, issues))
    }

    /// The configured accounts, in the order every account index refers to.
    pub fn accounts(&self) -> &[AccountConfig] {
        &self.accounts
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
