//! Pure geometry and view-state logic for the message list.
//!
//! Everything here is a plain function of numbers or of [`Selection`], kept
//! free of win32ui and `RowModel` types so it can be unit-tested without a
//! window, a message loop or any mail data. The widget (`message_list.rs`)
//! holds a [`ViewState`] and calls these functions from its input and paint
//! handlers.

use std::ops::Range;

use crate::selection::Selection;

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

    /// A recount after older rows were appended (`extend_rows`): only the row
    /// count changes; the scroll offset and the selection stay exactly as they
    /// were.
    pub fn recount(&mut self, new_len: usize) {
        self.len = new_len;
    }

    /// `count` rows were inserted at `at` (new mail, above what is shown). The
    /// selection follows its messages; a view scrolled away from the top also
    /// keeps showing the same messages, while one at the top shows the new rows.
    pub fn insert(&mut self, at: usize, count: usize, row_height: f32) {
        self.len += count;
        self.selection.insert(at, count);
        if self.scroll > 0.0 && at as f32 * row_height <= self.scroll {
            self.scroll += count as f32 * row_height;
        }
    }

    /// `count` rows were removed at `at` (moved or deleted). Rows that were
    /// scrolled off the top take their height off the offset so the messages
    /// in view stay put.
    pub fn remove(&mut self, at: usize, count: usize, row_height: f32) {
        let first_visible = (self.scroll / row_height) as usize;
        let above = (at + count).min(first_visible).saturating_sub(at);
        self.len = self.len.saturating_sub(count);
        self.selection.remove(at, count);
        self.scroll = (self.scroll - above as f32 * row_height).max(0.0);
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
    fn inserting_above_moves_the_selection_and_keeps_the_messages_in_view() {
        let mut view = ViewState { len: 100, scroll: 350.0, selection: selection(&[3, 7], Some(3), Some(7)) };
        view.insert(0, 2, 50.0);
        assert_eq!(view.len, 102);
        assert_eq!(view.scroll, 450.0);
        assert_eq!(view.selection, selection(&[5, 9], Some(5), Some(9)));
    }

    #[test]
    fn inserting_while_at_the_top_shows_the_new_rows() {
        let mut view = ViewState { len: 10, scroll: 0.0, selection: selection(&[0], Some(0), Some(0)) };
        view.insert(0, 3, 50.0);
        assert_eq!(view.scroll, 0.0);
        assert_eq!(view.selection.selected, vec![3]);
    }

    #[test]
    fn removing_rows_shifts_the_rest_and_drops_removed_selection() {
        let mut view = ViewState { len: 10, scroll: 0.0, selection: selection(&[2, 3, 6], Some(2), Some(6)) };
        view.remove(2, 2, 50.0);
        assert_eq!(view.len, 8);
        assert_eq!(view.selection, selection(&[4], None, Some(4)));
    }

    #[test]
    fn removing_rows_above_the_view_keeps_the_messages_in_view() {
        let mut view = ViewState { len: 100, scroll: 500.0, selection: Selection::new() };
        view.remove(0, 4, 50.0);
        assert_eq!(view.scroll, 300.0);
        view.remove(50, 4, 50.0);
        assert_eq!(view.scroll, 300.0);
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
