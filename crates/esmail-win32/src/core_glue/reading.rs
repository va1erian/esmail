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

use super::links;

/// The reading pane's colours, as `0xRRGGBB` values a frontend maps from its
/// theme. Only the header block, notices and (in the dark palette) messages
/// that set no colours of their own use them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    /// Whether this is the dark palette (the one that recolours messages).
    pub dark: bool,
    /// The pane behind the message.
    pub background: u32,
    /// Body and heading text.
    pub text: u32,
    /// Header field values and notices.
    pub muted: u32,
    /// The header block's fill.
    pub header_background: u32,
    /// The rule under the header block.
    pub header_border: u32,
    /// Links in a themed body.
    pub link: u32,
}

impl Palette {
    /// Dark text on white, as messages are written.
    pub const LIGHT: Palette =
        Palette { dark: false, background: 0xffffff, text: 0x1a1a1a, muted: 0x555555, header_background: 0xf3f3f3, header_border: 0xd8d8d8, link: 0x0b57d0 };
    /// Light text on the dark window colour.
    pub const DARK: Palette =
        Palette { dark: true, background: 0x202020, text: 0xe6e6e6, muted: 0xb0b0b0, header_background: 0x2b2b2b, header_border: 0x3c3c3c, link: 0x8ab4f8 };
}

/// How the reading pane looks: the theme's palette, and whether messages keep
/// the colours they were written with even in the dark palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Appearance {
    /// The theme's colours.
    pub palette: Palette,
    /// View > Original colours.
    pub original_colours: bool,
}

impl Appearance {
    /// Whether `body` is given the palette's page and text colours: only in
    /// the dark palette, unless the user asked for original colours or the
    /// message sets its own backgrounds (then it is complete as written, and
    /// light text on its light background would be unreadable).
    pub fn themes_body(&self, body: &str) -> bool {
        self.palette.dark && !self.original_colours && !sets_own_backgrounds(body)
    }

    /// The colour behind the page for a message shown with `themed_body`.
    pub fn page_background(&self, themed_body: bool) -> u32 {
        if themed_body || !self.palette.dark { self.palette.background } else { Palette::LIGHT.background }
    }
}

fn sets_own_backgrounds(body: &str) -> bool {
    ["background", "bgcolor"].iter().any(|needle| contains_ignore_case(body.as_bytes(), needle.as_bytes()))
}

fn contains_ignore_case(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window.eq_ignore_ascii_case(needle))
}

fn header_style(palette: &Palette) -> String {
    format!(
        "<style>.esmail-head{{background:#{:06x};border-bottom:1px solid #{:06x};margin:-12px -12px 12px -12px;padding:12px}}.esmail-head .subject{{font-size:18px;font-weight:bold;margin-bottom:6px;color:#{:06x}}}.esmail-head .field{{color:#{:06x};font-size:13px}}.esmail-head .field b{{color:#{:06x}}}.esmail-head .attachment{{font-size:13px;margin-top:4px;color:#{:06x}}}.esmail-head a{{color:#{:06x}}}</style>",
        palette.header_background, palette.header_border, palette.text, palette.muted, palette.text, palette.muted, palette.link
    )
}

/// The rules that give a message with no colours of its own the dark
/// palette. They sit before the message's own `<style>` blocks and inline
/// styles, which therefore still win.
fn body_style(palette: &Palette) -> String {
    format!("<style>body{{background:#{:06x};color:#{:06x}}}a{{color:#{:06x}}}</style>", palette.background, palette.text, palette.link)
}

/// The full document for `header`'s message: `body` is the HTML from
/// `render_message`, `attachments` its non-inline parts. `themed_body` is
/// [`Appearance::themes_body`]'s answer for `body`.
pub fn document(header: &MailHeader, body: &str, attachments: &[Attachment], palette: &Palette, themed_body: bool) -> String {
    let mut styles = header_style(palette);
    if themed_body {
        styles.push_str(&body_style(palette));
    }
    let block = header_block(header, attachments);
    // `render_message` opens with a `<style>` block; the header goes right after
    // it so it inherits the page's font and margins.
    match body.find("</style>") {
        Some(end) => {
            let end = end + "</style>".len();
            format!("{}{styles}{block}{}", &body[..end], &body[end..])
        }
        None => format!("{styles}{block}{body}"),
    }
}

