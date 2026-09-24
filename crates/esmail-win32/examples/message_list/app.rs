//! A win32ui window showing the [`MessageList`] fed by generated mock headers,
//! with a fake mailbox tree on the left.
//!
//! ```text
//! cargo run -p esmail-win32 --example message_list -- --rows 100000 --theme light
//! cargo run -p esmail-win32 --example message_list -- --screenshot out.png --theme dark --select 3..8
//! cargo run -p esmail-win32 --example message_list --release -- --bench-scroll
//! cargo run -p esmail-win32 --example message_list -- --trace-scroll --verbose
//! ```
//!
//! `--rows N` sets the row count, `--theme light|dark` the palette,
//! `--screenshot out.png` captures once and exits, `--scroll N` scrolls so row
//! `N` is visible, and `--select A..B` selects that inclusive range before the
//! capture. `--bench-scroll` drives the widget with synthetic `WM_VSCROLL` /
//! `WM_MOUSEWHEEL` input and prints latency percentiles before exiting;
//! `--trace-scroll` prints per-event timestamps while a human scrolls for real.
//! `--verbose` turns on the per-event diagnostics that release builds keep
//! quiet. Every mode has a timer-guarded exit path.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::Arc;

use esmail::imap::MailHeader;
use esmail::view_model::RowModel;
use esmail_win32::MessageList;
use win32ui::prelude::*;
use win32ui::{column, split_row};

use crate::bench;

enum Msg {
    Selected(Vec<usize>),
    Open(usize),
    Delete(Vec<usize>),
    ToggleFlag(usize),
    Context(usize, Point),
    Tick,
}

struct App {
    list: MessageList<Msg>,
    rows: usize,
    screenshot: Option<String>,
    scroll: Option<usize>,
    select: Option<(usize, usize)>,
    hold: Option<u64>,
    bench: Option<bench::Bench>,
    trace: bool,
    verbose: bool,
    applied: bool,
    ticks: u64,
    /// The paint sequence number last reported by `--trace-scroll`.
    last_traced_paint: u64,
    /// The scroll sequence number last reported by `--trace-scroll`.
    last_traced_scroll: u64,
    /// App start, for absolute `--trace-scroll` timestamps.
    started: std::time::Instant,
}

/// A `TreeSource` of fixed fake mailbox names.
struct Mailboxes;

impl TreeSource for Mailboxes {
    fn children(&self, parent: Option<i64>) -> Vec<TreeEntry> {
        if parent.is_some() {
            return Vec::new();
        }
        ["Inbox", "Starred", "Sent", "Drafts", "Archive", "Trash", "Spam"]
            .iter()
            .enumerate()
            .map(|(i, name)| TreeEntry::leaf(*name, i as i64))
            .collect()
    }
}

pub(crate) fn main() {
    env_logger::init();

    let mut rows = 100_000usize;
    let mut theme = Theme::light();
    let mut screenshot: Option<String> = None;
    let mut scroll: Option<usize> = None;
    let mut select: Option<(usize, usize)> = None;
    let mut hold: Option<u64> = None;
    let mut bench = false;
    let mut trace = false;
    let mut verbose = false;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--rows" => {
                rows = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(rows);
                i += 1;
            }
            "--theme" => {
                if args.get(i + 1).map(String::as_str) == Some("dark") {
                    theme = Theme::dark();
                }
                i += 1;
            }
            "--screenshot" => {
                screenshot = args.get(i + 1).cloned();
                i += 1;
            }
            "--scroll" => {
                scroll = args.get(i + 1).and_then(|s| s.parse().ok());
                i += 1;
            }
            "--select" => {
                select = args.get(i + 1).and_then(|s| parse_range(s));
                i += 1;
            }
            "--hold" => {
                hold = args.get(i + 1).and_then(|s| s.parse().ok());
                i += 1;
            }
            "--bench-scroll" => bench = true,
            "--trace-scroll" => trace = true,
            "--verbose" => verbose = true,
            other => {
                eprintln!("message_list: unknown flag {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    let result = win32ui::run_app(
        WindowSpec::new("esMail message list")
            .size(dip(1000.0), dip(640.0))
            .theme(theme),
        |ui| {
            let tree = TreeView::new(ui, Rect::default(), Box::new(Mailboxes))
                .expect("mailbox tree")
                .on_select(|_| None);
            let list = MessageList::new(ui)
                .expect("message list")
                .on_select(|rows| Some(Msg::Selected(rows.to_vec())))
                .on_open(|row| Some(Msg::Open(row)))
                .on_delete(|rows| Some(Msg::Delete(rows.to_vec())))
                .on_flag(|row| Some(Msg::ToggleFlag(row)))
                .on_context(|row, at| Some(Msg::Context(row, at)));
            let started = std::time::Instant::now();
            list.set_rows(make_rows(rows));
            let set_rows_micros = started.elapsed().as_secs_f64() * 1_000_000.0;
            eprintln!("message_list: set_rows({rows}) took {set_rows_micros:.1} us");

            ui.set_layout(column![split_row![tree, list.fill(1)]
                .position(dip(200.0))
                .min(dip(120.0), dip(300.0))]);

            // A timer drives the screenshot capture, the bench and the trace
            // poll; a plain interactive run has none.
            let millis = if bench || trace {
                5
            } else if screenshot.is_some() {
                50
            } else {
                0
            };
            let timer = (millis > 0).then(|| ui.set_timer(millis).ok()).flatten();
            if let Some(timer) = timer {
                ui.on_timer(move |id| (id == timer).then_some(Msg::Tick));
            }
            App {
                list,
                rows,
                screenshot,
                scroll,
                select,
                hold,
                bench: bench.then(bench::Bench::new),
                trace,
                verbose,
                applied: false,
                ticks: 0,
                last_traced_paint: 0,
                last_traced_scroll: 0,
                started: std::time::Instant::now(),
            }
        },
    );
    if let Err(error) = result {
        eprintln!("message_list failed: {error}");
        std::process::exit(1);
    }
}

