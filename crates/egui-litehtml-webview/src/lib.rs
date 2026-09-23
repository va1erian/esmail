//! `egui-litehtml-webview` -- a reusable egui widget that renders HTML/CSS
//! message bodies via [litehtml](https://github.com/litehtml/litehtml)
//! (through the `va1erian/litehtml-rs` Rust bindings).
//!
//! # Why litehtml
//!
//! No legitimate mail client executes JavaScript in HTML email, so a full
//! JS-capable browser engine is more than laying out message bodies needs.
//! litehtml is a JS-less HTML/CSS layout and rendering engine, which keeps
//! the resulting binary small.
//!
//! # Design: a render worker thread
//!
//! litehtml's parse + layout is slow on real-world newsletter HTML (several
//! seconds for some marketing mail) and fetching remote images is network
//! I/O, so **none of it runs on the UI thread**. Each [`WebView`] owns one
//! background thread (the "worker") that owns the painter engine (see
//! `painter.rs`) outright -- it is `!Send` (it holds `Rc`s), so it is created
//! *on* the worker and never crosses a thread boundary. The UI thread only
//! ever:
//!
//! * sends the worker a `Render` job after a `load`/`reload`/resize, and
//! * receives finished outputs -- display lists -- in [`WebView::show`]. The
//!   worker calls `Context::request_repaint` when it has something to show.
//!
//! Render jobs carry a monotonically increasing id. The worker drops
//! superseded render jobs from its queue and re-checks the id between
//! stages, so dragging the window edge (a job per width) or clicking through
//! messages quickly never queues up seconds of stale layout work; the UI
//! likewise ignores frames whose id is not the newest.
//!
//! # Painting with egui's own painter
//!
//! The worker lays the page out with litehtml, measuring text with egui's own
//! font stack (system fonts are found with `fontdb`, see `fonts.rs`), and
//! *records a display list* -- rects, gradient meshes, glyph runs, image quads,
//! clips. The UI thread paints that list with `egui::Painter` every frame,
//! culled to the visible region. There is no bitmap canvas, no tiling and no
//! texture upload of the page, and text is crisp at any DPI.
//!
//! # No persisted `litehtml::Document`
//!
//! `litehtml::Document<'a>` borrows its `DocumentContainer` mutably for the
//! `Document`'s own lifetime, which makes storing both as sibling fields a
//! self-referential-struct problem. The worker sidesteps that: it stores only
//! the container (which holds the fonts and decoded images, reused across
//! jobs) and builds a `Document` fresh for each pass, dropping it straight
//! after. The decoded images have no eviction API of their own, so the worker
//! drops and rebuilds the whole engine once it has decoded past its budget:
//! rare, but it keeps a long-lived view from accumulating one texture per
//! image ever shown.
//!
//! # Text selection without a `Document`
//!
//! Selecting text needs the page's text and geometry long after the
//! `Document` is gone, so the worker records them while it is alive: every
//! draw pass walks the laid-out document once and sends a [`TextRunTable`]
//! (one run per word: box, text, per-character x offsets, containing block,
//! forced line breaks) with the frame. Hit-testing, dragging, double/triple
//! click, the highlight (painted by egui over the page, never into it) and
//! the copied text are all plain geometry over that table on the UI thread,
//! so nothing re-lays-out per pointer move. A selection is two carets (run
//! index + character); it survives a re-layout of the same text (resize,
//! images arriving) and is dropped when a different page loads. Ctrl+C and
//! Ctrl+A act only while the view has egui focus, so text fields keep theirs.
//!
//! # Links without a `Document`
//!
//! Which link is under the pointer is answered the same way. The draw pass
//! also records a [`LinkTable`] (every `<a href>` with its per-line boxes and
//! the blocks and images inside it), and the UI thread resolves clicks and the
//! hand cursor by point-in-rectangle lookup: instant, with no second parse +
//! layout per click (issue #27).
//!
//! # Render sequence (one `Render` job)
//!
//! 1. **Record**: build a `Document`, `render()` it at the requested width,
//!    `draw()` it into a fresh display list.
//! 2. **Discover images**: URLs are only known after the layout has walked
//!    the document. `data:` URIs are decoded locally (litehtml has no
//!    network layer; `esmail` inlines `cid:` parts as `data:` URIs first);
//!    everything else goes to [`WebViewHandler::intercept`], several at a
//!    time. If remote images are involved, the text-only list from step 1 is
//!    sent right away so the message is readable while images arrive.
//! 3. **Record again** with the images loaded (an image can change layout, so
//!    this is a full pass from scratch), and send that list. Repeats if the
//!    redraw turns up more URLs, up to a small limit.
//!
//! # Sanitization stays the host's job
//!
//! litehtml's `email` feature includes its own `prepare_html`/
//! `prepare_email_html` pipeline with script-stripping sanitization. This
//! crate does not use it. `esmail`'s `render.rs` already runs the message
//! through `ammonia` (a dedicated, well-audited HTML sanitizer) before any
//! of this crate's code sees it, and that stays the real trust boundary --
//! defense in depth, not replaced by litehtml's own safety net, per the
//! approved migration plan.

#![warn(missing_docs)]

pub use url;

mod fonts;
mod painter;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use litehtml::html::decode_data_uri;

mod selection;
mod links;
mod text_runs;
pub use selection::{Selection, TextPos};
pub use links::{Link, LinkTable};
pub use text_runs::{TextRun, TextRunTable};

/// How many image URLs the worker fetches at the same time.
const MAX_PARALLEL_FETCHES: usize = 8;

/// Upper bound on draw passes for one render job. Pass 1 discovers image
/// URLs, pass 2 draws with them loaded; a third covers URLs only discovered
/// once the images' real sizes changed the layout. Anything past that is a
/// pathological document and is shown as-is.
const MAX_PASSES: usize = 3;

// ─── Public API types ───────────────────────────────────────────────────────

/// What to load in the webview.
///
/// Only an in-memory HTML string is supported. There are no `Url` or
/// `HtmlWithBase` variants: litehtml has no network layer of its own by
/// design, so there is nothing "navigate to a URL" could mean at this layer -- the one
/// caller that wants that (`esmail`'s `ESMAIL_PREVIEW=<http url>` dev path)
/// fetches synchronously with `ureq` and hands the result in as `Html`
/// instead. Nothing in `esmail` today uses relative links/resources against
/// a non-trivial base, so `HtmlWithBase` was dropped rather than ported
/// speculatively; both can come back if something real needs them.
#[derive(Clone)]
pub enum WebViewSource {
    /// Render an in-memory HTML string.
    Html(String),
}

/// Events emitted by [`WebView::show`].
#[derive(Debug, Clone)]
pub enum WebViewEvent {
    /// The user clicked a link (an `<a href>`). litehtml has no navigation
    /// concept of its own -- there is nothing to allow/deny -- so every
    /// anchor click unconditionally becomes this event; the host is always
    /// the one that decides what to do with it (open in the system browser,
    /// etc.).
    ///
    /// Resolved on the UI thread from the frame's [`LinkTable`], so it is
    /// returned by the [`WebView::show`] call that saw the click.
    LinkClicked(String),
}

/// One resource load litehtml's layout discovered it wants: an image `src`
/// found while parsing/laying out the document. Deliberately minimal --
/// litehtml hands this crate just a URL string per pending image, not a
/// full HTTP request object, so there is no request method/headers/redirect
/// info to carry here.
pub struct ImageRequest {
    /// The image URL as it appeared in the document's markup (already
    /// resolved from `cid:` to a `data:` URL upstream by `esmail`'s
    /// `render.rs`, for any part that had a match -- see the crate's module
    /// doc. `data:` URLs never reach [`WebViewHandler::intercept`] at all;
    /// this crate decodes those itself. Only `http(s)` (or any other
    /// non-local scheme) URLs are handed to the handler.
    pub url: String,
}

/// What [`WebViewHandler::intercept`] decided to do with one pending image.
pub enum InterceptOutcome {
    /// Do not fetch this image. Functionally identical to [`Self::Block`]
    /// today (this crate has no network layer of its own to fall back to),
    /// kept as a distinct outcome for symmetry with the request/response
    /// shape and in case a default fetcher is ever added later.
    Allow,
    /// Do not fetch this image; it is simply left unloaded (no broken-image
    /// placeholder is drawn -- litehtml just never gets pixels for it).
    Block,
    /// Serve these bytes as the image's data, fetched however the host saw
    /// fit (e.g. `esmail`'s `MessageViewHandler` uses `ureq` once the user
    /// has clicked "Load remote images" -- see B5 in PLAN.md).
    Serve(Vec<u8>),
}

/// Host-supplied policy for which images a [`WebView`] is allowed to load.
///
/// Called **on the worker thread**, possibly from several worker-spawned
/// threads at once (up to [`MAX_PARALLEL_FETCHES`]) -- which is why it is
/// `Send + Sync`, takes `&self`, and is free to block on network I/O (that
/// is the whole point of it not running on the UI thread). Anything the
/// host wants to change while a view is alive (e.g. an "allow remote
/// images" switch) needs interior mutability, such as an `AtomicBool`.
///
/// There is no `navigation` method: litehtml has no navigation concept at all
/// (see [`WebViewEvent::LinkClicked`]'s doc), so there is nothing to decide
/// there.
pub trait WebViewHandler: Send + Sync {
    /// Called for every image URL the document's layout wants loaded, other
    /// than `data:` URLs (decoded locally, never reaching this hook -- see
    /// [`ImageRequest::url`]). Defaults to [`InterceptOutcome::Allow`] (no
    /// fetch), matching this crate having no default image fetcher.
    fn intercept(&self, request: &ImageRequest) -> InterceptOutcome {
        let _ = request;
        InterceptOutcome::Allow
    }
}

/// The [`WebViewHandler`] used when a [`WebViewConfig`] does not supply one:
/// no image is ever fetched.
struct DefaultHandler;
impl WebViewHandler for DefaultHandler {}

// ─── WebViewHost ─────────────────────────────────────────────────────────────

