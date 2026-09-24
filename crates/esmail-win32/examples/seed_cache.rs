//! Fills the local mail cache with `COUNT` synthetic messages, to measure how
//! fast esmail-win32 shows a big cached mailbox.
//!
//! ```text
//! ESMAIL_DATA_DIR=<temp dir> cargo run --release -p esmail-win32 --example seed_cache -- \
//!     alice@example.com@localhost INBOX 20000
//! ```
//!
//! The account id is the `id` in the account's config; uids run 1..=COUNT, so
//! against a `mail-mock-server` with `INBOX_COUNT` the same, the server agrees
//! with the cache. Refuses to run without `ESMAIL_DATA_DIR`, so it can never
//! write into the real cache by accident.

use esmail::db::{DbActor, DbCommand, DbEvent};
use esmail::imap::{FLAG_SEEN, MailHeader};
use esmail::search_query::ParsedQuery;
use tokio::sync::mpsc;

/// Headers per command, to keep each transaction a sensible size.
const CHUNK: u32 = 1000;

fn header(uid: u32) -> MailHeader {
    // Newest message last uid; one message every 7 minutes from 2025-01-01.
    let minutes = uid * 7;
    let (day, hour, minute) = (1 + minutes / 1440 % 28, minutes / 60 % 24, minutes % 60);
    let month = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"][(minutes / 1440 / 28 % 12) as usize];
    MailHeader {
        uid,
        subject: format!("Cached message #{uid}"),
        from: format!("Sender {uid} <sender{uid}@example.com>"),
        to: "alice@example.com".to_string(),
        date: format!("{day:02} {month} 2025 {hour:02}:{minute:02}:00 +0000"),
        message_id: format!("<seed{uid}@example.com>"),
        flags: if uid % 3 == 0 { vec![FLAG_SEEN.to_string()] } else { Vec::new() },
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [account_id, mailbox, count] = args.as_slice() else {
        eprintln!("usage: seed_cache ACCOUNT_ID MAILBOX COUNT");
        std::process::exit(2);
    };
    if std::env::var_os("ESMAIL_DATA_DIR").is_none() {
        eprintln!("seed_cache: set ESMAIL_DATA_DIR to a scratch directory first");
        std::process::exit(2);
    }
    let count: u32 = count.parse().expect("COUNT is a number");

    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let (commands, command_rx) = mpsc::channel(8);
    let (events_tx, mut events) = mpsc::channel(8);
    DbActor::spawn(runtime.handle(), command_rx, events_tx);
    runtime.block_on(async {
        for first in (1..=count).step_by(CHUNK as usize) {
            let headers = (first..(first + CHUNK).min(count + 1)).map(header).collect();
            let command = DbCommand::IndexHeadersSearchable { account_id: account_id.clone(), mailbox: mailbox.clone(), headers };
            commands.send(command).await.expect("cache actor is running");
        }
        // Commands run in order, so the answer to a search means the writes are done.
        let query = ParsedQuery::parse("seed-barrier");
        commands.send(DbCommand::Search { account_id: None, query, mailbox: None }).await.expect("cache actor is running");
        match events.recv().await {
            Some(DbEvent::Error(message)) => panic!("cache error: {message}"),
            _ => println!("seeded {count} messages into {account_id} / {mailbox}"),
        }
    });
}
