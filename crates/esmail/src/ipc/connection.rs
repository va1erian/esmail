//! JSON lines over any [`Transport`] stream, and the handshake that opens every
//! connection.
//!
//! The listener ([`Server`]) reads the client's `Hello` first and answers with
//! `Welcome` only if its token matches; a connection that says anything else,
//! says nothing for [`HANDSHAKE_TIMEOUT`], sends an over-long line or a wrong
//! token is dropped without a single byte written to it.

use std::io;
use std::marker::PhantomData;
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};

use super::message::{PROTOCOL_VERSION, ToGui, ToListener};
use super::{Endpoint, Transport, token};

/// How long a new connection has to complete the handshake.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);

/// The longest line accepted, in bytes. Every message is a few dozen bytes; a
/// bound keeps a misbehaving peer from making the reader buffer without limit.
const MAX_LINE: usize = 16 * 1024;

/// One end of an established connection: sends `Tx`, receives `Rx`.
pub struct Connection<S, Tx, Rx> {
    reader: BufReader<ReadHalf<S>>,
    writer: WriteHalf<S>,
    _messages: PhantomData<fn(Tx) -> Rx>,
}

impl<S, Tx, Rx> Connection<S, Tx, Rx>
where
    S: tokio::io::AsyncRead + AsyncWrite + Unpin,
    Tx: Serialize,
    Rx: DeserializeOwned,
{
    fn new(stream: S) -> Self {
        let (reader, writer) = tokio::io::split(stream);
        Self { reader: BufReader::new(reader), writer, _messages: PhantomData }
    }

    pub async fn send(&mut self, message: &Tx) -> io::Result<()> {
        let mut line = serde_json::to_vec(message).map_err(io::Error::other)?;
        line.push(b'\n');
        self.writer.write_all(&line).await?;
        self.writer.flush().await
    }

    /// The next message, or `None` once the peer has closed the connection.
    /// A line that is too long or is not a valid message is an error.
    pub async fn recv(&mut self) -> io::Result<Option<Rx>> {
        let mut line = Vec::new();
        // `take` gives each call a fresh bound; one byte over the limit is how
        // "the line does not fit" is told apart from "the line ends exactly here".
        let read = (&mut self.reader).take(MAX_LINE as u64 + 1).read_until(b'\n', &mut line).await?;
        if read == 0 {
            return Ok(None);
        }
        if line.last() != Some(&b'\n') && line.len() > MAX_LINE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "message line is too long"));
        }
        serde_json::from_slice(&line).map(Some).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

/// The listener's side: accepts connections and lets in only those that pass
/// the handshake.
pub struct Server<T: Transport> {
    listener: T::Listener,
    token: String,
}

impl<T: Transport> Server<T> {
    /// Claim `endpoint`. Fails if it is already held.
    pub fn bind(endpoint: &Endpoint, token: String) -> io::Result<Self> {
        Ok(Self { listener: T::bind(endpoint)?, token })
    }

    /// The next client that completes the handshake. Connections that fail it
    /// are dropped and the wait continues; only an error from the transport
    /// itself is returned.
    pub async fn accept(&mut self) -> io::Result<Connection<T::Stream, ToGui, ToListener>> {
        loop {
            let stream = T::accept(&mut self.listener).await?;
            match tokio::time::timeout(HANDSHAKE_TIMEOUT, self.handshake(stream)).await {
                Ok(Ok(connection)) => return Ok(connection),
                Ok(Err(e)) => log::debug!("ipc: refused a connection: {e}"),
                Err(_) => log::debug!("ipc: a connection did not say hello in time"),
            }
        }
    }

    async fn handshake(&self, stream: T::Stream) -> io::Result<Connection<T::Stream, ToGui, ToListener>> {
        let mut connection = Connection::new(stream);
        match connection.recv().await? {
            Some(ToListener::Hello { token, version }) if version == PROTOCOL_VERSION && token::matches(&token, &self.token) => {}
            Some(ToListener::Hello { version, .. }) if version != PROTOCOL_VERSION => {
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("protocol version {version}")));
            }
            _ => return Err(io::Error::new(io::ErrorKind::PermissionDenied, "bad or missing hello")),
        }
        connection.send(&ToGui::Welcome { version: PROTOCOL_VERSION }).await?;
        Ok(connection)
    }
}

