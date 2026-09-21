//! The rendering engine: litehtml's layout is recorded as a **display list**
//! and painted with [`egui::Painter`] every frame.
//!
//! # Shape of it
//!
//! * [`PainterContainer`] is a [`DocumentContainer`] that (a) measures text
//!   with egui's own text stack (via [`FontBook`], so layout and painting
//!   agree) and (b) turns every draw callback into a [`Cmd`] in document
//!   coordinates. It runs on the worker thread.
//! * The finished [`DisplayList`] crosses to the UI thread as an `Arc`.
//!   [`paint`] replays it into the widget's `Ui`, skipping everything outside
//!   the clip rect, so a long newsletter costs only what is on screen.
//! * There is no canvas: no growth passes, no texture tiling, no flattening
//!   onto white (a white rect is painted first), and text stays crisp at any
//!   `pixels_per_point`.
//!
//! # What it does and does not draw
//!
//! CSS `font-family` lists are resolved (see [`crate::fonts`]),
//! `text-decoration` (underlines on links!) is drawn, uniform rounded borders
//! are drawn as rounded borders, and `vh` units resolve against a stable
//! viewport. Overflow clips ignore their border radius (egui clip rects are
//! rectangles), gradients on rounded boxes are painted square, conic gradients
//! fall back to their first colour, and only solid / dashed / dotted border
//! styles exist.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use egui::epaint::text::{FontDefinitions, FontFamily, FontId, LayoutJob, TextFormat};
use egui::epaint::{Mesh, Vertex, WHITE_UV};
use egui::{Color32, CornerRadius, Pos2, Rect, Shape, Stroke, StrokeKind, TextureFilter, TextureHandle, TextureOptions, Vec2, pos2, vec2};
use litehtml::email::EMAIL_MASTER_CSS;
use litehtml::{
    BackgroundLayer, BackgroundRepeat, Border, BorderRadiuses, BorderStyle, Borders, Color, ColorPoint,
    ConicGradient, Document, DocumentContainer, DrawContext, FontDescription, FontHandle, FontMetrics,
    FontStyle, LinearGradient, ListMarker, ListStyleType, MediaFeatures, MediaType, Position, RadialGradient,
    Size, TextDecorationLine, TextTransform,
};

use crate::fonts::{FAMILY_PREFIX, FONT_PREFIX, FontBook};
use crate::{LinkTable, Output, TextRunTable};

/// Height reported to litehtml as the viewport's: a fixed, plausible window
/// height, so `60vh` means the same thing on every render.
const VIEWPORT_HEIGHT: f32 = 800.0;

/// litehtml's own UA stylesheet, restated (see the file's header for why).
const LITEHTML_MASTER_CSS: &str = include_str!("litehtml_master.css");

/// The user-agent stylesheet: litehtml's defaults, then the email rules
/// (`body{margin:0}`, `p{margin:0}`, `td{padding:0}`, `table{border-collapse}`
/// ...), then litehtml's `table{text-align:left}` reset that `from_html` only
/// adds to *its* default.
///
/// It must go in the **master** slot. The other one, `user_styles`, is applied
/// *after* the document's own `<style>` blocks, so those rules override the
/// message's CSS: a message's `p{margin:1em 0}` or `td.pad{padding:20px}` lost
/// to the UA's `p{margin:0}` / `td{padding:0}`. Master rules are applied first,
/// which is what "user agent" means, so the message wins and presentational
/// attributes (`cellpadding`, `bgcolor`, ...) sit above them.
fn ua_sheet() -> &'static str {
    static SHEET: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SHEET.get_or_init(|| format!("{LITEHTML_MASTER_CSS}\n{EMAIL_MASTER_CSS}\ntable {{ text-align: left; }}\n"))
}

/// Most tiles of a repeated background image drawn along one axis / in total.
const MAX_TILES_PER_AXIS: usize = 2048;
const MAX_TILES: usize = 20_000;

// ─── Display list ───────────────────────────────────────────────────────────

/// Everything needed to draw one font of the document.
pub(crate) struct FontSlot {
    pub id: FontId,
    /// Skew the glyphs: italic was asked for and the face is upright.
    pub synth_italic: bool,
    pub ascent: f32,
    /// Height of a line of this font (ascent + descent + line gap).
    pub height: f32,
    /// `text-decoration-line` / `-color` of the element the font was made for
    /// (litehtml makes them part of the font). Alpha 0 = the text colour.
    pub decoration: TextDecorationLine,
    pub decoration_color: Color,
}

/// One paint operation, in document coordinates (points, origin at the
/// document's top-left).
pub(crate) enum Cmd {
    Rect { rect: Rect, radius: CornerRadius, fill: Color32 },
    Outline { rect: Rect, radius: CornerRadius, stroke: Stroke },
    Line { a: Pos2, b: Pos2, stroke: Stroke, dash: Option<(f32, f32)> },
    Circle { center: Pos2, radius: f32, fill: Color32, stroke: Stroke },
    Mesh { mesh: Arc<Mesh>, bounds: Rect },
    Text { origin: Pos2, width: f32, text: Arc<str>, slot: Arc<FontSlot>, color: Color32 },
    Image { texture: TextureHandle, rect: Rect },
    PushClip(Rect),
    PopClip,
}

impl Cmd {
    /// Where this paints, for culling. `None` for clip bookkeeping.
    fn bounds(&self) -> Option<Rect> {
        Some(match self {
            Cmd::Rect { rect, .. } | Cmd::Outline { rect, .. } | Cmd::Image { rect, .. } => rect.expand(1.0),
            Cmd::Line { a, b, stroke, .. } => Rect::from_two_pos(*a, *b).expand(stroke.width + 1.0),
            Cmd::Circle { center, radius, stroke, .. } => {
                Rect::from_center_size(*center, Vec2::splat(radius * 2.0 + stroke.width + 2.0))
            }
            Cmd::Mesh { bounds, .. } => *bounds,
            // Wide enough for an italic skew and glyph overhang.
            Cmd::Text { origin, width, slot, .. } => {
                Rect::from_min_size(*origin, vec2(*width, slot.height)).expand2(vec2(slot.height * 0.3 + 2.0, 2.0))
            }
            Cmd::PushClip(_) | Cmd::PopClip => return None,
        })
    }
}

/// A laid-out document, ready to paint any number of times.
pub(crate) struct DisplayList {
    pub cmds: Vec<Cmd>,
    /// Every egui font family a [`Cmd::Text`] refers to. They must exist in
    /// the context's fonts before the list is painted (egui panics on an
    /// unknown family) -- see [`fonts_ready`].
    pub families: Vec<FontFamily>,
    /// Content size, points.
    pub size: Vec2,
}

/// What the worker sends the UI for one pass of a page.
pub(crate) struct ListFrame {
    pub id: u64,
    pub list: Arc<DisplayList>,
    /// The fonts the list's text was measured with.
    pub defs: Arc<FontDefinitions>,
    pub layout_width: f32,
    pub scale: f32,
    /// Where the text is, for selection (see [`TextRunTable`]).
    pub runs: Arc<TextRunTable>,
    /// Where the links are, for clicks and the hover cursor.
    pub links: Arc<LinkTable>,
}

