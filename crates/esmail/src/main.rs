use esmail::{auth, compose, config, db, idle_watch, imap, notify, oauth, render, screenshot, search_query, secrets, smtp};
/// Tray icon + Windows toast notifications (B10). Windows-only: see
/// notify.rs's module doc for why the pure detection logic lives separately
/// and builds everywhere.
#[cfg(target_os = "windows")]
use esmail::tray;

use egui_litehtml_webview::{
    ImageRequest, InterceptOutcome, WebView, WebViewConfig, WebViewHandler, WebViewHost,
    WebViewSource,
};
use imap::{ImapActor, ImapCommand, ImapEvent, MailHeader};
use db::{DbActor, DbCommand, DbEvent};
use config::{AccountConfig, Config};
use search_query::ParsedQuery;
use secrecy::SecretString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Image-loading policy for the single [`WebView`] esmail reuses to show
/// every message body.
///
/// litehtml has no navigation concept at all (a message body view never
/// "navigates" anywhere) -- every link click unconditionally becomes a
/// [`egui_litehtml_webview::WebViewEvent::LinkClicked`], opened in the
/// system browser (see below), with no policy decision left to make.
///
/// Remote `http(s)` images are blocked unless `allow_remote` is set, which
/// the "Load remote images" button flips for the message currently showing.
/// This is the real blocking mechanism B5 calls for — markup alone can't
/// stop a network fetch, so `render.rs` leaves every remote URL in the
/// message's HTML exactly as it was, and this is what actually decides
/// whether the request happens at all. litehtml has no network stack of its
/// own, so when a remote image *is* allowed, this handler fetches it
/// itself with `ureq` and hands the bytes back via
/// `InterceptOutcome::Serve` — there is no "let the engine fetch it"
/// option to fall back on.
///
/// `intercept` runs on the webview's render thread, several calls at a time
/// (see [`WebViewHandler`]), so the UI thread flips `allow_remote` through a
/// shared `Arc` -- an atomic, so it never has to wait for a download in
/// flight -- and blocking on the network here is fine.
struct MessageViewHandler {
    allow_remote: AtomicBool,
    agent: ureq::Agent,
}

impl MessageViewHandler {
    /// Give up on a single image after this long, so one dead tracking-pixel
    /// host cannot hold up the message's remaining images indefinitely (ureq
    /// has no timeout at all unless asked).
    const IMAGE_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

    fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Self::IMAGE_FETCH_TIMEOUT))
            .build();
        Self {
            allow_remote: AtomicBool::new(false),
            agent: ureq::Agent::new_with_config(config),
        }
    }

    fn allow_remote(&self) -> bool {
        self.allow_remote.load(Ordering::Relaxed)
    }

    fn set_allow_remote(&self, allow: bool) {
        self.allow_remote.store(allow, Ordering::Relaxed);
    }
}

impl WebViewHandler for MessageViewHandler {
    fn intercept(&self, request: &ImageRequest) -> InterceptOutcome {
        if !self.allow_remote() {
            return InterceptOutcome::Block;
        }
        match self.agent.get(&request.url).call() {
            Ok(response) => match response.into_body().read_to_vec() {
                Ok(bytes) => InterceptOutcome::Serve(bytes),
                Err(e) => {
                    log::warn!("could not read remote image body {}: {e}", request.url);
                    InterceptOutcome::Block
                }
            },
            Err(e) => {
                log::warn!("could not fetch remote image {}: {e}", request.url);
                InterceptOutcome::Block
            }
        }
    }
}

/// A dismissable error notice (B9), replacing the old pattern of clobbering
/// `EsMailApp::status` with `format!("Error: {e}")`/`format!("DB Error: {e}")`
/// — which lost whatever the status string was showing before (e.g. "Page 3
/// of 9") the moment an unrelated background error arrived, and gave the
/// user no way to see more than the single most recent one. `status` itself
/// stays for transient, non-error progress text ("Connecting...", "Page 3 of
/// 9") — this only replaces the error half of that one field's job.
struct Banner {
    id: u64,
    message: String,
}

/// The outcome of a "Sign in with Google" browser round trip, sent from the
/// task that ran it (see `EsMailApp::begin_google_sign_in`) back to the UI.
/// Carries the account fields as they were when the button was clicked, so
/// editing the form while the browser is open can't change which account the
/// freshly authorized token gets attached to.
enum OAuthMessage {
    /// The system browser could not be launched; the sign-in is still
    /// waiting, so the user can open `url` themselves.
    BrowserUnavailable { url: String },
    Authorized { host: String, port: u16, username: String, auth: auth::Auth },
    Failed(String),
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
        ctx.request_repaint();
    }
    let grant = pending.finish(client).await?;
    oauth::TokenSource::from_grant(client.clone(), grant)
}

/// The Settings window's editable copy of the Google OAuth client, so typing
/// changes nothing until Save (Cancel just drops it).
struct SettingsForm {
    client_id: String,
    client_secret: String,
}

/// The host the "Sign in with Google" option is offered for.
const GMAIL_IMAP_HOST: &str = "imap.gmail.com";

struct EsMailApp {
    web_view: WebView,
    /// Creates views; see `egui_litehtml_webview::WebViewHost`'s own doc for
    /// why this is little more than a texture-id counter now. Kept as a
    /// field (rather than a local dropped right after `new_view`) because a
    /// second view (a compose preview, say) would be created from this same
    /// host, so its lifetime should match the app's, not just the
    /// constructor's.
    /// Unread today since nothing currently creates a second view --
    /// allowed explicitly rather than silently dropping the field.
    #[allow(dead_code)]
    web_view_host: WebViewHost,
    /// Bound to `web_view` at construction. Toggled per-message by the "Load
    /// remote images" button; reset whenever a new message is opened to
    /// blocked, or to allowed if its sender is on
    /// `Config::image_trusted_senders`. See [`MessageViewHandler`].
    message_view_handler: Arc<MessageViewHandler>,
    screenshotter: screenshot::Screenshotter,
    /// Show only the webview, with no IMAP account. See ESMAIL_PREVIEW.
    preview: bool,
    imap_tx: mpsc::Sender<ImapCommand>,
    imap_rx: mpsc::Receiver<ImapEvent>,
    db_tx: mpsc::Sender<DbCommand>,
    db_rx: mpsc::Receiver<DbEvent>,
    smtp_tx: mpsc::Sender<smtp::SmtpCommand>,
    smtp_rx: mpsc::Receiver<smtp::SmtpEvent>,
    /// Sender half of the channel `spawn_new_mail_watch`'s task reads
    /// [`idle_watch::MailboxChanged`] pushes from. Kept on `EsMailApp` so the
    /// "Connect" button can hand a fresh clone to `idle_watch::spawn` once it
    /// knows the account's host/username/password — `idle_watch` itself has
    /// no way to learn those except from the same login form `ImapCommand::
    /// Connect` already reads them from.
    idle_wake_tx: mpsc::Sender<idle_watch::MailboxChanged>,
    /// The running `idle_watch` task, if any. Each Connect replaces it: the
    /// old one is aborted (which drops its IDLE connection) before a new one
    /// starts with the new credentials. Both halves matter -- without the
    /// abort every retry would leave one more IDLE connection running
    /// forever, and without the replacement a retry after a typo'd password,
    /// or "Sign in again" after a revoked Google token, would leave the watch
    /// retrying the old credentials for the rest of the session.
    idle_watch_task: Option<tokio::task::JoinHandle<()>>,

    /// The login form's "Sign in with Google" choice: OAuth2 through the
    /// browser instead of a password. Only offered for Gmail.
    use_oauth: bool,
    /// What the current (or most recent) connection authenticates with.
    /// SMTP sends reuse it, so an OAuth account's sends share the IMAP
    /// connection's cached access token instead of each refreshing their own.
    current_auth: Option<auth::Auth>,
    oauth_tx: mpsc::Sender<OAuthMessage>,
    oauth_rx: mpsc::Receiver<OAuthMessage>,
    /// The running browser round trip, if any. Aborting it closes its local
    /// redirect listener, which is how "Cancel" works.
    oauth_task: Option<tokio::task::JoinHandle<()>>,
    /// The open Settings window's form, if it is open.
    settings: Option<SettingsForm>,

    /// Saved accounts (host/port/username; no passwords — those are in the OS
    /// keyring, see `secrets`). Persisted to `config.toml`.
    config: Config,

    // UI state
    host: String,
    port: String,
    username: String,
    password: String,
    /// SMTP host for the login form, prefilled from
    /// [`config::derive_smtp_host`]'s guess but editable — see B7 in
    /// PLAN.md. TLS mode is fixed to `Ssl`/465 for now; `StartTls`/`None`
    /// have no UI toggle yet, only the `AccountConfig` fields to hold them.
    smtp_host: String,
    smtp_port: String,
    status: String,
    is_connected: bool,

    /// First-run wizard (B9): an email address typed on the login screen, to
    /// look up in `config::provider_for_email` and autofill the host/port
    /// fields from — see `apply_provider_wizard`. Not itself persisted; it
    /// only ever feeds the other fields, which are.
    wizard_email: String,

    /// Active error banners (B9), newest last. See [`Banner`].
    banners: Vec<Banner>,
    /// Monotonic source for `Banner::id`, so a dismiss click can target the
    /// exact banner clicked even if another one is added/removed the same
    /// frame — mirrors `next_req_id`'s reasoning.
    next_banner_id: u64,

    /// Current theme preference (B9), mirrored from `config.theme` and kept
    /// in sync with it on every toggle. Applied to the `egui::Context` once
    /// at startup and again whenever the toggle button changes it.
    theme: config::ThemeMode,

    /// The window's last-known outer rect, refreshed every frame from
    /// `egui::ViewportInfo::outer_rect` (B9's window-geometry persistence).
    /// `None` until the platform has reported one at least once (e.g. not
    /// available on Wayland/Android — see that field's own doc in egui).
    window_geometry: Option<config::WindowGeometry>,
    /// Set once geometry has been written to `config.toml` for the close
    /// currently in progress, so the write happens exactly once rather than
    /// once per frame between the close request and the process actually
    /// exiting.
    geometry_saved_on_close: bool,

    /// The mailbox tree (B8), flattened for the left panel's list UI -- see
    /// `imap::flatten_tree`'s doc for why a flat, owned `Vec` rather than a
    /// real recursive tree widget.
    mailbox_rows: Vec<imap::MailboxRow>,
    /// `STATUS (UNSEEN)` per mailbox (B8), refreshed whenever `Mailboxes`
    /// arrives and after a flag/move changes what's unread. A mailbox
    /// missing from this map (rather than present with `0`) means its count
    /// hasn't been fetched yet, not that it's read.
    unread_counts: std::collections::HashMap<String, u32>,
    selected_mailbox: String,

    headers: Vec<MailHeader>,
    selected_uid: Option<u32>,
    /// Multi-select (B8): every UID selected via shift/ctrl-click, in
    /// addition to `selected_uid` (the one whose body is actually shown --
    /// always the most recently *plain*-clicked message, or the sole member
    /// of a multi-selection made by ctrl/shift-clicking from scratch).
    /// Bulk actions (archive/delete/mark read or unread) act on this set
    /// when it's non-empty, falling back to `selected_uid` alone otherwise.
    selected_uids: std::collections::BTreeSet<u32>,
    /// Anchor for shift-click range selection: the last *plain* (no
    /// modifier) click, or the single UID a ctrl-click started a fresh
    /// selection from.
    select_anchor: Option<u32>,
    /// Set when a message is opened, cleared once its `\Seen` flag has been
    /// sent (or the user navigates away first) -- B8's "mark as read with a
    /// delay" so briefly passing over a message in the list doesn't mark it
    /// read. Checked once per frame in `ui()`.
    pending_mark_seen: Option<(u32, std::time::Instant)>,
    /// The search box `TextEdit`'s widget id, captured where it's drawn so
    /// Ctrl+F (B8) can `request_focus` it from the keyboard-shortcut check
    /// below, which runs outside that closure (and so has no access to a
    /// freshly-computed id of its own -- egui ids depend on the enclosing
    /// panel, not just the widget's own salt).
    search_box_id: Option<egui::Id>,
    current_page: u32,
    total_pages: u32,
    /// Attachments for the currently-open message (B6), if fetched directly
    /// from IMAP. Cleared whenever a different message is opened. A message
    /// opened from a cached search result never populates this — the cache
    /// only stores rendered HTML, not the raw bytes attachments come from;
    /// see PLAN.md §B6.
    current_attachments: Vec<render::Attachment>,
    /// Lowercased address of the open message's sender (`None` when nothing
    /// is open or the header has no address). What the remote-images bar
    /// offers to trust, and what `open_message` looked up in
    /// `Config::image_trusted_senders` when it opened the message.
    current_sender: Option<String>,
    /// The currently-open message's rendered HTML, kept only so
    /// Reply/Reply All/Forward (B7) can quote it — see `compose.rs`. Empty
    /// when no message is loaded.
    current_message_html: String,

    /// The compose window's state, when one is open — `None` means it's
    /// closed. See `compose.rs`.
    compose: Option<compose::ComposeState>,
    compose_status: String,

    /// Monotonic source for `ImapCommand::FetchHeaders`/`FetchBody` request
    /// ids. Only the reply matching `current_headers_req`/`current_body_req`
    /// is applied; an older one arriving late (e.g. a slow page-2 fetch
    /// answered after the user already moved to page 3) is dropped instead of
    /// clobbering newer state.
    next_req_id: u64,
    current_headers_req: u64,
    current_body_req: u64,

    // Search and Progress
    search_query: String,
    search_results: Option<Vec<MailHeader>>,
    download_progress: Option<(u32, u32)>,

    /// The tray icon (B10), or `None` if either it couldn't be created (see
    /// `tray::TrayState::new`'s doc) or this is a preview/screenshot run,
    /// where a tray icon would be unwanted background noise for what's
    /// meant to be a one-shot, no-account render. Window-close falls back to
    /// exiting normally whenever this is `None`, rather than hiding a window
    /// with no way to bring it back.
    #[cfg(target_os = "windows")]
    tray: Option<tray::TrayState>,
    /// Set by the tray's "Quit" action; the next close-request is then
    /// allowed to actually close the app instead of being redirected to
    /// "hide to tray". See `EsMailApp::logic`.
    #[cfg(target_os = "windows")]
    exit_requested: bool,
}

