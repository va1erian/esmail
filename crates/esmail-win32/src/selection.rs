//! The message list's selection: which rows are selected, the anchor and focus
//! that range selection and keyboard movement need, and where a navigation key
//! moves the focus. Plain data and functions, unit-tested without a window.

use xui_win32::Key;

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

    /// Rows `at..at + count` were inserted: every index at or after `at` moves
    /// down so the same messages stay selected and focused.
    pub fn insert(&mut self, at: usize, count: usize) {
        let shift = |row: usize| if row >= at { row + count } else { row };
        self.selected.iter_mut().for_each(|row| *row = shift(*row));
        self.anchor = self.anchor.map(shift);
        self.focus = self.focus.map(shift);
    }

    /// Rows `at..at + count` were removed: they leave the selection and every
    /// later index moves up. An anchor or focus that was removed is dropped.
    pub fn remove(&mut self, at: usize, count: usize) {
        let end = at + count;
        let shift = |row: usize| if row >= end { Some(row - count) } else if row < at { Some(row) } else { None };
        self.selected = self.selected.iter().filter_map(|&row| shift(row)).collect();
        self.anchor = self.anchor.and_then(shift);
        self.focus = self.focus.and_then(shift);
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

/// Where a navigation key moves the focus in a `len`-row list showing `page`
/// rows at a time, or `None` for any other key. With nothing focused Down and
/// PageDown start at the first row, Up and PageUp at the last.
pub fn nav_target(key: Key, focus: Option<usize>, len: usize, page: usize) -> Option<usize> {
    let last = len.checked_sub(1)?;
    let target = match key {
        Key::HOME => 0,
        Key::END => last,
        Key::UP => focus.map_or(last, |row| row.saturating_sub(1)),
        Key::DOWN => focus.map_or(0, |row| row + 1),
        Key::PAGE_UP => focus.map_or(last, |row| row.saturating_sub(page)),
        Key::PAGE_DOWN => focus.map_or(0, |row| row + page),
        _ => return None,
    };
    Some(target.min(last))
}

/// What a navigation key did, from [`navigate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Moved {
    /// The row the focus moved to (it may be where it already was, at an end).
    pub focus: usize,
    /// Whether the selection may have changed, so the app should hear of it.
    /// False for Ctrl+key, which only moves the focus.
    pub selection_changed: bool,
}

