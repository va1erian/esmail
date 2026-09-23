//! The recording half of the renderer: a [`DocumentContainer`] that measures
//! text with DirectWrite and turns every draw callback into a [`Cmd`] in
//! document coordinates. (The [`Engine`](crate::engine::Engine) that runs one
//! layout+record pass lives in `engine.rs`.)
//!
//! This is a straight port of `egui-litehtml-webview`'s `painter.rs`, with the
//! egui types replaced by DirectWrite (`win32ui::d2d`) and the neutral [`Cmd`]
//! from `list.rs`. The invariants are the same: the text engine that measures
//! must be the one that paints, and litehtml's values map identically.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use litehtml::email::EMAIL_MASTER_CSS;
use litehtml::{
    BackgroundLayer, BackgroundRepeat, Border, BorderRadiuses, BorderStyle, Borders, Color,
    ColorPoint, ConicGradient, DocumentContainer, DrawContext, FontDescription, FontHandle,
    FontMetrics, FontStyle, LinearGradient, ListMarker, ListStyleType, MediaFeatures, MediaType,
    Position, RadialGradient, Size, TextDecorationLine, TextTransform,
};
use win32ui::d2d::{Font, FontSpec, TextSystem};

use crate::geom::{Point, Radius, Rect, Rgba};
use crate::list::{
    BorderEdge, BorderKind, BorderPaint, Cmd, EdgePaint, FontDesc, FontKey, Image, ImageKey,
    Stroke, decompose_borders, normalize_stops,
};

/// Height reported to litehtml as the viewport's, so `vh` units are stable.
const VIEWPORT_HEIGHT: f32 = 800.0;

/// litehtml's own UA stylesheet, restated (see `egui-litehtml-webview`'s
/// `painter.rs` for why it is restated rather than left to `from_html`).
const LITEHTML_MASTER_CSS: &str = include_str!("litehtml_master.css");

/// The user-agent stylesheet: litehtml's defaults, then the email rules, then
/// the `table{text-align:left}` reset. In the master slot so the document's own
/// CSS wins (see `egui-litehtml-webview`).
pub(crate) fn ua_sheet() -> &'static str {
    static SHEET: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SHEET.get_or_init(|| format!("{LITEHTML_MASTER_CSS}\n{EMAIL_MASTER_CSS}\ntable {{ text-align: left; }}\n"))
}

/// Most tiles of a repeated background image drawn along one axis / in total.
const MAX_TILES_PER_AXIS: usize = 2048;
const MAX_TILES: usize = 20_000;

fn c32(c: Color) -> Rgba {
    Rgba::with_alpha(c.r, c.g, c.b, c.a)
}

fn rect_of(p: &Position) -> Rect {
    Rect::from_min_size(p.x, p.y, p.width, p.height)
}

/// litehtml's per-corner elliptical radii, in top-left, top-right,
/// bottom-right, bottom-left order (the order the D2D painter expects).
fn corner_radii(r: &BorderRadiuses) -> [Radius; 4] {
    [
        Radius::new(r.top_left_x, r.top_left_y),
        Radius::new(r.top_right_x, r.top_right_y),
        Radius::new(r.bottom_right_x, r.bottom_right_y),
        Radius::new(r.bottom_left_x, r.bottom_left_y),
    ]
}

fn border_kind(style: BorderStyle) -> BorderKind {
    match style {
        BorderStyle::None | BorderStyle::Hidden => BorderKind::None,
        BorderStyle::Solid => BorderKind::Solid,
        BorderStyle::Double => BorderKind::Double,
        BorderStyle::Dashed => BorderKind::Dashed,
        BorderStyle::Dotted => BorderKind::Dotted,
        BorderStyle::Groove => BorderKind::Groove,
        BorderStyle::Ridge => BorderKind::Ridge,
        BorderStyle::Inset => BorderKind::Inset,
        BorderStyle::Outset => BorderKind::Outset,
    }
}

fn border_edge(border: &Border) -> BorderEdge {
    BorderEdge { width: border.width, color: c32(border.color), kind: border_kind(border.style) }
}

// ─── The container ──────────────────────────────────────────────────────────

/// One resolved font, kept for measuring and for the slot's metrics.
struct Slot {
    key: FontKey,
    font: Font,
    ascent: f32,
    /// Height of a line of this font (ascent + descent + line gap).
    height: f32,
    font_size: f32,
    decoration: TextDecorationLine,
    decoration_color: Color,
}

