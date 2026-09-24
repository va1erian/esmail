//! The render worker thread: owns the `!Send` litehtml `Document` + container
//! (see `container.rs`) and runs one render job at a time, dropping superseded
//! jobs. This is a stripped port of `egui-litehtml-webview`'s worker: no
//! remote image fetching (the prototype decodes `data:` URIs only), so a job
//! is at most two passes (discover images, then redraw with them loaded).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use litehtml::html::decode_data_uri;
use win32ui::d2d::TextSystem;

use crate::engine::Engine;
use crate::list::Frame;

/// Fetches the bytes of a remote image, or `None` when it cannot be had.
pub type ImageFetcher = Arc<dyn Fn(&str) -> Option<Vec<u8>> + Send + Sync>;

/// The fetcher the UI thread installs and the render thread reads.
pub(crate) type ImageSource = Arc<Mutex<Option<ImageFetcher>>>;

/// Upper bound on draw passes for one render job: pass 1 discovers `data:`
/// image URLs, pass 2 draws with them decoded. (Remote images are out of scope
/// for this prototype; a later milestone hands them to a host fetch hook.)
const MAX_PASSES: usize = 2;

/// UI thread -> worker.
pub(crate) enum Job {
    Render(RenderJob),
}

pub(crate) struct RenderJob {
    pub id: u64,
    pub html: Arc<String>,
    /// Layout width in device-independent pixels.
    pub width: f32,
    /// Forget which image URLs were already requested before starting.
    pub reset_images: bool,
}

/// Worker -> UI thread.
pub(crate) enum Output {
    /// A finished display list of the page.
    Frame(Frame),
    /// The document could not be rendered at all.
    Failed { id: u64 },
}

/// Everything that lives on the worker thread.
pub(crate) struct Worker {
    /// Created on first use, on this thread (the `Document` it builds is `!Send`).
    engine: Option<Engine>,
    text: TextSystem,
    out: Sender<Output>,
    latest_id: Arc<AtomicU64>,
    /// Posts to the UI thread when a frame is ready.
    wake: Arc<dyn Fn() + Send + Sync>,
    /// A render was abandoned mid-pass after litehtml recorded image URLs as
    /// requested; the next job must forget them or a resize mid-load leaves the
    /// message permanently missing images.
    reset_images_next: bool,
    images: ImageSource,
}

impl Worker {
    pub(crate) fn new(
        text: TextSystem,
        out: Sender<Output>,
        latest_id: Arc<AtomicU64>,
        wake: Arc<dyn Fn() + Send + Sync>,
        images: ImageSource,
    ) -> Self {
        Self { engine: None, text, out, latest_id, wake, reset_images_next: false, images }
    }

    fn engine(&mut self) -> &mut Engine {
        self.engine.get_or_insert_with(|| Engine::new(self.text.clone()))
    }

    fn superseded(&self, id: u64) -> bool {
        self.latest_id.load(Ordering::SeqCst) != id
    }

    fn send(&self, output: Output) {
        let _ = self.out.send(output);
        (self.wake)();
    }

    /// Serve jobs until the [`HtmlWidget`](crate::HtmlWidget) (the only sender)
    /// is dropped.
    pub(crate) fn run(mut self, jobs: Receiver<Job>) {
        while let Ok(first) = jobs.recv() {
            // Everything queued while the last job ran: only the newest render
            // matters.
            if let Some(job) = newest_job(std::iter::once(first).chain(jobs.try_iter())) {
                self.render(&job);
            }
        }
    }

    fn render(&mut self, job: &RenderJob) {
        let t_total = Instant::now();
        if job.reset_images || std::mem::take(&mut self.reset_images_next) {
            self.engine().clear_pending_images();
        }
        let width = job.width.max(1.0);

        let mut passes = 0;
        let mut ok = true;
        while passes < MAX_PASSES {
            if self.superseded(job.id) {
                self.reset_images_next = true;
                return;
            }
            let Some(height) = self.engine().draw_pass(&job.html, width) else {
                ok = false;
                break;
            };
            passes += 1;

            let pending = self.engine().take_pending_images();
            if pending.is_empty() || passes == MAX_PASSES {
                self.emit_frame(job.id, width, height);
                break;
            }

            // `data:` URIs are decoded here; remote URLs go to the host's fetcher,
            // if it installed one, and stay unloaded (the display list never gets
            // pixels for them) otherwise.
            let fetcher = self.images.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
            let mut loaded = false;
            for (url, _) in pending {
                let bytes = decode_data_uri(&url).or_else(|| fetcher.as_ref().and_then(|fetch| fetch(&url)));
                if let Some(bytes) = bytes {
                    self.engine().load_image_data(&url, &bytes);
                    loaded = true;
                }
            }
            if !loaded {
                self.emit_frame(job.id, width, height);
                break;
            }
            // Images changed what there is to draw: go around again.
        }

        let elapsed = t_total.elapsed();
        log::debug!("d2d render job {}: total={elapsed:?} passes={passes}", job.id);
        if !ok {
            self.send(Output::Failed { id: job.id });
        }
    }

    fn emit_frame(&mut self, id: u64, width: f32, content_height: f32) {
        let frame = self.engine().frame(id, width, content_height);
        self.send(Output::Frame(frame));
    }
}

/// Keeps only the newest render job of a queue, OR-ing every dropped job's
/// `reset_images` into the survivor (a `load` that got superseded by a resize
/// must still be forgotten).
fn newest_job(jobs: impl Iterator<Item = Job>) -> Option<RenderJob> {
    let mut render: Option<RenderJob> = None;
    let mut reset_images = false;
    for job in jobs {
        let Job::Render(r) = job;
        reset_images |= r.reset_images;
        render = Some(r);
    }
    render.map(|mut r| {
        r.reset_images = reset_images;
        r
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: u64, reset: bool) -> Job {
        Job::Render(RenderJob { id, html: Arc::from("x".to_string()), width: 100.0, reset_images: reset })
    }

    #[test]
    fn superseded_jobs_collapse_to_the_newest() {
        let chosen = newest_job(vec![job(1, false), job(2, false), job(3, true)].into_iter());
        let Some(chosen) = chosen else { panic!("no job") };
        assert_eq!(chosen.id, 3);
    }

    #[test]
    fn a_dropped_load_still_forces_a_reset() {
        // A `load` (id 2, reset) superseded by a resize (id 3, no reset): the
        // survivor must still forget requested image URLs.
        let chosen = newest_job(vec![job(1, false), job(2, true), job(3, false)].into_iter());
        let Some(chosen) = chosen else { panic!("no job") };
        assert_eq!(chosen.id, 3);
        assert!(chosen.reset_images, "the dropped load's reset must survive");
    }

    #[test]
    fn an_empty_queue_has_no_job() {
        assert!(newest_job(Vec::new().into_iter()).is_none());
    }
}
