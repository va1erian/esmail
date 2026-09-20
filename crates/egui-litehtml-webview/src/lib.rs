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
//! background thread (the "worker") that owns the
//! [`litehtml::pixbuf::PixbufContainer`] outright -- it is `!Send` (it holds
//! `Rc`s), so it is created *on* the worker and never crosses a thread
//! boundary. The UI thread only ever:
//!
//! * sends the worker a job (`Render` after a `load`/`reload`/resize,
//!   `HitTest` after a click), and
//! * receives finished outputs -- ready-to-upload pixel frames and clicked
//!   links -- in [`WebView::show`], uploading the newest frame to an egui
//!   texture. The worker calls `Context::request_repaint` when it has
//!   something to show.
//!
//! Render jobs carry a monotonically increasing id. The worker drops
//! superseded render jobs from its queue and re-checks the id between
//! stages, so dragging the window edge (a job per width) or clicking through
//! messages quickly never queues up seconds of stale layout work; the UI
//! likewise ignores frames whose id is not the newest.
//!
//! # No persisted `litehtml::Document`
//!
//! `litehtml::Document<'a>` borrows its `DocumentContainer` mutably for the
//! `Document`'s own lifetime, which makes storing both as sibling fields a
//! self-referential-struct problem. The worker sidesteps that: it stores only
//! the container (which holds the pixels, fonts and decoded images, reused
//! across jobs) and builds a `Document` fresh for each pass, dropping it
//! straight after.
//!
//! # Text selection without a `Document`
//!
//! Selecting text needs the page's text and geometry long after the
//! `Document` is gone, so the worker records them while it is alive: every
//! draw pass walks the laid-out document once and sends a [`TextRunTable`]
//! (one run per word: box, text, per-character x offsets, containing block,
//! forced line breaks) with the frame. Hit-testing, dragging, double/triple
//! click, the highlight (painted by egui over the tiles, never into them) and
//! the copied text are all plain geometry over that table on the UI thread,
//! so nothing re-lays-out per pointer move. A selection is two carets (run
//! index + character); it survives a re-layout of the same text (resize,
//! images arriving) and is dropped when a different page loads. Ctrl+C and
//! Ctrl+A act only while the view has egui focus, so text fields keep theirs.
//!
//! # Render sequence (one `Render` job)
//!
//! 1. **Draw** into a cleared canvas: build a `Document`, `render()` it at
//!    the requested width, `draw()` it. If the content turns out taller than
//!    the canvas, grow the canvas (never shrunk between messages -- resizing
//!    is what forces the extra pass) and draw once more.
//! 2. **Discover images**: URLs are only known after the layout has walked
//!    the document. `data:` URIs are decoded locally (litehtml has no
//!    network layer; `esmail` inlines `cid:` parts as `data:` URIs first);
//!    everything else goes to [`WebViewHandler::intercept`], several at a
//!    time. If remote images are involved, the text-only frame from step 1 is
//!    sent right away so the message is readable while images arrive.
//! 3. **Redraw** with the images loaded (an image can change layout, so this
//!    is a full pass on a *cleared* canvas -- drawing over the previous pass
//!    leaves its text behind), and send that frame. Repeats if the redraw
//!    turns up more URLs, up to a small limit.
//!
//! The frame sent to the UI is flattened onto opaque white and cropped to
//! the content height, so it is exactly what should be displayed. It is cut
//! into a grid of tiles no larger than the GPU's maximum texture side (a
//! newsletter at high DPI is easily taller than 8192px; one texture per
//! message would fail to upload), each its own egui texture, painted edge to
//! edge.
//!
//! # Two backends, side by side
//!
//! [`Backend::Pixbuf`] is everything described above. [`Backend::Painter`]
//! keeps the same worker, job queue, superseding and image pipeline but swaps
//! what happens to litehtml's layout: instead of rasterizing into a bitmap,
//! the worker measures text with egui's own font stack and *records a display
//! list* (rects, gradient meshes, glyph runs, image quads, clips), which the UI
//! thread paints with `egui::Painter` every frame, culled to the visible
//! region. No canvas, no tiles, no flattening onto white; text is crisp at any
//! DPI. See `painter.rs` and `fonts.rs`. Both are always compiled in and
//! selectable per view ([`WebViewConfig::with_backend`],
//! [`WebView::set_backend`]) so a page can be compared under each.
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

use litehtml::email::EMAIL_MASTER_CSS;
use litehtml::html::decode_data_uri;
use litehtml::pixbuf::PixbufContainer;
use litehtml::{Document, DrawContext};

mod selection;
mod text_runs;
pub use selection::{Selection, TextPos};
pub use text_runs::{TextRun, TextRunTable};

/// Height (logical points) the pixel canvas starts at, before any message has
/// been measured.
///
/// It is a *capacity*: content taller than the canvas cannot be drawn until
/// the canvas is resized, and a `Document` cannot survive a resize, so the
/// whole parse + layout has to be done again. On real newsletters that
/// parse + layout is the expensive part (seconds), so a too-small seed
/// doubled the cost of opening the first tall message. Marketing mail is
/// routinely 2000-4000px tall, hence this value; a canvas this size costs
/// only a few tens of MB and a zero-fill per pass. It grows (never shrinks)
/// when a message needs more.
///
/// **Known limitation:** this crate renders a message body as one static
/// image at its full content height (so [`WebView::show`]'s `ScrollArea` can
/// scroll it natively), not into a fixed-size viewport the way a real
/// browser window is. `PixbufContainer` has no separate "viewport size" from
/// "canvas size" -- both come from the same `resize_with_scale` call -- so
/// `vh` units end up relative to *the canvas height*, not a stable window
/// size. (A canvas seeded at height 1 would resolve `1vh` to ~0.01px,
/// collapsing any `height: NNvh` block to nothing -- a real bug, caught by
/// the `ESMAIL_PREVIEW=demo` screenshot check against the demo page's own
/// `.tall { height: 60vh; }` block.) In practice this is a non-issue for real
/// mail: no mainstream mail client preserves or predictably renders
/// viewport-relative units in HTML email, so authors do not rely on them.
const INITIAL_CANVAS_HEIGHT: u32 = 4000;

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
    /// Arrives a little after the click, not synchronously: working out
    /// which link (if any) sits under the pointer needs a layout pass, which
    /// runs on the worker thread.
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

// ─── Backend ─────────────────────────────────────────────────────────────────

/// Which engine turns litehtml's layout into pixels for a view.
///
/// Both are always compiled in and can be switched per view at runtime
/// ([`WebViewConfig::with_backend`], [`WebView::set_backend`]), so the same
/// page can be compared under each. See the crate docs for the design of each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Backend {
    /// litehtml's `PixbufContainer`: tiny-skia + cosmic-text rasterize the
    /// page into a bitmap on the worker thread, shown as GPU textures.
    #[default]
    Pixbuf,
    /// egui's own painter: the worker lays the page out (measuring text with
    /// egui's fonts) and records a display list, which the UI thread paints
    /// every frame.
    Painter,
}

impl Backend {
    /// The other one.
    pub fn other(self) -> Self {
        match self {
            Backend::Pixbuf => Backend::Painter,
            Backend::Painter => Backend::Pixbuf,
        }
    }

    /// `"pixbuf"` or `"painter"`.
    pub fn name(self) -> &'static str {
        match self {
            Backend::Pixbuf => "pixbuf",
            Backend::Painter => "painter",
        }
    }

    /// Parse a name as written in an environment variable: `pixbuf` /
    /// `tiny-skia` / `skia`, or `painter` / `egui`, in any case.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "pixbuf" | "tiny-skia" | "tinyskia" | "skia" => Some(Backend::Pixbuf),
            "painter" | "egui" => Some(Backend::Painter),
            _ => None,
        }
    }
}

