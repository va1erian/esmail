//! The backend-neutral display list: the commands a laid-out document becomes
//! and the types they refer to. Nothing here depends on egui or xui-win32, so
//! the list could be replayed by any painter.

use std::sync::Arc;

use crate::geom::{Point, Radius, Rect, Rgba};
use crate::links::LinkTable;
use crate::text_runs::TextRunTable;

/// Identifies one font of a document. Indexes into [`DisplayList::fonts`].
pub type FontKey = u32;
/// Identifies one decoded image of a document. Indexes into
/// [`DisplayList::images`].
pub type ImageKey = u32;

/// Everything the painter needs to rebuild one font: DirectWrite (and the
/// Direct2D painter) resolves this through its own text system, so the widths
/// it paints match the widths litehtml measured.
#[derive(Clone, Debug, PartialEq)]
pub struct FontDesc {
    /// The CSS `font-family` list, as written.
    pub family: String,
    /// The em size in device-independent pixels.
    pub size: f32,
    /// Weight from 100 to 900.
    pub weight: u16,
    /// Whether the face is italic.
    pub italic: bool,
}

/// A decoded RGBA image, ready to upload.
#[derive(Clone)]
pub struct Image {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// RGBA pixel data, four bytes per pixel.
    pub rgba: Vec<u8>,
}

/// One colour in a gradient, at `offset` in `0.0..=1.0`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GradientStop {
    /// The position along the gradient, in `0.0..=1.0`.
    pub offset: f32,
    /// The colour at that position.
    pub color: Rgba,
}

/// A linear gradient from `start` to `end`.
#[derive(Clone, Debug, PartialEq)]
pub struct LinearGradient {
    /// Where the gradient starts.
    pub start: Point,
    /// Where the gradient ends.
    pub end: Point,
    /// The colours at their positions; at least two.
    pub stops: Vec<GradientStop>,
}

/// A radial gradient centred at `center` with elliptical radii.
#[derive(Clone, Debug, PartialEq)]
pub struct RadialGradient {
    /// The centre of the gradient.
    pub center: Point,
    /// The horizontal radius.
    pub radius_x: f32,
    /// The vertical radius.
    pub radius_y: f32,
    /// The colours at their positions; at least two.
    pub stops: Vec<GradientStop>,
}

/// A solid pen: a width and an RGBA colour.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stroke {
    /// The line width.
    pub width: f32,
    /// The line colour.
    pub color: Rgba,
}

impl Stroke {
    /// A solid stroke `width` wide.
    pub const fn solid(width: f32, color: Rgba) -> Stroke {
        Stroke { width, color }
    }
}

/// How a line or border edge is broken up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dash {
    /// An unbroken line.
    Solid,
    /// Dashes (segment and gap are three times the width).
    Dashed,
    /// Dots (segment and gap are the width, round caps).
    Dotted,
}

