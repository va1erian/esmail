//! Drives esmail's real `ImapActor`/`SmtpActor` (the exact command/event
//! channels `main.rs` wires up) against `mail-mock-server`, an in-process
//! IMAP + SMTP server built for this purpose -- see
//! `crates/mail-mock-server/README.md` for why a hand-rolled server and how
//! its TLS trust works.
//!
//! Needs the bundled test CA trusted (see that README); tests here check
//! for `ESMAIL_TEST_CA_TRUSTED=1` and skip with an explanatory message
//! rather than failing confusingly on a machine that hasn't set that up.
//! CI does the trust step then sets the var -- see `.github/workflows/ci.yml`.
//!
//! Fast, correctness-focused tests run by default; `#[ignore]`d
//! concurrent-load stress tests run with `-- --ignored`.

use std::time::Duration;

use esmail::compose::ComposeState;
use esmail::idle_watch;
use esmail::imap::{ImapActor, ImapCommand, ImapEvent};
use esmail::progress::{Progress, ProgressKind};
use esmail::smtp::{SmtpAccount, SmtpActor, SmtpCommand, SmtpEvent};
use esmail::auth::Auth;
use tokio::sync::mpsc;
use tokio::time::timeout;

use mail_mock_server::fixtures::{TEST_PASSWORD, TEST_USER};

const RECV_TIMEOUT: Duration = Duration::from_secs(20);

fn ca_trusted() -> bool {
    std::env::var("ESMAIL_TEST_CA_TRUSTED").is_ok()
}

macro_rules! skip_unless_ca_trusted {
    () => {
        if !ca_trusted() {
            eprintln!(
                "skipping: set ESMAIL_TEST_CA_TRUSTED=1 once mail-mock-server/certs/ca.crt is trusted \
                 (see crates/mail-mock-server/README.md)"
            );
            return;
        }
    };
}

/// A running mock server plus a connected `ImapActor`'s command/event
/// handles -- everything a test needs to drive esmail's client exactly as
/// `main.rs` does.
struct Harness {
    server: mail_mock_server::RunningServer,
    imap_cmd: mpsc::Sender<ImapCommand>,
    imap_evt: mpsc::Receiver<ImapEvent>,
}

async fn start_harness(inbox_count: u32) -> Harness {
    let store = mail_mock_server::new_store();
    {
        let mut guard = store.lock().unwrap();
        mail_mock_server::fixtures::seed(&mut guard, inbox_count);
    }
    let server = mail_mock_server::start(store).await.expect("start mock servers");

    let (imap_cmd_tx, imap_cmd_rx) = mpsc::channel(32);
    let (imap_evt_tx, mut imap_evt_rx) = mpsc::channel(32);
    ImapActor::spawn(&tokio::runtime::Handle::current(), imap_cmd_rx, imap_evt_tx);

    imap_cmd_tx
        .send(ImapCommand::Connect {
            host: "localhost".to_string(),
            port: server.imap_addr.port(),
            username: TEST_USER.to_string(),
            auth: Auth::password(TEST_PASSWORD),
        })
        .await
        .unwrap();

    match timeout(RECV_TIMEOUT, imap_evt_rx.recv()).await {
        Ok(Some(ImapEvent::Connected)) => {}
        other => panic!("expected Connected, got {other:?} (is the test CA actually trusted?)"),
    }

    Harness { server, imap_cmd: imap_cmd_tx, imap_evt: imap_evt_rx }
}

#[tokio::test]
async fn connect_and_fetch_mailboxes() {
    skip_unless_ca_trusted!();
    let mut h = start_harness(0).await;

    h.imap_cmd.send(ImapCommand::FetchMailboxes).await.unwrap();
    match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::Mailboxes(mailboxes) => {
            let names: Vec<&str> = mailboxes.iter().map(|m| m.name.as_str()).collect();
            assert!(names.contains(&"INBOX"));
            assert!(names.contains(&"Sent"));
            // B8: the mock server advertises RFC 6154 special-use
            // attributes for the well-known mailboxes `Store::add_user`
            // seeds -- `imap.rs::fetch_mailboxes` should have picked those
            // up rather than falling back to name-based guessing for them.
            let sent = mailboxes.iter().find(|m| m.name == "Sent").expect("Sent present");
            assert_eq!(sent.special_use, Some(esmail::imap::SpecialUse::Sent));
            assert_eq!(sent.delimiter.as_deref(), Some("/"));
        }
        other => panic!("expected Mailboxes, got {other:?}"),
    }
}

#[tokio::test]
async fn fetch_headers_paginates_the_seeded_inbox() {
    skip_unless_ca_trusted!();
    // 2 fixture messages + 120 plain ones = 122, which at 50/page is 3 pages.
    let mut h = start_harness(120).await;

    h.imap_cmd.send(ImapCommand::FetchHeaders { mailbox: "INBOX".to_string(), page: 1, req_id: 1 }).await.unwrap();
    match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::Headers { headers, total_pages, mailbox_state, .. } => {
            assert_eq!(headers.len(), 50);
            assert_eq!(total_pages, 3);
            assert!(mailbox_state.uid_next > 122);
        }
        other => panic!("expected Headers, got {other:?}"),
    }

    h.imap_cmd.send(ImapCommand::FetchHeaders { mailbox: "INBOX".to_string(), page: 3, req_id: 2 }).await.unwrap();
    match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::Headers { headers, page, .. } => {
            assert_eq!(page, 3);
            assert_eq!(headers.len(), 22); // 122 - 2*50
        }
        other => panic!("expected Headers, got {other:?}"),
    }
}

