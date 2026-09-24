//! What a click on a link in the reading pane should do.
//!
//! A message is untrusted content, so only `http`, `https` and `mailto` links
//! leave the app, and only when they are well formed. The header block's own
//! attachment links use a scheme no message can usefully forge: the worst a
//! forged one does is ask where to save an attachment.

/// The scheme of the attachment links the header block carries.
pub const ATTACHMENT_SCHEME: &str = "esmail-attachment";

/// The longest link opened; longer ones are almost always an attack or a bug.
const MAX_LINK_BYTES: usize = 8192;

/// What to do with a clicked link.
#[derive(Debug, PartialEq, Eq)]
pub enum LinkAction {
    /// Open this address in the default browser.
    Browser(String),
    /// Start a message to `to`, with `subject` when the link names one.
    Mail {
        /// The address.
        to: String,
        /// The subject the link asks for.
        subject: Option<String>,
    },
    /// Save attachment number `n` of the open message.
    SaveAttachment(usize),
    /// Open attachment number `n` of the open message.
    OpenAttachment(usize),
    /// Save every attachment of the open message.
    SaveAllAttachments,
    /// Do nothing, and say why.
    Refuse(String),
}

/// The href of the "Save" link of attachment `n`.
pub fn save_href(n: usize) -> String {
    format!("{ATTACHMENT_SCHEME}:save/{n}")
}

/// The href of the "Open" link of attachment `n`.
pub fn open_href(n: usize) -> String {
    format!("{ATTACHMENT_SCHEME}:open/{n}")
}

/// The href of the "Save all" link.
pub fn save_all_href() -> String {
    format!("{ATTACHMENT_SCHEME}:save-all")
}

/// Decides what clicking `href` does.
pub fn classify(href: &str) -> LinkAction {
    let href = href.trim();
    if href.len() > MAX_LINK_BYTES || href.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return LinkAction::Refuse("Blocked a link that is malformed.".to_string());
    }
    let Some((scheme, rest)) = href.split_once(':') else {
        return LinkAction::Refuse("Blocked a link with no scheme.".to_string());
    };
    match scheme.to_ascii_lowercase().as_str() {
        "http" | "https" if rest.starts_with("//") && rest.len() > 2 => LinkAction::Browser(href.to_string()),
        "mailto" => mail(rest),
        ATTACHMENT_SCHEME => attachment(rest),
        other => LinkAction::Refuse(format!("Blocked a link that opens \"{other}:\"; only web and mail links are opened.")),
    }
}

fn mail(rest: &str) -> LinkAction {
    let (address, query) = rest.split_once('?').unwrap_or((rest, ""));
    let to = percent_decode(address);
    if !to.contains('@') {
        return LinkAction::Refuse("Blocked a mail link with no address.".to_string());
    }
    let subject = query
        .split('&')
        .find_map(|pair| pair.split_once('=').filter(|(name, _)| name.eq_ignore_ascii_case("subject")))
        .map(|(_, value)| percent_decode(value));
    LinkAction::Mail { to, subject }
}

fn attachment(rest: &str) -> LinkAction {
    let parsed = match rest.split_once('/') {
        Some(("save", n)) => n.parse().ok().map(LinkAction::SaveAttachment),
        Some(("open", n)) => n.parse().ok().map(LinkAction::OpenAttachment),
        None if rest == "save-all" => Some(LinkAction::SaveAllAttachments),
        _ => None,
    };
    parsed.unwrap_or_else(|| LinkAction::Refuse("Blocked a malformed attachment link.".to_string()))
}

/// Decodes `%XX` escapes. (`+` stays a plus: `mailto:` is not a form.)
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let escaped = (bytes[i] == b'%')
            .then(|| bytes.get(i + 1..i + 3))
            .flatten()
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escaped {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_links_open_in_the_browser() {
        assert_eq!(classify("https://example.com/a?b=c"), LinkAction::Browser("https://example.com/a?b=c".into()));
        assert_eq!(classify("HTTP://example.com"), LinkAction::Browser("HTTP://example.com".into()));
    }

    #[test]
    fn other_schemes_are_refused() {
        for href in ["javascript:alert(1)", "file:///c:/windows/system32/calc.exe", "ms-msdt:/id", "data:text/html,x", "ftp://x/y", "example.com"] {
            assert!(matches!(classify(href), LinkAction::Refuse(_)), "{href}");
        }
    }

    #[test]
    fn a_link_with_control_characters_or_spaces_is_refused() {
        assert!(matches!(classify("https://exa mple.com"), LinkAction::Refuse(_)));
        assert!(matches!(classify("https://example.com/\u{0}"), LinkAction::Refuse(_)));
        assert!(matches!(classify("https://"), LinkAction::Refuse(_)));
        assert!(matches!(classify(&format!("https://e.com/{}", "a".repeat(9000))), LinkAction::Refuse(_)));
    }

    #[test]
    fn mail_links_name_an_address_and_maybe_a_subject() {
        assert_eq!(classify("mailto:ann@example.com"), LinkAction::Mail { to: "ann@example.com".into(), subject: None });
        assert_eq!(
            classify("mailto:ann%40example.com?cc=x&Subject=Hello%20there"),
            LinkAction::Mail { to: "ann@example.com".into(), subject: Some("Hello there".into()) }
        );
        assert!(matches!(classify("mailto:nobody"), LinkAction::Refuse(_)));
    }

    #[test]
    fn attachment_links_round_trip() {
        assert_eq!(classify(&save_href(2)), LinkAction::SaveAttachment(2));
        assert_eq!(classify(&open_href(0)), LinkAction::OpenAttachment(0));
        assert_eq!(classify(&save_all_href()), LinkAction::SaveAllAttachments);
        assert!(matches!(classify("esmail-attachment:save/x"), LinkAction::Refuse(_)));
    }
}
