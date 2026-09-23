//! Frontend-agnostic view helpers, pulled out of `main.rs` (issue #97).
//!
//! The pure, egui-free pieces of the UI live here: the small formatters, the
//! special-use mailbox lookup, shift-click range selection, and [`RowModel`],
//! the text half of a message-list row. The egui frontend builds the same
//! [`RowModel`] and formats the same strings as a future Win32 frontend, so
//! the two cannot drift apart on wording.

use std::collections::BTreeSet;

use crate::imap::{MailHeader, MailboxRow, SpecialUse};
use crate::progress::ProgressKind;

/// The user-facing label for a progress report. Kept here rather than in the
/// protocol modules so all UI copy stays in one place (see [`ProgressKind`]).
pub fn progress_label(kind: ProgressKind, mailbox: &str) -> String {
    match kind {
        ProgressKind::Index => format!("Indexing {mailbox}..."),
        ProgressKind::Flags => "Updating flags...".to_string(),
        ProgressKind::Move => "Moving messages...".to_string(),
        ProgressKind::Append => "Saving to Sent...".to_string(),
        ProgressKind::Export => "Exporting message...".to_string(),
        ProgressKind::Attachment => "Saving attachment...".to_string(),
        ProgressKind::Send => "Sending message...".to_string(),
    }
}

/// `filename` comes straight from the message's own
/// Content-Disposition/Content-Type header — an attacker-controlled sender's
/// mail. Taking only the final path component (and falling back to a fixed
/// name if that leaves nothing usable) keeps a crafted `"../../../whatever"`
/// or an absolute path from writing outside the caller's chosen directory,
/// since `Path::join` would otherwise honor either verbatim.
///
/// Splits on `/` *and* `\` manually rather than using `std::path::Path`:
/// `Path`'s separator handling is host-OS-dependent, so on a Linux build
/// `Path::new(r"C:\Windows\System32\evil.dll").file_name()` treats the
/// whole string as one component (`\` isn't a separator on Unix) and
/// returns it unstripped. A sender-controlled filename is untrusted
/// regardless of which OS esmail happens to be running on, so the
/// stripping has to be too.
pub fn safe_attachment_filename(filename: &str) -> String {
    match filename.rsplit(['/', '\\']).next() {
        Some(name) if !name.is_empty() && name != "." && name != ".." => name.to_string(),
        _ => "attachment".to_string(),
    }
}

/// A default file name for exporting a message: its subject, reduced to
/// characters that are safe in a file name on every OS, plus `.eml`. Falls
/// back to the UID when the subject leaves nothing usable.
pub fn export_file_name(subject: &str, uid: u32) -> String {
    let cleaned: String = subject
        .chars()
        .map(|c| if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.') { c } else { '_' })
        .collect();
    let cleaned: String = cleaned.trim_matches(|c: char| c == '.' || c == '_' || c.is_whitespace()).chars().take(60).collect();
    if cleaned.is_empty() {
        format!("message-{uid}.eml")
    } else {
        format!("{}.eml", cleaned.trim_end())
    }
}

/// The set of UIDs between `anchor` and `uid` (inclusive) in `list`'s
/// current order, for shift-click range selection (B8). Falls back to just
/// `{uid}` if either isn't actually in `list` (e.g. the anchor was on a page
/// that's since been paged away from).
pub fn select_range(list: &[MailHeader], anchor: u32, uid: u32) -> BTreeSet<u32> {
    let idx_a = list.iter().position(|h| h.uid == anchor);
    let idx_b = list.iter().position(|h| h.uid == uid);
    match (idx_a, idx_b) {
        (Some(a), Some(b)) => {
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            list[lo..=hi].iter().map(|h| h.uid).collect()
        }
        _ => std::iter::once(uid).collect(),
    }
}

/// The real mailbox name for a special-use role, from whichever
/// `MailboxRow` in `rows` classifies as `want` -- pure half of
/// `EsMailApp::special_use_mailbox`, pulled out so it's testable without
/// constructing a whole `EsMailApp`. Falls back to `default` if no row
/// matches (e.g. before `Mailboxes` has arrived, or a server that
/// advertises no special-use attributes and has no conventionally-named
/// folder for `want` either -- see `imap::SpecialUse::from_name`).
pub fn find_special_use_mailbox(rows: &[MailboxRow], want: SpecialUse, default: &str) -> String {
    rows.iter()
        .find(|row| row.special_use == Some(want))
        .and_then(|row| row.full_name.clone())
        .unwrap_or_else(|| default.to_string())
}

/// A human-readable size, e.g. `"4.2 KB"`. Only goes up to MB since a mail
/// attachment in the GB range would be unusual enough to want the exact byte
/// count anyway.
pub fn format_size(bytes: usize) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= MB {
        format!("{:.1} MB", bytes / MB)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes / KB)
    } else {
        format!("{} B", bytes as u64)
    }
}

