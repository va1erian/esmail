//! `AppCore` driven headless (issue #108): a real `AccountSession` against
//! `mail-mock-server`, with the test standing in for the frontend -- it calls
//! `pump()` where an egui frame or a Win32 message would, and reads the core's
//! public state instead of drawing it.
//!
//! Needs the bundled test CA trusted, like `imap_smtp_integration.rs`; skips
//! with a message unless `ESMAIL_TEST_CA_TRUSTED` is set.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use esmail::app::{AppCore, Changes, ConnState};
use esmail::auth::Auth;
use esmail::config::AccountConfig;
use esmail::db::DbCommand;
use esmail::imap::ImapCommand;
use esmail::progress::ProgressKind;
use mail_mock_server::fixtures::{TEST_PASSWORD, TEST_USER, seed};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};

const WAIT: Duration = Duration::from_secs(30);

macro_rules! skip_unless_ca_trusted {
    () => {
        if std::env::var("ESMAIL_TEST_CA_TRUSTED").is_err() {
            eprintln!(
                "skipping: set ESMAIL_TEST_CA_TRUSTED=1 once mail-mock-server/certs/ca.crt is trusted \
                 (see crates/mail-mock-server/README.md)"
            );
            return;
        }
    };
}

/// The core plus the outside world it talks to: the mock server, the wakes it
/// asked for and the cache commands it issued.
struct Harness {
    core: AppCore,
    account: AccountConfig,
    wakes: Arc<AtomicUsize>,
    db_rx: mpsc::Receiver<DbCommand>,
    /// Everything `pump` returned so far, folded together.
    changes: Changes,
    _server: mail_mock_server::RunningServer,
}

async fn harness(password: &str) -> Harness {
    let store = mail_mock_server::new_store();
    seed(&mut store.lock().unwrap(), 3);
    let server = mail_mock_server::start(store).await.expect("start mock servers");

    let wakes = Arc::new(AtomicUsize::new(0));
    let waker = {
        let wakes = Arc::clone(&wakes);
        Arc::new(move || {
            wakes.fetch_add(1, Ordering::Relaxed);
        })
    };
    let (db_tx, db_rx) = mpsc::channel(64);
    let mut core = AppCore::new(Handle::current(), waker, Arc::new(|_, _, _| {}), db_tx);

    let account = AccountConfig::new("Alice".into(), "localhost".into(), server.imap_addr.port(), TEST_USER.into());
    core.open_session(&account, Auth::password(password.to_string()), None);
    Harness { core, account, wakes, db_rx, changes: Changes::default(), _server: server }
}

