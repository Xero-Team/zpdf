//! Axial / radial shading evaluation (ISO 32000-1 §8.7.4.5.3–4).
//!
//! A [`ShadingDef`] is built by the interpreter (which resolves the PDF
//! objects and colorspace) and holds a pre-sampled 256-entry RGB lookup of the
//! shading function over its /Domain. [`rasterize`] renders it into an RGBA
//! buffer covering a page-space rectangle, which the backends draw through the
//! ordinary image pipeline.

use zpdf_core::{Matrix, Rect};

/// Public `rasterize` callers may supply dimensions directly. Keep a single
/// shading allocation within the core page-pixel budget and reject arithmetic
/// overflow before constructing the RGBA buffer.
const MAX_RASTER_PIXELS: u64 = 64_000_000;

#[derive(Debug, Clone)]
pub struct ShadingDef {
    pub kind: ShadingKind,
    /// 256 RGB samples of the color function over /Domain.
    pub lut: Vec<[f32; 3]>,
    pub extend_start: bool,
    pub extend_end: bool,
    /// Maps shading space to page space (CTM for `sh`, pattern /Matrix for
    /// shading patterns).
    pub to_page: Matrix,
}

#[derive(Debug, Clone)]
pub enum ShadingKind {
    Axial {
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
    },
    Radial {
        x0: f64,
        y0: f64,
        r0: f64,
        x1: f64,
        y1: f64,
        r1: f64,
    },
    /// Types 4–7 (free-form/lattice Gouraud meshes, Coons/tensor patches):
    /// pre-tessellated triangles already transformed into **page space**, each
    /// vertex carrying its resolved RGB. Rasterized by Gouraud interpolation
    /// rather than the LUT path (`lut` holds the mean colour for the
    /// pattern-stroke fallback; `to_page`/`extend_*` are unused).
    Mesh {
        triangles: Vec<MeshTriangle>,
    },
}

/// A mesh-shading vertex: page-space position with resolved RGB.
#[derive(Debug, Clone, Copy)]
pub struct MeshVertex {
    pub x: f32,
    pub y: f32,
    pub rgb: [f32; 3],
}

/// A mesh-shading triangle (three Gouraud-shaded vertices).
#[derive(Debug, Clone, Copy)]
pub struct MeshTriangle {
    pub v: [MeshVertex; 3],
}

impl ShadingDef {
    /// Mean LUT color — used as the solid approximation for shading-pattern
    /// strokes and as a last-resort fill.
    pub fn average_rgb(&self) -> [f32; 3] {
        let n = self.lut.len().max(1) as f32;
        let mut acc = [0.0f32; 3];
        for c in &self.lut {
            acc[0] += c[0];
            acc[1] += c[1];
            acc[2] += c[2];
        }
        [acc[0] / n, acc[1] / n, acc[2] / n]
    }

    fn sample(&self, s: f64) -> [f32; 3] {
        if self.lut.is_empty() {
            return [0.0; 3];
        }
        let idx = (s.clamp(0.0, 1.0) * 255.0).round() as usize;
        self.lut[idx.min(self.lut.len() - 1)]
    }

    /// Parametric position of a shading-space point, or `None` where the
    /// shading paints nothing (outside an unextended ramp or radial cone).
    fn param_at(&self, x: f64, y: f64) -> Option<f64> {
        match &self.kind {
            ShadingKind::Axial { x0, y0, x1, y1 } => {
                let (x0, y0, x1, y1) = (*x0, *y0, *x1, *y1);
                let (dx, dy) = (x1 - x0, y1 - y0);
                let len2 = dx * dx + dy * dy;
                if len2 < f64::EPSILON {
                    return None;
                }
                let t = ((x - x0) * dx + (y - y0) * dy) / len2;
                self.clamp_extend(t)
            }
            ShadingKind::Radial {
                x0,
                y0,
                r0,
                x1,
                y1,
                r1,
            } => {
                let (x0, y0, r0, x1, y1, r1) = (*x0, *y0, *r0, *x1, *y1, *r1);
                // Solve |p - c(s)| = r(s) with c(s) = c0 + s·(c1-c0),
                // r(s) = r0 + s·(r1-r0) for the largest valid s (per spec).
                let (cdx, cdy, dr) = (x1 - x0, y1 - y0, r1 - r0);
                let (px, py) = (x - x0, y - y0);
                let a = cdx * cdx + cdy * cdy - dr * dr;
                let b = px * cdx + py * cdy + r0 * dr;
                let c = px * px + py * py - r0 * r0;
                // Per spec: the LARGEST s inside the (possibly extended)
                // domain wins — so try the larger root through the extend
                // test first, but fall back to the smaller in-domain root
                // before giving up (decentered gradients with extend off).
                let roots: [Option<f64>; 2] = if a.abs() < 1e-9 {
                    if b.abs() < 1e-12 {
                        return None;
                    }
                    [Some(c / (2.0 * b)), None]
                } else {
                    let disc = b * b - a * c;
                    if disc < 0.0 {
                        return None;
                    }
                    let sq = disc.sqrt();
                    let s1 = (b + sq) / a;
                    let s2 = (b - sq) / a;
                    [Some(s1.max(s2)), Some(s1.min(s2))]
                };
                for s in roots.into_iter().flatten() {
                    if r0 + s * dr >= 0.0 {
                        if let Some(t) = self.clamp_extend(s) {
                            return Some(t);
                        }
                    }
                }
                None
            }
            // Meshes are not parametric; `rasterize` handles them directly.
            ShadingKind::Mesh { .. } => None,
        }
    }

