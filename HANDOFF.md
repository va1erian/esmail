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

## In progress: #36, text selection and copy

Full design in the issue. Chosen approach (its option A): during the render
pass, while the `Document` is alive, record a table of text runs (rect in
document points, text, font info) and ship it to the UI thread with the frame.
Hit-testing, drag, highlight and copy then work from that table with no
`Document`. Link rectangles (#27) share the same plumbing.
Increments: 1. run table; 2. click/drag/double-click + overlay highlight;
3. copy, select-all, focus model; 4. auto-scroll, hover cursor; 5. keep the
selection across re-layout. See git log for which have landed.
