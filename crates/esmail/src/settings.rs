//! The Settings window: tabs for what applies to the whole app (General, the
//! Google sign-in client) and for the accounts, each account with its own
//! sub-dialog.
//!
//! The window is drawn from a [`SettingsState`] that is taken out of
//! `EsMailApp` for the duration of the frame, so the drawing code can read the
//! app (accounts, connection states, config) while editing the form. Buttons
//! do not act on the spot: they push an [`Action`], and the actions run once
//! the window has been drawn, when `EsMailApp` can be borrowed mutably again.
//! Typing changes nothing until Save (Cancel just drops the form).

use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Tab {
    General,
    Accounts,
    Google,
}

/// The open Settings window.
pub(super) struct SettingsState {
    tab: Tab,
    /// Editable copy of the Google OAuth client (Google tab).
    client_id: String,
    client_secret: String,
    /// The per-account sub-dialog, when one is open.
    account: Option<AccountDialog>,
}

/// One account's editable settings. Identity (username and IMAP host) is not
/// editable: it is what the account id, the keyring entries and the mail cache
/// are keyed on -- to change either, add the account again.
struct AccountDialog {
    id: AccountId,
    username: String,
    imap_host: String,
    display_name: String,
    imap_port: String,
    smtp_host: String,
    smtp_port: String,
    smtp_tls: config::TlsMode,
    watch_mailbox: String,
    /// Google sign-in is only offered for Gmail.
    is_gmail: bool,
    /// The sign-in method chosen in the dialog.
    auth: config::AuthKind,
    /// A new password; empty keeps the saved one.
    new_password: String,
    /// The "Remove account" button was pressed and is asking to be sure.
    confirm_remove: bool,
}

impl AccountDialog {
    fn new(account: &AccountConfig) -> Self {
        Self {
            id: account.id.clone(),
            username: account.username.clone(),
            imap_host: account.imap_host.clone(),
            display_name: account.display_name.clone(),
            imap_port: account.imap_port.to_string(),
            smtp_host: account.smtp_host.clone(),
            smtp_port: account.smtp_port.to_string(),
            smtp_tls: account.smtp_tls,
            watch_mailbox: account.watch_mailbox.clone().unwrap_or_default(),
            is_gmail: account.imap_host == GMAIL_IMAP_HOST,
            auth: account.auth,
            new_password: String::new(),
            confirm_remove: false,
        }
    }
}

/// What a click in the Settings window asks for.
enum Action {
    SetTheme(config::ThemeMode),
    SaveGoogle,
    AddAccount,
    Edit(AccountId),
    Connect(AccountId),
    Disconnect(AccountId),
    SignIn(AccountId),
    CancelSignIn(AccountId),
    Remove(AccountId),
    SaveAccount,
    CloseAccountDialog,
}

pub(super) fn tls_label(tls: config::TlsMode) -> &'static str {
    match tls {
        config::TlsMode::Ssl => "SSL/TLS",
        config::TlsMode::StartTls => "STARTTLS",
        config::TlsMode::None => "None (insecure)",
    }
}

fn auth_label(auth: config::AuthKind) -> &'static str {
    match auth {
        config::AuthKind::Password => "Password",
        config::AuthKind::GoogleOAuth => "Google",
    }
}

const GREEN: egui::Color32 = egui::Color32::from_rgb(60, 160, 80);
const AMBER: egui::Color32 = egui::Color32::from_rgb(210, 150, 30);
const RED: egui::Color32 = egui::Color32::from_rgb(180, 40, 40);

impl EsMailApp {
    /// Open the Settings window on `tab`, its forms filled from what is saved.
    pub(super) fn open_settings(&mut self, tab: Tab) {
        let saved = self.config.google_oauth.as_ref();
        self.settings = Some(SettingsState {
            tab,
            client_id: saved.map(|c| c.client_id.clone()).unwrap_or_default(),
            client_secret: saved.and_then(|c| c.client_secret.clone()).unwrap_or_default(),
            account: None,
        });
    }

