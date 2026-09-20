//! Text selection over a [`TextRunTable`]: pure geometry, no layout.
//!
//! A selection is two carets, each "before character `ch` of run `run`".
//! Because runs are in document order, comparing carets compares positions in
//! the document, and a selection is the text between them. Everything here
//! takes points in the table's own space (document points).

use egui::{Pos2, Rect};

use crate::text_runs::{TextRun, TextRunTable};

/// A caret position: the boundary before character `ch` of run `run`
/// (`ch == char_count` is the boundary after the run's last character).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TextPos {
    /// Index into [`TextRunTable::runs`].
    pub run: usize,
    /// Character (not byte) offset within the run.
    pub ch: usize,
}

/// The text between two carets. `anchor` is where the gesture started and
/// `head` where it is now, so `head` may come before `anchor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    /// Where the selection started.
    pub anchor: TextPos,
    /// The end that moves.
    pub head: TextPos,
}

impl Selection {
    /// A selection covering nothing yet, at `pos`.
    pub fn caret(pos: TextPos) -> Self {
        Self { anchor: pos, head: pos }
    }

    /// `(first, last)` in document order.
    pub fn ordered(&self) -> (TextPos, TextPos) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// Whether nothing is selected.
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
}

/// How far apart two runs' vertical centres may be, as a fraction of the
/// shorter one's height, and still count as the same line.
const SAME_LINE: f32 = 0.5;

fn is_blank(run: &TextRun) -> bool {
    run.text.trim().is_empty()
}

/// `(vertical, horizontal)` distance from `p` to `rect`, zero inside it.
fn distance(rect: &Rect, p: Pos2) -> (f32, f32) {
    let dy = (rect.min.y - p.y).max(p.y - rect.max.y).max(0.0);
    let dx = (rect.min.x - p.x).max(p.x - rect.max.x).max(0.0);
    (dy, dx)
}

fn same_line(a: &TextRun, b: &TextRun) -> bool {
    let tolerance = a.rect.height().min(b.rect.height()) * SAME_LINE;
    (a.rect.center().y - b.rect.center().y).abs() < tolerance
}

/// Characters are grouped so a double-click on "Taux," picks "Taux" and a
/// click on the comma picks the punctuation.
fn char_class(c: char) -> u8 {
    if c.is_alphanumeric() || c == '_' {
        0
    } else if c.is_whitespace() {
        1
    } else {
        2
    }
}

impl TextRunTable {
    /// Whether `p` is over a piece of text (for the I-beam cursor).
    pub fn is_text_at(&self, p: Pos2) -> bool {
        self.runs.iter().any(|r| !is_blank(r) && r.rect.contains(p))
    }

    /// The caret position nearest to `p`. Like a browser, a point in the
    /// margin or between lines snaps to the closest text (same line first),
    /// so a drag can wander off the text without losing the selection.
    pub fn pos_at(&self, p: Pos2) -> Option<TextPos> {
        let mut best: Option<(usize, (f32, f32))> = None;
        for (i, run) in self.runs.iter().enumerate() {
            if is_blank(run) {
                continue;
            }
            let d = distance(&run.rect, p);
            let better = match &best {
                None => true,
                Some((_, b)) => d.0.total_cmp(&b.0).then(d.1.total_cmp(&b.1)).is_lt(),
            };
            if better {
                best = Some((i, d));
                if d == (0.0, 0.0) {
                    break;
                }
            }
        }
        let (i, _) = best?;
        let run = &self.runs[i];
        let x = p.x - run.rect.min.x;
        let ch = match run.offsets.iter().position(|&o| o > x) {
            None => run.char_count(),
            Some(0) => 0,
            Some(k) => {
                // `x` falls in slot k-1: nearer to its start or its end.
                let (lo, hi) = (run.offsets[k - 1], run.offsets[k]);
                if x - lo < hi - x { k - 1 } else { k }
            }
        };
        Some(TextPos { run: i, ch })
    }

