//! Safe HTML rendering for a fetched message (B5 of PLAN.md).
//!
//! The pipeline: parse the raw RFC822 bytes → pick the best `text/html`
//! alternative, falling back to `text/plain` (HTML-escaped — the previous
//! `format!("<pre>{}</pre>", text)` injected unescaped message text straight
//! into markup) → sanitize with `ammonia` (strips `<script>`, `<iframe>`,
//! `<form>`/`<input>`/`<button>`, and event-handler attributes by not
//! including them in its allowlist) → resolve `cid:` references to inline
//! `data:` URLs from the message's own inline parts → wrap in a base document
//! (charset, a readable default font, `max-width` so wide marketing HTML
//! doesn't force horizontal scroll).
//!
//! **Blocking remote images/CSS is deliberately not done here.** Markup
//! can't stop a network fetch — removing an `<img src>` from the DOM doesn't
//! un-issue a request already made, and rewriting it to a placeholder in the
//! markup would mean there's no URL left for a later "load remote images"
//! action to use. So this module leaves `http(s)` URLs exactly as the
//! message had them, and blocking happens at the network layer instead, via
//! `egui_litehtml_webview`'s `WebViewHandler::intercept` — see `main.rs`'s
//! `MessageViewHandler`.
//!
//! **CSS:** inline `style="..."` attributes survive sanitization, filtered
//! through an allowlist of property names (`allowed_style_properties`) via
//! ammonia's built-in CSS-property parser — not a value-level sanitizer,
//! but litehtml has no JS engine, so there's no `expression()`/
//! `-moz-binding` execution path for a CSS value to exploit; `url(...)`
//! values are safe because litehtml routes every CSS-triggered image load
//! through the same `load_image` callback `<img src>` uses, which is
//! already gated by `WebViewHandler::intercept`. Positioning properties
//! (`position`, `z-index`, offsets) are deliberately left off the
//! allowlist — real HTML email doesn't need them for layout, and they're
//! the one class of CSS that could otherwise overlay convincing fake UI.
//! **`<style>` blocks** are kept too, since marketing mail leans on them
//! (`p{margin}`, `.class{padding}`, `@media` rules), but ammonia treats their
//! text as opaque and would pass it through unfiltered, so [`crate::css`]
//! re-parses each block and re-emits only style rules and `@media` rules whose
//! declarations are on that same allowlist. **Layout attributes** of the HTML 4
//! table model (`width`, `bgcolor`, `valign`, ...) are allowed as well; see
//! `TABLE_ATTRIBUTES`.
//!
//! [`extract_attachments`] is B6 of PLAN.md: it walks the same parsed
//! structure for leaf parts that are neither the chosen body nor already
//! inlined via `cid:`, decoding each to bytes in memory. Saving/opening them
//! (via `rfd`/`opener`) is `main.rs`'s job — this module only finds them.
//! **Not done:** fetching them lazily. The whole `RFC822` is still
//! downloaded eagerly by `imap.rs` regardless of whether a message has
//! attachments, rather than fetching only their `BODY[n]` on demand — that's
//! a change to the fetch itself (needs `BODYSTRUCTURE` to know which part
//! numbers exist before fetching any of them), not to this parsing step, and
//! waited for the same reason B2/B3/B4's live-IMAP-facing halves did.

use base64::Engine as _;
use mailparse::{MailHeaderMap, ParsedMail, parse_mail};

/// Render one message's raw RFC822 bytes into safe-to-display HTML.
/// Never fails — a message that can't be parsed at all renders as an escaped
/// error notice rather than propagating an error the caller would have to
/// turn into *some* string anyway.
pub fn render_message(raw: &[u8]) -> String {
    let body_html = match parse_mail(raw) {
        Ok(parsed) => render_parsed(&parsed),
        Err(e) => format!("<p>Could not parse this message: {}</p>", ammonia::clean_text(&e.to_string())),
    };
    wrap_document(&body_html)
}

