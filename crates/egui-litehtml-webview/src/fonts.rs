//! Font handling for the painter engine: turning
//! a CSS `font-family` list + weight + style into something egui can draw.
//!
//! egui knows nothing about system fonts, and its [`FontId`] has no weight or
//! italic. This module bridges the two:
//!
//! * [`fontdb`] finds faces on the system and applies the CSS font-matching
//!   rules (nearest weight, italic -> oblique -> normal). Every entry of the
//!   `font-family` list is tried in order; the faces found become that text's
//!   *chain*.
//! * `fontdb` only indexes each family's *typographic* name (`Segoe UI`), but
//!   CSS authors write the legacy, style-linked names those faces also carry
//!   (`Segoe UI Semibold`, `Calibri Light`, `Open Sans Light`). Those are looked
//!   up in a second index, built from the fonts' name tables the first time a
//!   name misses; without it they would fall through to the last-resort face.
//! * A **variable** font (one file, a `wght` axis) is registered once per
//!   requested weight with that weight as an egui variation coordinate, so
//!   `font-weight: 700` on Bahnschrift really is bold; and a trailing weight
//!   word on a name that does not exist (`Bahnschrift Light`, `Inter SemiBold`)
//!   is read as that weight of the base family.
//! * Each distinct chain becomes one named egui font family (`lh:<n>`), backed
//!   by one registered font file per face. Faces are registered **lazily**, the
//!   first time a document asks for them, so nothing is copied for fonts never
//!   used.
//! * When a character is missing from a chain (CJK, dingbats, ...), the system
//!   is searched for a face that has it, and that face is added as a fallback
//!   to every chain.
//!
//! The [`FontBook`] lives on the worker thread and keeps a **private**
//! [`Fonts`] for measuring text: litehtml needs widths synchronously, while
//! installing fonts into the shared [`egui::Context`] only takes effect on the
//! next UI pass. The UI thread installs the same definitions before painting
//! (see [`crate::painter::install_fonts`]), so the widths measured here are the
//! widths that get painted.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use egui::Color32;
use egui::epaint::text::{
    FontData, FontDefinitions, FontFamily, FontId, FontTweak, Fonts, TextOptions, VariationCoords,
};
use fontdb::{Database, Family, Query, Style, Weight, ID};

/// Prefix of the egui family names this module creates.
pub(crate) const FAMILY_PREFIX: &str = "lh:";
/// Prefix of the egui font-data names this module creates.
pub(crate) const FONT_PREFIX: &str = "lhf";

/// Hands every [`FontBook`] its own number, which goes into the names it gives
/// to egui: views share one `egui::Context`, and its font definitions are keyed
/// by name, so two books both calling their first family `lh:0` would replace
/// each other's fonts.
static NEXT_BOOK_ID: AtomicU64 = AtomicU64::new(0);

// Candidate families for CSS generic names, best first. fontdb's own generics
// name a single family each (Arial, Times New Roman, ...), which is missing on
// most Linux systems.
const SANS: &[&str] = &["Arial", "Liberation Sans", "Helvetica", "DejaVu Sans", "Noto Sans", "Roboto", "Segoe UI", "Verdana"];
const SERIF: &[&str] = &["Times New Roman", "Liberation Serif", "DejaVu Serif", "Noto Serif", "Georgia"];
const MONO: &[&str] = &["Consolas", "Courier New", "Liberation Mono", "DejaVu Sans Mono", "Noto Sans Mono"];
const SYSTEM_UI: &[&str] = &["Segoe UI", "Roboto", "Cantarell", "Ubuntu", "Noto Sans", "DejaVu Sans", "Arial"];
const CURSIVE: &[&str] = &["Comic Sans MS", "Segoe Script", "Apple Chancery"];
const FANTASY: &[&str] = &["Impact", "Papyrus"];

