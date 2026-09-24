//! New mail arriving while nobody is looking: a real account session against
//! `mail-mock-server` reports it through the hook `Core::start` was given (which
//! the window turns into a toast), and the account id the toast carries finds
//! the account again when the toast is clicked.
//!
//! Needs the bundled test CA trusted, like the library's integration tests;
//! skips with a message unless `ESMAIL_TEST_CA_TRUSTED` is set.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use esmail::config::{AccountConfig, Config};
use esmail::imap::ImapEvent;
use esmail::notify::{account_from_launch_arguments, launch_arguments};
use esmail_win32::core_glue::Core;
use esmail_win32::core_glue::resident::account_index;
use mail_mock_server::fixtures::{TEST_PASSWORD, TEST_USER};

const WAIT: Duration = Duration::from_secs(30);

#[test]
fn new_mail_is_announced_with_the_account_a_click_opens() {
    if std::env::var("ESMAIL_TEST_CA_TRUSTED").is_err() {
        eprintln!("skipping: set ESMAIL_TEST_CA_TRUSTED=1 once mail-mock-server/certs/ca.crt is trusted");
        return;
    }
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let store = mail_mock_server::new_store();
    mail_mock_server::fixtures::seed(&mut store.lock().unwrap(), 0);
    let server = runtime.block_on(mail_mock_server::start(store.clone())).unwrap();

    let mut config = Config::default();
    let mut work = AccountConfig::new("Work".into(), "localhost".into(), server.imap_addr.port(), TEST_USER.into());
    work.id = "work".into();
    config.accounts.push(work);
    // SAFETY: this is the only test in the binary, so nothing else reads the environment meanwhile.
    unsafe { std::env::set_var("ESMAIL_PASSWORD", TEST_PASSWORD) };

    let toasts: Arc<Mutex<Vec<(String, String, String)>>> = Arc::default();
    let record = Arc::clone(&toasts);
    let notify = Arc::new(move |account: &str, title: &str, body: &str| {
        record.lock().unwrap().push((account.into(), title.into(), body.into()));
    });
    let (mut core, issues) = Core::start(&config, esmail::waker::noop(), notify).unwrap();
    assert!(issues.is_empty(), "{issues:?}");

    let deadline = Instant::now() + WAIT;
    let mut polled = false;
    let mut delivered = 0;
    while Instant::now() < deadline && toasts.lock().unwrap().is_empty() {
        polled |= core.pump().iter().any(|(_, event)| matches!(event, ImapEvent::MailboxPolled { .. }));
        // Only mail after the first poll counts as new; a delivery before the
        // IDLE connection is idling is missed by design, so keep delivering.
        if polled {
            delivered += 1;
            let raw = format!("From: carol@example.com\r\nTo: {TEST_USER}\r\nSubject: hello {delivered}\r\nMessage-ID: <hello{delivered}@example.com>\r\n\r\nhi\r\n");
            store.lock().unwrap().deliver("INBOX", raw.into_bytes());
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    let toasts = toasts.lock().unwrap();
    let (account, title, _) = toasts.first().expect("a toast within 30 s (is the test CA trusted?)");
    assert!(title.starts_with("Work: New mail"), "{title}");
    // What the toast's launch arguments carry comes back as the same account.
    let clicked = account_from_launch_arguments(&launch_arguments(account)).unwrap();
    assert_eq!(account_index(core.accounts(), &clicked), Some(0));
}
