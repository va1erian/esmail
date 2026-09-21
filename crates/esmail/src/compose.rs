//! Compose window state and Reply/Reply-All/Forward derivation (B7 of
//! PLAN.md). Sending itself lives in `smtp.rs`; this module only builds the
//! editable state a compose window starts from — pure and unit-tested, no
//! network or UI.
//!
//! **Known limitation inherited from `MailHeader`:** `imap.rs`'s envelope
//! parsing only ever kept the *first* From/To address
//! (`addrs.and_then(|f| f.first())`, predating this module), not the full
//! recipient list. So Reply-All's Cc can only ever be "the original To
//! address, if it wasn't me" rather than a real multi-recipient list — it
//! degrades gracefully (an empty Cc is still a correct, just less complete,
//! answer) rather than fabricating recipients that were never captured.
//! Fixing this needs `imap.rs` to capture every address, not just the first.

use crate::imap::MailHeader;

/// Everything a compose window edits. Reply-derived state comes from
/// [`ComposeState::reply`]/[`ComposeState::reply_all`], forwards from
/// [`ComposeState::forward`]; a blank one is just `ComposeState::default()`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ComposeState {
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
    /// `Message-ID` of the message being replied to, angle brackets
    /// included — goes straight into the `In-Reply-To` header. `None` for a
    /// new message or a forward (forwarding doesn't reply to anything).
    pub in_reply_to: Option<String>,
    /// Simplified to just the immediate parent's `Message-ID`, not the full
    /// ancestor chain a `References` header ideally carries — `imap.rs`
    /// doesn't fetch the original `References` header today. Still a valid
    /// `References` value (RFC 5322 doesn't require more than one), just a
    /// shorter thread than a client that tracked the whole chain would show.
    pub references: Option<String>,
    pub attachments: Vec<(String, Vec<u8>)>,
    /// The account this message is sent from, and whose Sent folder gets the
    /// copy: `AccountConfig::id`. `None` until the UI picks one -- it
    /// defaults to the account of the message being replied to, else the
    /// active one (see `main.rs`). Carrying it here rather than in a separate
    /// app-level field keeps it with the message it belongs to, which is what
    /// a compose-window-per-message design (#34) will need.
    pub account_id: Option<String>,
}

impl ComposeState {
    /// This message, sent from `account_id`.
    pub fn with_account(mut self, account_id: Option<String>) -> Self {
        self.account_id = account_id;
        self
    }

    /// Reply to just the sender.
    pub fn reply(original: &MailHeader, original_body_html: &str) -> Self {
        Self::from_original(original, original_body_html, String::new())
    }

    /// Reply to the sender and (best-effort — see the module docs) the
    /// other original recipients, minus `my_address`.
    pub fn reply_all(original: &MailHeader, original_body_html: &str, my_address: &str) -> Self {
        let cc = if addresses_match(&original.to, my_address) {
            String::new()
        } else {
            original.to.clone()
        };
        Self::from_original(original, original_body_html, cc)
    }

    fn from_original(original: &MailHeader, original_body_html: &str, cc: String) -> Self {
        Self {
            to: original.from.clone(),
            cc,
            bcc: String::new(),
            subject: add_prefix(&original.subject, "Re:"),
            body: quote_original(original, original_body_html),
            in_reply_to: non_empty(&original.message_id),
            references: non_empty(&original.message_id),
            attachments: Vec::new(),
            account_id: None,
        }
    }

    /// Forward: quotes the original like a reply does, but addresses nobody
    /// yet (the user picks a new recipient) and doesn't reply to anything —
    /// forwarding isn't part of the original thread.
    pub fn forward(original: &MailHeader, original_body_html: &str) -> Self {
        Self {
            to: String::new(),
            cc: String::new(),
            bcc: String::new(),
            subject: add_prefix(&original.subject, "Fwd:"),
            body: quote_original(original, original_body_html),
            in_reply_to: None,
            references: None,
            attachments: Vec::new(),
            account_id: None,
        }
    }
}

/// `subject` with `"{prefix} "` in front, unless it already starts with that
/// prefix (case-insensitively) — so replying to a reply doesn't pile up
/// "Re: Re: Re: ...".
fn add_prefix(subject: &str, prefix: &str) -> String {
    let already_has_it = subject
        .trim_start()
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix));
    if already_has_it {
        subject.to_string()
    } else {
        format!("{prefix} {subject}")
    }
}