impl win32ui::App for App {
    type Msg = Msg;

    fn update(&mut self, msg: Msg, ui: &mut Ui<Msg>) {
        match msg {
            Msg::Selected(rows) => {
                if self.verbose {
                    eprintln!("message_list: selected {rows:?}");
                }
            }
            Msg::Open(row) => {
                if self.verbose {
                    eprintln!("message_list: open row {row}");
                }
            }
            Msg::Delete(rows) => {
                if self.verbose {
                    eprintln!("message_list: delete {rows:?}");
                }
            }
            Msg::ToggleFlag(row) => {
                if self.verbose {
                    eprintln!("message_list: toggle flag on row {row}");
                }
            }
            Msg::Context(row, at) => {
                if self.verbose {
                    eprintln!("message_list: context on row {row} at {at:?}");
                }
            }
            Msg::Tick => self.tick(ui),
        }
    }
}

impl App {
    fn tick(&mut self, ui: &mut Ui<Msg>) {
        self.ticks += 1;
        if !self.applied {
            if let Some(row) = self.scroll {
                self.list.ensure_visible(row);
            }
            if let Some((a, b)) = self.select {
                let rows: Vec<usize> = (a..=b).collect();
                self.list.set_selection(&rows);
                // Focus the list so the selected rows show the focused
                // selection fill rather than the unfocused grey.
                self.list.focus();
            }
            // The bench starts part-way down so every input kind has room to
            // scroll in both directions.
            if self.bench.is_some() {
                self.list.ensure_visible(self.rows / 2);
            }
            self.applied = true;
        }

        if self.bench.is_some() {
            self.drive_bench(ui);
            return;
        }
        if self.trace {
            self.trace_scroll();
        }

        if self.screenshot.is_some() && self.applied && self.ticks > 2 {
            self.finish_screenshot(ui);
        } else if let Some(hold) = self.hold {
            if self.ticks * 50 >= hold {
                ui.quit();
            }
        } else if self.ticks > 12_000 {
            // A run that never settles must still have an exit path.
            eprintln!("message_list: timed out waiting to capture");
            ui.quit();
        }
    }

    /// Advances the bench one step per timer tick: waits for the paint that
    /// follows the previous input, records its latency, then sends the next.
    fn drive_bench(&mut self, ui: &mut Ui<Msg>) {
        let hwnd = self.list.control().hwnd();
        let timing = self.list.timing();
        let Some(bench) = self.bench.as_mut() else { return };

        if let Some((pending, kind)) = bench.pending {
            if timing.paint_seq >= bench.awaiting {
                let begin = timing.paint_begin.unwrap_or(pending);
                bench.samples.push(bench::Sample {
                    kind,
                    latency_ns: begin.saturating_duration_since(pending).as_nanos(),
                    layout_us: timing.phases.layout,
                    draw_us: timing.phases.draw,
                    paint_us: timing.paint_micros,
                    rows: timing.last_rows,
                });
                bench.paints += 1;
                bench.pending = None;
            } else if pending.elapsed() > std::time::Duration::from_millis(2000) {
                // A scroll that does not repaint within two seconds is the
                // bug this bench exists to catch; report it and exit rather
                // than hang forever.
                eprintln!(
                    "bench-scroll: no paint within 2s of input {} ({}) — the scroll host moved the offset without repainting",
                    bench.sent,
                    kind.name(),
                );
                ui.quit();
                return;
            } else {
                // The paint for the last input has not run yet; wait.
                return;
            }
        }

        if bench.sent >= bench::INPUTS {
            bench::report(&bench.samples, bench.paints, bench.started);
            ui.quit();
            return;
        }

        let seq_before = self.list.timing().paint_seq;
        bench.kind.send(hwnd);
        bench.pending = Some((std::time::Instant::now(), bench.kind));
        bench.awaiting = seq_before + 1;
        bench.sent += 1;
        bench.step();
    }