/// The text half of one message-list row: everything `message_row` draws or
/// announces, computed once from a [`MailHeader`] so a non-egui frontend can
/// paint the same row. The raw header strings are kept alongside the
/// display-ready ones because the row's hover tooltip and its screen-reader
/// label show those verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowModel {
    /// The sender as the row shows it: the display name, else the bare
    /// address, else `"(unknown sender)"`.
    pub sender: String,
    /// The subject as the row shows it, or `"(no subject)"` when empty.
    pub subject: String,
    /// The raw `From` header, shown in the row's hover tooltip.
    pub from: String,
    /// The raw `Subject` header, shown in the row's hover tooltip.
    pub raw_subject: String,
    /// The raw `Date` header, announced by screen readers.
    pub raw_date: String,
    /// The message's date as `("YYYY-MM-DD", "HH:MM")` in the local time
    /// zone, or `None` when the header is missing or unreadable.
    pub local_date_time: Option<(String, String)>,
    pub seen: bool,
    pub flagged: bool,
}

impl RowModel {
    pub fn from_header(header: &MailHeader) -> Self {
        let sender = header.sender_name();
        let sender = if sender.is_empty() { "(unknown sender)".to_string() } else { sender };
        let subject = if header.subject.is_empty() { "(no subject)".to_string() } else { header.subject.clone() };
        Self {
            sender,
            subject,
            from: header.from.clone(),
            raw_subject: header.subject.clone(),
            raw_date: header.date.clone(),
            local_date_time: header.local_date_time(),
            seen: header.is_seen(),
            flagged: header.is_flagged(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_size_uses_bytes_below_one_kb() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
    }

    #[test]
    fn format_size_uses_kb_between_one_kb_and_one_mb() {
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(4300), "4.2 KB");
    }

    #[test]
    fn format_size_uses_mb_at_one_mb_and_above() {
        assert_eq!(format_size(1024 * 1024), "1.0 MB");
        assert_eq!(format_size(5 * 1024 * 1024 + 512 * 1024), "5.5 MB");
    }

    #[test]
    fn progress_label_names_the_mailbox_only_when_indexing() {
        assert_eq!(progress_label(ProgressKind::Index, "INBOX"), "Indexing INBOX...");
        assert_eq!(progress_label(ProgressKind::Flags, "INBOX"), "Updating flags...");
        assert_eq!(progress_label(ProgressKind::Send, "INBOX"), "Sending message...");
    }

    // ── safe_attachment_filename ─────────────────────────────────────────────

    #[test]
    fn safe_attachment_filename_passes_an_ordinary_name_through() {
        assert_eq!(safe_attachment_filename("report.pdf"), "report.pdf");
    }

    #[test]
    fn safe_attachment_filename_strips_relative_traversal() {
        // Regression test: a crafted "../../../whatever" from a malicious
        // sender's Content-Disposition header must not be able to write
        // outside the caller's chosen directory when joined onto it.
        assert_eq!(safe_attachment_filename("../../../evil.exe"), "evil.exe");
        assert_eq!(safe_attachment_filename("../../etc/passwd"), "passwd");
    }

    #[test]
    fn safe_attachment_filename_strips_a_windows_absolute_path() {
        assert_eq!(
            safe_attachment_filename(r"C:\Windows\System32\evil.dll"),
            "evil.dll"
        );
    }

    #[test]
    fn safe_attachment_filename_falls_back_when_nothing_usable_remains() {
        assert_eq!(safe_attachment_filename(""), "attachment");
        assert_eq!(safe_attachment_filename(".."), "attachment");
        assert_eq!(safe_attachment_filename("/"), "attachment");
    }

    // ── export_file_name ─────────────────────────────────────────────────────

    #[test]
    fn export_file_name_uses_the_subject_with_unsafe_characters_replaced() {
        assert_eq!(export_file_name("Votre projet: un coup de pouce.", 7), "Votre projet_ un coup de pouce.eml");
        assert_eq!(export_file_name(r"a/b\c?d", 7), "a_b_c_d.eml");
    }

    #[test]
    fn export_file_name_falls_back_to_the_uid() {
        assert_eq!(export_file_name("", 42), "message-42.eml");
        assert_eq!(export_file_name("???", 42), "message-42.eml");
    }

    #[test]
    fn export_file_name_is_bounded() {
        let name = export_file_name(&"x".repeat(500), 1);
        assert_eq!(name.len(), 60 + ".eml".len());
    }

    // ── find_special_use_mailbox ─────────────────────────────────────────────

    fn row(full_name: Option<&str>, special_use: Option<SpecialUse>) -> MailboxRow {
        let label = full_name.unwrap_or("").to_string();
        MailboxRow { depth: 0, key: label.clone(), label, full_name: full_name.map(str::to_string), special_use, has_children: false }
    }

    #[test]
    fn find_special_use_mailbox_prefers_a_classified_mailbox_over_the_default() {
        // Regression test for issue #9: Gmail's Sent folder is
        // "[Gmail]/Sent Mail", not "Sent" -- special-use discovery must
        // pick the real name over the hardcoded fallback.
        let rows = vec![
            row(Some("INBOX"), Some(SpecialUse::Inbox)),
            row(Some("[Gmail]/Sent Mail"), Some(SpecialUse::Sent)),
        ];
        assert_eq!(find_special_use_mailbox(&rows, SpecialUse::Sent, "Sent"), "[Gmail]/Sent Mail");
    }

    #[test]
    fn find_special_use_mailbox_falls_back_to_default_when_nothing_matches() {
        let rows = vec![row(Some("INBOX"), Some(SpecialUse::Inbox))];
        assert_eq!(find_special_use_mailbox(&rows, SpecialUse::Trash, "Trash"), "Trash");
    }

    #[test]
    fn find_special_use_mailbox_falls_back_when_mailboxes_have_not_loaded_yet() {
        assert_eq!(find_special_use_mailbox(&[], SpecialUse::Archive, "Archive"), "Archive");
    }

    // ── select_range ─────────────────────────────────────────────────────────

    fn headers(uids: &[u32]) -> Vec<MailHeader> {
        uids.iter()
            .map(|&uid| MailHeader {
                uid,
                subject: String::new(),
                from: String::new(),
                to: String::new(),
                date: String::new(),
                message_id: String::new(),
                flags: Vec::new(),
            })
            .collect()
    }

    #[test]
    fn select_range_takes_the_inclusive_span_between_anchor_and_target() {
        let list = headers(&[5, 6, 7, 8, 9]);
        assert_eq!(select_range(&list, 6, 8), BTreeSet::from([6, 7, 8]));
    }

    #[test]
    fn select_range_works_with_the_anchor_after_the_target() {
        let list = headers(&[5, 6, 7, 8, 9]);
        assert_eq!(select_range(&list, 8, 6), BTreeSet::from([6, 7, 8]));
    }

    #[test]
    fn select_range_falls_back_to_the_target_when_the_anchor_is_gone() {
        let list = headers(&[5, 6, 7]);
        assert_eq!(select_range(&list, 99, 6), BTreeSet::from([6]));
        assert_eq!(select_range(&list, 6, 99), BTreeSet::from([99]));
    }

    // ── RowModel ─────────────────────────────────────────────────────────────

    fn header(subject: &str, from: &str, date: &str, flags: &[&str]) -> MailHeader {
        MailHeader {
            uid: 1,
            subject: subject.to_string(),
            from: from.to_string(),
            to: String::new(),
            date: date.to_string(),
            message_id: String::new(),
            flags: flags.iter().map(|f| f.to_string()).collect(),
        }
    }

    #[test]
    fn row_model_falls_back_for_an_empty_sender_and_subject() {
        let row = RowModel::from_header(&header("", "", "", &[]));
        assert_eq!(row.sender, "(unknown sender)");
        assert_eq!(row.subject, "(no subject)");
        assert_eq!(row.raw_subject, "");
        assert!(!row.seen);
        assert!(!row.flagged);
        assert_eq!(row.local_date_time, None);
    }

    #[test]
    fn row_model_keeps_the_display_text_raw_headers_and_flags() {
        let h = header(
            "Hello",
            "Jane Doe <jane@example.com>",
            "Mon, 15 Sep 2025 10:36:43 +0200",
            &["\\Seen", "\\Flagged"],
        );
        let row = RowModel::from_header(&h);
        assert_eq!(row.sender, "Jane Doe");
        assert_eq!(row.from, "Jane Doe <jane@example.com>");
        assert_eq!(row.subject, "Hello");
        assert_eq!(row.raw_subject, "Hello");
        assert_eq!(row.raw_date, "Mon, 15 Sep 2025 10:36:43 +0200");
        assert!(row.seen);
        assert!(row.flagged);
        assert!(row.local_date_time.is_some());
    }
}
