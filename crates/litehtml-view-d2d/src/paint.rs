//! The painting half of the renderer: replays a [`DisplayList`] into a
//! [`D2dCanvas`], culling to the visible region and applying the scroll offset.

use std::collections::HashMap;

use xui_win32::d2d::{
    Cap, D2dCanvas, DashStyle, Font, FontSpec, GradientStop, ImageId, Interpolation,
    LinearGradient as D2dLinearGradient, PointF, RadialGradient as D2dRadialGradient,
    Radius as D2dRadius, RectF, Rgba as D2dRgba, RoundedRect, Stroke as D2dStroke, TextSystem,
};
use xui_win32::Color;

use crate::geom::{Point, Radius, Rect, Rgba};
use crate::list::{Cmd, Dash, DisplayList, FontKey, ImageKey};
use crate::selection::{Selection, TextPos};
use crate::text_runs::{TextRun, TextRunTable};

fn to_rgba(color: Rgba) -> D2dRgba {
    D2dRgba::with_alpha(color.r, color.g, color.b, color.a)
}

fn to_point(p: Point) -> PointF {
    PointF::new(p.x, p.y)
}

fn to_rect(r: Rect) -> RectF {
    RectF::new(r.left, r.top, r.right, r.bottom)
}

fn to_radius(r: Radius) -> D2dRadius {
    D2dRadius::new(r.x, r.y)
}

fn to_rounded(r: Rect, radii: [Radius; 4]) -> RoundedRect {
    RoundedRect::new(to_rect(r), radii.map(to_radius))
}

fn to_stroke(s: crate::list::Stroke) -> D2dStroke {
    D2dStroke::solid(s.width)
}

fn is_rounded(radii: &[Radius; 4]) -> bool {
    radii.iter().any(|r| r.x > 0.0 || r.y > 0.0)
}

/// A painter that caches the device resources a replay needs (resolved fonts
/// and uploaded images), so painting a frame allocates nothing per command
/// beyond what Direct2D itself needs.
pub struct Painter {
    text: TextSystem,
    fonts: HashMap<FontKey, Font>,
    images: HashMap<ImageKey, ImageId>,
}

impl Painter {
    /// Creates a painter over `text`, shared with the worker that laid the
    /// document out (so painted widths match measured widths).
    pub fn new(text: TextSystem) -> Painter {
        Painter { text, fonts: HashMap::new(), images: HashMap::new() }
    }

    /// Replays `list` with its top-left scrolled to `scroll` device-independent
    /// pixels above the viewport's top. Only what intersects the viewport is
    /// drawn; a tall newsletter costs only what is on screen. `background`
    /// shows wherever the document paints nothing of its own.
    pub fn paint(&mut self, list: &DisplayList, canvas: &mut D2dCanvas, viewport: Rect, scroll: f32, background: Color) {
        let t = std::time::Instant::now();
        canvas.clear(background);
        canvas.push_clip(to_rect(viewport));
        canvas.set_translation(0.0, -scroll);

        // `viewport` translated into document space: the commands' own
        // coordinates.
        let visible = viewport.translate(0.0, scroll);
        let mut clips: Vec<Rect> = Vec::new();
        let mut drawn = 0usize;

        for cmd in &list.cmds {
            match cmd {
                Cmd::PushClip { rect, radii } => {
                    clips.push(*rect);
                    if is_rounded(radii) {
                        let _ = canvas.push_clip_rounded(to_rounded(*rect, *radii));
                    } else {
                        canvas.push_clip(to_rect(*rect));
                    }
                    continue;
                }
                Cmd::PopClip => {
                    clips.pop();
                    let _ = canvas.pop_clip();
                    continue;
                }
                _ => {}
            }
            if let Some(bounds) = cmd.bounds() {
                let narrowed = clips.iter().fold(visible, |a, c| a.intersect(*c).unwrap_or(Rect::default()));
                if !narrowed.intersects(bounds) {
                    continue;
                }
            }
            drawn += 1;
            self.draw(canvas, list, cmd);
        }
        log::debug!("d2d paint: replayed {drawn}/{} cmds in {:?}", list.cmds.len(), t.elapsed());
    }

