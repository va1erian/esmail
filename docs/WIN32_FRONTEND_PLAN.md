# Plan: a frontend-agnostic core and a native Win32 frontend

Draft, 2026-09-23. Status: proposal, nothing implemented.

**Goal.** Two outcomes that reinforce each other:

1. **esMail**: a small, fast mail client whose Windows build has no GL, no
   egui, no winit. It should start instantly, idle at near-zero CPU, and look
   and behave like a Windows application: native controls, keyboard navigation,
   accessibility and IME for free.
2. **[`win32ui`](https://github.com/va1erian/win32ui)**: a composable,
   reusable, idiomatic Rust library for native Windows UI. esMail is its second
   consumer after emusic, which is a good test of whether its abstractions
   generalise.

The route is three tracks that can run partly in parallel:

- **A.** Extract a frontend-agnostic `esmail-core` from `main.rs`. The egui
  app stays the only frontend, and stays green, the whole time.
- **B.** Make the litehtml webview backend-neutral: a shared layout and
  display-list core, with an egui painter and a Direct2D painter on top.
- **C.** Grow `win32ui` to cover what a mail client needs, then build
  `esmail-win32` on top of A and B.

---

## 1. Where things stand

### esMail today

| Area | egui coupling |
|---|---|
| `imap.rs`, `smtp.rs`, `db.rs`, `idle_watch.rs`, `session.rs`, `watcher.rs`, `render.rs`, `oauth.rs`, `auth.rs`, `config.rs`, `ipc/`, `listener.rs`, `notify.rs`, `platform/` | **None.** They are already reached through channels plus a `Hooks { notify, repaint }` callback. This is the good news: the hard parts are already UI-free. |
| `emoji.rs` | Paints Twemoji textures into egui galleys. |
| `icons.rs` | `From<Rgba> for egui::IconData` only. |
| `screenshot.rs`, `window_fit.rs` | egui/winit-specific by nature. |
| **`main.rs` (4197 lines)** | `EsMailApp` mixes **all** application logic with drawing: the account list and active account, channel draining (`imap_rx`/`db_rx`/`smtp_rx`/`oauth_rx`/attachment IO/toast clicks), outbox retries, draft autosave, bulk actions, selection, banners, progress, keyboard shortcuts. Wakeups are `ctx.request_repaint_of(ROOT)` closures created in `EsMailApp::new`. |
| `accounts.rs`, `settings.rs`, `compose_ui.rs`, `compose_window.rs` | `impl EsMailApp` blocks that combine logic with egui widgets. |
| `egui-litehtml-webview` | The worker, jobs, image fetching, `TextRunTable`, `LinkTable` and `Selection` are conceptually neutral but use `egui::Rect`/`Pos2`. `painter.rs` measures text with egui's font stack and records `Cmd`s in egui types (`Color32`, `Mesh`, `TextureHandle`, `FontId`). `fonts.rs` installs faces into the shared `egui::Context`. |

`main.rs` is already far past the 500-line module limit in AGENTS.md, so
splitting it pays for itself even without a second frontend.

### win32ui today (commit `e0d1aad`, ~5.4 kLOC)

`Window` + `WindowHandler` with a typed `Message` enum, `WindowClass`, a
`run`/`quit` loop, a registered wake message, RAII GDI objects, a
double-buffered `Paint`/`Canvas`, and five controls: an owner-data, custom-drawn
`ListView`; a lazy `TreeView`; owner-drawn `Toolbar` and `StatusBar`; and
`Label`. A thread-local registry routes each control's own `WM_NOTIFY`s back to
it. The invariants are good and worth keeping: `unsafe` only in `sys/`, no
`windows` types in the public API, controls own and destroy their `HWND`, and
small files.

---

## 2. What is missing in win32ui

Ordered by how much the rest depends on each item. **Blocking** means
esmail-win32 cannot ship without it.

### 2.1 Foundations (blocking, fix first)

1. **Reentrant messages are dropped.** `sys::window::window_proc` sends a
   message to `DefWindowProcW` when that window's handler is already on the
   stack. This is what keeps it free of UB, but in practice a whole class of
   notifications disappear silently. Examples: `ListView::select()` →
   synchronous `LVN_ITEMCHANGED` to the parent; `SetWindowPos` inside a handler
   → `WM_SIZE`; `WM_COMMAND` from a child created during `WM_CREATE`;
   `TrackPopupMenu` and modal dialogs that pump messages. Pick one of these
   fixes:
   - *Recommended:* the handler becomes `&self`, and state lives in
     `Cell`/`RefCell` fields borrowed briefly. That is the idiomatic shape for
     a reentrant callback API, and it matches how the controls already keep
     `Rc<RefCell<Inner>>`.
   - *Alternative:* keep `&mut self`, but **queue** a reentrant *notification*
     (anything that does not need a synchronous result) and deliver it once the
     outer call returns. Messages that need a synchronous answer still go to
     `DefWindowProc`, and this should be logged in debug builds.
2. **A cross-thread waker.** `Window::post_wake` needs a `&Window`, and
   `Window` is not `Send`. Add a `WakeHandle: Send + Sync + Clone` (an `Hwnd`
   and the registered message id). This is exactly the `RepaintFn` that
   `esmail-core` hands its workers. Coalesce wakes with an `AtomicBool` so a
   burst of IMAP events posts one message, not hundreds.
3. **Non-Windows builds.** esMail CI builds the whole workspace on Linux. The
   `windows` dependency is target-gated, but the code uses it
   unconditionally. Put `#![cfg(windows)]` on the crate root, or on its
   modules, so it compiles to an empty crate elsewhere, and do the same in
   `esmail-win32`.
4. **The `windows` crate version.** win32ui pins `windows 0.58` and esMail
   uses `0.62`, so both would be linked. Align on `0.62`, or on `windows-sys`
   for the raw calls, which is lighter. `Error::Win32(windows::core::Error)`
   also leaks a `windows` type into the public API. Wrap it as an
   `HRESULT` code plus a message.
5. **Message coverage.** Keyboard (`WM_KEYDOWN/UP`, `WM_SYSKEYDOWN`,
   `WM_CHAR`) with a `Modifiers` struct; `WM_MOUSEWHEEL/HWHEEL`; double-click
   (`CS_DBLCLKS`); middle and X buttons (`MouseButton::Middle` exists but is
   never decoded); `WM_MOUSELEAVE` + `TrackMouseEvent` for hover;
   `WM_SETCURSOR`; `WM_SETFOCUS/KILLFOCUS`; `WM_CONTEXTMENU`;
   `WM_GETMINMAXINFO`; `WM_ACTIVATE`; `WM_SETTINGCHANGE` for live light/dark
   switching; `WM_CTLCOLOR*` for dark edits and statics;
   `WM_CAPTURECHANGED`; `WM_QUERYENDSESSION/ENDSESSION`;
   `WM_INITMENUPOPUP`. Keep `Message::Other` as the escape hatch.
6. **The message loop.** It needs `IsDialogMessage` for Tab and Shift+Tab
   between controls in ordinary windows, not just dialogs; accelerator tables
   (`TranslateAccelerator`) for app shortcuts such as Ctrl+N, Delete and
   Ctrl+R, so they work whichever child has focus; and an optional
   per-iteration hook.
7. **Window API.** `set_icon`; min/max size; `get/set_placement`
   (`GetWindowPlacement` round-trips maximized state and restore bounds, which
   replaces `window_fit.rs` and the geometry code in `config.rs`); owner and
   modal windows (`EnableWindow` on the owner); `set_foreground` (a
   replacement for `bring_window_to_front`); enable/disable; focus;
   `SetCapture`; cursor shape; the DWM dark title bar
   (`DWMWA_USE_IMMERSIVE_DARK_MODE`); and monitor work areas.

### 2.2 Controls (blocking unless noted)

| Control | Needed for | Notes |
|---|---|---|
| **Edit**, single- and multi-line | search box, compose body, subject, recipients, settings, add-account form | cue banner (`EM_SETCUEBANNER`), password (`ES_PASSWORD`), read-only, `EN_CHANGE` event, select-all/limit. Dark mode through `WM_CTLCOLOREDIT` plus `DarkMode_CFD`/`DarkMode_Explorer` scrollbars. |
| **Button**: push, default, checkbox, radio, group box | every dialog | `BN_CLICKED` already decodes. |
| **ComboBox** (drop-down list) | the compose "From" account, theme choice, provider presets | `CBN_SELCHANGE`. |
| **Tab control** | Settings (General / Accounts / ...) | or a simple owner-drawn segmented control |
| **Menus**: menu bar and popup | message-list and attachment context menus, the tray menu, "..." menus | `TrackPopupMenuEx`, check/radio items, `WM_MENUCOMMAND`; dark menus need owner-draw or the undocumented `SetPreferredAppMode` (see risks) |
| **Tooltip** | toolbar buttons, `on_hover_text` equivalents | `TOOLTIPS_CLASS` attached per tool |
| **Splitter** (custom) | the tree \| list \| reading pane layout; the widths are persisted | a thin custom window that drags with `SetCapture` |
| **Scroll container** (custom) | long settings pages, the reading pane host | `WM_VSCROLL`/`SCROLLINFO` + wheel |
| **Progress bar** | sync/bulk progress | `PROGRESS_CLASS`, or owner-drawn in the status bar |
| **SysLink** (nice to have) | "Sign in with Google" help, README links | |
| **Task dialog / message box** | confirm delete, forget account, errors | `TaskDialogIndirect` |
| **File dialogs** | attachment save/open, `.eml` export, add attachment | `rfd` already works on a raw `HWND` owner, so keep it at first. A native `IFileDialog` wrapper is a later win32ui addition (and removes a dependency). |
| **Tray icon** | the background listener already uses `tray-icon` | Keep it. A `Shell_NotifyIcon` wrapper in win32ui is optional and would drop a dependency. |

### 2.3 Gaps in the existing controls

- **ListView is emusic-shaped.** `set_playing`, `ListViewTheme::playing`
  and a zebra painted by the control. esMail's list (`message_row`) needs:
  - a **per-row painter hook**, e.g. `ListSource::paint_row(item, &mut RowCanvas, state)`
    or a `RowStyle { weight, color, accent_bar }`. Unread rows are bold with an
    accent bar and read rows are dimmed; each row has a flag icon, an
    attachment glyph, two lines (sender/date over subject), and colour emoji;
  - **variable/custom row height** (`LVS_OWNERDRAWFIXED` + `WM_MEASUREITEM`,
    or an image-list height trick);
  - **multi-select**: a selected-set query, a range anchor, plus
    `ensure_visible`, `set_focused` and `redraw_items(range)` for a single
    flag change, and `set_item_count` with `LVSICF_NOSCROLL` so a sync does
    not jump the scroll;
  - **no allocation per cell per paint**. `ListSource::text` returns a fresh
    `String` on every `LVN_GETDISPINFO`. Write into a caller-supplied buffer,
    or return `Cow<str>`. This runs on the render hot path (AGENTS.md
    performance bar).
- **TreeView** needs `refresh()`/diff when the mailbox list changes,
  programmatic `select(id)` / `expand(id, bool)`, a `Collapsed` event (fold
  state is persisted in `config.toml`), and custom draw for bold unread counts,
  special-use icons and a dark selection.
- **Toolbar** needs tooltips, disabled and toggled states, and optional
  drop-down buttons.
- **StatusBar** ignores its `_id` argument. Harmless, but tidy it.

### 2.4 Painting

- `Canvas::fill_rect`/`round_rect`/`triangle` create and destroy a GDI
  brush **per call**, and `Paint::begin` allocates a full-client back buffer
  **per `WM_PAINT`** and blits the whole client, ignoring `rcPaint`. Cache
  brushes by colour and the back buffer by size, and blit only the dirty
  rectangle.
- **GDI cannot draw colour emoji**, has no anti-aliasing and no alpha. That is
  fine for common-control custom draw, but it is not enough for the message
  list's emoji or for HTML mail. Add a **`d2d` module**: an
  `ID2D1HwndRenderTarget` (or a DXGI swap-chain device context), a
  DirectWrite `TextFormat`/`TextLayout` with
  `D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT`, and cached brushes, bitmaps and
  geometries. Handle device loss (`D2DERR_RECREATE_TARGET`). Section 4 builds
  the webview on this module, and the message-list row painter can use it too.
- A measuring context outside `WM_PAINT` (a DirectWrite text layout), for
  layout decisions.
- `Window::capture() -> Rgba` via `PrintWindow(PW_RENDERFULLCONTENT)`, so
  HANDOFF.md's "verify visual work by looking at it" and `screenshot.rs`'s F12
  and auto-capture carry over.

### 2.5 Cross-cutting

- **Theming.** `Theme` is a hard-coded emusic palette. It should follow the
  system: `AppsUseLightTheme` + `WM_SETTINGCHANGE("ImmersiveColorSet")`,
  the accent colour (`UISettings`/`DwmGetColorizationColor`), high contrast
  (`SPI_GETHIGHCONTRAST`, where custom draw should step aside), and dark
  scrollbars, edits, combos and menus.
- **Layout.** Only `Rect::split_*` exists. Add a small, DPI-aware layout
  helper: dock (top/bottom/left/fill), a horizontal/vertical stack with
  fixed and fill slots, and margins in 96-DPI units scaled once. Don't build a
  general layout engine.
- **Accessibility.** Native controls are accessible for free, which is
  another reason to prefer them over owner-drawn replacements. Custom windows
  (splitter, banner, and above all the webview) need a UI Automation provider
  (`WM_GETOBJECT` → `IRawElementProviderSimple`, with a text pattern for the
  message body). This can land after the MVP, but plan for it.
- **Clipboard**: text read/write, for copying selections and links. **Drag and
  drop** (OLE `IDropTarget`, dropping attachments into compose) can come later.
- **Composability.** Make the pattern the toolbar and status bar already use
  internally public: a `CustomControl` trait (paint + input + preferred
  size) hosted in its own window class. That lets applications, and later
  win32ui itself, add controls without touching `sys/`.
- **Docs and naming.** Remove the emusic references (`#106`, "album art",
  `playing`) from the public docs and API, and publish on crates.io once the
  API settles.

---

## 3. Making esMail frontend-agnostic

### 3.1 Target crate layout

```
crates/
  esmail-core/           lib: everything in src/lib.rs today + the extracted app model
  esmail-egui/           bin: today's UI (main.rs split into views/)
  esmail-win32/          bin: the new frontend, `#![cfg(windows)]`, stub elsewhere
  litehtml-view/         webview core: worker, display list, text runs, links, selection
  litehtml-view-egui/    egui painter + widget  (today's egui-litehtml-webview)
  litehtml-view-d2d/     Direct2D/DirectWrite painter + win32ui control
  mail-mock-server/      unchanged
```

The `--background` listener already has no egui (`listener.rs`), so it moves
into `esmail-core` as `listener::run()`. Each frontend's `main` dispatches
`--background` to it, and both binaries interoperate over the existing IPC.
Shipping the listener as its own tiny `esmail-listener.exe` is an option, but
not required.

### 3.2 The app model: `esmail_core::app`

The seam is a **retained model with intents in and change notifications
out**. That suits Win32, which is retained-mode, and costs egui nothing
(egui ignores the change set and redraws).

```rust
pub struct AppCore { /* accounts, active, channels, outbox/draft state, selection, banners, progress */ }

pub enum Intent {
    SelectAccount(AccountId), SelectMailbox(String), OpenMessage(u32),
    SelectRange { anchor: u32, to: u32 }, ToggleFlag(Flag), Delete, Archive,
    Search(String), LoadRemoteImages { always_for_sender: bool },
    SaveAttachment { index: usize, path: PathBuf }, ExportEml(PathBuf),
    Compose(ComposeKind), Send(ComposeId), ... ,
}

bitflags! { pub struct Changes: u32 {
    const ACCOUNTS; const MAILBOXES; const MESSAGE_LIST; const MESSAGE_ROWS;
    const READING_PANE; const BANNERS; const PROGRESS; const COMPOSE; const SETTINGS; } }

impl AppCore {
    pub fn new(config: Config, waker: Waker, platform: Arc<dyn Platform>) -> Self;
    pub fn dispatch(&mut self, intent: Intent) -> Changes;
    /// Drains every worker channel; call when woken.
    pub fn pump(&mut self) -> Changes;
    // read-only view state
    pub fn mailbox_rows(&self) -> &[MailboxRow];
    pub fn message_list(&self) -> &[MailHeader];
    pub fn row(&self, i: usize) -> RowModel;          // sender, date, subject, flags, unread
    pub fn reading_pane(&self) -> Option<&ReadingPane>; // sanitized html, attachments, remote-image state
    pub fn banners(&self) -> &[Banner];
    ...
}

pub type Waker = Arc<dyn Fn() + Send + Sync>;       // egui: request_repaint; win32: WakeHandle::wake
pub trait Platform: Send + Sync {                    // things that need an OS window/owner
    fn open_path(&self, path: &Path) -> io::Result<()>;
    fn open_url(&self, url: &str) -> io::Result<()>;
}
```

File dialogs stay **in the frontends**. They need an owner window and are
modal, so the frontend runs the dialog and sends back an `Intent` carrying the
chosen path.

### 3.3 Extraction steps

Each step is its own PR, and the egui app must behave the same after it:

1. **Wakeups.** Replace every `ctx.request_repaint_of(ROOT)` closure in
   `EsMailApp::new` with one `Waker` passed in from outside. Mechanical.
2. **Pure helpers out of `main.rs`.** `select_range`,
   `find_special_use_mailbox`, `format_size`, `safe_attachment_filename`,
   `export_file_name`, `progress_label`, and the text half of `message_row`
   (sender, date and flag formatting → `RowModel`) move to
   `esmail-core::view_model`, with unit tests.
3. **Shortcuts become data.** A `Shortcut { key, mods } -> Command` table in
   core. egui maps `egui::Key` and Win32 maps virtual keys to the same table,
   and Win32 builds its accelerator table from it.
4. **Event handling moves into `AppCore`.** `handle_imap_events`,
   `handle_db_events`, `handle_smtp_events`, attachment IO, OAuth messages,
   toast clicks, `poll_outbox` and draft autosave move into `AppCore::pump`,
   one channel per PR. `EsMailApp` shrinks to `AppCore` plus widget state.
5. **Actions move into `AppCore::dispatch`.** Toolbar buttons and menu items
   stop calling actors directly.
6. **Split the crates**: `esmail-core` / `esmail-egui`. Leave the Cargo
   package name for the egui binary as `esmail` until the frontends switch
   over, so the installer and scripts need no changes.
7. **Leftovers.** `emoji.rs` splits into segmentation (core) and drawing
   (egui only; Win32 uses DirectWrite colour fonts, see §5). `icons.rs` keeps
   `Rgba` in core, and the `IconData` conversion moves to egui. `config.rs`'s
   `WindowGeometry` becomes an opaque per-frontend blob, so Win32 can store a
   `WINDOWPLACEMENT` and egui can keep its outer rect.

**A payoff beyond Win32:** `AppCore` can be driven headless against
`mail-mock-server`. Tests like "open message → load remote images → delete
→ undo" become ordinary integration tests, where today they need a GUI.

### 3.4 Threading model (unchanged)

The tokio runtime and actors stay on background threads. The UI thread only
ever runs `dispatch`/`pump` and reads state, so neither frontend needs `Send`
UI state. The Win32 loop is: worker → `Waker` → coalesced `PostMessage(WM_APP_WAKE)`
→ `pump()` → `Changes` → targeted control updates (`set_item_count`,
`redraw_items`, `tree.refresh`, `webview.load`, `invalidate`).

---

## 4. Plugging litehtml into a non-egui frontend

What litehtml requires: a `DocumentContainer` that **measures text** during
layout and receives **draw callbacks** during `draw()`, and a `Document` that
borrows the container and so is rebuilt for every pass (see the
`egui-litehtml-webview` crate docs). Today both halves are egui: measurement
uses egui's font stack, and draws become egui `Cmd`s. The invariant that
cannot be broken is **the text engine that measures must be the one that
paints**, or line breaks and glyph positions drift.

### Option A (recommended): a neutral display list with pluggable text and paint backends

Split `egui-litehtml-webview` into:

- **`litehtml-view` (core, no UI dependency).** The worker thread, job ids,
  supersession, image discovery, bounded parallel fetch, `WebViewHandler`, the
  image cache budget, `TextRunTable`, `LinkTable`, `Selection`, and the
  pointer state machine for selection (press, drag, double and triple
  click). It uses its own `Point`/`Rect`/`Rgba` types. `Cmd` becomes
  backend-neutral:
  ```rust
  enum Cmd { Rect{..}, Outline{..}, Line{..}, Circle{..},
             LinearGradient{..}, RadialGradient{..},      // data, not an egui Mesh
             Text { origin, width, text: Arc<str>, font: FontKey, color },
             Image { image: ImageKey, rect },              // decoded RGBA shared by Arc
             PushClip { rect, radii }, PopClip }
  ```
  Text goes through a trait whose instance is **created on the worker**, since
  the current engine is `!Send` and that stays true:
  ```rust
  pub trait TextBackend {
      fn create_font(&mut self, desc: &FontDescription) -> (FontKey, FontMetrics);
      fn text_width(&mut self, text: &str, font: FontKey) -> f32;
  }
  pub trait Backend: Send + Sync + 'static {
      type Text: TextBackend;
      fn new_text_backend(&self) -> Self::Text;   // called on the worker
  }
  ```
  A `ListFrame` carries `Arc<DisplayList>` + `Arc<FontTable>` (the
  `FontKey → description` map) + the tables. Images cross threads as
  `Arc<RgbaImage>` keyed by an id, and each painter caches its own GPU copy
  (an egui `TextureHandle`, or an `ID2D1Bitmap`), evicted when frames drop
  the key.
- **`litehtml-view-egui`.** Today's `fonts.rs` (the fontdb discovery and all
  its legacy-name and variable-font work) becomes the egui `TextBackend`, and
  `painter.rs`'s replay loop becomes `paint(&DisplayList, &egui::Painter)`.
  Rendering should stay pixel-for-pixel the same. Check it with the
  `render_fixtures` benchmark and the fixture screenshots before merging.
- **`litehtml-view-d2d`.**
  - *Text:* DirectWrite. An `IDWriteFactory` (shared, thread-safe) plus a
    `IDWriteTextFormat` per `FontKey` created on the worker; width from an
    `IDWriteTextLayout` (`GetMetrics().widthIncludingTrailingWhitespace`).
    DirectWrite already does what `fonts.rs` hand-rolls: GDI-compatible legacy
    family names (`Segoe UI Semibold`, `Arial Black`), variable-font weight
    axes, per-glyph system fallback for CJK and symbols, and **colour emoji**.
    On Windows, `fontdb` + `ttf-parser` drop out.
  - *Paint:* a win32ui `CustomControl` (`WebViewControl`) that replays the
    list into an `ID2D1RenderTarget`. Cull to the visible rect, then:
    `FillRoundedRectangle`, `DrawRoundedRectangle` with stroke styles for
    dashed and dotted borders, `ID2D1LinearGradientBrush`/`RadialGradientBrush`,
    `DrawTextLayout` (layouts cached per `Text` cmd within a frame, or
    `DrawGlyphRun` if profiling asks for it), `DrawBitmap` with high-quality
    cubic interpolation, and `PushLayer` with a rounded-rect geometry mask.
    That last one **fixes a limitation the egui painter has**: egui clips are
    rectangular, so today rounded overflow clips and rounded gradients are
    painted square.
  - *Host:* native scrollbar (`SCROLLINFO`) + wheel, `IDC_HAND` over a
    `LinkTable` hit, `SetCapture` while dragging a selection, Ctrl+C/Ctrl+A,
    selection highlight painted over the page, and a `WM_SIZE` →
    `submit_render(width, dpi/96)` on a DPI change.

Costs: a moderate refactor of a 5 kLOC crate, and two text backends to keep
consistent with litehtml's `FontDescription` handling. It is also the only
option that keeps the current properties: no per-page bitmap, crisp at any
DPI, and paint cost proportional to what is on screen.

### Option B: rasterize on the worker and blit a bitmap

Turn litehtml-rs's `pixbuf` feature (tiny-skia + cosmic-text) back on, render
the page into BGRA tiles on the worker, and `StretchDIBits`/`DrawBitmap` them.
The frontend work is trivial and the renderer is the same on every frontend.
But docs/RENDERERS.md already measured this and rejected it: +1.3 MiB, about
68 MB for a 2600 pt newsletter, worse font-family matching, no link
underlines, and a re-raster on every DPI change. Only worth it as a
**stopgap** for bringing up esmail-win32 before Option A's D2D painter exists.

### Option C: draw from litehtml callbacks directly with GDI or D2D on the UI thread

This is how litehtml's own Windows sample works. It is rejected: layout of
real marketing mail takes seconds (see #32), and `draw()` needs a live
`Document`, which cannot be kept between passes. It would mean re-laying out
on the UI thread or re-architecting around a persistent document.

### Option D: host an egui/glow child window inside the Win32 frontend

This works on day one with zero renderer changes, but it brings back GL,
egui, winit and twemoji, which defeats the "small" goal. **Not recommended**
even as a transition. If a stopgap is needed, B is cheaper and throwaway.

**Recommendation:** A. Order the work so the core split (litehtml-view +
litehtml-view-egui, with no visible change) lands before any D2D code. Then
the egui app proves the neutral core is right before a second backend
depends on it.

---

## 5. The esmail-win32 frontend

```
crates/esmail-win32/src/
  main.rs            init, --background dispatch, single-instance/IPC, run()
  app.rs             MainWindow: WindowHandler owning AppCore + views; Wake → pump → apply(Changes)
  layout.rs          toolbar / splitter(tree | list | reading) / status bar
  views/mailbox_tree.rs   TreeView + TreeSource over AppCore::mailbox_rows
  views/message_list.rs   ListView + row painter (DirectWrite: bold unread, accent bar, colour emoji)
  views/reading_pane.rs   header block, remote-images bar, attachment chips, WebViewControl
  views/banner.rs         error/info banners (custom control)
  compose.rs         one top-level Window per ComposeId (Edit controls, From ComboBox, attachments)
  settings.rs        owned modal window with Tab control; account dialog
  accelerators.rs    from core's shortcut table
  theme.rs           system light/dark + accent → win32ui Theme; WM_SETTINGCHANGE
```

**Size.** Compared with today's 11.5 MiB egui build, it drops eframe, egui,
egui_glow, winit, glow, twemoji-assets (about 4.4 MiB, replaced by Segoe UI
Emoji through DirectWrite), fontdb and ttf-parser, and it adds only `windows`
features the app already partly links. Set a target (say, under 6 MiB) and
measure each milestone with the `docs/PERFORMANCE.md` method. Don't take the
number on faith.

**Speed.** Startup should need no GL context creation. Idle should be
event-driven only: no 250 ms `request_repaint_after` polling, because a
coalesced wake replaces it. The virtual ListView paints only visible rows.

---

## 6. Milestones

| # | Deliverable | Depends on |
|---|---|---|
| M0 | win32ui foundations (§2.1): reentrancy, `WakeHandle`, `cfg(windows)`, `windows` 0.62, keyboard/wheel/focus messages, accelerators + `IsDialogMessage`, window placement | — |
| M1 | `AppCore` extraction (§3.3 steps 1–5) inside the egui crate; `main.rs` under control | — (parallel with M0) |
| M2 | Crate split: `esmail-core`, `esmail-egui`; headless `AppCore` integration tests | M1 |
| M3 | `litehtml-view` + `litehtml-view-egui`; no visible change, fixture-render and benchmark parity | — (parallel) |
| M4 | win32ui: `d2d` module, Edit/Button/ComboBox/Tab/Menu/Tooltip/Splitter, ListView row painter + multi-select + no-alloc source, TreeView refresh/select/collapse, capture | M0 |
| M5 | `litehtml-view-d2d` + `WebViewControl`, with a standalone example rendering the fixtures side by side with egui | M3, M4 |
| M6 | esmail-win32 **read-only**: accounts, tree, list, reading pane, links, selection/copy, remote images, attachments save/open, search | M2, M5 |
| M7 | Write paths: flags, delete/archive, bulk actions, compose windows, drafts/outbox, settings, OAuth, tray/toasts via the listener | M6 |
| M8 | Parity pass, dark/high-contrast/DPI/accessibility review, size and startup measurements; decide the default Windows frontend and installer | M7 |

M0/M1/M3 can proceed in parallel, and each maps to a handful of dispatchable
issues.

---

## 7. Risks and open questions

- **Dark mode for common controls** is only partly documented. `SetWindowTheme("DarkMode_Explorer")`
  is fine; dark menus and the dark context-menu theme need the undocumented
  `uxtheme` ordinals (`SetPreferredAppMode`) or owner-drawn menus. Recommend
  owner-drawn popup menus in win32ui, so there are no undocumented APIs.
- **Accessibility of the webview**: a UI Automation text provider is real
  work, but without it screen readers see an empty pane. It needs scheduling
  (post-M8 at the latest), not dropping.
- **Two frontends, one core.** Maintenance cost is the main long-term risk.
  `AppCore` keeping *all* behaviour, with frontends as thin views, is what
  keeps it affordable. A frontend PR that adds logic instead of an `Intent`
  is the smell to review for.
- **Measurement drift** between litehtml's font model and DirectWrite.
  `FontDescription` weight/style/decoration mapping needs its own fixture
  tests, like `fonts::tests` for egui.
- **Open: should the egui frontend survive M8?** It remains the Linux and
  macOS story. The recommendation is to keep it, since the core split makes
  it cheap.
- **Open: where should `WebViewControl` live?** It can live in
  `litehtml-view-d2d` (esMail workspace) now, and move under win32ui as an
  optional `litehtml` feature once both APIs are stable. win32ui itself should
  not depend on litehtml by default.
- **Open: one executable or two?** Separate `esmail.exe` builds per frontend
  are simplest. The installer ships the Win32 one on Windows once M8 passes.
