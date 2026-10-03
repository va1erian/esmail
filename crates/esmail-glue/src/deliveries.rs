//! Which messages are on their way out, and what each SMTP outcome asks for.
//!
//! A send is attempted straight away. When it fails, the message is recorded in
//! the outbox table so it is retried later (by this app or the egui one) and is
//! not lost with its window; when it succeeds, the outbox row and the draft it
//! may have left behind are done. This type keeps the bookkeeping that ties the
//! three together and stays free of any I/O, so the decisions are unit tested.

use std::collections::{HashMap, HashSet};

use esmail::compose::{ComposeId, ComposeState};
use esmail::db::OutboxItem;

/// A message being sent right now, kept in case the send fails and the outbox
/// needs its content.
struct Flight {
    account: usize,
    state: ComposeState,
}

/// What a successful send leaves to do.
#[derive(Debug, PartialEq, Eq)]
pub struct Delivered {
    /// The account it was sent from, whose Sent folder gets a copy.
    pub account: usize,
    /// The outbox row to mark done, when the message had failed before.
    pub outbox_row: Option<i64>,
}

/// What a failed send asks the outbox to do.
#[derive(Debug, PartialEq)]
pub enum Failure {
    /// The message already has an outbox row: back it off further.
    Backoff {
        /// The row.
        row: i64,
    },
    /// First failure: record the message so it is retried.
    Enqueue {
        /// The account's index.
        account: usize,
        /// The message as it was sent.
        state: ComposeState,
    },
}

/// A retry the outbox timer found due.
#[derive(Debug)]
pub struct Retry {
    /// The id the attempt's outcome will carry back.
    pub id: ComposeId,
    /// The account's index.
    pub account: usize,
    /// The message to send.
    pub state: ComposeState,
}

/// Sends in flight and the outbox rows compose ids own.
#[derive(Default)]
pub struct Deliveries {
    last_id: ComposeId,
    in_flight: HashMap<ComposeId, Flight>,
    /// The outbox row a compose id owns, once its message failed to send.
    outbox_rows: HashMap<ComposeId, i64>,
}

impl Deliveries {
    /// A fresh id for a compose window or a background retry.
    pub fn next_id(&mut self) -> ComposeId {
        self.last_id += 1;
        self.last_id
    }

    /// Records that `state` is being sent from `account` under `id`.
    pub fn begin(&mut self, id: ComposeId, account: usize, state: ComposeState) {
        self.in_flight.insert(id, Flight { account, state });
    }

    /// Takes back a `begin` whose command could not be queued.
    pub fn cancel(&mut self, id: ComposeId) {
        self.in_flight.remove(&id);
    }

    /// Whether `id` is being sent.
    pub fn is_sending(&self, id: ComposeId) -> bool {
        self.in_flight.contains_key(&id)
    }

    /// The outbox says which row `id`'s message was recorded in.
    pub fn outbox_enqueued(&mut self, id: ComposeId, row: i64) {
        self.outbox_rows.insert(id, row);
    }

    /// The outbox row `id` owns, if any.
    pub fn outbox_row(&self, id: ComposeId) -> Option<i64> {
        self.outbox_rows.get(&id).copied()
    }

    /// `id` is gone for good (sent, or discarded by the user): forgets it and
    /// returns its outbox row, if it had one.
    pub fn forget(&mut self, id: ComposeId) -> Option<i64> {
        self.in_flight.remove(&id);
        self.outbox_rows.remove(&id)
    }

    /// The send `id` succeeded.
    pub fn delivered(&mut self, id: ComposeId) -> Option<Delivered> {
        let flight = self.in_flight.remove(&id)?;
        Some(Delivered { account: flight.account, outbox_row: self.outbox_rows.remove(&id) })
    }

    /// The send `id` failed.
    pub fn failed(&mut self, id: ComposeId) -> Option<Failure> {
        let flight = self.in_flight.remove(&id)?;
        Some(match self.outbox_rows.get(&id) {
            Some(&row) => Failure::Backoff { row },
            None => Failure::Enqueue { account: flight.account, state: flight.state },
        })
    }

