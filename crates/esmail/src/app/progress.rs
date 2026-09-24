//! The status bar's progress slot and bulk-action tracking.

use crate::progress::{Progress, ProgressKind};

use super::{AppCore, BulkAction, ProgressView};

impl AppCore {
    /// Show `kind` as the operation the status bar is reporting, replacing
    /// whatever was there before (one slot at a time -- see [`ProgressView`]).
    pub fn set_progress(&mut self, kind: ProgressKind, progress: Progress) {
        self.progress = Some(ProgressView { kind, progress });
    }

    /// Clear the status bar, but only if it is still showing `kind`: a
    /// terminal event for an operation another one already replaced must not
    /// blank the newer operation's indicator.
    pub fn clear_progress(&mut self, kind: ProgressKind) {
        if self.progress.as_ref().is_some_and(|view| view.kind == kind) {
            self.progress = None;
        }
    }

    /// Whether a bulk action is already running, in which case starting a
    /// second one is disabled rather than shown concurrently (issue #79).
    /// Covers both a flag/move loop tracked here and an indexing run reported
    /// by the IMAP actor.
    pub fn bulk_action_in_flight(&self) -> bool {
        self.bulk_action.is_some() || self.progress.as_ref().is_some_and(|view| view.kind.is_bulk())
    }

    /// Start the status bar counting a bulk action's `targets`, and remember
    /// them so [`Self::advance_bulk_action`] knows when it is done.
    pub fn begin_bulk_action(&mut self, kind: ProgressKind, targets: &[u32]) {
        let total = targets.len() as u32;
        // A single-message action is one round trip and needs no progress UI
        // (issue #79); only an aggregate loop over a selection does.
        if total < 2 {
            return;
        }
        self.bulk_action = Some(BulkAction { kind, total, pending: targets.iter().copied().collect() });
        self.set_progress(kind, Progress::Counted { current: 0, total });
    }

    /// Tick a bulk action's progress for one message. A reply for a `uid` this
    /// action did not target (e.g. B8's delayed mark-as-read firing while a
    /// bulk flag change is running) is ignored.
    pub(super) fn advance_bulk_action(&mut self, kind: ProgressKind, uid: u32) {
        let Some(action) = self.bulk_action.as_mut() else { return };
        if action.kind != kind || !action.pending.remove(&uid) {
            return;
        }
        let done = action.total - action.pending.len() as u32;
        let total = action.total;
        let finished = action.pending.is_empty();
        if finished {
            self.bulk_action = None;
            self.clear_progress(kind);
        } else {
            self.set_progress(kind, Progress::Counted { current: done, total });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Hooks;
    use tokio::sync::mpsc;

    fn core() -> AppCore {
        let (db_tx, _db_rx) = mpsc::channel(1);
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        AppCore::new(rt.handle().clone(), crate::waker::noop(), Hooks::none().notify, db_tx)
    }

    fn counted(core: &AppCore) -> Option<(u32, u32)> {
        match core.progress.as_ref()?.progress {
            Progress::Counted { current, total } => Some((current, total)),
            _ => None,
        }
    }

    #[test]
    fn a_single_target_needs_no_bulk_tracking() {
        let mut core = core();
        core.begin_bulk_action(ProgressKind::Flags, &[7]);
        assert!(core.bulk_action.is_none() && core.progress.is_none());
        assert!(!core.bulk_action_in_flight());
    }

    #[test]
    fn a_bulk_action_counts_replies_and_finishes() {
        let mut core = core();
        core.begin_bulk_action(ProgressKind::Move, &[1, 2, 3]);
        assert!(core.bulk_action_in_flight());
        assert_eq!(counted(&core), Some((0, 3)));

        core.advance_bulk_action(ProgressKind::Move, 2);
        assert_eq!(counted(&core), Some((1, 3)));
        core.advance_bulk_action(ProgressKind::Move, 1);
        core.advance_bulk_action(ProgressKind::Move, 3);
        assert!(core.bulk_action.is_none() && core.progress.is_none());
    }

    #[test]
    fn replies_for_other_uids_or_kinds_are_ignored() {
        let mut core = core();
        core.begin_bulk_action(ProgressKind::Flags, &[1, 2]);
        core.advance_bulk_action(ProgressKind::Flags, 9);
        core.advance_bulk_action(ProgressKind::Move, 1);
        assert_eq!(counted(&core), Some((0, 2)));
    }

    #[test]
    fn clearing_progress_leaves_a_newer_operation_alone() {
        let mut core = core();
        core.set_progress(ProgressKind::Send, Progress::Indeterminate);
        core.clear_progress(ProgressKind::Attachment);
        assert!(core.progress.is_some());
        core.clear_progress(ProgressKind::Send);
        assert!(core.progress.is_none());
    }

    #[test]
    fn banners_get_distinct_ids() {
        let mut core = core();
        core.push_banner("a".into());
        core.push_banner("b".into());
        assert_ne!(core.banners[0].id, core.banners[1].id);
    }
}