/// The worker-side `DocumentContainer`.
pub(crate) struct D2dContainer {
    text: TextSystem,
    slots: HashMap<usize, Slot>,
    next_font: usize,
    fonts: Vec<FontDesc>,
    viewport: Position,
    cmds: Vec<Cmd>,
    images: HashMap<String, ImageKey>,
    image_data: Vec<Arc<Image>>,
    pending_images: Vec<(String, bool)>,
    requested_images: HashSet<String>,
    /// Diagnostic counters for the render-job log line.
    measure_calls: Cell<u64>,
    measure_hits: Cell<u64>,
}

impl D2dContainer {
    pub(crate) fn new(text: TextSystem) -> Self {
        Self {
            text,
            slots: HashMap::new(),
            next_font: 1,
            fonts: Vec::new(),
            viewport: Position { x: 0.0, y: 0.0, width: 1.0, height: VIEWPORT_HEIGHT },
            cmds: Vec::new(),
            images: HashMap::new(),
            image_data: Vec::new(),
            pending_images: Vec::new(),
            requested_images: HashSet::new(),
            measure_calls: Cell::new(0),
            measure_hits: Cell::new(0),
        }
    }

    pub(crate) fn begin(&mut self, width: f32) {
        self.viewport = Position { x: 0.0, y: 0.0, width, height: VIEWPORT_HEIGHT };
        self.cmds.clear();
        self.measure_calls.set(0);
        self.measure_hits.set(0);
    }

    pub(crate) fn load_image_data(&mut self, url: &str, bytes: &[u8]) {
        if self.images.contains_key(url) {
            return;
        }
        let Ok(img) = image::load_from_memory(bytes) else { return };
        let rgba = img.to_rgba8();
        let key = self.image_data.len() as ImageKey;
        self.image_data.push(Arc::new(Image {
            width: rgba.width(),
            height: rgba.height(),
            rgba: rgba.into_raw(),
        }));
        self.images.insert(url.to_string(), key);
    }