/// Regression test for GitHub issue #13 ("Message body sometimes gets
/// permanently stuck on 'Loading message...'"). The root cause: a failed
/// `FetchBody` used to come back as a generic `ImapEvent::Error` with no
/// `uid`/`req_id` on it, so `main.rs` had no way to tell it apart from an
/// unrelated error and clear the "Loading message..." placeholder
/// `open_message` sets before the fetch -- the placeholder stayed up
/// forever. Fetching a UID that doesn't exist is the simplest way to force
/// `ImapActor::fetch_body`'s "Message not found" error path (the same code
/// path a dropped connection or an exhausted worker reconnect would hit);
/// this asserts the reply is a `BodyFailed` carrying the exact `uid`/
/// `req_id` the request went out with, which `main.rs` now matches the same
/// way it matches a successful `Body` reply.
#[tokio::test]
async fn fetch_body_for_a_missing_uid_reports_a_matchable_body_failure() {
    skip_unless_ca_trusted!();
    let mut h = start_harness(0).await;

    let missing_uid = 999_999;
    h.imap_cmd.send(ImapCommand::FetchBody { mailbox: "INBOX".to_string(), uid: missing_uid, req_id: 7 }).await.unwrap();
    match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::BodyFailed { uid, req_id, error } => {
            assert_eq!(uid, missing_uid);
            assert_eq!(req_id, 7);
            assert!(!error.is_empty());
        }
        other => panic!("expected BodyFailed, got {other:?} -- a plain Error here is exactly the bug issue #13 describes: main.rs can't attribute it to this request and clear the loading placeholder"),
    }
}

/// Regression test for GitHub issue #80: a body fetch that fails against an
/// *existing* body-worker session must be retried once on a fresh connection
/// before the failure is surfaced, rather than reporting `BodyFailed`
/// immediately. The mock server drops the connection on the next RFC822
/// fetch to simulate the dead/aborted socket from the report; the worker's
/// retry reconnects and the fetch then succeeds.
#[tokio::test]
async fn fetch_body_retries_once_on_a_fresh_connection_after_a_dropped_socket() {
    skip_unless_ca_trusted!();
    let mut h = start_harness(0).await;

    h.imap_cmd.send(ImapCommand::FetchHeaders { mailbox: "INBOX".to_string(), page: 1, req_id: 1 }).await.unwrap();
    let headers = match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::Headers { headers, .. } => headers,
        other => panic!("expected Headers, got {other:?}"),
    };
    let uid = headers.iter().find(|h| h.subject.contains("Report with image")).expect("fixture message present").uid;

    // A successful fetch first, so the body worker has its own live session
    // and the injected failure really hits an existing one.
    h.imap_cmd.send(ImapCommand::FetchBody { mailbox: "INBOX".to_string(), uid, req_id: 2 }).await.unwrap();
    assert!(matches!(
        timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap(),
        ImapEvent::Body { req_id: 2, .. }
    ));

    h.server.store.lock().unwrap().drop_next_rfc822_fetch = true;
    h.imap_cmd.send(ImapCommand::FetchBody { mailbox: "INBOX".to_string(), uid, req_id: 3 }).await.unwrap();
    match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::Body { uid: got_uid, req_id: 3, .. } => assert_eq!(got_uid, uid),
        other => panic!(
            "expected Body after the retry, got {other:?} -- issue #80: a FetchBody that fails on an existing session must be retried once on a fresh connection"
        ),
    }
}

#[tokio::test]
async fn fetch_body_renders_html_resolves_cid_and_finds_the_attachment() {
    skip_unless_ca_trusted!();
    let mut h = start_harness(0).await;

    h.imap_cmd.send(ImapCommand::FetchHeaders { mailbox: "INBOX".to_string(), page: 1, req_id: 1 }).await.unwrap();
    let headers = match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::Headers { headers, .. } => headers,
        other => panic!("expected Headers, got {other:?}"),
    };
    let report = headers.iter().find(|h| h.subject.contains("Report with image")).expect("fixture message present");

    h.imap_cmd.send(ImapCommand::FetchBody { mailbox: "INBOX".to_string(), uid: report.uid, req_id: 2 }).await.unwrap();
    match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::Body { html, attachments, .. } => {
            // render.rs resolves cid: to an inline data: URL rather than
            // leaving the original cid: reference in the markup.
            assert!(html.contains("data:image/png"), "expected an inlined cid: image, got: {html}");
            assert!(!html.contains("cid:pixel@example.com"));
            assert_eq!(attachments.len(), 1);
            assert_eq!(attachments[0].filename, "notes.txt");
        }
        other => panic!("expected Body, got {other:?}"),
    }
}