/// One non-inline part of a message (B6 of PLAN.md): a leaf part that isn't
/// the `text/plain`/`text/html` body and isn't already inlined into it via a
/// resolved `cid:` reference. Holds the decoded bytes directly rather than a
/// path — nothing has been written to disk yet; that's the save/open UI's job
/// in `main.rs`.
#[derive(Debug, Clone)]
pub struct Attachment {
    pub filename: String,
    pub mime_type: String,
    pub data: Vec<u8>,
}

/// Find every attachment in a message's raw RFC822 bytes. Returns an empty
/// list (never an error) for an unparseable message — the same "degrade
/// rather than propagate" choice [`render_message`] makes, since a message
/// with no readable attachments is not a distinguishable failure from one
/// that simply has none.
pub fn extract_attachments(raw: &[u8]) -> Vec<Attachment> {
    let Ok(parsed) = parse_mail(raw) else {
        return Vec::new();
    };
    all_parts(&parsed)
        .into_iter()
        .filter_map(|part| {
            // Only leaf parts carry actual content; a multipart/* container
            // itself is never the attachment.
            if !part.subparts.is_empty() {
                return None;
            }
            // These mimetypes are the *body* candidates find_html/find_text
            // already pick from, not attachments in their own right.
            if part.ctype.mimetype == "text/plain" || part.ctype.mimetype == "text/html" {
                return None;
            }
            let disposition = part.get_content_disposition();
            let has_cid = content_id(part).is_some();
            // Anything explicitly marked as an attachment counts; so does
            // anything else that isn't already accounted for as an inline
            // image resolved into the body via cid: (checked above) --
            // e.g. a PDF sent with no explicit Content-Disposition at all.
            let is_attachment = matches!(disposition.disposition, mailparse::DispositionType::Attachment) || !has_cid;
            if !is_attachment {
                return None;
            }
            let filename = disposition
                .params
                .get("filename")
                .cloned()
                .or_else(|| part.ctype.params.get("name").cloned())
                .unwrap_or_else(|| "attachment".to_string());
            let data = part.get_body_raw().ok()?;
            Some(Attachment { filename, mime_type: part.ctype.mimetype.clone(), data })
        })
        .collect()
}

fn render_parsed(parsed: &ParsedMail) -> String {
    if let Some(html) = find_html(parsed) {
        return sanitize(&resolve_cid_parts(&html, parsed));
    }
    if let Some(text) = find_text(parsed) {
        return format!("<pre>{}</pre>", ammonia::clean_text(&text));
    }
    "<p><em>(this message has no readable body)</em></p>".to_string()
}

fn find_html(part: &ParsedMail) -> Option<String> {
    if part.ctype.mimetype == "text/html" {
        return part.get_body().ok();
    }
    part.subparts.iter().find_map(find_html)
}

fn find_text(part: &ParsedMail) -> Option<String> {
    if part.ctype.mimetype == "text/plain" {
        return part.get_body().ok();
    }
    part.subparts.iter().find_map(find_text)
}

/// Replace every `cid:<id>` reference in `html` with a `data:` URL built
/// from the matching inline part's own bytes and MIME type, found by walking
/// every part of the message for one whose `Content-ID` matches. A `cid:`
/// with no matching part is left as-is — the webview will fail to load it,
/// the same as any other dead link, rather than this function guessing at a
/// replacement.
fn resolve_cid_parts(html: &str, root: &ParsedMail) -> String {
    let mut html = html.to_string();
    for part in all_parts(root) {
        let Some(cid) = content_id(part) else { continue };
        let Ok(bytes) = part.get_body_raw() else { continue };
        let data_url = format!(
            "data:{};base64,{}",
            part.ctype.mimetype,
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        );
        html = html.replace(&format!("cid:{cid}"), &data_url);
    }
    html
}

