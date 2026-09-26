//! Neutral geometry and colour types shared by the display list and the
//! painter. They mirror what the Direct2D backend needs without depending on
//! egui or xui-win32, so the list itself stays backend-neutral (see `list.rs`).

/// A point in device-independent pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    /// Horizontal coordinate.
    pub x: f32,
    /// Vertical coordinate.
    pub y: f32,
}

impl Point {
    /// Creates a point.
    pub const fn new(x: f32, y: f32) -> Point {
        Point { x, y }
    }
}

/// An axis-aligned rectangle in device-independent pixels, by its edges.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    /// Left edge.
    pub left: f32,
    /// Top edge.
    pub top: f32,
    /// Right edge.
    pub right: f32,
    /// Bottom edge.
    pub bottom: f32,
}

impl Rect {
    /// Creates a rectangle from its edges.
    pub const fn new(left: f32, top: f32, right: f32, bottom: f32) -> Rect {
        Rect { left, top, right, bottom }
    }

    /// Creates a rectangle from its top-left corner and its size.
    pub const fn from_min_size(x: f32, y: f32, width: f32, height: f32) -> Rect {
        Rect { left: x, top: y, right: x + width, bottom: y + height }
    }

    /// The width.
    pub fn width(&self) -> f32 {
        self.right - self.left
    }

    /// The height.
    pub fn height(&self) -> f32 {
        self.bottom - self.top
    }

    /// Whether the point is inside the rectangle (edges on the right/bottom
    /// do not count).
    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.left && p.x < self.right && p.y >= self.top && p.y < self.bottom
    }

    /// The centre of the rectangle.
    pub fn center(&self) -> Point {
        Point::new((self.left + self.right) / 2.0, (self.top + self.bottom) / 2.0)
    }

    /// The smallest rectangle covering both this rectangle and `other`.
    pub fn union(&self, other: Rect) -> Rect {
        Rect {
            left: self.left.min(other.left),
            top: self.top.min(other.top),
            right: self.right.max(other.right),
            bottom: self.bottom.max(other.bottom),
        }
    }

    /// The rectangle moved by `(x, y)`.
    pub fn translate(&self, x: f32, y: f32) -> Rect {
        Rect {
            left: self.left + x,
            top: self.top + y,
            right: self.right + x,
            bottom: self.bottom + y,
        }
    }

    /// The rectangle grown by `d` on every side.
    pub fn expand(&self, d: f32) -> Rect {
        Rect {
            left: self.left - d,
            top: self.top - d,
            right: self.right + d,
            bottom: self.bottom + d,
        }
    }

    /// Whether this rectangle overlaps `other` (edges touching do not count).
    pub fn intersects(&self, other: Rect) -> bool {
        self.left < other.right && self.right > other.left && self.top < other.bottom && self.bottom > other.top
    }

    /// The intersection of two overlapping rectangles, or `None`.
    pub fn intersect(&self, other: Rect) -> Option<Rect> {
        let rect = Rect {
            left: self.left.max(other.left),
            top: self.top.max(other.top),
            right: self.right.min(other.right),
            bottom: self.bottom.min(other.bottom),
        };
        (rect.width() > 0.0 && rect.height() > 0.0).then_some(rect)
    }
}

/// An elliptical corner radius: an x and a y half-axis, as CSS `border-radius`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Radius {
    /// The horizontal half-axis.
    pub x: f32,
    /// The vertical half-axis.
    pub y: f32,
}

impl Radius {
    /// A radius with equal x and y.
    pub const fn uniform(radius: f32) -> Radius {
        Radius { x: radius, y: radius }
    }

    /// A radius with separate x and y half-axes.
    pub const fn new(x: f32, y: f32) -> Radius {
        Radius { x, y }
    }
}

/// An RGBA colour with a straight (not premultiplied) alpha channel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rgba {
    /// Red channel.
    pub r: u8,
    /// Green channel.
    pub g: u8,
    /// Blue channel.
    pub b: u8,
    /// Alpha channel: `0` transparent, `255` opaque.
    pub a: u8,
}

impl Rgba {
    /// An opaque colour from its channels.
    pub const fn rgb(r: u8, g: u8, b: u8) -> Rgba {
        Rgba { r, g, b, a: 0xFF }
    }

    /// A colour from its channels and alpha.
    pub const fn with_alpha(r: u8, g: u8, b: u8, a: u8) -> Rgba {
        Rgba { r, g, b, a }
    }

    /// Opaque white.
    pub const WHITE: Rgba = Rgba::rgb(0xFF, 0xFF, 0xFF);
    /// Opaque black.
    pub const BLACK: Rgba = Rgba::rgb(0x00, 0x00, 0x00);
    /// Fully transparent black.
    pub const TRANSPARENT: Rgba = Rgba::with_alpha(0x00, 0x00, 0x00, 0x00);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rectangles_intersect_and_intersect() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(5.0, 5.0, 15.0, 15.0);
        assert!(a.intersects(b));
        assert_eq!(a.intersect(b), Some(Rect::new(5.0, 5.0, 10.0, 10.0)));
        // Touching edges do not intersect.
        assert!(!a.intersects(Rect::new(10.0, 0.0, 20.0, 10.0)));
        assert_eq!(a.intersect(Rect::new(10.0, 0.0, 20.0, 10.0)), None);
    }

    #[test]
    fn translate_and_expand_are_pure_geometry() {
        let rect = Rect::from_min_size(1.0, 2.0, 4.0, 6.0);
        assert_eq!(rect.translate(-1.0, 3.0), Rect::new(0.0, 5.0, 4.0, 11.0));
        assert_eq!(rect.expand(2.0), Rect::new(-1.0, 0.0, 7.0, 10.0));
    }
}
