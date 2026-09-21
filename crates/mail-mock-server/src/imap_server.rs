//! A minimal IMAP4rev1 server -- just enough of the protocol for esmail's
//! `imap.rs` to drive: `LOGIN`, `AUTHENTICATE XOAUTH2`, `LIST`, `EXAMINE`, `FETCH (UID ENVELOPE)`,
//! `UID FETCH <n|n:m|n:*> (RFC822 | (UID ENVELOPE))`, `APPEND`, `IDLE`,
//! `LOGOUT`. Nothing else esmail sends is implemented.
//!
//! `APPEND` delivers straight into `Store` via `Store::deliver`, same as
//! `smtp_server.rs`'s `DATA` handler -- an `APPEND`ed message becomes
//! indistinguishable from one that arrived over SMTP once it lands, which
//! is the correct behavior (a real server's `Sent` folder holds exactly
//! that: client-appended copies, not anything the server itself received).
//! No `APPENDUID`/`UIDPLUS` response extension, no flags/date-time
//! arguments -- `imap.rs::ImapCommand::Append` sends none of those, only a
//! bare `APPEND <mailbox> {n}` followed by the literal.
//!
//! `IDLE` (RFC 2177) pushes an untagged `* N EXISTS` as soon as
//! `Store::deliver` lands a message in the selected mailbox, by subscribing
//! to `store.notify` (a `tokio::sync::broadcast` fed on every delivery) for
//! as long as the client is idling. This is deliberately the *only* signal
//! IDLE reports here -- no `EXPUNGE`, no flag-change `FETCH` -- since
//! nothing in this server ever removes a message or changes a flag; a real
//! server's IDLE can report either, and `esmail::idle_watch`'s client-side
//! handling treats any push as "something changed, go re-`EXAMINE`" rather
//! than trying to parse which kind, so this narrower mock is still a
//! faithful test of that contract.
//!
//! Every response that carries a string uses IMAP's `{n}\r\n<bytes>` literal
//! syntax instead of quoted strings. Literals need no escaping for any
//! content (including CRLF, quotes, non-ASCII), which sidesteps needing a
//! byte-perfect quoted-string quoter to satisfy `imap-proto`'s parser --
//! verified against the vendored `imap-proto-0.16.7` grammar
//! (`parser/core.rs::literal`).

use base64::Engine as _;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio_native_tls::TlsAcceptor;
use tokio_native_tls::native_tls::{Identity, TlsAcceptor as NativeTlsAcceptor};

use crate::store::SharedStore;

pub async fn spawn(store: SharedStore, bind_addr: &str, pkcs12: &[u8], pkcs12_password: &str) -> anyhow::Result<std::net::SocketAddr> {
    let identity = Identity::from_pkcs12(pkcs12, pkcs12_password)?;
    let acceptor: TlsAcceptor = NativeTlsAcceptor::new(identity)?.into();
    let listener = TcpListener::bind(bind_addr).await?;
    let addr = listener.local_addr()?;

    tokio::spawn(async move {
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    log::warn!("imap accept error: {e}");
                    continue;
                }
            };
            let acceptor = acceptor.clone();
            let store = store.clone();
            tokio::spawn(async move {
                match acceptor.accept(stream).await {
                    Ok(tls) => {
                        if let Err(e) = handle_connection(tls, store).await {
                            log::debug!("imap session {peer} ended: {e}");
                        }
                    }
                    Err(e) => log::warn!("imap TLS handshake with {peer} failed: {e}"),
                }
            });
        }
    });

    Ok(addr)
}

/// Wraps a byte string as an IMAP literal: `{n}\r\n<bytes>`.
fn literal(bytes: &[u8]) -> Vec<u8> {
    let mut out = format!("{{{}}}\r\n", bytes.len()).into_bytes();
    out.extend_from_slice(bytes);
    out
}

fn nstring(s: &str) -> Vec<u8> {
    if s.is_empty() { b"NIL".to_vec() } else { literal(s.as_bytes()) }
}

