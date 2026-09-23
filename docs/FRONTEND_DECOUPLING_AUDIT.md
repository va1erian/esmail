# Frontend decoupling audit

What stops a second frontend (native Win32 on win32ui, see
`WIN32_FRONTEND_PLAN.md` section 3) from consuming the esMail core today.
Evidence is `file:line` at `origin/main` 7863f4f plus the `Waker` change
(#96); commands are quoted with their output. Severity: **blocker** (a second
frontend cannot be built or cannot work without it), **important** (works but
duplicates logic or is fragile), **nice**. Size: S under a day, M a few days,
L a week or more.

Headline: the *library* half is nearly clean (it compiles without egui after
three trivial edits, see a.3), but it is not *packaged* that way, so every
dependent drags in eframe, winit and glow. The real work is `main.rs`: about
three quarters of its 4 024 lines are application logic interleaved with egui calls.

## a. Dependency leakage

**a.1** `esmail` is one package with a `[lib]` and a `[[bin]]`
(`crates/esmail/Cargo.toml:9-15`), and the egui crates are unconditional
`[dependencies]` (`Cargo.toml:18-24`: `egui-litehtml-webview`, `eframe`,
`egui`, `egui_glow`, `winit`). Anything that depends on the library gets them
all:

```
$ cargo tree -p esmail -e normal | grep -i -E "egui|eframe|winit|glow|epaint" | sort -u
eframe v0.36.2, egui v0.36.2, egui-winit, egui_glow, egui-litehtml-webview,
epaint v0.36.2, epaint_default_fonts, glow v0.17.0, glutin-winit, winit v0.30.13
$ cargo tree -p esmail -e normal --prefix none | sort -u | wc -l
297   (62 of them exist only because of egui: skrifa, harfrust, glutin,
       vello, litehtml + litehtml-sys (C++), arboard, accesskit, ...)
```