/// One paint operation, in document coordinates (device-independent pixels,
/// origin at the document's top-left).
#[derive(Clone, Debug)]
pub enum Cmd {
    /// A filled rectangle (rounded when any corner radius is non-zero).
    Rect {
        /// The rectangle.
        rect: Rect,
        /// One radius per corner.
        radii: [Radius; 4],
        /// The fill colour.
        fill: Rgba,
    },
    /// A rounded outline of one colour and width all round.
    Outline {
        /// The rectangle.
        rect: Rect,
        /// One radius per corner.
        radii: [Radius; 4],
        /// The pen.
        stroke: Stroke,
    },
    /// A line.
    Line {
        /// The start point.
        a: Point,
        /// The end point.
        b: Point,
        /// The pen.
        stroke: Stroke,
        /// How the line is broken up.
        dash: Dash,
    },
    /// A filled/outlined circle (list markers).
    Circle {
        /// The centre.
        center: Point,
        /// The radius.
        radius: f32,
        /// The fill colour.
        fill: Rgba,
        /// The outline pen.
        stroke: Stroke,
    },
    /// A text run. `width` and `height` are the run's box, for culling.
    Text {
        /// The run's top-left corner.
        origin: Point,
        /// The run's laid-out width.
        width: f32,
        /// The run's line height.
        height: f32,
        /// The text.
        text: Arc<str>,
        /// The font, into [`DisplayList::fonts`].
        font: FontKey,
        /// The text colour.
        color: Rgba,
    },
    /// One tile of an image, drawn into `rect`.
    Image {
        /// The image, into [`DisplayList::images`].
        image: ImageKey,
        /// Where to draw it.
        rect: Rect,
    },
    /// A linear-gradient fill over `rect`.
    LinearGradient {
        /// The filled rectangle.
        rect: Rect,
        /// The gradient.
        gradient: LinearGradient,
    },
    /// A radial-gradient fill over `rect`.
    RadialGradient {
        /// The filled rectangle.
        rect: Rect,
        /// The gradient.
        gradient: RadialGradient,
    },
    /// Restricts drawing to `rect` (rounded when any radius is non-zero) until
    /// the matching [`Cmd::PopClip`].
    PushClip {
        /// The clip rectangle.
        rect: Rect,
        /// One radius per corner.
        radii: [Radius; 4],
    },
    /// Ends the innermost clip.
    PopClip,
}

impl Cmd {
    /// Where this paints, for culling; `None` for clip bookkeeping.
    pub fn bounds(&self) -> Option<Rect> {
        Some(match self {
            Cmd::Rect { rect, .. }
            | Cmd::Outline { rect, .. }
            | Cmd::Image { rect, .. }
            | Cmd::LinearGradient { rect, .. }
            | Cmd::RadialGradient { rect, .. } => rect.expand(1.0),
            Cmd::Line { a, b, stroke, .. } => {
                Rect::new(a.x.min(b.x), a.y.min(b.y), a.x.max(b.x), a.y.max(b.y)).expand(stroke.width + 1.0)
            }
            Cmd::Circle { center, radius, stroke, .. } => {
                let r = radius + stroke.width / 2.0 + 1.0;
                Rect::new(center.x - r, center.y - r, center.x + r, center.y + r)
            }
            Cmd::Text { origin, width, height, .. } => Rect::from_min_size(origin.x, origin.y, *width, *height).expand(2.0),
            Cmd::PushClip { .. } | Cmd::PopClip => return None,
        })
    }
}

/// A laid-out document, ready to paint any number of times.
#[derive(Clone)]
pub struct DisplayList {
    /// The paint operations, in order.
    pub cmds: Vec<Cmd>,
    /// The content size (width, height) in device-independent pixels.
    pub size: (f32, f32),
    /// The document's fonts, indexed by [`FontKey`].
    pub fonts: Vec<FontDesc>,
    /// The document's decoded images, indexed by [`ImageKey`].
    pub images: Vec<Arc<Image>>,
}

/// A finished render of one page, as the worker sends it to the UI thread.
pub struct Frame {
    /// The render job this frame answers.
    pub id: u64,
    /// The display list.
    pub list: Arc<DisplayList>,
    /// Where the page's text is, for selection (see [`TextRunTable`]).
    pub runs: Arc<TextRunTable>,
    /// Where the page's links are, for clicks and the hover cursor.
    pub links: Arc<LinkTable>,
}

// ─── Border decomposition (pure, testable) ──────────────────────────────────

/// How a border edge is styled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BorderKind {
    /// Not drawn.
    None,
    /// Drawn as a solid fill (and treated as such for a rounded outline).
    Solid,
    /// Drawn as a solid fill.
    Double,
    /// Dashed.
    Dashed,
    /// Dotted.
    Dotted,
    /// Drawn as a solid fill (bevelled looks are flattened).
    Groove,
    /// Drawn as a solid fill.
    Ridge,
    /// Drawn as a solid fill.
    Inset,
    /// Drawn as a solid fill.
    Outset,
}

impl BorderKind {
    /// Whether the edge is drawn at all.
    pub fn is_drawn(self) -> bool {
        !matches!(self, BorderKind::None)
    }

