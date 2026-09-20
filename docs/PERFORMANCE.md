# Measuring render performance

How the render-performance work in issues #27-#32 was done, so it can be
repeated: on the same message after a change, on a new problem email, or on a
different machine. Everything here is about **one render of a message body**:
parse -> layout -> paint (litehtml), plus what `esmail` does around it.

This document measures the default **Pixbuf** backend (tiny-skia); for the
egui-painter backend and how the two compare, see [RENDERERS.md](RENDERERS.md).

Reference numbers are at the [end](#reference-numbers-meilleurtauxeml); compare
against those, on the same machine, before believing a change helped.

## 0. Vocabulary

A render of one message is three phases (the webview logs them separately):

| Phase | What it is | Where the code is |
|---|---|---|
| **parse** | `Document::from_html`: HTML + CSS parsing, style resolution, and measuring every text node | litehtml (C++) calling back into litehtml-rs `text_width` |
| **layout** | `Document::render`: block / table / inline layout | litehtml (C++) |
| **paint** | `Document::draw`: backgrounds, borders, text, images | litehtml-rs `pixbuf.rs` (tiny-skia, cosmic-text, swash) |

Also: **init** is `PixbufContainer::new` loading system fonts, once per worker
thread. **Cold** = the first document a fresh container renders (empty font and
glyph caches). **Warm** = a later document in the same session.

## 1. The quick check: the in-repo benchmark

Fixtures live in `crates/esmail/tests/fixtures/` (see the README there for how
to add one; redact it first, they are public).

```text
RUST_LOG=egui_litehtml_webview=debug \
  cargo test -p esmail --test render_fixtures --release -- --nocapture --include-ignored
```

- prints wall-clock time per fixture and width (400 / 700 / 1100 pt), which
  includes starting the worker thread and loading fonts;
- with `RUST_LOG` set, also prints a `layout_and_draw: parse=.. render(layout)=.. draw(paint)=..`
  line for every pass and a `render job N: total=..` summary.

The same log lines come from the real app, which is the other quick check:

```text
RUST_LOG=egui_litehtml_webview=debug ESMAIL_PREVIEW=crates/esmail/tests/fixtures/meilleurtaux.eml cargo run -p esmail
```

(add `ESMAIL_SCREENSHOT=out.png` to capture once rendering has finished and exit;
`ESMAIL_PREVIEW` also takes an `.html` file or `demo`). Nothing to install.

Without `--release` you are measuring an unoptimized build. Dev builds render
the fixture roughly 15x slower than release (see §3), so always say which you
measured.

## 2. Where inside a render the time goes: `tools/render-profiler`

When "parse is slow" is not enough and you need to know *which function*.

### What it is

`tools/render-profiler` is a small standalone Rust program (its own workspace,
Windows / MSVC / x86_64 only). It renders an HTML file on a worker thread the
way `egui-litehtml-webview`'s worker does, several times with one reused
`PixbufContainer`, while a second thread **suspends the render thread about
every 0.7 ms, walks its stack with dbghelp (`StackWalk64`), and resumes it**.
Each sample is tagged with the phase the worker was in. It then prints, per
phase, the top functions by *self* time (the top frame) and *inclusive* time
(anywhere in the stack). Because dbghelp reads the PDB, **C++ (litehtml) and
Rust frames both resolve**, including cosmic-text / rustybuzz / swash / tiny-skia.

It exists because the setup it was written on had no admin rights (ETW-based
profilers such as samply and WPR need them) and no debugger. If you do have one
of those, use it; this is the no-privileges fallback.

### Running it

1. Dump the HTML the webview is actually given (sanitized, after `render_message`)
   for each fixture into a directory:

   ```text
   mkdir C:\prof\dump
   ESMAIL_DUMP_DIR=C:/prof/dump cargo test -p esmail --test render_fixtures dump_fixtures_as_html -- --ignored
   ```

2. Build the profiler in release **with a short target directory** (see
   [Windows pitfalls](#windows-pitfalls)) and pick the optimization level to
   profile:

   ```text
   cd tools/render-profiler
   CARGO_TARGET_DIR=C:/prof/tool cargo build --release --config 'profile.release.opt-level=3'
   ```

3. Run it:

   ```text
   C:/prof/tool/release/render-profiler.exe C:/prof/dump/meilleurtaux.html 1000 25 1.25
   ```

   Arguments: `<html> [width-pt=1000] [iterations=3] [scale=1.25]`. Use **25+
   iterations** for a profile (a few hundred samples per phase; the per-iteration
   `iter N:` lines still show each pass's timing), 3 if you only want timings.
   Iteration 0 is the cold case, the rest are warm.

### Reading the output

```text
init container (font system): 34ms
iter 0: parse 145ms  layout 128ms  paint 73ms  total 346ms  (content height 2587)
...
symbols: ok=1 symtype=3 (3 = PDB loaded, 0 = none) pdb="...render_profiler.pdb"

=== phase parse: 1590 samples ===
-- SELF (top frame), top 28
 12.6%  RtlAllocateHeap
  8.9%  ttf_parser::ggg::layout_table::impl$7::parse
-- INCLUSIVE (anywhere in the stack), top 28
 63.0%  litehtml::cb_text_width
 ...
```

- **Check `symtype=3` first.** `0` means no symbols; every frame is then a raw
  address (`?7ff6...`). See the pitfalls.
- **Inclusive** answers "which subsystem is expensive" (`cb_text_width` = 63% of
  parse). **Self** answers "which leaf is hot" (heap allocation, a parser). Read
  inclusive first, then self inside the subtree that matters.
- Percentages are shares *of that phase's samples*, not of the whole render.
  Multiply by the phase time to get milliseconds.
- Frames are attributed after inlining, so an inlined helper shows up under its
  caller. The `idle` phase is dropping the `Document`.
- Sampling noise: with ~1000 samples a phase, differences under ~2-3% are noise.

### Caveats and known limits

- **Possible deadlock, by design of the technique.** If the render thread is
  suspended while it holds the process heap lock and the sampler then allocates,
  the process hangs. The sampler avoids allocating while the target is
  suspended, and it has not hung in practice, but if a run hangs at the start
  (dbghelp loading a module for the first time), kill it and run again.
- Windows / MSVC / x86_64 only. dbghelp needs the `.pdb` next to the `.exe`; the
  tool tells dbghelp to look in the exe's own directory.
- It measures litehtml + litehtml-rs, not `esmail`'s own code around a render
  (sanitizing, tiling, texture upload); for those use `RUST_LOG` timing or add a
  timer. The container is reused across iterations exactly like the app's worker,
  but resize/click/image passes are not modelled; run the phases you care about.

## 3. Comparing build profiles

The biggest finding of the original investigation was that most of the wait was
the *build profile*, not the code. To repeat the comparison, build the profiler
at different optimization levels with `--config` (no manifest edits):

```text
cd tools/render-profiler
# everything at opt-level 3 / z / 0
CARGO_TARGET_DIR=C:/prof/t3 cargo build --release --config 'profile.release.opt-level=3'
CARGO_TARGET_DIR=C:/prof/tz cargo build --release --config 'profile.release.opt-level="z"'
CARGO_TARGET_DIR=C:/prof/t0 cargo build --release --config 'profile.release.opt-level=0'

# opt-level 0, but only the C++ (litehtml-sys) optimized
CARGO_TARGET_DIR=C:/prof/tB cargo build --release --config 'profile.release.opt-level=0' \
  --config 'profile.release.package.litehtml-sys.opt-level=3'
# opt-level 0, but all Rust dependencies optimized and the C++ left at 0
CARGO_TARGET_DIR=C:/prof/tC cargo build --release --config 'profile.release.opt-level=0' \
  --config 'profile.release.package."*".opt-level=3' \
  --config 'profile.release.package.litehtml-sys.opt-level=0'
# opt-level 0, but every dependency (Rust and C++) optimized: what the dev-profile fix does
CARGO_TARGET_DIR=C:/prof/tA cargo build --release --config 'profile.release.opt-level=0' \
  --config 'profile.release.package."*".opt-level=3'
```

then run each `render-profiler.exe` with the same arguments (3 iterations is
enough) and compare the warm `iter 1` / `iter 2` lines.

Things that are easy to get wrong:

- `opt-level` is an **integer for numbers, a quoted string for `s`/`z`**:
  `opt-level=0` but `opt-level="z"`. A quoted `"0"` is rejected.
- The C++ in `litehtml-sys` is compiled by the `cc` crate, which takes its
  optimization level from *that package's* `opt-level`. That is why
  `package.litehtml-sys.opt-level` moves layout time and the Rust-side settings
  do not.
- `package."*"` means every package that is **not a workspace member**. In the
  profiler that is all the dependencies; in the main repo it is everything except
  our own crates, which is exactly what you want for a dev profile.

To check a profile change in the **real workspace** rather than the harness,
edit `Cargo.toml` (for example `[profile.dev.package."*"] opt-level = 3`),
rebuild, and run the app command from §1; compare the `render job` total. That
is how the dev-profile fix in #31 was verified (3.5 s -> 234 ms). A full
dependency rebuild took ~1.5 min.

## 4. Trying an optimization before proposing it

The two litehtml-rs fixes (#28 text-width cache, #29 glyph cache) were measured
before any PR by prototyping in a local copy of the crate:

1. Copy the `litehtml` crate (`src/`, `Cargo.toml`) from
   `~/.cargo/git/checkouts/litehtml-rs-*/<rev>/litehtml/` next to the profiler.
2. In the copy's `Cargo.toml`, replace `litehtml-sys = { path = "../litehtml-sys", ... }`
   with the same dependency as a git dependency
   (`git = "https://github.com/va1erian/litehtml-rs.git"`); the `-sys` crate
   (which carries the C++) is not copied.
3. In the profiler's `Cargo.toml` add
   ```toml
   [patch."https://github.com/va1erian/litehtml-rs.git"]
   litehtml = { path = "litehtml-local" }
   ```
4. Make the change, guard it with an environment variable while experimenting
   (`if std::env::var_os("NO_WIDTH_CACHE").is_some() { old path }`), and run the
   profiler with and without the variable. That gives a same-binary before/after
   and rules out build noise.
5. Do **not** commit the copy; open the PR against litehtml-rs (which is
   consumed by git, so `cargo update -p litehtml` picks it up once merged).

## Windows pitfalls

All of these were hit for real:

- **Long paths break the toolchain.** `link.exe` failed with `LNK1104` and `cl.exe`
  failed with `C1083 cannot open include file` when the checkout and target
  directory were deep (the Claude worktree paths are ~150 characters). Use a
  short `CARGO_TARGET_DIR` (`C:/prof/...`), and for the C++ submodule build keep
  the whole checkout at a short path too.
- **`symtype=0` / `?7ff6...` frames.** The PDB was not found or not loaded. The
  `.pdb` must sit next to the `.exe` (it does after a normal `cargo build`), and
  the build must have `debug = true` (the tool's `[profile.release]` sets it,
  and the C++ is compiled by `cc`, which follows the profile's debug setting).
  The first version of the tool got `symtype=0` with a null search path and
  `SYMOPT_DEFERRED_LOADS`; it started working after passing the exe's directory
  as the search path and dropping the deferred-load flag. Both were changed at
  once, so which one mattered was not isolated; the tool now does both.
- **Long runs are quiet.** Layout and parse of a pathological document can take
  minutes with no output (the original 17-deep-table hang). Run under `timeout`,
  and remember the app prints nothing until the first pass finishes.

## Reference numbers (`meilleurtaux.eml`)

One machine (Windows 11, MSVC), `tools/render-profiler`, fixture HTML at 1000 pt,
scale 1.25, warm iterations; expect about +-10% run to run. Take new numbers on
your machine before comparing.

| Build | parse | layout | paint | total |
|---|---|---|---|---|
| everything opt-level 3 | ~130 ms | ~125 ms | ~75 ms | **~330 ms** |
| everything `z` (the release profile) | ~235 ms | ~115 ms | ~125 ms | **~475 ms** |
| everything 0 (dev) | ~4300 ms | ~480 ms | ~1430 ms | **~6200 ms** |
| 0, only C++ optimized | ~4900 ms | ~120 ms | ~1700 ms | ~6700 ms |
| 0, only Rust deps optimized | ~160 ms | ~510 ms | ~72 ms | ~745 ms |
| 0, all deps optimized (`[profile.dev.package."*"] opt-level = 3`) | ~125 ms | ~115 ms | ~78 ms | **~320 ms** |

In the real app (dev build, fixture preview): render job 3.5 s before the
dev-profile change, 234 ms after. One-time font init: 33-63 ms.

Profile shares at opt-level 3 (of each phase's samples): parse 63% in
`cb_text_width` -> cosmic-text shaping (8,602 calls, ~408 distinct texts);
layout ~33% in heap allocation and ~9% in `go_inside_inline::select`; paint 65%
in `draw_text` (38% glyph rasterization because the glyph cache is bypassed,
16.8% TrueType hinting), 32% in `draw_solid_fill`, 12% in `ensure_clip_mask`.
Details and proposed fixes: #32, #28, #29, #30.
