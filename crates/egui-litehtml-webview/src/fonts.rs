//! Font handling for the [`Backend::Painter`](crate::Backend) engine: turning
//! a CSS `font-family` list + weight + style into something egui can draw.
//!
//! egui knows nothing about system fonts, and its [`FontId`] has no weight or
//! italic. This module bridges the two:
//!
//! * [`fontdb`] finds faces on the system and applies the CSS font-matching
//!   rules (nearest weight, italic -> oblique -> normal). Every entry of the
//!   `font-family` list is tried in order; the faces found become that text's
//!   *chain*. (The Pixbuf backend hands the whole comma-separated list to
//!   cosmic-text as a single family name, which matches nothing, so mail
//!   written for `Arial, Helvetica, sans-serif` falls back to the platform
//!   default face instead.)
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
use egui::epaint::text::{FontData, FontDefinitions, FontFamily, FontId, FontTweak, Fonts, TextOptions};
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
    keys: HashMap<ID, String>,
    /// Face chain -> egui family.
    chains: HashMap<Vec<ID>, FontFamily>,
    /// Faces added to every chain because a glyph was missing.
    fallbacks: Vec<ID>,
    /// Characters for which no fallback face exists (do not search again).
    no_fallback: HashSet<char>,
    fallback_candidates: Option<Vec<ID>>,
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

        let mut chain: Vec<ID> = Vec::new();
        for entry in css_family.split(',') {
            let entry = entry.trim().trim_matches(|c| c == '"' || c == '\'').trim();
            if entry.is_empty() {
                continue;
            }
            if let Some(id) = self.query_entry(entry, weight, style)
                && !chain.contains(&id)
            {
                chain.push(id);
            }
        }
        if chain.is_empty() {
            let last_resort = self
                .query_list(SANS, weight, style)
                .or_else(|| self.db.faces().next().map(|f| f.id));
            chain.extend(last_resort);
        }

        let resolved = match chain.first().copied() {
            None => Resolved { family: FontFamily::Proportional, synth_italic: italic, primary: None },
            Some(primary) => {
                let synth_italic = italic && self.db.face(primary).is_none_or(|f| f.style == Style::Normal);
                Resolved { family: self.family_for(chain), synth_italic, primary: Some(primary) }
            }
        };
        self.resolved.insert(key, resolved.clone());
        resolved
    }

    /// One entry of a `font-family` list -> a face, if this system has it.
    fn query_entry(&self, entry: &str, weight: Weight, style: Style) -> Option<ID> {
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
            Some(candidates) => self.query_list(candidates, weight, style),
            None => {
                // Named family. Windows has no Helvetica; mail nearly always
                // lists it as "Arial's sibling", so let it stand for sans.
                let named = self.query_one(Family::Name(entry), weight, style);
                if named.is_none() && matches!(lower.as_str(), "helvetica" | "helvetica neue") {
                    return self.query_list(SANS, weight, style);
                }
                named
            }
        }
    }

    fn query_list(&self, candidates: &[&str], weight: Weight, style: Style) -> Option<ID> {
        candidates.iter().find_map(|name| self.query_one(Family::Name(name), weight, style))
    }

    fn query_one(&self, family: Family<'_>, weight: Weight, style: Style) -> Option<ID> {
        self.db.query(&Query { families: &[family], weight, style, ..Default::default() })
    }

    // ── egui registration ───────────────────────────────────────────────

    /// Register the faces of `chain` (once) and return its egui family.
    fn family_for(&mut self, chain: Vec<ID>) -> FontFamily {
        if let Some(f) = self.chains.get(&chain) {
            return f.clone();
        }
        for id in &chain {
            self.register_face(*id);
        }
        let family = FontFamily::Name(format!("{FAMILY_PREFIX}{}:{}", self.id, self.chains.len()).into());
        self.chains.insert(chain, family.clone());
        self.rebuild_family_lists();
        family
    }

    fn register_face(&mut self, id: ID) -> Option<String> {
        if let Some(k) = self.keys.get(&id) {
            return Some(k.clone());
        }
        let t = std::time::Instant::now();
        let (bytes, index) = self.db.with_face_data(id, |data, index| (data.to_vec(), index))?;
        log::debug!("fonts: registered {:?} ({} KB) in {:?}", self.db.face(id).map(|f| f.post_script_name.clone()), bytes.len() / 1024, t.elapsed());
        let key = format!("{FONT_PREFIX}{}_{}", self.id, self.keys.len());
        let data = FontData { font: std::borrow::Cow::Owned(bytes), index, tweak: FontTweak::default() };
        self.defs.font_data.insert(key.clone(), Arc::new(data));
        self.keys.insert(id, key.clone());
        Some(key)
    }

    /// Every chain: its own faces, then the shared glyph fallbacks, then
    /// egui's bundled fonts.
    fn rebuild_family_lists(&mut self) {
        for (chain, family) in &self.chains {
            let list: Vec<String> = chain
                .iter()
                .chain(self.fallbacks.iter())
                .filter_map(|id| self.keys.get(id).cloned())
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
                    if self.fallbacks.contains(&id) || self.register_face(id).is_none() {
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
        // The whole point versus PixbufContainer: `Arial,Helvetica,sans-serif`
        // must measure like `Arial` where Arial is installed.
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
        let key = &b.keys[&fallback];
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
}