/// A very loose "is this the same mailbox" check for the module's one use —
/// deciding whether Reply-All's Cc should include the original To address,
/// or drop it because it's already the sender ("me"). `original_to` is
/// typically `"Name <user@host>"` (see the module docs on why it's a single
/// address, not a list); this treats any exact substring match of
/// `my_address` in it as "yes, that's me" rather than parsing out the
/// address precisely, since the alternative (silently duplicating a
/// recipient) is worse than an occasional false negative.
fn addresses_match(original_to: &str, my_address: &str) -> bool {
    !my_address.is_empty() && original_to.contains(my_address)
}

fn non_empty(s: &str) -> Option<String> {
    if s.is_empty() { None } else { Some(s.to_string()) }
}

/// A plain-text quoted copy of the original message: an attribution line,
/// then every line of its body prefixed with `"> "`. `original_body_html` is
/// the already-sanitized HTML `render.rs` produced (this is quoting what the
/// *reader* saw, not the raw message) — stripped back to plain text with
/// `ammonia::Builder::empty()`, which keeps text nodes and drops every tag.
fn quote_original(original: &MailHeader, original_body_html: &str) -> String {
    let plain = ammonia::Builder::empty().clean(original_body_html).to_string();
    let quoted_lines: Vec<String> = plain.lines().map(|line| format!("> {line}")).collect();
    format!(
        "\n\nOn {}, {} wrote:\n{}",
        original.date,
        original.from,
        quoted_lines.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn original() -> MailHeader {
        MailHeader {
            uid: 1,
            subject: "Dinner plans".to_string(),
            from: "Alice <alice@example.com>".to_string(),
            to: "Bob <bob@example.com>".to_string(),
            date: "Mon, 1 Jan 2026 12:00:00 +0000".to_string(),
            message_id: "<abc123@example.com>".to_string(),
            flags: Vec::new(),
        }
    }

    #[test]
    fn reply_addresses_the_original_sender() {
        let state = ComposeState::reply(&original(), "<p>See you at 7</p>");
        assert_eq!(state.to, "Alice <alice@example.com>");
        assert_eq!(state.cc, "");
    }

    #[test]
    fn reply_prefixes_the_subject_once() {
        let state = ComposeState::reply(&original(), "body");
        assert_eq!(state.subject, "Re: Dinner plans");
    }

    #[test]
    fn replying_to_a_reply_does_not_double_the_prefix() {
        let mut msg = original();
        msg.subject = "Re: Dinner plans".to_string();
        let state = ComposeState::reply(&msg, "body");
        assert_eq!(state.subject, "Re: Dinner plans");
    }

    #[test]
    fn reply_sets_in_reply_to_and_references_from_the_message_id() {
        let state = ComposeState::reply(&original(), "body");
        assert_eq!(state.in_reply_to.as_deref(), Some("<abc123@example.com>"));
        assert_eq!(state.references.as_deref(), Some("<abc123@example.com>"));
    }

    #[test]
    fn reply_to_a_message_with_no_message_id_leaves_threading_headers_unset() {
        let mut msg = original();
        msg.message_id = String::new();
        let state = ComposeState::reply(&msg, "body");
        assert_eq!(state.in_reply_to, None);
        assert_eq!(state.references, None);
    }

    #[test]
    fn reply_quotes_the_body_as_plain_text_with_an_attribution_line() {
        let state = ComposeState::reply(&original(), "<p>See you at <b>7</b></p>");
        assert!(state.body.contains("Alice <alice@example.com> wrote"));
        assert!(state.body.contains("> See you at 7"));
        assert!(!state.body.contains("<p>"));
        assert!(!state.body.contains("<b>"));
    }

    #[test]
    fn reply_all_ccs_the_original_recipient_when_it_is_not_me() {
        let state = ComposeState::reply_all(&original(), "body", "someone-else@example.com");
        assert_eq!(state.cc, "Bob <bob@example.com>");
    }

    #[test]
    fn reply_all_drops_myself_from_cc_rather_than_ccing_my_own_address() {
        let state = ComposeState::reply_all(&original(), "body", "bob@example.com");
        assert_eq!(state.cc, "");
    }

    #[test]
    fn forward_addresses_nobody_and_does_not_reply_to_anything() {
        let state = ComposeState::forward(&original(), "body");
        assert_eq!(state.to, "");
        assert_eq!(state.in_reply_to, None);
        assert_eq!(state.references, None);
    }

    #[test]
    fn forward_prefixes_the_subject_with_fwd() {
        let state = ComposeState::forward(&original(), "body");
        assert_eq!(state.subject, "Fwd: Dinner plans");
    }

    #[test]
    fn a_blank_compose_state_has_no_threading_headers() {
        let state = ComposeState::default();
        assert_eq!(state.to, "");
        assert_eq!(state.in_reply_to, None);
    }
}
