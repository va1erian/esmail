//! What each account's session is doing, for the Accounts window and the
//! banner that offers to reconnect.

use esmail::config::{AccountConfig, AuthKind};
use esmail::imap::ImapEvent;

use super::manage::AccountRow;

/// The state of one account's session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Signing in for the first time since the window opened.
    Connecting,
    /// Signed in.
    Connected,
    /// The connection dropped; the session is trying again.
    Reconnecting,
    /// The account could not sign in: what the server or the keyring said.
    Failed(String),
}

impl Status {
    /// Why the account cannot sign in, while it cannot.
    pub fn failure(&self) -> Option<&str> {
        match self {
            Status::Failed(error) => Some(error),
            _ => None,
        }
    }

    /// The status after `event`, if the event changes it. An error only counts
    /// while the account has not signed in: later ones belong to single
    /// requests (a folder that cannot be opened) and say nothing about the
    /// account.
    pub fn after(&self, event: &ImapEvent) -> Option<Status> {
        match event {
            ImapEvent::Connected => Some(Status::Connected),
            ImapEvent::Disconnected => Some(Status::Reconnecting),
            // A command sent while the session is still signing in is refused with
            // this; it says nothing about the credentials.
            ImapEvent::Error(error) if error.starts_with("not connected") => None,
            ImapEvent::Error(error) if !matches!(self, Status::Connected | Status::Reconnecting) || (matches!(self, Status::Reconnecting) && is_sign_in_error(error)) => Some(Status::Failed(error.clone())),
            _ => None,
        }
    }

    fn label(&self) -> String {
        match self {
            Status::Connecting => "Connecting...".to_string(),
            Status::Connected => "Connected".to_string(),
            Status::Reconnecting => "Disconnected, reconnecting...".to_string(),
            Status::Failed(error) => format!("Not signed in: {}", error.lines().next().unwrap_or_default()),
        }
    }
}

/// Whether `error` says the server or the token endpoint no longer accepts the
/// account's credentials, as opposed to a network failure. A session that was
/// signed in and then finds its credentials refused (a password changed, a
/// Google token that expired) must not sit at "reconnecting" for ever.
fn is_sign_in_error(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    ["authenticationfailed", "authentication failed", "invalid credentials", "login failed", "sign in with google again", "no longer accepts the saved sign-in"]
        .iter()
        .any(|marker| error.contains(marker))
}

/// The Accounts window's rows for `accounts` and their `statuses`.
pub fn rows(accounts: &[AccountConfig], statuses: &[Status]) -> Vec<AccountRow> {
    accounts
        .iter()
        .zip(statuses)
        .map(|(account, status)| AccountRow {
            id: account.id.clone(),
            name: account.display_name.clone(),
            address: account.username.clone(),
            google: account.auth == AuthKind::GoogleOAuth,
            status: status.label(),
            failed: matches!(status, Status::Failed(_)),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signing_in_and_dropping_are_tracked() {
        assert_eq!(Status::Connecting.after(&ImapEvent::Connected), Some(Status::Connected));
        assert_eq!(Status::Connected.after(&ImapEvent::Disconnected), Some(Status::Reconnecting));
        assert_eq!(Status::Reconnecting.after(&ImapEvent::Connected), Some(Status::Connected));
    }

    #[test]
    fn an_error_before_signing_in_fails_the_account() {
        let error = ImapEvent::Error("LOGIN failed".into());
        assert_eq!(Status::Connecting.after(&error), Some(Status::Failed("LOGIN failed".into())));
        assert_eq!(Status::Failed("x".into()).after(&error), Some(Status::Failed("LOGIN failed".into())));
    }

    #[test]
    fn a_command_sent_before_the_session_is_up_does_not_fail_the_account() {
        assert_eq!(Status::Connecting.after(&ImapEvent::Error("not connected yet".into())), None);
    }

    #[test]
    fn an_error_of_one_request_leaves_a_connected_account_alone() {
        assert_eq!(Status::Connected.after(&ImapEvent::Error("no such folder".into())), None);
        assert_eq!(Status::Reconnecting.after(&ImapEvent::Error("timeout".into())), None);
    }

    #[test]
    fn a_refused_login_after_signing_in_fails_the_account_but_a_timeout_does_not() {
        let refused = ImapEvent::Error("No Response: [AUTHENTICATIONFAILED] Invalid credentials".into());
        assert_eq!(Status::Reconnecting.after(&refused), Some(Status::Failed("No Response: [AUTHENTICATIONFAILED] Invalid credentials".into())));
        let expired = ImapEvent::Error("Google no longer accepts the saved sign-in. Sign in with Google again.".into());
        assert!(matches!(Status::Reconnecting.after(&expired), Some(Status::Failed(_))));
        assert_eq!(Status::Reconnecting.after(&ImapEvent::Error("connection timed out".into())), None);
        assert_eq!(Status::Connected.after(&refused), None, "a connected account's request errors are not sign-in failures");
    }

    #[test]
    fn rows_pair_accounts_with_statuses_and_flag_failures() {
        let mut google = AccountConfig::new("Me".into(), "imap.gmail.com".into(), 993, "me@gmail.com".into());
        google.auth = AuthKind::GoogleOAuth;
        let plain = AccountConfig::new("Work".into(), "imap.example.com".into(), 993, "w@example.com".into());
        let rows = rows(&[google, plain], &[Status::Failed("bad token\nmore".into()), Status::Connected]);
        assert!(rows[0].google && rows[0].failed);
        assert_eq!(rows[0].status, "Not signed in: bad token");
        assert!(!rows[1].google && !rows[1].failed);
        assert_eq!(rows[1].status, "Connected");
    }
}
