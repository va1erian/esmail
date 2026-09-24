//! SMTP sending (B7 of PLAN.md), following the same actor pattern as
//! `imap.rs`/`db.rs`: a tokio task behind an mpsc channel, so sending never
//! blocks the UI thread.
//!
//! This module only builds and delivers one message at a time; it does not
//! know about drafts or the retry queue at all. Those live in `db.rs`'s
//! `outbox`/`drafts` tables and `main.rs`'s `try_send_outbox_item`/
//! `poll_outbox`/`autosave_draft_now`: clicking Send in `main.rs` durably
//! enqueues the message first (keyed by the same `ComposeId` the compose
//! window itself is named by), then this module attempts it; on
//! [`SmtpEvent::Error`] the outbox row is backed off and retried
//! automatically rather than depending on the user noticing and re-sending.
//!
//! **`APPEND` to Sent now lands, `APPEND` to Drafts still doesn't** -- a
//! draft is autosaved into the local `drafts` table (`db.rs`), not to the
//! account's real IMAP `Drafts` mailbox, so it isn't visible from another
//! mail client the way a real `APPEND ... \Draft` would be. `SmtpEvent::Sent`
//! carries the sent message's raw RFC822 bytes so `main.rs` can hand them to
//! `ImapCommand::Append` and save a copy to the account's Sent folder -- see
//! that command's doc in `imap.rs` for what's still a bounded subset of the
//! original ask (a hardcoded "Sent" mailbox name, not real `\Sent`
//! special-use-flag discovery with a name-based fallback).

use lettre::message::{Attachment, Message, MultiPart, SinglePart, header::ContentType};
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};
use secrecy::ExposeSecret;
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use crate::auth::Auth;
use crate::compose::{ComposeId, ComposeState};
use crate::config::TlsMode;

/// Everything sending needs that isn't in the [`ComposeState`] itself.
pub struct SmtpAccount {
    pub host: String,
    pub port: u16,
    pub tls: TlsMode,
    pub username: String,
    pub auth: Auth,
    /// `"Display Name <address@host>"` (or just the bare address) — this
    /// account's own address, used as the `From`.
    pub from_address: String,
}

pub enum SmtpCommand {
    /// `id` names the compose window the message came from. It comes back on
    /// the resulting event, so several sends can be in flight at once and
    /// each result reaches the right window.
    Send { id: ComposeId, account: SmtpAccount, compose: ComposeState },
}

#[derive(Debug)]
pub enum SmtpEvent {
    /// `raw` is the exact RFC822 bytes handed to the SMTP transport, so a
    /// caller can `APPEND` an identical copy to Sent without re-building the
    /// message (which would risk it drifting from what was actually sent —
    /// a fresh `Message-ID`, say, from calling `build_message` a second
    /// time).
    Sent { id: ComposeId, raw: Vec<u8> },
    Error { id: ComposeId, error: String },
}

pub struct SmtpActor {
    cmd_rx: mpsc::Receiver<SmtpCommand>,
    event_tx: mpsc::Sender<SmtpEvent>,
}

impl SmtpActor {
    pub fn spawn(runtime: &Handle, cmd_rx: mpsc::Receiver<SmtpCommand>, event_tx: mpsc::Sender<SmtpEvent>) {
        let mut actor = SmtpActor { cmd_rx, event_tx };
        runtime.spawn(async move {
            actor.run().await;
        });
    }

    async fn run(&mut self) {
        while let Some(cmd) = self.cmd_rx.recv().await {
            match cmd {
                SmtpCommand::Send { id, account, compose } => match Self::send(&account, &compose).await {
                    Ok(raw) => {
                        let _ = self.event_tx.send(SmtpEvent::Sent { id, raw }).await;
                    }
                    Err(e) => {
                        let _ = self.event_tx.send(SmtpEvent::Error { id, error: e.to_string() }).await;
                    }
                },
            }
        }
    }

