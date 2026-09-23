//! The `HtmlView` widget: a win32ui [`CustomWidget`] that owns a render worker
//! and paints its latest frame with Direct2D.

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;

use win32ui::d2d::{D2dCanvas, RectF, TextSystem};
use win32ui::gdi::Canvas;
use win32ui::{
    Color, Custom, CustomWidget, Input, Key, Rect as PxRect, Result, Size, Theme, Ui, WidgetCx,
};

use crate::geom::Rect;
use crate::list::Frame;
use crate::paint::Painter;
use crate::worker::{Job, Output, RenderJob, Worker};

/// How many device-independent pixels one wheel notch scrolls.
const WHEEL_LINE_DIP: f32 = 60.0;
/// How many device-independent pixels one arrow-key press scrolls.
const KEY_LINE_DIP: f32 = 40.0;

/// The owner-drawn widget behind an [`HtmlView`]. All mutable state lives in
/// `Cell`/`RefCell` fields because the win32ui `CustomWidget` trait hands the
/// widget `&self`.
pub struct HtmlWidget {
    painter: RefCell<Painter>,
    tx: Sender<Job>,
    rx: Receiver<Output>,
    latest_id: Arc<AtomicU64>,
    submitted_id: Cell<u64>,
    requested_width: Cell<f32>,
    reset_images: Cell<bool>,
    dirty: Cell<bool>,
    failed: Cell<bool>,
    html: RefCell<String>,
    frame: RefCell<Option<Frame>>,
    /// Vertical scroll offset in device-independent pixels.
    scroll: Cell<f32>,
    /// The viewport height as of the last paint, for page-scroll clamping.
    viewport_height: Cell<f32>,
}

impl HtmlWidget {
    fn new(text: TextSystem, tx: Sender<Job>, rx: Receiver<Output>, latest_id: Arc<AtomicU64>, html: String) -> Self {
        Self {
            painter: RefCell::new(Painter::new(text)),
            tx,
            rx,
            latest_id,
            submitted_id: Cell::new(0),
            requested_width: Cell::new(0.0),
            reset_images: Cell::new(false),
            dirty: Cell::new(true),
            failed: Cell::new(false),
            html: RefCell::new(html),
            frame: RefCell::new(None),
            scroll: Cell::new(0.0),
            viewport_height: Cell::new(0.0),
        }
    }

    /// Load a new page, dropping the current frame immediately so a slow render
    /// does not show the previous page.
    pub fn load(&self, html: String) {
        *self.html.borrow_mut() = html;
        self.frame.borrow_mut().take();
        self.scroll.set(0.0);
        self.failed.set(false);
        self.reset_images.set(true);
        self.dirty.set(true);
    }

    /// Whether the newest render has finished and its frame is available.
    pub fn is_ready(&self) -> bool {
        self.frame.borrow().as_ref().is_some_and(|f| f.id == self.submitted_id.get())
    }

    /// Sets the vertical scroll offset (device-independent pixels), clamped on
    /// the next paint.
    pub fn set_scroll(&self, y: f32) {
        self.scroll.set(y.max(0.0));
    }

    fn submit(&self, width: f32) {
        self.submitted_id.set(self.submitted_id.get() + 1);
        self.latest_id.store(self.submitted_id.get(), Ordering::SeqCst);
        let job = RenderJob {
            id: self.submitted_id.get(),
            html: Arc::from(self.html.borrow().clone()),
            width,
            reset_images: self.reset_images.take(),
        };
        if self.tx.send(Job::Render(job)).is_err() {
            self.failed.set(true);
        }
        self.requested_width.set(width);
        self.dirty.set(false);
    }

    fn poll(&self) {
        loop {
            match self.rx.try_recv() {
                Ok(Output::Frame(frame)) if frame.id == self.submitted_id.get() => {
                    *self.frame.borrow_mut() = Some(frame);
                }
                Ok(Output::Failed { id }) if id == self.submitted_id.get() => self.failed.set(true),
                // A frame or failure for a superseded job.
                Ok(_) => {}
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.failed.set(true);
                    break;
                }
            }
        }
    }

    fn content_height(&self) -> f32 {
        self.frame.borrow().as_ref().map_or(0.0, |f| f.list.size.1)
    }

    fn max_scroll(&self) -> f32 {
        (self.content_height() - self.viewport_height.get()).max(0.0)
    }

    fn scroll_by(&self, delta: f32) {
        let next = (self.scroll.get() + delta).clamp(0.0, self.max_scroll());
        self.scroll.set(next);
    }
}

impl CustomWidget for HtmlWidget {
    type Event = ();

