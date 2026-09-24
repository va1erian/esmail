//! Address suggestions under the recipient fields: which candidates match what
//! is being typed, and the text edit that accepting one makes.

use std::rc::Rc;

use esmail::contacts::{Contacts, complete_recipient, recipient_token};

/// How many suggestions are offered at once.
const MAX_SUGGESTIONS: usize = 6;

/// A recipient field's text and where its caret is, as the edit reports them.
pub struct Typing<'a> {
    /// The whole field.
    pub text: &'a str,
    /// The caret, in UTF-16 code units (what a native edit counts in).
    pub caret: usize,
}

/// The suggestions for the field being typed in.
pub struct Suggester {
    contacts: Rc<Contacts>,
    shown: Vec<String>,
}

impl Suggester {
    pub fn new(contacts: Rc<Contacts>) -> Suggester {
        Suggester { contacts, shown: Vec::new() }
    }

    /// Recomputes the suggestions for `typing` and returns them, best first.
    pub fn update(&mut self, typing: &Typing) -> &[String] {
        let token = recipient_token(typing.text, char_index(typing.text, typing.caret));
        self.shown = self.contacts.matching(&token, typing.text, MAX_SUGGESTIONS).into_iter().map(|contact| contact.display.clone()).collect();
        &self.shown
    }

    /// Forgets the suggestions.
    pub fn clear(&mut self) {
        self.shown.clear();
    }

    /// The suggestion at `index`, if it is still on offer.
    pub fn get(&self, index: usize) -> Option<&str> {
        self.shown.get(index).map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.shown.is_empty()
    }
}

/// Accepting `suggestion` in `typing`: the field's new text and the caret's new
/// position in UTF-16 code units.
pub fn accept(typing: &Typing, suggestion: &str) -> (String, usize) {
    let (text, caret) = complete_recipient(typing.text, char_index(typing.text, typing.caret), suggestion);
    let utf16 = text.chars().take(caret).map(char::len_utf16).sum();
    (text, utf16)
}

/// The character index of the UTF-16 offset `units` in `text`.
fn char_index(text: &str, units: usize) -> usize {
    let mut seen = 0;
    for (index, c) in text.chars().enumerate() {
        if seen >= units {
            return index;
        }
        seen += c.len_utf16();
    }
    text.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use esmail::imap::MailHeader;

    fn header(from: &str) -> MailHeader {
        MailHeader { uid: 1, subject: String::new(), from: from.into(), to: String::new(), date: String::new(), message_id: String::new(), flags: Vec::new() }
    }

    fn suggester() -> Suggester {
        let headers = [header("Alice <alice@example.com>"), header("Albert <albert@example.org>")];
        Suggester::new(Rc::new(Contacts::from_headers(headers.iter(), [])))
    }

    #[test]
    fn typing_a_prefix_offers_the_matching_contacts() {
        let mut suggester = suggester();
        let shown = suggester.update(&Typing { text: "al", caret: 2 });
        assert_eq!(shown.len(), 2);
        assert_eq!(suggester.get(0), Some("Alice <alice@example.com>"));
    }

    #[test]
    fn only_the_token_under_the_caret_is_matched() {
        let mut suggester = suggester();
        assert!(suggester.update(&Typing { text: "alice@example.com, ", caret: 20 }).is_empty());
        assert_eq!(suggester.update(&Typing { text: "alice@example.com, alb", caret: 23 }).len(), 1);
    }

    #[test]
    fn a_contact_already_in_the_field_is_not_offered_again() {
        let mut suggester = suggester();
        let shown = suggester.update(&Typing { text: "Alice <alice@example.com>, al", caret: 30 });
        assert_eq!(shown, ["Albert <albert@example.org>"]);
    }

    #[test]
    fn accepting_replaces_the_token_and_puts_the_caret_after_the_separator() {
        let (text, caret) = accept(&Typing { text: "a@x.com, al", caret: 11 }, "Alice <alice@example.com>");
        assert_eq!(text, "a@x.com, Alice <alice@example.com>, ");
        assert_eq!(caret, text.len());
    }

    #[test]
    fn the_caret_is_counted_in_utf16_units_even_after_an_astral_character() {
        assert_eq!(char_index("\u{1F600}ab", 2), 1);
        let (text, caret) = accept(&Typing { text: "\u{1F600}x, al", caret: 7 }, "Al <al@x.com>");
        assert_eq!(text.chars().take_while(|c| *c != 'A').count(), 4);
        assert_eq!(caret, text.encode_utf16().count());
    }
}
