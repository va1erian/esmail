//! The Accounts window: every configured account with its state, and the
//! commands to add, edit, reconnect and remove them. Like the form it runs its
//! own `App` and asks the main window to do the work.

use win32ui::prelude::*;
use win32ui::{column, row};

use crate::app::{Msg, chrome};

/// What the window shows of one account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountRow {
    /// `AccountConfig::id`.
    pub id: String,
    /// The name shown in the folder list.
    pub name: String,
    /// The login address.
    pub address: String,
    /// Whether the account signs in with Google.
    pub google: bool,
    /// Connected, connecting, or what went wrong.
    pub status: String,
    /// The account could not sign in.
    pub failed: bool,
}

/// What the window asks the main window for.
pub enum Request {
    /// Open the form for a new account.
    Add,
    /// Open the form for this account (also how it is reconnected).
    Edit(String),
    /// Forget this account (the window has already asked).
    Remove(String),
    /// The window is gone.
    Closed,
}

/// The messages of the Accounts window.
pub enum ManageMsg {
    /// The accounts, as they are now.
    Show(Vec<AccountRow>),
    /// The selection changed.
    Selected,
    Add,
    Edit,
    Remove,
    Close,
    /// The theme to wear, and whether to keep following the system's.
    SetTheme(Theme, bool),
}

/// What opening the window needs.
pub struct Init {
    /// The accounts to start with.
    pub rows: Vec<AccountRow>,
    /// The main window's queue.
    pub host: Proxy<Msg>,
    /// Keep following the system theme.
    pub follow_system_theme: bool,
    /// `--acrylic`.
    pub acrylic: bool,
}

/// The error colour of a row whose account cannot sign in: readable on both
/// themes.
const FAILED: Color = Color::hex(0xE5484D);

struct ManageApp {
    host: Proxy<Msg>,
    list: ListView<AccountRow, ManageMsg>,
    edit: Button<ManageMsg>,
    remove: Button<ManageMsg>,
    rows: Vec<AccountRow>,
    /// Kept alive: a widget's window is destroyed with it.
    _widgets: Vec<Box<dyn AsControl>>,
}

/// Opens the window owned by `ui`'s window.
pub fn open(ui: &Ui<Msg>, init: Init) -> win32ui::Result<WindowHandle<ManageMsg>> {
    let spec = chrome::acrylic(WindowSpec::new("Accounts").size(dip(640.0), dip(380.0)), init.acrylic);
    ui.open_window::<ManageApp, _>(spec, move |ui| ManageApp::new(ui, init))
}

fn button(ui: &mut Ui<ManageMsg>, text: &str, msg: fn() -> ManageMsg) -> win32ui::Result<Button<ManageMsg>> {
    Ok(Button::new(ui, text)?.on_click(move || Some(msg())))
}

impl ManageApp {
    fn new(ui: &mut Ui<ManageMsg>, init: Init) -> ManageApp {
        let list = ListView::new(ui)
            .expect("accounts list")
            .column("Account", dip(150.0), |row: &AccountRow| row.name.as_str())
            .column("Address", dip(170.0), |row: &AccountRow| row.address.as_str())
            .column("Sign in", dip(80.0), |row: &AccountRow| if row.google { "Google" } else { "Password" })
            .column("Status", Fill, |row: &AccountRow| row.status.as_str())
            .row_style(|row| if row.failed { RowStyle::new().text(FAILED) } else { RowStyle::new() })
            .on_select(|_| Some(ManageMsg::Selected))
            .on_activate(|_| Some(ManageMsg::Edit))
            .on_key(|key, _| (key == Key::DELETE).then_some(ManageMsg::Remove));
        let add = button(ui, "Add...", || ManageMsg::Add).expect("add button");
        let edit = button(ui, "Edit...", || ManageMsg::Edit).expect("edit button");
        let remove = button(ui, "Remove", || ManageMsg::Remove).expect("remove button");
        let close = button(ui, "Close", || ManageMsg::Close).expect("close button").default();
        let spacer = Label::new(ui, Rect::default(), "").expect("spacer");
        let margins = Insets::new(dip(12.0), dip(12.0) + ui.title_bar_height(), dip(12.0), dip(12.0));
        ui.set_layout(
            column![
                list.fill(1),
                row![add.width(dip(90.0)), edit.width(dip(130.0)), remove.width(dip(90.0)), spacer.fill(1), close.width(dip(90.0))].spacing(dip(8.0)).height(dip(32.0))
            ]
            .spacing(dip(10.0))
            .margins(margins),
        );
        ui.accelerator(Shortcut::key(Key::ESCAPE), || Some(ManageMsg::Close));
        ui.on_close(|| Some(ManageMsg::Close));
        ui.follow_system_theme(init.follow_system_theme);
        let mut app = ManageApp { host: init.host, list, edit, remove, rows: Vec::new(), _widgets: vec![Box::new(add), Box::new(close), Box::new(spacer)] };
        app.show(init.rows);
        app.list.focus();
        app
    }

    fn ask(&self, request: Request) {
        let _ = self.host.send(Msg::ManageRequest(request));
    }

    /// Shows `rows`, keeping the selected account selected.
    fn show(&mut self, rows: Vec<AccountRow>) {
        let selected = self.selected_id();
        let index = selected.and_then(|id| rows.iter().position(|row| row.id == id)).or((!rows.is_empty()).then_some(0));
        self.rows = rows;
        self.list.set_model(self.rows.clone());
        if let Some(index) = index {
            self.list.select(index);
        }
        self.selection_changed();
    }

    fn selected_id(&self) -> Option<String> {
        self.list.selected().and_then(|index| self.rows.as_slice().get(index)).map(|row| row.id.clone())
    }

    /// The buttons that act on an account follow the selection.
    fn selection_changed(&self) {
        let selected = self.list.selected().and_then(|index| self.rows.as_slice().get(index));
        self.edit.set_enabled(selected.is_some());
        self.remove.set_enabled(selected.is_some());
        let label = match selected {
            Some(row) if row.failed && row.google => "Sign in again...",
            Some(row) if row.failed => "Reconnect...",
            _ => "Edit...",
        };
        self.edit.set_text(label);
    }

    fn remove(&self, ui: &Ui<ManageMsg>) {
        let Some(row) = self.list.selected().and_then(|index| self.rows.as_slice().get(index)) else { return };
        let confirmed = TaskDialog::new(format!("Remove {}?", row.name))
            .content("The account and its saved password are forgotten and its mail is removed from this computer's cache. The mail on the server is untouched.")
            .buttons([("Remove", true), ("Cancel", false)])
            .default(false)
            .icon(TaskDialogIcon::Warning)
            .show(ui)
            .is_ok_and(|(answer, _)| answer);
        if confirmed {
            self.ask(Request::Remove(row.id.clone()));
        }
    }
}

impl App for ManageApp {
    type Msg = ManageMsg;

    fn update(&mut self, msg: ManageMsg, ui: &mut Ui<ManageMsg>) {
        match msg {
            ManageMsg::Show(rows) => self.show(rows),
            ManageMsg::Selected => self.selection_changed(),
            ManageMsg::Add => self.ask(Request::Add),
            ManageMsg::Edit => {
                if let Some(id) = self.selected_id() {
                    self.ask(Request::Edit(id));
                }
            }
            ManageMsg::Remove => self.remove(ui),
            ManageMsg::Close => ui.close(),
            ManageMsg::SetTheme(theme, follow) => {
                ui.follow_system_theme(follow);
                ui.set_theme(theme);
            }
        }
    }
}

impl Drop for ManageApp {
    fn drop(&mut self) {
        self.ask(Request::Closed);
    }
}
