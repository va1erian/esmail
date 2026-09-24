//! Sends through the real SMTP actor to `mail-mock-server` (plain-text SMTP, so
//! no test CA is needed) and checks what the receiving side got, and what the
//! delivery bookkeeping asks for after a failure.

use std::sync::Arc;
use std::time::{Duration, Instant};

use esmail::auth::Auth;
use esmail::compose::ComposeState;
use esmail::config::TlsMode;
use esmail::render::extract_attachments;
use esmail::smtp::{SmtpAccount, SmtpEvent};
use esmail_win32::core_glue::{Deliveries, Failure, Sender};
use mail_mock_server::fixtures::{TEST_PASSWORD, TEST_USER};

fn account(port: u16) -> SmtpAccount {
    SmtpAccount {
        host: "127.0.0.1".into(),
        port,
        tls: TlsMode::None,
        username: TEST_USER.into(),
        auth: Auth::password(TEST_PASSWORD),
        from_address: TEST_USER.into(),
    }
}

fn next_outcome(sender: &Sender) -> SmtpEvent {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Some(event) = sender.pump().into_iter().next() {
            return event;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("no send outcome within 20 s");
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap()
}

#[test]
fn a_reply_with_an_attachment_arrives_with_its_threading_headers_and_bytes() {
    let runtime = runtime();
    let store = mail_mock_server::new_store();
    mail_mock_server::fixtures::seed(&mut store.lock().unwrap(), 0);
    let server = runtime.block_on(mail_mock_server::start(store.clone())).unwrap();
    let sender = Sender::start(runtime.handle(), esmail::waker::noop());
    let bytes: Vec<u8> = (0..=255u8).cycle().take(5000).collect();
    let state = ComposeState {
        to: "Bob <bob@example.com>".into(),
        subject: "Re: Lunch".into(),
        body: "See you there.".into(),
        in_reply_to: Some("<orig-1@example.com>".into()),
        references: Some("<orig-1@example.com>".into()),
        attachments: vec![("menu.bin".into(), bytes.clone())],
        ..Default::default()
    };

    sender.send(7, account(server.smtp_addr.port()), state).unwrap();
    let SmtpEvent::Sent { id, raw } = next_outcome(&sender) else { panic!("the send failed") };
    assert_eq!(id, 7);

    let delivered = {
        let store = store.lock().unwrap();
        store.mailbox("INBOX").unwrap().messages.last().unwrap().raw.clone()
    };
    assert!(String::from_utf8_lossy(&raw).contains("Subject: Re: Lunch"), "the copy for the Sent folder is the sent message");
    let text = String::from_utf8_lossy(&delivered);
    assert!(text.contains("In-Reply-To: <orig-1@example.com>"), "{text}");
    assert!(text.contains("References: <orig-1@example.com>"));
    let attachments = extract_attachments(&delivered);
    assert_eq!(attachments.len(), 1);
    assert_eq!((attachments[0].filename.as_str(), &attachments[0].data), ("menu.bin", &bytes));
}

#[test]
fn a_refused_connection_fails_the_send_and_asks_for_the_message_to_be_kept() {
    let runtime = runtime();
    let closed_port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let sender = Sender::start(runtime.handle(), Arc::new(|| {}));
    let mut deliveries = Deliveries::default();
    let state = ComposeState { to: "bob@example.com".into(), subject: "Keep me".into(), body: "text".into(), ..Default::default() };
    let id = deliveries.next_id();

    deliveries.begin(id, 0, state.clone());
    sender.send(id, account(closed_port), state.clone()).unwrap();
    let SmtpEvent::Error { id: failed_id, .. } = next_outcome(&sender) else { panic!("the send should have failed") };

    assert_eq!(failed_id, id);
    assert_eq!(deliveries.failed(id), Some(Failure::Enqueue { account: 0, state }));
}
