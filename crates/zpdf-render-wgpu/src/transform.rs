//! Coordinate transform and GPU vertex/uniform types.
//!
//! The page->NDC mapping is the correctness keystone. The CPU oracle maps a PDF
//! page-unit point `(x, y)` (origin bottom-left, +Y up) to a device pixel
//! (origin top-left, +Y down), relative to the page rect (CropBox / nonzero
//! MediaBox origins shift it), as:
//!
//! ```text
//! px = (x - rect.x0) * scale
//! py = (rect.y1 - y) * scale           scale = dpi / 72
//! ```
//!
//! Fills/strokes are tessellated in **device-pixel space** (baking `scale` +
//! flip_y at build time), matching the CPU's `build_skia_path`. The vertex shader
//! then only converts device pixels to clip space.

use zpdf_core::Point;
use zpdf_display_list::Color;

/// Uniform consumed by `vs_pixel` for the pixel->NDC step. Exactly 16 bytes.
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
pub struct PageUniform {
    pub w_px: f32,
    pub h_px: f32,
    pub scale: f32,
    pub page_height: f32,
}

/// Vertex for solid fills/strokes/Type3. Position in device pixels; color is
/// premultiplied and integer-quantized (see [`quantize_premul`]).
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
pub struct SolidVertex {
    pub pos: [f32; 2],
    pub color: [f32; 4],
}

/// Vertex for image quads. Position in device pixels, UV in [0,1], and `color`
/// carries `[1, 1, 1, draw.alpha]` (the per-draw opacity).
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
pub struct TexturedVertex {
    pub pos: [f32; 2],
    pub uv: [f32; 2],
    pub color: [f32; 4],
}

/// Host-side page-units -> device-pixel mapping, identical to the CPU oracle's
/// `to_pixel_x` / `flip_y`. The CTM is already baked into display-list path
/// coordinates by the interpreter, so this is origin shift + scale + Y flip.
#[derive(Copy, Clone)]
pub struct PageMap {
    pub scale: f32,
    /// Page rect bounds (f32, matching the CPU's stored fields): the left edge
    /// and top edge of the rendered page rect in page units. The fixed Y-flip
    /// maps page y to `(y1 - y) * scale`, so the bottom edge is not needed.
    pub x0: f32,
    pub y1: f32,
}

impl PageMap {
    pub fn new(rect: zpdf_core::Rect, scale: f32) -> Self {
        Self {
            scale,
            x0: rect.x0 as f32,
            y1: rect.y1 as f32,
        }
    }

    /// Map a page-space point to a device-pixel lyon point. Casts the f64 page
    /// coordinate to f32 *before* the origin shift + scale, exactly as
    /// `build_skia_path` does.
    pub fn pt(&self, p: Point) -> lyon::math::Point {
        let x = (p.x as f32 - self.x0) * self.scale;
        let y = (self.y1 - p.y as f32) * self.scale;
        lyon::math::point(x, y)
    }
}

/// Quantize a straight color then premultiply, reproducing tiny-skia's pipeline:
/// the CPU passes straight 8-bit `(c*255) as u8` to tiny-skia, which premultiplies
/// internally with integer fixed-point `(channel*alpha + 127) / 255`. Matching that
/// here (rather than f32 `c*a`) avoids LSB drift on translucent draws. For opaque
/// draws this reduces to the straight quantized color.
pub fn quantize_premul(c: &Color, alpha: f32) -> [f32; 4] {
    let cr = (c.r * 255.0) as u8;
    let cg = (c.g * 255.0) as u8;
    let cb = (c.b * 255.0) as u8;
    let ca = (c.a * alpha * 255.0) as u8;
    let premul = |ch: u8| -> f32 {
        let p = ((ch as u32 * ca as u32 + 127) / 255) as u8;
        p as f32 / 255.0
    };
    [premul(cr), premul(cg), premul(cb), ca as f32 / 255.0]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_premul_equals_straight_quantized() {
        // ca == 255 -> premultiply is a no-op; channels are the straight 8-bit values.
        let q = quantize_premul(&Color::rgba(1.0, 0.5, 0.0, 1.0), 1.0);
        assert_eq!(q[3], 1.0);
        assert_eq!(q[0], 1.0); // (1.0*255) as u8 = 255
        assert_eq!(q[1], 127.0 / 255.0); // (0.5*255) as u8 = 127
        assert_eq!(q[2], 0.0);
    }

    #[test]
    fn translucent_uses_integer_premultiply() {
        // alpha 0.5 -> ca = 127; premul(255) = (255*127 + 127)/255 = 127.
        let q = quantize_premul(&Color::rgba(1.0, 1.0, 1.0, 1.0), 0.5);
        assert_eq!(q[3], 127.0 / 255.0);
        assert_eq!(q[0], 127.0 / 255.0);
    }

    #[test]
    fn page_map_flips_y_like_cpu() {
        let m = PageMap {
            scale: 2.0,
            x0: 0.0,
            y1: 100.0,
        };
        // (10, 80) page -> (20, (100-80)*2 = 40) device pixels.
        let p = m.pt(Point::new(10.0, 80.0));
        assert_eq!(p.x, 20.0);
        assert_eq!(p.y, 40.0);
    }

    #[test]
    fn page_map_honors_nonzero_origin() {
        // CropBox-style rect (100,50)-(120,70).
        let m = PageMap::new(zpdf_core::Rect::new(100.0, 50.0, 120.0, 70.0), 2.0);
        // (105, 55) page -> ((105-100)*2, (70-55)*2) = (10, 30) device pixels.
        let p = m.pt(Point::new(105.0, 55.0));
        assert_eq!(p.x, 10.0);
        assert_eq!(p.y, 30.0);
    }
}
