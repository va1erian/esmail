//! Row painting: the Direct2D + DirectWrite drawing of one message-list row.
//!
//! The layout mirrors `message_row` in `crates/esmail/src/main.rs` (same
//! paddings, sizes and ordering), but text is drawn by DirectWrite rather than
//! egui's painter. Two deliberate differences, both wins for the native
//! renderer: an unread sender is *really* bold (DirectWrite has weights; egui
//! fakes it by drawing the text twice), and colour emoji come from the system
//! emoji font through DirectWrite's glyph fallback — no Twemoji assets.

use win32ui::d2d::{D2dCanvas, Font, PointF, RectF, Stroke, TextSystem};
use win32ui::Theme;

use esmail::view_model::RowModel;

/// Horizontal padding inside a row.
const PAD_X: f32 = 10.0;
/// Vertical padding above and below the two text lines.
const PAD_Y: f32 = 6.0;
/// Width of the unread accent bar on the left edge.
const ACCENT: f32 = 3.0;
/// Gap between the sender and subject lines.
const LINE_GAP: f32 = 2.0;
/// Space kept between a line's text and whatever sits at its right end.
const RIGHT_GAP: f32 = 8.0;

/// Sender text size, matching egui.
const SENDER_SIZE: f32 = 14.5;
/// Subject text size, matching egui.
const SUBJECT_SIZE: f32 = 13.0;
/// Timestamp text size, matching egui.
const TIMESTAMP_SIZE: f32 = 12.0;

/// The ellipsis DirectWrite has no built-in trimming for, appended by
/// [`ellipsize`].
const ELLIPSIS: char = '\u{2026}';

/// The fonts a message-list row is drawn with, resolved once.
pub struct Fonts {
    /// Sender line, regular weight.
    pub sender: Font,
    /// Sender line, bold (unread rows).
    pub sender_bold: Font,
    /// Subject line.
    pub subject: Font,
    /// The right-aligned date and time.
    pub timestamp: Font,
    /// The fixed row height in device-independent pixels, from the two text
    /// lines plus padding. Constant for every row, which is what makes the
    /// list virtualizable.
    pub row_height: f32,
}

impl Fonts {
    /// Resolves the four fonts and computes the fixed row height.
    pub fn new(text: &TextSystem) -> win32ui::Result<Fonts> {
        let family = "Segoe UI, sans-serif";
        let sender = text.font(&win32ui::d2d::FontSpec::new(family, SENDER_SIZE))?;
        let sender_bold =
            text.font(&win32ui::d2d::FontSpec::new(family, SENDER_SIZE).weight(700))?;
        let subject = text.font(&win32ui::d2d::FontSpec::new(family, SUBJECT_SIZE))?;
        let timestamp = text.font(&win32ui::d2d::FontSpec::new(family, TIMESTAMP_SIZE))?;
        let row_height =
            PAD_Y * 2.0 + sender.metrics().line_height() + LINE_GAP + subject.metrics().line_height();
        Ok(Fonts {
            sender,
            sender_bold,
            subject,
            timestamp,
            row_height,
        })
    }
}

/// How a row is presented for one paint: its selection and hover state, and
/// whether the list as a whole has the keyboard focus (which picks the focused
/// vs unfocused selection fill).
#[derive(Clone, Copy, Debug, Default)]
pub struct RowVisual {
    /// The row is selected.
    pub selected: bool,
    /// The pointer is over the row.
    pub hovered: bool,
    /// The list has the keyboard focus.
    pub focused: bool,
}

