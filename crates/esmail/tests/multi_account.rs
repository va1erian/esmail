//! Several accounts connected at once (issue #35): two real `AccountSession`s
//! -- each its own `ImapActor`, `IDLE` watch and new-mail watermark -- against
//! two `mail-mock-server` instances, checking that each account is notified
//! about exactly its own mail, that one account failing does not affect the
//! other, and that dropping a session really stops its watching.
//!
//! The mock server keeps one global mailbox namespace per instance, so "two
//! users" is two servers with one user each rather than two users on one
//! server; from the client's side that is the same thing (two hosts/ports,
//! two credentials).
//!
//! Needs the bundled test CA trusted, like `imap_smtp_integration.rs`; skips
//! with a message unless `ESMAIL_TEST_CA_TRUSTED` is set.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use esmail::session::{AccountEvent, AccountSession, Hooks, SessionParams};
use esmail::auth::Auth;
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

/// A one-user mock server plus what it takes to open a session on it.
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
    fn params(&self, id: &str, label: &str, password: &str) -> SessionParams {
        SessionParams {
            id: id.to_string(),
            label: label.to_string(),
            host: "localhost".to_string(),
            port: self.inner.imap_addr.port(),
            username: self.user.to_string(),
            auth: Auth::password(password.to_string()),
            watch_mailbox: "INBOX".to_string(),
        }
    }

    fn deliver(&self, from: &str, subject: &str) {
        let raw = format!(
            "From: {from}\r\nTo: {}\r\nSubject: {subject}\r\nMessage-ID: <{subject}-{from}>\r\n\r\nhi\r\n",
            self.user
        );
        self.inner.store.lock().unwrap().deliver("INBOX", raw.into_bytes());
    }
}

/// What the UI side of the wiring saw: every tagged event (just its kind, as
/// `Debug` text) and every toast.
#[derive(Clone, Default)]
struct Seen {
    events: Arc<Mutex<Vec<(String, String)>>>,
    toasts: Arc<Mutex<Vec<(String, String)>>>,
    /// The account id each toast was shown for (what a click would report
    /// back), with its title, in the same order as `toasts`.
    toast_accounts: Arc<Mutex<Vec<(String, String)>>>,
}

impl Seen {
    fn has_event(&self, account: &str, kind: &str) -> bool {
        self.events.lock().unwrap().iter().any(|(a, k)| a == account && k.starts_with(kind))
    }

    fn toasts_titled(&self, prefix: &str) -> Vec<(String, String)> {
        self.toasts.lock().unwrap().iter().filter(|(t, _)| t.starts_with(prefix)).cloned().collect()
    }