    /// Records a text run whose box is `pos`. Shared by `draw_text` and
    /// numbered list markers.
    fn push_text(&mut self, text: &str, font: FontHandle, color: Color, pos: Position, decorate: bool) {
        let Some(slot) = self.slots.get(&font.0) else { return };
        let (key, ascent, height, font_size) = (slot.key, slot.ascent, slot.height, slot.font_size);
        let (decoration, decoration_color) = (slot.decoration, slot.decoration_color);
        let color32 = c32(color);
        let lines = if decorate { decoration } else { TextDecorationLine::NONE };
        if text.trim().is_empty() && lines == TextDecorationLine::NONE {
            return;
        }
        let origin = Point::new(pos.x, pos.y + pos.height - height);
        if !text.trim().is_empty() {
            self.cmds.push(Cmd::Text {
                origin,
                width: pos.width,
                height,
                text: Arc::from(text),
                font: key,
                color: color32,
            });
        }
        if lines != TextDecorationLine::NONE {
            let col = if decoration_color.a == 0 { color32 } else { c32(decoration_color) };
            let baseline = origin.y + ascent;
            let thick = (font_size / 14.0).max(1.0).min(3.0);
            let mut line = |top: f32| {
                self.cmds.push(Cmd::Rect {
                    rect: Rect::from_min_size(pos.x, top, pos.width, thick),
                    radii: [Radius::default(); 4],
                    fill: col,
                });
            };
            if lines.contains(TextDecorationLine::UNDERLINE) {
                line(baseline + font_size * 0.1);
            }
            if lines.contains(TextDecorationLine::LINE_THROUGH) {
                line(baseline - font_size * 0.3 - thick / 2.0);
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
        self.cmds.push(Cmd::Rect {
            rect: rect_of(&b),
            radii: corner_radii(&layer.border_radius()),
            fill: c32(color),
        });
    }

    fn gradient_stops(points: &[ColorPoint]) -> Vec<crate::list::GradientStop> {
        normalize_stops(&points.iter().map(|p| (p.offset, c32(p.color))).collect::<Vec<_>>())
    }

    /// Forget which image URLs were already requested.
    pub(crate) fn clear_pending_images(&mut self) {
        self.pending_images.clear();
        self.requested_images.clear();
    }

    /// Image URLs layout discovered that are not loaded yet.
    pub(crate) fn take_pending_images(&mut self) -> Vec<(String, bool)> {
        std::mem::take(&mut self.pending_images)
    }

    /// Diagnostic counters, for the render-job log line.
    pub(crate) fn stats(&self) -> (usize, usize, usize, u64, u64) {
        (
            self.cmds.len(),
            self.fonts.len(),
            self.image_data.len(),
            self.measure_calls.get(),
            self.measure_hits.get(),
        )
    }

    /// The recorded commands, fonts and images of the finished pass.
    pub(crate) fn take_frame(&mut self) -> (Vec<Cmd>, Vec<FontDesc>, Vec<Arc<Image>>) {
        let cmds = std::mem::take(&mut self.cmds);
        let fonts = std::mem::take(&mut self.fonts);
        (cmds, fonts, self.image_data.clone())
    }
}

impl DocumentContainer for D2dContainer {
    fn create_font(&mut self, d: &FontDescription) -> (FontHandle, FontMetrics) {
        let size = d.size().max(1.0);
        let spec = FontSpec::new(d.family(), size)
            .weight(d.weight().clamp(1, 1000) as u16)
            .italic(matches!(d.style(), FontStyle::Italic));
        let font = self
            .text
            .font(&spec)
            .unwrap_or_else(|_| self.text.font(&FontSpec::new("Segoe UI", size)).expect("Segoe UI resolves"));
        let m = font.metrics();
        let height = m.line_height();
        let ascent = m.ascent;
        let ch_width = font.width("0");

        let key = self.fonts.len() as FontKey;
        self.fonts.push(FontDesc {
            family: spec.family.clone(),
            size,
            weight: spec.weight,
            italic: spec.italic,
        });
        let handle = self.next_font;
        self.next_font += 1;
        self.slots.insert(
            handle,
            Slot { key, font, ascent, height, font_size: size, decoration: d.decoration_line(), decoration_color: d.decoration_color() },
        );

        let metrics = FontMetrics {
            font_size: size,
            height,
            ascent,
            descent: height - ascent,
            x_height: m.x_height,
            ch_width,
            draw_spaces: true,
            sub_shift: size * 0.3,
            super_shift: size * 0.4,
        };
        (FontHandle(handle), metrics)
    }

    fn delete_font(&mut self, _font: FontHandle) {
        // Slots are keyed into the display list and cheap; keep them.
    }

    fn text_width(&self, text: &str, font: FontHandle) -> f32 {
        let Some(slot) = self.slots.get(&font.0) else { return text.len() as f32 * 8.0 };
        let before = slot.font.cached_widths();
        let width = slot.font.width(text);
        let hit = (slot.font.cached_widths() == before) as u64;
        self.measure_calls.set(self.measure_calls.get() + 1);
        self.measure_hits.set(self.measure_hits.get() + hit);
        width
    }

    fn draw_text(&mut self, _hdc: DrawContext, text: &str, font: FontHandle, color: Color, pos: Position) {
        self.push_text(text, font, color, pos, true);
    }

    fn draw_list_marker(&mut self, _hdc: DrawContext, marker: &ListMarker) {
        let pos = marker.pos();
        let color = c32(marker.color());
        let center = Point::new(pos.x + pos.width / 2.0, pos.y + pos.height / 2.0);
        let radius = pos.width.min(pos.height) / 2.0;
        match marker.marker_type() {
            ListStyleType::None => {}
            ListStyleType::Disc => {
                if radius > 0.0 {
                    self.cmds.push(Cmd::Circle {
                        center,
                        radius,
                        fill: color,
                        stroke: Stroke::solid(0.0, Rgba::TRANSPARENT),
                    });
                }
            }
            ListStyleType::Circle => {
                if radius > 0.0 {
                    self.cmds.push(Cmd::Circle {
                        center,
                        radius,
                        fill: Rgba::TRANSPARENT,
                        stroke: Stroke::solid(1.0, color),
                    });
                }
            }
            ListStyleType::Square => {
                self.cmds.push(Cmd::Rect { rect: rect_of(&pos), radii: [Radius::default(); 4], fill: color });
            }
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
        let Some(&key) = self.images.get(src) else { return Size::default() };
        self.image_data.get(key as usize).map_or_else(Size::default, |i| {
            Size { width: i.width as f32, height: i.height as f32 }
        })
    }

    fn draw_image(&mut self, _hdc: DrawContext, layer: &BackgroundLayer, url: &str, _base_url: &str) {
        let Some(&key) = self.images.get(url) else { return };
        let Some(img) = self.image_data.get(key as usize) else { return };
        if img.width == 0 || img.height == 0 {
            return;
        }
        // `origin_box` is one tile *after* width/height attributes,
        // `background-size` and `background-position` are applied.
        let origin = layer.origin_box();
        let (tile_w, tile_h) = if origin.width > 0.0 && origin.height > 0.0 {
            (origin.width, origin.height)
        } else {
            (img.width as f32, img.height as f32)
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
            self.cmds.push(Cmd::PushClip { rect: rect_of(&clip), radii: [Radius::default(); 4] });
        }
        let mut count = 0;
        'rows: for &y in &ys {
            for &x in &xs {
                if count >= MAX_TILES {
                    break 'rows;
                }
                count += 1;
                let rect = Rect::from_min_size(x, y, tile_w, tile_h);
                self.cmds.push(Cmd::Image { image: key, rect });
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
        let stops = Self::gradient_stops(&points);
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
        // litehtml's gradient line is in document coordinates already.
        let (s, e) = (gradient.start(), gradient.end());
        let start = Point::new(s.x, s.y);
        let d = Point::new(e.x - s.x, e.y - s.y);
        if d.x * d.x + d.y * d.y < 1e-6 {
            if let Some(p) = points.last() {
                self.fill(layer, p.color);
            }
            return;
        }
        self.cmds.push(Cmd::LinearGradient {
            rect: rect_of(&b),
            gradient: crate::list::LinearGradient { start, end: Point::new(e.x, e.y), stops },
        });
    }

    fn draw_radial_gradient(&mut self, _hdc: DrawContext, layer: &BackgroundLayer, gradient: &RadialGradient) {
        let points = gradient.color_points();
        let stops = Self::gradient_stops(&points);
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
        let c = gradient.position();
        let r = gradient.radius();
        self.cmds.push(Cmd::RadialGradient {
            rect: rect_of(&b),
            gradient: crate::list::RadialGradient {
                center: Point::new(c.x, c.y),
                radius_x: r.x.max(0.001),
                radius_y: r.y.max(0.001),
                stops,
            },
        });
    }

    fn draw_conic_gradient(&mut self, _hdc: DrawContext, layer: &BackgroundLayer, gradient: &ConicGradient) {
        // Unsupported: fall back to the first colour.
        if let Some(p) = gradient.color_points().first() {
            self.fill(layer, p.color);
        }
    }

    fn draw_borders(&mut self, _hdc: DrawContext, borders: &Borders, pos: Position, _root: bool) {
        let rect = rect_of(&pos);
        let radii = corner_radii(&borders.radius);
        let top = border_edge(&borders.top);
        let right = border_edge(&borders.right);
        let bottom = border_edge(&borders.bottom);
        let left = border_edge(&borders.left);
        match decompose_borders(rect, radii, top, right, bottom, left) {
            BorderPaint::Outline { rect, radii, stroke } => {
                self.cmds.push(Cmd::Outline { rect, radii, stroke });
            }
            BorderPaint::Edges(edges) => {
                for edge in edges {
                    match edge {
                        EdgePaint::Solid { rect, color } => {
                            self.cmds.push(Cmd::Rect { rect, radii: [Radius::default(); 4], fill: color });
                        }
                        EdgePaint::Line { a, b, width, color, dash } => {
                            self.cmds.push(Cmd::Line { a, b, stroke: Stroke::solid(width, color), dash });
                        }
                    }
                }
            }
        }
    }

    fn set_caption(&mut self, _caption: &str) {}

    fn on_anchor_click(&mut self, _url: &str) {}

    fn set_clip(&mut self, pos: Position, radius: BorderRadiuses) {
        self.cmds.push(Cmd::PushClip { rect: rect_of(&pos), radii: corner_radii(&radius) });
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
            device_width: self.viewport.width,
            device_height: self.viewport.height,
            color: 8,
            color_index: 0,
            monochrome: 0,
            resolution: 96.0,
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