/// Creates [`WebView`]s.
///
/// Views need no engine to own, no window handle and no GL context, only their
/// `egui::Context` (the fonts a page uses are installed into it, next to
/// whatever the host set up; the names added are unique per view, so any number
/// of views can share one context). This type still exists (rather than a bare
/// associated function on `WebView`) to keep the call shape `esmail`'s `main.rs` already uses -- one host per
/// window, producing any number of views -- even though today it is little
/// more than an id counter so two views in the same window don't collide on
/// one egui texture name.
#[derive(Default)]
pub struct WebViewHost {
    next_view_id: std::cell::Cell<u64>,
}

impl WebViewHost {
    /// Create a host. Takes nothing: views need nothing from the host window
    /// (no window handle, no GL context).
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new view showing `config.source`, and start its worker
    /// thread. `ctx` is cloned into the worker so it can wake the UI when a
    /// frame is ready.
    pub fn new_view(&self, ctx: &egui::Context, config: WebViewConfig) -> WebView {
        let view_id = self.next_view_id.get();
        self.next_view_id.set(view_id + 1);

        let WebViewSource::Html(html) = config.source;
        let handler: Arc<dyn WebViewHandler> = config.handler.unwrap_or_else(|| Arc::new(DefaultHandler));

        let (job_tx, job_rx) = mpsc::channel();
        let (out_tx, out_rx) = mpsc::channel();
        let latest_id = Arc::new(AtomicU64::new(0));

        let worker_ctx = ctx.clone();
        let worker_latest = latest_id.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("litehtml-worker-{view_id}"))
            .spawn(move || {
                // Created here, not on the caller's thread: the engine is
                // !Send. (The engine itself is created lazily, on the first
                // job: loading system fonts is slow enough that it should not
                // block the UI at startup either.)
                Worker::new(worker_ctx, handler, out_tx, worker_latest).run(job_rx);
            });
        if let Err(e) = &spawned {
            log::error!("egui-litehtml-webview: could not start the render thread: {e}");
        }

        WebView {
            html: Arc::new(html),
            tx: job_tx,
            rx: out_rx,
            latest_id,
            submitted_id: 0,
            list: None,
            fonts_requested: None,
            scroll_id: format!("egui_litehtml_webview_{view_id}"),
            frame_size: egui::Vec2::ZERO,
            frame_layout_width: 1.0,
            frame_scale: 1.0,
            requested_width: 1.0,
            requested_dpi: ctx.pixels_per_point(),
            dirty: true,
            reset_images: true,
            rendering: false,
            failed: spawned.is_err(),
            runs: Arc::default(),
            runs_signature: 0,
            links: Arc::default(),
            click_events: Vec::new(),
            selection: None,
            last_render: None,
            scroll_y: 0.0,
            pending_scroll: None,
            content_shown: false,
        }
    }
}

// ─── WebView ─────────────────────────────────────────────────────────────────

/// How a view should start up.
pub struct WebViewConfig {
    /// The page to load first.
    pub source: WebViewSource,
    /// Which images this view is allowed to load. `None` uses
    /// [`DefaultHandler`]'s behaviour: no image is ever fetched.
    pub handler: Option<Arc<dyn WebViewHandler>>,
}

impl WebViewConfig {
    /// A config that loads `source`, with the default (no images fetched)
    /// policy.
    pub fn new(source: WebViewSource) -> Self {
        Self { source, handler: None }
    }

    /// Use `handler` for this view's image-loading decisions instead of the
    /// default policy.
    pub fn with_handler(mut self, handler: Arc<dyn WebViewHandler>) -> Self {
        self.handler = Some(handler);
        self
    }
}

/// One embedded HTML view, drawn with [`WebView::show`].
///
/// Created by [`WebViewHost::new_view`]. Holds only UI-thread state: the
/// heavy lifting happens on the worker thread it talks to over channels (see
/// the crate module doc).
pub struct WebView {
    /// The HTML currently loaded. Shared with the worker's jobs.
    html: Arc<String>,
    tx: Sender<Job>,
    rx: Receiver<Output>,
    /// Id of the newest render job, shared with the worker so it can notice
    /// (between stages) that the job it is running has been superseded.
    latest_id: Arc<AtomicU64>,
    /// Id of the newest render job this view submitted; frames with any
    /// other id are stale and ignored.
    submitted_id: u64,
    /// The current display list, if a frame has arrived.
    list: Option<painter::ListFrame>,
    /// The font definitions last handed to the context, and the pass they were
    /// handed over in (see [`WebView::ensure_fonts`]).
    fonts_requested: Option<(Arc<egui::epaint::text::FontDefinitions>, u64)>,
    /// Unique per view, so two views cannot collide on one egui id.
    scroll_id: String,
    /// What the current frame should be displayed as, in egui points.
    frame_size: egui::Vec2,
    /// The layout width the current frame was rendered at, in points --
    /// hit tests must be laid out at the same width to line up.
    frame_layout_width: f32,
    /// `pixels_per_point` the current frame was rendered at.
    frame_scale: f32,
    /// The width/DPI of the newest render job submitted, to notice when the
    /// widget has since changed size.
    requested_width: f32,
    requested_dpi: f32,
    /// Set by [`WebView::load`]/[`WebView::reload`]; cleared by submitting a
    /// render job on the next `show()`.
    dirty: bool,
    /// The next render job must forget which image URLs were already
    /// requested -- set by `load`/`reload` (see their docs).
    reset_images: bool,
    /// A render job is in flight.
    rendering: bool,
    /// The worker reported that the document could not be rendered at all.
    failed: bool,
    /// Where the text is in the current frame (see [`TextRunTable`]).
    runs: Arc<TextRunTable>,
    /// Identifies the text `runs` holds (not where it is), so a selection can
    /// be kept when only the layout changed.
    runs_signature: u64,
    /// Where the links are in the current frame (see [`LinkTable`]).
    links: Arc<LinkTable>,
    /// Events raised while drawing (link clicks), returned by `show`.
    click_events: Vec<WebViewEvent>,
    /// The selected text, as carets into `runs`.
    selection: Option<Selection>,
    /// How long the worker took over the newest finished render job.
    last_render: Option<Duration>,
    /// Vertical scroll offset as of the last `show()`, points.
    scroll_y: f32,
    /// Scroll offset to apply on the next `show()`.
    pending_scroll: Option<f32>,
    /// The current page has been allocated in the scroll area at least once.
    content_shown: bool,
}

impl WebView {
    // ── Public API ──────────────────────────────────────────────────────────

    /// Load a new source, replacing whatever is currently shown. Triggers a
    /// fresh render on the next [`WebView::show`].
    ///
    /// The previous page is dropped immediately (a "Rendering..."
    /// indicator is shown until the first frame of the new page arrives)
    /// rather than left on screen: a slow render would otherwise show the
    /// *previous* message under the *new* message's headers for seconds.
    pub fn load(&mut self, source: WebViewSource) {
        let WebViewSource::Html(html) = source;
        self.html = Arc::new(html);
        self.discard_frames();
        self.runs = Arc::default();
        self.runs_signature = 0;
        self.links = Arc::default();
        self.click_events.clear();
        self.selection = None;
        self.failed = false;
        // Invalidate whatever the worker is (or has just finished) rendering
        // for the *previous* page right now, not when `show()` next submits
        // the new job: a frame for the old page that lands in between would
        // otherwise still match `submitted_id` and get displayed under the
        // new page's headers. This also lets the worker abandon the old job
        // sooner.
        self.submitted_id += 1;
        self.latest_id.store(self.submitted_id, Ordering::SeqCst);
        // New page: image URLs from the old one should not suppress
        // re-discovery, even if by coincidence a URL string repeats.
        self.reset_images = true;
        self.dirty = true;
    }

    /// Re-run the render sequence for the currently-loaded HTML, keeping the
    /// current frame on screen until the new one is ready.
    ///
    /// Used by `esmail`'s "Load remote images" button: the HTML itself never
    /// loses its original `http(s)` URLs (B5 in PLAN.md), so once the
    /// handler starts allowing them, a `reload()` against the same document
    /// is what actually re-requests them -- forgetting which URLs were
    /// already requested is required for that, since without it every URL
    /// would still be marked "already requested" from the blocked first pass.
    pub fn reload(&mut self) {
        self.reset_images = true;
        self.dirty = true;
    }

    /// How long the worker took over the newest finished render (parse +
    /// layout + record, image passes included), or `None` before the first
    /// one.
    pub fn last_render_time(&self) -> Option<Duration> {
        self.last_render
    }

    /// The vertical scroll offset, in points, as of the last [`WebView::show`].
    pub fn scroll_offset(&self) -> f32 {
        self.scroll_y
    }

    /// Scroll to `y` points on the next [`WebView::show`] that has a page to
    /// scroll (so it may be called before the first render has finished).
    pub fn set_scroll_offset(&mut self, y: f32) {
        self.pending_scroll = Some(y);
    }

    /// Whether the worker has (or is about to have) work outstanding for
    /// the current page. Useful for hosts that want to wait for the page to
    /// settle, e.g. before taking a screenshot.
    pub fn is_rendering(&self) -> bool {
        self.rendering || self.dirty
    }

    /// The size, in egui points, of the page as last rendered, or `None`
    /// until the first frame has arrived. Its height is the document's
    /// content height at the width it was laid out at.
    pub fn content_size(&self) -> Option<egui::Vec2> {
        self.has_frame().then_some(self.frame_size)
    }

    /// Where the text of the frame currently shown is, in document points
    /// (the frame's own coordinate space, origin at its top-left). Empty until
    /// the first frame arrives.
    pub fn text_runs(&self) -> &TextRunTable {
        &self.runs
    }

