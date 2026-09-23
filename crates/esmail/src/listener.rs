//! `esmail --background`: the resident listener process (phase 2 of
//! `docs/BACKGROUND-LISTENER.md`).
//!
//! The listener holds one [`Watcher`] (one IMAP session per account, each with
//! its own IDLE watch and new-mail toast), an [`ipc::Server`] for a GUI to
//! connect to, and the tray icon -- and nothing else. There is no egui, no GL
//! context, no webview and no fonts, which is the point: the GUI process can
//! exit while mail keeps being watched here.
//!
//! **Two threads, on purpose.** The async work runs on a current-thread tokio
//! runtime on a background thread. The tray needs a Win32 message pump, and
//! `tray-icon` delivers its events through a hidden window's window procedure,
//! so the main thread runs a winit event loop with **no windows**: it pumps
//! the thread's messages, blocking with no polling and needing no `unsafe`.
//! (Winit requires the event loop on the main thread on Windows, which is why
//! it cannot share the runtime thread.) [`Wake`] carries the little the two
//! threads say to each other.
//!
//! **The endpoint is the lock.** [`Server::bind`] claims the endpoint derived
//! from the data directory exclusively (a named pipe on Windows, a socket file
//! elsewhere), so a second `--background` finds it taken and exits quietly --
//! no second lock file, the endpoint's own uniqueness is reused.
//!
//! The tray's "Quit" stops the listener. "Show esMail" is a no-op for now: the
//! GUI client that would receive `ToGui::Show` is phase 3.

use std::sync::Arc;

use anyhow::Context;
use esmail::auth::Auth;
use esmail::config::Config;
use esmail::ipc::message::{ToGui, ToListener};
use esmail::ipc::{self, Endpoint, LocalSocket, Server, Transport};
use esmail::platform::{self, TrayAction, TrayState};
use esmail::watcher::{self, Change, Watcher};
use esmail::{paths, secrets, shell};
use tokio::sync::mpsc;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::window::WindowId;

/// One end of an accepted GUI connection.
type Connection = ipc::Connection<<LocalSocket as Transport>::Stream, ToGui, ToListener>;

/// What the async listener asks the message loop on the main thread to do.
enum Wake {
    /// A new unread total for the tray tooltip.
    Unread(u32),
    /// Stop the message loop (the async side is done, or is stopping).
    Quit,
}

/// Run the listener until the tray's Quit. Returns immediately when another
/// `--background` already holds the endpoint, so a duplicate launch is a quiet
/// no-op rather than an error.
pub fn run() -> anyhow::Result<()> {
    let event_loop = EventLoop::<Wake>::with_user_event()
        .build()
        .map_err(|e| anyhow::anyhow!("could not create the listener message loop: {e}"))?;
    let proxy = event_loop.create_proxy();
    let (tray_tx, tray_rx) = mpsc::unbounded_channel::<TrayAction>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<anyhow::Result<bool>>();

    let listener = std::thread::Builder::new()
        .name("esmail-listener".to_string())
        .spawn({
            let proxy = proxy.clone();
            move || {
                let _stop_on_drop = StopOnDrop(proxy.clone());
                let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(runtime) => runtime,
                    Err(e) => {
                        let _ = ready_tx.send(Err(anyhow::anyhow!("could not start the async runtime: {e}")));
                        return;
                    }
                };
                runtime.block_on(async move {
                    let server = match claim_endpoint().await {
                        Ok(Some(server)) => server,
                        Ok(None) => {
                            let _ = ready_tx.send(Ok(false));
                            return;
                        }
                        Err(e) => {
                            let _ = ready_tx.send(Err(e));
                            return;
                        }
                    };
                    let _ = ready_tx.send(Ok(true));
                    serve(server, tray_rx, proxy).await;
                });
            }
        })
        .map_err(|e| anyhow::anyhow!("could not start the listener thread: {e}"))?;

    match ready_rx.recv() {
        Ok(Ok(true)) => {}
        Ok(Ok(false)) => {
            log::info!("a background listener is already running; exiting");
            let _ = listener.join();
            return Ok(());
        }
        Ok(Err(e)) => {
            let _ = listener.join();
            return Err(e);
        }
        Err(_) => return Err(anyhow::anyhow!("the listener thread stopped before it started")),
    }

    let registered = match shell::register_notification_identity() {
        Ok(()) => true,
        Err(e) => {
            log::warn!("listener: could not register the notification identity: {e}");
            false
        }
    };
    platform::use_own_notification_identity(registered);

    let mut app = ListenerApp { tray: None, tray_tx, unread: 0 };
    let result = event_loop.run_app(&mut app);
    // Dropping the app closes `tray_tx`; that ends the listener thread's
    // `select!` even if no Quit action was read.
    drop(app);
    let _ = listener.join();
    result.map_err(|e| anyhow::anyhow!("the listener message loop stopped: {e}"))
}