    pub(super) fn show_settings_window(&mut self, ctx: &egui::Context) {
        let Some(mut state) = self.settings.take() else { return };
        // Read here, once the window is known to be open: this reads
        // environment variables, which is not something to do on every frame
        // of a closed window. Lets the Google tab say when an environment
        // variable overrides what is typed.
        let active_source = oauth::google_client_with_source(self.config.google_oauth.as_ref()).map(|(_, source)| source);

        let mut actions: Vec<Action> = Vec::new();
        let mut open = true;
        egui::Window::new("Settings")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size([580.0, 440.0])
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut state.tab, Tab::General, "General");
                    ui.selectable_value(&mut state.tab, Tab::Accounts, "Accounts");
                    ui.selectable_value(&mut state.tab, Tab::Google, "Google");
                });
                ui.separator();
                match state.tab {
                    Tab::General => self.general_tab(ui, &mut actions),
                    Tab::Accounts => self.accounts_tab(ui, &mut actions),
                    Tab::Google => google_tab(ui, &mut state, active_source, &mut actions),
                }
            });

        // The per-account sub-dialog: its own window, so it can sit next to
        // the list it was opened from.
        if let Some(dialog) = &mut state.account {
            let mut dialog_open = true;
            egui::Window::new(format!("Account \u{2014} {}", dialog.display_name.trim()))
                .id(egui::Id::new(("account_dialog", &dialog.id)))
                .open(&mut dialog_open)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [40.0, 20.0])
                .show(ctx, |ui| self.account_dialog_ui(ui, dialog, &mut actions));
            if !dialog_open {
                actions.push(Action::CloseAccountDialog);
            }
        }

        let mut keep_open = open;
        for action in actions {
            match action {
                Action::SetTheme(theme) => self.apply_theme(ctx, theme),
                Action::SaveGoogle => self.apply_google_settings(&state),
                Action::AddAccount => {
                    self.adding_account = true;
                    keep_open = false;
                }
                Action::Edit(id) => {
                    if let Some(account) = self.config.accounts.iter().find(|a| a.id == id) {
                        state.account = Some(AccountDialog::new(account));
                    }
                }
                Action::Connect(id) => self.connect_saved(&id),
                Action::Disconnect(id) => self.disconnect_account(&id),
                Action::SignIn(id) => self.sign_in_again(&id),
                Action::CancelSignIn(id) => self.cancel_google_sign_in(&id),
                Action::Remove(id) => {
                    self.remove_account(&id);
                    self.listener.notify_config_changed();
                    if state.account.as_ref().is_some_and(|d| d.id == id) {
                        state.account = None;
                    }
                }
                Action::SaveAccount => {
                    if let Some(dialog) = state.account.take() {
                        // Kept open when it could not be applied (a bad
                        // number, a missing password), so nothing typed is lost.
                        if let Some(dialog) = self.apply_account_dialog(dialog) {
                            state.account = Some(dialog);
                        }
                    }
                }
                Action::CloseAccountDialog => state.account = None,
            }
        }
        if keep_open {
            self.settings = Some(state);
        }
    }

    fn general_tab(&self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.heading("Appearance");
        ui.horizontal(|ui| {
            ui.label("Theme");
            for mode in [config::ThemeMode::Dark, config::ThemeMode::Light, config::ThemeMode::System] {
                if ui.selectable_label(self.theme == mode, mode.label()).clicked() {
                    actions.push(Action::SetTheme(mode));
                }
            }
        });
        ui.add_space(8.0);
        ui.heading("Accounts");
        let connected = self.accounts.iter().filter(|v| v.state == ConnState::Connected).count();
        ui.label(format!(
            "{} saved, {connected} connected. Every connected account is watched for new mail \
             at once, even while this window is closed to the tray.",
            self.config.accounts.len()
        ));
        ui.add_space(8.0);
        egui::CollapsingHeader::new("Keyboard shortcuts").show(ui, |ui| {
            egui::Grid::new("shortcuts").num_columns(2).spacing([16.0, 2.0]).show(ui, |ui| {
                for (key, what) in [
                    ("j / k", "Next / previous message"),
                    ("r", "Reply"),
                    ("a", "Archive"),
                    ("f", "Star / unstar"),
                    ("Del", "Delete to Trash"),
                    ("Ctrl+F", "Search"),
                    ("Ctrl+N", "New message"),
                ] {
                    ui.monospace(key);
                    ui.label(what);
                    ui.end_row();
                }
            });
        });
    }

    fn accounts_tab(&self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.horizontal(|ui| {
            ui.heading("Accounts");
            if ui.button("Add account\u{2026}").clicked() {
                actions.push(Action::AddAccount);
            }
        });
        if self.config.accounts.is_empty() {
            ui.weak("No saved accounts yet.");
            return;
        }
        egui::ScrollArea::vertical().auto_shrink([false, true]).show(ui, |ui| {
            for account in &self.config.accounts {
                let view = self.view(&account.id);
                let signing_in = self.oauth_tasks.contains_key(&account.id);
                let (color, status) = match view.map(|v| &v.state) {
                    _ if signing_in => (AMBER, "Waiting for Google sign-in in the browser\u{2026}".to_string()),
                    None => (egui::Color32::GRAY, "Not connected".to_string()),
                    Some(ConnState::Connected) => (GREEN, "Connected".to_string()),
                    Some(ConnState::Connecting) => (AMBER, "Connecting\u{2026}".to_string()),
                    Some(ConnState::Disconnected) => (AMBER, "Reconnecting\u{2026}".to_string()),
                    Some(ConnState::Failed(error)) => (RED, error.clone()),
                };
                ui.group(|ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        // A drawn dot, not a "●" character: egui's bundled
                        // fonts have no such glyph and it shows as a box.
                        let (dot, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                        ui.painter().circle_filled(dot.center(), 4.5, color);
                        ui.strong(&account.display_name);
                        ui.weak(format!("{} \u{b7} {}", account.username, auth_label(account.auth)));
                    });
                    ui.colored_label(color, status);
                    ui.horizontal(|ui| {
                        if ui.button("Edit\u{2026}").clicked() {
                            actions.push(Action::Edit(account.id.clone()));
                        }
                        if view.is_some() {
                            if ui.button("Disconnect").clicked() {
                                actions.push(Action::Disconnect(account.id.clone()));
                            }
                        } else if ui.button("Connect").clicked() {
                            actions.push(Action::Connect(account.id.clone()));
                        }
                        if account.auth == config::AuthKind::GoogleOAuth {
                            if signing_in {
                                if ui.button("Cancel sign-in").clicked() {
                                    actions.push(Action::CancelSignIn(account.id.clone()));
                                }
                            } else if ui.button("Sign in again").clicked() {
                                actions.push(Action::SignIn(account.id.clone()));
                            }
                        }
                    });
                });
            }
        });
    }

    fn account_dialog_ui(&self, ui: &mut egui::Ui, dialog: &mut AccountDialog, actions: &mut Vec<Action>) {
        egui::Grid::new(("account_grid", &dialog.id)).num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            ui.label("Name");
            ui.add(egui::TextEdit::singleline(&mut dialog.display_name).desired_width(260.0));
            ui.end_row();

            ui.label("Address");
            ui.label(&dialog.username);
            ui.end_row();

            ui.label("IMAP server");
            ui.horizontal(|ui| {
                ui.label(&dialog.imap_host);
                ui.add(egui::TextEdit::singleline(&mut dialog.imap_port).desired_width(50.0));
            });
            ui.end_row();

            ui.label("SMTP server");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut dialog.smtp_host).desired_width(190.0));
                ui.add(egui::TextEdit::singleline(&mut dialog.smtp_port).desired_width(50.0));
            });
            ui.end_row();

            ui.label("SMTP security");
            egui::ComboBox::from_id_salt(("smtp_tls", &dialog.id)).selected_text(tls_label(dialog.smtp_tls)).show_ui(
                ui,
                |ui| {
                    for tls in [config::TlsMode::Ssl, config::TlsMode::StartTls, config::TlsMode::None] {
                        ui.selectable_value(&mut dialog.smtp_tls, tls, tls_label(tls));
                    }
                },
            );
            ui.end_row();

            ui.label("New-mail mailbox");
            ui.add(egui::TextEdit::singleline(&mut dialog.watch_mailbox).hint_text("INBOX").desired_width(260.0));
            ui.end_row();
        });
        ui.weak("The mailbox watched for new mail (toasts, tray count). Leave empty for INBOX.");

        ui.separator();
        ui.strong("Sign-in");
        if dialog.is_gmail {
            ui.horizontal(|ui| {
                ui.radio_value(&mut dialog.auth, config::AuthKind::Password, "Password");
                ui.radio_value(&mut dialog.auth, config::AuthKind::GoogleOAuth, "Google (no app password)");
            });
        }
        match dialog.auth {
            config::AuthKind::Password => {
                ui.add(
                    egui::TextEdit::singleline(&mut dialog.new_password)
                        .password(true)
                        .hint_text("New password (empty keeps the saved one)")
                        .desired_width(300.0),
                );
            }
            config::AuthKind::GoogleOAuth => {
                let signed_in = secrets::get_password(&dialog.id, "oauth").is_some();
                if signed_in {
                    ui.colored_label(GREEN, "Signed in with Google.");
                } else {
                    ui.colored_label(AMBER, "Not signed in yet: Save opens Google's consent page in your browser.");
                }
                if oauth::google_client(self.config.google_oauth.as_ref()).is_none() {
                    ui.colored_label(RED, "No Google OAuth client is configured (Settings > Google).");
                }
                if signed_in && ui.button("Sign in again").clicked() {
                    actions.push(Action::SignIn(dialog.id.clone()));
                }
            }
        }

        ui.separator();
        ui.horizontal(|ui| {
            if ui.button("Save").clicked() {
                actions.push(Action::SaveAccount);
            }
            if ui.button("Cancel").clicked() {
                actions.push(Action::CloseAccountDialog);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if dialog.confirm_remove {
                    if ui.button("Keep").clicked() {
                        dialog.confirm_remove = false;
                    }
                    if ui
                        .button(egui::RichText::new("Remove for good").color(RED))
                        .on_hover_text("Disconnects it and deletes its saved passwords and sign-in")
                        .clicked()
                    {
                        actions.push(Action::Remove(dialog.id.clone()));
                    }
                } else if ui.button("Remove account\u{2026}").clicked() {
                    dialog.confirm_remove = true;
                }
            });
        });
    }

    /// Save the per-account dialog. Returns the dialog back when it could not
    /// be applied, so it stays open with what was typed; `None` once done.
    fn apply_account_dialog(&mut self, dialog: AccountDialog) -> Option<AccountDialog> {
        let Some(index) = self.config.accounts.iter().position(|a| a.id == dialog.id) else {
            return None; // Removed while the dialog was open.
        };
        let Ok(imap_port) = dialog.imap_port.trim().parse::<u16>() else {
            self.push_banner("The IMAP port must be a number.".to_string());
            return Some(dialog);
        };
        let Ok(smtp_port) = dialog.smtp_port.trim().parse::<u16>() else {
            self.push_banner("The SMTP port must be a number.".to_string());
            return Some(dialog);
        };
        let mut account = self.config.accounts[index].clone();
        let switching_to_google =
            dialog.auth == config::AuthKind::GoogleOAuth && account.auth != config::AuthKind::GoogleOAuth;
        let new_password = (dialog.auth == config::AuthKind::Password && !dialog.new_password.is_empty())
            .then(|| SecretString::from(dialog.new_password.clone()));
        if dialog.auth == config::AuthKind::Password
            && account.auth == config::AuthKind::GoogleOAuth
            && new_password.is_none()
        {
            self.push_banner("Enter a password to stop using Google sign-in for this account.".to_string());
            return Some(dialog);
        }

        let name = dialog.display_name.trim();
        if !name.is_empty() {
            account.display_name = name.to_string();
        }
        account.imap_port = imap_port;
        account.smtp_host = dialog.smtp_host.trim().to_string();
        account.smtp_port = smtp_port;
        account.smtp_tls = dialog.smtp_tls;
        let watch = dialog.watch_mailbox.trim();
        account.watch_mailbox = (!watch.is_empty()).then(|| watch.to_string());
        if let Some(password) = &new_password {
            account.auth = config::AuthKind::Password;
            for kind in ["imap", "smtp"] {
                if let Err(e) = secrets::set_password(&account.id, kind, password) {
                    log::warn!("could not save the {kind} password to the OS keyring: {e}");
                }
            }
            secrets::delete_password(&account.id, "oauth");
        }
        // Assigned rather than `upsert_account`: that keeps an old
        // `watch_mailbox` when the new one is `None`, which here means "cleared".
        self.config.accounts[index] = account.clone();
        if let Err(e) = self.config.save() {
            self.push_banner(format!("Could not save the account: {e}"));
        }
        // The listener watches the same accounts: reload now rather than on
        // its own next event.
        self.listener.notify_config_changed();

        if switching_to_google {
            // The account keeps its old sign-in until Google has approved:
            // `persist_pending` switches it over once the new session connects.
            let mut google = account;
            google.auth = config::AuthKind::GoogleOAuth;
            self.begin_google_sign_in(google);
        } else if self.view(&account.id).is_some() || new_password.is_some() {
            // A live session keeps the label, ports and password it started
            // with; replace it so the change takes effect.
            match accounts::saved_auth(&self.config, &account) {
                Ok(auth) => self.connect_account(account, auth, false),
                Err(reason) => self.push_banner(format!("{}: {reason}", account.display_name)),
            }
        }
        None
    }

    /// Save the Google tab into `config.toml`.
    fn apply_google_settings(&mut self, state: &SettingsState) {
        let client_id = state.client_id.trim().to_string();
        let client_secret = state.client_secret.trim().to_string();
        self.config.google_oauth = (!client_id.is_empty()).then(|| config::OAuthClientConfig {
            client_id,
            client_secret: (!client_secret.is_empty()).then_some(client_secret),
        });
        match self.config.save() {
            Ok(()) => self.status = "Settings saved".to_string(),
            Err(e) => self.push_banner(format!("Could not save settings: {e}")),
        }
        self.listener.notify_config_changed();
        // A first-time user who has just configured a client and is looking
        // at the Gmail defaults most likely wants Sign in with Google.
        if self.config.accounts.is_empty()
            && self.host.trim() == GMAIL_IMAP_HOST
            && oauth::google_client(self.config.google_oauth.as_ref()).is_some()
        {
            self.use_oauth = true;
        }
    }
}

