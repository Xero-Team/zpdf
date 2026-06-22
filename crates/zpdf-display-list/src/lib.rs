use zpdf_core::{Matrix, Point, Rect};

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

#[derive(Debug, Clone)]
pub struct Path {
    pub elements: Vec<PathElement>,
}

impl Path {
    pub fn new() -> Self {
        Self {
            elements: Vec::new(),
        }
    }

    pub fn move_to(&mut self, p: Point) {
        self.elements.push(PathElement::MoveTo(p));
    }

    pub fn line_to(&mut self, p: Point) {
        self.elements.push(PathElement::LineTo(p));
    }

    pub fn curve_to(&mut self, c1: Point, c2: Point, end: Point) {
        self.elements.push(PathElement::CurveTo(c1, c2, end));
    }

    pub fn close(&mut self) {
        self.elements.push(PathElement::Close);
    }

    pub fn rect(&mut self, r: Rect) {
        self.move_to(Point::new(r.x0, r.y0));
        self.line_to(Point::new(r.x1, r.y0));
        self.line_to(Point::new(r.x1, r.y1));
        self.line_to(Point::new(r.x0, r.y1));
        self.close();
    }

    pub fn is_empty(&self) -> bool {
        self.elements.is_empty()
    }
}

impl Default for Path {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy)]
pub enum PathElement {
    MoveTo(Point),
    LineTo(Point),
    CurveTo(Point, Point, Point),
    Close,
}

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

#[derive(Debug, Clone)]
pub enum Paint {
    Solid(Color),
    Pattern(u32),
    Shading(u32),
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
        self.active & (1 << i) != 0
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

#[derive(Debug, Clone, Copy)]
pub struct PositionedGlyph {
    pub glyph_id: u16,
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
