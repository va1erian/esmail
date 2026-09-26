//! The account form's widgets and their layout.

use esmail::config::{AuthKind, TlsMode};
use xui_win32::prelude::*;
use xui_win32::row;

use super::window::FormMsg;

/// Width of the field captions, in dips.
const CAPTION_WIDTH: f32 = 116.0;
/// Height of one field row, in dips.
const ROW_HEIGHT: f32 = 28.0;
/// Height of the two sign-in radio buttons together, in dips.
const AUTH_HEIGHT: f32 = 56.0;
/// Height of the status and note lines, in dips: room for three wrapped lines.
const STATUS_HEIGHT: f32 = 66.0;

/// Which optional parts of the form are showing.
#[derive(Clone, Copy, Default, PartialEq)]
pub struct View {
    /// The welcome text of the first-run form.
    pub intro: bool,
    /// The sign-in method is a password (else Google).
    pub password: bool,
    /// The password is shown as typed.
    pub revealed: bool,
    /// The server settings.
    pub servers: bool,
    /// A Google sign-in is waiting for the browser.
    pub waiting_for_browser: bool,
    /// An existing Google account can sign in again.
    pub can_sign_in_again: bool,
}

/// Every widget of the window.
pub struct Widgets {
    pub intro: Label,
    pub name: Edit<FormMsg>,
    pub email: Edit<FormMsg>,
    pub auth: RadioGroup<AuthKind, FormMsg>,
    pub password: Edit<FormMsg>,
    /// The password as typed: shown in place of `password` while "Show" is on.
    pub revealed: Edit<FormMsg>,
    pub show_password: CheckBox<FormMsg>,
    pub note: Label,
    pub show_servers: CheckBox<FormMsg>,
    pub imap_host: Edit<FormMsg>,
    pub imap_port: Edit<FormMsg>,
    pub smtp_host: Edit<FormMsg>,
    pub smtp_port: Edit<FormMsg>,
    pub security: ComboBox<TlsMode, FormMsg>,
    pub status: Label,
    pub connect: Button<FormMsg>,
    pub sign_in_again: Button<FormMsg>,
    pub cancel_sign_in: Button<FormMsg>,
    pub cancel: Button<FormMsg>,
    /// Field captions and the spacer that pushes the buttons right. Kept alive:
    /// a label's window is destroyed with it.
    captions: Captions,
}

struct Captions {
    name: Label,
    email: Label,
    auth: Label,
    password: Label,
    imap_host: Label,
    imap_port: Label,
    smtp_host: Label,
    smtp_port: Label,
    security: Label,
    spacer: Label,
}

/// A single-line edit where Enter connects.
fn field(ui: &mut Ui<FormMsg>) -> xui_win32::Result<Edit<FormMsg>> {
    Ok(Edit::single_line(ui)?.on_submit(|| Some(FormMsg::Connect)))
}

/// A port number edit.
fn port(ui: &mut Ui<FormMsg>) -> xui_win32::Result<Edit<FormMsg>> {
    Ok(field(ui)?.number_only(true).max_length(5).on_change(|_| Some(FormMsg::ServersEdited)))
}

fn button(ui: &mut Ui<FormMsg>, text: &str, msg: fn() -> FormMsg) -> xui_win32::Result<Button<FormMsg>> {
    Ok(Button::new(ui, text)?.on_click(move || Some(msg())))
}

fn caption(ui: &mut Ui<FormMsg>, text: &str) -> xui_win32::Result<Label> {
    Label::new(ui, Rect::default(), text)
}

impl Widgets {
    /// Creates the widgets, without laying them out. They are created in the
    /// order Tab visits them.
    pub fn build(ui: &mut Ui<FormMsg>) -> xui_win32::Result<Widgets> {
        let name = field(ui)?.cue("Optional: shown in the folder list");
        let email = field(ui)?
            .cue("you@example.com")
            .on_change(|text| Some(FormMsg::EmailChanged(text.to_string())))
            .on_focus(|focused| Some(FormMsg::EmailFocus(focused)));
        let auth = RadioGroup::new(ui, [("Password", AuthKind::Password), ("Sign in with Google (no app password)", AuthKind::GoogleOAuth)])?
            .on_select(|kind| Some(FormMsg::AuthChanged(*kind)));
        let password = Edit::password(ui)?.on_submit(|| Some(FormMsg::Connect)).on_change(|text| Some(FormMsg::PasswordChanged(text.to_string())));
        let revealed = field(ui)?.on_change(|text| Some(FormMsg::PasswordChanged(text.to_string())));
        let show_password = CheckBox::new(ui, "Show")?.on_toggle(|shown| Some(FormMsg::ShowPassword(shown)));
        let show_servers = CheckBox::new(ui, "Show server settings")?.on_toggle(|shown| Some(FormMsg::ShowServers(shown)));
        let imap_host = field(ui)?.on_change(|_| Some(FormMsg::ServersEdited));
        let imap_port = port(ui)?;
        let smtp_host = field(ui)?.on_change(|_| Some(FormMsg::ServersEdited));
        let smtp_port = port(ui)?;
        let security = ComboBox::new(ui, [("SSL/TLS", TlsMode::Ssl), ("STARTTLS", TlsMode::StartTls), ("None (local test servers only)", TlsMode::None)])?
            .select(&TlsMode::Ssl)
            .on_select(|_| Some(FormMsg::ServersEdited));
        let sign_in_again = button(ui, "Sign in again", || FormMsg::SignInAgain)?;
        let cancel_sign_in = button(ui, "Cancel sign-in", || FormMsg::CancelSignIn)?;
        let connect = button(ui, "Connect", || FormMsg::Connect)?.default();
        let cancel = button(ui, "Cancel", || FormMsg::Cancel)?;
        Ok(Widgets {
            intro: caption(ui, "")?,
            name,
            email,
            auth,
            password,
            revealed,
            show_password,
            note: caption(ui, "")?,
            show_servers,
            imap_host,
            imap_port,
            smtp_host,
            smtp_port,
            security,
            status: caption(ui, "")?,
            connect,
            sign_in_again,
            cancel_sign_in,
            cancel,
            captions: Captions {
                name: caption(ui, "Name")?,
                email: caption(ui, "Email address")?,
                auth: caption(ui, "Sign in with")?,
                password: caption(ui, "Password")?,
                imap_host: caption(ui, "IMAP server")?,
                imap_port: caption(ui, "IMAP port")?,
                smtp_host: caption(ui, "SMTP server")?,
                smtp_port: caption(ui, "SMTP port")?,
                security: caption(ui, "SMTP security")?,
                spacer: caption(ui, "")?,
            },
        })
    }

