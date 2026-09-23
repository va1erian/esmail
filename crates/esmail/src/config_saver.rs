//! Persists `config.toml` on a background thread so a preference toggle never
//! blocks a frame on disk I/O. One worker thread owns every asynchronous
//! write, so two saves can't overlap; queued saves are coalesced to the
//! newest, and a save issued moments before quitting is flushed by
//! [`ConfigSaver::flush`]/`Drop`.

use std::sync::mpsc;
use std::thread::JoinHandle;

use esmail::config::Config;

enum Message {
    Save(Config),
    Flush(mpsc::Sender<()>),
}

pub(super) struct ConfigSaver {
    tx: Option<mpsc::Sender<Message>>,
    handle: Option<JoinHandle<()>>,
}

impl ConfigSaver {
    pub(super) fn new() -> Self {
        let (tx, rx) = mpsc::channel::<Message>();
        let handle = std::thread::Builder::new()
            .name("esmail-config-save".to_string())
            .spawn(move || Self::run(rx))
            .ok();
        Self { tx: Some(tx), handle }
    }

    /// Queue `config` to be written. Only the newest of several queued saves
    /// has to reach the disk.
    pub(super) fn save(&self, config: &Config) {
        let Some(tx) = &self.tx else {
            return;
        };
        if tx.send(Message::Save(config.clone())).is_err() {
            log::warn!("config saver thread has stopped; preference will not be saved");
        }
    }

    /// Block until every queued save has been written. Called as the app
    /// exits, so a preference toggled just before quitting is not lost.
    pub(super) fn flush(&self) {
        let Some(tx) = &self.tx else {
            return;
        };
        let (done_tx, done_rx) = mpsc::channel();
        if tx.send(Message::Flush(done_tx)).is_ok() {
            let _ = done_rx.recv();
        }
    }

    fn run(rx: mpsc::Receiver<Message>) {
        while let Ok(message) = rx.recv() {
            match message {
                Message::Save(mut config) => {
                    // Coalesce the rest of the batch; a Flush ends it.
                    loop {
                        match rx.try_recv() {
                            Ok(Message::Save(newer)) => config = newer,
                            Ok(Message::Flush(done)) => {
                                Self::write(&config);
                                let _ = done.send(());
                                break;
                            }
                            Err(mpsc::TryRecvError::Empty) => {
                                Self::write(&config);
                                break;
                            }
                            Err(mpsc::TryRecvError::Disconnected) => {
                                Self::write(&config);
                                return;
                            }
                        }
                    }
                }
                Message::Flush(done) => {
                    let _ = done.send(());
                }
            }
        }
    }

    fn write(config: &Config) {
        if let Err(e) = config.save() {
            log::warn!("could not persist config.toml: {e}");
        }
    }
}

impl Drop for ConfigSaver {
    fn drop(&mut self) {
        // Closing the channel lets the worker drain what is queued, then join
        // so the last write has actually landed before the process exits.
        self.tx.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