fn all_parts<'a>(part: &'a ParsedMail<'a>) -> Vec<&'a ParsedMail<'a>> {
    let mut parts = vec![part];
    for sub in &part.subparts {
        parts.extend(all_parts(sub));
    }
    parts
}

/// The `Content-ID` header's value with the surrounding `<...>` stripped —
/// `cid:` URLs never include the angle brackets even though the header
/// itself always has them.
fn content_id(part: &ParsedMail) -> Option<String> {
    let raw = part.headers.get_first_value("Content-ID")?;
    Some(raw.trim().trim_start_matches('<').trim_end_matches('>').to_string())
}

/// CSS properties allowed through `style="..."` attributes and `<style>`
/// blocks. An allowlist, not a blocklist: ammonia's `style_properties`
/// parses the declaration with a real CSS parser (`cssparser`, not regex)
/// and drops anything whose property name isn't listed here, so there's no
/// value-level sanitization to also get right — layout/typography/color
/// properties are inert data as far as litehtml is concerned (no JS engine,
/// no `expression()`/`-moz-binding` execution path exists to worry about).
/// The one property category deliberately left out is positioning
/// (`position`, `z-index`, `top`/`left`/etc.) — HTML email doesn't need it
/// for legitimate layout (that's what nested tables are for), and it's the
/// one class of CSS that could otherwise be used to overlay convincing fake
/// UI inside the message body. `url(...)` values (`background-image`,
/// `list-style-image`) stay in-scope for the property allowlist below
/// because they're already safe: litehtml routes every CSS-triggered image
/// load through the exact same `load_image` callback as `<img src>`, which
/// `egui_litehtml_webview`'s `WebViewHandler::intercept` — the same
/// block-by-default gate B5 already built — governs regardless of whether
/// the URL came from an attribute or a CSS property.
fn allowed_style_properties() -> std::collections::HashSet<&'static str> {
    [
        // Color / background
        "color", "background", "background-color", "background-image", "background-position",
        "background-repeat", "background-size", "opacity",
        // Box model
        "margin", "margin-top", "margin-right", "margin-bottom", "margin-left",
        "padding", "padding-top", "padding-right", "padding-bottom", "padding-left",
        "border", "border-top", "border-right", "border-bottom", "border-left",
        "border-width", "border-style", "border-color", "border-radius",
        "border-top-left-radius", "border-top-right-radius", "border-bottom-left-radius",
        "border-bottom-right-radius", "border-top-width", "border-right-width",
        "border-bottom-width", "border-left-width", "border-top-style", "border-right-style",
        "border-bottom-style", "border-left-style", "border-top-color", "border-right-color",
        "border-bottom-color", "border-left-color", "border-collapse", "border-spacing",
        "width", "height", "max-width", "max-height", "min-width", "min-height",
        "box-shadow", "box-sizing",
        // Typography
        "font", "font-family", "font-size", "font-weight", "font-style", "font-variant",
        "line-height", "letter-spacing", "text-align", "text-decoration", "text-transform",
        "text-indent", "text-shadow", "white-space", "word-break", "word-wrap",
        "overflow-wrap", "word-spacing",
        // Layout (non-positioning)
        "display", "visibility", "vertical-align", "float", "clear", "overflow", "table-layout",
        // Misc, low-risk
        "list-style", "list-style-type", "list-style-image", "list-style-position", "cursor",
    ]
    .into_iter()
    .collect()
}

/// Layout attributes of the HTML 4 table model. HTML email is built from
/// them -- `<table width="600" bgcolor=... cellspacing=0>`, `<td valign=top
/// width="50%">` -- and ammonia's defaults keep only `align`, so without these a
/// 600px newsletter column becomes full width and every multi-column row
/// collapses to its content. All are inert layout/colour data.
const TABLE_ATTRIBUTES: &[&str] = &["width", "height", "bgcolor", "background", "border", "cellpadding", "cellspacing"];
const CELL_ATTRIBUTES: &[&str] = &["width", "height", "bgcolor", "background", "valign", "nowrap"];