    /// Whether the edge is drawn as a solid fill (not a dashed/dotted line).
    pub fn is_solid(self) -> bool {
        matches!(
            self,
            BorderKind::Solid | BorderKind::Double | BorderKind::Groove | BorderKind::Ridge | BorderKind::Inset | BorderKind::Outset
        )
    }
}

/// One border edge, as recorded from litehtml.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BorderEdge {
    /// The edge width.
    pub width: f32,
    /// The edge colour.
    pub color: Rgba,
    /// The edge style.
    pub kind: BorderKind,
}

/// One painted edge after decomposition.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EdgePaint {
    /// A solid fill of the edge's rectangle.
    Solid {
        /// The edge's rectangle.
        rect: Rect,
        /// The fill colour.
        color: Rgba,
    },
    /// A dashed/dotted line down the middle of the edge.
    Line {
        /// The start point.
        a: Point,
        /// The end point.
        b: Point,
        /// The stroke width.
        width: f32,
        /// The line colour.
        color: Rgba,
        /// How the line is broken up.
        dash: Dash,
    },
}

/// What `decompose_borders` produces: either a single rounded outline, or the
/// four edges painted individually.
#[derive(Clone, Debug, PartialEq)]
pub enum BorderPaint {
    /// One colour and width all round, with rounded corners.
    Outline {
        /// The box.
        rect: Rect,
        /// One radius per corner.
        radii: [Radius; 4],
        /// The pen.
        stroke: Stroke,
    },
    /// Each drawn edge on its own.
    Edges(Vec<EdgePaint>),
}

/// Turns a box's four borders into paint operations. A box whose borders are
/// all drawn, all solid, one colour and one width, with rounded corners,
/// collapses to a single rounded outline; anything else is decomposed into
/// four edges so dashed/dotted edges are stroked.
pub fn decompose_borders(
    rect: Rect,
    radii: [Radius; 4],
    top: BorderEdge,
    right: BorderEdge,
    bottom: BorderEdge,
    left: BorderEdge,
) -> BorderPaint {
    let drawn = |edge: BorderEdge| edge.width > 0.0 && edge.kind.is_drawn();
    let rounded = radii.iter().any(|r| r.x > 0.0 || r.y > 0.0);
    let edges = [top, right, bottom, left];
    if rounded
        && edges.iter().all(|e| drawn(*e) && e.kind.is_solid())
        && edges.iter().all(|e| e.width == top.width && e.color == top.color)
    {
        return BorderPaint::Outline { rect, radii, stroke: Stroke::solid(top.width, top.color) };
    }

    let mut paints = Vec::new();
    // Each edge's rectangle inside the box, in top, bottom, left, right order.
    let edges = [
        (top, Rect::from_min_size(rect.left, rect.top, rect.width(), top.width), true),
        (bottom, Rect::from_min_size(rect.left, rect.bottom - bottom.width, rect.width(), bottom.width), true),
        (left, Rect::from_min_size(rect.left, rect.top, left.width, rect.height()), false),
        (right, Rect::from_min_size(rect.right - right.width, rect.top, right.width, rect.height()), false),
    ];
    for (edge, edge_rect, horizontal) in edges {
        if !drawn(edge) {
            continue;
        }
        if edge.kind.is_solid() {
            paints.push(EdgePaint::Solid { rect: edge_rect, color: edge.color });
            continue;
        }
        let (a, b) = if horizontal {
            (Point::new(edge_rect.left, edge_rect.top + edge_rect.height() / 2.0), Point::new(edge_rect.right, edge_rect.top + edge_rect.height() / 2.0))
        } else {
            (Point::new(edge_rect.left + edge_rect.width() / 2.0, edge_rect.top), Point::new(edge_rect.left + edge_rect.width() / 2.0, edge_rect.bottom))
        };
        let dash = match edge.kind {
            BorderKind::Dashed => Dash::Dashed,
            _ => Dash::Dotted,
        };
        paints.push(EdgePaint::Line { a, b, width: edge.width, color: edge.color, dash });
    }
    BorderPaint::Edges(paints)
}

