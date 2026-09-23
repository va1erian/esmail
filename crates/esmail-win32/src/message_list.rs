//! The public [`MessageList`] and the owner-drawn widget behind it.
//!
//! A [`MessageList`] is a win32ui [`Custom`] widget painted with Direct2D +
//! DirectWrite, hosted in the built-in vertical scroll host
//! ([`Custom::with_vscroll`]). The scroll host owns a native scrollbar and the
//! wheel/thumb/key scrolling; the widget paints only the visible rows (see
//! [`crate::state::visible_range`]) in document coordinates, because the host
//! translates the canvas by the offset before `paint_d2d` runs.
//!
//! Selection, focus and scroll-offset state live in the pure
//! [`crate::state::ViewState`], so the input handling here is thin glue over
//! unit-tested transitions.

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use esmail::view_model::RowModel;
use win32ui::d2d::{D2dCanvas, RectF, TextSystem};
use win32ui::gdi::Canvas;
use win32ui::{
    AsControl, Control, Custom, CustomWidget, Input, Key, MouseButton, Point, Rect, Renderer,
    Theme, Themed, Ui, WidgetCx, dip,
};

use crate::paint::{self, Fonts, Phases, RowVisual};
use crate::state::{ViewState, row_at, scroll_for_row, visible_range};
use crate::timing::{invalidate, Timing};

/// An event raised by a [`MessageList`], mapped to the app's `Msg` by the
/// closures given at construction.
#[derive(Clone, Debug)]
pub enum MessageListEvent {
    /// The selection changed. Carries every selected row, ascending.
    Selected(Vec<usize>),
    /// A row was opened (Enter or double-click).
    Open(usize),
    /// The selected rows were deleted.
    Delete(Vec<usize>),
    /// The flag of the focused row should toggle (Space).
    ToggleFlag(usize),
    /// A context menu was requested for `row` at the pointer position `at`
    /// (client coordinates, device pixels).
    Context {
        /// The row the pointer is over.
        row: usize,
        /// The pointer position in client coordinates (device pixels).
        at: Point,
    },
}

type SelectMapper<M> = Box<dyn Fn(&[usize]) -> Option<M>>;
type RowMapper<M> = Box<dyn Fn(usize) -> Option<M>>;
type ContextMapper<M> = Box<dyn Fn(usize, Point) -> Option<M>>;

/// The app-side event mappings a [`MessageList`] is built with.
struct MessageListEvents<M> {
    on_select: Option<SelectMapper<M>>,
    on_open: Option<RowMapper<M>>,
    on_delete: Option<Box<dyn Fn(&[usize]) -> Option<M>>>,
    on_flag: Option<RowMapper<M>>,
    on_context: Option<ContextMapper<M>>,
}

impl<M> MessageListEvents<M> {
    fn new() -> MessageListEvents<M> {
        MessageListEvents {
            on_select: None,
            on_open: None,
            on_delete: None,
            on_flag: None,
            on_context: None,
        }
    }
}

/// The owner-drawn widget behind a [`MessageList`]. All mutable state lives in
/// `Cell`/`RefCell` fields because the `CustomWidget` trait hands the widget
/// `&self`.
struct MessageListWidget {
    fonts: Fonts,
    rows: RefCell<Arc<[RowModel]>>,
    view: RefCell<ViewState>,
    /// The window scale from device to independent pixels (dpi / 96).
    scale: Cell<f32>,
    /// The viewport height in device-independent pixels, from the last paint.
    viewport: Cell<f32>,
    /// The row under the pointer, if any.
    hover: Cell<Option<usize>>,
    /// Whether the list has the keyboard focus.
    focused: Cell<bool>,
    /// Ctrl/Shift are tracked from key events because mouse events do not carry
    /// modifier state (see the win32ui gap in the PR).
    ctrl: Cell<bool>,
    shift: Cell<bool>,
    /// How long the last `paint_d2d` took, in microseconds (diagnostic).
    paint_micros: Cell<f64>,
    /// The layout/draw split of the last `paint_d2d` (diagnostic).
    phases: Cell<Phases>,
    /// Monotonic paint counter (diagnostic).
    paint_seq: Cell<u64>,
    /// When the last `paint_d2d` began (diagnostic).
    paint_begin: Cell<Option<Instant>>,
    /// Monotonic scroll counter (diagnostic).
    scroll_seq: Cell<u64>,
    /// When the last scroll was applied (diagnostic).
    scroll_at: Cell<Option<Instant>>,
}

