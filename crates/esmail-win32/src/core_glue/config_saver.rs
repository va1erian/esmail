//! Writes `config.toml` on a thread of its own, so a preference change never
//! makes the window wait for the disk. One worker does every write, so two
//! saves cannot overlap, and queued saves are coalesced to the newest.

use std::sync::mpsc;
use std::thread::JoinHandle;

use esmail::config::Config;

/// A background writer of the config file.
pub struct ConfigSaver {
    queue: Option<mpsc::Sender<Config>>,
    worker: Option<JoinHandle<()>>,
}

impl ConfigSaver {
    /// Starts the worker.
    pub fn start() -> ConfigSaver {
        let (queue, saves) = mpsc::channel::<Config>();
        let worker = std::thread::Builder::new()
            .name("esmail-config-save".to_string())
            .spawn(move || {
                while let Ok(mut config) = saves.recv() {
                    while let Ok(newer) = saves.try_recv() {
                        config = newer;
                    }
                    if let Err(error) = config.save() {
                        log::warn!("could not write config.toml: {error}");
                    }
                }
            })
            .ok();
        ConfigSaver { queue: Some(queue), worker }
    }

    /// Queues `config` to be written.
    pub fn save(&self, config: &Config) {
        if self.queue.as_ref().is_none_or(|queue| queue.send(config.clone()).is_err()) {
            log::warn!("the config writer has stopped; the change will not be saved");
        }
    }
}

impl Drop for ConfigSaver {
    /// Closing the queue lets the worker finish what is queued; joining it means
    /// the last write has landed before the process exits.
    fn drop(&mut self) {
        self.queue.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
