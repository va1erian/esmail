# Plan

Current design and roadmap for esmail, an IMAP/SMTP mail client with a native
Windows frontend (the egui frontend moved to
[va1erian/esmail-egui](https://github.com/va1erian/esmail-egui)).
[HANDOFF.md](HANDOFF.md) is the how-to-work guide; this file is what exists,
why, and what is left. Open work is tracked as GitHub issues
(`va1erian/esmail`).

## Architecture

Four crates in one workspace:

- **`crates/esmail`** — the egui-free core: `imap.rs` (IMAP actor plus a
  second body-worker connection), `idle_watch.rs` (IMAP IDLE push), `smtp.rs`,
  `compose.rs`, `db.rs` (SQLite cache + FTS5), `search_query.rs`, `render.rs`
  (parse -> sanitize with `ammonia` -> resolve `cid:`), `config.rs`/`secrets.rs`
  (TOML config, passwords in the OS keyring), `app/` (the frontend-agnostic
  `AppCore` model), `notify.rs` + `platform/` (Windows toasts + tray),
  `uninstall.rs` (the installer's `--purge-data`), and `emoji.rs` (Twemoji
  segmentation; the artwork is decoded in the frontend).
- **`crates/esmail-win32`** — the native Windows frontend on
  [win32ui](https://github.com/va1erian/win32ui): a 3-pane window, compose,
  Drafts/Outbox, accounts/settings, tray + notifications, in `src/app/` over
  the reusable widgets and `core_glue/` adapters.
- **`crates/litehtml-view-d2d`** — the message-body webview: [litehtml](https://github.com/va1erian/litehtml-rs)
  layout painted with Direct2D/DirectWrite through win32ui. JS-less by design:
  no legitimate mail client executes JS in email.
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
- A frame is a display list replayed with Direct2D/DirectWrite through win32ui,
  culled to the visible region, so tall messages cost only what is on screen.
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
  images" (or "Always load from <sender>", kept in `config.toml`), attachment
  chips (save/open), inline `style=` via a property allowlist, Export... to
  `.eml`, flags, delete/archive, collapsible mailbox tree with special-use
  folders (fold state kept in `esmail-win32`'s `win32-settings.toml`),
  multi-select, keyboard shortcuts. The message list draws each row by hand
  (`message_list/` in `esmail-win32`): unread rows get an accent bar and a
  strong sender, read rows are dimmed.
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
| #32 | Umbrella: render-time breakdown and the path to sub-second |
| #50 | Google sign-in follow-ups: client secret storage, revoke on forget, other providers (7-day expiry warning landed as #71) |
| #54 | Sign the Windows executable and installer to avoid the SmartScreen warning |
| #60 | Sync/search: offline mode, UID paging, indexing, server-side `UID SEARCH` (unapplied filters landed as #69) |
| #61 | Reading: partial (`BODYSTRUCTURE`) fetch, batched flag/move; IDLE staying INBOX-only is a deliberate choice for now (search-cache attachments landed as #68) |
| #63 | Notifications: real AUMID, settings, verification on a real machine (click-to-open landed) |
| #83-#89 | Rich-text compose series — deferred, not required for basic use |

Landed since the table was last trimmed: the native Windows frontend reached
parity with the egui one across iterations 1-7 (toolbar, login/accounts,
compose, system theme, attachments, links, multi-account, tray/notifications,
trusted senders, Drafts/Outbox, auth banners, Settings, Download All, folder
folds), and the egui frontend moved to its own repository.

## Risks

- `panic = "abort"` in release: an `expect` anywhere kills the app.
- Windows long paths break `link.exe`/`cl.exe` in deep worktree `target/`
  directories; use a short `CARGO_TARGET_DIR` (see docs/PERFORMANCE.md).