    /// Turns the rows the outbox says are due into sends. A row is skipped
    /// while an open window owns it (its user may press Send at any moment) or
    /// while it is already being sent; rows of an account that is not
    /// configured are returned in the second list so they can be backed off.
    pub fn retries(
        &mut self,
        due: Vec<OutboxItem>,
        window_open: impl Fn(ComposeId) -> bool,
        account_index: impl Fn(&str) -> Option<usize>,
    ) -> (Vec<Retry>, Vec<OutboxItem>) {
        let owned: HashSet<i64> = self.outbox_rows.iter().filter(|(id, _)| window_open(**id) || self.in_flight.contains_key(id)).map(|(_, row)| *row).collect();
        let (mut retries, mut unsendable) = (Vec::new(), Vec::new());
        for item in due.into_iter().filter(|item| !owned.contains(&item.id)) {
            let Some(account) = account_index(&item.account_id) else {
                unsendable.push(item);
                continue;
            };
            let id = self.next_id();
            self.outbox_rows.insert(id, item.id);
            self.in_flight.insert(id, Flight { account, state: item.compose.clone() });
            retries.push(Retry { id, account, state: item.compose });
        }
        (retries, unsendable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(subject: &str) -> ComposeState {
        ComposeState { subject: subject.into(), ..Default::default() }
    }

    fn item(row: i64) -> OutboxItem {
        OutboxItem { id: row, account_id: "a".into(), compose: state("queued"), attempts: 1, last_error: None }
    }

    #[test]
    fn ids_are_never_reused() {
        let mut deliveries = Deliveries::default();
        assert_ne!(deliveries.next_id(), deliveries.next_id());
    }

    #[test]
    fn a_first_failure_asks_for_the_message_to_be_recorded() {
        let mut deliveries = Deliveries::default();
        deliveries.begin(1, 2, state("hello"));
        assert_eq!(deliveries.failed(1), Some(Failure::Enqueue { account: 2, state: state("hello") }));
        assert!(!deliveries.is_sending(1));
    }

    #[test]
    fn a_failure_of_a_recorded_message_backs_its_row_off() {
        let mut deliveries = Deliveries::default();
        deliveries.begin(1, 0, state("hello"));
        deliveries.outbox_enqueued(1, 7);
        assert_eq!(deliveries.failed(1), Some(Failure::Backoff { row: 7 }));
        assert_eq!(deliveries.outbox_row(1), Some(7));
    }

    #[test]
    fn a_delivery_hands_back_the_account_and_the_row_to_close() {
        let mut deliveries = Deliveries::default();
        deliveries.begin(1, 3, state("hello"));
        deliveries.outbox_enqueued(1, 9);
        assert_eq!(deliveries.delivered(1), Some(Delivered { account: 3, outbox_row: Some(9) }));
        assert_eq!(deliveries.outbox_row(1), None);
        assert_eq!(deliveries.delivered(1), None, "an outcome is reported once");
    }

    #[test]
    fn forgetting_a_message_returns_its_row_so_a_discard_can_delete_it() {
        let mut deliveries = Deliveries::default();
        deliveries.outbox_enqueued(4, 11);
        assert_eq!(deliveries.forget(4), Some(11));
        assert_eq!(deliveries.forget(4), None);
    }

    #[test]
    fn due_rows_become_sends_unless_a_window_or_a_send_owns_them() {
        let mut deliveries = Deliveries::default();
        deliveries.outbox_enqueued(1, 10);
        deliveries.outbox_enqueued(2, 20);
        deliveries.begin(2, 0, state("in flight"));
        let (retries, unsendable) = deliveries.retries(vec![item(10), item(20), item(30)], |id| id == 1, |_| Some(0));
        assert!(unsendable.is_empty());
        assert_eq!(retries.len(), 1);
        assert_eq!(deliveries.outbox_row(retries[0].id), Some(30));
        assert!(deliveries.is_sending(retries[0].id));
    }

    #[test]
    fn a_row_of_an_unknown_account_is_returned_rather_than_retried() {
        let mut deliveries = Deliveries::default();
        let (retries, unsendable) = deliveries.retries(vec![item(5)], |_| false, |_| None);
        assert!(retries.is_empty());
        assert_eq!(unsendable.len(), 1);
    }
}