// ─── Painting (UI thread) ───────────────────────────────────────────────────

/// The font names of a family that egui can use: everything this crate
/// registered (`has_data` is only consulted for egui's own bundled fonts, which
/// a host may have replaced).
fn usable_names(names: &[String], has_data: impl Fn(&str) -> bool) -> Vec<String> {
    names.iter().filter(|n| n.starts_with(FONT_PREFIX) || has_data(n)).cloned().collect()
}

/// Does `ctx` have the fonts `defs` describes for `families`?
///
/// Comparing the family's *list* and not just its name matters: a family keeps
/// its name when the worker later adds a fallback face to it, and a context
/// still holding the shorter list would paint the new glyphs as replacement
/// boxes while litehtml laid them out with the fallback's widths.
pub(crate) fn fonts_ready(ctx: &egui::Context, defs: &FontDefinitions, families: &[FontFamily]) -> bool {
    ctx.fonts(|f| {
        let active = f.definitions();
        families.iter().all(|fam| {
            if !matches!(fam, FontFamily::Name(_)) {
                return true;
            }
            match (active.families.get(fam), defs.families.get(fam)) {
                (Some(have), Some(want)) => *have == usable_names(want, |n| active.font_data.contains_key(n)),
                // Nothing to compare against: all we can ask is that egui knows it.
                (Some(_), None) => true,
                (None, _) => false,
            }
        })
    })
}

/// Add the worker's fonts to `ctx`, keeping whatever the host installed.
/// Takes effect at the start of the next pass.
pub(crate) fn install_fonts(ctx: &egui::Context, defs: &FontDefinitions) {
    let mut merged = ctx.fonts(|f| f.definitions().clone());
    for (name, data) in &defs.font_data {
        if name.starts_with(FONT_PREFIX) {
            merged.font_data.insert(name.clone(), data.clone());
        }
    }
    for (family, names) in &defs.families {
        if matches!(family, FontFamily::Name(n) if n.starts_with(FAMILY_PREFIX)) {
            // The tail names egui's bundled fonts; a host that replaced those
            // must not make the whole family unresolvable.
            let usable = usable_names(names, |n| merged.font_data.contains_key(n));
            merged.families.insert(family.clone(), usable);
        }
    }
    ctx.set_fonts(merged);
}

/// Replay `list` with the document's top-left at `origin`. Only what
/// intersects the painter's clip rect is drawn.
pub(crate) fn paint(list: &DisplayList, painter: &egui::Painter, origin: Pos2) {
    let visible = painter.clip_rect();
    let shift = origin.to_vec2();
    let mut clips: Vec<Rect> = Vec::new();
    let mut current = painter.clone();
    let narrowed = |clips: &[Rect]| painter.with_clip_rect(clips.iter().fold(visible, |a, c| a.intersect(*c)));

    for cmd in &list.cmds {
        match cmd {
            Cmd::PushClip(r) => {
                clips.push(r.translate(shift));
                current = narrowed(&clips);
                continue;
            }
            Cmd::PopClip => {
                clips.pop();
                current = narrowed(&clips);
                continue;
            }
            _ => {}
        }
        if cmd.bounds().is_some_and(|b| !visible.intersects(b.translate(shift))) {
            continue;
        }
        match cmd {
            Cmd::Rect { rect, radius, fill } => {
                current.rect_filled(rect.translate(shift), *radius, *fill);
            }
            Cmd::Outline { rect, radius, stroke } => {
                current.rect_stroke(rect.translate(shift), *radius, *stroke, StrokeKind::Inside);
            }
            Cmd::Line { a, b, stroke, dash } => match dash {
                Some((dash, gap)) => current.extend(Shape::dashed_line(&[*a + shift, *b + shift], *stroke, *dash, *gap)),
                None => {
                    current.line_segment([*a + shift, *b + shift], *stroke);
                }
            },
            Cmd::Circle { center, radius, fill, stroke } => {
                current.circle(*center + shift, *radius, *fill, *stroke);
            }
            Cmd::Mesh { mesh, .. } => {
                let mut m = Mesh::clone(mesh);
                for v in &mut m.vertices {
                    v.pos += shift;
                }
                current.add(Shape::mesh(m));
            }
            Cmd::Text { origin, text, slot, color, .. } => {
                let mut format = TextFormat::simple(slot.id.clone(), *color);
                format.italics = slot.synth_italic;
                let galley = current.layout_job(LayoutJob::simple_format(text.to_string(), format));
                current.galley(*origin + shift, galley, *color);
            }
            Cmd::Image { texture, rect } => {
                let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
                current.image(texture.id(), rect.translate(shift), uv, Color32::WHITE);
            }
            Cmd::PushClip(_) | Cmd::PopClip => unreachable!("handled above"),
        }
    }
}

// ─── Geometry / colour helpers ──────────────────────────────────────────────

fn c32(c: Color) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r, c.g, c.b, c.a)
}

fn rect_of(p: &Position) -> Rect {
    Rect::from_min_size(pos2(p.x, p.y), vec2(p.width, p.height))
}

fn is_rounded(r: &BorderRadiuses) -> bool {
    [
        r.top_left_x, r.top_left_y, r.top_right_x, r.top_right_y,
        r.bottom_right_x, r.bottom_right_y, r.bottom_left_x, r.bottom_left_y,
    ]
    .iter()
    .any(|v| *v > 0.0)
}

/// egui corner radii are one whole number per corner: elliptical radii
/// collapse to the larger axis.
fn corner_radius(r: &BorderRadiuses) -> CornerRadius {
    let u = |a: f32, b: f32| a.max(b).round().clamp(0.0, 255.0) as u8;
    CornerRadius {
        nw: u(r.top_left_x, r.top_left_y),
        ne: u(r.top_right_x, r.top_right_y),
        sw: u(r.bottom_left_x, r.bottom_left_y),
        se: u(r.bottom_right_x, r.bottom_right_y),
    }
}

fn same_color(a: Color, b: Color) -> bool {
    (a.r, a.g, a.b, a.a) == (b.r, b.g, b.b, b.a)
}

fn drawn(border: &Border) -> bool {
    border.width > 0.0 && !matches!(border.style, BorderStyle::None | BorderStyle::Hidden)
}

fn solid_like(style: BorderStyle) -> bool {
    matches!(
        style,
        BorderStyle::Solid | BorderStyle::Double | BorderStyle::Groove | BorderStyle::Ridge | BorderStyle::Inset | BorderStyle::Outset
    )
}

// ─── Gradients ──────────────────────────────────────────────────────────────

/// `(offset, premultiplied RGBA 0..255)`.
type Stops = Vec<(f32, [f32; 4])>;

