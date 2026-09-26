//! The link table: where every `<a href>` of a laid-out document is.
//!
//! Like the [`TextRunTable`](crate::TextRunTable), it is recorded during the
//! draw pass, while the litehtml `Document` is alive, and shipped to the UI
//! thread with the frame. Clicks and the hover cursor are then a
//! point-in-rectangle lookup: no second parse + layout per click, and no
//! `Document` needed.
//!
//! Coordinates are document points (device-independent pixels), the same space
//! as the text runs and the display list.

use litehtml::{Document, Element};

use crate::geom::{Point, Rect};

/// Guards against pathological nesting; real mail is far shallower.
const MAX_DEPTH: usize = 512;

/// One `<a href>` and the areas of the page that activate it.
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    /// The `href` attribute, as written.
    pub href: String,
    /// Every box that belongs to the link, in document points: one per line
    /// for an anchor that wraps, plus the box of each block or image inside it
    /// (`<a><img></a>`, `<a><table>...</table></a>`).
    pub rects: Vec<Rect>,
}

/// Every link of one laid-out document, in document order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LinkTable {
    /// The links, first to last.
    pub links: Vec<Link>,
}

impl LinkTable {
    /// Walk `doc` (which must have been laid out) and record its links.
    pub fn collect(doc: &Document<'_>) -> Self {
        let mut links = Vec::new();
        let Some(root) = doc.root() else {
            return Self { links };
        };
        let mut stack: Vec<(Element<'_>, usize)> = vec![(root, 0)];
        while let Some((el, depth)) = stack.pop() {
            if el.is_text() {
                continue;
            }
            if el.tag_name() == "a"
                && let Some(href) = el.attr("href")
            {
                let rects = link_rects(&el);
                if !rects.is_empty() {
                    links.push(Link { href, rects });
                }
                // Anchors do not nest, and `link_rects` already covered the
                // inside.
                continue;
            }
            if depth >= MAX_DEPTH {
                continue;
            }
            for i in (0..el.children_count()).rev() {
                if let Some(child) = el.child_at(i) {
                    stack.push((child, depth + 1));
                }
            }
        }
        Self { links }
    }

    /// The `href` of the link at `p`. Where links overlap the later one in
    /// document order wins, as it is painted on top.
    pub fn href_at(&self, p: Point) -> Option<&str> {
        self.links
            .iter()
            .rev()
            .find(|l| l.rects.iter().any(|r| r.contains(p)))
            .map(|l| l.href.as_str())
    }
}

fn rect_of(p: litehtml::Position) -> Option<Rect> {
    (p.width > 0.0 && p.height > 0.0)
        .then(|| Rect::from_min_size(p.x, p.y, p.width, p.height))
}

fn push_children<'a>(el: &Element<'a>, depth: usize, stack: &mut Vec<(Element<'a>, usize)>) {
    for i in 0..el.children_count() {
        if let Some(child) = el.child_at(i) {
            stack.push((child, depth));
        }
    }
}

/// The areas an anchor covers: its per-line inline boxes, or its own box if it
/// is not inline (`display:block`), plus everything non-inline inside it.
fn link_rects<'a>(anchor: &Element<'a>) -> Vec<Rect> {
    let mut rects: Vec<Rect> = anchor.inline_boxes().into_iter().filter_map(rect_of).collect();
    if rects.is_empty() {
        rects.extend(rect_of(anchor.placement()));
    }
    let mut stack: Vec<(Element<'a>, usize)> = Vec::new();
    push_children(anchor, 1, &mut stack);
    while let Some((el, depth)) = stack.pop() {
        if el.is_text() {
            continue;
        }
        // Inline descendants (`<b>`, `<span>`) sit inside the anchor's own
        // line boxes; blocks, images and tables have boxes of their own.
        if el.inline_boxes_count() == 0 {
            rects.extend(rect_of(el.placement()));
        }
        if depth < MAX_DEPTH {
            push_children(&el, depth + 1, &mut stack);
        }
    }
    rects
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table_for(html: &str, width: f32) -> std::sync::Arc<LinkTable> {
        let mut engine = crate::engine::Engine::new(xui_win32::d2d::TextSystem::new().unwrap());
        engine.draw_pass(html, width).expect("parses");
        engine.links.clone()
    }

    #[test]
    fn a_block_link_covers_its_box() {
        let t = table_for(
            r#"<body style="margin:0"><a href="https://example.com/x" style="display:block;height:40px">go</a></body>"#,
            300.0,
        );
        assert_eq!(t.href_at(Point::new(5.0, 5.0)), Some("https://example.com/x"));
        assert_eq!(t.href_at(Point::new(5.0, 300.0)), None);
    }

    #[test]
    fn a_link_that_wraps_covers_every_line() {
        let t = table_for(
            r#"<body style="margin:0"><p style="margin:0">before <a href="https://example.com/w">one two three four five six seven eight nine ten eleven twelve</a> after</p></body>"#,
            120.0,
        );
        let link = &t.links[0];
        assert!(link.rects.len() >= 2, "expected one box per line: {:?}", link.rects);
        // The centre of each line's box hits the link.
        for r in &link.rects {
            assert_eq!(t.href_at(r.center()), Some("https://example.com/w"));
        }
        // Text outside the anchor on the first line does not.
        assert_eq!(t.href_at(Point::new(1.0, link.rects[0].center().y)), None);
    }

    #[test]
    fn an_image_link_covers_the_image() {
        let t = table_for(
            r#"<body style="margin:0"><a href="https://example.com/i"><img width="60" height="30"></a></body>"#,
            300.0,
        );
        assert_eq!(t.href_at(Point::new(30.0, 15.0)), Some("https://example.com/i"));
        assert_eq!(t.href_at(Point::new(200.0, 15.0)), None);
    }

    #[test]
    fn a_link_around_blocks_and_tables_covers_them() {
        let t = table_for(
            r#"<body style="margin:0"><a href="https://example.com/t"><table><tr><td style="width:100px;height:50px">cell</td></tr></table></a><p>after</p></body>"#,
            300.0,
        );
        assert_eq!(t.href_at(Point::new(50.0, 9.0)), Some("https://example.com/t"));
    }

    #[test]
    fn an_anchor_without_href_is_not_a_link() {
        let t = table_for(r#"<body><a name="top">anchor</a> <a>bare</a></body>"#, 300.0);
        assert!(t.links.is_empty());
    }
}
