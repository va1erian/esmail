//! Diagnostic timing for the message list: the [`Timing`] snapshot the
//! example's `--bench-scroll`/`--trace-scroll` read, and the scroll-invalidate
//! workaround that keeps the painted rows in step with the scroll thumb.

use std::time::Instant;

use crate::paint::Phases;

/// A snapshot of the widget's recent paint/scroll timing. Timestamps are
/// monotonic [`Instant`]s taken on the UI thread, so the difference between
/// `paint_begin` and `scroll_at` is the input-to-paint latency.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// How many scrolls the host has applied.
    pub scroll_seq: u64,
    /// When the most recent scroll was applied, if any.
    pub scroll_at: Option<Instant>,
    /// How many `paint_d2d` frames have been drawn.
    pub paint_seq: u64,
    /// When the most recent frame began, if any.
    pub paint_begin: Option<Instant>,
    /// How long the most recent frame took, in microseconds.
    pub paint_micros: f64,
    /// The layout/draw split of the most recent frame, in microseconds.
    pub phases: Phases,
}

/// Workaround for a win32ui gap (see the PR's "win32ui root cause"): the
/// custom-widget scroll host moves the scroll offset and the native scrollbar
/// thumb but never invalidates the widget, so the painted rows stay stale until
/// some unrelated event repaints them. Invalidate here on every scroll to keep
/// the rows in step with the thumb.
pub(crate) fn invalidate(hwnd: win32ui::Hwnd) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Gdi::InvalidateRect;
    // SAFETY: `hwnd` names the widget's live child window; a null rect and
    // `true` mean "erase and repaint the whole client area".
    unsafe {
        let _ = InvalidateRect(Some(HWND(hwnd.raw() as *mut core::ffi::c_void)), None, true);
    }
}