// ─── WebViewHost ─────────────────────────────────────────────────────────────

/// Creates [`WebView`]s.
///
/// Neither [`Backend`] needs an engine to own, a window handle or a GL context:
/// [`Backend::Pixbuf`] renders entirely on the CPU, and [`Backend::Painter`] only
/// needs the views' `egui::Context` (it installs the fonts its pages use into
/// it, next to whatever the host set up; the names it adds are unique per view,
/// so any number of views can share one context). This type still exists (rather than a bare associated function on `WebView`) to
/// keep the call shape `esmail`'s `main.rs` already uses -- one host per
/// window, producing any number of views -- even though today it is little
/// more than an id counter so two views in the same window don't collide on
/// one egui texture name.
#[derive(Default)]
pub struct WebViewHost {
    next_view_id: std::cell::Cell<u64>,
}

impl WebViewHost {
    /// Create a host. Takes nothing: neither backend needs anything from the
    /// host window (no window handle, no GL context).
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
                // Created here, not on the caller's thread: PixbufContainer
                // is !Send. (The engines themselves are created lazily, on
                // the first job that needs them: loading system fonts is slow
                // enough that it should not block the UI at startup either.)
                Worker::new(worker_ctx, handler, out_tx, worker_latest).run(job_rx);
            });
        if let Err(e) = &spawned {
            log::error!("egui-litehtml-webview: could not start the render thread: {e}");
        }

        WebView {
            html: Arc::new(html),
            backend: config.backend,
            tx: job_tx,
            rx: out_rx,
            latest_id,
            submitted_id: 0,
            textures: Vec::new(),
            list: None,
            fonts_requested: None,
            texture_name: format!("egui_litehtml_webview_{view_id}"),
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
    /// Which engine paints the view. Defaults to [`Backend::Pixbuf`].
    pub backend: Backend,
}

impl WebViewConfig {
    /// A config that loads `source`, with the default (no images fetched)
    /// policy and the default [`Backend`].
    pub fn new(source: WebViewSource) -> Self {
        Self { source, handler: None, backend: Backend::default() }
    }

    /// Use `handler` for this view's image-loading decisions instead of the
    /// default policy.
    pub fn with_handler(mut self, handler: Arc<dyn WebViewHandler>) -> Self {
        self.handler = Some(handler);
        self
    }