fn stops_of(points: &[ColorPoint]) -> Stops {
    let mut stops: Stops = points
        .iter()
        .map(|p| {
            let a = p.color.a as f32 / 255.0;
            (p.offset, [p.color.r as f32 * a, p.color.g as f32 * a, p.color.b as f32 * a, p.color.a as f32])
        })
        .collect();
    stops.sort_by(|a, b| a.0.total_cmp(&b.0));
    stops
}

/// Colour at `t`, interpolated in premultiplied sRGB like CSS does; `Pad`
/// beyond the ends.
fn sample(stops: &Stops, t: f32) -> Color32 {
    let Some(first) = stops.first() else { return Color32::TRANSPARENT };
    let mut c = first.1;
    if t >= first.0 {
        c = stops.last().map_or(c, |l| l.1);
        for w in stops.windows(2) {
            let ((o0, c0), (o1, c1)) = (w[0], w[1]);
            if t >= o0 && t <= o1 {
                let f = if o1 > o0 { (t - o0) / (o1 - o0) } else { 1.0 };
                c = std::array::from_fn(|i| c0[i] + (c1[i] - c0[i]) * f);
                break;
            }
        }
    }
    let b = |v: f32| v.round().clamp(0.0, 255.0) as u8;
    Color32::from_rgba_premultiplied(b(c[0]), b(c[1]), b(c[2]), b(c[3]))
}

/// Vertices on a `xs` x `ys` grid, coloured by `color_at`; Gouraud shading does
/// the rest.
fn grid_mesh(xs: &[f32], ys: &[f32], color_at: impl Fn(Pos2) -> Color32) -> Mesh {
    let mut mesh = Mesh::default();
    if xs.len() < 2 || ys.len() < 2 {
        return mesh;
    }
    for &y in ys {
        for &x in xs {
            let pos = pos2(x, y);
            mesh.vertices.push(Vertex { pos, uv: WHITE_UV, color: color_at(pos) });
        }
    }
    let w = xs.len() as u32;
    for j in 0..ys.len() as u32 - 1 {
        for i in 0..w - 1 {
            let a = j * w + i;
            let (b, c, d) = (a + 1, a + w, a + w + 1);
            mesh.add_triangle(a, b, c);
            mesh.add_triangle(b, d, c);
        }
    }
    mesh
}

fn uniform(lo: f32, hi: f32, n: usize) -> Vec<f32> {
    (0..=n).map(|i| lo + (hi - lo) * i as f32 / n as f32).collect()
}

/// Grid lines along one axis for a gradient that only varies along it: the
/// rect's edges plus wherever a colour stop falls, so a multi-stop gradient is
/// exact.
fn stop_lines(lo: f32, hi: f32, origin: f32, delta: f32, stops: &Stops) -> Vec<f32> {
    let mut v = vec![lo, hi];
    v.extend(stops.iter().map(|(o, _)| origin + delta * o).filter(|x| *x > lo && *x < hi));
    v.sort_by(f32::total_cmp);
    v.dedup_by(|a, b| (*a - *b).abs() < 1e-3);
    v
}

// ─── Image tiling (ported from litehtml-rs's `PixbufContainer::draw_image`) ──

/// Origins along one axis of every tile of a background of `size` starting at
/// `origin` that could be visible in `[clip_start, clip_end)`.
fn tile_starts(origin: f32, size: f32, clip_start: f32, clip_end: f32, repeat: bool) -> Vec<f32> {
    if !repeat || size <= 0.0 {
        return vec![origin];
    }
    let k = ((origin - clip_start) / size).ceil();
    let mut x = origin - k * size;
    let mut out = Vec::new();
    while x < clip_end && out.len() < MAX_TILES_PER_AXIS {
        out.push(x);
        x += size;
    }
    out
}

struct ImageEntry {
    texture: TextureHandle,
    /// Natural size in CSS px. The texture may be smaller (downscaled to fit
    /// the GPU's limit); it is always drawn stretched to the laid-out box.
    width: f32,
    height: f32,
}

// ─── The container ──────────────────────────────────────────────────────────

pub(crate) struct PainterContainer {
    /// `RefCell`: `DocumentContainer::text_width` takes `&self`.
    /// `Rc`ed so [`PainterContainer::text_measure_fn`] can outlive the `&mut`
    /// borrow a `Document` takes of the container.
    book: Rc<RefCell<FontBook>>,
    fonts: Rc<RefCell<HashMap<usize, Arc<FontSlot>>>>,
    next_font_id: usize,
    /// egui `pixels_per_point` of the current pass.
    scale: f32,
    viewport: Position,
    cmds: Vec<Cmd>,
    families: HashSet<FontFamily>,
    images: HashMap<String, ImageEntry>,
    pending_images: Vec<(String, bool)>,
    requested_images: HashSet<String>,
    ctx: egui::Context,
}

impl PainterContainer {
    fn new(ctx: &egui::Context) -> Self {
        let max_side = ctx.input(|i| i.max_texture_side);
        Self {
            book: Rc::new(RefCell::new(FontBook::new(max_side))),
            fonts: Rc::new(RefCell::new(HashMap::new())),
            next_font_id: 1,
            scale: 1.0,
            viewport: Position { x: 0.0, y: 0.0, width: 1.0, height: VIEWPORT_HEIGHT },
            cmds: Vec::new(),
            families: HashSet::new(),
            images: HashMap::new(),
            pending_images: Vec::new(),
            requested_images: HashSet::new(),
            ctx: ctx.clone(),
        }
    }

    /// A width function that does not borrow the container, for
    /// [`TextRunTable::collect`] (it runs while the `Document` is alive, i.e.
    /// while the container is mutably borrowed). Widths are in points, measured
    /// with the same fonts and scale as layout. Take it *after* [`Self::begin`].
    fn text_measure_fn(&self) -> impl Fn(&str, FontHandle) -> f32 + use<> {
        let (book, fonts, scale) = (self.book.clone(), self.fonts.clone(), self.scale);
        move |text: &str, font: FontHandle| -> f32 {
            let Some(slot) = fonts.borrow().get(&font.0).cloned() else { return text.len() as f32 * 8.0 };
            book.borrow_mut().measure(text, &slot.id, scale)
        }
    }

    /// Reset for a fresh layout + draw pass.
    fn begin(&mut self, width: f32, scale: f32) {
        self.scale = scale;
        self.viewport = Position { x: 0.0, y: 0.0, width, height: VIEWPORT_HEIGHT };
        self.cmds.clear();
        self.families.clear();
        self.book.borrow_mut().begin_pass();
    }

