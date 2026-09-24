//! The account form window: one secondary window that adds an account or edits
//! one. Like a compose window it runs its own `App` and never touches the mail
//! core: it asks the main window (through a `Proxy`) to try the connection and
//! save the account, and the main window answers with the [`FormMsg`]s that
//! report progress or the failure, so the form stays filled for a retry.

use esmail::config::AuthKind;
use esmail_win32::core_glue::account_form::{AccountForm, Detection, Field, GMAIL_IMAP_HOST, detect, security_for_smtp_port};
use win32ui::prelude::*;

use super::form::{View, Widgets};
use crate::app::{Msg, chrome};

/// What the window is for.
#[derive(Clone, PartialEq, Eq)]
pub enum Mode {
    /// A new account; `first_run` when there is no account yet.
    New { first_run: bool },
    /// The saved account with this id.
    Edit { id: String },
}

/// What the form asks the main window for.
pub enum Request {
    /// Try these settings and, when they work, save the account.
    Connect {
        /// What the user typed.
        form: AccountForm,
        /// The saved account being edited, if any.
        editing: Option<String>,
        /// Go through Google's consent page even if a token is saved.
        sign_in_again: bool,
    },
    /// Stop trying (the user cancelled, or closed the window).
    Abort,
    /// The window is gone.
    Closed,
}

/// The messages of an account form window.
pub enum FormMsg {
    /// The email address changed.
    EmailChanged(String),
    /// The email field gained or lost the keyboard focus.
    EmailFocus(bool),
    /// The password (masked or shown) changed.
    PasswordChanged(String),
    /// The sign-in method changed.
    AuthChanged(AuthKind),
    /// "Show" next to the password.
    ShowPassword(bool),
    /// "Show server settings".
    ShowServers(bool),
    /// A server field changed.
    ServersEdited,
    /// Connect (Enter).
    Connect,
    /// Go through Google's consent page again.
    SignInAgain,
    /// Stop waiting for the browser.
    CancelSignIn,
    /// Cancel (Esc) or the close box.
    Cancel,
    /// The main window is trying the connection.
    Working(String),
    /// The browser is open on Google's consent page.
    AwaitingBrowser,
    /// The browser could not be opened; this is the address to open by hand.
    BrowserUnavailable(String),
    /// The connection failed; the form stays as it is.
    Failed(String),
    /// The theme to wear, and whether to keep following the system's.
    SetTheme(Theme, bool),
}

/// What opening a window needs.
pub struct Init {
    /// What the window is for.
    pub mode: Mode,
    /// The fields to start with.
    pub form: AccountForm,
    /// The main window's queue.
    pub host: Proxy<Msg>,
    /// The sign-in that saved the account can be repeated (a Google account).
    pub can_sign_in_again: bool,
    /// Keep following the system theme.
    pub follow_system_theme: bool,
    /// `--acrylic`.
    pub acrylic: bool,
}

/// The server fields as last filled in by a preset, to tell the user's edits
/// from the ones the form made itself.
#[derive(Clone, PartialEq, Default)]
struct Filled {
    imap_host: String,
    imap_port: String,
    smtp_host: String,
    smtp_port: String,
    security: Option<esmail::config::TlsMode>,
}

struct FormApp {
    widgets: Widgets,
    mode: Mode,
    host: Proxy<Msg>,
    view: View,
    filled: Filled,
    /// The user changed a server field: presets no longer overwrite them.
    servers_edited: bool,
    /// The main window is trying the connection.
    busy: bool,
}

/// Opens the form owned by `ui`'s window.
pub fn open(ui: &Ui<Msg>, init: Init) -> win32ui::Result<WindowHandle<FormMsg>> {
    let title = match &init.mode {
        Mode::New { first_run: true } => "Welcome to esMail",
        Mode::New { first_run: false } => "Add account",
        Mode::Edit { .. } => "Edit account",
    };
    let spec = chrome::acrylic(WindowSpec::new(title).size(dip(560.0), dip(560.0)), init.acrylic);
    ui.open_window::<FormApp, _>(spec, move |ui| FormApp::new(ui, init))
}

