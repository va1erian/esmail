//! Pure selection, geometry and view-state logic for the message list.
//!
//! Everything here is a plain function of numbers or of [`Selection`], kept
//! free of win32ui and `RowModel` types so it can be unit-tested without a
//! window, a message loop or any mail data. The widget (`message_list.rs`)
//! holds a [`ViewState`] and calls these functions from its input and paint
//! handlers.

use std::ops::Range;

/// The rows that are selected, plus the anchor and focus that range selection
/// and keyboard movement need.
///
/// `selected` is always ascending and deduplicated. `anchor` is the fixed end
/// of a Shift range (set by a plain click or Ctrl+click); `focus` is the row
/// the keyboard moves and the one Enter/Space act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// Selected row indices, ascending.
    pub selected: Vec<usize>,
    /// The anchor of the last plain/Ctrl+click, for Shift ranges.
    pub anchor: Option<usize>,
    /// The focused row.
    pub focus: Option<usize>,
}

impl Default for Selection {
    fn default() -> Selection {
        Selection::new()
    }
}

impl Selection {
    /// An empty selection.
    pub fn new() -> Selection {
        Selection {
            selected: Vec::new(),
            anchor: None,
            focus: None,
        }
    }

    /// A plain click on `index`: select only it.
    pub fn click(&mut self, index: usize) {
        self.selected = vec![index];
        self.anchor = Some(index);
        self.focus = Some(index);
    }

    /// A Ctrl+click on `index`: toggle it, and move anchor/focus to it.
    pub fn ctrl_click(&mut self, index: usize) {
        match self.selected.binary_search(&index) {
            Ok(pos) => {
                self.selected.remove(pos);
            }
            Err(pos) => self.selected.insert(pos, index),
        }
        self.anchor = Some(index);
        self.focus = Some(index);
    }

    /// A Shift+click on `index`: extend the inclusive range from the anchor
    /// (or the focus, or `index` itself when there is nothing to extend from).
    /// The anchor is left unchanged, so a further Shift+click keeps extending
    /// from the same place.
    pub fn shift_click(&mut self, index: usize) {
        let anchor = self.anchor.or(self.focus).unwrap_or(index);
        self.selected = range(anchor, index);
        self.focus = Some(index);
    }

    /// A keyboard move to `index` (already clamped to the model).
    ///
    /// * plain: select only `index`;
    /// * Ctrl: move the focus without changing the selection;
    /// * Shift: extend the range from the anchor to `index`.
    ///
    /// Not yet reached from the widget: win32ui's `Custom::with_vscroll` scroll
    /// host consumes Up/Down/PageUp/PageDown/Home/End for scrolling before the
    /// widget sees them, so there is no key event left to move the focus with.
    /// Wired once that gap (reported in the PR) is fixed.
    #[allow(dead_code)]
    pub fn move_focus(&mut self, index: usize, ctrl: bool, shift: bool) {
        if ctrl {
            self.focus = Some(index);
        } else if shift {
            let anchor = self.anchor.or(self.focus).unwrap_or(index);
            self.selected = range(anchor, index);
            self.focus = Some(index);
        } else {
            self.click(index);
        }
    }

    /// Select every row of a `len`-row model.
    pub fn select_all(&mut self, len: usize) {
        self.selected = if len == 0 { Vec::new() } else { (0..len).collect() };
        self.anchor = Some(0);
        self.focus = Some(0);
    }

    /// Replace the selection from the app (`set_selection`), keeping only rows
    /// in bounds, ascending and deduplicated.
    pub fn replace(&mut self, rows: &[usize], len: usize) {
        let mut next: Vec<usize> = rows.iter().copied().filter(|&row| row < len).collect();
        next.sort_unstable();
        next.dedup();
        self.selected = next;
        self.anchor = self.selected.first().copied();
        self.focus = self.selected.first().copied();
    }

    /// Drop the selection and the focus.
    pub fn clear(&mut self) {
        self.selected.clear();
        self.anchor = None;
        self.focus = None;
    }

    /// Whether `index` is selected.
    pub fn contains(&self, index: usize) -> bool {
        self.selected.binary_search(&index).is_ok()
    }
}

/// The inclusive span between `a` and `b`, ascending. This is the index-space
/// equivalent of `esmail::view_model::select_range`, which does the same for
/// UIDs over a `MailHeader` list — the message list selects by index, so the
/// anchor/target are indices rather than UIDs.
fn range(a: usize, b: usize) -> Vec<usize> {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    (lo..=hi).collect()
}