    fn clamp_extend(&self, t: f64) -> Option<f64> {
        if t < 0.0 {
            if self.extend_start {
                Some(0.0)
            } else {
                None
            }
        } else if t > 1.0 {
            if self.extend_end {
                Some(1.0)
            } else {
                None
            }
        } else {
            Some(t)
        }
    }
}

/// Rasterize the shading into a `w`×`h` premultiplied-RGBA buffer covering
/// the page-space rect `region` (row 0 = top edge, i.e. `region.y1`).
/// Returns `None` if the shading-space transform is singular.
pub fn rasterize(def: &ShadingDef, region: Rect, w: u32, h: u32) -> Option<Vec<u8>> {
    // Mesh shadings carry their own page-space geometry — Gouraud-rasterize the
    // triangles directly instead of inverse-mapping a parametric ramp.
    if let ShadingKind::Mesh { triangles } = &def.kind {
        return rasterize_mesh(triangles, region, w, h);
    }
    let inv = def.to_page.inverse()?;
    let mut buf = vec![0u8; rgba_len(w, h)?];
    let (rw, rh) = (region.width(), region.height());
    for j in 0..h {
        // Pixel-center page-space y, row 0 at the top.
        let py = region.y1 - (j as f64 + 0.5) / h as f64 * rh;
        for i in 0..w {
            let px = region.x0 + (i as f64 + 0.5) / w as f64 * rw;
            // page -> shading space
            let sx = inv.a * px + inv.c * py + inv.e;
            let sy = inv.b * px + inv.d * py + inv.f;
            if let Some(t) = def.param_at(sx, sy) {
                let rgb = def.sample(t);
                let o = ((j as usize) * (w as usize) + i as usize) * 4;
                buf[o] = (rgb[0].clamp(0.0, 1.0) * 255.0).round() as u8;
                buf[o + 1] = (rgb[1].clamp(0.0, 1.0) * 255.0).round() as u8;
                buf[o + 2] = (rgb[2].clamp(0.0, 1.0) * 255.0).round() as u8;
                buf[o + 3] = 255;
            }
        }
    }
    Some(buf)
}

/// Gouraud-rasterize page-space `triangles` into a `w`×`h` premultiplied-RGBA
/// buffer covering page-space `region` (row 0 = top). Every covered pixel is
/// opaque, so straight RGB equals premultiplied.
fn rasterize_mesh(triangles: &[MeshTriangle], region: Rect, w: u32, h: u32) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; rgba_len(w, h)?];
    let (rw, rh) = (region.width(), region.height());
    if rw <= 0.0 || rh <= 0.0 || w == 0 || h == 0 {
        return Some(buf);
    }
    let to_px = |x: f32, y: f32| -> (f32, f32) {
        (
            ((x as f64 - region.x0) / rw * w as f64) as f32,
            ((region.y1 - y as f64) / rh * h as f64) as f32,
        )
    };
    for tri in triangles {
        raster_triangle(&mut buf, w, h, tri, &to_px);
    }
    Some(buf)
}

fn rgba_len(w: u32, h: u32) -> Option<usize> {
    let pixels = u64::from(w).checked_mul(u64::from(h))?;
    if pixels > MAX_RASTER_PIXELS {
        return None;
    }
    usize::try_from(pixels.checked_mul(4)?).ok()
}