/// "Export message" saves the message's exact raw source, so a specific
/// real-world email can be kept as a test case. Round-trip it: what lands in
/// the file must be the full RFC822 message (headers, the cid: image part,
/// the attachment) -- proven by rendering the file the way `ESMAIL_PREVIEW=
/// x.eml` does and getting the same inlined image a live `FetchBody` gives.
#[tokio::test]
async fn export_message_writes_the_raw_rfc822_source() {
    skip_unless_ca_trusted!();
    let mut h = start_harness(0).await;

    h.imap_cmd.send(ImapCommand::FetchHeaders { mailbox: "INBOX".to_string(), page: 1, req_id: 1 }).await.unwrap();
    let headers = match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::Headers { headers, .. } => headers,
        other => panic!("expected Headers, got {other:?}"),
    };
    let report = headers.iter().find(|h| h.subject.contains("Report with image")).expect("fixture message present");

    let dir = std::env::temp_dir().join(format!("esmail-export-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("report.eml");

    h.imap_cmd
        .send(ImapCommand::ExportMessage { mailbox: "INBOX".to_string(), uid: report.uid, path: path.clone() })
        .await
        .unwrap();
    match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::Exported { path: exported } => assert_eq!(exported, path),
        other => panic!("expected Exported, got {other:?}"),
    }

    let raw = std::fs::read(&path).unwrap();
    let text = String::from_utf8_lossy(&raw);
    assert!(text.contains("Subject: Report with image"), "not a raw RFC822 message: {text}");
    let html = esmail::render::render_message(&raw);
    assert!(html.contains("data:image/png"), "the exported file did not round-trip through render_message: {html}");
    assert_eq!(esmail::render::extract_attachments(&raw).len(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn export_message_for_a_missing_uid_reports_a_failure_and_writes_nothing() {
    skip_unless_ca_trusted!();
    let mut h = start_harness(0).await;

    let dir = std::env::temp_dir().join(format!("esmail-export-test-missing-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("nothing.eml");

    h.imap_cmd
        .send(ImapCommand::ExportMessage { mailbox: "INBOX".to_string(), uid: 999_999, path: path.clone() })
        .await
        .unwrap();
    match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::ExportFailed { error } => assert!(!error.is_empty()),
        other => panic!("expected ExportFailed, got {other:?}"),
    }
    assert!(!path.exists(), "a failed export must not leave a file behind");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn fetch_body_decodes_an_rfc2047_encoded_unicode_subject() {
    skip_unless_ca_trusted!();
    let mut h = start_harness(0).await;

    h.imap_cmd.send(ImapCommand::FetchHeaders { mailbox: "INBOX".to_string(), page: 1, req_id: 1 }).await.unwrap();
    let headers = match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::Headers { headers, .. } => headers,
        other => panic!("expected Headers, got {other:?}"),
    };
    let unicode = headers.iter().find(|h| h.subject.contains("Café")).expect("unicode fixture present, RFC 2047 decoded");
    assert!(unicode.subject.contains("日本語"), "subject was: {:?}", unicode.subject);
}

/// B3: the IMAP-side half of turning a `SyncPlan::FetchFrom` into an actual
/// fetch. `ImapCommand::FetchHeadersFrom` reuses the same envelope-only,
/// unpaged fetch `FetchNewHeaders` (B10) already exercises indirectly via
/// the notify tests, but as its own command/event pair -- see its doc in
/// imap.rs for why -- so it's worth its own direct coverage here rather
/// than assuming the shared underlying fetch still behaves under a new
/// entry point.
#[tokio::test]
async fn fetch_headers_from_returns_envelopes_from_the_given_uid_onward() {
    skip_unless_ca_trusted!();
    let mut h = start_harness(5).await; // 2 fixtures + 5 = 7 total, UIDs 1..=7

    h.imap_cmd.send(ImapCommand::FetchHeaders { mailbox: "INBOX".to_string(), page: 1, req_id: 1 }).await.unwrap();
    match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::Headers { mailbox_state, .. } => assert_eq!(mailbox_state.uid_next, 8),
        other => panic!("expected Headers, got {other:?}"),
    }

    h.imap_cmd.send(ImapCommand::FetchHeadersFrom { mailbox: "INBOX".to_string(), first_uid: 5 }).await.unwrap();
    match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::HeadersFrom { mailbox, headers } => {
            assert_eq!(mailbox, "INBOX");
            let mut uids: Vec<u32> = headers.iter().map(|h| h.uid).collect();
            uids.sort();
            assert_eq!(uids, vec![5, 6, 7]);
        }
        other => panic!("expected HeadersFrom, got {other:?}"),
    }
}

#[tokio::test]
async fn bulk_download_streams_every_message_with_progress() {
    skip_unless_ca_trusted!();
    let mut h = start_harness(10).await; // 2 fixtures + 10 = 12 total

    h.imap_cmd.send(ImapCommand::BulkDownload { mailbox: "INBOX".to_string() }).await.unwrap();

    let mut mail_data_count = 0;
    let final_progress = loop {
        match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
            ImapEvent::MailData { .. } => mail_data_count += 1,
            ImapEvent::Progress { kind: ProgressKind::Index, progress: Progress::Counted { current, total } } => {
                assert_eq!(total, 12);
                if current == total {
                    break current;
                }
            }
            other => panic!("unexpected event during bulk download: {other:?}"),
        }
    };
    assert_eq!(mail_data_count, 12);
    assert_eq!(final_progress, 12);
}

/// B2's whole reason to exist: before the session-pool split, `BulkDownload`
/// and `FetchHeaders` shared one `ImapActor` session processed from a single
/// command queue, so a `FetchHeaders` sent while a `BulkDownload` was
/// running had to wait behind every one of its body fetches. Now
/// `BulkDownload` is handed off to a dedicated worker connection
/// (`imap.rs::spawn_body_worker`) the moment it's received, leaving the
/// primary session free to answer `FetchHeaders` immediately.
#[tokio::test]
async fn bulk_download_does_not_block_a_concurrent_header_fetch() {
    skip_unless_ca_trusted!();
    // Enough messages that a fully-serialized implementation (one EXAMINE +
    // one ENVELOPE fetch + 100 individual RFC822 fetches, each a real
    // network round trip) would take noticeably longer than answering one
    // FetchHeaders does -- large enough to make the race not-close, without
    // being so large the test itself becomes slow.
    let mut h = start_harness(100).await; // 2 fixtures + 100 = 102 total

    h.imap_cmd.send(ImapCommand::BulkDownload { mailbox: "INBOX".to_string() }).await.unwrap();
    h.imap_cmd.send(ImapCommand::FetchHeaders { mailbox: "INBOX".to_string(), page: 1, req_id: 42 }).await.unwrap();

    let mut bulk_download_finished = false;
    let headers_arrived_while_bulk_still_running = loop {
        match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
            ImapEvent::Headers { req_id: 42, .. } => break !bulk_download_finished,
            ImapEvent::Progress { kind: ProgressKind::Index, progress: Progress::Counted { current, total } } => {
                if current == total {
                    bulk_download_finished = true;
                }
            }
            ImapEvent::MailData { .. } => {}
            other => panic!("unexpected event: {other:?}"),
        }
    };

    assert!(
        headers_arrived_while_bulk_still_running,
        "FetchHeaders was only answered after BulkDownload finished (or never), \
         suggesting they're still serialized on one connection rather than split \
         across a primary session and a body worker"
    );
}

