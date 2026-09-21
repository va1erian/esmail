//! New-mail detection and notification text (B10 in PLAN.md).
//!
//! Everything in this module is pure and platform-independent, unlike
//! `tray.rs` (Windows-only tray icon + toast plumbing). It exists so the
//! logic that decides *whether* to notify and *what* the toast says can be
//! unit tested without a Windows toast API, a live IMAP server, or even a
//! GUI -- the same split `search_query.rs`/`db.rs::sync_decision` already
//! use for their own pure cores.
//!
//! Detection is a UID watermark, deliberately independent of `db.rs`'s own
//! `sync_decision`/`SyncPlan`: that machinery exists to drive the SQLite
//! cache (and wipes it on a UIDVALIDITY change), which is a different
//! concern with different failure modes than "should a toast pop up". B10's
//! background poll (see `ImapCommand::PollMailbox` in `imap.rs`) only ever
//! does a cheap `EXAMINE`, never touches the DbActor, and keeps its own
//! watermark in memory for exactly the one mailbox it watches (INBOX -- see
//! PLAN.md §B10 for why the scope stops there).

use crate::imap::MailHeader;

/// UIDVALIDITY/UIDNEXT as last observed by the notification watcher for one
/// mailbox. Same two numbers `db.rs`'s `sync_decision` uses, kept as a
/// separate type so the two pieces of logic can't be accidentally mixed up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MailWatermark {
    pub uid_validity: u32,
    pub uid_next: u32,
}

/// What `update_watermark` decided happened between two observations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatermarkUpdate {
    /// First observation of this mailbox (or a UIDVALIDITY change, which
    /// means the server reassigned UIDs and "new since last time" can't be
    /// answered meaningfully). Never notify on a `Baseline` -- otherwise
    /// every login, or every reconnect, would "discover" the user's entire
    /// mailbox as new mail and fire a toast for each message in it.
    Baseline,
    /// UIDNEXT advanced under an unchanged UIDVALIDITY: real new mail.
    /// `first_new_uid` is the first UID worth fetching to build a
    /// notification; `count` is how many UIDs arrived (`current.uid_next -
    /// previous.uid_next`).
    NewMail { first_new_uid: u32, count: u32 },
    /// Nothing changed since the last observation.
    Unchanged,
}

/// Fold one new mailbox observation into the running watermark, deciding
/// whether it represents new mail worth notifying about. Returns the
/// watermark to keep for next time alongside the decision -- callers should
/// always store the first element, even for `Baseline`/`Unchanged`, so the
/// *next* call has the right `previous`.
pub fn update_watermark(
    previous: Option<MailWatermark>,
    current: MailWatermark,
) -> (MailWatermark, WatermarkUpdate) {
    let update = match previous {
        None => WatermarkUpdate::Baseline,
        Some(p) if p.uid_validity != current.uid_validity => WatermarkUpdate::Baseline,
        Some(p) if current.uid_next > p.uid_next => WatermarkUpdate::NewMail {
            first_new_uid: p.uid_next,
            count: current.uid_next - p.uid_next,
        },
        Some(_) => WatermarkUpdate::Unchanged,
    };
    (current, update)
}

/// Longest a sanitized `From`/`Subject` field is allowed to be before
/// truncation. Chosen to comfortably fit Windows' toast layout without a
/// pathological header (nothing stops a sender from mailing a 10 KB
/// Subject) producing a toast that's silently clipped or dropped by the
/// notification API in some unpredictable way.
const MAX_FIELD_LEN: usize = 120;

/// Collapse a header field down to one safe display line: control
/// characters (including `\n`/`\r`) become spaces, runs of whitespace
/// collapse to one, and the result is truncated with a trailing `…`.
///
/// This is content sanitization, not markup escaping -- `tray.rs`'s
/// `winrt_notification::Toast` already XML-escapes whatever text it's given
/// (verified by reading its source: `title()`/`text1()` run content through
/// `xml::escape::escape_str_attribute`), so a crafted Subject containing
/// `</text><text>` can't break out of the toast's XML. What escaping does
/// *not* prevent is a subject or sender containing literal newlines/control
/// bytes from displaying as extra toast lines or otherwise fighting the
/// layout -- a legitimate-looking multi-line header is still exactly one
/// line by the time it gets here.
fn sanitize_toast_field(input: &str) -> String {
    let collapsed: String = input
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let collapsed = collapsed.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&collapsed, MAX_FIELD_LEN)
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{truncated}\u{2026}")
}

/// Build a toast's (title, body) from the headers `ImapCommand::
/// FetchNewHeaders` fetched for a `WatermarkUpdate::NewMail`. `None` if
/// there's nothing to say (an empty list -- e.g. the fetch raced with the
/// messages being deleted again before it completed).
///
/// One message names its sender in the title and shows the subject; more
/// than one collapses to a count rather than naming every sender, both to
/// keep the toast short and to avoid one call site having to decide how
/// many names is too many.
pub fn build_notification(headers: &[MailHeader]) -> Option<(String, String)> {
    match headers.len() {
        0 => None,
        1 => {
            let from = sanitize_toast_field(&headers[0].from);
            let from = if from.is_empty() { "someone".to_string() } else { from };
            let subject = sanitize_toast_field(&headers[0].subject);
            let subject = if subject.is_empty() {
                "(no subject)".to_string()
            } else {
                subject
            };
            Some((format!("New mail from {from}"), subject))
        }
        n => Some(("New mail".to_string(), format!("{n} new messages in INBOX"))),
    }
}

