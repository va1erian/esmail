//! The public [`MessageList`]: a win32ui [`Custom`] widget painted with
//! Direct2D + DirectWrite, hosted in the built-in vertical scroll host
//! ([`Custom::with_vscroll`]).
//!
//! The scroll host owns a native scrollbar and the wheel/thumb scrolling; the
//! widget (`widget.rs`) paints only the visible rows (see
//! [`crate::state::visible_range`]) in document coordinates, because the host
//! translates the canvas by the offset before `paint_d2d` runs. The navigation
//! keys reach the widget first (win32ui's `CustomWidget::key`), which moves the
//! focused row and asks this wrapper to scroll it into view.
//!
//! Selection, focus and scroll-offset state live in the pure
//! [`crate::state::ViewState`], so the input handling is thin glue over
//! unit-tested transitions.

mod rows;
mod widget;

use std::cell::{OnceCell, RefCell};
use std::rc::{Rc, Weak};
use std::time::Instant;

use win32ui::d2d::TextSystem;
use win32ui::{AsControl, Control, Custom, Point, Rect, Theme, Themed, Ui, dip};

use crate::core_glue::compose::Kind;
use crate::events::MessageListEvents;
use crate::paint::Fonts;
use crate::state::{clamp_scroll, is_near_end, scroll_for_row};
use crate::timing::{Timing, invalidate};
use widget::MessageListWidget;

pub use crate::events::MessageListEvent;

/// A virtualized message list: a win32ui [`Custom`] widget painted with
/// Direct2D + DirectWrite, hosted in the built-in vertical scroll host.
pub struct MessageList<M: 'static> {
    custom: Rc<Custom<MessageListWidget, M>>,
    events: Rc<RefCell<MessageListEvents<M>>>,
    ui: Ui<M>,
}

/// Scrolls the minimum amount so `row` is fully visible.
fn scroll_row_into_view<M: 'static>(custom: &Custom<MessageListWidget, M>, row: usize) {
    let widget = custom.widget();
    let widget = widget.borrow();
    let view = widget.view.borrow();
    if row >= view.len {
        return;
    }
    let target = scroll_for_row(row, widget.fonts.row_height, widget.viewport.get(), view.scroll);
    drop(view);
    drop(widget);
    custom.scroll_to(dip(target));
}

