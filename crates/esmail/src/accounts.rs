//! Signing accounts in, out and away: the login form's Connect button, the
//! "Sign in with Google" browser round trip, saving what worked, and the
//! per-account credentials -- all keyed by account id so any number of
//! accounts, password and Google alike, can be open at once.
//!
//! Every account has its own credential ([`auth::Auth`]): a password, or its
//! own Google token source with its own refresh token in the keyring under
//! `(account id, "oauth")`. Nothing here is shared between accounts except the
//! Google OAuth *client* (`config.google_oauth`), which identifies esmail to
//! Google rather than any one mailbox.

use super::*;

/// The outcome of a "Sign in with Google" browser round trip, sent from the
/// task that ran it (see [`EsMailApp::begin_google_sign_in`]) back to the UI.
/// Carries the account as it was when the sign-in started, so editing the
/// form (or the Settings dialog) while the browser is open can't change which
/// account the freshly authorized token gets attached to.
pub(super) enum OAuthMessage {
    /// The system browser could not be launched; the sign-in is still
    /// waiting, so the user can open `url` themselves.
    BrowserUnavailable { url: String },
    Authorized { account: AccountConfig, auth: auth::Auth },
    Failed { account_id: AccountId, error: String },
}

/// The whole browser round trip: consent page, redirect, code exchange.
async fn run_google_sign_in(
    client: &oauth::OAuthClient,
    username: &str,
    tx: &mpsc::Sender<OAuthMessage>,
    ctx: &egui::Context,
) -> anyhow::Result<Arc<oauth::TokenSource>> {
    let pending = oauth::begin(client, username).await?;
    if let Err(e) = opener::open_browser(&pending.url) {
        log::warn!("could not open the browser for Google sign-in: {e}");
        let _ = tx.send(OAuthMessage::BrowserUnavailable { url: pending.url.clone() }).await;
        // `_of(ROOT)`: this runs on a background task with no viewport pass of
        // its own, so the ambiguous `request_repaint()` can end up waking
        // whichever viewport (e.g. a compose window animating its own
        // spinner) last ran a pass instead of the root viewport that
        // actually drains `OAuthMessage` -- see `main.rs`'s `session_hooks`
        // for the fuller version of this note.
        ctx.request_repaint_of(egui::ViewportId::ROOT);
    }
    let grant = pending.finish(client).await?;
    oauth::TokenSource::from_grant(client.clone(), grant)
}

// Reading a saved account's credential lives in the library (`auth`), so the
// background listener can use it too.
pub(super) use esmail::auth::saved_auth;

impl EsMailApp {
    /// Fill the login form from a saved account and pull its password back
    /// out of the OS keyring, if there is one.
    pub(super) fn fill_form_from(&mut self, account: &AccountConfig) {
        self.host = account.imap_host.clone();
        self.port = account.imap_port.to_string();
        self.username = account.username.clone();
        self.smtp_host = account.smtp_host.clone();
        self.smtp_port = account.smtp_port.to_string();
        self.smtp_tls = account.smtp_tls;
        self.use_oauth = account.auth == config::AuthKind::GoogleOAuth;
        // An OAuth account has no password to restore; its refresh token is
        // looked up when Connect is clicked.
        self.password = if self.use_oauth {
            String::new()
        } else {
            secrets::get_password(&account.id, "imap")
                .map(|s| secrecy::ExposeSecret::expose_secret(&s).to_string())
                .unwrap_or_default()
        };
    }

    /// Whether the form is currently set to sign in with Google: the box is
    /// ticked *and* the host is one Google's OAuth actually applies to (the
    /// box is only shown for Gmail, but stays ticked if the host is edited
    /// afterwards).
    pub(super) fn oauth_active(&self) -> bool {
        self.use_oauth && self.host.trim() == GMAIL_IMAP_HOST
    }