/// Applies a navigation key to `selection` in a `len`-row list showing `page`
/// rows at a time: Up/Down/PageUp/PageDown/Home/End move the focus, Shift
/// extends the range from the anchor, Ctrl moves the focus alone. Returns
/// `None` (and changes nothing) for any other key or an empty list.
pub fn navigate(selection: &mut Selection, key: Key, ctrl: bool, shift: bool, len: usize, page: usize) -> Option<Moved> {
    let focus = nav_target(key, selection.focus, len, page)?;
    selection.move_focus(focus, ctrl, shift);
    Some(Moved { focus, selection_changed: !ctrl })
}
/// The inclusive span between `a` and `b`, ascending. This is the index-space
/// equivalent of `esmail::view_model::select_range`, which does the same for
/// UIDs over a `MailHeader` list — the message list selects by index, so the
/// anchor/target are indices rather than UIDs.
fn range(a: usize, b: usize) -> Vec<usize> {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    (lo..=hi).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selection(selected: &[usize], anchor: Option<usize>, focus: Option<usize>) -> Selection {
        Selection { selected: selected.to_vec(), anchor, focus }
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

    #[test]
    fn navigation_keys_move_the_focus_and_stop_at_the_ends() {
        assert_eq!(nav_target(Key::DOWN, Some(3), 10, 5), Some(4));
        assert_eq!(nav_target(Key::DOWN, Some(9), 10, 5), Some(9));
        assert_eq!(nav_target(Key::UP, Some(0), 10, 5), Some(0));
        assert_eq!(nav_target(Key::PAGE_DOWN, Some(7), 10, 5), Some(9));
        assert_eq!(nav_target(Key::PAGE_UP, Some(7), 10, 5), Some(2));
        assert_eq!(nav_target(Key::HOME, Some(7), 10, 5), Some(0));
        assert_eq!(nav_target(Key::END, None, 10, 5), Some(9));
    }

    #[test]
    fn navigation_without_a_focus_starts_at_an_end_and_an_empty_list_has_no_target() {
        assert_eq!(nav_target(Key::DOWN, None, 10, 5), Some(0));
        assert_eq!(nav_target(Key::UP, None, 10, 5), Some(9));
        assert_eq!(nav_target(Key::DOWN, None, 0, 5), None);
        assert_eq!(nav_target(Key::A, Some(1), 10, 5), None);
    }

    fn navigated(sel: &mut Selection, key: Key, ctrl: bool, shift: bool) -> Option<Moved> {
        navigate(sel, key, ctrl, shift, 20, 5)
    }

    #[test]
    fn a_plain_navigation_key_selects_only_the_row_it_lands_on() {
        let mut sel = selection(&[3, 4], Some(3), Some(4));
        let moved = navigated(&mut sel, Key::DOWN, false, false);
        assert_eq!(moved, Some(Moved { focus: 5, selection_changed: true }));
        assert_eq!(sel, selection(&[5], Some(5), Some(5)));
    }

    #[test]
    fn shift_navigation_extends_the_range_and_can_shrink_it_again() {
        let mut sel = selection(&[6], Some(6), Some(6));
        navigated(&mut sel, Key::DOWN, false, true);
        navigated(&mut sel, Key::PAGE_DOWN, false, true);
        assert_eq!(sel.selected, (6..=12).collect::<Vec<_>>());
        assert_eq!(sel.anchor, Some(6));
        navigated(&mut sel, Key::UP, false, true);
        assert_eq!(sel.selected, (6..=11).collect::<Vec<_>>());
        navigated(&mut sel, Key::HOME, false, true);
        assert_eq!(sel.selected, (0..=6).collect::<Vec<_>>());
        assert_eq!(sel.focus, Some(0));
    }

    #[test]
    fn ctrl_navigation_moves_the_focus_and_leaves_the_selection() {
        let mut sel = selection(&[6], Some(6), Some(6));
        let moved = navigated(&mut sel, Key::END, true, false);
        assert_eq!(moved, Some(Moved { focus: 19, selection_changed: false }));
        assert_eq!(sel, selection(&[6], Some(6), Some(19)));
        navigated(&mut sel, Key::UP, true, false);
        assert_eq!(sel, selection(&[6], Some(6), Some(18)));
    }

    #[test]
    fn navigation_stops_at_both_ends() {
        let mut sel = selection(&[19], Some(19), Some(19));
        assert_eq!(navigated(&mut sel, Key::DOWN, false, false).map(|m| m.focus), Some(19));
        navigated(&mut sel, Key::HOME, false, false);
        assert_eq!(navigated(&mut sel, Key::PAGE_UP, false, false).map(|m| m.focus), Some(0));
        assert_eq!(sel, selection(&[0], Some(0), Some(0)));
    }

    #[test]
    fn other_keys_and_an_empty_list_leave_the_selection_alone() {
        let mut sel = selection(&[2], Some(2), Some(2));
        assert_eq!(navigated(&mut sel, Key::A, false, false), None);
        assert_eq!(navigate(&mut sel, Key::DOWN, false, false, 0, 5), None);
        assert_eq!(sel, selection(&[2], Some(2), Some(2)));
    }

    #[test]
    fn navigation_with_nothing_focused_starts_at_the_top_or_bottom() {
        let mut sel = Selection::new();
        navigated(&mut sel, Key::DOWN, false, false);
        assert_eq!(sel, selection(&[0], Some(0), Some(0)));
        let mut sel = Selection::new();
        navigated(&mut sel, Key::UP, false, true);
        assert_eq!((sel.selected, sel.focus), (vec![19], Some(19)));
    }
}