impl<M: 'static> MessageList<M> {
    /// Creates the list, adopting `ui`'s theme.
    pub fn new(ui: &mut Ui<M>) -> win32ui::Result<MessageList<M>> {
        let text = TextSystem::new()?;
        let fonts = Fonts::new(&text)?;
        let scale = ui.dpi() as f32 / 96.0;
        let events = Rc::new(RefCell::new(MessageListEvents::new()));
        let events_for_dispatch = events.clone();
        let events_for_scroll = events.clone();
        // The event mapping runs inside the widget and needs the finished
        // `Custom` to scroll it, so it reaches it through a slot filled below.
        let scroller: Rc<OnceCell<Weak<Custom<MessageListWidget, M>>>> = Rc::new(OnceCell::new());
        let scroller_for_dispatch = scroller.clone();
        let custom = Custom::new(ui, MessageListWidget::new(fonts, scale))?;
        let widget_handle = custom.widget();
        let hwnd = custom.control().hwnd();
        let custom = custom
            .on_event(move |event| {
                let events = events_for_dispatch.borrow();
                match event {
                    MessageListEvent::Selected(rows) => events.on_select.as_ref().and_then(|f| f(&rows)),
                    MessageListEvent::Open(row) => events.on_open.as_ref().and_then(|f| f(row)),
                    MessageListEvent::Delete(rows) => events.on_delete.as_ref().and_then(|f| f(&rows)),
                    MessageListEvent::ToggleFlag(row) => events.on_toggle_flag.as_ref().and_then(|f| f(row)),
                    MessageListEvent::Compose(kind) => events.on_compose.as_ref().and_then(|f| f(kind)),
                    MessageListEvent::Context { row, at } => events.on_context.as_ref().and_then(|f| f(row, at)),
                    MessageListEvent::Focus(row) => {
                        if let Some(custom) = scroller_for_dispatch.get().and_then(Weak::upgrade) {
                            scroll_row_into_view(&custom, row);
                        }
                        None
                    }
                }
            })
            .with_vscroll()
            // The scroll host owns the offset; mirror it into the widget's view
            // state so `paint_d2d` knows which rows are visible, and repaint
            // immediately (the host moves the thumb but never invalidates).
            .on_scroll(move |offset| {
                let widget = widget_handle.borrow();
                let row_height = widget.fonts.row_height;
                let viewport = widget.viewport.get();
                let mut view = widget.view.borrow_mut();
                // The host clamps too, but an explicit clamp here keeps the
                // mirror in `[0, max_scroll]` even if a future host hands us an
                // overscrolled or non-finite offset.
                view.scroll = clamp_scroll(offset.value(), row_height, viewport, view.len);
                let near_end = is_near_end(view.scroll, viewport, row_height, view.len);
                drop(view);
                widget.scroll_seq.set(widget.scroll_seq.get() + 1);
                widget.scroll_at.set(Some(Instant::now()));
                drop(widget);
                invalidate(hwnd);
                if near_end { events_for_scroll.borrow().on_near_end.as_ref().and_then(|f| f()) } else { None }
            });
        let custom = Rc::new(custom);
        let _ = scroller.set(Rc::downgrade(&custom));
        Ok(MessageList { custom, events, ui: ui.clone() })
    }

    /// Maps a selection change to a message.
    pub fn on_select(self, f: impl Fn(&[usize]) -> Option<M> + 'static) -> MessageList<M> {
        self.events.borrow_mut().on_select = Some(Box::new(f));
        self
    }

    /// Maps an open (a double-click, or [`open_focused`](Self::open_focused)) to a
    /// message.
    pub fn on_open(self, f: impl Fn(usize) -> Option<M> + 'static) -> MessageList<M> {
        self.events.borrow_mut().on_open = Some(Box::new(f));
        self
    }

    /// Maps a delete to a message.
    pub fn on_delete(self, f: impl Fn(&[usize]) -> Option<M> + 'static) -> MessageList<M> {
        self.events.borrow_mut().on_delete = Some(Box::new(f));
        self
    }

    /// Maps a flag toggle to a message with the row it was asked for: a click
    /// on a row's star (an outline star shows while the pointer is over an
    /// unflagged row) or Space on the focused row.
    pub fn on_toggle_flag(self, f: impl Fn(usize) -> Option<M> + 'static) -> MessageList<M> {
        self.events.borrow_mut().on_toggle_flag = Some(Box::new(f));
        self
    }

    /// Maps a right-click to a message, with the row and the pointer position.
    pub fn on_context(self, f: impl Fn(usize, Point) -> Option<M> + 'static) -> MessageList<M> {
        self.events.borrow_mut().on_context = Some(Box::new(f));
        self
    }

    /// Maps R, Shift+R and F on the focused row to a message: reply, reply to
    /// all, forward.
    pub fn on_compose(self, f: impl Fn(Kind) -> Option<M> + 'static) -> MessageList<M> {
        self.events.borrow_mut().on_compose = Some(Box::new(f));
        self
    }

    /// Maps "the view scrolled to within a few rows of the last one" to a
    /// message. Raised on every scroll while that holds, so the app dedups
    /// (it is how an app loads the next page of a long mailbox).
    pub fn on_near_end(self, f: impl Fn() -> Option<M> + 'static) -> MessageList<M> {
        self.events.borrow_mut().on_near_end = Some(Box::new(f));
        self
    }

    /// Every selected row, ascending.
    pub fn selection(&self) -> Vec<usize> {
        self.custom.widget().borrow().view.borrow().selection.selected.clone()
    }

    /// The row the keyboard moves (and Space acts on), if there is one.
    pub fn focus_row(&self) -> Option<usize> {
        self.custom.widget().borrow().view.borrow().selection.focus
    }

    /// Whether the list has the keyboard focus.
    pub fn has_focus(&self) -> bool {
        self.custom.widget().borrow().focused.get()
    }

    /// Opens the focused row, as if Enter were pressed on it. The window's
    /// dialog-style navigation takes Enter before a custom widget can see it,
    /// so the app routes the key here.
    pub fn open_focused(&self) {
        let Some(row) = self.focus_row() else { return };
        if let Some(msg) = self.events.borrow().on_open.as_ref().and_then(|f| f(row)) {
            self.ui.emit(msg);
        }
    }

    /// Makes `rows` the selection, deselecting everything else. Out-of-range
    /// and duplicate rows are dropped. Emits one selection message when the
    /// selection actually changed.
    pub fn set_selection(&self, rows: &[usize]) {
        let widget = self.custom.widget();
        let len = widget.borrow().view.borrow().len;
        let before = widget.borrow().view.borrow().selection.selected.clone();
        widget.borrow().view.borrow_mut().selection.replace(rows, len);
        let after = widget.borrow().view.borrow().selection.selected.clone();
        if before != after {
            if let Some(msg) = self.events.borrow().on_select.as_ref().and_then(|f| f(&after)) {
                self.ui.emit(msg);
            }
        }
        self.custom.invalidate();
    }

    /// Scrolls the minimum amount so `row` is fully visible.
    pub fn ensure_visible(&self, row: usize) {
        scroll_row_into_view(&self.custom, row);
    }

    /// Schedules a repaint.
    pub fn invalidate(&self) {
        self.custom.invalidate();
    }

    /// How long the last paint took, in microseconds (diagnostic).
    pub fn last_paint_micros(&self) -> f64 {
        self.custom.widget().borrow().paint_micros.get()
    }

    /// When a paint first drew a row, if one has (diagnostic).
    pub fn first_content_paint(&self) -> Option<Instant> {
        self.custom.widget().borrow().first_content.get()
    }

    /// A snapshot of the widget's recent paint/scroll timing (diagnostic).
    pub fn timing(&self) -> Timing {
        let widget = self.custom.widget();
        let widget = widget.borrow();
        Timing {
            scroll_seq: widget.scroll_seq.get(),
            scroll_at: widget.scroll_at.get(),
            paint_seq: widget.paint_seq.get(),
            paint_begin: widget.paint_begin.get(),
            paint_micros: widget.paint_micros.get(),
            phases: widget.phases.get(),
            last_rows: widget.last_rows.get(),
        }
    }

    /// The list's rectangle in screen coordinates, for positioning a context
    /// popup from an [`on_context`](Self::on_context) position.
    pub fn window_rect(&self) -> Rect {
        self.custom.window_rect()
    }
}

impl<M: 'static> AsControl for MessageList<M> {
    fn control(&self) -> &Control {
        self.custom.control()
    }
}

impl<M: 'static> Themed for MessageList<M> {
    fn apply_theme(&self, theme: &Theme) {
        self.custom.apply_theme(theme);
    }
}
