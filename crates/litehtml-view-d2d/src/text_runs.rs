//! The text-run table: where every piece of text in a laid-out document is.
//!
//! A litehtml `Document` cannot outlive one worker call (it borrows the
//! container), so anything that needs the document's text after that — text
//! selection, hover cursors, copy — works from this table instead. It is
//! built once per draw pass, while the `Document` is alive, and shipped to the
//! UI thread with the frame; from then on hit-testing, highlighting and copying
//! are plain geometry over [`TextRun`]s, with no layout and no `unsafe`.
//!
//! Coordinates are document points (device-independent pixels): the space
//! `Document::render` was laid out in, which is the display list's own space.

use litehtml::{Document, Element, FontHandle};

use crate::geom::Rect;
use crate::list::FontKey;

/// Guards against pathological nesting; real mail is far shallower.
const MAX_DEPTH: usize = 512;

/// Text laid out shorter than this (document points) is not readable and is
/// left out. Marketing mail hides its "preheader" (preview text) with
/// `font-size:1px`/`max-height:0`-style tricks rather than `display:none`, and
/// litehtml lays those words out as a few-point box in the corner of the page;
/// selecting or copying them would be selecting text nobody can see.
const MIN_RUN_HEIGHT: f32 = 4.0;

/// One text leaf of the document. litehtml splits text into one element per
/// word (plus separate whitespace elements), so a run is usually a word.
#[derive(Debug, Clone, PartialEq)]
pub struct TextRun {
    /// Bounding box in document points.
    pub rect: Rect,
    /// The text as litehtml reports it, with every whitespace character
    /// (newline, tab, no-break space) normalised to a plain space.
    pub text: String,
    /// `chars + 1` x positions, in document points from `rect.left`: where
    /// the boundary before each character falls, and finally the run's end.
    /// Monotonically non-decreasing; the last one is `rect.width()`.
    ///
    /// These are measured left-to-right, so they line up with the glyphs for
    /// left-to-right text; right-to-left runs need DirectWrite's own
    /// hit-testing (`Layout::hit_test_point`) for accurate per-character
    /// boundaries. The widget uses that where a font is available.
    pub offsets: Vec<f32>,
    /// The box of the nearest block-level ancestor (the paragraph, table cell
    /// or list item this text lives in). Runs of the same block compare equal;
    /// a change means a hard break (paragraph, cell, list item) rather than
    /// the text merely wrapping.
    pub block: Rect,
    /// How many forced line breaks (`<br>`, or a newline in `<pre>` text)
    /// sit between the previous run and this one. Within one block, a run on a
    /// new line with none of these is just the text wrapping.
    pub breaks_before: u8,
    /// The font this run was laid out with, into the frame's font table, so
    /// the UI thread can rebuild a DirectWrite layout for accurate hit-testing
    /// and selection boxes.
    pub font: FontKey,
}

impl TextRun {
    /// Number of characters (not bytes) in the run.
    pub fn char_count(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }
}

/// Every text run of one laid-out document, in document (reading) order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TextRunTable {
    /// The runs, first to last.
    pub runs: Vec<TextRun>,
}

impl TextRunTable {
    /// A hash of the runs' text in order, ignoring where they are: equal
    /// before and after a re-layout, different for a different message.
    pub fn text_signature(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for r in &self.runs {
            r.text.hash(&mut h);
        }
        h.finish()
    }