`cargo tree -p esmail-win32` cannot be run: that crate is not on `main` or on
the plan branch yet (#104 is in flight). Any dependent of the `esmail` library
sees exactly the `-p esmail` tree above, so that is the measurement.

**a.2** Lib modules that name egui (`grep -n egui crates/esmail/src/*.rs`,
`main.rs` and the bin-only `compose_ui.rs`, `compose_window.rs`,
`settings.rs`, `window_fit.rs` excluded):

| Module | Line | What | Fix |
|---|---|---|---|
| `emoji.rs` | 107-190 | `paint`, `paint_in_rect`, texture cache on `egui::Context`; the segmentation half (`Segment`s, `twemoji-assets`, `unicode-segmentation`) is pure | split: segmentation stays, drawing moves to the egui side |
| `screenshot.rs` | 57-105 | whole module is `ViewportCommand::Screenshot` driving | move to the egui side (bin-only) |
| `icons.rs` | 27-31 | `impl From<Rgba> for egui::IconData` | move the impl next to its caller in `main.rs` |
| `listener.rs` | 38-41 | `winit` event loop for the window-less tray (real dependency, not egui) | gate behind a `listener` feature, or accept `winit` as the one shared dependency |
| `render.rs`, `view_model.rs`, `imap.rs`, `session.rs`, `watcher.rs`, `config.rs`, `lib.rs` | comments only | none | none |

`config.rs` is already clean: `ThemeMode` deliberately avoids
`egui::ThemePreference` (`config.rs:213-216`); the conversion lives in
`main.rs:687-689` and, duplicated, `main.rs:1796-1798`.

**a.3** Proof. Temporarily removing `egui-litehtml-webview`, `eframe`, `egui`
and `egui_glow` from `Cargo.toml`, deleting `pub mod emoji;` and
`pub mod screenshot;` from `lib.rs` and the `From<Rgba> for egui::IconData`
impl (about 12 lines) makes the library compile:

```
$ cargo check --lib -p esmail            # clean target dir
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 12.43s
$ cargo tree -p esmail -e normal | grep -i -E "egui|eframe|winit|glow|epaint"
└── winit v0.30.13                        # only listener.rs, see above
$ cargo tree -p esmail -e normal --prefix none | sort -u | wc -l
241
```

For comparison a clean `cargo check --lib -p esmail` with egui in the tree
takes about 46 s (30 s for `egui-litehtml-webview` and its litehtml build
script, then 16 s for the rest), 297 crates versus 241 (the 241 figure was
taken with `Cargo.toml` edited, nothing else changed). A `cargo build` of the
egui tree costs far more than `check` does (glow, glutin, vello, litehtml C++).

**a.4** Other things that belong on the shared side but sit in the binary
because the `bin` owns the modules (`main.rs:13-20`): `listener_client.rs`
(pure `esmail::ipc` client, no egui), `config_saver.rs` (a config write
thread, no egui). The `--quit` / `--purge-data` / `--background` /
`--open-account` argument handling is inline in `main()` (`main.rs:3826-3870`)
and `run_gui` (`3873-3932`), so a second frontend would copy it.

| Gap | Severity | Size |
|---|---|---|
| egui deps unconditional in the package the lib shares with the bin | blocker | S-M (make them `optional` behind an `egui-frontend` feature with `required-features` on the bin, or do the crate split, issue 10/10) |
| `emoji.rs` / `screenshot.rs` / `icons.rs` egui halves in the lib | blocker (without them the feature gate cannot compile) | S |
| `listener.rs` pulls `winit` | nice | S |
| `listener_client.rs`, `config_saver.rs`, CLI dispatch stranded in the bin | important | S-M |

## b. Logic trapped in the UI

Every function below is in `crates/esmail/src/main.rs` unless noted. Sizes
are line counts of the function bodies. `impl EsMailApp` blocks: 503-2529
(about 2 030 lines, 75 methods) and 2540-2660 (`handle_tray`,
`open_pending_account`); `impl eframe::App` 2662-3596 (`logic` 20 lines,
`ui` about 900 lines). Ordered by how much they block extraction:

| # | Range | What it mixes | Lines | Blocks step |
|---|---|---|---|---|
| 1 | `ui()` 2693-3596 | draws *and* dispatches: `BulkDownload` at 2759, `DbCommand::Search` at 2820, `ListDrafts`/`ListOutbox` at 3024/3028, `activate`/`fetch_headers`/`open_message` at 3200/3227/3266/3340, plus selection with `egui::Modifiers` at 3289/3319 | ~900 | 5 |
| 2 | `handle_imap_events` 896-1232 | 337 lines of state transitions: connection state, mailbox tree, header pages, body/attachments, progress, bulk-action completion, banners, notification of failures | 337 | 4 |
| 3 | `handle_db_events` 1233-1357, `handle_smtp_events` 1398-1486, `handle_attachment_io` 1968-1993, `handle_oauth_events` (`accounts.rs:213-`) | outbox/draft/search results, Sent copies, retry bookkeeping, banners | ~330 | 4 |
| 4 | Selection and message actions 2052-2342: `open_message`, `action_targets`, `store_flags_on_selection`, `toggle_star_on_selection`, `move_selection`, `archive_selection`, `delete_selection`, `open_search_hit`, `clear_search`, `handle_mark_seen_delay` | pure model logic reading `self.headers`, `selected_uids`, `search_results`; only the entry points are egui | ~290 | 5 |
| 5 | Account lifecycle 1658-1791 and `accounts.rs`: `activate`, `disconnect_account`, `push_banner`, `apply_provider_wizard` (1828), `connect_clicked`, `begin_google_sign_in`, `handle_oauth_events` | session and OAuth orchestration; `accounts.rs` is 331 lines of it and `settings.rs` (550 lines) mixes form drawing with `apply_account_dialog` | ~450 | 4-5 |
| 6 | Compose orchestration 1487-1606, `compose_window.rs` (373 lines) and `compose_ui.rs` (397) | `open_compose`, `close_compose`, `process_compose_windows` (send requests, `SmtpCommand::Send` at 1551 (and 1314 for outbox retries)), `autosave_drafts` 1358, `poll_outbox` 1389, `mark_send_started/finished` 1951-1967; the window is a `show_viewport_deferred` viewport, so the state (`Shared`) is coupled to egui's viewport model | ~400 | 4-5 |
| 7 | Progress and bulk actions 1895-1950 (`set_progress`, `begin_bulk_action`, `advance_bulk_action`) | logic only, but they take `ProgressKind`/`Progress` from `progress.rs` (already lib) | ~55 | 4 |
| 8 | `handle_keyboard_shortcuts` 2343-2425 | reads `egui::Key` and `Modifiers` and calls the actions above directly (see d) | ~85 | 3 |
| 9 | `handle_tray` 2541-2641 | single-instance requests, listener messages, tray/toast muting, close-to-tray, unread tooltip, close confirmation | ~100 | 4 |
| 10 | Drafts / Outbox windows 2426-2530 | `DbCommand::ListDrafts`, `DeleteDraft` (`db.rs:144`), `DeleteOutbox` (`db.rs:127`) etc. issued from `show_*_window` | ~105 | 5 |
| 11 | `EsMailApp::new` 506-895 | builds the runtime-facing wiring (channels, actors, hooks) *and* the egui state (webview, theme, fonts) in one 390-line function | ~390 | 4 |
| 12 | State structs 125-230 (`Banner`, `ProgressView`, `BulkAction`, `ConnState`, `AccountView`, `AttachmentWrite`) and the roughly 270 lines of `EsMailApp` fields (229-502) | plain data, but private to the bin | ~370 | 4 |

Tests: main.rs has 1 unit test (`parse_open_account`), `accounts.rs` 0,
`settings.rs` 0. None of the logic above is covered except through the
`compose_window` tests (8) and the actor integration tests.

## c. UI-thread and threading assumptions

- **Runtime ownership.** `main()` creates `tokio::runtime::Runtime::new()`
  and `block_on(run_gui())` (`main.rs:3869-3870`). Every actor and forwarder
  is then started with a bare `tokio::spawn` that needs an *ambient* runtime:
  `imap.rs:578`, `imap.rs:1268`, `smtp.rs:72`, `session.rs:112`,
  `idle_watch.rs:83`, `listener_client.rs:69,101`, `accounts.rs:185`,
  `main.rs:586,603`. A frontend that does not own a tokio runtime cannot call
  any of these; `db.rs:220` (`DbActor::spawn`) is the only one that needs no
  runtime. The core has to own or be handed a `tokio::runtime::Handle`.
- **Channels drained with `try_recv` in the frame loop**, all on the UI
  thread: `imap_rx` (`main.rs:897`), `db_rx` (1234), `smtp_rx` (1399),
  `attachment_io_rx` (1969, a `std::sync::mpsc`), `listener.try_recv`
  (2562), `toast_click_rx` (2585), `oauth_rx` (`accounts.rs:214`). A
  frontend needs one `pump()` that drains all seven, called when woken.
  The ordering constraint is already documented: `logic()` (2672-2692)
  drains `db` and attachment IO a second time because `ui()` does not run
  while the window is hidden, which is exactly the "pump on wake, not on
  paint" model.
- **Polling that a wake-driven pump must replace.** (i) `request_repaint_after(250 ms)`
  twice in `handle_tray` (`main.rs:2582`, `2636`) keeps `tray.poll_actions`,
  `shell::take_request` (2547, a file poll) and the listener channel alive
  while the window is hidden. (ii) A 200 ms unconditional heartbeat thread
  (`main.rs:547-551`), a workaround for Windows throttling an unfocused
  window's repaints (see the note there); a Win32 message loop does not have
  this problem but the tray, `shell::take_request` and toast handler still
  need a wake source. (iii) Timers hidden in `logic()`: `autosave_drafts`
  (1358, `DRAFT_AUTOSAVE_INTERVAL`), `poll_outbox` (1389,
  `OUTBOX_POLL_INTERVAL`), `handle_mark_seen_delay` (2312): all are
  `Instant` comparisons that only fire because something keeps repainting.
  The core needs an explicit `next_deadline()` so the frontend can arm a
  timer (`SetTimer` on Win32).
- **One raw egui wake remains** after #96: `spawn_attachment_write` calls
  `ctx.request_repaint()` (`main.rs:3667`, unscoped, not the root viewport
  waker) from a plain thread; it should take the `Waker`. `screenshot.rs:65`
  is egui-only and stays.
- **`!Send` / `Rc` / `RefCell`.** None in core state: `grep -n "Rc<\|RefCell\|Cell<"`
  over `main.rs`, `accounts.rs`, `settings.rs`, `compose_*.rs`,
  `listener_client.rs`, `config_saver.rs` returns nothing. `EsMailApp` itself
  holds egui/eframe objects (`WebView`, `WebViewHost`, `egui::Context` and
  `screenshotter`) and `platform::TrayState`, so it is a UI-thread-only
  object; that is fine as long as the core state moves out of it. The
  compose windows share state through `Arc<Mutex<Shared>>`
  (`compose_window.rs`), and the SMTP forwarder mutates it from a tokio task
  (`main.rs:603-625`), so *compose state is written from two threads*: an
  `AppCore` has to keep that lock or turn it into events.
- **Blocking calls on the UI thread.** `rfd::FileDialog` in
  `save_attachment` (`main.rs:2012`), `export_selected_message` (2031) and
  `compose_ui.rs:152` (attachment picker): modal, fine, but the *decision*
  (what to do with the chosen path) is inline; per the plan, dialogs stay in the
  frontend and return an `Intent`. `opener::open`
  (`main.rs:3654`) runs on the attachment thread, `opener::open_browser`
  (`accounts.rs:35`) runs in a tokio task, neither blocks a frame. `block_on`
  only at start-up (3870). `thread::sleep` only in the heartbeat thread
  (549). No `arboard` / clipboard code of our own (egui does copy/paste).
- **Global mutable state that assumes one frontend**:
  `platform::set_toast_click_handler` installs a single process-wide handler
  (`platform/windows.rs:235-245`, a `OnceLock`, first call wins); `shell::acquire_single_instance`
  is a process-wide named mutex. Both are fine but should be exposed as
  services (see e).

| Gap | Severity | Size |
|---|---|---|
| Actors need an ambient tokio runtime; nothing hands the core a `Handle` | blocker | S-M |
| Seven `try_recv` drains scattered through `logic`/`ui`/`handle_tray`; no single `pump()` | blocker | L (steps 4) |
| Time-based work (autosave, outbox, mark-seen) only runs because egui keeps repainting; no `next_deadline()` | important | M |
| `spawn_attachment_write` still uses a raw `egui::Context` wake | important | S |
| Compose state written from the SMTP forwarder task and the UI | important | M |

## d. Leaky abstractions

- **UI constructs actor commands.** In `main.rs`: 17 `ImapCommand::`, 21
  `DbCommand::` and 2 `SmtpCommand::` sites, plus 5 `SmtpEvent::` matches;
  `accounts.rs` 1 `DbCommand::`. In `ui()` alone: 2759, 2820, 3024, 3028
  (see b.1). The `MailHeader`, `ImapEvent`, `DbEvent`, `SmtpEvent` types are
  the actors' wire protocol, and today they are the model too: the UI holds
  `headers: Vec<MailHeader>` and matches on `ImapEvent` variants directly.
  A `dispatch(Intent)` layer (step 5) has to hide these.
- **egui types in shared data.** `EsMailApp` fields: `search_box_id:
  Option<egui::Id>` (`main.rs:403`), `window_icon: Option<Arc<egui::IconData>>` (435),
  `egui_ctx`, `screenshotter`. Colours are computed inline from
  `egui::Color32` (`main.rs:2860-2861` banners, `3052-3054` connection state
  dots, `2492`). `ConnState` (172) carries no colour, but the mapping to
  colour is in `ui()`, so a Win32 frontend re-derives it. Click handling passes
  `egui::Modifiers` through (`main.rs:3289`, `3319`) into the range-selection
  logic. The message-row text is already `view_model::RowModel` (#97); the
  colours and fonts are not.
- **Shortcuts are egui keys.** `handle_keyboard_shortcuts` (2343-2425) matches
  `egui::Key::{F,N,J,K,Enter,R,A,Delete,Backspace}` at 2354-2362 and gates on
  `search_focused` via egui memory. There is no `Command` enum, so a Win32
  accelerator table cannot be built from it (step 3).
- **Theme conversion duplicated.** `ThemeMode` to `egui::ThemePreference` at
  `main.rs:687-689` and `1796-1798`.
- **Config persists frontend-specific things.** `WindowGeometry` (`config.rs:246-256`)
  is documented as egui outer-rect units and lives in the shared
  `config.toml` under `window`; a Win32 frontend would read wrong sizes on a
  high-DPI monitor and overwrite the egui frontend's geometry. Folder fold
  state (`collapsed_folders`, `config.rs:278-289`) is logical and can stay shared.
  `config.rs:266` even documents "eframe's own built-in default size".
- **The webview is egui-specific.** `WebView`, `WebViewHost` and
  `MessageViewHandler` (`main.rs:67-125`) wrap `egui-litehtml-webview`; the
  interception rules (remote-image blocking, `cid:`) are already in the lib
  (`render.rs`), but `MessageViewHandler::allow_remote` state lives in the
  handler, not in a model.

| Gap | Severity | Size |
|---|---|---|
| UI issues ~40 actor commands and matches actor events directly | blocker | L |
| Shortcuts not data | important | S-M |
| `WindowGeometry` shared across frontends, egui units | important | S |
| Colours / connection-state presentation in `ui()` | nice | S |
| Theme conversion duplicated | nice | S |

## e. Cross-cutting services

| Service | Where | State | Gap |
|---|---|---|---|
| Tray / toast | `platform/` (`TrayState`, `show_new_mail_toast`, `set_toast_click_handler`), `notify.rs` (decisions, pure), `icons.rs` | lib, clean API; but `TrayState::poll_actions` is polled (c) and the click handler is a process global | needs wake-driven delivery; win32 frontend would register its own tray, so `TrayState` must be optional |
| Background listener + IPC | `listener.rs::run()`, `ipc/`, `docs/BACKGROUND-LISTENER.md`; client in bin-only `listener_client.rs` | already egui-free; `main()` dispatches `--background` inline (3852) | move client + CLI dispatch to the lib; `listener.rs` pulls `winit` (a) |
| Single instance / requests | `shell.rs` (`acquire_single_instance`, `send_request`, `take_request` = polled file) | lib, clean | `take_request` is a poll, no wake source |
| Autostart / shell registration | `shell.rs` (`register_notification_identity`), installer `--quit`/`--purge-data` in `main()` and `uninstall.rs` | lib, clean | CLI parsing lives in `main.rs` |
| OAuth loopback | `oauth.rs` (719 lines: `begin`, `finish`, `TokenSource`) | lib, headless-tested (`tests/oauth_integration.rs`) | orchestration (`run_google_sign_in`, `oauth_tasks`, `handle_oauth_events`) is in `accounts.rs`; browser open is `opener::open_browser` at `accounts.rs:35` with no `Platform` seam |
| Attachments save / open | `render.rs` extraction (lib), `AttachmentWrite`, `spawn_attachment_write`, `opener::open` in `main.rs:3631-3690`, dialogs at 2012/2031 | bin-only | needs `Platform::open_path`; `paths::clean_attachments_dir` is in the lib already |
| Drafts / outbox | `db.rs` (1642 lines: `DbCommand::SaveDraft`, `EnqueueOutbox`, `DueOutbox`), timers in `main.rs:1358-1397` | actor in lib, scheduling in the UI | scheduling is UI-owned (c) |
| First-run / account setup | `accounts.rs`, provider wizard `main.rs:1828`, the provider table (`config.rs:126`) | mixed with form state | needs a model (`AccountForm`) |
| Errors / banners | `Banner { id, message }` (`main.rs:125`), `push_banner` 1658, `status: String` (15 sites) | bin-only, a plain string | move with `AppCore`; keep as `Vec<Banner>` + `Changes::BANNERS` |
| i18n / strings | none: every user-facing string is an inline English literal (e.g. `main.rs:2860`) | n/a | nice: no extraction needed for a first Win32 frontend; note that `view_model` formatters are the one place strings are shared |

## f. Testability

Runs headless against `crates/mail-mock-server` today (`tests/`):
`imap_smtp_integration.rs` (`ImapActor`, `SmtpActor`), `multi_account.rs`
(several `AccountSession`s), `watcher.rs` (new-mail watcher and toasts via
`Hooks`), `oauth_integration.rs`, `render_fixtures.rs` (drives the webview
with an `egui::Context`, so egui-bound), plus lib unit tests (`db`,
`view_model`, `notify`, `search_query`, `config`, `ipc`).
`session::Hooks::none()` and `waker::noop()` are the headless seams.

Cannot run headless: everything in b. `EsMailApp::new` takes an
`eframe::CreationContext` (`main.rs:506`), and `handle_imap_events`,
selection, bulk actions, outbox retry, draft autosave, compose send/error
handling and OAuth orchestration have no test entry point. Scenarios such as
"open message, load remote images, delete, undo" and "SMTP fails, item lands
in the outbox, retry succeeds" are only testable through a real window.
Fixing this is the biggest side benefit of `AppCore`.

## Follow-up issues

Steps 1 and 2 of #98 are #96 and #97. The rest, in dependency order:

| # | Title | #98 step | Contents | Size |
|---|---|---|---|---|
| 3/10 (#106) | Make egui optional in the `esmail` package (feature gate) | 6 (early) | a: move `emoji`/`screenshot`/`icons` egui halves out, optional egui deps behind `egui-frontend`, `listener` winit, CI job `cargo check --lib --no-default-features`; gives win32 a compile-time guarantee before the full split | S-M |
| 4/10 (#107) | Shortcuts as data | 3 | `Shortcut{key,mods} -> Command` table and dispatch in the lib; egui adapter; theme conversion in one place | S-M |
| 5/10 (#108) | AppCore skeleton: state types, runtime handle, `pump()` for IMAP events | 4 | move b.12 state, inject a tokio `Handle`, `Changes`, move `handle_imap_events` (b.2) and progress/bulk (b.7) | L |
| 6/10 (#109) | AppCore `pump()`: db, smtp, attachment IO, oauth, toast/tray/shell/listener requests, timers | 4 | b.3, b.6 (part), b.9; one wake-driven pump, `next_deadline()`, remove the 250 ms polling, convert the last raw wake | L |
| 7/10 (#110) | AppCore `dispatch()`: selection, message actions, search, accounts, compose, drafts/outbox | 5 | b.1, b.4, b.5, b.6, b.10; UI stops building actor commands | L |
| 8/10 (#111) | Platform services and CLI in the lib | 7 | `Platform` trait (`open_path`, `open_url`), `listener_client` and `config_saver` to the lib, shared `cli` module for `--quit/--purge-data/--background/--open-account`, toast-click handler through the `Waker` | M |
| 9/10 (#112) | Per-frontend window geometry and presentation state | 7 | opaque per-frontend geometry blob in `config.toml`, `emoji` segmentation/drawing split, colours for `ConnState`/banners out of `ui()` | S-M |
| 10/10 (#113) | Split into `esmail-core` / `esmail-egui`, plus headless `AppCore` tests | 6 | crate split (binary stays `esmail`), tests driving `AppCore` against `mail-mock-server` for the scenarios in f | M-L |
