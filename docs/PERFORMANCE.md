# Measuring render performance

How the render-performance work in issues #27-#32 was done, so it can be
repeated: on the same message after a change, on a new problem email, or on a
different machine. Everything here is about **one render of a message body**:
layout -> record (litehtml + egui's painter), plus what `esmail` does around it.

**History:** this document was first written against the tiny-skia renderer
(`PixbufContainer`) and a standalone sampling profiler (`tools/render-profiler`);
both are gone (see [RENDERERS.md](RENDERERS.md) for why). The profiler, its
usage notes, the build-profile comparison recipe and the per-function profile
shares are in git history (the last commit that has them is `dbb65f9`, also on the
branch `backup/pixbuf-and-painter-renderers`). What is kept here is what still
applies to the current renderer.

## 0. Vocabulary

A render of one message, as the webview logs it:

| Phase | What it is | Where the code is |
|---|---|---|
| **layout** | `Document::render`: block / table / inline layout, calling back into `PainterContainer::text_width` | litehtml (C++) |
| **record** | `Document::draw`: every draw callback becomes a display-list command | `painter.rs` |
| **text_runs** | walking the laid-out document for the selection table | `text_runs.rs` |

`Document::from_html` (HTML + CSS parsing, style resolution) is not logged as a
phase of its own; it is inside the `render job N: total=..` figure but not in
`layout=`, `record=` or `text_runs=`.

Painting the display list is per frame on the UI thread and is not part of a
render job (~0.1-0.3 ms). **Init** is the first job of a worker: indexing the
system fonts (~30 ms, once per worker thread). **Cold** = the first document a
fresh worker renders (empty font and measurement caches). **Warm** = a later
document in the same session.

## 1. The quick check: the in-repo benchmark

Fixtures live in `crates/esmail/tests/fixtures/` (see the README there for how
to add one; redact it first, they are public).

```text
RUST_LOG=egui_litehtml_webview=debug \
  cargo test -p esmail --test render_fixtures --release -- --nocapture --include-ignored
```

- prints wall-clock time per fixture and width (400 / 700 / 1100 pt), which
  includes starting the worker thread and loading fonts;
- with `RUST_LOG` set, also prints a `painter draw_pass: layout=.. record=..
  text_runs=..` line for every pass and a `render job N: total=..` summary.

The same log lines come from the real app, which is the other quick check:

```text
RUST_LOG=egui_litehtml_webview=debug ESMAIL_PREVIEW=crates/esmail/tests/fixtures/meilleurtaux.eml cargo run -p esmail
```

(add `ESMAIL_SCREENSHOT=out.png` to capture once rendering has finished and exit;
`ESMAIL_PREVIEW` also takes an `.html` file or `demo`). Nothing to install.

Without `--release` you are measuring an unoptimized build. Dev builds were
roughly 15x slower than release when this was first measured (see §2), so always
say which you measured.

## 2. Build profiles matter more than the code

The biggest finding of the original investigation was that most of the wait was
the *build profile*, not the code: the vendored C++ litehtml and the Rust
dependencies were compiled at opt-level 0 in a dev build. The fix proposed in
#31 is `[profile.dev.package."*"] opt-level = 3`, which took the fixture's render
job from 3.5 s to 234 ms in the real app. (It is not in this workspace's
`Cargo.toml` as of this writing, so a plain `cargo run` is still the slow case.)

To check a profile change, edit `Cargo.toml`, rebuild, and run the app command
from §1; compare the `render job` total. A full dependency rebuild takes about
1.5 minutes. Things that are easy to get wrong:

- `opt-level` is an **integer for numbers, a quoted string for `s`/`z`**:
  `opt-level=0` but `opt-level="z"`. A quoted `"0"` is rejected.
- The C++ in `litehtml-sys` is compiled by the `cc` crate, which takes its
  optimization level from *that package's* `opt-level`. That is why
  `package.litehtml-sys.opt-level` moves layout time and the Rust-side settings
  do not.
- `package."*"` means every package that is **not a workspace member**: exactly
  the dependencies, which is what a dev profile wants.

## 3. Trying an optimization before proposing it

The litehtml-rs fixes (#28 text-width cache, #29 glyph cache) were measured
before any PR by prototyping in a local copy of the crate, with an environment
variable guarding the new code path so one binary gives a same-build
before/after. Do not commit the copy; open the PR against litehtml-rs (which is
consumed by git, so `cargo update -p litehtml` picks it up once merged).

## Windows pitfalls

- **Long paths break the toolchain.** `link.exe` failed with `LNK1104` and `cl.exe`
  failed with `C1083 cannot open include file` when the checkout and target
  directory were deep (the Claude worktree paths are ~150 characters). Use a
  short `CARGO_TARGET_DIR`, and for the C++ submodule build keep the whole
  checkout at a short path too.
- **Long runs are quiet.** Layout of a pathological document can take minutes
  with no output (the original 17-deep-table hang). Run under `timeout`, and
  remember the app prints nothing until the first pass finishes.

## Reference numbers (`meilleurtaux.eml`), tiny-skia era

Measured with the removed profiler on one machine (Windows 11, MSVC), fixture
HTML at 1000 pt, scale 1.25, warm iterations; about +-10% run to run. They
describe the old renderer (its *paint* phase was tiny-skia rasterization), so
use them for the build-profile comparison, not as a target for the current one;
for the painter's own numbers see [RENDERERS.md](RENDERERS.md).

| Build | parse | layout | paint | total |
|---|---|---|---|---|
| everything opt-level 3 | ~130 ms | ~125 ms | ~75 ms | **~330 ms** |
| everything `z` (the release profile) | ~235 ms | ~115 ms | ~125 ms | **~475 ms** |
| everything 0 (dev) | ~4300 ms | ~480 ms | ~1430 ms | **~6200 ms** |
| 0, only C++ optimized | ~4900 ms | ~120 ms | ~1700 ms | ~6700 ms |
| 0, only Rust deps optimized | ~160 ms | ~510 ms | ~72 ms | ~745 ms |
| 0, all deps optimized (`[profile.dev.package."*"] opt-level = 3`) | ~125 ms | ~115 ms | ~78 ms | **~320 ms** |
