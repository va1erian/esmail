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
use interprocess::local_socket::Name;

use super::{Endpoint, Transport};

pub struct LocalSocket;

impl Transport for LocalSocket {
    type Listener = Listener;
    type Stream = Stream;

    fn bind(endpoint: &Endpoint) -> io::Result<Self::Listener> {
        platform::bind(name_for_bind(endpoint)?)
    }

    async fn accept(listener: &mut Self::Listener) -> io::Result<Self::Stream> {
        listener.accept().await
    }

    async fn connect(endpoint: &Endpoint) -> io::Result<Self::Stream> {
        Stream::connect(name_for_connect(endpoint)?).await
    }
}

#[cfg(windows)]
fn name_for_bind(endpoint: &Endpoint) -> io::Result<Name<'static>> {
    platform::name(endpoint)
}

#[cfg(windows)]
fn name_for_connect(endpoint: &Endpoint) -> io::Result<Name<'static>> {
    platform::name(endpoint)
}

#[cfg(unix)]
fn name_for_bind(endpoint: &Endpoint) -> io::Result<Name<'static>> {
    platform::prepare_directory(endpoint)?;
    platform::name(endpoint)
}

#[cfg(unix)]
fn name_for_connect(endpoint: &Endpoint) -> io::Result<Name<'static>> {
    platform::name(endpoint)
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

    pub fn bind(name: Name<'static>) -> io::Result<Listener> {
        ListenerOptions::new()
            .name(name)
            .security_descriptor(SecurityDescriptor::deserialize(OWNER_ONLY)?)
            .create_tokio()
    }
}

#[cfg(unix)]
mod platform {
    use std::io;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
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

    pub fn name(endpoint: &Endpoint) -> io::Result<Name<'static>> {
        directory(endpoint).join("ipc.sock").to_fs_name::<GenericFilePath>()
    }

    pub fn bind(name: Name<'static>) -> io::Result<Listener> {
        // A socket file left by a listener that died is replaced: which process
        // may listen is decided by the single-instance lock, not by this file.
        ListenerOptions::new().name(name).try_overwrite(true).create_tokio()
    }
}
