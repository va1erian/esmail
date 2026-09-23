//! Off-screen window recovery: a saved window position can name a monitor
//! that has since been unplugged or rearranged, which on Windows leaves the
//! window invisible with no way to drag it back. This is the pure geometry
//! behind putting it back on the primary monitor.

use egui::{Pos2, Rect};

/// Where to move a `current`-sized window so it is visible again, or `None`
/// if it already overlaps one of `monitors` (so a correctly placed window on
/// a secondary screen is never moved). Centered on `primary`.
pub(super) fn recentered_position(current: Rect, primary: Rect, monitors: &[Rect]) -> Option<Pos2> {
    if monitors.is_empty() || monitors.iter().any(|monitor| current.intersects(*monitor)) {
        return None;
    }
    Some(Pos2::new(
        primary.min.x + (primary.width() - current.width()).max(0.0) / 2.0,
        primary.min.y + (primary.height() - current.height()).max(0.0) / 2.0,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{pos2, vec2};

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect::from_min_size(pos2(x, y), vec2(w, h))
    }

    #[test]
    fn a_window_on_a_connected_monitor_is_left_alone() {
        let primary = rect(0.0, 0.0, 1920.0, 1080.0);
        let window = rect(100.0, 100.0, 800.0, 600.0);
        assert_eq!(recentered_position(window, primary, &[primary]), None);
    }

    #[test]
    fn a_window_partly_off_the_edge_is_left_alone() {
        // Still grabbable, so not the case this recovery targets.
        let primary = rect(0.0, 0.0, 1920.0, 1080.0);
        let window = rect(1900.0, 100.0, 800.0, 600.0);
        assert_eq!(recentered_position(window, primary, &[primary]), None);
    }

    #[test]
    fn a_window_on_a_removed_monitor_is_recentered() {
        let primary = rect(0.0, 0.0, 1920.0, 1080.0);
        let window = rect(3000.0, 200.0, 800.0, 600.0);
        assert_eq!(
            recentered_position(window, primary, &[primary]),
            Some(pos2((1920.0 - 800.0) / 2.0, (1080.0 - 600.0) / 2.0))
        );
    }

    #[test]
    fn a_window_larger_than_the_monitor_is_pinned_to_its_origin() {
        let primary = rect(0.0, 0.0, 1920.0, 1080.0);
        let window = rect(3000.0, 200.0, 2400.0, 1400.0);
        assert_eq!(recentered_position(window, primary, &[primary]), Some(pos2(0.0, 0.0)));
    }

    #[test]
    fn a_secondary_monitor_still_counts_as_on_screen() {
        let primary = rect(0.0, 0.0, 1920.0, 1080.0);
        let secondary = rect(1920.0, 0.0, 1920.0, 1080.0);
        let window = rect(2000.0, 100.0, 800.0, 600.0);
        assert_eq!(recentered_position(window, primary, &[primary, secondary]), None);
    }

    #[test]
    fn no_monitors_means_nothing_to_recenter_onto() {
        let window = rect(3000.0, 200.0, 800.0, 600.0);
        assert_eq!(recentered_position(window, rect(0.0, 0.0, 0.0, 0.0), &[]), None);
    }
}