/// [`build_notification`] with the account's label in front of the title
/// (`Work: New mail from Bob`), so a toast says which account the mail
/// arrived in once there can be several. An empty label leaves the title
/// unchanged.
pub fn build_account_notification(label: &str, headers: &[MailHeader]) -> Option<(String, String)> {
    let (title, body) = build_notification(headers)?;
    let label = sanitize_toast_field(label);
    if label.is_empty() {
        return Some((title, body));
    }
    Some((format!("{label}: {title}"), body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wm(uid_validity: u32, uid_next: u32) -> MailWatermark {
        MailWatermark { uid_validity, uid_next }
    }

    fn header(from: &str, subject: &str) -> MailHeader {
        MailHeader {
            uid: 1,
            subject: subject.to_string(),
            from: from.to_string(),
            to: "me@example.com".to_string(),
            date: "2026-01-01".to_string(),
            message_id: "<test@example.com>".to_string(),
            flags: Vec::new(),
        }
    }

    // ── update_watermark ─────────────────────────────────────────────────

    #[test]
    fn first_observation_is_a_baseline_not_new_mail() {
        // A first login (or first poll after a reconnect) must never be
        // reported as "new mail" -- otherwise every login discovers the
        // whole mailbox as new and fires a toast per message.
        let (stored, update) = update_watermark(None, wm(100, 50));
        assert_eq!(stored, wm(100, 50));
        assert_eq!(update, WatermarkUpdate::Baseline);
    }

    #[test]
    fn unchanged_uid_next_is_unchanged() {
        let (_, update) = update_watermark(Some(wm(100, 50)), wm(100, 50));
        assert_eq!(update, WatermarkUpdate::Unchanged);
    }

    #[test]
    fn advancing_uid_next_is_new_mail_with_the_right_range_and_count() {
        let (_, update) = update_watermark(Some(wm(100, 50)), wm(100, 53));
        assert_eq!(
            update,
            WatermarkUpdate::NewMail { first_new_uid: 50, count: 3 }
        );
    }

    #[test]
    fn changed_uid_validity_is_a_baseline_not_new_mail() {
        // UIDVALIDITY changing means the server reassigned every UID in the
        // mailbox; there's no meaningful "new since last time" to report, so
        // this resets the baseline instead of (wrongly) computing a
        // UID delta across two different numbering schemes.
        let (stored, update) = update_watermark(Some(wm(100, 50)), wm(200, 5));
        assert_eq!(stored, wm(200, 5));
        assert_eq!(update, WatermarkUpdate::Baseline);
    }

    #[test]
    fn uid_next_going_backwards_does_not_underflow_or_report_new_mail() {
        // Shouldn't happen on a real server, but a defensive test: this must
        // not panic on subtraction overflow, and treating it as "new mail"
        // with a nonsensical count would be worse than just ignoring it.
        let (_, update) = update_watermark(Some(wm(100, 50)), wm(100, 10));
        assert_eq!(update, WatermarkUpdate::Unchanged);
    }

    // ── build_notification ───────────────────────────────────────────────

    #[test]
    fn no_headers_means_no_notification() {
        assert_eq!(build_notification(&[]), None);
    }

    #[test]
    fn single_message_names_the_sender_in_the_title() {
        let headers = [header("alice@example.com", "Lunch?")];
        let (title, body) = build_notification(&headers).unwrap();
        assert_eq!(title, "New mail from alice@example.com");
        assert_eq!(body, "Lunch?");
    }

    #[test]
    fn multiple_messages_collapse_to_a_count_instead_of_naming_every_sender() {
        let headers = [
            header("alice@example.com", "Lunch?"),
            header("bob@example.com", "Re: Lunch?"),
        ];
        let (title, body) = build_notification(&headers).unwrap();
        assert_eq!(title, "New mail");
        assert_eq!(body, "2 new messages in INBOX");
    }

    #[test]
    fn blank_subject_and_sender_fall_back_to_placeholders() {
        let headers = [header("", "")];
        let (title, body) = build_notification(&headers).unwrap();
        assert_eq!(title, "New mail from someone");
        assert_eq!(body, "(no subject)");
    }

    #[test]
    fn account_notification_prefixes_the_title_with_the_account_label() {
        let headers = [header("Bob", "Lunch?")];
        let (title, body) = build_account_notification("Work", &headers).unwrap();
        assert_eq!(title, "Work: New mail from Bob");
        assert_eq!(body, "Lunch?");
    }

    #[test]
    fn account_notification_with_a_blank_label_matches_the_plain_one() {
        let headers = [header("Bob", "Lunch?")];
        assert_eq!(build_account_notification("  ", &headers), build_notification(&headers));
        assert_eq!(build_account_notification("Work", &[]), None);
    }

    // ── sanitize_toast_field (via build_notification) ───────────────────

    #[test]
    fn newlines_and_control_characters_collapse_to_a_single_line() {
        // Regression guard: a crafted Subject header containing "\r\n\r\n"
        // (an attempt to fake extra toast lines, e.g. impersonating a
        // system message below the real subject) must not survive as
        // anything but a single space in the sanitized output.
        let headers = [header("alice@example.com", "hi\r\n\r\nFAKE: you won a prize")];
        let (_, body) = build_notification(&headers).unwrap();
        assert_eq!(body, "hi FAKE: you won a prize");
        assert!(!body.contains('\n') && !body.contains('\r'));
    }

    #[test]
    fn long_subject_is_truncated_with_an_ellipsis() {
        let long_subject = "x".repeat(500);
        let headers = [header("alice@example.com", &long_subject)];
        let (_, body) = build_notification(&headers).unwrap();
        assert_eq!(body.chars().count(), MAX_FIELD_LEN);
        assert!(body.ends_with('\u{2026}'));
    }

    #[test]
    fn short_subject_is_left_alone() {
        let headers = [header("alice@example.com", "short")];
        let (_, body) = build_notification(&headers).unwrap();
        assert_eq!(body, "short");
    }
}
