//! Replacing and editing the list's model: a new mailbox, an older page, a
//! changed row, new mail, a message that left.

use std::sync::Arc;

use esmail::view_model::RowModel;
use win32ui::dip;

use super::MessageList;
use crate::state::ViewState;

impl<M: 'static> MessageList<M> {
    /// Replaces the model with a longer one that starts with the same rows (an
    /// older page appended), keeping the scroll position and the selection.
    pub fn extend_rows(&self, rows: Arc<[RowModel]>) {
        let len = rows.len();
        let widget = self.custom.widget();
        *widget.borrow().rows.borrow_mut() = rows;
        widget.borrow().view.borrow_mut().recount(len);
        self.resize_content();
    }

    /// Replaces the model. This is a new mailbox: the selection is dropped and
    /// the view returns to the top.
    pub fn set_rows(&self, rows: Arc<[RowModel]>) {
        let len = rows.len();
        {
            let widget = self.custom.widget();
            let w = widget.borrow_mut();
            *w.rows.borrow_mut() = rows;
            w.view.borrow_mut().set_rows(len);
        }
        self.resize_content();
        self.custom.scroll_to(dip(0.0));
    }

    /// Replaces the model with one of the same length whose rows changed (a
    /// flag or seen toggle). Scroll and selection are untouched.
    pub fn update_rows(&self, rows: Arc<[RowModel]>) {
        *self.custom.widget().borrow().rows.borrow_mut() = rows;
        self.custom.invalidate();
    }

    /// Replaces the model with one that has `count` more rows starting at `at`
    /// (new mail). The selection follows its messages and a scrolled view keeps
    /// showing the same ones.
    pub fn insert_rows(&self, rows: Arc<[RowModel]>, at: usize, count: usize) {
        self.reshape(rows, |view, row_height| view.insert(at, count, row_height));
    }

    /// Replaces the model with one that lacks the `count` rows that were at
    /// `at` (moved or deleted). The selection follows its messages.
    pub fn remove_rows(&self, rows: Arc<[RowModel]>, at: usize, count: usize) {
        self.reshape(rows, |view, row_height| view.remove(at, count, row_height));
    }

    fn reshape(&self, rows: Arc<[RowModel]>, apply: impl FnOnce(&mut ViewState, f32)) {
        let widget = self.custom.widget();
        let (scroll, changed) = {
            let w = widget.borrow();
            *w.rows.borrow_mut() = rows;
            let mut view = w.view.borrow_mut();
            let before = view.scroll;
            apply(&mut view, w.fonts.row_height);
            (view.scroll, view.scroll != before)
        };
        self.resize_content();
        if changed {
            self.custom.scroll_to(dip(scroll));
        }
    }

    /// Tells the scroll host how tall the model is now, and repaints.
    fn resize_content(&self) {
        let widget = self.custom.widget();
        let widget = widget.borrow();
        let len = widget.view.borrow().len;
        self.custom.set_content_height(dip(len as f32 * widget.fonts.row_height));
        self.custom.invalidate();
    }
}
