//! OAuth2 sign-in for Gmail, so an account does not need an app password.
//!
//! The flow is the standard "installed application" one (RFC 8252):
//!
//! 1. [`begin`] binds a listener on `127.0.0.1:<free port>` and builds the
//!    Google consent URL, with a PKCE challenge (RFC 7636) and a random
//!    `state`. The caller opens that URL in the system browser.
//! 2. [`PendingAuthorization::finish`] waits for Google to redirect the
//!    browser back to the listener, checks `state`, and exchanges the
//!    returned code (plus the PKCE verifier) for tokens.
//! 3. The long-lived *refresh token* is what gets kept (in the OS keyring, see
//!    `secrets`). The short-lived *access token* -- the thing IMAP/SMTP
//!    `XOAUTH2` actually presents -- is refreshed on demand by [`TokenSource`].
//!
//! **A client id is required, and it is yours.** Google only issues tokens to
//! registered applications, so esmail cannot ship a working client id of its
//! own the way it ships provider host names. Create a "Desktop app" OAuth
//! client in Google Cloud Console and hand its id (and secret -- Google issues
//! one for desktop clients and its token endpoint insists on it, though it is
//! not confidential in the way a server's would be) to esmail through the
//! environment, `config.toml`, or a build-time variable: see [`google_client`]
//! and the README.
//!
//! **Not done:** revoking the refresh token at Google when an account is
//! forgotten (it is only deleted locally; revoke it under Google Account >
//! Security > Third-party access), and providers other than Google.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::config::OAuthClientConfig;

pub const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
/// Full IMAP/SMTP access. Google has no narrower scope that works for both.
pub const GOOGLE_MAIL_SCOPE: &str = "https://mail.google.com/";

/// An access token this close to expiring is refreshed instead of used, so it
/// cannot lapse between being handed out and the server checking it.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);
/// How long to wait for the user to finish in the browser.
const AUTHORIZE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// A connection to the redirect listener that says nothing for this long is
/// dropped, so a stray local connection cannot hold up the real redirect.
const REDIRECT_READ_TIMEOUT: Duration = Duration::from_secs(5);
const TOKEN_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Google's access tokens last an hour; anything claiming longer than a day
/// is clamped to a day rather than trusted.
const MAX_TOKEN_LIFETIME_SECS: u64 = 24 * 60 * 60;
/// Google expires a refresh token after this long while the OAuth app is
/// still in "Testing" (unverified). Testing mode is accepted for now (issue
/// #71); the app warns before the lapse rather than only failing on the next
/// connect.
pub const TESTING_MODE_REFRESH_TOKEN_LIFETIME: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Start warning this long before that expiry: long enough to act, short
/// enough not to nag.
pub const REFRESH_TOKEN_WARNING_MARGIN: Duration = Duration::from_secs(2 * 24 * 60 * 60);

/// The saved sign-in can no longer be used and only a fresh browser sign-in
/// will help. Its own type so retry loops can tell it from a transient
/// failure (network down, server hiccup) and stop instead of hammering the
/// token endpoint; the `Display` text is what the user sees.
#[derive(Debug)]
pub struct SignInExpired(pub String);

impl std::fmt::Display for SignInExpired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SignInExpired {}

/// Unix seconds right now -- the clock [`refresh_token_expiring_soon`] and
/// `AccountConfig::oauth_token_issued_at` are expressed in.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Whether a Google account whose refresh token was issued at `issued_at`
/// (unix seconds) should be warned, at `now`, that Google's Testing-mode
/// expiry is close. `None` -- an account saved before the issue time was
/// recorded, or a token restored from the keyring -- is never a warning:
/// there is nothing to base a countdown on, and the [`SignInExpired`] path
/// still catches it on the next connect.
pub fn refresh_token_expiring_soon(issued_at: Option<i64>, now: i64) -> bool {
    let Some(issued_at) = issued_at else { return false };
    now.saturating_add(REFRESH_TOKEN_WARNING_MARGIN.as_secs() as i64)
        >= issued_at.saturating_add(TESTING_MODE_REFRESH_TOKEN_LIFETIME.as_secs() as i64)
}

/// A registered OAuth application.
#[derive(Clone)]
pub struct OAuthClient {
    pub client_id: String,
    pub client_secret: Option<String>,
    pub auth_url: String,
    pub token_url: String,
    pub scope: String,
}