    /// The account the Add account form describes. Starts from the saved
    /// entry when this account already exists, so reconnecting through the
    /// form keeps what the form has no field for (a display name chosen in
    /// Settings, the watch mailbox).
    pub(super) fn account_from_form(&self) -> AccountConfig {
        // Trimmed, like the values a connection is made with, so a stray
        // space in the form can't make the keyring/config key differ from
        // the account that actually signed in.
        let username = self.username.trim().to_string();
        let host = self.host.trim().to_string();
        let fresh = AccountConfig::new(username.clone(), host.clone(), self.port.parse().unwrap_or(993), username);
        let mut account = self.config.accounts.iter().find(|a| a.id == fresh.id).cloned().unwrap_or(fresh);
        account.imap_port = self.port.parse().unwrap_or(993);
        // AccountConfig::new only guesses smtp_host/smtp_port; the login
        // form's fields (pre-filled from that guess, but editable) win.
        if !self.smtp_host.trim().is_empty() {
            account.smtp_host = self.smtp_host.trim().to_string();
        }
        if let Ok(port) = self.smtp_port.trim().parse() {
            account.smtp_port = port;
        }
        account.smtp_tls = self.smtp_tls;
        account.auth = if self.oauth_active() { config::AuthKind::GoogleOAuth } else { config::AuthKind::Password };
        account
    }

    /// The Connect button. With a password that just connects. With Google
    /// sign-in it reuses the refresh token saved from an earlier approval, and
    /// only sends the user to the browser when there is none.
    pub(super) fn connect_clicked(&mut self, ctx: &egui::Context) {
        let account = self.account_from_form();
        // The form stays up until this account has connected (the `Connected`
        // handler dismisses it), so a failure can be corrected in place.
        self.adding_account = true;
        if !self.oauth_active() {
            let auth = auth::Auth::password(self.password.clone());
            self.connect_account(account, auth, true);
            return;
        }
        let Some(client) = self.google_client_or_explain() else { return };
        match secrets::get_password(&account.id, "oauth") {
            Some(refresh_token) => {
                let source = oauth::TokenSource::from_refresh_token(client, refresh_token);
                self.connect_account(account, auth::Auth::OAuth(source), true);
            }
            None => self.begin_google_sign_in(ctx, account),
        }
    }

    /// Open a session for `account` with `auth`, next to the ones already
    /// open. An account that is already open is replaced: the old session is
    /// dropped first, which really stops its connections and watcher, so a
    /// retry after a typo'd password, a fresh sign-in after a revoked Google
    /// token, or a Settings change cannot leave the old credentials running.
    /// With `persist`, the account and credential are saved once it connects
    /// (see [`Self::persist_pending`]).
    pub(super) fn connect_account(&mut self, account: AccountConfig, auth: auth::Auth, persist: bool) {
        self.accounts.retain(|v| v.id() != account.id);
        self.status = format!("Connecting {}...", account.display_name);
        let pending = persist.then(|| (account.clone(), auth.clone()));
        let view = spawn_account_view(&account, auth, pending, &self.imap_events_tx, &self.session_hooks);
        self.accounts.push(view);
    }

    /// Reconnect a saved account from its saved credential.
    pub(super) fn connect_saved(&mut self, account_id: &str) {
        let Some(account) = self.config.accounts.iter().find(|a| a.id == account_id).cloned() else { return };
        match saved_auth(&self.config, &account) {
            Ok(auth) => self.connect_account(account, auth, false),
            Err(reason) => self.push_banner(format!("{}: {reason}", account.display_name)),
        }
    }

    /// The configured Google OAuth client, or (with a banner saying how to
    /// configure one) `None`. Google issues tokens only to registered
    /// applications, so unlike a password this cannot work out of the box --
    /// see `oauth`'s module doc.
    pub(super) fn google_client_or_explain(&mut self) -> Option<oauth::OAuthClient> {
        let client = oauth::google_client(self.config.google_oauth.as_ref());
        if client.is_none() {
            self.push_banner(
                "Google sign-in needs an OAuth client id: enter it under Settings > Google, or \
                 set ESMAIL_GOOGLE_CLIENT_ID and ESMAIL_GOOGLE_CLIENT_SECRET. See the esmail README."
                    .to_string(),
            );
        }
        client
    }