impl MessageListWidget {
    fn index_at(&self, _x: i32, y: i32) -> Option<usize> {
        let scale = self.scale.get();
        let view = self.view.borrow();
        let doc_y = y as f32 / scale + view.scroll;
        row_at(doc_y, self.fonts.row_height, view.len)
    }

    fn emit_selected(&self, cx: &WidgetCx<MessageListEvent>) {
        cx.emit(MessageListEvent::Selected(
            self.view.borrow().selection.selected.clone(),
        ));
    }
}

impl CustomWidget for MessageListWidget {
    type Event = MessageListEvent;

    fn renderer(&self) -> Renderer {
        Renderer::Direct2D
    }

    // This widget is Direct2D-only; the GDI fallback (used only when Direct2D
    // cannot create a surface) leaves the list blank.
    fn paint(&self, _canvas: &Canvas, _bounds: Rect, _theme: &Theme) {}

    fn paint_d2d(&self, canvas: &mut D2dCanvas<'_>, bounds: RectF, theme: &Theme) {
        self.viewport.set(bounds.height());
        let started = Instant::now();
        self.paint_begin.set(Some(started));
        let view = self.view.borrow();
        let rows = self.rows.borrow();
        let range = visible_range(view.scroll, bounds.height(), self.fonts.row_height, view.len);
        let selection = &view.selection;
        let focused = self.focused.get();
        let hover = self.hover.get();
        let mut phases = Phases::default();
        for index in range {
            let Some(row) = rows.get(index) else { break };
            let top = index as f32 * self.fonts.row_height;
            let rect = RectF::new(0.0, top, bounds.width(), top + self.fonts.row_height);
            let visual = RowVisual {
                selected: selection.contains(index),
                hovered: hover == Some(index),
                focused,
            };
            paint::paint_row(canvas, row, rect, visual, &self.fonts, theme, &mut phases);
        }
        drop(rows);
        drop(view);
        self.paint_micros.set(started.elapsed().as_secs_f64() * 1_000_000.0);
        self.phases.set(phases);
        self.paint_seq.set(self.paint_seq.get() + 1);
    }

    fn input(&self, input: Input, cx: &mut WidgetCx<MessageListEvent>) {
        match input {
            Input::MouseDown { x, y, button: MouseButton::Left } => {
                cx.focus();
                let Some(index) = self.index_at(x, y) else { return };
                let (ctrl, shift) = (self.ctrl.get(), self.shift.get());
                {
                    let mut view = self.view.borrow_mut();
                    let sel = &mut view.selection;
                    if ctrl {
                        sel.ctrl_click(index);
                    } else if shift {
                        sel.shift_click(index);
                    } else {
                        sel.click(index);
                    }
                }
                self.emit_selected(cx);
                cx.invalidate();
            }
            Input::MouseDown { x, y, button: MouseButton::Right } => {
                cx.focus();
                let Some(index) = self.index_at(x, y) else { return };
                // A right-click selects the row when it is not part of the
                // selection, then requests a context menu (Explorer behaviour).
                {
                    let mut view = self.view.borrow_mut();
                    if !view.selection.contains(index) {
                        view.selection.click(index);
                        drop(view);
                        self.emit_selected(cx);
                    }
                }
                cx.emit(MessageListEvent::Context {
                    row: index,
                    at: Point::new(x, y),
                });
            }
            Input::MouseDoubleClick { x, y, button: MouseButton::Left } => {
                if let Some(index) = self.index_at(x, y) {
                    cx.emit(MessageListEvent::Open(index));
                }
            }
            Input::MouseMove { x, y } => {
                self.hover.set(self.index_at(x, y));
                cx.invalidate();
            }
            Input::MouseLeave => {
                self.hover.set(None);
                cx.invalidate();
            }
            Input::KeyDown { key, modifiers, .. } => {
                if key == Key::CONTROL {
                    self.ctrl.set(true);
                }
                if key == Key::SHIFT {
                    self.shift.set(true);
                }
                if modifiers.ctrl && key == Key::A {
                    let len = self.view.borrow().len;
                    self.view.borrow_mut().selection.select_all(len);
                    self.emit_selected(cx);
                    cx.invalidate();
                } else if key == Key::RETURN {
                    if let Some(index) = self.view.borrow().selection.focus {
                        cx.emit(MessageListEvent::Open(index));
                    }
                } else if key == Key::DELETE {
                    let selected = self.view.borrow().selection.selected.clone();
                    if !selected.is_empty() {
                        cx.emit(MessageListEvent::Delete(selected));
                    }
                } else if key == Key::SPACE {
                    if let Some(index) = self.view.borrow().selection.focus {
                        cx.emit(MessageListEvent::ToggleFlag(index));
                    }
                }
            }
            Input::KeyUp { key, .. } => {
                if key == Key::CONTROL {
                    self.ctrl.set(false);
                }
                if key == Key::SHIFT {
                    self.shift.set(false);
                }
            }
            Input::SetFocus => {
                self.focused.set(true);
                cx.invalidate();
            }
            Input::KillFocus => {
                self.focused.set(false);
                cx.invalidate();
            }
            _ => {}
        }
    }
}

