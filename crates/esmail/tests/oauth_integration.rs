//! "Sign in with Google" without Google: the OAuth flow of `esmail::oauth`
//! against a fake token endpoint, and `XOAUTH2` login over IMAP, SMTP and
//! IDLE against `mail-mock-server` (which accepts a per-user access token
//! alongside its password -- see `Store::add_oauth_token`).
//!
//! The flow tests need nothing but loopback sockets. The mail-server tests
//! need the mock server's test CA trusted, exactly like
//! `imap_smtp_integration.rs`, and skip the same way without it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use esmail::auth::Auth;
use esmail::compose::ComposeState;
use esmail::idle_watch;
use esmail::imap::{ImapActor, ImapCommand, ImapEvent};
use esmail::oauth::{self, OAuthClient, TokenGrant, TokenSource};
use esmail::smtp::{SmtpAccount, SmtpActor, SmtpCommand, SmtpEvent};
use mail_mock_server::fixtures::TEST_USER;
use secrecy::{ExposeSecret, SecretString};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::timeout;

const RECV_TIMEOUT: Duration = Duration::from_secs(20);

macro_rules! skip_unless_ca_trusted {
    () => {
        if std::env::var("ESMAIL_TEST_CA_TRUSTED").is_err() {
            eprintln!(
                "skipping: set ESMAIL_TEST_CA_TRUSTED=1 once mail-mock-server/certs/ca.crt is trusted \
                 (see crates/mail-mock-server/README.md)"
            );
            return;
        }
    };
}

// ── A fake Google token endpoint ────────────────────────────────────────────

struct FakeGoogle {
    token_url: String,
    /// The form fields of every request the endpoint has received.
    requests: Arc<Mutex<Vec<HashMap<String, String>>>>,
}

impl FakeGoogle {
    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    fn client(&self) -> OAuthClient {
        OAuthClient {
            client_id: "test-client-id".to_string(),
            client_secret: Some("test-client-secret".to_string()),
            auth_url: "https://accounts.example.test/authorize".to_string(),
            token_url: self.token_url.clone(),
            scope: "https://mail.google.com/".to_string(),
        }
    }
}

/// Serve `POST /token` on a loopback port. `respond` maps the decoded form
/// to `(HTTP status, JSON body)`.
async fn fake_token_endpoint<F>(respond: F) -> FakeGoogle
where
    F: Fn(&HashMap<String, String>) -> (u16, String) + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let token_url = format!("http://127.0.0.1:{}/token", listener.local_addr().unwrap().port());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let respond = Arc::new(respond);

    let log = requests.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else { return };
            let (log, respond) = (log.clone(), respond.clone());
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 2048];
                let (body_start, content_length) = loop {
                    let n = stream.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..pos]).to_ascii_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        break (pos + 4, len);
                    }
                };
                while buf.len() < body_start + content_length {
                    let n = stream.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let form: HashMap<String, String> =
                    url::form_urlencoded::parse(&buf[body_start..body_start + content_length]).into_owned().collect();
                log.lock().unwrap().push(form.clone());
                let (status, body) = respond(&form);
                let reply = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });

    FakeGoogle { token_url, requests }
}

fn token_json(access: &str, expires_in: u64, refresh: Option<&str>) -> (u16, String) {
    let refresh = refresh.map(|r| format!(",\"refresh_token\":\"{r}\"")).unwrap_or_default();
    (200, format!("{{\"access_token\":\"{access}\",\"expires_in\":{expires_in},\"token_type\":\"Bearer\"{refresh}}}"))
}