// ── B8: flags, move-to-Trash/Archive, unread counts ─────────────────────────

/// `StoreFlags` (B8): `+FLAGS`/`-FLAGS` round trip, exercising both add and
/// remove in the same call the way `main.rs::store_flags_on_selection` never
/// actually needs to (each button only ever adds or only removes) but
/// `ImapActor::store_flags` is written to support either combination.
#[tokio::test]
async fn store_flags_adds_and_removes_in_one_call() {
    skip_unless_ca_trusted!();
    let mut h = start_harness(0).await;

    h.imap_cmd.send(ImapCommand::FetchHeaders { mailbox: "INBOX".to_string(), page: 1, req_id: 1 }).await.unwrap();
    let headers = match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::Headers { headers, .. } => headers,
        other => panic!("expected Headers, got {other:?}"),
    };
    let uid = headers[0].uid;
    assert!(!headers[0].is_seen(), "a freshly delivered message starts unseen");

    h.imap_cmd
        .send(ImapCommand::StoreFlags {
            mailbox: "INBOX".to_string(),
            uid,
            add: vec!["\\Seen".to_string(), "\\Flagged".to_string()],
            remove: vec![],
            req_id: 99,
        })
        .await
        .unwrap();
    match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::FlagsUpdated { uid: got_uid, flags, req_id: 99, .. } => {
            assert_eq!(got_uid, uid);
            assert!(flags.iter().any(|f| f == "\\Seen"));
            assert!(flags.iter().any(|f| f == "\\Flagged"));
        }
        other => panic!("expected FlagsUpdated, got {other:?}"),
    }

    // Now remove \Seen only -- \Flagged should survive.
    h.imap_cmd
        .send(ImapCommand::StoreFlags {
            mailbox: "INBOX".to_string(),
            uid,
            add: vec![],
            remove: vec!["\\Seen".to_string()],
            req_id: 100,
        })
        .await
        .unwrap();
    match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::FlagsUpdated { flags, req_id: 100, .. } => {
            assert!(!flags.iter().any(|f| f == "\\Seen"));
            assert!(flags.iter().any(|f| f == "\\Flagged"));
        }
        other => panic!("expected FlagsUpdated, got {other:?}"),
    }
}

