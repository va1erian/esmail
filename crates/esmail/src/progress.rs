//! Progress reporting for middle-to-long operations (issue #79).
//!
//! An operation qualifies as "middle-to-long" if it either makes more than
//! one network/disk round trip (a bulk flag/move over a selection, indexing a
//! mailbox) or is a single step whose duration scales with the user's data
//! (an SMTP send, a message export, an attachment's disk write). A single
//! round trip for one message does not qualify on its own.
//!
//! The app tracks **one** such operation at a time: a new report replaces
//! whatever the previous one was, and the UI disables starting a second bulk
//! action while one is in flight. A report names its [`ProgressKind`] -- the
//! UI turns that into the label -- and is either counted (a loop over items,
//! drawn as a bar) or indeterminate (a single step with no natural count,
//! drawn as a spinner). There is deliberately no cancellation support.

/// What an in-flight operation is doing. A closed enum rather than free-form
/// text so the protocol modules (`imap.rs`, `smtp.rs`) never invent user-facing
/// copy -- that stays in `main.rs`'s rendering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgressKind {
    /// `BulkDownload`: indexing a mailbox into the local cache.
    Index,
    /// A bulk flag change over a selection (mark read/unread, star/unstar).
    Flags,
    /// A bulk archive/delete move over a selection.
    Move,
    /// Saving a sent message into the account's Sent folder (`APPEND`).
    Append,
    /// Exporting a message's raw source to an `.eml` file.
    Export,
    /// Writing an attachment to disk (`Save…`/`Open`).
    Attachment,
    /// An SMTP send.
    Send,
}

impl ProgressKind {
    /// Whether this is a bulk action over a selection. The UI disables
    /// starting a second one while any is already running, rather than
    /// tracking several at once (issue #79's one-at-a-time decision).
    pub fn is_bulk(self) -> bool {
        matches!(self, ProgressKind::Index | ProgressKind::Flags | ProgressKind::Move)
    }
}

/// How far an operation has got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    /// A loop over items: `current` of `total` are done. Drawn as a bar.
    Counted { current: u32, total: u32 },
    /// A single step with no useful count. Drawn as a spinner.
    Indeterminate,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_selection_wide_kinds_are_bulk() {
        assert!(ProgressKind::Index.is_bulk());
        assert!(ProgressKind::Flags.is_bulk());
        assert!(ProgressKind::Move.is_bulk());
        assert!(!ProgressKind::Append.is_bulk());
        assert!(!ProgressKind::Export.is_bulk());
        assert!(!ProgressKind::Attachment.is_bulk());
        assert!(!ProgressKind::Send.is_bulk());
    }
}
