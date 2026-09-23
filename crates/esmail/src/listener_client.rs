//! The GUI's side of the background listener (phase 3 of
//! `docs/BACKGROUND-LISTENER.md`): find it, start it when it is not running,
//! and hold a live connection to it.
//!
//! Best effort, and never in the way of startup: the connection is made on a
//! background task. Until it succeeds -- and forever, when no listener could be
//! started, `ESMAIL_NO_LISTENER` is set, or this is not Windows -- the GUI is
//! in **fallback mode**: it keeps its own tray and hides to it on close,
//! exactly as before. Once [`ListenerClient::is_connected`] is true the
//! listener owns the tray and the new-mail toasts, so the GUI drops its own
//! tray and closing the window exits the process.
//!
//! One long-lived connection carries "a GUI is here" (the listener treats its
//! close as the GUI exiting); a `ConfigChanged` opens a short second connection
//! rather than sharing the reader, so neither side ever multiplexes reads and
//! writes on one stream.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use esmail::ipc::message::{ToGui, ToListener};
use esmail::ipc::{self, Endpoint, LocalSocket, Transport};
use tokio::sync::mpsc;

/// How long to keep trying to reach a listener we just started, or one that is
/// still starting up, before giving up and running in fallback mode.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const RETRY_EVERY: Duration = Duration::from_millis(200);

/// The GUI's link to the background listener. Cheap to poll: a flag and a
/// channel.
pub(super) struct ListenerClient {
    incoming: mpsc::Receiver<ToGui>,
    connected: Arc<AtomicBool>,
    /// Where and how to reach the listener, set once a connection succeeds.
    link: Arc<OnceLock<Link>>,
}

#[derive(Clone)]
struct Link {
    endpoint: Endpoint,
    token: String,
}

impl ListenerClient {
    /// Start the connection task and return immediately. See the module doc
    /// for when this stays disconnected.
    pub(super) fn start() -> Self {
        if std::env::var_os("ESMAIL_NO_LISTENER").is_some() {
            log::info!("ESMAIL_NO_LISTENER is set; running without a background listener");
            return Self::disabled();
        }
        // The listener's tray is Windows-only (see `platform/other.rs`), so a
        // listener elsewhere would be headless and unquittable; keep today's
        // single-process behavior there.
        if !cfg!(windows) {
            return Self::disabled();
        }
        let Some(data_dir) = esmail::paths::data_dir() else {
            log::warn!("no data directory; running without a background listener");
            return Self::disabled();
        };
        let (incoming_tx, incoming) = mpsc::channel(16);
        let connected = Arc::new(AtomicBool::new(false));
        let link = Arc::new(OnceLock::new());
        tokio::spawn({
            let connected = Arc::clone(&connected);
            let link = Arc::clone(&link);
            async move { connect_to_listener(data_dir, incoming_tx, connected, link).await }
        });
        Self { incoming, connected, link }
    }

    /// A client that never connects: preview/screenshot runs, or a platform
    /// with no listener.
    pub(super) fn disabled() -> Self {
        let (_, incoming) = mpsc::channel(1);
        Self { incoming, connected: Arc::new(AtomicBool::new(false)), link: Arc::new(OnceLock::new()) }
    }

    /// Whether the listener is reachable right now.
    pub(super) fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// The next message the listener sent, if one is waiting.
    pub(super) fn try_recv(&mut self) -> Option<ToGui> {
        self.incoming.try_recv().ok()
    }

    /// Tell the listener the account list changed, so it reloads without
    /// waiting for its own next event. A no-op when not connected.
    pub(super) fn notify_config_changed(&self) {
        if !self.is_connected() {
            return;
        }
        let Some(link) = self.link.get().cloned() else { return };
        tokio::spawn(async move {
            match ipc::connect::<LocalSocket>(&link.endpoint, &link.token).await {
                Ok(mut connection) => {
                    if let Err(e) = connection.send(&ToListener::ConfigChanged).await {
                        log::debug!("could not report a config change to the listener: {e}");
                    }
                }
                Err(e) => log::debug!("could not reach the listener to report a config change: {e}"),
            }
        });
    }
}

/// Probe, spawn if needed, connect, then hold the connection open and forward
/// what the listener says. Ends (leaving `connected` false) when the
/// connection closes or cannot be made.
async fn connect_to_listener(
    data_dir: PathBuf,
    incoming: mpsc::Sender<ToGui>,
    connected: Arc<AtomicBool>,
    link: Arc<OnceLock<Link>>,
) {
    let endpoint = Endpoint::for_data_dir(&data_dir);
    if !listener_is_running(&endpoint).await {
        if let Err(e) = spawn_listener() {
            log::warn!("could not start the background listener: {e}");
            return;
        }
    }

    let token_path = ipc::token::path();
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    let mut connection = loop {
        if let Some(token) = token_path.as_deref().and_then(|path| ipc::token::read(path).ok()) {
            match ipc::connect::<LocalSocket>(&endpoint, &token).await {
                Ok(connection) => {
                    let _ = link.set(Link { endpoint: endpoint.clone(), token });
                    break connection;
                }
                Err(e) => log::debug!("the background listener is not ready yet: {e}"),
            }
        }
        if Instant::now() >= deadline {
            log::warn!("no background listener after {CONNECT_TIMEOUT:?}; running without one");
            return;
        }
        tokio::time::sleep(RETRY_EVERY).await;
    };
    log::info!("connected to the background listener");
    connected.store(true, Ordering::Relaxed);

    loop {
        match connection.recv().await {
            Ok(Some(message)) => {
                if incoming.send(message).await.is_err() {
                    break;
                }
            }
            Ok(None) => break,
            Err(e) => {
                log::debug!("the background listener connection ended: {e}");
                break;
            }
        }
    }
    connected.store(false, Ordering::Relaxed);
    log::info!("the background listener connection is gone");
}

/// The same probe the listener's own `claim_endpoint` uses: a successful
/// connect means something is already listening on the endpoint.
async fn listener_is_running(endpoint: &Endpoint) -> bool {
    LocalSocket::connect(endpoint).await.is_ok()
}

/// Start `esmail --background` as a detached child. A second listener on a
/// taken endpoint exits on its own, so this is safe to call when one might
/// already be starting.
fn spawn_listener() -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("--background")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // No console window for the resident process.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command.spawn().map(|_| ())
}