// ─── Gradient stop mapping (pure, testable) ────────────────────────────────

/// Sorts gradient stops by offset, deduplicating coincident positions (keeping
/// the last colour, as CSS does). Stops carry straight RGBA; Direct2D
/// interpolates them itself.
pub fn normalize_stops(stops: &[(f32, Rgba)]) -> Vec<GradientStop> {
    let mut stops: Vec<GradientStop> =
        stops.iter().map(|(offset, color)| GradientStop { offset: *offset, color: *color }).collect();
    stops.sort_by(|a, b| a.offset.total_cmp(&b.offset));
    let mut out: Vec<GradientStop> = Vec::with_capacity(stops.len());
    for stop in stops {
        if let Some(last) = out.last_mut()
            && (last.offset - stop.offset).abs() < 1e-4
        {
            last.color = stop.color;
        } else {
            out.push(stop);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_uniform_rounded_border_is_a_single_outline() {
        let rect = Rect::new(0.0, 0.0, 40.0, 40.0);
        let radii = [Radius::uniform(8.0); 4];
        let solid = BorderEdge { width: 2.0, color: Rgba::rgb(0, 0, 255), kind: BorderKind::Solid };
        let paint = decompose_borders(rect, radii, solid, solid, solid, solid);
        assert!(matches!(paint, BorderPaint::Outline { stroke, .. } if stroke.width == 2.0 && stroke.color == Rgba::rgb(0, 0, 255)));
    }

    #[test]
    fn a_square_border_decomposes_into_four_edges() {
        let rect = Rect::new(0.0, 0.0, 40.0, 40.0);
        let solid = BorderEdge { width: 2.0, color: Rgba::rgb(0, 0, 255), kind: BorderKind::Solid };
        let BorderPaint::Edges(edges) = decompose_borders(rect, [Radius::default(); 4], solid, solid, solid, solid) else {
            panic!("expected edges");
        };
        assert_eq!(edges.len(), 4);
        assert!(edges.iter().all(|e| matches!(e, EdgePaint::Solid { .. })));
    }

    #[test]
    fn a_dashed_edge_is_a_line_and_undrawn_edges_are_dropped() {
        let rect = Rect::new(0.0, 0.0, 40.0, 40.0);
        let none = BorderEdge { width: 0.0, color: Rgba::BLACK, kind: BorderKind::None };
        let dashed = BorderEdge { width: 2.0, color: Rgba::rgb(255, 0, 0), kind: BorderKind::Dashed };
        let BorderPaint::Edges(edges) = decompose_borders(rect, [Radius::default(); 4], dashed, none, dashed, none) else {
            panic!("expected edges");
        };
        assert_eq!(edges.len(), 2);
        assert!(edges.iter().all(|e| matches!(e, EdgePaint::Line { dash: Dash::Dashed, .. })));
    }

    #[test]
    fn gradient_stops_are_sorted_and_coincident_ones_deduplicated() {
        let stops = normalize_stops(&[(1.0, Rgba::BLACK), (0.0, Rgba::WHITE), (0.5, Rgba::rgb(1, 2, 3)), (0.5, Rgba::rgb(9, 9, 9))]);
        assert_eq!(stops.len(), 3);
        assert_eq!(stops[0].offset, 0.0);
        assert_eq!(stops[1].color, Rgba::rgb(9, 9, 9), "the last colour at a stop wins");
        assert_eq!(stops[2].offset, 1.0);
    }

    #[test]
    fn culling_bounds_cover_every_visible_cmd_kind() {
        let text = Cmd::Text {
            origin: Point::new(10.0, 20.0),
            width: 50.0,
            height: 16.0,
            text: Arc::from("hello"),
            font: 0,
            color: Rgba::BLACK,
        };
        assert_eq!(text.bounds(), Some(Rect::new(8.0, 18.0, 62.0, 38.0)));
        let clip = Cmd::PushClip { rect: Rect::default(), radii: [Radius::default(); 4] };
        assert_eq!(clip.bounds(), None);
    }
}