impl EsMailApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        init_logging();
        
        let (imap_cmd_tx, imap_cmd_rx) = mpsc::channel(32);
        let (imap_evt_tx, imap_evt_rx) = mpsc::channel(32);
        
        let (db_cmd_tx, db_cmd_rx) = mpsc::channel(32);
        let (db_evt_tx, db_evt_rx) = mpsc::channel(32);

        let egui_ctx = cc.egui_ctx.clone();
        
        // Wrap IMAP events: forward every event to the UI channel (bumping a
        // repaint), same as before B10 existed. `spawn_new_mail_watch` also
        // watches this same stream for the new-mail signal (B10) and turns
        // it into a background poll timer + a toast -- as a plain tokio
        // task, not anything hung off `EsMailApp::ui`/`logic`, it keeps
        // running (and can keep showing toasts) for as long as the process
        // is alive, independent of whether the main window is visible. See
        // its doc comment and tray.rs for how the window survives being
        // "closed".
        let (tx, rx) = mpsc::channel(32);
        // `idle_watch::spawn` (created once the "Connect" button knows the
        // account's credentials — see its call site) sends here whenever its
        // dedicated IDLE connection sees the server push something, so
        // `spawn_new_mail_watch` can poll immediately instead of waiting for
        // its own timer. A small buffer is enough: this only ever carries a
        // "go check" signal, never data, and a missed send just means the
        // next poll-timer tick catches it instead.
        let (idle_wake_tx, idle_wake_rx) = mpsc::channel(4);
        spawn_new_mail_watch(rx, imap_evt_tx, imap_cmd_tx.clone(), egui_ctx.clone(), idle_wake_rx);
        ImapActor::spawn(imap_cmd_rx, tx);

        // Wrap DB events
        let (tx_db, mut rx_db) = mpsc::channel(32);
        let ctx_clone_db = egui_ctx.clone();
        tokio::spawn(async move {
            while let Some(evt) = rx_db.recv().await {
                let _ = db_evt_tx.send(evt).await;
                ctx_clone_db.request_repaint();
            }
        });
        DbActor::spawn(db_cmd_rx, tx_db);

        let (smtp_cmd_tx, smtp_cmd_rx) = mpsc::channel(8);
        let (smtp_evt_tx, smtp_evt_rx) = mpsc::channel(8);
        let (tx_smtp, mut rx_smtp) = mpsc::channel(8);
        let ctx_clone_smtp = egui_ctx.clone();
        tokio::spawn(async move {
            while let Some(evt) = rx_smtp.recv().await {
                let _ = smtp_evt_tx.send(evt).await;
                ctx_clone_smtp.request_repaint();
            }
        });
        smtp::SmtpActor::spawn(smtp_cmd_rx, tx_smtp);

        // Preview mode: render one page full-window with no IMAP account, so the
        // webview itself can be exercised and screenshotted. ESMAIL_PREVIEW is
        // either a path to an HTML file, a URL, or "demo" for a built-in page.
        let preview = std::env::var("ESMAIL_PREVIEW").ok();
        let source = match preview.as_deref() {
            None => WebViewSource::Html(
                "<h1>Welcome to esMail</h1><p>Connect to your IMAP account to start reading.</p>"
                    .to_string(),
            ),
            Some("demo") => WebViewSource::Html(preview_demo_html()),
            // litehtml has no network layer of its own (see
            // egui-litehtml-webview's module doc) -- a URL preview target
            // fetches synchronously with ureq and hands the result in as
            // plain HTML, rather than a `WebViewSource::Url` variant that no
            // longer exists.
            Some(target) if target.starts_with("http") => match ureq::get(target).call() {
                Ok(response) => match response.into_body().read_to_string() {
                    Ok(html) => WebViewSource::Html(html),
                    Err(e) => WebViewSource::Html(format!("<h1>could not read body of {target}</h1><p>{e}</p>")),
                },
                Err(e) => WebViewSource::Html(format!("<h1>could not fetch {target}</h1><p>{e}</p>")),
            },
            // An exported message (see the "Export..." button): render it
            // through the same pipeline a live message goes through, so a
            // saved real-world email can be reproduced without an account.
            Some(path) if path.to_ascii_lowercase().ends_with(".eml") => match std::fs::read(path) {
                Ok(raw) => WebViewSource::Html(render::render_message(&raw)),
                Err(e) => WebViewSource::Html(format!("<h1>could not read {path}</h1><p>{e}</p>")),
            },
            Some(path) => match std::fs::read_to_string(path) {
                Ok(html) => WebViewSource::Html(html),
                Err(e) => WebViewSource::Html(format!("<h1>could not read {path}</h1><p>{e}</p>")),
            },
        };
        
        let mut config = Config::load();
        if config.migrate_legacy() {
            if let Err(e) = config.save() {
                log::warn!("could not persist migrated config: {e}");
            }
        }

        // Apply the saved theme (B9) once, up front, rather than defaulting
        // to egui's own built-in dark theme for one frame first -- avoids a
        // visible flash on launch for a user who picked Light.
        egui_ctx.set_theme(match config.theme {
            config::ThemeMode::Dark => egui::ThemePreference::Dark,
            config::ThemeMode::Light => egui::ThemePreference::Light,
            config::ThemeMode::System => egui::ThemePreference::System,
        });

        // Prefill the login form from the first saved account, if any; its
        // password (if the OS keyring has one) comes along too, so a
        // returning user does not have to retype it.
        let (host_str, port_str, username_str, password_str, smtp_host_str, smtp_port_str) =
            match config.accounts.first() {
                Some(account) => {
                    let password = secrets::get_password(&account.id, "imap")
                        .map(|s| secrecy::ExposeSecret::expose_secret(&s).to_string())
                        .unwrap_or_default();
                    (
                        account.imap_host.clone(),
                        account.imap_port.to_string(),
                        account.username.clone(),
                        password,
                        account.smtp_host.clone(),
                        account.smtp_port.to_string(),
                    )
                }
                None => (
                    "imap.gmail.com".to_string(),
                    "993".to_string(),
                    String::new(),
                    String::new(),
                    "smtp.gmail.com".to_string(),
                    "465".to_string(),
                ),
            };
        let initial_status = "Ready".to_string();

        // The form starts on "Sign in with Google" for a returning OAuth
        // user, and for a brand-new one whenever a Google client is
        // configured (the empty form defaults to Gmail's host).
        let use_oauth = match config.accounts.first() {
            Some(account) => account.auth == config::AuthKind::GoogleOAuth,
            None => oauth::google_client(config.google_oauth.as_ref()).is_some(),
        };
        let (oauth_tx, oauth_rx) = mpsc::channel(4);

        // One host per window; a second view (a compose preview, say) would
        // come from this same host. The webview needs nothing from `cc` (no
        // window handle, no GL context -- see egui-litehtml-webview's
        // `WebViewHost` doc), so this is infallible.
        let web_view_host = WebViewHost::new();
        let message_view_handler = Arc::new(MessageViewHandler::new());
        let web_view = web_view_host.new_view(
            &cc.egui_ctx,
            WebViewConfig::new(source).with_handler(message_view_handler.clone()),
        );

        // Skipped in preview/screenshot mode: HANDOFF.md's automated
        // screenshot verification runs a one-shot, no-account render and
        // exits on its own -- a tray icon there would be unwanted
        // background noise (and a needless dependency on the tray shell
        // being available) for a run nothing ever clicks on.
        #[cfg(target_os = "windows")]
        let tray = if preview.is_none() {
            match tray::TrayState::new() {
                Ok(t) => Some(t),
                Err(e) => {
                    log::warn!(
                        "could not create the system tray icon; closing the window will exit \
                         esMail normally instead of minimizing it: {e}"
                    );
                    None
                }
            }
        } else {
            None
        };

        let initial_theme = config.theme;

        Self {
            web_view_host,
            web_view,
            message_view_handler,
            screenshotter: screenshot::Screenshotter::from_env(),
            preview: preview.is_some(),
            imap_tx: imap_cmd_tx,
            imap_rx: imap_evt_rx,
            db_tx: db_cmd_tx,
            db_rx: db_evt_rx,
            smtp_tx: smtp_cmd_tx,
            smtp_rx: smtp_evt_rx,
            idle_wake_tx,
            idle_watch_task: None,
            use_oauth,
            current_auth: None,
            oauth_tx,
            oauth_rx,
            oauth_task: None,
            settings: None,
            config,
            host: host_str,
            port: port_str,
            username: username_str,
            password: password_str,
            smtp_host: smtp_host_str,
            smtp_port: smtp_port_str,
            status: initial_status,
            is_connected: false,
            wizard_email: String::new(),
            banners: Vec::new(),
            next_banner_id: 0,
            theme: initial_theme,
            window_geometry: None,
            geometry_saved_on_close: false,
            mailbox_rows: Vec::new(),
            unread_counts: std::collections::HashMap::new(),
            selected_mailbox: "INBOX".to_string(),
            headers: Vec::new(),
            selected_uid: None,
            selected_uids: std::collections::BTreeSet::new(),
            select_anchor: None,
            pending_mark_seen: None,
            search_box_id: None,
            current_page: 1,
            total_pages: 1,
            current_attachments: Vec::new(),
            current_sender: None,
            current_message_html: String::new(),
            compose: None,
            compose_status: String::new(),
            next_req_id: 0,
            current_headers_req: 0,
            current_body_req: 0,
            search_query: String::new(),
            search_results: None,
            download_progress: None,
            #[cfg(target_os = "windows")]
            tray,
            #[cfg(target_os = "windows")]
            exit_requested: false,
        }
    }

    fn handle_imap_events(&mut self) {
        while let Ok(evt) = self.imap_rx.try_recv() {
            match evt {
                ImapEvent::Connected => {
                    self.status = "Connected!".to_string();
                    self.is_connected = true;
                    self.persist_current_account();
                    let _ = self.imap_tx.try_send(ImapCommand::FetchMailboxes);
                    self.fetch_headers(self.selected_mailbox.clone(), 1);
                }
                ImapEvent::Disconnected => {
                    self.status = "Connection lost, reconnecting...".to_string();
                }
                ImapEvent::Error(e) => {
                    self.push_banner(format!("IMAP error: {e}"));
                }
                ImapEvent::Mailboxes(mbs) => {
                    // B8: render as a tree (name split on the server's
                    // delimiter, special-use folders first) instead of a
                    // flat alphabetical list.
                    let names: Vec<String> = mbs.iter().map(|m| m.name.clone()).collect();
                    self.mailbox_rows = imap::flatten_tree(&imap::mailbox_tree(&mbs));
                    let _ = self.imap_tx.try_send(ImapCommand::FetchUnreadCounts { mailboxes: names });
                }
                ImapEvent::Headers { mailbox, headers, page, total_pages, req_id, mailbox_state } => {
                    // Only the most recently issued FetchHeaders' reply is
                    // applied; an older one arriving late (e.g. the mailbox
                    // was changed again before it came back) is dropped.
                    if req_id == self.current_headers_req && mailbox == self.selected_mailbox {
                        self.headers = headers;
                        self.current_page = page;
                        self.total_pages = total_pages;
                        self.status = format!("Page {} of {}", page, total_pages);
                    }
                    // Rides along on every header fetch regardless of
                    // req_id/mailbox staleness — db.rs's cache bookkeeping for
                    // `mailbox` should stay current even if this particular
                    // reply is no longer the one the UI is showing.
                    let _ = self.db_tx.try_send(DbCommand::ReportMailboxState {
                        account_id: self.account_id(),
                        mailbox,
                        uid_validity: mailbox_state.uid_validity,
                        uid_next: mailbox_state.uid_next,
                    });
                }
                ImapEvent::Body { uid, html, attachments, req_id } => {
                    // Matching `req_id`+`uid` is sufficient on its own now
                    // that `open_message` bumps `current_body_req` on every
                    // open (including a cache-served search result) -- a
                    // stale live reply from before a search-result open can
                    // no longer slip through just because `uid` happens to
                    // coincide, since its `req_id` is guaranteed stale too.
                    // (This used to also require `self.search_results.is_none()`,
                    // which incidentally also blocked the *legitimate* case
                    // fixed here: a live fallback fetch issued while the
                    // search results list is still showing, from a DB cache
                    // miss -- see the `DbEvent::MailFetchFailed` arm below.)
                    if req_id == self.current_body_req && self.selected_uid == Some(uid) {
                        self.current_message_html = html.clone();
                        self.web_view.load(WebViewSource::Html(html));
                        self.current_attachments = attachments;
                    }
                }
                ImapEvent::BodyFailed { uid, req_id, error } => {
                    // Without this, a failed body fetch (dropped connection,
                    // exhausted reconnect retries, message no longer on the
                    // server) left `open_message`'s "Loading message..."
                    // placeholder on screen forever: the generic `Error`
                    // variant this used to arrive as carries no uid/req_id,
                    // so nothing could tell it apart from an unrelated error
                    // and resolve the pending fetch. This is the actual fix
                    // for the "stuck on Loading message..." bug (issue #13).
                    if req_id == self.current_body_req && self.selected_uid == Some(uid) {
                        let msg = format!("<i>Could not load message: {}</i>", ammonia::clean_text(&error));
                        self.current_message_html = msg.clone();
                        self.web_view.load(WebViewSource::Html(msg));
                    }
                    self.push_banner(format!("Could not load message: {error}"));
                }
                ImapEvent::Exported { path } => {
                    self.status = format!("Exported message to {}", path.display());
                }
                ImapEvent::ExportFailed { error } => {
                    self.push_banner(format!("Could not export message: {error}"));
                }
                ImapEvent::DownloadProgress { current, total } => {
                    self.download_progress = Some((current, total));
                    if current == total {
                        self.download_progress = None;
                        self.status = "Download complete".to_string();
                    }
                }
                ImapEvent::MailData { mailbox, header, body } => {
                    let _ = self.db_tx.try_send(DbCommand::IndexMail {
                        account_id: self.account_id(),
                        mailbox,
                        header,
                        body,
                    });
                }
                ImapEvent::MailboxPolled { .. } | ImapEvent::NewHeaders { .. } => {
                    // B10's new-mail signal: already consumed by
                    // `spawn_new_mail_watch` before this event reached the
                    // UI channel at all (it decides whether to poll again /
                    // fetch new envelopes / show a toast). Nothing left here
                    // for the UI to do with either variant.
                }
                ImapEvent::Appended { mailbox } => {
                    // B7: confirmation that the just-sent message was saved
                    // to `mailbox` (see the `Append` sent from
                    // `handle_smtp_events`'s `Sent` arm). Nothing for the UI
                    // to update -- the compose window and "Message sent"
                    // status already reflect the send itself, which
                    // succeeded independently of this.
                    log::debug!("appended sent message to {mailbox}");
                }
                ImapEvent::AppendFailed { mailbox, error } => {
                    // Deliberately not `self.status` -- see the variant's
                    // doc in imap.rs: the send itself already succeeded and
                    // is already reflected there, and this is a background,
                    // best-effort step the user never explicitly asked to
                    // watch. A banner (B9), unlike the old single `status`
                    // string this replaces, can say so *alongside* "Message
                    // sent" instead of only being able to overwrite it --
                    // which is exactly the problem that made this event a
                    // log-only affair up to now.
                    log::warn!("could not save sent message to {mailbox}: {error}");
                    self.push_banner(format!("Sent, but could not save a copy to {mailbox}: {error}"));
                }
                ImapEvent::HeadersFrom { mailbox, headers } => {
                    // B3: reply to the `FetchHeadersFrom` sent in
                    // `handle_db_events`'s `SyncPlan::FetchFrom`/`Resync`
                    // arm -- index into the cache now that these envelopes
                    // are in hand. See `ImapCommand::FetchHeadersFrom`'s doc
                    // for why this is a separate event from `NewHeaders`
                    // rather than reusing it.
                    let _ = self.db_tx.try_send(DbCommand::IndexHeaders {
                        account_id: self.account_id(),
                        mailbox,
                        headers,
                    });
                }
                ImapEvent::PollFailed(e) => {
                    // Deliberately not `self.status` -- see the variant's
                    // doc in imap.rs: a background poll failing every 60s
                    // shouldn't overwrite whatever the user is looking at.
                    log::warn!("background new-mail poll failed: {e}");
                }
                ImapEvent::FlagsUpdated { mailbox, uid, flags, req_id: _ } => {
                    // B8: reflect the server-confirmed flags back into the
                    // visible header list, any active search-results list,
                    // and the local cache. Only touches self.headers/
                    // self.unread_counts/self.search_results when `mailbox`
                    // matches what's actually on screen -- both lists only
                    // ever hold messages from `self.selected_mailbox`
                    // (search is itself scoped to it, see the `Search`
                    // send-site below), so an event for a different mailbox
                    // finding a same-numbered UID in either list would
                    // otherwise patch the wrong message's row and skew the
                    // unread count for a mailbox that wasn't actually
                    // touched. The DB write below is unaffected by this
                    // guard: it's already keyed by `mailbox`, so it's
                    // correct regardless of what's currently displayed.
                    if mailbox == self.selected_mailbox {
                        let was_seen = self.headers.iter().find(|h| h.uid == uid).map(|h| h.is_seen());
                        if let Some(header) = self.headers.iter_mut().find(|h| h.uid == uid) {
                            header.flags = flags.clone();
                        }
                        if let Some(results) = self.search_results.as_mut() {
                            if let Some(header) = results.iter_mut().find(|h| h.uid == uid) {
                                header.flags = flags.clone();
                            }
                        }
                        if let (Some(was_seen), Some(count)) = (was_seen, self.unread_counts.get_mut(&mailbox)) {
                            let now_seen = flags.iter().any(|f| f.eq_ignore_ascii_case(imap::FLAG_SEEN));
                            if was_seen && !now_seen {
                                *count += 1;
                            } else if !was_seen && now_seen {
                                *count = count.saturating_sub(1);
                            }
                        }
                    }
                    let _ = self.db_tx.try_send(DbCommand::UpdateFlags {
                        account_id: self.account_id(),
                        mailbox,
                        uid,
                        flags,
                    });
                }
                ImapEvent::FlagsUpdateFailed { mailbox: _, uid, error, req_id: _ } => {
                    self.push_banner(format!("Could not update flags on message {uid}: {error}"));
                }
                ImapEvent::Moved { mailbox, uid, dest, req_id: _ } => {
                    // B8: delete-to-Trash/archive succeeded -- drop the
                    // message from the visible list, any active
                    // search-results list, the cache, and any selection it
                    // was part of. See FlagsUpdated above for why the
                    // header/search-results/unread-count mutations are
                    // guarded on `mailbox == self.selected_mailbox`.
                    let was_unread = mailbox == self.selected_mailbox
                        && self.headers.iter().find(|h| h.uid == uid).map(|h| !h.is_seen()).unwrap_or(false);
                    if mailbox == self.selected_mailbox {
                        self.headers.retain(|h| h.uid != uid);
                        if let Some(results) = self.search_results.as_mut() {
                            results.retain(|h| h.uid != uid);
                        }
                    }
                    self.selected_uids.remove(&uid);
                    if self.selected_uid == Some(uid) {
                        self.selected_uid = None;
                        self.web_view.load(WebViewSource::Html("<i>Message moved.</i>".to_string()));
                    }
                    if was_unread {
                        if let Some(count) = self.unread_counts.get_mut(&mailbox) {
                            *count = count.saturating_sub(1);
                        }
                        // The message just landed in `dest` unread -- bump
                        // its count too if we're already tracking it (it
                        // may not be yet if FetchUnreadCounts hasn't
                        // completed), so the sidebar doesn't read "no new
                        // mail in Archive/Trash" for a message that just
                        // arrived there.
                        if let Some(count) = self.unread_counts.get_mut(&dest) {
                            *count += 1;
                        }
                    }
                    self.status = format!("Moved to {dest}");
                    let _ = self.db_tx.try_send(DbCommand::RemoveMessage {
                        account_id: self.account_id(),
                        mailbox,
                        uid,
                    });
                }
                ImapEvent::MoveFailed { mailbox: _, uid, error, req_id: _ } => {
                    self.push_banner(format!("Could not move message {uid}: {error}"));
                }
                ImapEvent::UnreadCounts(counts) => {
                    self.unread_counts = counts;
                }
            }
        }
    }

    fn handle_db_events(&mut self) {
        while let Ok(evt) = self.db_rx.try_recv() {
            match evt {
                DbEvent::SearchResult { headers } => {
                    self.search_results = Some(headers);
                }
                DbEvent::MailFetched { header, body } => {
                    if self.selected_uid == Some(header.uid) {
                        self.current_message_html = body.clone();
                        self.web_view.load(WebViewSource::Html(body));
                    }
                }
                DbEvent::MailFetchFailed { uid, error } => {
                    // Most commonly a cache miss: `open_message`'s `is_search`
                    // branch reads the body from the local cache, but
                    // `db.rs`'s `MAX_CACHED_BODIES` LRU cap means a search
                    // result's body can have been evicted since it was
                    // indexed -- routine on a mailbox with more messages
                    // than the cap (e.g. this was reproduced with a 2585-
                    // message Gmail account against a 2000-body cap). This
                    // used to be indistinguishable from any other DB error
                    // (`DbEvent::Error`, just a banner, nothing else), so
                    // "Loading message..." never got resolved either way --
                    // this is what root-caused issue #13. Falling back to a
                    // real `FetchBody` here both fixes that and actually
                    // loads the message rather than just reporting failure.
                    if self.selected_uid == Some(uid) {
                        log::debug!("cached body for uid {uid} unavailable ({error}); falling back to a live fetch");
                        self.fetch_body(self.selected_mailbox.clone(), uid);
                    }
                }
                DbEvent::SyncPlan { account_id, mailbox, plan } => {
                    // B3: turn a `FetchFrom`/`Resync` decision into an
                    // actual incremental fetch, so the cache accumulates
                    // message metadata for this mailbox over time instead of
                    // only ever being populated by `BulkDownload`. A
                    // `Resync` already wiped the cache's rows for this
                    // mailbox in `db.rs::report_mailbox_state` by the time
                    // this event arrives -- fetching from UID 1 repopulates
                    // it under the server's new UIDVALIDITY.
                    //
                    // `account_id` isn't used to route this -- there is
                    // exactly one account connected at a time today (see
                    // `EsMailApp::account_id`'s own doc), so it's implicitly
                    // always "the" account; kept on the event for when that
                    // stops being true.
                    let _ = account_id;
                    match plan {
                        db::SyncPlan::UpToDate => {}
                        db::SyncPlan::FetchFrom { first_new_uid } => {
                            let _ = self.imap_tx.try_send(ImapCommand::FetchHeadersFrom { mailbox, first_uid: first_new_uid });
                        }
                        db::SyncPlan::Resync => {
                            let _ = self.imap_tx.try_send(ImapCommand::FetchHeadersFrom { mailbox, first_uid: 1 });
                        }
                    }
                }
                DbEvent::Error(e) => {
                    self.push_banner(format!("Database error: {e}"));
                }
            }
        }
    }

    fn handle_smtp_events(&mut self) {
        while let Ok(evt) = self.smtp_rx.try_recv() {
            match evt {
                smtp::SmtpEvent::Sent { raw } => {
                    // The compose window closes on success; a failure (the
                    // Error arm below) leaves it open with the typed text
                    // intact instead, so nothing is lost -- see smtp.rs's
                    // module docs on why that's a deliberately smaller
                    // promise than a real retry queue.
                    self.compose = None;
                    self.compose_status.clear();
                    self.status = "Message sent".to_string();
                    // B7: save a copy to Sent, the way every other mail
                    // client does (SMTP itself doesn't). Best-effort -- a
                    // failure here only logs (via the generic
                    // ImapEvent::Error path), it doesn't reopen the compose
                    // window or otherwise imply the send itself failed,
                    // since it didn't.
                    let mailbox = self.special_use_mailbox(imap::SpecialUse::Sent, SENT_MAILBOX);
                    let _ = self.imap_tx.try_send(ImapCommand::Append { mailbox, raw });
                }
                smtp::SmtpEvent::Error(e) => {
                    self.compose_status = format!("Send failed: {e}");
                }
            }
        }
    }

    /// Add a new error banner (B9). Callers pass a complete, already-worded
    /// message; this just assigns it an id and appends it — dismissal is a
    /// separate click handled in `ui()`, since building the message needs
    /// `&mut self` at the call site but the dismiss button needs it while
    /// iterating `self.banners`, and those can't overlap in one place.
    fn push_banner(&mut self, message: String) {
        self.next_banner_id += 1;
        self.banners.push(Banner { id: self.next_banner_id, message });
    }

    /// Apply `theme` to both `self.config` (so it's saved) and the live
    /// `egui::Context` (so the toggle takes effect immediately, not just on
    /// the next launch).
    fn apply_theme(&mut self, ctx: &egui::Context, theme: config::ThemeMode) {
        self.theme = theme;
        self.config.theme = theme;
        ctx.set_theme(match theme {
            config::ThemeMode::Dark => egui::ThemePreference::Dark,
            config::ThemeMode::Light => egui::ThemePreference::Light,
            config::ThemeMode::System => egui::ThemePreference::System,
        });
        if let Err(e) = self.config.save() {
            log::warn!("could not persist theme preference: {e}");
        }
    }

    /// Persist `self.config` after a small preference change (image trust,
    /// folded folders), logging rather than surfacing a failure: losing the
    /// preference on the next launch is not worth a banner.
    fn save_config(&self, what: &str) {
        if let Err(e) = self.config.save() {
            log::warn!("could not persist {what}: {e}");
        }
    }

    /// Add (`trusted`) or remove `address` on the always-load-images list
    /// and save it if that changed anything.
    fn set_image_sender_trusted(&mut self, address: &str, trusted: bool) {
        if self.config.set_image_trusted(address, trusted) {
            self.save_config("remote-image sender list");
        }
    }

    /// First-run wizard (B9): if `self.wizard_email` names a domain
    /// `config::provider_for_email` recognizes, and the host fields still
    /// look untouched (empty, or still holding the generic `imap.gmail.com`/
    /// `993` placeholder `EsMailApp::new` seeds a brand-new login form
    /// with), fill in the guessed host/port/SMTP settings and the username.
    /// Never overwrites a host the user has actually typed or edited —
    /// there's no "are you sure" here, so silently clobbering a manual entry
    /// on every keystroke in the email field would be worse than not
    /// guessing at all.
    fn apply_provider_wizard(&mut self) {
        let Some(settings) = config::provider_for_email(&self.wizard_email) else {
            return;
        };
        let untouched = matches!(self.host.as_str(), "" | "imap.gmail.com");
        if !untouched {
            return;
        }
        // Runs on every keystroke in the email field while the host still
        // looks untouched, so the Sign in with Google choice may only be
        // *defaulted* here, never re-imposed: a user who unticked it must
        // not see it ticked again by their next keystroke. The untouched
        // default host is already Gmail, whose default `new()` set.
        if settings.imap_host != GMAIL_IMAP_HOST {
            self.use_oauth = false;
        } else if self.host.is_empty() {
            // Gmail can skip the app password when a Google client is set up.
            self.use_oauth = oauth::google_client(self.config.google_oauth.as_ref()).is_some();
        }
        self.host = settings.imap_host.to_string();
        self.port = settings.imap_port.to_string();
        self.smtp_host = settings.smtp_host.to_string();
        self.smtp_port = settings.smtp_port.to_string();
        if self.username.is_empty() {
            self.username = self.wizard_email.clone();
        }
    }

    /// Persist the window's last-tracked outer rect (B9) into `config.toml`,
    /// if the platform ever reported one (see `window_geometry`'s doc).
    /// Called once when a real close is going through — see `ui()`.
    fn save_window_geometry(&mut self) {
        if let Some(geometry) = self.window_geometry {
            self.config.window = Some(geometry);
            if let Err(e) = self.config.save() {
                log::warn!("could not persist window geometry: {e}");
            }
        }
    }

    /// Identifies the connected account to `db.rs`, in the same
    /// `username@host` shape [`AccountConfig::new`] uses for its `id` — so
    /// the cache keys line up with the saved-accounts list even though this
    /// is derived from the live login form rather than looked up from
    /// `self.config`.
    fn account_id(&self) -> String {
        // Trimmed, like the values a connection is made with, so a stray
        // space in the form can't make the keyring/config key differ from
        // the account that actually signed in.
        format!("{}@{}", self.username.trim(), self.host.trim())
    }

    /// A fresh request id for `FetchHeaders`/`FetchBody`, mechanically
    /// distinct from the last one handed out.
    fn next_req_id(&mut self) -> u64 {
        self.next_req_id += 1;
        self.next_req_id
    }

    /// Send `FetchHeaders`, recording its request id as the only one whose
    /// reply `handle_imap_events` will still accept.
    fn fetch_headers(&mut self, mailbox: String, page: u32) {
        let req_id = self.next_req_id();
        self.current_headers_req = req_id;
        let _ = self.imap_tx.try_send(ImapCommand::FetchHeaders { mailbox, page, req_id });
    }

    /// Send `FetchBody`, recording its request id the same way `fetch_headers` does.
    fn fetch_body(&mut self, mailbox: String, uid: u32) {
        let req_id = self.next_req_id();
        self.current_body_req = req_id;
        let _ = self.imap_tx.try_send(ImapCommand::FetchBody { mailbox, uid, req_id });
    }

    /// Save the open message's raw RFC822 source as an `.eml` file, chosen
    /// through a native save dialog. The fetch and the write happen on the
    /// IMAP body worker (`ImapCommand::ExportMessage`), not here. Does
    /// nothing if no message is open or the dialog is cancelled.
    fn export_selected_message(&mut self, header: &MailHeader) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Email message", &["eml"])
            .set_file_name(export_file_name(&header.subject, header.uid))
            .save_file()
        else {
            return;
        };
        self.status = format!("Exporting message to {}...", path.display());
        let _ = self.imap_tx.try_send(ImapCommand::ExportMessage {
            mailbox: self.selected_mailbox.clone(),
            uid: header.uid,
            path,
        });
    }

    /// Open a message the way a click on it (or `j`/`k` + `Enter`, see the
    /// keyboard-shortcut handling in `ui()`) does: select it, blank the
    /// viewer while it loads, and either `FetchBody` (a live message) or
    /// `DbCommand::FetchMail` (a cached search result) it -- then schedule
    /// B8's mark-as-read delay.
    fn open_message(&mut self, uid: u32, is_search: bool) {
        self.selected_uid = Some(uid);
        // A new message defaults to blocked remote content, same as any
        // other mail client; "Load remote images" opts back in per view, and
        // "Always load from ..." opts a sender in for good. The sender comes
        // from the header list because the body has not arrived yet, and the
        // handler is set before it does, so a trusted sender's first frame
        // already has its images.
        self.current_sender = self
            .search_results
            .as_ref()
            .unwrap_or(&self.headers)
            .iter()
            .find(|h| h.uid == uid)
            .and_then(MailHeader::sender_address);
        let trusted = self.current_sender.as_deref().is_some_and(|a| self.config.is_image_trusted(a));
        self.message_view_handler.set_allow_remote(trusted);
        self.current_attachments.clear();
        self.web_view.load(WebViewSource::Html("<i>Loading message...</i>".to_string()));
        if is_search {
            // Bump (invalidate) `current_body_req` even though this request
            // itself goes out over `db_tx`, not `imap_tx` -- otherwise a
            // still-in-flight live `FetchBody` from *before* this open would
            // keep matching `current_body_req` (unchanged) and, now that its
            // `uid` happens to equal this one too, could get applied here
            // once the guard below stopped gating on `search_results` (see
            // the `ImapEvent::Body` arm's doc). A cache miss below still
            // falls back to a real live fetch through `fetch_body`, which
            // hands out its own fresh id and legitimately updates this.
            self.current_body_req = self.next_req_id();
            let _ = self.db_tx.try_send(DbCommand::FetchMail {
                account_id: self.account_id(),
                mailbox: self.selected_mailbox.clone(),
                uid,
            });
        } else {
            self.fetch_body(self.selected_mailbox.clone(), uid);
        }
        // B8: don't mark \Seen immediately -- only after the message has
        // stayed open for MARK_SEEN_DELAY, so quickly arrowing past a
        // message in the list doesn't mark it read. `ui()` checks this once
        // per frame and fires the actual StoreFlags when it elapses.
        if !self.headers.iter().any(|h| h.uid == uid && h.is_seen()) {
            self.pending_mark_seen = Some((uid, std::time::Instant::now()));
        } else {
            self.pending_mark_seen = None;
        }
    }

    /// The UIDs a bulk action (B8's Mark read/unread, Star/Unstar, Archive,
    /// Delete) applies to: the multi-selection if non-empty, else the
    /// single open message, else nothing.
    fn action_targets(&self) -> Vec<u32> {
        if !self.selected_uids.is_empty() {
            self.selected_uids.iter().copied().collect()
        } else {
            self.selected_uid.into_iter().collect()
        }
    }

    /// Send `StoreFlags` for every target in [`Self::action_targets`].
    fn store_flags_on_selection(&mut self, add: Vec<String>, remove: Vec<String>) {
        let mailbox = self.selected_mailbox.clone();
        for uid in self.action_targets() {
            let req_id = self.next_req_id();
            let _ = self.imap_tx.try_send(ImapCommand::StoreFlags {
                mailbox: mailbox.clone(),
                uid,
                add: add.clone(),
                remove: remove.clone(),
                req_id,
            });
        }
    }

    /// A target UID's current `\Flagged` state, checked against whichever
    /// list is actually on screen for it (`search_results` when a search is
    /// active, else `headers`) -- used by `toggle_star_on_selection` so each
    /// message's own state decides its own direction.
    fn is_flagged_uid(&self, uid: u32) -> bool {
        self.search_results
            .as_ref()
            .unwrap_or(&self.headers)
            .iter()
            .any(|h| h.uid == uid && h.is_flagged())
    }

    /// Toggle `\Flagged` on every target in [`Self::action_targets`],
    /// per-message rather than applying one shared direction to the whole
    /// selection: a message that's already starred gets unstarred and one
    /// that isn't gets starred, independently of what its neighbors in the
    /// selection are doing. A single shared direction (star everything /
    /// unstar everything, decided from just one message's state) would
    /// silently flip messages the user never intended to touch whenever a
    /// multi-selection has mixed flag states.
    fn toggle_star_on_selection(&mut self) {
        let mailbox = self.selected_mailbox.clone();
        for uid in self.action_targets() {
            let req_id = self.next_req_id();
            let (add, remove) = if self.is_flagged_uid(uid) {
                (vec![], vec![imap::FLAG_FLAGGED.to_string()])
            } else {
                (vec![imap::FLAG_FLAGGED.to_string()], vec![])
            };
            let _ = self.imap_tx.try_send(ImapCommand::StoreFlags {
                mailbox: mailbox.clone(),
                uid,
                add,
                remove,
                req_id,
            });
        }
    }

    /// Send `MoveMessage` (Archive/Delete-to-Trash) for every target in
    /// [`Self::action_targets`].
    fn move_selection(&mut self, dest: &str) {
        let mailbox = self.selected_mailbox.clone();
        for uid in self.action_targets() {
            let req_id = self.next_req_id();
            let _ = self.imap_tx.try_send(ImapCommand::MoveMessage {
                mailbox: mailbox.clone(),
                uid,
                dest: dest.to_string(),
                req_id,
            });
        }
    }

    /// The real mailbox for a special-use role (B8's `imap::SpecialUse`
    /// discovery -- real `LIST` attributes with a name-based fallback,
    /// already used to sort/label the mailbox tree), falling back to
    /// `default` when no mailbox in the account currently classifies as
    /// that role -- e.g. before `Mailboxes` has arrived at all, or a server
    /// that advertises no special-use attributes and has no
    /// conventionally-named folder either. This is what closes the gap
    /// `SENT_MAILBOX`/`TRASH_MAILBOX`/`ARCHIVE_MAILBOX`'s own doc comments
    /// named: those hardcoded names are now only the fallback, not the only
    /// option, so an account whose folders aren't literally named "Sent"/
    /// "Trash"/"Archive" (Gmail's `[Gmail]/Sent Mail`, say) gets its real
    /// folder instead of a spurious new top-level mailbox.
    fn special_use_mailbox(&self, want: imap::SpecialUse, default: &str) -> String {
        find_special_use_mailbox(&self.mailbox_rows, want, default)
    }

    /// Archive every target in [`Self::action_targets`] to the account's
    /// real Archive mailbox (special-use-discovered, falling back to
    /// `ARCHIVE_MAILBOX`).
    fn archive_selection(&mut self) {
        let dest = self.special_use_mailbox(imap::SpecialUse::Archive, ARCHIVE_MAILBOX);
        self.move_selection(&dest);
    }

    /// Delete (move to Trash) every target in [`Self::action_targets`], to
    /// the account's real Trash mailbox (special-use-discovered, falling
    /// back to `TRASH_MAILBOX`).
    fn delete_selection(&mut self) {
        let dest = self.special_use_mailbox(imap::SpecialUse::Trash, TRASH_MAILBOX);
        self.move_selection(&dest);
    }

    /// Fill the login form from a saved account and pull its password back
    /// out of the OS keyring, if there is one.
    fn select_account(&mut self, account: &AccountConfig) {
        self.host = account.imap_host.clone();
        self.port = account.imap_port.to_string();
        self.username = account.username.clone();
        self.smtp_host = account.smtp_host.clone();
        self.smtp_port = account.smtp_port.to_string();
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
    fn oauth_active(&self) -> bool {
        self.use_oauth && self.host.trim() == GMAIL_IMAP_HOST
    }

    /// Hand `auth` to the IMAP actor for `host`/`port`/`username`, and start
    /// the IDLE watch alongside it. The one place a connection begins, for a
    /// typed password and for an OAuth token alike.
    fn start_connection(&mut self, host: String, port: u16, username: String, auth: auth::Auth) {
        self.status = "Connecting...".to_string();
        self.current_auth = Some(auth.clone());
        let _ = self.imap_tx.try_send(ImapCommand::Connect {
            host: host.clone(),
            port,
            username: username.clone(),
            auth: auth.clone(),
        });
        // A separate, dedicated IDLE connection (see idle_watch's module doc
        // for why it can't share ImapActor's session) so new-mail detection
        // is push-based instead of relying only on spawn_new_mail_watch's
        // poll timer. Started alongside the normal connect rather than only
        // after `ImapEvent::Connected` arrives: it does its own independent
        // login/reconnect and simply has nothing to push until it succeeds,
        // so there is no ordering requirement between the two. Replaces any
        // earlier watch -- see `idle_watch_task`'s doc.
        if let Some(previous) = self.idle_watch_task.take() {
            previous.abort();
        }
        self.idle_watch_task = Some(idle_watch::spawn(
            host,
            port,
            username,
            auth,
            NEW_MAIL_POLL_MAILBOX.to_string(),
            self.idle_wake_tx.clone(),
        ));
    }

    /// The Connect button. With a password that is just `start_connection`.
    /// With Google sign-in it reuses the refresh token saved from an earlier
    /// approval, and only sends the user to the browser when there is none.
    fn connect_clicked(&mut self, ctx: &egui::Context) {
        let port: u16 = self.port.parse().unwrap_or(993);
        // Trimmed: a trailing space would otherwise go out as part of the
        // XOAUTH2 user and be rejected, though the same account signed in
        // fine (`begin_google_sign_in` trims) the first time.
        let host = self.host.trim().to_string();
        let username = self.username.trim().to_string();
        if !self.oauth_active() {
            let auth = auth::Auth::password(self.password.clone());
            self.start_connection(host, port, username, auth);
            return;
        }
        let Some(client) = self.google_client_or_explain() else { return };
        match secrets::get_password(&self.account_id(), "oauth") {
            Some(refresh_token) => {
                let source = oauth::TokenSource::from_refresh_token(client, refresh_token);
                self.start_connection(host, port, username, auth::Auth::OAuth(source));
            }
            None => self.begin_google_sign_in(ctx),
        }
    }

    /// The configured Google OAuth client, or (with a banner saying how to
    /// configure one) `None`. Google issues tokens only to registered
    /// applications, so unlike a password this cannot work out of the box --
    /// see `oauth`'s module doc.
    fn google_client_or_explain(&mut self) -> Option<oauth::OAuthClient> {
        let client = oauth::google_client(self.config.google_oauth.as_ref());
        if client.is_none() {
            self.push_banner(
                "Google sign-in needs an OAuth client id: enter it under Settings (top right), or \
                 set ESMAIL_GOOGLE_CLIENT_ID and ESMAIL_GOOGLE_CLIENT_SECRET. See the esmail README."
                    .to_string(),
            );
        }
        client
    }

    /// Open the system browser on Google's consent page and wait, in a
    /// background task, for the redirect back; the result arrives as an
    /// [`OAuthMessage`]. Replaces any sign-in already in progress.
    fn begin_google_sign_in(&mut self, ctx: &egui::Context) {
        if self.username.trim().is_empty() {
            self.push_banner("Enter your Gmail address in the Username field first.".to_string());
            return;
        }
        let Some(client) = self.google_client_or_explain() else { return };
        if let Some(previous) = self.oauth_task.take() {
            previous.abort();
        }

        let host = self.host.trim().to_string();
        let port: u16 = self.port.parse().unwrap_or(993);
        let username = self.username.trim().to_string();
        let tx = self.oauth_tx.clone();
        let ctx = ctx.clone();
        self.status = "Waiting for Google sign-in in your browser...".to_string();
        self.oauth_task = Some(tokio::spawn(async move {
            let message = match run_google_sign_in(&client, &username, &tx, &ctx).await {
                Ok(source) => OAuthMessage::Authorized { host, port, username, auth: auth::Auth::OAuth(source) },
                Err(e) => OAuthMessage::Failed(format!("{e:#}")),
            };
            let _ = tx.send(message).await;
            ctx.request_repaint();
        }));
    }

    fn handle_oauth_events(&mut self) {
        while let Ok(message) = self.oauth_rx.try_recv() {
            match message {
                OAuthMessage::BrowserUnavailable { url } => {
                    self.push_banner(format!("Could not open your browser. Open this address to sign in: {url}"));
                }
                OAuthMessage::Authorized { host, port, username, auth } => {
                    self.oauth_task = None;
                    // The form was editable while the browser was open. The
                    // token belongs to the account the sign-in was started
                    // for, and `persist_current_account`/`smtp_account` read
                    // the form, so put those values back rather than let the
                    // token be saved under whatever is there now.
                    self.host = host.clone();
                    self.port = port.to_string();
                    self.username = username.clone();
                    self.start_connection(host, port, username, auth);
                }
                OAuthMessage::Failed(error) => {
                    self.oauth_task = None;
                    self.status = "Ready".to_string();
                    self.push_banner(format!("Google sign-in failed: {error}"));
                }
            }
        }
    }

    /// Persist the account currently in the login form: upsert it into
    /// `config.toml` and its password into the OS keyring (under both
    /// `"imap"` and `"smtp"` — B7 sends with the same credentials, since
    /// `AccountConfig::username` is documented as used for both). Called
    /// once a connection actually succeeds, not on every keystroke or click.
    fn persist_current_account(&mut self) {
        let username = self.username.trim().to_string();
        let mut account = AccountConfig::new(
            username.clone(),
            self.host.trim().to_string(),
            self.port.parse().unwrap_or(993),
            username,
        );
        // AccountConfig::new only guesses smtp_host/smtp_port; the login
        // form's fields (pre-filled from that guess, but editable) win.
        if !self.smtp_host.is_empty() {
            account.smtp_host = self.smtp_host.clone();
        }
        if let Ok(port) = self.smtp_port.parse() {
            account.smtp_port = port;
        }
        // What the connection that just succeeded actually used decides what
        // is saved -- not the form, which may have been touched since.
        match &self.current_auth {
            // No password anywhere: the refresh token is the credential, and
            // IMAP and SMTP both derive their access tokens from it.
            Some(auth::Auth::OAuth(source)) => {
                account.auth = config::AuthKind::GoogleOAuth;
                if let Err(e) = secrets::set_password(&account.id, "oauth", &source.refresh_token()) {
                    log::warn!("could not save the Google sign-in to the OS keyring: {e}");
                }
                // An account that used to sign in with a password must not
                // leave that password behind, unused, in the keyring.
                secrets::delete_password(&account.id, "imap");
                secrets::delete_password(&account.id, "smtp");
            }
            _ => {
                // ...and the reverse: a live refresh token for an account
                // that now uses a password.
                secrets::delete_password(&account.id, "oauth");
                let password = SecretString::from(self.password.clone());
                if let Err(e) = secrets::set_password(&account.id, "imap", &password) {
                    log::warn!("could not save IMAP password to the OS keyring: {e}");
                }
                if let Err(e) = secrets::set_password(&account.id, "smtp", &password) {
                    log::warn!("could not save SMTP password to the OS keyring: {e}");
                }
            }
        }
        self.config.upsert_account(account);
        if let Err(e) = self.config.save() {
            log::warn!("could not persist account config: {e}");
        }
    }

    /// Build the SMTP account `smtp.rs` needs to send, from the current
    /// login form and saved keyring password. `None` if there's no SMTP
    /// password saved yet — e.g. the very first connection, before
    /// [`EsMailApp::persist_current_account`] has ever run for this account.
    fn smtp_account(&self) -> Option<smtp::SmtpAccount> {
        let account_id = self.account_id();
        let auth = match &self.current_auth {
            // The very same token source the IMAP connection uses.
            Some(auth) if auth.is_oauth() => auth.clone(),
            _ => auth::Auth::Password(secrets::get_password(&account_id, "smtp")?),
        };
        Some(smtp::SmtpAccount {
            host: self.smtp_host.clone(),
            port: self.smtp_port.parse().unwrap_or(465),
            tls: config::TlsMode::Ssl,
            username: self.username.trim().to_string(),
            auth,
            from_address: self.username.trim().to_string(),
        })
    }

    /// Draws the compose window when `self.compose` is `Some`, and handles
    /// its Send/Attach/Discard buttons. A separate top-level `egui::Window`
    /// rather than part of the main layout — B7 says "compose window", and
    /// this can stay open (or get discarded) independent of what the user
    /// does with the message list behind it.
    /// B8's mark-as-read delay: fires the actual `StoreFlags` once
    /// `MARK_SEEN_DELAY` has elapsed since `open_message` scheduled it,
    /// provided the same message is still the one open (otherwise the timer
    /// is simply dropped -- the message the user moved on to gets its own
    /// timer from its own `open_message` call). Checked once per frame.
    fn handle_mark_seen_delay(&mut self) {
        let Some((uid, at)) = self.pending_mark_seen else { return };
        if self.selected_uid != Some(uid) {
            self.pending_mark_seen = None;
            return;
        }
        if at.elapsed() < MARK_SEEN_DELAY {
            return;
        }
        self.pending_mark_seen = None;
        let mailbox = self.selected_mailbox.clone();
        let req_id = self.next_req_id();
        let _ = self.imap_tx.try_send(ImapCommand::StoreFlags {
            mailbox,
            uid,
            add: vec![imap::FLAG_SEEN.to_string()],
            remove: vec![],
            req_id,
        });
    }

    /// B8's keyboard shortcuts: `j`/`k` (next/previous message, and open
    /// it), `Enter` (re-open the current selection -- a harmless no-op
    /// today since `j`/`k` already open as they move, kept for the shortcut
    /// list's own sake and so a future "highlight without opening" cursor
    /// has something to bind to), `r` (Reply), `a` (Archive), `f`
    /// (star/unstar), `Del`/`Backspace` (delete to Trash), `Ctrl+F` (focus
    /// search), `Ctrl+N` (compose). Disabled while the compose window is
    /// open (its own text fields need every keystroke) or the search box
    /// has focus (so typing "j"/"f"/etc. into a search query doesn't also
    /// fire a shortcut).
    fn handle_keyboard_shortcuts(&mut self, ui: &mut egui::Ui) {
        if self.compose.is_some() {
            return;
        }
        let search_focused = self
            .search_box_id
            .is_some_and(|id| ui.memory(|m| m.has_focus(id)));
        if search_focused {
            return;
        }

        let (ctrl_f, ctrl_n, next, prev, enter, reply, archive, star, delete) = ui.input(|i| {
            let ctrl = i.modifiers.ctrl || i.modifiers.command;
            (
                ctrl && i.key_pressed(egui::Key::F),
                ctrl && i.key_pressed(egui::Key::N),
                !ctrl && i.key_pressed(egui::Key::J),
                !ctrl && i.key_pressed(egui::Key::K),
                !ctrl && i.key_pressed(egui::Key::Enter),
                !ctrl && i.key_pressed(egui::Key::R),
                !ctrl && i.key_pressed(egui::Key::A),
                !ctrl && i.key_pressed(egui::Key::F),
                !ctrl && (i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace)),
            )
        });

        if ctrl_f {
            if let Some(id) = self.search_box_id {
                ui.memory_mut(|m| m.request_focus(id));
            }
        }
        if ctrl_n {
            self.compose = Some(compose::ComposeState::default());
            self.compose_status.clear();
        }
        if next || prev {
            let is_search = self.search_results.is_some();
            let list = self.search_results.as_ref().unwrap_or(&self.headers);
            if !list.is_empty() {
                let idx = self.selected_uid.and_then(|uid| list.iter().position(|h| h.uid == uid));
                let new_idx = match idx {
                    Some(i) if next => (i + 1).min(list.len() - 1),
                    Some(i) => i.saturating_sub(1), // prev
                    None => 0,
                };
                let uid = list[new_idx].uid;
                self.selected_uids.clear();
                self.select_anchor = Some(uid);
                self.open_message(uid, is_search);
            }
        }
        if enter {
            if let Some(uid) = self.selected_uid {
                let is_search = self.search_results.is_some();
                self.open_message(uid, is_search);
            }
        }
        if reply {
            if let Some(uid) = self.selected_uid {
                if let Some(header) = self.headers.iter().find(|h| h.uid == uid).cloned() {
                    self.compose = Some(compose::ComposeState::reply(&header, &self.current_message_html));
                    self.compose_status.clear();
                }
            }
        }
        if archive {
            self.archive_selection();
        }
        if delete {
            self.delete_selection();
        }
        if star {
            self.toggle_star_on_selection();
        }
    }

    /// Open the Settings window, filled from what is saved.
    fn open_settings(&mut self) {
        let saved = self.config.google_oauth.as_ref();
        self.settings = Some(SettingsForm {
            client_id: saved.map(|c| c.client_id.clone()).unwrap_or_default(),
            client_secret: saved.and_then(|c| c.client_secret.clone()).unwrap_or_default(),
        });
    }

    /// Draws the Settings window when `self.settings` is `Some`: the Google
    /// OAuth client id and secret that "Sign in with Google" needs (see
    /// `oauth`'s module doc for why esmail can't supply its own).
    fn show_settings_window(&mut self, ctx: &egui::Context) {
        // Only while the window is open: this reads environment variables,
        // which is not something to do on every frame of a closed window.
        if self.settings.is_none() {
            return;
        }
        // Read before the form is borrowed, so the window can say when an
        // environment variable is overriding what is typed here.
        let active_source = oauth::google_client_with_source(self.config.google_oauth.as_ref()).map(|(_, source)| source);
        let Some(form) = &mut self.settings else {
            return;
        };

        let mut open = true;
        let mut save_clicked = false;
        let mut cancel_clicked = false;
        egui::Window::new("Settings")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.heading("Sign in with Google");
                ui.label(
                    "Lets Gmail accounts sign in through the browser instead of an app password. \
                     Create a \"Desktop app\" OAuth client in Google Cloud Console and enter its \
                     credentials here (see the esmail README).",
                );
                ui.add_space(6.0);
                egui::Grid::new("google_oauth_settings").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                    ui.label("Client ID");
                    ui.add(egui::TextEdit::singleline(&mut form.client_id).desired_width(340.0));
                    ui.end_row();
                    ui.label("Client secret");
                    ui.add(egui::TextEdit::singleline(&mut form.client_secret).password(true).desired_width(340.0));
                    ui.end_row();
                });
                ui.add_space(4.0);
                match active_source {
                    Some(oauth::ClientSource::Environment) => {
                        ui.colored_label(
                            egui::Color32::from_rgb(200, 140, 30),
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
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        save_clicked = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel_clicked = true;
                    }
                });
            });

        if save_clicked {
            if let Some(form) = self.settings.take() {
                self.apply_settings(form);
            }
        } else if cancel_clicked || !open {
            self.settings = None;
        }
    }

    /// Persist the Settings form into `config.toml`.
    fn apply_settings(&mut self, form: SettingsForm) {
        let client_id = form.client_id.trim().to_string();
        let client_secret = form.client_secret.trim().to_string();
        self.config.google_oauth = (!client_id.is_empty()).then(|| config::OAuthClientConfig {
            client_id,
            client_secret: (!client_secret.is_empty()).then_some(client_secret),
        });
        match self.config.save() {
            Ok(()) => self.status = "Settings saved".to_string(),
            Err(e) => self.push_banner(format!("Could not save settings: {e}")),
        }
        // A first-time user who has just configured a client and is looking
        // at the Gmail defaults most likely wants Sign in with Google.
        if self.config.accounts.is_empty()
            && self.host.trim() == GMAIL_IMAP_HOST
            && oauth::google_client(self.config.google_oauth.as_ref()).is_some()
        {
            self.use_oauth = true;
        }
    }

    fn show_compose_window(&mut self, ctx: &egui::Context) {
        let Some(compose) = &mut self.compose else {
            return;
        };

        let mut open = true;
        let mut send_clicked = false;
        let mut discard_clicked = false;
        egui::Window::new("Compose")
            .open(&mut open)
            .default_size([480.0, 420.0])
            .show(ctx, |ui| {
                egui::Grid::new("compose_grid").num_columns(2).show(ui, |ui| {
                    ui.label("To:");
                    ui.add(egui::TextEdit::singleline(&mut compose.to).desired_width(f32::INFINITY));
                    ui.end_row();

                    ui.label("Cc:");
                    ui.add(egui::TextEdit::singleline(&mut compose.cc).desired_width(f32::INFINITY));
                    ui.end_row();

                    ui.label("Bcc:");
                    ui.add(egui::TextEdit::singleline(&mut compose.bcc).desired_width(f32::INFINITY));
                    ui.end_row();

                    ui.label("Subject:");
                    ui.add(egui::TextEdit::singleline(&mut compose.subject).desired_width(f32::INFINITY));
                    ui.end_row();
                });

                ui.separator();

                if !compose.attachments.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        for (filename, data) in &compose.attachments {
                            ui.label(format!("{filename} ({})", format_size(data.len())));
                        }
                    });
                }
                if ui.button("Attach file…").clicked() {
                    if let Some(path) = rfd::FileDialog::new().pick_file() {
                        match std::fs::read(&path) {
                            Ok(data) => {
                                let filename = path
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| "attachment".to_string());
                                compose.attachments.push((filename, data));
                            }
                            Err(e) => {
                                self.compose_status = format!("Could not read {}: {e}", path.display());
                            }
                        }
                    }
                }

                ui.add_sized(
                    ui.available_size() - egui::vec2(0.0, 60.0),
                    egui::TextEdit::multiline(&mut compose.body),
                );

                ui.horizontal(|ui| {
                    if ui.button("Send").clicked() {
                        send_clicked = true;
                    }
                    if ui.button("Discard").clicked() {
                        discard_clicked = true;
                    }
                    if !self.compose_status.is_empty() {
                        ui.label(egui::RichText::new(&self.compose_status).color(egui::Color32::RED));
                    }
                });
            });

        if send_clicked {
            match self.smtp_account() {
                Some(account) => {
                    let compose = self.compose.clone().expect("just matched Some above");
                    let _ = self.smtp_tx.try_send(smtp::SmtpCommand::Send { account, compose });
                    self.compose_status = "Sending…".to_string();
                }
                None => {
                    self.compose_status =
                        "No SMTP password on file yet — connect once via IMAP first.".to_string();
                }
            }
        }
        if discard_clicked || !open {
            self.compose = None;
            self.compose_status.clear();
        }
    }
}

