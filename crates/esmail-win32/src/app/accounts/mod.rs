//! Accounts, from the main window's side: the account form and the Accounts
//! window it opens, and what they ask for.
//!
//! Trying a connection, the Google sign-in and saving run on a small runtime of
//! their own (`connect`), not on the mail core's, because a change to the
//! accounts replaces the core: its sessions are numbered by the account's
//! position, so the simple way to keep every index right is to start again.
//! The outcome comes back as a [`Msg::AccountOutcome`].

mod connect;
mod form;
mod manage;
mod status;
mod window;

use esmail::config::{AuthKind, Config};
use esmail::imap::ImapEvent;
use esmail::secrets;
use esmail_win32::core_glue::account_form::AccountForm;
use esmail_win32::core_glue::links::reconnect_href;
use esmail_win32::core_glue::{Core, FolderTree};
use win32ui::prelude::*;

use super::{App, Msg};
use manage::ManageMsg;
pub use manage::Request as ManageRequest;
use status::Status;
pub use window::Request as FormRequest;
use window::{FormMsg, Mode};

/// What the work behind the form reported.
pub enum Outcome {
    /// Progress worth showing.
    Working(String),
    /// The browser is open on Google's consent page.
    AwaitingBrowser,
    /// The browser could not be opened; the address to open by hand.
    BrowserUnavailable(String),
    /// It did not work: why, in words for the form.
    Failed(String),
    /// The account (with this id) is saved; this is the configuration now.
    Saved { config: Config, id: String },
    /// The account with this id is gone; this is the configuration now.
    Removed { config: Config, id: String },
}

/// The account windows and the work in flight.
#[derive(Default)]
pub struct Accounts {
    connect: connect::Worker,
    form: Option<WindowHandle<FormMsg>>,
    manage: Option<WindowHandle<ManageMsg>>,
    /// One entry per account of the running core.
    status: Vec<Status>,
}

impl Accounts {
    /// Every account of a freshly started core is connecting.
    pub fn reset_status(&mut self, accounts: usize) {
        self.status = vec![Status::Connecting; accounts];
    }

    /// For each account, why it cannot sign in, if it cannot.
    pub fn failures(&self) -> Vec<Option<String>> {
        self.status.iter().map(|status| status.failure().map(str::to_string)).collect()
    }

    /// Tells whichever account windows are open which theme to wear.
    pub fn set_theme(&self, theme: Theme, follow_system: bool) {
        if let Some(form) = &self.form {
            let _ = form.send(FormMsg::SetTheme(theme, follow_system));
        }
        if let Some(manage) = &self.manage {
            let _ = manage.send(ManageMsg::SetTheme(theme, follow_system));
        }
    }

    /// The form window while it is open.
    pub fn form_window(&self) -> Option<&WindowHandle<FormMsg>> {
        self.form.as_ref().filter(|form| form.is_alive())
    }
}

impl App {
    /// File > Add account..., and the first-run form.
    pub(super) fn add_account(&mut self, ui: &Ui<Msg>) {
        let first_run = self.core.accounts().is_empty();
        self.open_form(ui, Mode::New { first_run }, AccountForm::default(), false);
    }

    /// The form for the saved account at `index` (also how it is reconnected).
    pub(super) fn edit_account(&mut self, ui: &Ui<Msg>, index: usize) {
        let Some(account) = self.config.accounts.as_slice().get(index) else { return };
        let password = match account.auth {
            AuthKind::Password => secrets::get_password(&account.id, "imap").map(|secret| secrecy_text(&secret)).unwrap_or_default(),
            AuthKind::GoogleOAuth => String::new(),
        };
        let google = account.auth == AuthKind::GoogleOAuth;
        let form = AccountForm::from_account(account, password);
        self.open_form(ui, Mode::Edit { id: account.id.clone() }, form, google);
    }

