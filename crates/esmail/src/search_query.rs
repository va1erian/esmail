//! Search query grammar (B4 of PLAN.md): parse a small DSL — bare text,
//! `from:`, `to:`, `subject:`, `body:`, `since:`, `before:`, `is:unread`,
//! `has:attachment` — into a [`ParsedQuery`], then apply it to `db.rs`'s
//! local index.
//!
//! **What's wired:** bare text (OR'd across subject/from/body, matching the
//! plan's "bare text → `OR SUBJECT x FROM x`") and the `from:`/`to:`/
//! `subject:`/`body:` fielded filters become an FTS5 `MATCH` expression via
//! [`ParsedQuery::to_fts_match`]; `since:`/`before:` and `is:unread` narrow
//! the results via [`ParsedQuery::message_filters`], which `db.rs::search`
//! checks against each hit's cached `date`/flags. A query made only of those
//! filters (no text) still searches: `search` scans the scoped messages
//! rather than going through FTS.
//!
//! **What's parsed but not applied yet:** `has:attachment` parses correctly
//! (see the tests) but is ignored, because `messages` carries no
//! attachment-presence column yet (that lands with the search-cache
//! attachments work). [`ParsedQuery::is_empty`] treats it as no query at all,
//! so a `has:attachment`-only query can't silently match everything.
//!
//! **What's not here at all:** turning a [`ParsedQuery`] into IMAP search
//! keys for the server-side `UID SEARCH` path PLAN.md's B4 also describes.
//! That's live-network-facing code with no way to verify it in this
//! environment, the same reasoning B2 and B3 note for their own deferred
//! halves — see PLAN.md §B4.

/// A search query, split into its structured filters and free-text terms.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedQuery {
    /// Bare terms with no `key:` prefix — matched against subject, from, and
    /// body (any of the three).
    pub text: Vec<String>,
    pub from: Vec<String>,
    pub to: Vec<String>,
    pub subject: Vec<String>,
    pub body: Vec<String>,
    /// From `since:`, a `YYYY-MM-DD` date — see
    /// [`ParsedQuery::message_filters`].
    pub since: Option<String>,
    /// From `before:`, a `YYYY-MM-DD` date — see
    /// [`ParsedQuery::message_filters`].
    pub before: Option<String>,
    /// From `is:unread`.
    pub is_unread: bool,
    /// From `has:attachment`; parsed but not applied yet — see the module
    /// docs.
    pub has_attachment: bool,
}

impl ParsedQuery {
    /// Parse `input`. Never fails — anything that isn't a recognized
    /// `key:value` token (including a bare `key:` with nothing after it)
    /// falls back to a bare text term, so a query that doesn't use the DSL at
    /// all just becomes free text, and no input is silently dropped.
    pub fn parse(input: &str) -> Self {
        let mut q = ParsedQuery::default();
        for token in tokenize(input) {
            if let Some(rest) = non_empty_suffix(&token, "from:") {
                q.from.push(rest);
            } else if let Some(rest) = non_empty_suffix(&token, "to:") {
                q.to.push(rest);
            } else if let Some(rest) = non_empty_suffix(&token, "subject:") {
                q.subject.push(rest);
            } else if let Some(rest) = non_empty_suffix(&token, "body:") {
                q.body.push(rest);
            } else if let Some(rest) = non_empty_suffix(&token, "since:") {
                q.since = Some(rest);
            } else if let Some(rest) = non_empty_suffix(&token, "before:") {
                q.before = Some(rest);
            } else if token == "is:unread" {
                q.is_unread = true;
            } else if token == "has:attachment" {
                q.has_attachment = true;
            } else if !token.is_empty() {
                q.text.push(token);
            }
        }
        q
    }

    /// Whether [`ParsedQuery::to_fts_match`] would have anything to search
    /// on — `false` when the query was empty, or held only filters that
    /// aren't wired into the FTS query yet (`since`/`before`/`is:unread`/
    /// `has:attachment`).
    pub fn is_fts_empty(&self) -> bool {
        self.text.is_empty()
            && self.from.is_empty()
            && self.to.is_empty()
            && self.subject.is_empty()
            && self.body.is_empty()
    }

    /// Whether the query has nothing to narrow on at all: no text and no
    /// applied filter. `has:attachment` is parsed but not applied (see the
    /// module docs), so a query of only `has:attachment` is empty here —
    /// treating it as a real query would return every message unfiltered.
    pub fn is_empty(&self) -> bool {
        self.is_fts_empty() && self.since.is_none() && self.before.is_none() && !self.is_unread
    }

