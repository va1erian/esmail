//! The real [`Transport`]: a local socket from the `interprocess` crate.
//!
//! * **Windows** -- a named pipe, `\\.\pipe\<endpoint>`. No port and no network
//!   stack. The first instance of the pipe is created exclusively, so a second
//!   listener (or a process squatting on the name) is refused; remote clients
//!   are refused (`interprocess` leaves `accept_remote` off); and the pipe's
//!   ACL is replaced with one that lets only its owner in. (The default ACL
//!   gives Everyone read access.)
//! * **Unix** -- a socket file inside a directory created mode 0700, so no other
//!   user can even reach it. It lives in `$XDG_RUNTIME_DIR` when there is one.

use std::io;

use interprocess::local_socket::tokio::{Listener, Stream};
use interprocess::local_socket::traits::tokio::{Listener as _, Stream as _};
use super::{Endpoint, Transport};

pub struct LocalSocket;

impl Transport for LocalSocket {
    type Listener = Listener;
    type Stream = Stream;

    fn bind(endpoint: &Endpoint) -> io::Result<Self::Listener> {
        platform::bind(endpoint)
    }

    async fn accept(listener: &mut Self::Listener) -> io::Result<Self::Stream> {
        listener.accept().await
    }

    async fn connect(endpoint: &Endpoint) -> io::Result<Self::Stream> {
        Stream::connect(platform::name(endpoint)?).await
    }
}

#[cfg(windows)]
mod platform {
    use std::io;

    use interprocess::local_socket::tokio::Listener;
    use interprocess::local_socket::{GenericNamespaced, ListenerOptions, Name, ToNsName};
    use interprocess::os::windows::local_socket::ListenerOptionsExt;
    use interprocess::os::windows::security_descriptor::SecurityDescriptor;
    use widestring::u16cstr;

    use super::Endpoint;

    /// A protected DACL (`D:P`) with one entry: allow (`A`) everything (`GA`) to
    /// the object's owner (`OW`, "owner rights"). Nobody else -- not Everyone,
    /// not the anonymous account, not other users -- gets any access.
    const OWNER_ONLY: &widestring::U16CStr = u16cstr!("D:P(A;;GA;;;OW)");

    pub fn name(endpoint: &Endpoint) -> io::Result<Name<'static>> {
        endpoint.name().to_string().to_ns_name::<GenericNamespaced>()
    }

    pub fn bind(endpoint: &Endpoint) -> io::Result<Listener> {
        ListenerOptions::new()
            .name(name(endpoint)?)
            .security_descriptor(SecurityDescriptor::deserialize(OWNER_ONLY)?)
            .create_tokio()
    }
}

#[cfg(unix)]
mod platform {
    use std::io;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;

    use interprocess::local_socket::tokio::Listener;
    use interprocess::local_socket::{GenericFilePath, ListenerOptions, Name, ToFsName};

    use super::Endpoint;

    fn directory(endpoint: &Endpoint) -> PathBuf {
        let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        base.join(endpoint.name())
    }

    /// Create the directory the socket goes in, readable only by its owner
    /// (and re-tightened if it already existed with looser permissions).
    pub fn prepare_directory(endpoint: &Endpoint) -> io::Result<()> {
        let dir = directory(endpoint);
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
    }

    pub fn socket_path(endpoint: &Endpoint) -> PathBuf {
        directory(endpoint).join("ipc.sock")
    }

    pub fn name(endpoint: &Endpoint) -> io::Result<Name<'static>> {
        socket_path(endpoint).to_fs_name::<GenericFilePath>()
    }

    /// Bind the socket, refusing if a live listener already holds it -- the same
    /// guarantee the Windows pipe gives -- but replacing a socket file that a
    /// listener left behind when it died (nothing answers on it).
    pub fn bind(endpoint: &Endpoint) -> io::Result<Listener> {
        prepare_directory(endpoint)?;
        let try_bind = || ListenerOptions::new().name(name(endpoint)?).create_tokio();
        match try_bind() {
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
                let path = socket_path(endpoint);
                if UnixStream::connect(&path).is_ok() {
                    return Err(e);
                }
                std::fs::remove_file(&path)?;
                try_bind()
            }
            other => other,
        }
    }
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;

    #[tokio::test]
    async fn a_socket_file_left_by_a_dead_listener_is_replaced() {
        let endpoint = Endpoint::named(format!("esmail-test-stale-{}", std::process::id()));
        platform::prepare_directory(&endpoint).unwrap();
        let path = platform::socket_path(&endpoint);
        // Bind and drop a plain listener: the socket file stays, nothing answers on it.
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists());

        let listener = LocalSocket::bind(&endpoint).expect("a stale socket file must not block the listener");
        assert!(LocalSocket::bind(&endpoint).is_err(), "but a live listener still holds the name");
        drop(listener);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
