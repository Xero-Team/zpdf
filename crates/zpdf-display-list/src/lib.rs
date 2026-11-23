use zpdf_core::{Matrix, Rect};

mod path;

#[derive(Debug, Clone)]
pub struct DisplayList {
    pub page_rect: Rect,
    pub commands: Vec<RenderCommand>,
}

impl DisplayList {
    pub fn new(page_rect: Rect) -> Self {
        Self {
            page_rect,
            commands: Vec::new(),
        }
    }

    pub fn push(&mut self, cmd: RenderCommand) {
        self.commands.push(cmd);
    }
}

#[derive(Debug, Clone)]
pub enum RenderCommand {
    FillPath {
        path: Path,
        rule: FillRule,
        paint: Paint,
        alpha: f32,
        /// Overprint (PDF 8.6.7) for this fill, or `None` for a normal paint.
        overprint: Option<Overprint>,
    },
    StrokePath {
        path: Path,
        style: StrokeStyle,
        paint: Paint,
        alpha: f32,
        /// Overprint (PDF 8.6.7) for this stroke, or `None` for a normal paint.
        overprint: Option<Overprint>,
    },
    DrawGlyphRun(GlyphRun),
    DrawImage(ImageDraw),
    PushClip {
        path: Path,
        rule: FillRule,
    },
    /// Intersect the clip with a *stroked* path's outline (not its fill).
    /// Used to clip a pattern/shading paint to a stroke, since stroke geometry
    /// is the backend's job. Released by the matching [`RenderCommand::PopClip`].
    PushClipStroke {
        path: Path,
        style: StrokeStyle,
    },
    PopClip,
    PushBlendGroup {
        blend_mode: BlendMode,
        isolated: bool,
        knockout: bool,
        bounds: Rect,
        /// Group constant alpha applied when compositing onto the backdrop
        /// (the ExtGState /ca in effect when a transparency group is painted).
        alpha: f32,
        /// ExtGState /SMask soft mask modulating the group composite.
        mask: Option<SoftMask>,
    },
    PopBlendGroup,
}

