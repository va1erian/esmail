//! Which part of the list a change repaints: the rows it touched, not the whole
//! widget (xui-win32 clips the paint to the rectangle handed to `invalidate_rect`).

use xui_win32::Rect;

use crate::selection::Selection;

/// The rows whose look changes when the selection goes from `before` to `after`:
/// the ones that entered or left it, and the old and new focus.
pub fn changed_rows(before: &Selection, after: &Selection) -> Vec<usize> {
    let mut rows: Vec<usize> = before.selected.iter().filter(|row| !after.selected.contains(row)).chain(after.selected.iter().filter(|row| !before.selected.contains(row))).copied().collect();
    rows.extend(before.focus.into_iter().chain(after.focus));
    rows.sort_unstable();
    rows.dedup();
    rows
}

/// The client rectangle, in device pixels, of `row` in a list scrolled to
/// `scroll` (dips) with rows `row_height` (dips) tall, `scale` device pixels
/// per dip and `width` device pixels wide. Rounded outwards so no edge pixel is
/// left stale.
pub fn row_rect(row: usize, row_height: f32, scroll: f32, scale: f32, width: f32) -> Rect {
    let top = (row as f32 * row_height - scroll) * scale;
    let bottom = ((row + 1) as f32 * row_height - scroll) * scale;
    Rect::new(0, top.floor() as i32, width.ceil() as i32, bottom.ceil() as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selection(selected: &[usize], focus: Option<usize>) -> Selection {
        Selection { selected: selected.to_vec(), anchor: focus, focus }
    }

    #[test]
    fn a_click_repaints_the_old_and_the_new_row_only() {
        let rows = changed_rows(&selection(&[2], Some(2)), &selection(&[5], Some(5)));
        assert_eq!(rows, [2, 5]);
    }

    #[test]
    fn extending_a_range_repaints_just_the_rows_it_added() {
        let rows = changed_rows(&selection(&[2], Some(2)), &selection(&[2, 3, 4], Some(4)));
        assert_eq!(rows, [2, 3, 4]);
    }

    #[test]
    fn an_unchanged_selection_repaints_nothing_beyond_its_focus() {
        assert_eq!(changed_rows(&selection(&[1], Some(1)), &selection(&[1], Some(1))), [1]);
        assert!(changed_rows(&selection(&[], None), &selection(&[], None)).is_empty());
    }

    #[test]
    fn a_row_rect_follows_the_scroll_and_scale() {
        let rect = row_rect(3, 50.0, 120.0, 1.25, 400.0);
        assert_eq!((rect.left, rect.top, rect.right, rect.bottom), (0, 37, 400, 100));
    }
}
