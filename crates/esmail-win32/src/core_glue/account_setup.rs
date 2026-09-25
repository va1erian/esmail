//! Trying an account's credentials and saving what worked: the work behind the
//! account form's Connect button, kept off the UI thread by the caller.
//!
//! Nothing is saved until both the IMAP login and the SMTP login succeed, so a
//! wrong password is never stored. Passwords go to the OS keyring
//! (`esmail::secrets`), never to `config.toml`, exactly as the egui frontend
//! stores them.

use std::time::Duration;

use esmail::auth::Auth;
use esmail::config::{AccountConfig, AuthKind, Config, OAuthClientConfig};
use esmail::oauth::{self, OAuthClient, SignInExpired};
use esmail::smtp::{self, SmtpAccount};
use esmail::{imap, secrets};

/// How long one server may take to answer before the test gives up: an
/// unreachable host otherwise waits for the operating system's connect timeout.
const SERVER_TIMEOUT: Duration = Duration::from_secs(20);

/// The keyring kinds an account can hold a secret under.
const SECRET_KINDS: [&str; 3] = ["imap", "smtp", "oauth"];

/// The client id and secret Google sign-in needs, or the explanation of how to
/// provide them. Google issues tokens only to registered applications, so this
/// cannot work out of the box.
pub fn google_client_or_explain(config: &Config) -> Result<OAuthClient, String> {
    oauth::google_client(config.google_oauth.as_ref()).ok_or_else(|| {
        "Google sign-in needs an OAuth client id. Set ESMAIL_GOOGLE_CLIENT_ID and \
         ESMAIL_GOOGLE_CLIENT_SECRET, or enter them under File > Settings.... \
         See the esMail README."
            .to_string()
    })
}

/// The `[google_oauth]` config entry for typed Settings fields: `None` when the
/// id is blank (sign-in off), and no secret when the secret is blank.
pub fn google_client_config(client_id: &str, client_secret: &str) -> Option<OAuthClientConfig> {
    let id = client_id.trim();
    if id.is_empty() {
        return None;
    }
    let secret = client_secret.trim();
    Some(OAuthClientConfig { client_id: id.to_string(), client_secret: (!secret.is_empty()).then(|| secret.to_string()) })
}

/// Google credentials for `account`: the refresh token saved from an earlier
/// approval, unless `sign_in_again`, else the whole browser round trip (consent
/// page, redirect, code exchange). `awaiting_browser` is called once the user
/// has to act in the browser; `browser_failed` gets the address when the system
/// browser cannot be opened, so the user can open it by hand (the sign-in keeps
/// waiting).
pub async fn google_auth(
    client: &OAuthClient,
    account: &AccountConfig,
    sign_in_again: bool,
    awaiting_browser: impl FnOnce(),
    browser_failed: impl FnOnce(String),
) -> Result<Auth, String> {
    if !sign_in_again {
        if let Some(refresh_token) = secrets::get_password(&account.id, "oauth") {
            return Ok(Auth::OAuth(oauth::TokenSource::from_refresh_token(client.clone(), refresh_token)));
        }
    }
    awaiting_browser();
    let sign_in = async {
        let pending = oauth::begin(client, &account.username).await?;
        if let Err(error) = opener::open_browser(&pending.url) {
            log::warn!("could not open the browser for Google sign-in: {error}");
            browser_failed(pending.url.clone());
        }
        let grant = pending.finish(client).await?;
        oauth::TokenSource::from_grant(client.clone(), grant)
    };
    match sign_in.await {
        Ok(source) => Ok(Auth::OAuth(source)),
        Err(error) => Err(format!("Google sign-in did not finish: {error:#}")),
    }
}

/// Logs in to IMAP and SMTP with `auth`. `Err` is a message fit for the form,
/// saying which server failed and why.
pub async fn test_connection(account: &AccountConfig, auth: &Auth) -> Result<(), String> {
    let imap_login = imap::check_login(&account.imap_host, account.imap_port, &account.username, auth);
    match tokio::time::timeout(SERVER_TIMEOUT, imap_login).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(explain(Server::Imap, account, &error)),
        Err(_) => return Err(explain_timeout(Server::Imap, account)),
    }
    let smtp = SmtpAccount {
        host: account.smtp_host.clone(),
        port: account.smtp_port,
        tls: account.smtp_tls,
        username: account.username.clone(),
        auth: auth.clone(),
        from_address: account.username.clone(),
    };
    match tokio::time::timeout(SERVER_TIMEOUT, smtp::check_login(&smtp)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(explain(Server::Smtp, account, &error)),
        Err(_) => Err(explain_timeout(Server::Smtp, account)),
    }
}

