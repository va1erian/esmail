//! What the GUI and the listener say to each other. One JSON object per line,
//! tagged by `type`.

use serde::{Deserialize, Serialize};

/// Bumped when a message changes incompatibly; a `Hello` with any other value
/// is refused, so a listener left over from an older install and a newer GUI
/// (or the reverse) fail cleanly instead of misreading each other.
pub const PROTOCOL_VERSION: u32 = 1;

/// Sent by the GUI (the client) to the listener.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToListener {
    /// The first message on every connection: proves the sender read the
    /// per-session token, and names the protocol it speaks.
    Hello { token: String, version: u32 },
    /// The accounts in `config.toml` changed (added, removed, edited in
    /// Settings): reload them.
    ConfigChanged,
}

/// Sent by the listener to the GUI (the client).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToGui {
    /// The answer to a valid `Hello`; nothing is sent before it.
    Welcome { version: u32 },
    /// Come to the front (the tray icon was clicked, or esMail was launched
    /// again).
    Show,
    /// Show the mailbox of this account (a toast was clicked).
    OpenAccount { account: String },
    /// Exit.
    Quit,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_tagged_objects() {
        let hello = serde_json::to_string(&ToListener::Hello { token: "t".into(), version: 1 }).unwrap();
        assert_eq!(hello, r#"{"type":"hello","token":"t","version":1}"#);
        assert_eq!(serde_json::to_string(&ToGui::Show).unwrap(), r#"{"type":"show"}"#);
        assert_eq!(
            serde_json::to_string(&ToGui::OpenAccount { account: "a@b".into() }).unwrap(),
            r#"{"type":"open_account","account":"a@b"}"#
        );
    }

    #[test]
    fn messages_round_trip() {
        for message in [ToGui::Welcome { version: 1 }, ToGui::Show, ToGui::OpenAccount { account: "x".into() }, ToGui::Quit] {
            let text = serde_json::to_string(&message).unwrap();
            assert_eq!(serde_json::from_str::<ToGui>(&text).unwrap(), message);
        }
        let text = serde_json::to_string(&ToListener::ConfigChanged).unwrap();
        assert_eq!(serde_json::from_str::<ToListener>(&text).unwrap(), ToListener::ConfigChanged);
    }

    #[test]
    fn an_unknown_message_is_an_error_not_a_panic() {
        assert!(serde_json::from_str::<ToGui>(r#"{"type":"format_disk"}"#).is_err());
        assert!(serde_json::from_str::<ToGui>("not json").is_err());
    }
}