/// `MoveMessage` (B8) against this mock server, which has no `MOVE`
/// extension -- so this exercises the COPY+STORE+EXPUNGE fallback path
/// specifically, not real `MOVE` (see `ImapActor::move_message`'s doc for
/// why the fallback is the one this mock can actually prove works). After
/// the move, the message must be gone from INBOX and present in Trash.
#[tokio::test]
async fn move_message_falls_back_to_copy_store_expunge_and_the_message_relocates() {
    skip_unless_ca_trusted!();
    let mut h = start_harness(0).await;

    h.imap_cmd.send(ImapCommand::FetchHeaders { mailbox: "INBOX".to_string(), page: 1, req_id: 1 }).await.unwrap();
    let headers = match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::Headers { headers, .. } => headers,
        other => panic!("expected Headers, got {other:?}"),
    };
    let uid = headers[0].uid;
    let subject = headers[0].subject.clone();

    h.imap_cmd
        .send(ImapCommand::MoveMessage { mailbox: "INBOX".to_string(), uid, dest: "Trash".to_string(), req_id: 7 })
        .await
        .unwrap();
    match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::Moved { mailbox, uid: got_uid, dest, req_id: 7 } => {
            assert_eq!(mailbox, "INBOX");
            assert_eq!(got_uid, uid);
            assert_eq!(dest, "Trash");
        }
        other => panic!("expected Moved, got {other:?}"),
    }

    h.imap_cmd.send(ImapCommand::FetchHeaders { mailbox: "INBOX".to_string(), page: 1, req_id: 2 }).await.unwrap();
    match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::Headers { headers, .. } => {
            assert!(!headers.iter().any(|h| h.uid == uid), "moved message must no longer be in INBOX");
        }
        other => panic!("expected Headers, got {other:?}"),
    }

    h.imap_cmd.send(ImapCommand::FetchHeaders { mailbox: "Trash".to_string(), page: 1, req_id: 3 }).await.unwrap();
    match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::Headers { headers, .. } => {
            assert!(headers.iter().any(|h| h.subject == subject), "moved message must now be in Trash");
        }
        other => panic!("expected Headers, got {other:?}"),
    }
}

/// `FetchUnreadCounts` (B8's `STATUS (UNSEEN)`): starts equal to the
/// mailbox's full message count (nothing is `\Seen` on delivery) and drops
/// by one once a message is marked `\Seen`.
#[tokio::test]
async fn fetch_unread_counts_reflects_seen_flags() {
    skip_unless_ca_trusted!();
    let mut h = start_harness(3).await; // 2 fixtures + 3 = 5 in INBOX

    h.imap_cmd.send(ImapCommand::FetchUnreadCounts { mailboxes: vec!["INBOX".to_string()] }).await.unwrap();
    match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::UnreadCounts(counts) => assert_eq!(counts.get("INBOX"), Some(&5)),
        other => panic!("expected UnreadCounts, got {other:?}"),
    }

    h.imap_cmd.send(ImapCommand::FetchHeaders { mailbox: "INBOX".to_string(), page: 1, req_id: 1 }).await.unwrap();
    let uid = match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::Headers { headers, .. } => headers[0].uid,
        other => panic!("expected Headers, got {other:?}"),
    };
    h.imap_cmd
        .send(ImapCommand::StoreFlags { mailbox: "INBOX".to_string(), uid, add: vec!["\\Seen".to_string()], remove: vec![], req_id: 2 })
        .await
        .unwrap();
    assert!(matches!(
        timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap(),
        ImapEvent::FlagsUpdated { .. }
    ));

    h.imap_cmd.send(ImapCommand::FetchUnreadCounts { mailboxes: vec!["INBOX".to_string()] }).await.unwrap();
    match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::UnreadCounts(counts) => assert_eq!(counts.get("INBOX"), Some(&4)),
        other => panic!("expected UnreadCounts, got {other:?}"),
    }
}

/// The real round trip this whole server exists to make testable: esmail's
/// `SmtpActor` sends a message, and esmail's `ImapActor` can then see it.
#[tokio::test]
async fn send_via_smtp_then_see_it_over_imap() {
    skip_unless_ca_trusted!();
    let mut h = start_harness(0).await; // 2 fixtures in INBOX to start

    let (smtp_cmd_tx, smtp_cmd_rx) = mpsc::channel(8);
    let (smtp_evt_tx, mut smtp_evt_rx) = mpsc::channel(8);
    SmtpActor::spawn(&tokio::runtime::Handle::current(), smtp_cmd_rx, smtp_evt_tx);

    let account = SmtpAccount {
        host: "127.0.0.1".to_string(),
        port: h.server.smtp_addr.port(),
        tls: esmail::config::TlsMode::None,
        username: TEST_USER.to_string(),
        auth: Auth::password(TEST_PASSWORD),
        from_address: TEST_USER.to_string(),
    };
    let compose = ComposeState {
        to: TEST_USER.to_string(),
        subject: "Sent from the integration test".to_string(),
        body: "This message was sent over SMTP by the test.".to_string(),
        ..Default::default()
    };

    smtp_cmd_tx.send(SmtpCommand::Send { id: 1, account, compose }).await.unwrap();
    match timeout(RECV_TIMEOUT, smtp_evt_rx.recv()).await.unwrap().unwrap() {
        SmtpEvent::Sent { .. } => {}
        SmtpEvent::Error { error, .. } => panic!("send failed: {error}"),
    }

    h.imap_cmd.send(ImapCommand::FetchHeaders { mailbox: "INBOX".to_string(), page: 1, req_id: 1 }).await.unwrap();
    match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::Headers { headers, mailbox_state, .. } => {
            assert_eq!(headers.len(), 3); // 2 fixtures + the one just sent
            assert!(headers.iter().any(|h| h.subject == "Sent from the integration test"));
            assert!(mailbox_state.uid_next >= 4);
        }
        other => panic!("expected Headers, got {other:?}"),
    }
}