    /// Paint with `backend` instead of the default.
    pub fn with_backend(mut self, backend: Backend) -> Self {
        self.backend = backend;
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
    /// The engine painting this view.
    backend: Backend,
    tx: Sender<Job>,
    rx: Receiver<Output>,
    /// Id of the newest render job, shared with the worker so it can notice
    /// (between stages) that the job it is running has been superseded.
    latest_id: Arc<AtomicU64>,
    /// Id of the newest render job this view submitted; frames with any
    /// other id are stale and ignored.
    submitted_id: u64,
    /// [`Backend::Pixbuf`]: the current frame, one texture per tile (see
    /// [`Tile`]). Reused across frames when the tile count is unchanged;
    /// reallocating per frame would be wasteful. Empty until the first frame
    /// arrives.
    textures: Vec<TileTexture>,
    /// [`Backend::Painter`]: the current display list.
    list: Option<painter::ListFrame>,
    /// The font definitions last handed to the context, and the pass they were
    /// handed over in (see [`WebView::ensure_fonts`]).
    fonts_requested: Option<(Arc<egui::epaint::text::FontDefinitions>, u64)>,
    /// Unique per view, so two views cannot collide on one egui texture.
    texture_name: String,
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
    /// The previous page's pixels are dropped immediately (a "Rendering..."
    /// indicator is shown until the first frame of the new page arrives)
    /// rather than left on screen: a slow render would otherwise show the
    /// *previous* message under the *new* message's headers for seconds.
    pub fn load(&mut self, source: WebViewSource) {
        let WebViewSource::Html(html) = source;
        self.html = Arc::new(html);
        self.discard_frames();
        self.runs = Arc::default();
        self.runs_signature = 0;
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

    /// Which engine is painting this view.
    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// Switch engines and re-render the current page with the new one. The
    /// old picture is dropped straight away (as in [`WebView::load`]).
    pub fn set_backend(&mut self, backend: Backend) {
        if backend == self.backend {
            return;
        }
        // Come back to the same spot (comparing engines is the point of
        // switching): the placeholder shown meanwhile has no content, so egui
        // would otherwise clamp the offset to 0.
        self.pending_scroll.get_or_insert(self.scroll_y);
        self.backend = backend;
        self.discard_frames();
        self.failed = false;
        self.submitted_id += 1;
        self.latest_id.store(self.submitted_id, Ordering::SeqCst);
        self.reset_images = true;
        self.dirty = true;
    }

    /// How long the worker took over the newest finished render (parse +
    /// layout + paint / record, image passes included), or `None` before the
    /// first one. For comparing backends.
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
        let events = self.poll_worker(ui.ctx());

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
        let mut area = egui::ScrollArea::vertical().id_salt(&self.texture_name);
        // Held back until the page has been laid out in the scroll area on an
        // earlier frame: with no content there egui would clamp the offset to 0
        // and the request would be lost. (A frame can have a picture yet still
        // show a placeholder -- the Painter backend waits for its fonts.)
        if self.content_shown
            && let Some(y) = self.pending_scroll.take()
        {
            area = area.vertical_scroll_offset(y);
        }
        let output = area.show(ui, |ui| match self.backend {
            Backend::Pixbuf => self.show_pixbuf(ui),
            Backend::Painter => self.show_painter(ui),
        });
        self.scroll_y = output.state.offset.y;

        events
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

    /// Draw the highlight over the picture. It is painted by egui on top of
    /// the tiles, not into them, so moving the selection never re-renders or
    /// re-uploads the page. Points in the run table are document points,
    /// which are also egui points relative to the picture's corner.
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

    /// Pointer and keyboard handling for the picture at `rect`.
    fn interact(&mut self, ui: &mut egui::Ui, resp: &egui::Response, rect: egui::Rect) {
        let to_doc = |pos: egui::Pos2| (pos - rect.min).to_pos2();
        let shift = ui.input(|i| i.modifiers.shift);

        if resp.hovered() || resp.dragged() {
            let over_text = ui
                .input(|i| i.pointer.hover_pos())
                .is_some_and(|p| self.runs.is_text_at(to_doc(p)));
            if over_text || resp.dragged() {
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
                if let Some(pos) = resp.interact_pointer_pos() {
                    let _ = self.tx.send(Job::HitTest(HitTestJob {
                        html: self.html.clone(),
                        width: self.frame_layout_width,
                        scale: self.frame_scale,
                        x: pos.x - rect.left(),
                        y: pos.y - rect.top(),
                        backend: self.backend,
                    }));
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

    /// Whether there is a picture (or display list) for the current backend.
    fn has_frame(&self) -> bool {
        match self.backend {
            Backend::Pixbuf => !self.textures.is_empty(),
            Backend::Painter => self.list.is_some(),
        }
    }

    fn discard_frames(&mut self) {
        self.textures.clear();
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

    /// [`Backend::Pixbuf`]: the page is a grid of tiles (a GPU texture has a
    /// maximum side length, and a long message is taller than that), painted
    /// edge to edge.
    fn show_pixbuf(&mut self, ui: &mut egui::Ui) {
        if self.textures.is_empty() {
            self.show_placeholder(ui);
            return;
        }
        let (rect, resp) = ui.allocate_exact_size(self.frame_size, egui::Sense::click_and_drag());
        self.content_shown = true;
        let clip = ui.clip_rect();
        let scale = self.frame_scale;
        for tile in &self.textures {
            let [w, h] = tile.handle.size();
            let min = rect.min + egui::vec2(tile.x_px as f32, tile.y_px as f32) / scale;
            let tile_rect = egui::Rect::from_min_size(min, egui::vec2(w as f32, h as f32) / scale);
            // A long message is many screens of tiles; only paint
            // the ones that can be seen.
            if tile_rect.intersects(clip) {
                ui.painter().image(
                    tile.handle.id(),
                    tile_rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }
        }
        self.paint_selection(ui, rect);
        self.interact(ui, &resp, rect);
    }

    /// [`Backend::Painter`]: replay the display list, culled to what is
    /// visible.
    fn show_painter(&mut self, ui: &mut egui::Ui) {
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
            backend: self.backend,
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

    /// Take the text table of a new frame (either backend's).
    fn accept_runs(&mut self, runs: Arc<TextRunTable>) {
        self.runs = runs;
        // A new layout of the same text (a resize, images arriving, or the
        // other backend) keeps the selection: carets are run indexes, and the
        // same words come out in the same order. Different text (another
        // message) cannot.
        let signature = self.runs.text_signature();
        if signature != self.runs_signature {
            self.selection = None;
            self.runs_signature = signature;
        }
    }

    /// Apply everything the worker has produced so far. Returns the link
    /// clicks among it.
    fn poll_worker(&mut self, ctx: &egui::Context) -> Vec<WebViewEvent> {
        let mut events = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(Output::Frame(frame)) if frame.id == self.submitted_id => {
                    self.frame_size = frame.size_points;
                    self.frame_layout_width = frame.layout_width;
                    self.frame_scale = frame.scale;
                    self.accept_runs(frame.runs);
                    self.textures.truncate(frame.tiles.len());
                    for (i, tile) in frame.tiles.into_iter().enumerate() {
                        match self.textures.get_mut(i) {
                            Some(existing) => {
                                existing.handle.set(tile.image, egui::TextureOptions::LINEAR);
                                existing.x_px = tile.x_px;
                                existing.y_px = tile.y_px;
                            }
                            None => self.textures.push(TileTexture {
                                handle: ctx.load_texture(
                                    format!("{}_{i}", self.texture_name),
                                    tile.image,
                                    egui::TextureOptions::LINEAR,
                                ),
                                x_px: tile.x_px,
                                y_px: tile.y_px,
                            }),
                        }
                    }
                }
                Ok(Output::List(frame)) if frame.id == self.submitted_id => {
                    self.frame_size = frame.list.size;
                    self.frame_layout_width = frame.layout_width;
                    self.frame_scale = frame.scale;
                    self.accept_runs(frame.runs.clone());
                    self.list = Some(frame);
                }
                Ok(Output::Stats { id, elapsed }) if id == self.submitted_id => self.last_render = Some(elapsed),
                Ok(Output::Done { id, ok }) if id == self.submitted_id => {
                    self.rendering = false;
                    self.failed = !ok && !self.has_frame();
                }
                Ok(Output::Link(url)) => events.push(WebViewEvent::LinkClicked(url)),
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
        events
    }
}

// ─── Worker protocol ─────────────────────────────────────────────────────────

/// UI thread -> worker.
enum Job {
    Render(RenderJob),
    HitTest(HitTestJob),
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
    /// Which engine paints it.
    backend: Backend,
}

struct HitTestJob {
    html: Arc<String>,
    /// The layout width of the frame that was clicked, egui points.
    width: f32,
    scale: f32,
    /// Click position within the frame, egui points.
    x: f32,
    y: f32,
    /// The engine the clicked frame came from (fonts differ, so layout does).
    backend: Backend,
}

/// Worker -> UI thread.
enum Output {
    /// A finished (or intermediate) bitmap of the page ([`Backend::Pixbuf`]).
    Frame(Frame),
    /// A finished (or intermediate) display list ([`Backend::Painter`]).
    List(painter::ListFrame),
    /// How long render job `id` took overall. Sent just before its `Done`.
    Stats { id: u64, elapsed: Duration },
    /// The render job `id` has nothing more to send. `ok` is false when the
    /// document could not be rendered at all.
    Done { id: u64, ok: bool },
    /// An anchor was clicked.
    Link(String),
}

struct Frame {
    id: u64,
    /// Row-major grid of tiles that together make up the picture: opaque
    /// RGBA, cropped to the content height.
    tiles: Vec<Tile>,
    /// What to display the whole picture as, in egui points.
    size_points: egui::Vec2,
    layout_width: f32,
    scale: f32,
    /// The text of the page as laid out for this picture.
    runs: Arc<TextRunTable>,
}

/// One rectangle of a [`Frame`]. A GPU texture has a maximum side length
/// (`egui::InputState::max_texture_side`: 2048 headless, 8192-16384 on
/// typical GL), and a long newsletter at high DPI is taller than that, so
/// the picture is cut into tiles that each fit.
struct Tile {
    /// Top-left of the tile within the picture, in device pixels.
    x_px: usize,
    y_px: usize,
    image: egui::ColorImage,
}

/// A [`Tile`] uploaded to the GPU.
struct TileTexture {
    handle: egui::TextureHandle,
    x_px: usize,
    y_px: usize,
}

/// Cut `0..total` into consecutive `(start, len)` runs of at most `max`.
fn tile_ranges(total: usize, max: usize) -> Vec<(usize, usize)> {
    let max = max.max(1);
    (0..total).step_by(max).map(|start| (start, max.min(total - start))).collect()
}

/// Composite premultiplied-alpha RGBA in place onto opaque white. See
/// [`PixbufEngine::frame`] for why: `out = src_channel + (255 - alpha)`.
fn flatten_onto_white(rgba: &mut [u8]) {
    for px in rgba.chunks_exact_mut(4) {
        let alpha = px[3];
        if alpha == 255 {
            continue;
        }
        let carry = 255 - alpha;
        px[0] = px[0].saturating_add(carry);
        px[1] = px[1].saturating_add(carry);
        px[2] = px[2].saturating_add(carry);
        px[3] = 255;
    }
}

/// Copy the `w` x `h` block at `(x, y)` out of `pixels` (rows of
/// `stride_px` pixels, 4 bytes each) as a flattened, ready-to-upload tile.
fn extract_tile(pixels: &[u8], stride_px: usize, x: usize, y: usize, w: usize, h: usize) -> egui::ColorImage {
    let mut buf = Vec::with_capacity(w * h * 4);
    for row in y..y + h {
        let start = (row * stride_px + x) * 4;
        buf.extend_from_slice(&pixels[start..start + w * 4]);
    }
    flatten_onto_white(&mut buf);
    egui::ColorImage::from_rgba_premultiplied([w, h], &buf)
}

// ─── Engines ─────────────────────────────────────────────────────────────────

/// One way of turning litehtml's layout into something the UI can show. The
/// worker drives whichever [`Backend`] a job asks for through this; everything
/// around it (job queue, superseding, image discovery and fetching) is shared.
trait Engine {
    /// Forget which image URLs were already requested.
    fn clear_pending_images(&mut self);
    /// Image URLs layout discovered that are not loaded yet.
    fn take_pending_images(&mut self) -> Vec<(String, bool)>;
    /// Decode `bytes` and remember them as the image at `url`.
    fn load_image_data(&mut self, url: &str, bytes: &[u8]);
    /// Lay the document out at `width` points and paint / record it from
    /// scratch. Returns the content height in points, or `None` if the HTML
    /// could not be parsed.
    fn draw_pass(&mut self, html: &str, width: f32, scale: f32) -> Option<f32>;
    /// What the UI needs to show the last `draw_pass`.
    fn frame(&mut self, id: u64, width: f32, scale: f32, content_height: f32, max_texture_side: usize) -> Option<Output>;
    /// The anchor URL under document-local `(x, y)` (points), if any.
    fn hit_test(&mut self, html: &str, width: f32, scale: f32, x: f32, y: f32) -> Option<String>;
}

/// [`Backend::Pixbuf`]: litehtml-rs's `PixbufContainer` (tiny-skia, cosmic-text).
struct PixbufEngine {
    /// Owns the rendered pixels, the fonts and the decoded images. Reused
    /// across jobs (fonts are expensive to load; decoded images are keyed by
    /// URL, so re-opening a message does not re-download its images).
    container: PixbufContainer,
    /// The height (logical points) `container`'s pixel buffer is allocated
    /// for -- a *capacity*, not necessarily the content height. Never
    /// shrunk: reusing a too-tall buffer from a previous, longer message
    /// costs nothing but some unused canvas, and avoids the extra
    /// full parse+layout+draw pass that growing it forces. Only grows, when
    /// freshly-drawn content turns out not to fit.
    container_height: f32,
    /// The text of the page as of the last draw pass; sent with each frame.
    runs: Arc<TextRunTable>,
}

impl PixbufEngine {
    fn new(ctx: &egui::Context) -> Self {
        Self {
            container: PixbufContainer::new_with_scale(1, INITIAL_CANVAS_HEIGHT, ctx.pixels_per_point()),
            container_height: INITIAL_CANVAS_HEIGHT as f32,
            runs: Arc::default(),
        }
    }

    /// Parse + lay out + draw into the container's current pixel buffer.
    /// Returns the content height, and whether it was drawn: when the content
    /// is taller than `max_height` the (pointless) paint is skipped and the
    /// caller is expected to grow the canvas and call again.
    fn layout_and_draw(&mut self, html: &str, width: f32, max_height: f32) -> Option<(f32, bool)> {
        // Captured before the `Document` takes its mutable borrow of the
        // container; used to record the text while the `Document` is alive.
        let measure = self.container.text_measure_fn();
        let t_parse = Instant::now();
        // `master_css: None` is deliberate, not an oversight: the vendored
        // litehtml C++ core only falls back to its own **built-in** master
        // stylesheet (which is where `<h1>`/`<p>`/`<div>`/`<table>`/etc. get
        // their default `display: block`/`table-row`/etc. -- see
        // `litehtml_c.cpp`'s `lh_document_create_from_string`) when the
        // `master_css` argument is null. Passing `Some(EMAIL_MASTER_CSS)`
        // there *replaces* the built-in stylesheet outright rather than
        // layering on top of it, which produced a real, visible bug: every
        // element collapsed onto one or two inline-flowed lines.
        // `EMAIL_MASTER_CSS` (margin/table/link resets suited to email)
        // belongs as `user_styles` instead, which litehtml applies *after*
        // the built-in master and the document's own styles -- still low
        // enough specificity (plain type selectors) that a message's own
        // inline `style="..."` attributes win where they conflict.
        let mut doc = match Document::from_html(html, &mut self.container, None, Some(EMAIL_MASTER_CSS)) {
            Ok(doc) => doc,
            Err(e) => {
                log::warn!("egui-litehtml-webview: failed to parse message HTML: {e}");
                return None;
            }
        };
        let t_parse = t_parse.elapsed();

        let t_render = Instant::now();
        let _ = doc.render(width);
        let t_render = t_render.elapsed();

        let height = doc.height().max(1.0);
        if height > max_height + 0.5 {
            log::debug!("layout_and_draw: parse={t_parse:?} render(layout)={t_render:?} (too tall for the canvas, not drawn)");
            return Some((height, false));
        }

        let t_paint = Instant::now();
        doc.draw(DrawContext::default(), 0.0, 0.0, None);
        let t_paint = t_paint.elapsed();

        let t_runs = Instant::now();
        self.runs = Arc::new(TextRunTable::collect(&doc, &measure));
        let t_runs = t_runs.elapsed();

        log::debug!(
            "layout_and_draw: parse={t_parse:?} render(layout)={t_render:?} draw(paint)={t_paint:?} \
             text_runs={t_runs:?} ({})",
            self.runs.runs.len(),
        );
        Some((height, true))
    }

    /// Resize the pixel buffer to `width` x `height` (logical points) at
    /// `scale`. Clears existing pixel content -- callers must draw again
    /// afterwards.
    fn resize_container(&mut self, width: f32, height: f32, scale: f32) {
        let w = width.ceil().max(1.0) as u32;
        let h = height.ceil().max(1.0) as u32;
        self.container.resize_with_scale(w, h, scale);
        self.container_height = height;
    }
}

impl Engine for PixbufEngine {
    fn clear_pending_images(&mut self) {
        self.container.clear_pending_images();
    }

    fn take_pending_images(&mut self) -> Vec<(String, bool)> {
        self.container.take_pending_images()
    }

    fn load_image_data(&mut self, url: &str, bytes: &[u8]) {
        self.container.load_image_data(url, bytes);
    }

    /// Clear the canvas, then lay out and draw the document into it; if the
    /// content turns out taller than the canvas, grow it and draw once more.
    ///
    /// The canvas is *always* cleared first, even when its size is not
    /// changing: litehtml's `draw()` only paints where CSS says to, so
    /// drawing a second pass over the first leaves the first pass's text and
    /// images visible wherever the layout moved (overlapping text -- seen
    /// with image-heavy mail, whose second pass lays out differently once
    /// the images have real sizes). `resize_with_scale` is what resets the
    /// pixmap to transparent, and it is cheap (an alloc + zero-fill).
    fn draw_pass(&mut self, html: &str, width: f32, scale: f32) -> Option<f32> {
        self.resize_container(width, self.container_height, scale);
        let (height, drawn) = self.layout_and_draw(html, width, self.container_height)?;
        if drawn {
            return Some(height);
        }
        // Did not fit: grow the canvas and lay out again from scratch.
        self.resize_container(width, height, scale);
        Some(self.layout_and_draw(html, width, f32::INFINITY).map_or(height, |(h, _)| h))
    }

    /// Send the container's current pixels to the UI as a [`Frame`].
    ///
    /// `PixbufContainer::pixels`'s own doc comment says it returns
    /// **premultiplied** RGBA. It also starts out (and is cleared to)
    /// transparent, and litehtml only paints where CSS actually says to --
    /// unlike a real browser, which always paints an opaque white canvas. A
    /// message with no explicit `body { background }` (the overwhelming
    /// common case) would otherwise show whatever egui panel colour sits
    /// behind the texture through every unpainted region (dark-on-dark text
    /// in a dark theme). So flatten onto opaque white here. Compositing
    /// premultiplied-alpha `src` over opaque white simplifies to
    /// `out = src_channel + (255 - alpha)` per channel, so this needs no
    /// general alpha-blend math, just one add per byte.
    ///
    /// Only the rows up to `content_height` are sent: the canvas is usually
    /// taller than the content (see `container_height`), and the UI wants a
    /// picture that is exactly what should be displayed. The picture is cut
    /// into tiles no larger than the GPU's maximum texture side (read from
    /// the context, which eframe keeps in step with the GL limit), and each
    /// tile is flattened separately, so no full-size intermediate copy is made.
    fn frame(&mut self, id: u64, width: f32, scale: f32, content_height: f32, max_texture_side: usize) -> Option<Output> {
        let w = self.container.width() as usize;
        let canvas_rows = self.container.height() as usize;
        if w == 0 || canvas_rows == 0 {
            return None;
        }
        let rows = ((content_height * scale).ceil() as usize).clamp(1, canvas_rows);
        let pixels = self.container.pixels();
        let mut tiles = Vec::new();
        for (y, h) in tile_ranges(rows, max_texture_side) {
            for (x, tile_w) in tile_ranges(w, max_texture_side) {
                tiles.push(Tile { x_px: x, y_px: y, image: extract_tile(pixels, w, x, y, tile_w, h) });
            }
        }
        Some(Output::Frame(Frame {
            id,
            tiles,
            size_points: egui::vec2(w as f32 / scale, rows as f32 / scale),
            layout_width: width,
            scale,
            runs: self.runs.clone(),
        }))
    }

    /// Feed litehtml a down+up click at document-local `(x, y)` (logical
    /// points, the same space `render()` was called with) and report the
    /// anchor URL if that completed a click on a link. Layout alone is
    /// enough for litehtml's own hit-testing, no `draw()` needed -- but it
    /// is still a full parse + layout, since no `Document` is kept between
    /// jobs (see the crate module doc).
    fn hit_test(&mut self, html: &str, width: f32, scale: f32, x: f32, y: f32) -> Option<String> {
        let width = width.max(1.0);
        self.resize_container(width, self.container_height, scale.max(0.1));
        let Ok(mut doc) = Document::from_html(html, &mut self.container, None, Some(EMAIL_MASTER_CSS)) else {
            return None;
        };
        let _ = doc.render(width);
        doc.on_lbutton_down(x, y, x, y);
        doc.on_lbutton_up(x, y, x, y);
        drop(doc);
        self.container.take_anchor_click()
    }
}

// ─── Worker ──────────────────────────────────────────────────────────────────

/// Most raw image bytes [`Worker::fetched`] keeps before it starts over.
const FETCHED_CACHE_LIMIT: usize = 64 * 1024 * 1024;

/// Everything that lives on the worker thread.
struct Worker {
    /// Engines are created on first use, on this thread (both are `!Send`).
    pixbuf: Option<PixbufEngine>,
    painter: Option<painter::PainterEngine>,
    handler: Arc<dyn WebViewHandler>,
    ctx: egui::Context,
    out: Sender<Output>,
    /// See [`WebView::latest_id`].
    latest_id: Arc<AtomicU64>,
    /// Cumulative count of `load_image_data` calls -- diagnostic only,
    /// logged per job. Neither engine's decoded-image cache has an eviction
    /// API, so this is a proxy for how large they have grown.
    total_images_loaded: u64,
    /// The previous render job was abandoned (superseded) after litehtml had
    /// already recorded its image URLs as requested. Those URLs would then
    /// never be requested again -- the container skips URLs it has seen --
    /// so the next job must forget them, or a resize mid-load leaves the
    /// message permanently missing images.
    reset_images_next: bool,
    /// Raw bytes of every remote image fetched so far, so that switching
    /// [`Backend`] (whose engine has its own, empty image cache) does not
    /// download the page's images a second time.
    fetched: HashMap<String, Arc<Vec<u8>>>,
    fetched_bytes: usize,
}

impl Worker {
    fn new(
        ctx: egui::Context,
        handler: Arc<dyn WebViewHandler>,
        out: Sender<Output>,
        latest_id: Arc<AtomicU64>,
    ) -> Self {
        Self {
            pixbuf: None,
            painter: None,
            handler,
            ctx,
            out,
            latest_id,
            total_images_loaded: 0,
            reset_images_next: false,
            fetched: HashMap::new(),
            fetched_bytes: 0,
        }
    }

    /// The engine for `backend`, created on first use. That first use is
    /// where system fonts get loaded.
    fn engine(&mut self, backend: Backend) -> &mut dyn Engine {
        match backend {
            Backend::Pixbuf => self.pixbuf.get_or_insert_with(|| PixbufEngine::new(&self.ctx)),
            Backend::Painter => self.painter.get_or_insert_with(|| painter::PainterEngine::new(&self.ctx)),
        }
    }

    /// Serve jobs until the [`WebView`] (the only sender) is dropped.
    fn run(mut self, jobs: Receiver<Job>) {
        while let Ok(first) = jobs.recv() {
            // Everything queued while the last job ran: only the newest
            // render matters (the UI ignores older ones' frames anyway).
            let mut render: Option<RenderJob> = None;
            let mut reset_images = false;
            let mut hit_tests = Vec::new();
            for job in std::iter::once(first).chain(jobs.try_iter()) {
                match job {
                    Job::Render(r) => {
                        // A dropped job may have been the one asking to
                        // forget requested URLs (a `load`); its replacement
                        // (say, a resize) must still do that.
                        reset_images |= r.reset_images;
                        render = Some(r);
                    }
                    Job::HitTest(h) => hit_tests.push(h),
                }
            }
            if let Some(mut r) = render {
                r.reset_images = reset_images;
                self.render(&r);
            }
            for h in &hit_tests {
                self.hit_test(h);
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
        let backend = job.backend;
        if job.reset_images || std::mem::take(&mut self.reset_images_next) {
            self.engine(backend).clear_pending_images();
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
            let Some(height) = self.engine(backend).draw_pass(&job.html, width, scale) else {
                ok = false;
                break;
            };
            passes += 1;
            // Whether this pass's picture has already gone to the UI.
            let mut emitted = false;

            let pending = self.engine(backend).take_pending_images();
            if pending.is_empty() || passes == MAX_PASSES {
                self.emit_frame(job.id, backend, width, scale, height);
                break;
            }

            let (local, remote): (Vec<String>, Vec<String>) =
                pending.into_iter().map(|(url, _)| url).partition(|url| url.starts_with("data:"));
            let mut loaded = self.load_images(
                backend,
                local
                    .into_iter()
                    .filter_map(|url| resolve_image_bytes(&url, &*self.handler).map(|bytes| (url, Arc::new(bytes))))
                    .collect(),
            );

            // Images another engine (or an earlier job) already downloaded.
            let (cached, remote): (Vec<String>, Vec<String>) =
                remote.into_iter().partition(|url| self.fetched.contains_key(url));
            let cached: Vec<(String, Arc<Vec<u8>>)> =
                cached.into_iter().map(|url| { let bytes = self.fetched[&url].clone(); (url, bytes) }).collect();
            loaded |= self.load_images(backend, cached);

            if !remote.is_empty() {
                // Let the user read the text while images download.
                self.emit_frame(job.id, backend, width, scale, height);
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
                loaded |= self.load_images(backend, downloaded);
            }

            if !loaded {
                if !emitted {
                    self.emit_frame(job.id, backend, width, scale, height);
                }
                break;
            }
            // Images changed what there is to draw (and possibly where):
            // go around again on a fresh canvas.
        }

        let elapsed = t_total.elapsed();
        log::debug!(
            "render job {} ({}): total={elapsed:?} passes={passes} remote_fetched={fetched} html_len={} \
             total_images_loaded={}",
            job.id, backend.name(), job.html.len(), self.total_images_loaded,
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

    /// Decode `images` into `backend`'s engine. Returns whether any loaded.
    fn load_images(&mut self, backend: Backend, images: Vec<(String, Arc<Vec<u8>>)>) -> bool {
        let mut any = false;
        for (url, bytes) in images {
            self.engine(backend).load_image_data(&url, &bytes);
            self.total_images_loaded += 1;
            any = true;
        }
        any
    }

    /// Send `backend`'s current picture of the page to the UI.
    fn emit_frame(&mut self, id: u64, backend: Backend, width: f32, scale: f32, content_height: f32) {
        let max_side = self.ctx.input(|i| i.max_texture_side);
        if let Some(output) = self.engine(backend).frame(id, width, scale, content_height, max_side) {
            self.send(output);
        }
    }

    /// Report the anchor under a click, if any. Needs the same layout the
    /// clicked frame had, so it runs on the engine that drew it.
    fn hit_test(&mut self, job: &HitTestJob) {
        let (width, scale) = (job.width.max(1.0), job.scale.max(0.1));
        if let Some(url) = self.engine(job.backend).hit_test(&job.html, width, scale, job.x, job.y) {
            self.send(Output::Link(url));
        }
    }
}

/// Decide how to resolve one pending image URL, without touching the
/// container -- pulled out so it's testable without a real
/// [`PixbufContainer`]/`Document`. `data:` URLs are decoded locally; a
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
            backend: Backend::Pixbuf,
        });
        out_rx.try_iter().collect()
    }

    fn last_frame(outputs: &[Output]) -> &Frame {
        outputs
            .iter()
            .rev()
            .find_map(|o| match o {
                Output::Frame(f) => Some(f),
                _ => None,
            })
            .expect("a frame was sent")
    }

    fn pixel(frame: &Frame, x: usize, y: usize) -> [u8; 4] {
        let tile = frame
            .tiles
            .iter()
            .find(|t| {
                (t.x_px..t.x_px + t.image.size[0]).contains(&x) && (t.y_px..t.y_px + t.image.size[1]).contains(&y)
            })
            .expect("pixel is inside the frame");
        tile.image.pixels[(y - tile.y_px) * tile.image.size[0] + (x - tile.x_px)].to_array()
    }

    /// Total size of the picture, in device pixels.
    fn size_px(frame: &Frame) -> [usize; 2] {
        [
            frame.tiles.iter().map(|t| t.x_px + t.image.size[0]).max().unwrap(),
            frame.tiles.iter().map(|t| t.y_px + t.image.size[1]).max().unwrap(),
        ]
    }

    #[test]
    fn an_image_is_scaled_to_its_laid_out_size_not_drawn_at_its_natural_size() {
        // A 1x1 image displayed at 100x100 must fill that whole box; drawn at
        // its natural size (the old behaviour) it would be a single pixel.
        let html = format!(
            r#"<body style="margin:0"><img src="{RED_1X1_PNG}" width="100" height="100"></body>"#
        );
        let outputs = render_in_process(&html, 300.0, Arc::new(DefaultHandler));
        assert!(matches!(outputs.last(), Some(Output::Done { id: 1, ok: true })));
        let frame = last_frame(&outputs);
        assert_eq!(size_px(frame)[0], 300);
        // Cropped to the content: 100px of image, not the 800px seed canvas.
        assert!((100..110).contains(&size_px(frame)[1]), "height was {}", size_px(frame)[1]);
        for (x, y) in [(2, 2), (50, 50), (97, 97)] {
            assert_eq!(pixel(frame, x, y), [255, 0, 0, 255], "inside the image at ({x},{y})");
        }
        assert_eq!(pixel(frame, 150, 50), [255, 255, 255, 255], "outside the image");
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
        // Exactly one black rule line in the final frame: at y = 100 (below
        // the 100px image), and not also where pass 1 would have put it.
        let black_rows: Vec<usize> = (0..size_px(frame)[1])
            .filter(|&y| pixel(frame, 150, y) == [0, 0, 0, 255])
            .collect();
        assert_eq!(black_rows, vec![100], "rule drawn at the wrong place(s): {black_rows:?}");
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
        let job = |id, reset_images| RenderJob { id, html: html.clone(), width: 100.0, scale: 1.0, reset_images, backend: Backend::Pixbuf };
        worker.render(&job(1, true));
        worker.render(&job(2, false));
        let outputs: Vec<Output> = out_rx.try_iter().collect();
        let frame = last_frame(&outputs);
        assert_eq!(frame.id, 2);
        assert_eq!(pixel(frame, 10, 10), [255, 0, 0, 255], "the image was never re-requested");
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
    fn tile_ranges_cover_the_whole_extent_in_order() {
        assert_eq!(tile_ranges(10, 4), vec![(0, 4), (4, 4), (8, 2)]);
        assert_eq!(tile_ranges(8, 4), vec![(0, 4), (4, 4)]);
        assert_eq!(tile_ranges(3, 4), vec![(0, 3)]);
        assert_eq!(tile_ranges(5, 0), vec![(0, 1), (1, 1), (2, 1), (3, 1), (4, 1)], "a zero limit must not hang");
        assert!(tile_ranges(0, 4).is_empty());
    }

    #[test]
    fn extract_tile_copies_the_right_block() {
        // A 5x4 picture whose red channel encodes the pixel index.
        let pixels: Vec<u8> = (0..20u8).flat_map(|i| [i, 0, 0, 255]).collect();
        let tile = extract_tile(&pixels, 5, 3, 1, 2, 2);
        assert_eq!(tile.size, [2, 2]);
        let reds: Vec<u8> = tile.pixels.iter().map(|p| p.r()).collect();
        assert_eq!(reds, vec![8, 9, 13, 14]);
    }

    #[test]
    fn a_frame_taller_than_the_texture_limit_is_split_into_tiles_that_fit() {
        // Headless egui reports a 2048px texture limit; this page is far taller.
        let ctx = egui::Context::default();
        let max = ctx.input(|i| i.max_texture_side);
        let (out_tx, out_rx) = mpsc::channel();
        let mut worker = Worker::new(ctx, Arc::new(DefaultHandler), out_tx, Arc::new(AtomicU64::new(1)));
        let html = r#"<body style="margin:0"><div style="height:5000px;background:#f00"></div><div style="height:10px;background:#00f"></div></body>"#;
        worker.render(&RenderJob { id: 1, html: Arc::new(html.to_string()), width: 100.0, scale: 1.0, reset_images: true, backend: Backend::Pixbuf });
        let outputs: Vec<Output> = out_rx.try_iter().collect();
        let frame = last_frame(&outputs);

        assert!(frame.tiles.len() >= 3, "5010px at {max}px per tile needs at least 3 tiles, got {}", frame.tiles.len());
        assert!(frame.tiles.iter().all(|t| t.image.size[0] <= max && t.image.size[1] <= max));
        assert_eq!(size_px(frame), [100, 5010], "tiles must add up to the whole picture");
        // Content is intact across every seam.
        for y in [0, max - 1, max, 2 * max - 1, 2 * max, 4999] {
            assert_eq!(pixel(frame, 50, y), [255, 0, 0, 255], "red band at y={y}");
        }
        assert_eq!(pixel(frame, 50, 5005), [0, 0, 255, 255], "blue strip in the last tile");
    }

    #[test]
    fn show_paints_a_message_taller_than_the_texture_limit_without_error() {
        // egui debug-asserts on an oversize texture upload, so merely
        // getting through `show` is the check.
        let ctx = egui::Context::default();
        let host = WebViewHost::new();
        let html = r#"<body style="margin:0"><div style="height:5000px;background:#eee">tall</div></body>"#;
        let mut view = host.new_view(&ctx, WebViewConfig::new(WebViewSource::Html(html.to_string())));
        let deadline = Instant::now() + Duration::from_secs(60);
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(300.0, 300.0))),
            ..Default::default()
        };
        loop {
            // Nothing uploads the textures headless; egui asserts that an
            // unapplied `TexturesDelta` is cleared rather than dropped.
            ctx.run_ui(input(), |ui| {
                view.show(ui);
            }).textures_delta.clear();
            if !view.is_rendering() {
                break;
            }
            assert!(Instant::now() < deadline, "render never finished");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(view.textures.len() >= 3);
        assert!(view.content_size().unwrap().y >= 5000.0);
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
    fn a_click_on_a_link_reports_its_url_via_the_worker() {
        let (out_tx, out_rx) = mpsc::channel();
        let mut worker = Worker::new(
            egui::Context::default(),
            Arc::new(DefaultHandler),
            out_tx,
            Arc::new(AtomicU64::new(1)),
        );
        let html = Arc::new(
            r#"<body style="margin:0"><a href="https://example.com/x" style="display:block;height:40px">go</a></body>"#
                .to_string(),
        );
        worker.hit_test(&HitTestJob { html: html.clone(), width: 200.0, scale: 1.0, x: 5.0, y: 5.0, backend: Backend::Pixbuf });
        assert!(matches!(out_rx.try_recv(), Ok(Output::Link(url)) if url == "https://example.com/x"));
        worker.hit_test(&HitTestJob { html, width: 200.0, scale: 1.0, x: 5.0, y: 300.0, backend: Backend::Pixbuf });
        assert!(out_rx.try_recv().is_err(), "a click on empty space is not a link click");
    }

    #[test]
    fn show_renders_off_the_ui_thread_and_ends_up_with_a_texture() {
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
        assert!(view.textures.is_empty());
        while view.is_rendering() {
            assert!(Instant::now() < deadline, "render never finished");
            std::thread::sleep(Duration::from_millis(10));
            ctx.run_ui(input(), |ui| {
                view.show(ui);
            }).textures_delta.clear();
        }
        assert!(!view.textures.is_empty());
        assert!(!view.failed);
    }

    // ── Selection interaction, driven with synthetic egui input ─────────

    /// A view in a headless egui context that tests poke with pointer and
    /// keyboard events, one frame at a time.
    /// Selection is geometry over the text-run table, so it has to behave the same
    /// whichever engine drew the page: every test below runs once per backend.
    const BACKENDS: [Backend; 2] = [Backend::Pixbuf, Backend::Painter];

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
        /// An egui text field shown above the view, to test focus.
        field: Option<String>,
        field_id: egui::Id,
    }

    const SCREEN: egui::Vec2 = egui::vec2(400.0, 300.0);

    impl Harness {
        fn new(backend: Backend, html: &str, with_field: bool) -> Self {
            let ctx = egui::Context::default();
            let view = WebViewHost::new()
                .new_view(&ctx, WebViewConfig::new(WebViewSource::Html(html.to_string())).with_backend(backend));
            let mut h = Self {
                ctx,
                view,
                origin: egui::Pos2::ZERO,
                time: 1.0,
                modifiers: egui::Modifiers::NONE,
                commands: Vec::new(),
                links: Vec::new(),
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
            let name = self.view.texture_name.clone();
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
        for backend in BACKENDS {
            dragging_across_text_selects_it_on(backend);
        }
    }

    fn dragging_across_text_selects_it_on(backend: Backend) {
        let mut h = Harness::new(backend, TWO_PARAS, false);
        h.drag(h.left_of("alpha"), h.right_of("gamma"));
        assert_eq!(h.selected().as_deref(), Some("alpha beta gamma"));
        assert!(h.view.has_selection());

        // Dragging backwards, into the second paragraph, is the same gesture.
        h.drag(h.right_of("delta"), h.left_of("alpha"));
        assert_eq!(h.selected().as_deref(), Some("alpha beta gamma\n\ndelta"));
    }

    #[test]
    fn a_drag_starting_on_a_link_selects_instead_of_following_it() {
        for backend in BACKENDS {
            a_drag_starting_on_a_link_selects_instead_of_following_it_on(backend);
        }
    }

    fn a_drag_starting_on_a_link_selects_instead_of_following_it_on(backend: Backend) {
        let html = r#"<body style="margin:0"><p style="margin:0"><a href="https://example.com/x">click here now</a></p></body>"#;
        let mut h = Harness::new(backend, html, false);
        h.drag(h.left_of("click"), h.right_of("now"));
        assert_eq!(h.selected().as_deref(), Some("click here now"));
        // The worker would answer a hit test within moments: give it the chance.
        std::thread::sleep(Duration::from_millis(500));
        h.frame(vec![], egui::Modifiers::NONE);
        assert!(h.links.is_empty(), "a drag must not open the link: {:?}", h.links);
    }

    #[test]
    fn a_plain_click_on_a_link_still_reports_it_and_clears_the_selection() {
        for backend in BACKENDS {
            a_plain_click_on_a_link_still_reports_it_and_clears_the_selection_on(backend);
        }
    }

    fn a_plain_click_on_a_link_still_reports_it_and_clears_the_selection_on(backend: Backend) {
        let html = r#"<body style="margin:0"><p style="margin:0"><a href="https://example.com/x">link</a> and other words</p></body>"#;
        let mut h = Harness::new(backend, html, false);
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
        for backend in BACKENDS {
            double_click_selects_a_word_and_triple_click_the_paragraph_on(backend);
        }
    }

    fn double_click_selects_a_word_and_triple_click_the_paragraph_on(backend: Backend) {
        let mut h = Harness::new(backend, TWO_PARAS, false);
        let p = h.middle_of("beta");
        h.click(p);
        h.click(p);
        assert_eq!(h.selected().as_deref(), Some("beta"));
        h.click(p);
        assert_eq!(h.selected().as_deref(), Some("alpha beta gamma"));
    }

    #[test]
    fn shift_click_extends_the_selection() {
        for backend in BACKENDS {
            shift_click_extends_the_selection_on(backend);
        }
    }

    fn shift_click_extends_the_selection_on(backend: Backend) {
        let mut h = Harness::new(backend, TWO_PARAS, false);
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
        for backend in BACKENDS {
            copy_puts_the_selection_on_the_clipboard_on(backend);
        }
    }

    fn copy_puts_the_selection_on_the_clipboard_on(backend: Backend) {
        let mut h = Harness::new(backend, TWO_PARAS, false);
        let p = h.middle_of("epsilon");
        h.click(p);
        h.click(p);
        assert!(h.copied().is_empty());
        h.frame(vec![egui::Event::Copy], egui::Modifiers::NONE);
        assert_eq!(h.copied(), vec!["epsilon".to_string()]);
    }

    #[test]
    fn copy_with_nothing_selected_does_nothing() {
        for backend in BACKENDS {
            copy_with_nothing_selected_does_nothing_on(backend);
        }
    }

    fn copy_with_nothing_selected_does_nothing_on(backend: Backend) {
        let mut h = Harness::new(backend, TWO_PARAS, false);
        h.click(h.middle_of("delta"));
        h.frame(vec![egui::Event::Copy], egui::Modifiers::NONE);
        assert!(h.copied().is_empty());
    }

    #[test]
    fn ctrl_a_selects_everything_when_the_view_has_focus() {
        for backend in BACKENDS {
            ctrl_a_selects_everything_when_the_view_has_focus_on(backend);
        }
    }

    fn ctrl_a_selects_everything_when_the_view_has_focus_on(backend: Backend) {
        let mut h = Harness::new(backend, TWO_PARAS, false);
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
        for backend in BACKENDS {
            copy_belongs_to_a_focused_text_field_not_to_the_selection_on(backend);
        }
    }

    fn copy_belongs_to_a_focused_text_field_not_to_the_selection_on(backend: Backend) {
        let mut h = Harness::new(backend, TWO_PARAS, true);
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
        for backend in BACKENDS {
            clicking_outside_the_view_clears_the_selection_on(backend);
        }
    }

    fn clicking_outside_the_view_clears_the_selection_on(backend: Backend) {
        let mut h = Harness::new(backend, TWO_PARAS, true);
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
        for backend in BACKENDS {
            the_selection_survives_a_relayout_of_the_same_text_but_not_a_new_page_on(backend);
        }
    }

    fn the_selection_survives_a_relayout_of_the_same_text_but_not_a_new_page_on(backend: Backend) {
        let mut h = Harness::new(backend, TWO_PARAS, false);
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
        for backend in BACKENDS {
            dragging_below_the_visible_area_scrolls_the_page_and_keeps_selecting_on(backend);
        }
    }

    fn dragging_below_the_visible_area_scrolls_the_page_and_keeps_selecting_on(backend: Backend) {
        let many: String = (0..60).map(|i| format!("<p style=\"margin:0 0 10px\">line number {i}</p>")).collect();
        let mut h = Harness::new(backend, &format!("<body style=\"margin:0\">{many}</body>"), false);
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

    #[test]
    fn a_selection_survives_switching_backend() {
        // Same text, same runs: the engines differ in fonts, not in what the words are.
        let mut h = Harness::new(Backend::Pixbuf, TWO_PARAS, false);
        h.drag(h.left_of("alpha"), h.right_of("gamma"));
        let before = h.selected();
        assert_eq!(before.as_deref(), Some("alpha beta gamma"));
        h.view.set_backend(Backend::Painter);
        h.settle();
        assert_eq!(h.view.backend(), Backend::Painter);
        assert_eq!(h.selected(), before, "the selection was dropped by the switch");
        h.view.set_backend(Backend::Pixbuf);
        h.settle();
        assert_eq!(h.selected(), before);
    }

    // ── Backend::Painter ────────────────────────────────────────────────

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
    fn the_painter_backend_waits_for_its_fonts_and_then_paints_text() {
        // egui panics on a font family it does not know; the view must hold
        // the display list back until the context has the worker's fonts.
        let ctx = egui::Context::default();
        let host = WebViewHost::new();
        let html = r#"<body style="margin:0"><p style="font-family:Arial,sans-serif;font-size:20px">Hello</p><p>go &#10148;</p></body>"#;
        let mut view = host.new_view(
            &ctx,
            WebViewConfig::new(WebViewSource::Html(html.to_string())).with_backend(Backend::Painter),
        );
        assert_eq!(view.backend(), Backend::Painter);
        // "Hello", "go" and the arrow: three runs.
        let text = show_until_text_is_painted(&ctx, &mut view, 400.0, 3);
        assert_eq!(text, 3, "one text shape per recorded run, and no placeholder label left over");
        assert!(view.textures.is_empty(), "the painter backend uploads no page bitmap");
        assert!(view.content_size().is_some_and(|s| s.y > 20.0));
        assert!(view.last_render_time().is_some());
    }

    #[test]
    fn switching_backend_re_renders_the_same_page_with_the_other_engine() {
        let ctx = egui::Context::default();
        let host = WebViewHost::new();
        let html = r#"<body style="margin:0"><p style="font-size:20px">Hello</p></body>"#;
        let mut view = host.new_view(&ctx, WebViewConfig::new(WebViewSource::Html(html.to_string())));
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 300.0))),
            ..Default::default()
        };
        let settle = |view: &mut WebView| {
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                ctx.run_ui(input(), |ui| {
                    view.show(ui);
                }).textures_delta.clear();
                if !view.is_rendering() {
                    return;
                }
                assert!(Instant::now() < deadline, "render never finished");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        settle(&mut view);
        assert!(!view.textures.is_empty() && view.list.is_none());

        view.set_backend(Backend::Painter);
        assert!(view.content_size().is_none(), "the old engine's picture is dropped at once");
        assert!(view.is_rendering());
        settle(&mut view);
        assert!(view.textures.is_empty() && view.list.is_some());
        assert!(view.content_size().is_some());

        view.set_backend(Backend::Pixbuf);
        settle(&mut view);
        assert!(!view.textures.is_empty() && view.list.is_none());
    }

    #[test]
    fn a_link_click_goes_to_the_engine_that_drew_the_frame() {
        let (out_tx, out_rx) = mpsc::channel();
        let mut worker = Worker::new(egui::Context::default(), Arc::new(DefaultHandler), out_tx, Arc::new(AtomicU64::new(1)));
        let html = Arc::new(
            r#"<body style="margin:0"><a href="https://example.com/p" style="display:block;height:40px">go</a></body>"#.to_string(),
        );
        worker.hit_test(&HitTestJob { html, width: 200.0, scale: 1.0, x: 5.0, y: 5.0, backend: Backend::Painter });
        assert!(matches!(out_rx.try_recv(), Ok(Output::Link(url)) if url == "https://example.com/p"));
        assert!(worker.painter.is_some() && worker.pixbuf.is_none(), "only the painter engine was needed");
    }

    #[test]
    fn a_downloaded_image_is_not_fetched_again_when_the_other_backend_needs_it() {
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
        let job = |id, backend| RenderJob { id, html: html.clone(), width: 100.0, scale: 1.0, reset_images: true, backend };
        worker.render(&job(1, Backend::Painter));
        worker.latest_id.store(2, Ordering::SeqCst);
        worker.render(&job(2, Backend::Pixbuf));
        assert_eq!(*handler.calls.lock().unwrap(), 1, "the second engine must reuse the first download");
        let outputs: Vec<Output> = out_rx.try_iter().collect();
        let frame = last_frame(&outputs);
        assert_eq!(frame.id, 2);
        assert_eq!(pixel(frame, 10, 10), [255, 0, 0, 255], "and still draw it");
    }

    #[test]
    fn a_scroll_offset_set_before_the_first_render_is_applied_once_there_is_a_page() {
        for backend in [Backend::Pixbuf, Backend::Painter] {
            let ctx = egui::Context::default();
            let host = WebViewHost::new();
            let html = r#"<body style="margin:0"><div style="height:5000px;background:#eee">tall</div></body>"#;
            let mut view = host.new_view(&ctx, WebViewConfig::new(WebViewSource::Html(html.to_string())).with_backend(backend));
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
                assert!(Instant::now() < deadline, "{backend:?}: render never finished");
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!((view.scroll_offset() - 700.0).abs() < 1.0, "{backend:?}: scrolled to {}", view.scroll_offset());
        }
    }

    #[test]
    fn the_selection_highlight_is_painted_over_the_painter_backends_display_list() {
        let ctx = egui::Context::default();
        let host = WebViewHost::new();
        let html = r#"<body style="margin:0"><p style="font-size:20px">alpha beta</p></body>"#;
        let mut view = host.new_view(
            &ctx,
            WebViewConfig::new(WebViewSource::Html(html.to_string())).with_backend(Backend::Painter),
        );
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

    #[test]
    fn switching_backend_keeps_the_scroll_position() {
        // Comparing engines means looking at the same spot: the placeholder shown
        // while the other engine renders must not reset the scroll to the top.
        let ctx = egui::Context::default();
        let host = WebViewHost::new();
        let html = r#"<body style="margin:0"><div style="height:5000px;background:#eee">tall</div></body>"#;
        let mut view = host.new_view(&ctx, WebViewConfig::new(WebViewSource::Html(html.to_string())));
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(300.0, 400.0))),
            ..Default::default()
        };
        let settle = |view: &mut WebView| {
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
        };
        view.set_scroll_offset(700.0);
        settle(&mut view);
        assert!((view.scroll_offset() - 700.0).abs() < 1.0);
        for backend in [Backend::Painter, Backend::Pixbuf] {
            view.set_backend(backend);
            settle(&mut view);
            assert!((view.scroll_offset() - 700.0).abs() < 1.0, "{backend:?}: scrolled to {}", view.scroll_offset());
        }
    }
}
