//! The strip under the toolbar that says an account needs signing in again:
//! the same message whatever is open in the reading pane, unlike the reading
//! pane's own notice, which a selected message covers.

use std::cell::RefCell;

use esmail::oauth::now_unix;
use esmail_win32::core_glue::{Notice, account_notice};
use win32ui::prelude::*;

use super::{App, Msg};

/// The strip's widgets and what they show now.
pub struct NoticeBar {
    label: Label,
    button: Button<Msg>,
    shown: RefCell<Option<Notice>>,
}

impl NoticeBar {
    pub fn new(ui: &mut Ui<Msg>) -> win32ui::Result<NoticeBar> {
        let label = Label::new(ui, Rect::new(0, 0, 0, 0), "")?;
        let button = Button::new(ui, "Sign in again...")?.on_click(|| Some(Msg::NoticeAction));
        label.set_visible(false);
        button.set_visible(false);
        Ok(NoticeBar { label, button, shown: RefCell::new(None) })
    }

    /// The strip's layout item, while there is something to say.
    pub fn layout(&self) -> Option<LayoutItem> {
        self.shown.borrow().as_ref()?;
        Some(Layout::row().margins(Insets::symmetric(dip(8.0), dip(3.0))).spacing(dip(8.0)).item(self.label.fill(1)).item(self.button.width(dip(150.0))).height(dip(38.0)))
    }

    /// The account the button acts on.
    pub fn account(&self) -> Option<usize> {
        self.shown.borrow().as_ref().map(|notice| notice.account)
    }

    /// Shows `notice` (or hides the strip). Returns whether the strip appeared
    /// or vanished, which changes the window's layout.
    fn show(&self, notice: Option<Notice>) -> bool {
        if *self.shown.borrow() == notice {
            return false;
        }
        let appeared_or_vanished = self.shown.borrow().is_some() != notice.is_some();
        if let Some(notice) = &notice {
            let more = if notice.others > 0 { format!(" (and {} more account(s))", notice.others) } else { String::new() };
            self.label.set_text(&format!("{}{more}", notice.text));
            self.button.set_text(notice.action);
        }
        self.label.set_visible(notice.is_some());
        self.button.set_visible(notice.is_some());
        self.shown.replace(notice);
        appeared_or_vanished
    }
}

impl App {
    /// Shows or hides the sign-in notice to match the accounts' state.
    pub(super) fn sync_notice(&self, ui: &Ui<Msg>) {
        let notice = account_notice(&self.config.accounts, &self.accounts.failures(), now_unix());
        if self.notice_bar.show(notice) {
            self.layout(ui);
        }
    }
}