    fn load_image_data(&mut self, url: &str, bytes: &[u8]) {
        let Ok(img) = image::load_from_memory(bytes) else { return };
        let (w, h) = (img.width(), img.height());
        let max = self.ctx.input(|i| i.max_texture_side) as u32;
        let img = if w > max || h > max {
            let f = max as f32 / w.max(h) as f32;
            img.resize(((w as f32 * f) as u32).max(1), ((h as f32 * f) as u32).max(1), image::imageops::FilterType::Triangle)
        } else {
            img
        };
        let rgba = img.to_rgba8();
        let pixels = egui::ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw());
        let options = TextureOptions { mipmap_mode: Some(TextureFilter::Linear), ..TextureOptions::LINEAR };
        let texture = self.ctx.load_texture("egui_litehtml_webview_image", pixels, options);
        self.images.insert(url.to_string(), ImageEntry { texture, width: w as f32, height: h as f32 });
    }

    /// Record a text run whose box is `pos`. Shared by `draw_text` and numbered
    /// list markers.
    fn push_text(&mut self, text: &str, font: FontHandle, color: Color, pos: Position, decorate: bool) {
        let Some(slot) = self.fonts.borrow().get(&font.0).cloned() else { return };
        let color32 = c32(color);
        let lines = if decorate { slot.decoration } else { TextDecorationLine::NONE };
        if text.trim().is_empty() && lines == TextDecorationLine::NONE {
            return; // nothing to see
        }
        self.book.borrow_mut().ensure_glyphs(text, &slot.id);
        self.families.insert(slot.id.family.clone());

        // `pos` is the run's box, `slot.height` tall like the font said; the
        // baseline sits `descent` above its bottom, which is `ascent` below the
        // top of a box of the font's own height.
        let origin = pos2(pos.x, pos.y + pos.height - slot.height);
        if !text.trim().is_empty() {
            self.cmds.push(Cmd::Text { origin, width: pos.width, text: Arc::from(text), slot: slot.clone(), color: color32 });
        }

        if lines != TextDecorationLine::NONE {
            let col = if slot.decoration_color.a == 0 { color32 } else { c32(slot.decoration_color) };
            let baseline = origin.y + slot.ascent;
            let size = slot.id.size;
            let thick = (size / 14.0).max(1.0 / self.scale).min(3.0);
            let mut line = |top: f32| {
                self.cmds.push(Cmd::Rect {
                    rect: Rect::from_min_size(pos2(pos.x, top), vec2(pos.width, thick)),
                    radius: CornerRadius::ZERO,
                    fill: col,
                });
            };
            if lines.contains(TextDecorationLine::UNDERLINE) {
                line(baseline + size * 0.1);
            }
            if lines.contains(TextDecorationLine::LINE_THROUGH) {
                line(baseline - size * 0.3 - thick / 2.0);
            }
            if lines.contains(TextDecorationLine::OVERLINE) {
                line(origin.y);
            }
        }
    }

    fn fill(&mut self, layer: &BackgroundLayer, color: Color) {
        if color.a == 0 {
            return;
        }
        let b = layer.border_box();
        if b.width <= 0.0 || b.height <= 0.0 {
            return;
        }
        self.cmds.push(Cmd::Rect { rect: rect_of(&b), radius: corner_radius(&layer.border_radius()), fill: c32(color) });
    }

    fn push_mesh(&mut self, mesh: Mesh) {
        if mesh.is_empty() {
            return;
        }
        let bounds = mesh.calc_bounds();
        self.cmds.push(Cmd::Mesh { mesh: Arc::new(mesh), bounds });
    }
}

impl DocumentContainer for PainterContainer {
    fn create_font(&mut self, d: &FontDescription) -> (FontHandle, FontMetrics) {
        let size = d.size().max(1.0);
        let mut book = self.book.borrow_mut();
        let resolved = book.resolve(d.family(), d.weight(), matches!(d.style(), FontStyle::Italic));
        let id = FontId::new(size, resolved.family.clone());
        let (ascent, height, ch_width) = book.metrics(&id, self.scale);
        let x_height = book.x_height(resolved.primary, size).unwrap_or(size * 0.5);
        drop(book);

        let handle = self.next_font_id;
        self.next_font_id += 1;
        self.fonts.borrow_mut().insert(
            handle,
            Arc::new(FontSlot {
                id,
                synth_italic: resolved.synth_italic,
                ascent,
                height,
                decoration: d.decoration_line(),
                decoration_color: d.decoration_color(),
            }),
        );

        let metrics = FontMetrics {
            font_size: size,
            height,
            ascent,
            descent: height - ascent,
            x_height,
            ch_width,
            draw_spaces: true,
            sub_shift: size * 0.3,
            super_shift: size * 0.4,
        };
        (FontHandle(handle), metrics)
    }

    fn delete_font(&mut self, _font: FontHandle) {
        // Slots are `Arc`ed into the display list and cheap; keep them.
    }

    fn text_width(&self, text: &str, font: FontHandle) -> f32 {
        let Some(slot) = self.fonts.borrow().get(&font.0).cloned() else { return text.len() as f32 * 8.0 };
        self.book.borrow_mut().measure(text, &slot.id, self.scale)
    }

    fn draw_text(&mut self, _hdc: DrawContext, text: &str, font: FontHandle, color: Color, pos: Position) {
        self.push_text(text, font, color, pos, true);
    }

    fn draw_list_marker(&mut self, _hdc: DrawContext, marker: &ListMarker) {
        let pos = marker.pos();
        let color = c32(marker.color());
        let center = pos2(pos.x + pos.width / 2.0, pos.y + pos.height / 2.0);
        let radius = pos.width.min(pos.height) / 2.0;
        match marker.marker_type() {
            ListStyleType::None => {}
            ListStyleType::Disc => {
                if radius > 0.0 {
                    self.cmds.push(Cmd::Circle { center, radius, fill: color, stroke: Stroke::NONE });
                }
            }
            ListStyleType::Circle => {
                if radius > 0.0 {
                    self.cmds.push(Cmd::Circle { center, radius, fill: Color32::TRANSPARENT, stroke: Stroke::new(1.0, color) });
                }
            }
            ListStyleType::Square => {
                self.cmds.push(Cmd::Rect { rect: rect_of(&pos), radius: CornerRadius::ZERO, fill: color });
            }
            // Numbered / lettered: "N." text.
            _ => self.push_text(&format!("{}.", marker.index()), marker.font(), marker.color(), pos, false),
        }
    }

    fn load_image(&mut self, src: &str, _baseurl: &str, redraw_on_ready: bool) {
        if src.is_empty() || self.images.contains_key(src) || self.requested_images.contains(src) {
            return;
        }
        self.requested_images.insert(src.to_string());
        self.pending_images.push((src.to_string(), redraw_on_ready));
    }

    fn get_image_size(&self, src: &str, _baseurl: &str) -> Size {
        self.images.get(src).map_or_else(Size::default, |i| Size { width: i.width, height: i.height })
    }