impl FormApp {
    fn new(ui: &mut Ui<FormMsg>, init: Init) -> FormApp {
        let widgets = Widgets::build(ui).expect("account form widgets");
        let editing = matches!(init.mode, Mode::Edit { .. });
        let first_run = init.mode == Mode::New { first_run: true };
        let form = &init.form;
        widgets.name.set_text(&form.display_name);
        widgets.email.set_text(&form.email);
        widgets.auth.set_selected(&form.auth);
        widgets.password.set_text(&form.password);
        widgets.revealed.set_text(&form.password);
        widgets.imap_host.set_text(&form.imap_host);
        widgets.imap_port.set_text(&form.imap_port);
        widgets.smtp_host.set_text(&form.smtp_host);
        widgets.smtp_port.set_text(&form.smtp_port);
        widgets.security.set_selected(&form.smtp_tls);
        if first_run {
            widgets.intro.set_text("Welcome to esMail. Add your email account to get started. Your password is kept in the Windows credential store, never in a file.");
        }
        let view = View {
            intro: first_run,
            password: form.auth == AuthKind::Password,
            servers: editing,
            can_sign_in_again: init.can_sign_in_again,
            ..View::default()
        };
        widgets.show_servers.set_checked(view.servers);
        ui.accelerator(Shortcut::key(Key::ESCAPE), || Some(FormMsg::Cancel));
        ui.on_close(|| Some(FormMsg::Cancel));
        ui.follow_system_theme(init.follow_system_theme);
        let mut app = FormApp {
            widgets,
            mode: init.mode,
            host: init.host,
            view,
            filled: Filled::default(),
            servers_edited: editing,
            busy: false,
        };
        app.filled = app.server_fields();
        app.widgets.arrange(ui, app.view);
        app.update_note();
        if editing && form.auth == AuthKind::Password {
            app.widgets.password.focus();
        } else {
            app.widgets.email.focus();
        }
        app
    }

    fn server_fields(&self) -> Filled {
        let w = &self.widgets;
        Filled {
            imap_host: w.imap_host.text(),
            imap_port: w.imap_port.text(),
            smtp_host: w.smtp_host.text(),
            smtp_port: w.smtp_port.text(),
            security: w.security.selected().copied(),
        }
    }

    /// The form as typed.
    fn form(&self) -> AccountForm {
        let w = &self.widgets;
        let fields = self.server_fields();
        AccountForm {
            display_name: w.name.text(),
            email: w.email.text(),
            auth: w.auth.selected_value().unwrap_or(AuthKind::Password),
            password: w.password.text(),
            imap_host: fields.imap_host,
            imap_port: fields.imap_port,
            smtp_host: fields.smtp_host,
            smtp_port: fields.smtp_port,
            smtp_tls: fields.security.unwrap_or(esmail::config::TlsMode::Ssl),
        }
    }

    fn ask(&self, request: Request) {
        let _ = self.host.send(Msg::AccountRequest(request));
    }

    fn show_status(&self, ui: &Ui<FormMsg>, text: &str) {
        self.widgets.status.set_text(text);
        ui.relayout();
    }

    /// Fills the server fields and the sign-in method from a detected provider,
    /// and opens the server settings when the provider is only a guess.
    fn apply(&mut self, ui: &Ui<FormMsg>, detection: &Detection) {
        let preset = &detection.preset;
        let w = &self.widgets;
        w.imap_host.set_text(&preset.imap_host);
        w.imap_port.set_text(&preset.imap_port.to_string());
        w.smtp_host.set_text(&preset.smtp_host);
        w.smtp_port.set_text(&preset.smtp_port.to_string());
        w.security.set_selected(&preset.smtp_tls);
        w.auth.set_selected(&preset.auth);
        self.filled = self.server_fields();
        self.view.password = preset.auth == AuthKind::Password;
        if !detection.known {
            self.view.servers = true;
            self.widgets.show_servers.set_checked(true);
        }
        self.update_note();
        self.widgets.arrange(ui, self.view);
    }

    /// The line under the sign-in fields: what was detected, or how Google
    /// sign-in works.
    fn update_note(&self) {
        let form = self.form();
        let note = if form.auth == AuthKind::GoogleOAuth {
            if form.imap_host.trim() == GMAIL_IMAP_HOST {
                "Your browser opens so you can approve access; no app password is needed."
            } else {
                "Sign in with Google only works for Gmail (imap.gmail.com)."
            }
        } else {
            match detect(&form.email) {
                Some(detection) if detection.known && !self.servers_edited => "Server settings filled in for this provider.",
                Some(detection) if !detection.known && !self.servers_edited => "This provider is not known: check the server settings below.",
                _ => "",
            }
        };
        self.widgets.note.set_text(note);
    }

    fn email_changed(&mut self, ui: &Ui<FormMsg>, email: &str, settled: bool) {
        if self.servers_edited {
            return;
        }
        if let Some(detection) = detect(email).filter(|detection| detection.known || settled) {
            self.apply(ui, &detection);
        }
    }

    fn servers_edited(&mut self) {
        let now = self.server_fields();
        if now == self.filled {
            return;
        }
        self.servers_edited = true;
        if now.smtp_port != self.filled.smtp_port {
            if let Some(security) = now.smtp_port.trim().parse().ok().and_then(security_for_smtp_port) {
                self.widgets.security.set_selected(&security);
            }
        }
        self.filled = now;
        self.update_note();
    }