    async fn send(account: &SmtpAccount, compose: &ComposeState) -> anyhow::Result<Vec<u8>> {
        let message = build_message(account, compose)?;
        let raw = message.formatted();
        let transport = build_transport(account).await?;
        transport.send(message).await?;
        Ok(raw)
    }
}

/// `async` because an OAuth account may have to refresh its access token
/// first. The transport is built per send, so the token it carries is never
/// older than the send that uses it.
async fn build_transport(account: &SmtpAccount) -> anyhow::Result<AsyncSmtpTransport<Tokio1Executor>> {
    let secret = account.auth.secret().await?;
    let credentials = Credentials::new(account.username.clone(), secret.expose_secret().to_string());
    let builder = match account.tls {
        TlsMode::Ssl => AsyncSmtpTransport::<Tokio1Executor>::relay(&account.host)?,
        TlsMode::StartTls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&account.host)?,
        // Only useful for a local/test server -- same caveat as the identical
        // TlsMode::None case in config.rs's doc comment.
        TlsMode::None => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&account.host),
    };
    let builder = builder.port(account.port).credentials(credentials);
    // lettre otherwise picks from what the server advertises, in its own
    // order; an OAuth account must use XOAUTH2 (its "password" is a token).
    let builder = if account.auth.is_oauth() { builder.authentication(vec![Mechanism::Xoauth2]) } else { builder };
    Ok(builder.build())
}

/// Builds the message `compose` describes, or says why it cannot be sent (an
/// address that does not parse, say). Public so a frontend can check a message
/// before it is sent, with the very code that will send it.
pub fn build_message(account: &SmtpAccount, compose: &ComposeState) -> anyhow::Result<Message> {
    let mut builder = Message::builder()
        .from(account.from_address.parse()?)
        .subject(compose.subject.clone());
    for address in split_addresses(&compose.to) {
        builder = builder.to(address.parse()?);
    }
    for address in split_addresses(&compose.cc) {
        builder = builder.cc(address.parse()?);
    }
    for address in split_addresses(&compose.bcc) {
        builder = builder.bcc(address.parse()?);
    }
    if let Some(id) = &compose.in_reply_to {
        builder = builder.in_reply_to(id.clone());
    }
    if let Some(id) = &compose.references {
        builder = builder.references(id.clone());
    }

    if compose.attachments.is_empty() {
        Ok(builder.body(compose.body.clone())?)
    } else {
        let mut multipart = MultiPart::mixed().singlepart(SinglePart::plain(compose.body.clone()));
        for (filename, data) in &compose.attachments {
            let content_type = ContentType::parse(guess_mime_type(filename))
                .unwrap_or_else(|_| ContentType::parse("application/octet-stream").expect("static value is valid"));
            multipart = multipart.singlepart(Attachment::new(filename.clone()).body(data.clone(), content_type));
        }
        Ok(builder.multipart(multipart)?)
    }
}

/// Comma-separated addresses, e.g. from a To/Cc/Bcc field, trimmed and with
/// empty entries (a trailing comma, blank field) dropped.
fn split_addresses(field: &str) -> Vec<String> {
    field.split(',').map(str::trim).filter(|a| !a.is_empty()).map(str::to_string).collect()
}