    fn draw_image(&mut self, _hdc: DrawContext, layer: &BackgroundLayer, url: &str, _base_url: &str) {
        let Some(img) = self.images.get(url) else { return };
        let texture = img.texture.clone();
        if img.width <= 0.0 || img.height <= 0.0 {
            return;
        }
        // `origin_box` is one tile *after* width/height attributes,
        // `background-size` and `background-position` are applied.
        let origin = layer.origin_box();
        let (tile_w, tile_h) = if origin.width > 0.0 && origin.height > 0.0 {
            (origin.width, origin.height)
        } else {
            (img.width, img.height)
        };
        let clip_box = layer.clip_box();
        let clip = if clip_box.width > 0.0 && clip_box.height > 0.0 {
            clip_box
        } else {
            Position { x: origin.x, y: origin.y, width: tile_w, height: tile_h }
        };

        let repeat = layer.repeat();
        let repeat_x = matches!(repeat, BackgroundRepeat::Repeat | BackgroundRepeat::RepeatX);
        let repeat_y = matches!(repeat, BackgroundRepeat::Repeat | BackgroundRepeat::RepeatY);
        let xs = tile_starts(origin.x, tile_w, clip.x, clip.x + clip.width, repeat_x);
        let ys = tile_starts(origin.y, tile_h, clip.y, clip.y + clip.height, repeat_y);

        // Clip only when some tile actually spills over the clip box.
        let eps = 0.5;
        let spills = xs.first().is_some_and(|&x| x < clip.x - eps)
            || xs.last().is_some_and(|&x| x + tile_w > clip.x + clip.width + eps)
            || ys.first().is_some_and(|&y| y < clip.y - eps)
            || ys.last().is_some_and(|&y| y + tile_h > clip.y + clip.height + eps);
        if spills {
            self.cmds.push(Cmd::PushClip(rect_of(&clip)));
        }
        // Snap tiles to whole device pixels so neighbours leave no seams.
        let s = self.scale;
        let snap = |v: f32| (v * s).round() / s;
        let (dw, dh) = (snap(tile_w).max(1.0 / s), snap(tile_h).max(1.0 / s));
        let mut count = 0;
        'rows: for &y in &ys {
            for &x in &xs {
                if count >= MAX_TILES {
                    break 'rows;
                }
                count += 1;
                let rect = Rect::from_min_size(pos2(snap(x), snap(y)), vec2(dw, dh));
                self.cmds.push(Cmd::Image { texture: texture.clone(), rect });
            }
        }
        if spills {
            self.cmds.push(Cmd::PopClip);
        }
    }

    fn draw_solid_fill(&mut self, _hdc: DrawContext, layer: &BackgroundLayer, color: Color) {
        self.fill(layer, color);
    }

    fn draw_linear_gradient(&mut self, _hdc: DrawContext, layer: &BackgroundLayer, gradient: &LinearGradient) {
        let points = gradient.color_points();
        let stops = stops_of(&points);
        if stops.len() < 2 {
            if let Some(p) = points.first() {
                self.fill(layer, p.color);
            }
            return;
        }
        let b = layer.border_box();
        if b.width <= 0.0 || b.height <= 0.0 {
            return;
        }
        let rect = rect_of(&b);
        // litehtml's gradient line is in document coordinates already (it adds
        // the layer's origin box itself). Adding the box origin again -- as
        // litehtml-rs's `PixbufContainer` does -- would push the line out of
        // any box that is not at the document's (0, 0), and the gradient would
        // clamp to a flat colour.
        let (s, e) = (gradient.start(), gradient.end());
        let start = pos2(s.x, s.y);
        let d = pos2(e.x, e.y) - start;
        let len2 = d.length_sq();
        if len2 < 1e-6 {
            if let Some(p) = points.last() {
                self.fill(layer, p.color);
            }
            return;
        }
        let t_at = |p: Pos2| (p - start).dot(d) / len2;
        // Along one axis the grid can be exact; a diagonal gradient is
        // approximated on a coarse grid.
        let (xs, ys) = if d.y.abs() < 1e-3 {
            (stop_lines(rect.min.x, rect.max.x, start.x, d.x, &stops), vec![rect.min.y, rect.max.y])
        } else if d.x.abs() < 1e-3 {
            (vec![rect.min.x, rect.max.x], stop_lines(rect.min.y, rect.max.y, start.y, d.y, &stops))
        } else {
            (uniform(rect.min.x, rect.max.x, 24), uniform(rect.min.y, rect.max.y, 24))
        };
        self.push_mesh(grid_mesh(&xs, &ys, |p| sample(&stops, t_at(p))));
    }

    fn draw_radial_gradient(&mut self, _hdc: DrawContext, layer: &BackgroundLayer, gradient: &RadialGradient) {
        let points = gradient.color_points();
        let stops = stops_of(&points);
        if stops.len() < 2 {
            if let Some(p) = points.first() {
                self.fill(layer, p.color);
            }
            return;
        }
        let b = layer.border_box();
        if b.width <= 0.0 || b.height <= 0.0 {
            return;
        }
        let rect = rect_of(&b);
        // Absolute, like the linear gradient's line (see above).
        let (c, r) = (gradient.position(), gradient.radius());
        let center = pos2(c.x, c.y);
        let (rx, ry) = (r.x.max(0.001), r.y.max(0.001));
        let t_at = |p: Pos2| (((p.x - center.x) / rx).powi(2) + ((p.y - center.y) / ry).powi(2)).sqrt();
        self.push_mesh(grid_mesh(&uniform(rect.min.x, rect.max.x, 32), &uniform(rect.min.y, rect.max.y, 32), |p| {
            sample(&stops, t_at(p))
        }));
    }

    fn draw_conic_gradient(&mut self, _hdc: DrawContext, layer: &BackgroundLayer, gradient: &ConicGradient) {
        // Unsupported: fall back to the first colour.
        if let Some(p) = gradient.color_points().first() {
            self.fill(layer, p.color);
        }
    }

    fn draw_borders(&mut self, _hdc: DrawContext, borders: &Borders, pos: Position, _root: bool) {
        let rect = rect_of(&pos);
        let sides = [&borders.top, &borders.right, &borders.bottom, &borders.left];

        // One colour and width all round, with rounded corners: a real
        // rounded outline.
        if is_rounded(&borders.radius)
            && sides.iter().all(|s| drawn(s) && solid_like(s.style) && s.width == borders.top.width && same_color(s.color, borders.top.color))
        {
            let w = borders.top.width;
            self.cmds.push(Cmd::Outline {
                rect,
                radius: corner_radius(&borders.radius),
                stroke: Stroke::new(w, c32(borders.top.color)),
            });
            return;
        }

        let edges = [
            (&borders.top, Rect::from_min_size(rect.min, vec2(rect.width(), borders.top.width)), true),
            (
                &borders.bottom,
                Rect::from_min_size(pos2(rect.min.x, rect.max.y - borders.bottom.width), vec2(rect.width(), borders.bottom.width)),
                true,
            ),
            (&borders.left, Rect::from_min_size(rect.min, vec2(borders.left.width, rect.height())), false),
            (
                &borders.right,
                Rect::from_min_size(pos2(rect.max.x - borders.right.width, rect.min.y), vec2(borders.right.width, rect.height())),
                false,
            ),
        ];
        for (border, edge, horizontal) in edges {
            if !drawn(border) {
                continue;
            }
            let color = c32(border.color);
            if solid_like(border.style) {
                self.cmds.push(Cmd::Rect { rect: edge, radius: CornerRadius::ZERO, fill: color });
                continue;
            }
            // Dashed / dotted: a stroked line down the middle of the edge.
            let (a, b) = if horizontal {
                (pos2(edge.min.x, edge.center().y), pos2(edge.max.x, edge.center().y))
            } else {
                (pos2(edge.center().x, edge.min.y), pos2(edge.center().x, edge.max.y))
            };
            let w = border.width;
            let dash = match border.style {
                BorderStyle::Dashed => (w * 3.0, w * 3.0),
                _ => (w, w),
            };
            self.cmds.push(Cmd::Line { a, b, stroke: Stroke::new(w, color), dash: Some(dash) });
        }
    }

    fn set_caption(&mut self, _caption: &str) {}

    // Clicks are resolved on the UI thread from the link table (see
    // `LinkTable`), so litehtml is never asked to dispatch one.
    fn on_anchor_click(&mut self, _url: &str) {}

    fn set_clip(&mut self, pos: Position, _radius: BorderRadiuses) {
        // Radius ignored: egui clips to rectangles.
        self.cmds.push(Cmd::PushClip(rect_of(&pos)));
    }

    fn del_clip(&mut self) {
        self.cmds.push(Cmd::PopClip);
    }

    fn get_viewport(&self) -> Position {
        self.viewport
    }

    fn get_media_features(&self) -> MediaFeatures {
        MediaFeatures {
            media_type: MediaType::Screen,
            width: self.viewport.width,
            height: self.viewport.height,
            device_width: self.viewport.width * self.scale,
            device_height: self.viewport.height * self.scale,
            color: 8,
            color_index: 0,
            monochrome: 0,
            resolution: 96.0 * self.scale,
        }
    }

    fn transform_text(&self, text: &str, tt: TextTransform) -> String {
        match tt {
            TextTransform::Uppercase => text.to_uppercase(),
            TextTransform::Lowercase => text.to_lowercase(),
            TextTransform::Capitalize => {
                let mut result = String::with_capacity(text.len());
                let mut capitalize_next = true;
                for ch in text.chars() {
                    if capitalize_next && ch.is_alphabetic() {
                        result.extend(ch.to_uppercase());
                        capitalize_next = false;
                    } else {
                        result.push(ch);
                        if ch.is_whitespace() {
                            capitalize_next = true;
                        }
                    }
                }
                result
            }
            TextTransform::None => text.to_string(),
        }
    }
}