    fn connect(&mut self, ui: &Ui<FormMsg>, sign_in_again: bool) {
        if self.busy {
            return;
        }
        if self.widgets.imap_host.text().trim().is_empty() {
            if let Some(detection) = detect(&self.widgets.email.text()) {
                self.apply(ui, &detection);
            }
        }
        let form = self.form();
        if let Err(problem) = form.to_account(None) {
            self.show_status(ui, &format!("Error: {}", problem.message));
            self.focus(problem.field);
            return;
        }
        let editing = match &self.mode {
            Mode::Edit { id } => Some(id.clone()),
            Mode::New { .. } => None,
        };
        self.set_busy(ui, true);
        self.show_status(ui, "Connecting...");
        self.ask(Request::Connect { form, editing, sign_in_again });
    }

    fn focus(&self, field: Field) {
        let w = &self.widgets;
        match field {
            Field::Email => w.email.focus(),
            Field::Password if self.view.revealed => w.revealed.focus(),
            Field::Password => w.password.focus(),
            Field::ImapHost => w.imap_host.focus(),
            Field::ImapPort => w.imap_port.focus(),
            Field::SmtpHost => w.smtp_host.focus(),
            Field::SmtpPort => w.smtp_port.focus(),
        }
    }

    fn set_busy(&mut self, ui: &Ui<FormMsg>, busy: bool) {
        self.busy = busy;
        self.widgets.set_locked(busy);
        if !busy {
            self.view.waiting_for_browser = false;
        }
        self.widgets.arrange(ui, self.view);
    }

    fn cancel(&mut self, ui: &Ui<FormMsg>) {
        if self.busy {
            self.ask(Request::Abort);
        }
        ui.close();
    }

    fn show_password(&mut self, ui: &Ui<FormMsg>, shown: bool) {
        self.view.revealed = shown;
        self.widgets.arrange(ui, self.view);
        if shown { &self.widgets.revealed } else { &self.widgets.password }.focus();
    }

    /// Keeps the masked and the shown password edit in step.
    fn password_changed(&self, text: &str) {
        for edit in [&self.widgets.password, &self.widgets.revealed] {
            if edit.text() != text {
                edit.set_text(text);
            }
        }
    }

    fn auth_changed(&mut self, ui: &Ui<FormMsg>, kind: AuthKind) {
        self.view.password = kind == AuthKind::Password;
        self.update_note();
        self.widgets.arrange(ui, self.view);
    }
}

impl App for FormApp {
    type Msg = FormMsg;

    fn update(&mut self, msg: FormMsg, ui: &mut Ui<FormMsg>) {
        match msg {
            FormMsg::EmailChanged(email) => self.email_changed(ui, &email, false),
            FormMsg::EmailFocus(false) => {
                let email = self.widgets.email.text();
                self.email_changed(ui, &email, true);
            }
            FormMsg::EmailFocus(true) => {}
            FormMsg::PasswordChanged(text) => self.password_changed(&text),
            FormMsg::AuthChanged(kind) => self.auth_changed(ui, kind),
            FormMsg::ShowPassword(shown) => self.show_password(ui, shown),
            FormMsg::ShowServers(shown) => {
                self.view.servers = shown;
                self.widgets.arrange(ui, self.view);
            }
            FormMsg::ServersEdited => self.servers_edited(),
            FormMsg::Connect => self.connect(ui, false),
            FormMsg::SignInAgain => self.connect(ui, true),
            FormMsg::CancelSignIn => {
                self.ask(Request::Abort);
                self.set_busy(ui, false);
                self.show_status(ui, "Sign-in cancelled.");
            }
            FormMsg::Cancel => self.cancel(ui),
            FormMsg::Working(text) => self.show_status(ui, &text),
            FormMsg::AwaitingBrowser => {
                self.view.waiting_for_browser = true;
                self.widgets.arrange(ui, self.view);
                self.show_status(ui, "Finish signing in with Google in your browser...");
            }
            FormMsg::BrowserUnavailable(url) => {
                let copied = win32ui::clipboard::set_text(ui.hwnd(), &url).is_ok();
                let hint = if copied { "The address is on the clipboard: paste it into a browser." } else { url.as_str() };
                self.show_status(ui, &format!("Could not open your browser. {hint}"));
            }
            FormMsg::Failed(error) => {
                self.set_busy(ui, false);
                self.show_status(ui, &format!("Error: {error}"));
            }
            FormMsg::SetTheme(theme, follow) => {
                ui.follow_system_theme(follow);
                ui.set_theme(theme);
            }
        }
    }
}

impl Drop for FormApp {
    fn drop(&mut self) {
        self.ask(Request::Closed);
    }
}
