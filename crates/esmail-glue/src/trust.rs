//! Which senders' remote images load without asking ("Always load from ...").
//! The list is `Config::image_trusted_senders`, shared with the other frontends.

use esmail::config::Config;
use esmail::imap::MailHeader;

/// Whether `header`'s sender is on the always-load list. A message with no
/// readable sender address is never trusted.
pub fn sender_trusted(config: &Config, header: &MailHeader) -> bool {
    header.sender_address().is_some_and(|address| config.is_image_trusted(&address))
}

/// The longest sender name the "Always load from ..." button shows.
const MAX_BUTTON_NAME: usize = 24;

/// `name` shortened to fit a button, with an ellipsis where it was cut.
pub fn button_name(name: &str) -> String {
    if name.chars().count() <= MAX_BUTTON_NAME {
        return name.to_string();
    }
    let kept: String = name.chars().take(MAX_BUTTON_NAME - 1).collect();
    format!("{kept}\u{2026}")
}

/// Trusts (or stops trusting) `header`'s sender. Returns whether the config
/// changed, i.e. whether it needs saving.
pub fn set_sender_trusted(config: &mut Config, header: &MailHeader, trusted: bool) -> bool {
    header.sender_address().is_some_and(|address| config.set_image_trusted(&address, trusted))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from(sender: &str) -> MailHeader {
        MailHeader {
            uid: 1,
            subject: String::new(),
            from: sender.into(),
            to: String::new(),
            date: String::new(),
            message_id: String::new(),
            flags: Vec::new(),
        }
    }

    #[test]
    fn trusting_a_sender_covers_every_spelling_of_the_address() {
        let mut config = Config::default();
        let news = from("News <News@Example.com>");
        assert!(!sender_trusted(&config, &news));
        assert!(set_sender_trusted(&mut config, &news, true));
        assert!(sender_trusted(&config, &from("news@example.com")));
        assert!(!sender_trusted(&config, &from("other@example.com")));
    }

    #[test]
    fn trusting_twice_or_untrusting_a_stranger_changes_nothing() {
        let mut config = Config::default();
        let news = from("news@example.com");
        assert!(set_sender_trusted(&mut config, &news, true));
        assert!(!set_sender_trusted(&mut config, &news, true));
        assert!(set_sender_trusted(&mut config, &news, false));
        assert!(!set_sender_trusted(&mut config, &news, false));
        assert!(!sender_trusted(&config, &news));
    }

    #[test]
    fn a_long_sender_name_is_cut_to_fit_the_button() {
        assert_eq!(button_name("alice@example.com"), "alice@example.com");
        let cut = button_name("someone.with.a.very.long.address@subdomain.example.com");
        assert_eq!(cut.chars().count(), MAX_BUTTON_NAME);
        assert!(cut.ends_with('\u{2026}'));
    }

    #[test]
    fn a_sender_without_an_address_cannot_be_trusted() {
        let mut config = Config::default();
        let nobody = from("");
        assert!(!set_sender_trusted(&mut config, &nobody, true));
        assert!(!sender_trusted(&config, &nobody));
    }
}
