//! Renders an HTML file (or `.eml` fixture, or the built-in `demo` page) in a
//! win32ui window with an [`HtmlView`] filling it.
//!
//! ```text
//! cargo run -p litehtml-view-d2d --example view -- <file.html>
//! cargo run -p litehtml-view-d2d --example view -- demo --screenshot out.png
//! ```
//!
//! `--screenshot out.png` renders once, captures via `Window::capture` and
//! exits (the capture path is guarded by a timer so a render always has an
//! exit path). `--width`/`--height` set the window size in design units.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use litehtml_view_d2d::HtmlView;
use win32ui::column;
use win32ui::prelude::*;

enum Msg {
    FrameReady,
    Tick,
}

struct App {
    view: HtmlView<Msg>,
    screenshot: Option<String>,
    ticks: u64,
    scroll: Option<f32>,
    applied_scroll: bool,
}

const DEMO: &str = r#"<!doctype html>
<meta charset="utf-8">
<style>
  body { font: 16px/1.5 system-ui, sans-serif; margin: 2rem; color: #111; }
  table { border-collapse: collapse; } td, th { border: 1px solid #999; padding: .3rem .6rem; }
  .tall { height: 60vh; background: linear-gradient(#eee, #fff); }
</style>
<h1>esMail webview preview</h1>
<p>Accented text to check character encoding: <b>&eacute;&agrave;&uuml;&ccedil;</b> &euro; &mdash; &ldquo;quoted&rdquo;.</p>
<p><a href="https://example.com/clicked">A link</a> &mdash; clicking it should emit LinkClicked and not navigate.</p>
<table><tr><th>From</th><th>Subject</th></tr><tr><td>a@b.c</td><td>Hello</td></tr></table>
<p>Type here to check keyboard input: <input type="text" size="30" placeholder="type me"></p>
<div class="tall">Scroll down past this block to check scrolling.</div>
<h2 id="bottom">Bottom of the page</h2>
"#;

pub(crate) fn main() {
    env_logger::init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut path: Option<String> = None;
    let mut screenshot: Option<String> = None;
    let mut scroll: Option<f32> = None;
    let mut width = 700.0f32;
    let mut height = 900.0f32;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--screenshot" => {
                screenshot = args.get(i + 1).cloned();
                i += 2;
            }
            "--scroll" => {
                scroll = args.get(i + 1).and_then(|s| s.parse().ok());
                i += 2;
            }
            "--width" => {
                width = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(width);
                i += 2;
            }
            "--height" => {
                height = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(height);
                i += 2;
            }
            flag if flag.starts_with("--") => {
                eprintln!("view: unknown flag {flag}");
                std::process::exit(2);
            }
            name => {
                path = Some(name.to_string());
                i += 1;
            }
        }
    }

    let Some(name) = path else {
        eprintln!("usage: cargo run -p litehtml-view-d2d --example view -- <file.html|file.eml|demo> [--screenshot out.png]");
        std::process::exit(2);
    };
    let html = match load(&name) {
        Ok(html) => html,
        Err(e) => {
            eprintln!("view: {e}");
            std::process::exit(1);
        }
    };

    let result = win32ui::run_app(
        WindowSpec::new("litehtml-view-d2d").size(dip(width), dip(height)),
        |ui| {
            let view = HtmlView::new(ui, html, || Msg::FrameReady).expect("create the view");
            ui.set_layout(column![view.fill(1)]);
            let timer = screenshot.as_ref().map(|_| ui.set_timer(100).ok()).flatten();
            if let Some(timer) = timer {
                ui.on_timer(move |id| (id == timer).then_some(Msg::Tick));
            }
            App { view, screenshot, ticks: 0, scroll, applied_scroll: false }
        },
    );
    if let Err(error) = result {
        eprintln!("view failed: {error}");
        std::process::exit(1);
    }
}

fn load(name: &str) -> std::result::Result<String, String> {
    if name == "demo" {
        return Ok(DEMO.to_string());
    }
    let bytes = std::fs::read(name).map_err(|e| format!("cannot read {name}: {e}"))?;
    if name.ends_with(".eml") {
        let parsed = mailparse::parse_mail(&bytes).map_err(|e| format!("cannot parse {name}: {e}"))?;
        return find_html(&parsed).ok_or_else(|| format!("{name} has no text/html body"));
    }
    String::from_utf8(bytes).map_err(|e| format!("{name} is not UTF-8 HTML: {e}"))
}

fn find_html(part: &mailparse::ParsedMail<'_>) -> Option<String> {
    if part.ctype.mimetype == "text/html" {
        return part.get_body().ok();
    }
    part.subparts.iter().find_map(find_html)
}

fn write_screenshot(image: &RgbaImage, path: &Path) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let file = File::create(path)?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), image.width, image.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&image.pixels)?;
    Ok(())
}

impl win32ui::App for App {
    type Msg = Msg;

    fn update(&mut self, msg: Msg, ui: &mut Ui<Msg>) {
        match msg {
            Msg::FrameReady => self.view.invalidate(),
            Msg::Tick => {
                self.ticks += 1;
                if !self.applied_scroll && self.view.is_ready() {
                    if let Some(y) = self.scroll {
                        self.view.set_scroll(y);
                    }
                    self.applied_scroll = true;
                }
                if self.screenshot.is_some() && self.applied_scroll && self.view.is_ready() {
                    let path = self.screenshot.clone().unwrap();
                    match ui.capture() {
                        Ok(image) => match write_screenshot(&image, Path::new(&path)) {
                            Ok(()) => eprintln!("view: wrote screenshot to {path}"),
                            Err(e) => eprintln!("view: screenshot failed: {e}"),
                        },
                        Err(e) => eprintln!("view: screenshot failed: {e}"),
                    }
                    ui.quit();
                } else if self.ticks > 6000 {
                    // A render that never settles must still have an exit path.
                    eprintln!("view: timed out waiting for a frame");
                    ui.quit();
                }
            }
        }
    }
}