    /// Draw the view into `ui` (inside its own scroll area) and return any
    /// queued [`WebViewEvent`]s.
    ///
    /// Call once per frame. Never blocks on layout or the network: it only
    /// collects whatever the worker has finished since the last call, and
    /// hands it new work when the page or the widget's width/DPI changed.
    pub fn show(&mut self, ui: &mut egui::Ui) -> Vec<WebViewEvent> {
        self.poll_worker();

        let dpi = ui.ctx().pixels_per_point();
        let avail_width = ui.available_width().max(1.0);
        if self.dirty
            || (avail_width - self.requested_width).abs() > 0.5
            || (dpi - self.requested_dpi).abs() > 0.001
        {
            self.submit_render(avail_width, dpi);
        }

        // The whole document is rendered up front at its full content
        // height -- unlike the Servo-backed predecessor, which had to poll a
        // JS bridge and paint a hand-rolled overlay scrollbar (issue #15)
        // because Servo exposed no scroll-position getter/setter at all. A
        // plain `ScrollArea` around a normally-sized allocation gets a native
        // scrollbar and native wheel-scroll for free.
        let mut area = egui::ScrollArea::vertical().id_salt(&self.scroll_id);
        // Held back until the page has been laid out in the scroll area on an
        // earlier frame: with no content there egui would clamp the offset to 0
        // and the request would be lost. (A frame can have a picture yet still
        // show a placeholder -- the view waits for the frame's fonts.)
        if self.content_shown
            && let Some(y) = self.pending_scroll.take()
        {
            area = area.vertical_scroll_offset(y);
        }
        let output = area.show(ui, |ui| self.show_page(ui));
        self.scroll_y = output.state.offset.y;

        std::mem::take(&mut self.click_events)
    }

    /// The selected text, as a copy should read, or `None` if nothing is
    /// selected.
    pub fn selected_text(&self) -> Option<String> {
        let sel = self.selection.filter(|s| !s.is_empty())?;
        Some(self.runs.selection_text(&sel)).filter(|t| !t.is_empty())
    }

    /// Whether any text is selected.
    pub fn has_selection(&self) -> bool {
        self.selected_text().is_some()
    }

    /// Select all the text of the page.
    pub fn select_all(&mut self) {
        self.selection = self.runs.select_all();
    }

    /// Drop the selection.
    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    // ── Private: selection ───────────────────────────────────────────────

    /// Draw the highlight over the page. It is painted by egui on top of the
    /// display list, not into it, so moving the selection never re-renders the
    /// page. Points in the run table are document points, which are also egui
    /// points relative to the page's corner.
    fn paint_selection(&self, ui: &egui::Ui, rect: egui::Rect) {
        let Some(sel) = self.selection.filter(|s| !s.is_empty()) else {
            return;
        };
        let clip = ui.clip_rect();
        // The page is always white, so a translucent fill keeps the text
        // legible underneath, whatever the app theme.
        let color = ui.visuals().selection.bg_fill.gamma_multiply(0.55);
        for r in self.runs.selection_rects(&sel) {
            let r = r.translate(rect.min.to_vec2());
            if r.intersects(clip) {
                ui.painter().rect_filled(r, 0.0, color);
            }
        }
    }

    /// Pointer and keyboard handling for the page at `rect`.
    fn interact(&mut self, ui: &mut egui::Ui, resp: &egui::Response, rect: egui::Rect) {
        let to_doc = |pos: egui::Pos2| (pos - rect.min).to_pos2();
        let shift = ui.input(|i| i.modifiers.shift);

        if resp.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
        } else if resp.hovered()
            && let Some(p) = ui.input(|i| i.pointer.hover_pos()).map(to_doc)
        {
            // A link is a hand even where it is text (or an image, which is
            // not text at all); other text is an I-beam.
            if self.links.href_at(p).is_some() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            } else if self.runs.is_text_at(p) {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
            }
        }

        // A drag that starts anywhere (a link included) selects.
        if resp.drag_started_by(egui::PointerButton::Primary) {
            resp.request_focus();
            // egui reports a drag only once the pointer has moved a few points,
            // so the anchor is where the button went down, not where it is.
            let origin = ui.input(|i| i.pointer.press_origin()).or(resp.interact_pointer_pos());
            if let Some(at) = origin.and_then(|p| self.runs.pos_at(to_doc(p))) {
                match self.selection {
                    Some(sel) if shift => self.selection = Some(Selection { anchor: sel.anchor, head: at }),
                    _ => self.selection = Some(Selection::caret(at)),
                }
            }
        } else if resp.dragged_by(egui::PointerButton::Primary) {
            if let Some(pos) = ui.input(|i| i.pointer.latest_pos()) {
                if let (Some(sel), Some(at)) = (self.selection, self.runs.pos_at(to_doc(pos))) {
                    self.selection = Some(Selection { anchor: sel.anchor, head: at });
                    // Dragging past the top or bottom edge keeps scrolling:
                    // bring the caret's line into view. The pointer stays put
                    // while the page moves, so ask for another frame.
                    let clip = ui.clip_rect();
                    if pos.y < clip.min.y || pos.y > clip.max.y {
                        let line = self.runs.runs[at.run].rect.translate(rect.min.to_vec2());
                        ui.scroll_to_rect(line.expand2(egui::vec2(0.0, line.height())), None);
                        ui.ctx().request_repaint();
                    }
                }
            }
        }

        if resp.clicked_by(egui::PointerButton::Primary) {
            resp.request_focus();
            if resp.triple_clicked() {
                self.select_at(resp, rect, |runs, p| runs.block_at(p));
            } else if resp.double_clicked() {
                self.select_at(resp, rect, |runs, p| runs.word_at(p));
            } else if shift && self.selection.is_some() {
                // Shift-click extends from where the selection began.
                if let (Some(sel), Some(at)) = (
                    self.selection,
                    resp.interact_pointer_pos().and_then(|p| self.runs.pos_at(to_doc(p))),
                ) {
                    self.selection = Some(Selection { anchor: sel.anchor, head: at });
                }
            } else {
                self.selection = None;
                if let Some(href) = resp.interact_pointer_pos().and_then(|p| self.links.href_at(to_doc(p))) {
                    self.click_events.push(WebViewEvent::LinkClicked(href.to_string()));
                }
            }
        }

        // A click anywhere else takes the selection with it.
        if ui.input(|i| i.pointer.any_pressed()) && !resp.contains_pointer() && !resp.context_menu_opened() {
            self.selection = None;
        }

        // Keyboard: only while this view has focus, so the search box and the
        // compose fields keep their own Ctrl+C / Ctrl+A.
        if resp.has_focus() {
            if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::A)) {
                self.select_all();
            }
            let copy = ui.input_mut(|i| {
                let n = i.events.len();
                i.events.retain(|e| !matches!(e, egui::Event::Copy));
                i.events.len() != n
            });
            if copy {
                self.copy_selection(ui.ctx());
            }
        }

        resp.context_menu(|ui| {
            if ui.add_enabled(self.has_selection(), egui::Button::new("Copy")).clicked() {
                self.copy_selection(ui.ctx());
                ui.close();
            }
            if ui.button("Select all").clicked() {
                self.select_all();
                ui.close();
            }
        });
    }

    /// Select whatever `pick` chooses at the pointer.
    fn select_at(
        &mut self,
        resp: &egui::Response,
        rect: egui::Rect,
        pick: impl Fn(&TextRunTable, egui::Pos2) -> Option<Selection>,
    ) {
        if let Some(p) = resp.interact_pointer_pos() {
            self.selection = pick(&self.runs, (p - rect.min).to_pos2());
        }
    }

    /// Put the selection on the clipboard.
    fn copy_selection(&self, ctx: &egui::Context) {
        if let Some(text) = self.selected_text() {
            ctx.copy_text(text);
        }
    }

    // ── Private: painting ───────────────────────────────────────────────────

    /// Whether there is a display list to show.
    fn has_frame(&self) -> bool {
        self.list.is_some()
    }

    fn discard_frames(&mut self) {
        self.list = None;
        self.fonts_requested = None;
        self.last_render = None;
        self.content_shown = false;
    }

    /// What to show while there is nothing to paint yet.
    fn show_placeholder(&self, ui: &mut egui::Ui) {
        if self.failed {
            ui.label("Could not render this message.");
        } else if self.rendering {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Rendering...");
            });
        }
    }

    /// Replay the display list, culled to what is visible.
    fn show_page(&mut self, ui: &mut egui::Ui) {
        let Some((list, defs)) = self.list.as_ref().map(|f| (f.list.clone(), f.defs.clone())) else {
            self.show_placeholder(ui);
            return;
        };
        if !self.ensure_fonts(ui.ctx(), &list.families, &defs) {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Loading fonts...");
            });
            return;
        }
        let (rect, resp) = ui.allocate_exact_size(list.size, egui::Sense::click_and_drag());
        self.content_shown = true;
        // litehtml only paints where CSS says to; a browser canvas is white.
        ui.painter().rect_filled(rect, 0.0, egui::Color32::WHITE);
        painter::paint(&list, ui.painter(), rect.min);
        self.paint_selection(ui, rect);
        self.interact(ui, &resp, rect);
    }

    /// egui panics when asked to lay out a font family it does not know, and
    /// new fonts only reach it at the start of the next pass. So the list is
    /// only painted once the context reports every family it uses; until then
    /// this hands the worker's fonts over and asks for another frame.
    fn ensure_fonts(
        &mut self,
        ctx: &egui::Context,
        families: &[egui::epaint::text::FontFamily],
        defs: &Arc<egui::epaint::text::FontDefinitions>,
    ) -> bool {
        if painter::fonts_ready(ctx, defs, families) {
            return true;
        }
        let pass = ctx.cumulative_pass_nr();
        // Install once per set of definitions -- but again if they still have
        // not shown up a few passes later (the host may have replaced the
        // context's fonts in between).
        let stale = self.fonts_requested.as_ref().is_none_or(|(d, at)| !Arc::ptr_eq(d, defs) || pass > at + 3);
        if stale {
            painter::install_fonts(ctx, defs);
            self.fonts_requested = Some((defs.clone(), pass));
        }
        ctx.request_repaint();
        false
    }

    // ── Private: talking to the worker ───────────────────────────────────

    /// Queue a render of the current HTML at `width` logical points.
    fn submit_render(&mut self, width: f32, dpi: f32) {
        self.submitted_id += 1;
        // Publish the new id before the job itself, so a render already in
        // progress can notice it has been superseded as early as possible.
        self.latest_id.store(self.submitted_id, Ordering::SeqCst);
        let job = RenderJob {
            id: self.submitted_id,
            html: self.html.clone(),
            width,
            scale: dpi,
            reset_images: std::mem::take(&mut self.reset_images),
        };
        if self.tx.send(Job::Render(job)).is_err() {
            log::error!("egui-litehtml-webview: the render thread is gone");
            self.failed = true;
            self.rendering = false;
        } else {
            self.rendering = true;
        }
        self.requested_width = width;
        self.requested_dpi = dpi;
        self.dirty = false;
    }

    /// Take the text and link tables of a new frame.
    fn accept_runs(&mut self, runs: Arc<TextRunTable>, links: Arc<LinkTable>) {
        self.runs = runs;
        self.links = links;
        // A new layout of the same text (a resize or images arriving) keeps
        // the selection: carets are run indexes, and the
        // same words come out in the same order. Different text (another
        // message) cannot.
        let signature = self.runs.text_signature();
        if signature != self.runs_signature {
            self.selection = None;
            self.runs_signature = signature;
        }
    }

    /// Apply everything the worker has produced so far.
    fn poll_worker(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(Output::List(frame)) if frame.id == self.submitted_id => {
                    self.frame_size = frame.list.size;
                    self.frame_layout_width = frame.layout_width;
                    self.frame_scale = frame.scale;
                    self.accept_runs(frame.runs.clone(), frame.links.clone());
                    self.list = Some(frame);
                }
                Ok(Output::Stats { id, elapsed }) if id == self.submitted_id => self.last_render = Some(elapsed),
                Ok(Output::Done { id, ok }) if id == self.submitted_id => {
                    self.rendering = false;
                    self.failed = !ok && !self.has_frame();
                }
                // A frame/completion for a superseded job.
                Ok(_) => {}
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if self.rendering {
                        log::error!("egui-litehtml-webview: the render thread died mid-render");
                        self.rendering = false;
                        self.failed = !self.has_frame();
                    }
                    break;
                }
            }
        }
    }
}