/// Where to look for a glyph no face in a chain has, best first. Colour emoji
/// fonts are deliberately absent: egui draws outlines only, so they would show
/// as blanks; egui's bundled monochrome emoji font is already in every chain.
const GLYPH_FALLBACKS: &[&str] = &[
    "Segoe UI Symbol",
    "Microsoft YaHei",
    "Yu Gothic",
    "Malgun Gothic",
    "Microsoft JhengHei",
    "Nirmala UI",
    "Leelawadee UI",
    "Meiryo",
    "MS Gothic",
    "SimSun",
    "Arial Unicode MS",
    "Noto Sans CJK SC",
    "Noto Sans CJK JP",
    "Noto Sans CJK KR",
    "Noto Sans Arabic",
    "Noto Sans Hebrew",
    "Noto Sans Thai",
    "Noto Sans Symbols",
    "Noto Sans Symbols 2",
    "DejaVu Sans",
    "Apple Symbols",
    "PingFang SC",
    "Hiragino Sans",
    "Segoe UI",
    "Tahoma",
    "Arial",
];

/// One face as egui gets it: a fontdb face, plus for a variable font the
/// `wght` value it is pinned to (0 = the file as it is).
type Face = (ID, u16);

/// Weight words that can end a family name (`Segoe UI Semibold`), longest
/// first so `extra light` is not read as `light`.
const WEIGHT_WORDS: &[(&str, u16)] = &[
    ("extra light", 200),
    ("extralight", 200),
    ("ultra light", 200),
    ("ultralight", 200),
    ("semi light", 350),
    ("semilight", 350),
    ("semi bold", 600),
    ("semibold", 600),
    ("demi bold", 600),
    ("demibold", 600),
    ("extra bold", 800),
    ("extrabold", 800),
    ("ultra bold", 800),
    ("ultrabold", 800),
    ("thin", 100),
    ("light", 300),
    ("regular", 400),
    ("medium", 500),
    ("bold", 700),
    ("black", 900),
    ("heavy", 900),
];

/// `bahnschrift light` -> (`bahnschrift`, 300): a family name ending in a
/// weight word. `None` when there is no base left (`light` alone).
fn split_weight_word(lower_name: &str) -> Option<(&str, u16)> {
    WEIGHT_WORDS.iter().find_map(|(word, weight)| {
        let base = lower_name.strip_suffix(word)?.strip_suffix(' ')?.trim_end();
        (!base.is_empty()).then_some((base, *weight))
    })
}

/// What a `(font-family list, weight, italic)` request resolved to.
#[derive(Clone)]
pub(crate) struct Resolved {
    /// The egui family to draw with.
    pub family: FontFamily,
    /// Italic was asked for but the matched face is upright: egui skews the
    /// glyphs itself (`TextFormat::italics`).
    pub synth_italic: bool,
    /// The face matched first, for metrics egui does not expose.
    pub primary: Option<ID>,
}

pub(crate) struct FontBook {
    /// Distinguishes this book's egui names from every other book's.
    id: u64,
    db: Database,
    /// egui's bundled defaults plus everything registered here.
    defs: FontDefinitions,
    /// Font-data names egui's own `Proportional` family ends with: the tail of
    /// every chain.
    default_tail: Vec<String>,
    /// fontdb face -> name in `defs.font_data`.
    keys: HashMap<Face, String>,
    /// Face chain -> egui family.
    chains: HashMap<Vec<Face>, FontFamily>,
    /// Faces added to every chain because a glyph was missing.
    fallbacks: Vec<ID>,
    /// Characters for which no fallback face exists (do not search again).
    no_fallback: HashSet<char>,
    fallback_candidates: Option<Vec<ID>>,
    /// Lower-cased legacy family name (name-table ID 1) -> faces, built on the
    /// first miss. See the module docs.
    legacy_names: Option<HashMap<String, Vec<ID>>>,
    /// Face -> the `(min, max)` of its `wght` axis, if it has one.
    wght_axes: HashMap<ID, Option<(u16, u16)>>,
    resolved: HashMap<(String, u16, bool), Resolved>,
    /// Measures text. Rebuilt from `defs` whenever that changes.
    private: Fonts,
    /// `defs` changed since `private` / `shared` were built.
    dirty: bool,
    /// Snapshot of `defs`, handed to the UI thread.
    shared: Arc<FontDefinitions>,
    max_texture_side: usize,
}

fn text_options(max_texture_side: usize) -> TextOptions {
    TextOptions { max_texture_side, ..Default::default() }
}

