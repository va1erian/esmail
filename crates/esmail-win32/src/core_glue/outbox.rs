//! Drafts and the send-retry queue, both tables of the cache database. The
//! rows are shared with the egui app: a draft saved here shows in its Drafts
//! window and a message queued there is retried here.

use esmail::compose::{ComposeId, ComposeState};
use esmail::db::DbCommand;

use super::Cache;

impl Cache {
    /// Writes `state` as a draft (into the row `state.draft_id` names, else a
    /// new one). The row's id comes back as [`CacheEvent::DraftSaved`].
    ///
    /// [`CacheEvent::DraftSaved`]: super::CacheEvent::DraftSaved
    pub fn save_draft(&self, compose_id: ComposeId, state: ComposeState) {
        self.command(DbCommand::SaveDraft { id: state.draft_id, compose_id, account_id: state.account_id.clone(), compose: state });
    }

    /// Forgets a draft.
    pub fn delete_draft(&self, id: i64) {
        self.command(DbCommand::DeleteDraft { id });
    }

    /// Asks for every draft; they come back as [`CacheEvent::Drafts`].
    ///
    /// [`CacheEvent::Drafts`]: super::CacheEvent::Drafts
    pub fn list_drafts(&self) {
        self.command(DbCommand::ListDrafts);
    }

    /// Asks for one draft's message; it comes back as [`CacheEvent::DraftLoaded`].
    ///
    /// [`CacheEvent::DraftLoaded`]: super::CacheEvent::DraftLoaded
    pub fn load_draft(&self, id: i64) {
        self.command(DbCommand::LoadDraft { id });
    }

    /// Asks for every queued message; they come back as [`CacheEvent::Outbox`].
    ///
    /// [`CacheEvent::Outbox`]: super::CacheEvent::Outbox
    pub fn list_outbox(&self) {
        self.command(DbCommand::ListOutbox);
    }

    /// Records a message that failed to send so it is tried again later. The
    /// row's id comes back as [`CacheEvent::OutboxEnqueued`].
    ///
    /// [`CacheEvent::OutboxEnqueued`]: super::CacheEvent::OutboxEnqueued
    pub fn enqueue_outbox(&self, compose_id: ComposeId, account_id: String, state: ComposeState) {
        self.command(DbCommand::EnqueueOutbox { id: None, compose_id, account_id, compose: state });
    }

    /// Asks for the outbox rows that are due for another attempt; they come
    /// back as [`CacheEvent::OutboxDue`].
    ///
    /// [`CacheEvent::OutboxDue`]: super::CacheEvent::OutboxDue
    pub fn due_outbox(&self) {
        self.command(DbCommand::DueOutbox);
    }

    /// The outbox row's message went out.
    pub fn mark_outbox_sent(&self, id: i64) {
        self.command(DbCommand::MarkOutboxSent { id });
    }

    /// The outbox row's message failed again: back it off.
    pub fn mark_outbox_failed(&self, id: i64, error: String) {
        self.command(DbCommand::MarkOutboxFailed { id, error });
    }

    /// Drops a queued message (the user discarded it).
    pub fn delete_outbox(&self, id: i64) {
        self.command(DbCommand::DeleteOutbox { id });
    }
}