// ─── Worker protocol ─────────────────────────────────────────────────────────

/// UI thread -> worker.
enum Job {
    Render(RenderJob),
}

struct RenderJob {
    id: u64,
    html: Arc<String>,
    /// Layout width, egui points.
    width: f32,
    /// egui `pixels_per_point`.
    scale: f32,
    /// Forget which image URLs were already requested before starting.
    reset_images: bool,
}

/// Worker -> UI thread.
enum Output {
    /// A finished (or intermediate) display list of the page.
    List(painter::ListFrame),
    /// How long render job `id` took overall. Sent just before its `Done`.
    Stats { id: u64, elapsed: Duration },
    /// The render job `id` has nothing more to send. `ok` is false when the
    /// document could not be rendered at all.
    Done { id: u64, ok: bool },
}

// ─── Worker ──────────────────────────────────────────────────────────────────

/// Most raw image bytes [`Worker::fetched`] keeps before it starts over.
const FETCHED_CACHE_LIMIT: usize = 64 * 1024 * 1024;

/// Most raw image bytes the worker will decode into one engine before dropping
/// it -- and with it every decoded texture -- and building a fresh one. The
/// engine's decoded-image cache has no eviction API of its own (see
/// `painter.rs`), and its decoded size is not visible from here, so the bytes
/// handed to `load_image_data` stand in for it; left alone the cache grows for
/// as long as the view lives, one texture per image URL ever shown. Rebuilding
/// also re-discovers system fonts, so this is deliberately generous: a backstop
/// against a long session, not a per-message cost.
const DECODED_IMAGE_LIMIT: usize = 64 * 1024 * 1024;

/// Everything that lives on the worker thread.
struct Worker {
    /// Created on first use, on this thread (it is `!Send`).
    painter: Option<painter::PainterEngine>,
    handler: Arc<dyn WebViewHandler>,
    ctx: egui::Context,
    out: Sender<Output>,
    /// See [`WebView::latest_id`].
    latest_id: Arc<AtomicU64>,
    /// Cumulative count of `load_image_data` calls -- diagnostic only,
    /// logged per job. The engine's decoded-image cache has no eviction API,
    /// so this is a proxy for how large it has grown.
    total_images_loaded: u64,
    /// The previous render job was abandoned (superseded) after litehtml had
    /// already recorded its image URLs as requested. Those URLs would then
    /// never be requested again -- the container skips URLs it has seen --
    /// so the next job must forget them, or a resize mid-load leaves the
    /// message permanently missing images.
    reset_images_next: bool,
    /// Raw bytes of every remote image fetched so far, so that opening a page
    /// again does not download its images a second time.
    fetched: HashMap<String, Arc<Vec<u8>>>,
    fetched_bytes: usize,
    /// Raw image bytes handed to the engine since it was built, the worker's
    /// proxy for the size of its decoded-image cache (see
    /// [`DECODED_IMAGE_LIMIT`]). Starts over whenever the engine is dropped.
    decoded_bytes: usize,
}

impl Worker {
    fn new(
        ctx: egui::Context,
        handler: Arc<dyn WebViewHandler>,
        out: Sender<Output>,
        latest_id: Arc<AtomicU64>,
    ) -> Self {
        Self {
            painter: None,
            handler,
            ctx,
            out,
            latest_id,
            total_images_loaded: 0,
            reset_images_next: false,
            fetched: HashMap::new(),
            fetched_bytes: 0,
            decoded_bytes: 0,
        }
    }

    /// The engine, created on first use. That first use is where system fonts
    /// get loaded.
    fn engine(&mut self) -> &mut painter::PainterEngine {
        self.painter.get_or_insert_with(|| painter::PainterEngine::new(&self.ctx))
    }

    /// Serve jobs until the [`WebView`] (the only sender) is dropped.
    fn run(mut self, jobs: Receiver<Job>) {
        while let Ok(first) = jobs.recv() {
            // Everything queued while the last job ran: only the newest
            // render matters (the UI ignores older ones' frames anyway).
            let mut render: Option<RenderJob> = None;
            let mut reset_images = false;
            for job in std::iter::once(first).chain(jobs.try_iter()) {
                match job {
                    Job::Render(r) => {
                        // A dropped job may have been the one asking to
                        // forget requested URLs (a `load`); its replacement
                        // (say, a resize) must still do that.
                        reset_images |= r.reset_images;
                        render = Some(r);
                    }
                }
            }
            if let Some(mut r) = render {
                r.reset_images = reset_images;
                self.render(&r);
            }
        }
    }

    fn superseded(&self, id: u64) -> bool {
        self.latest_id.load(Ordering::SeqCst) != id
    }

    fn send(&self, output: Output) {
        // Only fails when the view is gone, in which case nobody cares.
        let _ = self.out.send(output);
        self.ctx.request_repaint();
    }

    /// Run one render job; see the crate module doc for the sequence.
    fn render(&mut self, job: &RenderJob) {
        let t_total = Instant::now();
        if self.evict_decoded_images_if_over(DECODED_IMAGE_LIMIT) {
            log::debug!("decoded images passed {DECODED_IMAGE_LIMIT} bytes; dropped the engine");
        }
        if job.reset_images || std::mem::take(&mut self.reset_images_next) {
            self.engine().clear_pending_images();
        }
        let width = job.width.max(1.0);
        let scale = job.scale.max(0.1);

        let mut passes = 0;
        let mut ok = true;
        let mut fetched = 0usize;
        while passes < MAX_PASSES {
            if self.superseded(job.id) {
                self.reset_images_next = true;
                return;
            }
            let Some(height) = self.engine().draw_pass(&job.html, width, scale) else {
                ok = false;
                break;
            };
            passes += 1;
            // Whether this pass's picture has already gone to the UI.
            let mut emitted = false;

            let pending = self.engine().take_pending_images();
            if pending.is_empty() || passes == MAX_PASSES {
                self.emit_frame(job.id, width, scale, height);
                break;
            }

            let (local, remote): (Vec<String>, Vec<String>) =
                pending.into_iter().map(|(url, _)| url).partition(|url| url.starts_with("data:"));
            let mut loaded = self.load_images(
                local
                    .into_iter()
                    .filter_map(|url| resolve_image_bytes(&url, &*self.handler).map(|bytes| (url, Arc::new(bytes))))
                    .collect(),
            );

            // Images an earlier job already downloaded.
            let (cached, remote): (Vec<String>, Vec<String>) =
                remote.into_iter().partition(|url| self.fetched.contains_key(url));
            let cached: Vec<(String, Arc<Vec<u8>>)> =
                cached.into_iter().map(|url| { let bytes = self.fetched[&url].clone(); (url, bytes) }).collect();
            loaded |= self.load_images(cached);

            if !remote.is_empty() {
                // Let the user read the text while images download.
                self.emit_frame(job.id, width, scale, height);
                emitted = true;
                let t = Instant::now();
                let downloaded = fetch_all(remote, &*self.handler, &self.latest_id, job.id);
                if self.superseded(job.id) {
                    self.reset_images_next = true;
                    return;
                }
                fetched += downloaded.len();
                log::debug!("fetched {} remote image(s) in {:?}", downloaded.len(), t.elapsed());
                let downloaded: Vec<(String, Arc<Vec<u8>>)> =
                    downloaded.into_iter().map(|(url, bytes)| (url, Arc::new(bytes))).collect();
                self.remember_fetched(&downloaded);
                loaded |= self.load_images(downloaded);
            }

            if !loaded {
                if !emitted {
                    self.emit_frame(job.id, width, scale, height);
                }
                break;
            }
            // Images changed what there is to draw (and possibly where):
            // go around again from scratch.
        }

        let elapsed = t_total.elapsed();
        log::debug!(
            "render job {}: total={elapsed:?} passes={passes} remote_fetched={fetched} html_len={} \
             total_images_loaded={}",
            job.id, job.html.len(), self.total_images_loaded,
        );
        self.send(Output::Stats { id: job.id, elapsed });
        self.send(Output::Done { id: job.id, ok });
    }

