// A release build on Windows is a GUI program: without this, launching it from
// the Start menu or Explorer opens a console window behind the app. Debug
// builds keep the console so `cargo run` shows the log.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use esmail::{auth, compose, config, db, emoji, icons, imap, oauth, paths, render, screenshot, search_query, secrets, session, shell, smtp, uninstall};
mod accounts;
mod compose_window;
mod settings;
/// Tray icon + new-mail toasts (B10): the OS-specific side lives behind
/// `platform`, so nothing below names a platform.
use esmail::platform;

use egui_litehtml_webview::{
    ImageRequest, InterceptOutcome, WebView, WebViewConfig, WebViewHandler, WebViewHost,
    WebViewSource,
};
use imap::{ImapCommand, ImapEvent, MailHeader};
use db::{DbActor, DbCommand, DbEvent};
use compose::{ComposeId, ComposeState};
use compose_window::ComposeWindow;
use config::{AccountConfig, Config};
use search_query::ParsedQuery;
use secrecy::SecretString;
use session::{AccountEvent, AccountId, AccountSession, Hooks, SessionParams};
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

/// The host the "Sign in with Google" option is offered for.
const GMAIL_IMAP_HOST: &str = "imap.gmail.com";

/// Where one account's connection stands, for the folder pane.
#[derive(Debug, Clone, PartialEq)]
enum ConnState {
    /// The first connect is still in flight.
    Connecting,
    Connected,
    /// The connection dropped; the actor is reconnecting on its own.
    Disconnected,
    /// The first connect failed (bad password, unreachable server). The
    /// session is kept so the failure shows next to the account, and so
    /// "Reconnect" has something to replace.
    Failed(String),
}

/// One signed-in account and the state that is per account: the session
/// (actor + watcher, see `session.rs`), its mailbox tree and its unread
/// counts. Dropping it is a real logout.
struct AccountView {
    session: AccountSession,
    state: ConnState,
    /// The mailbox tree (B8), flattened for the folder pane -- see
    /// `imap::flatten_tree`'s doc for why a flat, owned `Vec` rather than a
    /// real recursive tree widget.
    mailbox_rows: Vec<imap::MailboxRow>,
    /// `STATUS (UNSEEN)` per mailbox (B8), refreshed whenever `Mailboxes`
    /// arrives and after a flag/move changes what's unread. A mailbox
    /// missing from this map (rather than present with `0`) means its count
    /// hasn't been fetched yet, not that it's read.
    unread_counts: std::collections::HashMap<String, u32>,
    /// What the session signs in with -- a password, or the account's own
    /// Google token source. Kept so SMTP sends reuse the very same OAuth
    /// source (one cached access token per account, not one per connection).
    auth: auth::Auth,
    /// For an account added through the form: the config entry and credential
    /// to save once the connection actually succeeds (not on every click,
    /// and never for a password the server rejected). `None` for an account
    /// that came from the saved list.
    pending_persist: Option<(AccountConfig, auth::Auth)>,
}

impl AccountView {
    fn id(&self) -> &str {
        self.session.id()
    }

    fn label(&self) -> &str {
        self.session.label()
    }

    fn total_unread(&self) -> u32 {
        self.mailbox_rows
            .iter()
            .filter_map(|r| r.full_name.as_ref())
            .filter(|name| name.eq_ignore_ascii_case("INBOX"))
            .filter_map(|name| self.unread_counts.get(name))
            .sum()
    }
}

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
    /// Every connected (or connecting) account, in the order they were
    /// added. Each holds its own `ImapActor`, `IDLE` watch and new-mail
    /// watermark (see `session.rs`); removing one drops all of them.
    accounts: Vec<AccountView>,
    /// The account the message list and reading pane show. `None` until the
    /// first account has connected, and again once the last one is gone.
    active: Option<AccountId>,
    /// Handed (cloned) to each new [`AccountSession`], whose forwarder tags
    /// what it forwards with the account id.
    imap_events_tx: mpsc::Sender<AccountEvent>,
    imap_rx: mpsc::Receiver<AccountEvent>,
    /// Callbacks every new session gets: the toast and the repaint request.
    session_hooks: Hooks,
    /// Show the "Add account" form even though accounts are already
    /// connected. With no accounts the form is shown regardless.
    adding_account: bool,
    /// The account each in-flight SMTP send was issued for, by compose id, so
    /// `Sent` can save the copy to *that* account's Sent folder -- even when
    /// the window that sent it has been closed meanwhile.
    sending_from: std::collections::HashMap<ComposeId, AccountId>,
    /// The outbox row a compose id's message is durably recorded under,
    /// once a send from it has failed at least once -- absent until then,
    /// since the common case (send succeeds first try) never needs one. A
    /// second failed Send from the same still-open window updates this same
    /// row rather than creating a duplicate (see `handle_smtp_events`'s
    /// `Error` arm). Cleared for a real window in `close_compose`; a
    /// synthetic id minted for a background retry (`poll_outbox`) is
    /// one-shot and simply never reused, so its entry (if the retry also
    /// failed) is left to age out rather than chased down -- a handful of
    /// stale 16-byte entries per retried send is not worth extra bookkeeping
    /// to avoid.
    outbox_owner: std::collections::HashMap<ComposeId, i64>,
    /// Outbox row ids with a send currently outstanding -- real (a window's
    /// own retry) or synthetic (`poll_outbox`'s background retry) -- so a
    /// poll during that window doesn't dispatch a second, concurrent attempt
    /// at the same row. Cleared once that attempt's `Sent`/`Error` comes
    /// back.
    outbox_in_flight: std::collections::HashSet<i64>,
    /// When `poll_outbox` last asked the db for due retries -- see
    /// `OUTBOX_POLL_INTERVAL`.
    last_outbox_check: std::time::Instant,
    /// When each open compose window last autosaved itself as a draft -- see
    /// `DRAFT_AUTOSAVE_INTERVAL`. A window's entry is removed once it closes
    /// (`close_compose`), so the map only ever holds entries for windows
    /// that are actually still open.
    compose_last_autosave: std::collections::HashMap<ComposeId, std::time::Instant>,
    /// The Outbox window's contents, refreshed by `DbEvent::OutboxList` --
    /// `None` while the window is closed. `main.rs`'s own module docs on
    /// `settings.rs`'s pattern apply here too: a snapshot taken when opened,
    /// not a live view.
    outbox_window: Option<Vec<db::OutboxItem>>,
    /// The Drafts window's contents, refreshed by `DbEvent::DraftList` --
    /// `None` while the window is closed.
    drafts_window: Option<Vec<db::DraftSummary>>,
    db_tx: mpsc::Sender<DbCommand>,
    db_rx: mpsc::Receiver<DbEvent>,
    smtp_tx: mpsc::Sender<smtp::SmtpCommand>,
    smtp_rx: mpsc::Receiver<smtp::SmtpEvent>,
    /// The login form's "Sign in with Google" choice: OAuth2 through the
    /// browser instead of a password. Only offered for Gmail.
    use_oauth: bool,
    /// Where the browser round trips report back; see `accounts.rs`.
    oauth_tx: mpsc::Sender<accounts::OAuthMessage>,
    oauth_rx: mpsc::Receiver<accounts::OAuthMessage>,
    /// The running browser round trips, one per account (several accounts can
    /// be waiting for their consent page at once). Aborting one closes its
    /// local redirect listener, which is how "Cancel" works.
    oauth_tasks: std::collections::HashMap<AccountId, tokio::task::JoinHandle<()>>,
    /// The open Settings window, if it is open. See `settings.rs`.
    settings: Option<settings::SettingsState>,

    /// Saved accounts (host/port/username; no passwords — those are in the OS
    /// keyring, see `secrets`). Persisted to `config.toml`.
    config: Config,

    // The "Add account" form.
    host: String,
    port: String,
    username: String,
    password: String,
    /// SMTP host for the login form, prefilled from
    /// [`config::derive_smtp_host`]'s guess but editable — see B7 in
    /// PLAN.md.
    smtp_host: String,
    smtp_port: String,
    /// SMTP security for the login form, prefilled from the guessed account
    /// (`Ssl`, matching [`AccountConfig::new`]'s default) but editable —
    /// servers that need `StartTls`/`None` used to require editing the
    /// account in Settings after adding it.
    smtp_tls: config::TlsMode,
    status: String,

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

    /// The mailbox open in the message list, within the `active` account.
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

    /// The open compose windows, one native window per message (see
    /// `compose_window.rs`). Sending or discarding one leaves the rest alone.
    compose_windows: Vec<ComposeWindow>,
    /// The same windows, keyed by id and behind a `Mutex` so the SMTP
    /// forwarder task's background thread can reach a specific one directly
    /// -- see `compose_window.rs`'s module docs and
    /// `ComposeWindow::mark_sent_and_hide`. Kept in sync with
    /// `compose_windows` by `open_compose`/`close_compose`; a clone of the
    /// `Arc` was handed to that task when it was spawned.
    compose_registry: Arc<std::sync::Mutex<std::collections::HashMap<ComposeId, ComposeWindow>>>,
    /// Source of [`ComposeId`]s.
    next_compose_id: ComposeId,
    /// The window icon, shared with the compose windows.
    window_icon: Option<Arc<egui::IconData>>,
    /// Asking whether to quit although compose windows hold unsent text.
    confirm_quit: bool,

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
    /// Where each entry of `search_results` lives (account, mailbox), index
    /// for index. A search can span accounts, and a UID means nothing without
    /// them. Empty exactly when `search_results` is `None`.
    search_origins: Vec<(AccountId, String)>,
    /// Search every account instead of only the active one.
    search_all_accounts: bool,
    /// Opening a search hit from another mailbox moved `active` /
    /// `selected_mailbox` there without reloading `headers`; when the search
    /// is cleared the message list has to be fetched again.
    headers_stale: bool,
    download_progress: Option<(u32, u32)>,

    /// The tray icon (B10), or `None` if either it couldn't be created (see
    /// `platform::TrayState::new`'s doc, or this platform has none) or this is a preview/screenshot run,
    /// where a tray icon would be unwanted background noise for what's
    /// meant to be a one-shot, no-account render. Window-close falls back to
    /// exiting normally whenever this is `None`, rather than hiding a window
    /// with no way to bring it back.
    tray: Option<platform::TrayState>,
    /// Account ids of new-mail toasts that were clicked, sent from the thread
    /// the click arrives on; drained in `handle_tray`.
    toast_click_rx: mpsc::Receiver<AccountId>,
    /// Set by the tray's "Quit" action; the next close-request is then
    /// allowed to actually close the app instead of being redirected to
    /// "hide to tray". See `EsMailApp::logic`.
    exit_requested: bool,
}