    /// Open the system browser on Google's consent page for `account` and
    /// wait, in a background task, for the redirect back; the result arrives
    /// as an [`OAuthMessage`]. Replaces a sign-in already in progress for the
    /// *same* account; other accounts' sign-ins are independent (each has its
    /// own local redirect port).
    pub(super) fn begin_google_sign_in(&mut self, ctx: &egui::Context, account: AccountConfig) {
        if account.username.trim().is_empty() {
            self.push_banner("Enter your Gmail address in the Username field first.".to_string());
            return;
        }
        let Some(client) = self.google_client_or_explain() else { return };
        if let Some(previous) = self.oauth_tasks.remove(&account.id) {
            previous.abort();
        }
        let tx = self.oauth_tx.clone();
        let ctx = ctx.clone();
        self.status = format!("Waiting for Google sign-in of {} in your browser...", account.display_name);
        let id = account.id.clone();
        let task = tokio::spawn(async move {
            let message = match run_google_sign_in(&client, &account.username, &tx, &ctx).await {
                Ok(source) => OAuthMessage::Authorized { auth: auth::Auth::OAuth(source), account },
                Err(e) => OAuthMessage::Failed { account_id: account.id.clone(), error: format!("{e:#}") },
            };
            let _ = tx.send(message).await;
            // See `run_google_sign_in`'s note above on why `_of(ROOT)`.
            ctx.request_repaint_of(egui::ViewportId::ROOT);
        });
        self.oauth_tasks.insert(id, task);
    }

    /// Sign in again to a saved Google account (a revoked or expired token, or
    /// simply wanting to redo the consent page).
    pub(super) fn sign_in_again(&mut self, ctx: &egui::Context, account_id: &str) {
        let Some(mut account) = self.config.accounts.iter().find(|a| a.id == account_id).cloned() else { return };
        account.auth = config::AuthKind::GoogleOAuth;
        self.begin_google_sign_in(ctx, account);
    }

    /// Stop waiting for an account's browser sign-in. Aborting the task
    /// closes its redirect listener.
    pub(super) fn cancel_google_sign_in(&mut self, account_id: &str) {
        if let Some(task) = self.oauth_tasks.remove(account_id) {
            task.abort();
            self.status = "Ready".to_string();
        }
    }

    pub(super) fn handle_oauth_events(&mut self) {
        while let Ok(message) = self.oauth_rx.try_recv() {
            match message {
                OAuthMessage::BrowserUnavailable { url } => {
                    self.push_banner(format!("Could not open your browser. Open this address to sign in: {url}"));
                }
                OAuthMessage::Authorized { account, auth } => {
                    self.oauth_tasks.remove(&account.id);
                    // The account is the one the sign-in was started for, not
                    // whatever the form or a dialog holds now.
                    self.connect_account(account, auth, true);
                }
                OAuthMessage::Failed { account_id, error } => {
                    self.oauth_tasks.remove(&account_id);
                    self.status = "Ready".to_string();
                    let label = self.account_label(&account_id);
                    self.push_banner(format!("Google sign-in for {label} failed: {error}"));
                }
            }
        }
    }