/// Claim the endpoint, or `None` if a listener already holds it. See the
/// module doc for why binding is the lock. Runs on the listener's own runtime,
/// since a tokio listener is tied to the runtime that created it.
async fn claim_endpoint() -> anyhow::Result<Option<Server<LocalSocket>>> {
    let data_dir = paths::data_dir().context("no data directory available")?;
    let endpoint = Endpoint::for_data_dir(&data_dir);
    // Probe before binding: on Windows a taken pipe and a real permission
    // problem both surface as `PermissionDenied`, and only the former means a
    // listener is already running.
    if LocalSocket::connect(&endpoint).await.is_ok() {
        return Ok(None);
    }
    let token = ipc::token::generate()?;
    let server = Server::<LocalSocket>::bind(&endpoint, token.clone())
        .with_context(|| format!("could not bind the listener endpoint {}", endpoint.name()))?;
    if let Some(path) = ipc::token::path() {
        ipc::token::write(&path, &token).context("could not write the ipc token")?;
    }
    Ok(Some(server))
}

/// The async half: watch the accounts and serve GUI connections until the tray
/// asks to quit (or the endpoint stops accepting connections).
async fn serve(server: Server<LocalSocket>, mut tray: mpsc::UnboundedReceiver<TrayAction>, proxy: EventLoopProxy<Wake>) {
    let (reload_tx, mut reload_rx) = mpsc::channel(1);
    // Kept so `reload_rx` never closes when every client disconnects; a closed
    // channel would make the reload branch fire with `None` in a busy loop.
    let _reload_tx_keepalive = reload_tx.clone();
    let (connection_tx, mut connection_rx) = mpsc::channel(8);
    tokio::spawn(accept_connections(server, connection_tx));

    let mut watcher = Watcher::new(Arc::new(platform::show_new_mail_toast));
    apply_config(&mut watcher);

    loop {
        // A reload is applied after the `select!`: `watcher.next_change()`
        // borrows `watcher` for as long as the select expression lives, so the
        // watcher cannot be touched from inside a branch.
        let mut reload = false;
        tokio::select! {
            change = watcher.next_change() => match change {
                Change::Unread { total, .. } => {
                    let _ = proxy.send_event(Wake::Unread(total));
                }
                Change::State { account, state } => log::debug!("listener: {account}: {state:?}"),
            },
            connection = connection_rx.recv() => match connection {
                Some(connection) => spawn_client(connection, reload_tx.clone()),
                None => {
                    log::error!("listener: the endpoint stopped accepting connections");
                    break;
                }
            },
            _ = reload_rx.recv() => reload = true,
            action = tray.recv() => match action {
                Some(TrayAction::Quit) | None => break,
                Some(TrayAction::Show) => {
                    log::info!("listener: tray Open clicked (the GUI client arrives in phase 3)");
                }
            },
        }
        if reload {
            log::info!("listener: reloading accounts after a config change");
            apply_config(&mut watcher);
        }
    }
    let _ = proxy.send_event(Wake::Quit);
}