    async fn wait_for_event(&self, account: &str, kind: &str) {
        timeout(WAIT, async {
            while !self.has_event(account, kind) {
                sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no {kind} event for {account} within {WAIT:?} (is the test CA trusted?)"));
    }
}

/// Channel + hooks a session is spawned with, and a task that drains the
/// channel into `Seen` (the forwarder blocks if nobody reads it, as the real
/// UI does every frame).
fn wiring() -> (mpsc::Sender<AccountEvent>, Hooks, Seen) {
    let seen = Seen::default();
    let (tx, mut rx) = mpsc::channel::<AccountEvent>(64);
    let sink = seen.clone();
    tokio::spawn(async move {
        while let Some((account, event)) = rx.recv().await {
            let kind = format!("{event:?}");
            sink.events.lock().unwrap().push((account, kind));
        }
    });
    let toasts = seen.toasts.clone();
    let toast_accounts = seen.toast_accounts.clone();
    let hooks = Hooks {
        notify: Arc::new(move |account, title, body| {
            toast_accounts.lock().unwrap().push((account.to_string(), title.to_string()));
            toasts.lock().unwrap().push((title.to_string(), body.to_string()));
        }),
        repaint: Arc::new(|| {}),
    };
    (tx, hooks, seen)
}

/// Deliver a message to `server` every 500ms until `done` says enough toasts
/// arrived. Retrying rather than delivering once because a push that lands
/// before the `IDLE` connection is actually idling is missed by design (see
/// `idle_push_notifies_of_new_mail_without_polling`); once it *is* idling the
/// very next delivery is seen.
async fn deliver_until(servers: &[(&Server, &str, &str)], done: impl Fn() -> bool) {
    timeout(WAIT, async {
        let mut n = 0;
        while !done() {
            n += 1;
            for (server, from, subject) in servers {
                server.deliver(from, &format!("{subject}-{n}"));
            }
            sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("expected notifications within {WAIT:?}"));
}

#[tokio::test]
async fn each_account_is_notified_of_its_own_mail_and_only_its_own() {
    skip_unless_ca_trusted!();
    let a = server("alice@work.example", "pw-a").await;
    let b = server("bob@home.example", "pw-b").await;
    let (tx, hooks, seen) = wiring();

    let _work = AccountSession::spawn(a.params("alice@work", "Work", a.password), tx.clone(), hooks.clone());
    let _home = AccountSession::spawn(b.params("bob@home", "Home", b.password), tx, hooks);

    // Both connect independently, and the first poll each session makes after
    // connecting has set its watermark: mail delivered from here on is new.
    for account in ["alice@work", "bob@home"] {
        seen.wait_for_event(account, "Connected").await;
        seen.wait_for_event(account, "MailboxPolled").await;
    }

    let probe = seen.clone();
    deliver_until(&[(&a, "carol@work.example", "for-work"), (&b, "dave@home.example", "for-home")], move || {
        !probe.toasts_titled("Work:").is_empty() && !probe.toasts_titled("Home:").is_empty()
    })
    .await;

    // Every toast names one of the two accounts, and carries only that
    // account's mail: nothing crossed over.
    let all = seen.toasts.lock().unwrap().clone();
    for (title, body) in &all {
        if let Some(rest) = title.strip_prefix("Work:") {
            assert!(
                rest.contains("carol@work.example") || body.contains("new messages"),
                "Work toast should be about Work mail: {title:?} / {body:?}"
            );
            assert!(!body.contains("for-home") && !rest.contains("dave"), "Home mail leaked into Work: {title:?} / {body:?}");
        } else if let Some(rest) = title.strip_prefix("Home:") {
            assert!(
                rest.contains("dave@home.example") || body.contains("new messages"),
                "Home toast should be about Home mail: {title:?} / {body:?}"
            );
            assert!(!body.contains("for-work") && !rest.contains("carol"), "Work mail leaked into Home: {title:?} / {body:?}");
        } else {
            panic!("toast does not name an account: {title:?} / {body:?}");
        }
    }
}

#[tokio::test]
async fn a_bad_password_on_one_account_does_not_stop_the_other() {
    skip_unless_ca_trusted!();
    let a = server("alice@work.example", "pw-a").await;
    let b = server("bob@home.example", "pw-b").await;
    let (tx, hooks, seen) = wiring();

    let _bad = AccountSession::spawn(a.params("alice@work", "Work", "not-the-password"), tx.clone(), hooks.clone());
    let _good = AccountSession::spawn(b.params("bob@home", "Home", b.password), tx, hooks);

    seen.wait_for_event("alice@work", "Error").await;
    seen.wait_for_event("bob@home", "Connected").await;
    seen.wait_for_event("bob@home", "MailboxPolled").await;
    assert!(!seen.has_event("alice@work", "Connected"), "the bad-password account must not connect");

    let probe = seen.clone();
    deliver_until(&[(&b, "dave@home.example", "for-home")], move || !probe.toasts_titled("Home:").is_empty()).await;
    assert!(seen.toasts_titled("Work:").is_empty());
}

#[tokio::test]
async fn dropping_a_session_stops_its_watching_and_leaves_the_other_untouched() {
    skip_unless_ca_trusted!();
    let a = server("alice@work.example", "pw-a").await;
    let b = server("bob@home.example", "pw-b").await;
    let (tx, hooks, seen) = wiring();

    let work = AccountSession::spawn(a.params("alice@work", "Work", a.password), tx.clone(), hooks.clone());
    let _home = AccountSession::spawn(b.params("bob@home", "Home", b.password), tx, hooks);
    for account in ["alice@work", "bob@home"] {
        seen.wait_for_event(account, "Connected").await;
        seen.wait_for_event(account, "MailboxPolled").await;
    }

    // Both are watching: prove it before tearing anything down.
    let probe = seen.clone();
    deliver_until(&[(&a, "carol@work.example", "before"), (&b, "dave@home.example", "before")], move || {
        !probe.toasts_titled("Work:").is_empty() && !probe.toasts_titled("Home:").is_empty()
    })
    .await;

    // Logout / Remove account on Work is just dropping its session.
    drop(work);
    sleep(Duration::from_millis(500)).await; // let in-flight work settle
    let work_toasts = seen.toasts_titled("Work:").len();
    let home_toasts = seen.toasts_titled("Home:").len();

    // Work mail keeps arriving but nothing is watching it any more; Home,
    // untouched, keeps notifying.
    let probe = seen.clone();
    deliver_until(&[(&a, "carol@work.example", "after"), (&b, "dave@home.example", "after")], move || {
        probe.toasts_titled("Home:").len() > home_toasts
    })
    .await;
    sleep(Duration::from_secs(1)).await;
    assert_eq!(
        seen.toasts_titled("Work:").len(),
        work_toasts,
        "a dropped session must not keep watching (no stale IDLE connection)"
    );
}

/// Nothing limits the number of accounts to two: four at once, each with its
/// own server, session, watcher and watermark, and each notified about its
/// own mail only. Every mailbox's mail is tagged with its account's name so a
/// toast that crossed over is recognisable.
#[tokio::test]
async fn four_accounts_each_get_only_their_own_notifications() {
    skip_unless_ca_trusted!();
    const NAMES: [&str; 4] = ["Alpha", "Bravo", "Charlie", "Delta"];
    let (tx, hooks, seen) = wiring();

    let mut servers = Vec::new();
    let mut sessions = Vec::new();
    for name in NAMES {
        let user: &'static str = Box::leak(format!("user@{}.example", name.to_lowercase()).into_boxed_str());
        let server = server(user, "pw").await;
        sessions.push(AccountSession::spawn(
            server.params(&format!("id-{name}"), name, "pw"),
            tx.clone(),
            hooks.clone(),
        ));
        servers.push(server);
    }
    for name in NAMES {
        let id = format!("id-{name}");
        seen.wait_for_event(&id, "Connected").await;
        seen.wait_for_event(&id, "MailboxPolled").await;
    }

    // One sender per account, named after it, and a subject saying the same.
    let senders: Vec<String> = NAMES.iter().map(|n| format!("from-{}@elsewhere.example", n.to_lowercase())).collect();
    let subjects: Vec<String> = NAMES.iter().map(|n| format!("for-{}", n.to_lowercase())).collect();
    let deliveries: Vec<(&Server, &str, &str)> = servers
        .iter()
        .zip(senders.iter().zip(subjects.iter()))
        .map(|(server, (from, subject))| (server, from.as_str(), subject.as_str()))
        .collect();
    let probe = seen.clone();
    deliver_until(&deliveries, move || NAMES.iter().all(|n| !probe.toasts_titled(&format!("{n}:")).is_empty())).await;

    for (title, body) in seen.toasts.lock().unwrap().iter() {
        let owner = NAMES
            .iter()
            .find(|n| title.starts_with(&format!("{n}:")))
            .unwrap_or_else(|| panic!("toast does not name an account: {title:?}"));
        let own_sender = format!("from-{}@", owner.to_lowercase());
        // A single-message toast names the sender in the title and the
        // subject in the body; a batch only counts messages.
        assert!(
            title.contains(&own_sender) || body.contains("new messages"),
            "{owner} toast is not about {owner} mail: {title:?} / {body:?}"
        );
        for other in NAMES.iter().filter(|n| *n != owner) {
            assert!(
                !title.contains(&format!("from-{}@", other.to_lowercase()))
                    && !body.contains(&format!("for-{}", other.to_lowercase())),
                "{other} mail leaked into a {owner} toast: {title:?} / {body:?}"
            );
        }
    }
    // Each toast carries the id of the account it names, which is what a click
    // on it reports back to open that account.
    for (account, title) in seen.toast_accounts.lock().unwrap().iter() {
        let owner = NAMES.iter().find(|n| title.starts_with(&format!("{n}:"))).expect("toast names an account");
        assert_eq!(account, &format!("id-{owner}"), "toast {title:?} carries the wrong account id");
    }
    drop(sessions);
}
