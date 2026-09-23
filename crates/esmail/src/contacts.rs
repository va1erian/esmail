//! Recipient autocomplete: the addresses a compose window can suggest, and
//! the pure text edits accepting one makes. Split out of `compose.rs` (which
//! keeps the `ComposeState` derivation) to keep both modules under the
//! 500-line bar; the split changes no behavior.
//!
//! There is no persistent address book yet (see #62): [`Contacts`] is rebuilt
//! from the message headers the app has in memory, so it knows the currently
//! loaded mailbox and any open search results, plus the accounts' own
//! addresses. Feeding it from the `messages` cache instead is the follow-up
//! that would make it a real address book; the UI would not change.

use crate::compose::split_addresses;
use crate::imap::MailHeader;

/// One recipient autocomplete suggestion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contact {
    /// Lowercased bare address, the key matching and dedupe use.
    pub address: String,
    /// What accepting this suggestion inserts, e.g.
    /// `"Alice <alice@example.com>"` (or the bare address when the header had
    /// no display name).
    pub display: String,
}

/// The addresses a compose window can suggest as recipients.
#[derive(Debug, Clone, Default)]
pub struct Contacts {
    contacts: Vec<Contact>,
}

impl Contacts {
    /// Collect suggestions from message headers (both their `From` and `To`)
    /// and the accounts' own addresses, deduped by address.
    pub fn from_headers<'a>(
        headers: impl IntoIterator<Item = &'a MailHeader>,
        own_addresses: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        let mut contacts = Self::default();
        for address in own_addresses {
            if let Some(contact) = contact_from_field(address) {
                contacts.add(contact);
            }
        }
        for header in headers {
            for entry in split_addresses(&header.from).into_iter().chain(split_addresses(&header.to)) {
                if let Some(contact) = contact_from_field(&entry) {
                    contacts.add(contact);
                }
            }
        }
        contacts
    }

    fn add(&mut self, contact: Contact) {
        if let Some(existing) = self.contacts.iter_mut().find(|c| c.address == contact.address) {
            // A later header may carry a display name for an address first
            // seen bare; prefer the named form.
            if existing.display.eq_ignore_ascii_case(&existing.address) {
                existing.display = contact.display;
            }
            return;
        }
        self.contacts.push(contact);
    }

    /// Up to `limit` suggestions matching `query` (case-insensitive substring
    /// of the address or display name), excluding addresses already in
    /// `field`. Addresses that start with `query` sort first, so typing "al"
    /// offers `alice@…` before someone whose name merely contains "al".
    pub fn matching(&self, query: &str, field: &str, limit: usize) -> Vec<&Contact> {
        let query = query.trim().to_ascii_lowercase();
        if query.is_empty() {
            return Vec::new();
        }
        let taken = addresses_in(field);
        let mut matches: Vec<&Contact> = self
            .contacts
            .iter()
            .filter(|c| !taken.iter().any(|a| a == &c.address))
            .filter(|c| c.address.contains(&query) || c.display.to_ascii_lowercase().contains(&query))
            .collect();
        matches.sort_by_key(|c| !c.address.starts_with(&query));
        matches.truncate(limit);
        matches
    }
}

/// The [`Contact`] an address field entry denotes, or `None` when it has no
/// usable address. Handles both `"Name <user@host>"` and `"user@host"`.
fn contact_from_field(field: &str) -> Option<Contact> {
    let field = field.trim();
    let address = address_of(field)?;
    Some(Contact { address, display: field.to_string() })
}

/// Every address already written into a recipient field, lowercased.
fn addresses_in(field: &str) -> Vec<String> {
    split_addresses(field).iter().filter_map(|entry| contact_from_field(entry)).map(|c| c.address).collect()
}

/// The bare address out of `"Name <user@host>"` / `"user@host"`, lowercased.
/// `imap.rs` parses `From` the same way, but that helper is private there and
/// this module's scope is the recipient fields, so the parse is kept local
/// rather than widening `imap.rs`'s API for it.
fn address_of(field: &str) -> Option<String> {
    let field = field.trim();
    let addr = match (field.rfind('<'), field.rfind('>')) {
        (Some(open), Some(close)) if open < close => &field[open + 1..close],
        _ => field,
    };
    let addr = addr.trim();
    let (local, domain) = addr.split_once('@')?;
    (!local.is_empty() && !domain.is_empty()).then(|| addr.to_ascii_lowercase())
}