impl Harness {
    /// Pump until `done` holds for the core, as a frontend woken repeatedly
    /// would.
    async fn pump_until(&mut self, what: &str, done: impl Fn(&AppCore) -> bool) {
        let result = timeout(WAIT, async {
            loop {
                let changes = self.core.pump();
                if changes.reading_pane.is_some() {
                    self.changes.reading_pane = changes.reading_pane;
                }
                self.changes.persist_accounts.extend(changes.persist_accounts);
                if done(&self.core) {
                    return;
                }
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(result.is_ok(), "timed out waiting for {what} (is the test CA trusted?)");
    }

    /// The `DbCommand`s issued so far, without waiting for more.
    fn db_commands(&mut self) -> Vec<DbCommand> {
        std::iter::from_fn(|| self.db_rx.try_recv().ok()).collect()
    }

    async fn connect_and_list(&mut self) {
        self.pump_until("the first page of headers", |core| core.headers.len() == 5).await;
    }
}

#[tokio::test]
async fn connecting_activates_the_account_and_lists_its_inbox() {
    skip_unless_ca_trusted!();
    let mut h = harness(TEST_PASSWORD).await;
    assert_eq!(h.core.accounts[0].state, ConnState::Connecting);

    h.connect_and_list().await;

    assert_eq!(h.core.accounts[0].state, ConnState::Connected);
    assert_eq!(h.core.active.as_deref(), Some(h.account.id.as_str()));
    assert_eq!(h.core.selected_mailbox, "INBOX");
    assert_eq!((h.core.current_page, h.core.total_pages), (1, 1));
    assert_eq!(h.core.status, "Page 1 of 1");
    assert!(h.wakes.load(Ordering::Relaxed) > 0, "sessions must wake the frontend");
    assert!(h.core.banners.is_empty());
    assert!(
        h.changes.persist_accounts.is_empty(),
        "an account that was not added through the form must not be saved"
    );
    assert!(
        h.db_commands().iter().any(|c| matches!(c, DbCommand::ReportMailboxState { mailbox, .. } if mailbox == "INBOX")),
        "the header page must report the mailbox state to the cache"
    );

    h.pump_until("the mailbox tree and unread counts", |core| core.accounts[0].total_unread() == 5).await;
    assert!(h.core.mailbox_names(&h.account.id).iter().any(|name| name == "INBOX"));
}

#[tokio::test]
async fn a_rejected_password_marks_the_account_failed_and_raises_a_banner() {
    skip_unless_ca_trusted!();
    let mut h = harness("not-the-password").await;

    h.pump_until("the login failure", |core| !core.banners.is_empty()).await;

    assert!(matches!(h.core.accounts[0].state, ConnState::Failed(_)));
    assert!(h.core.banners[0].message.starts_with("IMAP error:"), "{}", h.core.banners[0].message);
    assert!(h.core.active.is_none() && h.core.headers.is_empty());
}

#[tokio::test]
async fn opening_a_message_shows_its_body_and_drops_the_superseded_reply() {
    skip_unless_ca_trusted!();
    let mut h = harness(TEST_PASSWORD).await;
    h.connect_and_list().await;
    let plain: Vec<_> = h.core.headers.iter().filter(|header| header.subject.starts_with("Test message #")).collect();
    let (first, second) = (plain[0].uid, plain[1].uid);
    // The renderer entity-encodes the spaces of a plain-text body.
    let second_body = format!("of&#32;{}", plain[1].subject.to_lowercase().replace(' ', "&#32;"));

    // What the frontend's `open_message` does: select, then ask for the body.
    // The second open supersedes the first before its reply arrives.
    h.core.selected_uid = Some(first);
    h.core.fetch_body("INBOX".into(), first);
    h.core.selected_uid = Some(second);
    h.core.fetch_body("INBOX".into(), second);

    h.pump_until("the second message's body", |core| core.current_message_html.contains(&second_body)).await;
    assert_eq!(h.changes.reading_pane.as_deref(), Some(h.core.current_message_html.as_str()));

    // Nothing later may replace it, in particular not the first message's reply.
    sleep(Duration::from_millis(300)).await;
    h.core.pump();
    assert!(h.core.current_message_html.contains(&second_body));
}

#[tokio::test]
async fn a_bulk_flag_change_counts_replies_updates_headers_and_the_cache() {
    skip_unless_ca_trusted!();
    let mut h = harness(TEST_PASSWORD).await;
    h.connect_and_list().await;
    h.pump_until("the unread counts", |core| core.accounts[0].total_unread() == 5).await;
    h.db_commands();

    let targets = [h.core.headers[0].uid, h.core.headers[1].uid];
    h.core.begin_bulk_action(ProgressKind::Flags, &targets);
    assert!(h.core.bulk_action_in_flight());
    for uid in targets {
        let req_id = h.core.next_req_id();
        h.core.send_imap(ImapCommand::StoreFlags {
            mailbox: "INBOX".into(),
            uid,
            add: vec!["\\Seen".into()],
            remove: vec![],
            req_id,
        });
    }

    h.pump_until("both flag updates", |core| !core.bulk_action_in_flight()).await;

    assert!(h.core.progress.is_none());
    assert!(h.core.headers.iter().filter(|header| targets.contains(&header.uid)).all(|header| header.is_seen()));
    assert_eq!(h.core.accounts[0].total_unread(), 3);
    let updated: Vec<u32> = h
        .db_commands()
        .into_iter()
        .filter_map(|command| match command {
            DbCommand::UpdateFlags { uid, .. } => Some(uid),
            _ => None,
        })
        .collect();
    assert_eq!(updated.len(), 2);
    assert!(targets.iter().all(|uid| updated.contains(uid)));
}

#[tokio::test]
async fn disconnecting_the_active_account_clears_the_list_and_the_reading_pane() {
    skip_unless_ca_trusted!();
    let mut h = harness(TEST_PASSWORD).await;
    h.connect_and_list().await;
    h.core.take_changes();

    h.core.disconnect_account(&h.account.id);

    assert!(h.core.accounts.is_empty() && h.core.active.is_none() && h.core.headers.is_empty());
    assert_eq!(h.core.take_changes().reading_pane, Some(String::new()));
}
