//! Which asynchronous replies still matter.
//!
//! IMAP replies come back on a channel, long after the selection that asked for
//! them may have moved on. [`Latest`] tags list fetches so a late page for a
//! folder the user already left is dropped. [`BodyLoads`] does the same for
//! message bodies and also keeps at most one fetch in flight: arrowing down the
//! list asks for a body per row, and queueing all of them behind a slow server
//! would show every stale message in turn before the one the user stopped on.

/// A counter of requests where only the newest reply is wanted.
#[derive(Debug, Default)]
pub struct Latest {
    current: u64,
}

impl Latest {
    /// Starts a request, superseding every earlier one, and returns its id.
    pub fn begin(&mut self) -> u64 {
        self.current += 1;
        self.current
    }

    /// Whether `id` is still the newest request.
    pub fn is_current(&self, id: u64) -> bool {
        id == self.current
    }
}

/// What to do with a body reply, and what to fetch next.
#[derive(Debug, PartialEq, Eq)]
pub struct Finished<K> {
    /// The reply is for the message the user is looking at: show it.
    pub show: bool,
    /// The message to fetch now that the connection is free, with its request id.
    pub next: Option<(K, u64)>,
}

/// Body fetches for a selection that changes faster than the server answers.
#[derive(Debug)]
pub struct BodyLoads<K> {
    ids: Latest,
    in_flight: Option<(K, u64)>,
    wanted: Option<K>,
}

impl<K> Default for BodyLoads<K> {
    fn default() -> Self {
        Self { ids: Latest::default(), in_flight: None, wanted: None }
    }
}

impl<K: Clone + PartialEq> BodyLoads<K> {
    /// The user selected `key`. Returns the request to send now, or `None` when
    /// it has to wait for the fetch already in flight (or is that fetch).
    pub fn want(&mut self, key: K) -> Option<(K, u64)> {
        if self.wanted.as_ref() == Some(&key) {
            return None;
        }
        self.wanted = Some(key);
        self.start_wanted()
    }

    /// The user selected nothing (or left the folder): whatever is in flight is
    /// no longer wanted.
    pub fn cancel(&mut self) {
        self.wanted = None;
    }

    /// A reply (body or failure) for request `id` arrived.
    pub fn finished(&mut self, key: &K, id: u64) -> Finished<K> {
        if self.in_flight.as_ref().is_none_or(|(k, i)| k != key || *i != id) {
            return Finished { show: false, next: None };
        }
        self.in_flight = None;
        let show = self.wanted.as_ref() == Some(key);
        let next = if show { None } else { self.start_wanted() };
        Finished { show, next }
    }

    fn start_wanted(&mut self) -> Option<(K, u64)> {
        if self.in_flight.is_some() {
            return None;
        }
        let key = self.wanted.clone()?;
        let id = self.ids.begin();
        self.in_flight = Some((key.clone(), id));
        Some((key, id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_newest_list_request_is_current() {
        let mut latest = Latest::default();
        let first = latest.begin();
        let second = latest.begin();
        assert!(!latest.is_current(first));
        assert!(latest.is_current(second));
    }

    #[test]
    fn the_first_selection_is_fetched_immediately() {
        let mut loads = BodyLoads::default();
        assert_eq!(loads.want(7u32), Some((7, 1)));
    }

    #[test]
    fn selections_made_while_a_fetch_is_in_flight_collapse_to_the_last_one() {
        let mut loads = BodyLoads::default();
        let (_, first) = loads.want(1u32).unwrap();
        assert_eq!(loads.want(2), None);
        assert_eq!(loads.want(3), None);

        let done = loads.finished(&1, first);
        assert!(!done.show, "message 1 is no longer selected");
        let (key, id) = done.next.expect("the latest selection is fetched next");
        assert_eq!(key, 3);
        assert!(loads.finished(&3, id).show);
    }

    #[test]
    fn a_reply_for_the_selected_message_is_shown_and_starts_nothing() {
        let mut loads = BodyLoads::default();
        let (_, id) = loads.want(1u32).unwrap();
        assert_eq!(loads.finished(&1, id), Finished { show: true, next: None });
    }

    #[test]
    fn a_reply_that_matches_no_request_is_ignored() {
        let mut loads = BodyLoads::default();
        let (_, id) = loads.want(1u32).unwrap();
        assert_eq!(loads.finished(&1, id + 1), Finished { show: false, next: None });
        assert_eq!(loads.finished(&2, id), Finished { show: false, next: None });
    }

    #[test]
    fn cancelling_drops_the_reply_and_fetches_nothing_more() {
        let mut loads = BodyLoads::default();
        let (_, id) = loads.want(1u32).unwrap();
        loads.cancel();
        assert_eq!(loads.finished(&1, id), Finished { show: false, next: None });
        assert_eq!(loads.want(1), Some((1, 2)), "selecting it again fetches it again");
    }

    #[test]
    fn reselecting_the_message_being_fetched_does_not_refetch() {
        let mut loads = BodyLoads::default();
        loads.want(1u32).unwrap();
        assert_eq!(loads.want(1), None);
    }
}