/// Windows only (B10): tray icon polling + minimize-to-tray. Kept in its own
/// `impl` block, called only from `EsMailApp::logic`, so the cfg-gating
/// needed to keep this out of non-Windows builds stays contained to one
/// place instead of scattered through the main `ui()`/`impl EsMailApp` code.
#[cfg(target_os = "windows")]
impl EsMailApp {
    fn handle_tray(&mut self, ctx: &egui::Context) {
        let Some(tray) = &self.tray else { return };

        for action in tray.poll_actions() {
            match action {
                tray::TrayAction::Show => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                tray::TrayAction::Quit => {
                    self.exit_requested = true;
                    // Hidden windows don't organically generate another
                    // close-request -- nothing is clicking their (invisible)
                    // close button -- so ask for one explicitly. The check
                    // below sees `exit_requested` and lets it through rather
                    // than redirecting it to "hide to tray" again.
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }

        // The redirect: a first close-request (the user clicked the window's
        // own close button) is canceled and turned into "hide instead",
        // *unless* it was `self.exit_requested` that triggered this request
        // (tray Quit), in which case letting it proceed is the point.
        if ctx.input(|i| i.viewport().close_requested()) && !self.exit_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }

        // `logic()` (unlike `ui()`) keeps running while the window is
        // hidden, but only when something requests a repaint -- nothing
        // does that for us just because a tray click landed in `tray-icon`'s
        // own event channel, so ask again here to keep polling it promptly.
        ctx.request_repaint_after(std::time::Duration::from_millis(250));
    }
}

impl eframe::App for EsMailApp {
    /// Called every frame `ui()` is, *and* while the window is hidden as
    /// long as a repaint keeps getting requested (see `handle_tray`'s last
    /// line) -- unlike `ui()`, which eframe skips entirely while hidden.
    /// That's the whole mechanism B10's tray support depends on: draining
    /// tray-icon/menu clicks and the close-to-tray redirect both need to
    /// keep working after the main window is gone, so they live here rather
    /// than in `ui()`. New-mail polling and toast notifications do *not*
    /// need to be here -- see `spawn_new_mail_watch`, a plain tokio task
    /// that runs independent of both `logic()` and `ui()`.
    #[cfg(target_os = "windows")]
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_tray(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.screenshotter.update(ui.ctx(), !self.web_view.is_rendering());

        // Window-geometry persistence (B9): keep the latest known outer rect
        // around every frame (cheap -- just a field write, no I/O), and
        // write it to config.toml exactly once when a real close is going
        // through. `close_requested()` fires the same frame the window's
        // own close button (or, on Windows, the tray's "Quit") is clicked;
        // `handle_tray`'s hide-to-tray redirect on Windows cancels most of
        // those, so `exit_requested` (set only by that Quit path) has to
        // gate this the same way it gates the redirect itself, or a plain
        // window close on Windows would never reach this at all.
        if let Some(rect) = ui.ctx().input(|i| i.viewport().outer_rect) {
            self.window_geometry = Some(config::WindowGeometry {
                x: rect.min.x,
                y: rect.min.y,
                width: rect.width(),
                height: rect.height(),
            });
        }
        #[cfg(target_os = "windows")]
        let closing = ui.ctx().input(|i| i.viewport().close_requested()) && self.exit_requested;
        #[cfg(not(target_os = "windows"))]
        let closing = ui.ctx().input(|i| i.viewport().close_requested());
        if closing && !self.geometry_saved_on_close {
            self.geometry_saved_on_close = true;
            self.save_window_geometry();
        }

        if self.preview {
            egui::CentralPanel::default().show(ui, |ui| {
                for event in self.web_view.show(ui) {
                    // `WebViewEvent` has one variant today (LinkClicked) --
                    // matched with `let` rather than `if let` since the
                    // latter is a no-op refutability check. Restore `if let`
                    // if a second variant is ever added.
                    let egui_litehtml_webview::WebViewEvent::LinkClicked(url) = event;
                    log::info!("preview: link clicked -> {url}");
                }
            });
            return;
        }

        self.handle_oauth_events();
        self.handle_imap_events();
        self.handle_db_events();
        self.handle_smtp_events();

        if self.is_connected {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Download All (This Mailbox)").clicked() {
                        let _ = self.imap_tx.try_send(ImapCommand::BulkDownload { mailbox: self.selected_mailbox.clone() });
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Logout").clicked() {
                        self.is_connected = false;
                        self.headers.clear();
                        self.selected_uid = None;
                        self.selected_uids.clear();
                        self.select_anchor = None;
                        self.pending_mark_seen = None;
                        self.mailbox_rows.clear();
                        self.unread_counts.clear();
                        self.status = "Logged out".to_string();
                        self.web_view.load(WebViewSource::Html("<h1>Logged out</h1>".to_string()));
                        ui.close();
                    }
                });
                if ui.button("New Message").clicked() {
                    self.compose = Some(compose::ComposeState::default());
                    self.compose_status.clear();
                }
            });
        }

        egui::Panel::top("top_panel").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("esMail");
                ui.separator();
                
                if self.is_connected {
                    ui.label("Search:");
                    // A fixed id (rather than the auto-generated one) so
                    // Ctrl+F (B8) can `request_focus` it from outside this
                    // closure, where `self.search_query`'s borrow isn't
                    // available to re-add the same widget.
                    let search_resp = ui.add(egui::TextEdit::singleline(&mut self.search_query).id_salt("search_box").hint_text("Enter keywords..."));
                    self.search_box_id = Some(search_resp.id);
                    if search_resp.changed() || (search_resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) {
                        // `from:`/`to:`/`subject:`/`body:` and bare text all
                        // become an FTS5 MATCH expression; `since:`/`before:`/
                        // `is:unread`/`has:attachment` parse but aren't
                        // applied yet (see search_query.rs) — a query made
                        // only of those is treated the same as an empty one.
                        match ParsedQuery::parse(&self.search_query).to_fts_match() {
                            Some(fts_query) => {
                                let _ = self.db_tx.try_send(DbCommand::Search {
                                    account_id: self.account_id(),
                                    query: fts_query,
                                    mailbox: Some(self.selected_mailbox.clone()),
                                });
                            }
                            None => {
                                self.search_results = None;
                            }
                        }
                    }
                    if ui.button("Clear").clicked() {
                        self.search_query.clear();
                        self.search_results = None;
                    }
                    ui.separator();
                }

                ui.label(&self.status);

                // Theme toggle (B9): right-aligned so it stays in a
                // consistent spot regardless of how long `self.status` is.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let label = format!("Theme: {}", self.theme.label());
                    if ui.button(label).on_hover_text("Cycle Dark / Light / System").clicked() {
                        let next = self.theme.next();
                        self.apply_theme(ui.ctx(), next);
                    }
                    if ui.button("Settings").clicked() {
                        self.open_settings();
                    }
                });
            });
        });

        // Error banners (B9) — see `Banner`'s doc. Shown below the top panel
        // so they don't shove the search box around; a dismissed banner is
        // just removed from the list, nothing more.
        if !self.banners.is_empty() {
            egui::Panel::top("error_banners").show(ui, |ui| {
                let mut dismissed = None;
                for banner in &self.banners {
                    ui.horizontal(|ui| {
                        ui.colored_label(egui::Color32::from_rgb(180, 40, 40), "⚠");
                        ui.colored_label(egui::Color32::from_rgb(180, 40, 40), &banner.message);
                        if ui.small_button("x").on_hover_text("Dismiss").clicked() {
                            dismissed = Some(banner.id);
                        }
                    });
                }
                if let Some(id) = dismissed {
                    self.banners.retain(|b| b.id != id);
                }
            });
        }

        if self.is_connected {
            self.handle_mark_seen_delay();
            self.handle_keyboard_shortcuts(ui);
        }

        if !self.is_connected {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.group(|ui| {
                        ui.set_width(300.0);
                        ui.heading("Login");

                        if !self.config.accounts.is_empty() {
                            ui.label("Saved accounts:");
                            let mut to_remove = None;
                            for account in self.config.accounts.clone() {
                                ui.horizontal(|ui| {
                                    if ui.button(&account.display_name).clicked() {
                                        self.select_account(&account);
                                    }
                                    if ui.small_button("x").on_hover_text("Forget this account").clicked() {
                                        to_remove = Some(account.id.clone());
                                    }
                                });
                            }
                            if let Some(id) = to_remove {
                                secrets::delete_password(&id, "imap");
                                secrets::delete_password(&id, "oauth");
                                self.config.remove_account(&id);
                                if let Err(e) = self.config.save() {
                                    log::warn!("could not persist account removal: {e}");
                                }
                            }
                            ui.separator();
                        }

                        // First-run wizard (B9): only shown before any
                        // account has ever been saved -- a returning user
                        // picking a saved account above, or editing an
                        // already-filled-in host, has nothing this would
                        // usefully guess. Typing a recognized domain here
                        // (gmail.com, outlook.com, ...) fills in the fields
                        // below from `config::PROVIDERS`; an unrecognized
                        // domain leaves them for the user to type directly,
                        // same as before this existed.
                        if self.config.accounts.is_empty() {
                            let email_resp = ui.add(
                                egui::TextEdit::singleline(&mut self.wizard_email)
                                    .hint_text("Email address (gmail.com, outlook.com, ...)"),
                            );
                            if email_resp.changed() {
                                self.apply_provider_wizard();
                            }
                            ui.separator();
                        }

                        ui.add(egui::TextEdit::singleline(&mut self.host).hint_text("IMAP Host"));
                        ui.add(egui::TextEdit::singleline(&mut self.port).hint_text("Port"));
                        ui.add(egui::TextEdit::singleline(&mut self.username).hint_text("Username"));

                        // Gmail can sign in through the browser instead of
                        // an app password (see `oauth`'s module doc). The
                        // box only appears for Gmail's host, and replaces the
                        // password field when ticked.
                        if self.host.trim() == GMAIL_IMAP_HOST {
                            ui.checkbox(&mut self.use_oauth, "Sign in with Google (no app password)");
                        }
                        let oauth_active = self.oauth_active();
                        if !oauth_active {
                            ui.add(egui::TextEdit::singleline(&mut self.password).password(true).hint_text("Password"));
                        }

                        // Guessed by config::derive_smtp_host (imap. -> smtp.)
                        // when this is a brand new account; editable since
                        // that guess is often wrong. Used by B7's Send.
                        ui.add(egui::TextEdit::singleline(&mut self.smtp_host).hint_text("SMTP Host"));
                        ui.add(egui::TextEdit::singleline(&mut self.smtp_port).hint_text("SMTP Port"));

                        if self.oauth_task.is_some() {
                            ui.label("Finish signing in with Google in your browser...");
                            if ui.button("Cancel").clicked() {
                                if let Some(task) = self.oauth_task.take() {
                                    task.abort();
                                }
                                self.status = "Ready".to_string();
                            }
                        } else {
                            ui.horizontal(|ui| {
                                if ui.button("Connect").clicked() {
                                    self.connect_clicked(ui.ctx());
                                }
                                if oauth_active
                                    && ui
                                        .button("Sign in again")
                                        .on_hover_text("Go through Google's consent page again, even if this account was approved before")
                                        .clicked()
                                {
                                    self.begin_google_sign_in(ui.ctx());
                                }
                            });
                            if oauth_active {
                                ui.weak("The first time, your browser opens so you can approve access.");
                            }
                        }
                    });
                });
            });
        } else {
            // Mailbox tree (B8) as its own column, separate from the
            // message-list column below -- previously both lived stacked in
            // one narrow `left_panel`, which squeezed the tree into a
            // `max_height(220.0)` scroll area regardless of how much vertical
            // room the window actually had. As its own resizable panel, the
            // tree gets the full column width and full available height.
            egui::Panel::left("mailbox_panel").resizable(true).default_size(240.0).show(ui, |ui| {
                ui.heading("Mailboxes");
                egui::ScrollArea::vertical().id_salt("mailboxes_scroll").show(ui, |ui| {
                    ui.with_layout(egui::Layout::top_down_justified(egui::Align::LEFT), |ui| {
                        // Deferred past the loop for the same reason as the
                        // message list below: fetch_headers needs &mut self,
                        // which can't happen while `row` still borrows
                        // self.mailbox_rows.
                        let mut clicked_mailbox = None;
                        // Same deferral for a fold toggle: it edits
                        // `self.config`, which `row` (borrowed from
                        // `self.mailbox_rows`) is still alive across.
                        let mut toggled_folder: Option<(String, bool)> = None;
                        let account_id = self.account_id();
                        let collapsed: std::collections::BTreeSet<String> = self
                            .config
                            .collapsed_folders
                            .iter()
                            .filter_map(|k| k.strip_prefix(&account_id)?.strip_prefix('\t'))
                            .map(str::to_string)
                            .collect();
                        for index in imap::visible_rows(&self.mailbox_rows, &collapsed) {
                            let row = &self.mailbox_rows[index];
                            let is_collapsed = row.has_children && collapsed.contains(&row.key);
                            // A folded node shows its whole subtree's unread
                            // count, so mail in a hidden child is not lost.
                            let subtree_unread = |own: Option<&String>| -> u32 {
                                let own = own.and_then(|n| self.unread_counts.get(n)).copied().unwrap_or(0);
                                let below: u32 = if is_collapsed {
                                    imap::descendants(&self.mailbox_rows, index)
                                        .iter()
                                        .filter_map(|r| r.full_name.as_ref().and_then(|n| self.unread_counts.get(n)))
                                        .sum()
                                } else {
                                    0
                                };
                                own + below
                            };
                            let mut indent = |ui: &mut egui::Ui| {
                                ui.add_space(row.depth as f32 * 14.0);
                                if row.has_children {
                                    let arrow = if is_collapsed { "\u{25b8}" } else { "\u{25be}" };
                                    let hint = if is_collapsed { "Expand" } else { "Collapse" };
                                    if ui.add(egui::Button::new(arrow).frame(false).small()).on_hover_text(hint).clicked() {
                                        toggled_folder = Some((row.key.clone(), !is_collapsed));
                                    }
                                } else {
                                    // Keeps leaf labels lined up with the
                                    // labels of their siblings that have an
                                    // arrow.
                                    ui.add_space(ui.spacing().interact_size.y * 0.75);
                                }
                            };
                            let Some(full_name) = &row.full_name else {
                                // A hierarchy node with no mailbox of its own
                                // (see MailboxNode::full_name's doc) -- shown
                                // as a plain, unclickable label. Also covers
                                // a real `LIST`ed name the server marked
                                // `\Noselect` (e.g. Gmail's `[Gmail]`) --
                                // `mailbox_tree` leaves `full_name` unset for
                                // those too, since neither can be
                                // `SELECT`/`EXAMINE`d.
                                let unread = subtree_unread(None);
                                ui.horizontal(|ui| {
                                    indent(ui);
                                    let label = if unread > 0 { format!("{}  ({unread})", row.label) } else { row.label.clone() };
                                    ui.label(egui::RichText::new(label).weak());
                                });
                                continue;
                            };
                            let is_selected = self.selected_mailbox == *full_name;
                            let unread = subtree_unread(Some(full_name));
                            let label = if unread > 0 {
                                format!("{}  ({unread})", row.label)
                            } else {
                                row.label.clone()
                            };
                            ui.horizontal(|ui| {
                                indent(ui);
                                if ui.add(egui::Button::selectable(is_selected, label)).clicked() {
                                    clicked_mailbox = Some(full_name.clone());
                                }
                            });
                        }
                        if let Some((key, collapse)) = toggled_folder {
                            if self.config.set_folder_collapsed(&account_id, &key, collapse) {
                                self.save_config("folded mailbox folders");
                            }
                        }
                        if let Some(mb) = clicked_mailbox {
                            self.selected_mailbox = mb.clone();
                            self.selected_uid = None;
                            self.selected_uids.clear();
                            self.select_anchor = None;
                            self.current_page = 1;
                            self.fetch_headers(mb, 1);
                        }
                    });
                });
            });

            // Message list, as its own column next to the mailbox tree.
            egui::Panel::left("message_list_panel").resizable(true).default_size(320.0).show(ui, |ui| {
                let title = if self.search_results.is_some() {
                    "Search Results".to_string()
                } else {
                    self.selected_mailbox.clone()
                };
                ui.horizontal(|ui| {
                    ui.heading(&title);
                    if self.search_results.is_none() {
                        if ui.button("Refresh").clicked() {
                            self.fetch_headers(self.selected_mailbox.clone(), self.current_page);
                        }
                    }
                });

                // Bulk actions (B8): act on the multi-selection when
                // non-empty, otherwise the single open message. Always
                // shown (rather than only once something's selected) so
                // their availability doesn't jump around as selection
                // changes -- each is simply a no-op send if there's nothing
                // to act on.
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Mark read").clicked() {
                        self.store_flags_on_selection(vec![imap::FLAG_SEEN.to_string()], vec![]);
                    }
                    if ui.button("Mark unread").clicked() {
                        self.store_flags_on_selection(vec![], vec![imap::FLAG_SEEN.to_string()]);
                    }
                    if ui.button("★ Star").clicked() {
                        self.store_flags_on_selection(vec![imap::FLAG_FLAGGED.to_string()], vec![]);
                    }
                    if ui.button("☆ Unstar").clicked() {
                        self.store_flags_on_selection(vec![], vec![imap::FLAG_FLAGGED.to_string()]);
                    }
                    if ui.button("Archive").clicked() {
                        self.archive_selection();
                    }
                    if ui.button("Delete").clicked() {
                        self.delete_selection();
                    }
                });

                if self.search_results.is_none() {
                    egui::Panel::bottom("pagination_panel").show(ui, |ui| {
                        ui.horizontal(|ui| {
                            if ui.button("<").clicked() && self.current_page > 1 {
                                self.current_page -= 1;
                                self.fetch_headers(self.selected_mailbox.clone(), self.current_page);
                            }
                            ui.label(format!("Page {} of {}", self.current_page, self.total_pages));
                            if ui.button(">").clicked() && self.current_page < self.total_pages {
                                self.current_page += 1;
                                self.fetch_headers(self.selected_mailbox.clone(), self.current_page);
                            }
                        });
                    });
                }
                ui.separator();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.with_layout(egui::Layout::top_down_justified(egui::Align::LEFT), |ui| {
                        // `clicked` defers the FetchBody/FetchMail send
                        // until after `list`'s borrow of self.headers /
                        // self.search_results ends below: fetch_body takes
                        // &mut self, which the borrow checker won't allow
                        // while `list` (borrowed from those same fields) is
                        // still alive across the loop.
                        let list = self.search_results.as_ref().unwrap_or(&self.headers);
                        let is_search = self.search_results.is_some();
                        let mut clicked: Option<(u32, egui::Modifiers)> = None;
                        for header in list {
                            let is_selected = self.selected_uids.contains(&header.uid) || self.selected_uid == Some(header.uid);
                            let resp = message_row(ui, header, is_selected);
                            if resp.clicked() {
                                clicked = Some((header.uid, ui.input(|i| i.modifiers)));
                            }
                        }
                        if let Some((uid, modifiers)) = clicked {
                            if modifiers.shift && self.select_anchor.is_some() {
                                let anchor = self.select_anchor.expect("just checked is_some");
                                self.selected_uids = select_range(list, anchor, uid);
                            } else if modifiers.command || modifiers.ctrl {
                                if self.selected_uids.is_empty() {
                                    if let Some(prev) = self.selected_uid {
                                        self.selected_uids.insert(prev);
                                    }
                                }
                                if !self.selected_uids.remove(&uid) {
                                    self.selected_uids.insert(uid);
                                }
                                self.select_anchor = Some(uid);
                            } else {
                                self.selected_uids.clear();
                                self.select_anchor = Some(uid);
                            }
                            self.open_message(uid, is_search);
                        }
                    });
                });
            });

            if let Some((current, total)) = self.download_progress {
                egui::Panel::bottom("progress_status").show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(format!("Indexing {}... ", self.selected_mailbox));
                        ui.add(egui::ProgressBar::new(current as f32 / total as f32)
                            .text(format!("{}/{}", current, total)));
                    });
                });
            }

            egui::CentralPanel::default().show(ui, |ui| {
                if let Some(uid) = self.selected_uid {
                    // Cloned rather than borrowed: the Reply/Reply All/
                    // Forward buttons below need `&mut self.compose` while
                    // this is in scope, which can't coexist with a borrow of
                    // `self.headers` (the same reason the mailbox/message
                    // list loops elsewhere in this file defer their sends).
                    if let Some(header) = self.headers.iter().find(|h| h.uid == uid).cloned() {
                        egui::Panel::top("mail_info").show(ui, |ui| {
                            egui::Grid::new("mail_info_grid").num_columns(2).show(ui, |ui| {
                                ui.label(egui::RichText::new("From:").strong());
                                ui.add(egui::Label::new(&header.from).selectable(true));
                                ui.end_row();

                                ui.label(egui::RichText::new("To:").strong());
                                ui.add(egui::Label::new(&header.to).selectable(true));
                                ui.end_row();

                                ui.label(egui::RichText::new("Date:").strong());
                                ui.add(egui::Label::new(&header.date).selectable(true));
                                ui.end_row();

                                ui.label(egui::RichText::new("Subject:").strong());
                                ui.add(egui::Label::new(&header.subject).selectable(true));
                                ui.end_row();
                            });
                            ui.horizontal(|ui| {
                                if ui.button("Reply").clicked() {
                                    self.compose = Some(compose::ComposeState::reply(&header, &self.current_message_html));
                                    self.compose_status.clear();
                                }
                                if ui.button("Reply All").clicked() {
                                    self.compose = Some(compose::ComposeState::reply_all(
                                        &header,
                                        &self.current_message_html,
                                        &self.username,
                                    ));
                                    self.compose_status.clear();
                                }
                                if ui.button("Forward").clicked() {
                                    self.compose = Some(compose::ComposeState::forward(&header, &self.current_message_html));
                                    self.compose_status.clear();
                                }
                                ui.separator();
                                // Single-message flag/move shortcuts (B8) --
                                // the toolbar in the left panel does the same
                                // thing but over `action_targets()` (the
                                // multi-selection, falling back to this one
                                // open message), so these exist for the
                                // common "just this one" case without first
                                // needing to select it in the list.
                                let star_label = if header.is_flagged() { "☆ Unstar" } else { "★ Star" };
                                if ui.button(star_label).clicked() {
                                    self.toggle_star_on_selection();
                                }
                                if ui.button("Mark unread").clicked() {
                                    self.store_flags_on_selection(vec![], vec![imap::FLAG_SEEN.to_string()]);
                                }
                                if ui.button("Archive").clicked() {
                                    self.archive_selection();
                                }
                                if ui.button("Delete").clicked() {
                                    self.delete_selection();
                                }
                                if ui
                                    .button("Export...")
                                    .on_hover_text("Save this message's raw source as an .eml file")
                                    .clicked()
                                {
                                    self.export_selected_message(&header);
                                }
                            });
                        });
                    }

                    // Every message opens with remote content blocked (see
                    // MessageViewHandler); this is the opt-in per B5. Always
                    // shown rather than only when the message actually has
                    // remote images — knowing whether it does would mean
                    // parsing the HTML again here just to answer that.
                    let sender_trusted = self.current_sender.as_deref().is_some_and(|a| self.config.is_image_trusted(a));
                    if !self.message_view_handler.allow_remote() {
                        egui::Panel::top("remote_images_bar").show(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                ui.label("Remote images are blocked for this message.");
                                if ui.button("Load remote images").clicked() {
                                    self.message_view_handler.set_allow_remote(true);
                                    // The markup never lost its original
                                    // http(s) URLs (see render.rs) -- a
                                    // reload against the same document is
                                    // enough for the now-unblocked requests
                                    // to actually go out.
                                    self.web_view.reload();
                                }
                                if let Some(sender) = self.current_sender.clone() {
                                    if ui
                                        .button(format!("Always load from {sender}"))
                                        .on_hover_text("Load remote images automatically for every message from this address")
                                        .clicked()
                                    {
                                        self.set_image_sender_trusted(&sender, true);
                                        self.message_view_handler.set_allow_remote(true);
                                        self.web_view.reload();
                                    }
                                }
                            });
                        });
                    } else if sender_trusted {
                        // Shown only for a sender on the always-load list,
                        // so it is clear why nothing was blocked and how to
                        // undo it. A message loaded once via "Load remote
                        // images" gets no bar: that choice was this
                        // message's alone.
                        egui::Panel::top("remote_images_bar").show(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                let sender = self.current_sender.clone().unwrap_or_default();
                                ui.label(egui::RichText::new(format!("Remote images load automatically for {sender}.")).weak());
                                if ui.button("Stop").clicked() {
                                    self.set_image_sender_trusted(&sender, false);
                                    self.message_view_handler.set_allow_remote(false);
                                    self.web_view.reload();
                                }
                            });
                        });
                    }

                    if !self.current_attachments.is_empty() {
                        egui::Panel::top("attachments_bar").show(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                for attachment in &self.current_attachments {
                                    ui.group(|ui| {
                                        ui.label(format!(
                                            "{} — {}, {}",
                                            attachment.filename,
                                            attachment.mime_type,
                                            format_size(attachment.data.len())
                                        ));
                                        if ui.button("Save…").clicked() {
                                            save_attachment(attachment);
                                        }
                                        if ui.button("Open").clicked() {
                                            if let Err(e) = open_attachment(attachment) {
                                                log::warn!("could not open attachment {}: {e}", attachment.filename);
                                            }
                                        }
                                    });
                                }
                            });
                        });
                    }
                }

                let events = self.web_view.show(ui);
                for event in events {
                    // See the preview-mode match arm above for why this is
                    // `let` rather than `if let`.
                    let egui_litehtml_webview::WebViewEvent::LinkClicked(url) = event;
                    ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                }
            });
        }

        self.show_compose_window(ui.ctx());
        self.show_settings_window(ui.ctx());
    }
}

