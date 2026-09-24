//! The reading pane's document: the message's header block placed in front of
//! the sanitised body that `esmail::render` produced.
//!
//! The body already handles plain-text messages (escaped, in a `<pre>`), so
//! this only adds what the pane shows above it: subject, sender, recipients,
//! date and the attachment names. Remote images are never fetched (the HTML
//! view has no network layer), which is the "off by default" behaviour.

use esmail::imap::MailHeader;
use esmail::render::Attachment;
use esmail::view_model::format_size;

const HEADER_STYLE: &str = "<style>\
.esmail-head{background:#f3f3f3;border-bottom:1px solid #d8d8d8;margin:-12px -12px 12px -12px;padding:12px}\
.esmail-head .subject{font-size:18px;font-weight:bold;margin-bottom:6px}\
.esmail-head .field{color:#555;font-size:13px}\
.esmail-head .field b{color:#1a1a1a}\
</style>";

/// The full document for `header`'s message: `body` is the HTML from
/// `render_message`, `attachments` its non-inline parts.
pub fn document(header: &MailHeader, body: &str, attachments: &[Attachment]) -> String {
    let block = header_block(header, attachments);
    // `render_message` opens with a `<style>` block; the header goes right after
    // it so it inherits the page's font and margins.
    match body.find("</style>") {
        Some(end) => {
            let end = end + "</style>".len();
            format!("{}{HEADER_STYLE}{block}{}", &body[..end], &body[end..])
        }
        None => format!("{HEADER_STYLE}{block}{body}"),
    }
}

/// A short notice in the reading pane's style, for "nothing selected" and for
/// errors that have no message to attach to.
pub fn notice(text: &str) -> String {
    format!(
        "<!doctype html><meta charset=\"utf-8\"><body style=\"font-family:'Segoe UI',sans-serif;font-size:14px;color:#555;margin:24px\">{}</body>",
        escape(text)
    )
}

fn header_block(header: &MailHeader, attachments: &[Attachment]) -> String {
    let subject = if header.subject.is_empty() { "(no subject)" } else { &header.subject };
    let mut block = format!("<div class=\"esmail-head\"><div class=\"subject\">{}</div>", escape(subject));
    field(&mut block, "From", &header.from);
    field(&mut block, "To", &header.to);
    match header.local_date_time() {
        Some((day, time)) => field(&mut block, "Date", &format!("{day} {time}")),
        None => field(&mut block, "Date", &header.date),
    }
    if !attachments.is_empty() {
        let names: Vec<String> = attachments.iter().map(|a| format!("{} ({})", a.filename, format_size(a.data.len()))).collect();
        field(&mut block, "Attachments", &names.join(", "));
    }
    block.push_str("</div>");
    block
}

fn field(block: &mut String, name: &str, value: &str) {
    if value.is_empty() {
        return;
    }
    block.push_str(&format!("<div class=\"field\"><b>{name}:</b> {}</div>", escape(value)));
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> MailHeader {
        MailHeader {
            uid: 1,
            subject: "Lunch <today>".into(),
            from: "Jane <jane@example.com>".into(),
            to: "me@example.com".into(),
            date: "Mon, 1 Sep 2025 10:36:43 +0200".into(),
            message_id: String::new(),
            flags: Vec::new(),
        }
    }

    #[test]
    fn header_fields_are_escaped_not_injected() {
        let html = document(&header(), "<p>hi</p>", &[]);
        assert!(html.contains("Lunch &lt;today&gt;"));
        assert!(html.contains("Jane &lt;jane@example.com&gt;"));
        assert!(!html.contains("<today>"));
    }

    #[test]
    fn the_header_block_goes_after_the_bodys_style_block() {
        let html = document(&header(), "<style>p{}</style><p>hi</p>", &[]);
        let style = html.find("p{}").unwrap();
        let block = html.find("esmail-head\"").unwrap();
        let body = html.find("<p>hi</p>").unwrap();
        assert!(style < block && block < body);
    }

    #[test]
    fn a_body_without_a_style_block_still_gets_the_header() {
        let html = document(&header(), "<p>hi</p>", &[]);
        assert!(html.ends_with("<p>hi</p>"));
        assert!(html.contains("esmail-head\""));
    }

    #[test]
    fn empty_fields_are_left_out_and_attachments_are_listed_with_sizes() {
        let mut header = header();
        header.to.clear();
        let attachment = Attachment { filename: "report.pdf".into(), mime_type: "application/pdf".into(), data: vec![0; 2048] };
        let html = document(&header, "", &[attachment]);
        assert!(!html.contains("<b>To:</b>"));
        assert!(html.contains("report.pdf (2.0 KB)"));
    }

    #[test]
    fn a_missing_subject_reads_no_subject() {
        let mut header = header();
        header.subject.clear();
        assert!(document(&header, "", &[]).contains("(no subject)"));
    }
}