impl FontBook {
    pub(crate) fn new(max_texture_side: usize) -> Self {
        let t = std::time::Instant::now();
        let mut db = Database::new();
        db.load_system_fonts();
        log::debug!("fonts: indexed {} system faces in {:?}", db.len(), t.elapsed());
        let defs = FontDefinitions::default();
        let default_tail = defs.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
        Self {
            id: NEXT_BOOK_ID.fetch_add(1, Ordering::Relaxed),
            db,
            private: Fonts::new(text_options(max_texture_side), defs.clone()),
            shared: Arc::new(defs.clone()),
            defs,
            default_tail,
            keys: HashMap::new(),
            chains: HashMap::new(),
            fallbacks: Vec::new(),
            no_fallback: HashSet::new(),
            fallback_candidates: None,
            legacy_names: None,
            wght_axes: HashMap::new(),
            resolved: HashMap::new(),
            dirty: false,
            max_texture_side,
        }
    }

    // ── Resolution ──────────────────────────────────────────────────────

    /// Resolve a CSS `font-family` list. Never fails: with no usable face at
    /// all the result is egui's own proportional font.
    pub(crate) fn resolve(&mut self, css_family: &str, weight: i32, italic: bool) -> Resolved {
        let key = (css_family.to_string(), weight.clamp(1, 1000) as u16, italic);
        if let Some(r) = self.resolved.get(&key) {
            return r.clone();
        }
        let style = if italic { Style::Italic } else { Style::Normal };
        let weight = Weight(key.1);

        let mut chain: Vec<Face> = Vec::new();
        for entry in css_family.split(',') {
            let entry = entry.trim().trim_matches(|c| c == '"' || c == '\'').trim();
            if entry.is_empty() {
                continue;
            }
            if let Some((id, entry_weight)) = self.query_entry(entry, weight, style) {
                let face = self.pin_weight(id, entry_weight);
                if !chain.contains(&face) {
                    chain.push(face);
                }
            }
        }
        if chain.is_empty() {
            let last_resort = self
                .query_list(SANS, weight, style)
                .or_else(|| self.db.faces().next().map(|f| f.id));
            if let Some(id) = last_resort {
                chain.push(self.pin_weight(id, weight));
            }
        }

        let resolved = match chain.first().copied() {
            None => Resolved { family: FontFamily::Proportional, synth_italic: italic, primary: None },
            Some((primary, _)) => {
                let synth_italic = italic && self.db.face(primary).is_none_or(|f| f.style == Style::Normal);
                Resolved { family: self.family_for(chain), synth_italic, primary: Some(primary) }
            }
        };
        self.resolved.insert(key, resolved.clone());
        resolved
    }

    /// One entry of a `font-family` list -> a face, if this system has it, and
    /// the weight to use for it (the request's, unless the name itself carried
    /// one: `Bahnschrift Light`).
    fn query_entry(&mut self, entry: &str, weight: Weight, style: Style) -> Option<(ID, Weight)> {
        let lower = entry.to_ascii_lowercase();
        let generic: Option<&[&str]> = match lower.as_str() {
            "sans-serif" | "ui-sans-serif" => Some(SANS),
            "serif" | "ui-serif" => Some(SERIF),
            "monospace" | "ui-monospace" => Some(MONO),
            "system-ui" | "-apple-system" | "blinkmacsystemfont" | "ui-rounded" => Some(SYSTEM_UI),
            "cursive" => Some(CURSIVE),
            "fantasy" => Some(FANTASY),
            _ => None,
        };
        match generic {
            Some(candidates) => self.query_list(candidates, weight, style).map(|id| (id, weight)),
            None => {
                // Named family. Windows has no Helvetica; mail nearly always
                // lists it as "Arial's sibling", so let it stand for sans.
                let named = self
                    .query_one(Family::Name(entry), weight, style)
                    .or_else(|| self.query_legacy(&lower, weight, style))
                    .map(|id| (id, weight));
                if named.is_some() {
                    return named;
                }
                // `Bahnschrift Light`, `Inter SemiBold`: not a family of its own
                // (a variable font's named instance, or a weight that is not
                // installed), but a weight of the base family.
                if let Some((base, word_weight)) = split_weight_word(&lower) {
                    // A bold request on top of a named weight (`<b>` inside
                    // `Segoe UI Light`) wins, as it would for any family.
                    let effective = Weight(if weight.0 >= 600 { weight.0 } else { word_weight });
                    let base = &entry[..base.len()];
                    let found = self
                        .query_one(Family::Name(base), effective, style)
                        .or_else(|| self.query_legacy(&base.to_lowercase(), effective, style));
                    if let Some(id) = found {
                        return Some((id, effective));
                    }
                }
                if matches!(lower.as_str(), "helvetica" | "helvetica neue") {
                    return self.query_list(SANS, weight, style).map(|id| (id, weight));
                }
                None
            }
        }
    }