/// Mailbox `spawn_new_mail_watch` polls (B10). Hardcoded rather than
/// following `selected_mailbox`: watching whatever mailbox happens to be
/// selected would mean a background task's behavior silently changes based
/// on what the user last clicked in the UI, and would poll nothing at all
/// for a user who is reading a different folder. INBOX is the one mailbox
/// every account has and the one "new mail" conventionally means; see
/// PLAN.md §B10 for the fuller reasoning and what a per-mailbox version
/// would need.
const NEW_MAIL_POLL_MAILBOX: &str = "INBOX";
/// How often `spawn_new_mail_watch` asks `ImapActor` to check
/// [`NEW_MAIL_POLL_MAILBOX`] for new mail.
const NEW_MAIL_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
/// Fallback for where a sent message is `APPEND`ed after sending (B7), used
/// only when [`EsMailApp::special_use_mailbox`] finds no `\Sent`-classified
/// mailbox (via real `LIST` attributes or the name-based fallback in
/// `imap::SpecialUse::from_name`) in the account -- e.g. before
/// `FetchMailboxes`'s reply has arrived at all. See issue #9: this constant
/// used to be the *only* destination, silently creating a new top-level
/// mailbox on any account whose Sent folder wasn't literally named "Sent"
/// (Gmail's `[Gmail]/Sent Mail`, say).
const SENT_MAILBOX: &str = "Sent";
/// Delete-to-Trash's fallback destination (B8). Same
/// only-used-when-special-use-discovery-comes-up-empty caveat as
/// `SENT_MAILBOX` above.
const TRASH_MAILBOX: &str = "Trash";
/// Archive's destination (B8). Same caveat as `TRASH_MAILBOX`.
const ARCHIVE_MAILBOX: &str = "Archive";
/// How long a message must stay open before B8 marks it `\Seen` -- long
/// enough that quickly arrowing past messages with `j`/`k` doesn't mark them
/// all read, short enough that actually reading one still marks it promptly.
const MARK_SEEN_DELAY: std::time::Duration = std::time::Duration::from_millis(1200);

