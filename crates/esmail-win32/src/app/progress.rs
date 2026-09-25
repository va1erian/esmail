//! Progress for middle-to-long operations (issue #79), shown as a small bar
//! beside the status text.
//!
//! The core tracks one operation at a time, so a new report replaces whatever
//! was on screen. Only `BulkDownload` ("Download All") reports here so far; the
//! bar is hidden whenever nothing is running.

use esmail::progress::{Progress, ProgressKind};
use esmail::view_model::progress_label;
use win32ui::prelude::*;

use super::{App, Msg};

/// Whether an `Index` report has reached the last message, so the bar should
/// give way to "Download complete".
pub(super) fn index_finished(kind: ProgressKind, progress: Progress) -> bool {
    kind == ProgressKind::Index && matches!(progress, Progress::Counted { current, total } if current == total)
}

impl App {
    /// Shows `progress` for `kind`, replacing whatever was there.
    pub(super) fn set_progress(&mut self, ui: &Ui<Msg>, kind: ProgressKind, progress: Progress) {
        let was_showing = self.progress.is_some();
        match progress {
            Progress::Counted { current, total } => {
                self.progress_bar.set_marquee(false);
                self.progress_bar.set_range(0..=total.max(1) as i32);
                self.progress_bar.set_value(current as i32);
            }
            Progress::Indeterminate => self.progress_bar.set_marquee(true),
        }
        self.progress_bar.set_visible(true);
        if !was_showing {
            ui.relayout();
        }
        self.progress = Some(kind);
        let mailbox = self.open.as_ref().map(|open| open.folder().mailbox.as_str()).unwrap_or("");
        self.set_status(&progress_label(kind, mailbox));
    }

    /// Hides the bar; the status text is left for the caller to replace.
    pub(super) fn clear_progress(&mut self, ui: &Ui<Msg>) {
        if self.progress.take().is_some() {
            self.progress_bar.set_visible(false);
            ui.relayout();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexing_is_done_only_at_the_last_message() {
        assert!(!index_finished(ProgressKind::Index, Progress::Counted { current: 0, total: 10 }));
        assert!(!index_finished(ProgressKind::Index, Progress::Counted { current: 9, total: 10 }));
        assert!(index_finished(ProgressKind::Index, Progress::Counted { current: 10, total: 10 }));
    }

    #[test]
    fn another_kind_is_never_indexing_done() {
        assert!(!index_finished(ProgressKind::Move, Progress::Counted { current: 10, total: 10 }));
        assert!(!index_finished(ProgressKind::Index, Progress::Indeterminate));
    }
}
