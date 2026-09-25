//! Colour emoji in single-line labels (the message list): segmentation and
//! placeholder layout, frontend-agnostic.
//!
//! A frontend whose text stack draws only monochrome outline fonts uses this to
//! swap each emoji for a fixed-width placeholder, lay the text out as usual,
//! then paint the Twemoji artwork over the placeholder's slot in the laid-out
//! row — without giving up the frontend's own text measuring and ellipsis
//! truncation. The native win32 frontend draws colour emoji directly with
//! DirectWrite and does not need it.
//!
//! 1. [`prepare`] replaces every emoji in a string with one fixed-width
//!    placeholder character and remembers where they were;
//! 2. the caller lays that text out as usual;
//! 3. the caller paints each emoji's artwork over the placeholder's slot.
//!
//! An emoji is a whole grapheme cluster (`👨‍👩‍👧`, `🇫🇷`, `👍🏽`, `1️⃣` are one each),
//! looked up as a unit; a cluster the artwork set does not know stays text.
//! This module only decides *what* is an emoji and where it was; it needs no
//! GUI toolkit, and [`artwork_png`] hands a frontend the raw artwork to draw.

use unicode_segmentation::UnicodeSegmentation;

use twemoji_assets::png::PngTwemojiAsset;

/// Laid out in place of each emoji: an em space, blank and about one em wide.
pub const PLACEHOLDER: char = '\u{2003}';

/// Text with its emoji swapped for placeholders. See [`prepare`].
#[derive(Debug, PartialEq, Eq)]
pub struct Prepared {
    /// The text to lay out: `original` with each emoji replaced by
    /// [`PLACEHOLDER`].
    pub text: String,
    /// Where each replaced emoji was, as `(char index in text, emoji)`. One
    /// glyph of the laid-out row per `char`, so the index is also the glyph's.
    pub emoji: Vec<(usize, String)>,
}

/// Swap every emoji in `original` for a placeholder. Plain ASCII, the common
/// case, is returned untouched without any lookups.
pub fn prepare(original: &str) -> Prepared {
    if original.is_ascii() {
        return Prepared { text: original.to_owned(), emoji: Vec::new() };
    }
    let mut text = String::with_capacity(original.len());
    let mut emoji = Vec::new();
    let mut chars = 0;
    for cluster in original.graphemes(true) {
        if is_emoji(cluster) {
            emoji.push((chars, cluster.to_owned()));
            text.push(PLACEHOLDER);
            chars += 1;
        } else {
            text.push_str(cluster);
            chars += cluster.chars().count();
        }
    }
    Prepared { text, emoji }
}

fn is_emoji(cluster: &str) -> bool {
    !cluster.is_ascii() && artwork(cluster).is_some() && has_emoji_presentation(cluster)
}

/// The artwork for `cluster`, trying it as written and then without its
/// variation selectors (the artwork set files some emoji, keycaps for one,
/// under the shorter form).
fn artwork(cluster: &str) -> Option<&'static PngTwemojiAsset> {
    PngTwemojiAsset::from_emoji(cluster).or_else(|| {
        let bare: String = cluster.chars().filter(|&c| c != VARIATION_SELECTOR_16).collect();
        PngTwemojiAsset::from_emoji(&bare)
    })
}

/// The raw PNG bytes for `cluster`, for a frontend that draws the artwork
/// itself (the example in `artwork`'s doc). `None` when the artwork set does
/// not know the cluster.
pub fn artwork_png(cluster: &str) -> Option<&'static [u8]> {
    Some(artwork(cluster)?)
}

const VARIATION_SELECTOR_16: char = '\u{FE0F}';
const COMBINING_KEYCAP: char = '\u{20E3}';

/// Whether `cluster` should be drawn as a picture rather than as text. The
/// artwork set also knows symbols that are ordinary text far more often than
/// emoji (`©`, `®`, `™`, `★`, `↔`), and turning every "© 2026" in a mail into
/// an image would be wrong. Those only count as emoji when the sender asked
/// for it with U+FE0F, or the character is emoji by default.
fn has_emoji_presentation(cluster: &str) -> bool {
    cluster.contains(VARIATION_SELECTOR_16)
        || cluster.contains(COMBINING_KEYCAP)
        || cluster.chars().next().is_some_and(is_default_emoji)
}

