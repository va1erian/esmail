//! IMAP `IDLE` (RFC 2177) push watch for new mail -- a dedicated connection
//! that sends `MailboxChanged` the moment the server pushes anything during
//! an idle wait, instead of `main.rs`'s `spawn_new_mail_watch` having to
//! poll on a fixed timer to find out. See PLAN.md's "IMAP push (IDLE)"
//! section for the design rationale and what B10's polling still covers.
//!
//! **Why a separate connection from `ImapActor`'s.** `async_imap::Handle`'s
//! own doc comment says it plainly: "As long as a `Handle` is active, the
//! mailbox cannot be otherwise accessed." Sharing one session between IDLE
//! and header/body fetches would mean every fetch has to interrupt IDLE
//! (send `DONE`, do the fetch, re-issue `IDLE`) around itself -- exactly the
//! control+worker session split PLAN.md §B2 called for and deferred for lack
//! of anything to verify it against. `mail-mock-server` (added since) makes
//! that verifiable now, but the *full* pool rework is still separate,
//! larger, real follow-on work; this module sidesteps needing it by keeping
//! IDLE on its own always-open connection that does nothing but idle,
//! independent of `ImapActor`'s session.
//!
//! **What a push means.** IDLE reports "something changed" without saying
//! what -- new mail, an expunge, a flag change all look the same from here.
//! This module makes no attempt to tell them apart: `MailboxChanged` just
//! means "go re-`EXAMINE`", exactly what B10's `ImapCommand::PollMailbox`
//! already does. `main.rs` wires a `MailboxChanged` the same way it wires a
//! poll-timer tick: send `PollMailbox`, let the existing watermark logic in
//! `notify.rs` decide whether it was actually new mail.

use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::auth::Auth;

/// How long to hold one `IDLE` before re-issuing it. RFC 2177 recommends
/// terminating and restarting at least every 29 minutes, since a server may
/// treat a longer-idle client as dead and drop the connection.
const IDLE_ROUND_TRIP: Duration = Duration::from_secs(29 * 60);
/// Cap on reconnect backoff, same value `ImapActor::ensure_connected` uses
/// (see `imap.rs::MAX_RECONNECT_DELAY`) -- kept independent rather than
/// shared, since this is a wholly separate connection with its own lifetime.
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);
/// A connection that survived at least this long before failing counts as
/// "was actually working" for backoff-reset purposes, rather than a
/// fast-failing reconnect loop being allowed to reset itself back to instant
/// retries forever.
const CONNECTED_LONG_ENOUGH_TO_RESET_BACKOFF: Duration = Duration::from_secs(60);

/// Sent whenever the watched mailbox's `IDLE` connection observes a push
/// from the server. Carries no detail on purpose -- see the module doc.
#[derive(Debug)]
pub struct MailboxChanged;

/// Spawns a task that holds a dedicated `IDLE` connection against `mailbox`
/// for as long as the process runs, sending [`MailboxChanged`] on `wake_tx`
/// every time the server pushes something during the idle wait. Reconnects
/// with exponential backoff on any error -- login failure, TLS/IO error, a
/// server that doesn't support `IDLE` and answers `BAD` -- since unlike a
/// one-shot command this has no caller left to report the error to; it just
/// keeps trying, the same "never leaves the actor stuck" tradeoff
/// `ImapActor::ensure_connected` documents for its own reconnect loop.
///
/// Returns the task's handle so a caller that later has *different*
/// credentials (a fresh sign-in after the old one was revoked) can `abort()`
/// this watch and spawn a new one. Aborting drops the connection.
pub fn spawn(
    host: String,
    port: u16,
    username: String,
    auth: Auth,
    mailbox: String,
    wake_tx: mpsc::Sender<MailboxChanged>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut delay = Duration::from_secs(1);
        loop {
            let started = Instant::now();
            if let Err(e) = run(&host, port, &username, &auth, &mailbox, &wake_tx).await {
                log::warn!("IMAP IDLE watch on {mailbox} lost: {e}");
            }
            if started.elapsed() >= CONNECTED_LONG_ENOUGH_TO_RESET_BACKOFF {
                delay = Duration::from_secs(1);
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(MAX_RECONNECT_DELAY);
        }
    })
}

/// Connects, logs in, selects `mailbox`, then idles in a loop until an error
/// ends the connection. Never returns `Ok` -- the only way out is an error,
/// which `spawn`'s loop turns into a backoff-and-retry.
async fn run(
    host: &str,
    port: u16,
    username: &str,
    auth: &Auth,
    mailbox: &str,
    wake_tx: &mpsc::Sender<MailboxChanged>,
) -> anyhow::Result<()> {
    // The same TLS-connect-and-log-in sequence the main session uses, which
    // also asks `auth` for a fresh secret on every (re)connect: an OAuth
    // access token from the last attempt may have expired by the time the
    // backoff loop comes back.
    let mut session = crate::imap::connect_session(host, port, username, auth).await?;
    // `EXAMINE`, not `SELECT`: this connection only ever watches, it never
    // needs (or wants) write access to flags/expunge.
    session.examine(mailbox).await?;

    loop {
        let mut idle = session.idle();
        idle.init().await?;
        let (wait, _stop) = idle.wait_with_timeout(IDLE_ROUND_TRIP);
        let response = wait.await?;
        session = idle.done().await?;

        match response {
            async_imap::extensions::idle::IdleResponse::NewData(_) => {
                // Send-error means the receiver (main.rs's watch task) is
                // gone, i.e. the app is shutting down -- nothing left to do
                // but let this task end along with it.
                if wake_tx.send(MailboxChanged).await.is_err() {
                    return Ok(());
                }
            }
            async_imap::extensions::idle::IdleResponse::Timeout => {
                // Just the 29-minute round trip; re-issue IDLE, nothing to
                // report.
            }
            async_imap::extensions::idle::IdleResponse::ManualInterrupt => {
                // Nothing here ever fires the StopSource this would come
                // from; unreachable in practice, handled for completeness.
            }
        }
    }
}