/// Background watcher for B10 (new-mail notifications). Forwards every
/// `ImapEvent` from `ImapActor` to the UI channel (bumping a repaint) --
/// exactly what the bridging task this replaced did -- while additionally:
///
/// - tracking whether the account is currently connected (from `Connected`/
///   `Disconnected`, which pass through this same stream already),
/// - asking for a [`ImapCommand::PollMailbox`] on [`NEW_MAIL_POLL_INTERVAL`]
///   whenever connected, **and** immediately on every `idle_wake` push (IMAP
///   `IDLE`, via `idle_watch` -- see its module doc) rather than waiting for
///   the timer, so new mail shows up within about as long as the round trip
///   takes instead of up to [`NEW_MAIL_POLL_INTERVAL`] later. The interval
///   timer still runs unconditionally: it is what keeps working if `IDLE`
///   isn't supported by the server, or `idle_watch`'s connection is
///   mid-reconnect, so nothing regresses versus B10's original poll-only
///   behavior -- `IDLE` only ever makes new mail show up *sooner*,
/// - folding each [`ImapEvent::MailboxPolled`] into `notify::update_watermark`
///   and, on a `NewMail` verdict, requesting the envelopes that describe it,
/// - turning the resulting [`ImapEvent::NewHeaders`] into a toast via
///   `notify::build_notification` + [`notify_new_mail`].
///
/// This is a plain tokio task, not anything driven by `EsMailApp::logic`/
/// `ui`, so it keeps running -- and can keep showing toasts -- for as long
/// as the process is alive, independent of whether the main window is
/// visible. That's what "notifications work even with the window closed"
/// means in practice here: the process (and this task) survives a window
/// close because `tray.rs` turns that close into hide-to-tray instead of
/// exit; nothing about *this* function knows or cares whether the window is
/// visible.
///
/// The in-memory UID watermark this keeps is deliberately not `db.rs`'s
/// `sync_decision`/cache -- see `notify.rs`'s module doc for why -- and is
/// deliberately not a field on `EsMailApp`: keeping it as a local in this
/// task's own async block means no other code can accidentally read or
/// reset it, and it needs no `Send`/lock story since it never leaves this
/// task.
fn spawn_new_mail_watch(
    mut actor_events: mpsc::Receiver<ImapEvent>,
    ui_events: mpsc::Sender<ImapEvent>,
    imap_tx: mpsc::Sender<ImapCommand>,
    ctx: egui::Context,
    mut idle_wake: mpsc::Receiver<idle_watch::MailboxChanged>,
) {
    tokio::spawn(async move {
        let mut connected = false;
        let mut watermark: Option<notify::MailWatermark> = None;
        let mut poll_interval = tokio::time::interval(NEW_MAIL_POLL_INTERVAL);
        poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Disabled once `idle_wake` closes (which nothing currently does --
        // `idle_watch::spawn`'s task loops forever -- but a channel that
        // keeps returning `None` would otherwise busy-loop this `select!`)
        // so the timer-only path keeps working even in that case.
        let mut idle_wake_open = true;

        loop {
            tokio::select! {
                evt = actor_events.recv() => {
                    let Some(evt) = evt else { break };
                    match &evt {
                        ImapEvent::Connected => {
                            connected = true;
                            // A fresh connection (or reconnection) starts a
                            // new baseline -- see `notify::update_watermark`'s
                            // doc for why the first observation after one
                            // must never itself be reported as "new mail".
                            watermark = None;
                        }
                        ImapEvent::Disconnected => connected = false,
                        ImapEvent::MailboxPolled { mailbox, state } if mailbox == NEW_MAIL_POLL_MAILBOX => {
                            let (next, update) = notify::update_watermark(
                                watermark,
                                notify::MailWatermark {
                                    uid_validity: state.uid_validity,
                                    uid_next: state.uid_next,
                                },
                            );
                            watermark = Some(next);
                            if let notify::WatermarkUpdate::NewMail { first_new_uid, .. } = update {
                                let _ = imap_tx.try_send(ImapCommand::FetchNewHeaders {
                                    mailbox: NEW_MAIL_POLL_MAILBOX.to_string(),
                                    first_uid: first_new_uid,
                                });
                            }
                        }
                        ImapEvent::NewHeaders { mailbox, headers } if mailbox == NEW_MAIL_POLL_MAILBOX => {
                            if let Some((title, body)) = notify::build_notification(headers) {
                                notify_new_mail(&title, &body);
                            }
                        }
                        _ => {}
                    }
                    if ui_events.send(evt).await.is_err() {
                        break; // EsMailApp is gone; nothing left to forward to.
                    }
                    ctx.request_repaint();
                }
                _ = poll_interval.tick(), if connected => {
                    let _ = imap_tx.try_send(ImapCommand::PollMailbox {
                        mailbox: NEW_MAIL_POLL_MAILBOX.to_string(),
                    });
                }
                woke = idle_wake.recv(), if idle_wake_open => {
                    match woke {
                        Some(idle_watch::MailboxChanged) if connected => {
                            let _ = imap_tx.try_send(ImapCommand::PollMailbox {
                                mailbox: NEW_MAIL_POLL_MAILBOX.to_string(),
                            });
                        }
                        Some(idle_watch::MailboxChanged) => {
                            // A push arrived while `ImapActor`'s own session
                            // is disconnected/reconnecting -- nothing to poll
                            // with right now; the timer (once `connected`
                            // again) or the next push will catch it.
                        }
                        None => idle_wake_open = false,
                    }
                }
            }
        }
    });
}

/// Show a new-mail toast on Windows; elsewhere, just log it. B10 is
/// Windows-only (see PLAN.md §B10) -- this is the one place that
/// distinction is made, so `spawn_new_mail_watch` above doesn't need its own
/// `#[cfg]`.
fn notify_new_mail(title: &str, body: &str) {
    #[cfg(target_os = "windows")]
    tray::show_new_mail_toast(title, body);
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (title, body);
        log::info!("new mail: {title} -- {body} (desktop notifications are Windows-only, see PLAN.md §B10)");
    }
}

