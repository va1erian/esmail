//! A local, authenticated byte stream between esMail's two processes: the tiny
//! background listener and the GUI (see `docs/BACKGROUND-LISTENER.md`).
//!
//! Three layers, each replaceable without touching the ones above it:
//!
//! * [`Transport`] -- how two processes on one machine reach each other:
//!   `bind` a name, `accept` and `connect`. The stream it produces is just
//!   `AsyncRead + AsyncWrite`. [`LocalSocket`] is the real one (a named pipe on
//!   Windows, a Unix domain socket elsewhere, both through the `interprocess`
//!   crate); [`Memory`] is an in-process one for tests. Adding an OS or a
//!   mechanism means implementing this trait, nothing else.
//! * [`Connection`] -- JSON lines over any such stream, with a bounded line
//!   length, plus the [`Server`] / [`connect`] handshake that proves the client
//!   knows a per-session token before either side says anything else.
//! * [`message`] -- what the two sides actually say to each other.
//!
//! **Who may connect.** A transport must keep other users out on its own
//! (Windows: an owner-only ACL on the pipe; Unix: a socket in a directory only
//! its owner can enter). The token is a second lock for a same-user process
//! that should not be talking to us, and the listener sends nothing until it
//! has read a valid `Hello`, so a client that merely manages to connect learns
//! nothing.

mod connection;
mod local;
mod memory;
pub mod message;
pub mod token;

use std::future::Future;
use std::io;
use std::path::Path;

use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};

pub use connection::{Connection, Server, connect};
pub use local::LocalSocket;
pub use memory::Memory;

/// Where a listener can be reached. A logical name; each [`Transport`] maps it
/// onto its own address space (`\\.\pipe\<name>`, a socket path, ...).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    name: String,
}

impl Endpoint {
    /// The endpoint of the esMail whose data directory is `data_dir`. Derived
    /// from a hash of that path, so two users on one machine never collide, and
    /// a profile relocated with `ESMAIL_DATA_DIR` (a test, a portable copy) has
    /// an endpoint of its own -- exactly like the single-instance lock.
    pub fn for_data_dir(data_dir: &Path) -> Self {
        let digest = Sha256::digest(data_dir.to_string_lossy().as_bytes());
        let hex: String = digest.iter().take(8).map(|byte| format!("{byte:02x}")).collect();
        Self { name: format!("esmail-{hex}") }
    }

    /// An endpoint with an explicit name, for tests.
    pub fn named(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

/// A way for two processes on one machine to talk. See the module doc.
pub trait Transport: 'static {
    type Listener: Send + 'static;
    type Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static;

    /// Claim `endpoint` for listening. Fails if another listener already holds
    /// it: nothing may be able to squat on the name and impersonate the
    /// listener, or share it.
    fn bind(endpoint: &Endpoint) -> io::Result<Self::Listener>;

    /// Wait for the next connection.
    fn accept(listener: &mut Self::Listener) -> impl Future<Output = io::Result<Self::Stream>> + Send;

    /// Connect to a listener on `endpoint`.
    fn connect(endpoint: &Endpoint) -> impl Future<Output = io::Result<Self::Stream>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_endpoint_is_stable_per_data_dir_and_differs_between_them() {
        let a = Endpoint::for_data_dir(Path::new(r"C:\Users\alice\AppData\Local\esmail\data"));
        let again = Endpoint::for_data_dir(Path::new(r"C:\Users\alice\AppData\Local\esmail\data"));
        let bob = Endpoint::for_data_dir(Path::new(r"C:\Users\bob\AppData\Local\esmail\data"));
        assert_eq!(a, again);
        assert_ne!(a, bob);
        assert!(a.name().starts_with("esmail-") && a.name().len() == "esmail-".len() + 16, "{}", a.name());
    }
}
