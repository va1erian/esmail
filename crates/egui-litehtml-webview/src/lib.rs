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

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use litehtml::email::EMAIL_MASTER_CSS;
use litehtml::html::decode_data_uri;
use litehtml::pixbuf::PixbufContainer;
use litehtml::{Document, DrawContext};

mod text_runs;
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

// ─── WebViewHost ─────────────────────────────────────────────────────────────

/// Creates [`WebView`]s.
///
/// litehtml's `pixbuf` backend renders entirely on the CPU, so there is no
/// engine to own, no window handle, and no GL context to set up. This type
/// still exists (rather than a bare associated function on `WebView`) to
/// keep the call shape `esmail`'s `main.rs` already uses -- one host per
/// window, producing any number of views -- even though today it is little
/// more than an id counter so two views in the same window don't collide on
/// one egui texture name.
#[derive(Default)]
pub struct WebViewHost {
    next_view_id: std::cell::Cell<u64>,
}

impl WebViewHost {
    /// Create a host. Takes nothing: litehtml's CPU-only `pixbuf` backend
    /// needs nothing from the host window (no window handle, no GL context).
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
                // is !Send. It also loads system fonts, which is slow
                // enough that it should not block the UI at startup either.
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
            textures: Vec::new(),
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
    /// The current frame, one texture per tile (see [`Tile`]). Reused across
    /// frames when the tile count is unchanged; reallocating per frame would
    /// be wasteful. Empty until the first frame arrives.
    textures: Vec<TileTexture>,
    /// Unique per view, so two views cannot collide on one egui texture.
    texture_name: String,
    /// What the current frame should be displayed as, in egui points.
    frame_size: egui::Vec2,
    /// The layout width the current texture was rendered at, in points --
    /// hit tests must be laid out at the same width to line up.
    frame_layout_width: f32,
    /// `pixels_per_point` the current texture was rendered at.
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
        self.textures.clear();
        self.runs = Arc::default();
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
        (!self.textures.is_empty()).then_some(self.frame_size)
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
        // height, so a plain `ScrollArea` around a normally-sized allocation
        // gets a native scrollbar and native wheel-scroll for free. The
        // picture is a grid of tiles (a GPU texture has a maximum side length, and a long
        // message is taller than that), painted edge to edge.
        egui::ScrollArea::vertical()
            .id_salt(&self.texture_name)
            .show(ui, |ui| {
                if self.textures.is_empty() {
                    if self.failed {
                        ui.label("Could not render this message.");
                    } else if self.rendering {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Rendering...");
                        });
                    }
                    return;
                }
                let (rect, resp) = ui.allocate_exact_size(self.frame_size, egui::Sense::click());
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

                if resp.clicked() {
                    if let Some(pos) = resp.interact_pointer_pos() {
                        let _ = self.tx.send(Job::HitTest(HitTestJob {
                            html: self.html.clone(),
                            width: self.frame_layout_width,
                            scale: self.frame_scale,
                            x: pos.x - rect.left(),
                            y: pos.y - rect.top(),
                        }));
                    }
                }
            });

        events
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
                    self.runs = frame.runs;
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
                Ok(Output::Done { id, ok }) if id == self.submitted_id => {
                    self.rendering = false;
                    self.failed = !ok && self.textures.is_empty();
                }
                Ok(Output::Link(url)) => events.push(WebViewEvent::LinkClicked(url)),
                // A frame/completion for a superseded job.
                Ok(_) => {}
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if self.rendering {
                        log::error!("egui-litehtml-webview: the render thread died mid-render");
                        self.rendering = false;
                        self.failed = self.textures.is_empty();
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
}

struct HitTestJob {
    html: Arc<String>,
    /// The layout width of the frame that was clicked, egui points.
    width: f32,
    scale: f32,
    /// Click position within the frame, egui points.
    x: f32,
    y: f32,
}