impl EsMailApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        init_logging();
        
        // Every account's events arrive here, tagged with the account id by
        // that account's `AccountSession` forwarder (session.rs). Each
        // session also runs its own new-mail watch (B10) as a plain tokio
        // task, not anything hung off `EsMailApp::ui`/`logic`, so toasts
        // keep coming for as long as the process is alive, independent of
        // whether the main window is visible. See platform/windows.rs for how the window
        // survives being "closed".
        let (imap_events_tx, imap_rx) = mpsc::channel(64);
        let (db_cmd_tx, db_cmd_rx) = mpsc::channel(32);
        let (db_evt_tx, db_evt_rx) = mpsc::channel(32);

        let egui_ctx = cc.egui_ctx.clone();

        // A safety-net heartbeat for the root viewport (#34's compose windows
        // exposed this): on Windows, once no esMail window has focus -- which,
        // once a compose window is open, means the main window unless the
        // user deliberately clicks back onto it -- the OS can delay an
        // already-scheduled repaint of it by a long time (observed: well over
        // a minute), even one requested via the correctly-targeted, otherwise
        // instant `request_repaint_of(ROOT)`. `platform::disable_background_throttling`
        // (called once from `main()`) opts the whole process out of the
        // specific throttle documented for this, but wasn't enough by itself
        // in testing, so this thread also just unconditionally re-requests a
        // root repaint on a short, fixed interval for the app's whole
        // lifetime -- cheap (an idle egui pass is not expensive), and it
        // bounds the worst case for whatever isn't handled some other way
        // (see `compose_window.rs`'s module docs for the part that is: a
        // compose window's own send result no longer waits on this at all).
        {
            let ctx = egui_ctx.clone();
            std::thread::spawn(move || {
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    ctx.request_repaint_of(egui::ViewportId::ROOT);
                }
            });
        }

        // A click on a new-mail toast arrives on a thread of the OS's, so it is
        // only handed over here: the account id goes into a channel (drained in
        // `handle_tray`) and the window is asked to come forward and repaint.
        let (toast_click_tx, toast_click_rx) = mpsc::channel(8);
        platform::set_toast_click_handler({
            let ctx = egui_ctx.clone();
            move |account| {
                let _ = toast_click_tx.try_send(account);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                ctx.request_repaint_of(egui::ViewportId::ROOT);
            }
        });
        let session_hooks = Hooks {
            notify: Arc::new(platform::show_new_mail_toast),
            repaint: {
                let ctx = egui_ctx.clone();
                // `request_repaint_of(ROOT)`: the events this wakes
                // (`handle_imap_events` and friends) are only ever drained by
                // the root viewport's `logic()`/`ui()`, so that's the one
                // that must wake up -- see the heartbeat thread above for why
                // that alone isn't always prompt on Windows.
                Arc::new(move || ctx.request_repaint_of(egui::ViewportId::ROOT))
            },
        };

        // Wrap DB events
        let (tx_db, mut rx_db) = mpsc::channel(32);
        let ctx_clone_db = egui_ctx.clone();
        tokio::spawn(async move {
            while let Some(evt) = rx_db.recv().await {
                let _ = db_evt_tx.send(evt).await;
                ctx_clone_db.request_repaint_of(egui::ViewportId::ROOT);
            }
        });
        DbActor::spawn(db_cmd_rx, tx_db);

        let compose_registry: Arc<std::sync::Mutex<std::collections::HashMap<ComposeId, ComposeWindow>>> =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));

        let (smtp_cmd_tx, smtp_cmd_rx) = mpsc::channel(8);
        let (smtp_evt_tx, smtp_evt_rx) = mpsc::channel(8);
        let (tx_smtp, mut rx_smtp) = mpsc::channel(8);
        let ctx_clone_smtp = egui_ctx.clone();
        let compose_registry_smtp = compose_registry.clone();
        tokio::spawn(async move {
            while let Some(evt) = rx_smtp.recv().await {
                // Act on the compose window directly, from this thread, as
                // well as forwarding below -- see `compose_window.rs`'s
                // module docs for why a successful send (or a failure) isn't
                // left to wait on the main window's `logic()` to notice.
                let window = {
                    let id = match &evt {
                        smtp::SmtpEvent::Sent { id, .. } | smtp::SmtpEvent::Error { id, .. } => *id,
                    };
                    compose_registry_smtp.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(&id).cloned()
                };
                if let Some(window) = window {
                    match &evt {
                        smtp::SmtpEvent::Sent { .. } => window.mark_sent_and_hide(&ctx_clone_smtp),
                        smtp::SmtpEvent::Error { error, .. } => {
                            window.set_error_and_wake(&ctx_clone_smtp, format!("Send failed: {error}"));
                        }
                    }
                }
                let _ = smtp_evt_tx.send(evt).await;
                // The bookkeeping `handle_smtp_events` still does with this
                // (dropping the window from `compose_windows`/`compose_registry`,
                // saving the Sent copy) isn't user-visible, so it can wait for
                // the root viewport's own pace -- `_of(ROOT)`, same reasoning
                // as `session_hooks.repaint` above.
                ctx_clone_smtp.request_repaint_of(egui::ViewportId::ROOT);
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
        let (host_str, port_str, username_str, password_str, smtp_host_str, smtp_port_str, smtp_tls_val) =
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
                        account.smtp_tls,
                    )
                }
                None => (
                    "imap.gmail.com".to_string(),
                    "993".to_string(),
                    String::new(),
                    String::new(),
                    "smtp.gmail.com".to_string(),
                    "465".to_string(),
                    config::TlsMode::Ssl,
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
        let tray = if preview.is_none() {
            match platform::TrayState::new() {
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

        // Connect every saved account whose credential the keyring still has
        // (a password, or a Google refresh token), so all of them are being
        // watched from the moment the app starts. A preview run has no
        // accounts by design; an account with no usable credential is left
        // for the Add account form / Settings, with a note saying why.
        let mut accounts = Vec::new();
        let mut startup_notes = Vec::new();
        if preview.is_none() {
            let now = oauth::now_unix();
            for account in &config.accounts {
                match accounts::saved_auth(&config, account) {
                    Ok(auth) => {
                        accounts.push(spawn_account_view(account, auth, None, &imap_events_tx, &session_hooks));
                        if let Some(note) = auth::oauth_expiry_warning(account, now) {
                            startup_notes.push(note);
                        }
                    }
                    Err(reason) => {
                        log::info!("not connecting {} at startup: {reason}", account.id);
                        if account.auth == config::AuthKind::GoogleOAuth {
                            startup_notes.push(format!("{}: {reason}", account.display_name));
                        }
                    }
                }
            }
        }

        let mut app = Self {
            web_view_host,
            web_view,
            message_view_handler,
            screenshotter: screenshot::Screenshotter::from_env(),
            preview: preview.is_some(),
            accounts,
            active: None,
            imap_events_tx,
            imap_rx,
            session_hooks,
            adding_account: false,
            sending_from: std::collections::HashMap::new(),
            outbox_owner: std::collections::HashMap::new(),
            outbox_in_flight: std::collections::HashSet::new(),
            // Subtracted so the very first `logic()` tick polls right away
            // (e.g. an outbox row left over from a crash mid-send), rather
            // than waiting a full `OUTBOX_POLL_INTERVAL` after launch.
            last_outbox_check: std::time::Instant::now() - OUTBOX_POLL_INTERVAL,
            compose_last_autosave: std::collections::HashMap::new(),
            outbox_window: None,
            drafts_window: None,
            db_tx: db_cmd_tx,
            db_rx: db_evt_rx,
            smtp_tx: smtp_cmd_tx,
            smtp_rx: smtp_evt_rx,
            config,
            host: host_str,
            port: port_str,
            username: username_str,
            password: password_str,
            smtp_host: smtp_host_str,
            smtp_port: smtp_port_str,
            smtp_tls: smtp_tls_val,
            status: initial_status,
            wizard_email: String::new(),
            banners: Vec::new(),
            next_banner_id: 0,
            theme: initial_theme,
            window_geometry: None,
            geometry_saved_on_close: false,
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
            compose_windows: Vec::new(),
            compose_registry,
            next_compose_id: 0,
            window_icon: icons::window_icon().map(|icon| Arc::new(egui::IconData::from(icon))),
            confirm_quit: false,
            next_req_id: 0,
            current_headers_req: 0,
            current_body_req: 0,
            search_query: String::new(),
            search_results: None,
            search_origins: Vec::new(),
            search_all_accounts: false,
            headers_stale: false,
            download_progress: None,
            tray,
            toast_click_rx,
            exit_requested: false,
            use_oauth,
            oauth_tx,
            oauth_rx,
            oauth_tasks: std::collections::HashMap::new(),
            settings: None,
        };
        for note in startup_notes {
            app.push_banner(note);
        }
        app
    }

    /// Route what every account's session produced since the last frame.
    /// Events that change per-account state (mailbox tree, unread counts,
    /// connection state, cache bookkeeping) apply to the account they name;
    /// events about the message list and the reading pane only apply while
    /// that account is the `active` one, since those show one account's
    /// mailbox at a time.
    fn handle_imap_events(&mut self) {
        while let Ok((account, evt)) = self.imap_rx.try_recv() {
            // The account was removed while this was still queued.
            if self.view(&account).is_none() {
                continue;
            }
            let is_active = self.active.as_deref() == Some(account.as_str());
            match evt {
                ImapEvent::Connected => {
                    let from_form = self.view(&account).is_some_and(|v| v.pending_persist.is_some());
                    if let Some(view) = self.view_mut(&account) {
                        view.state = ConnState::Connected;
                    }
                    self.persist_pending(&account);
                    // An account connecting in the background (a saved one
                    // at startup, or a reconnect) must not dismiss the Add
                    // account form someone is typing into; only the account
                    // that form started does.
                    if from_form {
                        self.adding_account = false;
                    }
                    self.status = format!("Connected: {}", self.account_label(&account));
                    self.send_imap_to(&account, ImapCommand::FetchMailboxes);
                    if self.active.is_none() {
                        self.activate(&account, "INBOX".to_string());
                    } else if is_active {
                        self.fetch_headers(self.selected_mailbox.clone(), 1);
                    }
                }
                ImapEvent::Disconnected => {
                    if let Some(view) = self.view_mut(&account) {
                        view.state = ConnState::Disconnected;
                    }
                    if is_active {
                        self.status = "Connection lost, reconnecting...".to_string();
                    }
                }
                ImapEvent::Error(e) => {
                    if let Some(view) = self.view_mut(&account) {
                        // A first connect that failed, or a reconnect that gave
                        // up (a revoked Google sign-in, say): either way the
                        // account is not going to recover by itself.
                        if matches!(view.state, ConnState::Connecting | ConnState::Disconnected) {
                            view.state = ConnState::Failed(e.clone());
                        }
                    }
                    self.push_account_banner(&account, format!("IMAP error: {e}"));
                }
                ImapEvent::Mailboxes(mbs) => {
                    // B8: render as a tree (name split on the server's
                    // delimiter, special-use folders first) instead of a
                    // flat alphabetical list.
                    let names: Vec<String> = mbs.iter().map(|m| m.name.clone()).collect();
                    if let Some(view) = self.view_mut(&account) {
                        view.mailbox_rows = imap::flatten_tree(&imap::mailbox_tree(&mbs));
                    }
                    self.send_imap_to(&account, ImapCommand::FetchUnreadCounts { mailboxes: names });
                }
                ImapEvent::Headers { mailbox, headers, page, total_pages, req_id, mailbox_state } => {
                    // Only the most recently issued FetchHeaders' reply is
                    // applied; an older one arriving late (e.g. the mailbox
                    // was changed again before it came back) is dropped.
                    if is_active && req_id == self.current_headers_req && mailbox == self.selected_mailbox {
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
                        account_id: account,
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
                    if is_active && req_id == self.current_body_req && self.selected_uid == Some(uid) {
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
                    if is_active && req_id == self.current_body_req && self.selected_uid == Some(uid) {
                        let msg = format!("<i>Could not load message: {}</i>", ammonia::clean_text(&error));
                        self.current_message_html = msg.clone();
                        self.web_view.load(WebViewSource::Html(msg));
                    }
                    self.push_account_banner(&account, format!("Could not load message: {error}"));
                }
                ImapEvent::Exported { path } => {
                    self.status = format!("Exported message to {}", path.display());
                }
                ImapEvent::ExportFailed { error } => {
                    self.push_account_banner(&account, format!("Could not export message: {error}"));
                }
                ImapEvent::DownloadProgress { current, total } => {
                    if is_active {
                        self.download_progress = Some((current, total));
                        if current == total {
                            self.download_progress = None;
                            self.status = "Download complete".to_string();
                        }
                    }
                }
                ImapEvent::MailData { mailbox, header, body, attachments } => {
                    let _ = self.db_tx.try_send(DbCommand::IndexMail {
                        account_id: account,
                        mailbox,
                        header,
                        body,
                        attachments,
                    });
                }
                ImapEvent::MailboxPolled { .. } => {
                    // B10's new-mail signal: already consumed by the
                    // account's forwarder (session.rs) before this event
                    // reached the UI channel at all (it decides whether to
                    // poll again / fetch new envelopes / show a toast).
                    // Nothing left here for the UI to do.
                }
                ImapEvent::NewHeaders { .. } => {
                    // Same: the forwarder already turned it into a toast.
                    // What the UI still owes is the unread counts, so an
                    // account that is not on screen (with several accounts
                    // most are not) shows its new mail in the folder pane.
                    let names = self.mailbox_names(&account);
                    if !names.is_empty() {
                        self.send_imap_to(&account, ImapCommand::FetchUnreadCounts { mailboxes: names });
                    }
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
                    self.push_account_banner(&account, format!("Sent, but could not save a copy to {mailbox}: {error}"));
                }
                ImapEvent::HeadersFrom { mailbox, headers } => {
                    // B3: reply to the `FetchHeadersFrom` sent in
                    // `handle_db_events`'s `SyncPlan::FetchFrom`/`Resync`
                    // arm -- index into the cache now that these envelopes
                    // are in hand. See `ImapCommand::FetchHeadersFrom`'s doc
                    // for why this is a separate event from `NewHeaders`
                    // rather than reusing it.
                    let _ = self.db_tx.try_send(DbCommand::IndexHeaders {
                        account_id: account,
                        mailbox,
                        headers,
                    });
                }
                ImapEvent::PollFailed(e) => {
                    // Deliberately not `self.status` -- see the variant's
                    // doc in imap.rs: a background poll failing every 60s
                    // shouldn't overwrite whatever the user is looking at.
                    log::warn!("background new-mail poll for {account} failed: {e}");
                }
                ImapEvent::FlagsUpdated { mailbox, uid, flags, req_id: _ } => {
                    // B8: reflect the server-confirmed flags back into the
                    // visible header list, any active search-results list,
                    // and the local cache. Only touches self.headers/
                    // self.search_results when `mailbox` of the active
                    // account is what's actually on screen -- both lists
                    // only ever hold messages from `self.selected_mailbox`
                    // (search is itself scoped to it, see the `Search`
                    // send-site below), so an event for a different mailbox
                    // (or account) finding a same-numbered UID in either
                    // list would otherwise patch the wrong message's row.
                    // The unread count belongs to the event's own account
                    // and mailbox, and the DB write is keyed by both, so
                    // both are correct regardless of what's displayed.
                    let mut was_seen = None;
                    if is_active && mailbox == self.selected_mailbox {
                        was_seen = self.headers.iter().find(|h| h.uid == uid).map(|h| h.is_seen());
                        if let Some(header) = self.headers.iter_mut().find(|h| h.uid == uid) {
                            header.flags = flags.clone();
                        }
                    }
                    // Search results can come from any account and mailbox, so
                    // they are matched on their own origin rather than on what
                    // is open.
                    if let Some(results) = self.search_results.as_mut() {
                        for (i, header) in results.iter_mut().enumerate() {
                            let origin = self.search_origins.get(i);
                            if header.uid == uid && origin.is_some_and(|(a, m)| *a == account && *m == mailbox) {
                                header.flags = flags.clone();
                            }
                        }
                    }
                    if let Some(was_seen) = was_seen {
                        if let Some(count) = self.view_mut(&account).and_then(|v| v.unread_counts.get_mut(&mailbox)) {
                            let now_seen = flags.iter().any(|f| f.eq_ignore_ascii_case(imap::FLAG_SEEN));
                            if was_seen && !now_seen {
                                *count += 1;
                            } else if !was_seen && now_seen {
                                *count = count.saturating_sub(1);
                            }
                        }
                    }
                    let _ = self.db_tx.try_send(DbCommand::UpdateFlags {
                        account_id: account,
                        mailbox,
                        uid,
                        flags,
                    });
                }
                ImapEvent::FlagsUpdateFailed { mailbox: _, uid, error, req_id: _ } => {
                    self.push_account_banner(&account, format!("Could not update flags on message {uid}: {error}"));
                }
                ImapEvent::Moved { mailbox, uid, dest, req_id: _ } => {
                    // B8: delete-to-Trash/archive succeeded -- drop the
                    // message from the visible list, any active
                    // search-results list, the cache, and any selection it
                    // was part of. See FlagsUpdated above for why the
                    // header/search-results mutations are guarded on this
                    // being the active account's selected mailbox.
                    let on_screen = is_active && mailbox == self.selected_mailbox;
                    let was_unread = on_screen
                        && self.headers.iter().find(|h| h.uid == uid).map(|h| !h.is_seen()).unwrap_or(false);
                    if on_screen {
                        self.headers.retain(|h| h.uid != uid);
                    }
                    if let Some(results) = self.search_results.as_mut() {
                        let origins = std::mem::take(&mut self.search_origins);
                        let (kept, kept_origins): (Vec<_>, Vec<_>) = results
                            .drain(..)
                            .zip(origins)
                            .filter(|(h, (a, m))| !(h.uid == uid && *a == account && *m == mailbox))
                            .unzip();
                        *results = kept;
                        self.search_origins = kept_origins;
                    }
                    if is_active {
                        self.selected_uids.remove(&uid);
                        if self.selected_uid == Some(uid) {
                            self.selected_uid = None;
                            self.web_view.load(WebViewSource::Html("<i>Message moved.</i>".to_string()));
                        }
                        self.status = format!("Moved to {dest}");
                    }
                    if was_unread {
                        if let Some(view) = self.view_mut(&account) {
                            if let Some(count) = view.unread_counts.get_mut(&mailbox) {
                                *count = count.saturating_sub(1);
                            }
                            // The message just landed in `dest` unread --
                            // bump its count too if we're already tracking
                            // it (it may not be yet if FetchUnreadCounts
                            // hasn't completed), so the sidebar doesn't
                            // read "no new mail in Archive/Trash" for a
                            // message that just arrived there.
                            if let Some(count) = view.unread_counts.get_mut(&dest) {
                                *count += 1;
                            }
                        }
                    }
                    let _ = self.db_tx.try_send(DbCommand::RemoveMessage {
                        account_id: account,
                        mailbox,
                        uid,
                    });
                }
                ImapEvent::MoveFailed { mailbox: _, uid, error, req_id: _ } => {
                    self.push_account_banner(&account, format!("Could not move message {uid}: {error}"));
                }
                ImapEvent::UnreadCounts(counts) => {
                    if let Some(view) = self.view_mut(&account) {
                        view.unread_counts = counts;
                    }
                }
            }
        }
    }

    fn handle_db_events(&mut self) {
        while let Ok(evt) = self.db_rx.try_recv() {
            match evt {
                DbEvent::SearchResult { hits } => {
                    self.search_origins = hits.iter().map(|h| (h.account_id.clone(), h.mailbox.clone())).collect();
                    self.search_results = Some(hits.into_iter().map(|h| h.header).collect());
                }
                DbEvent::MailFetched { header, body, attachments } => {
                    if self.selected_uid == Some(header.uid) {
                        self.current_message_html = body.clone();
                        self.web_view.load(WebViewSource::Html(body));
                        self.current_attachments = attachments;
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
                    // Routed to the account the plan was made for, which is
                    // not necessarily the active one: a header fetch for a
                    // different account may still be answering.
                    match plan {
                        db::SyncPlan::UpToDate => {}
                        db::SyncPlan::FetchFrom { first_new_uid } => {
                            self.send_imap_to(&account_id, ImapCommand::FetchHeadersFrom { mailbox, first_uid: first_new_uid });
                        }
                        db::SyncPlan::Resync => {
                            self.send_imap_to(&account_id, ImapCommand::FetchHeadersFrom { mailbox, first_uid: 1 });
                        }
                    }
                }
                DbEvent::OutboxEnqueued { id, compose_id } => {
                    self.outbox_owner.insert(compose_id, id);
                }
                DbEvent::OutboxDue { items } => {
                    for item in items {
                        // Skip a row a still-open window owns -- the user
                        // could click Send on it at any moment, and that
                        // must not race a background attempt at the same
                        // row. It becomes eligible again once that window
                        // closes (or, if it isn't owned by any window,
                        // right away).
                        let owned_by_open_window = self
                            .outbox_owner
                            .iter()
                            .any(|(cid, rid)| *rid == item.id && self.compose_windows.iter().any(|w| w.id() == *cid));
                        if owned_by_open_window || self.outbox_in_flight.contains(&item.id) {
                            continue;
                        }
                        self.next_compose_id += 1;
                        let synthetic_id = self.next_compose_id;
                        match self.smtp_account_for(&item.account_id) {
                            Some(account) => {
                                self.outbox_in_flight.insert(item.id);
                                self.outbox_owner.insert(synthetic_id, item.id);
                                self.sending_from.insert(synthetic_id, item.account_id.clone());
                                let _ = self.smtp_tx.try_send(smtp::SmtpCommand::Send { id: synthetic_id, account, compose: item.compose });
                            }
                            None => {
                                // No SMTP credential on file for this account
                                // (removed, renamed, or never connected) --
                                // back it off like any other failure rather
                                // than retrying every single poll forever.
                                let _ = self.db_tx.try_send(DbCommand::MarkOutboxFailed {
                                    id: item.id,
                                    error: "No SMTP password on file for this account".to_string(),
                                });
                            }
                        }
                    }
                }
                DbEvent::OutboxList { items } => {
                    self.outbox_window = Some(items);
                }
                DbEvent::DraftSaved { id, compose_id } => {
                    if let Some(window) = self.compose_windows.iter().find(|w| w.id() == compose_id) {
                        window.set_draft_id(id);
                    }
                }
                DbEvent::DraftList { items } => {
                    self.drafts_window = Some(items);
                }
                DbEvent::DraftLoaded { id, mut compose } => {
                    compose.draft_id = Some(id);
                    self.open_compose(compose, compose_window::Focus::Body);
                    self.drafts_window = None;
                }
                DbEvent::Error(e) => {
                    self.push_banner(format!("Database error: {e}"));
                }
            }
        }
    }

    /// Autosaves every open compose window that's due and has something
    /// worth keeping (a blank, just-opened window has nothing to save yet).
    /// Called from `logic()`, so it keeps working while the main window is
    /// hidden.
    fn autosave_drafts(&mut self) {
        let now = std::time::Instant::now();
        for window in &self.compose_windows {
            let id = window.id();
            let due = self.compose_last_autosave.get(&id).is_none_or(|t| now.duration_since(*t) >= DRAFT_AUTOSAVE_INTERVAL);
            if !due {
                continue;
            }
            self.compose_last_autosave.insert(id, now);
            let compose = window.snapshot();
            let has_content = !compose.to.is_empty()
                || !compose.cc.is_empty()
                || !compose.bcc.is_empty()
                || !compose.subject.is_empty()
                || !compose.body.is_empty();
            if !has_content {
                continue;
            }
            let _ = self.db_tx.try_send(DbCommand::SaveDraft {
                id: compose.draft_id,
                compose_id: id,
                account_id: compose.account_id.clone(),
                compose,
            });
        }
    }

    /// Ask the db for outbox rows due for a (re)send, at most once every
    /// `OUTBOX_POLL_INTERVAL` -- called from `logic()`, which keeps ticking
    /// (via `handle_tray`'s repaint request) even while the window is
    /// hidden, so a queued retry still goes out while minimized to the tray.
    fn poll_outbox(&mut self) {
        let now = std::time::Instant::now();
        if now.duration_since(self.last_outbox_check) < OUTBOX_POLL_INTERVAL {
            return;
        }
        self.last_outbox_check = now;
        let _ = self.db_tx.try_send(DbCommand::DueOutbox);
    }

    fn handle_smtp_events(&mut self, ctx: &egui::Context) {
        while let Ok(evt) = self.smtp_rx.try_recv() {
            match evt {
                smtp::SmtpEvent::Sent { id, raw } => {
                    // A message that had failed at least once (and so
                    // picked up an autosaved draft along the way) is done
                    // being a draft now that it's actually gone out.
                    if let Some(window) = self.compose_windows.iter().find(|w| w.id() == id) {
                        if let Some(draft_id) = window.snapshot().draft_id {
                            let _ = self.db_tx.try_send(DbCommand::DeleteDraft { id: draft_id });
                        }
                    }
                    // Only the window that sent it closes; a failure (the
                    // Error arm below) leaves its window open with the typed
                    // text intact instead, so nothing is lost.
                    self.close_compose(ctx, id);
                    // This id had already failed and durably recorded
                    // itself in the outbox at least once (see the `Error`
                    // arm) -- that attempt just succeeded, so the row is
                    // done.
                    if let Some(outbox_id) = self.outbox_owner.remove(&id) {
                        let _ = self.db_tx.try_send(DbCommand::MarkOutboxSent { id: outbox_id });
                        self.outbox_in_flight.remove(&outbox_id);
                    }
                    self.status = "Message sent".to_string();
                    // B7: save a copy to Sent, the way every other mail
                    // client does (SMTP itself doesn't). Best-effort -- a
                    // failure here only logs (via the generic
                    // ImapEvent::Error path), it doesn't imply the send
                    // itself failed, since it didn't. The copy goes to the
                    // account the message was sent *from*, into that
                    // account's own Sent folder (special-use discovery is
                    // per account).
                    if let Some(account) = self.sending_from.remove(&id) {
                        let mailbox = self.special_use_mailbox_for(&account, imap::SpecialUse::Sent, SENT_MAILBOX);
                        self.send_imap_to(&account, ImapCommand::Append { mailbox, raw });
                    }
                }
                smtp::SmtpEvent::Error { id, error } => {
                    self.sending_from.remove(&id);
                    // Durable retry: an id that already owns an outbox row
                    // (a retry, background or manual, failing again) just
                    // gets backed off further -- its content is already
                    // saved. A first-ever failure creates that row, using
                    // the still-open window to recover the message's
                    // content (there is no other copy of it by this point).
                    // A first failure with no window left (discarded while
                    // this send was still in flight) has nothing to recover
                    // it from and is not retried -- a narrow, accepted gap
                    // alongside the crash-mid-send one; see smtp.rs's module
                    // docs.
                    match self.outbox_owner.get(&id).copied() {
                        Some(outbox_id) => {
                            let _ = self.db_tx.try_send(DbCommand::MarkOutboxFailed { id: outbox_id, error: error.clone() });
                            self.outbox_in_flight.remove(&outbox_id);
                        }
                        None => {
                            if let Some(window) = self.compose_windows.iter().find(|w| w.id() == id) {
                                if let Some(account_id) = window.account_id() {
                                    let _ = self.db_tx.try_send(DbCommand::EnqueueOutbox {
                                        id: None,
                                        compose_id: id,
                                        account_id,
                                        compose: window.snapshot(),
                                    });
                                }
                            }
                        }
                    }
                    let message = format!("Send failed: {error}");
                    // Normally a no-op: the SMTP forwarder's background
                    // thread already called `set_error_and_wake` on this same
                    // window the moment the event arrived (see `main()`'s
                    // `compose_registry_smtp` block) -- this is just the
                    // fallback for the window having been closed meanwhile,
                    // which leaves nobody to show it to but the main window.
                    match self.compose_windows.iter().find(|w| w.id() == id) {
                        Some(window) => window.set_error_and_wake(ctx, message),
                        None => self.push_banner(message),
                    }
                }
            }
        }
    }

    /// Opens a compose window for `state`. Its viewport is created by the
    /// next [`Self::show_compose_windows`].
    fn open_compose(&mut self, state: ComposeState, focus: compose_window::Focus) {
        self.next_compose_id += 1;
        let window = ComposeWindow::new(self.next_compose_id, state, focus);
        // Kept in `compose_registry` too -- see `compose_window.rs`'s module
        // docs -- for as long as this window is open.
        self.compose_registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(window.id(), window.clone());
        self.compose_windows.push(window);
    }

    /// Drops the window, which closes its OS window at the next frame of the
    /// main window. That frame never comes while the main window is hidden,
    /// so the window is also hidden on the spot -- and, for a send that
    /// completed, it is normally already hidden by the time this runs at
    /// all; see `compose_window.rs`'s module docs.
    fn close_compose(&mut self, ctx: &egui::Context, id: ComposeId) {
        if let Some(pos) = self.compose_windows.iter().position(|w| w.id() == id) {
            let window = self.compose_windows.remove(pos);
            self.compose_registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&id);
            ctx.send_viewport_cmd_to(window.viewport_id(), egui::ViewportCommand::Visible(false));
            // `_of(ROOT)`: dropping the window from `compose_windows` only
            // actually closes it once the root viewport runs another `ui()`
            // pass (the one that stops calling `show_compose_windows` for
            // this id) -- see the heartbeat thread's note in `new()` for why
            // that isn't always prompt on Windows, though this particular
            // repaint is cosmetic bookkeeping now, not what makes the window
            // disappear.
            ctx.request_repaint_of(egui::ViewportId::ROOT);
        }
    }

    /// What the compose windows ask for, run from `logic()` so it keeps
    /// working while the main window is hidden in the tray: send the messages
    /// whose Send was clicked and drop the windows that are finished.
    fn process_compose_windows(&mut self, ctx: &egui::Context) {
        let finished: Vec<ComposeId> =
            self.compose_windows.iter().filter(|w| w.is_finished()).map(ComposeWindow::id).collect();
        for id in finished {
            self.close_compose(ctx, id);
        }

        let mut requests = Vec::new();
        for window in &self.compose_windows {
            if let Some(compose) = window.take_send_request() {
                requests.push((window.id(), window.account_id(), compose));
            }
        }
        for (id, from, compose) in requests {
            let Some(window) = self.compose_windows.iter().find(|w| w.id() == id) else { continue };
            let error = match from.as_deref().map(|from| (from, self.smtp_account_for(from))) {
                Some((from, Some(account))) => {
                    // Remembered so `Sent` saves the copy to this account's
                    // Sent folder -- see `sending_from`.
                    self.sending_from.insert(id, from.to_string());
                    if self.smtp_tx.try_send(smtp::SmtpCommand::Send { id, account, compose }).is_ok() {
                        None
                    } else {
                        self.sending_from.remove(&id);
                        Some("Could not queue the message for sending.".to_string())
                    }
                }
                None => Some("Choose an account to send from.".to_string()),
                Some((_, None)) => Some("No SMTP password on file yet — connect once via IMAP first.".to_string()),
            };
            match error {
                None => window.set_sending(true),
                Some(error) => window.set_error(error),
            }
            ctx.request_repaint_of(window.viewport_id());
        }
    }

    /// Declares every compose window to egui; called each frame the main
    /// window is drawn (see `ComposeWindow::show`).
    fn show_compose_windows(&self, ctx: &egui::Context) {
        let accounts: Vec<(String, String)> =
            self.accounts.iter().map(|v| (v.id().to_string(), v.label().to_string())).collect();
        for window in &self.compose_windows {
            window.show(ctx, accounts.clone(), self.window_icon.clone());
        }
    }

    /// The "unsent messages" question raised by [`Self::request_quit`].
    fn show_quit_confirmation(&mut self, ctx: &egui::Context) {
        if !self.confirm_quit {
            return;
        }
        if !self.has_unsent_compose() {
            // Sent or discarded while the question was up.
            self.confirm_quit = false;
            return;
        }
        let mut quit = false;
        let mut keep = false;
        egui::Window::new("Quit esMail?").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
            ui.label("Some messages have not been sent. Quitting discards them.");
            ui.horizontal(|ui| {
                quit = ui.button("Quit anyway").clicked();
                keep = ui.button("Keep them open").clicked();
            });
        });
        if quit {
            self.confirm_quit = false;
            self.exit_requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else if keep {
            self.confirm_quit = false;
        }
    }

    /// Whether any compose window holds text that closing the app would lose.
    fn has_unsent_compose(&self) -> bool {
        self.compose_windows.iter().any(ComposeWindow::is_dirty)
    }

    /// Quit: at once, or -- when a compose window holds unsent text -- after
    /// asking in the main window.
    fn request_quit(&mut self, ctx: &egui::Context) {
        if self.has_unsent_compose() {
            self.confirm_quit = true;
            ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Focus);
        } else {
            self.exit_requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
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

    /// [`Self::push_banner`] for an error that belongs to one account. Names
    /// the account once there is more than one, so "IMAP error: login
    /// failed" says which of them; with a single account the text is what it
    /// always was.
    fn push_account_banner(&mut self, account: &str, message: String) {
        if self.accounts.len() > 1 {
            let label = self.account_label(account);
            self.push_banner(format!("{label}: {message}"));
        } else {
            self.push_banner(message);
        }
    }

    fn view(&self, account: &str) -> Option<&AccountView> {
        self.accounts.iter().find(|v| v.id() == account)
    }

    fn view_mut(&mut self, account: &str) -> Option<&mut AccountView> {
        self.accounts.iter_mut().find(|v| v.id() == account)
    }

    /// The account's display name, or its id if it is not (or no longer) a
    /// live session.
    fn account_label(&self, account: &str) -> String {
        self.view(account).map_or_else(|| account.to_string(), |v| v.label().to_string())
    }

    /// Every selectable mailbox of `account` (the ones `FetchUnreadCounts`
    /// can `STATUS`), from its current mailbox tree.
    fn mailbox_names(&self, account: &str) -> Vec<String> {
        self.view(account)
            .map(|v| v.mailbox_rows.iter().filter_map(|r| r.full_name.clone()).collect())
            .unwrap_or_default()
    }

    /// Send a command to one account's actor. Dropped if that account has
    /// been removed -- there is nothing left to answer it.
    fn send_imap_to(&self, account: &str, cmd: ImapCommand) {
        if let Some(view) = self.view(account) {
            let _ = view.session.imap_tx().try_send(cmd);
        }
    }

    /// Send a command to the active account's actor (a no-op with no active
    /// account). Everything the message list and reading pane do goes
    /// through here, since they only ever show the active account.
    fn send_imap(&self, cmd: ImapCommand) {
        if let Some(account) = &self.active {
            self.send_imap_to(account, cmd);
        }
    }

    /// Make `account` the one the message list and reading pane show, open
    /// `mailbox` in it, and start fetching its first page. Everything that
    /// belonged to the previous account's list -- selection, search results,
    /// the open message -- is dropped, since none of it is meaningful in the
    /// new one.
    fn activate(&mut self, account: &str, mailbox: String) {
        let switching = self.active.as_deref() != Some(account);
        self.active = Some(account.to_string());
        self.headers_stale = false;
        if switching {
            self.search_query.clear();
            self.search_results = None;
            self.search_origins.clear();
            self.headers.clear();
            self.web_view.load(WebViewSource::Html(String::new()));
            self.current_message_html.clear();
            self.current_attachments.clear();
        }
        self.selected_mailbox = mailbox.clone();
        self.selected_uid = None;
        self.selected_uids.clear();
        self.select_anchor = None;
        self.pending_mark_seen = None;
        self.current_page = 1;
        self.total_pages = 1;
        self.fetch_headers(mailbox, 1);
    }

    /// Real Logout / Remove account: drop the account's session, which stops
    /// its actor, its body worker and its `IDLE` watch (see
    /// `AccountSession`'s `Drop`), and leaves every other account alone. If
    /// it was the active one, the next remaining connected account takes
    /// over.
    fn disconnect_account(&mut self, account: &str) {
        let label = self.account_label(account);
        self.accounts.retain(|v| v.id() != account);
        // "All accounts" only exists as a choice with more than one.
        if self.accounts.len() < 2 {
            self.search_all_accounts = false;
        }
        if self.active.as_deref() == Some(account) {
            self.active = None;
            self.headers.clear();
            self.search_results = None;
            self.search_origins.clear();
            self.headers_stale = false;
            self.selected_uid = None;
            self.selected_uids.clear();
            self.select_anchor = None;
            self.pending_mark_seen = None;
            self.current_message_html.clear();
            self.current_attachments.clear();
            self.web_view.load(WebViewSource::Html(String::new()));
            let next = self
                .accounts
                .iter()
                .find(|v| v.state == ConnState::Connected)
                .map(|v| v.id().to_string());
            if let Some(next) = next {
                self.activate(&next, "INBOX".to_string());
            }
        }
        self.status = format!("Logged out of {label}");
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

    /// The id `db.rs` keys the active account's cache on -- `AccountConfig::
    /// id`, the same `username@host` string the keyring uses -- or `None`
    /// before any account is active. Every DB command names its account
    /// explicitly; this is only for the ones the UI itself issues on behalf
    /// of the message list.
    fn active_account_id(&self) -> Option<AccountId> {
        self.active.clone()
    }

    /// The active account's login name, which doubles as its address --
    /// empty before any account is active.
    fn active_username(&self) -> String {
        self.active
            .as_deref()
            .and_then(|id| self.config.accounts.iter().find(|a| a.id == id))
            .map(|a| a.username.clone())
            .unwrap_or_default()
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
        self.send_imap(ImapCommand::FetchHeaders { mailbox, page, req_id });
    }

    /// Send `FetchBody`, recording its request id the same way `fetch_headers` does.
    fn fetch_body(&mut self, mailbox: String, uid: u32) {
        let req_id = self.next_req_id();
        self.current_body_req = req_id;
        self.send_imap(ImapCommand::FetchBody { mailbox, uid, req_id });
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
        self.send_imap(ImapCommand::ExportMessage {
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
            if let Some(account_id) = self.active_account_id() {
                let _ = self.db_tx.try_send(DbCommand::FetchMail {
                    account_id,
                    mailbox: self.selected_mailbox.clone(),
                    uid,
                });
            }
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
            self.send_imap(ImapCommand::StoreFlags {
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
        match &self.search_results {
            Some(results) => results
                .iter()
                .enumerate()
                .any(|(i, h)| h.uid == uid && h.is_flagged() && self.in_open_context(i)),
            None => self.headers.iter().any(|h| h.uid == uid && h.is_flagged()),
        }
    }

    /// Whether search result `i` lives in the mailbox currently open (the
    /// `active` account's `selected_mailbox`) -- the context UID-based
    /// actions and the reading pane refer to.
    fn in_open_context(&self, i: usize) -> bool {
        match (&self.active, self.search_origins.get(i)) {
            (Some(active), Some((account, mailbox))) => account == active && *mailbox == self.selected_mailbox,
            _ => false,
        }
    }

    /// The header of the open message, from the search results when one of
    /// them is open (it may be in a mailbox `headers` does not hold), else
    /// from the message list.
    fn selected_header(&self) -> Option<MailHeader> {
        let uid = self.selected_uid?;
        if let Some(results) = &self.search_results {
            if let Some(h) = results.iter().enumerate().find(|(i, h)| h.uid == uid && self.in_open_context(*i)).map(|(_, h)| h) {
                return Some(h.clone());
            }
        }
        self.headers.iter().find(|h| h.uid == uid).cloned()
    }

    /// Open search result `i`: move to its account and mailbox first if it
    /// lives elsewhere, then open it from the cache like any search result.
    fn open_search_hit(&mut self, i: usize) {
        let (Some(header), Some((account, mailbox))) =
            (self.search_results.as_ref().and_then(|r| r.get(i)), self.search_origins.get(i).cloned())
        else {
            return;
        };
        let uid = header.uid;
        // A hit can come from an account that is saved but not connected. Moving
        // the reading pane there would leave every action (flags, move, reply)
        // with no session to run on, so ask for the connection instead.
        if self.view(&account).is_none() {
            let label = self.account_label(&account);
            self.push_banner(format!("{label} is not connected. Connect it under Settings > Accounts to open this message."));
            return;
        }
        if self.active.as_deref() != Some(account.as_str()) || self.selected_mailbox != mailbox {
            // The message list still holds the previous mailbox's page;
            // `clear_search` reloads it.
            self.headers_stale = true;
            self.active = Some(account);
            self.selected_mailbox = mailbox;
        }
        self.open_message(uid, true);
    }

    /// Leave search: drop the results, and reload the message list if
    /// opening a hit moved it to another mailbox meanwhile.
    fn clear_search(&mut self) {
        self.search_results = None;
        self.search_origins.clear();
        if std::mem::take(&mut self.headers_stale) {
            self.fetch_headers(self.selected_mailbox.clone(), 1);
        }
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
            self.send_imap(ImapCommand::StoreFlags {
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
            self.send_imap(ImapCommand::MoveMessage {
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
    ///
    /// Discovery is per account -- each account has its own mailbox tree --
    /// so this asks about `account`, not "the" account.
    fn special_use_mailbox_for(&self, account: &str, want: imap::SpecialUse, default: &str) -> String {
        let rows = self.view(account).map_or(&[][..], |v| v.mailbox_rows.as_slice());
        find_special_use_mailbox(rows, want, default)
    }

    /// [`Self::special_use_mailbox_for`] for the active account.
    fn special_use_mailbox(&self, want: imap::SpecialUse, default: &str) -> String {
        match &self.active {
            Some(account) => self.special_use_mailbox_for(account, want, default),
            None => default.to_string(),
        }
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
        self.send_imap(ImapCommand::StoreFlags {
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
    /// search), `Ctrl+N` (compose). Disabled while the search box
    /// has focus (so typing "j"/"f"/etc. into a search query doesn't also
    /// fire a shortcut). Compose windows are separate native windows with
    /// their own input, so they need no special-casing here.
    fn handle_keyboard_shortcuts(&mut self, ui: &mut egui::Ui) {
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
            self.open_compose(ComposeState::default().with_account(self.active_account_id()), compose_window::Focus::To);
        }
        if next || prev {
            let is_search = self.search_results.is_some();
            let list = self.search_results.as_ref().unwrap_or(&self.headers);
            if !list.is_empty() {
                // In search results the open message is found by UID within
                // the open mailbox, since UIDs repeat across accounts.
                let idx = self.selected_uid.and_then(|uid| {
                    list.iter().enumerate().position(|(i, h)| h.uid == uid && (!is_search || self.in_open_context(i)))
                });
                let new_idx = match idx {
                    Some(i) if next => (i + 1).min(list.len() - 1),
                    Some(i) => i.saturating_sub(1), // prev
                    None => 0,
                };
                let uid = list[new_idx].uid;
                self.selected_uids.clear();
                self.select_anchor = Some(uid);
                if is_search {
                    self.open_search_hit(new_idx);
                } else {
                    self.open_message(uid, false);
                }
            }
        }
        if enter {
            if let Some(uid) = self.selected_uid {
                let is_search = self.search_results.is_some();
                self.open_message(uid, is_search);
            }
        }
        if reply {
            if let Some(header) = self.selected_header() {
                self.open_compose(
                    ComposeState::reply(&header, &self.current_message_html).with_account(self.active_account_id()),
                    compose_window::Focus::Body,
                );
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
    /// The Drafts window: every autosaved/explicitly-saved draft, click to
    /// reopen it in Compose (which removes it from this list -- the window
    /// carries the same `draft_id` forward, so autosave from then on
    /// overwrites the same row rather than creating a second one).
    fn show_drafts_window(&mut self, ctx: &egui::Context) {
        let Some(drafts) = &self.drafts_window else { return };

        let mut open = true;
        let mut load_clicked = None;
        let mut delete_clicked = None;
        egui::Window::new("Drafts").open(&mut open).default_size([420.0, 320.0]).show(ctx, |ui| {
            if drafts.is_empty() {
                ui.weak("No saved drafts.");
            }
            egui::ScrollArea::vertical().show(ui, |ui| {
                for draft in drafts {
                    ui.horizontal(|ui| {
                        let to = if draft.to.is_empty() { "(no recipient)" } else { &draft.to };
                        let subject = if draft.subject.is_empty() { "(no subject)" } else { &draft.subject };
                        if ui.link(format!("{subject} — {to}")).clicked() {
                            load_clicked = Some(draft.id);
                        }
                        if ui.small_button("Delete").clicked() {
                            delete_clicked = Some(draft.id);
                        }
                    });
                }
            });
        });

        if let Some(id) = load_clicked {
            let _ = self.db_tx.try_send(DbCommand::LoadDraft { id });
        }
        if let Some(id) = delete_clicked {
            let _ = self.db_tx.try_send(DbCommand::DeleteDraft { id });
            if let Some(drafts) = &mut self.drafts_window {
                drafts.retain(|d| d.id != id);
            }
        }
        if !open {
            self.drafts_window = None;
        }
    }

    /// The Outbox window: every message still queued to send (pending, or
    /// retrying after a failure with `last_error`/`attempts` to show why).
    /// "Edit" pulls it back into Compose to fix and resend -- removing it
    /// from the outbox first, so re-sending can't double up with the
    /// background retry still trying the old copy.
    fn show_outbox_window(&mut self, ctx: &egui::Context) {
        let Some(items) = &self.outbox_window else { return };

        let mut open = true;
        let mut edit_clicked = None;
        let mut delete_clicked = None;
        egui::Window::new("Outbox").open(&mut open).default_size([460.0, 320.0]).show(ctx, |ui| {
            if items.is_empty() {
                ui.weak("Nothing queued to send.");
            }
            egui::ScrollArea::vertical().show(ui, |ui| {
                for item in items {
                    ui.horizontal(|ui| {
                        let to = if item.compose.to.is_empty() { "(no recipient)" } else { &item.compose.to };
                        let subject = if item.compose.subject.is_empty() { "(no subject)" } else { &item.compose.subject };
                        ui.label(format!("{subject} — {to}"));
                        if item.attempts > 0 {
                            let detail = match &item.last_error {
                                Some(e) => format!("retried {} time(s): {e}", item.attempts),
                                None => format!("retried {} time(s)", item.attempts),
                            };
                            ui.label(egui::RichText::new(detail).color(egui::Color32::RED).small());
                        }
                        if ui.small_button("Edit").clicked() {
                            edit_clicked = Some(item.id);
                        }
                        if ui.small_button("Delete").clicked() {
                            delete_clicked = Some(item.id);
                        }
                    });
                }
            });
        });

        if let Some(id) = edit_clicked {
            if let Some(items) = &mut self.outbox_window {
                if let Some(pos) = items.iter().position(|i| i.id == id) {
                    let item = items.remove(pos);
                    let _ = self.db_tx.try_send(DbCommand::DeleteOutbox { id });
                    self.open_compose(item.compose.with_account(Some(item.account_id)), compose_window::Focus::Body);
                }
            }
        }
        if let Some(id) = delete_clicked {
            let _ = self.db_tx.try_send(DbCommand::DeleteOutbox { id });
            if let Some(items) = &mut self.outbox_window {
                items.retain(|i| i.id != id);
            }
        }
        if !open {
            self.outbox_window = None;
        }
    }
}

/// Tray icon polling + minimize-to-tray (B10), and clicks on new-mail toasts.
/// Kept in its own `impl` block, called only from `EsMailApp::logic`. On a
/// platform without a tray `self.tray` is `None` and this does nothing.
impl EsMailApp {
    fn handle_tray(&mut self, ctx: &egui::Context) {
        // A later launch of esMail left a request for this one: come forward
        // (an ordinary second launch) or exit (`esmail --quit`, used by the
        // installer). Polled, since nothing wakes a hidden window for it.
        match shell::take_request() {
            Some(shell::Request::Show) => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
            Some(shell::Request::Quit) => {
                self.exit_requested = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            None => {}
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(250));
        // A clicked toast: open the account the mail arrived in, at the mailbox
        // being watched, so the new message is right there.
        while let Ok(account) = self.toast_click_rx.try_recv() {
            if self.view(&account).is_some() {
                let mailbox = self
                    .config
                    .accounts
                    .iter()
                    .find(|a| a.id == account)
                    .and_then(|a| a.watch_mailbox.clone())
                    .unwrap_or_else(|| session::DEFAULT_WATCH_MAILBOX.to_string());
                self.adding_account = false;
                self.activate(&account, mailbox);
            }
        }
        // The tooltip carries the unread total over all accounts.
        let unread: u32 = self.accounts.iter().map(AccountView::total_unread).sum();
        // Without a tray a close request really closes -- unless a compose
        // window holds unsent text, which is asked about first.
        if self.tray.is_none()
            && !self.exit_requested
            && ctx.input(|i| i.viewport().close_requested())
            && self.has_unsent_compose()
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.request_quit(ctx);
        }
        let Some(tray) = &mut self.tray else { return };
        tray.set_unread(unread);
        tray.refresh_icon();

        for action in tray.poll_actions() {
            match action {
                platform::TrayAction::Show => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                platform::TrayAction::Quit => {
                    // Hidden windows don't organically generate another
                    // close-request -- nothing is clicking their (invisible)
                    // close button -- so `request_quit` asks for one
                    // explicitly. The check below sees `exit_requested` and
                    // lets it through rather than redirecting it to "hide to
                    // tray" again. (Unsent compose windows are asked about
                    // first.)
                    self.request_quit(ctx);
                }
            }
        }

        // The redirect: a first close-request (the user clicked the main
        // window's own close button -- this only ever sees the root viewport's
        // input, so a compose window's close never lands here) is canceled and
        // turned into "hide instead",
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
    /// need to be here -- see `session::AccountSession`, whose forwarder is a plain tokio task
    /// that runs independent of both `logic()` and `ui()`.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // The compose windows' sends and results are handled here rather than
        // in `ui()`, so they keep working while the main window is hidden.
        self.handle_smtp_events(ctx);
        self.process_compose_windows(ctx);
        self.handle_tray(ctx);
        self.autosave_drafts();
        self.poll_outbox();
        // Draining this here too (`ui()` also does, whenever it runs) is what
        // makes a queued retry (`DbEvent::OutboxDue`) actually go out while
        // the window is hidden -- `ui()` is skipped entirely while hidden, so
        // without this a poll's reply would just sit in `db_rx` until the
        // window is shown again. Idempotent (`try_recv` on an
        // already-drained channel is just a no-op), so running it again in
        // `ui()` on a visible frame costs nothing.
        self.handle_db_events();
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
        // With a tray, a close request only hides the window unless Quit was
        // chosen; without one (no tray on this platform, or it could not be
        // created) a close request really closes.
        let closing = ui.ctx().input(|i| i.viewport().close_requested())
            && (self.exit_requested || (self.tray.is_none() && !self.has_unsent_compose()));
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

        // The main (folder pane + message list) view is shown as soon as
        // there is any account at all -- the login form is no longer a gate
        // -- unless the Add account form has been asked for.
        let main_view = !self.accounts.is_empty() && !self.adding_account;

        if main_view {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Add account…").clicked() {
                        self.adding_account = true;
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Download All (This Mailbox)").clicked() {
                        self.send_imap(ImapCommand::BulkDownload { mailbox: self.selected_mailbox.clone() });
                        ui.close();
                    }
                    ui.separator();
                    // Logs out of the *active* account only; every other
                    // account stays connected and watched. Also stops this
                    // account's IDLE watch (see `disconnect_account`).
                    let active_label = self.active.as_deref().map(|id| self.account_label(id));
                    let logout = match &active_label {
                        Some(label) => format!("Logout {label}"),
                        None => "Logout".to_string(),
                    };
                    if ui.add_enabled(self.active.is_some(), egui::Button::new(logout)).clicked() {
                        if let Some(id) = self.active.clone() {
                            self.disconnect_account(&id);
                        }
                        ui.close();
                    }
                });
            });
        }

        egui::Panel::top("top_panel").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("esMail");
                ui.separator();
                
                if main_view && self.active.is_some() {
                    ui.label("Search:");
                    // A fixed id (rather than the auto-generated one) so
                    // Ctrl+F (B8) can `request_focus` it from outside this
                    // closure, where `self.search_query`'s borrow isn't
                    // available to re-add the same widget.
                    let search_resp = ui.add(egui::TextEdit::singleline(&mut self.search_query).id_salt("search_box").hint_text("Enter keywords..."));
                    self.search_box_id = Some(search_resp.id);
                    // Only worth offering once there is more than one account
                    // to choose between.
                    let scope_changed = self.accounts.len() > 1
                        && ui.checkbox(&mut self.search_all_accounts, "All accounts").changed();
                    if search_resp.changed() || scope_changed || (search_resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) {
                        // `from:`/`to:`/`subject:`/`body:` and bare text all
                        // become an FTS5 MATCH expression; `since:`/`before:`/
                        // `is:unread`/`has:attachment` parse but aren't
                        // applied yet (see search_query.rs) — a query made
                        // only of those is treated the same as an empty one.
                        match ParsedQuery::parse(&self.search_query).to_fts_match() {
                            // Scoped to the active account's selected mailbox, or
                            // -- with "All accounts" -- every mailbox of every
                            // account (the FTS index is keyed per account, so this
                            // is one query).
                            Some(fts_query) => {
                                let (account_id, mailbox) = if self.search_all_accounts {
                                    (None, None)
                                } else {
                                    (self.active_account_id(), Some(self.selected_mailbox.clone()))
                                };
                                if self.search_all_accounts || account_id.is_some() {
                                    let _ = self.db_tx.try_send(DbCommand::Search {
                                        account_id,
                                        query: fts_query,
                                        mailbox,
                                    });
                                }
                            }
                            None => {
                                self.clear_search();
                            }
                        }
                    }
                    if ui.button("Clear").clicked() {
                        self.search_query.clear();
                        self.clear_search();
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
                        self.open_settings(settings::Tab::General);
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

        if main_view && self.active.is_some() {
            self.handle_mark_seen_delay();
            self.handle_keyboard_shortcuts(ui);
        }

        if !main_view {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.group(|ui| {
                        ui.set_width(300.0);
                        ui.heading(if self.accounts.is_empty() { "Login" } else { "Add account" });

                        if !self.config.accounts.is_empty() {
                            ui.label("Saved accounts:");
                            let mut to_remove = None;
                            for account in self.config.accounts.clone() {
                                ui.horizontal(|ui| {
                                    let connected = self.view(&account.id).is_some();
                                    let name = if connected {
                                        format!("{} (connected)", account.display_name)
                                    } else {
                                        account.display_name.clone()
                                    };
                                    if ui.button(name).clicked() {
                                        self.fill_form_from(&account);
                                    }
                                    if ui.small_button("x").on_hover_text("Forget this account").clicked() {
                                        to_remove = Some(account.id.clone());
                                    }
                                });
                            }
                            if let Some(id) = to_remove {
                                self.remove_account(&id);
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
                        egui::ComboBox::from_id_salt("wizard_smtp_tls")
                            .selected_text(settings::tls_label(self.smtp_tls))
                            .show_ui(ui, |ui| {
                                for tls in [config::TlsMode::Ssl, config::TlsMode::StartTls, config::TlsMode::None] {
                                    ui.selectable_value(&mut self.smtp_tls, tls, settings::tls_label(tls));
                                }
                            });

                        let form_id = self.account_from_form().id;
                        if self.oauth_tasks.contains_key(&form_id) {
                            ui.label("Finish signing in with Google in your browser...");
                            if ui.button("Cancel sign-in").clicked() {
                                self.cancel_google_sign_in(&form_id);
                            }
                        } else {
                            ui.horizontal(|ui| {
                                if ui.button("Connect").clicked() {
                                    // Starts a session for this account next
                                    // to any already open: its own actor, IDLE
                                    // watch and watermark (see session.rs).
                                    // Connecting an id that is already open
                                    // replaces its session, so a retry after a
                                    // typo'd password cannot leak a second
                                    // watcher.
                                    self.connect_clicked(ui.ctx());
                                }
                                if oauth_active
                                    && ui
                                        .button("Sign in again")
                                        .on_hover_text("Go through Google's consent page again, even if this account was approved before")
                                        .clicked()
                                {
                                    let account = self.account_from_form();
                                    self.begin_google_sign_in(ui.ctx(), account);
                                }
                                // Only offered once there is a main view to go
                                // back to.
                                if !self.accounts.is_empty() && ui.button("Cancel").clicked() {
                                    self.adding_account = false;
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
            // The folder pane, like Thunderbird's: each account is a
            // top-level node with its own connection status and mailbox
            // tree. It is its own column, separate from the message-list
            // column below -- previously both lived stacked in one narrow
            // `left_panel`, which squeezed the tree into a `max_height(220.0)`
            // scroll area regardless of how much vertical room the window
            // actually had. As its own resizable panel, the tree gets the
            // full column width and full available height.
            egui::Panel::left("mailbox_panel").resizable(true).default_size(240.0).show(ui, |ui| {
                // Compose/Drafts/Outbox as a stack of full-width buttons atop
                // the account tree, Thunderbird/Outlook-style, rather than
                // buried in the File menu -- these are the compose-related
                // actions used often enough to deserve one click instead of
                // two.
                let full_width = ui.available_width();
                if ui.add_sized([full_width, 32.0], egui::Button::new("Compose")).clicked() {
                    self.open_compose(
                        ComposeState { account_id: self.active_account_id(), ..Default::default() },
                        compose_window::Focus::To,
                    );
                }
                if ui.add_sized([full_width, 28.0], egui::Button::new("Drafts")).clicked() {
                    self.drafts_window = Some(Vec::new());
                    let _ = self.db_tx.try_send(DbCommand::ListDrafts);
                }
                if ui.add_sized([full_width, 28.0], egui::Button::new("Outbox")).clicked() {
                    self.outbox_window = Some(Vec::new());
                    let _ = self.db_tx.try_send(DbCommand::ListOutbox);
                }
                ui.separator();

                // Deferred past the loop for the same reason as the message
                // list below: acting on a click needs &mut self, which can't
                // happen while the rows still borrow `self.accounts`.
                let mut clicked_mailbox: Option<(AccountId, String)> = None;
                // Same deferral for a fold toggle: it edits `self.config`, which
                // the rows (borrowed from `self.accounts`) are alive across.
                let mut toggled_folder: Option<(AccountId, String, bool)> = None;
                let mut logout: Option<AccountId> = None;
                let mut reconnect: Option<AccountId> = None;
                let mut sign_in: Option<AccountId> = None;
                ui.horizontal(|ui| {
                    ui.heading("Accounts");
                    if ui.small_button("+").on_hover_text("Add account").clicked() {
                        self.adding_account = true;
                    }
                });
                egui::ScrollArea::vertical().id_salt("mailboxes_scroll").show(ui, |ui| {
                    ui.with_layout(egui::Layout::top_down_justified(egui::Align::LEFT), |ui| {
                        for view in &self.accounts {
                            let status_color = match &view.state {
                                ConnState::Connected => egui::Color32::from_rgb(60, 160, 80),
                                ConnState::Connecting | ConnState::Disconnected => egui::Color32::from_rgb(210, 150, 30),
                                ConnState::Failed(_) => egui::Color32::from_rgb(180, 40, 40),
                            };
                            let unread = view.total_unread();
                            let title = if unread > 0 {
                                format!("{}  ({unread})", view.label())
                            } else {
                                view.label().to_string()
                            };
                            let header = egui::RichText::new(title).strong();
                            egui::CollapsingHeader::new(header)
                                .id_salt(("account_node", view.id()))
                                .default_open(true)
                                .icon(move |ui, _openness, response| {
                                    // An envelope (Twemoji, see `emoji.rs`) with
                                    // the connection status as a dot on its
                                    // corner. The dot is drawn, not a "●"
                                    // character: egui's bundled fonts have no
                                    // such glyph, and it came out as a box.
                                    let rect = response.rect.expand(2.0);
                                    if !emoji::paint_in_rect(ui.ctx(), ui.painter(), rect, "\u{2709}\u{fe0f}") {
                                        ui.painter().circle_filled(response.rect.center(), 4.0, status_color);
                                        return;
                                    }
                                    let center = rect.right_bottom() - egui::vec2(3.0, 3.0);
                                    ui.painter().circle_filled(center, 4.5, ui.visuals().panel_fill);
                                    ui.painter().circle_filled(center, 3.0, status_color);
                                })
                                .show(ui, |ui| {
                                    match &view.state {
                                        ConnState::Connecting => {
                                            ui.label(egui::RichText::new("Connecting…").weak());
                                        }
                                        ConnState::Failed(error) => {
                                            ui.colored_label(egui::Color32::from_rgb(180, 40, 40), error);
                                            let is_google = self
                                                .config
                                                .accounts
                                                .iter()
                                                .any(|a| a.id == view.id() && a.auth == config::AuthKind::GoogleOAuth);
                                            if is_google {
                                                if ui.button("Sign in again").clicked() {
                                                    sign_in = Some(view.id().to_string());
                                                }
                                            } else if ui.button("Reconnect…").clicked() {
                                                reconnect = Some(view.id().to_string());
                                            }
                                        }
                                        ConnState::Disconnected => {
                                            ui.label(egui::RichText::new("Reconnecting…").weak());
                                        }
                                        ConnState::Connected => {}
                                    }
                                    let account_id = view.id().to_string();
                                    let collapsed: std::collections::BTreeSet<String> = self
                                        .config
                                        .collapsed_folders
                                        .iter()
                                        .filter_map(|k| k.strip_prefix(&account_id)?.strip_prefix('\t'))
                                        .map(str::to_string)
                                        .collect();
                                    for index in imap::visible_rows(&view.mailbox_rows, &collapsed) {
                                        let row = &view.mailbox_rows[index];
                                        let is_collapsed = row.has_children && collapsed.contains(&row.key);
                                        // A folded node shows its whole subtree's unread
                                        // count, so mail in a hidden child is not lost.
                                        let subtree_unread = |own: Option<&String>| -> u32 {
                                            let own = own.and_then(|n| view.unread_counts.get(n)).copied().unwrap_or(0);
                                            let below: u32 = if is_collapsed {
                                                imap::descendants(&view.mailbox_rows, index)
                                                    .iter()
                                                    .filter_map(|r| r.full_name.as_ref().and_then(|n| view.unread_counts.get(n)))
                                                    .sum()
                                            } else {
                                                0
                                            };
                                            own + below
                                        };
                                        let mut indent = |ui: &mut egui::Ui| {
                                            ui.add_space(row.depth as f32 * 14.0);
                                            // The arrow is drawn, not a "▸"/"▾" character:
                                            // egui's bundled fonts have neither, and they
                                            // came out as a box.
                                            let icon_width = ui.spacing().icon_width;
                                            if row.has_children {
                                                let (_, response) = ui.allocate_exact_size(egui::vec2(icon_width, icon_width), egui::Sense::click());
                                                let openness = if is_collapsed { 0.0 } else { 1.0 };
                                                egui::collapsing_header::paint_default_icon(ui, openness, &response);
                                                let hint = if is_collapsed { "Expand" } else { "Collapse" };
                                                if response.on_hover_text(hint).clicked() {
                                                    toggled_folder = Some((account_id.clone(), row.key.clone(), !is_collapsed));
                                                }
                                            } else {
                                                // Keeps leaf labels lined up with the
                                                // labels of their siblings that have an
                                                // arrow.
                                                ui.add_space(icon_width + ui.spacing().item_spacing.x);
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
                                        let is_selected = self.active.as_deref() == Some(view.id()) && self.selected_mailbox == *full_name;
                                        let unread = subtree_unread(Some(full_name));
                                        let label = if unread > 0 {
                                            format!("{}  ({unread})", row.label)
                                        } else {
                                            row.label.clone()
                                        };
                                        ui.horizontal(|ui| {
                                            indent(ui);
                                            if ui.add(egui::Button::selectable(is_selected, label)).clicked() {
                                                clicked_mailbox = Some((account_id.clone(), full_name.clone()));
                                            }
                                        });
                                    }
                                    if ui
                                        .small_button("Log out")
                                        .on_hover_text("Disconnect this account and stop watching it")
                                        .clicked()
                                    {
                                        logout = Some(view.id().to_string());
                                    }
                                });
                        }
                    });
                });
                if let Some((account, key, collapse)) = toggled_folder {
                    if self.config.set_folder_collapsed(&account, &key, collapse) {
                        self.save_config("folded mailbox folders");
                    }
                }
                if let Some((account, mb)) = clicked_mailbox {
                    self.activate(&account, mb);
                }
                if let Some(id) = logout {
                    self.disconnect_account(&id);
                }
                if let Some(id) = sign_in {
                    self.sign_in_again(ui.ctx(), &id);
                }
                if let Some(id) = reconnect {
                    if let Some(account) = self.config.accounts.iter().find(|a| a.id == id).cloned() {
                        self.fill_form_from(&account);
                    }
                    self.adding_account = true;
                }
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
                        // Clicks are by index: across accounts a UID alone
                        // does not identify a message.
                        let mut clicked: Option<(usize, egui::Modifiers)> = None;
                        // Hits from several accounts/mailboxes say where
                        // each one lives.
                        let show_origin = is_search && self.search_all_accounts;
                        for (i, header) in list.iter().enumerate() {
                            let in_open_context = !is_search || self.in_open_context(i);
                            let is_selected = in_open_context
                                && (self.selected_uids.contains(&header.uid) || self.selected_uid == Some(header.uid));
                            let resp = message_row(ui, header, is_selected);
                            if show_origin {
                                if let Some((account, mailbox)) = self.search_origins.get(i) {
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(format!("{} \u{b7} {mailbox}", self.account_label(account)))
                                                .small()
                                                .weak(),
                                        )
                                        .truncate(),
                                    );
                                }
                            }
                            if resp.clicked() {
                                clicked = Some((i, ui.input(|i| i.modifiers)));
                            }
                        }
                        if let Some((idx, modifiers)) = clicked {
                            let uid = list[idx].uid;
                            // A selection range only makes sense within one
                            // mailbox; hits from several are opened one at
                            // a time.
                            let modifiers = if show_origin { egui::Modifiers::NONE } else { modifiers };
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
                            if is_search {
                                self.open_search_hit(idx);
                            } else {
                                self.open_message(uid, false);
                            }
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
                if self.selected_uid.is_some() {
                    // Cloned rather than borrowed: the Reply/Reply All/
                    // Forward buttons below need `&mut self` while
                    // this is in scope, which can't coexist with a borrow of
                    // `self.headers` (the same reason the mailbox/message
                    // list loops elsewhere in this file defer their sends).
                    if let Some(header) = self.selected_header() {
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
                                    self.open_compose(
                                        ComposeState::reply(&header, &self.current_message_html).with_account(self.active_account_id()),
                                        compose_window::Focus::Body,
                                    );
                                }
                                if ui.button("Reply All").clicked() {
                                    // "Me" is the account that received the
                                    // message, so Reply All drops that
                                    // address from Cc.
                                    self.open_compose(
                                        ComposeState::reply_all(&header, &self.current_message_html, &self.active_username())
                                            .with_account(self.active_account_id()),
                                        compose_window::Focus::Body,
                                    );
                                }
                                if ui.button("Forward").clicked() {
                                    self.open_compose(
                                        ComposeState::forward(&header, &self.current_message_html).with_account(self.active_account_id()),
                                        compose_window::Focus::To,
                                    );
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
                                        .on_hover_text("Load remote images automatically for every message that says it is from this address (the From header is not authenticated)")
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

        self.show_compose_windows(ui.ctx());
        self.show_quit_confirmation(ui.ctx());
        self.show_drafts_window(ui.ctx());
        self.show_outbox_window(ui.ctx());
        self.show_settings_window(ui.ctx());
    }
}

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
/// How often `poll_outbox` asks the db for retries that are due -- an
/// enqueued send is also always attempted immediately (`DbEvent::OutboxEnqueued`),
/// so this interval only matters for a *failed* send's automatic retry, or
/// for a row left over from a previous run that crashed mid-send.
const OUTBOX_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(20);
/// How often an open compose window autosaves itself as a draft.
const DRAFT_AUTOSAVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Start a session for `account` and wrap it in the per-account UI state.
/// `pending_persist` is `Some` for an account added through the form, which
/// is only saved once its connection succeeds.
///
/// The session's new-mail watch (B10) runs as a plain tokio task, not
/// anything driven by `EsMailApp::logic`/`ui`, so it keeps running -- and can
/// keep showing toasts -- for as long as the process is alive, independent of
/// whether the main window is visible. That's what "notifications work even
/// with the window closed" means in practice: the process (and these tasks)
/// survives a window close because the tray (`platform`) turns that close into
/// hide-to-tray instead of exit.
fn spawn_account_view(
    account: &AccountConfig,
    auth: auth::Auth,
    pending_persist: Option<(AccountConfig, auth::Auth)>,
    events: &mpsc::Sender<AccountEvent>,
    hooks: &Hooks,
) -> AccountView {
    let session = AccountSession::spawn(
        SessionParams {
            id: account.id.clone(),
            label: account.display_name.clone(),
            host: account.imap_host.clone(),
            port: account.imap_port,
            username: account.username.clone(),
            auth: auth.clone(),
            watch_mailbox: account
                .watch_mailbox
                .clone()
                .unwrap_or_else(|| session::DEFAULT_WATCH_MAILBOX.to_string()),
        },
        events.clone(),
        hooks.clone(),
    );
    AccountView {
        session,
        state: ConnState::Connecting,
        mailbox_rows: Vec::new(),
        unread_counts: std::collections::HashMap::new(),
        auth,
        pending_persist,
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
    let dir = paths::attachments_dir();
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
/// normal-colour sender and a dimmed subject. The message's local time sits at
/// the right end of the sender line and its date at the right end of the
/// subject line (see `MailHeader::local_date_time`); a starred message gets a
/// ★ just left of the time. Painted by hand, not with a `Button`, because a
/// button cannot truncate two differently-styled lines.
fn message_row(ui: &mut egui::Ui, header: &MailHeader, selected: bool) -> egui::Response {
    const PAD_X: f32 = 10.0;
    const PAD_Y: f32 = 6.0;
    const ACCENT_BAR_WIDTH: f32 = 3.0;
    const LINE_GAP: f32 = 2.0;
    const SENDER_SIZE: f32 = 14.5;
    const SUBJECT_SIZE: f32 = 13.0;
    const TIMESTAMP_SIZE: f32 = 12.0;
    /// Space kept between a line's text and whatever is at its right end.
    const RIGHT_GAP: f32 = 8.0;

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
    let (date, time) = match header.local_date_time() {
        Some((date, time)) => (
            Some(one_line(ui, &date, TIMESTAMP_SIZE, subject_color, f32::INFINITY)),
            Some(one_line(ui, &time, TIMESTAMP_SIZE, subject_color, f32::INFINITY)),
        ),
        None => (None, None),
    };
    let reserved = |galley: &Option<std::sync::Arc<egui::Galley>>| galley.as_ref().map_or(0.0, |g| g.size().x + RIGHT_GAP);
    let full_width = (width - ACCENT_BAR_WIDTH - PAD_X * 2.0).max(0.0);
    let sender_width = (full_width - reserved(&time) - reserved(&star)).max(0.0);
    let subject_width = (full_width - reserved(&date)).max(0.0);

    let sender = header.sender_name();
    let sender = if sender.is_empty() { "(unknown sender)" } else { sender.as_str() };
    let subject = if header.subject.is_empty() { "(no subject)" } else { header.subject.as_str() };
    // Emoji are laid out as placeholders and painted as coloured images over
    // them below -- see `emoji.rs`.
    let sender_text = emoji::prepare(sender);
    let subject_text = emoji::prepare(subject);
    let sender_galley = one_line(ui, &sender_text.text, SENDER_SIZE, sender_color, sender_width);
    let subject_galley = one_line(ui, &subject_text.text, SUBJECT_SIZE, subject_color, subject_width);

    let height = PAD_Y * 2.0 + sender_galley.size().y + LINE_GAP + subject_galley.size().y;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click());
    // What a screen reader announces. Unread and starred are otherwise only
    // conveyed by colour and an icon (they used to be a "●"/"★" in the button
    // text), so they are spelled out here.
    response.widget_info(|| {
        let state = match (unread, header.is_flagged()) {
            (true, true) => "Unread, starred. ",
            (true, false) => "Unread. ",
            (false, true) => "Starred. ",
            (false, false) => "",
        };
        egui::WidgetInfo::selected(egui::WidgetType::Button, true, selected, format!("{state}{sender}: {subject}. {}", header.date))
    });

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
        emoji::paint(ui.ctx(), painter, &sender_galley, sender_pos, &sender_text.emoji);
        let right = rect.right() - PAD_X;
        // The smaller timestamps are bottom-aligned to the text they share a
        // line with, so they sit on its baseline instead of floating at the
        // top of the line.
        let time_reserved = reserved(&time);
        if let Some(time) = time {
            let y = sender_pos.y + sender_galley.size().y - time.size().y;
            painter.galley(egui::pos2(right - time.size().x, y), time, subject_color);
        }
        if let Some(star) = star {
            painter.galley(egui::pos2(right - time_reserved - star.size().x, sender_pos.y), star, star_color);
        }
        let subject_pos = egui::pos2(text_left, sender_pos.y + sender_galley.size().y + LINE_GAP);
        if let Some(date) = date {
            let y = subject_pos.y + subject_galley.size().y - date.size().y;
            painter.galley(egui::pos2(right - date.size().x, y), date, subject_color);
        }
        painter.galley(subject_pos, subject_galley.clone(), subject_color);
        emoji::paint(ui.ctx(), painter, &subject_galley, subject_pos, &subject_text.emoji);
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

/// Development runs that render one page and exit (`ESMAIL_PREVIEW`,
/// `ESMAIL_SCREENSHOT`) are not "the" running mail client: they must not take
/// the single-instance lock, register notifications, or start a tray icon.
fn is_dev_run() -> bool {
    std::env::var_os("ESMAIL_PREVIEW").is_some() || std::env::var_os("ESMAIL_SCREENSHOT").is_some()
}

#[tokio::main]
async fn main() -> eframe::Result {
    init_logging();
    // See `platform::disable_background_throttling`'s doc: without this,
    // compose windows (#34) and the tray/toast machinery can go unresponsive
    // for a long time once no esMail window has focus.
    platform::disable_background_throttling();

    // `esmail --purge-data`: what the Windows uninstaller runs when the user
    // chooses to remove their settings too. Deliberately ahead of the
    // single-instance check -- it must work whatever else is happening.
    // `esmail --quit`: ask the running copy to exit (the installer does this
    // before replacing or removing the program files) and return.
    if std::env::args().any(|arg| arg == "--quit") {
        if shell::acquire_single_instance() == shell::Instance::AlreadyRunning {
            let _ = shell::send_request(shell::Request::Quit);
        }
        return Ok(());
    }

    if std::env::args().any(|arg| arg == "--purge-data") {
        let problems = uninstall::purge_user_data();
        for problem in &problems {
            eprintln!("esmail: could not remove {problem}");
        }
        std::process::exit(if problems.is_empty() { 0 } else { 1 });
    }

    if !is_dev_run() {
        // esMail lives in the tray: a second launch should raise the window
        // that is already there, not start a competing process.
        if shell::acquire_single_instance() == shell::Instance::AlreadyRunning {
            let _ = shell::send_request(shell::Request::Show);
            return Ok(());
        }

        let registered = match shell::register_notification_identity() {
            Ok(()) => true,
            Err(e) => {
                log::warn!("could not register esMail's notification identity: {e}");
                false
            }
        };
        platform::use_own_notification_identity(registered);

        paths::clean_attachments_dir();
    }

    // Window-geometry persistence (B9): the saved size/position has to be
    // known before the window is created at all, so this reads config.toml
    // a second time here (`EsMailApp::new` also loads it, for the account
    // list and theme) rather than threading a pre-loaded `Config` through
    // `run_native`'s `Box<dyn FnOnce>` closure -- a second cheap file read on
    // startup is a small price for keeping `EsMailApp::new`'s signature
    // (`&eframe::CreationContext`, same as every other eframe app) untouched.
    let mut viewport = egui::ViewportBuilder::default().with_inner_size([1280.0, 720.0]);
    if let Some(icon) = icons::window_icon() {
        viewport = viewport.with_icon(egui::IconData::from(icon));
    }
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
