//! The rows of the Drafts and Outbox windows, as plain data: what each column
//! says, with the same fallbacks the egui windows use for a blank recipient or
//! subject.

use chrono::{DateTime, Local};
use esmail::compose::ComposeState;
use esmail::config::AccountConfig;
use esmail::db::{DraftSummary, OutboxItem};

/// One line of the Drafts or Outbox window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueRow {
    /// The `drafts` or `outbox` row this stands for.
    pub id: i64,
    /// The message's subject.
    pub subject: String,
    /// Who it is addressed to.
    pub to: String,
    /// A draft: when it was saved. An outbox message: the account it is sent from.
    pub detail: String,
    /// An outbox message: waiting, or why it did not go out. Empty for a draft.
    pub status: String,
    /// The message failed to send at least once.
    pub failed: bool,
}

fn or_placeholder(text: &str, placeholder: &str) -> String {
    if text.trim().is_empty() { placeholder.to_string() } else { text.to_string() }
}

/// The time a draft was saved, in the user's time zone.
fn saved_at(seconds: i64) -> String {
    DateTime::from_timestamp(seconds, 0).map_or_else(String::new, |utc| utc.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string())
}

/// The Drafts window's rows, newest first as the cache returns them.
pub fn draft_rows(drafts: &[DraftSummary]) -> Vec<QueueRow> {
    drafts
        .iter()
        .map(|draft| QueueRow {
            id: draft.id,
            subject: or_placeholder(&draft.subject, "(no subject)"),
            to: or_placeholder(&draft.to, "(no recipient)"),
            detail: saved_at(draft.updated_at),
            status: String::new(),
            failed: false,
        })
        .collect()
}

/// What an outbox message's Status column says.
fn outbox_status(item: &OutboxItem) -> String {
    match (item.attempts, &item.last_error) {
        (0, _) => "Waiting to send".to_string(),
        (attempts, Some(error)) => format!("Retried {attempts} time(s): {error}"),
        (attempts, None) => format!("Retried {attempts} time(s)"),
    }
}

/// The Outbox window's rows; `accounts` names the account each is sent from.
pub fn outbox_rows(items: &[OutboxItem], accounts: &[AccountConfig]) -> Vec<QueueRow> {
    items
        .iter()
        .map(|item| {
            let ComposeState { subject, to, .. } = &item.compose;
            let account = accounts.iter().find(|account| account.id == item.account_id).map_or(item.account_id.clone(), |account| account.display_name.clone());
            QueueRow {
                id: item.id,
                subject: or_placeholder(subject, "(no subject)"),
                to: or_placeholder(to, "(no recipient)"),
                detail: account,
                status: outbox_status(item),
                failed: item.attempts > 0,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: i64, attempts: i64, error: Option<&str>) -> OutboxItem {
        OutboxItem {
            id,
            account_id: "work".into(),
            compose: ComposeState { to: "bob@example.com".into(), subject: "Lunch".into(), ..Default::default() },
            attempts,
            last_error: error.map(str::to_string),
        }
    }

    #[test]
    fn a_blank_draft_shows_the_same_placeholders_as_the_egui_window() {
        let rows = draft_rows(&[DraftSummary { id: 4, subject: " ".into(), to: String::new(), updated_at: 0 }]);
        assert_eq!((rows[0].subject.as_str(), rows[0].to.as_str()), ("(no subject)", "(no recipient)"));
        assert!(!rows[0].failed && rows[0].status.is_empty());
    }

    #[test]
    fn a_fresh_outbox_message_is_waiting_and_a_retried_one_says_why_it_failed() {
        let mut work = AccountConfig::new("Work".into(), "imap.work.example".into(), 993, "me@work.example".into());
        work.id = "work".into();
        let fresh = item(1, 0, None);
        let mut tried = item(2, 3, Some("connection refused"));
        tried.account_id = "removed-account".into();
        let rows = outbox_rows(&[fresh, tried], &[work]);
        assert_eq!((rows[0].status.as_str(), rows[0].failed, rows[0].detail.as_str()), ("Waiting to send", false, "Work"));
        assert_eq!(rows[1].status, "Retried 3 time(s): connection refused");
        assert!(rows[1].failed);
        assert_eq!(rows[1].detail, "removed-account", "an account that is gone shows its id");
    }

    #[test]
    fn a_saved_time_is_formatted_and_an_impossible_one_is_blank() {
        assert_eq!(saved_at(1_700_000_000).len(), "2023-11-14 22:13".len());
        assert_eq!(saved_at(i64::MAX), "");
    }
}
