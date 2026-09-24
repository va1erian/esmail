//! Sending mail: the SMTP actor on the runtime, and its outcomes drained on the
//! UI thread the same way the IMAP events are.

use std::sync::mpsc as std_mpsc;

use esmail::auth::Auth;
use esmail::compose::{ComposeId, ComposeState};
use esmail::config::AccountConfig;
use esmail::secrets;
use esmail::smtp::{self, SmtpAccount, SmtpActor, SmtpCommand, SmtpEvent};
use esmail::waker::Waker;
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use super::PASSWORD_FALLBACK_VAR;

/// How many sends the actor queues before a new one is refused.
const COMMAND_QUEUE: usize = 16;

/// The SMTP actor and the channel its outcomes arrive on.
pub struct Sender {
    commands: mpsc::Sender<SmtpCommand>,
    events: std_mpsc::Receiver<SmtpEvent>,
}

impl Sender {
    /// Starts the actor on `runtime`; `waker` is called whenever an outcome is
    /// waiting.
    pub fn start(runtime: &Handle, waker: Waker) -> Sender {
        let (commands, command_rx) = mpsc::channel(COMMAND_QUEUE);
        let (actor_tx, mut actor_events) = mpsc::channel(COMMAND_QUEUE);
        let (events_tx, events) = std_mpsc::channel();
        SmtpActor::spawn(runtime, command_rx, actor_tx);
        runtime.spawn(async move {
            while let Some(event) = actor_events.recv().await {
                if events_tx.send(event).is_err() {
                    return;
                }
                waker();
            }
        });
        Sender { commands, events }
    }

    /// Queues `state` to be sent as `account`; its outcome comes back under `id`.
    pub fn send(&self, id: ComposeId, account: SmtpAccount, state: ComposeState) -> Result<(), String> {
        self.commands.try_send(SmtpCommand::Send { id, account, compose: state }).map_err(|_| "Could not queue the message for sending.".to_string())
    }

    /// Moves the outcomes that arrived since the last call out of the channel.
    /// Never blocks.
    pub fn pump(&self) -> Vec<SmtpEvent> {
        self.events.try_iter().collect()
    }
}

/// What `account` needs to send: its server settings and credentials. A Google
/// account sends with the token source its IMAP session uses; a password
/// account with its saved SMTP password, or the fallback variable's.
pub fn smtp_account(account: &AccountConfig, imap_auth: Option<&Auth>) -> Result<SmtpAccount, String> {
    let auth = match imap_auth {
        Some(auth) if auth.is_oauth() => auth.clone(),
        _ => match secrets::get_password(&account.id, "smtp") {
            Some(password) => Auth::Password(password),
            None => match std::env::var(PASSWORD_FALLBACK_VAR) {
                Ok(password) if !password.is_empty() => Auth::password(password),
                _ => return Err(format!("{} has no saved SMTP password; connect once in the egui esMail first.", account.display_name)),
            },
        },
    };
    Ok(SmtpAccount {
        host: account.smtp_host.clone(),
        port: account.smtp_port,
        tls: account.smtp_tls,
        username: account.username.clone(),
        auth,
        from_address: account.username.clone(),
    })
}

/// Whether `state` can be sent at all, in words for the compose window. This is
/// the message the actor will build, minus the attachments' bytes, so an
/// address that does not parse is reported now rather than queued for retries
/// that could never succeed.
pub fn check_sendable(account: &SmtpAccount, state: &ComposeState) -> Result<(), String> {
    if [&state.to, &state.cc, &state.bcc].iter().all(|field| field.trim().is_empty()) {
        return Err("Add at least one recipient.".to_string());
    }
    let without_attachments = ComposeState { attachments: Vec::new(), ..state.clone() };
    smtp::build_message(account, &without_attachments).map(drop).map_err(|error| format!("This message cannot be sent: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account() -> SmtpAccount {
        SmtpAccount {
            host: "localhost".into(),
            port: 25,
            tls: esmail::config::TlsMode::None,
            username: "me@example.com".into(),
            auth: Auth::password("secret"),
            from_address: "me@example.com".into(),
        }
    }

    fn to(recipients: &str) -> ComposeState {
        ComposeState { to: recipients.into(), subject: "hi".into(), ..Default::default() }
    }

    #[test]
    fn a_message_with_no_recipient_is_refused() {
        assert_eq!(check_sendable(&account(), &to("  ")).unwrap_err(), "Add at least one recipient.");
    }

    #[test]
    fn a_recipient_that_is_not_an_address_is_refused() {
        assert!(check_sendable(&account(), &to("not an address")).unwrap_err().starts_with("This message cannot be sent"));
    }

    #[test]
    fn a_bcc_only_message_can_be_sent() {
        let state = ComposeState { bcc: "b@example.com".into(), ..Default::default() };
        assert!(check_sendable(&account(), &state).is_ok());
    }

    #[test]
    fn several_recipients_with_names_are_accepted() {
        assert!(check_sendable(&account(), &to("Ann <ann@example.com>, bob@example.com")).is_ok());
    }
}