/// Several compose windows can have a send in flight at once: each result comes
/// back tagged with the id of the window that sent it (the one that failed
/// carries the failing id, the others their own).
#[tokio::test]
async fn smtp_results_carry_the_id_of_the_compose_window_that_sent() {
    skip_unless_ca_trusted!();
    let h = start_harness(0).await;

    let (smtp_cmd_tx, smtp_cmd_rx) = mpsc::channel(8);
    let (smtp_evt_tx, mut smtp_evt_rx) = mpsc::channel(8);
    SmtpActor::spawn(&tokio::runtime::Handle::current(), smtp_cmd_rx, smtp_evt_tx);

    let account = |password: &str| SmtpAccount {
        host: "127.0.0.1".to_string(),
        port: h.server.smtp_addr.port(),
        tls: esmail::config::TlsMode::None,
        username: TEST_USER.to_string(),
        auth: Auth::password(password),
        from_address: TEST_USER.to_string(),
    };
    let compose = |subject: &str| ComposeState {
        to: TEST_USER.to_string(),
        subject: subject.to_string(),
        ..Default::default()
    };

    smtp_cmd_tx.send(SmtpCommand::Send { id: 11, account: account(TEST_PASSWORD), compose: compose("first") }).await.unwrap();
    smtp_cmd_tx.send(SmtpCommand::Send { id: 12, account: account("wrong password"), compose: compose("second") }).await.unwrap();
    smtp_cmd_tx.send(SmtpCommand::Send { id: 13, account: account(TEST_PASSWORD), compose: compose("third") }).await.unwrap();

    let mut sent = Vec::new();
    let mut failed = Vec::new();
    for _ in 0..3 {
        match timeout(RECV_TIMEOUT, smtp_evt_rx.recv()).await.unwrap().unwrap() {
            SmtpEvent::Sent { id, .. } => sent.push(id),
            SmtpEvent::Error { id, .. } => failed.push(id),
        }
    }
    sent.sort();
    assert_eq!(sent, vec![11, 13]);
    assert_eq!(failed, vec![12]);
}

/// B7: after a send, `main.rs` also `APPEND`s a copy to Sent -- verify the
/// IMAP-side half of that directly (the raw bytes `SmtpEvent::Sent` carries
/// really do round-trip through `ImapCommand::Append` into a mailbox
/// `FetchHeaders` can then see), the same way `send_via_smtp_then_see_it_over_imap`
/// verifies the SMTP-then-INBOX half.
#[tokio::test]
async fn append_saves_a_sent_copy_that_fetch_headers_can_then_see() {
    skip_unless_ca_trusted!();
    let mut h = start_harness(0).await;

    let (smtp_cmd_tx, smtp_cmd_rx) = mpsc::channel(8);
    let (smtp_evt_tx, mut smtp_evt_rx) = mpsc::channel(8);
    SmtpActor::spawn(&tokio::runtime::Handle::current(), smtp_cmd_rx, smtp_evt_tx);

    let account = SmtpAccount {
        host: "127.0.0.1".to_string(),
        port: h.server.smtp_addr.port(),
        tls: esmail::config::TlsMode::None,
        username: TEST_USER.to_string(),
        auth: Auth::password(TEST_PASSWORD),
        from_address: TEST_USER.to_string(),
    };
    let compose = ComposeState {
        to: "someone-else@example.com".to_string(),
        subject: "Copy me to Sent".to_string(),
        body: "This should show up in Sent too.".to_string(),
        ..Default::default()
    };

    smtp_cmd_tx.send(SmtpCommand::Send { id: 1, account, compose }).await.unwrap();
    let raw = match timeout(RECV_TIMEOUT, smtp_evt_rx.recv()).await.unwrap().unwrap() {
        SmtpEvent::Sent { raw, .. } => raw,
        SmtpEvent::Error { error, .. } => panic!("send failed: {error}"),
    };
    assert!(String::from_utf8_lossy(&raw).contains("Copy me to Sent"), "raw bytes should be the actual sent message");

    h.imap_cmd.send(ImapCommand::Append { mailbox: "Sent".to_string(), raw }).await.unwrap();
    // #79: the append reports it is in flight (an indeterminate step -- a
    // large upload has no count) before its terminal event.
    match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::Progress { kind: ProgressKind::Append, progress: Progress::Indeterminate } => {}
        other => panic!("expected an Append progress report, got {other:?}"),
    }
    match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::Appended { mailbox } => assert_eq!(mailbox, "Sent"),
        other => panic!("expected Appended, got {other:?}"),
    }

    // fixtures.rs seeds one message into Sent already (see `seed`'s
    // `deliver_fixture(store, "Sent", plain_message(9999))`), so the
    // appended copy should be the second.
    h.imap_cmd.send(ImapCommand::FetchHeaders { mailbox: "Sent".to_string(), page: 1, req_id: 1 }).await.unwrap();
    match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::Headers { headers, .. } => {
            assert_eq!(headers.len(), 2);
            assert!(headers.iter().any(|h| h.subject == "Copy me to Sent"));
        }
        other => panic!("expected Headers, got {other:?}"),
    }
}

