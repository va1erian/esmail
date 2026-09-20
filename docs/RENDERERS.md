# Message renderer: egui painter

Message bodies are laid out by litehtml and painted with **egui's own painter**:
the worker records a display list, the UI thread replays it every frame. This
replaced the original tiny-skia renderer (litehtml-rs's `PixbufContainer`), which
was removed once the painter looked better on most real mail and cost less.

The removed renderer is kept on the branch `backup/pixbuf-and-painter-renderers`
(both engines, the `Backend` switch, F9 / `ESMAIL_RENDERER`, the
`renderer_compare` example). This page records what the comparison found; the
sections below that talk about "Pixbuf" describe that removed engine.

|  | tiny-skia (removed) | egui painter |
|---|---|---|
| Layout | litehtml | litehtml (same) |
| Text measuring + shaping | cosmic-text (rustybuzz) | egui's stack (harfrust), fonts found with `fontdb` |
| Painting | tiny-skia rasterizes a bitmap **on the worker**, uploaded as tiled textures | worker records a **display list**; the UI thread paints it with `egui::Painter` every frame, culled to the visible region |
| Per-frame CPU | none (textures) | ~0.1-0.3 ms |
| Memory (2600 pt newsletter, 1.25x) | ~68 MB (canvas + tile copy + `ColorImage` + GPU texture) | display list (~1000 commands) + glyph atlas |
| Release binary | +1.32 MiB (10.3%): 13,406,208 vs 12,021,248 bytes | -- |
| Code | `PixbufEngine` in `lib.rs` | `painter.rs`, `fonts.rs` |

Numbers for a page: `RUST_LOG=egui_litehtml_webview=debug` logs each render's
phases, and

```text
cargo test -p esmail --test render_fixtures --release bench_fixtures -- --nocapture --include-ignored
```

times the fixtures across widths. `WebView::last_render_time` reports the newest
render.

## What differed on screen, and why

**The painter is better at**
- **`font-family` lists.** litehtml hands the container the whole list
  (`Arial,Helvetica,sans-serif`). `PixbufContainer` passes it to cosmic-text as
  *one* family name, which matches nothing, so mail written for Arial renders in
  the platform default face (Segoe UI here; a serif at `font-weight:500`, visibly
  wrong). The painter resolves the list entry by entry with CSS weight/style
  matching. `fonts::tests` and `painter::tests` cover this.
- **Fonts that are not the default one.** Beyond plain family names, the painter
  finds the *legacy* names mail actually uses (`Segoe UI Semibold`, `Calibri
  Light`, `Arial Black`: `fontdb` only indexes the typographic family), applies
  `font-weight` to **variable** fonts through their `wght` axis (`Bahnschrift`,
  `Segoe UI Variable`), and reads a trailing weight word on an unknown name as a
  weight of the base family (`Bahnschrift Light`; an uninstalled weight such as
  `Open Sans Light` draws in the nearest installed weight of `Open Sans` rather
  than skipping to the next family in the list, which a browser would do).
  Without these, each of those names fell through to Arial.
- **`text-decoration`.** litehtml only passes underline / line-through as font
  flags and never draws them; `PixbufContainer` never reads them, so **links are
  not underlined**. The painter draws underline, line-through and overline.
- **Rounded borders** (uniform width/colour with `border-radius`) are drawn
  rounded; Pixbuf draws four square edges.
- Symbols and CJK: a missing glyph pulls in a system fallback face
  (`fonts::GLYPH_FALLBACKS`).
- `vh` resolves against a fixed 800 pt viewport instead of the canvas height.

**The painter is worse / different at**
- Overflow clips ignore their `border-radius` (egui clip rects are rectangles).
- A gradient on a rounded box is painted square; conic gradients fall back to
  their first colour (as in Pixbuf). Linear/radial are meshes, exact for
  one-axis multi-stop gradients and approximated on a grid otherwise.
- Corner radii are whole numbers per corner (elliptical radii collapse).
- Border styles are solid / dashed / dotted (double, groove, ... are solid, as
  in Pixbuf).
- No colour emoji (egui draws outlines); egui's bundled monochrome emoji is the
  last fallback.
- Very large images are downscaled to the GPU's texture limit on load.

## Bugs found on the way (in the removed Pixbuf path / litehtml-rs)

These are in `PixbufContainer` upstream (`va1erian/litehtml-rs`), not fixed there; the painter avoids them:

1. **Gradients are flat everywhere but the document origin.** litehtml's
   gradient `start`/`end`/radial `position` are already absolute (the C++ adds
   the layer's origin box); `PixbufContainer` adds `border_box.x/y` again, so
   the gradient line lands outside the box and clamps to one colour. The painter
   does not; regression tests in `painter::tests`.
2. `font-family` lists unresolved (above).
3. `text-decoration` never drawn (above).

Same in both engines, so not the painter's doing: word gaps are occasionally
uneven, at the same places in each (compare the "avec option" line of the
newsletter).

## Measurements (both engines, before the removal)

One Windows machine, `meilleurtaux.eml` (the newsletter fixture), opt-level 3.
Take your own before quoting these.

| | tiny-skia | Painter |
|---|---|---|
| Cold first render incl. worker start + font init, 400 / 700 / 1100 pt | 154 / 172 / 166 ms | 122 / 128 / 131 ms |
| Paint phase (per layout pass) | ~37-41 ms (tiny-skia) | ~0.4 ms (recording) |
| Content height, 400 / 700 / 1100 pt | 2782 / 2738 / 2546 pt | 2781 / 2737 / 2566 pt |
| text_width, cosmic-text vs egui, same font file | -- | 98% of runs within 0.5 pt, mean diff 0.17% |

Where the painter's fixed cost goes on a cold first render of this fixture
(`RUST_LOG=egui_litehtml_webview=debug`, `fonts:` lines): indexing the system
fonts ~30 ms (326 faces, once per worker), registering a face <2 ms, and
rebuilding the measuring `Fonts` 1-4 ms per new font family (~20 ms over the
fixture's 6). The rebuild is not incremental; it could be.

The painter also removes the bitmap path's growth pass, tile copies and the
flatten-onto-white pass.

## Text selection and copy

Shared machinery (#36, `text_runs.rs` / `selection.rs`). While the worker has the
litehtml `Document` alive it records a `TextRunTable` (one run per word: box,
text, per-character x offsets, containing block, forced breaks) and sends it with
each frame; selection, highlight and copy are then plain geometry on the UI
thread, painted by egui over the display list. The engine's width function
supplies the per-character offsets, so they match the fonts that drew the page.
Building the table costs ~17 ms for the fixture's 1020 runs.

## How the painter works (short)

- `painter::PainterContainer` is a `DocumentContainer` that measures with a
  **private** `epaint::Fonts` (litehtml needs widths synchronously; installing
  fonts into the shared `egui::Context` only takes effect next pass) and turns
  every draw callback into a `Cmd` in document coordinates.
- The display list and the font definitions it was measured with go to the UI
  thread. `WebView::ensure_fonts` installs them into the context and **holds
  painting until the context knows every family the list uses** -- egui panics
  on an unknown family. Widths measured on the worker are the widths painted.
- `fonts::FontBook` resolves CSS families with `fontdb`, registers each face
  with egui lazily, and adds fallback faces to every chain when a glyph is
  missing.

## Not done / ideas

- Make the measuring-font rebuild incremental (see the measurements above).
- Find-in-page and "Copy link address" (the `LinkTable` has the rectangles).