/// The Google tab: the OAuth client id and secret "Sign in with Google" needs
/// (see `oauth`'s module doc for why esmail can't supply its own). One client
/// serves every Google account; each account has its own token.
fn google_tab(
    ui: &mut egui::Ui,
    state: &mut SettingsState,
    active_source: Option<oauth::ClientSource>,
    actions: &mut Vec<Action>,
) {
    ui.heading("Sign in with Google");
    ui.label(
        "Lets Gmail accounts sign in through the browser instead of an app password. \
         Create a \"Desktop app\" OAuth client in Google Cloud Console and enter its \
         credentials here (see the esmail README). The same client serves every Google \
         account you add; each account signs in and gets its own token.",
    );
    ui.add_space(6.0);
    egui::Grid::new("google_oauth_settings").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
        ui.label("Client ID");
        ui.add(egui::TextEdit::singleline(&mut state.client_id).desired_width(340.0));
        ui.end_row();
        ui.label("Client secret");
        ui.add(egui::TextEdit::singleline(&mut state.client_secret).password(true).desired_width(340.0));
        ui.end_row();
    });
    ui.add_space(4.0);
    match active_source {
        Some(oauth::ClientSource::Environment) => {
            ui.colored_label(
                AMBER,
                "The ESMAIL_GOOGLE_CLIENT_ID environment variable is set and takes precedence \
                 over what is saved here.",
            );
        }
        Some(source) => {
            ui.weak(format!("Currently using: {}.", source.label()));
        }
        None => {
            ui.weak("No client configured yet.");
        }
    }
    ui.weak("Saved in config.toml. Leave the client ID empty to remove it.");
    ui.add_space(6.0);
    if ui.button("Save").clicked() {
        actions.push(Action::SaveGoogle);
    }
}