/// Paints one row into `canvas`. `rect` is the row's rectangle in document
/// coordinates (device-independent pixels); the canvas is already translated
/// by the scroll offset.
pub fn paint_row(
    canvas: &mut D2dCanvas<'_>,
    row: &RowModel,
    rect: RectF,
    visual: RowVisual,
    fonts: &Fonts,
    theme: &Theme,
) {
    let unread = !row.seen;
    let subject_color = if row.seen { theme.text_secondary } else { theme.text };

    if visual.selected {
        let fill = if visual.focused {
            theme.selection
        } else {
            theme.selection_unfocused
        };
        canvas.fill_rect(rect, fill);
    } else if visual.hovered {
        canvas.fill_rect(rect, theme.hover);
    }

    if unread {
        canvas.fill_rect(
            RectF::new(rect.left, rect.top, rect.left + ACCENT, rect.bottom),
            theme.accent,
        );
    }

    let text_left = rect.left + ACCENT + PAD_X;
    let right = rect.right - PAD_X;
    let full_width = (rect.width() - ACCENT - PAD_X * 2.0).max(0.0);

    let star = row.flagged.then_some("\u{2605}");
    let (date, time) = match &row.local_date_time {
        Some((date, time)) => (Some(date.as_str()), Some(time.as_str())),
        None => (None, None),
    };

    let star_w = star.map_or(0.0, |s| fonts.sender.width(s));
    let time_w = time.map_or(0.0, |t| fonts.timestamp.width(t));
    let date_w = date.map_or(0.0, |d| fonts.timestamp.width(d));

    let reserved_time = if time.is_some() { time_w + RIGHT_GAP } else { 0.0 };
    let reserved_star = if star.is_some() { star_w + RIGHT_GAP } else { 0.0 };
    let sender_w = (full_width - reserved_time - reserved_star).max(0.0);
    let subject_w = (full_width - if date.is_some() { date_w + RIGHT_GAP } else { 0.0 }).max(0.0);

    let sender_font = if unread {
        &fonts.sender_bold
    } else {
        &fonts.sender
    };
    let sender_text = ellipsize(sender_font, &row.sender, sender_w);
    let sender_origin = PointF::new(text_left, rect.top + PAD_Y);
    let sender_line_h = fonts.sender.metrics().line_height();
    if let Ok(sender_layout) = sender_font.layout(&sender_text, f32::INFINITY) {
        canvas.draw_text(&sender_layout, sender_origin, theme.text);
        if let Some(time) = time {
            if let Ok(line) = fonts.timestamp.layout(time, f32::INFINITY) {
                let y = sender_origin.y + sender_line_h - line.height();
                canvas.draw_text(&line, PointF::new(right - line.width(), y), subject_color);
            }
        }
        if let Some(star) = star {
            if let Ok(line) = fonts.sender.layout(star, f32::INFINITY) {
                canvas.draw_text(
                    &line,
                    PointF::new(right - reserved_time - line.width(), sender_origin.y),
                    theme.warning,
                );
            }
        }
    }

    let subject_text = ellipsize(&fonts.subject, &row.subject, subject_w);
    let subject_origin = PointF::new(text_left, sender_origin.y + sender_line_h + LINE_GAP);
    let subject_line_h = fonts.subject.metrics().line_height();
    if let Ok(subject_layout) = fonts.subject.layout(&subject_text, f32::INFINITY) {
        canvas.draw_text(&subject_layout, subject_origin, subject_color);
        if let Some(date) = date {
            if let Ok(line) = fonts.timestamp.layout(date, f32::INFINITY) {
                let y = subject_origin.y + subject_line_h - line.height();
                canvas.draw_text(&line, PointF::new(right - line.width(), y), subject_color);
            }
        }
    }

    canvas.draw_line(
        PointF::new(rect.left, rect.bottom - 0.5),
        PointF::new(rect.right, rect.bottom - 0.5),
        theme.border,
        Stroke::solid(1.0),
    );
}

/// Truncates `text` to fit on one line within `max_width` device-independent
/// pixels, appending [`ELLIPSIS`] when it does not fit. A string that fits is
/// returned unchanged, so the common short-sender/subject case allocates
/// nothing.
fn ellipsize(font: &Font, text: &str, max_width: f32) -> String {
    if text.is_empty() {
        return String::new();
    }
    if max_width <= 0.0 || font.width(text) <= max_width {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let available = (max_width - font.width(&ELLIPSIS.to_string())).max(0.0);
    // Longest prefix that fits, by binary search on the char count.
    let mut lo = 0usize;
    let mut hi = chars.len();
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        let prefix: String = chars[..mid].iter().collect();
        if font.width(&prefix) <= available {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    // Never end on a combining mark, ZWJ or variation selector, which would
    // leave a grapheme cluster dangling.
    while lo > 0 && is_continuation(chars[lo - 1]) {
        lo -= 1;
    }
    let mut out = String::with_capacity(lo + ELLIPSIS.len_utf8());
    out.extend(&chars[..lo]);
    out.push(ELLIPSIS);
    out
}

/// Characters that continue a grapheme cluster rather than start one.
fn is_continuation(c: char) -> bool {
    matches!(
        c,
        '\u{0300}'..='\u{036F}' // combining diacritics
            | '\u{1AB0}'..='\u{1AFF}'
            | '\u{1DC0}'..='\u{1DFF}'
            | '\u{20D0}'..='\u{20FF}'
            | '\u{FE00}'..='\u{FE0F}' // variation selectors
            | '\u{FE20}'..='\u{FE2F}'
            | '\u{200D}' // zero-width joiner
            | '\u{1F3FB}'..='\u{1F3FF}' // emoji skin-tone modifiers
    )
}