    fn draw(&mut self, canvas: &mut D2dCanvas, list: &DisplayList, cmd: &Cmd) {
        match cmd {
            Cmd::Rect { rect, radii, fill } => {
                if is_rounded(radii) {
                    canvas.fill_rounded(to_rounded(*rect, *radii), to_rgba(*fill));
                } else {
                    canvas.fill_rect_rgba(to_rect(*rect), to_rgba(*fill));
                }
            }
            Cmd::Outline { rect, radii, stroke } => {
                canvas.stroke_rounded(to_rounded(*rect, *radii), to_rgba(stroke.color), to_stroke(*stroke));
            }
            Cmd::Line { a, b, stroke, dash } => {
                let pen = match dash {
                    Dash::Solid => to_stroke(*stroke),
                    Dash::Dashed => to_stroke(*stroke).dash(DashStyle::Dashed),
                    Dash::Dotted => to_stroke(*stroke).dash(DashStyle::Dotted).cap(Cap::Round),
                };
                canvas.draw_line_rgba(to_point(*a), to_point(*b), to_rgba(stroke.color), pen);
            }
            Cmd::Circle { center, radius, fill, stroke } => {
                if fill.a > 0 {
                    canvas.fill_ellipse_rgba(to_point(*center), *radius, *radius, to_rgba(*fill));
                }
                if stroke.width > 0.0 {
                    canvas.stroke_ellipse_rgba(to_point(*center), *radius, *radius, to_rgba(stroke.color), to_stroke(*stroke));
                }
            }
            Cmd::Text { origin, text, font, color, .. } => {
                if color.a == 0 {
                    return;
                }
                let Some(font) = self.font(list, *font) else { return };
                let Ok(layout) = font.layout(text, f32::INFINITY) else { return };
                canvas.draw_text(&layout, to_point(*origin), Color::rgb(color.r, color.g, color.b));
            }
            Cmd::Image { image, rect } => {
                let Some(id) = self.image(canvas, list, *image) else { return };
                canvas.draw_image(id, to_rect(*rect), None, 1.0, Interpolation::HighQualityCubic);
            }
            Cmd::LinearGradient { rect, gradient } => {
                let stops: Vec<GradientStop> = gradient
                    .stops
                    .iter()
                    .map(|s| GradientStop::new(s.offset, to_rgba(s.color)))
                    .collect();
                let grad = D2dLinearGradient::new(to_point(gradient.start), to_point(gradient.end), stops);
                canvas.fill_rect_linear(to_rect(*rect), &grad);
            }
            Cmd::RadialGradient { rect, gradient } => {
                let stops: Vec<GradientStop> = gradient
                    .stops
                    .iter()
                    .map(|s| GradientStop::new(s.offset, to_rgba(s.color)))
                    .collect();
                let grad = D2dRadialGradient::new(
                    to_point(gradient.center),
                    gradient.radius_x,
                    gradient.radius_y,
                    stops,
                );
                canvas.fill_rect_radial(to_rect(*rect), &grad);
            }
            Cmd::PushClip { .. } | Cmd::PopClip => unreachable!("handled in the replay loop"),
        }
    }

    fn font(&mut self, list: &DisplayList, key: FontKey) -> Option<Font> {
        if let Some(font) = self.fonts.get(&key) {
            return Some(font.clone());
        }
        let desc = list.fonts.get(key as usize)?;
        let spec = FontSpec::new(desc.family.clone(), desc.size).weight(desc.weight).italic(desc.italic);
        let font = self.text.font(&spec).ok()?;
        self.fonts.insert(key, font.clone());
        Some(font)
    }

    /// Resolves the font for `key`, for the widget's hit-testing and selection
    /// boxes (which rebuild a DirectWrite layout per run).
    pub fn resolve_font(&mut self, list: &DisplayList, key: FontKey) -> Option<Font> {
        self.font(list, key)
    }

