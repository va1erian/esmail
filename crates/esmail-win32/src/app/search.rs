//! The search box above the list. Typing runs a query against the local cache
//! after a short pause; the results replace the folder's list until the box is
//! cleared (Esc), which brings the folder back.
//!
//! The query is `esmail::search_query`'s (`from:`, `subject:`, `since:`,
//! `is:unread`, ...). It runs in the cache actor, so nothing here waits for the
//! database; the answer comes back as a `CacheEvent::Search`.

use esmail::db::SearchHit;
use esmail::imap::MailHeader;
use esmail::search_query::ParsedQuery;
use esmail_win32::core_glue::{FolderRef, SearchResults};
use win32ui::prelude::*;
use win32ui::HasText;

use super::{App, Msg};

/// How long typing pauses before the query runs, so a fast typist sends one
/// search rather than one per key.
const DEBOUNCE_MILLIS: u32 = 250;

/// What the search box currently holds and shows.
#[derive(Default)]
pub struct SearchState {
    /// The box's text.
    text: String,
    /// The pending debounce timer, while typing has not paused yet.
    timer: Option<TimerId>,
    /// Whether the box has the keyboard focus.
    focused: bool,
    /// What the last query found; while `Some`, it replaces the folder's list.
    results: Option<SearchResults>,
}

impl SearchState {
    /// Whether search results are on screen instead of the folder.
    pub fn active(&self) -> bool {
        self.results.is_some()
    }

    /// The results on screen, if any.
    pub fn results(&self) -> Option<&SearchResults> {
        self.results.as_ref()
    }

    /// The results on screen, if any, for changing.
    pub fn results_mut(&mut self) -> Option<&mut SearchResults> {
        self.results.as_mut()
    }

    /// Records whether the box has the keyboard focus.
    pub fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    /// Whether `id` is the debounce timer's tick.
    pub fn owns(&self, id: TimerId) -> bool {
        self.timer == Some(id)
    }

    /// Drops the search and its pending query without touching the list, for
    /// when the list is about to be replaced anyway.
    pub fn reset(&mut self, ui: &Ui<Msg>) {
        self.stop_timer(ui);
        self.text.clear();
        self.results = None;
    }

    fn stop_timer(&mut self, ui: &Ui<Msg>) {
        if let Some(timer) = self.timer.take() {
            ui.kill_timer(timer);
        }
    }
}

impl App {
    /// Ctrl+F: put the cursor in the search box, selecting what is there.
    pub(super) fn focus_search(&self) {
        self.search_edit.focus();
        self.search_edit.select_all();
    }

    /// The folder and header behind list row `row`: a search hit while results
    /// are on screen, otherwise a message of the open folder.
    pub(super) fn message_at(&self, row: usize) -> Option<(FolderRef, MailHeader)> {
        if let Some(results) = self.search.results() {
            return results.get(row).map(|(folder, header)| (folder.clone(), header.clone()));
        }
        let open = self.open.as_ref()?;
        Some((open.folder().clone(), open.header(row)?.clone()))
    }

    /// Enter, which the window's dialog navigation would otherwise swallow: in
    /// the search box it searches at once, in the list it opens the focused row.
    pub(super) fn enter(&mut self, ui: &Ui<Msg>) {
        if self.search.focused {
            self.run_search(ui);
        } else if self.list.has_focus() {
            self.list.open_focused();
        }
    }

    /// The box's text changed: search once typing pauses, or show the folder
    /// again when the box was emptied.
    pub(super) fn search_changed(&mut self, ui: &Ui<Msg>, text: String) {
        if text == self.search.text {
            return;
        }
        self.search.text = text;
        self.search.stop_timer(ui);
        if self.search.text.trim().is_empty() {
            self.show_folder_again();
        } else {
            self.search.timer = ui.set_timer(DEBOUNCE_MILLIS).ok();
        }
    }

    /// The debounce timer fired: send the query to the cache.
    pub(super) fn run_search(&mut self, ui: &Ui<Msg>) {
        self.search.stop_timer(ui);
        let query = ParsedQuery::parse(&self.search.text);
        if query.is_empty() {
            self.set_status("Nothing to search for in that query");
            return;
        }
        self.set_status("Searching...");
        self.core.cache().search(query);
    }

    /// The cache answered a query. A reply that arrives after the box was
    /// emptied is stale and ignored.
    pub(super) fn search_arrived(&mut self, hits: Vec<SearchHit>) {
        if self.search.text.trim().is_empty() {
            return;
        }
        let account_ids: Vec<String> = self.core.accounts().iter().map(|account| account.id.clone()).collect();
        let results = SearchResults::new(hits, &account_ids);
        self.list.set_rows(results.rows());
        self.reset_reading_pane();
        let text = self.search.text.trim();
        let status = match results.len() {
            0 => format!("No cached message matches \"{text}\""),
            1 => format!("1 cached message matches \"{text}\" (Esc to clear)"),
            count => format!("{count} cached messages match \"{text}\" (Esc to clear)"),
        };
        self.search.results = Some(results);
        self.set_status(&status);
    }

    /// Esc: empty the box, show the folder again and hand the keyboard back to
    /// the list.
    pub(super) fn clear_search(&mut self, ui: &Ui<Msg>) {
        if self.search.text.is_empty() && !self.search.active() {
            return;
        }
        self.search_edit.set_text("");
        self.search.text.clear();
        self.search.stop_timer(ui);
        self.show_folder_again();
        self.list.focus();
    }

    /// Puts the open folder back in the list after a search.
    fn show_folder_again(&mut self) {
        if self.search.results.take().is_none() {
            return;
        }
        if let Some(open) = self.open.as_ref() {
            self.list.set_rows(open.rows());
        }
        self.reset_reading_pane();
        self.folder_status();
    }

    /// The list was replaced, so nothing is selected any more.
    fn reset_reading_pane(&mut self) {
        self.selected = None;
        self.message_shown = false;
        self.bodies.cancel();
        self.reader.show_notice("Select a message to read it.");
    }
}
