//! The owner-drawn widget behind a [`MessageList`](super::MessageList): its
//! state, its Direct2D painting and its mouse and keyboard handling.

use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::time::Instant;

use esmail::view_model::RowModel;
use win32ui::d2d::{D2dCanvas, RectF};
use win32ui::gdi::Canvas;
use win32ui::{CustomWidget, Input, Key, KeyResult, Modifiers, MouseButton, Point, Rect, Renderer, Theme, WidgetCx};

use crate::core_glue::compose::Kind;
use super::dirty;
use crate::events::MessageListEvent;
use crate::paint::{self, Fonts, Phases, RowVisual};
use crate::selection::navigate;
use crate::state::{ViewState, clamp_scroll, row_at, visible_range};

/// The owner-drawn widget behind a [`MessageList`](super::MessageList). All
/// mutable state lives in `Cell`/`RefCell` fields because the `CustomWidget`
/// trait hands the widget `&self`.
pub(super) struct MessageListWidget {
    pub(super) fonts: Fonts,
    pub(super) rows: RefCell<Arc<[RowModel]>>,
    pub(super) view: RefCell<ViewState>,
    /// The window scale from device to independent pixels (dpi / 96).
    scale: Cell<f32>,
    /// The viewport height in device-independent pixels, from the last paint.
    pub(super) viewport: Cell<f32>,
    /// The viewport width in device-independent pixels, from the last paint.
    width: Cell<f32>,
    /// The row under the pointer, if any.
    hover: Cell<Option<usize>>,
    /// Whether the pointer is over the hovered row's star.
    star_hover: Cell<bool>,
    /// Whether the list has the keyboard focus.
    pub(super) focused: Cell<bool>,
    /// Ctrl/Shift are tracked from key events because mouse events do not carry
    /// modifier state (see the win32ui gap in the PR).
    ctrl: Cell<bool>,
    shift: Cell<bool>,
    /// How long the last `paint_d2d` took, in microseconds (diagnostic).
    pub(super) paint_micros: Cell<f64>,
    /// The layout/draw split of the last `paint_d2d` (diagnostic).
    pub(super) phases: Cell<Phases>,
    /// Monotonic paint counter (diagnostic).
    pub(super) paint_seq: Cell<u64>,
    /// When the last `paint_d2d` began (diagnostic).
    pub(super) paint_begin: Cell<Option<Instant>>,
    /// Monotonic scroll counter (diagnostic).
    pub(super) scroll_seq: Cell<u64>,
    /// When the last scroll was applied (diagnostic).
    pub(super) scroll_at: Cell<Option<Instant>>,
    /// How many rows the last paint drew (diagnostic).
    pub(super) last_rows: Cell<usize>,
    /// When a paint first drew a row (diagnostic: time to first list content).
    pub(super) first_content: Cell<Option<Instant>>,
}

impl MessageListWidget {
    pub(super) fn new(fonts: Fonts, scale: f32) -> MessageListWidget {
        MessageListWidget {
            fonts,
            rows: RefCell::new(Arc::from([])),
            view: RefCell::new(ViewState::new()),
            scale: Cell::new(scale),
            viewport: Cell::new(0.0),
            width: Cell::new(0.0),
            hover: Cell::new(None),
            star_hover: Cell::new(false),
            focused: Cell::new(false),
            ctrl: Cell::new(false),
            shift: Cell::new(false),
            paint_micros: Cell::new(0.0),
            phases: Cell::new(Phases::default()),
            paint_seq: Cell::new(0),
            paint_begin: Cell::new(None),
            scroll_seq: Cell::new(0),
            scroll_at: Cell::new(None),
            last_rows: Cell::new(0),
            first_content: Cell::new(None),
        }
    }

