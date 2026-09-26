//! The public [`HtmlView`]: an HTML view hosted in its own xui-win32 child
//! window, rendered on a worker thread with litehtml + Direct2D (see the crate
//! docs and `widget.rs`'s `HtmlWidget`).

use std::sync::atomic::AtomicU64;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use xui_win32::d2d::TextSystem;
use xui_win32::{Custom, Result, Ui};

use crate::widget::HtmlWidget;
use crate::worker::{ImageFetcher, ImageSource, Worker};

/// An event raised by an [`HtmlView`], mapped to the app's message type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HtmlViewEvent {
    /// The user clicked a link (an `<a href>`). litehtml has no navigation
    /// concept of its own, so the host decides what to do with it.
    LinkClicked(String),
}

/// An HTML view hosted in its own xui-win32 child window, rendered on a worker
/// thread with litehtml + Direct2D.
///
/// The worker is created here and lives for the view's lifetime; `on_frame` is
/// the app message that tells the host a new frame is ready (mapped through a
/// `Proxy`, so the UI thread is never blocked), and `on_event` maps the view's
/// [`HtmlViewEvent`]s to the app's `Msg`.
pub struct HtmlView<M: 'static> {
    widget: Custom<HtmlWidget, M>,
    images: ImageSource,
}

impl<M: Send + 'static> HtmlView<M> {
    /// Creates the view, loading `html`, and starts its worker thread.
    pub fn new(
        ui: &mut Ui<M>,
        html: String,
        on_frame: impl Fn() -> M + Send + Sync + 'static,
        on_event: impl Fn(HtmlViewEvent) -> Option<M> + 'static,
    ) -> Result<HtmlView<M>> {
        let text = TextSystem::new()?;
        let (job_tx, job_rx) = mpsc::channel();
        let (out_tx, out_rx) = mpsc::channel();
        let latest_id = Arc::new(AtomicU64::new(0));
        let scale = ui.dpi() as f32 / 96.0;
        let images: ImageSource = Arc::new(Mutex::new(None));
        let worker_images = Arc::clone(&images);

        let widget = Custom::new(
            ui,
            HtmlWidget::new(text.clone(), job_tx.clone(), out_rx, latest_id.clone(), html, scale, ui.hwnd()),
        )?
        .on_event(on_event);

        // Worker -> UI wakeup: a `Proxy` post mapped through `on_frame`.
        let proxy = ui.proxy();
        let on_frame = Arc::new(on_frame);
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let _ = proxy.send(on_frame());
        });
        let spawned = std::thread::Builder::new()
            .name("litehtml-d2d-worker".to_string())
            .spawn(move || {
                let worker = Worker::new(text, out_tx, latest_id, wake, worker_images);
                worker.run(job_rx);
            });
        if let Err(e) = &spawned {
            log::error!("litehtml-view-d2d: could not start the render thread: {e}");
        }

        Ok(HtmlView { widget, images })
    }

    /// Sets how `http(s)` images are fetched, or `None` (the default) to leave
    /// them unloaded. The fetcher runs on the render thread and may block. It
    /// applies to the next [`load`](Self::load).
    pub fn set_image_fetcher(&self, fetcher: Option<ImageFetcher>) {
        *self.images.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = fetcher;
    }

    /// Loads a new page, replacing whatever is currently shown.
    pub fn load(&self, html: String) {
        self.widget.widget().borrow().load(html);
        self.widget.invalidate();
    }

    /// Sets the colour shown behind the document and wherever it paints
    /// nothing (white by default). Pair it with a stylesheet that sets the
    /// text colour, so unstyled messages follow the app's theme.
    pub fn set_background(&self, color: xui_win32::Color) {
        self.widget.widget().borrow().set_background(color);
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

    /// The selected text, as a copy should read, or `None` if nothing is
    /// selected.
    pub fn selected_text(&self) -> Option<String> {
        self.widget.widget().borrow().selected_text()
    }

    /// Whether any text is selected.
    pub fn has_selection(&self) -> bool {
        self.widget.widget().borrow().has_selection()
    }

    /// Select all the text of the page.
    pub fn select_all(&self) {
        self.widget.widget().borrow().select_all();
        self.widget.invalidate();
    }

    /// Drop the selection.
    pub fn clear_selection(&self) {
        self.widget.widget().borrow().clear_selection();
        self.widget.invalidate();
    }

    /// Repaints the widget (used when the worker reports a frame is ready).
    pub fn invalidate(&self) {
        self.widget.invalidate();
    }
}

impl<M: 'static> xui_win32::AsControl for HtmlView<M> {
    fn control(&self) -> &xui_win32::Control {
        self.widget.control()
    }
}
