//! Fills the local cache with a few drafts and outbox messages, for screenshots
//! of the Drafts and Outbox windows.
//!
//! ```text
//! ESMAIL_DATA_DIR=<temp dir> cargo run -p esmail-win32 --example seed_queues -- work
//! ```
//!
//! The argument is the account's `id` in its config. Refuses to run without
//! `ESMAIL_DATA_DIR`, so it can never write into the real cache by accident.

use esmail::compose::ComposeState;
use esmail::db::{DbActor, DbCommand, DbEvent};
use esmail::search_query::ParsedQuery;
use tokio::sync::mpsc;

fn message(to: &str, subject: &str, account: &str) -> ComposeState {
    ComposeState { to: to.into(), subject: subject.into(), body: "Hello.".into(), account_id: Some(account.into()), ..Default::default() }
}

fn main() {
    let Some(account) = std::env::args().nth(1) else {
        eprintln!("usage: seed_queues ACCOUNT_ID");
        std::process::exit(2);
    };
    if std::env::var_os("ESMAIL_DATA_DIR").is_none() {
        eprintln!("seed_queues: set ESMAIL_DATA_DIR to a scratch directory first");
        std::process::exit(2);
    }
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let (commands, command_rx) = mpsc::channel(16);
    let (events_tx, mut events) = mpsc::channel(16);
    DbActor::spawn(runtime.handle(), command_rx, events_tx);
    runtime.block_on(async {
        let drafts = [("bob@example.com", "Lunch on Friday?"), ("", "Notes for the offsite"), ("carol@example.com", "")];
        for (n, (to, subject)) in drafts.into_iter().enumerate() {
            let compose = message(to, subject, &account);
            let command = DbCommand::SaveDraft { id: None, compose_id: n as u64 + 1, account_id: compose.account_id.clone(), compose };
            commands.send(command).await.expect("cache actor is running");
        }
        for (n, (to, subject)) in [("dave@example.com", "Invoice 2026-09"), ("erin@example.com", "Re: Quarterly numbers")].into_iter().enumerate() {
            let command = DbCommand::EnqueueOutbox { id: None, compose_id: 10 + n as u64, account_id: account.clone(), compose: message(to, subject, &account) };
            commands.send(command).await.expect("cache actor is running");
        }
        // The second outbox message has failed a few times.
        commands.send(DbCommand::MarkOutboxFailed { id: 2, error: "connection refused (os error 10061)".into() }).await.expect("cache actor is running");
        // Commands run in order, so the answer to a search means the writes are done.
        commands.send(DbCommand::Search { account_id: None, query: ParsedQuery::parse("seed-barrier"), mailbox: None }).await.expect("cache actor is running");
        while let Some(event) = events.recv().await {
            match event {
                DbEvent::Error(message) => panic!("cache error: {message}"),
                DbEvent::SearchResult { .. } => break,
                _ => {}
            }
        }
        println!("seeded 3 drafts and 2 outbox messages for {account}");
    });
}