// ─── Engine ─────────────────────────────────────────────────────────────────

pub(crate) struct PainterEngine {
    container: PainterContainer,
    /// The text of the page as of the last draw pass; sent with each frame.
    pub(crate) runs: Arc<TextRunTable>,
    /// The links of the page as of the last draw pass; sent with each frame.
    pub(crate) links: Arc<LinkTable>,
}

impl PainterEngine {
    pub(crate) fn new(ctx: &egui::Context) -> Self {
        Self { container: PainterContainer::new(ctx), runs: Arc::default(), links: Arc::default() }
    }

    /// Forget which image URLs were already requested.
    pub(crate) fn clear_pending_images(&mut self) {
        self.container.pending_images.clear();
        self.container.requested_images.clear();
    }

    /// Image URLs layout discovered that are not loaded yet.
    pub(crate) fn take_pending_images(&mut self) -> Vec<(String, bool)> {
        std::mem::take(&mut self.container.pending_images)
    }

    /// Decode `bytes` and remember them as the image at `url`.
    pub(crate) fn load_image_data(&mut self, url: &str, bytes: &[u8]) {
        self.container.load_image_data(url, bytes);
    }

    /// Lay the document out at `width` points and record it from scratch.
    /// Returns the content height in points, or `None` if the HTML could not
    /// be parsed.
    pub(crate) fn draw_pass(&mut self, html: &str, width: f32, scale: f32) -> Option<f32> {
        self.container.begin(width, scale);
        // Captured before the `Document` takes its mutable borrow.
        let measure = self.container.text_measure_fn();
        let mut doc = match Document::from_html(html, &mut self.container, Some(ua_sheet()), None) {
            Ok(doc) => doc,
            Err(e) => {
                log::warn!("egui-litehtml-webview: failed to parse message HTML: {e}");
                return None;
            }
        };
        let t = std::time::Instant::now();
        let _ = doc.render(width);
        let t_layout = t.elapsed();
        let height = doc.height().max(1.0);
        let t = std::time::Instant::now();
        doc.draw(DrawContext::default(), 0.0, 0.0, None);
        let t_record = t.elapsed();
        let t = std::time::Instant::now();
        self.runs = Arc::new(TextRunTable::collect(&doc, &measure));
        self.links = Arc::new(LinkTable::collect(&doc));
        log::debug!(
            "painter draw_pass: layout={t_layout:?} record={t_record:?} text_runs={:?} ({})",
            t.elapsed(),
            self.runs.runs.len(),
        );
        Some(height)
    }