/// Save-as, via a native file picker pre-filled with the attachment's own
/// name. Does nothing if the user cancels the dialog; a write failure is
/// logged rather than surfaced (mirroring the "log, don't crash the UI over
/// it" treatment other best-effort I/O gets in this file).
fn save_attachment(attachment: &render::Attachment) {
    let Some(path) = rfd::FileDialog::new().set_file_name(&attachment.filename).save_file() else {
        return;
    };
    if let Err(e) = std::fs::write(&path, &attachment.data) {
        log::warn!("could not save attachment to {}: {e}", path.display());
    }
}

/// Open-with: write the attachment to a temp file (there is no path for it
/// yet — it only exists as bytes in memory) and hand that to the OS's
/// default handler for its type. The temp file is left behind rather than
/// cleaned up immediately, since the opened application may still be reading
/// it after this call returns.
fn open_attachment(attachment: &render::Attachment) -> std::io::Result<()> {
    let dir = std::env::temp_dir().join("esmail-attachments");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(safe_attachment_filename(&attachment.filename));
    std::fs::write(&path, &attachment.data)?;
    opener::open(&path).map_err(|e| std::io::Error::other(e.to_string()))
}

/// `filename` comes straight from the message's own
/// Content-Disposition/Content-Type header — an attacker-controlled sender's
/// mail. Taking only the final path component (and falling back to a fixed
/// name if that leaves nothing usable) keeps a crafted `"../../../whatever"`
/// or an absolute path from writing outside the caller's chosen directory,
/// since `Path::join` would otherwise honor either verbatim.
///
/// Splits on `/` *and* `\` manually rather than using `std::path::Path`:
/// `Path`'s separator handling is host-OS-dependent, so on a Linux build
/// `Path::new(r"C:\Windows\System32\evil.dll").file_name()` treats the
/// whole string as one component (`\` isn't a separator on Unix) and
/// returns it unstripped. A sender-controlled filename is untrusted
/// regardless of which OS esmail happens to be running on, so the
/// stripping has to be too.
fn safe_attachment_filename(filename: &str) -> String {
    match filename.rsplit(['/', '\\']).next() {
        Some(name) if !name.is_empty() && name != "." && name != ".." => name.to_string(),
        _ => "attachment".to_string(),
    }
}

