# Plan

Current design and roadmap for esmail, an IMAP/SMTP mail client built on
`egui`. [HANDOFF.md](HANDOFF.md) is the how-to-work guide; this file is what
exists, why, and what is left. Open work is tracked as GitHub issues
(`va1erian/esmail`).

## Architecture

Three crates in one workspace:

- **`crates/egui-litehtml-webview`** — an egui widget that renders HTML/CSS
  with [litehtml](https://github.com/va1erian/litehtml-rs) (layout painted with
  egui's own painter, pure CPU). JS-less by design: no legitimate mail client
  executes JS in email, and skipping a JS engine keeps the release binary small
  (~11.5 MiB).
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

Each `WebView` owns a render thread holding the (`!Send`) painter engine.
The UI thread sends render jobs and paints the finished display lists.

- Jobs carry an id; superseded jobs are dropped or abandoned between stages and
  the UI ignores frames that are not the newest. `load()` invalidates
  in-flight work.
- Remote images are fetched up to eight at a time, each with a timeout; a
  text-only frame is sent first when images are involved.
- `WebViewHandler` is `Send + Sync` with `&self` methods.
- A frame is a display list replayed by egui's painter every frame, culled to
  the visible region, so tall messages cost only what is on screen.
- A litehtml `Document` borrows the container and cannot be stored, so it is
  built, used and dropped inside one worker call. Text selection works from a
  `TextRunTable` recorded during the render pass and sent with each frame (word
  boxes, per-character offsets, block and forced-break info): selection,
  highlight and copy are geometry on the UI thread. Links work the same way: a
  `LinkTable` (`href` + per-line, block and image rectangles of every anchor)
  answers clicks and the hand cursor with a point-in-rectangle lookup (#27).
- Layout speed depends on litehtml-rs `master` (table-cell measurement
  memoization; without it layout is exponential in table nesting depth). Do
  not pin `Cargo.toml` to an older commit.
- Measuring: [docs/PERFORMANCE.md](docs/PERFORMANCE.md). Real-world test mail lives in
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
- **Auth:** password / app-password, plus "Sign in with Google" for Gmail:
  OAuth2 (PKCE, loopback redirect) and SASL `XOAUTH2` over IMAP, IDLE and
  SMTP, with the refresh token in the OS keyring (`oauth.rs`, `auth.rs`).
  It needs an OAuth client id the user registers themselves and enters under
  Settings (or env vars / `config.toml`, see README). Outlook
  still needs an app password; Google refresh tokens are not revoked on
  "forget account".

## Open work

| # | What |
|---|---|
| #36 | Text selection and copy: landed except "Copy link address"; see HANDOFF.md |
| #35 | Multiple accounts in one session, new-mail watching for each |
| #34 | Compose in a dedicated native window |
| #32 | Umbrella: render-time breakdown and the path to sub-second |
| #31 | Optimize dependencies in the dev profile (~15x faster dev renders) |
| #30 | litehtml-rs: paint fast paths |
| #29 | litehtml-rs: `draw_text` bypasses the glyph cache |
| #28 | litehtml-rs: cache text widths in `text_width` |

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