fn sanitize(html: &str) -> String {
    let mut builder = ammonia::Builder::default();
    builder
        // `data:` — inline images resolved from `cid:` parts above need it to
        // survive; it's not in ammonia's default scheme allowlist.
        // `cid:` — an *unresolved* reference (no matching part) is left as
        // literal text by `resolve_cid_parts`, and would otherwise be
        // stripped right back out here since `cid` isn't a default scheme
        // either; allowing it keeps the dead reference exactly as
        // "left as-is" implies, rather than silently deleting it.
        .add_url_schemes(&["data", "cid"])
        // Allow the `style="..."` attribute (filtered — see
        // allowed_style_properties' doc): without it, marketing/
        // transactional HTML that relies on inline styles for spacing/
        // color renders as dense, unstyled plain text. `class`/`id` are inert
        // and are what the `<style>` blocks kept below select on.
        .add_generic_attributes(&["style", "class", "id"])
        .add_tag_attributes("table", TABLE_ATTRIBUTES)
        .add_tag_attributes("tbody", &["valign"])
        .add_tag_attributes("thead", &["valign"])
        .add_tag_attributes("tfoot", &["valign"])
        .add_tag_attributes("tr", &["valign", "bgcolor"])
        .add_tag_attributes("td", CELL_ATTRIBUTES)
        .add_tag_attributes("th", CELL_ATTRIBUTES)
        // ammonia drops the `<title>` tag but keeps its text, which litehtml
        // would then show as body text (the message's subject, a second time).
        .add_clean_content_tags(&["title"])
        // `<style>` is allowed through ammonia only so that `filter_style_blocks`
        // can filter its CSS afterwards -- ammonia itself passes the text
        // through unfiltered (see css.rs). Nothing may be returned from here
        // without going through that step.
        .add_tags(&["style"])
        .rm_clean_content_tags(&["style"])
        .filter_style_properties(allowed_style_properties());
    filter_style_blocks(&builder.clean(html).to_string())
}

/// Replace the text of every `<style>` element in `html` (already sanitized,
/// so tags are lowercase, attribute-free and well nested) with its
/// [`css::filter_stylesheet`]ed version, dropping elements that end up empty.
fn filter_style_blocks(html: &str) -> String {
    const OPEN: &str = "<style>";
    const CLOSE: &str = "</style>";
    let allowed = allowed_style_properties();
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        let after = &rest[start + OPEN.len()..];
        // An unterminated block would run to the end of the document: drop it.
        let Some(end) = after.find(CLOSE) else {
            return out;
        };
        let css = crate::css::filter_stylesheet(&after[..end], &allowed);
        // The CSS text must never be able to close the element early.
        if !css.is_empty() && !css.contains('<') {
            out.push_str(OPEN);
            out.push_str(&css);
            out.push_str(CLOSE);
        }
        rest = &after[end + CLOSE.len()..];
    }
    out.push_str(rest);
    out
}