    /// Prints one line per scroll/paint event so a human can watch the
    /// timestamps while scrolling with a real mouse.
    fn trace_scroll(&mut self) {
        let timing = self.list.timing();
        let since = |t: std::time::Instant| (t - self.started).as_secs_f64() * 1_000.0;
        if timing.scroll_seq != self.last_traced_scroll {
            self.last_traced_scroll = timing.scroll_seq;
            if let Some(at) = timing.scroll_at {
                eprintln!(
                    "trace-scroll: scroll #{:<4} at {:>9.3} ms",
                    timing.scroll_seq,
                    since(at),
                );
            }
        }
        if timing.paint_seq != self.last_traced_paint {
            self.last_traced_paint = timing.paint_seq;
            if let Some(begin) = timing.paint_begin {
                eprintln!(
                    "trace-scroll: paint  #{:<4} at {:>9.3} ms (paint {:.1} us, layout {:.1} us, draw {:.1} us)",
                    timing.paint_seq,
                    since(begin),
                    timing.paint_micros,
                    timing.phases.layout,
                    timing.phases.draw,
                );
            }
        }
    }

    fn finish_screenshot(&self, ui: &mut Ui<Msg>) {
        let path = self.screenshot.clone().unwrap();
        match ui.capture() {
            Ok(image) => {
                let theme = ui.theme();
                let accent = count_color(&image, theme.accent);
                let selection = count_color(&image, theme.selection);
                let unfocused = count_color(&image, theme.selection_unfocused);
                eprintln!(
                    "message_list: accent {accent}, selection {selection}, selection_unfocused {unfocused}, paint {:.1} us",
                    self.list.last_paint_micros()
                );
                match write_png(&image, Path::new(&path)) {
                    Ok(()) => eprintln!("message_list: wrote screenshot to {path}"),
                    Err(e) => eprintln!("message_list: screenshot failed: {e}"),
                }
            }
            Err(e) => eprintln!("message_list: screenshot failed: {e}"),
        }
        ui.quit();
    }
}

/// Counts pixels equal to `color` (programmatic check: the unread accent bar and
/// the selected-row fill both come straight from theme tokens).
fn count_color(image: &RgbaImage, color: Color) -> usize {
    image
        .pixels
        .chunks_exact(4)
        .filter(|px| px[0] == color.r && px[1] == color.g && px[2] == color.b)
        .count()
}

fn write_png(image: &RgbaImage, path: &Path) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let file = File::create(path)?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), image.width, image.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&image.pixels)?;
    Ok(())
}

/// Parses `A..B` into an inclusive range.
fn parse_range(s: &str) -> Option<(usize, usize)> {
    let (a, b) = s.split_once("..")?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

/// Deterministic mock headers: a mix of unread/read, starred, long subjects,
/// emoji, CJK, RTL, missing subject and missing date.
fn make_rows(n: usize) -> Arc<[RowModel]> {
    const SAMPLES: &[(&str, &str)] = &[
        ("Jane Doe", "Lunch on Thursday at the new place"),
        ("René Martin", "Réception d'un virement — 50,25 €"),
        ("Acme Corp", "Your invoice #1234 is ready to view"),
        ("Newsletter", "The weekly digest: everything you missed this week"),
        ("田中 太郎", "会議の資料を送ります。確認お願いします。"),
        ("משה כהן", "חשבונית עבור חודש ספטמבר"),
        ("Alice", "🎉 You're invited to the launch party 🎉"),
        ("Bob", "Re: Re: Re: A very long subject line that keeps on going and going and going well past the width of a normal message list row"),
        ("", "A subject with no sender name at all"),
        ("Support", "Can you take a look at the attached screenshot please?"),
        ("Dev Team", "build: update the DirectWrite rendering to the latest"),
    ];
    (0..n)
        .map(|i| {
            let seen = i % 3 != 0;
            let flagged = i % 7 == 0;
            let (from, subject) = SAMPLES[i % SAMPLES.len()];
            let date = if i % 11 == 0 {
                String::new()
            } else {
                format!("Mon, {} Sep 2025 10:36:43 +0200", 1 + i % 28)
            };
            let mut flags = Vec::new();
            if seen {
                flags.push("\\Seen".to_string());
            }
            if flagged {
                flags.push("\\Flagged".to_string());
            }
            let header = MailHeader {
                uid: i as u32 + 1,
                subject: subject.to_string(),
                from: from.to_string(),
                to: String::new(),
                date,
                message_id: String::new(),
                flags,
            };
            RowModel::from_header(&header)
        })
        .collect()
}
