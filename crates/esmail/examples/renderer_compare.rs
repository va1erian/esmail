//! The two message renderers side by side, on the same page, scrolling together.
//!
//! Left: `Backend::Pixbuf` (litehtml-rs's `PixbufContainer`: tiny-skia +
//! cosmic-text rasterize a bitmap). Right: `Backend::Painter` (litehtml's layout
//! recorded as a display list and painted with egui). Each header shows how long
//! the worker took over that engine's newest render.
//!
//! ```text
//! cargo run --release -p esmail --example renderer_compare -- <page.eml | page.html> \
//!     [--images]              fetch remote images (default: blocked, like the app before "Load remote images")
//!     [--scroll <points>]     start scrolled this far down
//!     [--screenshot out.png]  capture the window once both have settled, then exit
//! ```
//!
//! `.eml` files go through `esmail::render::render_message`, i.e. exactly the
//! pipeline a live message takes; anything else is read as HTML.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use egui_litehtml_webview::{
    Backend, ImageRequest, InterceptOutcome, WebView, WebViewConfig, WebViewHandler, WebViewHost, WebViewSource,
};

/// Fetches remote images when asked to; blocks them otherwise.
struct Fetcher {
    allow: bool,
    agent: ureq::Agent,
}

impl WebViewHandler for Fetcher {
    fn intercept(&self, request: &ImageRequest) -> InterceptOutcome {
        if !self.allow {
            return InterceptOutcome::Block;
        }
        match self.agent.get(&request.url).call().map(|r| r.into_body().read_to_vec()) {
            Ok(Ok(bytes)) => InterceptOutcome::Serve(bytes),
            _ => InterceptOutcome::Block,
        }
    }
}

struct Args {
    page: PathBuf,
    images: bool,
    scroll: Option<f32>,
    screenshot: Option<PathBuf>,
}

fn parse_args() -> Args {
    let mut page = None;
    let (mut images, mut scroll, mut screenshot) = (false, None, None);
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--images" => images = true,
            "--scroll" => scroll = it.next().and_then(|v| v.parse().ok()),
            "--screenshot" => screenshot = it.next().map(PathBuf::from),
            _ => page = Some(PathBuf::from(a)),
        }
    }
    let Some(page) = page else {
        eprintln!("usage: renderer_compare <page.eml | page.html> [--images] [--scroll <points>] [--screenshot out.png]");
        std::process::exit(2);
    };
    Args { page, images, scroll, screenshot }
}

fn load(page: &std::path::Path) -> String {
    let is_eml = page.extension().is_some_and(|e| e.eq_ignore_ascii_case("eml"));
    if is_eml {
        let raw = std::fs::read(page).unwrap_or_else(|e| panic!("{}: {e}", page.display()));
        esmail::render::render_message(&raw)
    } else {
        std::fs::read_to_string(page).unwrap_or_else(|e| panic!("{}: {e}", page.display()))
    }
}

struct App {
    _host: WebViewHost,
    left: WebView,
    right: WebView,
    /// The offset both views were last brought to.
    synced: f32,
    /// `--scroll` still waiting to take effect in both views (until then the
    /// sync below must not read their still-zero offsets as a user scroll).
    initial_scroll: Option<f32>,
    initial_wait_frames: u32,
    screenshot: Option<PathBuf>,
    /// Frames both views have been settled for.
    settled_frames: u32,
    capture_requested: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, args: Args) -> Self {
        let html = load(&args.page);
        let handler = Arc::new(Fetcher {
            allow: args.images,
            agent: ureq::Agent::new_with_config(
                ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(15))).build(),
            ),
        });
        let host = WebViewHost::new();
        let make = |backend| {
            host.new_view(
                &cc.egui_ctx,
                WebViewConfig::new(WebViewSource::Html(html.clone()))
                    .with_handler(handler.clone())
                    .with_backend(backend),
            )
        };
        let (mut left, mut right) = (make(Backend::Pixbuf), make(Backend::Painter));
        // Held by the views until they have a page to scroll.
        if let Some(y) = args.scroll {
            left.set_scroll_offset(y);
            right.set_scroll_offset(y);
        }
        Self {
            left,
            right,
            _host: host,
            synced: 0.0,
            initial_scroll: args.scroll,
            initial_wait_frames: 0,
            screenshot: args.screenshot,
            settled_frames: 0,
            capture_requested: false,
        }
    }

    fn header(ui: &mut egui::Ui, view: &WebView, title: &str) {
        let render = view
            .last_render_time()
            .map_or_else(|| "rendering...".to_string(), |t| format!("{:.0} ms", t.as_secs_f64() * 1000.0));
        let size = view.content_size().map_or_else(String::new, |s| format!("  \u{b7}  {:.0} x {:.0} pt", s.x, s.y));
        ui.horizontal(|ui| {
            ui.strong(title);
            ui.monospace(format!("{}  \u{b7}  {render}{size}", view.backend().name()));
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        egui::CentralPanel::default().show(ui, |ui| {
            ui.columns(2, |cols| {
                Self::header(&mut cols[0], &self.left, "tiny-skia (Pixbuf)");
                cols[0].separator();
                for e in self.left.show(&mut cols[0]) {
                    log::info!("left: {e:?}");
                }
                Self::header(&mut cols[1], &self.right, "egui painter");
                cols[1].separator();
                for e in self.right.show(&mut cols[1]) {
                    log::info!("right: {e:?}");
                }
            });
        });

        // Scroll together: whichever moved since last frame drags the other.
        let (l, r) = (self.left.scroll_offset(), self.right.scroll_offset());
        if let Some(y) = self.initial_scroll {
            // Give up after a while: a page shorter than `y` never gets there.
            self.initial_wait_frames += 1;
            if ((l - y).abs() < 1.0 && (r - y).abs() < 1.0) || self.initial_wait_frames > 300 {
                self.initial_scroll = None;
                self.synced = l;
            }
            ctx.request_repaint();
        } else if (l - self.synced).abs() > 0.5 {
            self.synced = l;
            self.right.set_scroll_offset(l);
            ctx.request_repaint();
        } else if (r - self.synced).abs() > 0.5 {
            self.synced = r;
            self.left.set_scroll_offset(r);
            ctx.request_repaint();
        }

        // Screenshot mode.
        if let Some(path) = self.screenshot.clone() {
            ctx.request_repaint();
            let ready = !self.left.is_rendering() && !self.right.is_rendering() && self.initial_scroll.is_none();
            self.settled_frames = if ready { self.settled_frames + 1 } else { 0 };
            if self.settled_frames >= 30 && !self.capture_requested {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
                self.capture_requested = true;
            }
            let captured = ctx.input(|i| {
                i.events.iter().find_map(|e| match e {
                    egui::Event::Screenshot { image, .. } => Some(image.clone()),
                    _ => None,
                })
            });
            if let Some(image) = captured {
                let [w, h] = image.size;
                match image::RgbaImage::from_raw(w as u32, h as u32, image.as_raw().to_vec()) {
                    Some(png) => match png.save(&path) {
                        Ok(()) => eprintln!("wrote {}", path.display()),
                        Err(e) => eprintln!("could not write {}: {e}", path.display()),
                    },
                    None => eprintln!("screenshot buffer had the wrong length"),
                }
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }
}

fn main() -> eframe::Result {
    env_logger::init();
    let args = parse_args();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1600.0, 900.0]),
        ..Default::default()
    };
    eframe::run_native("esmail: renderer compare", options, Box::new(|cc| Ok(Box::new(App::new(cc, args)))))
}
