//! The Drafts and Outbox windows: one list of messages that are not sent yet,
//! and the commands that act on the selected one. Like the Accounts window each
//! runs its own `App` and asks the main window to do the work.

use xui_win32::prelude::*;
use xui_win32::{column, row};

use esmail_win32::core_glue::QueueRow;

use super::{QueueKind, Request};
use crate::app::{Msg, chrome};

/// The messages of a queue window.
pub enum QueueMsg {
    /// The messages, as they are now.
    Show(Vec<QueueRow>),
    /// The selection changed.
    Selected,
    Open,
    Retry,
    Delete,
    Close,
    /// The theme to wear, and whether to keep following the system's.
    SetTheme(Theme, bool),
}

/// What opening the window needs.
pub struct Init {
    pub kind: QueueKind,
    /// The main window's queue.
    pub host: Proxy<Msg>,
    /// Keep following the system theme.
    pub follow_system_theme: bool,
    /// `--acrylic`.
    pub acrylic: bool,
}

/// The error colour of a message that failed to send: readable on both themes.
const FAILED: Color = Color::hex(0xE5484D);

struct QueueApp {
    kind: QueueKind,
    host: Proxy<Msg>,
    list: ListView<QueueRow, QueueMsg>,
    open: Button<QueueMsg>,
    retry: Option<Button<QueueMsg>>,
    delete: Button<QueueMsg>,
    summary: Label,
    rows: Vec<QueueRow>,
    /// Kept alive: a widget's window is destroyed with it.
    _widgets: Vec<Box<dyn AsControl>>,
}

/// Opens the window owned by `ui`'s window.
pub fn open(ui: &Ui<Msg>, init: Init) -> xui_win32::Result<WindowHandle<QueueMsg>> {
    let (title, size) = match init.kind {
        QueueKind::Drafts => ("Drafts", (dip(640.0), dip(380.0))),
        QueueKind::Outbox => ("Outbox", (dip(900.0), dip(380.0))),
    };
    let spec = chrome::acrylic(WindowSpec::new(title).size(size.0, size.1), init.acrylic);
    ui.open_window::<QueueApp, _>(spec, move |ui| QueueApp::new(ui, init))
}

fn button(ui: &mut Ui<QueueMsg>, text: &str, msg: fn() -> QueueMsg) -> xui_win32::Result<Button<QueueMsg>> {
    Ok(Button::new(ui, text)?.on_click(move || Some(msg())))
}

impl QueueApp {
    fn new(ui: &mut Ui<QueueMsg>, init: Init) -> QueueApp {
        let kind = init.kind;
        let list = ListView::new(ui)
            .expect("message list")
            .column("Subject", dip(220.0), |row: &QueueRow| row.subject.as_str())
            .column("To", dip(190.0), |row: &QueueRow| row.to.as_str());
        let list = match kind {
            QueueKind::Drafts => list.column("Saved", Fill, |row: &QueueRow| row.detail.as_str()),
            QueueKind::Outbox => list.column("Account", dip(110.0), |row: &QueueRow| row.detail.as_str()).column("Status", Fill, |row: &QueueRow| row.status.as_str()),
        }
        .row_style(|row| if row.failed { RowStyle::new().text(FAILED) } else { RowStyle::new() })
        .on_select(|_| Some(QueueMsg::Selected))
        .on_activate(|_| Some(QueueMsg::Open))
        .on_key(|key, _| (key == Key::DELETE).then_some(QueueMsg::Delete));

        let open = button(ui, if kind == QueueKind::Drafts { "Open" } else { "Edit" }, || QueueMsg::Open).expect("open button");
        let retry = (kind == QueueKind::Outbox).then(|| button(ui, "Retry now", || QueueMsg::Retry).expect("retry button"));
        let delete = button(ui, "Delete", || QueueMsg::Delete).expect("delete button");
        let close = button(ui, "Close", || QueueMsg::Close).expect("close button").default();
        let summary = Label::new(ui, Rect::default(), "").expect("summary");
        open.set_tooltip(match kind {
            QueueKind::Drafts => "Continue writing this draft",
            QueueKind::Outbox => "Take this message out of the outbox and edit it",
        });
        delete.set_tooltip("Discard this message");

        let mut buttons = row![open.width(dip(90.0))];
        if let Some(retry) = &retry {
            buttons = buttons.item(retry.width(dip(110.0)));
        }
        let buttons = buttons.item(delete.width(dip(90.0))).item(summary.fill(1)).item(close.width(dip(90.0))).spacing(dip(8.0)).height(dip(32.0));
        let margins = Insets::new(dip(12.0), dip(12.0) + ui.title_bar_height(), dip(12.0), dip(12.0));
        ui.set_layout(column![list.fill(1), buttons].spacing(dip(10.0)).margins(margins));
        ui.accelerator(Shortcut::key(Key::ESCAPE), || Some(QueueMsg::Close));
        ui.on_close(|| Some(QueueMsg::Close));
        ui.follow_system_theme(init.follow_system_theme);
        list.focus();
        let app = QueueApp { kind, host: init.host, list, open, retry, delete, summary, rows: Vec::new(), _widgets: vec![Box::new(close)] };
        app.selection_changed();
        app
    }