/// A default file name for exporting a message: its subject, reduced to
/// characters that are safe in a file name on every OS, plus `.eml`. Falls
/// back to the UID when the subject leaves nothing usable.
fn export_file_name(subject: &str, uid: u32) -> String {
    let cleaned: String = subject
        .chars()
        .map(|c| if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.') { c } else { '_' })
        .collect();
    let cleaned: String = cleaned.trim_matches(|c: char| c == '.' || c == '_' || c.is_whitespace()).chars().take(60).collect();
    if cleaned.is_empty() {
        format!("message-{uid}.eml")
    } else {
        format!("{}.eml", cleaned.trim_end())
    }
}

/// One row of the message list: the sender on a first line, the subject
/// beneath it, each cut off with an ellipsis rather than wrapped so every row
/// has the same height.
///
/// Unread and read rows are told apart by more than a bullet: an unread row
/// gets an accent bar on its left edge, its sender in the strong text colour
/// (drawn twice, half a pixel apart, as egui ships no bold face) and its
/// subject in the normal colour; a read row has neither bar nor emphasis, a
/// normal-colour sender and a dimmed subject. A starred message gets a ★ at
/// the right end of the sender line. Painted by hand, not with a `Button`,
/// because a button cannot truncate two differently-styled lines.
fn message_row(ui: &mut egui::Ui, header: &MailHeader, selected: bool) -> egui::Response {
    const PAD_X: f32 = 10.0;
    const PAD_Y: f32 = 6.0;
    const ACCENT_BAR_WIDTH: f32 = 3.0;
    const LINE_GAP: f32 = 2.0;
    const SENDER_SIZE: f32 = 14.5;
    const SUBJECT_SIZE: f32 = 13.0;

    let unread = !header.is_seen();
    let visuals = ui.visuals();
    let (sender_color, subject_color) = if selected {
        (visuals.selection.stroke.color, visuals.selection.stroke.color)
    } else if unread {
        (visuals.strong_text_color(), visuals.text_color())
    } else {
        (visuals.text_color(), visuals.weak_text_color())
    };
    let accent = visuals.hyperlink_color;
    let star_color = visuals.warn_fg_color;
    let selected_fill = visuals.selection.bg_fill;
    let hovered_fill = visuals.widgets.hovered.weak_bg_fill;
    let separator = visuals.widgets.noninteractive.bg_stroke;

    // Lays `text` out on one line, truncated with an ellipsis at `width`.
    let one_line = |ui: &egui::Ui, text: &str, size: f32, color: egui::Color32, width: f32| {
        let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), egui::FontId::proportional(size), color);
        job.wrap = egui::text::TextWrapping::truncate_at_width(width);
        ui.painter().layout_job(job)
    };

    let width = ui.available_width();
    let star = header
        .is_flagged()
        .then(|| one_line(ui, "\u{2605}", SENDER_SIZE, star_color, f32::INFINITY));
    let star_width = star.as_ref().map_or(0.0, |g| g.size().x + 4.0);
    let text_width = (width - ACCENT_BAR_WIDTH - PAD_X * 2.0 - star_width).max(0.0);

    let sender = header.sender_name();
    let sender = if sender.is_empty() { "(unknown sender)" } else { sender.as_str() };
    let subject = if header.subject.is_empty() { "(no subject)" } else { header.subject.as_str() };
    let sender_galley = one_line(ui, sender, SENDER_SIZE, sender_color, text_width);
    let subject_galley = one_line(ui, subject, SUBJECT_SIZE, subject_color, text_width);

    let height = PAD_Y * 2.0 + sender_galley.size().y + LINE_GAP + subject_galley.size().y;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click());
    response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, selected, format!("{sender}: {subject}")));

    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        if selected {
            painter.rect_filled(rect, 0.0, selected_fill);
        } else if response.hovered() {
            painter.rect_filled(rect, 0.0, hovered_fill);
        }
        if unread {
            let bar = egui::Rect::from_min_size(rect.min, egui::vec2(ACCENT_BAR_WIDTH, rect.height()));
            painter.rect_filled(bar, 0.0, if selected { sender_color } else { accent });
        }
        let text_left = rect.left() + ACCENT_BAR_WIDTH + PAD_X;
        let sender_pos = egui::pos2(text_left, rect.top() + PAD_Y);
        painter.galley(sender_pos, sender_galley.clone(), sender_color);
        if unread {
            painter.galley(sender_pos + egui::vec2(0.5, 0.0), sender_galley.clone(), sender_color);
        }
        if let Some(star) = star {
            painter.galley(egui::pos2(rect.right() - PAD_X - star.size().x, sender_pos.y), star, star_color);
        }
        let subject_pos = egui::pos2(text_left, sender_pos.y + sender_galley.size().y + LINE_GAP);
        painter.galley(subject_pos, subject_galley, subject_color);
        painter.hline(rect.x_range(), rect.bottom(), separator);
    }

    response.on_hover_ui(|ui| {
        ui.label(&header.from);
        ui.label(&header.subject);
    })
}