/// Play the browser: request `/?<query>` from the redirect listener and
/// return the whole HTTP reply.
async fn browser_visits(redirect_uri: &str, path_and_query: &str) -> String {
    let addr = url::Url::parse(redirect_uri).unwrap();
    let mut stream = TcpStream::connect((addr.host_str().unwrap(), addr.port().unwrap())).await.unwrap();
    stream
        .write_all(format!("GET {path_and_query} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut reply = String::new();
    timeout(RECV_TIMEOUT, stream.read_to_string(&mut reply)).await.unwrap().unwrap();
    reply
}

/// The query parameters of the consent URL `oauth::begin` produced.
fn consent_params(pending_url: &str) -> HashMap<String, String> {
    url::Url::parse(pending_url).unwrap().query_pairs().into_owned().collect()
}

// ── The browser flow ────────────────────────────────────────────────────────

#[tokio::test]
async fn sign_in_flow_trades_the_code_for_tokens_with_a_matching_pkce_verifier() {
    // The consent page's code_challenge, filled in once `begin` has made it,
    // for the token endpoint to check the verifier against -- the check a
    // real authorization server does, so the flow only succeeds if the two
    // halves of PKCE really line up.
    let challenge = Arc::new(Mutex::new(String::new()));
    let expected_challenge = challenge.clone();
    let google = fake_token_endpoint(move |form| {
        let verifier_hash = URL_SAFE_NO_PAD.encode(Sha256::digest(form.get("code_verifier").map(String::as_str).unwrap_or("").as_bytes()));
        let ok = form.get("grant_type").map(String::as_str) == Some("authorization_code")
            && form.get("code").map(String::as_str) == Some("the-code")
            && form.get("client_id").map(String::as_str) == Some("test-client-id")
            && form.get("client_secret").map(String::as_str) == Some("test-client-secret")
            && form.get("redirect_uri").is_some_and(|r| r.starts_with("http://127.0.0.1:"))
            && verifier_hash == *expected_challenge.lock().unwrap();
        if ok {
            token_json("at-1", 3600, Some("rt-1"))
        } else {
            (400, r#"{"error":"invalid_request","error_description":"PKCE or client mismatch"}"#.to_string())
        }
    })
    .await;
    let client = google.client();

    let pending = oauth::begin(&client, "alice@gmail.com").await.unwrap();
    let params = consent_params(&pending.url);
    *challenge.lock().unwrap() = params["code_challenge"].clone();
    let (redirect_uri, state) = (params["redirect_uri"].clone(), params["state"].clone());

    let finish_client = client.clone();
    let finish = tokio::spawn(async move { pending.finish(&finish_client).await });

    // Noise that must not end the sign-in: a favicon fetch, a redirect
    // carrying someone else's state.
    assert!(browser_visits(&redirect_uri, "/favicon.ico").await.starts_with("HTTP/1.1 404"));
    assert!(browser_visits(&redirect_uri, "/?code=the-code&state=not-the-state").await.starts_with("HTTP/1.1 400"));
    assert_eq!(google.request_count(), 0, "a forged redirect must not reach the token endpoint");

    let reply = browser_visits(&redirect_uri, &format!("/?code=the-code&state={state}")).await;
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");

    let grant = timeout(RECV_TIMEOUT, finish).await.unwrap().unwrap().expect("sign-in should succeed");
    assert_eq!(grant.access_token.expose_secret(), "at-1");
    assert_eq!(grant.refresh_token.unwrap().expose_secret(), "rt-1");
    assert_eq!(google.request_count(), 1);
}

#[tokio::test]
async fn declining_on_the_consent_page_fails_the_sign_in() {
    let google = fake_token_endpoint(|_| token_json("never", 3600, Some("never"))).await;
    let client = google.client();

    let pending = oauth::begin(&client, "").await.unwrap();
    let params = consent_params(&pending.url);
    assert!(!params.contains_key("login_hint"), "an empty hint should be left out");
    let (redirect_uri, state) = (params["redirect_uri"].clone(), params["state"].clone());

    let finish_client = client.clone();
    let finish = tokio::spawn(async move { pending.finish(&finish_client).await });
    browser_visits(&redirect_uri, &format!("/?error=access_denied&state={state}")).await;

    let err = timeout(RECV_TIMEOUT, finish).await.unwrap().unwrap().err().expect("should fail").to_string();
    assert!(err.contains("access_denied"), "{err}");
    assert_eq!(google.request_count(), 0);
}

// ── Access tokens over time ─────────────────────────────────────────────────

#[tokio::test]
async fn token_source_caches_a_valid_access_token_and_refreshes_an_expiring_one() {
    let issued = Arc::new(AtomicUsize::new(0));
    let counter = issued.clone();
    let google = fake_token_endpoint(move |_| {
        let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
        // Alternate long- and short-lived tokens: 3600s is cached, 30s is
        // already inside the expiry margin so it can never be reused.
        token_json(&format!("at-{n}"), if n == 1 { 3600 } else { 30 }, None)
    })
    .await;
    let source = TokenSource::from_refresh_token(google.client(), SecretString::from("rt-saved"));

    assert_eq!(source.access_token().await.unwrap().expose_secret(), "at-1");
    assert_eq!(source.access_token().await.unwrap().expose_secret(), "at-1");
    assert_eq!(google.request_count(), 1, "a valid token must be served from the cache");

    // Expire the cache by handing out the short-lived token next.
    let short = TokenSource::from_refresh_token(google.client(), SecretString::from("rt-saved"));
    assert_eq!(short.access_token().await.unwrap().expose_secret(), "at-2");
    assert_eq!(short.access_token().await.unwrap().expose_secret(), "at-3");
    assert_eq!(google.request_count(), 3, "a token about to expire must be refreshed");

    let refresh = &google.requests.lock().unwrap()[0];
    assert_eq!(refresh["grant_type"], "refresh_token");
    assert_eq!(refresh["refresh_token"], "rt-saved");
    assert_eq!(refresh["client_id"], "test-client-id");
    assert_eq!(refresh["client_secret"], "test-client-secret");
}

#[tokio::test]
async fn simultaneous_callers_share_one_refresh() {
    let google = fake_token_endpoint(|_| token_json("at-shared", 3600, None)).await;
    let source = TokenSource::from_refresh_token(google.client(), SecretString::from("rt"));

    let callers: Vec<_> = (0..6)
        .map(|_| {
            let source = source.clone();
            tokio::spawn(async move { source.access_token().await.unwrap().expose_secret().to_string() })
        })
        .collect();
    for caller in callers {
        assert_eq!(caller.await.unwrap(), "at-shared");
    }
    assert_eq!(google.request_count(), 1);
}

#[tokio::test]
async fn a_rotated_refresh_token_replaces_the_saved_one() {
    let google = fake_token_endpoint(|_| token_json("at", 30, Some("rt-rotated"))).await;
    let source = TokenSource::from_refresh_token(google.client(), SecretString::from("rt-old"));
    source.access_token().await.unwrap();
    assert_eq!(source.refresh_token().expose_secret(), "rt-rotated");
}

#[tokio::test]
async fn a_revoked_refresh_token_asks_the_user_to_sign_in_again() {
    let google = fake_token_endpoint(|_| {
        (400, r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#.to_string())
    })
    .await;
    let source = TokenSource::from_refresh_token(google.client(), SecretString::from("rt-dead"));
    let err = source.access_token().await.err().expect("should fail").to_string();
    assert!(err.contains("Sign in with Google again"), "{err}");
}

#[tokio::test]
async fn a_sign_in_without_a_refresh_token_is_rejected_up_front() {
    let google = fake_token_endpoint(|_| token_json("at", 3600, None)).await;
    let grant = TokenGrant {
        access_token: SecretString::from("at"),
        expires_at: Instant::now() + Duration::from_secs(3600),
        refresh_token: None,
    };
    let err = TokenSource::from_grant(google.client(), grant).err().expect("should fail").to_string();
    assert!(err.contains("refresh token"), "{err}");
}

// ── XOAUTH2 against the mock mail server ────────────────────────────────────

const GOOD_TOKEN: &str = "ya29.good-access-token";

/// A mock mail server that accepts `GOOD_TOKEN` for `TEST_USER`, and a fake
/// Google that hands out `handed_out` as the access token.
async fn oauth_setup(handed_out: &'static str) -> (mail_mock_server::RunningServer, FakeGoogle, Auth) {
    let store = mail_mock_server::new_store();
    {
        let mut guard = store.lock().unwrap();
        mail_mock_server::fixtures::seed(&mut guard, 0);
        guard.add_oauth_token(TEST_USER, GOOD_TOKEN);
    }
    let server = mail_mock_server::start(store).await.expect("start mock servers");
    let google = fake_token_endpoint(move |_| token_json(handed_out, 3600, None)).await;
    let auth = Auth::OAuth(TokenSource::from_refresh_token(google.client(), SecretString::from("rt-saved")));
    (server, google, auth)
}

async fn imap_connect(server: &mail_mock_server::RunningServer, auth: Auth) -> (mpsc::Sender<ImapCommand>, mpsc::Receiver<ImapEvent>, ImapEvent) {
    let (cmd_tx, cmd_rx) = mpsc::channel(32);
    let (evt_tx, mut evt_rx) = mpsc::channel(32);
    ImapActor::spawn(cmd_rx, evt_tx);
    cmd_tx
        .send(ImapCommand::Connect {
            host: "localhost".to_string(),
            port: server.imap_addr.port(),
            username: TEST_USER.to_string(),
            auth,
        })
        .await
        .unwrap();
    let first = timeout(RECV_TIMEOUT, evt_rx.recv()).await.expect("no reply from the actor").expect("actor gone");
    (cmd_tx, evt_rx, first)
}

#[tokio::test]
async fn imap_logs_in_with_xoauth2_and_a_refreshed_access_token() {
    skip_unless_ca_trusted!();
    let (server, google, auth) = oauth_setup(GOOD_TOKEN).await;

    let (cmd, mut evt, first) = imap_connect(&server, auth).await;
    assert!(matches!(first, ImapEvent::Connected), "expected Connected, got {first:?}");
    assert_eq!(google.request_count(), 1, "the access token should have come from a refresh");

    // The session is real, not just a greeting.
    cmd.send(ImapCommand::FetchMailboxes).await.unwrap();
    match timeout(RECV_TIMEOUT, evt.recv()).await.unwrap().unwrap() {
        ImapEvent::Mailboxes(mailboxes) => assert!(mailboxes.iter().any(|m| m.name == "INBOX")),
        other => panic!("expected Mailboxes, got {other:?}"),
    }
}

#[tokio::test]
async fn imap_reports_a_rejected_access_token_as_a_login_error() {
    skip_unless_ca_trusted!();
    let (server, _google, auth) = oauth_setup("ya29.some-other-token").await;
    let (_cmd, _evt, first) = imap_connect(&server, auth).await;
    match first {
        ImapEvent::Error(e) => assert!(!e.is_empty()),
        other => panic!("a rejected token must not connect, got {other:?}"),
    }
}

#[tokio::test]
async fn imap_surfaces_a_revoked_sign_in_instead_of_connecting() {
    skip_unless_ca_trusted!();
    let (server, _google, _) = oauth_setup(GOOD_TOKEN).await;
    let dead = fake_token_endpoint(|_| (400, r#"{"error":"invalid_grant"}"#.to_string())).await;
    let auth = Auth::OAuth(TokenSource::from_refresh_token(dead.client(), SecretString::from("rt-dead")));

    let (_cmd, _evt, first) = imap_connect(&server, auth).await;
    match first {
        ImapEvent::Error(e) => assert!(e.contains("Sign in with Google again"), "{e}"),
        other => panic!("expected Error, got {other:?}"),
    }
}

async fn smtp_send(server: &mail_mock_server::RunningServer, auth: Auth) -> SmtpEvent {
    let (cmd_tx, cmd_rx) = mpsc::channel(8);
    let (evt_tx, mut evt_rx) = mpsc::channel(8);
    SmtpActor::spawn(cmd_rx, evt_tx);
    let account = SmtpAccount {
        host: "127.0.0.1".to_string(),
        port: server.smtp_addr.port(),
        tls: esmail::config::TlsMode::None,
        username: TEST_USER.to_string(),
        auth,
        from_address: TEST_USER.to_string(),
    };
    let compose = ComposeState {
        to: TEST_USER.to_string(),
        subject: "Sent with XOAUTH2".to_string(),
        body: "No app password was involved.".to_string(),
        ..Default::default()
    };
    cmd_tx.send(SmtpCommand::Send { account, compose }).await.unwrap();
    timeout(RECV_TIMEOUT, evt_rx.recv()).await.expect("no reply from the SMTP actor").expect("actor gone")
}

#[tokio::test]
async fn smtp_sends_with_xoauth2_and_the_message_lands() {
    let (server, _google, auth) = oauth_setup(GOOD_TOKEN).await;
    match smtp_send(&server, auth).await {
        SmtpEvent::Sent { .. } => {}
        SmtpEvent::Error(e) => panic!("send failed: {e}"),
    }
    let store = server.store.lock().unwrap();
    let inbox = store.mailbox("INBOX").unwrap();
    assert!(
        inbox.messages.iter().any(|m| m.envelope.subject == "Sent with XOAUTH2"),
        "the sent message should have been delivered"
    );
}

#[tokio::test]
async fn smtp_rejects_a_bad_access_token() {
    let (server, _google, auth) = oauth_setup("ya29.some-other-token").await;
    match smtp_send(&server, auth).await {
        SmtpEvent::Error(_) => {}
        SmtpEvent::Sent { .. } => panic!("a rejected token must not be able to send"),
    }
}

#[tokio::test]
async fn idle_watch_authenticates_with_xoauth2_and_still_gets_pushes() {
    skip_unless_ca_trusted!();
    let (server, _google, auth) = oauth_setup(GOOD_TOKEN).await;

    let (wake_tx, mut wake_rx) = mpsc::channel(4);
    let _idle = idle_watch::spawn(
        "localhost".to_string(),
        server.imap_addr.port(),
        TEST_USER.to_string(),
        auth,
        "INBOX".to_string(),
        wake_tx,
    );

    // Same retry-until-idling approach as imap_smtp_integration.rs's IDLE test.
    let result = timeout(RECV_TIMEOUT, async {
        loop {
            server.store.lock().unwrap().deliver(
                "INBOX",
                b"From: bob@example.com\r\nTo: alice@example.com\r\nSubject: pushed\r\n\r\nhi\r\n".to_vec(),
            );
            if let Ok(Some(idle_watch::MailboxChanged)) = timeout(Duration::from_millis(500), wake_rx.recv()).await {
                return;
            }
        }
    })
    .await;
    assert!(result.is_ok(), "expected a push over an XOAUTH2-authenticated IDLE connection");
}

/// Google sign-in works for several accounts at once, next to a password
/// account: each `AccountSession` has its own `TokenSource` (own refresh
/// token, own access token), each server accepts only its own account's token,
/// and each account is notified about its own mail.
#[tokio::test]
async fn several_google_accounts_and_a_password_account_run_side_by_side() {
    use esmail::session::{AccountSession, Hooks, SessionParams};

    skip_unless_ca_trusted!();

    // Three servers, one account each; the Google ones only accept their own token.
    let mut servers = Vec::new();
    let mut auths = Vec::new();
    let mut fakes = Vec::new(); // keep the fake token endpoints alive
    for (user, token) in [("a@gmail.example", "token-a"), ("b@gmail.example", "token-b")] {
        let store = mail_mock_server::new_store();
        {
            let mut guard = store.lock().unwrap();
            guard.add_user(user, "unused");
            guard.add_oauth_token(user, token);
        }
        servers.push((user, mail_mock_server::start(store).await.expect("start mock servers")));
        let google = fake_token_endpoint(move |_| token_json(token, 3600, None)).await;
        auths.push(Auth::OAuth(TokenSource::from_refresh_token(
            google.client(),
            SecretString::from(format!("refresh-{token}")),
        )));
        fakes.push(google);
    }
    let store = mail_mock_server::new_store();
    store.lock().unwrap().add_user("c@example.com", "pw-c");
    servers.push(("c@example.com", mail_mock_server::start(store).await.expect("start mock servers")));
    auths.push(Auth::password("pw-c"));

    let labels = ["GoogleA", "GoogleB", "Plain"];
    let events: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let toasts: Arc<Mutex<Vec<String>>> = Arc::default();
    let (tx, mut rx) = mpsc::channel(64);
    let sink = events.clone();
    tokio::spawn(async move {
        while let Some((account, event)) = rx.recv().await {
            sink.lock().unwrap().push((account, format!("{event:?}")));
        }
    });
    let toast_sink = toasts.clone();
    let hooks = Hooks {
        notify: Arc::new(move |title, _| toast_sink.lock().unwrap().push(title.to_string())),
        repaint: Arc::new(|| {}),
    };

    let mut sessions = Vec::new();
    for (i, label) in labels.iter().enumerate() {
        let (user, server) = &servers[i];
        sessions.push(AccountSession::spawn(
            SessionParams {
                id: format!("id-{label}"),
                label: label.to_string(),
                host: "localhost".to_string(),
                port: server.imap_addr.port(),
                username: user.to_string(),
                auth: auths[i].clone(),
                watch_mailbox: "INBOX".to_string(),
            },
            tx.clone(),
            hooks.clone(),
        ));
    }

    let has = |account: &str, kind: &str| {
        events.lock().unwrap().iter().any(|(a, k)| a == account && k.starts_with(kind))
    };
    timeout(RECV_TIMEOUT, async {
        while !labels.iter().all(|l| has(&format!("id-{l}"), "Connected") && has(&format!("id-{l}"), "MailboxPolled")) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("all three accounts (two Google, one password) should connect and set a baseline");

    timeout(RECV_TIMEOUT, async {
        let mut n = 0;
        while !labels.iter().all(|l| toasts.lock().unwrap().iter().any(|t| t.starts_with(&format!("{l}:")))) {
            n += 1;
            for (_, server) in &servers {
                server.store.lock().unwrap().deliver(
                    "INBOX",
                    format!("From: bob@example.com\r\nTo: x\r\nSubject: hello {n}\r\n\r\nhi\r\n").into_bytes(),
                );
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .expect("every account, Google or not, should be notified of its own mail");

    // Each Google account exchanged its own refresh token, and only that one.
    for (fake, token) in fakes.iter().zip(["refresh-token-a", "refresh-token-b"]) {
        let requests = fake.requests.lock().unwrap();
        assert!(!requests.is_empty(), "the token endpoint for {token} was never used");
        assert!(
            requests.iter().all(|r| r.get("refresh_token").map(String::as_str) == Some(token)),
            "an account presented another account's refresh token"
        );
    }
    drop(sessions);
}
