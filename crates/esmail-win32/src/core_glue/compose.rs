//! What a compose window starts from and what it keeps track of, as plain data.
//!
//! The reply/forward derivation (subject prefix, quoted body, `In-Reply-To` and
//! `References`) is `esmail::compose`'s; this only picks which one applies and
//! fills in the sending account.

use esmail::compose::ComposeState;
use esmail::config::AccountConfig;
use esmail::imap::MailHeader;

/// How a compose window was started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A blank message.
    New,
    /// Reply to the sender.
    Reply,
    /// Reply to the sender and the other recipients.
    ReplyAll,
    /// Forward, addressed to nobody yet.
    Forward,
}

impl Kind {
    /// Whether the message is derived from another one.
    pub fn needs_original(self) -> bool {
        self != Kind::New
    }
}

/// The message a window opens with: `kind` applied to `original` (its header
/// and its sanitised HTML body), sent from `account`.
pub fn initial_state(kind: Kind, original: Option<(&MailHeader, &str)>, account: &AccountConfig) -> ComposeState {
    let state = match (kind, original) {
        (Kind::Reply, Some((header, body))) => ComposeState::reply(header, body),
        (Kind::ReplyAll, Some((header, body))) => ComposeState::reply_all(header, body, &account.username),
        (Kind::Forward, Some((header, body))) => ComposeState::forward(header, body),
        _ => ComposeState::default(),
    };
    state.with_account(Some(account.id.clone()))
}

/// Whether there is anything worth keeping: a blank window is not autosaved.
pub fn has_content(state: &ComposeState) -> bool {
    [&state.to, &state.cc, &state.bcc, &state.subject, &state.body].iter().any(|field| !field.is_empty()) || !state.attachments.is_empty()
}

/// The window title for a message with this subject.
pub fn window_title(subject: &str) -> String {
    match subject.trim() {
        "" => "New message - esMail".to_string(),
        subject => format!("{subject} - esMail"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account() -> AccountConfig {
        AccountConfig::new("Me".into(), "imap.example.com".into(), 993, "me@example.com".into())
    }

    fn original() -> MailHeader {
        MailHeader {
            uid: 4,
            subject: "Dinner".into(),
            from: "Ann <ann@example.com>".into(),
            to: "me@example.com, Bob <bob@example.com>".into(),
            date: "Mon, 1 Jan 2026 12:00:00 +0000".into(),
            message_id: "<m1@example.com>".into(),
            flags: Vec::new(),
        }
    }

    #[test]
    fn a_new_message_is_blank_and_sent_from_the_account() {
        let state = initial_state(Kind::New, None, &account());
        assert!(!has_content(&state));
        assert_eq!(state.account_id.as_deref(), Some(account().id.as_str()));
    }

    #[test]
    fn a_reply_answers_the_sender_and_threads_onto_the_message() {
        let header = original();
        let state = initial_state(Kind::Reply, Some((&header, "<p>Are you free?</p>")), &account());
        assert_eq!(state.to, "Ann <ann@example.com>");
        assert_eq!(state.subject, "Re: Dinner");
        assert_eq!(state.in_reply_to.as_deref(), Some("<m1@example.com>"));
        assert!(state.body.contains("> Are you free?"));
    }

    #[test]
    fn reply_all_leaves_the_account_itself_out_of_the_cc() {
        let header = original();
        let state = initial_state(Kind::ReplyAll, Some((&header, "body")), &account());
        assert_eq!(state.cc, "Bob <bob@example.com>");
    }

    #[test]
    fn a_forward_is_addressed_to_nobody_and_starts_no_thread() {
        let header = original();
        let state = initial_state(Kind::Forward, Some((&header, "body")), &account());
        assert_eq!((state.to.as_str(), state.subject.as_str()), ("", "Fwd: Dinner"));
        assert_eq!(state.in_reply_to, None);
    }

    #[test]
    fn a_derived_kind_without_its_original_falls_back_to_a_blank_message() {
        assert!(!has_content(&initial_state(Kind::Reply, None, &account())));
    }

    #[test]
    fn an_attachment_alone_is_content() {
        let state = ComposeState { attachments: vec![("a.txt".into(), vec![1])], ..Default::default() };
        assert!(has_content(&state));
    }

    #[test]
    fn the_title_names_the_subject() {
        assert_eq!(window_title("  "), "New message - esMail");
        assert_eq!(window_title("Lunch"), "Lunch - esMail");
    }
}
