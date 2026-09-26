//! Renders an HTML file (or `.eml` fixture, or the built-in `demo` page) in a
//! xui-win32 window with an [`HtmlView`] filling it.
//!
//! ```text
//! cargo run -p litehtml-view-d2d --example view -- <file.html>
//! cargo run -p litehtml-view-d2d --example view -- demo --screenshot out.png
//! cargo run -p litehtml-view-d2d --example view -- --select-demo
//! ```
//!
//! `--screenshot out.png` renders once, captures via `Window::capture` and
//! exits (the capture path is guarded by a timer so a render always has an
//! exit path). `--width`/`--height` set the window size in design units.
//! `--select-demo` renders a page of several blocks (including an RTL and a CJK
//! phrase), selects everything, and captures a screenshot so the highlight can
//! be checked. Link clicks print their `href`.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use litehtml_view_d2d::{HtmlView, HtmlViewEvent};
use xui_win32::column;
use xui_win32::prelude::*;

enum Msg {
    FrameReady,
    Link(String),
    Tick,
}

struct App {
    view: HtmlView<Msg>,
    screenshot: Option<String>,
    ticks: u64,
    scroll: Option<f32>,
    applied_scroll: bool,
    select_demo: bool,
    selected: bool,
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

/// A page for `--select-demo`: several blocks, an RTL phrase and a CJK phrase,
/// all on a light background so the selection highlight stands out.
const SELECT_DEMO: &str = r#"<!doctype html>
<meta charset="utf-8">
<style>
  body { font: 18px/1.6 system-ui, sans-serif; margin: 2rem; color: #111; background: #fff; }
  p { margin: 1.2rem 0; }
</style>
<h1>Selection demo</h1>
<p>First block: the quick brown fox jumps over the lazy dog.</p>
<p>Second block: selection should highlight across every one of these blocks.</p>
<ul><li>List item one</li><li>List item two</li></ul>
<p>Third block: more prose to select, with some <b>bold</b> and <i>italic</i> text.</p>
<p dir="rtl" lang="he">שלום עולם, מה שלומך?</p>
<p lang="ja">日本語のテキストです。中文文本。</p>
"#;

pub(crate) fn main() {
    env_logger::init();

    let args: &[String] = &std::env::args().skip(1).collect::<Vec<_>>();
    let mut path: Option<String> = None;
    let mut screenshot: Option<String> = None;
    let mut scroll: Option<f32> = None;
    let mut select_demo = false;
    let mut width = 700.0f32;
    let mut height = 900.0f32;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--screenshot" => {
                screenshot = args.get(i + 1).cloned();
                i += 2;
            }
            "--select-demo" => {
                select_demo = true;
                i += 1;
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

    let html = if select_demo {
        screenshot.get_or_insert_with(|| "select-demo.png".to_string());
        SELECT_DEMO.to_string()
    } else {
        let Some(name) = path else {
            eprintln!("usage: cargo run -p litehtml-view-d2d --example view -- <file.html|file.eml|demo> [--screenshot out.png] [--select-demo]");
            std::process::exit(2);
        };
        match load(&name) {
            Ok(html) => html,
            Err(e) => {
                eprintln!("view: {e}");
                std::process::exit(1);
            }
        }
    };

    let result = xui_win32::run_app(
        WindowSpec::new("litehtml-view-d2d").size(dip(width), dip(height)),
        |ui| {
            let view = HtmlView::new(
                ui,
                html,
                || Msg::FrameReady,
                |event| match event {
                    HtmlViewEvent::LinkClicked(href) => Some(Msg::Link(href)),
                },
            )
            .expect("create the view");
            ui.set_layout(column![view.fill(1)]);
            let timer = screenshot.as_ref().map(|_| ui.set_timer(100).ok()).flatten();
            if let Some(timer) = timer {
                ui.on_timer(move |id| (id == timer).then_some(Msg::Tick));
            }
            App { view, screenshot, ticks: 0, scroll, applied_scroll: false, select_demo, selected: false }
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

/// Counts pixels that look like the translucent selection highlight over a
/// white page (a distinctly blue, non-white colour). A non-zero count is the
/// programmatic check that the highlight is visible.
fn count_highlight(image: &RgbaImage) -> usize {
    let mut n = 0;
    for px in image.pixels.chunks_exact(4) {
        let (r, g, b) = (px[0] as i32, px[1] as i32, px[2] as i32);
        if b >= 200 && b - r >= 40 && b - g >= 20 {
            n += 1;
        }
    }
    n
}

impl xui_win32::App for App {
    type Msg = Msg;

    fn update(&mut self, msg: Msg, ui: &mut Ui<Msg>) {
        match msg {
            Msg::FrameReady => self.view.invalidate(),
            Msg::Link(href) => eprintln!("view: link clicked: {href}"),
            Msg::Tick => {
                self.ticks += 1;
                if !self.applied_scroll && self.view.is_ready() {
                    if let Some(y) = self.scroll {
                        self.view.set_scroll(y);
                    }
                    self.applied_scroll = true;
                }
                if self.select_demo && self.applied_scroll && self.view.is_ready() && !self.selected {
                    self.view.select_all();
                    self.selected = true;
                    self.ticks = 0;
                }
                if self.screenshot.is_some() && self.applied_scroll && self.view.is_ready() && !self.select_demo {
                    self.finish_screenshot(ui);
                } else if self.screenshot.is_some() && self.selected && self.ticks > 1 {
                    // A couple of frames after `select_all` so the highlight is
                    // painted before we capture.
                    self.finish_screenshot(ui);
                } else if self.ticks > 6000 {
                    // A render that never settles must still have an exit path.
                    eprintln!("view: timed out waiting for a frame");
                    ui.quit();
                }
            }
        }
    }
}

impl App {
    fn finish_screenshot(&self, ui: &mut Ui<Msg>) {
        let path = self.screenshot.clone().unwrap();
        match ui.capture() {
            Ok(image) => {
                let highlight = count_highlight(&image);
                eprintln!("view: highlight pixels: {highlight}");
                match write_screenshot(&image, Path::new(&path)) {
                    Ok(()) => eprintln!("view: wrote screenshot to {path}"),
                    Err(e) => eprintln!("view: screenshot failed: {e}"),
                }
            }
            Err(e) => eprintln!("view: screenshot failed: {e}"),
        }
        ui.quit();
    }
}
