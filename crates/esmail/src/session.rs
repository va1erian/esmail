//! One connected account: its own [`ImapActor`] (control session + body
//! worker), its own `IDLE` watch, its own new-mail watermark and poll timer
//! -- everything `main.rs` used to hold exactly one of (issue #35).
//!
//! An [`AccountSession`] owns all of that, and dropping it tears all of it
//! down: the forwarder task is aborted, the [`idle_watch::IdleWatch`] is
//! cancelled, and once the last command sender is gone the actor exits and
//! closes its connections. That drop *is* Logout / Remove account, so no
//! per-account resource can outlive the account (the stale-`IDLE` bug the
//! single global `idle_watch_started` flag used to paper over).
//!
//! **Attribution.** [`ImapEvent`] variants carry a mailbox / uid / request id
//! but no account. Rather than touching every variant, the per-account
//! forwarding task tags whatever it forwards as `(AccountId, ImapEvent)` --
//! the UI drains one merged channel and routes on the id.
//!
//! **Isolation.** Nothing here is shared between accounts: separate
//! channels, separate watermark (a local of the forwarder task), separate
//! reconnect backoff in `imap.rs` and `idle_watch.rs`. A dead server or a
//! wrong password only ever stalls that one account's tasks.

use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::auth::Auth;
use crate::idle_watch;
use crate::imap::{ImapActor, ImapCommand, ImapEvent};
use crate::notify;
use crate::waker::{self, Waker};

/// Stable identifier of an account -- `AccountConfig::id`, the same
/// `username@host` string `db.rs` keys its cache on and the keyring keys
/// passwords on.
pub type AccountId = String;

/// An [`ImapEvent`] tagged with the account whose session produced it.
pub type AccountEvent = (AccountId, ImapEvent);

/// How often each account's forwarder asks its actor to check the watched
/// mailbox for new mail, on top of the `IDLE` pushes. The timer is what keeps
/// working if the server has no `IDLE` or the `IDLE` connection is
/// mid-reconnect.
pub const NEW_MAIL_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// The mailbox watched for new mail unless an account says otherwise: the
/// one mailbox every account has and the one "new mail" conventionally
/// means.
pub const DEFAULT_WATCH_MAILBOX: &str = "INBOX";

/// What an [`AccountSession`] needs to connect and to watch.
pub struct SessionParams {
    pub id: AccountId,
    /// Shown in toasts (`Work: New mail from ...`); usually
    /// `AccountConfig::display_name`.
    pub label: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: Auth,
    /// Mailbox watched for new mail. A per-account setting rather than a
    /// global constant; [`DEFAULT_WATCH_MAILBOX`] to start with.
    pub watch_mailbox: String,
}

/// Shows a new-mail toast: `(account id, title, body)`. The id is what a click
/// on the toast reports back, so it can open that account.
pub type NotifyFn = Arc<dyn Fn(&str, &str, &str) + Send + Sync>;

/// Callbacks into the UI layer. Plain closures so this module needs neither
/// egui nor the Windows toast API, and tests can observe the toasts.
#[derive(Clone)]
pub struct Hooks {
    /// Show a new-mail toast: `(account id, title, body)`. The title already names the
    /// account.
    pub notify: NotifyFn,
    /// Ask the UI to repaint because an event was queued for it.
    pub repaint: Waker,
}

impl Hooks {
    /// Hooks that do nothing, for headless use.
    pub fn none() -> Self {
        Self { notify: Arc::new(|_, _, _| {}), repaint: waker::noop() }
    }
}

/// A live account connection. See the module doc; dropping it is a real
/// logout.
pub struct AccountSession {
    id: AccountId,
    label: String,
    imap_tx: mpsc::Sender<ImapCommand>,
    forwarder: JoinHandle<()>,
}

