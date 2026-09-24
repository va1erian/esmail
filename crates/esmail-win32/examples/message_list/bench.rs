//! Synthetic scroll input and latency measurement for `--bench-scroll`.
//!
//! The bench drives the real widget window with `WM_VSCROLL` / `WM_MOUSEWHEEL`
//! sent straight to the widget `HWND`, so it exercises the exact scroll-host
//! path a real scrollbar click or wheel notch takes (not a back-door call into
//! `MessageList`). It measures the time from the input being handled to the
//! next `paint_d2d` beginning, plus the paint's own layout/draw split.
//!
//! The input cycle walks down, jumps to the top through `SB_THUMBTRACK`, then
//! overscrolls further up (wheel-up, `SB_LINEUP`, `SB_PAGEUP`, `SB_TOP`). Every
//! input must leave at least one row painted; [`report`] asserts that and prints
//! the minimum rows per input kind (#116).

use std::time::Instant;

use win32ui::Hwnd;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    SendMessageW, SB_LINEDOWN, SB_LINEUP, SB_PAGEDOWN, SB_PAGEUP, SB_THUMBTRACK, SB_TOP,
    WM_MOUSEWHEEL, WM_VSCROLL,
};

/// How many inputs the bench sends before summarising and exiting.
pub(crate) const INPUTS: usize = 200;

/// One wheel notch, in `WHEEL_DELTA` units (120 = one notch).
const NOTCH: i16 = 120;

/// The scroll inputs the bench cycles through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScrollKind {
    WheelDown,
    LineDown,
    PageDown,
    ThumbTrack,
    WheelUp,
    LineUp,
    PageUp,
    Top,
}

impl ScrollKind {
    pub(crate) fn name(self) -> &'static str {
        match self {
            ScrollKind::WheelDown => "wheel",
            ScrollKind::LineDown => "line-down",
            ScrollKind::PageDown => "page-down",
            ScrollKind::ThumbTrack => "thumb-track",
            ScrollKind::WheelUp => "wheel-up",
            ScrollKind::LineUp => "line-up",
            ScrollKind::PageUp => "page-up",
            ScrollKind::Top => "top",
        }
    }

    fn next(self) -> ScrollKind {
        match self {
            ScrollKind::WheelDown => ScrollKind::LineDown,
            ScrollKind::LineDown => ScrollKind::PageDown,
            ScrollKind::PageDown => ScrollKind::ThumbTrack,
            // `ThumbTrack` reads a track position of 0, so the next four inputs
            // all land at the top and overscroll past it: the case the
            // blank-viewport bug showed up in.
            ScrollKind::ThumbTrack => ScrollKind::WheelUp,
            ScrollKind::WheelUp => ScrollKind::LineUp,
            ScrollKind::LineUp => ScrollKind::PageUp,
            ScrollKind::PageUp => ScrollKind::Top,
            ScrollKind::Top => ScrollKind::WheelDown,
        }
    }

    /// Sends one input of this kind to the widget window.
    pub(crate) fn send(self, hwnd: Hwnd) {
        match self {
            ScrollKind::WheelDown => {
                // A wheel notch down: negative `WHEEL_DELTA` in the high word.
                let wparam = ((-NOTCH) as u16 as usize) << 16;
                send(hwnd, WM_MOUSEWHEEL, WPARAM(wparam), 0);
            }
            ScrollKind::WheelUp => {
                // A wheel notch up: positive `WHEEL_DELTA` in the high word.
                let wparam = (NOTCH as u16 as usize) << 16;
                send(hwnd, WM_MOUSEWHEEL, WPARAM(wparam), 0);
            }
            ScrollKind::LineDown => vscroll(hwnd, SB_LINEDOWN, 0),
            ScrollKind::PageDown => vscroll(hwnd, SB_PAGEDOWN, 0),
            // The high word position is only 16 bits and is ignored by win32ui
            // in favour of `GetScrollInfo(SIF_TRACKPOS)`; without a real drag
            // that reads 0, so this input measures the track path's latency
            // rather than a meaningful destination.
            ScrollKind::ThumbTrack => vscroll(hwnd, SB_THUMBTRACK, 32_000),
            ScrollKind::LineUp => vscroll(hwnd, SB_LINEUP, 0),
            ScrollKind::PageUp => vscroll(hwnd, SB_PAGEUP, 0),
            ScrollKind::Top => vscroll(hwnd, SB_TOP, 0),
        }
    }
}

/// One measured input and its resulting paint.
pub(crate) struct Sample {
    pub(crate) kind: ScrollKind,
    /// Input handled to `paint_d2d` begin, in nanoseconds.
    pub(crate) latency_ns: u128,
    pub(crate) layout_us: f64,
    pub(crate) draw_us: f64,
    pub(crate) paint_us: f64,
    /// How many rows the paint that followed this input drew.
    pub(crate) rows: usize,
}

/// The bench driver, advanced once per tick from `app::App::tick`.
pub(crate) struct Bench {
    pub(crate) kind: ScrollKind,
    pub(crate) sent: usize,
    /// The input waiting for its paint: its send time and its kind.
    pub(crate) pending: Option<(Instant, ScrollKind)>,
    /// The paint sequence number expected after the pending input paints.
    pub(crate) awaiting: u64,
    pub(crate) samples: Vec<Sample>,
    pub(crate) paints: u64,
    pub(crate) started: Instant,
}

