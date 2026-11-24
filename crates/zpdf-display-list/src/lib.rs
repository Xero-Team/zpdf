use zpdf_core::{Matrix, Rect};

mod path;

#[derive(Debug, Clone)]
pub struct DisplayList {
    pub page_rect: Rect,
    /// Visual commands executed in order by render backends.
    ///
    /// Callers that also retain [`Self::expanded_glyph_runs`] must treat
    /// `commands` as append-only while those ranges are live (do not insert,
    /// remove, or reorder entries before a recorded range). Call
    /// [`Self::clear_expanded_glyph_runs`] before mutating `commands` in-place
    /// so recorded ranges cannot silently refer to different commands.
    pub commands: Vec<RenderCommand>,
    expanded_glyph_runs: Vec<ExpandedGlyphRun>,
}

impl DisplayList {
    pub fn new(page_rect: Rect) -> Self {
        Self {
            page_rect,
            commands: Vec::new(),
            expanded_glyph_runs: Vec::new(),
        }
    }

    pub fn push(&mut self, cmd: RenderCommand) {
        self.commands.push(cmd);
    }

    /// Record a semantic glyph run whose visual marks have already been
    /// expanded into `commands[cmd_start..cmd_end]` (for example, Type 3
    /// CharProcs interpreted directly into `RenderCommand`s).
    ///
    /// Visual backends execute `commands` directly so each glyph paints once;
    /// text-preserving exporters such as `zpdf-pptx-export` can decode the
    /// semantic run and skip its expanded geometry range.
    ///
    /// # Panics
    ///
    /// Panics if `cmd_start > cmd_end`, `cmd_end > self.commands.len()`, or
    /// `cmd_start` overlaps a previously recorded range (`cmd_start <
    /// previous.cmd_end`).
    pub fn push_expanded_glyph_run(&mut self, cmd_start: usize, cmd_end: usize, run: GlyphRun) {
        assert!(
            cmd_start <= cmd_end && cmd_end <= self.commands.len(),
            "expanded glyph run range {cmd_start}..{cmd_end} out of bounds for {} commands",
            self.commands.len()
        );
        if let Some(prev) = self.expanded_glyph_runs.last() {
            assert!(
                cmd_start >= prev.cmd_end,
                "expanded glyph run range {cmd_start}..{cmd_end} overlaps previous range {}..{}",
                prev.cmd_start,
                prev.cmd_end
            );
        }
        self.expanded_glyph_runs.push(ExpandedGlyphRun {
            cmd_start,
            cmd_end,
            run,
        });
    }

    /// Discard all recorded expanded glyph runs. Call before removing,
    /// inserting, or reordering entries in `self.commands` in-place.
    pub fn clear_expanded_glyph_runs(&mut self) {
        self.expanded_glyph_runs.clear();
    }

    /// Semantic glyph runs already expanded into `commands`, ordered by
    /// `cmd_start` with non-overlapping `[cmd_start, cmd_end)` ranges.
    pub fn expanded_glyph_runs(&self) -> &[ExpandedGlyphRun] {
        &self.expanded_glyph_runs
    }
}

/// Semantic glyph run whose visual output has already been expanded into
/// `DisplayList::commands[cmd_start..cmd_end]`.
#[derive(Debug, Clone)]
pub struct ExpandedGlyphRun {
    pub cmd_start: usize,
    pub cmd_end: usize,
    pub run: GlyphRun,
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
    /// Self-contained inline bitmap draw (for example, a Type 3 CharProc
    /// inline image expanded without an external `ImageCache`).
    DrawInlineImage(InlineImageDraw),
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

    fn sample_glyph_run(font_id: FontId) -> GlyphRun {
        GlyphRun {
            font_id,
            font_size: 12.0,
            glyphs: vec![PositionedGlyph {
                glyph_id: 65,
                code_len: 1,
                code: 65,
                x: 0.0,
                y: 0.0,
                advance: 6.0,
            }],
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
            transform: Matrix::identity(),
            h_scale: 1.0,
        }
    }

    #[test]
    fn expanded_glyph_runs_preserve_command_range() {
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        assert!(dl.expanded_glyph_runs().is_empty());
        for _ in 0..4 {
            dl.push(RenderCommand::PopClip);
        }
        dl.push_expanded_glyph_run(1, 4, sample_glyph_run(2));
        let runs = dl.expanded_glyph_runs();
        assert_eq!(runs.len(), 1);
        assert_eq!((runs[0].cmd_start, runs[0].cmd_end), (1, 4));
        assert_eq!(runs[0].run.font_id, 2);

        dl.clear_expanded_glyph_runs();
        assert!(dl.expanded_glyph_runs().is_empty());
    }

    #[test]
    #[should_panic(expected = "out of bounds")]
    fn expanded_glyph_run_rejects_end_past_commands_len() {
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        dl.push(RenderCommand::PopClip);
        dl.push_expanded_glyph_run(0, 2, sample_glyph_run(1));
    }

    #[test]
    #[should_panic(expected = "out of bounds")]
    fn expanded_glyph_run_rejects_inverted_range() {
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        dl.push(RenderCommand::PopClip);
        dl.push(RenderCommand::PopClip);
        dl.push_expanded_glyph_run(2, 1, sample_glyph_run(1));
    }

    #[test]
    #[should_panic(expected = "overlaps")]
    fn expanded_glyph_run_rejects_overlapping_range() {
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        for _ in 0..4 {
            dl.push(RenderCommand::PopClip);
        }
        dl.push_expanded_glyph_run(0, 3, sample_glyph_run(1));
        dl.push_expanded_glyph_run(2, 4, sample_glyph_run(2));
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

/// Self-contained decoded RGBA8 bitmap carried directly on a
/// [`RenderCommand::DrawInlineImage`].
#[derive(Debug, Clone)]
pub struct InlineImage {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    pub has_alpha: bool,
    /// True only when the source image is a PDF `/ImageMask` stencil.
    pub is_image_mask: bool,
    pub premultiplied: bool,
}

#[derive(Debug, Clone)]
pub struct InlineImageDraw {
    pub image: std::sync::Arc<InlineImage>,
    pub transform: Matrix,
    pub alpha: f32,
}

// -- Blend --

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
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

impl BlendMode {
    /// Return the standard PDF `/BM` name for this blend mode (ISO 32000-1 §11.3.5).
    pub const fn pdf_name(self) -> &'static str {
        match self {
            Self::Normal => "Normal",
            Self::Multiply => "Multiply",
            Self::Screen => "Screen",
            Self::Overlay => "Overlay",
            Self::Darken => "Darken",
            Self::Lighten => "Lighten",
            Self::ColorDodge => "ColorDodge",
            Self::ColorBurn => "ColorBurn",
            Self::HardLight => "HardLight",
            Self::SoftLight => "SoftLight",
            Self::Difference => "Difference",
            Self::Exclusion => "Exclusion",
            Self::Hue => "Hue",
            Self::Saturation => "Saturation",
            Self::Color => "Color",
            Self::Luminosity => "Luminosity",
        }
    }
}