/// A small built-in extension → MIME type table, rather than a
/// `mime_guess`-style dependency for what's realistically going to be a
/// handful of common attachment types. Anything unrecognized becomes
/// `application/octet-stream`, which every mail client treats as "just an
/// attachment, no special handling" -- a safe, generic fallback rather than
/// a guess that could be wrong.
fn guess_mime_type(filename: &str) -> &'static str {
    let ext = filename.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "txt" => "text/plain",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "zip" => "application/zip",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_addresses_trims_and_drops_empty_entries() {
        assert_eq!(
            split_addresses(" alice@example.com ,bob@example.com,, carol@example.com"),
            vec!["alice@example.com", "bob@example.com", "carol@example.com"]
        );
    }

    #[test]
    fn split_addresses_of_an_empty_field_is_empty() {
        assert!(split_addresses("").is_empty());
        assert!(split_addresses("   ").is_empty());
    }

    #[test]
    fn guess_mime_type_recognizes_common_extensions() {
        assert_eq!(guess_mime_type("report.pdf"), "application/pdf");
        assert_eq!(guess_mime_type("photo.JPG"), "image/jpeg");
    }

    #[test]
    fn guess_mime_type_falls_back_to_octet_stream() {
        assert_eq!(guess_mime_type("mystery.xyz"), "application/octet-stream");
        assert_eq!(guess_mime_type("no_extension"), "application/octet-stream");
    }

    #[test]
    fn build_message_with_no_attachments_produces_a_plain_body() {
        let account = SmtpAccount {
            host: "smtp.example.com".to_string(),
            port: 465,
            tls: TlsMode::Ssl,
            username: "alice@example.com".to_string(),
            auth: Auth::password("hunter2"),
            from_address: "Alice <alice@example.com>".to_string(),
        };
        let compose = ComposeState {
            to: "bob@example.com".to_string(),
            subject: "Hello".to_string(),
            body: "Hi Bob".to_string(),
            ..Default::default()
        };
        let message = build_message(&account, &compose).expect("should build");
        let raw = String::from_utf8_lossy(&message.formatted()).to_string();
        assert!(raw.contains("Hi Bob"));
        assert!(raw.contains("Subject: Hello"));
    }

    #[test]
    fn build_message_sets_threading_headers_when_present() {
        let account = SmtpAccount {
            host: "smtp.example.com".to_string(),
            port: 465,
            tls: TlsMode::Ssl,
            username: "alice@example.com".to_string(),
            auth: Auth::password("hunter2"),
            from_address: "alice@example.com".to_string(),
        };
        let compose = ComposeState {
            to: "bob@example.com".to_string(),
            subject: "Re: Hello".to_string(),
            body: "Hi Bob".to_string(),
            in_reply_to: Some("<abc@example.com>".to_string()),
            references: Some("<abc@example.com>".to_string()),
            ..Default::default()
        };
        let message = build_message(&account, &compose).expect("should build");
        let raw = String::from_utf8_lossy(&message.formatted()).to_string();
        assert!(raw.contains("In-Reply-To: <abc@example.com>"));
        assert!(raw.contains("References: <abc@example.com>"));
    }

    #[test]
    fn build_message_rejects_an_unparseable_recipient_rather_than_silently_dropping_it() {
        let account = SmtpAccount {
            host: "smtp.example.com".to_string(),
            port: 465,
            tls: TlsMode::Ssl,
            username: "alice@example.com".to_string(),
            auth: Auth::password("hunter2"),
            from_address: "alice@example.com".to_string(),
        };
        let compose = ComposeState {
            to: "not an email address".to_string(),
            subject: "Hello".to_string(),
            body: "Hi".to_string(),
            ..Default::default()
        };
        assert!(build_message(&account, &compose).is_err());
    }

    #[test]
    fn build_message_with_an_attachment_includes_it_as_a_separate_part() {
        let account = SmtpAccount {
            host: "smtp.example.com".to_string(),
            port: 465,
            tls: TlsMode::Ssl,
            username: "alice@example.com".to_string(),
            auth: Auth::password("hunter2"),
            from_address: "alice@example.com".to_string(),
        };
        let compose = ComposeState {
            to: "bob@example.com".to_string(),
            subject: "Files".to_string(),
            body: "see attached".to_string(),
            attachments: vec![("notes.txt".to_string(), b"hello world".to_vec())],
            ..Default::default()
        };
        let message = build_message(&account, &compose).expect("should build");
        let raw = String::from_utf8_lossy(&message.formatted()).to_string();
        assert!(raw.contains("notes.txt"));
        assert!(raw.contains("see attached"));
    }
}