/// The set of UIDs between `anchor` and `uid` (inclusive) in `list`'s
/// current order, for shift-click range selection (B8). Falls back to just
/// `{uid}` if either isn't actually in `list` (e.g. the anchor was on a page
/// that's since been paged away from).
fn select_range(list: &[MailHeader], anchor: u32, uid: u32) -> std::collections::BTreeSet<u32> {
    let idx_a = list.iter().position(|h| h.uid == anchor);
    let idx_b = list.iter().position(|h| h.uid == uid);
    match (idx_a, idx_b) {
        (Some(a), Some(b)) => {
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            list[lo..=hi].iter().map(|h| h.uid).collect()
        }
        _ => std::iter::once(uid).collect(),
    }
}

/// The real mailbox name for a special-use role, from whichever
/// `MailboxRow` in `rows` classifies as `want` -- pure half of
/// `EsMailApp::special_use_mailbox`, pulled out so it's testable without
/// constructing a whole `EsMailApp`. Falls back to `default` if no row
/// matches (e.g. before `Mailboxes` has arrived, or a server that
/// advertises no special-use attributes and has no conventionally-named
/// folder for `want` either -- see `imap::SpecialUse::from_name`).
fn find_special_use_mailbox(rows: &[imap::MailboxRow], want: imap::SpecialUse, default: &str) -> String {
    rows.iter()
        .find(|row| row.special_use == Some(want))
        .and_then(|row| row.full_name.clone())
        .unwrap_or_else(|| default.to_string())
}

/// A human-readable size, e.g. `"4.2 KB"`. Only goes up to MB since a mail
/// attachment in the GB range would be unusual enough to want the exact byte
/// count anyway.
fn format_size(bytes: usize) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= MB {
        format!("{:.1} MB", bytes / MB)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes / KB)
    } else {
        format!("{} B", bytes as u64)
    }
}

#[tokio::main]
async fn main() -> eframe::Result {
    // Window-geometry persistence (B9): the saved size/position has to be
    // known before the window is created at all, so this reads config.toml
    // a second time here (`EsMailApp::new` also loads it, for the account
    // list and theme) rather than threading a pre-loaded `Config` through
    // `run_native`'s `Box<dyn FnOnce>` closure -- a second cheap file read on
    // startup is a small price for keeping `EsMailApp::new`'s signature
    // (`&eframe::CreationContext`, same as every other eframe app) untouched.
    let mut viewport = egui::ViewportBuilder::default().with_inner_size([1280.0, 720.0]);
    if let Some(geometry) = config::Config::load().window {
        viewport = viewport
            .with_inner_size([geometry.width, geometry.height])
            .with_position([geometry.x, geometry.y]);
    }
    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "esMail",
        native_options,
        Box::new(|cc| Ok(Box::new(EsMailApp::new(cc)))),
    )
}

/// A page that exercises the parts of the webview we care about for mail:
/// text flow, images, tables, links, forms, and scrolling past the fold.
fn preview_demo_html() -> String {
    r#"<!doctype html>
<meta charset="utf-8">
<style>
  body { font: 16px/1.5 system-ui, sans-serif; margin: 2rem; color: #111; }
  table { border-collapse: collapse; } td, th { border: 1px solid #999; padding: .3rem .6rem; }
  .tall { height: 60vh; background: linear-gradient(#eee, #fff); }
</style>
<h1>esMail webview preview</h1>
<p>Accented text to check character encoding: <b>&eacute;&agrave;&uuml;&ccedil;</b> &euro; &mdash; &ldquo;quoted&rdquo;.</p>
<p><a href="https://example.com/clicked">A link</a> &mdash; clicking it should emit LinkClicked and not navigate.</p>
<table><tr><th>From</th><th>Subject</th></tr><tr><td>a@b.c</td><td>Hello</td></tr></table>
<p>Type here to check keyboard input: <input type="text" size="30" placeholder="type me"></p>
<div class="tall">Scroll down past this block to check scrolling.</div>
<h2 id="bottom">Bottom of the page</h2>
"#
    .to_string()
}

/// Install the logger.
///
/// `fontdb` (which the message renderer uses to find system fonts) can still complain
/// about individual malformed fonts installed on the system, which says
/// nothing about this application -- `RUST_LOG` overrides the default if
/// that gets noisy.
///
/// `html5ever` (the parser inside `ammonia`, our sanitizer) logs a warning
/// for every misnested table node ("foster parenting not implemented"), and
/// marketing HTML is full of them -- one message can produce hundreds of
/// identical lines, none actionable. It is muted even when `RUST_LOG` is set
/// (people set that to see *our* debug output), unless `RUST_LOG` mentions
/// `html5ever` itself.
fn init_logging() {
    const QUIET: &str = "warn,fontdb=error";

    let mut filter = std::env::var("RUST_LOG").unwrap_or_else(|_| QUIET.to_string());
    if !filter.contains("html5ever") {
        filter.push_str(",html5ever=error");
    }
    let _ = env_logger::Builder::new().parse_filters(&filter).try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_size_uses_bytes_below_one_kb() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
    }

    #[test]
    fn format_size_uses_kb_between_one_kb_and_one_mb() {
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(4300), "4.2 KB");
    }

    #[test]
    fn format_size_uses_mb_at_one_mb_and_above() {
        assert_eq!(format_size(1024 * 1024), "1.0 MB");
        assert_eq!(format_size(5 * 1024 * 1024 + 512 * 1024), "5.5 MB");
    }

    // ── safe_attachment_filename ─────────────────────────────────────────────

    #[test]
    fn safe_attachment_filename_passes_an_ordinary_name_through() {
        assert_eq!(safe_attachment_filename("report.pdf"), "report.pdf");
    }

    #[test]
    fn safe_attachment_filename_strips_relative_traversal() {
        // Regression test: a crafted "../../../whatever" from a malicious
        // sender's Content-Disposition header must not be able to write
        // outside the caller's chosen directory when joined onto it.
        assert_eq!(safe_attachment_filename("../../../evil.exe"), "evil.exe");
        assert_eq!(safe_attachment_filename("../../etc/passwd"), "passwd");
    }

    #[test]
    fn safe_attachment_filename_strips_a_windows_absolute_path() {
        assert_eq!(
            safe_attachment_filename(r"C:\Windows\System32\evil.dll"),
            "evil.dll"
        );
    }

    // ── export_file_name ─────────────────────────────────────────────────────

    #[test]
    fn export_file_name_uses_the_subject_with_unsafe_characters_replaced() {
        assert_eq!(export_file_name("Votre projet: un coup de pouce.", 7), "Votre projet_ un coup de pouce.eml");
        assert_eq!(export_file_name(r"a/b\c?d", 7), "a_b_c_d.eml");
    }

    #[test]
    fn export_file_name_falls_back_to_the_uid() {
        assert_eq!(export_file_name("", 42), "message-42.eml");
        assert_eq!(export_file_name("???", 42), "message-42.eml");
    }

    #[test]
    fn export_file_name_is_bounded() {
        let name = export_file_name(&"x".repeat(500), 1);
        assert_eq!(name.len(), 60 + ".eml".len());
    }

    // ── find_special_use_mailbox ─────────────────────────────────────────────

    fn row(full_name: Option<&str>, special_use: Option<imap::SpecialUse>) -> imap::MailboxRow {
        let label = full_name.unwrap_or("").to_string();
        imap::MailboxRow { depth: 0, key: label.clone(), label, full_name: full_name.map(str::to_string), special_use, has_children: false }
    }

    #[test]
    fn find_special_use_mailbox_prefers_a_classified_mailbox_over_the_default() {
        // Regression test for issue #9: Gmail's Sent folder is
        // "[Gmail]/Sent Mail", not "Sent" -- special-use discovery must
        // pick the real name over the hardcoded fallback.
        let rows = vec![
            row(Some("INBOX"), Some(imap::SpecialUse::Inbox)),
            row(Some("[Gmail]/Sent Mail"), Some(imap::SpecialUse::Sent)),
        ];
        assert_eq!(find_special_use_mailbox(&rows, imap::SpecialUse::Sent, "Sent"), "[Gmail]/Sent Mail");
    }

    #[test]
    fn find_special_use_mailbox_falls_back_to_default_when_nothing_matches() {
        let rows = vec![row(Some("INBOX"), Some(imap::SpecialUse::Inbox))];
        assert_eq!(find_special_use_mailbox(&rows, imap::SpecialUse::Trash, "Trash"), "Trash");
    }

    #[test]
    fn find_special_use_mailbox_falls_back_when_mailboxes_have_not_loaded_yet() {
        assert_eq!(find_special_use_mailbox(&[], imap::SpecialUse::Archive, "Archive"), "Archive");
    }

    #[test]
    fn safe_attachment_filename_falls_back_when_nothing_usable_remains() {
        assert_eq!(safe_attachment_filename(""), "attachment");
        assert_eq!(safe_attachment_filename(".."), "attachment");
        assert_eq!(safe_attachment_filename("/"), "attachment");
    }
}
