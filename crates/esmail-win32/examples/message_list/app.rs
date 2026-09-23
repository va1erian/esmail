//! A win32ui window showing the [`MessageList`] fed by generated mock headers,
//! with a fake mailbox tree on the left.
//!
//! ```text
//! cargo run -p esmail-win32 --example message_list -- --rows 100000 --theme light
//! cargo run -p esmail-win32 --example message_list -- --screenshot out.png --theme dark --select 3..8
//! ```
//!
//! `--rows N` sets the row count, `--theme light|dark` the palette,
//! `--screenshot out.png` captures once and exits, `--scroll N` scrolls so row
//! `N` is visible, and `--select A..B` selects that inclusive range before the
//! capture. The capture path is guarded by a timer, so a screenshot run always
//! has an exit path.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::Arc;

use esmail::imap::MailHeader;
use esmail::view_model::RowModel;
use esmail_win32::MessageList;
use win32ui::prelude::*;
use win32ui::{column, split_row};

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
    screenshot: Option<String>,
    scroll: Option<usize>,
    select: Option<(usize, usize)>,
    hold: Option<u64>,
    applied: bool,
    ticks: u64,
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

            let timer = screenshot.as_ref().and_then(|_| ui.set_timer(50).ok());
            if let Some(timer) = timer {
                ui.on_timer(move |id| (id == timer).then_some(Msg::Tick));
            }
            App {
                list,
                screenshot,
                scroll,
                select,
                hold,
                applied: false,
                ticks: 0,
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
            Msg::Selected(rows) => eprintln!("message_list: selected {rows:?}"),
            Msg::Open(row) => eprintln!("message_list: open row {row}"),
            Msg::Delete(rows) => eprintln!("message_list: delete {rows:?}"),
            Msg::ToggleFlag(row) => eprintln!("message_list: toggle flag on row {row}"),
            Msg::Context(row, at) => {
                eprintln!("message_list: context on row {row} at {at:?}");
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
            self.applied = true;
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