    /// The `wght` range of a variable face, `None` for an ordinary one.
    fn wght_axis(&mut self, id: ID) -> Option<(u16, u16)> {
        if let Some(axis) = self.wght_axes.get(&id) {
            return *axis;
        }
        let axis = self
            .db
            .with_face_data(id, |data, index| {
                let face = ttf_parser::Face::parse(data, index).ok()?;
                let wght = ttf_parser::Tag::from_bytes(b"wght");
                face.variation_axes()
                    .into_iter()
                    .find(|a| a.tag == wght)
                    .map(|a| (a.min_value.ceil() as u16, a.max_value.floor() as u16))
            })
            .flatten();
        self.wght_axes.insert(id, axis);
        axis
    }

    /// `id` as egui should get it for `weight`: pinned to that weight if it is a
    /// variable font (clamped to what the font offers), untouched otherwise.
    fn pin_weight(&mut self, id: ID, weight: Weight) -> Face {
        match self.wght_axis(id) {
            Some((min, max)) if min <= max => (id, weight.0.clamp(min, max)),
            _ => (id, 0),
        }
    }

    /// A legacy family name (`segoe ui semibold`) -> its best face for the
    /// request. Such a family holds only a few style-linked faces (regular and
    /// italic of one weight), so nearest weight within the wanted style is enough.
    fn query_legacy(&mut self, lower_name: &str, weight: Weight, style: Style) -> Option<ID> {
        let ids = self.legacy_index().get(lower_name)?.clone();
        ids.into_iter().min_by_key(|id| {
            let face = self.db.face(*id);
            let style_penalty = match face.map(|f| f.style) {
                Some(s) if s == style => 0,
                Some(Style::Oblique) if style == Style::Italic => 1,
                _ => 2,
            };
            let weight_gap = face.map_or(i32::MAX, |f| (f.weight.0 as i32 - weight.0 as i32).abs());
            (style_penalty, weight_gap)
        })
    }

    /// Built once, from every face's name table (~a few ms per hundred faces).
    fn legacy_index(&mut self) -> &HashMap<String, Vec<ID>> {
        if self.legacy_names.is_none() {
            let t = std::time::Instant::now();
            let mut index: HashMap<String, Vec<ID>> = HashMap::new();
            let ids: Vec<ID> = self.db.faces().map(|f| f.id).collect();
            for id in ids {
                let names: Vec<String> = self
                    .db
                    .with_face_data(id, |data, face_index| {
                        let face = ttf_parser::Face::parse(data, face_index).ok()?;
                        Some(
                            face.names()
                                .into_iter()
                                .filter(|n| n.name_id == ttf_parser::name_id::FAMILY && n.is_unicode())
                                .filter_map(|n| n.to_string())
                                .collect::<Vec<_>>(),
                        )
                    })
                    .flatten()
                    .unwrap_or_default();
                for name in names {
                    let faces = index.entry(name.to_lowercase()).or_default();
                    if !faces.contains(&id) {
                        faces.push(id);
                    }
                }
            }
            log::debug!("fonts: indexed legacy family names of {} faces in {:?}", self.db.len(), t.elapsed());
            self.legacy_names = Some(index);
        }
        self.legacy_names.as_ref().expect("just built")
    }

    fn query_list(&self, candidates: &[&str], weight: Weight, style: Style) -> Option<ID> {
        candidates.iter().find_map(|name| self.query_one(Family::Name(name), weight, style))
    }

    fn query_one(&self, family: Family<'_>, weight: Weight, style: Style) -> Option<ID> {
        self.db.query(&Query { families: &[family], weight, style, ..Default::default() })
    }

    // ── egui registration ───────────────────────────────────────────────