    /// Keep the raw bytes of freshly downloaded images (see [`Worker::fetched`]).
    fn remember_fetched(&mut self, images: &[(String, Arc<Vec<u8>>)]) {
        for (url, bytes) in images {
            if self.fetched_bytes + bytes.len() > FETCHED_CACHE_LIMIT {
                self.fetched.clear();
                self.fetched_bytes = 0;
            }
            if self.fetched.insert(url.clone(), bytes.clone()).is_none() {
                self.fetched_bytes += bytes.len();
            }
        }
    }

    /// Decode `images` into the engine. Returns whether any loaded.
    fn load_images(&mut self, images: Vec<(String, Arc<Vec<u8>>)>) -> bool {
        let mut any = false;
        for (url, bytes) in images {
            self.decoded_bytes += bytes.len();
            self.engine().load_image_data(&url, &bytes);
            self.total_images_loaded += 1;
            any = true;
        }
        any
    }

    /// Drop the engine -- releasing every decoded image texture with it --
    /// once more than `limit` image bytes have been decoded into it since it
    /// was built, and start the accounting over. Returns whether it dropped
    /// one.
    ///
    /// Called between render jobs, never mid-pass: the engine must outlive the
    /// pass still using it. A single document whose own images pass `limit`
    /// is therefore rebuilt once per job rather than thrashing within one.
    fn evict_decoded_images_if_over(&mut self, limit: usize) -> bool {
        if self.decoded_bytes <= limit {
            return false;
        }
        self.painter = None;
        self.decoded_bytes = 0;
        true
    }

    /// Send the engine's current display list to the UI.
    fn emit_frame(&mut self, id: u64, width: f32, scale: f32, content_height: f32) {
        let output = self.engine().frame(id, width, scale, content_height);
        self.send(output);
    }
}

/// Decide how to resolve one pending image URL, without touching the
/// container -- pulled out so it's testable without a real
/// container/`Document`. `data:` URLs are decoded locally; a
/// surviving `cid:` URL means `esmail`'s `render.rs` found no matching part
/// and there's nothing to fetch (see [`ImageRequest::url`]'s doc); anything
/// else goes to `handler`.
fn resolve_image_bytes(url: &str, handler: &dyn WebViewHandler) -> Option<Vec<u8>> {
    if let Some(data) = decode_data_uri(url) {
        return Some(data);
    }
    if url.starts_with("cid:") {
        return None;
    }
    let request = ImageRequest { url: url.to_string() };
    match handler.intercept(&request) {
        InterceptOutcome::Serve(bytes) => Some(bytes),
        InterceptOutcome::Allow | InterceptOutcome::Block => None,
    }
}