    fn open_form(&mut self, ui: &Ui<Msg>, mode: Mode, form: AccountForm, can_sign_in_again: bool) {
        if !self.editable {
            return self.set_status("Accounts cannot be changed while a --profile is open.");
        }
        if self.accounts.form_window().is_some() {
            return self.set_status("The account window is already open.");
        }
        let init = window::Init { mode, form, host: ui.proxy(), can_sign_in_again, follow_system_theme: self.theme == esmail_win32::core_glue::ThemeChoice::System, acrylic: self.acrylic };
        match window::open(ui, init) {
            Ok(handle) => self.accounts.form = Some(handle),
            Err(error) => self.banner(&format!("Could not open the account window: {error}")),
        }
    }

    /// File > Accounts...
    pub(super) fn manage_accounts(&mut self, ui: &Ui<Msg>) {
        if self.accounts.manage.as_ref().is_some_and(WindowHandle::is_alive) {
            return;
        }
        let init = manage::Init {
            rows: status::rows(&self.config.accounts, &self.accounts.status),
            host: ui.proxy(),
            follow_system_theme: self.theme == esmail_win32::core_glue::ThemeChoice::System,
            acrylic: self.acrylic,
        };
        match manage::open(ui, init) {
            Ok(handle) => self.accounts.manage = Some(handle),
            Err(error) => self.banner(&format!("Could not open the Accounts window: {error}")),
        }
    }

    /// The form asked for something.
    pub(super) fn form_request(&mut self, ui: &Ui<Msg>, request: FormRequest) {
        match request {
            FormRequest::Connect { form, editing, sign_in_again } => self.connect(ui, form, editing, sign_in_again),
            FormRequest::Abort => self.accounts.connect.abort(),
            FormRequest::Closed => self.accounts.form = None,
        }
    }

    /// The Accounts window asked for something.
    pub(super) fn manage_request(&mut self, ui: &Ui<Msg>, request: ManageRequest) {
        match request {
            ManageRequest::Add => self.add_account(ui),
            ManageRequest::Edit(id) => {
                if let Some(index) = self.config.accounts.iter().position(|account| account.id == id) {
                    self.edit_account(ui, index);
                }
            }
            ManageRequest::Remove(id) => self.remove_account(ui, id),
            ManageRequest::Closed => self.accounts.manage = None,
        }
    }

    /// The work behind the form reported.
    pub(super) fn account_outcome(&mut self, ui: &Ui<Msg>, outcome: Outcome) {
        let tell = |form: &Option<WindowHandle<FormMsg>>, msg| {
            if let Some(form) = form {
                let _ = form.send(msg);
            }
        };
        match outcome {
            Outcome::Working(text) => tell(&self.accounts.form, FormMsg::Working(text)),
            Outcome::AwaitingBrowser => tell(&self.accounts.form, FormMsg::AwaitingBrowser),
            Outcome::BrowserUnavailable(url) => tell(&self.accounts.form, FormMsg::BrowserUnavailable(url)),
            Outcome::Failed(error) => {
                self.accounts.connect.finished();
                match &self.accounts.form {
                    Some(form) => tell(&Some(form.clone()), FormMsg::Failed(error)),
                    None => self.banner(&error),
                }
            }
            Outcome::Saved { config, id } => {
                self.accounts.connect.finished();
                if let Some(form) = self.accounts.form.take() {
                    form.close();
                }
                self.reload_accounts(ui, config);
                if let Some(index) = self.config.accounts.iter().position(|account| account.id == id) {
                    self.wanted_account = index;
                    self.wanted_folder = None;
                    self.open_from_cache(ui);
                    self.set_status(&format!("Signing in {}...", self.config.accounts[index].display_name));
                }
            }
            Outcome::Removed { config, id } => {
                self.reload_accounts(ui, config);
                self.core.cache().remove_account(&id);
                self.set_status("Account removed");
            }
        }
    }

    fn remove_account(&mut self, ui: &Ui<Msg>, id: String) {
        if !self.editable {
            return self.set_status("Accounts cannot be changed while a --profile is open.");
        }
        if self.composes.any_open() {
            return self.banner("Close the message windows before removing an account.");
        }
        connect::remove(ui, self.config.clone(), id);
    }