impl OAuthClient {
    pub fn google(client_id: impl Into<String>, client_secret: Option<String>) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret,
            auth_url: GOOGLE_AUTH_URL.to_string(),
            token_url: GOOGLE_TOKEN_URL.to_string(),
            scope: GOOGLE_MAIL_SCOPE.to_string(),
        }
    }
}

/// Where the Google OAuth client in use came from, in order of precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientSource {
    /// The `ESMAIL_GOOGLE_CLIENT_ID` / `ESMAIL_GOOGLE_CLIENT_SECRET`
    /// environment variables.
    Environment,
    /// `[google_oauth]` in `config.toml`, which the Settings window edits.
    Config,
    /// The same variables read when esmail was *built*, for a distribution
    /// that bakes its own client in.
    Build,
}

impl ClientSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Environment => "environment variables",
            Self::Config => "saved settings",
            Self::Build => "the build",
        }
    }
}

/// Find the Google OAuth client to sign in with, or `None` if none is
/// configured. See [`ClientSource`] for where it can come from, and
/// [`google_client_with_source`] to also learn which one won.
pub fn google_client(configured: Option<&OAuthClientConfig>) -> Option<OAuthClient> {
    google_client_with_source(configured).map(|(client, _)| client)
}

/// [`google_client`], plus the source that supplied the client.
pub fn google_client_with_source(configured: Option<&OAuthClientConfig>) -> Option<(OAuthClient, ClientSource)> {
    let env = |name: &str| std::env::var(name).ok();
    resolve_google_client(
        (env("ESMAIL_GOOGLE_CLIENT_ID"), env("ESMAIL_GOOGLE_CLIENT_SECRET")),
        configured,
        (
            option_env!("ESMAIL_GOOGLE_CLIENT_ID").map(str::to_string),
            option_env!("ESMAIL_GOOGLE_CLIENT_SECRET").map(str::to_string),
        ),
    )
}

/// The precedence logic of [`google_client_with_source`], with the ambient
/// environment passed in so it can be tested. An id and its secret always
/// come from the same source, so an id from one place is never paired with a
/// secret from another; blank values count as absent.
fn resolve_google_client(
    environment: (Option<String>, Option<String>),
    configured: Option<&OAuthClientConfig>,
    build: (Option<String>, Option<String>),
) -> Option<(OAuthClient, ClientSource)> {
    let non_empty = |v: Option<&str>| v.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());

    let candidates = [
        (ClientSource::Environment, non_empty(environment.0.as_deref()), non_empty(environment.1.as_deref())),
        (
            ClientSource::Config,
            configured.and_then(|c| non_empty(Some(&c.client_id))),
            configured.and_then(|c| non_empty(c.client_secret.as_deref())),
        ),
        (ClientSource::Build, non_empty(build.0.as_deref()), non_empty(build.1.as_deref())),
    ];
    candidates
        .into_iter()
        .find_map(|(source, id, secret)| id.map(|id| (OAuthClient::google(id, secret), source)))
}

// ── PKCE ────────────────────────────────────────────────────────────────────