    /// The row under a client point (device pixels) and whether the point is on
    /// that row's star.
    fn hit(&self, x: i32, y: i32) -> Option<(usize, bool)> {
        let scale = self.scale.get();
        let (x, y) = (x as f32 / scale, y as f32 / scale);
        let view = self.view.borrow();
        let doc_y = y + view.scroll;
        let row_height = self.fonts.row_height;
        let index = row_at(doc_y, row_height, view.len)?;
        let on_star = self
            .rows
            .borrow()
            .get(index)
            .is_some_and(|row| paint::star_hit(row, &self.fonts, self.width.get(), x, doc_y - index as f32 * row_height));
        Some((index, on_star))
    }

    /// Repaints just `rows` (those on screen; the host clips the paint).
    fn invalidate_rows(&self, cx: &WidgetCx<MessageListEvent>, rows: impl IntoIterator<Item = usize>) {
        let scroll = self.view.borrow().scroll;
        for row in rows {
            cx.invalidate_rect(dirty::row_rect(row, self.fonts.row_height, scroll, self.scale.get(), self.width.get() * self.scale.get()));
        }
    }

    fn emit_selected(&self, cx: &WidgetCx<MessageListEvent>) {
        cx.emit(MessageListEvent::Selected(self.view.borrow().selection.selected.clone()));
    }

    fn mouse_down(&self, x: i32, y: i32, cx: &WidgetCx<MessageListEvent>) {
        cx.focus();
        let Some((index, on_star)) = self.hit(x, y) else { return };
        if on_star {
            cx.emit(MessageListEvent::ToggleFlag(index));
            return;
        }
        let before = self.view.borrow().selection.clone();
        {
            let mut view = self.view.borrow_mut();
            let selection = &mut view.selection;
            if self.ctrl.get() {
                selection.ctrl_click(index);
            } else if self.shift.get() {
                selection.shift_click(index);
            } else {
                selection.click(index);
            }
        }
        self.emit_selected(cx);
        self.invalidate_rows(cx, dirty::changed_rows(&before, &self.view.borrow().selection));
    }

    fn context_menu(&self, x: i32, y: i32, cx: &WidgetCx<MessageListEvent>) {
        cx.focus();
        let Some((index, _)) = self.hit(x, y) else { return };
        // A right-click selects the row when it is not part of the selection,
        // then requests a context menu (Explorer behaviour).
        let outside = !self.view.borrow().selection.contains(index);
        if outside {
            self.view.borrow_mut().selection.click(index);
            self.emit_selected(cx);
        }
        cx.emit(MessageListEvent::Context { row: index, at: Point::new(x, y) });
    }

    /// Tracks the hovered row and star, repainting only when either changed.
    fn mouse_move(&self, x: i32, y: i32, cx: &WidgetCx<MessageListEvent>) {
        let (row, star) = match self.hit(x, y) {
            Some((row, star)) => (Some(row), star),
            None => (None, false),
        };
        let previous = self.hover.replace(row);
        let row_changed = previous != row;
        let star_changed = self.star_hover.replace(star) != star;
        if row_changed || star_changed {
            self.invalidate_rows(cx, previous.into_iter().chain(row));
        }
    }

