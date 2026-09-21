//! An in-process [`Transport`] for tests: endpoints are names in a process-wide
//! table and a connection is a [`tokio::io::duplex`] pipe handed to the
//! listener. It behaves like the real ones where it matters -- binding a name
//! that is taken fails, and dropping the listener frees the name -- so the layer
//! above can be tested without opening a real pipe or socket.

use std::collections::HashMap;
use std::io;
use std::sync::{LazyLock, Mutex};

use tokio::io::DuplexStream;
use tokio::sync::mpsc;

use super::{Endpoint, Transport};

/// Buffer of each direction of a connection.
const BUFFER: usize = 16 * 1024;

static ENDPOINTS: LazyLock<Mutex<HashMap<String, mpsc::UnboundedSender<DuplexStream>>>> = LazyLock::new(Mutex::default);

pub struct Memory;

/// Holds a name in the table for as long as it lives.
pub struct MemoryListener {
    name: String,
    incoming: mpsc::UnboundedReceiver<DuplexStream>,
}

impl Drop for MemoryListener {
    fn drop(&mut self) {
        ENDPOINTS.lock().unwrap().remove(&self.name);
    }
}

impl Transport for Memory {
    type Listener = MemoryListener;
    type Stream = DuplexStream;

    fn bind(endpoint: &Endpoint) -> io::Result<Self::Listener> {
        let mut endpoints = ENDPOINTS.lock().unwrap();
        if endpoints.contains_key(endpoint.name()) {
            return Err(io::Error::new(io::ErrorKind::AddrInUse, format!("{} is already being listened on", endpoint.name())));
        }
        let (tx, incoming) = mpsc::unbounded_channel();
        endpoints.insert(endpoint.name().to_string(), tx);
        Ok(MemoryListener { name: endpoint.name().to_string(), incoming })
    }

    async fn accept(listener: &mut Self::Listener) -> io::Result<Self::Stream> {
        listener.incoming.recv().await.ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "the listener was closed"))
    }

    async fn connect(endpoint: &Endpoint) -> io::Result<Self::Stream> {
        let sender = ENDPOINTS
            .lock()
            .unwrap()
            .get(endpoint.name())
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("nothing is listening on {}", endpoint.name())))?;
        let (client, server) = tokio::io::duplex(BUFFER);
        sender.send(server).map_err(|_| io::Error::new(io::ErrorKind::ConnectionRefused, "the listener went away"))?;
        Ok(client)
    }
}