fn random_url_safe(byte_len: usize) -> anyhow::Result<String> {
    let mut bytes = vec![0u8; byte_len];
    getrandom::fill(&mut bytes).map_err(|e| anyhow!("could not read OS randomness: {e}"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// `BASE64URL(SHA256(verifier))`, the `S256` code challenge.
fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

// ── Browser round trip ──────────────────────────────────────────────────────

/// A sign-in that has been started: the listener is bound and waiting, and
/// `url` is what the user has to open.
pub struct PendingAuthorization {
    /// The Google consent URL to open in the browser.
    pub url: String,
    listener: TcpListener,
    redirect_uri: String,
    state: String,
    verifier: String,
}

/// Start a sign-in: bind the loopback listener and build the consent URL.
/// `login_hint` (the account's email address) pre-selects that account on
/// Google's account chooser; pass `""` for none.
pub async fn begin(client: &OAuthClient, login_hint: &str) -> anyhow::Result<PendingAuthorization> {
    // The port is only known once bound, and the redirect URI has to name it.
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.context("could not open a local port for the sign-in redirect")?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}");

    let verifier = random_url_safe(32)?;
    let state = random_url_safe(16)?;

    let mut params = vec![
        ("client_id", client.client_id.as_str()),
        ("redirect_uri", redirect_uri.as_str()),
        ("response_type", "code"),
        ("scope", client.scope.as_str()),
        ("state", state.as_str()),
        ("code_challenge_method", "S256"),
    ];
    let challenge = pkce_challenge(&verifier);
    params.push(("code_challenge", challenge.as_str()));
    // Without these Google only returns a refresh token the first time an
    // account ever consents, and a later sign-in would come back without one.
    params.push(("access_type", "offline"));
    params.push(("prompt", "consent"));
    if !login_hint.trim().is_empty() {
        params.push(("login_hint", login_hint.trim()));
    }
    let url = url::Url::parse_with_params(&client.auth_url, &params).context("invalid OAuth authorization URL")?;

    Ok(PendingAuthorization { url: url.to_string(), listener, redirect_uri, state, verifier })
}

impl PendingAuthorization {
    /// Wait for the browser to come back, then trade the code for tokens.
    /// Gives up after five minutes. Dropping this (or aborting the task
    /// running it) closes the listener, which is how "Cancel" works.
    pub async fn finish(self, client: &OAuthClient) -> anyhow::Result<TokenGrant> {
        let code = tokio::time::timeout(AUTHORIZE_TIMEOUT, await_redirect(&self.listener, &self.state))
            .await
            .map_err(|_| anyhow!("timed out waiting for the Google sign-in to finish in the browser"))??;

        let mut form = client_form(client);
        form.push(("grant_type", "authorization_code".to_string()));
        form.push(("code", code));
        form.push(("redirect_uri", self.redirect_uri));
        form.push(("code_verifier", self.verifier));
        token_request(client, form).await
    }
}

/// Serve the redirect: returns the authorization `code` once a request with
/// the right `state` arrives. Requests that are not the redirect (a browser's
/// favicon fetch) or that carry the wrong `state` (anything else on this
/// machine poking the port) are answered and ignored rather than aborting
/// the sign-in -- otherwise any local process could cancel it.
async fn await_redirect(listener: &TcpListener, expected_state: &str) -> anyhow::Result<String> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        let request_line = match tokio::time::timeout(REDIRECT_READ_TIMEOUT, read_request_line(&mut stream)).await {
            Ok(Ok(line)) => line,
            _ => continue,
        };
        // "GET /?code=...&state=... HTTP/1.1"
        let Some(target) = request_line.split_whitespace().nth(1) else {
            respond(&mut stream, "400 Bad Request", "Bad request.").await;
            continue;
        };
        let Ok(url) = url::Url::parse(&format!("http://127.0.0.1{target}")) else {
            respond(&mut stream, "400 Bad Request", "Bad request.").await;
            continue;
        };
        if url.path() != "/" {
            respond(&mut stream, "404 Not Found", "Not found.").await;
            continue;
        }

        let param = |name: &str| url.query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.into_owned());
        if param("state").as_deref() != Some(expected_state) {
            respond(&mut stream, "400 Bad Request", "This sign-in link does not match the request esmail made.").await;
            continue;
        }
        if let Some(error) = param("error") {
            respond(&mut stream, "200 OK", "Sign-in was not completed. You can close this tab and return to esmail.").await;
            bail!("Google sign-in was not completed ({error})");
        }
        let Some(code) = param("code") else {
            respond(&mut stream, "400 Bad Request", "Missing authorization code.").await;
            continue;
        };
        respond(&mut stream, "200 OK", "Signed in. You can close this tab and return to esmail.").await;
        return Ok(code);
    }
}

/// Read up to the end of the request's first line (bounded, so a client that
/// never sends a newline cannot grow the buffer without limit).
async fn read_request_line(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while buf.len() < 8192 && !buf.contains(&b'\n') {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let end = buf.iter().position(|&b| b == b'\n').unwrap_or(buf.len());
    Ok(String::from_utf8_lossy(&buf[..end]).trim_end_matches('\r').to_string())
}

/// A minimal one-shot HTTP reply. `message` is always esmail's own static
/// text -- nothing from the request is reflected into the page.
async fn respond(stream: &mut TcpStream, status: &str, message: &str) {
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>esmail</title></head>\
         <body style=\"font-family:sans-serif;margin:3em\"><p>{message}</p></body></html>"
    );
    let reply = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(reply.as_bytes()).await;
    let _ = stream.shutdown().await;
}

// ── Token endpoint ──────────────────────────────────────────────────────────

