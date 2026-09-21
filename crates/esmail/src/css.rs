//! Filtering the CSS of a message's `<style>` blocks (B5 of PLAN.md).
//!
//! `ammonia` filters the `style="..."` *attribute* property by property, but
//! treats a `<style>` element's text as opaque, so allowing the tag would pass
//! its CSS through completely unfiltered. Marketing mail is full of `<style>`
//! blocks though (class-based spacing, `table{border-collapse}`, `p{margin}`,
//! `@media` rules for narrow windows), and without them it renders wrongly.
//! So [`filter_stylesheet`] re-parses each block with a real CSS tokenizer
//! (`cssparser`, not a regex) and re-emits only what is known to be safe:
//!
//! * **Style rules** and **`@media` rules** (litehtml evaluates the query
//!   against the pane width, so a narrow pane gets the message's mobile rules).
//!   Every other at-rule is dropped: `@import` (a second, unblockable-by-markup
//!   fetch path), `@font-face`, `@keyframes`, `@charset`, `@namespace`, ...
//! * **Declarations** whose property is on the same allowlist the `style`
//!   attribute uses -- so `position`, `z-index`, offsets etc. stay banned in
//!   stylesheets too -- with their `!important` flag. A value mentioning
//!   `expression(` or `javascript:` is dropped as well; neither does anything in
//!   litehtml, this is belt and braces.
//!
//! `url(...)` values stay: litehtml loads every CSS image through the same
//! callback as `<img src>`, which `WebViewHandler::intercept` gates.
//!
//! The output is rebuilt from tokens rather than copied, and a selector or value
//! containing `<`, `{` or `}` is rejected, so the result can never contain a
//! `</style>` that would end the element early.

use std::collections::HashSet;

use cssparser::{
    AtRuleParser, BasicParseErrorKind, CowRcStr, DeclarationParser, ParseError, Parser, ParserInput, ParserState,
    QualifiedRuleParser, RuleBodyItemParser, RuleBodyParser, StyleSheetParser, Token, parse_important,
};

/// Filter the text of one `<style>` element down to allowlisted rules.
/// Returns an empty string when nothing survives.
pub fn filter_stylesheet(css: &str, allowed: &HashSet<&'static str>) -> String {
    let mut input = ParserInput::new(css);
    let mut parser = Parser::new(&mut input);
    let mut filter = Filter { allowed, context: Context::Top };
    let mut out = String::new();
    for rule in StyleSheetParser::new(&mut parser, &mut filter).flatten() {
        out.push_str(&rule);
        out.push('\n');
    }
    out
}

#[derive(Clone, Copy, PartialEq)]
enum Context {
    /// Rules at the top level of the sheet.
    Top,
    /// Inside `@media { ... }`: style rules only, no nesting.
    Media,
    /// Inside a rule's `{ ... }`: declarations only.
    Declarations,
}

struct Filter<'a> {
    allowed: &'a HashSet<&'static str>,
    context: Context,
}

/// A selector list, media query or declaration value: must not be able to
/// close the block it is emitted into, or the `<style>` element.
fn is_safe_text(text: &str) -> bool {
    !text.is_empty() && !text.contains(['<', '{', '}'])
}

/// Everything up to the end of the delimited `input`, as source text.
fn consume_text<'i>(input: &mut Parser<'i, '_>) -> String {
    let start = input.position();
    while input.next_including_whitespace_and_comments().is_ok() {}
    input.slice_from(start).trim().to_string()
}

impl<'i> DeclarationParser<'i> for Filter<'_> {
    type Declaration = String;
    type Error = ();

    fn parse_value<'t>(
        &mut self,
        name: CowRcStr<'i>,
        input: &mut Parser<'i, 't>,
        _start: &ParserState,
    ) -> Result<String, ParseError<'i, ()>> {
        let name = name.to_ascii_lowercase();
        let Some(&allowed_name) = self.allowed.get(name.as_str()) else {
            return Err(input.new_custom_error(()));
        };
        // The value runs up to `!important` or the end of the declaration.
        let start = input.position();
        loop {
            let before = input.state();
            match input.next_including_whitespace_and_comments() {
                Ok(Token::Delim('!')) => {
                    input.reset(&before);
                    break;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        let value = input.slice_from(start).trim().to_string();
        let important = input.try_parse(parse_important).is_ok();
        let lower = value.to_ascii_lowercase();
        if !is_safe_text(&value) || lower.contains("expression(") || lower.contains("javascript:") {
            return Err(input.new_custom_error(()));
        }
        Ok(format!("{allowed_name}:{value}{};", if important { " !important" } else { "" }))
    }
}

impl<'i> AtRuleParser<'i> for Filter<'_> {
    type Prelude = String;
    type AtRule = String;
    type Error = ();

    fn parse_prelude<'t>(&mut self, name: CowRcStr<'i>, input: &mut Parser<'i, 't>) -> Result<String, ParseError<'i, ()>> {
        if self.context != Context::Top || !name.eq_ignore_ascii_case("media") {
            return Err(input.new_error(BasicParseErrorKind::AtRuleInvalid(name)));
        }
        let query = consume_text(input);
        if is_safe_text(&query) { Ok(query) } else { Err(input.new_custom_error(())) }
    }

    fn parse_block<'t>(
        &mut self,
        query: String,
        _start: &ParserState,
        input: &mut Parser<'i, 't>,
    ) -> Result<String, ParseError<'i, ()>> {
        let mut inner = Filter { allowed: self.allowed, context: Context::Media };
        let rules: String = StyleSheetParser::new(input, &mut inner).flatten().collect();
        if rules.is_empty() {
            return Err(input.new_custom_error(()));
        }
        Ok(format!("@media {query} {{\n{rules}}}"))
    }
}