/// Scanline-fill one triangle with barycentric RGB interpolation. The top-left
/// fill rule ensures pixels on a shared edge are painted by exactly one of the
/// two adjacent triangles (no seams, no double blend).
fn raster_triangle(
    buf: &mut [u8],
    w: u32,
    h: u32,
    tri: &MeshTriangle,
    to_px: &impl Fn(f32, f32) -> (f32, f32),
) {
    let p = [
        to_px(tri.v[0].x, tri.v[0].y),
        to_px(tri.v[1].x, tri.v[1].y),
        to_px(tri.v[2].x, tri.v[2].y),
    ];
    let c = [tri.v[0].rgb, tri.v[1].rgb, tri.v[2].rgb];
    let minx = p
        .iter()
        .map(|q| q.0)
        .fold(f32::INFINITY, f32::min)
        .floor()
        .max(0.0) as i32;
    let maxx = p
        .iter()
        .map(|q| q.0)
        .fold(f32::NEG_INFINITY, f32::max)
        .ceil()
        .min(w as f32) as i32;
    let miny = p
        .iter()
        .map(|q| q.1)
        .fold(f32::INFINITY, f32::min)
        .floor()
        .max(0.0) as i32;
    let maxy = p
        .iter()
        .map(|q| q.1)
        .fold(f32::NEG_INFINITY, f32::max)
        .ceil()
        .min(h as f32) as i32;
    let area = edge(p[0], p[1], p[2]);
    if area.abs() < 1e-7 {
        return; // degenerate
    }
    let inv = 1.0 / area;
    // An edge a→b is a "top or left" edge (its zero-coverage pixels are filled).
    let tl = |a: (f32, f32), b: (f32, f32)| -> bool { a.1 > b.1 || (a.1 == b.1 && a.0 < b.0) };
    let (tl0, tl1, tl2) = (tl(p[1], p[2]), tl(p[2], p[0]), tl(p[0], p[1]));
    for y in miny..maxy {
        for x in minx..maxx {
            let s = (x as f32 + 0.5, y as f32 + 0.5);
            let w0 = edge(p[1], p[2], s);
            let w1 = edge(p[2], p[0], s);
            let w2 = edge(p[0], p[1], s);
            let inside = if area > 0.0 {
                (w0 > 0.0 || (w0 == 0.0 && tl0))
                    && (w1 > 0.0 || (w1 == 0.0 && tl1))
                    && (w2 > 0.0 || (w2 == 0.0 && tl2))
            } else {
                (w0 < 0.0 || (w0 == 0.0 && tl0))
                    && (w1 < 0.0 || (w1 == 0.0 && tl1))
                    && (w2 < 0.0 || (w2 == 0.0 && tl2))
            };
            if !inside {
                continue;
            }
            let (b0, b1, b2) = (w0 * inv, w1 * inv, w2 * inv);
            let r = b0 * c[0][0] + b1 * c[1][0] + b2 * c[2][0];
            let g = b0 * c[0][1] + b1 * c[1][1] + b2 * c[2][1];
            let bl = b0 * c[0][2] + b1 * c[1][2] + b2 * c[2][2];
            let o = ((y as usize) * (w as usize) + x as usize) * 4;
            buf[o] = (r.clamp(0.0, 1.0) * 255.0).round() as u8;
            buf[o + 1] = (g.clamp(0.0, 1.0) * 255.0).round() as u8;
            buf[o + 2] = (bl.clamp(0.0, 1.0) * 255.0).round() as u8;
            buf[o + 3] = 255;
        }
    }
}

