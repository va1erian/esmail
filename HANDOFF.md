# Handoff

Read this before touching anything. [PLAN.md](PLAN.md) has the design and the
open-work list; this is how to work here.

## Environment

Work in your session's git worktree (`git worktree list`); never `cd` to the
parent repo. Worktree paths can be long, which breaks `link.exe`/`cl.exe` in a
deep `target/`; use a short `CARGO_TARGET_DIR` (and keep the checkout itself at
a short path for the litehtml C++ build).

```bash
cargo check --workspace     # ~2s warm
cargo test --workspace      # run the whole workspace
cargo build --release -p esmail-win32
```

The integration suite needs `mail-mock-server`'s test CA trusted once per
machine and `ESMAIL_TEST_CA_TRUSTED=1` set, otherwise it skips with a note
(see `crates/mail-mock-server/README.md`). CI runs with `--include-ignored`, so
an `#[ignore]`d test must be a harmless no-op when it lacks its input.

Files on disk may have CRLF line endings; prefer the Edit tool over scripts
that match multi-line strings.

## Layout

```
crates/esmail/src/           the egui-free core: imap.rs, idle_watch.rs, smtp.rs,
                             compose.rs, db.rs, search_query.rs, render.rs,
                             config.rs/secrets.rs, notify.rs + platform/, uninstall.rs
crates/esmail/src/app/       AppCore: the frontend-agnostic model (used by esmail-egui)
crates/esmail-win32/src/app/ the native Windows frontend
crates/esmail-win32/src/core_glue/  adapters over the core for the win32 app
crates/esmail-win32/src/message_list/  the reusable, virtualized message list widget
crates/litehtml-view-d2d/    the Direct2D/DirectWrite message-body webview
crates/mail-mock-server/     in-process IMAP + SMTP server for tests
crates/esmail/tests/fixtures/  redacted .eml test cases (+ README)
```

The egui/eframe frontend is **not** in this repository any more; it lives at
[va1erian/esmail-egui](https://github.com/va1erian/esmail-egui), built on this
core. A message-body render is measured with `litehtml-view-d2d`'s own example.

## Verify visual work by looking at it

`cargo check` passing says nothing about rendering or input. A change once
compiled, passed every test and still silently broke page layout; only
comparing screenshots caught it.

The native frontend has a screenshot mode:

```powershell
cargo run -p esmail-win32 -- --screenshot shot.png --folder INBOX
```

It opens, waits for the first list content (and any requested window), writes
`shot.png` and exits. `--theme light|dark`, `--select ROW`, `--accounts`,
`--settings`, `--show drafts|outbox` and `--compose new|reply|reply-all|forward`
shape what is captured. Open the PNG with the Read tool before and after any
change to `show()` or sizing. Release builds are a GUI program (no console);
debug builds keep the console for the `--screenshot` timing line.

If UI tests are flaky or would grab focus, win32ui ships a Windows Sandbox
runner (`scripts/sandbox/run.ps1` in the win32ui repository) that runs an app
built on it off the desktop.

## Things worth knowing

- The mock server only implements the IMAP/SMTP commands the tests needed
  (see `imap_server.rs`'s module doc). If your change sends a new command or
  fetch shape, extend the mock server first.
- An account normally holds three IMAP connections (primary, body worker,
  IDLE); that is intended.
- `esmail-win32` must stay egui-free (CI checks `cargo tree -p esmail-win32`).
- The window icon is the resource `build.rs` embeds (id 1), installed with
  win32ui's `Ui::set_icon` / `Icon::from_resource`.
- `--quit` and `--purge-data` exist for the installer; `--background` (the
  separate listener process) was removed with the egui frontend.
- Redact fixtures before committing them (fixtures README).

## Working agreement

- One phase per commit, with a message explaining why.
- Verify before claiming: run the command, look at the screenshot.
- Say what you did not do; do not quietly narrow scope.
- If a plan item turns out wrong, fix the plan in the same commit.

## State

Check with `cargo test --workspace` (and, with the test CA trusted,
`cargo test --workspace -- --include-ignored`).