    /// The word under `p`, for a double-click.
    pub fn word_at(&self, p: Pos2) -> Option<Selection> {
        let pos = self.pos_at(p)?;
        let run = &self.runs[pos.run];
        let chars: Vec<char> = run.text.chars().collect();
        // The character the caret sits on, or the last one if it is at the end.
        let i = pos.ch.min(chars.len().saturating_sub(1));
        let class = char_class(chars[i]);
        let mut from = i;
        while from > 0 && char_class(chars[from - 1]) == class {
            from -= 1;
        }
        let mut to = i + 1;
        while to < chars.len() && char_class(chars[to]) == class {
            to += 1;
        }
        Some(Selection {
            anchor: TextPos { run: pos.run, ch: from },
            head: TextPos { run: pos.run, ch: to },
        })
    }

    /// The whole block (paragraph, table cell, list item) under `p`, for a
    /// triple-click.
    pub fn block_at(&self, p: Pos2) -> Option<Selection> {
        let pos = self.pos_at(p)?;
        let block = self.runs[pos.run].block;
        let mut first = pos.run;
        while first > 0 && self.runs[first - 1].block == block {
            first -= 1;
        }
        let mut last = pos.run;
        while last + 1 < self.runs.len() && self.runs[last + 1].block == block {
            last += 1;
        }
        Some(Selection {
            anchor: TextPos { run: first, ch: 0 },
            head: TextPos { run: last, ch: self.runs[last].char_count() },
        })
    }

    /// Everything, or `None` if there is no text.
    pub fn select_all(&self) -> Option<Selection> {
        let first = self.runs.iter().position(|r| !is_blank(r))?;
        let last = self.runs.iter().rposition(|r| !is_blank(r))?;
        Some(Selection {
            anchor: TextPos { run: first, ch: 0 },
            head: TextPos { run: last, ch: self.runs[last].char_count() },
        })
    }

    /// Boxes to paint as the highlight, in document points: one per line,
    /// with the spaces between selected words filled in.
    pub fn selection_rects(&self, sel: &Selection) -> Vec<Rect> {
        let (start, end) = sel.ordered();
        let mut out: Vec<Rect> = Vec::new();
        for i in start.run..=end.run.min(self.runs.len().saturating_sub(1)) {
            let run = &self.runs[i];
            let from = if i == start.run { start.ch } else { 0 };
            let to = if i == end.run { end.ch } else { run.char_count() };
            if from >= to {
                continue;
            }
            let rect = Rect::from_min_max(
                egui::pos2(run.rect.min.x + run.offsets[from], run.rect.min.y),
                egui::pos2(run.rect.min.x + run.offsets[to.min(run.char_count())], run.rect.max.y),
            );
            let extends_last = out.last().is_some_and(|last| {
                (last.center().y - rect.center().y).abs() < rect.height() * SAME_LINE
                    && (rect.min.x - last.max.x).abs() < 2.0
            });
            if extends_last {
                let last = out.last_mut().unwrap();
                *last = last.union(rect);
            } else if !is_blank(run) {
                out.push(rect);
            }
            // A blank run (a space) that does not touch the previous
            // highlight is collapsed whitespace litehtml left lying around,
            // not a visible gap: it gets no box.
        }
        out
    }

    /// The selected text, as a copy should read: words joined by spaces,
    /// hard breaks (paragraph, list item, table row, `<br>`) as newlines,
    /// a blank line between separate paragraphs, and wrapped lines joined
    /// back up.
    pub fn selection_text(&self, sel: &Selection) -> String {
        let (start, end) = sel.ordered();
        let mut out = String::new();
        let mut prev: Option<&TextRun> = None;
        let mut saw_space = false;
        for i in start.run..=end.run.min(self.runs.len().saturating_sub(1)) {
            let run = &self.runs[i];
            let from = if i == start.run { start.ch } else { 0 };
            let to = if i == end.run { end.ch } else { run.char_count() };
            if from >= to {
                continue;
            }
            let piece: String = run.text.chars().skip(from).take(to - from).collect();
            if piece.trim().is_empty() {
                saw_space = true;
                continue;
            }
            if let Some(prev) = prev {
                out.push_str(separator(prev, run, saw_space));
            }
            // Leading/trailing spaces inside a piece are a selection edge
            // falling on whitespace; the separator already decided.
            out.push_str(piece.trim());
            prev = Some(run);
            saw_space = piece.ends_with(' ');
        }
        out
    }
}