    fn key_down(&self, key: Key, modifiers: Modifiers, cx: &WidgetCx<MessageListEvent>) {
        if key == Key::CONTROL {
            self.ctrl.set(true);
        }
        if key == Key::SHIFT {
            self.shift.set(true);
        }
        let focus = self.view.borrow().selection.focus;
        if modifiers.ctrl && key == Key::A {
            let len = self.view.borrow().len;
            self.view.borrow_mut().selection.select_all(len);
            self.emit_selected(cx);
            cx.invalidate();
        } else if key == Key::DELETE {
            let selected = self.view.borrow().selection.selected.clone();
            if !selected.is_empty() {
                cx.emit(MessageListEvent::Delete(selected));
            }
        } else if key == Key::R && !modifiers.ctrl {
            cx.emit(MessageListEvent::Compose(if modifiers.shift { Kind::ReplyAll } else { Kind::Reply }));
        } else if key == Key::SPACE {
            if let Some(index) = focus {
                cx.emit(MessageListEvent::ToggleFlag(index));
            }
        } else if key == Key::F && !modifiers.ctrl {
            cx.emit(MessageListEvent::Compose(Kind::Forward));
        }
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
        let viewport = bounds.height();
        let row_height = self.fonts.row_height;
        self.viewport.set(viewport);
        self.width.set(bounds.width());
        let started = Instant::now();
        self.paint_begin.set(Some(started));

        let mut view = self.view.borrow_mut();
        // The host translates the canvas by its own (clamped) offset; keep the
        // mirror in range so it can index the model whatever the host reports.
        view.scroll = clamp_scroll(view.scroll, row_height, viewport, view.len);
        let range = visible_range(view.scroll, viewport, row_height, view.len);
        self.last_rows.set(range.len());
        if !range.is_empty() && self.first_content.get().is_none() {
            self.first_content.set(Some(started));
        }

        let rows = self.rows.borrow();
        let selection = &view.selection;
        let focused = self.focused.get();
        let hover = self.hover.get();
        let star_hover = self.star_hover.get();
        let mut phases = Phases::default();
        for index in range {
            let Some(row) = rows.get(index) else { break };
            let top = index as f32 * row_height;
            let rect = RectF::new(0.0, top, bounds.width(), top + row_height);
            let hovered = hover == Some(index);
            let visual = RowVisual { selected: selection.contains(index), hovered, star_hovered: hovered && star_hover, focused };
            paint::paint_row(canvas, row, rect, visual, &self.fonts, theme, &mut phases);
        }
        drop(rows);
        drop(view);
        self.paint_micros.set(started.elapsed().as_secs_f64() * 1_000_000.0);
        self.phases.set(phases);
        self.paint_seq.set(self.paint_seq.get() + 1);
    }

    // Without this the window's dialog-style navigation takes the arrow keys to
    // move between controls before the list ever sees them.
    fn wants_arrow_keys(&self) -> bool {
        true
    }

    /// The scroll host offers the navigation keys here before it scrolls with
    /// them. Moving the focused row and claiming the key stops the host from
    /// scrolling too; the list brings the new focus into view itself (see
    /// `MessageListEvent::Focus`).
    fn key(&self, key: Key, modifiers: Modifiers, cx: &mut WidgetCx<MessageListEvent>) -> KeyResult {
        let page = ((self.viewport.get() / self.fonts.row_height) as usize).max(1);
        let before = self.view.borrow().selection.clone();
        let moved = {
            let mut view = self.view.borrow_mut();
            let len = view.len;
            navigate(&mut view.selection, key, modifiers.ctrl, modifiers.shift, len, page)
        };
        let Some(moved) = moved else { return KeyResult::Ignored };
        if moved.selection_changed {
            self.emit_selected(cx);
        }
        cx.emit(MessageListEvent::Focus(moved.focus));
        self.invalidate_rows(cx, dirty::changed_rows(&before, &self.view.borrow().selection));
        KeyResult::Handled
    }

    fn input(&self, input: Input, cx: &mut WidgetCx<MessageListEvent>) {
        match input {
            Input::MouseDown { x, y, button: MouseButton::Left } => self.mouse_down(x, y, cx),
            Input::MouseDown { x, y, button: MouseButton::Right } => self.context_menu(x, y, cx),
            Input::MouseDoubleClick { x, y, button: MouseButton::Left } => {
                if let Some((index, false)) = self.hit(x, y) {
                    cx.emit(MessageListEvent::Open(index));
                }
            }
            Input::MouseMove { x, y } => self.mouse_move(x, y, cx),
            Input::MouseLeave => {
                self.hover.set(None);
                self.star_hover.set(false);
                cx.invalidate();
            }
            Input::KeyDown { key, modifiers, .. } => self.key_down(key, modifiers, cx),
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
