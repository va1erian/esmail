//! `--screenshot`: capture the window once it has settled, then exit.

use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;

use win32ui::{Ui, WindowHandle};

use super::Msg;
use super::compose::ComposeMsg;

/// Ticks (50 ms each) to keep waiting after the window looks ready, so the
/// last paint lands before the capture.
const SETTLE_TICKS: u32 = 6;
/// Ticks after the widgets were told to repaint, so their paint has reached
/// the window's composed surface (which is what `PrintWindow` copies).
const REPAINT_TICKS: u32 = 4;
/// Ticks after which a window that never gets ready is reported instead of
/// hanging (40 s).
const GIVE_UP_TICKS: u32 = 800;

/// A pending capture.
pub struct Capture {
    path: PathBuf,
    settled: u32,
    waited: u32,
}

/// What the caller does after a tick.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    Wait,
    /// Repaint the widgets; the capture follows a few ticks later.
    Repaint,
    Capture,
}

impl Capture {
    pub fn new(path: PathBuf) -> Capture {
        Capture { path, settled: 0, waited: 0 }
    }

    /// Called every tick with whether the content is on screen.
    pub fn step(&mut self, ui: &Ui<Msg>, content_ready: bool) -> Step {
        // The window must be active for its Direct2D children to be composed.
        if !ui.is_foreground() {
            ui.set_foreground();
        }
        self.waited += 1;
        self.settled = if content_ready { self.settled + 1 } else { 0 };
        match self.settled {
            SETTLE_TICKS => Step::Repaint,
            n if n >= SETTLE_TICKS + REPAINT_TICKS => Step::Capture,
            _ if self.waited >= GIVE_UP_TICKS => Step::Capture,
            _ => Step::Wait,
        }
    }

    /// Where a compose window is captured: next to the main window's file, with
    /// `-compose` before the extension.
    fn compose_path(&self) -> PathBuf {
        let stem = self.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        self.path.with_file_name(format!("{stem}-compose.png"))
    }

    /// Writes the PNG and quits (with a failure code if it could not).
    pub fn finish(&self, ui: &mut Ui<Msg>, compose: Option<&WindowHandle<ComposeMsg>>) {
        let result = ui.capture().map_err(|e| e.to_string()).and_then(|image| write_png(&image, &self.path).map_err(|e| e.to_string()));
        let result = result.and_then(|()| match compose {
            Some(window) => window.capture().map_err(|e| e.to_string()).and_then(|image| write_png(&image, &self.compose_path()).map_err(|e| e.to_string())),
            None => Ok(()),
        });
        match result {
            Ok(()) if self.settled >= SETTLE_TICKS + REPAINT_TICKS => {
                eprintln!("esmail-win32: wrote {}", self.path.display());
                ui.quit();
            }
            Ok(()) => {
                eprintln!("esmail-win32: wrote {} but the window never finished loading", self.path.display());
                ui.quit_with(1);
            }
            Err(error) => {
                eprintln!("esmail-win32: screenshot failed: {error}");
                ui.quit_with(1);
            }
        }
    }
}

fn write_png(image: &win32ui::RgbaImage, path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut encoder = png::Encoder::new(BufWriter::new(File::create(path)?), image.width, image.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(&image.pixels)?;
    Ok(())
}