/// What goes between two consecutive words of a copy.
fn separator(prev: &TextRun, next: &TextRun, saw_space: bool) -> &'static str {
    if same_line(prev, next) {
        // Adjacent words with a space run between them (or a visible gap, as
        // between two table cells) are separate words.
        return if saw_space || next.rect.min.x - prev.rect.max.x > 1.0 { " " } else { "" };
    }
    if next.block != prev.block {
        // A new paragraph/cell/item. A vertical gap bigger than half a line
        // is the margin between paragraphs.
        let margin = next.rect.min.y - prev.rect.max.y > next.rect.height() * 0.5;
        return if margin || next.breaks_before >= 2 { "\n\n" } else { "\n" };
    }
    // Same block, new line: the text wrapped unless a `<br>` (or a newline in
    // preformatted text) forced the break.
    match next.breaks_before {
        0 => " ",
        1 => "\n",
        _ => "\n\n",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text_runs::TextRunTable;
    use litehtml::pixbuf::PixbufContainer;
    use litehtml::Document;

    fn table_for(html: &str, width: f32) -> TextRunTable {
        let mut container = PixbufContainer::new_with_scale(width as u32, 600, 1.0);
        let measure = container.text_measure_fn();
        let mut doc = Document::from_html(html, &mut container, None, None).unwrap();
        let _ = doc.render(width);
        TextRunTable::collect(&doc, &measure)
    }

    /// The text `select_all` would copy.
    fn copied(html: &str, width: f32) -> String {
        let t = table_for(html, width);
        let sel = t.select_all().expect("some text");
        t.selection_text(&sel)
    }

    fn run_of<'a>(t: &'a TextRunTable, text: &str) -> (usize, &'a TextRun) {
        t.runs.iter().enumerate().find(|(_, r)| r.text == text).unwrap_or_else(|| panic!("no run {text:?}"))
    }

    // ── hit testing ─────────────────────────────────────────────────────

    #[test]
    fn a_point_on_a_word_lands_between_the_right_characters() {
        let t = table_for("<body><p>Hello world</p></body>", 300.0);
        let (i, hello) = run_of(&t, "Hello");
        // Just inside the left edge: before 'H'; just inside the right: after 'o'.
        assert_eq!(t.pos_at(hello.rect.left_center() + egui::vec2(0.5, 0.0)), Some(TextPos { run: i, ch: 0 }));
        assert_eq!(t.pos_at(hello.rect.right_center() - egui::vec2(0.5, 0.0)), Some(TextPos { run: i, ch: 5 }));
        // The middle of the second character's slot picks the boundary nearer to it.
        let mid_of_e = hello.rect.min.x + (hello.offsets[1] + hello.offsets[2]) / 2.0;
        let just_left = egui::pos2(mid_of_e - 0.5, hello.rect.center().y);
        let just_right = egui::pos2(mid_of_e + 0.5, hello.rect.center().y);
        assert_eq!(t.pos_at(just_left), Some(TextPos { run: i, ch: 1 }));
        assert_eq!(t.pos_at(just_right), Some(TextPos { run: i, ch: 2 }));
    }

    #[test]
    fn a_point_off_the_text_snaps_to_the_nearest_line_then_the_nearest_word() {
        let t = table_for("<body><p>first line</p><div style=\"height:100px\"></div><p>far below</p></body>", 300.0);
        let (first, w) = run_of(&t, "first");
        // Far to the right of the first line, level with it: end of that line.
        let (line_end, _) = run_of(&t, "line");
        let pos = t.pos_at(egui::pos2(290.0, w.rect.center().y)).unwrap();
        assert_eq!(pos, TextPos { run: line_end, ch: 4 });
        // In the left margin, level with it: start of the line.
        assert_eq!(t.pos_at(egui::pos2(0.0, w.rect.center().y)), Some(TextPos { run: first, ch: 0 }));
        // Above the page: the nearest word of the first line (horizontally).
        assert_eq!(t.pos_at(egui::pos2(100.0, -50.0)).unwrap().run, line_end);
        assert_eq!(t.pos_at(egui::pos2(0.0, -50.0)), Some(TextPos { run: first, ch: 0 }));
        // Below the page: end of the last line.
        let (below, _) = run_of(&t, "below");
        assert_eq!(t.pos_at(egui::pos2(100.0, 5000.0)), Some(TextPos { run: below, ch: 5 }));
    }

    #[test]
    fn is_text_at_is_true_only_over_words() {
        let t = table_for("<body><p>Hello</p></body>", 300.0);
        let (_, hello) = run_of(&t, "Hello");
        assert!(t.is_text_at(hello.rect.center()));
        assert!(!t.is_text_at(egui::pos2(290.0, hello.rect.center().y)));
    }

    #[test]
    fn an_empty_document_has_nothing_to_hit() {
        let t = table_for("<body></body>", 300.0);
        assert_eq!(t.pos_at(egui::pos2(1.0, 1.0)), None);
        assert_eq!(t.select_all(), None);
    }

    // ── word / block / all ──────────────────────────────────────────────

    #[test]
    fn double_click_selects_a_word_without_its_punctuation() {
        let t = table_for("<body><p>Taux, immobilier</p></body>", 300.0);
        let (_, run) = run_of(&t, "Taux,");
        let sel = t.word_at(run.rect.left_center() + egui::vec2(3.0, 0.0)).unwrap();
        assert_eq!(t.selection_text(&sel), "Taux");
        // On the comma itself, the comma.
        let on_comma = egui::pos2(run.rect.max.x - 1.0, run.rect.center().y);
        assert_eq!(t.selection_text(&t.word_at(on_comma).unwrap()), ",");
    }

    #[test]
    fn triple_click_selects_the_whole_paragraph_and_only_that() {
        let t = table_for("<body><p>one two three</p><p>next para</p></body>", 300.0);
        let (_, two) = run_of(&t, "two");
        let sel = t.block_at(two.rect.center()).unwrap();
        assert_eq!(t.selection_text(&sel), "one two three");
    }

    #[test]
    fn select_all_covers_every_paragraph() {
        assert_eq!(copied("<body><p>one</p><p>two</p></body>", 300.0), "one\n\ntwo");
    }

    // ── highlight rectangles ────────────────────────────────────────────

    #[test]
    fn a_selection_within_one_word_highlights_exactly_those_characters() {
        let t = table_for("<body><p>Hello</p></body>", 300.0);
        let (i, hello) = run_of(&t, "Hello");
        let sel = Selection { anchor: TextPos { run: i, ch: 1 }, head: TextPos { run: i, ch: 3 } };
        let rects = t.selection_rects(&sel);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].min.x, hello.rect.min.x + hello.offsets[1]);
        assert_eq!(rects[0].max.x, hello.rect.min.x + hello.offsets[3]);
        assert_eq!((rects[0].min.y, rects[0].max.y), (hello.rect.min.y, hello.rect.max.y));
    }

    #[test]
    fn a_line_of_words_is_one_continuous_highlight() {
        let t = table_for("<body><p>alpha beta gamma</p></body>", 300.0);
        let rects = t.selection_rects(&t.select_all().unwrap());
        assert_eq!(rects.len(), 1, "the spaces between words must be filled: {rects:?}");
        let (_, alpha) = run_of(&t, "alpha");
        let (_, gamma) = run_of(&t, "gamma");
        assert_eq!(rects[0].min.x, alpha.rect.min.x);
        assert_eq!(rects[0].max.x, gamma.rect.max.x);
    }

    #[test]
    fn two_paragraphs_highlight_as_two_boxes() {
        let t = table_for("<body><p>one</p><p>two</p></body>", 300.0);
        assert_eq!(t.selection_rects(&t.select_all().unwrap()).len(), 2);
    }

    #[test]
    fn collapsed_whitespace_between_blocks_is_not_highlighted() {
        // Whitespace between tags becomes runs at the left margin; they must
        // not show up as stray blobs when everything is selected.
        let t = table_for("<body>\n<div>\n<p>one</p>\n</div>\n\n<p>two</p>\n</body>", 300.0);
        for r in t.selection_rects(&t.select_all().unwrap()) {
            let (_, one) = run_of(&t, "one");
            assert!(r.min.x >= one.rect.min.x - 0.5, "stray highlight at {r:?}");
        }
    }

    // ── copied text ─────────────────────────────────────────────────────

    #[test]
    fn words_in_a_sentence_are_joined_by_single_spaces() {
        assert_eq!(copied("<body><p>Hello   big\n <b>bold</b> world.</p></body>", 400.0), "Hello big bold world.");
    }

    #[test]
    fn paragraphs_are_separated_by_a_blank_line() {
        assert_eq!(copied("<body><p>First para.</p><p>Second para.</p></body>", 400.0), "First para.\n\nSecond para.");
    }

    #[test]
    fn list_items_are_one_per_line() {
        assert_eq!(copied("<body><ul><li>one</li><li>two</li><li>three</li></ul></body>", 400.0), "one\ntwo\nthree");
    }

    #[test]
    fn a_line_break_inside_a_paragraph_is_kept() {
        assert_eq!(copied("<body><p>line one<br>line two</p></body>", 400.0), "line one\nline two");
    }

    #[test]
    fn text_that_merely_wrapped_is_joined_back_into_one_line() {
        let text = "The quick brown fox jumps over the lazy dog and keeps running far away";
        let html = format!("<body><p>{text}</p></body>");
        let t = table_for(&html, 120.0);
        // Make sure the test really wraps.
        let ys: std::collections::BTreeSet<i32> = t.runs.iter().map(|r| r.rect.min.y as i32).collect();
        assert!(ys.len() >= 3, "expected the paragraph to wrap, got lines at {ys:?}");
        assert_eq!(copied(&html, 120.0), text);
    }

    #[test]
    fn table_cells_read_across_then_down() {
        let html = "<body><table><tr><td>a1</td><td>b1</td></tr><tr><td>a2</td><td>b2</td></tr></table></body>";
        assert_eq!(copied(html, 400.0), "a1 b1\na2 b2");
    }

    #[test]
    fn nested_layout_tables_read_cell_by_cell_without_gluing_words_together() {
        let html = "<body><table><tr><td><table><tr><td>Rentrée</td></tr><tr><td><b>2026</b></td></tr></table></td>\
                    <td>Je découvre</td></tr></table></body>";
        assert_eq!(copied(html, 500.0), "Rentrée\n2026\nJe découvre");
    }

    #[test]
    fn cells_side_by_side_on_one_line_are_separated_by_a_space() {
        let html = "<body><table><tr><td>Rentrée</td><td>2026</td></tr></table></body>";
        assert_eq!(copied(html, 500.0), "Rentrée 2026");
    }

    #[test]
    fn preformatted_text_keeps_its_lines_and_blank_lines() {
        assert_eq!(copied("<body><pre>one\ntwo\n\nfour</pre></body>", 400.0), "one\ntwo\n\nfour");
    }

    #[test]
    fn several_br_in_a_row_make_a_blank_line() {
        assert_eq!(copied("<body><p>one<br><br>two</p></body>", 400.0), "one\n\ntwo");
    }

    #[test]
    fn a_partial_selection_copies_from_the_middle_of_one_word_to_the_middle_of_another() {
        let t = table_for("<body><p>alpha beta gamma</p></body>", 300.0);
        let (a, _) = run_of(&t, "alpha");
        let (g, _) = run_of(&t, "gamma");
        let sel = Selection { anchor: TextPos { run: a, ch: 2 }, head: TextPos { run: g, ch: 3 } };
        assert_eq!(t.selection_text(&sel), "pha beta gam");
        // Dragging backwards is the same selection.
        let back = Selection { anchor: sel.head, head: sel.anchor };
        assert_eq!(t.selection_text(&back), "pha beta gam");
        assert_eq!(t.selection_rects(&back), t.selection_rects(&sel));
    }

    #[test]
    fn an_empty_selection_copies_nothing() {
        let t = table_for("<body><p>abc</p></body>", 300.0);
        let sel = Selection::caret(TextPos { run: 0, ch: 1 });
        assert!(sel.is_empty());
        assert_eq!(t.selection_text(&sel), "");
        assert!(t.selection_rects(&sel).is_empty());
    }
}
