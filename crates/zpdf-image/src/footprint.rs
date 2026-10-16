//! Device-footprint minification of decoded images.
//!
//! The CPU backend box-filters an image to its device footprint before
//! sampling it whenever an axis is minified below 0.5x. The same rule and
//! filter can run earlier, while the page is interpreted for that backend, so
//! the image cache never retains the full-resolution source. Both callers use
//! these functions; a copy in either place would let the two drift apart.

use zpdf_core::Matrix;

use crate::DecodedImage;

/// The size the CPU backend box-filters an image of `width`×`height` samples to
/// before drawing it under `transform` (unit square → page space) at `scale`
/// device pixels per point. `None` when the backend samples the source
/// directly: no axis falls below 0.5x, or the target is the source size.
///
/// The arithmetic is f32 throughout, matching the backend bit for bit.
pub fn box_filter_size(
    transform: &Matrix,
    scale: f32,
    width: u32,
    height: u32,
) -> Option<(u32, u32)> {
    if width == 0 || height == 0 {
        return None;
    }
    let (a, b, c, d) = (
        transform.a as f32,
        transform.b as f32,
        transform.c as f32,
        transform.d as f32,
    );
    // Device pixels spanned by each image axis: the column lengths of the
    // scaled transform.
    let dev_w = (a * a + b * b).sqrt() * scale;
    let dev_h = (c * c + d * d).sqrt() * scale;
    let fx = dev_w / width as f32;
    let fy = dev_h / height as f32;
    // Below ~0.5x per axis, bilinear sampling starts skipping source pixels
    // entirely (thin strokes in downscaled scans break up).
    if !(fx < 0.5 || fy < 0.5) {
        return None;
    }
    let target_width = ((width as f32 * fx.min(1.0)).ceil() as u32).clamp(1, width);
    let target_height = ((height as f32 * fy.min(1.0)).ceil() as u32).clamp(1, height);
    (target_width < width || target_height < height).then_some((target_width, target_height))
}

/// Box-filter `image` to `width`×`height`, keeping its alpha, stencil and
/// premultiplication flags.
pub fn box_downscale(image: &DecodedImage, width: u32, height: u32) -> DecodedImage {
    DecodedImage {
        width,
        height,
        data: box_downscale_rgba(&image.data, image.width, image.height, width, height),
        has_alpha: image.has_alpha,
        is_image_mask: image.is_image_mask,
        premultiplied: image.premultiplied,
    }
}

/// Box-filter (area-average) downscale of a tight RGBA8 buffer. Each target
/// pixel averages its covering source block, so no source pixel is dropped —
/// unlike point/bilinear sampling at strong minification. Channels are averaged
/// independently, which is correct for the premultiplied data tiny-skia consumes.
pub fn box_downscale_rgba(data: &[u8], sw: u32, sh: u32, tw: u32, th: u32) -> Vec<u8> {
    debug_assert!(tw >= 1 && th >= 1 && tw <= sw && th <= sh);
    let mut out = vec![0u8; tw as usize * th as usize * 4];
    let (sw64, sh64, tw64, th64) = (sw as u64, sh as u64, tw as u64, th as u64);
    for ty in 0..th as u64 {
        let y0 = (ty * sh64 / th64) as u32;
        let y1 = (((ty + 1) * sh64).div_ceil(th64) as u32).clamp(y0 + 1, sh);
        for tx in 0..tw as u64 {
            let x0 = (tx * sw64 / tw64) as u32;
            let x1 = (((tx + 1) * sw64).div_ceil(tw64) as u32).clamp(x0 + 1, sw);
            let mut acc = [0u64; 4];
            for sy in y0..y1 {
                let row = (sy as usize * sw as usize + x0 as usize) * 4;
                for sx in 0..(x1 - x0) as usize {
                    let px = row + sx * 4;
                    acc[0] += data[px] as u64;
                    acc[1] += data[px + 1] as u64;
                    acc[2] += data[px + 2] as u64;
                    acc[3] += data[px + 3] as u64;
                }
            }
            let n = ((x1 - x0) as u64) * ((y1 - y0) as u64);
            let o = (ty as usize * tw as usize + tx as usize) * 4;
            for ch in 0..4 {
                out[o + ch] = ((acc[ch] + n / 2) / n) as u8;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_size_follows_the_half_scale_threshold() {
        // A unit square placed at 100x100 points and rendered at scale 2 spans
        // 200 device pixels per axis.
        let placed = Matrix::new(100.0, 0.0, 0.0, 100.0, 0.0, 0.0);
        assert_eq!(box_filter_size(&placed, 2.0, 1000, 1000), Some((200, 200)));
        // 0.8x is sampled directly.
        assert_eq!(box_filter_size(&placed, 2.0, 250, 250), None);
        // Only the minified axis shrinks; the other keeps its samples.
        assert_eq!(box_filter_size(&placed, 2.0, 1000, 150), Some((200, 150)));
        // Translation and rotation do not change the footprint.
        let rotated = Matrix::new(0.0, 100.0, -100.0, 0.0, 37.0, -5.0);
        assert_eq!(box_filter_size(&rotated, 2.0, 1000, 1000), Some((200, 200)));
        // Degenerate inputs never produce a target.
        assert_eq!(box_filter_size(&placed, 2.0, 0, 10), None);
        let nan = Matrix::new(f64::NAN, 0.0, 0.0, f64::NAN, 0.0, 0.0);
        assert_eq!(box_filter_size(&nan, 2.0, 1000, 1000), None);
        // A sub-pixel footprint still keeps one sample.
        let tiny = Matrix::new(0.01, 0.0, 0.0, 0.01, 0.0, 0.0);
        assert_eq!(box_filter_size(&tiny, 1.0, 1000, 1000), Some((1, 1)));
    }

    #[test]
    fn box_downscale_averages_blocks_and_keeps_flags() {
        // 4x2 → 2x1: each output pixel averages a 2x2 block.
        let image = DecodedImage {
            width: 4,
            height: 2,
            data: vec![
                0, 0, 0, 255, 255, 255, 255, 255, 100, 100, 100, 255, 200, 200, 200, 255, //
                0, 0, 0, 255, 255, 255, 255, 255, 100, 100, 100, 255, 200, 200, 200, 255,
            ],
            has_alpha: true,
            is_image_mask: true,
            premultiplied: true,
        };
        let out = box_downscale(&image, 2, 1);
        assert_eq!((out.width, out.height), (2, 1));
        assert_eq!(out.data, [128, 128, 128, 255, 150, 150, 150, 255]);
        assert!(out.has_alpha && out.is_image_mask && out.premultiplied);
        assert_eq!(out.data.capacity(), out.data.len());
    }
}