/// One address structure: `(name adl mailbox host)`, or `NIL` if absent.
fn address(addr: &Option<(String, String, String)>) -> Vec<u8> {
    match addr {
        None => b"NIL".to_vec(),
        Some((name, mailbox, host)) => {
            let mut out = b"((".to_vec();
            out.extend_from_slice(&nstring(name));
            out.push(b' ');
            out.extend_from_slice(b"NIL ");
            out.extend_from_slice(&nstring(mailbox));
            out.push(b' ');
            out.extend_from_slice(&nstring(host));
            out.extend_from_slice(b"))");
            out
        }
    }
}

/// One `* <seq> FETCH (UID <uid> ENVELOPE (...) FLAGS (...))` line -- shared
/// by the sequence-number `FETCH` handler and `UID FETCH`'s `ENVELOPE` mode,
/// which otherwise built an identical response by hand in two places. FLAGS
/// is always included alongside ENVELOPE (B8 needs both, and `async_imap`'s
/// parser is happy to see more items than a bare `FETCH (UID ENVELOPE)`
/// request asked for).
fn envelope_fetch_response(seq: u32, msg: &crate::store::StoredMessage) -> Vec<u8> {
    let mut r = format!("* {seq} FETCH (UID {} ENVELOPE (", msg.uid).into_bytes();
    r.extend_from_slice(&nstring(&msg.envelope.date));
    r.push(b' ');
    r.extend_from_slice(&nstring(&msg.envelope.subject));
    r.push(b' ');
    r.extend_from_slice(&address(&msg.envelope.from));
    r.push(b' ');
    r.extend_from_slice(&address(&msg.envelope.from)); // sender
    r.push(b' ');
    r.extend_from_slice(&address(&msg.envelope.from)); // reply-to
    r.push(b' ');
    r.extend_from_slice(&address(&msg.envelope.to));
    r.extend_from_slice(b" NIL NIL NIL "); // cc bcc in-reply-to
    r.extend_from_slice(&nstring(&msg.envelope.message_id));
    r.extend_from_slice(b") FLAGS (");
    r.extend_from_slice(msg.flags.join(" ").as_bytes());
    r.extend_from_slice(b"))\r\n");
    r
}

/// One `* <seq> FETCH (FLAGS (...))` line -- the response `STORE`/`UID
/// STORE` (B8) sends back so the client learns the message's resulting flag
/// list without a second round trip.
fn flags_fetch_response(seq: u32, msg: &crate::store::StoredMessage) -> Vec<u8> {
    format!("* {seq} FETCH (FLAGS ({}))\r\n", msg.flags.join(" ")).into_bytes()
}