    /// Walk `doc` (which must have been laid out) and record its text.
    ///
    /// `measure` returns the width of `text` in the given font, in document
    /// points; `font_key` maps a litehtml font handle to the frame's font
    /// index. Both are captured before the `Document` takes its mutable borrow
    /// of the container, so they do not borrow it during collection.
    pub fn collect(
        doc: &Document<'_>,
        measure: &dyn Fn(&str, FontHandle) -> f32,
        font_key: &dyn Fn(FontHandle) -> FontKey,
    ) -> Self {
        let mut runs = Vec::new();
        let Some(root) = doc.root() else {
            return Self { runs };
        };
        // Explicit stack, children pushed in reverse, so the pop order is
        // document order without recursing (nesting can be deep).
        let mut stack: Vec<(Element<'_>, usize)> = vec![(root, 0)];
        let mut breaks = 0u8;
        while let Some((el, depth)) = stack.pop() {
            if el.is_text() {
                match run_for(&el, measure, font_key) {
                    Some(mut run) => {
                        // Breaks are counted up to the next word: a blank run
                        // (a space) in between must not swallow them.
                        if !run.text.trim().is_empty() {
                            // (Zero-size leaves such as `<head>` come before
                            // the first word; there is nothing to break from.)
                            let pending = std::mem::take(&mut breaks);
                            run.breaks_before = if runs.is_empty() { 0 } else { pending };
                        }
                        runs.push(run);
                    }
                    // A newline in `<pre>` text is a zero-width text element.
                    None if el.get_text().contains('\n') && el.placement().width <= 0.0 => {
                        breaks = breaks.saturating_add(1);
                    }
                    None => {}
                }
                continue;
            }
            // `<br>` is an element with no children, no inline boxes and
            // (unlike `<img>`, `<hr>` or an empty `<div>`) no size at all.
            if el.children_count() == 0 && el.inline_boxes_count() == 0 {
                let p = el.placement();
                if p.width <= 0.0 && p.height <= 0.0 {
                    breaks = breaks.saturating_add(1);
                }
            }
            if depth >= MAX_DEPTH {
                continue;
            }
            for i in (0..el.children_count()).rev() {
                if let Some(child) = el.child_at(i) {
                    stack.push((child, depth + 1));
                }
            }
        }
        Self { runs }
    }
}

/// The nearest ancestor that is not an inline element (inline elements have
/// per-line boxes; blocks do not).
fn block_rect(el: &Element<'_>) -> Rect {
    let mut current = el.parent();
    while let Some(parent) = current {
        if parent.inline_boxes_count() == 0 {
            let p = parent.placement();
            return Rect::from_min_size(p.x, p.y, p.width, p.height);
        }
        current = parent.parent();
    }
    let p = el.placement();
    Rect::from_min_size(p.x, p.y, p.width, p.height)
}

fn run_for(
    el: &Element<'_>,
    measure: &dyn Fn(&str, FontHandle) -> f32,
    font_key: &dyn Fn(FontHandle) -> FontKey,
) -> Option<TextRun> {
    // litehtml reports the raw source text: a run between two tags can be "\n"
    // or "\u{a0}". Layout collapses those to one space, and so does a copy, so
    // store them as that (one char to one char, so `offsets` still lines up).
    let text: String = el.get_text().chars().map(|c| if c.is_whitespace() { ' ' } else { c }).collect();
    if text.is_empty() {
        return None;
    }
    let p = el.placement();
    // Text that was not laid out (`display: none`, collapsed whitespace) has
    // no box of its own; see `MIN_RUN_HEIGHT` for text hidden by shrinking it.
    if p.width <= 0.0 || p.height < MIN_RUN_HEIGHT {
        return None;
    }
    let font = match el.font() {
        FontHandle(0) => el.parent().map_or(FontHandle(0), |parent| parent.font()),
        f => f,
    };
    let key = font_key(font);
    let chars: Vec<char> = text.chars().collect();
    let mut offsets = Vec::with_capacity(chars.len() + 1);
    offsets.push(0.0);
    let mut prefix = String::with_capacity(text.len());
    for c in &chars[..chars.len() - 1] {
        prefix.push(*c);
        // Shaping can make a prefix measure a hair wider than the whole, and
        // must never go backwards.
        let x = measure(&prefix, font).clamp(*offsets.last().unwrap(), p.width);
        offsets.push(x);
    }
    offsets.push(p.width);
    Some(TextRun {
        rect: Rect::from_min_size(p.x, p.y, p.width, p.height),
        text,
        offsets,
        block: block_rect(el),
        breaks_before: 0,
        font: key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use xui_win32::d2d::TextSystem;

    fn table_for(html: &str, width: f32) -> std::sync::Arc<TextRunTable> {
        let mut engine = crate::engine::Engine::new(TextSystem::new().unwrap());
        engine.draw_pass(html, width).expect("parses");
        engine.runs.clone()
    }

    fn joined(t: &TextRunTable) -> String {
        t.runs.iter().map(|r| r.text.as_str()).collect()
    }

    #[test]
    fn runs_come_out_in_reading_order_with_words_and_spaces() {
        let t = table_for("<body><p>Hello big <b>world</b></p><ul><li>one</li><li>two</li></ul></body>", 300.0);
        assert_eq!(joined(&t), "Hello big worldonetwo");
        assert_eq!(t.runs[0].text, "Hello");
        // Words on one line share a baseline and advance left to right.
        let (hello, big) = (&t.runs[0], &t.runs[2]);
        assert_eq!(hello.rect.top, big.rect.top);
        assert!(big.rect.left > hello.rect.right);
        // The list items are on later lines than the paragraph.
        let one = t.runs.iter().find(|r| r.text == "one").unwrap();
        let two = t.runs.iter().find(|r| r.text == "two").unwrap();
        assert!(one.rect.top > hello.rect.bottom - 1.0);
        assert!(two.rect.top > one.rect.top);
    }

    #[test]
    fn text_that_is_not_displayed_is_left_out() {
        let t = table_for(
            r#"<body><p>shown</p><div style="display:none">hidden</div><span style="display:none">also hidden</span></body>"#,
            300.0,
        );
        assert_eq!(joined(&t), "shown");
    }

    #[test]
    fn runs_know_which_block_they_belong_to() {
        let t = table_for(
            "<body><p>Hello <b>big</b> world</p><p>next paragraph</p></body>",
            300.0,
        );
        let blocks: Vec<_> = t.runs.iter().filter(|r| !r.text.trim().is_empty()).map(|r| (r.text.as_str(), r.block)).collect();
        // Inline <b> does not start a new block; the second <p> does.
        assert_eq!(blocks[0].1, blocks[1].1);
        assert_eq!(blocks[1].1, blocks[2].1);
        assert_ne!(blocks[2].1, blocks[3].1);
        assert_eq!(blocks[3].1, blocks[4].1);
    }

    #[test]
    fn forced_line_breaks_are_counted_on_the_word_after_them() {
        let t = table_for("<body><p>x<br>y <br><br>z w</p><pre>a\nb\n\nc</pre></body>", 300.0);
        let breaks = |w: &str| t.runs.iter().find(|r| r.text == w).unwrap().breaks_before;
        assert_eq!((breaks("x"), breaks("y"), breaks("z"), breaks("w")), (0, 1, 2, 0));
        assert_eq!((breaks("a"), breaks("b"), breaks("c")), (0, 1, 2));
    }

    #[test]
    fn an_empty_span_or_an_image_is_not_a_line_break() {
        let t = table_for("<body><p>x<span></span>y <img width=10 height=10>z<hr>w</p></body>", 300.0);
        assert!(t.runs.iter().all(|r| r.breaks_before == 0), "{:?}", t.runs.iter().map(|r| (&r.text, r.breaks_before)).collect::<Vec<_>>());
    }

    #[test]
    fn whitespace_between_tags_becomes_a_plain_space() {
        let t = table_for("<body><p>a</p>\n\t<p>b&nbsp;c</p></body>", 300.0);
        assert!(t.runs.iter().all(|r| !r.text.contains(['\n', '\t', '\u{a0}'])), "{:?}", t.runs);
        assert!(joined(&t).contains("b c"), "{:?}", joined(&t));
    }

    #[test]
    fn text_shrunk_to_nothing_is_left_out() {
        let t = table_for(
            r#"<body><div style="font-size:1px;line-height:1px;max-height:0;overflow:hidden">preheader</div><p>shown</p></body>"#,
            300.0,
        );
        assert_eq!(joined(&t), "shown");
    }

    #[test]
    fn offsets_have_one_boundary_per_character_and_end_at_the_run_width() {
        let t = table_for("<body><p>Wörld café 日本</p></body>", 300.0);
        assert!(!t.runs.is_empty());
        for r in &t.runs {
            assert_eq!(r.offsets.len(), r.text.chars().count() + 1, "{:?}", r.text);
            assert_eq!(r.char_count(), r.text.chars().count());
            assert_eq!(r.offsets[0], 0.0);
            assert_eq!(*r.offsets.last().unwrap(), r.rect.width());
            assert!(r.offsets.windows(2).all(|w| w[0] <= w[1]), "{:?}: {:?}", r.text, r.offsets);
        }
    }

    #[test]
    fn a_wider_character_gets_a_wider_slot() {
        // Proportional font: "i" is much narrower than "W", so the offsets
        // must come from measuring, not from dividing the width evenly.
        let t = table_for("<body><p>iiW</p></body>", 300.0);
        let r = &t.runs[0];
        assert_eq!(r.text, "iiW");
        let (i_w, cap_w) = (r.offsets[1] - r.offsets[0], r.offsets[3] - r.offsets[2]);
        assert!(cap_w > i_w * 1.5, "W ({cap_w}) should be far wider than i ({i_w})");
    }
}