/// An ExtGState /SMask soft mask: the mask group's content pre-interpreted
/// into page-space commands (geometry is fixed at `gs` time per PDF
/// 11.6.5.2), rasterized by the backend at composite resolution.
#[derive(Debug, Clone)]
pub struct SoftMask {
    pub kind: SoftMaskKind,
    /// The /G transparency group's interpreted content. Font/image ids refer
    /// to the same caches as the surrounding display list.
    pub commands: std::sync::Arc<DisplayList>,
    /// Page-space translation to apply to `commands` when rasterizing. Lets a
    /// mask built once for a tiling-pattern cell be reused at every tile
    /// position (the tile CTMs differ only by translation), instead of
    /// re-interpreting the mask group per tile.
    pub offset: (f32, f32),
    /// /BC backdrop luminosity (0..1) for areas the group leaves unpainted.
    /// Luminosity masks default to 0 (fully masked out).
    pub backdrop_luma: f32,
    /// /TR transfer function, pre-sampled over [0,1] into 256 steps.
    pub transfer: Option<std::sync::Arc<[u8; 256]>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SoftMaskKind {
    /// Mask value = group luminosity over the /BC backdrop.
    Luminosity,
    /// Mask value = group alpha.
    Alpha,
}

// -- Path --

pub use path::{Path, PathElement, PathIntoIter, PathIter};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillRule {
    NonZero,
    EvenOdd,
}

// -- Stroke --

#[derive(Debug, Clone)]
pub struct StrokeStyle {
    pub width: f32,
    pub cap: LineCap,
    pub join: LineJoin,
    pub miter_limit: f32,
    pub dash: Option<DashPattern>,
}

impl Default for StrokeStyle {
    fn default() -> Self {
        Self {
            width: 1.0,
            cap: LineCap::Butt,
            join: LineJoin::Miter,
            miter_limit: 10.0,
            dash: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineCap {
    Butt,
    Round,
    Square,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineJoin {
    Miter,
    Round,
    Bevel,
}

#[derive(Debug, Clone)]
pub struct DashPattern {
    pub array: Vec<f32>,
    pub phase: f32,
}

// -- Paint --

#[derive(Debug, Clone, Copy)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub fn rgb(r: f32, g: f32, b: f32) -> Self {
        Self { r, g, b, a: 1.0 }
    }

    pub fn rgba(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    pub fn gray(v: f32) -> Self {
        Self::rgb(v, v, v)
    }

    pub fn black() -> Self {
        Self::gray(0.0)
    }

    pub fn white() -> Self {
        Self::gray(1.0)
    }
}

/// Maximum shifted cell copies backends draw when an overlapping tiling
/// pattern's `/BBox` is larger than `(|XStep|, |YStep|)`. Real-world patterns
/// rarely exceed a few copies; this bounds adversarial huge-bbox/tiny-step
/// inputs while allowing wide sparse or moderately dense overlaps to render
/// completely.
pub const MAX_TILING_OVERLAP_COPIES: u64 = 256;

/// Integer tile-grid shift ranges `(di, dj)` whose shifted cell ink overlaps
/// a [`TilingPattern`]'s fundamental period `[bbox.x0, bbox.x0 + |x_step|] ×
/// [bbox.y0, bbox.y0 + |y_step|]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TilingOverlapRanges {
    pub di: std::ops::RangeInclusive<i64>,
    pub dj: std::ops::RangeInclusive<i64>,
    /// True when the cell's ink overlaps more than [`MAX_TILING_OVERLAP_COPIES`]
    /// shifted copies and the returned ranges were clamped to stay bounded.
    pub truncated: bool,
}

/// A PDF tiling pattern (ISO 32000-1 §8.7.3): the single cell's content
/// pre-interpreted in pattern space into a detached [`DisplayList`], repeated
/// across the `(x_step, y_step)` grid and mapped to page space by `matrix`.
#[derive(Debug, Clone)]
pub struct TilingPattern {
    /// The cell's interpreted commands in pattern space (clipped to `bbox`).
    /// Font and image IDs refer to the same caches as the surrounding display
    /// list.
    pub commands: std::sync::Arc<DisplayList>,
    /// Pattern-space cell bounding box (`/BBox`, normalized so `x0 < x1`,
    /// `y0 < y1`).
    pub bbox: Rect,
    /// Pattern-space horizontal spacing (`/XStep`, non-zero; may be negative).
    pub x_step: f64,
    /// Pattern-space vertical spacing (`/YStep`, non-zero; may be negative).
    pub y_step: f64,
    /// Pattern space → page space transform (`base_ctm · /Matrix`).
    pub matrix: Matrix,
    /// `/TilingType` (1 = constant spacing, 2 = no distortion, 3 = constant
    /// spacing and faster tiling).
    pub tiling_type: u8,
}

impl TilingPattern {
    /// Conservative pattern-space bounding box of painted primitives inside
    /// `self.commands`, intersected with `self.bbox`. Returns `None` when no
    /// primitive paints inside `bbox`, or falls back to `bbox` when any
    /// command's bounds require external font metrics (e.g. `DrawGlyphRun`).
    pub fn ink_bounds(&self) -> Option<Rect> {
        let bbox = self.bbox.normalize();
        if ![bbox.x0, bbox.y0, bbox.x1, bbox.y1]
            .iter()
            .all(|v| v.is_finite())
            || bbox.width() <= 0.0
            || bbox.height() <= 0.0
        {
            return None;
        }
        let mut has_ink = false;
        let (mut x0, mut y0) = (f64::INFINITY, f64::INFINITY);
        let (mut x1, mut y1) = (f64::NEG_INFINITY, f64::NEG_INFINITY);

        let mut include_rect = |rx0: f64, ry0: f64, rx1: f64, ry1: f64| -> bool {
            if ![rx0, ry0, rx1, ry1].iter().all(|v| v.is_finite()) {
                return false;
            }
            has_ink = true;
            x0 = x0.min(rx0);
            y0 = y0.min(ry0);
            x1 = x1.max(rx1);
            y1 = y1.max(ry1);
            true
        };

        let path_rect = |path: &Path| -> Option<(f64, f64, f64, f64)> {
            let mut any = false;
            let (mut px0, mut py0) = (f64::INFINITY, f64::INFINITY);
            let (mut px1, mut py1) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
            let mut add = |p: zpdf_core::Point| {
                any = true;
                px0 = px0.min(p.x);
                py0 = py0.min(p.y);
                px1 = px1.max(p.x);
                py1 = py1.max(p.y);
            };
            for elem in path {
                match elem {
                    PathElement::MoveTo(p) | PathElement::LineTo(p) => add(p),
                    PathElement::CurveTo(c1, c2, end) => {
                        add(c1);
                        add(c2);
                        add(end);
                    }
                    PathElement::Close => {}
                }
            }
            any.then_some((px0, py0, px1, py1))
        };

        for cmd in &self.commands.commands {
            match cmd {
                RenderCommand::FillPath { path, .. } => {
                    if let Some((px0, py0, px1, py1)) = path_rect(path) {
                        if !include_rect(px0, py0, px1, py1) {
                            return Some(bbox);
                        }
                    }
                }
                RenderCommand::StrokePath { path, style, .. } => {
                    if let Some((px0, py0, px1, py1)) = path_rect(path) {
                        let pad = if style.width.is_finite() && style.width > 0.0 {
                            let w = style.width as f64;
                            let miter = if style.miter_limit.is_finite() {
                                (style.miter_limit as f64).clamp(1.0, 100.0)
                            } else {
                                10.0
                            };
                            (w * 0.5 * miter).max(w)
                        } else {
                            // Hairline stroke: pad conservatively.
                            1.0
                        };
                        if !include_rect(px0 - pad, py0 - pad, px1 + pad, py1 + pad) {
                            return Some(bbox);
                        }
                    }
                }
                RenderCommand::DrawImage(draw) => {
                    let corners = [
                        zpdf_core::Point::new(0.0, 0.0),
                        zpdf_core::Point::new(1.0, 0.0),
                        zpdf_core::Point::new(0.0, 1.0),
                        zpdf_core::Point::new(1.0, 1.0),
                    ];
                    let (mut ix0, mut iy0) = (f64::INFINITY, f64::INFINITY);
                    let (mut ix1, mut iy1) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
                    for c in &corners {
                        let p = c.transform(&draw.transform);
                        ix0 = ix0.min(p.x);
                        iy0 = iy0.min(p.y);
                        ix1 = ix1.max(p.x);
                        iy1 = iy1.max(p.y);
                    }
                    if !include_rect(ix0, iy0, ix1, iy1) {
                        return Some(bbox);
                    }
                }
                // Glyph extents depend on font outlines not stored in DisplayList.
                RenderCommand::DrawGlyphRun(_) => return Some(bbox),
                RenderCommand::PushClip { .. }
                | RenderCommand::PushClipStroke { .. }
                | RenderCommand::PopClip
                | RenderCommand::PushBlendGroup { .. }
                | RenderCommand::PopBlendGroup => {}
            }
        }

        if !has_ink {
            return None;
        }
        let ix0 = x0.max(bbox.x0);
        let iy0 = y0.max(bbox.y0);
        let ix1 = x1.min(bbox.x1);
        let iy1 = y1.min(bbox.y1);
        if ix1 <= ix0 || iy1 <= iy0 {
            return None;
        }
        Some(Rect::new(ix0, iy0, ix1, iy1))
    }

    /// Compute the integer shift ranges `(di, dj)` whose shifted cell copies
    /// overlap the fundamental tile period `[bbox.x0, bbox.x0 + |x_step|] ×
    /// [bbox.y0, bbox.y0 + |y_step|]`.
    pub fn overlap_copy_ranges(&self) -> TilingOverlapRanges {
        let empty = || TilingOverlapRanges {
            di: std::ops::RangeInclusive::new(1, 0),
            dj: std::ops::RangeInclusive::new(1, 0),
            truncated: false,
        };
        let bbox = self.bbox.normalize();
        let (bw, bh) = (bbox.width(), bbox.height());
        let (x_step, y_step) = (self.x_step, self.y_step);
        if ![bbox.x0, bbox.y0, bw, bh, x_step, y_step]
            .iter()
            .all(|v| v.is_finite())
            || bw <= 0.0
            || bh <= 0.0
            || x_step == 0.0
            || y_step == 0.0
        {
            return empty();
        }
        let abs_x = x_step.abs();
        let abs_y = y_step.abs();

        // Fast path: non-overlapping cell (`/BBox` fits within one step on both axes).
        if bw <= abs_x + 1e-9 && bh <= abs_y + 1e-9 {
            return TilingOverlapRanges {
                di: 0..=0,
                dj: 0..=0,
                truncated: false,
            };
        }

        let Some(ink) = self.ink_bounds() else {
            return empty();
        };

        // Shifts `d` such that shifted ink `[ink_lo + d*step, ink_hi + d*step]`
        // has strict positive-length intersection with `[period_lo, period_lo + |step|]`.
        let axis_range = |period_lo: f64, ink_lo: f64, ink_hi: f64, step: f64| -> (i64, i64) {
            let abs_s = step.abs();
            let t0 = (period_lo - ink_hi) / step;
            let t1 = ((period_lo - ink_lo) + abs_s) / step;
            let (lo, hi) = if step > 0.0 { (t0, t1) } else { (t1, t0) };
            if !lo.is_finite() || !hi.is_finite() {
                return (1, 0);
            }
            let min_d = (lo.floor() as i64).saturating_add(1);
            let max_d = (hi.ceil() as i64).saturating_sub(1);
            (min_d, max_d)
        };

        let (i0, mut i1) = axis_range(bbox.x0, ink.x0, ink.x1, x_step);
        let (j0, mut j1) = axis_range(bbox.y0, ink.y0, ink.y1, y_step);
        if i0 > i1 || j0 > j1 {
            return empty();
        }

        let nx = i1.abs_diff(i0).saturating_add(1);
        let ny = j1.abs_diff(j0).saturating_add(1);
        let mut truncated = false;
        if nx.saturating_mul(ny) > MAX_TILING_OVERLAP_COPIES {
            truncated = true;
            let (cap_x, cap_y) = if ny <= 16 {
                (MAX_TILING_OVERLAP_COPIES / ny.max(1), ny)
            } else if nx <= 16 {
                (nx, MAX_TILING_OVERLAP_COPIES / nx.max(1))
            } else {
                (16, 16)
            };
            if nx > cap_x {
                i1 = i0.saturating_add((cap_x - 1) as i64);
            }
            if ny > cap_y {
                j1 = j0.saturating_add((cap_y - 1) as i64);
            }
        }

        TilingOverlapRanges {
            di: i0..=i1,
            dj: j0..=j1,
            truncated,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Paint {
    Solid(Color),
    Pattern(u32),
    Shading(u32),
    Tiling(std::sync::Arc<TilingPattern>),
}

/// Overprint descriptor (PDF 8.6.7) for a painted primitive whose source colour
/// lives in a device-colorant space (DeviceCMYK / DeviceGray / Separation /
/// DeviceN) and whose ExtGState enables overprint (`/OP`, `/op`, `/OPM`).
///
/// Backends composite it in **naïve subtractive CMYK**: the colorants whose bit
/// is set in `active` are painted from `cmyk`; the rest are read straight from
/// the backdrop, so the operation never disturbs colorants it does not name
/// (e.g. K-only black text overprints onto a colour without knocking it out).
/// The conversion uses `zpdf_color::cmyk_to_rgb_naive` /
/// `zpdf_color::rgb_to_cmyk_naive` so untouched colorants round-trip exactly.
///
/// `active` is never `0b1111` (all colorants painted == a normal opaque paint,
/// emitted without an `Overprint`), but may be `0` (paints nothing — e.g. white
/// in DeviceGray under the nonzero rule).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Overprint {
    /// Source colour as process-colorant tints `(C, M, Y, K)`, each in `0..=1`.
    pub cmyk: [f32; 4],
    /// Bitmask of painted colorants: `C=1, M=2, Y=4, K=8`.
    pub active: u8,
}

impl Overprint {
    pub const C: u8 = 1;
    pub const M: u8 = 2;
    pub const Y: u8 = 4;
    pub const K: u8 = 8;

    /// True if colorant `i` (0=C,1=M,2=Y,3=K) is painted by this operation.
    #[inline]
    pub fn paints(&self, i: usize) -> bool {
        i < 4 && self.active & (1u8 << i) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overprint_channel_query_is_total() {
        let op = Overprint {
            cmyk: [0.0; 4],
            active: Overprint::C | Overprint::K,
        };
        assert!(op.paints(0));
        assert!(op.paints(3));
        assert!(!op.paints(4));
        assert!(!op.paints(usize::MAX));
    }

    #[test]
    fn positioned_glyph_default_has_unspecified_code_len() {
        let glyph = PositionedGlyph {
            glyph_id: 7,
            code: 0x0102,
            x: 10.0,
            y: 20.0,
            advance: 6.0,
            ..PositionedGlyph::default()
        };
        assert_eq!(glyph.code_len, 0);
        assert_eq!(std::mem::size_of::<PositionedGlyph>(), 20);
    }

    #[test]
    fn tiling_overlap_copy_ranges_handles_wide_sparse_negative_and_capped() {
        let make = |bbox: Rect, x_step: f64, y_step: f64, ink: Rect| {
            let mut dl = DisplayList::new(bbox);
            let mut p = Path::new();
            p.rect(ink);
            dl.push(RenderCommand::FillPath {
                path: p,
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::black()),
                alpha: 1.0,
                overprint: None,
            });
            TilingPattern {
                commands: std::sync::Arc::new(dl),
                bbox,
                x_step,
                y_step,
                matrix: Matrix::identity(),
                tiling_type: 1,
            }
        };

        // Sparse ink far past 4 steps (di = -4 and di = -9).
        let r1 = make(
            Rect::new(0.0, 0.0, 50.0, 10.0),
            10.0,
            10.0,
            Rect::new(45.0, 2.0, 47.0, 8.0),
        )
        .overlap_copy_ranges();
        assert_eq!(r1.di, -4..=-4);
        assert_eq!(r1.dj, 0..=0);
        assert!(!r1.truncated);

        let r2 = make(
            Rect::new(0.0, 0.0, 100.0, 10.0),
            10.0,
            10.0,
            Rect::new(90.0, 2.0, 99.0, 8.0),
        )
        .overlap_copy_ranges();
        assert_eq!(r2.di, -9..=-9);
        assert_eq!(r2.dj, 0..=0);
        assert!(!r2.truncated);

        // Negative step wraps in the opposite index direction.
        let r3 = make(
            Rect::new(0.0, 0.0, 50.0, 10.0),
            -10.0,
            10.0,
            Rect::new(45.0, 2.0, 47.0, 8.0),
        )
        .overlap_copy_ranges();
        assert_eq!(r3.di, 4..=4);
        assert_eq!(r3.dj, 0..=0);
        assert!(!r3.truncated);

        // Pathological dense overlap exceeds MAX_TILING_OVERLAP_COPIES and flags truncation.
        let r4 = make(
            Rect::new(0.0, 0.0, 5000.0, 5000.0),
            10.0,
            10.0,
            Rect::new(0.0, 0.0, 5000.0, 5000.0),
        )
        .overlap_copy_ranges();
        assert!(r4.truncated);
        let nx = r4.di.end().abs_diff(*r4.di.start()) + 1;
        let ny = r4.dj.end().abs_diff(*r4.dj.start()) + 1;
        assert!(nx * ny <= MAX_TILING_OVERLAP_COPIES);
    }
}

// -- Text --

pub type FontId = u32;
pub type ImageId = u32;

#[derive(Debug, Clone)]
pub struct GlyphRun {
    pub font_id: FontId,
    pub font_size: f32,
    pub glyphs: Vec<PositionedGlyph>,
    pub paint: Paint,
    pub alpha: f32,
    /// Overprint (PDF 8.6.7) for this glyph run, or `None` for a normal paint.
    pub overprint: Option<Overprint>,
    pub transform: Matrix,
    /// Horizontal text-scaling factor (Tz/100). Scales the glyph *shape* x only;
    /// per-glyph advances already include it. Almost always 1.0; negative values
    /// (e.g. Tz -100) mirror glyphs horizontally — see `outline_to_pixel`.
    pub h_scale: f32,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PositionedGlyph {
    pub glyph_id: u16,
    /// Source byte length of `code` in the PDF text string (`1..=4`), or `0`
    /// when unspecified by a synthetic caller (or via `..PositionedGlyph::default()`),
    /// in which case exporters infer the byte width from the logical font's
    /// CMap or `/ToUnicode` table. Fits in the alignment padding after
    /// `glyph_id` without increasing struct size (20 bytes).
    pub code_len: u8,
    /// Raw big-endian source character code bytes from the PDF string (before
    /// CID/CMap mapping). `/ToUnicode` CMaps are keyed by these packed source
    /// bytes, not by the glyph id; exporters that only need outlines can ignore
    /// it.
    pub code: u32,
    pub x: f32,
    pub y: f32,
    pub advance: f32,
}

// -- Image --

#[derive(Debug, Clone)]
pub struct ImageDraw {
    pub image_id: ImageId,
    pub transform: Matrix,
    pub alpha: f32,
    /// True only when the source image is a PDF `/ImageMask` stencil.
    pub is_image_mask: bool,
}

// -- Blend --

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BlendMode {
    #[default]
    Normal,
    Multiply,
    Screen,
    Overlay,
    Darken,
    Lighten,
    ColorDodge,
    ColorBurn,
    HardLight,
    SoftLight,
    Difference,
    Exclusion,
    Hue,
    Saturation,
    Color,
    Luminosity,
}