/// The client's side: connect to the listener on `endpoint` and prove it knows
/// `token`. Fails with `PermissionDenied` if the listener does not answer with
/// `Welcome` (a wrong token or version gets the connection dropped).
pub async fn connect<T: Transport>(endpoint: &Endpoint, token: &str) -> io::Result<Connection<T::Stream, ToListener, ToGui>> {
    let stream = T::connect(endpoint).await?;
    let mut connection = Connection::new(stream);
    connection.send(&ToListener::Hello { token: token.to_string(), version: PROTOCOL_VERSION }).await?;
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, connection.recv()).await {
        Ok(Ok(Some(ToGui::Welcome { version }))) if version == PROTOCOL_VERSION => Ok(connection),
        Ok(Ok(_)) | Ok(Err(_)) => Err(io::Error::new(io::ErrorKind::PermissionDenied, "the listener refused the connection")),
        Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "the listener did not answer")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{LocalSocket, Memory};
    use std::sync::atomic::{AtomicU32, Ordering};

    const TOKEN: &str = "s3cret";

    /// A name no other test (or process) is using.
    fn endpoint(tag: &str) -> Endpoint {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        Endpoint::named(format!("esmail-test-{}-{tag}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)))
    }

    /// The same behaviour, checked against every transport.
    macro_rules! transport_tests {
        ($($name:ident: $transport:ty,)*) => {
            $(
                mod $name {
                    use super::*;
                    type T = $transport;

                    #[tokio::test]
                    async fn a_client_with_the_token_is_welcomed_and_messages_flow_both_ways() {
                        let endpoint = endpoint("flow");
                        let mut server = Server::<T>::bind(&endpoint, TOKEN.into()).unwrap();
                        let client = tokio::spawn({
                            let endpoint = endpoint.clone();
                            async move {
                                let mut client = connect::<T>(&endpoint, TOKEN).await.unwrap();
                                client.send(&ToListener::ConfigChanged).await.unwrap();
                                assert_eq!(client.recv().await.unwrap(), Some(ToGui::OpenAccount { account: "a@b".into() }));
                                assert_eq!(client.recv().await.unwrap(), Some(ToGui::Quit));
                                assert_eq!(client.recv().await.unwrap(), None, "the listener closed the connection");
                            }
                        });
                        let mut connection = server.accept().await.unwrap();
                        assert_eq!(connection.recv().await.unwrap(), Some(ToListener::ConfigChanged));
                        connection.send(&ToGui::OpenAccount { account: "a@b".into() }).await.unwrap();
                        connection.send(&ToGui::Quit).await.unwrap();
                        drop(connection);
                        client.await.unwrap();
                    }

                    #[tokio::test]
                    async fn a_wrong_token_is_refused_and_the_next_valid_client_still_gets_in() {
                        let endpoint = endpoint("token");
                        let mut server = Server::<T>::bind(&endpoint, TOKEN.into()).unwrap();
                        let accepted = tokio::spawn(async move { server.accept().await.map(|_| ()) });

                        let refused = connect::<T>(&endpoint, "guess").await;
                        assert_eq!(refused.err().map(|e| e.kind()), Some(io::ErrorKind::PermissionDenied));
                        connect::<T>(&endpoint, TOKEN).await.expect("a client with the right token is let in");
                        accepted.await.unwrap().unwrap();
                    }

                    #[tokio::test]
                    async fn the_listener_sends_nothing_before_a_valid_hello() {
                        let endpoint = endpoint("silent");
                        let mut server = Server::<T>::bind(&endpoint, TOKEN.into()).unwrap();
                        let _accept = tokio::spawn(async move { server.accept().await });

                        let mut raw = T::connect(&endpoint).await.unwrap();
                        let mut byte = [0u8; 1];
                        let heard = tokio::time::timeout(Duration::from_millis(400), raw.read(&mut byte)).await;
                        assert!(heard.is_err(), "a connection that has not said hello was sent something: {heard:?}");
                    }

                    #[tokio::test]
                    async fn garbage_and_over_long_hellos_are_dropped_without_a_reply() {
                        let endpoint = endpoint("garbage");
                        let mut server = Server::<T>::bind(&endpoint, TOKEN.into()).unwrap();
                        let accepted = tokio::spawn(async move { server.accept().await.map(|_| ()) });

                        for payload in [b"not json\n".to_vec(), vec![b'a'; MAX_LINE + 100], b"{\"type\":\"config_changed\"}\n".to_vec()] {
                            let mut raw = T::connect(&endpoint).await.unwrap();
                            raw.write_all(&payload).await.unwrap();
                            let mut reply = Vec::new();
                            let end = tokio::time::timeout(Duration::from_secs(5), raw.read_to_end(&mut reply)).await;
                            assert!(matches!(end, Ok(_)), "the connection should be closed on the client");
                            assert!(reply.is_empty(), "the listener answered a bad hello: {reply:?}");
                        }
                        connect::<T>(&endpoint, TOKEN).await.expect("still serving after the bad ones");
                        accepted.await.unwrap().unwrap();
                    }

                    #[tokio::test]
                    async fn a_hello_for_another_protocol_version_is_refused() {
                        let endpoint = endpoint("version");
                        let mut server = Server::<T>::bind(&endpoint, TOKEN.into()).unwrap();
                        let _accept = tokio::spawn(async move { server.accept().await });

                        let mut raw = Connection::<_, ToListener, ToGui>::new(T::connect(&endpoint).await.unwrap());
                        raw.send(&ToListener::Hello { token: TOKEN.into(), version: PROTOCOL_VERSION + 1 }).await.unwrap();
                        let answer = tokio::time::timeout(Duration::from_secs(5), raw.recv()).await.expect("the listener closes the connection");
                        assert!(matches!(answer, Ok(None) | Err(_)), "got {answer:?}");
                    }

                    #[tokio::test]
                    async fn a_second_listener_on_the_same_endpoint_is_refused() {
                        let endpoint = endpoint("squat");
                        let _first = Server::<T>::bind(&endpoint, TOKEN.into()).unwrap();
                        assert!(Server::<T>::bind(&endpoint, "other".into()).is_err(), "the name must not be shareable or squattable");
                    }

                    #[tokio::test]
                    async fn connecting_to_nothing_fails() {
                        assert!(connect::<T>(&endpoint("nobody"), TOKEN).await.is_err());
                    }
                }
            )*
        };
    }

    transport_tests! {
        over_memory: Memory,
        over_a_local_socket: LocalSocket,
    }
}