    /// Build an FTS5 `MATCH` expression for `messages_fts` (see `db.rs`'s
    /// schema) covering the fielded and bare-text parts of the query.
    /// `None` when there is no text to match — an empty query, or one made
    /// only of `since:`/`before:`/`is:unread`, which `db.rs::search` applies
    /// without FTS. SQLite's `MATCH` rejects an empty string, so a caller
    /// must not pass one through.
    pub fn to_fts_match(&self) -> Option<String> {
        if self.is_fts_empty() {
            return None;
        }
        let mut clauses = Vec::new();
        for term in &self.from {
            clauses.push(format!("from_addr:{}", fts_quote(term)));
        }
        for term in &self.to {
            clauses.push(format!("to_addr:{}", fts_quote(term)));
        }
        for term in &self.subject {
            clauses.push(format!("subject:{}", fts_quote(term)));
        }
        for term in &self.body {
            clauses.push(format!("body:{}", fts_quote(term)));
        }
        for term in &self.text {
            let q = fts_quote(term);
            clauses.push(format!("(subject:{q} OR from_addr:{q} OR body:{q})"));
        }
        Some(clauses.join(" AND "))
    }

    /// The `messages`-column filters, with `since:`/`before:` already parsed
    /// into Unix timestamps so a caller can check many hits without
    /// re-parsing the query dates once per row. An unparseable date is
    /// dropped (the filter narrows nothing) rather than excluding every hit.
    pub fn message_filters(&self) -> MessageFilters {
        MessageFilters {
            since: self.since.as_deref().and_then(parse_date),
            before: self.before.as_deref().and_then(parse_date),
            unread_only: self.is_unread,
        }
    }
}

/// The `messages`-column part of a [`ParsedQuery`], ready to check against a
/// cached message's `date` and `\Seen` flag.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MessageFilters {
    /// `since:` (inclusive), Unix seconds UTC; `None` when absent or the
    /// value wasn't a `YYYY-MM-DD` date.
    pub since: Option<i64>,
    /// `before:` (exclusive), Unix seconds UTC; `None` when absent or the
    /// value wasn't a `YYYY-MM-DD` date.
    pub before: Option<i64>,
    /// `is:unread`: keep only messages without `\Seen`.
    pub unread_only: bool,
}

impl MessageFilters {
    /// Whether a message as cached (`date` is the raw header date string,
    /// `is_seen` its `\Seen` flag) passes every filter set here. A message
    /// whose date this can't parse is excluded by a date filter -- it can't
    /// be shown to fall inside the range.
    pub fn matches(&self, date: &str, is_seen: bool) -> bool {
        if self.unread_only && is_seen {
            return false;
        }
        if self.since.is_some() || self.before.is_some() {
            let Some(message_ts) = message_timestamp(date) else {
                return false;
            };
            if self.since.is_some_and(|since| message_ts < since) {
                return false;
            }
            if self.before.is_some_and(|before| message_ts >= before) {
                return false;
            }
        }
        true
    }
}

/// `YYYY-MM-DD` as Unix seconds at 00:00 UTC, [`None`] for anything else.
fn parse_date(s: &str) -> Option<i64> {
    let date = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()?;
    Some(date.and_hms_opt(0, 0, 0)?.and_utc().timestamp())
}

/// An IMAP envelope date string as Unix seconds. `None` for an empty or
/// unparseable date. Also used by `db.rs::search` to order a filter-only
/// result set, which has no FTS rank to sort by.
pub(crate) fn message_timestamp(s: &str) -> Option<i64> {
    mailparse::dateparse(s).ok().filter(|&t| t > 0)
}