#[tokio::test]
async fn disconnected_session_reconnects_and_serves_the_next_request() {
    skip_unless_ca_trusted!();
    let mut h = start_harness(0).await;

    // `ImapActor` clears `self.session` on any command error and
    // reconnects on the next command via `ensure_connected` -- simulate
    // that by fetching a mailbox that doesn't exist (a `NO` response,
    // which `fetch_headers` turns into an `Err`), then confirming a normal
    // request right after still succeeds.
    h.imap_cmd.send(ImapCommand::FetchHeaders { mailbox: "NoSuchMailbox".to_string(), page: 1, req_id: 1 }).await.unwrap();
    match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
        ImapEvent::Error(_) => {}
        other => panic!("expected Error for a nonexistent mailbox, got {other:?}"),
    }

    h.imap_cmd.send(ImapCommand::FetchMailboxes).await.unwrap();
    // ensure_connected reports Disconnected before it starts reconnecting.
    let mut saw_disconnected = false;
    loop {
        match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
            ImapEvent::Disconnected => saw_disconnected = true,
            ImapEvent::Connected => {}
            ImapEvent::Mailboxes(mailboxes) => {
                assert!(mailboxes.iter().any(|m| m.name == "INBOX"));
                break;
            }
            other => panic!("unexpected event while reconnecting: {other:?}"),
        }
    }
    assert!(saw_disconnected);
}

/// `idle_watch`'s reason to exist: a push should arrive close to
/// instantly, well inside `RECV_TIMEOUT`, rather than needing
/// the forwarder's 60-second poll timer to notice. This is the one
/// test in this file for `mail-mock-server`'s `IDLE` handling too --
/// `imap_server.rs`'s untagged-`EXISTS`-on-delivery push and `idle_watch`'s
/// client side are really one feature, verified together.
#[tokio::test]
async fn idle_push_notifies_of_new_mail_without_polling() {
    skip_unless_ca_trusted!();
    let h = start_harness(0).await; // 2 fixtures already in INBOX

    let (wake_tx, mut wake_rx) = mpsc::channel(4);
    let _idle = idle_watch::spawn(
        "localhost".to_string(),
        h.server.imap_addr.port(),
        TEST_USER.to_string(),
        Auth::password(TEST_PASSWORD),
        "INBOX".to_string(),
        wake_tx,
    );

    // `idle_watch::spawn`'s connect+login+EXAMINE+IDLE-init round trip
    // (three real TCP/TLS exchanges) has no observable "now idling" signal
    // from outside the module -- deliberately: adding one would mean
    // production code carrying a test-only hook. A push landing before the
    // client has actually started idling is silently missed by design (RFC
    // 2177 IDLE, like this mock's implementation of it, only pushes to
    // connections already idling), so instead of guessing a delay, retry
    // delivery every 500ms until a push arrives or the outer timeout gives
    // up -- once the connection *is* idling, the very next delivery is
    // always seen.
    let deliver = || {
        let mut store = h.server.store.lock().unwrap();
        store.deliver(
            "INBOX",
            b"From: bob@example.com\r\nTo: alice@example.com\r\nSubject: pushed\r\n\r\nhi\r\n".to_vec(),
        );
    };

    let result = timeout(RECV_TIMEOUT, async {
        loop {
            deliver();
            if let Ok(Some(idle_watch::MailboxChanged)) = timeout(Duration::from_millis(500), wake_rx.recv()).await {
                return;
            }
        }
    })
    .await;

    assert!(result.is_ok(), "expected a MailboxChanged push within {RECV_TIMEOUT:?} of repeated deliveries (is the test CA actually trusted?)");
}

mod stress {
    use super::*;

