//! `esmail-win32` — the esMail message list rebuilt as a reusable
//! [win32ui](https://github.com/va1erian/win32ui) component, with no egui.
//!
//! This is the native-Win32 prototype for one complex screen of esMail: the
//! message list. [`MessageList`] is a win32ui `CustomWidget` painted with
//! Direct2D + DirectWrite, hosted in win32ui's built-in vertical scroll host,
//! so it looks and scrolls like a native list. It is virtualized: rows have a
//! fixed DPI-aware height and only the visible ones are painted, so a list of
//! 100,000 rows costs what a handful cost.
//!
//! Row text and formatting come from [`esmail::view_model::RowModel`], the same
//! model the egui frontend paints, so the two frontends cannot drift apart on
//! wording (unread accent bar, bold sender, dimmed read rows, star, right-aligned
//! date/time, ellipsized subject). Colour emoji, CJK and RTL text render through
//! DirectWrite's own glyph fallback and bidi — there are no Twemoji assets.
//!
//! # API
//!
//! Events are mapped to the app's own `Msg` by closures at construction, in the
//! widget-layer style (no control ids):
//!
//! ```ignore
//! let list = MessageList::new(ui)?
//!     .on_select(|rows| Some(Msg::Selected(rows.to_vec())))
//!     .on_open(|row| Some(Msg::Open(row)))
//!     .on_delete(|rows| Some(Msg::Delete(rows.to_vec())))
//!     .on_context(|row, at| Some(Msg::Context(row, at)));
//!
//! list.set_rows(rows);            // replace the model (clears selection, scrolls to top)
//! list.rows_changed(10..12);      // repaint a range (a flag/seen change)
//! list.rows_inserted(0..5);       // refresh after an insert, keeping scroll + selection
//! list.set_selection(&[3, 7]);    // select from the app
//! let sel = list.selection();     // ascending, deduplicated indices
//! list.ensure_visible(42);        // scroll so row 42 is fully visible
//! ```
//!
//! # On other targets
//!
//! Only the widgets are Windows-only. [`core_glue`], the frontend-independent
//! layer that drives esMail's IMAP actors, builds everywhere so its logic is
//! unit-tested on Linux CI too; the `esmail-win32` binary is an empty `main`
//! there.

#![warn(missing_docs)]

pub mod core_glue;
#[cfg(windows)]
mod events;
#[cfg(windows)]
mod message_list;
#[cfg(windows)]
mod paint;
#[cfg(windows)]
mod state;
#[cfg(windows)]
mod timing;

#[cfg(windows)]
pub use message_list::{MessageList, MessageListEvent};
#[cfg(windows)]
pub use paint::Phases;
#[cfg(windows)]
pub use timing::Timing;