/// A virtualized message list: a win32ui [`Custom`] widget painted with
/// Direct2D + DirectWrite, hosted in the built-in vertical scroll host.
pub struct MessageList<M: 'static> {
    custom: Custom<MessageListWidget, M>,
    events: Rc<RefCell<MessageListEvents<M>>>,
    ui: Ui<M>,
}

impl<M: 'static> MessageList<M> {
    /// Creates the list, adopting `ui`'s theme.
    pub fn new(ui: &mut Ui<M>) -> win32ui::Result<MessageList<M>> {
        let text = TextSystem::new()?;
        let fonts = Fonts::new(&text)?;
        let scale = ui.dpi() as f32 / 96.0;
        let events = Rc::new(RefCell::new(MessageListEvents::new()));
        let events_for_dispatch = events.clone();
        let widget = MessageListWidget {
            fonts,
            rows: RefCell::new(Arc::from([])),
            view: RefCell::new(ViewState::new()),
            scale: Cell::new(scale),
            viewport: Cell::new(0.0),
            hover: Cell::new(None),
            focused: Cell::new(false),
            ctrl: Cell::new(false),
            shift: Cell::new(false),
            paint_micros: Cell::new(0.0),
            phases: Cell::new(Phases::default()),
            paint_seq: Cell::new(0),
            paint_begin: Cell::new(None),
            scroll_seq: Cell::new(0),
            scroll_at: Cell::new(None),
        };
        let custom = Custom::new(ui, widget)?;
        let widget_handle = custom.widget();
        let hwnd = custom.control().hwnd();
        let custom = custom
            .on_event(move |event| {
                let events = events_for_dispatch.borrow();
                match event {
                    MessageListEvent::Selected(rows) => {
                        events.on_select.as_ref().and_then(|f| f(&rows))
                    }
                    MessageListEvent::Open(row) => events.on_open.as_ref().and_then(|f| f(row)),
                    MessageListEvent::Delete(rows) => {
                        events.on_delete.as_ref().and_then(|f| f(&rows))
                    }
                    MessageListEvent::ToggleFlag(row) => {
                        events.on_flag.as_ref().and_then(|f| f(row))
                    }
                    MessageListEvent::Context { row, at } => {
                        events.on_context.as_ref().and_then(|f| f(row, at))
                    }
                }
            })
            .with_vscroll()
            // The scroll host owns the offset; mirror it into the widget's view
            // state so `paint_d2d` knows which rows are visible, and repaint
            // immediately (the host moves the thumb but never invalidates — see
            // `invalidate`).
            .on_scroll(move |offset| {
                let widget = widget_handle.borrow();
                widget.view.borrow_mut().scroll = offset.value();
                widget.scroll_seq.set(widget.scroll_seq.get() + 1);
                widget.scroll_at.set(Some(Instant::now()));
                drop(widget);
                invalidate(hwnd);
                None
            });
        Ok(MessageList {
            custom,
            events,
            ui: ui.clone(),
        })
    }

    /// Maps a selection change to a message.
    pub fn on_select(self, f: impl Fn(&[usize]) -> Option<M> + 'static) -> MessageList<M> {
        self.events.borrow_mut().on_select = Some(Box::new(f));
        self
    }

    /// Maps an open (Enter or double-click) to a message.
    pub fn on_open(self, f: impl Fn(usize) -> Option<M> + 'static) -> MessageList<M> {
        self.events.borrow_mut().on_open = Some(Box::new(f));
        self
    }

    /// Maps a delete to a message.
    pub fn on_delete(self, f: impl Fn(&[usize]) -> Option<M> + 'static) -> MessageList<M> {
        self.events.borrow_mut().on_delete = Some(Box::new(f));
        self
    }

    /// Maps a flag toggle (Space) to a message.
    pub fn on_flag(self, f: impl Fn(usize) -> Option<M> + 'static) -> MessageList<M> {
        self.events.borrow_mut().on_flag = Some(Box::new(f));
        self
    }

    /// Maps a right-click to a message, with the row and the pointer position.
    pub fn on_context(self, f: impl Fn(usize, Point) -> Option<M> + 'static) -> MessageList<M> {
        self.events.borrow_mut().on_context = Some(Box::new(f));
        self
    }

    /// Replaces the model. This is a new mailbox: the selection is dropped and
    /// the view returns to the top.
    pub fn set_rows(&self, rows: Arc<[RowModel]>) {
        let len = rows.len();
        let row_height = {
            let widget = self.custom.widget();
            let w = widget.borrow_mut();
            *w.rows.borrow_mut() = rows;
            w.view.borrow_mut().set_rows(len);
            w.fonts.row_height
        };
        self.custom.set_content_height(dip(len as f32 * row_height));
        self.custom.scroll_to(dip(0.0));
        self.custom.invalidate();
    }

    /// Repaints the visible rows after the rows in `range` changed (a flag or
    /// seen toggle). Scroll and selection are untouched. `range` is accepted
    /// for `ListView` API parity; the list repaints only what is visible
    /// anyway, so a whole-widget repaint is already cheap.
    pub fn rows_changed(&self, _range: Range<usize>) {
        self.custom.invalidate();
    }

    /// Refreshes after rows were inserted, keeping the scroll position and the
    /// selection. The row count is re-read from the model (which the caller has
    /// already replaced via [`set_rows`](Self::set_rows)); `range` describes
    /// where the rows went in and is kept for `ListView` API parity.
    pub fn rows_inserted(&self, _range: Range<usize>) {
        let widget = self.custom.widget();
        let row_height = widget.borrow().fonts.row_height;
        let len = widget.borrow().rows.borrow().len();
        widget.borrow().view.borrow_mut().recount(len);
        self.custom.set_content_height(dip(len as f32 * row_height));
        self.custom.invalidate();
    }

    /// Every selected row, ascending.
    pub fn selection(&self) -> Vec<usize> {
        self.custom.widget().borrow().view.borrow().selection.selected.clone()
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
        let widget = self.custom.widget();
        let w = widget.borrow();
        let view = w.view.borrow();
        if row >= view.len {
            return;
        }
        let target = scroll_for_row(row, w.fonts.row_height, w.viewport.get(), view.scroll);
        drop(view);
        drop(w);
        self.custom.scroll_to(dip(target));
    }

    /// Schedules a repaint.
    pub fn invalidate(&self) {
        self.custom.invalidate();
    }

    /// How long the last paint took, in microseconds (diagnostic).
    pub fn last_paint_micros(&self) -> f64 {
        self.custom.widget().borrow().paint_micros.get()
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