    /// Shows the parts `view` asks for and lays the form out with them. A hidden
    /// part is also left out of the layout, so the rest closes up.
    pub fn arrange(&self, ui: &Ui<FormMsg>, view: View) {
        let c = &self.captions;
        let password = view.password && !view.revealed;
        let revealed = view.password && view.revealed;
        let visibility: [(&[&dyn AsControl], bool); 6] = [
            (&[&self.intro], view.intro),
            (&[&c.password, &self.show_password], view.password),
            (&[&self.password], password),
            (&[&self.revealed], revealed),
            (&[&c.imap_host, &c.imap_port, &c.smtp_host, &c.smtp_port, &c.security, &self.imap_host, &self.imap_port, &self.smtp_host, &self.smtp_port, &self.security], view.servers),
            (&[&self.cancel_sign_in], view.waiting_for_browser),
        ];
        for (controls, shown) in visibility {
            for control in controls {
                control.set_visible(shown);
            }
        }
        self.sign_in_again.set_visible(view.can_sign_in_again && !view.waiting_for_browser);

        let caption = |label: &Label| label.width(dip(CAPTION_WIDTH));
        let line = |label: &Label, edit: &Edit<FormMsg>| row![caption(label), edit.fill(1)].spacing(dip(6.0)).height(dip(ROW_HEIGHT));
        let pair = |label: &Label, edit: &Edit<FormMsg>, port_label: &Label, port: &Edit<FormMsg>| {
            row![caption(label), edit.fill(1), port_label.width(dip(64.0)), port.width(dip(64.0))].spacing(dip(6.0)).height(dip(ROW_HEIGHT))
        };
        let mut rows = Vec::new();
        if view.intro {
            rows.push(self.intro.height(dip(58.0)));
        }
        rows.push(line(&c.name, &self.name));
        rows.push(line(&c.email, &self.email));
        rows.push(row![caption(&c.auth), self.auth.layout().fill(1)].spacing(dip(6.0)).height(dip(AUTH_HEIGHT)));
        if view.password {
            let entry = if revealed { &self.revealed } else { &self.password };
            rows.push(row![caption(&c.password), entry.fill(1), self.show_password.width(dip(72.0))].spacing(dip(6.0)).height(dip(ROW_HEIGHT)));
        }
        rows.push(self.note.height(dip(38.0)));
        rows.push(self.show_servers.height(dip(ROW_HEIGHT)));
        if view.servers {
            rows.push(pair(&c.imap_host, &self.imap_host, &c.imap_port, &self.imap_port));
            rows.push(pair(&c.smtp_host, &self.smtp_host, &c.smtp_port, &self.smtp_port));
            rows.push(row![caption(&c.security), self.security.fill(1)].spacing(dip(6.0)).height(dip(ROW_HEIGHT)));
        }
        rows.push(self.status.height(dip(STATUS_HEIGHT)));
        let buttons = row![
            c.spacer.fill(1),
            self.cancel_sign_in.width(dip(130.0)),
            self.sign_in_again.width(dip(120.0)),
            self.connect.width(dip(110.0)),
            self.cancel.width(dip(90.0))
        ]
        .spacing(dip(8.0))
        .height(dip(32.0));
        rows.push(buttons);
        let margins = Insets::new(dip(16.0), dip(16.0) + ui.title_bar_height(), dip(16.0), dip(16.0));
        ui.set_layout(rows.into_iter().fold(Layout::column(), Layout::item).spacing(dip(6.0)).margins(margins));
    }

    /// The edits that hold typed text, in tab order.
    pub fn edits(&self) -> [&Edit<FormMsg>; 8] {
        [&self.name, &self.email, &self.password, &self.revealed, &self.imap_host, &self.imap_port, &self.smtp_host, &self.smtp_port]
    }

    /// Locks the form while a connection is being tried.
    pub fn set_locked(&self, locked: bool) {
        for edit in self.edits() {
            edit.set_read_only(locked);
        }
        self.auth.set_enabled(!locked);
        self.security.set_enabled(!locked);
        self.show_password.set_enabled(!locked);
        self.show_servers.set_enabled(!locked);
        self.connect.set_enabled(!locked);
        self.sign_in_again.set_enabled(!locked);
    }
}