/// Accept GUI connections and hand them to [`serve`]. Ends (closing the
/// channel) on a transport error, which [`serve`] treats as fatal.
async fn accept_connections(mut server: Server<LocalSocket>, connections: mpsc::Sender<Connection>) {
    loop {
        match server.accept().await {
            Ok(connection) => {
                if connections.send(connection).await.is_err() {
                    break;
                }
            }
            Err(e) => {
                log::error!("listener: could not accept an ipc connection: {e}");
                break;
            }
        }
    }
}

/// Read one client's messages. The handshake already consumed its `Hello`, so
/// the only thing a client says is `ConfigChanged`; a closed connection just
/// ends this task.
fn spawn_client(mut connection: Connection, reload: mpsc::Sender<()>) {
    tokio::spawn(async move {
        loop {
            match connection.recv().await {
                Ok(Some(ToListener::ConfigChanged)) => {
                    if reload.send(()).await.is_err() {
                        break;
                    }
                }
                // The handshake consumed the only `Hello`; a second one is
                // meaningless but not worth dropping the connection over.
                Ok(Some(ToListener::Hello { .. })) => {}
                Ok(None) => break,
                Err(e) => {
                    log::debug!("listener: ipc client connection ended: {e}");
                    break;
                }
            }
        }
    });
}

/// (Re)load `config.toml` into `watcher`. A credential the keyring cannot
/// produce leaves that account unwatched, with a log line, rather than failing
/// the reload.
fn apply_config(watcher: &mut Watcher) {
    let (accounts, problems) = watcher::load_accounts(&Config::load());
    for (account, auth) in &accounts {
        // A rotated refresh token must be written back to the keyring, or the
        // next start would load the stale one the provider already replaced.
        if let Auth::OAuth(source) = auth {
            let id = account.id.clone();
            source.on_rotation(move |token| match secrets::set_password(&id, "oauth", token) {
                Ok(()) => log::info!("listener: saved a rotated Google refresh token for {id}"),
                Err(e) => log::warn!("listener: could not save the rotated Google refresh token for {id}: {e}"),
            });
        }
    }
    watcher.apply_accounts(accounts);
    for (name, reason) in problems {
        log::warn!("listener: not watching {name}: {reason}");
    }
}

/// The message loop's application: owns the tray (created once the loop is
/// running, on this same thread) and forwards tray clicks to the async side.
struct ListenerApp {
    tray: Option<TrayState>,
    tray_tx: mpsc::UnboundedSender<TrayAction>,
    /// Latest unread total, applied when the tray is created if it arrived
    /// before then.
    unread: u32,
}

impl ApplicationHandler<Wake> for ListenerApp {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {
        if self.tray.is_some() {
            return;
        }
        match TrayState::new() {
            Ok(mut tray) => {
                tray.set_unread(self.unread);
                self.tray = Some(tray);
            }
            Err(e) => log::warn!("listener: no tray icon here; the listener keeps running until the process is stopped: {e}"),
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: Wake) {
        match event {
            Wake::Unread(total) => {
                self.unread = total;
                if let Some(tray) = &mut self.tray {
                    tray.set_unread(total);
                }
            }
            Wake::Quit => event_loop.exit(),
        }
    }

    fn window_event(&mut self, _event_loop: &ActiveEventLoop, _window_id: WindowId, _event: WindowEvent) {}

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(tray) = &mut self.tray else {
            return;
        };
        tray.refresh_icon();
        for action in tray.poll_actions() {
            match action {
                TrayAction::Quit => {
                    let _ = self.tray_tx.send(TrayAction::Quit);
                    event_loop.exit();
                    return;
                }
                other => {
                    let _ = self.tray_tx.send(other);
                }
            }
        }
    }
}

/// Wakes the message loop when the listener thread ends, including on a panic,
/// so a dead listener never leaves a tray behind with no work.
struct StopOnDrop(EventLoopProxy<Wake>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        let _ = self.0.send_event(Wake::Quit);
    }
}