async fn handle_connection<S>(stream: S, store: SharedStore) -> anyhow::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);

    write_half.write_all(b"* OK esmail mail-mock-server ready\r\n").await?;

    let mut authenticated_user: Option<String> = None;
    let mut selected_mailbox: Option<String> = None;
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(()); // client closed the connection
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            continue;
        }

        let tokens = tokenize(line);
        let Some(tag) = tokens.first() else { continue };
        let tag = tag.clone();
        let verb = tokens.get(1).map(|s| s.to_ascii_uppercase()).unwrap_or_default();

        match verb.as_str() {
            "LOGIN" => {
                let (Some(user), Some(pass)) = (tokens.get(2), tokens.get(3)) else {
                    write_half.write_all(format!("{tag} BAD LOGIN needs two arguments\r\n").as_bytes()).await?;
                    continue;
                };
                let ok = store.lock().unwrap().check_login(user, pass);
                if ok {
                    authenticated_user = Some(user.clone());
                    write_half.write_all(format!("{tag} OK LOGIN completed\r\n").as_bytes()).await?;
                } else {
                    write_half.write_all(format!("{tag} NO LOGIN failed\r\n").as_bytes()).await?;
                }
            }
            "AUTHENTICATE" => {
                // Only XOAUTH2, the one mechanism esmail's OAuth sign-in uses.
                if !tokens.get(2).is_some_and(|m| m.eq_ignore_ascii_case("XOAUTH2")) {
                    write_half.write_all(format!("{tag} NO unsupported authentication mechanism\r\n").as_bytes()).await?;
                    continue;
                }
                // The initial response may ride on the command line, or
                // follow an empty `+` continuation (what `async_imap` does).
                let initial = match tokens.get(3) {
                    Some(inline) => inline.clone(),
                    None => {
                        write_half.write_all(b"+ \r\n").await?;
                        let mut response = String::new();
                        if reader.read_line(&mut response).await? == 0 {
                            return Ok(());
                        }
                        response.trim_end_matches(['\r', '\n']).to_string()
                    }
                };
                let user = base64::engine::general_purpose::STANDARD
                    .decode(initial.as_bytes())
                    .ok()
                    .and_then(|payload| store.lock().unwrap().check_xoauth2(&payload));
                match user {
                    Some(user) => {
                        authenticated_user = Some(user);
                        write_half.write_all(format!("{tag} OK AUTHENTICATE completed\r\n").as_bytes()).await?;
                    }
                    None => {
                        // Like Gmail: a rejected token gets a base64 JSON error
                        // in a second continuation, which the client must
                        // acknowledge with an empty line before the tagged NO.
                        let error = base64::engine::general_purpose::STANDARD
                            .encode(br#"{"status":"401","schemes":"Bearer","scope":"https://mail.google.com/"}"#);
                        write_half.write_all(format!("+ {error}\r\n").as_bytes()).await?;
                        let mut ack = String::new();
                        reader.read_line(&mut ack).await?;
                        write_half.write_all(format!("{tag} NO AUTHENTICATE failed\r\n").as_bytes()).await?;
                    }
                }
            }
            "LIST" => {
                if authenticated_user.is_none() {
                    write_half.write_all(format!("{tag} NO not authenticated\r\n").as_bytes()).await?;
                    continue;
                }
                let names = store.lock().unwrap().mailbox_names();
                for name in names {
                    // RFC 6154 special-use attributes for the well-known
                    // mailboxes `Store::add_user` seeds (B8's mailbox-tree
                    // sorting reads these; case-insensitive by name here
                    // since this mock has no real per-mailbox "role"
                    // concept, only the name `add_user`/tests chose).
                    let special_use = match name.to_ascii_lowercase().as_str() {
                        "sent" => " \\Sent",
                        "drafts" => " \\Drafts",
                        "trash" => " \\Trash",
                        "archive" => " \\Archive",
                        "junk" | "spam" => " \\Junk",
                        _ => "",
                    };
                    let mut resp = format!("* LIST (\\HasNoChildren{special_use}) \"/\" ").into_bytes();
                    resp.extend_from_slice(&literal(name.as_bytes()));
                    resp.extend_from_slice(b"\r\n");
                    write_half.write_all(&resp).await?;
                }
                write_half.write_all(format!("{tag} OK LIST completed\r\n").as_bytes()).await?;
            }
            "EXAMINE" | "SELECT" => {
                if authenticated_user.is_none() {
                    write_half.write_all(format!("{tag} NO not authenticated\r\n").as_bytes()).await?;
                    continue;
                }
                let Some(mailbox_name) = tokens.get(2).cloned() else {
                    write_half.write_all(format!("{tag} BAD {verb} needs a mailbox\r\n").as_bytes()).await?;
                    continue;
                };
                // Scoped so the `MutexGuard` (not `Send`) is dropped before
                // any `.await` below -- otherwise the connection-handling
                // future itself stops being `Send`, which `tokio::spawn`
                // requires.
                let found = {
                    let guard = store.lock().unwrap();
                    guard.mailbox(&mailbox_name).map(|mb| (mb.messages.len(), mb.uid_validity, mb.uid_next))
                };
                let Some((exists, uid_validity, uid_next)) = found else {
                    write_half.write_all(format!("{tag} NO mailbox does not exist\r\n").as_bytes()).await?;
                    continue;
                };

                selected_mailbox = Some(mailbox_name);
                let readonly = if verb == "EXAMINE" { " [READ-ONLY]" } else { "" };
                write_half.write_all(b"* FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft)\r\n").await?;
                write_half.write_all(b"* OK [PERMANENTFLAGS ()] Flags permitted.\r\n").await?;
                write_half.write_all(format!("* {exists} EXISTS\r\n").as_bytes()).await?;
                write_half.write_all(b"* 0 RECENT\r\n").await?;
                write_half.write_all(format!("* OK [UIDVALIDITY {uid_validity}] UIDs valid\r\n").as_bytes()).await?;
                write_half.write_all(format!("* OK [UIDNEXT {uid_next}] Predicted next UID\r\n").as_bytes()).await?;
                write_half.write_all(format!("{tag} OK{readonly} {verb} completed\r\n").as_bytes()).await?;
            }
            "FETCH" => {
                let Some(mailbox_name) = selected_mailbox.clone() else {
                    write_half.write_all(format!("{tag} NO no mailbox selected\r\n").as_bytes()).await?;
                    continue;
                };
                let Some(seq_set) = tokens.get(2).cloned() else {
                    write_half.write_all(format!("{tag} BAD FETCH needs a sequence set\r\n").as_bytes()).await?;
                    continue;
                };
                let (start, end) = parse_range(&seq_set);
                let responses = {
                    let guard = store.lock().unwrap();
                    guard.mailbox(&mailbox_name).map(|mailbox| {
                        let total = mailbox.messages.len() as u32;
                        let end = end.min(total);
                        let mut responses = Vec::new();
                        for seq in start.max(1)..=end {
                            if let Some(msg) = mailbox.messages.get((seq - 1) as usize) {
                                responses.push(envelope_fetch_response(seq, msg));
                            }
                        }
                        responses
                    })
                };
                let Some(responses) = responses else {
                    write_half.write_all(format!("{tag} NO mailbox gone\r\n").as_bytes()).await?;
                    continue;
                };
                for r in responses {
                    write_half.write_all(&r).await?;
                }
                write_half.write_all(format!("{tag} OK FETCH completed\r\n").as_bytes()).await?;
            }
            "UID" if tokens.get(2).map(|s| s.eq_ignore_ascii_case("FETCH")).unwrap_or(false) => {
                let Some(mailbox_name) = selected_mailbox.clone() else {
                    write_half.write_all(format!("{tag} NO no mailbox selected\r\n").as_bytes()).await?;
                    continue;
                };
                let Some(uid_spec) = tokens.get(3) else {
                    write_half.write_all(format!("{tag} BAD UID FETCH needs a UID\r\n").as_bytes()).await?;
                    continue;
                };
                // The fetch-item spec is everything after the UID/range --
                // one bare token (`RFC822`) or several once `tokenize` has
                // split a parenthesized list like `(UID ENVELOPE)` on its
                // internal space (`imap.rs::fetch_body` sends the former,
                // `imap.rs::fetch_new_headers`/`fetch_headers_from` the
                // latter). Matched by substring rather than parsed
                // structurally -- esmail's client only ever sends exactly
                // these two shapes (see this module's doc comment), so a
                // full IMAP fetch-item-list grammar would be effort spent
                // on inputs that never arrive.
                let items = tokens[4..].join(" ").to_ascii_uppercase();
                let wants_envelope = items.contains("ENVELOPE");
                let wants_rfc822 = items.contains("RFC822");

                // A bare UID (`5`) is its own one-message range; `async_imap`
                // also sends open-ended ranges (`5:*`) for "from this UID
                // onward" fetches -- `*` here always means "the highest UID
                // this mailbox currently has", same as real IMAP.
                let (start, end) = if let Some((a, b)) = uid_spec.split_once(':') {
                    let a: u32 = a.parse().unwrap_or(1);
                    let b = if b == "*" { u32::MAX } else { b.parse().unwrap_or(a) };
                    (a, b)
                } else {
                    match uid_spec.parse::<u32>() {
                        Ok(uid) => (uid, uid),
                        Err(_) => {
                            write_half.write_all(format!("{tag} BAD invalid UID\r\n").as_bytes()).await?;
                            continue;
                        }
                    }
                };

                let matches: Vec<(u32, crate::store::StoredMessage)> = {
                    let guard = store.lock().unwrap();
                    guard
                        .mailbox(&mailbox_name)
                        .map(|mb| {
                            mb.messages
                                .iter()
                                .enumerate()
                                .filter(|(_, m)| m.uid >= start && m.uid <= end)
                                .map(|(i, m)| ((i + 1) as u32, m.clone()))
                                .collect()
                        })
                        .unwrap_or_default()
                };

                for (seq, msg) in &matches {
                    if wants_envelope {
                        write_half.write_all(&envelope_fetch_response(*seq, msg)).await?;
                    } else if wants_rfc822 {
                        let mut r = format!("* {seq} FETCH (UID {} RFC822 ", msg.uid).into_bytes();
                        r.extend_from_slice(&literal(&msg.raw));
                        r.extend_from_slice(b")\r\n");
                        write_half.write_all(&r).await?;
                    }
                }
                write_half.write_all(format!("{tag} OK UID FETCH completed\r\n").as_bytes()).await?;
            }
            "APPEND" => {
                if authenticated_user.is_none() {
                    write_half.write_all(format!("{tag} NO not authenticated\r\n").as_bytes()).await?;
                    continue;
                }
                let Some(mailbox_name) = tokens.get(2).cloned() else {
                    write_half.write_all(format!("{tag} BAD APPEND needs a mailbox\r\n").as_bytes()).await?;
                    continue;
                };
                // async_imap::Session::append sends `APPEND "<mailbox>"
                // {<len>}` -- no flags, no date-time, matching
                // `ImapCommand::Append`'s doc on what this server needs to
                // support. The literal size is `tokens[3]` as `{n}`.
                let Some(size_tok) = tokens.get(3) else {
                    write_half.write_all(format!("{tag} BAD APPEND needs a literal\r\n").as_bytes()).await?;
                    continue;
                };
                let Some(size) = size_tok.strip_prefix('{').and_then(|s| s.strip_suffix('}')).and_then(|s| s.parse::<usize>().ok()) else {
                    write_half.write_all(format!("{tag} BAD invalid literal size\r\n").as_bytes()).await?;
                    continue;
                };

                write_half.write_all(b"+ send literal data\r\n").await?;

                let mut raw = vec![0u8; size];
                reader.read_exact(&mut raw).await?;
                // The client follows the literal's exact byte count with a
                // trailing CRLF that terminates the APPEND command line --
                // not part of the message, so it's read and discarded here
                // rather than appended to `raw`.
                let mut trailing_crlf = [0u8; 2];
                reader.read_exact(&mut trailing_crlf).await?;

                let uid = store.lock().unwrap().deliver(&mailbox_name, raw);
                write_half.write_all(format!("{tag} OK [APPENDUID 1 {uid}] APPEND completed\r\n").as_bytes()).await?;
            }
            "STORE" | "UID" if verb == "STORE" || tokens.get(2).map(|s| s.eq_ignore_ascii_case("STORE")).unwrap_or(false) => {
                let is_uid = verb == "UID";
                let Some(mailbox_name) = selected_mailbox.clone() else {
                    write_half.write_all(format!("{tag} NO no mailbox selected\r\n").as_bytes()).await?;
                    continue;
                };
                // For plain STORE: tokens = [tag, STORE, set, mode, flags...]
                // For UID STORE: tokens = [tag, UID, STORE, set, mode, flags...]
                let base = if is_uid { 3 } else { 2 };
                let (Some(set_tok), Some(mode_tok)) = (tokens.get(base), tokens.get(base + 1)) else {
                    write_half.write_all(format!("{tag} BAD STORE needs a set and mode\r\n").as_bytes()).await?;
                    continue;
                };
                let mode = mode_tok.to_ascii_uppercase();
                let flags_joined = tokens[base + 2..].join(" ");
                let flags: Vec<String> = flags_joined
                    .trim_start_matches('(')
                    .trim_end_matches(')')
                    .split_whitespace()
                    .map(|s| s.to_string())
                    .collect();
                let (start, end) = parse_range(set_tok);

                let target_uid = if is_uid {
                    // A single UID (or a small range); esmail's client only
                    // ever sends one at a time, so treat start as the uid.
                    Some(start)
                } else {
                    None
                };

                let result = {
                    let mut guard = store.lock().unwrap();
                    match guard.mailbox_mut(&mailbox_name) {
                        None => None,
                        Some(mailbox) => {
                        // Resolve the sequence-number set to UIDs first (for
                        // plain STORE) so `store_flags` (UID-addressed) and
                        // the response's sequence number both line up.
                        let candidates: Vec<(u32, u32)> = if let Some(uid) = target_uid {
                            mailbox.messages.iter().enumerate()
                                .filter(|(_, m)| m.uid == uid)
                                .map(|(i, m)| ((i + 1) as u32, m.uid))
                                .collect()
                        } else {
                            mailbox.messages.iter().enumerate()
                                .filter(|(i, _)| { let seq = (*i + 1) as u32; seq >= start.max(1) && seq <= end })
                                .map(|(i, m)| ((i + 1) as u32, m.uid))
                                .collect()
                        };
                        let (add, remove): (Vec<String>, Vec<String>) = match mode.as_str() {
                            "+FLAGS" | "+FLAGS.SILENT" => (flags.clone(), Vec::new()),
                            "-FLAGS" | "-FLAGS.SILENT" => (Vec::new(), flags.clone()),
                            // Plain "FLAGS"/"FLAGS.SILENT" replaces the set --
                            // esmail's client never sends this (only +/-),
                            // but handle it for completeness: clear via a
                            // wildcard remove, then add the new set.
                            _ => (flags.clone(), vec!["\\Seen".into(), "\\Flagged".into(), "\\Deleted".into(), "\\Answered".into(), "\\Draft".into()]),
                        };
                        let silent = mode.ends_with(".SILENT");
                        let mut responses = Vec::new();
                        for (seq, uid) in candidates {
                            if mailbox.store_flags(uid, &add, &remove).is_some() && !silent {
                                if let Some(msg) = mailbox.messages.iter().find(|m| m.uid == uid) {
                                    responses.push(flags_fetch_response(seq, msg));
                                }
                            }
                        }
                        Some(responses)
                        }
                    }
                };
                match result {
                    Some(responses) => {
                        for r in responses {
                            write_half.write_all(&r).await?;
                        }
                        write_half.write_all(format!("{tag} OK STORE completed\r\n").as_bytes()).await?;
                    }
                    None => {
                        write_half.write_all(format!("{tag} NO mailbox gone\r\n").as_bytes()).await?;
                    }
                }
            }
            "COPY" | "UID" if verb == "COPY" || tokens.get(2).map(|s| s.eq_ignore_ascii_case("COPY")).unwrap_or(false) => {
                let is_uid = verb == "UID";
                let Some(mailbox_name) = selected_mailbox.clone() else {
                    write_half.write_all(format!("{tag} NO no mailbox selected\r\n").as_bytes()).await?;
                    continue;
                };
                let base = if is_uid { 3 } else { 2 };
                let (Some(set_tok), Some(dest)) = (tokens.get(base), tokens.get(base + 1)) else {
                    write_half.write_all(format!("{tag} BAD COPY needs a set and destination\r\n").as_bytes()).await?;
                    continue;
                };
                let (start, _end) = parse_range(set_tok);
                // esmail's client only ever COPYs one message (the
                // move-to-Trash/Archive fallback); a sequence-number COPY
                // would need resolving start..end to UIDs first, same as
                // STORE above -- not needed for what's exercised here.
                // Resolved to `Option<u32>` (rather than matching straight
                // into the `None` arm's `.await`) so the `MutexGuard` --
                // which, having a `Drop` impl, stays lexically "live" until
                // its enclosing block ends, not just its last use -- is
                // dropped before any `.await` in this function, which
                // `tokio::spawn`'s `Send` bound on the whole connection
                // future requires.
                let resolved_uid = if is_uid {
                    Some(start)
                } else {
                    let guard = store.lock().unwrap();
                    guard.mailbox(&mailbox_name).and_then(|mb| mb.messages.get((start.saturating_sub(1)) as usize)).map(|m| m.uid)
                };
                let Some(uid) = resolved_uid else {
                    write_half.write_all(format!("{tag} NO no such message\r\n").as_bytes()).await?;
                    continue;
                };
                let copied = store.lock().unwrap().copy_message(&mailbox_name, uid, dest);
                match copied {
                    Some(new_uid) => {
                        write_half.write_all(format!("{tag} OK [COPYUID 1 {uid} {new_uid}] COPY completed\r\n").as_bytes()).await?;
                    }
                    None => {
                        write_half.write_all(format!("{tag} NO no such message\r\n").as_bytes()).await?;
                    }
                }
            }
            "EXPUNGE" | "UID" if verb == "EXPUNGE" || tokens.get(2).map(|s| s.eq_ignore_ascii_case("EXPUNGE")).unwrap_or(false) => {
                let Some(mailbox_name) = selected_mailbox.clone() else {
                    write_half.write_all(format!("{tag} NO no mailbox selected\r\n").as_bytes()).await?;
                    continue;
                };
                // UID EXPUNGE's argument (a UID set to limit the expunge to)
                // is ignored -- esmail's client only ever calls plain
                // EXPUNGE (see imap.rs::move_message), and this mock never
                // has more than the caller's own \Deleted message anyway.
                let removed = {
                    let mut guard = store.lock().unwrap();
                    guard.mailbox_mut(&mailbox_name).map(|mb| mb.expunge())
                };
                match removed {
                    Some(removed) => {
                        // Real EXPUNGE reports the *sequence numbers* that
                        // were removed, at the time each was removed --
                        // esmail's client (`imap.rs::move_message`) doesn't
                        // read this stream's contents at all, only that the
                        // command completed OK, so a simplified "1" per
                        // removed message (rather than exact renumbering) is
                        // sufficient here.
                        for _ in &removed {
                            write_half.write_all(b"* 1 EXPUNGE\r\n").await?;
                        }
                        write_half.write_all(format!("{tag} OK EXPUNGE completed\r\n").as_bytes()).await?;
                    }
                    None => {
                        write_half.write_all(format!("{tag} NO mailbox gone\r\n").as_bytes()).await?;
                    }
                }
            }
            "STATUS" => {
                if authenticated_user.is_none() {
                    write_half.write_all(format!("{tag} NO not authenticated\r\n").as_bytes()).await?;
                    continue;
                }
                let Some(mailbox_name) = tokens.get(2).cloned() else {
                    write_half.write_all(format!("{tag} BAD STATUS needs a mailbox\r\n").as_bytes()).await?;
                    continue;
                };
                let found = {
                    let guard = store.lock().unwrap();
                    guard.mailbox(&mailbox_name).map(|mb| (mb.messages.len() as u32, mb.unseen_count()))
                };
                let Some((exists, unseen)) = found else {
                    write_half.write_all(format!("{tag} NO mailbox does not exist\r\n").as_bytes()).await?;
                    continue;
                };
                let mut resp = b"* STATUS ".to_vec();
                resp.extend_from_slice(&literal(mailbox_name.as_bytes()));
                resp.extend_from_slice(format!(" (MESSAGES {exists} UNSEEN {unseen})\r\n").as_bytes());
                write_half.write_all(&resp).await?;
                write_half.write_all(format!("{tag} OK STATUS completed\r\n").as_bytes()).await?;
            }
            "LOGOUT" => {
                write_half.write_all(b"* BYE logging out\r\n").await?;
                write_half.write_all(format!("{tag} OK LOGOUT completed\r\n").as_bytes()).await?;
                return Ok(());
            }
            "CAPABILITY" => {
                write_half.write_all(b"* CAPABILITY IMAP4rev1 IDLE\r\n").await?;
                write_half.write_all(format!("{tag} OK CAPABILITY completed\r\n").as_bytes()).await?;
            }
            "IDLE" => {
                if authenticated_user.is_none() {
                    write_half.write_all(format!("{tag} NO not authenticated\r\n").as_bytes()).await?;
                    continue;
                }
                let Some(mailbox_name) = selected_mailbox.clone() else {
                    write_half.write_all(format!("{tag} NO no mailbox selected\r\n").as_bytes()).await?;
                    continue;
                };
                write_half.write_all(b"+ idling\r\n").await?;
                let mut changes = store.lock().unwrap().notify.subscribe();
                // A fresh `String` rather than reusing the outer `line`: the
                // outer loop's `line` gets shadowed to `&str` a few lines up
                // (`let line = line.trim_end_matches(...)`), so that binding
                // isn't a `String` (with `.clear()`/`read_line`'s `&mut
                // String`) in this scope any more.
                let mut idle_line = String::new();
                loop {
                    idle_line.clear();
                    tokio::select! {
                        n = reader.read_line(&mut idle_line) => {
                            let n = n?;
                            if n == 0 {
                                return Ok(()); // client closed the connection mid-IDLE
                            }
                            if idle_line.trim_end_matches(['\r', '\n']).eq_ignore_ascii_case("DONE") {
                                write_half.write_all(format!("{tag} OK IDLE terminated\r\n").as_bytes()).await?;
                                break;
                            }
                            // RFC 2177 only expects DONE while idling; anything
                            // else is silently ignored rather than treated as a
                            // new command, matching real servers' behavior.
                        }
                        changed = changes.recv() => {
                            // `Lagged` (a burst of deliveries overran the
                            // channel's buffer) and `Closed` both just mean
                            // "keep idling" here -- a lagged receiver still
                            // gets the *next* delivery, and closed can't
                            // happen while `store` (which owns the sender)
                            // outlives every connection.
                            if let Ok(mailbox) = changed {
                                if mailbox == mailbox_name {
                                    let exists = store.lock().unwrap().mailbox(&mailbox_name).map(|mb| mb.messages.len());
                                    if let Some(exists) = exists {
                                        write_half.write_all(format!("* {exists} EXISTS\r\n").as_bytes()).await?;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            "NOOP" => {
                write_half.write_all(format!("{tag} OK NOOP completed\r\n").as_bytes()).await?;
            }
            _ => {
                write_half.write_all(format!("{tag} BAD unknown or unsupported command\r\n").as_bytes()).await?;
            }
        }
    }
}

/// Splits an IMAP command line into tokens, treating a `"..."` run
/// (unescaping `\"` and `\\`) as a single token -- enough for the small,
/// self-controlled command set esmail's client actually sends (see
/// `async-imap`'s `quote!` macro, which always double-quotes its string
/// arguments).
fn tokenize(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut chars = line.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c == ' ' {
            chars.next();
            continue;
        }
        if c == '"' {
            chars.next();
            let mut s = String::new();
            while let Some(c) = chars.next() {
                match c {
                    '"' => break,
                    '\\' => {
                        if let Some(next) = chars.next() {
                            s.push(next);
                        }
                    }
                    _ => s.push(c),
                }
            }
            tokens.push(s);
        } else {
            let mut s = String::new();
            while let Some(&c) = chars.peek() {
                if c == ' ' {
                    break;
                }
                s.push(c);
                chars.next();
            }
            tokens.push(s);
        }
    }
    tokens
}

/// Parses a `FETCH` sequence set of the shapes esmail's client sends:
/// a single number (`"5"`) or an inclusive range (`"1:50"`).
fn parse_range(spec: &str) -> (u32, u32) {
    if let Some((a, b)) = spec.split_once(':') {
        let a: u32 = a.parse().unwrap_or(1);
        let b: u32 = b.parse().unwrap_or(a);
        (a, b)
    } else {
        let n: u32 = spec.parse().unwrap_or(1);
        (n, n)
    }
}