/// Worker -> UI thread.
enum Output {
    /// A finished (or intermediate) picture of the page.
    Frame(Frame),
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
/// [`Worker::emit_frame`] for why: `out = src_channel + (255 - alpha)`.
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

// ─── Worker ──────────────────────────────────────────────────────────────────

/// Everything that lives on the worker thread.
struct Worker {
    /// Owns the rendered pixels, the fonts and the decoded images. Reused
    /// across jobs (fonts are expensive to load; decoded images are keyed by
    /// URL, so re-opening a message does not re-download its images).
    container: PixbufContainer,
    handler: Arc<dyn WebViewHandler>,
    ctx: egui::Context,
    out: Sender<Output>,
    /// See [`WebView::latest_id`].
    latest_id: Arc<AtomicU64>,
    /// The height (logical points) `container`'s pixel buffer is allocated
    /// for -- a *capacity*, not necessarily the content height. Never
    /// shrunk: reusing a too-tall buffer from a previous, longer message
    /// costs nothing but some unused canvas, and avoids the extra
    /// full parse+layout+draw pass that growing it forces. Only grows, when
    /// freshly-drawn content turns out not to fit.
    container_height: f32,
    /// The text of the page as of the last draw pass; sent with each frame.
    runs: Arc<TextRunTable>,
    /// Cumulative count of `load_image_data` calls -- diagnostic only,
    /// logged per job. `PixbufContainer`'s decoded-image cache has no
    /// eviction API, so this is a proxy for how large it has grown.
    total_images_loaded: u64,
    /// The previous render job was abandoned (superseded) after litehtml had
    /// already recorded its image URLs as requested. Those URLs would then
    /// never be requested again -- the container skips URLs it has seen --
    /// so the next job must forget them, or a resize mid-load leaves the
    /// message permanently missing images.
    reset_images_next: bool,
}

impl Worker {
    fn new(
        ctx: egui::Context,
        handler: Arc<dyn WebViewHandler>,
        out: Sender<Output>,
        latest_id: Arc<AtomicU64>,
    ) -> Self {
        Self {
            container: PixbufContainer::new_with_scale(1, INITIAL_CANVAS_HEIGHT, ctx.pixels_per_point()),
            handler,
            ctx,
            out,
            latest_id,
            container_height: INITIAL_CANVAS_HEIGHT as f32,
            runs: Arc::default(),
            total_images_loaded: 0,
            reset_images_next: false,
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
        if job.reset_images || std::mem::take(&mut self.reset_images_next) {
            self.container.clear_pending_images();
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
            let Some(height) = self.draw_pass(&job.html, width, scale) else {
                ok = false;
                break;
            };
            passes += 1;
            // Whether this pass's picture has already gone to the UI.
            let mut emitted = false;

            let pending = self.container.take_pending_images();
            if pending.is_empty() || passes == MAX_PASSES {
                self.emit_frame(job.id, width, scale, height);
                break;
            }

            let (local, remote): (Vec<String>, Vec<String>) =
                pending.into_iter().map(|(url, _)| url).partition(|url| url.starts_with("data:"));
            let mut loaded = self.load_images(local.into_iter().filter_map(|url| {
                resolve_image_bytes(&url, &*self.handler).map(|bytes| (url, bytes))
            }).collect());

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
                loaded |= self.load_images(downloaded);
            }

            if !loaded {
                if !emitted {
                    self.emit_frame(job.id, width, scale, height);
                }
                break;
            }
            // Images changed what there is to draw (and possibly where):
            // go around again on a fresh canvas.
        }

        log::debug!(
            "render job {}: total={:?} passes={passes} remote_fetched={fetched} html_len={} \
             container_height={:.0} total_images_loaded={}",
            job.id, t_total.elapsed(), job.html.len(), self.container_height, self.total_images_loaded,
        );
        self.send(Output::Done { id: job.id, ok });
    }

    /// Decode `images` into the container. Returns whether any loaded.
    fn load_images(&mut self, images: Vec<(String, Vec<u8>)>) -> bool {
        let mut any = false;
        for (url, bytes) in images {
            self.container.load_image_data(&url, &bytes);
            self.total_images_loaded += 1;
            any = true;
        }
        any
    }

    /// Clear the canvas, then lay out and draw the document into it; if the
    /// content turns out taller than the canvas, grow it and draw once more.
    /// Returns the content height (logical points), or `None` if the HTML
    /// could not be parsed.
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
    fn emit_frame(&self, id: u64, width: f32, scale: f32, content_height: f32) {
        let w = self.container.width() as usize;
        let canvas_rows = self.container.height() as usize;
        if w == 0 || canvas_rows == 0 {
            return;
        }
        let rows = ((content_height * scale).ceil() as usize).clamp(1, canvas_rows);
        let max_side = self.ctx.input(|i| i.max_texture_side);
        let pixels = self.container.pixels();
        let mut tiles = Vec::new();
        for (y, h) in tile_ranges(rows, max_side) {
            for (x, tile_w) in tile_ranges(w, max_side) {
                tiles.push(Tile { x_px: x, y_px: y, image: extract_tile(pixels, w, x, y, tile_w, h) });
            }
        }
        self.send(Output::Frame(Frame {
            id,
            tiles,
            size_points: egui::vec2(w as f32 / scale, rows as f32 / scale),
            layout_width: width,
            scale,
            runs: self.runs.clone(),
        }));
    }

    /// Feed litehtml a down+up click at document-local `(x, y)` (logical
    /// points, the same space `render()` was called with) and report the
    /// anchor URL if that completed a click on a link. Layout alone is
    /// enough for litehtml's own hit-testing, no `draw()` needed -- but it
    /// is still a full parse + layout, since no `Document` is kept between
    /// jobs (see the crate module doc).
    fn hit_test(&mut self, job: &HitTestJob) {
        let width = job.width.max(1.0);
        self.resize_container(width, self.container_height, job.scale.max(0.1));
        let Ok(mut doc) = Document::from_html(&job.html, &mut self.container, None, Some(EMAIL_MASTER_CSS)) else {
            return;
        };
        let _ = doc.render(width);
        doc.on_lbutton_down(job.x, job.y, job.x, job.y);
        doc.on_lbutton_up(job.x, job.y, job.x, job.y);
        drop(doc);
        if let Some(url) = self.container.take_anchor_click() {
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
        let job = |id, reset_images| RenderJob { id, html: html.clone(), width: 100.0, scale: 1.0, reset_images };
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
        worker.render(&RenderJob { id: 1, html: Arc::new(html.to_string()), width: 100.0, scale: 1.0, reset_images: true });
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
        worker.hit_test(&HitTestJob { html: html.clone(), width: 200.0, scale: 1.0, x: 5.0, y: 5.0 });
        assert!(matches!(out_rx.try_recv(), Ok(Output::Link(url)) if url == "https://example.com/x"));
        worker.hit_test(&HitTestJob { html, width: 200.0, scale: 1.0, x: 5.0, y: 300.0 });
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
}