/// The parts of a [`MessageList`](crate::MessageList)'s view that change
/// without any win32ui call: the row count, the scroll offset (in
/// device-independent pixels) and the selection.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewState {
    /// Number of rows.
    pub len: usize,
    /// Scroll offset from the top, in device-independent pixels.
    pub scroll: f32,
    /// The selection.
    pub selection: Selection,
}

impl ViewState {
    /// An empty view at the top.
    pub fn new() -> ViewState {
        ViewState {
            len: 0,
            scroll: 0.0,
            selection: Selection::new(),
        }
    }

    /// A full model replace (`set_rows`): a new mailbox, so the selection is
    /// dropped and the view returns to the top.
    pub fn set_rows(&mut self, len: usize) {
        self.len = len;
        self.scroll = 0.0;
        self.selection.clear();
    }

    /// A recount after rows were inserted (`rows_inserted`): only the row
    /// count changes; the scroll offset and the selection stay exactly as they
    /// were.
    pub fn recount(&mut self, new_len: usize) {
        self.len = new_len;
    }
}

impl Default for ViewState {
    fn default() -> ViewState {
        ViewState::new()
    }
}

/// The largest meaningful scroll offset for `len` fixed-height rows in a
/// `viewport`-tall window: the content height minus the window, or zero when
/// the content fits. Used to keep a mirrored offset in range.
pub fn max_scroll(row_height: f32, viewport: f32, len: usize) -> f32 {
    (len as f32 * row_height - viewport).max(0.0)
}

/// Clamps a scroll offset to `[0, max_scroll]`. A non-finite offset (NaN, or a
/// value from a host that overscrolled past the top) becomes zero, so callers
/// can trust the result to index the model.
pub fn clamp_scroll(scroll: f32, row_height: f32, viewport: f32, len: usize) -> f32 {
    if !scroll.is_finite() {
        return 0.0;
    }
    scroll.clamp(0.0, max_scroll(row_height, viewport, len))
}

/// How close to the last row (in rows) a scroll must get to count as near the end.
const NEAR_END_ROWS: f32 = 10.0;

/// Whether a viewport `viewport` tall, scrolled `scroll` down `len` rows of
/// `row_height` each, shows the last rows or is within [`NEAR_END_ROWS`] of them.
/// An empty model is always at its end.
pub fn is_near_end(scroll: f32, viewport: f32, row_height: f32, len: usize) -> bool {
    len as f32 * row_height - scroll - viewport <= NEAR_END_ROWS * row_height
}

/// The row indices that intersect a viewport `viewport` device-independent
/// pixels tall, scrolled `scroll` device-independent pixels down a model of
/// `len` fixed-height (`row_height`) rows. Empty only when there is nothing to
/// show; for a non-empty model the range always holds at least one row, even
/// for an offset at, above or beyond either end.
pub fn visible_range(scroll: f32, viewport: f32, row_height: f32, len: usize) -> Range<usize> {
    if len == 0 || !(viewport > 0.0) || !(row_height > 0.0) {
        return 0..0;
    }
    let scroll = clamp_scroll(scroll, row_height, viewport, len);
    let first = (scroll / row_height).floor() as usize;
    let last = ((scroll + viewport) / row_height).ceil() as usize;
    first..last.clamp(first + 1, len)
}

/// The row under a document y (device-independent pixels), or `None` when the
/// point is above the first row or past the last.
pub fn row_at(y: f32, row_height: f32, len: usize) -> Option<usize> {
    if y < 0.0 || row_height <= 0.0 {
        return None;
    }
    let index = (y / row_height) as usize;
    (index < len).then_some(index)
}

