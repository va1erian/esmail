//! The application artwork, embedded in the binary.
//!
//! `assets/` holds the sources (`icon.svg`, `tray-icon.svg`) and the rasters
//! made from them. The Windows executable's own icon (`assets/icon.ico`) is
//! attached by `build.rs`; this module supplies what the running program
//! draws itself: the window icon and the tray icon.
//!
//! The tray glyph is a single dark colour on a transparent background, so on
//! a dark taskbar it would be invisible. [`tray_icon`] therefore repaints it
//! white when asked to -- keeping only the alpha channel, which is all the
//! artwork's shape lives in.

/// Tray artwork. 32 px: the shell scales it to the 16 or 24 px a 100% or 150%
/// display wants and shows it as is at 200%.
const TRAY_PNG: &[u8] = include_bytes!("../assets/tray-32.png");

/// The window/taskbar icon: the full-colour application icon.
pub const WINDOW_ICON_PNG: &[u8] = include_bytes!("../assets/icon-128.png");

/// Straight-alpha RGBA pixels, `width * height * 4` bytes.
pub struct Rgba {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl From<Rgba> for egui::IconData {
    fn from(icon: Rgba) -> Self {
        Self { rgba: icon.pixels, width: icon.width, height: icon.height }
    }
}

fn decode(png: &[u8]) -> Option<Rgba> {
    let image = image::load_from_memory_with_format(png, image::ImageFormat::Png).ok()?.into_rgba8();
    let (width, height) = image.dimensions();
    Some(Rgba { pixels: image.into_raw(), width, height })
}

/// The application icon, for the window.
pub fn window_icon() -> Option<Rgba> {
    decode(WINDOW_ICON_PNG)
}

/// The tray icon. `light_glyph` selects a white glyph, for a dark taskbar.
pub fn tray_icon(light_glyph: bool) -> Option<Rgba> {
    let mut icon = decode(TRAY_PNG)?;
    if light_glyph {
        for pixel in icon.pixels.chunks_exact_mut(4) {
            pixel[0] = 0xff;
            pixel[1] = 0xff;
            pixel[2] = 0xff;
        }
    }
    Some(icon)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_icon_decodes_square() {
        let icon = window_icon().expect("embedded PNG decodes");
        assert_eq!(icon.width, icon.height);
        assert_eq!(icon.pixels.len(), (icon.width * icon.height * 4) as usize);
    }

    #[test]
    fn light_glyph_keeps_the_shape_and_whitens_the_colour() {
        let dark = tray_icon(false).unwrap();
        let light = tray_icon(true).unwrap();
        assert!(dark.pixels.chunks_exact(4).any(|p| p[3] > 0), "glyph is not empty");
        for (d, l) in dark.pixels.chunks_exact(4).zip(light.pixels.chunks_exact(4)) {
            assert_eq!(d[3], l[3], "alpha (the shape) is unchanged");
            assert_eq!(&l[..3], &[0xff, 0xff, 0xff]);
        }
    }
}