    fn ask(&self, request: Request) {
        let _ = self.host.send(Msg::QueueRequest(self.kind, request));
    }

    /// Shows `rows`, keeping the selected message selected.
    fn show(&mut self, rows: Vec<QueueRow>) {
        let selected = self.selected_id();
        let index = selected.and_then(|id| rows.iter().position(|row| row.id == id)).or((!rows.is_empty()).then_some(0));
        self.rows = rows;
        self.list.set_model(self.rows.clone());
        if let Some(index) = index {
            self.list.select(index);
        }
        self.summary.set_text(&match (self.kind, self.rows.len()) {
            (QueueKind::Drafts, 0) => "No saved drafts.".to_string(),
            (QueueKind::Outbox, 0) => "Nothing queued to send.".to_string(),
            (_, count) => format!("{count} message(s)"),
        });
        self.selection_changed();
    }

    fn selected(&self) -> Option<&QueueRow> {
        self.list.selected().and_then(|index| self.rows.as_slice().get(index))
    }

    fn selected_id(&self) -> Option<i64> {
        self.selected().map(|row| row.id)
    }

    /// The buttons that act on a message follow the selection.
    fn selection_changed(&self) {
        let any = self.selected().is_some();
        self.open.set_enabled(any);
        self.delete.set_enabled(any);
        if let Some(retry) = &self.retry {
            retry.set_enabled(any);
        }
    }

    fn delete(&self, ui: &Ui<QueueMsg>) {
        let Some(row) = self.selected() else { return };
        let confirmed = TaskDialog::new(format!("Discard \"{}\"?", row.subject))
            .content(match self.kind {
                QueueKind::Drafts => "The draft is deleted.",
                QueueKind::Outbox => "The message is removed from the outbox and will not be sent.",
            })
            .buttons([("Discard", true), ("Cancel", false)])
            .default(false)
            .icon(TaskDialogIcon::Warning)
            .show(ui)
            .is_ok_and(|(answer, _)| answer);
        if confirmed {
            self.ask(Request::Delete(row.id));
        }
    }
}

impl App for QueueApp {
    type Msg = QueueMsg;

    fn update(&mut self, msg: QueueMsg, ui: &mut Ui<QueueMsg>) {
        match msg {
            QueueMsg::Show(rows) => self.show(rows),
            QueueMsg::Selected => self.selection_changed(),
            QueueMsg::Open => {
                if let Some(id) = self.selected_id() {
                    self.ask(Request::Open(id));
                }
            }
            QueueMsg::Retry => {
                if let Some(id) = self.selected_id() {
                    self.ask(Request::Retry(id));
                }
            }
            QueueMsg::Delete => self.delete(ui),
            QueueMsg::Close => ui.close(),
            QueueMsg::SetTheme(theme, follow) => {
                ui.follow_system_theme(follow);
                ui.set_theme(theme);
            }
        }
    }
}

impl Drop for QueueApp {
    fn drop(&mut self) {
        self.ask(Request::Closed);
    }
}