/// What the token endpoint handed back.
pub struct TokenGrant {
    pub access_token: SecretString,
    pub expires_at: Instant,
    /// Present on the first exchange (given `access_type=offline`); usually
    /// absent when refreshing, since the existing one stays valid.
    pub refresh_token: Option<SecretString>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    expires_in: Option<u64>,
    refresh_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

fn client_form(client: &OAuthClient) -> Vec<(&'static str, String)> {
    let mut form = vec![("client_id", client.client_id.clone())];
    if let Some(secret) = &client.client_secret {
        form.push(("client_secret", secret.clone()));
    }
    form
}

async fn token_request(client: &OAuthClient, form: Vec<(&'static str, String)>) -> anyhow::Result<TokenGrant> {
    let url = client.token_url.clone();
    // `ureq` is blocking, so it runs off the async workers.
    tokio::task::spawn_blocking(move || post_token_form(&url, &form)).await?
}

fn post_token_form(url: &str, form: &[(&'static str, String)]) -> anyhow::Result<TokenGrant> {
    // Google explains a rejected grant in the body of a 4xx, so a non-2xx
    // status must come back as a response rather than a bare `Err`.
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(TOKEN_REQUEST_TIMEOUT)).build(),
    );
    let mut response = agent
        .post(url)
        .send_form(form.iter().map(|(k, v)| (*k, v.as_str())))
        .context("could not reach the OAuth token endpoint")?;
    let status = response.status();
    let body = response.body_mut().read_to_string().context("could not read the OAuth token response")?;
    parse_token_response(status.as_u16(), &body, Instant::now())
}

fn parse_token_response(status: u16, body: &str, now: Instant) -> anyhow::Result<TokenGrant> {
    let parsed: TokenResponse = serde_json::from_str(body)
        .map_err(|_| anyhow!("the OAuth token endpoint answered HTTP {status} with something that is not JSON"))?;

    if let Some(error) = parsed.error {
        let detail = parsed.error_description.map(|d| format!(": {d}")).unwrap_or_default();
        match error.as_str() {
            // The refresh token was revoked, expired (Google expires them
            // after 7 days while an app is in "Testing"), or the account's
            // password changed -- or it was issued to a different OAuth
            // client than the one now configured (`unauthorized_client`),
            // e.g. after the client id was changed in Settings.
            "invalid_grant" | "unauthorized_client" => {
                return Err(SignInExpired(format!(
                    "Google no longer accepts the saved sign-in{detail}. Sign in with Google again."
                ))
                .into());
            }
            "invalid_client" => {
                bail!("Google rejected the OAuth client ID or secret{detail}. Check them under Settings.")
            }
            _ => bail!("Google sign-in failed ({error}{detail})"),
        }
    }
    let access_token = parsed.access_token.ok_or_else(|| anyhow!("the OAuth token response had no access_token (HTTP {status})"))?;
    // Bounded, because `Instant + Duration` panics on overflow and this
    // number comes off the network.
    let lifetime = Duration::from_secs(parsed.expires_in.unwrap_or(3600).min(MAX_TOKEN_LIFETIME_SECS));
    Ok(TokenGrant {
        access_token: SecretString::from(access_token),
        expires_at: now + lifetime,
        refresh_token: parsed.refresh_token.map(SecretString::from),
    })
}

// ── Access tokens over time ─────────────────────────────────────────────────

/// The refresh token plus a cached access token, shared (behind an `Arc`, see
/// [`crate::auth::Auth`]) by every connection an account opens.
pub struct TokenSource {
    client: OAuthClient,
    /// A plain mutex, never held across an `.await`, so the UI thread can
    /// read it synchronously to save the token to the keyring.
    refresh_token: StdMutex<SecretString>,
    /// Unix seconds a fresh sign-in issued the refresh token, for the
    /// Testing-mode expiry warning; `None` for a token restored from the
    /// keyring, whose issue time predates this.
    issued_at: Option<i64>,
    /// Also serializes refreshes: the actor, the body worker and the IDLE
    /// watch can all need a token at once, and all but the first should find
    /// a fresh one waiting rather than each hitting the token endpoint.
    access: tokio::sync::Mutex<Option<(SecretString, Instant)>>,
}

impl TokenSource {
    /// From a refresh token saved earlier; the first use fetches an access token.
    pub fn from_refresh_token(client: OAuthClient, refresh_token: SecretString) -> Arc<Self> {
        Arc::new(Self { client, refresh_token: StdMutex::new(refresh_token), issued_at: None, access: tokio::sync::Mutex::new(None) })
    }

    /// From a fresh sign-in, reusing the access token it already returned.
    pub fn from_grant(client: OAuthClient, grant: TokenGrant) -> anyhow::Result<Arc<Self>> {
        let refresh_token = grant.refresh_token.ok_or_else(|| {
            anyhow!(
                "Google did not return a refresh token, so the sign-in cannot be kept. Remove esmail under \
                 Google Account > Security > Third-party access and sign in again."
            )
        })?;
        Ok(Arc::new(Self {
            client,
            refresh_token: StdMutex::new(refresh_token),
            issued_at: Some(now_unix()),
            access: tokio::sync::Mutex::new(Some((grant.access_token, grant.expires_at))),
        }))
    }

    /// The current refresh token, for saving to the keyring.
    pub fn refresh_token(&self) -> SecretString {
        self.refresh_token.lock().expect("refresh token mutex is never poisoned").clone()
    }

    /// Unix seconds a fresh sign-in issued this refresh token (`from_grant`),
    /// or `None` for one restored from the keyring.
    pub fn issued_at(&self) -> Option<i64> {
        self.issued_at
    }

    /// A valid access token, refreshing it first if needed.
    pub async fn access_token(&self) -> anyhow::Result<SecretString> {
        let mut cached = self.access.lock().await;
        if let Some((token, expires_at)) = cached.as_ref() {
            if *expires_at > Instant::now() + EXPIRY_MARGIN {
                return Ok(token.clone());
            }
        }

        let mut form = client_form(&self.client);
        form.push(("grant_type", "refresh_token".to_string()));
        form.push(("refresh_token", self.refresh_token().expose_secret().to_string()));
        let grant = token_request(&self.client, form).await?;

        if let Some(rotated) = grant.refresh_token {
            *self.refresh_token.lock().expect("refresh token mutex is never poisoned") = rotated;
        }
        *cached = Some((grant.access_token.clone(), grant.expires_at));
        Ok(grant.access_token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_the_rfc_7636_example() {
        // RFC 7636 appendix B.
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn random_values_are_url_safe_and_different_each_time() {
        let a = random_url_safe(32).unwrap();
        let b = random_url_safe(32).unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), 43); // 32 bytes, unpadded base64url
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn token_response_yields_a_grant() {
        let now = Instant::now();
        let grant = parse_token_response(
            200,
            r#"{"access_token":"ya29.a","expires_in":3599,"refresh_token":"1//r","token_type":"Bearer"}"#,
            now,
        )
        .unwrap();
        assert_eq!(grant.access_token.expose_secret(), "ya29.a");
        assert_eq!(grant.refresh_token.unwrap().expose_secret(), "1//r");
        assert_eq!(grant.expires_at, now + Duration::from_secs(3599));
    }

    #[test]
    fn invalid_grant_tells_the_user_to_sign_in_again() {
        let err = parse_token_response(
            400,
            r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#,
            Instant::now(),
        )
        .err()
        .unwrap()
        .to_string();
        assert!(err.contains("Sign in with Google again"), "{err}");
        assert!(err.contains("expired or revoked"), "{err}");
    }

    #[test]
    fn a_revoked_or_foreign_token_is_a_typed_sign_in_expired_error() {
        for body in [r#"{"error":"invalid_grant"}"#, r#"{"error":"unauthorized_client"}"#] {
            let err = parse_token_response(400, body, Instant::now()).err().unwrap();
            assert!(err.is::<SignInExpired>(), "{body}: {err}");
        }
        // Transient or configuration problems are not.
        let err = parse_token_response(500, r#"{"error":"server_error"}"#, Instant::now()).err().unwrap();
        assert!(!err.is::<SignInExpired>());
    }

    #[test]
    fn a_wrong_client_id_or_secret_points_at_settings() {
        let err = parse_token_response(401, r#"{"error":"invalid_client"}"#, Instant::now()).err().unwrap();
        assert!(!err.is::<SignInExpired>());
        assert!(err.to_string().contains("Settings"), "{err}");
    }

    #[test]
    fn an_absurd_expires_in_is_clamped_instead_of_overflowing() {
        let now = Instant::now();
        let grant = parse_token_response(200, &format!(r#"{{"access_token":"a","expires_in":{}}}"#, u64::MAX), now).unwrap();
        assert_eq!(grant.expires_at, now + Duration::from_secs(MAX_TOKEN_LIFETIME_SECS));
    }

    #[test]
    fn other_token_errors_and_garbage_are_reported() {
        let err = parse_token_response(400, r#"{"error":"redirect_uri_mismatch"}"#, Instant::now()).err().unwrap().to_string();
        assert!(err.contains("redirect_uri_mismatch"), "{err}");
        let err = parse_token_response(502, "<html>Bad gateway</html>", Instant::now()).err().unwrap().to_string();
        assert!(err.contains("502"), "{err}");
    }

    #[test]
    fn refresh_token_warning_starts_two_days_before_the_seven_day_expiry() {
        const DAY: i64 = 24 * 60 * 60;
        let issued = 1_700_000_000;
        assert!(!refresh_token_expiring_soon(Some(issued), issued), "brand new");
        assert!(!refresh_token_expiring_soon(Some(issued), issued + 5 * DAY - 1), "just outside the margin");
        assert!(refresh_token_expiring_soon(Some(issued), issued + 5 * DAY), "enters the margin");
        assert!(refresh_token_expiring_soon(Some(issued), issued + 7 * DAY), "at the expiry");
        assert!(refresh_token_expiring_soon(Some(issued), issued + 30 * DAY), "long past it");
    }

    #[test]
    fn an_unrecorded_issue_time_is_never_a_warning() {
        assert!(!refresh_token_expiring_soon(None, 0));
        assert!(!refresh_token_expiring_soon(None, i64::MAX));
    }

    fn pair(id: &str, secret: &str) -> (Option<String>, Option<String>) {
        (Some(id.to_string()), Some(secret.to_string()))
    }

    const NONE: (Option<String>, Option<String>) = (None, None);

    #[test]
    fn a_configured_client_is_trimmed_and_carries_its_secret() {
        let configured = OAuthClientConfig { client_id: " abc ".into(), client_secret: Some("shh".into()) };
        let (client, source) = resolve_google_client(NONE, Some(&configured), NONE).unwrap();
        assert_eq!(source, ClientSource::Config);
        assert_eq!(client.client_id, "abc");
        assert_eq!(client.client_secret.as_deref(), Some("shh"));
        assert_eq!(client.scope, GOOGLE_MAIL_SCOPE);
    }

    #[test]
    fn environment_beats_config_beats_build() {
        let configured = OAuthClientConfig { client_id: "from-config".into(), client_secret: Some("cfg-secret".into()) };

        let (client, source) =
            resolve_google_client(pair("from-env", "env-secret"), Some(&configured), pair("from-build", "b")).unwrap();
        assert_eq!((source, client.client_id.as_str()), (ClientSource::Environment, "from-env"));

        let (client, source) = resolve_google_client(NONE, Some(&configured), pair("from-build", "b")).unwrap();
        assert_eq!((source, client.client_id.as_str()), (ClientSource::Config, "from-config"));

        let (client, source) = resolve_google_client(NONE, None, pair("from-build", "b")).unwrap();
        assert_eq!((source, client.client_id.as_str()), (ClientSource::Build, "from-build"));
    }

    #[test]
    fn an_id_is_never_paired_with_another_sources_secret() {
        // The environment supplies only an id; the config's secret must not
        // be borrowed for it.
        let configured = OAuthClientConfig { client_id: "cfg".into(), client_secret: Some("cfg-secret".into()) };
        let (client, source) =
            resolve_google_client((Some("env-id".into()), None), Some(&configured), NONE).unwrap();
        assert_eq!(source, ClientSource::Environment);
        assert_eq!(client.client_secret, None);
    }

    #[test]
    fn blank_values_count_as_absent() {
        assert!(resolve_google_client(NONE, None, NONE).is_none());
        let blank = OAuthClientConfig { client_id: "  ".into(), client_secret: Some("orphan".into()) };
        assert!(resolve_google_client((Some("".into()), None), Some(&blank), NONE).is_none());
    }

    #[tokio::test]
    async fn begin_builds_a_pkce_consent_url_pointing_at_a_live_loopback_port() {
        let client = OAuthClient::google("the-client-id", None);
        let pending = begin(&client, "alice@gmail.com").await.unwrap();
        let url = url::Url::parse(&pending.url).unwrap();
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();

        assert_eq!(url.host_str(), Some("accounts.google.com"));
        assert_eq!(q["client_id"], "the-client-id");
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["scope"], GOOGLE_MAIL_SCOPE);
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["code_challenge"], pkce_challenge(&pending.verifier));
        assert_eq!(q["access_type"], "offline");
        assert_eq!(q["login_hint"], "alice@gmail.com");
        assert_eq!(q["state"], pending.state);
        assert!(q["redirect_uri"].starts_with("http://127.0.0.1:"));
        assert_eq!(q["redirect_uri"], pending.redirect_uri);
    }
}