/// Split `input` on whitespace, except inside `"..."`, so
/// `subject:"hello world"` stays one token (the delimiting quotes are
/// stripped, not kept in the token — `parse` never sees them). A doubled
/// quote inside a quoted phrase (`""`) is a literal `"`, the same escaping
/// convention SQL/FTS5 string literals use, rather than closing the phrase.
fn tokenize(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if in_quotes && chars.peek() == Some(&'"') => {
                chars.next();
                current.push('"');
            }
            '"' => in_quotes = !in_quotes,
            c if c.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// `token.strip_prefix(prefix)`, but only when something follows the prefix
/// — a bare `"from:"` with nothing after it is treated as plain text instead
/// of an empty filter, since an empty `from:` term can't narrow anything.
fn non_empty_suffix(token: &str, prefix: &str) -> Option<String> {
    token.strip_prefix(prefix).filter(|rest| !rest.is_empty()).map(str::to_string)
}

/// Quote a term for use as an FTS5 string literal, escaping embedded `"`s by
/// doubling them (FTS5's own escaping rule, same as SQL string literals).
fn fts_quote(term: &str) -> String {
    format!("\"{}\"", term.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_words_become_text_terms() {
        let q = ParsedQuery::parse("hello world");
        assert_eq!(q.text, vec!["hello", "world"]);
    }

    #[test]
    fn recognizes_every_documented_field() {
        let q = ParsedQuery::parse(
            "from:alice to:bob subject:hi body:there since:2026-01-01 before:2026-02-01 is:unread has:attachment",
        );
        assert_eq!(q.from, vec!["alice"]);
        assert_eq!(q.to, vec!["bob"]);
        assert_eq!(q.subject, vec!["hi"]);
        assert_eq!(q.body, vec!["there"]);
        assert_eq!(q.since, Some("2026-01-01".to_string()));
        assert_eq!(q.before, Some("2026-02-01".to_string()));
        assert!(q.is_unread);
        assert!(q.has_attachment);
    }

    #[test]
    fn quoted_phrases_stay_together_after_a_field_prefix() {
        let q = ParsedQuery::parse(r#"subject:"hello world" plain text"#);
        assert_eq!(q.subject, vec!["hello world"]);
        assert_eq!(q.text, vec!["plain", "text"]);
    }

    #[test]
    fn a_bare_quoted_phrase_is_one_text_term() {
        let q = ParsedQuery::parse(r#""hello world""#);
        assert_eq!(q.text, vec!["hello world"]);
    }

    #[test]
    fn an_empty_field_value_falls_back_to_plain_text_rather_than_vanishing() {
        // "from:" with nothing after it can't narrow anything as a filter,
        // so treating the literal token as text keeps it from silently
        // disappearing from the query.
        let q = ParsedQuery::parse("from:");
        assert!(q.from.is_empty());
        assert_eq!(q.text, vec!["from:"]);
    }

    #[test]
    fn repeated_fields_all_accumulate() {
        let q = ParsedQuery::parse("from:alice from:bob");
        assert_eq!(q.from, vec!["alice", "bob"]);
    }

    #[test]
    fn empty_input_parses_to_nothing() {
        assert_eq!(ParsedQuery::parse(""), ParsedQuery::default());
        assert_eq!(ParsedQuery::parse("   "), ParsedQuery::default());
    }

    // ── to_fts_match ──────────────────────────────────────────────────────────

    #[test]
    fn bare_text_matches_across_subject_from_and_body() {
        let q = ParsedQuery::parse("dinner");
        assert_eq!(
            q.to_fts_match().unwrap(),
            "(subject:\"dinner\" OR from_addr:\"dinner\" OR body:\"dinner\")"
        );
    }

    #[test]
    fn fielded_terms_target_only_their_own_column() {
        let q = ParsedQuery::parse("from:alice subject:hello");
        assert_eq!(q.to_fts_match().unwrap(), "from_addr:\"alice\" AND subject:\"hello\"");
    }

    #[test]
    fn filter_only_query_has_no_fts_match_but_is_not_empty() {
        let q = ParsedQuery::parse("is:unread since:2026-01-01");
        assert!(q.is_fts_empty());
        assert_eq!(q.to_fts_match(), None);
        assert!(!q.is_empty());
    }

    #[test]
    fn empty_query_yields_no_fts_match() {
        assert_eq!(ParsedQuery::parse("").to_fts_match(), None);
    }

    #[test]
    fn embedded_quotes_in_a_term_are_escaped_not_left_to_break_the_match_string() {
        let q = ParsedQuery::parse(r#"subject:"say ""hi"""#);
        assert_eq!(q.subject, vec![r#"say "hi""#]);
        assert_eq!(q.to_fts_match().unwrap(), "subject:\"say \"\"hi\"\"\"");
    }

    // ── message_filters ──────────────────────────────────────────────────────

    #[test]
    fn has_attachment_alone_is_treated_as_no_query() {
        // Parsed but not applied yet (see the module docs): counting it as a
        // query would return every message as if the filter had matched.
        let q = ParsedQuery::parse("has:attachment");
        assert!(q.has_attachment);
        assert!(q.is_empty());
    }

    #[test]
    fn message_filters_parse_since_and_before_to_utc_midnight() {
        let q = ParsedQuery::parse("since:2026-01-01 before:2026-02-01 is:unread");
        let f = q.message_filters();
        // 2026-01-01 00:00 UTC and 2026-02-01 00:00 UTC.
        assert_eq!(f.since, Some(1_767_225_600));
        assert_eq!(f.before, Some(1_769_904_000));
        assert!(f.unread_only);
    }

    #[test]
    fn unparseable_date_filter_narrows_nothing_rather_than_excluding_everything() {
        let f = ParsedQuery::parse("since:tomorrow before:whenever").message_filters();
        assert_eq!(f, MessageFilters::default());
        assert!(f.matches("Tue, 13 Jan 2026 10:00:00 +0000", true));
    }

    #[test]
    fn message_filters_check_the_range_and_the_seen_flag() {
        let f = ParsedQuery::parse("since:2026-01-01 before:2026-02-01").message_filters();
        // Before the range.
        assert!(!f.matches("Wed, 31 Dec 2025 23:59:59 +0000", true));
        // First instant of the range (`since` is inclusive).
        assert!(f.matches("Thu, 01 Jan 2026 00:00:00 +0000", true));
        // Inside the range.
        assert!(f.matches("Sat, 17 Jan 2026 12:00:00 +0000", true));
        // `before` is exclusive.
        assert!(!f.matches("Sun, 01 Feb 2026 00:00:00 +0000", true));
        // No usable date: a date filter can't place it in the range.
        assert!(!f.matches("", true));

        let unread = ParsedQuery::parse("is:unread").message_filters();
        assert!(unread.matches("Thu, 01 Jan 2026 00:00:00 +0000", false));
        assert!(!unread.matches("Thu, 01 Jan 2026 00:00:00 +0000", true));
    }
}
