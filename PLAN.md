# Plan

Current design and roadmap for esmail, an IMAP/SMTP mail client built on
`egui`. [HANDOFF.md](HANDOFF.md) is the how-to-work guide; this file is what
exists, why, and what is left. Open work is tracked as GitHub issues
(`va1erian/esmail`).

## Architecture

Three crates in one workspace:

- **`crates/egui-litehtml-webview`** — an egui widget that renders HTML/CSS
  with [litehtml](https://github.com/va1erian/litehtml-rs) (`pixbuf` backend,
  pure CPU). JS-less by design: no legitimate mail client executes JS in
  email, and skipping a JS engine keeps the release binary small (~12 MiB).
  One `WebViewHost`, any number of `WebView`s.
- **`crates/esmail`** — the app: `main.rs` (UI), `imap.rs` (IMAP actor plus a
  second body-worker connection), `idle_watch.rs` (IMAP IDLE push), `smtp.rs`,
  `compose.rs`, `db.rs` (SQLite cache + FTS5), `search_query.rs`, `render.rs`
  (parse -> sanitize with `ammonia` -> resolve `cid:`), `config.rs`/`secrets.rs`
  (TOML config, passwords in the OS keyring), `notify.rs`/`tray.rs` (Windows
  toasts + tray).
- **`crates/mail-mock-server`** — an in-process IMAP + SMTP server with a
  throwaway TLS CA, used by `crates/esmail/tests/imap_smtp_integration.rs`.

### Webview rendering

Each `WebView` owns a render thread holding the (`!Send`) `PixbufContainer`.
The UI thread sends jobs (render, hit test) and uploads finished frames.

- Jobs carry an id; superseded jobs are dropped or abandoned between stages and
  the UI ignores frames that are not the newest. `load()` invalidates
  in-flight work.
- Remote images are fetched up to eight at a time, each with a timeout; a
  text-only frame is sent first when images are involved.
- `WebViewHandler` is `Send + Sync` with `&self` methods.
- The frame is cut into tiles no larger than the GPU's `max_texture_side`, so
  tall messages display.
- A litehtml `Document` borrows the container and cannot be stored, so it is
  built, used and dropped inside one worker call. Link hit tests therefore
  re-layout (#27), and text selection works from a `TextRunTable` recorded
  during the render pass and sent with each frame (word boxes, per-character
  offsets, block and forced-break info): selection, highlight and copy are
  geometry on the UI thread.
- Every draw pass starts from a cleared canvas; the canvas only grows (seeded
  at 4000 px) because a taller page forces a second parse + layout.
- Layout speed depends on litehtml-rs `master` (table-cell measurement
  memoization; without it layout is exponential in table nesting depth) and
  its `draw_image` fix. Do not pin `Cargo.toml` to an older commit.
- Measuring and profiling: [docs/PERFORMANCE.md](docs/PERFORMANCE.md),
  `tools/render-profiler`. Real-world test mail lives in
  `crates/esmail/tests/fixtures/` (redact first; see the README there).

### Mail client

- **Sessions:** `ImapActor` owns the primary session (headers, mailboxes); a
  second worker connection serves `FetchBody`/`BulkDownload`; a third does
  IDLE. Requests carry a `req_id` and stale replies are dropped. Errors clear
  the session and reconnect with backoff.
- **Cache:** `mailboxes`/`messages`/`bodies` tables, LRU-capped bodies, FTS5
  search behind a small query DSL. `sync_decision` (UIDVALIDITY/UIDNEXT)
  drives incremental header fetch.
- **Reading:** sanitized HTML, remote content blocked until "Load remote
  images", attachment chips (save/open), inline `style=` via a property
  allowlist, Export... to `.eml`, flags, delete/archive, mailbox tree with
  special-use folders, multi-select, keyboard shortcuts.
- **Composing:** plain-text compose with Reply/Reply All/Forward, attachments,
  SMTP via `lettre`, `APPEND` to the server's Sent folder.
- **Polish:** error banners, dark/light/system theme, window geometry
  persistence, provider-table first-run autofill.
- **Notifications (Windows only):** tray icon, new-mail toasts, polled every
  60 s and woken immediately by IDLE pushes.
- **Auth:** password / app-password only. OAuth2 is out of scope, so Gmail and
  Outlook work mainly with app passwords.

## Open work

| # | What |
|---|---|
| #36 | Text selection and copy: landed except link cursor / "Copy link address" (needs the #27 link table); see HANDOFF.md |
| #35 | Multiple accounts in one session, new-mail watching for each |
| #34 | Compose in a dedicated native window |
| #32 | Umbrella: render-time breakdown and the path to sub-second |
| #31 | Optimize dependencies in the dev profile (~15x faster dev renders) |
| #30 | litehtml-rs: paint fast paths |
| #29 | litehtml-rs: `draw_text` bypasses the glyph cache |
| #28 | litehtml-rs: cache text widths in `text_width` |
| #27 | Link hit test re-lays out the page (needs the same run table as #36) |

Not ticketed:

- **Sync/search:** header list always re-fetches (no offline mode); no UID
  paging; single-opened messages are not indexed; no server-side `UID SEARCH`;
  `since:`/`before:`/`is:unread`/`has:attachment` parse but are not applied.
- **Reading:** whole-message `RFC822` fetches (no `BODYSTRUCTURE`/partial
  fetch); attachments missing for messages opened from the search cache; no
  per-sender "always load images"; mailbox tree not collapsible; bulk
  flag/move use one round trip per message; IDLE is INBOX-only and the header
  list does not update live.
- **Compose:** no drafts, retry queue, rich text or recipient autocomplete;
  SMTP StartTls not selectable in the UI; Reply-All Ccs only the first address.
- **Notifications:** no click-to-open, no real AUMID, no settings, never
  verified on a real machine.
- **Polish:** per-operation progress; real first-run wizard; window saved on an
  unplugged monitor can reopen off-screen; theme toggle saves config on the UI
  thread.
- **Webview:** the decoded-image cache never evicts.

## Risks

- `panic = "abort"` in release: an `expect` anywhere kills the app.
- Windows long paths break `link.exe`/`cl.exe` in deep worktree `target/`
  directories; use a short `CARGO_TARGET_DIR` (see docs/PERFORMANCE.md).