    /// Starts again with `config`: every account gets a fresh session, and the
    /// folder pane, list and reading pane forget what the old ones showed. Message
    /// windows hold account numbers of the old core, so none may be open.
    fn reload_accounts(&mut self, ui: &Ui<Msg>, config: Config) {
        self.config = config;
        let waker = super::waker(ui);
        let (core, issues) = match Core::start(&self.config, waker, self.notify.clone()) {
            Ok(started) => started,
            Err(error) => return self.banner(&format!("Could not restart the mail core: {error}")),
        };
        self.core = core;
        self.accounts.reset_status(self.config.accounts.len());
        *self.folders.borrow_mut() = FolderTree::new(self.config.accounts.iter().map(|account| account.display_name.clone()));
        self.opened_accounts.clear();
        self.tree.refresh();
        self.clear_view(ui);
        for issue in issues {
            self.account_failed(issue.account, issue.message);
        }
        for account in 0..self.core.accounts().len() {
            self.core.cache().load_mailboxes(account);
        }
        if self.core.accounts().is_empty() {
            self.reader.show_notice("No accounts are set up. Use File > Add account... to add one.");
        }
        self.refresh_manage_window();
    }

    /// Empties the list and the reading pane, and forgets the open folder.
    fn clear_view(&mut self, ui: &Ui<Msg>) {
        self.bodies.cancel();
        self.selected = None;
        self.selected_in = None;
        self.seen.cancel(ui);
        self.search.reset(ui);
        self.search_edit.set_text("");
        self.list.set_rows(std::sync::Arc::from([]));
        self.reader.show_notice("Select a message to read it.");
        self.open = None;
        self.message_shown = false;
        self.refresh_title(ui);
    }

    /// Notes what `event` means for the account's status.
    pub(super) fn track_status(&mut self, account: usize, event: &ImapEvent) {
        let Some(current) = self.accounts.status.as_slice().get(account) else { return };
        let Some(next) = current.after(event) else { return };
        if let Status::Failed(error) = &next {
            self.account_failed(account, error.clone());
        } else {
            self.accounts.status[account] = next;
            self.refresh_manage_window();
        }
    }

    /// The account cannot sign in: says so in the status bar and, while no
    /// message covers it, in the reading pane with a link that opens the form.
    pub(in crate::app) fn account_failed(&mut self, account: usize, error: String) {
        let Some(name) = self.config.accounts.as_slice().get(account).map(|a| a.display_name.clone()) else { return };
        let text = format!("{name}: {error}");
        self.set_status(&format!("Error: {text} (File > Accounts... to reconnect)"));
        if self.selected.is_none() {
            self.reader.show_notice_with_link(&text, "Reconnect...", &reconnect_href(account));
        }
        if let Some(status) = self.accounts.status.get_mut(account) {
            *status = Status::Failed(error);
        }
        self.refresh_manage_window();
    }

    /// A command could not be sent because the account has no session: says why,
    /// with the reconnect link when the account failed to sign in.
    pub(in crate::app) fn account_unavailable(&mut self, account: usize) {
        match self.accounts.status.as_slice().get(account) {
            Some(Status::Failed(error)) => {
                let error = error.clone();
                self.account_failed(account, error);
            }
            _ => self.banner("this account is not connected"),
        }
    }

    fn refresh_manage_window(&self) {
        if let Some(manage) = &self.accounts.manage {
            let _ = manage.send(ManageMsg::Show(status::rows(&self.config.accounts, &self.accounts.status)));
        }
    }

    /// The reading pane's link to reconnect an account was clicked.
    pub(super) fn reconnect_account(&mut self, ui: &Ui<Msg>, account: usize) {
        self.edit_account(ui, account);
    }
}

fn secrecy_text(secret: &secrecy::SecretString) -> String {
    secrecy::ExposeSecret::expose_secret(secret).to_string()
}