/// Signed area (×2) of triangle `a,b,c` — the edge function for `c` against `a→b`.
#[inline]
fn edge(a: (f32, f32), b: (f32, f32), c: (f32, f32)) -> f32 {
    (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp_def(kind: ShadingKind, extend: (bool, bool)) -> ShadingDef {
        // black -> white ramp
        let lut = (0..256)
            .map(|i| {
                let v = i as f32 / 255.0;
                [v, v, v]
            })
            .collect();
        ShadingDef {
            kind,
            lut,
            extend_start: extend.0,
            extend_end: extend.1,
            to_page: Matrix::identity(),
        }
    }

    #[test]
    fn axial_param_and_extend() {
        let d = ramp_def(
            ShadingKind::Axial {
                x0: 0.0,
                y0: 0.0,
                x1: 10.0,
                y1: 0.0,
            },
            (false, true),
        );
        assert_eq!(d.param_at(5.0, 3.0), Some(0.5)); // perpendicular offset ignored
        assert_eq!(d.param_at(-1.0, 0.0), None); // before start, unextended
        assert_eq!(d.param_at(15.0, 0.0), Some(1.0)); // extended end
    }

    #[test]
    fn radial_concentric() {
        let d = ramp_def(
            ShadingKind::Radial {
                x0: 0.0,
                y0: 0.0,
                r0: 0.0,
                x1: 0.0,
                y1: 0.0,
                r1: 10.0,
            },
            (false, false),
        );
        let t = d.param_at(5.0, 0.0).unwrap();
        assert!((t - 0.5).abs() < 1e-9);
        assert!(d.param_at(20.0, 0.0).is_none());
    }

    #[test]
    fn radial_decentered_falls_back_to_smaller_root() {
        // Two equal circles side by side, extend off. Point (11,0) lies on
        // the s=0.9 circle (center (9,0), r=2); the larger root 1.3 is out of
        // domain, so the smaller in-domain root must win — not a dropout.
        let d = ramp_def(
            ShadingKind::Radial {
                x0: 0.0,
                y0: 0.0,
                r0: 2.0,
                x1: 10.0,
                y1: 0.0,
                r1: 2.0,
            },
            (false, false),
        );
        let t = d
            .param_at(11.0, 0.0)
            .expect("point is covered by s=0.9 circle");
        assert!((t - 0.9).abs() < 1e-9, "got {t}");
        // With extend on, the larger root (clamped to 1.0) wins instead.
        let d_ext = ramp_def(
            ShadingKind::Radial {
                x0: 0.0,
                y0: 0.0,
                r0: 2.0,
                x1: 10.0,
                y1: 0.0,
                r1: 2.0,
            },
            (false, true),
        );
        assert_eq!(d_ext.param_at(11.0, 0.0), Some(1.0));
    }

    #[test]
    fn rasterize_axial_gradient() {
        let d = ramp_def(
            ShadingKind::Axial {
                x0: 0.0,
                y0: 0.0,
                x1: 100.0,
                y1: 0.0,
            },
            (true, true),
        );
        let buf = rasterize(&d, Rect::new(0.0, 0.0, 100.0, 10.0), 10, 2).unwrap();
        // Leftmost pixel near black, rightmost near white, alpha opaque.
        assert!(buf[0] < 30, "left {}", buf[0]);
        let right = ((10 - 1) * 4) as usize;
        assert!(buf[right] > 225, "right {}", buf[right]);
        assert_eq!(buf[3], 255);
    }

    #[test]
    fn rasterize_mesh_triangle() {
        // Page-space triangle (0,0)=red, (1,0)=green, (0,1)=blue over the unit
        // square, rasterized at 4×4. Gouraud interpolation + coverage.
        let tri = MeshTriangle {
            v: [
                MeshVertex {
                    x: 0.0,
                    y: 0.0,
                    rgb: [1.0, 0.0, 0.0],
                },
                MeshVertex {
                    x: 1.0,
                    y: 0.0,
                    rgb: [0.0, 1.0, 0.0],
                },
                MeshVertex {
                    x: 0.0,
                    y: 1.0,
                    rgb: [0.0, 0.0, 1.0],
                },
            ],
        };
        let def = ShadingDef {
            kind: ShadingKind::Mesh {
                triangles: vec![tri],
            },
            lut: vec![[0.33, 0.33, 0.33]],
            extend_start: false,
            extend_end: false,
            to_page: Matrix::identity(),
        };
        let buf = rasterize(&def, Rect::new(0.0, 0.0, 1.0, 1.0), 4, 4).unwrap();
        // Bottom-left pixel (page near (0,0)) sits by the red corner: opaque & red-dominant.
        let o = (3 * 4) * 4; // row 3, col 0
        assert_eq!(buf[o + 3], 255, "covered pixel must be opaque");
        assert!(
            buf[o] > buf[o + 2] && buf[o] > buf[o + 1],
            "near (0,0) should be red-dominant: {:?}",
            &buf[o..o + 4]
        );
    }

    #[test]
    fn raster_dimensions_are_bounded_before_allocation() {
        let d = ramp_def(
            ShadingKind::Axial {
                x0: 0.0,
                y0: 0.0,
                x1: 1.0,
                y1: 0.0,
            },
            (false, false),
        );
        assert!(rasterize(&d, Rect::new(0.0, 0.0, 1.0, 1.0), u32::MAX, 2).is_none());
    }

    #[test]
    fn empty_lut_degrades_to_black() {
        let mut d = ramp_def(
            ShadingKind::Axial {
                x0: 0.0,
                y0: 0.0,
                x1: 1.0,
                y1: 0.0,
            },
            (true, true),
        );
        d.lut.clear();
        let buf = rasterize(&d, Rect::new(0.0, 0.0, 1.0, 1.0), 1, 1).unwrap();
        assert_eq!(buf, [0, 0, 0, 255]);
    }
}