impl Bench {
    pub(crate) fn new() -> Bench {
        Bench {
            kind: ScrollKind::WheelDown,
            sent: 0,
            pending: None,
            awaiting: 0,
            samples: Vec::new(),
            paints: 0,
            started: Instant::now(),
        }
    }

    /// The next input kind, advanced after the current one is sent.
    pub(crate) fn step(&mut self) {
        self.kind = self.kind.next();
    }
}

fn send(hwnd: Hwnd, msg: u32, wparam: WPARAM, lparam: isize) {
    // SAFETY: `hwnd` names the widget's live child window; the parameters are
    // plain integers with no pointers.
    unsafe {
        let _ = SendMessageW(
            HWND(hwnd.raw() as *mut core::ffi::c_void),
            msg,
            Some(wparam),
            Some(LPARAM(lparam)),
        );
    }
}

fn vscroll(hwnd: Hwnd, code: windows::Win32::UI::WindowsAndMessaging::SCROLLBAR_COMMAND, pos: u16) {
    let wparam = WPARAM(code.0 as usize | ((pos as usize) << 16));
    send(hwnd, WM_VSCROLL, wparam, 0);
}

/// The `p`-th percentile (0..=100) of a slice of sorted-per-call values.
fn percentile(values: &[u128], p: usize) -> u128 {
    if values.is_empty() {
        return 0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let index = ((p as f64 / 100.0) * (sorted.len() - 1) as f64).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

/// Prints the p50/p95/max summary for the whole run and per input kind.
pub(crate) fn report(samples: &[Sample], paints: u64, started: Instant) {
    let elapsed = started.elapsed().as_secs_f64();

    let latency: Vec<u128> = samples.iter().map(|s| s.latency_ns).collect();
    // Input handled to frame finished (paint_d2d end); EndDraw/present adds a
    // little more but is not observable from the widget side.
    let painted: Vec<u128> = samples
        .iter()
        .map(|s| s.latency_ns + (s.paint_us * 1_000.0) as u128)
        .collect();
    let paint: Vec<u128> = samples
        .iter()
        .map(|s| (s.paint_us * 1_000.0) as u128)
        .collect();
    let layout: Vec<u128> = samples
        .iter()
        .map(|s| (s.layout_us * 1_000.0) as u128)
        .collect();
    let draw: Vec<u128> = samples
        .iter()
        .map(|s| (s.draw_us * 1_000.0) as u128)
        .collect();

    eprintln!(
        "bench-scroll: {INPUTS} inputs in {elapsed:.2}s ({:.0} inputs/s), {paints} paints ({:.1} paints/s, {:.2} WM_PAINT/input)",
        INPUTS as f64 / elapsed,
        paints as f64 / elapsed,
        paints as f64 / INPUTS as f64
    );
    eprintln!(
        "  input->paint  p50={:>7.1}us p95={:>7.1}us max={:>7.1}us",
        percentile(&latency, 50) as f64 / 1_000.0,
        percentile(&latency, 95) as f64 / 1_000.0,
        percentile(&latency, 100) as f64 / 1_000.0,
    );
    eprintln!(
        "  input->painted p50={:>7.1}us p95={:>7.1}us max={:>7.1}us",
        percentile(&painted, 50) as f64 / 1_000.0,
        percentile(&painted, 95) as f64 / 1_000.0,
        percentile(&painted, 100) as f64 / 1_000.0,
    );
    eprintln!(
        "  paint         p50={:>7.1}us p95={:>7.1}us max={:>7.1}us",
        percentile(&paint, 50) as f64 / 1_000.0,
        percentile(&paint, 95) as f64 / 1_000.0,
        percentile(&paint, 100) as f64 / 1_000.0,
    );
    eprintln!(
        "  layout        p50={:>7.1}us   draw p50={:>7.1}us",
        percentile(&layout, 50) as f64 / 1_000.0,
        percentile(&draw, 50) as f64 / 1_000.0,
    );

    for kind in [
        ScrollKind::WheelDown,
        ScrollKind::LineDown,
        ScrollKind::PageDown,
        ScrollKind::ThumbTrack,
        ScrollKind::WheelUp,
        ScrollKind::LineUp,
        ScrollKind::PageUp,
        ScrollKind::Top,
    ] {
        let per: Vec<u128> = samples
            .iter()
            .filter(|s| s.kind == kind)
            .map(|s| s.latency_ns)
            .collect();
        if per.is_empty() {
            continue;
        }
        let min_rows = samples
            .iter()
            .filter(|s| s.kind == kind)
            .map(|s| s.rows)
            .min()
            .unwrap_or(0);
        eprintln!(
            "  {:<11} n={:>3} p50={:>7.1}us p95={:>7.1}us max={:>7.1}us min-rows={}",
            kind.name(),
            per.len(),
            percentile(&per, 50) as f64 / 1_000.0,
            percentile(&per, 95) as f64 / 1_000.0,
            percentile(&per, 100) as f64 / 1_000.0,
            min_rows,
        );
    }

    // The acceptance condition for #116: every input must leave at least one
    // row on screen. An empty viewport means the offset/translation pair the
    // paint used was out of step with the model.
    for sample in samples {
        assert!(
            sample.rows > 0,
            "bench-scroll: input {:?} painted no rows (blank viewport)",
            sample.kind,
        );
    }
}
