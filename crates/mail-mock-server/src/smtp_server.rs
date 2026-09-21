//! A minimal SMTP server -- `EHLO`, `AUTH PLAIN`/`AUTH XOAUTH2`, `MAIL FROM`, `RCPT TO`,
//! `DATA`, `RSET`, `QUIT` -- just what `smtp.rs`'s `lettre` transport sends.
//!
//! Plaintext only, matching `TlsMode::None` in `esmail::config`, which its
//! own doc comment flags as existing specifically "for a local/test server".
//! `smtp.rs::build_transport` uses `builder_dangerous` for that mode, so no
//! TLS handshake happens on this side either.

use base64::Engine as _;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

use crate::store::SharedStore;

pub async fn spawn(store: SharedStore, bind_addr: &str) -> anyhow::Result<std::net::SocketAddr> {
    let listener = TcpListener::bind(bind_addr).await?;
    let addr = listener.local_addr()?;

    tokio::spawn(async move {
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    log::warn!("smtp accept error: {e}");
                    continue;
                }
            };
            let store = store.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_connection(stream, store).await {
                    log::debug!("smtp session {peer} ended: {e}");
                }
            });
        }
    });

    Ok(addr)
}

#[derive(Default)]
struct Envelope {
    recipients: Vec<String>,
}

async fn handle_connection(stream: tokio::net::TcpStream, store: SharedStore) -> anyhow::Result<()> {
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);

    write_half.write_all(b"220 esmail mail-mock-server ready\r\n").await?;

    let mut authenticated = false;
    let mut envelope = Envelope::default();
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(());
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        let upper = trimmed.to_ascii_uppercase();

        if upper.starts_with("EHLO") || upper.starts_with("HELO") {
            write_half.write_all(b"250-esmail mock server\r\n250 AUTH PLAIN XOAUTH2\r\n").await?;
        } else if upper.starts_with("AUTH XOAUTH2") {
            let arg = trimmed["AUTH XOAUTH2".len()..].trim();
            let ok = base64::engine::general_purpose::STANDARD
                .decode(arg)
                .ok()
                .is_some_and(|payload| store.lock().unwrap().check_xoauth2(&payload).is_some());
            authenticated = ok;
            if ok {
                write_half.write_all(b"235 Authentication successful\r\n").await?;
            } else {
                write_half.write_all(b"535 Authentication failed\r\n").await?;
            }
        } else if upper.starts_with("AUTH PLAIN") {
            let arg = trimmed["AUTH PLAIN".len()..].trim();
            let ok = authenticate_plain(&store, arg);
            authenticated = ok;
            if ok {
                write_half.write_all(b"235 Authentication successful\r\n").await?;
            } else {
                write_half.write_all(b"535 Authentication failed\r\n").await?;
            }
        } else if upper.starts_with("MAIL FROM") {
            if !authenticated {
                write_half.write_all(b"530 Authentication required\r\n").await?;
                continue;
            }
            envelope = Envelope::default();
            write_half.write_all(b"250 OK\r\n").await?;
        } else if upper.starts_with("RCPT TO") {
            if !authenticated {
                write_half.write_all(b"530 Authentication required\r\n").await?;
                continue;
            }
            if let Some(addr) = extract_angle_address(trimmed) {
                envelope.recipients.push(addr);
            }
            write_half.write_all(b"250 OK\r\n").await?;
        } else if upper.starts_with("DATA") {
            if !authenticated || envelope.recipients.is_empty() {
                write_half.write_all(b"503 bad sequence of commands\r\n").await?;
                continue;
            }
            write_half.write_all(b"354 Start mail input; end with <CRLF>.<CRLF>\r\n").await?;
            let raw = read_dot_terminated_body(&mut reader).await?;

            {
                // Scoped so the `MutexGuard` (not `Send`) is dropped before
                // the `.await` below -- otherwise this connection's future
                // stops being `Send`, which `tokio::spawn` requires.
                let mut guard = store.lock().unwrap();
                for recipient in &envelope.recipients {
                    let mailbox = mailbox_for_recipient(recipient);
                    guard.deliver(mailbox, raw.clone());
                }
            }

            write_half.write_all(b"250 OK Message accepted\r\n").await?;
        } else if upper.starts_with("RSET") {
            envelope = Envelope::default();
            write_half.write_all(b"250 OK\r\n").await?;
        } else if upper.starts_with("NOOP") {
            write_half.write_all(b"250 OK\r\n").await?;
        } else if upper.starts_with("QUIT") {
            write_half.write_all(b"221 Bye\r\n").await?;
            return Ok(());
        } else {
            write_half.write_all(b"500 unrecognized command\r\n").await?;
        }
    }
}

fn authenticate_plain(store: &SharedStore, base64_arg: &str) -> bool {
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(base64_arg) else {
        return false;
    };
    // authzid \0 authcid \0 password
    let parts: Vec<&[u8]> = decoded.splitn(3, |&b| b == 0).collect();
    let Some((authcid, password)) = parts.get(1).zip(parts.get(2)) else {
        return false;
    };
    let (Ok(user), Ok(pass)) = (std::str::from_utf8(authcid), std::str::from_utf8(password)) else {
        return false;
    };
    store.lock().unwrap().check_login(user, pass)
}

fn extract_angle_address(line: &str) -> Option<String> {
    let start = line.find('<')?;
    let end = line[start..].find('>')? + start;
    Some(line[start + 1..end].to_string())
}

/// Test setups here have exactly one mailbox owner, so every delivery lands
/// in that account's INBOX regardless of which address it was addressed
/// to -- good enough for "send then fetch" round-trip tests, and simpler
/// than modeling multiple independent mailboxes.
fn mailbox_for_recipient(_recipient: &str) -> &'static str {
    "INBOX"
}

async fn read_dot_terminated_body<R: tokio::io::AsyncRead + Unpin>(reader: &mut BufReader<R>) -> anyhow::Result<Vec<u8>> {
    let mut raw = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            anyhow::bail!("connection closed mid-DATA");
        }
        if line == ".\r\n" || line == ".\n" {
            break;
        }
        // Dot-stuffing: a line starting with ".." represents a line whose
        // real content starts with a single ".".
        if let Some(stripped) = line.strip_prefix("..") {
            raw.extend_from_slice(b".");
            raw.extend_from_slice(stripped.as_bytes());
        } else {
            raw.extend_from_slice(line.as_bytes());
        }
    }
    Ok(raw)
}