    /// What the UI needs to show the last `draw_pass`.
    pub(crate) fn frame(&mut self, id: u64, width: f32, scale: f32, content_height: f32) -> Output {
        let cmds = std::mem::take(&mut self.container.cmds);
        let families = self.container.families.iter().cloned().collect();
        let defs = self.container.book.borrow_mut().definitions();
        Output::List(ListFrame {
            id,
            list: Arc::new(DisplayList { cmds, families, size: vec2(width, content_height) }),
            defs,
            layout_width: width,
            scale,
            runs: self.runs.clone(),
            links: self.links.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1x1 opaque red PNG.
    const RED_1X1_PNG: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

    struct Rendered {
        list: Arc<DisplayList>,
    }

    fn render(html: &str) -> Rendered {
        let ctx = egui::Context::default();
        let mut engine = PainterEngine::new(&ctx);
        let height = engine.draw_pass(html, 300.0, 1.0).expect("parses");
        let Output::List(frame) = engine.frame(1, 300.0, 1.0, height) else { panic!("no list frame") };
        Rendered { list: frame.list }
    }

    fn rects(list: &DisplayList) -> Vec<(Rect, Color32)> {
        list.cmds
            .iter()
            .filter_map(|c| match c {
                Cmd::Rect { rect, fill, .. } => Some((*rect, *fill)),
                _ => None,
            })
            .collect()
    }

    fn texts(list: &DisplayList) -> Vec<String> {
        list.cmds
            .iter()
            .filter_map(|c| match c {
                Cmd::Text { text, .. } => Some(text.to_string()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_background_becomes_a_rect_of_the_laid_out_size() {
        let r = render(r#"<body style="margin:0"><div style="width:100px;height:50px;background:#f00"></div></body>"#);
        let (rect, fill) = rects(&r.list).into_iter().next().expect("a rect");
        assert_eq!(fill, Color32::from_rgb(255, 0, 0));
        assert_eq!((rect.min, rect.size()), (pos2(0.0, 0.0), vec2(100.0, 50.0)));
        assert_eq!(r.list.size, vec2(300.0, 50.0));
    }

    #[test]
    fn text_is_recorded_with_the_requested_font_size_and_a_registered_family() {
        let r = render(r#"<body style="margin:0"><p style="margin:0;font-size:20px;font-family:Arial,sans-serif">Hello world</p></body>"#);
        assert!(texts(&r.list).join(" ").contains("Hello"), "{:?}", texts(&r.list));
        let Some(Cmd::Text { slot, origin, .. }) = r.list.cmds.iter().find(|c| matches!(c, Cmd::Text { .. })) else { panic!() };
        assert_eq!(slot.id.size, 20.0);
        assert!(origin.y >= -1.0 && origin.y < 10.0, "text top {}", origin.y);
        assert!(r.list.families.contains(&slot.id.family));
    }

    #[test]
    fn links_are_underlined_and_plain_text_is_not() {
        let link = render(r#"<body style="margin:0"><a href="https://example.com" style="text-decoration:underline">link</a></body>"#);
        let plain = render(r#"<body style="margin:0"><span>link</span></body>"#);
        let text_rect = |l: &DisplayList| match l.cmds.iter().find(|c| matches!(c, Cmd::Text { .. })) {
            Some(Cmd::Text { origin, width, slot, .. }) => Rect::from_min_size(*origin, vec2(*width, slot.height)),
            _ => panic!("no text"),
        };
        assert!(rects(&plain.list).is_empty(), "plain text draws no rect");
        let rs = rects(&link.list);
        assert_eq!(rs.len(), 1, "one underline: {rs:?}");
        let (line, _) = rs[0];
        let t = text_rect(&link.list);
        assert!(line.height() >= 1.0 && line.height() <= 3.0);
        assert!(line.min.y > t.min.y + t.height() * 0.5 && line.max.y <= t.max.y + 2.0, "underline {line:?} vs text {t:?}");
        assert!((line.width() - t.width()).abs() < 1.0, "underline spans the text");
    }

    #[test]
    fn strikethrough_and_overline_are_drawn_too() {
        let r = render(r#"<body style="margin:0"><s>gone</s> <span style="text-decoration:overline">over</span></body>"#);
        assert_eq!(rects(&r.list).len(), 2);
    }

    /// `(x, y, font size)` of every text run, in paint order.
    fn text_origins(list: &DisplayList) -> Vec<(f32, f32, f32)> {
        list.cmds
            .iter()
            .filter_map(|c| match c {
                Cmd::Text { origin, slot, .. } => Some((origin.x, origin.y, slot.id.size)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn ua_sheet_gives_email_defaults_and_keeps_litehtmls_own() {
        // Email rules: paragraphs are flush, the body has no margin.
        let flush = text_origins(&render("<p>one</p><p>two</p>").list);
        assert_eq!((flush[0].0, flush[1].1 - flush[0].1 < 25.0), (0.0, true), "{flush:?}");
        // litehtml's rules survive being restated: headings are bigger, <b> is bold.
        let sizes = text_origins(&render("<p>plain</p><h1>head</h1>").list);
        assert!(sizes[1].2 > sizes[0].2 * 1.5, "h1 should be larger than body text: {sizes:?}");
    }

    #[test]
    fn ua_sheet_yields_to_the_documents_own_css() {
        // The bug: the email sheet was applied after author CSS, so this
        // `margin` (and `padding` below) lost to `p{margin:0}` / `td{padding:0}`.
        let r = render("<style>p { margin: 30px 0 }</style><p>one</p><p>two</p>");
        let t = text_origins(&r.list);
        assert!(t[1].1 - t[0].1 > 40.0, "author p margin ignored: {t:?}");
        let r = render("<style>td { padding: 25px }</style><table><tr><td>cell</td></tr></table>");
        assert!(text_origins(&r.list)[0].0 >= 25.0, "author td padding ignored");
    }

    #[test]
    fn overflow_clips_are_balanced_and_rectangular() {
        let r = render(r#"<body style="margin:0"><div style="overflow:hidden;width:50px;height:50px"><div style="width:200px;height:200px;background:#0f0"></div></div></body>"#);
        let (mut depth, mut pushes) = (0i32, 0);
        for c in &r.list.cmds {
            match c {
                Cmd::PushClip(rect) => {
                    depth += 1;
                    pushes += 1;
                    assert_eq!(rect.size(), vec2(50.0, 50.0));
                }
                Cmd::PopClip => depth -= 1,
                _ => {}
            }
            assert!(depth >= 0);
        }
        assert!(pushes >= 1 && depth == 0, "pushes {pushes}, unbalanced by {depth}");
    }

    #[test]
    fn a_vertical_gradient_is_a_mesh_from_the_first_colour_to_the_last() {
        let r = render(r#"<body style="margin:0"><div style="width:100px;height:40px;background:linear-gradient(#ff0000,#0000ff)"></div></body>"#);
        let Some(Cmd::Mesh { mesh, bounds }) = r.list.cmds.iter().find(|c| matches!(c, Cmd::Mesh { .. })) else {
            panic!("no gradient mesh in {} cmds", r.list.cmds.len())
        };
        assert_eq!(bounds.size(), vec2(100.0, 40.0));
        let top = mesh.vertices.iter().filter(|v| v.pos.y == 0.0).map(|v| v.color).next().unwrap();
        let bottom = mesh.vertices.iter().filter(|v| v.pos.y == 40.0).map(|v| v.color).next().unwrap();
        assert_eq!(top, Color32::from_rgb(255, 0, 0));
        assert_eq!(bottom, Color32::from_rgb(0, 0, 255));
    }

    #[test]
    fn a_gradient_on_a_box_away_from_the_origin_still_spans_its_colours() {
        // Regression: the gradient line is absolute in litehtml. Treating it as
        // box-relative flattened every gradient not at (0, 0) to one colour.
        let r = render(r#"<body style="margin:0"><div style="height:50px"></div><div style="margin-left:30px;width:100px;height:40px;background:linear-gradient(#ff0000,#0000ff)"></div></body>"#);
        let Some(Cmd::Mesh { mesh, bounds }) = r.list.cmds.iter().find(|c| matches!(c, Cmd::Mesh { .. })) else { panic!("no mesh") };
        assert_eq!(bounds.min, pos2(30.0, 50.0));
        let at = |y: f32| mesh.vertices.iter().find(|v| v.pos.y == y).unwrap().color;
        assert_eq!((at(50.0), at(90.0)), (Color32::from_rgb(255, 0, 0), Color32::from_rgb(0, 0, 255)));
    }

    #[test]
    fn a_radial_gradient_off_the_origin_is_centred_on_its_box() {
        let r = render(r#"<body style="margin:0"><div style="height:50px"></div><div style="margin-left:30px;width:100px;height:100px;background:radial-gradient(circle,#ffffff,#000000)"></div></body>"#);
        let Some(Cmd::Mesh { mesh, .. }) = r.list.cmds.iter().find(|c| matches!(c, Cmd::Mesh { .. })) else { panic!("no mesh") };
        let at = |x: f32, y: f32| mesh.vertices.iter().min_by(|a, b| a.pos.distance(pos2(x, y)).total_cmp(&b.pos.distance(pos2(x, y)))).unwrap().color;
        let centre = at(80.0, 100.0);
        let corner = at(30.0, 50.0);
        assert!(centre.r() > 200, "white-ish in the middle, got {centre:?}");
        assert!(corner.r() < 60, "dark at the corner, got {corner:?}");
    }

    #[test]
    fn a_multi_stop_gradient_is_exact_at_the_middle_stop() {
        let r = render(r#"<body style="margin:0"><div style="width:100px;height:40px;background:linear-gradient(to right,#ff0000,#00ff00 25%,#0000ff)"></div></body>"#);
        let Some(Cmd::Mesh { mesh, .. }) = r.list.cmds.iter().find(|c| matches!(c, Cmd::Mesh { .. })) else { panic!("no mesh") };
        let at_25 = mesh.vertices.iter().find(|v| (v.pos.x - 25.0).abs() < 0.01).expect("a grid line at the stop").color;
        assert_eq!(at_25, Color32::from_rgb(0, 255, 0));
    }

    #[test]
    fn borders_are_drawn_per_edge_or_as_a_rounded_outline() {
        let square = render(r#"<body style="margin:0"><div style="width:40px;height:40px;border:2px solid #0000ff"></div></body>"#);
        assert_eq!(rects(&square.list).len(), 4, "four edges");
        let round = render(r#"<body style="margin:0"><div style="width:40px;height:40px;border:2px solid #0000ff;border-radius:8px"></div></body>"#);
        let Some(Cmd::Outline { radius, stroke, .. }) = round.list.cmds.iter().find(|c| matches!(c, Cmd::Outline { .. })) else {
            panic!("no outline")
        };
        assert_eq!((radius.nw, stroke.width), (8, 2.0));
    }

    #[test]
    fn list_bullets_are_circles_and_numbers_are_text() {
        let r = render(r#"<body style="margin:0"><ul><li>a</li></ul><ol><li>b</li></ol></body>"#);
        assert!(r.list.cmds.iter().any(|c| matches!(c, Cmd::Circle { .. })));
        assert!(texts(&r.list).iter().any(|t| t == "1."), "{:?}", texts(&r.list));
    }

    #[test]
    fn an_image_is_drawn_at_its_laid_out_size_once_loaded() {
        let html = format!(r#"<body style="margin:0"><img src="{RED_1X1_PNG}" width="100" height="100"></body>"#);
        let ctx = egui::Context::default();
        let mut engine = PainterEngine::new(&ctx);
        engine.draw_pass(&html, 300.0, 1.0).unwrap();
        let pending = engine.take_pending_images();
        assert_eq!(pending.len(), 1);
        engine.load_image_data(&pending[0].0, &litehtml::html::decode_data_uri(&pending[0].0).unwrap());
        let h = engine.draw_pass(&html, 300.0, 1.0).unwrap();
        let Output::List(frame) = engine.frame(1, 300.0, 1.0, h) else { panic!() };
        let Some(Cmd::Image { rect, .. }) = frame.list.cmds.iter().find(|c| matches!(c, Cmd::Image { .. })) else {
            panic!("no image cmd")
        };
        assert_eq!((rect.min, rect.size()), (pos2(0.0, 0.0), vec2(100.0, 100.0)));
    }

    #[test]
    fn a_frame_carries_the_links_of_the_page() {
        let html = r#"<body style="margin:0"><a href="https://example.com/x" style="display:block;height:40px">go</a></body>"#;
        let ctx = egui::Context::default();
        let mut engine = PainterEngine::new(&ctx);
        let h = engine.draw_pass(html, 300.0, 1.0).unwrap();
        let Output::List(frame) = engine.frame(1, 300.0, 1.0, h) else { panic!() };
        assert_eq!(frame.links.href_at(pos2(5.0, 5.0)), Some("https://example.com/x"));
        assert_eq!(frame.links.href_at(pos2(5.0, 300.0)), None);
    }

    #[test]
    fn a_font_family_list_measures_like_its_first_installed_family() {
        // A list like `Arial,sans-serif` must measure as Arial's width, not
        // as some platform fallback.
        let ctx = egui::Context::default();
        let mut c = PainterContainer::new(&ctx);
        let width_in = |c: &mut PainterContainer, family: &str| {
            let html = format!(r#"<body><span style="font-family:{family};font-size:14px">Temps</span></body>"#);
            let before: HashSet<usize> = c.fonts.borrow().keys().copied().collect();
            {
                let mut doc = Document::from_html(&html, &mut *c, None, None).unwrap();
                let _ = doc.render(500.0);
            }
            let handle = c.fonts.borrow().iter().find(|(h, s)| !before.contains(h) && s.id.size == 14.0).map(|(h, _)| *h).unwrap();
            c.text_width("Temps", FontHandle(handle))
        };
        let list = width_in(&mut c, "Arial,Helvetica,sans-serif");
        let single = width_in(&mut c, "Arial");
        assert!((list - single).abs() < 0.01, "list {list} vs single {single}");
    }

    #[test]
    fn replay_paints_only_what_intersects_the_clip() {
        // 100 stacked 10px bars; a window over the first 3 must paint ~3.
        let bars: String = (0..100).map(|_| r#"<div style="height:10px;background:#123456"></div>"#).collect();
        let r = render(&format!(r#"<body style="margin:0">{bars}</body>"#));
        let ctx = egui::Context::default();
        let count = |window: Rect| {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                let painter = ui.painter().with_clip_rect(window);
                paint(&r.list, &painter, Pos2::ZERO);
            });
            out.textures_delta.clear();
            out.shapes.len()
        };
        let all = count(Rect::from_min_size(Pos2::ZERO, vec2(300.0, 1000.0)));
        let few = count(Rect::from_min_size(Pos2::ZERO, vec2(300.0, 30.0)));
        assert!(all >= 100, "all bars painted: {all}");
        assert!(few <= 5, "culled to the window: {few}");
    }

    #[test]
    fn a_fallback_added_to_an_existing_family_is_reinstalled_in_the_context() {
        use crate::fonts::FontBook;
        let ctx = egui::Context::default();
        let pass = |ctx: &egui::Context| {
            ctx.run_ui(egui::RawInput::default(), |_| {}).textures_delta.clear();
        };
        pass(&ctx);
        let mut book = FontBook::new(2048);
        let family = book.resolve("Arial", 400, false).family;
        let families = [family.clone()];

        let first = book.definitions();
        assert!(!fonts_ready(&ctx, &first, &families), "nothing is installed yet");
        install_fonts(&ctx, &first);
        pass(&ctx);
        assert!(fonts_ready(&ctx, &first, &families));

        // A later document needs a glyph Arial lacks: same family name, longer list.
        book.ensure_glyphs("\u{2794}", &egui::epaint::text::FontId::new(14.0, family));
        let second = book.definitions();
        if Arc::ptr_eq(&first, &second) {
            return; // no system face has it: nothing to reinstall
        }
        assert!(
            !fonts_ready(&ctx, &second, &families),
            "the context still has the old list, so it must not count as ready"
        );
        install_fonts(&ctx, &second);
        pass(&ctx);
        assert!(fonts_ready(&ctx, &second, &families));
    }
}