    /// The caret nearest `doc`, using DirectWrite hit-testing for the
    /// character boundary (accurate for right-to-left and complex text, where
    /// the run table's left-to-right `offsets` are not).
    pub fn caret_at(&mut self, list: &DisplayList, runs: &TextRunTable, doc: Point) -> Option<TextPos> {
        let run = runs.nearest_run(doc)?;
        let text_run = &runs.runs[run];
        let font = self.resolve_font(list, text_run.font)?;
        let layout = font.layout(&text_run.text, f32::INFINITY).ok()?;
        let hit = layout.hit_test_point(doc.x - text_run.rect.left, 0.0);
        let byte = hit.index.min(text_run.text.len());
        let ch = text_run.text[..byte].chars().count();
        Some(TextPos { run, ch })
    }

    /// The highlight boxes of `sel`, in document coordinates, from DirectWrite's
    /// per-run selection rects, merged across words the way the run table's own
    /// offsets-based selection does (so a whole line highlights as one box).
    pub fn selection_rects(&mut self, list: &DisplayList, runs: &TextRunTable, sel: &Selection) -> Vec<Rect> {
        let (start, end) = sel.ordered();
        let mut out: Vec<Rect> = Vec::new();
        let Some(last_run) = runs.runs.len().checked_sub(1) else {
            return out;
        };
        for i in start.run..=end.run.min(last_run) {
            let run = &runs.runs[i];
            let from = if i == start.run { start.ch } else { 0 };
            let to = (if i == end.run { end.ch } else { run.char_count() }).min(run.char_count());
            if from >= to {
                continue;
            }
            let rect = run_rect(self, list, run, from, to).unwrap_or_else(|| Rect {
                left: run.rect.left + run.offsets[from],
                top: run.rect.top,
                right: run.rect.left + run.offsets[to],
                bottom: run.rect.bottom,
            });
            let extends_last = out.last().is_some_and(|last| {
                (last.center().y - rect.center().y).abs() < rect.height() * 0.5
                    && (rect.left - last.right).abs() < 2.0
            });
            if extends_last {
                let last = out.last_mut().unwrap();
                *last = last.union(rect);
            } else if !run.text.trim().is_empty() {
                out.push(rect);
            }
        }
        out
    }

    fn image(&mut self, canvas: &mut D2dCanvas, list: &DisplayList, key: ImageKey) -> Option<ImageId> {
        if let Some(id) = self.images.get(&key) {
            return Some(*id);
        }
        let image = list.images.get(key as usize)?;
        let wimg = xui_win32::RgbaImage {
            width: image.width,
            height: image.height,
            pixels: image.rgba.clone(),
        };
        let id = canvas.image(&wimg);
        self.images.insert(key, id);
        Some(id)
    }
}

/// The horizontal highlight extent of `run[from..to]`, from DirectWrite's
/// selection rects (the vertical is the run's own box, matching the highlight
/// of the egui widget). Falls back to `None` when the font cannot be resolved.
fn run_rect(painter: &mut Painter, list: &DisplayList, run: &TextRun, from: usize, to: usize) -> Option<Rect> {
    let font = painter.resolve_font(list, run.font)?;
    let layout = font.layout(&run.text, f32::INFINITY).ok()?;
    let from_byte = char_to_byte(&run.text, from);
    let to_byte = char_to_byte(&run.text, to);
    let boxes = layout.selection_rects(from_byte, to_byte);
    if boxes.is_empty() {
        return None;
    }
    let left = boxes.iter().map(|b| b.left).fold(f32::INFINITY, f32::min);
    let right = boxes.iter().map(|b| b.right).fold(f32::NEG_INFINITY, f32::max);
    Some(Rect {
        left: run.rect.left + left,
        top: run.rect.top,
        right: run.rect.left + right,
        bottom: run.rect.bottom,
    })
}

/// The byte offset of the `ch`-th character (clamped to the text's end).
fn char_to_byte(text: &str, ch: usize) -> usize {
    text.char_indices().nth(ch).map_or(text.len(), |(i, _)| i)
}
