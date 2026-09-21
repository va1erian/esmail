//! Colour emoji in single-line labels (the message list).
//!
//! egui draws text from monochrome outline fonts only, so an emoji comes out
//! as a black-and-white glyph, and only as many of them as its bundled font
//! happens to cover. This module draws the real coloured artwork instead,
//! without giving up egui's own text layout (measuring, ellipsis truncation):
//!
//! 1. [`prepare`] replaces every emoji in a string with one fixed-width
//!    placeholder character and remembers where they were;
//! 2. the caller lays that text out as usual;
//! 3. [`paint`] draws each emoji's image over the placeholder's slot in the
//!    laid-out row.
//!
//! An emoji is a whole grapheme cluster (`👨‍👩‍👧`, `🇫🇷`, `👍🏽`, `1️⃣` are one each),
//! looked up as a unit; a cluster the artwork set does not know stays text.
//! The images are decoded on first use and kept as egui textures.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use twemoji_assets::png::PngTwemojiAsset;
use unicode_segmentation::UnicodeSegmentation;

/// Laid out in place of each emoji: an em space, blank and about one em wide.
const PLACEHOLDER: char = '\u{2003}';

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

/// Draw `emoji` (from [`Prepared::emoji`]) over their placeholders in
/// `galley`, which the caller painted at `galley_pos`. A placeholder that
/// truncation cut off is skipped.
pub fn paint(ctx: &egui::Context, painter: &egui::Painter, galley: &egui::Galley, galley_pos: egui::Pos2, emoji: &[(usize, String)]) {
    let Some(row) = galley.rows.first() else { return };
    for (index, cluster) in emoji {
        let Some(glyph) = placeholder_glyph(&row.row, *index) else { continue };
        let Some(texture) = texture(ctx, cluster) else { continue };
        // A square as wide as the slot the placeholder reserved, but no
        // taller than the line, centred on it.
        let side = glyph.advance_width.min(row.row.size.y);
        let center = galley_pos + row.pos.to_vec2() + egui::vec2(glyph.pos.x + glyph.advance_width / 2.0, row.row.size.y / 2.0);
        let rect = egui::Rect::from_center_size(center, egui::vec2(side, side));
        paint_texture(painter, &texture, rect);
    }
}

/// Draw one emoji's artwork filling `rect`, for a place that is not a line of
/// text (an icon). Returns whether it was drawn: `false` for an emoji the
/// artwork set does not know, so the caller can fall back to something else.
pub fn paint_in_rect(ctx: &egui::Context, painter: &egui::Painter, rect: egui::Rect, emoji: &str) -> bool {
    let Some(texture) = texture(ctx, emoji) else { return false };
    paint_texture(painter, &texture, rect);
    true
}

fn paint_texture(painter: &egui::Painter, texture: &egui::TextureHandle, rect: egui::Rect) {
    let whole = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
    painter.image(texture.id(), rect, whole, egui::Color32::WHITE);
}

/// The glyph at `index` of `row`, provided it is still a placeholder. When a
/// line is truncated, egui ends it with an ellipsis glyph that takes the index
/// of the first character it cut off -- which can be an emoji's placeholder,
/// and must not have that emoji painted over it.
fn placeholder_glyph(row: &egui::epaint::text::Row, index: usize) -> Option<&egui::epaint::text::Glyph> {
    row.glyphs.get(index).filter(|glyph| glyph.chr == PLACEHOLDER)
}

/// Decoded emoji textures, by emoji. `None` records a cluster whose image
/// failed to decode, so it is not retried every frame.
#[derive(Clone, Default)]
struct Cache(Arc<Mutex<HashMap<String, Option<egui::TextureHandle>>>>);

fn texture(ctx: &egui::Context, emoji: &str) -> Option<egui::TextureHandle> {
    let cache = ctx.data_mut(|data| data.get_temp_mut_or_default::<Cache>(egui::Id::new("esmail-emoji-cache")).clone());
    // A poisoned lock only means another thread panicked mid-insert; the map
    // is still a valid cache, so keep using it.
    let mut map = cache.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    // `get` before `insert` rather than `entry`: this runs per emoji per
    // frame, and `entry` would allocate an owned key every time.
    if let Some(cached) = map.get(emoji) {
        return cached.clone();
    }
    let loaded = load(ctx, emoji);
    map.insert(emoji.to_owned(), loaded.clone());
    loaded
}

fn load(ctx: &egui::Context, emoji: &str) -> Option<egui::TextureHandle> {
    let png: &[u8] = artwork(emoji)?;
    let image = match image::load_from_memory_with_format(png, image::ImageFormat::Png) {
        Ok(image) => image.into_rgba8(),
        Err(e) => {
            log::warn!("could not decode the artwork for emoji {emoji}: {e}");
            return None;
        }
    };
    let size = [image.width() as usize, image.height() as usize];
    let pixels = egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw());
    Some(ctx.load_texture(format!("emoji-{emoji}"), pixels, egui::TextureOptions::LINEAR))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emoji_of(p: &Prepared) -> Vec<(usize, &str)> {
        p.emoji.iter().map(|(i, e)| (*i, e.as_str())).collect()
    }

    /// Lays `text` out on one line truncated at `width`, as `message_row` does.
    fn layout(ctx: &egui::Context, text: &str, width: f32) -> std::sync::Arc<egui::Galley> {
        let mut galley = None;
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), egui::FontId::proportional(13.0), egui::Color32::WHITE);
            job.wrap = egui::text::TextWrapping::truncate_at_width(width);
            galley = Some(ui.painter().layout_job(job));
        });
        // Dropping unapplied texture deltas trips a debug assertion.
        output.textures_delta.clear();
        galley.expect("the pass ran")
    }

    #[test]
    fn the_envelope_used_as_the_account_icon_has_artwork() {
        // `main.rs` draws this as each account's icon; without artwork it would
        // quietly fall back to a plain dot.
        assert!(artwork("\u{2709}\u{fe0f}").is_some());
    }

    #[test]
    fn a_truncation_ellipsis_is_never_taken_for_an_emoji_placeholder() {
        let ctx = egui::Context::default();
        let prepared = prepare("Lunch on Thursday at the new place 🍽️ and more");
        let &(index, _) = prepared.emoji.first().expect("one emoji");
        let (mut ellipsis_at_index, mut placeholder_at_index) = (false, false);
        // Every width from "nothing fits" to "everything fits": somewhere in
        // between the cut lands exactly on the placeholder.
        for width in 0..400 {
            let galley = layout(&ctx, &prepared.text, width as f32);
            let row = &galley.rows[0].row;
            match row.glyphs.get(index) {
                Some(g) if g.chr == PLACEHOLDER => placeholder_at_index = true,
                Some(_) => ellipsis_at_index = true,
                None => {}
            }
            let found = placeholder_glyph(row, index).is_some();
            assert_eq!(found, row.glyphs.get(index).is_some_and(|g| g.chr == PLACEHOLDER), "width {width}");
        }
        assert!(ellipsis_at_index, "no width put the ellipsis at the emoji's index; the test no longer tests anything");
        assert!(placeholder_at_index);
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
    fn every_emoji_prepared_has_artwork_that_decodes() {
        let ctx = egui::Context::default();
        for e in ["🦆", "👨‍👩‍👧", "🇫🇷", "👍🏽", "1️⃣", "✉️"] {
            assert!(load(&ctx, e).is_some(), "{e}");
        }
    }
}