impl<'i> QualifiedRuleParser<'i> for Filter<'_> {
    type Prelude = String;
    type QualifiedRule = String;
    type Error = ();

    fn parse_prelude<'t>(&mut self, input: &mut Parser<'i, 't>) -> Result<String, ParseError<'i, ()>> {
        let selectors = consume_text(input);
        if is_safe_text(&selectors) { Ok(selectors) } else { Err(input.new_custom_error(())) }
    }

    fn parse_block<'t>(
        &mut self,
        selectors: String,
        _start: &ParserState,
        input: &mut Parser<'i, 't>,
    ) -> Result<String, ParseError<'i, ()>> {
        let mut body = Filter { allowed: self.allowed, context: Context::Declarations };
        let declarations: String = RuleBodyParser::new(input, &mut body).flatten().collect();
        if declarations.is_empty() {
            return Err(input.new_custom_error(()));
        }
        Ok(format!("{selectors} {{ {declarations} }}\n"))
    }
}

impl<'i> RuleBodyItemParser<'i, String, ()> for Filter<'_> {
    fn parse_declarations(&self) -> bool {
        true
    }
    fn parse_qualified(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed() -> HashSet<&'static str> {
        ["color", "margin", "padding", "background-color", "display", "width", "font-size", "background-image"]
            .into_iter()
            .collect()
    }

    fn filter(css: &str) -> String {
        filter_stylesheet(css, &allowed())
    }

    #[test]
    fn allowlisted_declarations_survive_and_others_are_dropped() {
        let out = filter("p { margin: 1em 0; position: fixed; color: red !important; z-index: 9 }");
        assert!(out.contains("margin:1em 0;"), "{out}");
        assert!(out.contains("color:red !important;"), "{out}");
        assert!(!out.contains("position") && !out.contains("z-index"), "{out}");
    }

    #[test]
    fn a_rule_left_with_no_declarations_disappears() {
        assert_eq!(filter("div { position: absolute; top: 0 }"), "");
    }

    #[test]
    fn selectors_survive_verbatim() {
        let out = filter("table td.a > b, *:not(#x).c[data-a=\"1\"]:hover { color: red }");
        assert!(out.contains("table td.a > b, *:not(#x).c[data-a=\"1\"]:hover {"), "{out}");
    }

    #[test]
    fn media_rules_survive_with_their_filtered_contents() {
        let out = filter("@media only screen and (max-width: 480px) { .c { width: 100% !important; position: fixed } }");
        assert!(out.starts_with("@media only screen and (max-width: 480px) {"), "{out}");
        assert!(out.contains(".c { width:100% !important; }"), "{out}");
        assert!(!out.contains("position"), "{out}");
    }

    #[test]
    fn other_at_rules_are_dropped_but_following_rules_are_kept() {
        let out = filter(
            "@charset \"utf-8\"; @import url(https://t.example/x.css); @font-face { font-family: x; src: url(a) }\
             @keyframes k { from { color: red } } @media print { @media screen { p { color: red } } } b { color: blue }",
        );
        assert_eq!(out.trim(), "b { color:blue; }", "{out}");
    }

    #[test]
    fn dangerous_values_are_dropped() {
        let out = filter("p { width: expression(alert(1)); color: red; background-color: javascript:x }");
        assert!(out.contains("color:red;") && !out.contains("expression") && !out.contains("javascript"), "{out}");
    }

    #[test]
    fn the_output_cannot_end_the_style_element() {
        for css in [
            "p { color: red } </style><script>alert(1)</script>",
            "a<b { color: red }",
            "p { color: red; } p { background-color: </style> }",
            "@media </style> { p { color: red } }",
        ] {
            assert!(!filter(css).contains('<'), "{css:?} -> {}", filter(css));
        }
    }

    #[test]
    fn comments_html_comment_markers_and_garbage_are_tolerated() {
        let out = filter("<!-- /* c */ p { color: red } @@@ ;;; q { padding: 0 } -->");
        assert!(out.contains("p { color:red; }") && out.contains("q { padding:0; }"), "{out}");
    }

    #[test]
    fn url_values_are_kept_since_the_network_gate_handles_them() {
        let out = filter(".bg { background-image: url('https://img.example/a.png') !important }");
        assert!(out.contains("background-image:url('https://img.example/a.png') !important;"), "{out}");
    }
}