    /// Register the faces of `chain` (once) and return its egui family.
    fn family_for(&mut self, chain: Vec<Face>) -> FontFamily {
        if let Some(f) = self.chains.get(&chain) {
            return f.clone();
        }
        for face in &chain {
            self.register_face(*face);
        }
        let family = FontFamily::Name(format!("{FAMILY_PREFIX}{}:{}", self.id, self.chains.len()).into());
        self.chains.insert(chain, family.clone());
        self.rebuild_family_lists();
        family
    }

    fn register_face(&mut self, face: Face) -> Option<String> {
        if let Some(k) = self.keys.get(&face) {
            return Some(k.clone());
        }
        let (id, pinned_weight) = face;
        let t = std::time::Instant::now();
        let (bytes, index) = self.db.with_face_data(id, |data, index| (data.to_vec(), index))?;
        log::debug!(
            "fonts: registered {:?} wght={pinned_weight} ({} KB) in {:?}",
            self.db.face(id).map(|f| f.post_script_name.clone()),
            bytes.len() / 1024,
            t.elapsed()
        );
        let key = format!("{FONT_PREFIX}{}_{}", self.id, self.keys.len());
        let mut tweak = FontTweak::default();
        if pinned_weight != 0 {
            tweak.coords = VariationCoords::new([(b"wght", pinned_weight as f32)]);
        }
        let data = FontData { font: std::borrow::Cow::Owned(bytes), index, tweak };
        self.defs.font_data.insert(key.clone(), Arc::new(data));
        self.keys.insert(face, key.clone());
        Some(key)
    }

    /// Every chain: its own faces, then the shared glyph fallbacks, then
    /// egui's bundled fonts.
    fn rebuild_family_lists(&mut self) {
        for (chain, family) in &self.chains {
            let list: Vec<String> = chain
                .iter()
                .copied()
                .chain(self.fallbacks.iter().map(|id| (*id, 0)))
                .filter_map(|face| self.keys.get(&face).cloned())
                .chain(self.default_tail.iter().cloned())
                .collect();
            self.defs.families.insert(family.clone(), list);
        }
        self.dirty = true;
    }

    /// Bring `private` and `shared` up to date with `defs`.
    fn sync(&mut self) {
        if self.dirty {
            let t = std::time::Instant::now();
            self.private = Fonts::new(text_options(self.max_texture_side), self.defs.clone());
            self.shared = Arc::new(self.defs.clone());
            self.dirty = false;
            log::debug!("fonts: rebuilt the measuring fonts ({} families) in {:?}", self.chains.len(), t.elapsed());
        }
    }

    /// The definitions the UI thread must install before painting text laid
    /// out by this book.
    pub(crate) fn definitions(&mut self) -> Arc<FontDefinitions> {
        self.sync();
        self.shared.clone()
    }

    // ── Glyph fallback ──────────────────────────────────────────────────

    /// Make sure every character of `text` can be drawn with `font`, adding a
    /// system fallback face to all chains where one is found.
    pub(crate) fn ensure_glyphs(&mut self, text: &str, font: &FontId) {
        for c in text.chars() {
            if c.is_ascii() || c.is_control() || self.no_fallback.contains(&c) {
                continue;
            }
            self.sync();
            if self.private.has_glyph(font, c) {
                continue;
            }
            match self.find_fallback(c) {
                Some(id) => {
                    if self.fallbacks.contains(&id) || self.register_face((id, 0)).is_none() {
                        self.no_fallback.insert(c);
                        continue;
                    }
                    self.fallbacks.push(id);
                    self.rebuild_family_lists();
                }
                None => {
                    self.no_fallback.insert(c);
                }
            }
        }
    }

    fn find_fallback(&mut self, c: char) -> Option<ID> {
        if self.fallback_candidates.is_none() {
            let mut ids: Vec<ID> = Vec::new();
            for name in GLYPH_FALLBACKS {
                if let Some(id) = self.query_one(Family::Name(name), Weight::NORMAL, Style::Normal)
                    && !ids.contains(&id)
                {
                    ids.push(id);
                }
            }
            self.fallback_candidates = Some(ids);
        }
        self.fallback_candidates
            .as_ref()?
            .iter()
            .copied()
            .find(|id| self.face_has_glyph(*id, c))
    }

