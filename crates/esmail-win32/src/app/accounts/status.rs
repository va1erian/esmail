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
    /// The status after `event`, if the event changes it. An error only counts
    /// while the account has not signed in: later ones belong to single
    /// requests (a folder that cannot be opened) and say nothing about the
    /// account.
    pub fn after(&self, event: &ImapEvent) -> Option<Status> {
        match event {
            ImapEvent::Connected => Some(Status::Connected),
            ImapEvent::Disconnected => Some(Status::Reconnecting),
            ImapEvent::Error(error) if !matches!(self, Status::Connected | Status::Reconnecting) => Some(Status::Failed(error.clone())),
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
    fn an_error_of_one_request_leaves_a_connected_account_alone() {
        assert_eq!(Status::Connected.after(&ImapEvent::Error("no such folder".into())), None);
        assert_eq!(Status::Reconnecting.after(&ImapEvent::Error("timeout".into())), None);
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