/// Which server a failure came from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Server {
    Imap,
    Smtp,
}

impl Server {
    fn name(self) -> &'static str {
        match self {
            Server::Imap => "IMAP",
            Server::Smtp => "SMTP",
        }
    }

    fn host_port(self, account: &AccountConfig) -> (&str, u16) {
        match self {
            Server::Imap => (&account.imap_host, account.imap_port),
            Server::Smtp => (&account.smtp_host, account.smtp_port),
        }
    }
}

fn explain_timeout(server: Server, account: &AccountConfig) -> String {
    let (host, port) = server.host_port(account);
    format!("The {} server {host}:{port} did not answer within {} seconds. Check the host and port, and your network.", server.name(), SERVER_TIMEOUT.as_secs())
}

/// A failure in words a user can act on, with the server's own message after it
/// when that says more.
fn explain(server: Server, account: &AccountConfig, error: &anyhow::Error) -> String {
    if let Some(expired) = error.downcast_ref::<SignInExpired>() {
        return format!("Google sign-in has expired: {expired}. Choose Sign in again.");
    }
    let detail = format!("{error:#}");
    let (host, port) = server.host_port(account);
    let lower = detail.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|needle| lower.contains(needle));
    let headline = if has(&["certificate", "cert_", "sec_e_", "untrusted", "self signed", "self-signed", "handshake"]) {
        format!("Could not make a secure connection to {host}: the server's certificate is not trusted, or the port does not speak TLS. Check the host name and the security setting.")
    } else if has(&["no such host", "failed to lookup", "11001", "name or service not known", "nodename nor servname", "dns", "no address associated"]) {
        format!("Could not find the {} server {host}. Check the spelling of the host name.", server.name())
    } else if has(&["refused", "10061"]) {
        format!("The {} server {host} refused the connection on port {port}. Check the port, and that the server is running.", server.name())
    } else if has(&["timed out", "10060", "unreachable", "10065", "10051"]) {
        format!("The {} server {host}:{port} could not be reached. Check the host and port, and your network.", server.name())
    } else if has(&["authenticationfailed", "authentication failed", "invalid credentials", "login failed", "auth", "password", "credentials", "no response"]) {
        match account.auth {
            AuthKind::Password => format!("The {} server rejected the email address or password. Check both; Gmail, Outlook and iCloud need an app password.", server.name()),
            AuthKind::GoogleOAuth => format!("The {} server rejected the Google sign-in. Choose Sign in again.", server.name()),
        }
    } else {
        format!("Could not connect to the {} server {host}:{port}.", server.name())
    };
    format!("{headline}\nServer said: {detail}")
}

fn keyring_failed(error: impl std::fmt::Display) -> String {
    format!("Could not save the credentials to the OS keyring: {error}")
}

/// Saves `account` and its credential: the secret to the OS keyring, the
/// account to `config.toml`. Returns the saved configuration. Nothing is
/// written to `config.toml` if the keyring refuses the secret.
pub fn save_account(config: &Config, mut account: AccountConfig, auth: &Auth) -> Result<Config, String> {
    match auth {
        Auth::OAuth(source) => {
            account.auth = AuthKind::GoogleOAuth;
            // Google starts its Testing-mode 7-day clock at a fresh sign-in;
            // a token restored from the keyring reports no new issue time.
            if let Some(issued_at) = source.issued_at() {
                account.oauth_token_issued_at = Some(issued_at);
            }
            secrets::set_password(&account.id, "oauth", &source.refresh_token()).map_err(keyring_failed)?;
            secrets::delete_password(&account.id, "imap");
            secrets::delete_password(&account.id, "smtp");
        }
        Auth::Password(password) => {
            account.auth = AuthKind::Password;
            account.oauth_token_issued_at = None;
            secrets::set_password(&account.id, "imap", password).map_err(keyring_failed)?;
            secrets::set_password(&account.id, "smtp", password).map_err(keyring_failed)?;
            secrets::delete_password(&account.id, "oauth");
        }
    }
    let mut saved = config.clone();
    saved.upsert_account(account);
    saved.save().map_err(|error| format!("Could not write config.toml: {error}"))?;
    Ok(saved)
}