    fn face_has_glyph(&self, id: ID, c: char) -> bool {
        self.db
            .with_face_data(id, |data, index| {
                ttf_parser::Face::parse(data, index).ok().and_then(|f| f.glyph_index(c)).is_some()
            })
            .unwrap_or(false)
    }

    // ── Measuring ───────────────────────────────────────────────────────

    /// Start of one layout pass: drops cached layouts and recreates the glyph
    /// atlas when it is nearly full.
    pub(crate) fn begin_pass(&mut self) {
        self.sync();
        self.private.begin_pass(text_options(self.max_texture_side));
    }

    /// Width of `text` in points at `pixels_per_point`.
    pub(crate) fn measure(&mut self, text: &str, font: &FontId, pixels_per_point: f32) -> f32 {
        self.ensure_glyphs(text, font);
        self.sync();
        self.private
            .with_pixels_per_point(pixels_per_point)
            .layout_no_wrap(text.to_owned(), font.clone(), Color32::WHITE)
            .rect
            .width()
    }

    /// `(ascent, line height, advance of "0")` of `font`, in points.
    pub(crate) fn metrics(&mut self, font: &FontId, pixels_per_point: f32) -> (f32, f32, f32) {
        self.sync();
        let galley = self
            .private
            .with_pixels_per_point(pixels_per_point)
            .layout_no_wrap("0".to_owned(), font.clone(), Color32::WHITE);
        match galley.rows.first().and_then(|r| r.row.glyphs.first()) {
            Some(g) => (g.font_ascent, g.font_height, g.advance_width),
            None => (font.size * 0.8, font.size * 1.2, font.size * 0.55),
        }
    }