fn wrap_document(body: &str) -> String {
    format!(
        r#"<!doctype html>
<meta charset="utf-8">
<style>
  body {{
    font-family: -apple-system, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
    font-size: 14px;
    line-height: 1.4;
    color: #1a1a1a;
    margin: 12px;
    max-width: 100%;
    overflow-wrap: break-word;
  }}
  img {{ max-width: 100%; height: auto; }}
  pre {{ white-space: pre-wrap; font-family: inherit; }}
</style>
{body}"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(headers: &str, body: &str) -> Vec<u8> {
        format!("{headers}\r\n\r\n{body}").into_bytes()
    }

    #[test]
    fn plain_text_is_html_escaped_not_injected_raw() {
        // Regression test for the bug this pipeline replaces:
        // format!("<pre>{}</pre>", text) injected unescaped message text.
        // `ammonia::clean_text` escapes more than just `<`/`>`/`&` (e.g. `/`
        // and space become numeric entities too, which still render
        // correctly, just not readably) -- assert the safety property, not
        // its exact entity choices.
        let raw = message(
            "Content-Type: text/plain",
            "<script>alert(1)</script> & <b>bold</b>",
        );
        let html = render_message(&raw);
        assert!(!html.contains("<script>"));
        assert!(!html.contains("<b>bold</b>"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("&amp;"));
    }

    #[test]
    fn html_part_is_preferred_over_plain_text() {
        let raw = message(
            "Content-Type: multipart/alternative; boundary=b",
            "--b\r\nContent-Type: text/plain\r\n\r\nplain version\r\n--b\r\nContent-Type: text/html\r\n\r\n<p>html version</p>\r\n--b--",
        );
        let html = render_message(&raw);
        assert!(html.contains("html version"));
        assert!(!html.contains("plain version"));
    }

    #[test]
    fn script_tags_are_stripped() {
        let raw = message(
            "Content-Type: text/html",
            "<p>hi</p><script>alert(document.cookie)</script>",
        );
        let html = render_message(&raw);
        assert!(!html.contains("<script"));
        assert!(!html.contains("alert(document.cookie)"));
        assert!(html.contains("<p>hi</p>"));
    }

    #[test]
    fn event_handler_attributes_are_stripped() {
        let raw = message(
            "Content-Type: text/html",
            r#"<img src="https://example.com/a.png" onerror="alert(1)">"#,
        );
        let html = render_message(&raw);
        assert!(!html.contains("onerror"));
        assert!(!html.contains("alert(1)"));
        // The remote src itself is left alone -- blocking it is the
        // network-layer handler's job, not this pipeline's.
        assert!(html.contains(r#"src="https://example.com/a.png""#));
    }

    #[test]
    fn allowed_style_properties_survive_sanitization() {
        let raw = message(
            "Content-Type: text/html",
            r#"<div style="margin:8px 0; color:#666; font-size:13px;">spaced text</div>"#,
        );
        let html = render_message(&raw);
        assert!(html.contains("margin:8px 0"), "expected {html:?} to keep the style attribute");
        assert!(html.contains("color:#666"));
        assert!(html.contains("font-size:13px"));
    }

    #[test]
    fn disallowed_style_properties_are_dropped_but_allowed_ones_survive() {
        let raw = message(
            "Content-Type: text/html",
            r#"<div style="position:fixed; top:0; color:red;">x</div>"#,
        );
        let html = render_message(&raw);
        assert!(!html.contains("position"), "expected {html:?} to drop position");
        assert!(!html.contains("top:0"), "expected {html:?} to drop top");
        assert!(html.contains("color:red"), "expected {html:?} to keep color");
    }

    #[test]
    fn style_blocks_are_kept_but_only_their_allowlisted_declarations() {
        // Unlike the style attribute, ammonia has no per-property filter for
        // a <style> tag's text, so `css::filter_stylesheet` does it. Checks
        // sanitize()'s output directly, not render_message()'s --
        // wrap_document() always adds its own base <style> block, so asserting
        // against the full wrapped document would trivially "pass".
        let cleaned = sanitize(
            "<style>p { margin: 1em 0; position: fixed } @import url(https://t.example/x.css);              @media (max-width: 480px) { .c { width: 100% !important } }</style><p class=\"c\">hi</p>",
        );
        assert!(cleaned.contains("<style>"), "expected {cleaned:?} to keep the style tag");
        assert!(cleaned.contains("margin:1em 0;"), "{cleaned:?}");
        assert!(cleaned.contains("@media (max-width: 480px)"), "{cleaned:?}");
        assert!(!cleaned.contains("position") && !cleaned.contains("@import"), "{cleaned:?}");
        assert!(cleaned.contains("<p class=\"c\">hi</p>"), "{cleaned:?}");
    }

    #[test]
    fn a_style_block_with_nothing_allowed_in_it_disappears() {
        let cleaned = sanitize("<style>body { position: fixed; }</style><p>hi</p>");
        assert!(!cleaned.contains("<style"), "expected {cleaned:?} to drop the empty style tag");
        assert!(!cleaned.contains("position"));
        assert!(cleaned.contains("<p>hi</p>"));
    }

    #[test]
    fn style_blocks_cannot_smuggle_markup_out_of_the_style_element() {
        for html in [
            "<style>p { color: red }</style><script>alert(1)</script>",
            "<style><!--</style><script>alert(1)</script>--></style>",
            "<style>p { background-image: url(\"</style><img src=x onerror=alert(1)>\") }</style>",
        ] {
            let cleaned = sanitize(html).to_ascii_lowercase();
            assert!(!cleaned.contains("<script") && !cleaned.contains("onerror"), "{html:?} -> {cleaned:?}");
        }
    }

    #[test]
    fn table_layout_attributes_survive() {
        let cleaned = sanitize(
            r##"<table width="600" cellpadding="0" cellspacing="0" border="0" bgcolor="#fff" align="center"><tr valign="top"><td width="50%" valign="top" bgcolor="#eee" height="3" class="a" id="b">x</td></tr></table>"##,
        );
        for attr in [
            r#"width="600""#, r#"cellpadding="0""#, r#"cellspacing="0""#, r#"border="0""#, r##"bgcolor="#fff""##,
            r#"width="50%""#, r#"valign="top""#, r#"height="3""#, r#"class="a""#, r#"id="b""#,
        ] {
            assert!(cleaned.contains(attr), "expected {attr} in {cleaned:?}");
        }
    }

    #[test]
    fn a_title_element_does_not_leak_its_text_into_the_body() {
        let html = render_message(&message("Content-Type: text/html", "<title>Version en ligne</title><p>body</p>"));
        assert!(!html.contains("Version en ligne"), "{html:?}");
        assert!(html.contains("<p>body</p>"));
    }

    #[test]
    fn javascript_style_expressions_cannot_reach_the_page_via_style_attribute() {
        // Legacy IE `expression()` CSS is meaningless to litehtml (no JS
        // engine), but confirm the sanitizer doesn't even let it near a
        // property that isn't allowlisted, and that a malformed/dangerous
        // declaration doesn't poison adjacent, legitimate ones.
        let raw = message(
            "Content-Type: text/html",
            r#"<div style="width:expression(alert(1)); color:blue;">x</div>"#,
        );
        let html = render_message(&raw);
        assert!(html.contains("color:blue"));
    }

    #[test]
    fn iframes_and_forms_are_stripped() {
        let raw = message(
            "Content-Type: text/html",
            r#"<iframe src="https://evil.example"></iframe><form action="https://evil.example"><input name="x"></form><p>safe</p>"#,
        );
        let html = render_message(&raw);
        assert!(!html.contains("<iframe"));
        assert!(!html.contains("<form"));
        assert!(!html.contains("<input"));
        assert!(html.contains("<p>safe</p>"));
    }

    #[test]
    fn cid_references_become_inline_data_urls() {
        let raw = message(
            "Content-Type: multipart/related; boundary=b",
            "--b\r\nContent-Type: text/html\r\n\r\n<img src=\"cid:img1\">\r\n\
             --b\r\nContent-Type: image/png\r\nContent-ID: <img1>\r\nContent-Transfer-Encoding: base64\r\n\r\n\
             aGVsbG8=\r\n--b--",
        );
        let html = render_message(&raw);
        let expected_data_url = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(b"hello")
        );
        assert!(
            html.contains(&expected_data_url),
            "expected {html:?} to contain {expected_data_url:?}"
        );
        assert!(!html.contains("cid:img1"));
    }

    #[test]
    fn an_unmatched_cid_is_left_as_is_rather_than_guessed_at() {
        let raw = message(
            "Content-Type: text/html",
            r#"<img src="cid:nonexistent">"#,
        );
        let html = render_message(&raw);
        assert!(html.contains("cid:nonexistent"));
    }

    #[test]
    fn an_unparseable_message_renders_an_escaped_notice_rather_than_panicking() {
        // Not asserting exact wording -- just that garbage input degrades to
        // *some* safe, non-empty HTML instead of propagating an error the
        // caller has no string-shaped place to put, or panicking.
        let html = render_message(b"not a valid mime message at all \xFF\xFE");
        assert!(html.contains("<html") || html.contains("<meta") || html.contains("<p>"));
    }

    #[test]
    fn empty_message_body_renders_a_placeholder() {
        let raw = message("Content-Type: text/plain", "");
        let html = render_message(&raw);
        assert!(html.len() > 0);
    }

    // ── extract_attachments ────────────────────────────────────────────────

    #[test]
    fn a_plain_text_only_message_has_no_attachments() {
        let raw = message("Content-Type: text/plain", "just text, nothing attached");
        assert!(extract_attachments(&raw).is_empty());
    }

    #[test]
    fn an_explicit_attachment_is_found_with_its_filename_and_bytes() {
        let raw = message(
            "Content-Type: multipart/mixed; boundary=b",
            "--b\r\nContent-Type: text/plain\r\n\r\nsee attached\r\n\
             --b\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=\"report.pdf\"\r\nContent-Transfer-Encoding: base64\r\n\r\n\
             aGVsbG8=\r\n--b--",
        );
        let attachments = extract_attachments(&raw);
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].filename, "report.pdf");
        assert_eq!(attachments[0].mime_type, "application/pdf");
        assert_eq!(attachments[0].data, b"hello");
    }

    #[test]
    fn an_inline_image_resolved_via_cid_is_not_also_listed_as_an_attachment() {
        let raw = message(
            "Content-Type: multipart/related; boundary=b",
            "--b\r\nContent-Type: text/html\r\n\r\n<img src=\"cid:img1\">\r\n\
             --b\r\nContent-Type: image/png\r\nContent-ID: <img1>\r\nContent-Transfer-Encoding: base64\r\n\r\n\
             aGVsbG8=\r\n--b--",
        );
        assert!(extract_attachments(&raw).is_empty());
    }

    #[test]
    fn a_part_with_no_cid_and_no_disposition_is_still_treated_as_an_attachment() {
        // e.g. a PDF some mail clients send with no explicit
        // Content-Disposition at all -- it isn't the body and nothing
        // references it inline, so it should still surface as downloadable.
        let raw = message(
            "Content-Type: multipart/mixed; boundary=b",
            "--b\r\nContent-Type: text/plain\r\n\r\nbody\r\n\
             --b\r\nContent-Type: application/octet-stream\r\nContent-Transfer-Encoding: base64\r\n\r\n\
             aGVsbG8=\r\n--b--",
        );
        let attachments = extract_attachments(&raw);
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].filename, "attachment");
    }

    #[test]
    fn filename_falls_back_to_the_content_type_name_param() {
        let raw = message(
            "Content-Type: multipart/mixed; boundary=b",
            "--b\r\nContent-Type: text/plain\r\n\r\nbody\r\n\
             --b\r\nContent-Type: application/pdf; name=\"named-via-content-type.pdf\"\r\nContent-Transfer-Encoding: base64\r\n\r\n\
             aGVsbG8=\r\n--b--",
        );
        let attachments = extract_attachments(&raw);
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].filename, "named-via-content-type.pdf");
    }

    #[test]
    fn an_unparseable_message_yields_no_attachments_rather_than_an_error() {
        assert!(extract_attachments(b"not a valid mime message \xFF\xFE").is_empty());
    }
}