/// The smallest scroll offset that brings `row` fully into a viewport
/// `viewport` device-independent pixels tall: `ensure_visible`. Returns the
/// current `scroll` when the row is already fully visible.
pub fn scroll_for_row(row: usize, row_height: f32, viewport: f32, scroll: f32) -> f32 {
    let top = row as f32 * row_height;
    let bottom = top + row_height;
    if top < scroll {
        top
    } else if bottom > scroll + viewport {
        bottom - viewport
    } else {
        scroll
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn near_end_holds_within_ten_rows_of_the_last_one() {
        // 100 rows of 50 tall, viewport 500: the last row is at 4500..5000.
        assert!(!super::is_near_end(0.0, 500.0, 50.0, 100));
        assert!(!super::is_near_end(3999.0, 500.0, 50.0, 100));
        assert!(super::is_near_end(4000.0, 500.0, 50.0, 100));
        assert!(super::is_near_end(4500.0, 500.0, 50.0, 100));
        assert!(super::is_near_end(0.0, 500.0, 50.0, 0));
    }

    use super::*;

    fn selection(selected: &[usize], anchor: Option<usize>, focus: Option<usize>) -> Selection {
        Selection {
            selected: selected.to_vec(),
            anchor,
            focus,
        }
    }

    // ── Selection state machine ─────────────────────────────────────────────

    #[test]
    fn a_plain_click_selects_only_that_row() {
        let mut sel = selection(&[3, 5], Some(3), Some(5));
        sel.click(1);
        assert_eq!(sel.selected, vec![1]);
        assert_eq!(sel.anchor, Some(1));
        assert_eq!(sel.focus, Some(1));
    }

    #[test]
    fn ctrl_click_toggles_a_row_and_moves_the_focus() {
        let mut sel = selection(&[3, 5], Some(3), Some(5));
        sel.ctrl_click(7);
        assert_eq!(sel.selected, vec![3, 5, 7]);
        assert_eq!(sel.focus, Some(7));

        sel.ctrl_click(5);
        assert_eq!(sel.selected, vec![3, 7]);
        assert_eq!(sel.focus, Some(5));
    }

    #[test]
    fn shift_click_selects_the_span_from_the_anchor() {
        let mut sel = selection(&[3], Some(3), Some(3));
        sel.shift_click(7);
        assert_eq!(sel.selected, vec![3, 4, 5, 6, 7]);
        // The anchor is unchanged so a further Shift+click keeps extending.
        assert_eq!(sel.anchor, Some(3));
        sel.shift_click(5);
        assert_eq!(sel.selected, vec![3, 4, 5]);
        assert_eq!(sel.focus, Some(5));
    }

    #[test]
    fn shift_click_works_with_the_anchor_after_the_target() {
        let mut sel = selection(&[7], Some(7), Some(7));
        sel.shift_click(4);
        assert_eq!(sel.selected, vec![4, 5, 6, 7]);
    }

    #[test]
    fn shift_click_without_an_anchor_falls_back_to_the_focus() {
        let mut sel = selection(&[2, 9], None, Some(9));
        sel.shift_click(5);
        assert_eq!(sel.selected, vec![5, 6, 7, 8, 9]);
    }

    #[test]
    fn keyboard_move_selects_moves_or_extends() {
        let mut sel = selection(&[3], Some(3), Some(3));
        // Plain: select only.
        sel.move_focus(6, false, false);
        assert_eq!(sel.selected, vec![6]);
        assert_eq!(sel.focus, Some(6));
        // Ctrl: move focus, keep selection.
        sel.move_focus(9, true, false);
        assert_eq!(sel.selected, vec![6]);
        assert_eq!(sel.focus, Some(9));
        // Shift: extend from the anchor (still 6).
        sel.move_focus(4, false, true);
        assert_eq!(sel.selected, vec![4, 5, 6]);
        assert_eq!(sel.focus, Some(4));
    }

    #[test]
    fn select_all_keeps_an_empty_model_empty() {
        let mut sel = selection(&[1], Some(1), Some(1));
        sel.select_all(5);
        assert_eq!(sel.selected, vec![0, 1, 2, 3, 4]);

        sel.select_all(0);
        assert!(sel.selected.is_empty());
    }

    #[test]
    fn replace_drops_out_of_range_and_duplicate_rows() {
        let mut sel = Selection::new();
        sel.replace(&[9, 2, 2, 4], 5);
        assert_eq!(sel.selected, vec![2, 4]);
        assert_eq!(sel.focus, Some(2));
    }

    // ── visible-range computation ───────────────────────────────────────────

    const ROW: f32 = 48.0;

    #[test]
    fn visible_range_at_the_top() {
        assert_eq!(visible_range(0.0, 300.0, ROW, 100), 0..7);
    }

    #[test]
    fn visible_range_in_the_middle() {
        // scroll 1200 = row 25; a 300-dip viewport covers rows 25..=31.
        assert_eq!(visible_range(1200.0, 300.0, ROW, 100), 25..32);
    }

    #[test]
    fn visible_range_at_the_bottom_is_clamped() {
        let len = 100;
        let bottom = len as f32 * ROW - 300.0;
        let range = visible_range(bottom, 300.0, ROW, len);
        assert_eq!(range.end, len);
        assert!(range.start < len);
    }

    #[test]
    fn visible_range_is_empty_without_rows_or_viewport() {
        assert_eq!(visible_range(0.0, 0.0, ROW, 100), 0..0);
        assert_eq!(visible_range(50.0, 300.0, ROW, 0), 0..0);
    }

    #[test]
    fn visible_range_is_never_empty_for_a_nonempty_model() {
        let len = 100;
        for scroll in [
            -1_000.0,
            -0.5,
            0.0,
            12.0,
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::MAX,
            len as f32 * ROW,
            len as f32 * ROW + 500.0,
        ] {
            let range = visible_range(scroll, 300.0, ROW, len);
            assert!(
                !range.is_empty(),
                "scroll {scroll} produced an empty range {range:?}"
            );
            assert!(range.start < len && range.end <= len, "{range:?}");
        }
    }

    #[test]
    fn visible_range_at_or_above_the_top_shows_row_zero() {
        for scroll in [-1_000.0, -0.5, 0.0, f32::NAN, f32::NEG_INFINITY] {
            let range = visible_range(scroll, 300.0, ROW, 100);
            assert_eq!(range.start, 0, "scroll {scroll} hid row 0");
            assert!(range.contains(&0));
        }
    }

    #[test]
    fn visible_range_beyond_the_bottom_shows_the_last_row() {
        let len = 100;
        for scroll in [len as f32 * ROW, len as f32 * ROW + 1_000.0, f32::MAX] {
            let range = visible_range(scroll, 300.0, ROW, len);
            assert!(
                range.contains(&(len - 1)),
                "scroll {scroll} hid the last row"
            );
            assert_eq!(range.end, len);
        }
    }

    #[test]
    fn clamp_scroll_keeps_the_offset_in_range() {
        let len = 100;
        let max = len as f32 * ROW - 300.0;
        assert_eq!(clamp_scroll(-5.0, ROW, 300.0, len), 0.0);
        assert_eq!(clamp_scroll(f32::NAN, ROW, 300.0, len), 0.0);
        assert_eq!(clamp_scroll(120.0, ROW, 300.0, len), 120.0);
        assert_eq!(clamp_scroll(1e9, ROW, 300.0, len), max);
        // A model shorter than the viewport has nowhere to scroll.
        assert_eq!(clamp_scroll(1e9, ROW, 10_000.0, 3), 0.0);
    }

    // ── row_at ──────────────────────────────────────────────────────────────

    #[test]
    fn row_at_maps_a_y_to_its_row() {
        assert_eq!(row_at(0.0, ROW, 10), Some(0));
        assert_eq!(row_at(47.9, ROW, 10), Some(0));
        assert_eq!(row_at(48.0, ROW, 10), Some(1));
        assert_eq!(row_at(-1.0, ROW, 10), None);
        assert_eq!(row_at(480.0, ROW, 10), None);
    }

    // ── ensure_visible ──────────────────────────────────────────────────────

    #[test]
    fn scroll_for_row_leaves_a_visible_row_alone() {
        // Row 10 occupies 480..528; it is fully inside a 300-dip viewport at 450.
        assert_eq!(scroll_for_row(10, ROW, 300.0, 450.0), 450.0);
    }

    #[test]
    fn scroll_for_row_scrolls_up_when_the_row_is_above() {
        assert_eq!(scroll_for_row(3, ROW, 300.0, 450.0), 3.0 * ROW);
    }

    #[test]
    fn scroll_for_row_scrolls_down_when_the_row_is_below() {
        // Row 20 occupies 960..1008; a 300-dip viewport must start at 708.
        assert_eq!(scroll_for_row(20, ROW, 300.0, 0.0), 1008.0 - 300.0);
    }

    #[test]
    fn ensure_visible_of_the_first_row_scrolls_to_the_top() {
        assert_eq!(scroll_for_row(0, ROW, 300.0, 5_000.0), 0.0);
        assert_eq!(scroll_for_row(0, ROW, 300.0, 0.0), 0.0);
    }

    // ── rows_inserted ───────────────────────────────────────────────────────

    #[test]
    fn recount_after_insert_keeps_scroll_and_selection() {
        let mut view = ViewState {
            len: 100,
            scroll: 350.0,
            selection: selection(&[3, 7], Some(3), Some(7)),
        };
        view.recount(105);
        assert_eq!(view.len, 105);
        assert_eq!(view.scroll, 350.0);
        assert_eq!(view.selection.selected, vec![3, 7]);
        assert_eq!(view.selection.anchor, Some(3));
        assert_eq!(view.selection.focus, Some(7));
    }

    #[test]
    fn set_rows_resets_the_view_for_a_new_mailbox() {
        let mut view = ViewState {
            len: 100,
            scroll: 1234.0,
            selection: selection(&[3, 7], Some(3), Some(7)),
        };
        view.set_rows(40);
        assert_eq!(view.len, 40);
        assert_eq!(view.scroll, 0.0);
        assert!(view.selection.selected.is_empty());
    }
}
