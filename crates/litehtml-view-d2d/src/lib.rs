//! `litehtml-view-d2d` — a prototype HTML view that lays a page out with
//! [litehtml](https://github.com/litehtml/litehtml) (through
//! `va1erian/litehtml-rs`) and paints it with Direct2D/DirectWrite through
//! [win32ui](https://github.com/va1erian/win32ui).
//!
//! This is milestone 1 of the native-Win32 frontend: **render, scroll, resize,
//! DPI**. Links and text selection are milestone 2. It exists to judge quality
//! and speed against the current egui painter (`egui-litehtml-webview`) before
//! committing to the full plan in `docs/WIN32_FRONTEND_PLAN.md` section 4.
//!
//! # Design
//!
//! A render **worker thread** owns the `!Send` litehtml `Document` and its
//! container, exactly like `egui-litehtml-webview`. The UI thread sends render
//! jobs (HTML + a layout width in DIPs + a job id); superseded jobs are dropped
//! and the UI ignores frames that are not the newest.
//!
//! The container measures text with win32ui's `TextSystem` (DirectWrite) and
//! turns every draw callback into a backend-neutral [`Cmd`] — a display list of
//! rects, rounded rects, border edges, gradients, images and text runs — with
//! its own `Point`/`Rect`/`Rgba` types (`geom.rs`). The [`DisplayList`] crosses
//! to the UI thread as an `Arc`; [`Painter::paint`] replays it into a
//! `D2dCanvas`, culling to the visible region and applying the scroll offset, so
//! a tall newsletter costs only what is on screen.
//!
//! # Units
//!
//! Everything is laid out in **device-independent pixels** (DIPs); the D2D
//! surface scales to the monitor DPI, so text stays crisp at any scale and
//! nothing re-lays-out on a DPI change except when the width in DIPs changes.
//!
//! # Images
//!
//! `data:` URIs are decoded with the `image` crate on the worker into
//! `Arc<Image>`; paint uploads them via `canvas.image`. Remote images are out
//! of scope for the prototype (no network), so they are simply left unloaded.
//!
//! # On other targets
//!
//! This crate compiles to an empty crate on non-Windows so the workspace's
//! Linux CI stays green.

#![cfg(windows)]
#![warn(missing_docs)]

mod container;
mod engine;
mod geom;
mod list;
mod paint;
mod widget;
mod worker;

pub use crate::geom::{Point, Radius, Rect, Rgba};
pub use crate::list::{
    BorderKind, BorderPaint, Cmd, Dash, DisplayList, EdgePaint, FontDesc, FontKey, GradientStop,
    Image, ImageKey, LinearGradient, RadialGradient, Stroke, decompose_borders, normalize_stops,
};
pub use crate::paint::Painter;
pub use crate::widget::HtmlView;