    // This widget is Direct2D-only; the GDI fallback (used only when Direct2D
    // cannot create a surface) leaves the page blank.
    fn paint(&self, _canvas: &Canvas, _bounds: PxRect, _theme: &Theme) {}

    fn paint_d2d(&self, canvas: &mut D2dCanvas, bounds: RectF, _theme: &Theme) {
        let width = bounds.width().max(1.0);
        let height = bounds.height().max(1.0);
        self.viewport_height.set(height);

        if self.dirty.get() || (width - self.requested_width.get()).abs() > 0.5 {
            self.submit(width);
        }
        self.poll();

        let scroll = self.scroll.get().clamp(0.0, self.max_scroll());
        self.scroll.set(scroll);

        let viewport = Rect::new(0.0, 0.0, width, height);
        match self.frame.borrow().as_ref() {
            Some(frame) => self.painter.borrow_mut().paint(&frame.list, canvas, viewport, scroll),
            None => canvas.clear(Color::rgb(255, 255, 255)),
        }
    }

    fn input(&self, input: Input, cx: &mut WidgetCx<()>) {
        match input {
            Input::MouseWheel { delta, horizontal: false, .. } => {
                self.scroll_by(-delta as f32 / 120.0 * WHEEL_LINE_DIP);
                cx.invalidate();
            }
            Input::KeyDown { key, .. } => {
                let page = self.viewport_height.get();
                let delta = if key == Key::DOWN {
                    KEY_LINE_DIP
                } else if key == Key::UP {
                    -KEY_LINE_DIP
                } else if key == Key::PAGE_DOWN {
                    page
                } else if key == Key::PAGE_UP {
                    -page
                } else if key == Key::HOME {
                    -self.scroll.get()
                } else if key == Key::END {
                    self.max_scroll() - self.scroll.get()
                } else {
                    0.0
                };
                if delta != 0.0 {
                    self.scroll_by(delta);
                    cx.invalidate();
                }
            }
            _ => {}
        }
    }

    fn preferred_size(&self, _dpi: u32) -> Option<Size> {
        None
    }
}

/// An HTML view hosted in its own win32ui child window, rendered on a worker
/// thread with litehtml + Direct2D.
///
/// The worker is created here and lives for the view's lifetime; `on_frame` is
/// the app message that tells the host a new frame is ready (mapped through a
/// `Proxy`, so the UI thread is never blocked).
pub struct HtmlView<M: 'static> {
    widget: Custom<HtmlWidget, M>,
}

impl<M: Send + 'static> HtmlView<M> {
    /// Creates the view, loading `html`, and starts its worker thread.
    pub fn new(
        ui: &mut Ui<M>,
        html: String,
        on_frame: impl Fn() -> M + Send + Sync + 'static,
    ) -> Result<HtmlView<M>> {
        let text = TextSystem::new()?;
        let (job_tx, job_rx) = mpsc::channel();
        let (out_tx, out_rx) = mpsc::channel();
        let latest_id = Arc::new(AtomicU64::new(0));

        let widget = Custom::new(
            ui,
            HtmlWidget::new(text.clone(), job_tx.clone(), out_rx, latest_id.clone(), html),
        )?;

        // Worker -> UI wakeup: a `Proxy` post mapped through `on_frame`.
        let proxy = ui.proxy();
        let on_frame = Arc::new(on_frame);
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let _ = proxy.send(on_frame());
        });
        let spawned = std::thread::Builder::new()
            .name("litehtml-d2d-worker".to_string())
            .spawn(move || {
                let worker = Worker::new(text, out_tx, latest_id, wake);
                worker.run(job_rx);
            });
        if let Err(e) = &spawned {
            log::error!("litehtml-view-d2d: could not start the render thread: {e}");
        }

        Ok(HtmlView { widget })
    }

    /// Loads a new page, replacing whatever is currently shown.
    pub fn load(&self, html: String) {
        self.widget.widget().borrow().load(html);
        self.widget.invalidate();
    }

    /// Whether the newest render has finished.
    pub fn is_ready(&self) -> bool {
        self.widget.widget().borrow().is_ready()
    }

    /// Sets the vertical scroll offset (device-independent pixels).
    pub fn set_scroll(&self, y: f32) {
        self.widget.widget().borrow().set_scroll(y);
        self.widget.invalidate();
    }

    /// Repaints the widget (used when the worker reports a frame is ready).
    pub fn invalidate(&self) {
        self.widget.invalidate();
    }
}

impl<M: 'static> win32ui::AsControl for HtmlView<M> {
    fn control(&self) -> &win32ui::Control {
        self.widget.control()
    }
}