/// Characters drawn as emoji unless a U+FE0E asks otherwise (Unicode's
/// `Emoji_Presentation`), in the ranges where the answer is not simply
/// "everything": the astral planes are emoji, the BMP mostly is not.
fn is_default_emoji(c: char) -> bool {
    matches!(c,
        '\u{1F000}'..='\u{1FAFF}'
        | '\u{231A}'..='\u{231B}' | '\u{23E9}'..='\u{23EC}' | '\u{23F0}' | '\u{23F3}'
        | '\u{25FD}'..='\u{25FE}' | '\u{2614}'..='\u{2615}' | '\u{2648}'..='\u{2653}'
        | '\u{267F}' | '\u{2693}' | '\u{26A1}' | '\u{26AA}'..='\u{26AB}'
        | '\u{26BD}'..='\u{26BE}' | '\u{26C4}'..='\u{26C5}' | '\u{26CE}' | '\u{26D4}'
        | '\u{26EA}' | '\u{26F2}'..='\u{26F3}' | '\u{26F5}' | '\u{26FA}' | '\u{26FD}'
        | '\u{2705}' | '\u{270A}'..='\u{270B}' | '\u{2728}' | '\u{274C}' | '\u{274E}'
        | '\u{2753}'..='\u{2755}' | '\u{2757}' | '\u{2795}'..='\u{2797}' | '\u{27B0}'
        | '\u{27BF}' | '\u{2B1B}'..='\u{2B1C}' | '\u{2B50}' | '\u{2B55}')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emoji_of(p: &Prepared) -> Vec<(usize, &str)> {
        p.emoji.iter().map(|(i, e)| (*i, e.as_str())).collect()
    }

    #[test]
    fn the_envelope_used_as_the_account_icon_has_artwork() {
        // `main.rs` draws this as each account's icon; without artwork it would
        // quietly fall back to a plain dot.
        assert!(artwork("\u{2709}\u{fe0f}").is_some());
    }

    #[test]
    fn ascii_text_is_returned_unchanged() {
        let p = prepare("Hello, world 1 # *");
        assert_eq!(p.text, "Hello, world 1 # *");
        assert!(p.emoji.is_empty());
    }

    #[test]
    fn non_emoji_unicode_text_is_returned_unchanged() {
        let p = prepare("Réception d'un virement — 50,25 €");
        assert_eq!(p.text, "Réception d'un virement — 50,25 €");
        assert!(p.emoji.is_empty());
    }

    #[test]
    fn an_emoji_becomes_one_placeholder_and_is_remembered_by_char_index() {
        let p = prepare("Hi 🦆!");
        assert_eq!(p.text, format!("Hi {PLACEHOLDER}!"));
        assert_eq!(emoji_of(&p), vec![(3, "🦆")]);
    }

    #[test]
    fn indices_count_chars_not_bytes_after_multibyte_text() {
        let p = prepare("é🦆é🦆");
        assert_eq!(emoji_of(&p), vec![(1, "🦆"), (3, "🦆")]);
    }

    #[test]
    fn multi_codepoint_emoji_are_a_single_placeholder() {
        // ZWJ family, flag (two regional indicators), skin-toned thumb,
        // keycap, and a text-style symbol given emoji presentation.
        for e in ["👨‍👩‍👧", "🇫🇷", "👍🏽", "1️⃣", "✉️"] {
            let p = prepare(&format!("a{e}b"));
            assert_eq!(p.text, format!("a{PLACEHOLDER}b"), "{e}");
            assert_eq!(emoji_of(&p), vec![(1, e)], "{e}");
        }
    }

    #[test]
    fn adjacent_emoji_each_get_a_placeholder() {
        let p = prepare("🦆🦆");
        assert_eq!(p.text.chars().count(), 2);
        assert_eq!(emoji_of(&p), vec![(0, "🦆"), (1, "🦆")]);
    }

    #[test]
    fn symbols_that_are_usually_text_only_become_emoji_when_asked_or_by_default() {
        // Ordinary text: never an image.
        for s in ["©", "®", "™", "★", "↔"] {
            assert!(prepare(s).emoji.is_empty(), "{s} should stay text");
        }
        // Explicit emoji presentation (U+FE0F), or emoji by default.
        for s in ["©\u{FE0F}", "\u{2764}\u{FE0F}", "✅", "⭐", "⚡"] {
            assert_eq!(prepare(s).emoji.len(), 1, "{s} should be an emoji");
        }
    }

    #[test]
    fn a_bare_text_symbol_without_emoji_presentation_stays_text() {
        // "©" alone is text; only "©️" (with U+FE0F) is the emoji.
        assert!(prepare("©").emoji.is_empty());
    }

    #[test]
    fn artwork_png_returns_decodable_png_bytes() {
        let png = artwork_png("🦆").expect("the duck has artwork");
        assert!(image::load_from_memory_with_format(png, image::ImageFormat::Png).is_ok());
        assert!(artwork_png("not an emoji").is_none());
    }
}