/// Forgets the account `id`: its `config.toml` entry and every keyring secret.
pub fn remove_account(config: &Config, id: &str) -> Result<Config, String> {
    let mut saved = config.clone();
    saved.remove_account(id);
    saved.save().map_err(|error| format!("Could not write config.toml: {error}"))?;
    for kind in SECRET_KINDS {
        secrets::delete_password(id, kind);
    }
    Ok(saved)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(auth: AuthKind) -> AccountConfig {
        let mut account = AccountConfig::new("Me".into(), "imap.example.com".into(), 993, "me@example.com".into());
        account.auth = auth;
        account
    }

    fn explained(server: Server, auth: AuthKind, message: &str) -> String {
        explain(server, &account(auth), &anyhow::anyhow!("{message}"))
    }

    #[test]
    fn a_rejected_password_is_named_as_such() {
        let text = explained(Server::Imap, AuthKind::Password, "No Response: LOGIN failed");
        assert!(text.starts_with("The IMAP server rejected the email address or password"), "{text}");
        assert!(text.contains("Server said: No Response: LOGIN failed"));
    }

    #[test]
    fn an_smtp_rejection_says_smtp() {
        let text = explained(Server::Smtp, AuthKind::Password, "permanent error (535): 5.7.8 Authentication credentials invalid");
        assert!(text.starts_with("The SMTP server rejected"), "{text}");
    }

    #[test]
    fn a_rejected_google_sign_in_offers_signing_in_again() {
        let text = explained(Server::Imap, AuthKind::GoogleOAuth, "AUTHENTICATE failed");
        assert!(text.contains("Sign in again"), "{text}");
    }

    #[test]
    fn tls_trouble_mentions_the_certificate_and_the_host() {
        let text = explained(Server::Imap, AuthKind::Password, "The certificate chain was issued by an authority that is not trusted. (os error -2146762487)");
        assert!(text.contains("certificate is not trusted") && text.contains("imap.example.com"), "{text}");
    }

    #[test]
    fn a_missing_host_is_named() {
        let text = explained(Server::Imap, AuthKind::Password, "No such host is known. (os error 11001)");
        assert!(text.starts_with("Could not find the IMAP server imap.example.com"), "{text}");
    }

    #[test]
    fn a_closed_port_is_named() {
        let text = explained(Server::Smtp, AuthKind::Password, "No connection could be made because the target machine actively refused it. (os error 10061)");
        assert!(text.contains("refused the connection on port 465"), "{text}");
    }

    #[test]
    fn an_unknown_failure_still_shows_what_the_server_said() {
        let text = explained(Server::Imap, AuthKind::Password, "something odd");
        assert!(text.starts_with("Could not connect to the IMAP server imap.example.com:993."), "{text}");
        assert!(text.ends_with("Server said: something odd"));
    }

    #[test]
    fn an_expired_google_sign_in_says_so() {
        let error = anyhow::Error::new(SignInExpired("Google revoked the token".into()));
        let text = explain(Server::Imap, &account(AuthKind::GoogleOAuth), &error);
        assert!(text.starts_with("Google sign-in has expired: Google revoked the token"), "{text}");
    }

    #[test]
    fn a_blank_client_id_turns_google_sign_in_off() {
        assert_eq!(google_client_config("  ", "secret"), None);
        assert_eq!(google_client_config("", ""), None);
    }

    #[test]
    fn a_typed_client_is_trimmed_and_keeps_a_blank_secret_unset() {
        let client = google_client_config("  id.apps.googleusercontent.com  ", " ").unwrap();
        assert_eq!(client, OAuthClientConfig { client_id: "id.apps.googleusercontent.com".into(), client_secret: None });
        let with_secret = google_client_config("id", " s3cret ").unwrap();
        assert_eq!(with_secret.client_secret.as_deref(), Some("s3cret"));
    }
}
