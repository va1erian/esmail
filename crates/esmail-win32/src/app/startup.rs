//! How long the window took to show its first list rows, for `--screenshot`
//! runs to report.

use std::time::{Duration, Instant};

/// The first rows the list was given, and when.
struct FirstRows {
    after: Duration,
    source: &'static str,
    count: usize,
}

/// Startup timing, measured from when `main` began.
pub struct Startup {
    began: Instant,
    first_rows: Option<FirstRows>,
}

impl Startup {
    pub fn new(began: Instant) -> Startup {
        Startup { began, first_rows: None }
    }

    /// Records that the list was given `count` rows read from `source`. Only the
    /// first non-empty call counts.
    pub fn note_rows_from(&mut self, source: &'static str, count: usize) {
        if count > 0 && self.first_rows.is_none() {
            self.first_rows = Some(FirstRows { after: self.began.elapsed(), source, count });
        }
    }

    /// One line for stderr: when rows first reached the list and when a paint
    /// first drew them.
    pub fn report(&self, first_paint: Option<Instant>) -> String {
        let Some(rows) = &self.first_rows else { return "no list rows were ever shown".to_string() };
        let painted = first_paint.map_or("never painted".to_string(), |at| format!("first painted {:.0} ms after start", at.saturating_duration_since(self.began).as_secs_f64() * 1000.0));
        format!("{} rows from the {} reached the list {:.0} ms after start; {painted}", rows.count, rows.source, rows.after.as_secs_f64() * 1000.0)
    }
}