    /// Seeds a large inbox and fires every page fetch + every body fetch
    /// concurrently against one `ImapActor` (one real TLS session), then
    /// does the same for a burst of concurrent SMTP sends against a fresh
    /// `SmtpActor` per send (mirroring one connection per send, same as
    /// `smtp.rs::build_transport` does today -- no pooling). Not a
    /// benchmark, just a "does this fall over under concurrent load"
    /// check; run explicitly since it's slower than the rest of the suite.
    #[tokio::test]
    #[ignore]
    async fn many_concurrent_header_and_body_fetches_all_complete() {
        skip_unless_ca_trusted!();
        const INBOX_COUNT: u32 = 400;
        let mut h = start_harness(INBOX_COUNT).await;

        // ImapActor serializes commands (one mpsc receiver, one session),
        // so "concurrent" here means "fired without waiting for replies in
        // between" -- exercising req_id-based reply matching under a
        // saturated queue, which is exactly what B2's req_id bookkeeping
        // was for.
        let total_pages = (INBOX_COUNT + 2).div_ceil(50);
        for page in 1..=total_pages {
            h.imap_cmd.send(ImapCommand::FetchHeaders { mailbox: "INBOX".to_string(), page, req_id: page as u64 }).await.unwrap();
        }

        let mut headers_by_req = std::collections::HashMap::new();
        for _ in 0..total_pages {
            match timeout(RECV_TIMEOUT, h.imap_evt.recv()).await.unwrap().unwrap() {
                ImapEvent::Headers { req_id, headers, .. } => {
                    headers_by_req.insert(req_id, headers);
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert_eq!(headers_by_req.len(), total_pages as usize);
        let total_headers: usize = headers_by_req.values().map(|h| h.len()).sum();
        assert_eq!(total_headers as u32, INBOX_COUNT + 2);

        // Now hammer FetchBody for every UID we just learned about. Sending
        // and receiving must happen concurrently here: both the command and
        // event mpsc channels are bounded (32, matching `main.rs`'s own
        // sizing), so sending all ~400 commands before draining any events
        // deadlocks -- the actor blocks trying to push the 33rd event into
        // a full channel nobody's reading yet, which stops it from ever
        // draining the next command, which is exactly the 33rd send this
        // loop is itself blocked on.
        let all_uids: Vec<u32> = headers_by_req.values().flatten().map(|h| h.uid).collect();
        let sender_cmd_tx = h.imap_cmd.clone();
        let uids_to_send = all_uids.clone();
        let sender = tokio::spawn(async move {
            for (i, uid) in uids_to_send.iter().enumerate() {
                sender_cmd_tx.send(ImapCommand::FetchBody { mailbox: "INBOX".to_string(), uid: *uid, req_id: 1000 + i as u64 }).await.unwrap();
            }
        });
        let mut bodies_seen = 0;
        for _ in 0..all_uids.len() {
            match timeout(Duration::from_secs(60), h.imap_evt.recv()).await.unwrap().unwrap() {
                ImapEvent::Body { .. } => bodies_seen += 1,
                other => panic!("unexpected event: {other:?}"),
            }
        }
        sender.await.unwrap();
        assert_eq!(bodies_seen, all_uids.len());
    }

    #[tokio::test]
    #[ignore]
    async fn many_concurrent_smtp_sends_all_land_in_the_inbox() {
        skip_unless_ca_trusted!();
        let h = start_harness(0).await;
        const SEND_COUNT: usize = 50;

        let sends = (0..SEND_COUNT).map(|i| {
            let smtp_addr = h.server.smtp_addr;
            async move {
                let (cmd_tx, cmd_rx) = mpsc::channel(1);
                let (evt_tx, mut evt_rx) = mpsc::channel(1);
                SmtpActor::spawn(&tokio::runtime::Handle::current(), cmd_rx, evt_tx);
                let account = SmtpAccount {
                    host: "127.0.0.1".to_string(),
                    port: smtp_addr.port(),
                    tls: esmail::config::TlsMode::None,
                    username: TEST_USER.to_string(),
                    auth: Auth::password(TEST_PASSWORD),
                    from_address: TEST_USER.to_string(),
                };
                let compose = ComposeState {
                    to: TEST_USER.to_string(),
                    subject: format!("Stress message #{i}"),
                    body: format!("Body of stress message #{i}"),
                    ..Default::default()
                };
                cmd_tx.send(SmtpCommand::Send { id: 1, account, compose }).await.unwrap();
                match timeout(Duration::from_secs(30), evt_rx.recv()).await {
                    Ok(Some(SmtpEvent::Sent { .. })) => true,
                    other => {
                        eprintln!("send #{i} failed: {other:?}");
                        false
                    }
                }
            }
        });

        let results = futures::future::join_all(sends).await;
        let succeeded = results.iter().filter(|ok| **ok).count();
        assert_eq!(succeeded, SEND_COUNT, "{succeeded}/{SEND_COUNT} concurrent sends succeeded");

        // `seed()` always delivers its 2 hand-built fixture messages into
        // INBOX regardless of `inbox_count`, so INBOX has 2 + SEND_COUNT,
        // not just SEND_COUNT -- walk every page rather than assuming a
        // fixed page count, and check by subject rather than by total
        // count alone so a wrong delivery target would still be caught.
        let (imap_cmd_tx, imap_cmd_rx) = mpsc::channel(32);
        let (imap_evt_tx, mut imap_evt_rx) = mpsc::channel(32);
        ImapActor::spawn(&tokio::runtime::Handle::current(), imap_cmd_rx, imap_evt_tx);
        imap_cmd_tx
            .send(ImapCommand::Connect {
                host: "localhost".to_string(),
                port: h.server.imap_addr.port(),
                username: TEST_USER.to_string(),
                auth: Auth::password(TEST_PASSWORD),
            })
            .await
            .unwrap();
        assert!(matches!(timeout(RECV_TIMEOUT, imap_evt_rx.recv()).await.unwrap().unwrap(), ImapEvent::Connected));

        let mut stress_subjects_seen = std::collections::HashSet::new();
        let mut total_messages = 0u32;
        let mut page = 1;
        loop {
            imap_cmd_tx.send(ImapCommand::FetchHeaders { mailbox: "INBOX".to_string(), page, req_id: page as u64 }).await.unwrap();
            let (headers, total_pages) = match timeout(RECV_TIMEOUT, imap_evt_rx.recv()).await.unwrap().unwrap() {
                ImapEvent::Headers { headers, total_pages, .. } => (headers, total_pages),
                other => panic!("expected Headers, got {other:?}"),
            };
            total_messages += headers.len() as u32;
            for h in &headers {
                if h.subject.starts_with("Stress message #") {
                    stress_subjects_seen.insert(h.subject.clone());
                }
            }
            if page >= total_pages {
                break;
            }
            page += 1;
        }

        assert_eq!(total_messages, 2 + SEND_COUNT as u32, "2 fixtures + {SEND_COUNT} stress sends");
        assert_eq!(stress_subjects_seen.len(), SEND_COUNT, "every stress send should show up exactly once");
    }
}
