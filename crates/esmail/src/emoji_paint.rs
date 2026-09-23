//! The egui half of `emoji`: paint the artwork `esmail::emoji::prepare` found
//! over its placeholders. Lives in the binary so the shared library needs no
//! egui, and so a non-egui frontend can draw the artwork its own way from
//! `esmail::emoji::artwork_png`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use esmail::emoji::{PLACEHOLDER, artwork_png};

/// Draw `emoji` (from [`esmail::emoji::Prepared::emoji`]) over their
/// placeholders in `galley`, which the caller painted at `galley_pos`. A
/// placeholder that truncation cut off is skipped.
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
    let png: &[u8] = artwork_png(emoji)?;
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
    use esmail::emoji::prepare;

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
    fn every_emoji_prepared_has_artwork_that_decodes() {
        let ctx = egui::Context::default();
        for e in ["🦆", "👨‍👩‍👧", "🇫🇷", "👍🏽", "1️⃣", "✉️"] {
            assert!(load(&ctx, e).is_some(), "{e}");
        }
    }
}