impl AccountSession {
    /// Spawn the actor and the forwarder (which starts the `IDLE` watch once the
    /// account has connected), and start
    /// connecting. Events come out on `ui_events`, tagged with this
    /// account's id; the connection outcome arrives there as
    /// [`ImapEvent::Connected`] or [`ImapEvent::Error`].
    pub fn spawn(params: SessionParams, ui_events: mpsc::Sender<AccountEvent>, hooks: Hooks) -> Self {
        let SessionParams { id, label, host, port, username, auth, watch_mailbox } = params;

        let (imap_tx, imap_rx) = mpsc::channel(32);
        let (actor_tx, actor_rx) = mpsc::channel(32);
        ImapActor::spawn(imap_rx, actor_tx);

        let idle = IdleParams { host: host.clone(), port, username: username.clone(), auth: auth.clone() };
        let forwarder = tokio::spawn(forward_and_watch(
            id.clone(),
            label.clone(),
            watch_mailbox,
            actor_rx,
            ui_events,
            imap_tx.clone(),
            idle,
            hooks,
        ));

        // Queued before anyone else can send: the actor's first command is
        // always the connect. `try_send` cannot fail on a fresh channel.
        let _ = imap_tx.try_send(ImapCommand::Connect { host, port, username, auth });

        Self { id, label, imap_tx, forwarder }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// Commands for this account's actor.
    pub fn imap_tx(&self) -> &mpsc::Sender<ImapCommand> {
        &self.imap_tx
    }
}

impl Drop for AccountSession {
    fn drop(&mut self) {
        // The forwarder holds a clone of `imap_tx`; aborting it releases
        // that clone, and this struct's own sender goes right after, which
        // closes the actor's command channel and ends it. The `IDLE` watch is
        // owned by the forwarder, so aborting it cancels that too.
        self.forwarder.abort();
    }
}

/// The per-account forwarder: passes every actor event to the UI tagged with
/// `account`, and additionally
///
/// - tracks whether the account is currently connected,
/// - polls `watch_mailbox` on [`NEW_MAIL_POLL_INTERVAL`] while connected and
///   immediately on every `IDLE` push,
/// - folds each [`ImapEvent::MailboxPolled`] into a UID watermark
///   (`notify::update_watermark`) and, on new mail, fetches the envelopes
///   that describe it,
/// - turns the resulting [`ImapEvent::NewHeaders`] into a toast that names
///   the account.
///
/// The watermark is a local of this task, so no other code can read or reset
/// it and it is per account by construction. It is deliberately not
/// `db.rs`'s sync state -- see `notify.rs`'s module doc.
#[allow(clippy::too_many_arguments)]
async fn forward_and_watch(
    account: AccountId,
    label: String,
    watch_mailbox: String,
    mut actor_events: mpsc::Receiver<ImapEvent>,
    ui_events: mpsc::Sender<AccountEvent>,
    imap_tx: mpsc::Sender<ImapCommand>,
    idle_params: IdleParams,
    hooks: Hooks,
) {
    // A small buffer is enough: this only ever carries a "go check" signal,
    // never data, and a missed send just means the next timer tick catches it
    // instead.
    let (wake_tx, mut idle_wake) = mpsc::channel(4);
    let mut idle_start = Some((idle_params, wake_tx));
    // Owned here so it lives exactly as long as this task: aborting the
    // forwarder (a logout) drops it, which cancels the watch.
    let mut _idle: Option<idle_watch::IdleWatch> = None;
    let mut connected = false;
    let mut watermark: Option<notify::MailWatermark> = None;
    let mut poll_interval = tokio::time::interval(NEW_MAIL_POLL_INTERVAL);
    poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Disabled once `idle_wake` closes so a channel that keeps returning
    // `None` cannot busy-loop this `select!`; the timer-only path keeps
    // working in that case.
    let mut idle_wake_open = true;

    loop {
        tokio::select! {
            evt = actor_events.recv() => {
                let Some(evt) = evt else { break };
                match &evt {
                    ImapEvent::Connected => {
                        connected = true;
                        // Started on the first successful connect, once: an
                        // account that never got in (a wrong password, an
                        // unreachable server) must not have a background watch
                        // retrying those credentials every few seconds for as
                        // long as it stays in the list.
                        if let Some((p, wake_tx)) = idle_start.take() {
                            _idle = Some(idle_watch::spawn(p.host, p.port, p.username, p.auth, watch_mailbox.clone(), wake_tx));
                        }
                        // A fresh connection starts a new baseline -- see
                        // `notify::update_watermark`: the first observation
                        // after one must never itself count as new mail.
                        watermark = None;
                    }
                    ImapEvent::Disconnected => connected = false,
                    ImapEvent::MailboxPolled { mailbox, state } if *mailbox == watch_mailbox => {
                        let (next, update) = notify::update_watermark(
                            watermark,
                            notify::MailWatermark {
                                uid_validity: state.uid_validity,
                                uid_next: state.uid_next,
                            },
                        );
                        watermark = Some(next);
                        if let notify::WatermarkUpdate::NewMail { first_new_uid, .. } = update {
                            let _ = imap_tx.try_send(ImapCommand::FetchNewHeaders {
                                mailbox: watch_mailbox.clone(),
                                first_uid: first_new_uid,
                            });
                        }
                    }
                    ImapEvent::NewHeaders { mailbox, headers } if *mailbox == watch_mailbox => {
                        if let Some((title, body)) = notify::build_account_notification(&label, headers) {
                            (hooks.notify)(&account, &title, &body);
                        }
                    }
                    _ => {}
                }
                if ui_events.send((account.clone(), evt)).await.is_err() {
                    break; // The UI is gone; nothing left to forward to.
                }
                (hooks.repaint)();
            }
            _ = poll_interval.tick(), if connected => {
                let _ = imap_tx.try_send(ImapCommand::PollMailbox { mailbox: watch_mailbox.clone() });
            }
            woke = idle_wake.recv(), if idle_wake_open => {
                match woke {
                    Some(idle_watch::MailboxChanged) if connected => {
                        let _ = imap_tx.try_send(ImapCommand::PollMailbox { mailbox: watch_mailbox.clone() });
                    }
                    // A push while the actor's own session is down: nothing
                    // to poll with; the timer (once connected again) or the
                    // next push will catch it.
                    Some(idle_watch::MailboxChanged) => {}
                    None => idle_wake_open = false,
                }
            }
        }
    }
}

/// Where the forwarder opens the `IDLE` watch once the account has connected.
struct IdleParams {
    host: String,
    port: u16,
    username: String,
    auth: Auth,
}