/// The comma-separated recipient token the caret at character index `cursor`
/// sits in, trimmed -- what an autocomplete query matches against. Empty when
/// the caret is in the whitespace after a comma.
pub fn recipient_token(text: &str, cursor: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let (raw_start, raw_end) = token_bounds(&chars, cursor);
    let mut start = raw_start;
    while start < raw_end && chars[start].is_whitespace() {
        start += 1;
    }
    let mut end = raw_end;
    while end > start && chars[end - 1].is_whitespace() {
        end -= 1;
    }
    chars[start..end].iter().collect()
}

/// Replace the recipient token the caret at `cursor` is in with `insertion`,
/// normalising the `", "` that follows it. Returns the new text and the
/// character index the caret should move to -- just past the separator, ready
/// for the next recipient.
pub fn complete_recipient(text: &str, cursor: usize, insertion: &str) -> (String, usize) {
    let chars: Vec<char> = text.chars().collect();
    let (start, end) = token_bounds(&chars, cursor);

    let mut out: String = chars[..start].iter().collect();
    if start > 0 {
        out.push(' ');
    }
    out.push_str(insertion);
    out.push_str(", ");
    if end < chars.len() {
        let rest: String = chars[end + 1..].iter().collect();
        out.push_str(rest.trim_start());
    }
    let new_cursor = start + usize::from(start > 0) + insertion.chars().count() + 2;
    (out, new_cursor)
}

/// The raw `[start, end)` character bounds of the comma-separated token the
/// caret at `cursor` is in, before whitespace trimming.
fn token_bounds(chars: &[char], cursor: usize) -> (usize, usize) {
    let cursor = cursor.min(chars.len());
    let start = chars[..cursor].iter().rposition(|&c| c == ',').map_or(0, |i| i + 1);
    let end = chars[cursor..].iter().position(|&c| c == ',').map_or(chars.len(), |i| cursor + i);
    (start, end)
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

    fn contacts() -> Contacts {
        let mut first = original();
        first.from = "Alice <alice@example.com>".to_string();
        first.to = "Bob <bob@example.com>, carol@example.com".to_string();
        Contacts::from_headers([&first], ["me@example.com"])
    }

    #[test]
    fn contacts_collect_from_and_to_and_own_addresses_deduped() {
        let contacts = contacts();
        let displays: Vec<&str> = contacts.matching("example.com", "", 10).iter().map(|c| c.display.as_str()).collect();
        assert_eq!(
            displays,
            vec!["me@example.com", "Alice <alice@example.com>", "Bob <bob@example.com>", "carol@example.com"]
        );
    }

    #[test]
    fn matching_is_case_insensitive_and_prefers_address_prefixes() {
        let contacts = contacts();
        let names: Vec<&str> = contacts.matching("AL", "", 10).iter().map(|c| c.display.as_str()).collect();
        assert_eq!(names, vec!["Alice <alice@example.com>"]);
    }

    #[test]
    fn matching_skips_addresses_already_in_the_field() {
        let contacts = contacts();
        let names: Vec<&str> = contacts
            .matching("example.com", "Alice <alice@example.com>", 10)
            .iter()
            .map(|c| c.address.as_str())
            .collect();
        assert!(!names.contains(&"alice@example.com"));
        assert!(names.contains(&"bob@example.com"));
    }

    #[test]
    fn matching_returns_nothing_for_an_empty_query() {
        assert!(contacts().matching("  ", "", 10).is_empty());
    }

    #[test]
    fn a_bare_address_prefers_a_later_named_form() {
        let mut first = original();
        first.from = "alice@example.com".to_string();
        let mut second = original();
        second.from = "Alice <alice@example.com>".to_string();
        let contacts = Contacts::from_headers([&first, &second], []);
        let names: Vec<&str> = contacts.matching("alice", "", 10).iter().map(|c| c.display.as_str()).collect();
        assert_eq!(names, vec!["Alice <alice@example.com>"]);
    }

    #[test]
    fn recipient_token_reads_the_token_under_the_caret() {
        assert_eq!(recipient_token("Alice <a@x>, bo", 17), "bo");
        assert_eq!(recipient_token("Alice <a@x>, bo", 5), "Alice <a@x>");
        assert_eq!(recipient_token("a@x,  ", 6), "");
    }

    #[test]
    fn completing_the_last_token_appends_a_separator_and_moves_the_caret_past_it() {
        let (text, cursor) = complete_recipient("al", 2, "Alice <alice@example.com>");
        assert_eq!(text, "Alice <alice@example.com>, ");
        assert_eq!(cursor, text.chars().count());
    }

    #[test]
    fn completing_a_middle_token_keeps_the_rest() {
        let (text, _) = complete_recipient("a@x.com, bo, c@y.com", 10, "Bob <bob@example.com>");
        assert_eq!(text, "a@x.com, Bob <bob@example.com>, c@y.com");
    }
}