    /// Save an account whose connection has just succeeded -- upsert it into
    /// `config.toml` and its credential into the OS keyring -- so a wrong
    /// password is never saved, and nothing is saved on every click. What the
    /// connection that succeeded actually used decides what is stored. A
    /// no-op for an account that came from the saved list.
    pub(super) fn persist_pending(&mut self, account_id: &str) {
        let Some((mut account, auth)) = self.view_mut(account_id).and_then(|v| v.pending_persist.take()) else {
            return;
        };
        match &auth {
            // No password anywhere: the refresh token is the credential, and
            // IMAP and SMTP both derive their access tokens from it.
            auth::Auth::OAuth(source) => {
                account.auth = config::AuthKind::GoogleOAuth;
                // A fresh browser sign-in is when Google issues the refresh
                // token and starts its Testing-mode 7-day clock; remember that
                // so the app can warn before the lapse (see
                // `oauth::refresh_token_expiring_soon`). Reusing a keyring
                // token reports no new issue time and leaves the stored one.
                if let Some(issued_at) = source.issued_at() {
                    account.oauth_token_issued_at = Some(issued_at);
                }
                if let Err(e) = secrets::set_password(&account.id, "oauth", &source.refresh_token()) {
                    log::warn!("could not save the Google sign-in to the OS keyring: {e}");
                }
                // An account that used to sign in with a password must not
                // leave that password behind, unused, in the keyring.
                secrets::delete_password(&account.id, "imap");
                secrets::delete_password(&account.id, "smtp");
            }
            // B7 sends with the same credentials as IMAP, since
            // `AccountConfig::username` is documented as used for both.
            auth::Auth::Password(password) => {
                account.auth = config::AuthKind::Password;
                account.oauth_token_issued_at = None;
                // ...and the reverse: a live refresh token for an account
                // that now uses a password.
                secrets::delete_password(&account.id, "oauth");
                if let Err(e) = secrets::set_password(&account.id, "imap", password) {
                    log::warn!("could not save IMAP password to the OS keyring: {e}");
                }
                if let Err(e) = secrets::set_password(&account.id, "smtp", password) {
                    log::warn!("could not save SMTP password to the OS keyring: {e}");
                }
            }
        }
        self.config.upsert_account(account);
        if let Err(e) = self.config.save() {
            log::warn!("could not persist account config: {e}");
        }
    }

    /// Forget a saved account for good: stop its session if it has one
    /// (connections and watcher included), cancel a sign-in in progress, and
    /// delete its config entry and every keyring secret.
    pub(super) fn remove_account(&mut self, id: &str) {
        self.cancel_google_sign_in(id);
        if self.view(id).is_some() {
            self.disconnect_account(id);
        }
        for kind in ["imap", "smtp", "oauth"] {
            secrets::delete_password(id, kind);
        }
        // Its cached mail goes with it: otherwise it stays on disk and keeps
        // turning up in search across accounts.
        let _ = self.db_tx.try_send(DbCommand::RemoveAccount { account_id: id.to_string() });
        if self.search_origins.iter().any(|(account, _)| account == id) {
            self.search_results = None;
            self.search_origins.clear();
        }
        self.config.remove_account(id);
        if let Err(e) = self.config.save() {
            log::warn!("could not persist account removal: {e}");
        }
    }

    /// Build the SMTP account `smtp.rs` needs to send *as* `account_id`, from
    /// its saved config entry and credential. A Google account sends with the
    /// very token source its IMAP session uses; a password account with the
    /// saved SMTP password. `None` if the account has not been saved yet (its
    /// first connection has not succeeded) or has no SMTP password on file.
    pub(super) fn smtp_account_for(&self, account_id: &str) -> Option<smtp::SmtpAccount> {
        let account = self.config.accounts.iter().find(|a| a.id == account_id)?;
        let auth = match self.view(account_id).map(|v| &v.auth) {
            Some(auth) if auth.is_oauth() => auth.clone(),
            _ => auth::Auth::Password(secrets::get_password(account_id, "smtp")?),
        };
        Some(smtp::SmtpAccount {
            host: account.smtp_host.clone(),
            port: account.smtp_port,
            tls: account.smtp_tls,
            username: account.username.clone(),
            auth,
            from_address: account.username.clone(),
        })
    }
}