/// A short notice in the reading pane's style, for "nothing selected" and for
/// errors that have no message to attach to.
pub fn notice(text: &str, palette: &Palette) -> String {
    format!(
        "<!doctype html><meta charset=\"utf-8\"><body style=\"font-family:'Segoe UI',sans-serif;font-size:14px;color:#{:06x};margin:24px\">{}</body>",
        palette.muted,
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
    attachment_list(&mut block, attachments);
    block.push_str("</div>");
    block
}

/// One line per attachment with its Save and Open links, and Save all when there
/// are several.
fn attachment_list(block: &mut String, attachments: &[Attachment]) {
    if attachments.is_empty() {
        return;
    }
    block.push_str("<div class=\"attachments\">");
    for (n, attachment) in attachments.iter().enumerate() {
        block.push_str(&format!(
            "<div class=\"attachment\"><b>{}</b> ({}) <a href=\"{}\">Save</a> <a href=\"{}\">Open</a></div>",
            escape(&attachment.filename),
            format_size(attachment.data.len()),
            links::save_href(n),
            links::open_href(n)
        ));
    }
    if attachments.len() > 1 {
        block.push_str(&format!("<div class=\"attachment\"><a href=\"{}\">Save all</a></div>", links::save_all_href()));
    }
    block.push_str("</div>");
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
        let html = document(&header(), "<p>hi</p>", &[], &Palette::LIGHT, false);
        assert!(html.contains("Lunch &lt;today&gt;"));
        assert!(html.contains("Jane &lt;jane@example.com&gt;"));
        assert!(!html.contains("<today>"));
    }

    #[test]
    fn the_header_block_goes_after_the_bodys_style_block() {
        let html = document(&header(), "<style>p{}</style><p>hi</p>", &[], &Palette::LIGHT, false);
        let style = html.find("p{}").unwrap();
        let block = html.find("esmail-head\"").unwrap();
        let body = html.find("<p>hi</p>").unwrap();
        assert!(style < block && block < body);
    }

    #[test]
    fn a_body_without_a_style_block_still_gets_the_header() {
        let html = document(&header(), "<p>hi</p>", &[], &Palette::LIGHT, false);
        assert!(html.ends_with("<p>hi</p>"));
        assert!(html.contains("esmail-head\""));
    }

    #[test]
    fn empty_fields_are_left_out_and_attachments_are_listed_with_sizes() {
        let mut header = header();
        header.to.clear();
        let attachment = Attachment { filename: "report.pdf".into(), mime_type: "application/pdf".into(), data: vec![0; 2048] };
        let html = document(&header, "", &[attachment], &Palette::LIGHT, false);
        assert!(!html.contains("<b>To:</b>"));
        assert!(html.contains("report.pdf</b> (2.0 KB)"));
        assert!(html.contains("esmail-attachment:save/0") && html.contains("esmail-attachment:open/0"));
        assert!(!html.contains("Save all"), "one attachment needs no Save all");
    }

    fn appearance(palette: Palette, original_colours: bool) -> Appearance {
        Appearance { palette, original_colours }
    }

    #[test]
    fn only_the_dark_palette_themes_a_message_that_sets_no_backgrounds() {
        let plain = "<p style=\"color:red\">hi</p>";
        assert!(appearance(Palette::DARK, false).themes_body(plain));
        assert!(!appearance(Palette::LIGHT, false).themes_body(plain));
        assert!(!appearance(Palette::DARK, true).themes_body(plain));
    }

    #[test]
    fn a_message_with_its_own_background_keeps_it_in_the_dark_palette() {
        let dark = appearance(Palette::DARK, false);
        assert!(!dark.themes_body("<table BGCOLOR=\"#fff\"><tr><td>x</td></tr></table>"));
        assert!(!dark.themes_body("<div style=\"Background-Color:#eee\">x</div>"));
        assert_eq!(dark.page_background(false), 0xffffff);
        assert_eq!(dark.page_background(true), Palette::DARK.background);
    }

    #[test]
    fn a_themed_body_gets_the_dark_rules_before_the_messages_own_styles() {
        let html = document(&header(), "<style>p{}</style><p>hi</p>", &[], &Palette::DARK, true);
        assert!(html.find("body{background:#202020").unwrap() < html.find("<p>hi</p>").unwrap());
        assert!(html.contains(".esmail-head{background:#2b2b2b"));
        let untouched = document(&header(), "<p>hi</p>", &[], &Palette::DARK, false);
        assert!(!untouched.contains("body{background"));
    }

    #[test]
    fn a_missing_subject_reads_no_subject() {
        let mut header = header();
        header.subject.clear();
        assert!(document(&header, "", &[], &Palette::LIGHT, false).contains("(no subject)"));
    }
}