/// Resolve `urls` with up to [`MAX_PARALLEL_FETCHES`] threads at a time,
/// returning `(url, bytes)` for each one that produced data. Stops handing
/// out new URLs once render job `job_id` is superseded (in-flight requests
/// are left to finish -- they cannot be interrupted).
fn fetch_all(
    urls: Vec<String>,
    handler: &dyn WebViewHandler,
    latest_id: &AtomicU64,
    job_id: u64,
) -> Vec<(String, Vec<u8>)> {
    let threads = MAX_PARALLEL_FETCHES.min(urls.len());
    let queue = Mutex::new(VecDeque::from(urls));
    let results = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    if latest_id.load(Ordering::SeqCst) != job_id {
                        return;
                    }
                    let Some(url) = queue.lock().unwrap().pop_front() else {
                        return;
                    };
                    if let Some(bytes) = resolve_image_bytes(&url, handler) {
                        results.lock().unwrap().push((url, bytes));
                    }
                }
            });
        }
    });
    results.into_inner().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct RecordingHandler {
        seen: Mutex<Vec<String>>,
        outcome: fn() -> InterceptOutcome,
    }

    impl RecordingHandler {
        fn new(outcome: fn() -> InterceptOutcome) -> Self {
            Self { seen: Mutex::new(Vec::new()), outcome }
        }
        fn seen(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl WebViewHandler for RecordingHandler {
        fn intercept(&self, request: &ImageRequest) -> InterceptOutcome {
            self.seen.lock().unwrap().push(request.url.clone());
            (self.outcome)()
        }
    }

    /// A 1x1 opaque red PNG.
    const RED_1X1_PNG: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";
    /// A 10x100 opaque red PNG.
    const RED_10X100_PNG: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAoAAABkCAYAAAC/zKGXAAAAKklEQVR4nO3KMQ0AMBADseNPOqXwayUP3txqF4miKIqiKIqiKIqiuI/jA8dQyJqAFjd8AAAAAElFTkSuQmCC";

    #[test]
    fn resolve_image_bytes_decodes_a_data_uri_without_asking_the_handler() {
        // "hi" base64-encoded, arbitrary content -- only the round trip
        // through decode_data_uri matters here.
        let handler = RecordingHandler::new(|| InterceptOutcome::Allow);
        let bytes = resolve_image_bytes("data:text/plain;base64,aGk=", &handler);
        assert_eq!(bytes, Some(b"hi".to_vec()));
        assert!(handler.seen().is_empty(), "a data: URL must never reach the handler");
    }

    #[test]
    fn resolve_image_bytes_leaves_an_unmatched_cid_unresolved_without_asking_the_handler() {
        // render.rs (B5) already inlines every cid: part it can match as a
        // data: URL before the HTML reaches this crate -- a cid: surviving
        // to here means no match was found, and there's nothing to fetch.
        let handler = RecordingHandler::new(|| InterceptOutcome::Serve(vec![1]));
        let bytes = resolve_image_bytes("cid:missing-part", &handler);
        assert_eq!(bytes, None);
        assert!(handler.seen().is_empty(), "an unmatched cid: URL must never reach the handler either");
    }

    #[test]
    fn resolve_image_bytes_asks_the_handler_for_a_remote_url_and_serves_its_bytes() {
        let handler = RecordingHandler::new(|| InterceptOutcome::Serve(vec![9, 9, 9]));
        let bytes = resolve_image_bytes("https://example.com/pixel.png", &handler);
        assert_eq!(bytes, Some(vec![9, 9, 9]));
        assert_eq!(handler.seen(), vec!["https://example.com/pixel.png".to_string()]);
    }

    #[test]
    fn resolve_image_bytes_blocks_a_remote_url_when_the_handler_declines() {
        for outcome in [(|| InterceptOutcome::Allow) as fn() -> InterceptOutcome, || InterceptOutcome::Block] {
            let handler = RecordingHandler::new(outcome);
            let bytes = resolve_image_bytes("https://example.com/track.gif", &handler);
            assert_eq!(bytes, None);
        }
    }

    #[test]
    fn fetch_all_resolves_every_url_and_drops_the_ones_the_handler_declines() {
        struct Half;
        impl WebViewHandler for Half {
            fn intercept(&self, request: &ImageRequest) -> InterceptOutcome {
                if request.url.ends_with("/ok") {
                    InterceptOutcome::Serve(request.url.clone().into_bytes())
                } else {
                    InterceptOutcome::Block
                }
            }
        }
        let urls: Vec<String> = (0..20)
            .map(|i| format!("https://example.com/{i}/{}", if i % 2 == 0 { "ok" } else { "no" }))
            .collect();
        let latest = AtomicU64::new(7);
        let mut got = fetch_all(urls, &Half, &latest, 7);
        got.sort();
        assert_eq!(got.len(), 10);
        assert!(got.iter().all(|(url, bytes)| url.ends_with("/ok") && bytes == url.as_bytes()));
    }

    #[test]
    fn fetch_all_stops_once_the_job_is_superseded() {
        let handler = RecordingHandler::new(|| InterceptOutcome::Serve(vec![1]));
        let urls = vec!["https://example.com/a".to_string(), "https://example.com/b".to_string()];
        // The "latest" id is already a newer job's.
        let latest = AtomicU64::new(8);
        let got = fetch_all(urls, &handler, &latest, 7);
        assert!(got.is_empty());
        assert!(handler.seen().is_empty(), "no request may start for a superseded job");
    }

    /// Run a worker's render job to completion in-process (no thread) and
    /// collect what it sent.
    fn render_in_process(html: &str, width: f32, handler: Arc<dyn WebViewHandler>) -> Vec<Output> {
        let (out_tx, out_rx) = mpsc::channel();
        let latest = Arc::new(AtomicU64::new(1));
        let mut worker = Worker::new(egui::Context::default(), handler, out_tx, latest);
        worker.render(&RenderJob {
            id: 1,
            html: Arc::new(html.to_string()),
            width,
            scale: 1.0,
            reset_images: true,
        });
        out_rx.try_iter().collect()
    }

    fn last_frame(outputs: &[Output]) -> &painter::ListFrame {
        outputs
            .iter()
            .rev()
            .find_map(|o| match o {
                Output::List(f) => Some(f),
                _ => None,
            })
            .expect("a frame was sent")
    }

    /// The image commands of a frame's display list, in paint order.
    fn image_rects(frame: &painter::ListFrame) -> Vec<egui::Rect> {
        frame
            .list
            .cmds
            .iter()
            .filter_map(|c| match c {
                painter::Cmd::Image { rect, .. } => Some(*rect),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn an_image_is_scaled_to_its_laid_out_size_not_drawn_at_its_natural_size() {
        // A 1x1 image displayed at 100x100 must fill that whole box; drawn at
        // its natural size it would be a single pixel.
        let html = format!(
            r#"<body style="margin:0"><img src="{RED_1X1_PNG}" width="100" height="100"></body>"#
        );
        let outputs = render_in_process(&html, 300.0, Arc::new(DefaultHandler));
        assert!(matches!(outputs.last(), Some(Output::Done { id: 1, ok: true })));
        let frame = last_frame(&outputs);
        assert_eq!(frame.list.size.x, 300.0);
        assert!((100.0..110.0).contains(&frame.list.size.y), "height was {}", frame.list.size.y);
        assert_eq!(image_rects(frame), vec![egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(100.0, 100.0))]);
    }

    #[test]
    fn a_redraw_after_images_load_does_not_leave_the_first_pass_behind() {
        // With no width/height attributes the image's size is unknown until
        // it has been loaded, so pass 1 lays the rule below it out at y=0
        // and pass 2 at y=100. The rule must not appear at both places.
        let html = format!(
            r#"<body style="margin:0"><img src="{RED_10X100_PNG}" style="display:block"><div style="height:1px;background:#000"></div></body>"#
        );
        let outputs = render_in_process(&html, 200.0, Arc::new(DefaultHandler));
        let frame = last_frame(&outputs);
        // Exactly one black rule: at y = 100 (below the 100px image), and not
        // also where pass 1 would have put it.
        let black_rows: Vec<f32> = frame
            .list
            .cmds
            .iter()
            .filter_map(|c| match c {
                painter::Cmd::Rect { rect, fill, .. } if *fill == egui::Color32::BLACK => Some(rect.min.y),
                _ => None,
            })
            .collect();
        assert_eq!(black_rows, vec![100.0], "rule drawn at the wrong place(s): {black_rows:?}");
    }

    #[test]
    fn images_are_requested_again_after_a_render_was_superseded_mid_fetch() {
        // Job 1 discovers the remote image, and is then superseded while
        // its fetch is in flight (the handler bumps the latest id, as a
        // `show()` submitting a resize would). Job 2 -- same HTML, no
        // explicit reset, like a resize -- must still get the image.
        struct SupersedeOnce {
            latest: Arc<AtomicU64>,
            calls: Mutex<u32>,
            png: Vec<u8>,
        }
        impl WebViewHandler for SupersedeOnce {
            fn intercept(&self, _request: &ImageRequest) -> InterceptOutcome {
                let mut calls = self.calls.lock().unwrap();
                *calls += 1;
                if *calls == 1 {
                    self.latest.store(2, Ordering::SeqCst);
                    InterceptOutcome::Block
                } else {
                    InterceptOutcome::Serve(self.png.clone())
                }
            }
        }
        let latest = Arc::new(AtomicU64::new(1));
        let handler = Arc::new(SupersedeOnce {
            latest: latest.clone(),
            calls: Mutex::new(0),
            png: decode_data_uri(RED_1X1_PNG).unwrap(),
        });
        let (out_tx, out_rx) = mpsc::channel();
        let mut worker = Worker::new(egui::Context::default(), handler, out_tx, latest);
        let html = Arc::new(
            r#"<body style="margin:0"><img src="https://example.com/a.png" width="20" height="20"></body>"#.to_string(),
        );
        let job = |id, reset_images| RenderJob { id, html: html.clone(), width: 100.0, scale: 1.0, reset_images };
        worker.render(&job(1, true));
        worker.render(&job(2, false));
        let outputs: Vec<Output> = out_rx.try_iter().collect();
        let frame = last_frame(&outputs);
        assert_eq!(frame.id, 2);
        assert_eq!(image_rects(frame).len(), 1, "the image was never re-requested");
    }

    #[test]
    fn deeply_nested_layout_tables_finish_laying_out() {
        // Marketing mail nests layout tables (and floats them with
        // `align=left`) many levels deep. Without the table-cell measurement
        // memoization in the C++ litehtml this workspace pins (see the
        // `litehtml` dependency in the workspace Cargo.toml), layout time is
        // exponential in the nesting depth (measured on the unpatched
        // dependency: 12 levels 33ms, 20 levels 1.5s, 24 levels over 20s): a
        // real 17-level mail never finished. Run it on a thread so a
        // regression fails this test instead of hanging the whole suite.
        const DEPTH: usize = 24;
        let mut html = String::from(r#"<body style="margin:0">"#);
        for _ in 0..DEPTH {
            html.push_str(r#"<table align="left" style="width:100%"><tbody><tr><td>text "#);
        }
        html.push_str("deep");
        for _ in 0..DEPTH {
            html.push_str("</td></tr></tbody></table>");
        }
        html.push_str("</body>");

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let outputs = render_in_process(&html, 700.0, Arc::new(DefaultHandler));
            let _ = tx.send(outputs.len());
        });
        let outputs = rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("layout of 24 nested tables did not finish within 60s -- is the table-cell memoization missing?");
        assert!(outputs > 0);
    }

    #[test]
    fn a_frame_carries_the_text_runs_of_the_page_it_shows() {
        let outputs = render_in_process(
            r#"<body style="margin:0"><p>Hello world</p><p style="display:none">secret</p></body>"#,
            300.0,
            Arc::new(DefaultHandler),
        );
        let text: String = last_frame(&outputs).runs.runs.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(text, "Hello world");
    }

    #[test]
    fn a_view_exposes_the_runs_of_the_current_frame_and_forgets_them_on_load() {
        let ctx = egui::Context::default();
        let host = WebViewHost::new();
        let mut view = host.new_view(&ctx, WebViewConfig::new(WebViewSource::Html("<p>one two</p>".to_string())));
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 300.0))),
            ..Default::default()
        };
        assert!(view.text_runs().runs.is_empty());
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            ctx.run_ui(input(), |ui| {
                view.show(ui);
            }).textures_delta.clear();
            if !view.is_rendering() {
                break;
            }
            assert!(Instant::now() < deadline, "render never finished");
            std::thread::sleep(Duration::from_millis(10));
        }
        let text: String = view.text_runs().runs.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(text, "one two");
        view.load(WebViewSource::Html("<p>other</p>".to_string()));
        assert!(view.text_runs().runs.is_empty(), "the old page's runs must not outlive it");
    }

    #[test]
    fn a_frame_carries_the_links_of_the_page_it_shows() {
        let (out_tx, out_rx) = mpsc::channel();
        let mut worker = Worker::new(egui::Context::default(), Arc::new(DefaultHandler), out_tx, Arc::new(AtomicU64::new(1)));
        let html = Arc::new(
            r#"<body style="margin:0"><a href="https://example.com/x" style="display:block;height:40px">go</a></body>"#.to_string(),
        );
        worker.render(&RenderJob { id: 1, html, width: 200.0, scale: 1.0, reset_images: true });
        let outputs: Vec<Output> = out_rx.try_iter().collect();
        let links = &last_frame(&outputs).links;
        assert_eq!(links.href_at(egui::pos2(5.0, 5.0)), Some("https://example.com/x"));
        assert_eq!(links.href_at(egui::pos2(5.0, 300.0)), None, "empty space is not a link");
    }

    #[test]
    fn show_renders_off_the_ui_thread_and_ends_up_with_a_display_list() {
        let ctx = egui::Context::default();
        let host = WebViewHost::new();
        let mut view = host.new_view(
            &ctx,
            WebViewConfig::new(WebViewSource::Html("<h1>hello</h1><p>world</p>".to_string())),
        );
        let deadline = Instant::now() + Duration::from_secs(60);
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 300.0))),
            ..Default::default()
        };
        // The first `show()` must return without having rendered anything.
        ctx.run_ui(input(), |ui| {
            view.show(ui);
        }).textures_delta.clear();
        assert!(view.is_rendering());
        assert!(view.list.is_none());
        while view.is_rendering() {
            assert!(Instant::now() < deadline, "render never finished");
            std::thread::sleep(Duration::from_millis(10));
            ctx.run_ui(input(), |ui| {
                view.show(ui);
            }).textures_delta.clear();
        }
        assert!(view.list.is_some());
        assert!(!view.failed);
    }

    // ── Selection interaction, driven with synthetic egui input ─────────

    /// A view in a headless egui context that tests poke with pointer and
    /// keyboard events, one frame at a time.
    struct Harness {
        ctx: egui::Context,
        view: WebView,
        /// Where the picture's top-left corner is on screen (with no scroll).
        origin: egui::Pos2,
        time: f64,
        /// The modifier keys currently held.
        modifiers: egui::Modifiers,
        /// Everything the frames asked the platform to do (clipboard...).
        commands: Vec<egui::OutputCommand>,
        /// Link clicks the view reported.
        links: Vec<String>,
        /// The mouse cursor the last frame asked for.
        cursor: egui::CursorIcon,
        /// An egui text field shown above the view, to test focus.
        field: Option<String>,
        field_id: egui::Id,
    }

    const SCREEN: egui::Vec2 = egui::vec2(400.0, 300.0);

    impl Harness {
        fn new(html: &str, with_field: bool) -> Self {
            let ctx = egui::Context::default();
            let view = WebViewHost::new()
                .new_view(&ctx, WebViewConfig::new(WebViewSource::Html(html.to_string())));
            let mut h = Self {
                ctx,
                view,
                origin: egui::Pos2::ZERO,
                time: 1.0,
                modifiers: egui::Modifiers::NONE,
                commands: Vec::new(),
                links: Vec::new(),
                cursor: egui::CursorIcon::Default,
                field: with_field.then(String::new),
                field_id: egui::Id::new("test-field"),
            };
            h.settle();
            h
        }

        /// Run frames until the view has finished rendering.
        fn settle(&mut self) {
            self.frame(vec![], egui::Modifiers::NONE);
            let deadline = Instant::now() + Duration::from_secs(60);
            while self.view.is_rendering() {
                assert!(Instant::now() < deadline, "render never finished");
                std::thread::sleep(Duration::from_millis(10));
                self.frame(vec![], egui::Modifiers::NONE);
            }
            self.frame(vec![], egui::Modifiers::NONE);
        }

        fn frame(&mut self, events: Vec<egui::Event>, modifiers: egui::Modifiers) {
            self.time += 0.05;
            let mut events = events;
            if modifiers != self.modifiers {
                events.insert(0, egui::Event::ModifiersChanged(modifiers));
                self.modifiers = modifiers;
            }
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, SCREEN)),
                time: Some(self.time),
                events,
                ..Default::default()
            };
            let view = &mut self.view;
            let field = &mut self.field;
            let field_id = self.field_id;
            let mut origin = self.origin;
            let mut links = Vec::new();
            let mut out = self.ctx.run_ui(input, |ui| {
                if let Some(text) = field {
                    ui.add(egui::TextEdit::singleline(text).id(field_id));
                }
                origin = ui.cursor().min;
                for e in view.show(ui) {
                    let WebViewEvent::LinkClicked(url) = e;
                    links.push(url);
                }
            });
            self.origin = origin;
            self.links.extend(links);
            out.textures_delta.clear();
            self.commands.extend(out.platform_output.commands);
            self.cursor = out.platform_output.cursor_icon;
        }

        /// The cursor shown with the pointer at document point `p`.
        fn cursor_at(&mut self, p: egui::Pos2) -> egui::CursorIcon {
            let pos = self.at(p);
            self.frame(vec![egui::Event::PointerMoved(pos)], egui::Modifiers::NONE);
            self.frame(vec![], egui::Modifiers::NONE);
            self.cursor
        }

        /// Screen position of a point in document space.
        fn at(&self, doc: egui::Pos2) -> egui::Pos2 {
            self.origin + doc.to_vec2()
        }

        fn run(&self, text: &str) -> TextRun {
            self.view
                .text_runs()
                .runs
                .iter()
                .find(|r| r.text == text)
                .unwrap_or_else(|| panic!("no run {text:?}"))
                .clone()
        }

        /// The screen position just inside a word's left / right edge, and its middle.
        fn left_of(&self, text: &str) -> egui::Pos2 {
            self.at(self.run(text).rect.left_center() + egui::vec2(1.0, 0.0))
        }

        fn right_of(&self, text: &str) -> egui::Pos2 {
            self.at(self.run(text).rect.right_center() - egui::vec2(1.0, 0.0))
        }

        fn middle_of(&self, text: &str) -> egui::Pos2 {
            self.at(self.run(text).rect.center())
        }

        fn button(&mut self, pos: egui::Pos2, pressed: bool, modifiers: egui::Modifiers) {
            self.frame(
                vec![egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers }],
                modifiers,
            );
        }

        fn move_to(&mut self, pos: egui::Pos2) {
            self.frame(vec![egui::Event::PointerMoved(pos)], egui::Modifiers::NONE);
        }

        fn click(&mut self, pos: egui::Pos2) {
            self.move_to(pos);
            self.button(pos, true, egui::Modifiers::NONE);
            self.button(pos, false, egui::Modifiers::NONE);
        }

        /// Press at `from`, drag through a few points to `to`, release.
        fn drag(&mut self, from: egui::Pos2, to: egui::Pos2) {
            self.move_to(from);
            self.button(from, true, egui::Modifiers::NONE);
            for i in 1..=4 {
                self.move_to(from + (to - from) * (i as f32 / 4.0));
            }
            self.button(to, false, egui::Modifiers::NONE);
        }

        fn copied(&self) -> Vec<String> {
            self.commands
                .iter()
                .filter_map(|c| match c {
                    egui::OutputCommand::CopyText(t) => Some(t.clone()),
                    _ => None,
                })
                .collect()
        }

        fn selected(&self) -> Option<String> {
            self.view.selected_text()
        }

        /// The vertical scroll offset of the view's scroll area.
        fn scroll_offset(&self) -> f32 {
            let mut y = 0.0;
            let name = self.view.scroll_id.clone();
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, SCREEN)),
                ..Default::default()
            };
            let _ = self.ctx.run_ui(input, |ui| {
                let id = ui.make_persistent_id(egui::IdSalt::new(&name));
                y = egui::scroll_area::State::load(ui.ctx(), id).map_or(0.0, |s| s.offset.y);
            });
            y
        }
    }

    const TWO_PARAS: &str = r#"<body style="margin:0"><p style="margin:0 0 20px">alpha beta gamma</p><p style="margin:0">delta epsilon</p></body>"#;

    #[test]
    fn dragging_across_text_selects_it() {
        let mut h = Harness::new(TWO_PARAS, false);
        h.drag(h.left_of("alpha"), h.right_of("gamma"));
        assert_eq!(h.selected().as_deref(), Some("alpha beta gamma"));
        assert!(h.view.has_selection());

        // Dragging backwards, into the second paragraph, is the same gesture.
        h.drag(h.right_of("delta"), h.left_of("alpha"));
        assert_eq!(h.selected().as_deref(), Some("alpha beta gamma\n\ndelta"));
    }

    #[test]
    fn a_drag_starting_on_a_link_selects_instead_of_following_it() {
        let html = r#"<body style="margin:0"><p style="margin:0"><a href="https://example.com/x">click here now</a></p></body>"#;
        let mut h = Harness::new(html, false);
        h.drag(h.left_of("click"), h.right_of("now"));
        assert_eq!(h.selected().as_deref(), Some("click here now"));
        h.frame(vec![], egui::Modifiers::NONE);
        assert!(h.links.is_empty(), "a drag must not open the link: {:?}", h.links);
    }

    #[test]
    fn a_link_shows_a_hand_and_other_text_an_i_beam() {
        let html = r#"<body style="margin:0"><p style="margin:0">plain <a href="https://example.com/x">link words</a> tail</p><p style="margin:20px 0 0"><a href="https://example.com/i"><img width="60" height="30"></a></p></body>"#;
        let mut h = Harness::new(html, false);
        let (plain, link, tail) = (h.run("plain").rect, h.run("link").rect, h.run("tail").rect);
        assert_eq!(h.cursor_at(plain.center()), egui::CursorIcon::Text, "plain text is an I-beam");
        assert_eq!(h.cursor_at(link.center()), egui::CursorIcon::PointingHand, "a link is a hand");
        assert_eq!(h.cursor_at(link.left_center() + egui::vec2(1.0, 0.0)), egui::CursorIcon::PointingHand, "even at its very start");
        assert_eq!(h.cursor_at(link.right_center() - egui::vec2(1.0, 0.0)), egui::CursorIcon::PointingHand, "and its very end");
        assert_eq!(h.cursor_at(tail.center()), egui::CursorIcon::Text, "text after the link is an I-beam again");
        // An image is not text, but as a link it is still a hand.
        let below = egui::pos2(30.0, tail.bottom() + 20.0 + 15.0);
        assert_eq!(h.cursor_at(below), egui::CursorIcon::PointingHand, "an image link is a hand");
        assert_eq!(h.cursor_at(egui::pos2(300.0, below.y)), egui::CursorIcon::Default, "empty space is the default cursor");
    }

    #[test]
    fn a_plain_click_on_a_link_still_reports_it_and_clears_the_selection() {
        let html = r#"<body style="margin:0"><p style="margin:0"><a href="https://example.com/x">link</a> and other words</p></body>"#;
        let mut h = Harness::new(html, false);
        h.drag(h.left_of("words"), h.right_of("words"));
        assert!(h.view.has_selection());
        h.click(h.middle_of("link"));
        assert!(!h.view.has_selection(), "a click elsewhere clears the selection");
        let deadline = Instant::now() + Duration::from_secs(10);
        while h.links.is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            h.frame(vec![], egui::Modifiers::NONE);
        }
        assert_eq!(h.links, vec!["https://example.com/x".to_string()]);
    }

    #[test]
    fn double_click_selects_a_word_and_triple_click_the_paragraph() {
        let mut h = Harness::new(TWO_PARAS, false);
        let p = h.middle_of("beta");
        h.click(p);
        h.click(p);
        assert_eq!(h.selected().as_deref(), Some("beta"));
        h.click(p);
        assert_eq!(h.selected().as_deref(), Some("alpha beta gamma"));
    }

    #[test]
    fn shift_click_extends_the_selection() {
        let mut h = Harness::new(TWO_PARAS, false);
        let pa = h.middle_of("alpha");
        h.click(pa);
        h.click(pa); // double-click: "alpha"
        assert_eq!(h.selected().as_deref(), Some("alpha"));
        let shift = egui::Modifiers::SHIFT;
        let pg = h.right_of("gamma");
        h.move_to(pg);
        h.button(pg, true, shift);
        h.button(pg, false, shift);
        assert_eq!(h.selected().as_deref(), Some("alpha beta gamma"));
    }

    #[test]
    fn copy_puts_the_selection_on_the_clipboard() {
        let mut h = Harness::new(TWO_PARAS, false);
        let p = h.middle_of("epsilon");
        h.click(p);
        h.click(p);
        assert!(h.copied().is_empty());
        h.frame(vec![egui::Event::Copy], egui::Modifiers::NONE);
        assert_eq!(h.copied(), vec!["epsilon".to_string()]);
    }

    #[test]
    fn copy_with_nothing_selected_does_nothing() {
        let mut h = Harness::new(TWO_PARAS, false);
        h.click(h.middle_of("delta"));
        h.frame(vec![egui::Event::Copy], egui::Modifiers::NONE);
        assert!(h.copied().is_empty());
    }

    #[test]
    fn ctrl_a_selects_everything_when_the_view_has_focus() {
        let mut h = Harness::new(TWO_PARAS, false);
        h.click(h.middle_of("delta"));
        let key = egui::Event::Key {
            key: egui::Key::A,
            physical_key: Some(egui::Key::A),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        };
        h.frame(vec![key], egui::Modifiers::COMMAND);
        assert_eq!(h.selected().as_deref(), Some("alpha beta gamma\n\ndelta epsilon"));
    }

    #[test]
    fn copy_belongs_to_a_focused_text_field_not_to_the_selection() {
        let mut h = Harness::new(TWO_PARAS, true);
        // Select a word in the message...
        let p = h.middle_of("beta");
        h.click(p);
        h.click(p);
        assert_eq!(h.selected().as_deref(), Some("beta"));
        // ...then focus the text field and press Ctrl+C.
        h.ctx.memory_mut(|m| m.request_focus(h.field_id));
        h.frame(vec![], egui::Modifiers::NONE);
        h.frame(vec![egui::Event::Copy], egui::Modifiers::NONE);
        assert!(h.copied().is_empty(), "the message must not steal the copy: {:?}", h.copied());
    }

    #[test]
    fn clicking_outside_the_view_clears_the_selection() {
        let mut h = Harness::new(TWO_PARAS, true);
        let p = h.middle_of("beta");
        h.click(p);
        h.click(p);
        assert!(h.view.has_selection());
        // The text field sits above the picture.
        h.click(egui::pos2(20.0, 5.0));
        assert!(!h.view.has_selection());
    }

    #[test]
    fn the_selection_survives_a_relayout_of_the_same_text_but_not_a_new_page() {
        let mut h = Harness::new(TWO_PARAS, false);
        let p = h.middle_of("beta");
        h.click(p);
        h.click(p);
        assert_eq!(h.selected().as_deref(), Some("beta"));
        // Same text, laid out again (as when images arrive or "reload" runs).
        h.view.reload();
        h.settle();
        assert_eq!(h.selected().as_deref(), Some("beta"));
        // A different message drops it.
        h.view.load(WebViewSource::Html("<p>something else</p>".to_string()));
        assert!(!h.view.has_selection());
    }

    #[test]
    fn dragging_below_the_visible_area_scrolls_the_page_and_keeps_selecting() {
        let many: String = (0..60).map(|i| format!("<p style=\"margin:0 0 10px\">line number {i}</p>")).collect();
        let mut h = Harness::new(&format!("<body style=\"margin:0\">{many}</body>"), false);
        assert!(h.view.content_size().unwrap().y > 900.0);
        assert_eq!(h.scroll_offset(), 0.0);
        let start = h.left_of("line");
        h.move_to(start);
        h.button(start, true, egui::Modifiers::NONE);
        // Hold the pointer past the bottom edge of the 300pt-tall screen.
        let below = egui::pos2(start.x + 30.0, SCREEN.y + 30.0);
        for _ in 0..12 {
            h.move_to(below);
        }
        let offset = h.scroll_offset();
        assert!(offset > 20.0, "the page did not scroll (offset {offset})");
        // The selection reaches lines that were never on screen.
        let selected = h.selected().unwrap();
        assert!(selected.contains("line number 12"), "{selected:?}");
        h.button(below, false, egui::Modifiers::NONE);
    }

    // ── Painting ────────────────────────────────────────────────

    fn count_text_shapes(out: &egui::FullOutput) -> usize {
        out.shapes.iter().filter(|s| matches!(s.shape, egui::Shape::Text(_))).count()
    }

    /// Run `view` headless until it has finished rendering *and* painted at
    /// least `want` text shapes (the "Loading fonts..." placeholder is one too,
    /// so a bare "some text" would return too early). Returns how many.
    fn show_until_text_is_painted(ctx: &egui::Context, view: &mut WebView, width: f32, want: usize) -> usize {
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 400.0))),
            ..Default::default()
        };
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let mut out = ctx.run_ui(input(), |ui| {
                view.show(ui);
            });
            out.textures_delta.clear();
            let text = count_text_shapes(&out);
            if !view.is_rendering() && text >= want {
                return text;
            }
            assert!(Instant::now() < deadline, "never painted {want} text shapes (last count {text})");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn the_view_waits_for_its_fonts_and_then_paints_text() {
        // egui panics on a font family it does not know; the view must hold
        // the display list back until the context has the worker's fonts.
        let ctx = egui::Context::default();
        let host = WebViewHost::new();
        let html = r#"<body style="margin:0"><p style="font-family:Arial,sans-serif;font-size:20px">Hello</p><p>go &#10148;</p></body>"#;
        let mut view = host.new_view(&ctx, WebViewConfig::new(WebViewSource::Html(html.to_string())));
        // "Hello", "go" and the arrow: three runs.
        let text = show_until_text_is_painted(&ctx, &mut view, 400.0, 3);
        assert_eq!(text, 3, "one text shape per recorded run, and no placeholder label left over");
        assert!(view.content_size().is_some_and(|s| s.y > 20.0));
        assert!(view.last_render_time().is_some());
    }

    #[test]
    fn a_downloaded_image_is_not_fetched_again_by_a_later_render() {
        struct Counting {
            calls: Mutex<u32>,
            png: Vec<u8>,
        }
        impl WebViewHandler for Counting {
            fn intercept(&self, _request: &ImageRequest) -> InterceptOutcome {
                *self.calls.lock().unwrap() += 1;
                InterceptOutcome::Serve(self.png.clone())
            }
        }
        let handler = Arc::new(Counting { calls: Mutex::new(0), png: decode_data_uri(RED_1X1_PNG).unwrap() });
        let (out_tx, out_rx) = mpsc::channel();
        let mut worker = Worker::new(egui::Context::default(), handler.clone(), out_tx, Arc::new(AtomicU64::new(1)));
        let html = Arc::new(
            r#"<body style="margin:0"><img src="https://example.com/a.png" width="20" height="20"></body>"#.to_string(),
        );
        let job = |id| RenderJob { id, html: html.clone(), width: 100.0, scale: 1.0, reset_images: true };
        worker.render(&job(1));
        worker.latest_id.store(2, Ordering::SeqCst);
        worker.render(&job(2));
        assert_eq!(*handler.calls.lock().unwrap(), 1, "the second render must reuse the first download");
        let outputs: Vec<Output> = out_rx.try_iter().collect();
        let frame = last_frame(&outputs);
        assert_eq!(frame.id, 2);
        assert_eq!(image_rects(frame).len(), 1, "and still draw it");
    }

    #[test]
    fn the_engine_is_rebuilt_once_its_decoded_images_pass_the_budget() {
        let (out_tx, _out_rx) = mpsc::channel();
        let mut worker = Worker::new(
            egui::Context::default(),
            Arc::new(DefaultHandler),
            out_tx,
            Arc::new(AtomicU64::new(1)),
        );
        let html = Arc::new(format!(
            r#"<body style="margin:0"><img src="{RED_10X100_PNG}" width="10" height="100"></body>"#
        ));
        let job = |id| RenderJob { id, html: html.clone(), width: 100.0, scale: 1.0, reset_images: true };

        worker.render(&job(1));
        assert_eq!(worker.total_images_loaded, 1);
        assert!(worker.decoded_bytes > 0, "the image was decoded into the engine");

        // Pretend enough images have been decoded to pass the budget.
        worker.decoded_bytes = DECODED_IMAGE_LIMIT + 1;
        worker.latest_id.store(2, Ordering::SeqCst);
        worker.render(&job(2));

        assert_eq!(
            worker.total_images_loaded, 2,
            "an over-budget engine must be dropped, so its image is decoded again"
        );
    }

    #[test]
    fn evicting_decoded_images_leaves_an_under_budget_engine_alone() {
        let (out_tx, _out_rx) = mpsc::channel();
        let mut worker = Worker::new(
            egui::Context::default(),
            Arc::new(DefaultHandler),
            out_tx,
            Arc::new(AtomicU64::new(1)),
        );
        assert!(!worker.evict_decoded_images_if_over(0), "there is no engine yet");

        let html =
            format!(r#"<body style="margin:0"><img src="{RED_1X1_PNG}" width="20" height="20"></body>"#);
        worker.render(&RenderJob { id: 1, html: Arc::new(html), width: 100.0, scale: 1.0, reset_images: true });
        let decoded = worker.decoded_bytes;
        assert!(worker.painter.is_some());

        assert!(!worker.evict_decoded_images_if_over(DECODED_IMAGE_LIMIT), "well under the budget");
        assert!(worker.painter.is_some(), "an under-budget engine must survive");
        assert_eq!(worker.decoded_bytes, decoded, "a no-op eviction must not reset the accounting");
    }

    #[test]
    fn a_scroll_offset_set_before_the_first_render_is_applied_once_there_is_a_page() {
        let ctx = egui::Context::default();
        let host = WebViewHost::new();
        let html = r#"<body style="margin:0"><div style="height:5000px;background:#eee">tall</div></body>"#;
        let mut view = host.new_view(&ctx, WebViewConfig::new(WebViewSource::Html(html.to_string())));
        view.set_scroll_offset(700.0);
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(300.0, 400.0))),
            ..Default::default()
        };
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut settled = 0;
        while settled < 5 {
            ctx.run_ui(input(), |ui| {
                view.show(ui);
            }).textures_delta.clear();
            settled = if view.is_rendering() { 0 } else { settled + 1 };
            assert!(Instant::now() < deadline, "render never finished");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!((view.scroll_offset() - 700.0).abs() < 1.0, "scrolled to {}", view.scroll_offset());
    }

    #[test]
    fn the_selection_highlight_is_painted_over_the_display_list() {
        let ctx = egui::Context::default();
        let host = WebViewHost::new();
        let html = r#"<body style="margin:0"><p style="font-size:20px">alpha beta</p></body>"#;
        let mut view = host.new_view(&ctx, WebViewConfig::new(WebViewSource::Html(html.to_string())));
        // "alpha", a space, "beta".
        show_until_text_is_painted(&ctx, &mut view, 400.0, 2);
        assert!(view.selected_text().is_none());
        view.select_all();

        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 400.0))),
            ..Default::default()
        };
        let mut highlight = None;
        let mut out = ctx.run_ui(input, |ui| {
            highlight = Some(ui.visuals().selection.bg_fill.gamma_multiply(0.55));
            view.show(ui);
        });
        out.textures_delta.clear();
        let highlight = highlight.unwrap();
        let painted: Vec<egui::Rect> = out
            .shapes
            .iter()
            .filter_map(|s| match &s.shape {
                egui::Shape::Rect(r) if r.fill == highlight => Some(r.rect),
                _ => None,
            })
            .collect();
        assert!(!painted.is_empty(), "no highlight rect was painted for a select-all");
        // And it covers the words (the runs of a line are merged into one rect).
        // The view sits at the screen's origin here, so document and screen
        // coordinates coincide.
        let alpha = view.text_runs().runs.iter().find(|r| r.text == "alpha").unwrap().rect;
        assert!(painted.iter().any(|r| r.expand(1.0).contains_rect(alpha)), "{painted:?} vs {alpha:?}");
    }

}