    /// The face's own x-height, scaled to `size`.
    pub(crate) fn x_height(&self, face: Option<ID>, size: f32) -> Option<f32> {
        self.db.with_face_data(face?, |data, index| {
            let f = ttf_parser::Face::parse(data, index).ok()?;
            Some(f.x_height()? as f32 / f.units_per_em() as f32 * size)
        })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book() -> FontBook {
        FontBook::new(2048)
    }

    #[test]
    fn a_family_list_resolves_to_its_first_installed_family_not_the_platform_default() {
        let mut b = book();
        // `Arial,Helvetica,sans-serif` must measure like `Arial` where Arial is
        // installed.
        let list = b.resolve("Arial,Helvetica,sans-serif", 400, false);
        let single = b.resolve("Arial", 400, false);
        let unknown = b.resolve("No Such Font Anywhere", 400, false);
        let w = |b: &mut FontBook, r: &Resolved| b.measure("Temps", &FontId::new(14.0, r.family.clone()), 1.0);
        let (wl, ws) = (w(&mut b, &list), w(&mut b, &single));
        assert!((wl - ws).abs() < 0.01, "list {wl} vs single {ws}");
        // A list with nothing installed still resolves to *something* drawable.
        assert!(w(&mut b, &unknown) > 0.0);
    }

    #[test]
    fn bold_and_regular_resolve_to_different_faces() {
        let mut b = book();
        let regular = b.resolve("Arial", 400, false);
        let bold = b.resolve("Arial", 700, false);
        if b.db.query(&Query { families: &[Family::Name("Arial")], weight: Weight::BOLD, ..Default::default() }).is_none() {
            return; // no Arial on this machine
        }
        let w = |b: &mut FontBook, r: &Resolved| b.measure("Wide text sample", &FontId::new(14.0, r.family.clone()), 1.0);
        assert!(w(&mut b, &bold) > w(&mut b, &regular), "bold must be a wider face, not the regular one");
    }

    #[test]
    fn italic_is_synthesised_only_when_the_matched_face_is_upright() {
        let mut b = book();
        let r = b.resolve("Arial", 400, true);
        // Judge by the face that actually matched: which family stands in for
        // Arial depends on the machine.
        let matched_is_italic = r
            .primary
            .and_then(|id| b.db.face(id))
            .is_some_and(|f| f.style != Style::Normal);
        assert_eq!(r.synth_italic, !matched_is_italic);
        // An upright request never synthesises.
        assert!(!b.resolve("Arial", 400, false).synth_italic);
    }

    #[test]
    fn measuring_counts_spaces() {
        let mut b = book();
        let r = b.resolve("sans-serif", 400, false);
        let id = FontId::new(14.0, r.family);
        let (ab, a_b, space) = (b.measure("ab", &id, 1.0), b.measure("a b", &id, 1.0), b.measure(" ", &id, 1.0));
        assert!(space > 0.0, "a lone space has width");
        assert!(a_b > ab, "a space inside a run has width");
    }

    #[test]
    fn metrics_are_sane() {
        let mut b = book();
        let r = b.resolve("sans-serif", 400, false);
        let (asc, h, ch) = b.metrics(&FontId::new(16.0, r.family), 1.0);
        assert!((8.0..=20.0).contains(&asc), "ascent {asc}");
        assert!(h >= asc && h < 30.0, "height {h}");
        assert!((4.0..=16.0).contains(&ch), "advance of 0 is {ch}");
    }

    #[test]
    fn a_missing_glyph_pulls_in_a_fallback_face_and_a_new_definition_set() {
        let mut b = book();
        let r = b.resolve("Arial", 400, false);
        let id = FontId::new(14.0, r.family.clone());
        let before = b.definitions();
        // U+2794 HEAVY WIDE-HEADED RIGHTWARDS ARROW: not in Arial.
        b.ensure_glyphs("\u{2794}", &id);
        let after = b.definitions();
        let Some(&fallback) = b.fallbacks.first() else {
            return; // nothing on this machine has it; the char is simply skipped
        };
        assert!(!Arc::ptr_eq(&before, &after), "the UI thread must be handed new definitions");
        // The fallback is in the family the text is drawn with, ahead of egui's bundled fonts.
        let key = &b.keys[&(fallback, 0)];
        let list = &after.families[&r.family];
        let (at, bundled) = (list.iter().position(|k| k == key), list.iter().position(|k| *k == b.default_tail[0]));
        assert!(at.is_some() && at < bundled, "{list:?}");
        // Asking again changes nothing (no rebuild per character).
        b.ensure_glyphs("\u{2794}\u{2794}", &id);
        assert!(Arc::ptr_eq(&after, &b.definitions()));
        // NB: `Fonts::has_glyph` cannot check this -- egui reports the face that
        // owns its replacement glyph as "missing everything".
    }

    #[test]
    fn two_font_books_never_share_an_egui_family_or_font_name() {
        // Views share one egui::Context and its definitions are keyed by name:
        // if each book started counting from zero, the second view to install
        // would silently replace the first one's fonts.
        let (mut a, mut b) = (book(), book());
        let fa = a.resolve("Arial", 400, false).family;
        let fb = b.resolve("Times New Roman", 400, false).family;
        assert_ne!(fa, fb, "both books named their first family {fa:?}");
        let (da, db) = (a.definitions(), b.definitions());
        let ours = |d: &FontDefinitions| -> Vec<String> {
            d.font_data.keys().filter(|k| k.starts_with(FONT_PREFIX)).cloned().collect()
        };
        assert!(ours(&da).iter().all(|k| !ours(&db).contains(k)), "{:?} vs {:?}", ours(&da), ours(&db));
    }

    #[test]
    fn a_legacy_family_name_finds_its_face_instead_of_the_last_resort() {
        // "Segoe UI Semibold", "Calibri Light", "Open Sans Light" ...: names CSS
        // uses that fontdb does not index (it keeps only the typographic family).
        // Find such a face on this machine from the fonts themselves, so the test
        // holds wherever some exist and is a no-op where none do.
        let mut b = book();
        let ids: Vec<ID> = b.db.faces().map(|f| f.id).collect();
        let mut checked = 0;
        for id in ids {
            let typographic: Vec<String> = b.db.face(id).unwrap().families.iter().map(|(n, _)| n.to_lowercase()).collect();
            let legacy: Vec<String> = b
                .db
                .with_face_data(id, |data, i| {
                    let face = ttf_parser::Face::parse(data, i).ok()?;
                    Some(
                        face.names()
                            .into_iter()
                            .filter(|n| n.name_id == ttf_parser::name_id::FAMILY && n.is_unicode())
                            .filter_map(|n| n.to_string())
                            .collect::<Vec<_>>(),
                    )
                })
                .flatten()
                .unwrap_or_default();
            for name in legacy.into_iter().filter(|n| !typographic.contains(&n.to_lowercase())) {
                // Skip names another family also owns outright (fontdb would find it).
                if b.query_one(Family::Name(&name), Weight::NORMAL, Style::Normal).is_some() {
                    continue;
                }
                let r = b.resolve(&name, 400, false);
                let got = r.primary.and_then(|p| b.db.face(p)).map(|f| f.post_script_name.clone());
                let owner_ids: Vec<ID> = b.legacy_index().get(&name.to_lowercase()).cloned().unwrap_or_default();
                let owners: Vec<String> =
                    owner_ids.iter().filter_map(|i| b.db.face(*i)).map(|f| f.post_script_name.clone()).collect();
                assert!(got.as_ref().is_some_and(|g| owners.contains(g)), "{name:?} resolved to {got:?}, not one of {owners:?}");
                checked += 1;
                if checked >= 25 {
                    return;
                }
            }
        }
    }

    #[test]
    fn a_trailing_weight_word_is_split_off_a_family_name() {
        assert_eq!(split_weight_word("bahnschrift light"), Some(("bahnschrift", 300)));
        assert_eq!(split_weight_word("segoe ui semibold"), Some(("segoe ui", 600)));
        assert_eq!(split_weight_word("inter extra bold"), Some(("inter", 800)));
        assert_eq!(split_weight_word("inter extralight"), Some(("inter", 200)), "not read as plain `light`");
        // Nothing left of the name, or not a separate word.
        assert_eq!(split_weight_word("light"), None);
        assert_eq!(split_weight_word("black"), None);
        assert_eq!(split_weight_word("lightning"), None);
        assert_eq!(split_weight_word("arial"), None);
    }

    /// A variable font with a `wght` axis on this machine, if any.
    fn variable_face(b: &mut FontBook) -> Option<(ID, String, (u16, u16))> {
        let ids: Vec<ID> = b.db.faces().map(|f| f.id).collect();
        ids.into_iter().find_map(|id| {
            let axis = b.wght_axis(id).filter(|(min, max)| min < max)?;
            let name = b.db.face(id)?.families.first()?.0.clone();
            // Its family must resolve to it by name (some have static siblings).
            Some((id, name, axis))
        })
    }

    #[test]
    fn a_variable_font_is_pinned_to_the_requested_weight() {
        let mut b = book();
        let Some((_, family, (min, max))) = variable_face(&mut b) else { return };
        let light = b.resolve(&family, min as i32, false);
        let bold = b.resolve(&family, max as i32, false);
        assert_ne!(light.family, bold.family, "one file, two pinned weights: two egui fonts");
        let defs = b.definitions();
        let coords_of = |r: &Resolved| -> Option<VariationCoords> {
            let key = defs.families[&r.family].first()?;
            Some(defs.font_data[key].tweak.coords.clone())
        };
        assert_ne!(coords_of(&light), coords_of(&bold));
        // And it shows in the widths: a heavier instance of the same font is wider.
        let width = |b: &mut FontBook, r: &Resolved| b.measure("Hamburgefonstiv quick brown fox", &FontId::new(16.0, r.family.clone()), 1.0);
        let (wl, wb) = (width(&mut b, &light), width(&mut b, &bold));
        assert!(wb >= wl, "bold {wb} vs light {wl}");
    }

    #[test]
    fn a_weight_word_on_a_variable_fonts_name_resolves_to_that_font_not_the_last_resort() {
        let mut b = book();
        let Some((id, family, (min, _))) = variable_face(&mut b) else { return };
        // Only meaningful if `<family> Light` is not itself a family on this machine.
        let name = format!("{family} Light");
        if b.query_one(Family::Name(&name), Weight::NORMAL, Style::Normal).is_some()
            || b.legacy_index().contains_key(&name.to_lowercase())
        {
            return;
        }
        let r = b.resolve(&name, 400, false);
        assert_eq!(r.primary, Some(id), "{name:?} fell through to {:?}", r.primary.and_then(|p| b.db.face(p)).map(|f| f.post_script_name.clone()));
        let defs = b.definitions();
        let key = defs.families[&r.family].first().unwrap().clone();
        let want = 300u16.clamp(min, b.wght_axis(id).unwrap().1);
        assert_eq!(defs.font_data[&key].tweak.coords, VariationCoords::new([(b"wght", want as f32)]));
    }
}
