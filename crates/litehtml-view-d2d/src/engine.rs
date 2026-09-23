//! The [`Engine`]: runs one layout + record pass over the container and hands
//! back a finished [`Frame`]. The `Document` built here is `!Send`, so the
//! engine lives on the worker thread (see `worker.rs`).

use std::sync::Arc;

use litehtml::{Document, DrawContext};
use win32ui::d2d::TextSystem;

use crate::container::{D2dContainer, ua_sheet};
use crate::list::{DisplayList, Frame};

/// Everything the worker thread owns: the container plus the finished frame.
pub(crate) struct Engine {
    container: D2dContainer,
}

impl Engine {
    pub(crate) fn new(text: TextSystem) -> Self {
        Self { container: D2dContainer::new(text) }
    }

    /// Forget which image URLs were already requested.
    pub(crate) fn clear_pending_images(&mut self) {
        self.container.clear_pending_images();
    }

    /// Image URLs layout discovered that are not loaded yet.
    pub(crate) fn take_pending_images(&mut self) -> Vec<(String, bool)> {
        self.container.take_pending_images()
    }

    /// Decode `bytes` and remember them as the image at `url`.
    pub(crate) fn load_image_data(&mut self, url: &str, bytes: &[u8]) {
        self.container.load_image_data(url, bytes);
    }

    /// Lay the document out at `width` device-independent pixels and record it.
    /// Returns the content height, or `None` if the HTML could not be parsed.
    pub(crate) fn draw_pass(&mut self, html: &str, width: f32) -> Option<f32> {
        self.container.begin(width);
        let mut doc = match Document::from_html(html, &mut self.container, Some(ua_sheet()), None) {
            Ok(doc) => doc,
            Err(e) => {
                log::warn!("litehtml-view-d2d: failed to parse message HTML: {e}");
                return None;
            }
        };
        let t = std::time::Instant::now();
        let _ = doc.render(width);
        let t_layout = t.elapsed();
        let height = doc.height().max(1.0);
        let t = std::time::Instant::now();
        doc.draw(DrawContext::default(), 0.0, 0.0, None);
        let t_record = t.elapsed();
        drop(doc);
        let (cmds, fonts, images, calls, hits) = self.container.stats();
        log::debug!(
            "d2d draw_pass: layout={t_layout:?} record={t_record:?} cmds={cmds} fonts={fonts} \
             images={images} text_width={calls} ({hits} cache hits)",
        );
        Some(height)
    }

    /// What the UI needs to show the last `draw_pass`.
    pub(crate) fn frame(&mut self, id: u64, width: f32, content_height: f32) -> Frame {
        let (cmds, fonts, images) = self.container.take_frame();
        Frame {
            id,
            list: Arc::new(DisplayList { cmds, size: (width, content_height), fonts, images }),
        }
    }
}
