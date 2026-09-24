//! The headless `Watcher` (the core of the background listener) against real
//! `mail-mock-server` instances: an account connects, new mail produces a toast
//! *and* an unread-count change, and several accounts add up -- all with no UI.
//!
//! Needs the bundled test CA trusted, like `imap_smtp_integration.rs`; skips
//! with a message unless `ESMAIL_TEST_CA_TRUSTED` is set.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use esmail::auth::Auth;
use esmail::config::AccountConfig;
use esmail::watcher::{AccountState, Watcher};
use tokio::time::timeout;

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

/// A one-user mock server and the account config that points at it.
struct Server {
    inner: mail_mock_server::RunningServer,
    user: &'static str,
    password: &'static str,
}

async fn server(user: &'static str, password: &'static str) -> Server {
    let store = mail_mock_server::new_store();
    store.lock().unwrap().add_user(user, password);
    let inner = mail_mock_server::start(store).await.expect("start mock servers");
    Server { inner, user, password }
}

impl Server {
    fn account(&self, id: &str, label: &str) -> (AccountConfig, Auth) {
        let mut config = AccountConfig::new(label.to_string(), "localhost".to_string(), self.inner.imap_addr.port(), self.user.to_string());
        config.id = id.to_string();
        (config, Auth::password(self.password.to_string()))
    }

    fn deliver(&self, from: &str, subject: &str) {
        let raw = format!(
            "From: {from}\r\nTo: {}\r\nSubject: {subject}\r\nMessage-ID: <{subject}-{from}>\r\n\r\nhi\r\n",
            self.user
        );
        self.inner.store.lock().unwrap().deliver("INBOX", raw.into_bytes());
    }
}

type Toasts = Arc<Mutex<Vec<(String, String)>>>;

/// A watcher whose toasts are recorded as `(account id, title)`.
fn watcher() -> (Watcher, Toasts) {
    let toasts: Toasts = Arc::default();
    let sink = toasts.clone();
    let watcher = Watcher::new(tokio::runtime::Handle::current(), Arc::new(move |account, title, _body| {
        sink.lock().unwrap().push((account.to_string(), title.to_string()));
    }));
    (watcher, toasts)
}

/// Keep draining the watcher until `done(&watcher)` holds.
async fn wait_until(watcher: &mut Watcher, what: &str, done: impl Fn(&Watcher) -> bool) {
    let result = timeout(WAIT, async {
        while !done(watcher) {
            watcher.next_change().await;
        }
    })
    .await;
    assert!(result.is_ok(), "no {what} within {WAIT:?} (is the test CA trusted?)");
}

/// Wait until every account in `ids` is `Watching`: connected, and past the
/// first poll that only sets the baseline, so mail from here on is announced.
async fn wait_until_watching(watcher: &mut Watcher, ids: &[&str]) {
    wait_until(watcher, "every account watching", |w| ids.iter().all(|id| w.state(id) == Some(&AccountState::Watching))).await;
}

/// Deliver one message to each of `servers` about once a second, draining the
/// watcher meanwhile, until every account has toasted at least once. Delivered
/// repeatedly because the account's `IDLE` connection may not be listening yet
/// when the first message arrives; the next poll (or push) catches up. Returns
/// how many messages each server received.
async fn deliver_until_toasted(watcher: &mut Watcher, toasts: &Toasts, servers: &[(&Server, &str)]) -> Vec<usize> {
    let toasted = |toasts: &Toasts| servers.iter().all(|(_, account)| toasts.lock().unwrap().iter().any(|(a, _)| a == account));
    let mut delivered = vec![0; servers.len()];
    let result = timeout(WAIT, async {
        loop {
            for (i, (server, account)) in servers.iter().enumerate() {
                delivered[i] += 1;
                server.deliver("sender@example.com", &format!("{account}-{}", delivered[i]));
            }
            let _ = timeout(Duration::from_secs(1), async {
                while !toasted(toasts) {
                    watcher.next_change().await;
                }
            })
            .await;
            if toasted(toasts) {
                return;
            }
        }
    })
    .await;
    assert!(result.is_ok(), "no toast for every account within {WAIT:?}: {:?}", toasts.lock().unwrap());
    delivered
}

#[tokio::test]
async fn new_mail_gives_a_toast_and_an_unread_count() {
    skip_unless_ca_trusted!();
    let alice = server("alice", "pw-alice").await;
    let (mut watcher, toasts) = watcher();
    watcher.apply_accounts(vec![alice.account("alice@a", "Alice")]);

    wait_until_watching(&mut watcher, &["alice@a"]).await;
    assert_eq!(watcher.total_unread(), 0);

    let delivered = deliver_until_toasted(&mut watcher, &toasts, &[(&alice, "alice@a")]).await;
    let expected = delivered[0] as u32;
    wait_until(&mut watcher, "the unread count to match what was delivered", |w| w.total_unread() == expected).await;

    let toasts = toasts.lock().unwrap();
    for (account, title) in toasts.iter() {
        assert_eq!(account, "alice@a", "the toast reports which account to open");
        assert!(title.contains("Alice"), "the toast names the account: {title:?}");
    }
}

#[tokio::test]
async fn unread_counts_of_several_accounts_add_up() {
    skip_unless_ca_trusted!();
    let alice = server("alice", "pw-alice").await;
    let bob = server("bob", "pw-bob").await;
    let (mut watcher, toasts) = watcher();
    watcher.apply_accounts(vec![alice.account("alice@a", "Alice"), bob.account("bob@b", "Bob")]);
    wait_until_watching(&mut watcher, &["alice@a", "bob@b"]).await;

    let delivered = deliver_until_toasted(&mut watcher, &toasts, &[(&alice, "alice@a"), (&bob, "bob@b")]).await;
    let expected = (delivered[0] + delivered[1]) as u32;
    wait_until(&mut watcher, "the total to match what was delivered", |w| w.total_unread() == expected).await;

    let mut accounts: Vec<String> = toasts.lock().unwrap().iter().map(|(a, _)| a.clone()).collect();
    accounts.sort();
    accounts.dedup();
    assert_eq!(accounts, ["alice@a", "bob@b"], "each account toasts about its own mail");
}

#[tokio::test]
async fn a_removed_account_stops_counting() {
    skip_unless_ca_trusted!();
    let alice = server("alice", "pw-alice").await;
    let (mut watcher, _toasts) = watcher();
    watcher.apply_accounts(vec![alice.account("alice@a", "Alice")]);
    wait_until_watching(&mut watcher, &["alice@a"]).await;

    alice.deliver("x@example.com", "one");
    wait_until(&mut watcher, "an unread count", |w| w.total_unread() >= 1).await;

    watcher.apply_accounts(Vec::new());
    assert!(watcher.account_ids().is_empty());
    assert_eq!(watcher.total_unread(), 0);
}
