# Handoff

Read this before touching anything. [PLAN.md](PLAN.md) has the design and the
open-work list; this is how to work here.

## Environment

Work in your session's git worktree (`git worktree list`); never `cd` to the
parent repo. Worktree paths are long, which breaks `link.exe`/`cl.exe` in a
deep `target/`; use a short `CARGO_TARGET_DIR` (docs/PERFORMANCE.md, "Windows
pitfalls").

```bash
cargo check --workspace     # ~2s warm
cargo test --workspace      # ~5s warm; run the whole workspace
cargo build --bin esmail
```

The integration suite needs `mail-mock-server`'s test CA trusted once per
machine and `ESMAIL_TEST_CA_TRUSTED=1` set, otherwise it skips with a note
(see `crates/mail-mock-server/README.md`). CI runs with `--include-ignored`, so
an `#[ignore]`d test must be a harmless no-op when it lacks its input.

Files on disk may have CRLF line endings; prefer the Edit tool over scripts
that match multi-line strings.

## Layout

```
crates/egui-litehtml-webview/src/lib.rs   the webview widget (render thread)
crates/esmail/src/main.rs                 the app / UI
crates/esmail/src/{imap,idle_watch,smtp,compose,db,render,config,...}.rs
crates/esmail/tests/render_fixtures.rs    render conformance + timing on real mail
crates/esmail/tests/fixtures/             redacted .eml test cases (+ README)
crates/esmail/tests/imap_smtp_integration.rs   drives the app against the mock server
crates/mail-mock-server/                  in-process IMAP+SMTP server
docs/PERFORMANCE.md, tools/render-profiler/    measuring and profiling a render
```

## Verify visual work by looking at it

`cargo check` passing says nothing about rendering or input. A change once
compiled, passed every test and still silently broke page layout; only
comparing screenshots caught it.

```bash
ESMAIL_PREVIEW=demo ESMAIL_SCREENSHOT="$PWD/shot.png" ESMAIL_SCREENSHOT_FRAMES=90 \
  ./target/debug/esmail.exe
```

Then open `shot.png` with the Read tool. The app renders one page full-window
with no account, captures, and exits.

- `ESMAIL_PREVIEW` takes `demo`, an HTML file, an `.eml`, or a URL. Without it
  you get the login screen, which does not draw the webview.
- `ESMAIL_SCREENSHOT_FRAMES` counts frames after the webview finished
  rendering (it renders on a worker thread).
- F12 dumps a screenshot in a normal run. `shot-*.png` and
  `esmail-screenshot-*.png` are gitignored.
- Take a screenshot before and after any change to `show()` or sizing.

## Things worth knowing

- `egui::Panel::top` is current; `TopBottomPanel` is deprecated.
- Link clicks and hit tests resolve on the render thread, a frame or more after
  the click.
- The mock server only implements the IMAP/SMTP commands the tests needed
  (see `imap_server.rs`'s module doc). If your change sends a new command or
  fetch shape, extend the mock server first.
- An account normally holds three IMAP connections (primary, body worker,
  IDLE); that is intended.
- Redact fixtures before committing them (fixtures README).

## Working agreement

- One phase per commit, with a message explaining why.
- Verify before claiming: run the command, look at the screenshot.
- Say what you did not do; do not quietly narrow scope.
- If a plan item turns out wrong, fix the plan in the same commit.
- The webview crate stays internal (not published).

## State

Check with `cargo build --workspace` and `cargo test --workspace --
--include-ignored` (with `ESMAIL_TEST_CA_TRUSTED=1`). Last full run
(2026-09-19): 176 tests passing.

## #36: text selection and copy (mostly landed)

Design: while the render worker has the `Document` alive it records a
`TextRunTable` (one run per word: box, text, per-character x offsets, block,
forced breaks) and ships it with each frame (`text_runs.rs`). Everything else
is plain geometry on the UI thread (`selection.rs`, and `WebView::interact` in
the webview `lib.rs`): no `Document`, no per-move layout. See the module docs
of those files and the issue.

Landed: drag select (a drag that starts on a link selects; a plain click still
opens it), double-click word, triple-click paragraph, shift-click extend,
highlight painted by egui over the tiles, I-beam cursor over text, auto-scroll
while dragging past the top/bottom edge, Ctrl+C / Ctrl+A only while the view has
egui focus (so the search box and compose fields keep theirs), a Copy / Select
all context menu, and the selection surviving a re-layout of the same text.
Copies read as paragraphs with blank lines, list items and rows on their own
lines, and wrapped lines joined; verified on the Meilleurtaux fixture.

Things learned that are not obvious from the code:

- litehtml lays "preheader" text (hidden with `font-size:1px`, not `display:none`)
  out as a ~2 pt box, so runs shorter than 4 pt are dropped.
- Whitespace between tags arrives as runs with raw `"
"`/nbsp text and stray
  boxes at the left margin; run text is normalised to plain spaces and blank
  runs are never highlighted.
- A `<br>` is a childless element with a 0x0 box and no inline boxes; a newline
  in `<pre>` is a zero-width text element. Geometry alone cannot tell a `<br>`
  from a soft wrap, so forced breaks are counted at collection time.
- egui reports a drag only after the pointer has moved a few points; the
  anchor must come from `press_origin()`, not the position at `drag_started`.
- Building the table costs ~26 ms on a ~220 ms release render of the
  Meilleurtaux fixture (per-character measuring); it shrinks with #28.

Not done: hand cursor over links and "Copy link address" (need link rectangles,
the same table work as #27), keeping a selection across a re-layout that
changes the words (only identical text is kept), and no manual check on a real
window with a real mouse yet (the tests drive synthetic egui input; a
screenshot with everything selected looked right).
