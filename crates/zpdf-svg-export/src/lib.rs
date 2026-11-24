//! Vector-faithful SVG export from zpdf display lists.
//!
//! Converts the flat [`DisplayList`] command stream — the same one the CPU and
//! GPU render backends consume — into a standalone SVG document. Semantics
//! mirror the CPU backend command-for-command: solid paints only (patterns and
//! shadings are already rasterized or tiled upstream by the interpreter), text
//! as glyph-outline paths, Type3 glyphs via their interpreted vector content,
//! images as embedded base64 PNG.
//!
//! Coordinates are emitted in PDF points with the page's fixed Y-flip baked in
//! (`svg = (x - x0, y1 - y)`), so the viewBox equals the page box at 72 dpi and
//! no `transform` is needed on the root.
//!
//! Approximations (each logged once per export via `tracing`): overprint
//! composites paint normally, knockout groups composite as normal groups, and
//! soft-mask `/TR` transfer functions are ignored.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::rc::Rc;

use image::ImageEncoder;
use zpdf_display_list::{
    BlendMode, Color, DisplayList, FillRule, GlyphRun, ImageDraw, LineCap, LineJoin, Paint, Path,
    PathElement, RenderCommand, SoftMask, SoftMaskKind, StrokeStyle,
};
use zpdf_font::{FontCache, FontId, GlyphOutline, LoadedFont, OutlineCommand};
use zpdf_image::{DecodedImage, ImageCache, ImageId};

/// Options for SVG export.
#[derive(Debug, Clone)]
pub struct SvgOptions {
    /// Opaque background painted under the page content (PDF pages are
    /// conceptually white paper). `None` leaves the canvas transparent.
    pub background: Option<Color>,
    /// Wall-clock budget for one export (`None` disables). Mirrors the CPU
    /// backend's per-page render budget: adversarial display lists (huge
    /// Type3 fan-outs, thousands of pattern-cell images) stop emitting paint
    /// once the budget is spent, and the document stays well-formed.
    pub budget: Option<std::time::Duration>,
}

/// Same default as the CPU backend's `DEFAULT_RENDER_BUDGET`.
const DEFAULT_EXPORT_BUDGET: std::time::Duration = std::time::Duration::from_secs(8);

impl Default for SvgOptions {
    fn default() -> Self {
        Self {
            background: Some(Color::white()),
            budget: Some(DEFAULT_EXPORT_BUDGET),
        }
    }
}

/// Coordinates beyond this are considered corrupt (degenerate CTMs, fuzzer
/// geometry) and the whole path is skipped, mirroring the CPU backend's
/// `finish_within_limits` guard.
const MAX_COORD: f64 = 1.0e8;

/// Soft masks nest through their own display lists; bound the recursion the
/// same way the renderers bound blend-group depth.
const MAX_MASK_DEPTH: u8 = 8;
/// Outline commands kept by the per-export glyph outline cache. Past it,
/// further glyphs are extracted on every use, as without the cache.
const MAX_CACHED_OUTLINE_COMMANDS: usize = 512 * 1024;
/// Display-list commands kept by the per-export Type 3 glyph cache.
const MAX_CACHED_TYPE3_COMMANDS: usize = 256 * 1024;
/// Decoded image bytes kept by the per-export Type 3 glyph cache.
const MAX_CACHED_TYPE3_IMAGE_BYTES: u64 = 64 * 1024 * 1024;

/// Convert one page's display list to a standalone SVG document.
///
/// `fonts` and `images` must be the same per-page caches the display list was
/// interpreted with (glyph/image ids index into them). Malformed commands are
/// skipped with a `tracing` diagnostic rather than failing the page, matching
/// render-backend behavior.
pub fn display_list_to_svg(
    display_list: &DisplayList,
    fonts: &FontCache,
    images: &ImageCache,
    options: &SvgOptions,
) -> String {
    let rect = display_list.page_rect;
    let (mut width, mut height) = (rect.width(), rect.height());
    if !(width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0) {
        tracing::warn!(?rect, "degenerate page rect; emitting 1x1 SVG");
        width = 1.0;
        height = 1.0;
    }

    let mut writer = SvgWriter::new(rect, width, height, fonts, images, options);

    if let Some(bg) = &options.background {
        writer.out.push_str("<rect width=\"");
        push_num(&mut writer.out, width);
        writer.out.push_str("\" height=\"");
        push_num(&mut writer.out, height);
        writer.out.push_str("\" fill=\"");
        writer.out.push_str(&hex_color(bg));
        writer.out.push_str("\"/>\n");
    }

    writer.emit_commands(&display_list.commands, 0);
    writer.flush_pending_backdrop();

    let mut svg = String::with_capacity(writer.defs.len() + writer.out.len() + 192);
    svg.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    svg.push_str("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"");
    push_num(&mut svg, width);
    svg.push_str("pt\" height=\"");
    push_num(&mut svg, height);
    svg.push_str("pt\" viewBox=\"0 0 ");
    push_num(&mut svg, width);
    svg.push(' ');
    push_num(&mut svg, height);
    svg.push_str("\">\n");
    if !writer.defs.is_empty() {
        svg.push_str("<defs>\n");
        svg.push_str(&writer.defs);
        svg.push_str("</defs>\n");
    }
    svg.push_str(&writer.out);
    svg.push_str("</svg>\n");
    svg
}

/// A structural frame opened by a Push* command.
///
/// Clips are NOT emitted as `<g clip-path>` wrappers: in CSS/SVG compositing a
/// `clip-path` (or `mask`) on a group creates an *isolated* group, so a blend
/// mode inside it would composite against an empty backdrop instead of the
/// page — PDF clips don't isolate. Instead the active clip is tracked as a
/// chain of `<clipPath>` defs (intersected via `clip-path` on the `clipPath`
/// element itself) and referenced from every painted element, which keeps
/// blend groups siblings of their true backdrop. Clip frames record the state
/// to restore on the matching pop.
enum Frame {
    Clip {
        prev_clip: Option<u32>,
        prev_mask: Option<u32>,
    },
    Blend {
        open: bool,
        isolates: bool,
        blend_mode: BlendMode,
        isolated: bool,
        alpha: f32,
        mask_id: Option<u32>,
        prev_out: String,
        prev_pending: Option<PendingBackdrop>,
    },
}

struct SvgWriter<'a> {
    defs: String,
    out: String,
    has_opaque_background: bool,
    blend_depth: usize,
    isolated_depth: usize,
    backdrop_snapshots: usize,
    /// Open batch of pairwise-disjoint translucent multi-pass tiling items
    /// sharing one base backdrop `<g id="b{bg_id}">` snapshot, flushed as a
    /// single premultiplied `R * w + B * (1 - w)` SVG filter composition.
    pending_backdrop: Option<PendingBackdrop>,
    x0: f64,
    y1: f64,
    /// Uniform coordinate scale from current PDF space (page or pattern cell)
    /// into SVG user units. Defaults to `1.0` on the page; scaled inside
    /// `<pattern>` defs whose pattern space uses tiny units with a large
    /// `/Matrix` so 3-decimal SVG formatting preserves geometry precision.
    scale: f64,
    page_width: f64,
    page_height: f64,
    fonts: &'a FontCache,
    images: &'a ImageCache,
    next_id: u32,
    /// Innermost active `<clipPath>` def id (already intersected with outer clips).
    clip: Option<u32>,
    /// Innermost active stroke-clip `<mask>` def id (from `PushClipStroke`).
    stroke_mask: Option<u32>,
    /// Wall-clock deadline for the whole export (shared by mask recursion).
    deadline: Option<zpdf_core::time::Instant>,
    /// Latches once `deadline` passes so the clock is read only occasionally.
    over_budget: bool,
    budget_checks: u32,
    warned_overprint: bool,
    warned_knockout: bool,
    warned_transfer: bool,
    /// `<image>` defs already written (`None`: the image cannot be encoded).
    /// Every later draw of the image is a `<use>` of its def, so a repeated
    /// image is PNG-encoded and embedded once, not once per draw.
    image_defs: HashMap<ImageDefKey, Option<u32>>,
    /// Soft-mask defs already written. A soft mask the interpreter reuses
    /// (one `gs` applied to many objects shares one mask list) is written
    /// once and referenced by every group it masks.
    mask_defs: HashMap<MaskDefKey, u32>,
    /// Tiling-pattern `<pattern>` defs already written, deduplicated per page.
    /// Retains each source `Arc<DisplayList>` so pointer keys never alias freed
    /// allocations (e.g. temporary lifted nested cells from `blend_segments()`).
    pattern_defs: HashMap<PatternDefKey, (std::sync::Arc<DisplayList>, u32)>,
    /// Per-export memo of `TilingPattern::is_backdrop_dependent()`, keeping each
    /// cell `Arc<DisplayList>` alive so pointer keys stay unique.
    tiling_backdrop_cache: HashMap<usize, (std::sync::Arc<DisplayList>, bool)>,
    /// Outlines of the page's glyphs, extracted once each
    /// (`MAX_CACHED_OUTLINE_COMMANDS`).
    glyph_outlines: HashMap<(FontId, u16), Option<Rc<GlyphOutline>>>,
    glyph_outline_commands: usize,
    /// Type 3 glyph programs, interpreted once each
    /// (`MAX_CACHED_TYPE3_COMMANDS`, `MAX_CACHED_TYPE3_IMAGE_BYTES`).
    type3_glyphs: HashMap<(FontId, u16), Option<Rc<Type3Glyph>>>,
    type3_glyph_commands: usize,
    type3_glyph_image_bytes: u64,
}

/// What an `<image>` def holds: a page image, or a Type 3 stencil glyph's
/// image tinted with the text colour (RGBA bits).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum ImageDefKey {
    Page(ImageId),
    Type3Stencil {
        font: FontId,
        glyph: u16,
        image: ImageId,
        tint: [u32; 4],
    },
}

/// Identity of a soft-mask def: the shared mask list plus the parameters
/// written around it, and the nesting depth it was written at (masks nested
/// past [`MAX_MASK_DEPTH`] are left out of it).
#[derive(PartialEq, Eq, Hash)]
struct MaskDefKey {
    commands: *const DisplayList,
    kind: SoftMaskKind,
    offset: [u32; 2],
    backdrop_luma: u32,
    origin_scale: [u64; 3],
    depth: u8,
}

/// Identity of a tiling-pattern `<pattern>` def.
#[derive(PartialEq, Eq, Hash)]
struct PatternDefKey {
    commands: *const DisplayList,
    segment_index: usize,
    overlap_shift: Option<(i64, i64)>,
    copy_blend_mode: u8,
    bbox: [u64; 4],
    steps: [u64; 2],
    tile_dims: [u64; 2],
    matrix: [u64; 6],
    depth: u8,
}

/// Cap on separate `(overlap_copy × blend_segment)` SVG passes for one
/// overlapping backdrop-dependent tiling fill or stroke before collapsing into
/// the order-preserving two-tile affine filter fallback.
const MAX_SVG_TILING_BLEND_PASSES: usize = 1024;
/// Cap on per-copy step evaluations inside the SVG affine tile collapse
/// (`f(0)`, `f(1)`, `a_tile`), matching the CPU/GPU affine tile budget.
const MAX_AFFINE_TILE_PASSES: usize = 4096;
/// Cap on sequential page-backdrop `<g id="b...">` snapshots when filter
/// batches reach [`MAX_BACKDROP_FILTER_PASSES`]. Normal draws and blend groups
/// fold directly into the open filter batch as `Overlay` / `BlendOverlay` steps,
/// keeping almost all pages at a single depth-1 snapshot.
const MAX_BACKDROP_SNAPSHOTS: usize = 64;
/// Cap on total filter steps batched inside a single SVG `<filter>` chain.
const MAX_BACKDROP_FILTER_PASSES: usize = 2048;

enum TilingPassPlan {
    Passes(Vec<(u32, BlendMode)>),
    Affine { k_id: u32, mk_id: u32, a_id: u32 },
}

enum BackdropStep {
    /// Intervening Normal-blended content wrapped in `<g id="ov{id}">` and
    /// composited with Porter-Duff `over` onto the running filter backdrop.
    Overlay(u32),
    /// Intervening non-Normal blended element/group wrapped in `<g id="ov{id}">`
    /// and composited via `<feBlend mode="..."/>` onto the running backdrop.
    BlendOverlay { ov_id: u32, blend_mode: BlendMode },
    /// Multi-pass tiling pattern evaluated against the running backdrop and
    /// mixed once through `<g id="s{shape_id}">`.
    MultiPass {
        passes: Vec<(u32, BlendMode)>,
        shape_id: u32,
    },
    /// Order-preserving affine collapse `f(B) = K + (f(1) - K) * B` evaluated
    /// from black-seeded `p{k_id}`, white-seeded `p{mk_id}`, and alpha-coverage
    /// `p{a_id}`, mixed once through `<g id="s{shape_id}">`.
    Affine {
        k_id: u32,
        mk_id: u32,
        a_id: u32,
        shape_id: u32,
    },
}

struct PendingBackdrop {
    bg_id: u32,
    steps: Vec<BackdropStep>,
    total_passes: usize,
}

struct TilingPatternGeometry {
    bx0: f64,
    by0: f64,
    bbox_x1: f64,
    bbox_y1: f64,
    x_step: f64,
    y_step: f64,
    abs_y: f64,
    cell_scale: f64,
    tile_w: f64,
    tile_h: f64,
    matrix: zpdf_core::Matrix,
}

/// An interpreted Type 3 glyph procedure.
struct Type3Glyph {
    list: DisplayList,
    /// Images the procedure draws (stencil glyphs), by the ids in `list`.
    images: ImageCache,
    font_matrix: [f64; 6],
}

impl<'a> SvgWriter<'a> {
    fn new(
        rect: zpdf_core::Rect,
        width: f64,
        height: f64,
        fonts: &'a FontCache,
        images: &'a ImageCache,
        options: &SvgOptions,
    ) -> Self {
        Self {
            defs: String::new(),
            out: String::new(),
            has_opaque_background: options.background.is_some_and(|c| c.a >= 1.0),
            blend_depth: 0,
            isolated_depth: 0,
            backdrop_snapshots: 0,
            pending_backdrop: None,
            x0: rect.x0,
            y1: rect.y1,
            scale: 1.0,
            page_width: width,
            page_height: height,
            fonts,
            images,
            next_id: 0,
            clip: None,
            stroke_mask: None,
            deadline: options.budget.map(|b| zpdf_core::time::Instant::now() + b),
            over_budget: false,
            budget_checks: 0,
            warned_overprint: false,
            warned_knockout: false,
            warned_transfer: false,
            image_defs: HashMap::new(),
            mask_defs: HashMap::new(),
            pattern_defs: HashMap::new(),
            tiling_backdrop_cache: HashMap::new(),
            glyph_outlines: HashMap::new(),
            glyph_outline_commands: 0,
            type3_glyphs: HashMap::new(),
            type3_glyph_commands: 0,
            type3_glyph_image_bytes: 0,
        }
    }

    fn fresh_id(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id
    }

    /// Current PDF space → SVG space: Y-flip with uniform coordinate `scale`.
    fn map(&self, x: f64, y: f64) -> (f64, f64) {
        ((x - self.x0) * self.scale, (self.y1 - y) * self.scale)
    }

    /// True once the export's wall-clock budget is spent. Latches; reads the
    /// clock only every few calls (matching the CPU backend's `over_budget`).
    fn check_over_budget(&mut self) -> bool {
        if self.over_budget {
            return true;
        }
        let Some(deadline) = self.deadline else {
            return false;
        };
        self.budget_checks = self.budget_checks.wrapping_add(1);
        if (self.budget_checks == 1 || self.budget_checks.is_multiple_of(64))
            && zpdf_core::time::Instant::now() >= deadline
        {
            self.over_budget = true;
            tracing::warn!("svg export exceeded time budget; output truncated");
        }
        self.over_budget
    }

    fn emit_commands(&mut self, commands: &[RenderCommand], depth: u8) {
        let mut frames: Vec<Frame> = Vec::new();
        for cmd in commands {
            if self.check_over_budget() {
                break;
            }
            match cmd {
                RenderCommand::FillPath {
                    path,
                    rule,
                    paint,
                    alpha,
                    overprint,
                } => {
                    if overprint.is_some() {
                        self.warn_overprint();
                    }
                    self.emit_fill(path, rule, paint, *alpha, depth);
                }
                RenderCommand::StrokePath {
                    path,
                    style,
                    paint,
                    alpha,
                    overprint,
                } => {
                    if overprint.is_some() {
                        self.warn_overprint();
                    }
                    self.emit_stroke(path, style, paint, *alpha, depth);
                }
                RenderCommand::DrawGlyphRun(run) => {
                    self.emit_glyph_run(run, depth);
                }
                RenderCommand::DrawImage(draw) => {
                    self.emit_image(draw);
                }
                RenderCommand::PushClip { path, rule } => {
                    let frame = Frame::Clip {
                        prev_clip: self.clip,
                        prev_mask: self.stroke_mask,
                    };
                    self.push_clip(path, rule);
                    frames.push(frame);
                }
                RenderCommand::PushClipStroke { path, style } => {
                    let frame = Frame::Clip {
                        prev_clip: self.clip,
                        prev_mask: self.stroke_mask,
                    };
                    self.push_clip_stroke(path, style);
                    frames.push(frame);
                }
                RenderCommand::PopClip => match frames.last() {
                    Some(Frame::Clip {
                        prev_clip,
                        prev_mask,
                    }) => {
                        self.clip = *prev_clip;
                        self.stroke_mask = *prev_mask;
                        frames.pop();
                    }
                    _ => tracing::warn!("unbalanced PopClip ignored"),
                },
                RenderCommand::PushBlendGroup {
                    blend_mode,
                    isolated,
                    knockout,
                    alpha,
                    mask,
                    ..
                } => {
                    if *knockout {
                        self.warn_knockout();
                    }
                    let eff_alpha = if alpha.is_finite() {
                        alpha.clamp(0.0, 1.0)
                    } else {
                        1.0
                    };
                    let mask_id = mask
                        .as_ref()
                        .and_then(|m| self.emit_soft_mask_def(m, depth));
                    let blend = blend_css(*blend_mode);
                    let isolates =
                        *isolated || eff_alpha < 1.0 || blend.is_some() || mask_id.is_some();
                    if !isolates {
                        // Full-opacity, non-isolated Normal group with no active soft mask
                        // is a transparent passthrough: keep the active canvas backdrop and
                        // `blend_depth` unchanged so nested translucent patterns preserve
                        // the live backdrop instead of isolating.
                        frames.push(Frame::Blend {
                            open: false,
                            isolates: false,
                            blend_mode: BlendMode::Normal,
                            isolated: false,
                            alpha: 1.0,
                            mask_id: None,
                            prev_out: String::new(),
                            prev_pending: None,
                        });
                    } else {
                        let prev_out = std::mem::take(&mut self.out);
                        let prev_pending = self.pending_backdrop.take();
                        self.blend_depth += 1;
                        self.isolated_depth += 1;
                        frames.push(Frame::Blend {
                            open: true,
                            isolates: true,
                            blend_mode: *blend_mode,
                            isolated: *isolated,
                            alpha: eff_alpha,
                            mask_id,
                            prev_out,
                            prev_pending,
                        });
                    }
                }
                RenderCommand::PopBlendGroup => match frames.last() {
                    Some(Frame::Blend { .. }) => {
                        if let Some(Frame::Blend {
                            open,
                            isolates,
                            blend_mode,
                            isolated,
                            alpha,
                            mask_id,
                            prev_out,
                            prev_pending,
                        }) = frames.pop()
                        {
                            self.close_blend_group(
                                open,
                                isolates,
                                blend_mode,
                                isolated,
                                alpha,
                                mask_id,
                                prev_out,
                                prev_pending,
                                depth,
                            );
                        }
                    }
                    _ => tracing::warn!("unbalanced PopBlendGroup ignored"),
                },
            }
        }
        // A malformed stream can leave frames unclosed; keep the SVG well-formed.
        for frame in frames.drain(..).rev() {
            match frame {
                Frame::Blend {
                    open,
                    isolates,
                    blend_mode,
                    isolated,
                    alpha,
                    mask_id,
                    prev_out,
                    prev_pending,
                } => {
                    self.close_blend_group(
                        open,
                        isolates,
                        blend_mode,
                        isolated,
                        alpha,
                        mask_id,
                        prev_out,
                        prev_pending,
                        depth,
                    );
                }
                Frame::Clip {
                    prev_clip,
                    prev_mask,
                } => {
                    self.clip = prev_clip;
                    self.stroke_mask = prev_mask;
                }
            }
        }
    }

    /// `clip-path`/`mask` attributes referencing the active clip chain,
    /// appended to every painted element (never to a wrapping `<g>`, which
    /// would create an isolated compositing group — see [`Frame`]).
    fn placement_attrs(&self) -> String {
        let mut s = String::new();
        if let Some(c) = self.clip {
            let _ = write!(s, " clip-path=\"url(#c{c})\"");
        }
        if let Some(m) = self.stroke_mask {
            let _ = write!(s, " mask=\"url(#m{m})\"");
        }
        s
    }

    // -- fills / strokes --

    /// True when the active SVG canvas backdrop is guaranteed opaque everywhere
    /// (every pixel has `alpha == 1.0`), so identity-backdrop associative tile
    /// collapsing (`#ffffff` for `Multiply`, `#000000` for `Screen`) cannot
    /// paint unpainted tile pixels onto a transparent canvas.
    fn opaque_backdrop(&self, depth: u8) -> bool {
        depth == 0 && self.has_opaque_background && self.isolated_depth == 0
    }

    /// Cached per-export check of `tiling.is_backdrop_dependent()`.
    fn tiling_is_backdrop_dependent(&mut self, tiling: &zpdf_display_list::TilingPattern) -> bool {
        let ptr = std::sync::Arc::as_ptr(&tiling.commands) as *const () as usize;
        if let Some(&(_, dep)) = self.tiling_backdrop_cache.get(&ptr) {
            return dep;
        }
        let dep = tiling.is_backdrop_dependent();
        self.tiling_backdrop_cache
            .insert(ptr, (std::sync::Arc::clone(&tiling.commands), dep));
        dep
    }

    fn compute_tiling_pattern_geometry(
        &self,
        tiling: &zpdf_display_list::TilingPattern,
    ) -> Option<TilingPatternGeometry> {
        let bbox = tiling.bbox.normalize();
        let (bx0, by0) = (bbox.x0, bbox.y0);
        let (bw, bh) = (bbox.width(), bbox.height());
        let (x_step, y_step) = (tiling.x_step, tiling.y_step);
        if ![bx0, by0, bw, bh, x_step, y_step]
            .iter()
            .all(|v| v.is_finite())
            || bw <= 0.0
            || bh <= 0.0
            || x_step == 0.0
            || y_step == 0.0
        {
            return None;
        }
        let abs_x = x_step.abs();
        let abs_y = y_step.abs();

        // Map current PDF space (page or enclosing pattern) to current SVG space,
        // compose with `tiling.matrix`, and choose a uniform `cell_scale` so
        // pattern-SVG units stay in a well-conditioned range for 3-decimal
        // `push_num` formatting even when pattern space uses tiny units with a
        // large `/Matrix`. Uniform scaling preserves anisotropic stroke widths
        // and circular caps/joins under `patternTransform`.
        let outer_pdf_to_outer_svg = zpdf_core::Matrix::new(
            self.scale,
            0.0,
            0.0,
            -self.scale,
            -self.x0 * self.scale,
            self.y1 * self.scale,
        );
        let pat_pdf_to_outer_svg = outer_pdf_to_outer_svg.concat(&tiling.matrix);
        let len_x = (pat_pdf_to_outer_svg.a * abs_x).hypot(pat_pdf_to_outer_svg.b * abs_x);
        let len_y = (pat_pdf_to_outer_svg.c * abs_y).hypot(pat_pdf_to_outer_svg.d * abs_y);
        if !len_x.is_finite() || !len_y.is_finite() || len_x <= 0.0 || len_y <= 0.0 {
            return None;
        }
        let mut cell_scale = (len_x / abs_x).max(len_y / abs_y);
        let min_step = abs_x.min(abs_y);
        let max_extent = bw.max(bh).max(abs_x).max(abs_y);
        if min_step * cell_scale < 1.0 {
            cell_scale = 1.0 / min_step;
        }
        if max_extent * cell_scale > 5e5 {
            cell_scale = 5e5 / max_extent;
        }
        if !cell_scale.is_finite() || cell_scale <= 0.0 {
            return None;
        }
        let tile_w = abs_x * cell_scale;
        let tile_h = abs_y * cell_scale;
        if !coord_ok(tile_w) || !coord_ok(tile_h) || tile_w <= 0.0 || tile_h <= 0.0 {
            return None;
        }

        let pat_svg_to_pat_pdf = zpdf_core::Matrix::new(
            1.0 / cell_scale,
            0.0,
            0.0,
            -1.0 / cell_scale,
            bx0,
            by0 + abs_y,
        );
        let matrix = pat_pdf_to_outer_svg.concat(&pat_svg_to_pat_pdf);
        if ![matrix.a, matrix.b, matrix.c, matrix.d, matrix.e, matrix.f]
            .iter()
            .all(|v| v.is_finite())
        {
            return None;
        }
        Some(TilingPatternGeometry {
            bx0,
            by0,
            bbox_x1: bbox.x1,
            bbox_y1: bbox.y1,
            x_step,
            y_step,
            abs_y,
            cell_scale,
            tile_w,
            tile_h,
            matrix,
        })
    }

    fn write_pattern_def(&mut self, id: u32, g: &TilingPatternGeometry, body: &str) {
        let _ = write!(
            self.defs,
            "<pattern id=\"p{id}\" patternUnits=\"userSpaceOnUse\" x=\"0\" y=\"0\" width=\""
        );
        push_num(&mut self.defs, g.tile_w);
        self.defs.push_str("\" height=\"");
        push_num(&mut self.defs, g.tile_h);
        self.defs.push_str("\" patternTransform=\"matrix(");
        push_num(&mut self.defs, g.matrix.a);
        self.defs.push(' ');
        push_num(&mut self.defs, g.matrix.b);
        self.defs.push(' ');
        push_num(&mut self.defs, g.matrix.c);
        self.defs.push(' ');
        push_num(&mut self.defs, g.matrix.d);
        self.defs.push(' ');
        push_num(&mut self.defs, g.matrix.e);
        self.defs.push(' ');
        push_num(&mut self.defs, g.matrix.f);
        self.defs.push_str(")\">\n");
        self.defs.push_str(body);
        self.defs.push_str("</pattern>\n");
    }

    /// Emit a [`TilingPattern`] (or one blend segment / overlap copy of it) as
    /// an SVG `<pattern>` def and return its id.
    #[allow(clippy::too_many_arguments)]
    fn emit_tiling_pattern_def_for(
        &mut self,
        tiling: &zpdf_display_list::TilingPattern,
        commands: &DisplayList,
        segment_index: usize,
        overlap_shift: Option<(i64, i64)>,
        copy_blend_mode: BlendMode,
        seg_bounds: Option<zpdf_core::Rect>,
        depth: u8,
    ) -> Option<u32> {
        if depth >= MAX_MASK_DEPTH || commands.commands.is_empty() {
            return None;
        }
        let g = self.compute_tiling_pattern_geometry(tiling)?;

        let combine_copies = overlap_shift.is_none()
            && matches!(
                copy_blend_mode,
                BlendMode::Multiply | BlendMode::Screen | BlendMode::Darken | BlendMode::Lighten
            );
        let copy_css = if combine_copies {
            blend_css(copy_blend_mode)
        } else {
            None
        };
        let key_mode = if copy_css.is_some() {
            copy_blend_mode as u8
        } else {
            BlendMode::Normal as u8
        };

        let key = PatternDefKey {
            commands: std::sync::Arc::as_ptr(&tiling.commands),
            segment_index,
            overlap_shift,
            copy_blend_mode: key_mode,
            bbox: [g.bx0, g.by0, g.bbox_x1, g.bbox_y1].map(f64::to_bits),
            steps: [g.x_step, g.y_step].map(f64::to_bits),
            tile_dims: [g.tile_w, g.tile_h].map(f64::to_bits),
            matrix: [
                g.matrix.a, g.matrix.b, g.matrix.c, g.matrix.d, g.matrix.e, g.matrix.f,
            ]
            .map(f64::to_bits),
            depth,
        };
        if let Some(&(_, id)) = self.pattern_defs.get(&key) {
            return Some(id);
        }
        let id = self.fresh_id();
        self.pattern_defs
            .insert(key, (std::sync::Arc::clone(&tiling.commands), id));

        let (di_range, dj_range) = match overlap_shift {
            Some((di, dj)) => (di..=di, dj..=dj),
            None => {
                let overlap = tiling.overlap_copy_ranges();
                if overlap.truncated {
                    tracing::warn!(
                        "svg tiling pattern overlapping cell copies exceed {}; clamping",
                        zpdf_display_list::MAX_TILING_OVERLAP_COPIES
                    );
                }
                (overlap.di, overlap.dj)
            }
        };

        let saved_out = std::mem::take(&mut self.out);
        let saved_pending = self.pending_backdrop.take();
        let saved_x0 = self.x0;
        let saved_y1 = self.y1;
        let saved_scale = self.scale;
        let saved_w = self.page_width;
        let saved_h = self.page_height;
        let saved_blend_depth = std::mem::replace(&mut self.blend_depth, 0);
        let saved_isolated_depth = std::mem::replace(&mut self.isolated_depth, 0);
        let (saved_clip, saved_mask) = (self.clip.take(), self.stroke_mask.take());
        self.scale = g.cell_scale;
        self.page_width = g.tile_w;
        self.page_height = g.tile_h;

        let abs_x = g.x_step.abs();
        let mut drew_any = false;
        for dj in dj_range {
            for di in di_range.clone() {
                if let Some(b) = seg_bounds {
                    let dx = di as f64 * g.x_step;
                    let dy = dj as f64 * g.y_step;
                    if b.x1 + dx <= g.bx0 + 1e-9
                        || b.x0 + dx >= g.bx0 + abs_x - 1e-9
                        || b.y1 + dy <= g.by0 + 1e-9
                        || b.y0 + dy >= g.by0 + g.abs_y - 1e-9
                    {
                        continue;
                    }
                }
                self.x0 = g.bx0 - di as f64 * g.x_step;
                self.y1 = g.by0 + g.abs_y - dj as f64 * g.y_step;
                if let Some(css_name) = copy_css.filter(|_| drew_any) {
                    let _ = writeln!(
                        self.out,
                        "<g style=\"mix-blend-mode:{css_name};isolation:isolate;\">"
                    );
                    self.emit_commands(&commands.commands, depth + 1);
                    self.out.push_str("</g>\n");
                } else {
                    self.emit_commands(&commands.commands, depth + 1);
                }
                drew_any = true;
            }
        }

        self.flush_pending_backdrop();
        let body = std::mem::replace(&mut self.out, saved_out);
        self.pending_backdrop = saved_pending;
        self.x0 = saved_x0;
        self.y1 = saved_y1;
        self.scale = saved_scale;
        self.page_width = saved_w;
        self.page_height = saved_h;
        self.blend_depth = saved_blend_depth;
        self.isolated_depth = saved_isolated_depth;
        self.clip = saved_clip;
        self.stroke_mask = saved_mask;
        self.write_pattern_def(id, &g, &body);
        Some(id)
    }

    /// Collapse an all-`Multiply` or all-`Screen` segmented tiling pattern
    /// (including overlapping copies) into ONE `<pattern>` seeded with the
    /// blend mode's algebraic identity (`#ffffff` for `Multiply`, `#000000`
    /// for `Screen`). Because `Multiply` and `Screen` are associative and
    /// affine in the backdrop, evaluating all marks onto the identity tile
    /// lets the caller paint a single `<path>` with `mix-blend-mode` and
    /// apply the object's constant opacity once (ISO 32000-1 §11.6.7).
    fn emit_collapsed_steps(
        &mut self,
        steps: &[zpdf_display_list::TilingBlendStep],
        identity: &str,
        css_name: &str,
        tile_w: f64,
        tile_h: f64,
        depth: u8,
    ) {
        for step in steps {
            match step {
                zpdf_display_list::TilingBlendStep::Segment(seg) => {
                    let _ = writeln!(
                        self.out,
                        "<g style=\"mix-blend-mode:{css_name};isolation:isolate;\">"
                    );
                    self.emit_commands(&seg.commands.commands, depth + 1);
                    self.out.push_str("</g>\n");
                }
                zpdf_display_list::TilingBlendStep::OpacityGroup {
                    alpha,
                    mask,
                    steps: inner,
                } => {
                    let a = if alpha.is_finite() {
                        alpha.clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    if a <= 0.0 {
                        continue;
                    }
                    let mask_id = mask.as_ref().map(|m_seg| {
                        let mid = self.fresh_id();
                        let saved_out = std::mem::take(&mut self.out);
                        self.emit_commands(&m_seg.commands.commands, depth + 1);
                        let mask_body = std::mem::replace(&mut self.out, saved_out);
                        let _ = write!(
                            self.defs,
                            "<mask id=\"m{mid}\" maskUnits=\"userSpaceOnUse\">\n{mask_body}</mask>\n"
                        );
                        mid
                    });
                    self.out.push_str("<g opacity=\"");
                    push_num(&mut self.out, a as f64);
                    if let Some(mid) = mask_id {
                        let _ = write!(self.out, "\" mask=\"url(#m{mid})");
                    }
                    let _ = writeln!(
                        self.out,
                        "\" style=\"mix-blend-mode:{css_name};isolation:isolate;\">"
                    );
                    self.out.push_str("<rect x=\"0\" y=\"0\" width=\"");
                    push_num(&mut self.out, tile_w);
                    self.out.push_str("\" height=\"");
                    push_num(&mut self.out, tile_h);
                    let _ = writeln!(self.out, "\" fill=\"{identity}\"/>");
                    self.emit_collapsed_steps(inner, identity, css_name, tile_w, tile_h, depth);
                    self.out.push_str("</g>\n");
                }
            }
        }
    }

    fn emit_tiling_pattern_def_collapsed(
        &mut self,
        tiling: &zpdf_display_list::TilingPattern,
        steps: &[zpdf_display_list::TilingBlendStep],
        mode: BlendMode,
        depth: u8,
    ) -> Option<u32> {
        if depth >= MAX_MASK_DEPTH || steps.is_empty() {
            return None;
        }
        let identity = match mode {
            BlendMode::Multiply => "#ffffff",
            BlendMode::Screen => "#000000",
            _ => return None,
        };
        let css_name = blend_css(mode)?;
        let g = self.compute_tiling_pattern_geometry(tiling)?;

        let key = PatternDefKey {
            commands: std::sync::Arc::as_ptr(&tiling.commands),
            segment_index: usize::MAX,
            overlap_shift: None,
            copy_blend_mode: mode as u8,
            bbox: [g.bx0, g.by0, g.bbox_x1, g.bbox_y1].map(f64::to_bits),
            steps: [g.x_step, g.y_step].map(f64::to_bits),
            tile_dims: [g.tile_w, g.tile_h].map(f64::to_bits),
            matrix: [
                g.matrix.a, g.matrix.b, g.matrix.c, g.matrix.d, g.matrix.e, g.matrix.f,
            ]
            .map(f64::to_bits),
            depth,
        };
        if let Some(&(_, id)) = self.pattern_defs.get(&key) {
            return Some(id);
        }
        let id = self.fresh_id();
        self.pattern_defs
            .insert(key, (std::sync::Arc::clone(&tiling.commands), id));

        let overlap = tiling.overlap_copy_ranges();
        if overlap.truncated {
            tracing::warn!(
                "svg tiling pattern overlapping cell copies exceed {}; clamping",
                zpdf_display_list::MAX_TILING_OVERLAP_COPIES
            );
        }

        let saved_out = std::mem::take(&mut self.out);
        let saved_pending = self.pending_backdrop.take();
        let saved_x0 = self.x0;
        let saved_y1 = self.y1;
        let saved_scale = self.scale;
        let saved_w = self.page_width;
        let saved_h = self.page_height;
        let saved_blend_depth = std::mem::replace(&mut self.blend_depth, 0);
        let saved_isolated_depth = std::mem::replace(&mut self.isolated_depth, 0);
        let (saved_clip, saved_mask) = (self.clip.take(), self.stroke_mask.take());
        self.scale = g.cell_scale;
        self.page_width = g.tile_w;
        self.page_height = g.tile_h;

        self.out.push_str("<rect x=\"0\" y=\"0\" width=\"");
        push_num(&mut self.out, g.tile_w);
        self.out.push_str("\" height=\"");
        push_num(&mut self.out, g.tile_h);
        let _ = writeln!(self.out, "\" fill=\"{identity}\"/>");

        for dj in overlap.dj.clone() {
            for di in overlap.di.clone() {
                if !zpdf_display_list::TilingBlendStep::any_intersects_period(steps, tiling, di, dj)
                {
                    continue;
                }
                self.x0 = g.bx0 - di as f64 * g.x_step;
                self.y1 = g.by0 + g.abs_y - dj as f64 * g.y_step;
                self.emit_collapsed_steps(steps, identity, css_name, g.tile_w, g.tile_h, depth);
            }
        }

        self.flush_pending_backdrop();
        let body = std::mem::replace(&mut self.out, saved_out);
        self.pending_backdrop = saved_pending;
        self.x0 = saved_x0;
        self.y1 = saved_y1;
        self.scale = saved_scale;
        self.page_width = saved_w;
        self.page_height = saved_h;
        self.blend_depth = saved_blend_depth;
        self.isolated_depth = saved_isolated_depth;
        self.clip = saved_clip;
        self.stroke_mask = saved_mask;
        self.write_pattern_def(id, &g, &body);
        Some(id)
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_affine_steps(
        &mut self,
        steps: &[zpdf_display_list::TilingBlendStep],
        tiling: &zpdf_display_list::TilingPattern,
        di: i64,
        dj: i64,
        alpha_only: bool,
        remaining: &mut usize,
        depth: u8,
    ) {
        for step in steps {
            if *remaining == 0 || self.check_over_budget() {
                break;
            }
            match step {
                zpdf_display_list::TilingBlendStep::Segment(seg) => {
                    if !seg.intersects_period(tiling, di, dj) {
                        continue;
                    }
                    *remaining -= 1;
                    if alpha_only {
                        self.emit_commands(&seg.commands.commands, depth + 1);
                    } else if let Some(css_name) = blend_css(seg.blend_mode) {
                        let _ = writeln!(
                            self.out,
                            "<g style=\"mix-blend-mode:{css_name};isolation:isolate;\">"
                        );
                        self.emit_commands(&seg.commands.commands, depth + 1);
                        self.out.push_str("</g>\n");
                    } else {
                        self.emit_commands(&seg.commands.commands, depth + 1);
                    }
                }
                zpdf_display_list::TilingBlendStep::OpacityGroup {
                    alpha,
                    mask,
                    steps: inner,
                } => {
                    if !zpdf_display_list::TilingBlendStep::any_intersects_period(
                        inner, tiling, di, dj,
                    ) {
                        continue;
                    }
                    let a = if alpha.is_finite() {
                        alpha.clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    if a <= 0.0 {
                        continue;
                    }
                    if alpha_only {
                        let mask_id = mask.as_ref().map(|m_seg| {
                            let mid = self.fresh_id();
                            let saved_out = std::mem::take(&mut self.out);
                            self.emit_commands(&m_seg.commands.commands, depth + 1);
                            let mask_body = std::mem::replace(&mut self.out, saved_out);
                            let _ = write!(
                                self.defs,
                                "<mask id=\"m{mid}\" maskUnits=\"userSpaceOnUse\">\n{mask_body}</mask>\n"
                            );
                            mid
                        });
                        self.out.push_str("<g opacity=\"");
                        push_num(&mut self.out, a as f64);
                        if let Some(mid) = mask_id {
                            let _ = write!(self.out, "\" mask=\"url(#m{mid})");
                        }
                        self.out.push_str("\">\n");
                        self.emit_affine_steps(inner, tiling, di, dj, true, remaining, depth);
                        self.out.push_str("</g>\n");
                    } else {
                        self.emit_affine_steps(inner, tiling, di, dj, false, remaining, depth);
                    }
                }
            }
        }
    }

    /// Order-preserving bounded fallback for overlapping backdrop-dependent
    /// cells exceeding [`MAX_SVG_TILING_BLEND_PASSES`]: evaluate all `(dj, di)`
    /// overlap copies in exact copy order and all intersecting steps in exact
    /// draw order onto black-seeded (`p_k = f(0)`), white-seeded (`p_mk = f(1)`),
    /// and transparent coverage (`p_a`) `<pattern>` defs so the page `<filter>`
    /// computes `f(B) = K + (f(1) - K) * B` without reordering copies or steps.
    fn emit_tiling_pattern_def_affine(
        &mut self,
        tiling: &zpdf_display_list::TilingPattern,
        steps: &[zpdf_display_list::TilingBlendStep],
        depth: u8,
    ) -> Option<(u32, u32, u32)> {
        if depth >= MAX_MASK_DEPTH || steps.is_empty() {
            return None;
        }
        let g = self.compute_tiling_pattern_geometry(tiling)?;

        let make_key = |segment_index: usize| PatternDefKey {
            commands: std::sync::Arc::as_ptr(&tiling.commands),
            segment_index,
            overlap_shift: None,
            copy_blend_mode: BlendMode::Normal as u8,
            bbox: [g.bx0, g.by0, g.bbox_x1, g.bbox_y1].map(f64::to_bits),
            steps: [g.x_step, g.y_step].map(f64::to_bits),
            tile_dims: [g.tile_w, g.tile_h].map(f64::to_bits),
            matrix: [
                g.matrix.a, g.matrix.b, g.matrix.c, g.matrix.d, g.matrix.e, g.matrix.f,
            ]
            .map(f64::to_bits),
            depth,
        };
        let key_k = make_key(usize::MAX - 1);
        let key_mk = make_key(usize::MAX - 2);
        let key_a = make_key(usize::MAX - 3);
        if let (Some(&(_, k_id)), Some(&(_, mk_id)), Some(&(_, a_id))) = (
            self.pattern_defs.get(&key_k),
            self.pattern_defs.get(&key_mk),
            self.pattern_defs.get(&key_a),
        ) {
            return Some((k_id, mk_id, a_id));
        }

        let k_id = self.fresh_id();
        let mk_id = self.fresh_id();
        let a_id = self.fresh_id();
        self.pattern_defs
            .insert(key_k, (std::sync::Arc::clone(&tiling.commands), k_id));
        self.pattern_defs
            .insert(key_mk, (std::sync::Arc::clone(&tiling.commands), mk_id));
        self.pattern_defs
            .insert(key_a, (std::sync::Arc::clone(&tiling.commands), a_id));

        let overlap = tiling.overlap_copy_ranges();
        let saved_out = std::mem::take(&mut self.out);
        let saved_pending = self.pending_backdrop.take();
        let saved_x0 = self.x0;
        let saved_y1 = self.y1;
        let saved_scale = self.scale;
        let saved_w = self.page_width;
        let saved_h = self.page_height;
        let saved_blend_depth = std::mem::replace(&mut self.blend_depth, 0);
        let saved_isolated_depth = std::mem::replace(&mut self.isolated_depth, 0);
        let (saved_clip, saved_mask) = (self.clip.take(), self.stroke_mask.take());
        self.scale = g.cell_scale;
        self.page_width = g.tile_w;
        self.page_height = g.tile_h;

        let mut remaining = MAX_AFFINE_TILE_PASSES;
        for dj in overlap.dj.clone() {
            for di in overlap.di.clone() {
                if remaining == 0 || self.check_over_budget() {
                    break;
                }
                if !zpdf_display_list::TilingBlendStep::any_intersects_period(steps, tiling, di, dj)
                {
                    continue;
                }
                self.x0 = g.bx0 - di as f64 * g.x_step;
                self.y1 = g.by0 + g.abs_y - dj as f64 * g.y_step;
                self.emit_affine_steps(steps, tiling, di, dj, false, &mut remaining, depth);
            }
        }
        self.flush_pending_backdrop();
        let blended_steps = std::mem::take(&mut self.out);

        let mut remaining_a = MAX_AFFINE_TILE_PASSES;
        for dj in overlap.dj.clone() {
            for di in overlap.di.clone() {
                if remaining_a == 0 || self.check_over_budget() {
                    break;
                }
                if !zpdf_display_list::TilingBlendStep::any_intersects_period(steps, tiling, di, dj)
                {
                    continue;
                }
                self.x0 = g.bx0 - di as f64 * g.x_step;
                self.y1 = g.by0 + g.abs_y - dj as f64 * g.y_step;
                self.emit_affine_steps(steps, tiling, di, dj, true, &mut remaining_a, depth);
            }
        }
        self.flush_pending_backdrop();
        let body_a = std::mem::replace(&mut self.out, saved_out);
        self.pending_backdrop = saved_pending;
        self.x0 = saved_x0;
        self.y1 = saved_y1;
        self.scale = saved_scale;
        self.page_width = saved_w;
        self.page_height = saved_h;
        self.blend_depth = saved_blend_depth;
        self.isolated_depth = saved_isolated_depth;
        self.clip = saved_clip;
        self.stroke_mask = saved_mask;

        let mut seed_prefix = String::from("<rect x=\"0\" y=\"0\" width=\"");
        push_num(&mut seed_prefix, g.tile_w);
        seed_prefix.push_str("\" height=\"");
        push_num(&mut seed_prefix, g.tile_h);
        let mut body_k = String::with_capacity(seed_prefix.len() + blended_steps.len() + 24);
        body_k.push_str(&seed_prefix);
        body_k.push_str("\" fill=\"#000000\"/>\n");
        body_k.push_str(&blended_steps);

        let mut body_mk = String::with_capacity(seed_prefix.len() + blended_steps.len() + 24);
        body_mk.push_str(&seed_prefix);
        body_mk.push_str("\" fill=\"#ffffff\"/>\n");
        body_mk.push_str(&blended_steps);

        self.write_pattern_def(k_id, &g, &body_k);
        self.write_pattern_def(mk_id, &g, &body_mk);
        self.write_pattern_def(a_id, &g, &body_a);
        Some((k_id, mk_id, a_id))
    }

    /// Resolve a tiling pattern into either per-segment `(pattern_id, BlendMode)`
    /// passes or an order-preserving affine collapse `(k_id, mk_id, a_id)` when
    /// overlapping copies exceed [`MAX_SVG_TILING_BLEND_PASSES`].
    fn emit_tiling_plan(
        &mut self,
        tiling: &zpdf_display_list::TilingPattern,
        depth: u8,
    ) -> TilingPassPlan {
        if self.tiling_is_backdrop_dependent(tiling) {
            if let Some(steps) = tiling.blend_steps() {
                let overlap = tiling.overlap_copy_ranges();
                let overlap_nx = overlap
                    .di
                    .end()
                    .saturating_sub(*overlap.di.start())
                    .saturating_add(1)
                    .max(0) as usize;
                let overlap_ny = overlap
                    .dj
                    .end()
                    .saturating_sub(*overlap.dj.start())
                    .saturating_add(1)
                    .max(0) as usize;
                let overlap_count = overlap_nx.saturating_mul(overlap_ny);
                let total_segs = zpdf_display_list::TilingBlendStep::count_segments(&steps);
                let has_opacity_group = steps
                    .iter()
                    .any(|s| matches!(s, zpdf_display_list::TilingBlendStep::OpacityGroup { .. }));

                let uniform_assoc = zpdf_display_list::TilingBlendStep::uniform_blend_mode(&steps)
                    .filter(|&m| matches!(m, BlendMode::Multiply | BlendMode::Screen));
                if (total_segs > 1 || overlap_count > 1 || has_opacity_group)
                    && self.opaque_backdrop(depth)
                {
                    if let Some(mode) = uniform_assoc {
                        if let Some(id) =
                            self.emit_tiling_pattern_def_collapsed(tiling, &steps, mode, depth)
                        {
                            return TilingPassPlan::Passes(vec![(id, mode)]);
                        }
                    }
                }

                // Collect flat segments when there are no nested OpacityGroups.
                let mut flat_segments = Vec::with_capacity(total_segs);
                let mut all_flat = true;
                for step in &steps {
                    match step {
                        zpdf_display_list::TilingBlendStep::Segment(seg) => {
                            flat_segments.push(seg);
                        }
                        zpdf_display_list::TilingBlendStep::OpacityGroup { .. } => {
                            all_flat = false;
                            break;
                        }
                    }
                }
                if all_flat {
                    let mut passes = Vec::new();
                    let separate_copies = if overlap_count > 1 {
                        let mut total_passes = 0usize;
                        'count: for dj in overlap.dj.clone() {
                            for di in overlap.di.clone() {
                                total_passes = total_passes.saturating_add(
                                    zpdf_display_list::TilingBlendStep::count_intersecting_passes(
                                        &steps, tiling, di, dj,
                                    ),
                                );
                                if total_passes > MAX_SVG_TILING_BLEND_PASSES {
                                    break 'count;
                                }
                            }
                        }
                        if total_passes > MAX_SVG_TILING_BLEND_PASSES {
                            if depth == 0 {
                                if let Some((k_id, mk_id, a_id)) =
                                    self.emit_tiling_pattern_def_affine(tiling, &steps, depth)
                                {
                                    return TilingPassPlan::Affine { k_id, mk_id, a_id };
                                }
                            }
                            tracing::warn!(
                                total_passes,
                                limit = MAX_SVG_TILING_BLEND_PASSES,
                                "svg tiling blend plan exceeds pass budget; flattening overlap copies per step"
                            );
                            false
                        } else {
                            true
                        }
                    } else {
                        false
                    };
                    let mut remaining = MAX_SVG_TILING_BLEND_PASSES;
                    if separate_copies {
                        'copies: for dj in overlap.dj.clone() {
                            for di in overlap.di.clone() {
                                for seg in &flat_segments {
                                    if remaining == 0 {
                                        break 'copies;
                                    }
                                    if !seg.intersects_period(tiling, di, dj) {
                                        continue;
                                    }
                                    remaining -= 1;
                                    if let Some(id) = self.emit_tiling_pattern_def_for(
                                        tiling,
                                        &seg.commands,
                                        seg.segment_id,
                                        Some((di, dj)),
                                        BlendMode::Normal,
                                        seg.bounds,
                                        depth,
                                    ) {
                                        passes.push((id, seg.blend_mode));
                                    }
                                }
                            }
                        }
                    } else {
                        for seg in &flat_segments {
                            if remaining == 0 {
                                break;
                            }
                            remaining -= 1;
                            if let Some(id) = self.emit_tiling_pattern_def_for(
                                tiling,
                                &seg.commands,
                                seg.segment_id,
                                None,
                                seg.blend_mode,
                                seg.bounds,
                                depth,
                            ) {
                                passes.push((id, seg.blend_mode));
                            }
                        }
                    }
                    return TilingPassPlan::Passes(passes);
                }
            }
            tracing::warn!(
                "svg tiling pattern has unsegmentable backdrop-dependent marks; falling back to isolated pattern"
            );
        }
        TilingPassPlan::Passes(
            self.emit_tiling_pattern_def_for(
                tiling,
                &tiling.commands,
                0,
                None,
                BlendMode::Normal,
                None,
                depth,
            )
            .map(|id| vec![(id, BlendMode::Normal)])
            .unwrap_or_default(),
        )
    }

    /// Flush any open batch of backdrop-dependent filter steps into `self.out`
    /// (preserving any trailing elements already accumulated in `self.out` in
    /// exact painter's order). References `#b{bg_id}` only ONCE (zero
    /// exponential `<use>` expansion).
    fn flush_pending_backdrop(&mut self) {
        let Some(pending) = self.pending_backdrop.take() else {
            return;
        };
        let trailing = std::mem::take(&mut self.out);
        if pending.steps.is_empty() {
            let _ = writeln!(self.out, "<use href=\"#b{}\"/>", pending.bg_id);
            self.out.push_str(&trailing);
            return;
        }
        let w = self.page_width;
        let h = self.page_height;

        // Emit one reusable `<rect id="pr{rid}" ... fill="url(#p{pat_id})"/>`
        // per distinct pattern id referenced in this filter batch.
        let mut pat_rects: HashMap<u32, u32> = HashMap::new();
        let mut register_pat = |defs: &mut String, next_id: &mut u32, pat_id: u32| {
            if let std::collections::hash_map::Entry::Vacant(e) = pat_rects.entry(pat_id) {
                *next_id += 1;
                let rid = *next_id;
                let _ = write!(defs, "<rect id=\"pr{rid}\" x=\"0\" y=\"0\" width=\"");
                push_num(defs, w);
                defs.push_str("\" height=\"");
                push_num(defs, h);
                let _ = writeln!(defs, "\" fill=\"url(#p{pat_id})\"/>");
                e.insert(rid);
            }
        };
        for step in &pending.steps {
            match step {
                BackdropStep::Overlay(_) | BackdropStep::BlendOverlay { .. } => {}
                BackdropStep::MultiPass { passes, .. } => {
                    for &(pat_id, _) in passes {
                        register_pat(&mut self.defs, &mut self.next_id, pat_id);
                    }
                }
                BackdropStep::Affine {
                    k_id, mk_id, a_id, ..
                } => {
                    register_pat(&mut self.defs, &mut self.next_id, *k_id);
                    register_pat(&mut self.defs, &mut self.next_id, *mk_id);
                    register_pat(&mut self.defs, &mut self.next_id, *a_id);
                }
            }
        }

        let fid = self.fresh_id();
        let _ = write!(
            self.defs,
            "<filter id=\"f{fid}\" filterUnits=\"userSpaceOnUse\" x=\"0\" y=\"0\" width=\""
        );
        push_num(&mut self.defs, w);
        self.defs.push_str("\" height=\"");
        push_num(&mut self.defs, h);
        self.defs
            .push_str("\" color-interpolation-filters=\"sRGB\">\n");

        let mut cur = String::from("SourceGraphic");
        for (idx, step) in pending.steps.iter().enumerate() {
            match step {
                BackdropStep::Overlay(ov_id) => {
                    let ov_res = format!("ov{idx}");
                    let next_cur = format!("c{idx}");
                    let _ = write!(
                        self.defs,
                        "<feImage href=\"#ov{ov_id}\" x=\"0\" y=\"0\" width=\""
                    );
                    push_num(&mut self.defs, w);
                    self.defs.push_str("\" height=\"");
                    push_num(&mut self.defs, h);
                    let _ = writeln!(self.defs, "\" result=\"{ov_res}\"/>");
                    let _ = writeln!(
                        self.defs,
                        "<feComposite in=\"{ov_res}\" in2=\"{cur}\" operator=\"over\" result=\"{next_cur}\"/>"
                    );
                    cur = next_cur;
                }
                BackdropStep::BlendOverlay { ov_id, blend_mode } => {
                    let ov_res = format!("ov{idx}");
                    let next_cur = format!("c{idx}");
                    let _ = write!(
                        self.defs,
                        "<feImage href=\"#ov{ov_id}\" x=\"0\" y=\"0\" width=\""
                    );
                    push_num(&mut self.defs, w);
                    self.defs.push_str("\" height=\"");
                    push_num(&mut self.defs, h);
                    let _ = writeln!(self.defs, "\" result=\"{ov_res}\"/>");
                    if let Some(name) = blend_css(*blend_mode) {
                        let _ = writeln!(
                            self.defs,
                            "<feBlend in=\"{ov_res}\" in2=\"{cur}\" mode=\"{name}\" result=\"{next_cur}\"/>"
                        );
                    } else {
                        let _ = writeln!(
                            self.defs,
                            "<feComposite in=\"{ov_res}\" in2=\"{cur}\" operator=\"over\" result=\"{next_cur}\"/>"
                        );
                    }
                    cur = next_cur;
                }
                BackdropStep::MultiPass { passes, shape_id } => {
                    let mut acc = cur.clone();
                    for (p_idx, &(pat_id, blend_mode)) in passes.iter().enumerate() {
                        let rid = pat_rects[&pat_id];
                        let pat_res = format!("p{idx}_{p_idx}");
                        let next_acc = format!("a{idx}_{p_idx}");
                        let _ = write!(
                            self.defs,
                            "<feImage href=\"#pr{rid}\" x=\"0\" y=\"0\" width=\""
                        );
                        push_num(&mut self.defs, w);
                        self.defs.push_str("\" height=\"");
                        push_num(&mut self.defs, h);
                        let _ = writeln!(self.defs, "\" result=\"{pat_res}\"/>");
                        if let Some(name) = blend_css(blend_mode) {
                            let _ = writeln!(
                                self.defs,
                                "<feBlend in=\"{pat_res}\" in2=\"{acc}\" mode=\"{name}\" result=\"{next_acc}\"/>"
                            );
                        } else {
                            let _ = writeln!(
                                self.defs,
                                "<feComposite in=\"{pat_res}\" in2=\"{acc}\" operator=\"over\" result=\"{next_acc}\"/>"
                            );
                        }
                        acc = next_acc;
                    }
                    let wt_res = format!("w{idx}");
                    let rw_res = format!("rw{idx}");
                    let bw_res = format!("bw{idx}");
                    let next_cur = format!("c{idx}");
                    let _ = write!(
                        self.defs,
                        "<feImage href=\"#s{shape_id}\" x=\"0\" y=\"0\" width=\""
                    );
                    push_num(&mut self.defs, w);
                    self.defs.push_str("\" height=\"");
                    push_num(&mut self.defs, h);
                    let _ = writeln!(self.defs, "\" result=\"{wt_res}\"/>");
                    let _ = writeln!(
                        self.defs,
                        "<feComposite in=\"{acc}\" in2=\"{wt_res}\" operator=\"in\" result=\"{rw_res}\"/>"
                    );
                    let _ = writeln!(
                        self.defs,
                        "<feComposite in=\"{cur}\" in2=\"{wt_res}\" operator=\"out\" result=\"{bw_res}\"/>"
                    );
                    let _ = writeln!(
                        self.defs,
                        "<feComposite in=\"{rw_res}\" in2=\"{bw_res}\" operator=\"arithmetic\" k1=\"0\" k2=\"1\" k3=\"1\" k4=\"0\" result=\"{next_cur}\"/>"
                    );
                    cur = next_cur;
                }
                BackdropStep::Affine {
                    k_id,
                    mk_id,
                    a_id,
                    shape_id,
                } => {
                    let rk = pat_rects[k_id];
                    let rmk = pat_rects[mk_id];
                    let ra = pat_rects[a_id];
                    let pk = format!("pk{idx}");
                    let pmk = format!("pmk{idx}");
                    let pa = format!("pa{idx}");
                    let pkinv = format!("pki{idx}");
                    let pm = format!("pm{idx}");
                    let mb = format!("mb{idx}");
                    let res = format!("ar{idx}");
                    let panz = format!("pan{idx}");
                    let wt_res = format!("w{idx}");
                    let wte_res = format!("we{idx}");
                    let rw_res = format!("rw{idx}");
                    let bw_res = format!("bw{idx}");
                    let next_cur = format!("c{idx}");
                    for (rid, out_name) in [(rk, &pk), (rmk, &pmk), (ra, &pa)] {
                        let _ = write!(
                            self.defs,
                            "<feImage href=\"#pr{rid}\" x=\"0\" y=\"0\" width=\""
                        );
                        push_num(&mut self.defs, w);
                        self.defs.push_str("\" height=\"");
                        push_num(&mut self.defs, h);
                        let _ = writeln!(self.defs, "\" result=\"{out_name}\"/>");
                    }
                    // Keep alpha = 1.0 throughout M = (M + K) + (1 - K) - 1 so
                    // premultiplied filter buffers never clamp RGB against alpha = 0.
                    let _ = writeln!(
                        self.defs,
                        "<feComponentTransfer in=\"{pk}\" result=\"{pkinv}\">\
                         <feFuncR type=\"linear\" slope=\"-1\" intercept=\"1\"/>\
                         <feFuncG type=\"linear\" slope=\"-1\" intercept=\"1\"/>\
                         <feFuncB type=\"linear\" slope=\"-1\" intercept=\"1\"/>\
                         </feComponentTransfer>"
                    );
                    let _ = writeln!(
                        self.defs,
                        "<feComposite in=\"{pmk}\" in2=\"{pkinv}\" operator=\"arithmetic\" k1=\"0\" k2=\"1\" k3=\"1\" k4=\"-1\" result=\"{pm}\"/>"
                    );
                    let _ = writeln!(
                        self.defs,
                        "<feComposite in=\"{pm}\" in2=\"{cur}\" operator=\"arithmetic\" k1=\"1\" k2=\"0\" k3=\"0\" k4=\"0\" result=\"{mb}\"/>"
                    );
                    let _ = writeln!(
                        self.defs,
                        "<feComposite in=\"{mb}\" in2=\"{pk}\" operator=\"arithmetic\" k1=\"0\" k2=\"1\" k3=\"1\" k4=\"0\" result=\"{res}\"/>"
                    );
                    // Convert cell coverage alpha into a binary painted-pixel mask so
                    // unpainted gaps (pa.a == 0) leave `cur` untouched while painted
                    // pixels preserve `res` without double-attenuating semi-transparent marks.
                    let _ = writeln!(
                        self.defs,
                        "<feComponentTransfer in=\"{pa}\" result=\"{panz}\">\
                         <feFuncA type=\"linear\" slope=\"255\" intercept=\"0\"/>\
                         </feComponentTransfer>"
                    );
                    let _ = write!(
                        self.defs,
                        "<feImage href=\"#s{shape_id}\" x=\"0\" y=\"0\" width=\""
                    );
                    push_num(&mut self.defs, w);
                    self.defs.push_str("\" height=\"");
                    push_num(&mut self.defs, h);
                    let _ = writeln!(self.defs, "\" result=\"{wt_res}\"/>");
                    let _ = writeln!(
                        self.defs,
                        "<feComposite in=\"{wt_res}\" in2=\"{panz}\" operator=\"in\" result=\"{wte_res}\"/>"
                    );
                    let _ = writeln!(
                        self.defs,
                        "<feComposite in=\"{res}\" in2=\"{wte_res}\" operator=\"in\" result=\"{rw_res}\"/>"
                    );
                    let _ = writeln!(
                        self.defs,
                        "<feComposite in=\"{cur}\" in2=\"{wte_res}\" operator=\"out\" result=\"{bw_res}\"/>"
                    );
                    let _ = writeln!(
                        self.defs,
                        "<feComposite in=\"{rw_res}\" in2=\"{bw_res}\" operator=\"arithmetic\" k1=\"0\" k2=\"1\" k3=\"1\" k4=\"0\" result=\"{next_cur}\"/>"
                    );
                    cur = next_cur;
                }
            }
        }

        self.defs.push_str("</filter>\n");
        let _ = writeln!(
            self.out,
            "<use href=\"#b{}\" filter=\"url(#f{fid})\"/>",
            pending.bg_id
        );
        self.out.push_str(&trailing);
    }

    /// Snapshot the current scope's accumulated content into `<defs><g id="b{id}">`
    /// without re-emitting `<use>` into `self.out` (the caller's filter composition
    /// emits the sole `<use href="#b{id}" filter="..."/>` when flushed).
    fn snapshot_page_backdrop(&mut self, allow_empty: bool) -> Option<u32> {
        if self.out.is_empty() && !allow_empty {
            return None;
        }
        self.backdrop_snapshots += 1;
        let prior = std::mem::take(&mut self.out);
        let bg_id = self.fresh_id();
        if prior.is_empty() {
            let _ = write!(
                self.defs,
                "<g id=\"b{bg_id}\"><rect x=\"0\" y=\"0\" width=\""
            );
            push_num(&mut self.defs, self.page_width);
            self.defs.push_str("\" height=\"");
            push_num(&mut self.defs, self.page_height);
            self.defs
                .push_str("\" fill=\"#000000\" fill-opacity=\"0\"/></g>\n");
        } else {
            let _ = write!(self.defs, "<g id=\"b{bg_id}\">\n{prior}</g>\n");
        }
        Some(bg_id)
    }

    /// Fold any intervening Normal-blended content in `self.out` into an
    /// `Overlay` step on the currently open `pending_backdrop` so subsequent
    /// backdrop steps see it without starting a new snapshot.
    fn absorb_out_into_pending(&mut self) {
        if self.pending_backdrop.is_some() && !self.out.is_empty() {
            let mid = std::mem::take(&mut self.out);
            let ov_id = self.fresh_id();
            let _ = write!(self.defs, "<g id=\"ov{ov_id}\">\n{mid}</g>\n");
            if let Some(ref mut p) = self.pending_backdrop {
                p.steps.push(BackdropStep::Overlay(ov_id));
                p.total_passes = p.total_passes.saturating_add(1);
            }
        }
    }

    /// Enqueue one backdrop-dependent filter step onto the active linear
    /// `<filter>` chain, absorbing intervening normal draws as `Overlay` steps
    /// and starting a new snapshot only when needed within
    /// [`MAX_BACKDROP_SNAPSHOTS`].
    fn enqueue_backdrop_step(
        &mut self,
        step: BackdropStep,
        step_cost: usize,
        allow_empty: bool,
    ) -> bool {
        if let Some(ref p) = self.pending_backdrop {
            let extra = usize::from(!self.out.is_empty());
            if p.total_passes
                .saturating_add(extra)
                .saturating_add(step_cost)
                <= MAX_BACKDROP_FILTER_PASSES
            {
                self.absorb_out_into_pending();
                if let Some(ref mut p) = self.pending_backdrop {
                    p.total_passes = p.total_passes.saturating_add(step_cost);
                    p.steps.push(step);
                }
                return true;
            }
            self.flush_pending_backdrop();
        }
        if (allow_empty || !self.out.is_empty())
            && self.backdrop_snapshots < MAX_BACKDROP_SNAPSHOTS
            && step_cost <= MAX_BACKDROP_FILTER_PASSES
        {
            if let Some(bg_id) = self.snapshot_page_backdrop(allow_empty) {
                self.pending_backdrop = Some(PendingBackdrop {
                    bg_id,
                    steps: vec![step],
                    total_passes: step_cost,
                });
                return true;
            }
        }
        false
    }

    fn emit_tiling_fill_path_data(
        &mut self,
        d: &str,
        rule: &FillRule,
        tiling: &zpdf_display_list::TilingPattern,
        opacity: f32,
        depth: u8,
    ) {
        let plan = self.emit_tiling_plan(tiling, depth);
        match plan {
            TilingPassPlan::Affine { k_id, mk_id, a_id } => {
                if depth == 0 {
                    let shape_id = self.fresh_id();
                    let placement = self.placement_attrs();
                    let _ = write!(
                        self.defs,
                        "<g id=\"s{shape_id}\"><path d=\"{d}\" fill=\"#ffffff\""
                    );
                    if *rule == FillRule::EvenOdd {
                        self.defs.push_str(" fill-rule=\"evenodd\"");
                    }
                    push_opacity(&mut self.defs, "fill-opacity", opacity);
                    self.defs.push_str(&placement);
                    self.defs.push_str("/></g>\n");
                    if self.enqueue_backdrop_step(
                        BackdropStep::Affine {
                            k_id,
                            mk_id,
                            a_id,
                            shape_id,
                        },
                        4,
                        true,
                    ) {
                        return;
                    }
                    self.flush_pending_backdrop();
                }
                let placement = self.placement_attrs();
                let _ = write!(self.out, "<path d=\"{d}\" fill=\"url(#p{k_id})\"");
                if *rule == FillRule::EvenOdd {
                    self.out.push_str(" fill-rule=\"evenodd\"");
                }
                push_opacity(&mut self.out, "fill-opacity", opacity);
                self.out.push_str(&placement);
                self.out.push_str("/>\n");
            }
            TilingPassPlan::Passes(passes) => {
                if passes.is_empty() {
                    return;
                }
                let has_non_normal = passes.iter().any(|&(_, m)| m != BlendMode::Normal);
                if passes.len() > 1
                    && (opacity < 1.0
                        || (depth == 0 && self.pending_backdrop.is_some() && has_non_normal))
                {
                    if depth == 0 && (self.pending_backdrop.is_some() || !self.out.is_empty()) {
                        let shape_id = self.fresh_id();
                        let placement = self.placement_attrs();
                        let _ = write!(
                            self.defs,
                            "<g id=\"s{shape_id}\"><path d=\"{d}\" fill=\"#ffffff\""
                        );
                        if *rule == FillRule::EvenOdd {
                            self.defs.push_str(" fill-rule=\"evenodd\"");
                        }
                        push_opacity(&mut self.defs, "fill-opacity", opacity);
                        self.defs.push_str(&placement);
                        self.defs.push_str("/></g>\n");
                        let pass_count = passes.len();
                        if self.enqueue_backdrop_step(
                            BackdropStep::MultiPass {
                                passes: passes.clone(),
                                shape_id,
                            },
                            pass_count,
                            false,
                        ) {
                            return;
                        }
                    }

                    // Bounded isolated single-opacity fallback (when there is no
                    // backdrop yet, nested inside a pattern/mask, or past the
                    // filter/snapshot budget): evaluate all passes at 1.0 inside
                    // one isolated group and apply `opacity` ONCE.
                    if depth == 0 {
                        self.flush_pending_backdrop();
                    }
                    let clip_id = self.fresh_id();
                    let _ = write!(self.defs, "<clipPath id=\"c{clip_id}\"");
                    if let Some(outer) = self.clip {
                        let _ = write!(self.defs, " clip-path=\"url(#c{outer})\"");
                    }
                    let _ = write!(self.defs, "><path d=\"{d}\"");
                    if *rule == FillRule::EvenOdd {
                        self.defs.push_str(" clip-rule=\"evenodd\"");
                    }
                    self.defs.push_str("/></clipPath>\n");
                    self.out.push_str("<g");
                    push_opacity(&mut self.out, "opacity", opacity);
                    let _ = write!(
                        self.out,
                        " style=\"isolation:isolate;\" clip-path=\"url(#c{clip_id})\""
                    );
                    if let Some(m) = self.stroke_mask {
                        let _ = write!(self.out, " mask=\"url(#m{m})\"");
                    }
                    self.out.push_str(">\n");
                    for (id, blend_mode) in passes {
                        self.out.push_str("<rect x=\"0\" y=\"0\" width=\"");
                        push_num(&mut self.out, self.page_width);
                        self.out.push_str("\" height=\"");
                        push_num(&mut self.out, self.page_height);
                        let _ = write!(self.out, "\" fill=\"url(#p{id})\"");
                        if let Some(name) = blend_css(blend_mode) {
                            let _ = write!(self.out, " style=\"mix-blend-mode:{name};\"");
                        }
                        self.out.push_str("/>\n");
                    }
                    self.out.push_str("</g>\n");
                    return;
                }
                if depth == 0 && self.pending_backdrop.is_some() && has_non_normal {
                    if let [(id, blend_mode)] = passes.as_slice() {
                        let ov_id = self.fresh_id();
                        let placement = self.placement_attrs();
                        let _ = write!(
                            self.defs,
                            "<g id=\"ov{ov_id}\"><path d=\"{d}\" fill=\"url(#p{id})\""
                        );
                        if *rule == FillRule::EvenOdd {
                            self.defs.push_str(" fill-rule=\"evenodd\"");
                        }
                        push_opacity(&mut self.defs, "fill-opacity", opacity);
                        self.defs.push_str(&placement);
                        self.defs.push_str("/></g>\n");
                        if self.enqueue_backdrop_step(
                            BackdropStep::BlendOverlay {
                                ov_id,
                                blend_mode: *blend_mode,
                            },
                            1,
                            false,
                        ) {
                            return;
                        }
                    }
                    self.flush_pending_backdrop();
                }
                let placement = self.placement_attrs();
                for (id, blend_mode) in passes {
                    let _ = write!(self.out, "<path d=\"{d}\" fill=\"url(#p{id})\"");
                    if *rule == FillRule::EvenOdd {
                        self.out.push_str(" fill-rule=\"evenodd\"");
                    }
                    push_opacity(&mut self.out, "fill-opacity", opacity);
                    if let Some(name) = blend_css(blend_mode) {
                        let _ = write!(self.out, " style=\"mix-blend-mode:{name};\"");
                    }
                    self.out.push_str(&placement);
                    self.out.push_str("/>\n");
                }
            }
        }
    }

    fn emit_fill(&mut self, path: &Path, rule: &FillRule, paint: &Paint, alpha: f32, depth: u8) {
        match paint {
            Paint::Solid(color) => {
                let opacity = (color.a * alpha).clamp(0.0, 1.0);
                if opacity <= 0.0 {
                    return;
                }
                let Some(d) = self.path_data(path) else {
                    return;
                };
                self.out.push_str("<path d=\"");
                self.out.push_str(&d);
                self.out.push_str("\" fill=\"");
                self.out.push_str(&hex_color(color));
                self.out.push('"');
                if *rule == FillRule::EvenOdd {
                    self.out.push_str(" fill-rule=\"evenodd\"");
                }
                push_opacity(&mut self.out, "fill-opacity", opacity);
                let placement = self.placement_attrs();
                self.out.push_str(&placement);
                self.out.push_str("/>\n");
            }
            Paint::Tiling(tiling) => {
                let opacity = if alpha.is_finite() {
                    alpha.clamp(0.0, 1.0)
                } else {
                    0.0
                };
                if opacity <= 0.0 {
                    return;
                }
                let Some(d) = self.path_data(path) else {
                    return;
                };
                self.emit_tiling_fill_path_data(&d, rule, tiling, opacity, depth);
            }
            _ => {
                tracing::debug!("skipping non-solid fill paint (rasterized upstream)");
            }
        }
    }

    fn emit_stroke(
        &mut self,
        path: &Path,
        style: &StrokeStyle,
        paint: &Paint,
        alpha: f32,
        depth: u8,
    ) {
        match paint {
            Paint::Solid(color) => {
                let opacity = (color.a * alpha).clamp(0.0, 1.0);
                if opacity <= 0.0 {
                    return;
                }
                let Some(d) = self.path_data(path) else {
                    return;
                };
                self.out.push_str("<path d=\"");
                self.out.push_str(&d);
                self.out.push_str("\" fill=\"none\" stroke=\"");
                self.out.push_str(&hex_color(color));
                self.out.push('"');
                push_opacity(&mut self.out, "stroke-opacity", opacity);
                push_stroke_attrs(&mut self.out, style, self.scale);
                let placement = self.placement_attrs();
                self.out.push_str(&placement);
                self.out.push_str("/>\n");
            }
            Paint::Tiling(tiling) => {
                let opacity = if alpha.is_finite() {
                    alpha.clamp(0.0, 1.0)
                } else {
                    0.0
                };
                if opacity <= 0.0 {
                    return;
                }
                let Some(d) = self.path_data(path) else {
                    return;
                };
                let plan = self.emit_tiling_plan(tiling, depth);
                match plan {
                    TilingPassPlan::Affine { k_id, mk_id, a_id } => {
                        if depth == 0 {
                            let shape_id = self.fresh_id();
                            let placement = self.placement_attrs();
                            let _ = write!(
                                self.defs,
                                "<g id=\"s{shape_id}\"><path d=\"{d}\" fill=\"none\" stroke=\"#ffffff\""
                            );
                            push_opacity(&mut self.defs, "stroke-opacity", opacity);
                            push_stroke_attrs(&mut self.defs, style, self.scale);
                            self.defs.push_str(&placement);
                            self.defs.push_str("/></g>\n");
                            if self.enqueue_backdrop_step(
                                BackdropStep::Affine {
                                    k_id,
                                    mk_id,
                                    a_id,
                                    shape_id,
                                },
                                4,
                                true,
                            ) {
                                return;
                            }
                            self.flush_pending_backdrop();
                        }
                        let placement = self.placement_attrs();
                        let _ = write!(
                            self.out,
                            "<path d=\"{d}\" fill=\"none\" stroke=\"url(#p{k_id})\""
                        );
                        push_opacity(&mut self.out, "stroke-opacity", opacity);
                        push_stroke_attrs(&mut self.out, style, self.scale);
                        self.out.push_str(&placement);
                        self.out.push_str("/>\n");
                    }
                    TilingPassPlan::Passes(passes) => {
                        if passes.is_empty() {
                            return;
                        }
                        let has_non_normal = passes.iter().any(|&(_, m)| m != BlendMode::Normal);
                        if passes.len() > 1
                            && (opacity < 1.0
                                || (depth == 0
                                    && self.pending_backdrop.is_some()
                                    && has_non_normal))
                        {
                            if depth == 0
                                && (self.pending_backdrop.is_some() || !self.out.is_empty())
                            {
                                let shape_id = self.fresh_id();
                                let placement = self.placement_attrs();
                                let _ = write!(
                                    self.defs,
                                    "<g id=\"s{shape_id}\"><path d=\"{d}\" fill=\"none\" stroke=\"#ffffff\""
                                );
                                push_opacity(&mut self.defs, "stroke-opacity", opacity);
                                push_stroke_attrs(&mut self.defs, style, self.scale);
                                self.defs.push_str(&placement);
                                self.defs.push_str("/></g>\n");
                                let pass_count = passes.len();
                                if self.enqueue_backdrop_step(
                                    BackdropStep::MultiPass {
                                        passes: passes.clone(),
                                        shape_id,
                                    },
                                    pass_count,
                                    false,
                                ) {
                                    return;
                                }
                            }

                            // Bounded isolated single-opacity fallback for strokes.
                            if depth == 0 {
                                self.flush_pending_backdrop();
                            }
                            let mask_id = self.fresh_id();
                            let _ = write!(
                                self.defs,
                                "<mask id=\"m{mask_id}\" maskUnits=\"userSpaceOnUse\">"
                            );
                            if let Some(outer) = self.stroke_mask {
                                let _ = write!(self.defs, "<g mask=\"url(#m{outer})\">");
                            }
                            let _ = write!(
                                self.defs,
                                "<path d=\"{d}\" fill=\"none\" stroke=\"#ffffff\""
                            );
                            push_stroke_attrs(&mut self.defs, style, self.scale);
                            if let Some(c) = self.clip {
                                let _ = write!(self.defs, " clip-path=\"url(#c{c})\"");
                            }
                            self.defs.push_str("/>");
                            if self.stroke_mask.is_some() {
                                self.defs.push_str("</g>");
                            }
                            self.defs.push_str("</mask>\n");

                            self.out.push_str("<g");
                            push_opacity(&mut self.out, "opacity", opacity);
                            let _ = writeln!(
                                self.out,
                                " style=\"isolation:isolate;\" mask=\"url(#m{mask_id})\">"
                            );
                            for (id, blend_mode) in passes {
                                self.out.push_str("<rect x=\"0\" y=\"0\" width=\"");
                                push_num(&mut self.out, self.page_width);
                                self.out.push_str("\" height=\"");
                                push_num(&mut self.out, self.page_height);
                                let _ = write!(self.out, "\" fill=\"url(#p{id})\"");
                                if let Some(name) = blend_css(blend_mode) {
                                    let _ = write!(self.out, " style=\"mix-blend-mode:{name};\"");
                                }
                                self.out.push_str("/>\n");
                            }
                            self.out.push_str("</g>\n");
                            return;
                        }
                        if depth == 0 && self.pending_backdrop.is_some() && has_non_normal {
                            if let [(id, blend_mode)] = passes.as_slice() {
                                let ov_id = self.fresh_id();
                                let placement = self.placement_attrs();
                                let _ = write!(
                                    self.defs,
                                    "<g id=\"ov{ov_id}\"><path d=\"{d}\" fill=\"none\" stroke=\"url(#p{id})\""
                                );
                                push_opacity(&mut self.defs, "stroke-opacity", opacity);
                                push_stroke_attrs(&mut self.defs, style, self.scale);
                                self.defs.push_str(&placement);
                                self.defs.push_str("/></g>\n");
                                if self.enqueue_backdrop_step(
                                    BackdropStep::BlendOverlay {
                                        ov_id,
                                        blend_mode: *blend_mode,
                                    },
                                    1,
                                    false,
                                ) {
                                    return;
                                }
                            }
                            self.flush_pending_backdrop();
                        }
                        let placement = self.placement_attrs();
                        for (id, blend_mode) in passes {
                            let _ = write!(
                                self.out,
                                "<path d=\"{d}\" fill=\"none\" stroke=\"url(#p{id})\""
                            );
                            push_opacity(&mut self.out, "stroke-opacity", opacity);
                            if let Some(name) = blend_css(blend_mode) {
                                let _ = write!(self.out, " style=\"mix-blend-mode:{name};\"");
                            }
                            push_stroke_attrs(&mut self.out, style, self.scale);
                            self.out.push_str(&placement);
                            self.out.push_str("/>\n");
                        }
                    }
                }
            }
            _ => {
                tracing::debug!("skipping non-solid stroke paint (rasterized upstream)");
            }
        }
    }

    // -- clips --

    /// Register a `<clipPath>` def intersected with the enclosing clip (via
    /// `clip-path` on the `clipPath` element itself, SVG 1.1 §14.3.5) and make
    /// it the active clip. An explicitly empty path clips out all painting;
    /// invalid nonempty geometry leaves the outer clip in force.
    fn push_clip(&mut self, path: &Path, rule: &FillRule) {
        let Some(d) = (if path.is_empty() {
            Some(String::new())
        } else {
            self.path_data(path)
        }) else {
            return;
        };
        let id = self.fresh_id();
        let _ = write!(self.defs, "<clipPath id=\"c{id}\"");
        if let Some(outer) = self.clip {
            let _ = write!(self.defs, " clip-path=\"url(#c{outer})\"");
        }
        let _ = write!(self.defs, "><path d=\"{d}\"");
        if *rule == FillRule::EvenOdd {
            self.defs.push_str(" clip-rule=\"evenodd\"");
        }
        self.defs.push_str("/></clipPath>\n");
        self.clip = Some(id);
    }

    /// A stroked clip has no SVG `<clipPath>` equivalent; a luminance mask with
    /// a white stroke is exactly the coverage the CPU backend rasterizes. The
    /// def bakes in the enclosing clip and mask so intersection nests.
    fn push_clip_stroke(&mut self, path: &Path, style: &StrokeStyle) {
        let Some(d) = self.path_data(path) else {
            return;
        };
        let id = self.fresh_id();
        let _ = write!(
            self.defs,
            "<mask id=\"m{id}\" maskUnits=\"userSpaceOnUse\">"
        );
        if let Some(outer) = self.stroke_mask {
            let _ = write!(self.defs, "<g mask=\"url(#m{outer})\">");
        }
        let _ = write!(
            self.defs,
            "<path d=\"{d}\" fill=\"none\" stroke=\"#ffffff\""
        );
        push_stroke_attrs(&mut self.defs, style, self.scale);
        if let Some(c) = self.clip {
            let _ = write!(self.defs, " clip-path=\"url(#c{c})\"");
        }
        self.defs.push_str("/>");
        if self.stroke_mask.is_some() {
            self.defs.push_str("</g>");
        }
        self.defs.push_str("</mask>\n");
        self.stroke_mask = Some(id);
    }

    // -- transparency groups --

    fn write_blend_group_tag(
        &mut self,
        blend_mode: BlendMode,
        isolated: bool,
        alpha: f32,
        mask_id: Option<u32>,
        inner_body: &str,
    ) {
        self.out.push_str("<g");
        if alpha < 1.0 {
            self.out.push_str(" opacity=\"");
            push_num(&mut self.out, alpha as f64);
            self.out.push('"');
        }
        let blend = blend_css(blend_mode);
        if blend.is_some() || isolated {
            self.out.push_str(" style=\"");
            if let Some(name) = blend {
                let _ = write!(self.out, "mix-blend-mode:{name};");
            }
            if isolated {
                self.out.push_str("isolation:isolate;");
            }
            self.out.push('"');
        }
        if let Some(id) = mask_id {
            let _ = write!(self.out, " mask=\"url(#m{id})\"");
        }
        self.out.push_str(">\n");
        self.out.push_str(inner_body);
        self.out.push_str("</g>\n");
    }

    #[allow(clippy::too_many_arguments)]
    fn close_blend_group(
        &mut self,
        open: bool,
        isolates: bool,
        blend_mode: BlendMode,
        isolated: bool,
        alpha: f32,
        mask_id: Option<u32>,
        prev_out: String,
        prev_pending: Option<PendingBackdrop>,
        depth: u8,
    ) {
        if !open {
            return;
        }
        self.flush_pending_backdrop();
        let inner_body = std::mem::replace(&mut self.out, prev_out);
        self.pending_backdrop = prev_pending;
        self.blend_depth = self.blend_depth.saturating_sub(1);
        if isolates {
            self.isolated_depth = self.isolated_depth.saturating_sub(1);
        }
        if depth == 0 && blend_css(blend_mode).is_some() && self.pending_backdrop.is_some() {
            let ov_id = self.fresh_id();
            let _ = write!(self.defs, "<g id=\"ov{ov_id}\"");
            if alpha < 1.0 {
                self.defs.push_str(" opacity=\"");
                push_num(&mut self.defs, alpha as f64);
                self.defs.push('"');
            }
            if let Some(id) = mask_id {
                let _ = write!(self.defs, " mask=\"url(#m{id})\"");
            }
            self.defs.push_str(">\n");
            self.defs.push_str(&inner_body);
            self.defs.push_str("</g>\n");
            if self.enqueue_backdrop_step(
                BackdropStep::BlendOverlay { ov_id, blend_mode },
                1,
                false,
            ) {
                return;
            }
            self.flush_pending_backdrop();
        }
        self.write_blend_group_tag(blend_mode, isolated, alpha, mask_id, &inner_body);
    }

    /// Emit an ExtGState soft mask as an SVG `<mask>` def and return its id.
    /// SVG masks are luminance-based by default, which is exactly PDF's
    /// luminosity soft mask; alpha masks use `mask-type:alpha`.
    fn emit_soft_mask_def(&mut self, mask: &SoftMask, depth: u8) -> Option<u32> {
        if depth >= MAX_MASK_DEPTH {
            tracing::warn!("soft-mask nesting exceeds depth budget; mask dropped");
            return None;
        }
        if mask.transfer.is_some() {
            self.warn_transfer();
        }
        let key = MaskDefKey {
            commands: std::sync::Arc::as_ptr(&mask.commands),
            kind: mask.kind,
            offset: [mask.offset.0.to_bits(), mask.offset.1.to_bits()],
            backdrop_luma: mask.backdrop_luma.to_bits(),
            origin_scale: [self.x0.to_bits(), self.y1.to_bits(), self.scale.to_bits()],
            depth,
        };
        if let Some(&id) = self.mask_defs.get(&key) {
            return Some(id);
        }
        let id = self.fresh_id();
        self.mask_defs.insert(key, id);

        let saved_out = std::mem::take(&mut self.out);
        let saved_pending = self.pending_backdrop.take();
        let saved_blend_depth = std::mem::replace(&mut self.blend_depth, 0);
        let saved_isolated_depth = std::mem::replace(&mut self.isolated_depth, 0);
        // Mask content is self-contained (geometry fixed at `gs` time); it must
        // not inherit the page's active clip chain.
        let (saved_clip, saved_mask) = (self.clip.take(), self.stroke_mask.take());
        // /BC backdrop: areas the mask group leaves unpainted read as this
        // luminosity. SVG masks default to black (fully masked), so only a
        // non-zero backdrop needs painting.
        if mask.kind == SoftMaskKind::Luminosity && mask.backdrop_luma > 0.0 {
            let luma = Color::gray(mask.backdrop_luma.clamp(0.0, 1.0));
            self.out.push_str("<rect x=\"0\" y=\"0\" width=\"");
            push_num(&mut self.out, self.page_width);
            self.out.push_str("\" height=\"");
            push_num(&mut self.out, self.page_height);
            self.out.push_str("\" fill=\"");
            self.out.push_str(&hex_color(&luma));
            self.out.push_str("\"/>\n");
        }
        let (dx, dy) = (
            mask.offset.0 as f64 * self.scale,
            mask.offset.1 as f64 * self.scale,
        );
        let translated = dx != 0.0 || dy != 0.0;
        if translated {
            // Page-space translation; the Y-flip negates the vertical part.
            self.out.push_str("<g transform=\"translate(");
            push_num(&mut self.out, dx);
            self.out.push(' ');
            push_num(&mut self.out, -dy);
            self.out.push_str(")\">\n");
        }
        self.emit_commands(&mask.commands.commands, depth + 1);
        self.flush_pending_backdrop();
        if translated {
            self.out.push_str("</g>\n");
        }
        let body = std::mem::replace(&mut self.out, saved_out);
        self.pending_backdrop = saved_pending;
        self.blend_depth = saved_blend_depth;
        self.isolated_depth = saved_isolated_depth;
        self.clip = saved_clip;
        self.stroke_mask = saved_mask;

        let _ = write!(self.defs, "<mask id=\"m{id}\" maskUnits=\"userSpaceOnUse\"");
        if mask.kind == SoftMaskKind::Alpha {
            self.defs.push_str(" style=\"mask-type:alpha\"");
        }
        self.defs.push_str(">\n");
        self.defs.push_str(&body);
        self.defs.push_str("</mask>\n");
        Some(id)
    }

    // -- text --

    fn emit_glyph_run(&mut self, run: &GlyphRun, depth: u8) {
        if run.overprint.is_some() {
            self.warn_overprint();
        }
        let Some(font) = self.fonts.get(run.font_id) else {
            tracing::warn!(font_id = run.font_id, "glyph run references unknown font");
            return;
        };
        if !font.has_font_data() {
            return;
        }
        match &run.paint {
            Paint::Solid(color) => {
                let opacity = (color.a * run.alpha).clamp(0.0, 1.0);
                if opacity <= 0.0 {
                    return;
                }
                if font.is_type3() {
                    self.emit_type3_run(run, font, color, opacity);
                } else {
                    self.emit_outline_run(run, font, color, opacity);
                }
            }
            Paint::Tiling(tiling) => {
                let opacity = if run.alpha.is_finite() {
                    run.alpha.clamp(0.0, 1.0)
                } else {
                    0.0
                };
                if opacity <= 0.0 {
                    return;
                }
                if font.is_type3() {
                    self.emit_tiling_type3_run(run, font, tiling, opacity, depth);
                } else if let Some(d) = self.outline_run_path_data(run, font) {
                    self.emit_tiling_fill_path_data(&d, &FillRule::NonZero, tiling, opacity, depth);
                }
            }
            _ => {
                tracing::debug!("skipping non-solid text paint (rasterized upstream)");
            }
        }
    }

    /// Outline fonts: build one merged SVG path string for all glyphs of `run`.
    fn outline_run_path_data(&mut self, run: &GlyphRun, font: &LoadedFont) -> Option<String> {
        let upem = font.units_per_em;
        if !(upem.is_finite() && upem > 0.0) {
            return None;
        }
        let mut d = String::new();
        'glyphs: for glyph in &run.glyphs {
            if self.check_over_budget() {
                break;
            }
            let Some(outline) = self.glyph_outline(run.font_id, font, glyph.glyph_id) else {
                continue;
            };
            let off = (glyph.x, glyph.y);
            let mut ok = true;
            let mut part = String::new();
            for cmd in &outline.commands {
                if self.check_over_budget() {
                    // Never emit the unfinished glyph path.
                    break 'glyphs;
                }
                match *cmd {
                    OutlineCommand::MoveTo(x, y) => {
                        ok &= self.push_glyph_point(&mut part, 'M', &[(x, y)], upem, run, off);
                    }
                    OutlineCommand::LineTo(x, y) => {
                        ok &= self.push_glyph_point(&mut part, 'L', &[(x, y)], upem, run, off);
                    }
                    OutlineCommand::QuadTo(x1, y1, x, y) => {
                        ok &= self.push_glyph_point(
                            &mut part,
                            'Q',
                            &[(x1, y1), (x, y)],
                            upem,
                            run,
                            off,
                        );
                    }
                    OutlineCommand::CurveTo(x1, y1, x2, y2, x, y) => {
                        ok &= self.push_glyph_point(
                            &mut part,
                            'C',
                            &[(x1, y1), (x2, y2), (x, y)],
                            upem,
                            run,
                            off,
                        );
                    }
                    OutlineCommand::Close => part.push('Z'),
                }
                if !ok {
                    break;
                }
            }
            if ok {
                d.push_str(&part);
            }
        }
        (!d.is_empty()).then_some(d)
    }

    /// Outline fonts: all glyphs of the run merge into one nonzero-filled path.
    fn emit_outline_run(&mut self, run: &GlyphRun, font: &LoadedFont, color: &Color, opacity: f32) {
        let Some(d) = self.outline_run_path_data(run, font) else {
            return;
        };
        self.out.push_str("<path d=\"");
        self.out.push_str(&d);
        self.out.push_str("\" fill=\"");
        self.out.push_str(&hex_color(color));
        self.out.push('"');
        push_opacity(&mut self.out, "fill-opacity", opacity);
        let placement = self.placement_attrs();
        self.out.push_str(&placement);
        self.out.push_str("/>\n");
    }

    /// Type 3 glyph runs painted with a [`TilingPattern`]: render the glyph
    /// coverage as opaque white into a shape/mask def and composite the pattern
    /// through that coverage.
    fn emit_tiling_type3_run(
        &mut self,
        run: &GlyphRun,
        font: &LoadedFont,
        tiling: &zpdf_display_list::TilingPattern,
        opacity: f32,
        depth: u8,
    ) {
        let saved_out = std::mem::take(&mut self.out);
        let saved_pending = self.pending_backdrop.take();
        self.emit_type3_run(run, font, &Color::white(), 1.0);
        let shape_body = std::mem::replace(&mut self.out, saved_out);
        self.pending_backdrop = saved_pending;
        if shape_body.is_empty() {
            return;
        }
        let plan = self.emit_tiling_plan(tiling, depth);
        match plan {
            TilingPassPlan::Affine { k_id, mk_id, a_id } => {
                if depth == 0 {
                    let shape_id = self.fresh_id();
                    let _ = write!(self.defs, "<g id=\"s{shape_id}\"");
                    push_opacity(&mut self.defs, "opacity", opacity);
                    self.defs.push_str(">\n");
                    self.defs.push_str(&shape_body);
                    self.defs.push_str("</g>\n");
                    if self.enqueue_backdrop_step(
                        BackdropStep::Affine {
                            k_id,
                            mk_id,
                            a_id,
                            shape_id,
                        },
                        4,
                        true,
                    ) {
                        return;
                    }
                    self.flush_pending_backdrop();
                }
                let mask_id = self.fresh_id();
                let _ = write!(
                    self.defs,
                    "<mask id=\"m{mask_id}\" maskUnits=\"userSpaceOnUse\">\n{shape_body}</mask>\n"
                );
                self.out.push_str("<rect x=\"0\" y=\"0\" width=\"");
                push_num(&mut self.out, self.page_width);
                self.out.push_str("\" height=\"");
                push_num(&mut self.out, self.page_height);
                let _ = write!(
                    self.out,
                    "\" fill=\"url(#p{k_id})\" mask=\"url(#m{mask_id})\""
                );
                push_opacity(&mut self.out, "fill-opacity", opacity);
                self.out.push_str("/>\n");
            }
            TilingPassPlan::Passes(passes) => {
                if passes.is_empty() {
                    return;
                }
                let has_non_normal = passes.iter().any(|&(_, m)| m != BlendMode::Normal);
                if depth == 0
                    && (passes.len() > 1 || (self.pending_backdrop.is_some() && has_non_normal))
                {
                    if self.pending_backdrop.is_some() || !self.out.is_empty() {
                        let shape_id = self.fresh_id();
                        let _ = write!(self.defs, "<g id=\"s{shape_id}\"");
                        push_opacity(&mut self.defs, "opacity", opacity);
                        self.defs.push_str(">\n");
                        self.defs.push_str(&shape_body);
                        self.defs.push_str("</g>\n");
                        let pass_count = passes.len();
                        if self.enqueue_backdrop_step(
                            BackdropStep::MultiPass {
                                passes: passes.clone(),
                                shape_id,
                            },
                            pass_count,
                            false,
                        ) {
                            return;
                        }
                    }
                    self.flush_pending_backdrop();
                }
                let mask_id = self.fresh_id();
                let _ = write!(
                    self.defs,
                    "<mask id=\"m{mask_id}\" maskUnits=\"userSpaceOnUse\">\n{shape_body}</mask>\n"
                );
                if passes.len() > 1 {
                    self.out.push_str("<g");
                    push_opacity(&mut self.out, "opacity", opacity);
                    let _ = writeln!(
                        self.out,
                        " style=\"isolation:isolate;\" mask=\"url(#m{mask_id})\">"
                    );
                    for (id, blend_mode) in passes {
                        self.out.push_str("<rect x=\"0\" y=\"0\" width=\"");
                        push_num(&mut self.out, self.page_width);
                        self.out.push_str("\" height=\"");
                        push_num(&mut self.out, self.page_height);
                        let _ = write!(self.out, "\" fill=\"url(#p{id})\"");
                        if let Some(name) = blend_css(blend_mode) {
                            let _ = write!(self.out, " style=\"mix-blend-mode:{name};\"");
                        }
                        self.out.push_str("/>\n");
                    }
                    self.out.push_str("</g>\n");
                } else if let Some((id, blend_mode)) = passes.into_iter().next() {
                    self.out.push_str("<rect x=\"0\" y=\"0\" width=\"");
                    push_num(&mut self.out, self.page_width);
                    self.out.push_str("\" height=\"");
                    push_num(&mut self.out, self.page_height);
                    let _ = write!(
                        self.out,
                        "\" fill=\"url(#p{id})\" mask=\"url(#m{mask_id})\""
                    );
                    push_opacity(&mut self.out, "fill-opacity", opacity);
                    if let Some(name) = blend_css(blend_mode) {
                        let _ = write!(self.out, " style=\"mix-blend-mode:{name};\"");
                    }
                    self.out.push_str("/>\n");
                }
            }
        }
    }

    /// Append one SVG path verb whose points run through the glyph transform.
    /// Returns false (caller drops the glyph) on non-finite/corrupt geometry.
    fn push_glyph_point(
        &self,
        d: &mut String,
        verb: char,
        points: &[(f64, f64)],
        upem: f64,
        run: &GlyphRun,
        off: (f32, f32),
    ) -> bool {
        d.push(verb);
        for (i, &(gx, gy)) in points.iter().enumerate() {
            if i > 0 {
                d.push(' ');
            }
            // font units → text space; the shape x carries Th (Tz/100), the
            // offset already includes it (accumulated advance).
            let tx = gx / upem * run.font_size as f64 * run.h_scale as f64 + off.0 as f64;
            let ty = gy / upem * run.font_size as f64 + off.1 as f64;
            let tm = &run.transform;
            let px = tm.a * tx + tm.c * ty + tm.e;
            let py = tm.b * tx + tm.d * ty + tm.f;
            let (sx, sy) = self.map(px, py);
            if !coord_ok(sx) || !coord_ok(sy) {
                return false;
            }
            push_pair(d, sx, sy);
        }
        true
    }

    /// Outline of `glyph_id` of `font` (cache id `font_id`), extracted once
    /// per export while the cache has room.
    fn glyph_outline(
        &mut self,
        font_id: FontId,
        font: &LoadedFont,
        glyph_id: u16,
    ) -> Option<Rc<GlyphOutline>> {
        if let Some(outline) = self.glyph_outlines.get(&(font_id, glyph_id)) {
            return outline.clone();
        }
        let outline = font.glyph_outline(glyph_id).map(Rc::new);
        let commands = outline.as_ref().map_or(1, |o| o.commands.len().max(1));
        let total = self.glyph_outline_commands.saturating_add(commands);
        if total <= MAX_CACHED_OUTLINE_COMMANDS {
            self.glyph_outline_commands = total;
            self.glyph_outlines
                .insert((font_id, glyph_id), outline.clone());
        }
        outline
    }

    /// The interpreted procedure of Type 3 glyph `glyph_id` of `font` (cache
    /// id `font_id`), interpreted once per export while the cache has room.
    fn type3_glyph(
        &mut self,
        font_id: FontId,
        font: &LoadedFont,
        glyph_id: u16,
    ) -> Option<Rc<Type3Glyph>> {
        use zpdf_content::interpreter::ContentInterpreter;
        if let Some(glyph) = self.type3_glyphs.get(&(font_id, glyph_id)) {
            return glyph.clone();
        }
        let glyph = font
            .type3_glyph_stream(glyph_id)
            .map(|(stream, font_matrix)| {
                let glyph_rect = zpdf_core::Rect::new(0.0, -1000.0, 1000.0, 1000.0);
                let mut images = ImageCache::new();
                let mut interp = ContentInterpreter::new(glyph_rect).with_images(&mut images);
                if let Some(deadline) = self.deadline {
                    interp = interp.with_deadline(deadline);
                }
                let list = interp.interpret(stream);
                Rc::new(Type3Glyph {
                    list,
                    images,
                    font_matrix,
                })
            });
        let (commands, image_bytes) = glyph.as_ref().map_or((1, 0), |g| {
            (g.list.commands.len().max(1), g.images.bytes_used())
        });
        let total_commands = self.type3_glyph_commands.saturating_add(commands);
        let total_image_bytes = self.type3_glyph_image_bytes.saturating_add(image_bytes);
        if total_commands <= MAX_CACHED_TYPE3_COMMANDS
            && total_image_bytes <= MAX_CACHED_TYPE3_IMAGE_BYTES
        {
            self.type3_glyph_commands = total_commands;
            self.type3_glyph_image_bytes = total_image_bytes;
            self.type3_glyphs.insert((font_id, glyph_id), glyph.clone());
        }
        glyph
    }

    /// Type3 glyphs: interpret the glyph's content stream (as the CPU backend
    /// does) and emit its fills, strokes and images through the Type3
    /// transform chain.
    fn emit_type3_run(&mut self, run: &GlyphRun, font: &LoadedFont, color: &Color, opacity: f32) {
        for glyph in &run.glyphs {
            if self.check_over_budget() {
                break;
            }
            let Some(program) = self.type3_glyph(run.font_id, font, glyph.glyph_id) else {
                continue;
            };
            let font_matrix = program.font_matrix;
            // Glyph space → page space (SVG units are page points).
            let glyph_to_page = run
                .transform
                .concat(&zpdf_core::Matrix::translate(glyph.x as f64, 0.0))
                .concat(&zpdf_core::Matrix::scale(
                    run.font_size as f64 * run.h_scale as f64,
                    run.font_size as f64,
                ))
                .concat(&zpdf_core::Matrix::new(
                    font_matrix[0],
                    font_matrix[1],
                    font_matrix[2],
                    font_matrix[3],
                    font_matrix[4],
                    font_matrix[5],
                ));
            // Stroke widths are in glyph space; scale them by the whole
            // glyph-to-page transform (its mean axis scale), as the CPU
            // backend does.
            let stroke_scale = (glyph_to_page.a * glyph_to_page.d
                - glyph_to_page.b * glyph_to_page.c)
                .abs()
                .sqrt();

            for cmd in &program.list.commands {
                if self.check_over_budget() {
                    break;
                }
                match cmd {
                    RenderCommand::FillPath { path, rule, .. } => {
                        let Some(d) = self.type3_path_data(path, &font_matrix, run, glyph.x) else {
                            continue;
                        };
                        self.out.push_str("<path d=\"");
                        self.out.push_str(&d);
                        self.out.push_str("\" fill=\"");
                        self.out.push_str(&hex_color(color));
                        self.out.push('"');
                        if *rule == FillRule::EvenOdd {
                            self.out.push_str(" fill-rule=\"evenodd\"");
                        }
                        push_opacity(&mut self.out, "fill-opacity", opacity);
                        let placement = self.placement_attrs();
                        self.out.push_str(&placement);
                        self.out.push_str("/>\n");
                    }
                    RenderCommand::StrokePath { path, style, .. } => {
                        let Some(d) = self.type3_path_data(path, &font_matrix, run, glyph.x) else {
                            continue;
                        };
                        let width = (style.width as f64 * stroke_scale * self.scale).max(0.05);
                        if !width.is_finite() {
                            continue;
                        }
                        self.out.push_str("<path d=\"");
                        self.out.push_str(&d);
                        self.out.push_str("\" fill=\"none\" stroke=\"");
                        self.out.push_str(&hex_color(color));
                        self.out.push_str("\" stroke-width=\"");
                        push_num(&mut self.out, width);
                        self.out.push('"');
                        push_opacity(&mut self.out, "stroke-opacity", opacity);
                        let placement = self.placement_attrs();
                        self.out.push_str(&placement);
                        self.out.push_str("/>\n");
                    }
                    RenderCommand::DrawImage(draw) => {
                        let Some(image) = program.images.get(draw.image_id) else {
                            continue;
                        };
                        let transform = glyph_to_page.concat(&draw.transform);
                        let alpha = if draw.alpha.is_finite() {
                            draw.alpha.clamp(0.0, 1.0)
                        } else {
                            0.0
                        };
                        if draw.is_image_mask {
                            // The glyph interpreter paints stencils in its
                            // default black; the text colour is the paint.
                            let tint = [color.r, color.g, color.b, color.a].map(f32::to_bits);
                            let key = ImageDefKey::Type3Stencil {
                                font: run.font_id,
                                glyph: glyph.glyph_id,
                                image: draw.image_id,
                                tint,
                            };
                            self.place_image(key, image, Some(color), &transform, alpha * opacity);
                        } else {
                            // A coloured (`d0`) glyph's image keeps its own
                            // colours; cache it per glyph like a stencil.
                            let key = ImageDefKey::Type3Stencil {
                                font: run.font_id,
                                glyph: glyph.glyph_id,
                                image: draw.image_id,
                                tint: [u32::MAX; 4],
                            };
                            self.place_image(key, image, None, &transform, alpha * opacity);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn type3_path_data(
        &mut self,
        path: &Path,
        font_matrix: &[f64; 6],
        run: &GlyphRun,
        glyph_x_offset: f32,
    ) -> Option<String> {
        let (x0, y1, scale) = (self.x0, self.y1, self.scale);
        let map = |gx: f64, gy: f64| -> Option<(f64, f64)> {
            // FontMatrix → text space, scale by size (x also carries Th),
            // advance offset, then Tm/CTM, then the fixed page flip.
            let tx = font_matrix[0] * gx + font_matrix[2] * gy + font_matrix[4];
            let ty = font_matrix[1] * gx + font_matrix[3] * gy + font_matrix[5];
            let tx = tx * run.font_size as f64 * run.h_scale as f64 + glyph_x_offset as f64;
            let ty = ty * run.font_size as f64;
            let tm = &run.transform;
            let px = tm.a * tx + tm.c * ty + tm.e;
            let py = tm.b * tx + tm.d * ty + tm.f;
            let (sx, sy) = ((px - x0) * scale, (y1 - py) * scale);
            (coord_ok(sx) && coord_ok(sy)).then_some((sx, sy))
        };
        self.path_data_with(path, map)
    }

    // -- images --

    fn emit_image(&mut self, draw: &ImageDraw) {
        let alpha = if draw.alpha.is_finite() {
            draw.alpha.clamp(0.0, 1.0)
        } else {
            0.0
        };
        if alpha <= 0.0 {
            return;
        }
        let Some(image) = self.images.get(draw.image_id) else {
            tracing::warn!(
                image_id = draw.image_id,
                "image draw references unknown image"
            );
            return;
        };
        self.place_image(
            ImageDefKey::Page(draw.image_id),
            image,
            None,
            &draw.transform,
            alpha,
        );
    }

    /// Draw `image` (the def `key`, written on first use) over the unit square
    /// mapped by the page-space `transform`. A `tint` recolours a stencil's
    /// opaque samples.
    fn place_image(
        &mut self,
        key: ImageDefKey,
        image: &DecodedImage,
        tint: Option<&Color>,
        transform: &zpdf_core::Matrix,
        alpha: f32,
    ) {
        if alpha <= 0.0 {
            return;
        }
        // PDF images occupy the unit square mapped by the CTM, with sample row
        // 0 at the TOP edge (v = 1); composing with the fixed page flip at
        // scale 1 gives, for image pixel (ix, iy):
        //   svg_x = (a/iw)·ix + (-c/ih)·iy + (c + e - x0)
        //   svg_y = (-b/iw)·ix + (d/ih)·iy + (y1 - d - f)
        // — the same affine as the CPU backend's `render_image`.
        let tm = transform;
        let iw = image.width as f64;
        let ih = image.height as f64;
        let s = self.scale;
        let m = [
            tm.a * s / iw,
            -tm.b * s / iw,
            -tm.c * s / ih,
            tm.d * s / ih,
            (tm.c + tm.e - self.x0) * s,
            (self.y1 - tm.d - tm.f) * s,
        ];
        if m.iter().any(|v| !v.is_finite() || v.abs() > MAX_COORD) {
            tracing::warn!("skipping image with corrupt transform");
            return;
        }

        let id = match self.image_defs.get(&key) {
            Some(id) => *id,
            None => {
                let png = match tint {
                    Some(color) => encode_png(&tinted(image, color)),
                    None => encode_png(image),
                };
                let id = png.map(|png| {
                    let id = self.fresh_id();
                    let _ = write!(
                        self.defs,
                        "<image id=\"i{id}\" width=\"{}\" height=\"{}\" \
                         preserveAspectRatio=\"none\" href=\"data:image/png;base64,",
                        image.width, image.height
                    );
                    push_base64(&mut self.defs, &png);
                    self.defs.push_str("\"/>\n");
                    id
                });
                if id.is_none() {
                    tracing::warn!("skipping unencodable image");
                }
                self.image_defs.insert(key, id);
                id
            }
        };
        let Some(id) = id else {
            return;
        };

        // The `<use>`'s own `transform` redefines its user space, and
        // `clip-path` on an element resolves in that (post-transform) space —
        // so the active clip must go on a plain wrapper `<g>` instead. A
        // wrapper around a single Normal-blended leaf cannot change
        // compositing (the isolation a clipped group introduces only matters
        // for blend modes crossing it).
        let placement = self.placement_attrs();
        let wrapped = !placement.is_empty();
        if wrapped {
            let _ = write!(self.out, "<g{placement}>");
        }
        let _ = write!(self.out, "<use href=\"#i{id}\" transform=\"matrix(");
        for (i, v) in m.iter().enumerate() {
            if i > 0 {
                self.out.push(' ');
            }
            push_num(&mut self.out, *v);
        }
        self.out.push_str(")\"");
        push_opacity(&mut self.out, "opacity", alpha);
        self.out.push_str("/>");
        if wrapped {
            self.out.push_str("</g>");
        }
        self.out.push('\n');
    }

    // -- path serialization --

    fn path_data(&mut self, path: &Path) -> Option<String> {
        let (x0, y1, scale) = (self.x0, self.y1, self.scale);
        let map = |x: f64, y: f64| -> Option<(f64, f64)> {
            let (sx, sy) = ((x - x0) * scale, (y1 - y) * scale);
            (coord_ok(sx) && coord_ok(sy)).then_some((sx, sy))
        };
        self.path_data_with(path, map)
    }

    /// Serialize a path through a point-mapping closure. Returns `None` when
    /// the path is empty or any point maps to corrupt geometry (the caller
    /// skips the command, as the raster backends do).
    fn path_data_with(
        &mut self,
        path: &Path,
        map: impl Fn(f64, f64) -> Option<(f64, f64)>,
    ) -> Option<String> {
        if path.is_empty() {
            return None;
        }
        let mut d = String::new();
        for elem in path {
            if self.check_over_budget() {
                return None;
            }
            match elem {
                PathElement::MoveTo(p) => {
                    let (x, y) = map(p.x, p.y)?;
                    d.push('M');
                    push_pair(&mut d, x, y);
                }
                PathElement::LineTo(p) => {
                    let (x, y) = map(p.x, p.y)?;
                    d.push('L');
                    push_pair(&mut d, x, y);
                }
                PathElement::CurveTo(c1, c2, end) => {
                    let (x1, y1) = map(c1.x, c1.y)?;
                    let (x2, y2) = map(c2.x, c2.y)?;
                    let (x, y) = map(end.x, end.y)?;
                    d.push('C');
                    push_pair(&mut d, x1, y1);
                    d.push(' ');
                    push_pair(&mut d, x2, y2);
                    d.push(' ');
                    push_pair(&mut d, x, y);
                }
                PathElement::Close => d.push('Z'),
            }
        }
        (!d.is_empty()).then_some(d)
    }

    // -- one-shot diagnostics --

    fn warn_overprint(&mut self) {
        if !self.warned_overprint {
            self.warned_overprint = true;
            tracing::warn!("overprint has no SVG equivalent; painting normally");
        }
    }

    fn warn_knockout(&mut self) {
        if !self.warned_knockout {
            self.warned_knockout = true;
            tracing::warn!("knockout group has no SVG equivalent; compositing as normal group");
        }
    }

    fn warn_transfer(&mut self) {
        if !self.warned_transfer {
            self.warned_transfer = true;
            tracing::warn!("soft-mask /TR transfer function has no SVG equivalent; ignored");
        }
    }
}

fn coord_ok(v: f64) -> bool {
    v.is_finite() && v.abs() <= MAX_COORD
}

/// Format a coordinate/length with up to 3 decimals, trailing zeros trimmed.
fn push_num(out: &mut String, v: f64) {
    let r = (v * 1000.0).round() / 1000.0;
    if r == r.trunc() && r.abs() < 1e15 {
        let _ = write!(out, "{}", r as i64);
    } else {
        let mut s = format!("{r:.3}");
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
        out.push_str(&s);
    }
}

fn push_pair(out: &mut String, x: f64, y: f64) {
    push_num(out, x);
    out.push(' ');
    push_num(out, y);
}

fn push_opacity(out: &mut String, attr: &str, opacity: f32) {
    if opacity < 1.0 {
        let _ = write!(out, " {attr}=\"");
        push_num(out, opacity as f64);
        out.push('"');
    }
}

/// Round to the nearest byte, like the CPU backend's `color_to_paint`.
fn hex_color(c: &Color) -> String {
    format!(
        "#{:02x}{:02x}{:02x}",
        (c.r.clamp(0.0, 1.0) * 255.0).round() as u8,
        (c.g.clamp(0.0, 1.0) * 255.0).round() as u8,
        (c.b.clamp(0.0, 1.0) * 255.0).round() as u8,
    )
}

fn blend_css(mode: BlendMode) -> Option<&'static str> {
    Some(match mode {
        BlendMode::Normal => return None,
        BlendMode::Multiply => "multiply",
        BlendMode::Screen => "screen",
        BlendMode::Overlay => "overlay",
        BlendMode::Darken => "darken",
        BlendMode::Lighten => "lighten",
        BlendMode::ColorDodge => "color-dodge",
        BlendMode::ColorBurn => "color-burn",
        BlendMode::HardLight => "hard-light",
        BlendMode::SoftLight => "soft-light",
        BlendMode::Difference => "difference",
        BlendMode::Exclusion => "exclusion",
        BlendMode::Hue => "hue",
        BlendMode::Saturation => "saturation",
        BlendMode::Color => "color",
        BlendMode::Luminosity => "luminosity",
    })
}

fn push_stroke_attrs(out: &mut String, style: &StrokeStyle, scale: f64) {
    // PDF width 0 means "thinnest device line"; SVG width 0 renders nothing.
    let width = if style.width.is_finite() && style.width > 0.0 {
        style.width as f64 * scale
    } else {
        0.1
    };
    out.push_str(" stroke-width=\"");
    push_num(out, width);
    out.push('"');
    match style.cap {
        LineCap::Butt => {}
        LineCap::Round => out.push_str(" stroke-linecap=\"round\""),
        LineCap::Square => out.push_str(" stroke-linecap=\"square\""),
    }
    match style.join {
        LineJoin::Miter => {
            // SVG's default miterlimit is 4; PDF's is 10, so emit explicitly.
            if style.miter_limit.is_finite() && style.miter_limit >= 1.0 {
                out.push_str(" stroke-miterlimit=\"");
                push_num(out, style.miter_limit as f64);
                out.push('"');
            }
        }
        LineJoin::Round => out.push_str(" stroke-linejoin=\"round\""),
        LineJoin::Bevel => out.push_str(" stroke-linejoin=\"bevel\""),
    }
    if let Some(dash) = &style.dash {
        // SVG natively repeats odd-length dash arrays, matching PDF 8.4.3.6.
        if !zpdf_render::dash::is_degenerate(&dash.array) && dash.phase.is_finite() {
            out.push_str(" stroke-dasharray=\"");
            for (i, v) in dash.array.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                push_num(out, *v as f64 * scale);
            }
            out.push('"');
            if dash.phase != 0.0 {
                out.push_str(" stroke-dashoffset=\"");
                push_num(out, dash.phase as f64 * scale);
                out.push('"');
            }
        }
    }
}

/// PNG-encode a decoded RGBA image, un-premultiplying alpha first when needed
/// (PNG stores straight alpha; `DecodedImage` is premultiplied once a soft
/// mask or stencil has been folded in).
/// A stencil image recoloured with `color`: its coverage (alpha) is kept and
/// its colour replaced, as the CPU backend paints Type 3 stencil glyphs.
fn tinted(image: &DecodedImage, color: &Color) -> DecodedImage {
    let mut out = image.clone();
    let rgb = [color.r, color.g, color.b].map(|c| c.clamp(0.0, 1.0));
    for px in out.data.as_chunks_mut::<4>().0 {
        let a = px[3] as f32 / 255.0;
        let scale = if out.premultiplied { a } else { 1.0 };
        for (dst, c) in px[..3].iter_mut().zip(rgb) {
            *dst = (c * scale * 255.0).round() as u8;
        }
    }
    out
}

fn encode_png(image: &DecodedImage) -> Option<Vec<u8>> {
    let expected = (image.width as usize)
        .checked_mul(image.height as usize)?
        .checked_mul(4)?;
    if image.width == 0 || image.height == 0 || image.data.len() != expected {
        return None;
    }
    let data: Cow<'_, [u8]> = if image.premultiplied {
        let mut d = image.data.clone();
        unpremultiply(&mut d);
        Cow::Owned(d)
    } else {
        Cow::Borrowed(&image.data)
    };
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(
            &data,
            image.width,
            image.height,
            image::ExtendedColorType::Rgba8,
        )
        .ok()?;
    Some(png)
}

fn unpremultiply(data: &mut [u8]) {
    for px in data.as_chunks_mut::<4>().0 {
        let a = px[3] as u32;
        if a == 0 {
            px[0] = 0;
            px[1] = 0;
            px[2] = 0;
        } else if a < 255 {
            for c in &mut px[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
}

fn push_base64(out: &mut String, data: &[u8]) {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for chunk in data.chunks(3) {
        let b1 = chunk[0] as u32;
        let b2 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b3 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b1 << 16) | (b2 << 8) | b3;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zpdf_core::{Matrix, Point, Rect};
    use zpdf_display_list::{DashPattern, ImageDraw, PositionedGlyph};

    fn empty_caches() -> (FontCache, ImageCache) {
        (FontCache::new(), ImageCache::new())
    }

    #[test]
    fn hex_color_rounds_to_nearest_byte() {
        assert_eq!(hex_color(&Color::rgb(0.0, 0.5, 1.0)), "#0080ff");
        assert_eq!(hex_color(&Color::rgb(0.25, 0.5, 0.75)), "#4080bf");
        assert_eq!(
            hex_color(&Color::rgb(0.49 / 255.0, 0.5 / 255.0, 0.499)),
            "#00017f"
        );
        assert_eq!(
            hex_color(&Color::rgb(254.49 / 255.0, 254.5 / 255.0, 1.0)),
            "#feffff"
        );
    }

    #[test]
    fn hex_color_preserves_saturation_at_boundaries() {
        assert_eq!(hex_color(&Color::rgb(-1.0, 0.0, 2.0)), "#0000ff");
        assert_eq!(
            hex_color(&Color::rgb(f32::NAN, f32::NEG_INFINITY, f32::INFINITY)),
            "#0000ff"
        );
    }

    fn page() -> DisplayList {
        DisplayList::new(Rect::new(0.0, 0.0, 100.0, 200.0))
    }

    fn to_svg(dl: &DisplayList) -> String {
        let (fonts, images) = empty_caches();
        display_list_to_svg(dl, &fonts, &images, &SvgOptions::default())
    }

    fn rect_path(x0: f64, y0: f64, x1: f64, y1: f64) -> Path {
        let mut p = Path::new();
        p.rect(Rect::new(x0, y0, x1, y1));
        p
    }

    fn one_glyph_run() -> GlyphRun {
        GlyphRun {
            font_id: 0,
            font_size: 12.0,
            glyphs: vec![PositionedGlyph {
                glyph_id: 1,
                code_len: 1,
                code: 65,
                x: 0.0,
                y: 0.0,
                advance: 1.0,
            }],
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
            transform: Matrix::identity(),
            h_scale: 1.0,
        }
    }

    #[test]
    fn outline_and_type3_replay_check_deadlines_inside_one_glyph() {
        let (fonts, images) = empty_caches();
        let mut font = LoadedFont::new_placeholder("Budget".into());
        font.units_per_em = 1000.0;
        let run = one_glyph_run();
        for budget in [Some(std::time::Duration::ZERO), None] {
            let options = SvgOptions {
                background: None,
                budget,
            };
            let mut writer =
                SvgWriter::new(page().page_rect, 100.0, 200.0, &fonts, &images, &options);
            writer.glyph_outlines.insert(
                (0, 1),
                Some(Rc::new(GlyphOutline {
                    commands: vec![
                        OutlineCommand::MoveTo(0.0, 0.0),
                        OutlineCommand::LineTo(1.0, 1.0),
                        OutlineCommand::Close,
                    ],
                    advance_width: 1.0,
                })),
            );
            // The outer glyph check is #63; the first inner command is #64.
            writer.budget_checks = 62;
            writer.emit_outline_run(&run, &font, &Color::black(), 1.0);
            assert_eq!(writer.over_budget, budget.is_some());
            assert_eq!(writer.out.contains("<path"), budget.is_none());

            let mut writer =
                SvgWriter::new(page().page_rect, 100.0, 200.0, &fonts, &images, &options);
            let mut list = page();
            list.push(RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 1.0, 1.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::black()),
                alpha: 1.0,
                overprint: None,
            });
            writer.type3_glyphs.insert(
                (0, 1),
                Some(Rc::new(Type3Glyph {
                    list,
                    images: ImageCache::new(),
                    font_matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
                })),
            );
            writer.budget_checks = 62;
            writer.emit_type3_run(&run, &font, &Color::black(), 1.0);
            assert_eq!(writer.over_budget, budget.is_some());
            assert_eq!(writer.out.contains("<path"), budget.is_none());
        }
    }

    #[test]
    fn path_serialization_discards_an_unfinished_path_after_deadline() {
        let (fonts, images) = empty_caches();
        let options = SvgOptions {
            background: None,
            budget: Some(std::time::Duration::ZERO),
        };
        let mut writer = SvgWriter::new(page().page_rect, 100.0, 200.0, &fonts, &images, &options);
        writer.budget_checks = 63;
        assert!(writer.path_data(&rect_path(0.0, 0.0, 1.0, 1.0)).is_none());
        assert!(writer.over_budget);
        assert!(writer.out.is_empty());
    }

    #[test]
    fn fill_is_y_flipped_into_viewbox() {
        let mut dl = page();
        dl.push(RenderCommand::FillPath {
            path: rect_path(10.0, 20.0, 30.0, 40.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        let svg = to_svg(&dl);
        assert!(svg.contains("viewBox=\"0 0 100 200\""), "{svg}");
        // (10,20) bottom-left → svg y = 200-20 = 180; (30,40) → y = 160.
        assert!(svg.contains("M10 180L30 180L30 160L10 160Z"), "{svg}");
        assert!(svg.contains("fill=\"#ff0000\""), "{svg}");
        // NonZero is SVG's default; no fill-rule attribute.
        assert!(!svg.contains("fill-rule"), "{svg}");
    }

    #[test]
    fn even_odd_and_alpha_are_emitted() {
        let mut dl = page();
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 10.0, 10.0),
            rule: FillRule::EvenOdd,
            paint: Paint::Solid(Color::rgba(0.0, 0.0, 0.0, 1.0)),
            alpha: 0.5,
            overprint: None,
        });
        let svg = to_svg(&dl);
        assert!(svg.contains("fill-rule=\"evenodd\""), "{svg}");
        assert!(svg.contains("fill-opacity=\"0.5\""), "{svg}");
    }

    #[test]
    fn stroke_attributes_round_trip() {
        let mut dl = page();
        dl.push(RenderCommand::StrokePath {
            path: rect_path(0.0, 0.0, 50.0, 50.0),
            style: StrokeStyle {
                width: 2.5,
                cap: LineCap::Round,
                join: LineJoin::Bevel,
                miter_limit: 10.0,
                dash: Some(DashPattern {
                    array: vec![3.0],
                    phase: 1.5,
                }),
            },
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        let svg = to_svg(&dl);
        assert!(svg.contains("fill=\"none\""), "{svg}");
        assert!(svg.contains("stroke=\"#0000ff\""), "{svg}");
        assert!(svg.contains("stroke-width=\"2.5\""), "{svg}");
        assert!(svg.contains("stroke-linecap=\"round\""), "{svg}");
        assert!(svg.contains("stroke-linejoin=\"bevel\""), "{svg}");
        assert!(svg.contains("stroke-dasharray=\"3\""), "{svg}");
        assert!(svg.contains("stroke-dashoffset=\"1.5\""), "{svg}");
    }

    #[test]
    fn degenerate_dash_is_dropped() {
        let mut dl = page();
        dl.push(RenderCommand::StrokePath {
            path: rect_path(0.0, 0.0, 50.0, 50.0),
            style: StrokeStyle {
                dash: Some(DashPattern {
                    array: vec![0.0, 0.0],
                    phase: 0.0,
                }),
                ..StrokeStyle::default()
            },
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
        });
        let svg = to_svg(&dl);
        assert!(!svg.contains("stroke-dasharray"), "{svg}");
        // PDF miter default 10 must be explicit (SVG default is 4).
        assert!(svg.contains("stroke-miterlimit=\"10\""), "{svg}");
    }

    #[test]
    fn zero_width_stroke_gets_hairline() {
        let mut dl = page();
        dl.push(RenderCommand::StrokePath {
            path: rect_path(0.0, 0.0, 50.0, 50.0),
            style: StrokeStyle {
                width: 0.0,
                ..StrokeStyle::default()
            },
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
        });
        assert!(to_svg(&dl).contains("stroke-width=\"0.1\""));
    }

    #[test]
    fn clips_nest_and_balance() {
        let mut dl = page();
        dl.push(RenderCommand::PushClip {
            path: rect_path(0.0, 0.0, 50.0, 50.0),
            rule: FillRule::EvenOdd,
        });
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 10.0, 10.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PopClip);
        // A fill after the pop must NOT carry the clip reference.
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 5.0, 5.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        let svg = to_svg(&dl);
        assert!(svg.contains("<clipPath id=\"c1\">"), "{svg}");
        assert!(svg.contains("clip-rule=\"evenodd\""), "{svg}");
        // Clips are element attributes, never `<g>` wrappers (a `clip-path` on
        // a group isolates blending — PDF clips must not).
        assert!(!svg.contains("<g clip-path"), "{svg}");
        assert!(
            svg.contains("fill=\"#000000\" clip-path=\"url(#c1)\""),
            "{svg}"
        );
        assert!(!svg.contains("fill=\"#ffffff\" clip-path"), "{svg}");
    }

    #[test]
    fn nested_clips_intersect_via_clippath_chain() {
        let mut dl = page();
        dl.push(RenderCommand::PushClip {
            path: rect_path(0.0, 0.0, 80.0, 80.0),
            rule: FillRule::NonZero,
        });
        dl.push(RenderCommand::PushClip {
            path: rect_path(10.0, 10.0, 60.0, 60.0),
            rule: FillRule::NonZero,
        });
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 10.0, 10.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PopClip);
        dl.push(RenderCommand::PopClip);
        let svg = to_svg(&dl);
        // Inner def carries the outer def: intersection without wrappers.
        assert!(
            svg.contains("<clipPath id=\"c2\" clip-path=\"url(#c1)\">"),
            "{svg}"
        );
        assert!(svg.contains("clip-path=\"url(#c2)\"/>"), "{svg}");
    }

    #[test]
    fn unbalanced_stream_still_produces_wellformed_groups() {
        let mut dl = page();
        dl.push(RenderCommand::PopClip); // stray pop: ignored
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Screen,
            isolated: false,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
            alpha: 1.0,
            mask: None,
        }); // never popped: auto-closed
        let svg = to_svg(&dl);
        assert_eq!(svg.matches("<g ").count(), svg.matches("</g>").count());
    }

    /// A mask first written too deep to include its own nested mask must not
    /// stand in for the complete mask at a shallower use.
    #[test]
    fn mask_def_written_at_the_depth_limit_is_not_reused_shallower() {
        let fill = |color: Color| RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 10.0, 10.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(color),
            alpha: 1.0,
            overprint: None,
        };
        let masked = |mask: SoftMask, inner: RenderCommand| {
            [
                RenderCommand::PushBlendGroup {
                    blend_mode: BlendMode::Normal,
                    isolated: false,
                    knockout: false,
                    bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
                    alpha: 1.0,
                    mask: Some(mask),
                },
                inner,
                RenderCommand::PopBlendGroup,
            ]
        };
        let soft_mask = |commands: Vec<RenderCommand>| {
            let mut list = page();
            commands.into_iter().for_each(|c| list.push(c));
            SoftMask {
                kind: SoftMaskKind::Luminosity,
                commands: std::sync::Arc::new(list),
                offset: (0.0, 0.0),
                backdrop_luma: 0.0,
                transfer: None,
            }
        };
        let leaf = soft_mask(vec![fill(Color::rgb(0.2, 0.4, 0.6))]);
        let shared = soft_mask(masked(leaf, fill(Color::black())).to_vec());
        // `shared` first at depth MAX_MASK_DEPTH - 1, where `leaf` is dropped.
        let mut deep = shared.clone();
        for _ in 0..MAX_MASK_DEPTH - 1 {
            deep = soft_mask(masked(deep, fill(Color::black())).to_vec());
        }
        let mut dl = page();
        for c in masked(deep, fill(Color::black()))
            .into_iter()
            .chain(masked(shared, fill(Color::black())))
        {
            dl.push(c);
        }
        let svg = to_svg(&dl);
        assert!(
            svg.contains("#336699"),
            "the shallow use includes leaf: {svg}"
        );
    }

    #[test]
    fn skipped_clip_keeps_pop_balanced_and_outer_clip_in_force() {
        let mut dl = page();
        dl.push(RenderCommand::PushClip {
            path: rect_path(0.0, 0.0, 50.0, 50.0),
            rule: FillRule::NonZero,
        });
        let mut bad = Path::new();
        bad.move_to(Point::new(f64::NAN, 0.0));
        bad.line_to(Point::new(10.0, 10.0));
        dl.push(RenderCommand::PushClip {
            path: bad,
            rule: FillRule::NonZero,
        });
        // Under a skipped inner clip, the OUTER clip must stay in force —
        // strictly tighter than dropping clipping, like the CPU backend.
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 10.0, 10.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PopClip);
        dl.push(RenderCommand::PopClip);
        let svg = to_svg(&dl);
        assert!(svg.contains("clip-path=\"url(#c1)\"/>"), "{svg}");
        assert!(!svg.contains("id=\"c2\""), "{svg}");
    }

    #[test]
    fn blend_group_maps_to_css() {
        let mut dl = page();
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Multiply,
            isolated: true,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 100.0, 200.0),
            alpha: 0.75,
            mask: None,
        });
        dl.push(RenderCommand::PopBlendGroup);
        let svg = to_svg(&dl);
        assert!(svg.contains("opacity=\"0.75\""), "{svg}");
        assert!(svg.contains("mix-blend-mode:multiply"), "{svg}");
        assert!(svg.contains("isolation:isolate"), "{svg}");
    }

    #[test]
    fn luminosity_soft_mask_emits_mask_def() {
        let mut mask_dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 200.0));
        mask_dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 100.0, 100.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        let mut dl = page();
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Normal,
            isolated: false,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 100.0, 200.0),
            alpha: 1.0,
            mask: Some(SoftMask {
                kind: SoftMaskKind::Luminosity,
                commands: std::sync::Arc::new(mask_dl),
                offset: (5.0, 7.0),
                backdrop_luma: 0.25,
                transfer: None,
            }),
        });
        dl.push(RenderCommand::PopBlendGroup);
        let svg = to_svg(&dl);
        assert!(
            svg.contains("<mask id=\"m1\" maskUnits=\"userSpaceOnUse\">"),
            "{svg}"
        );
        assert!(svg.contains("mask=\"url(#m1)\""), "{svg}");
        // Backdrop luma 0.25 → gray(0.25) = #404040 (round to nearest).
        assert!(svg.contains("fill=\"#404040\""), "{svg}");
        // Page-space offset (5,7) → svg translate(5 -7).
        assert!(svg.contains("translate(5 -7)"), "{svg}");
        assert!(!svg.contains("mask-type:alpha"), "{svg}");
    }

    #[test]
    fn image_matrix_and_data_uri() {
        let mut dl = page();
        let (fonts, mut images) = empty_caches();
        let id = images.insert(DecodedImage {
            width: 2,
            height: 2,
            data: vec![255; 16],
            has_alpha: false,
            is_image_mask: false,
            premultiplied: false,
        });
        // CTM: unit square scaled 40×60, placed at (10, 20).
        dl.push(RenderCommand::DrawImage(ImageDraw {
            image_id: id,
            transform: Matrix {
                a: 40.0,
                b: 0.0,
                c: 0.0,
                d: 60.0,
                e: 10.0,
                f: 20.0,
            },
            alpha: 1.0,
            is_image_mask: false,
        }));
        let svg = display_list_to_svg(&dl, &fonts, &images, &SvgOptions::default());
        // svg_x = (40/2)ix + 10, svg_y = (60/2)iy + (200-60-20) = 30iy + 120.
        assert!(svg.contains("matrix(20 0 0 30 10 120)"), "{svg}");
        assert!(svg.contains("href=\"data:image/png;base64,"), "{svg}");
        assert!(svg.contains("preserveAspectRatio=\"none\""), "{svg}");
    }

    #[test]
    fn missing_font_and_image_are_skipped_without_panic() {
        let mut dl = page();
        dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
            font_id: 42,
            font_size: 12.0,
            glyphs: vec![PositionedGlyph {
                glyph_id: 1,
                code_len: 0,
                code: 0,
                x: 0.0,
                y: 0.0,
                advance: 6.0,
            }],
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
            transform: Matrix::identity(),
            h_scale: 1.0,
        }));
        dl.push(RenderCommand::DrawImage(ImageDraw {
            image_id: 7,
            transform: Matrix::identity(),
            alpha: 1.0,
            is_image_mask: false,
        }));
        let svg = to_svg(&dl);
        assert!(!svg.contains("<image"), "{svg}");
        assert!(svg.ends_with("</svg>\n"), "{svg}");
    }

    #[test]
    fn background_is_optional() {
        let dl = page();
        let (fonts, images) = empty_caches();
        let svg = display_list_to_svg(
            &dl,
            &fonts,
            &images,
            &SvgOptions {
                background: None,
                ..SvgOptions::default()
            },
        );
        assert!(!svg.contains("<rect"), "{svg}");
        let with_bg = display_list_to_svg(&dl, &fonts, &images, &SvgOptions::default());
        assert!(with_bg.contains("<rect width=\"100\" height=\"200\" fill=\"#ffffff\"/>"));
    }

    #[test]
    fn spent_budget_truncates_but_stays_wellformed() {
        let mut dl = page();
        for _ in 0..1000 {
            dl.push(RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 10.0, 10.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::black()),
                alpha: 1.0,
                overprint: None,
            });
        }
        let (fonts, images) = empty_caches();
        let svg = display_list_to_svg(
            &dl,
            &fonts,
            &images,
            &SvgOptions {
                budget: Some(std::time::Duration::ZERO),
                ..SvgOptions::default()
            },
        );
        // The clock is sampled every 64 commands, so at most a handful of
        // paints land before truncation — and the document still closes.
        assert!(svg.matches("<path").count() < 100, "{svg}");
        assert!(svg.ends_with("</svg>\n"), "{svg}");
    }

    #[test]
    fn base64_is_standard() {
        let mut s = String::new();
        push_base64(&mut s, b"Man");
        assert_eq!(s, "TWFu");
        s.clear();
        push_base64(&mut s, b"Ma");
        assert_eq!(s, "TWE=");
        s.clear();
        push_base64(&mut s, b"M");
        assert_eq!(s, "TQ==");
    }

    #[test]
    fn unpremultiply_inverts_multiply() {
        // 50% alpha premultiplied mid-gray: (64, 64, 64, 128) → ~(128, 128, 128).
        let mut px = vec![64u8, 64, 64, 128, 0, 0, 0, 0];
        unpremultiply(&mut px);
        assert!((px[0] as i32 - 128).abs() <= 1, "{px:?}");
        assert_eq!(&px[4..], &[0, 0, 0, 0]);
    }

    #[test]
    fn numbers_are_trimmed() {
        let mut s = String::new();
        push_num(&mut s, 1.0);
        s.push(' ');
        push_num(&mut s, 1.25);
        s.push(' ');
        push_num(&mut s, 1.2344);
        s.push(' ');
        push_num(&mut s, -0.5);
        assert_eq!(s, "1 1.25 1.234 -0.5");
    }

    #[test]
    fn tiling_pattern_emits_deduplicated_svg_pattern_def() {
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        let mut sq = Path::new();
        sq.rect(Rect::new(2.0, 2.0, 9.0, 9.0));
        cell.push(RenderCommand::FillPath {
            path: sq,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        let tiling = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 20.0, 20.0),
            x_step: 20.0,
            y_step: 20.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        for x in [0.0, 50.0] {
            let mut r = Path::new();
            r.rect(Rect::new(x, 0.0, x + 40.0, 40.0));
            dl.push(RenderCommand::FillPath {
                path: r,
                rule: FillRule::NonZero,
                paint: Paint::Tiling(std::sync::Arc::clone(&tiling)),
                alpha: 1.0,
                overprint: None,
            });
        }
        let svg = display_list_to_svg(
            &dl,
            &FontCache::new(),
            &ImageCache::new(),
            &SvgOptions::default(),
        );
        assert_eq!(
            svg.matches("<pattern ").count(),
            1,
            "shared tiling pattern must be defined once: {svg}"
        );
        assert_eq!(
            svg.matches("fill=\"url(#p1)\"").count(),
            2,
            "both fills must reference the pattern def: {svg}"
        );
    }

    #[test]
    fn tiling_pattern_tiny_units_with_large_matrix_preserves_precision() {
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 0.0004, 0.0004));
        let mut sq = Path::new();
        sq.rect(Rect::new(0.0001, 0.0001, 0.0003, 0.0003));
        cell.push(RenderCommand::FillPath {
            path: sq,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        let tiling = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 0.0004, 0.0004),
            x_step: 0.0004,
            y_step: 0.0004,
            matrix: zpdf_core::Matrix::scale(25_000.0, 25_000.0),
            tiling_type: 1,
        });
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        let mut r = Path::new();
        r.rect(Rect::new(0.0, 0.0, 100.0, 100.0));
        dl.push(RenderCommand::FillPath {
            path: r,
            rule: FillRule::NonZero,
            paint: Paint::Tiling(tiling),
            alpha: 1.0,
            overprint: None,
        });
        let svg = display_list_to_svg(
            &dl,
            &FontCache::new(),
            &ImageCache::new(),
            &SvgOptions::default(),
        );
        assert!(
            svg.contains("width=\"10\" height=\"10\""),
            "tiny pattern units scaled by 25000 must normalize to 10x10 pattern viewport: {svg}"
        );
        assert!(
            svg.contains("M2.5 7.5L7.5 7.5L7.5 2.5L2.5 2.5Z"),
            "cell geometry must preserve full coordinates instead of rounding to 0: {svg}"
        );
    }

    #[test]
    fn tiling_pattern_overlapping_bbox_beyond_four_steps_emits_visible_copy() {
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 10.0));
        let mut sq = Path::new();
        sq.rect(Rect::new(90.0, 2.0, 99.0, 8.0));
        cell.push(RenderCommand::FillPath {
            path: sq,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.5, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        let tiling = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 100.0, 10.0),
            x_step: 10.0,
            y_step: 10.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        let mut r = Path::new();
        r.rect(Rect::new(0.0, 0.0, 100.0, 100.0));
        dl.push(RenderCommand::FillPath {
            path: r,
            rule: FillRule::NonZero,
            paint: Paint::Tiling(tiling),
            alpha: 1.0,
            overprint: None,
        });
        let svg = display_list_to_svg(
            &dl,
            &FontCache::new(),
            &ImageCache::new(),
            &SvgOptions::default(),
        );
        // Copy di=-9 shifts [90..99] by -90 into [0..9] inside the 10x10 pattern viewport.
        assert!(
            svg.contains("M0 8L9 8L9 2L0 2Z"),
            "overlapping copy di=-9 must be emitted inside the 10x10 pattern: {svg}"
        );
    }

    #[test]
    fn tiling_pattern_multiply_preserves_page_backdrop_in_svg() {
        // Cell paints opaque white with Multiply on left half [0..10] and
        // opaque cyan with Multiply on right half [10..20].
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        cell.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Multiply,
            isolated: true,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 20.0, 20.0),
            alpha: 1.0,
            mask: None,
        });
        let mut left = Path::new();
        left.rect(Rect::new(0.0, 0.0, 10.0, 20.0));
        cell.push(RenderCommand::FillPath {
            path: left,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        let mut right = Path::new();
        right.rect(Rect::new(10.0, 0.0, 20.0, 20.0));
        cell.push(RenderCommand::FillPath {
            path: right,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 1.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        cell.push(RenderCommand::PopBlendGroup);

        let tiling = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 20.0, 20.0),
            x_step: 20.0,
            y_step: 20.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });

        // Blue left half [0..40], yellow right half [40..80]; tile across whole page.
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 80.0, 40.0));
        let mut bg_blue = Path::new();
        bg_blue.rect(Rect::new(0.0, 0.0, 40.0, 40.0));
        dl.push(RenderCommand::FillPath {
            path: bg_blue,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        let mut bg_yellow = Path::new();
        bg_yellow.rect(Rect::new(40.0, 0.0, 80.0, 40.0));
        dl.push(RenderCommand::FillPath {
            path: bg_yellow,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 1.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        let mut full = Path::new();
        full.rect(Rect::new(0.0, 0.0, 80.0, 40.0));
        dl.push(RenderCommand::FillPath {
            path: full,
            rule: FillRule::NonZero,
            paint: Paint::Tiling(tiling),
            alpha: 1.0,
            overprint: None,
        });

        let svg = display_list_to_svg(
            &dl,
            &FontCache::new(),
            &ImageCache::new(),
            &SvgOptions::default(),
        );
        assert!(
            svg.contains("style=\"mix-blend-mode:multiply;\""),
            "backdrop-dependent Multiply tiling must apply mix-blend-mode on the painted path: {svg}"
        );

        let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default())
            .expect("valid SVG");
        let mut pixmap = resvg::tiny_skia::Pixmap::new(80, 40).expect("pixmap");
        let sx = 80.0 / tree.size().width();
        let sy = 40.0 / tree.size().height();
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(sx, sy),
            &mut pixmap.as_mut(),
        );
        let px = |x: u32, y: u32| -> [u8; 3] {
            let p = pixmap.pixel(x, y).expect("in bounds");
            [p.red(), p.green(), p.blue()]
        };
        // Over blue [0..40]: white*blue = blue, cyan*blue = blue.
        assert_eq!(
            px(5, 20),
            [0, 0, 255],
            "white Multiply over blue must stay blue"
        );
        assert_eq!(
            px(15, 20),
            [0, 0, 255],
            "cyan Multiply over blue must stay blue"
        );
        // Over yellow [40..80]: white*yellow = yellow, cyan*yellow = green.
        assert_eq!(
            px(45, 20),
            [255, 255, 0],
            "white Multiply over yellow must stay yellow"
        );
        assert_eq!(
            px(55, 20),
            [0, 255, 0],
            "cyan Multiply over yellow must turn green"
        );
    }

    #[test]
    fn tiling_pattern_overlapping_multiply_applies_object_opacity_once_for_fill_and_stroke() {
        // Two opaque black Multiply marks covering the same point inside one cell:
        // painted at 50% opacity over white must yield ~#808080 (128), NOT ~#404040 (64).
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        for _ in 0..2 {
            cell.push(RenderCommand::PushBlendGroup {
                blend_mode: BlendMode::Multiply,
                isolated: true,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 20.0, 20.0),
                alpha: 1.0,
                mask: None,
            });
            cell.push(RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 20.0, 20.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::black()),
                alpha: 1.0,
                overprint: None,
            });
            cell.push(RenderCommand::PopBlendGroup);
        }
        let tiling = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 20.0, 20.0),
            x_step: 20.0,
            y_step: 20.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });

        // White canvas; red patch on [80..120] to verify backdrop preservation too.
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 120.0, 60.0));
        dl.push(RenderCommand::FillPath {
            path: rect_path(80.0, 0.0, 120.0, 60.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        // Fill over white [0..40] and over red [80..120] at 50% opacity.
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 40.0, 60.0),
            rule: FillRule::NonZero,
            paint: Paint::Tiling(std::sync::Arc::clone(&tiling)),
            alpha: 0.5,
            overprint: None,
        });
        dl.push(RenderCommand::FillPath {
            path: rect_path(80.0, 0.0, 120.0, 60.0),
            rule: FillRule::NonZero,
            paint: Paint::Tiling(std::sync::Arc::clone(&tiling)),
            alpha: 0.5,
            overprint: None,
        });
        // Thick 20pt vertical stroke over white at x=60 (covering x=50..70) at 50% opacity.
        let mut line = Path::new();
        line.move_to(Point::new(60.0, 0.0));
        line.line_to(Point::new(60.0, 60.0));
        dl.push(RenderCommand::StrokePath {
            path: line,
            style: StrokeStyle {
                width: 20.0,
                ..StrokeStyle::default()
            },
            paint: Paint::Tiling(tiling),
            alpha: 0.5,
            overprint: None,
        });

        let svg = display_list_to_svg(
            &dl,
            &FontCache::new(),
            &ImageCache::new(),
            &SvgOptions::default(),
        );
        let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default())
            .expect("valid SVG");
        let mut pixmap = resvg::tiny_skia::Pixmap::new(120, 60).expect("pixmap");
        let sx = 120.0 / tree.size().width();
        let sy = 60.0 / tree.size().height();
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(sx, sy),
            &mut pixmap.as_mut(),
        );
        let px = |x: u32, y: u32| -> [u8; 3] {
            let p = pixmap.pixel(x, y).expect("in bounds");
            [p.red(), p.green(), p.blue()]
        };

        let fill_white = px(20, 30);
        assert!(
            fill_white.iter().all(|&c| (124..=132).contains(&c)),
            "fill with two black Multiply marks at 50% over white must be ~128 (not ~64), got {fill_white:?}"
        );
        let stroke_white = px(60, 30);
        assert!(
            stroke_white.iter().all(|&c| (124..=132).contains(&c)),
            "stroke with two black Multiply marks at 50% over white must be ~128 (not ~64), got {stroke_white:?}"
        );
        let fill_red = px(100, 30);
        assert!(
            (124..=132).contains(&fill_red[0]) && fill_red[1] <= 2 && fill_red[2] <= 2,
            "fill with two black Multiply marks at 50% over red must be ~[128, 0, 0], got {fill_red:?}"
        );
        assert_eq!(px(45, 30), [255, 255, 255], "unpainted gap stays white");
    }

    #[test]
    fn tiling_pattern_mixed_blend_passes_apply_object_opacity_once_for_fill_and_stroke() {
        // Non-associative mixed cell: Normal red followed by Multiply black.
        // At 100%, the cell evaluates to black; at 50% over white, exact single-pass
        // opacity gives ~[128, 128, 128] (per-pass 50% opacity would yield ~[128, 64, 64]).
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        cell.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 20.0, 20.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        cell.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Multiply,
            isolated: true,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 20.0, 20.0),
            alpha: 1.0,
            mask: None,
        });
        cell.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 20.0, 20.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
        });
        cell.push(RenderCommand::PopBlendGroup);

        let tiling = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 20.0, 20.0),
            x_step: 20.0,
            y_step: 20.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 60.0));
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 40.0, 60.0),
            rule: FillRule::NonZero,
            paint: Paint::Tiling(std::sync::Arc::clone(&tiling)),
            alpha: 0.5,
            overprint: None,
        });
        let mut line = Path::new();
        line.move_to(Point::new(75.0, 0.0));
        line.line_to(Point::new(75.0, 60.0));
        dl.push(RenderCommand::StrokePath {
            path: line,
            style: StrokeStyle {
                width: 20.0,
                ..StrokeStyle::default()
            },
            paint: Paint::Tiling(tiling),
            alpha: 0.5,
            overprint: None,
        });

        let svg = display_list_to_svg(
            &dl,
            &FontCache::new(),
            &ImageCache::new(),
            &SvgOptions::default(),
        );
        let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default())
            .expect("valid SVG");
        let mut pixmap = resvg::tiny_skia::Pixmap::new(100, 60).expect("pixmap");
        let sx = 100.0 / tree.size().width();
        let sy = 60.0 / tree.size().height();
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(sx, sy),
            &mut pixmap.as_mut(),
        );
        let px = |x: u32, y: u32| -> [u8; 3] {
            let p = pixmap.pixel(x, y).expect("in bounds");
            [p.red(), p.green(), p.blue()]
        };

        let fill_px = px(20, 30);
        assert!(
            fill_px.iter().all(|&c| (124..=132).contains(&c)),
            "mixed Normal+Multiply fill at 50% over white must be ~[128,128,128] (not [128,64,64]), got {fill_px:?}"
        );
        let stroke_px = px(75, 30);
        assert!(
            stroke_px.iter().all(|&c| (124..=132).contains(&c)),
            "mixed Normal+Multiply stroke at 50% over white must be ~[128,128,128] (not [128,64,64]), got {stroke_px:?}"
        );
        assert_eq!(px(50, 30), [255, 255, 255], "unpainted gap stays white");
    }

    #[test]
    fn tiling_pattern_overlapping_above_64_passes_preserves_both_copies_in_svg() {
        // 2 overlapping copies × 33 mixed segments = 66 > 64:
        //  - 1 spanning 50%-gray Multiply mark across [0..16]×[0..4] (covered twice -> ~64 on white)
        //  - 32 alternating Normal/Multiply marks in a separate strip [0..8]×[4..8]
        //    so the cell is non-uniform (exercises the per-copy path, not uniform-Multiply collapse).
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 16.0, 8.0));
        cell.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Multiply,
            isolated: true,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 16.0, 8.0),
            alpha: 1.0,
            mask: None,
        });
        cell.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 16.0, 4.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::gray(0.5)),
            alpha: 1.0,
            overprint: None,
        });
        cell.push(RenderCommand::PopBlendGroup);
        for k in 0..16 {
            let x0 = (k % 4) as f64 * 2.0;
            cell.push(RenderCommand::FillPath {
                path: rect_path(x0, 4.0, x0 + 2.0, 8.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::rgb(0.8, 0.9, 1.0)),
                alpha: 1.0,
                overprint: None,
            });
            cell.push(RenderCommand::PushBlendGroup {
                blend_mode: BlendMode::Multiply,
                isolated: true,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 16.0, 8.0),
                alpha: 1.0,
                mask: None,
            });
            cell.push(RenderCommand::FillPath {
                path: rect_path(x0, 4.0, x0 + 2.0, 8.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::rgb(1.0, 0.9, 0.8)),
                alpha: 1.0,
                overprint: None,
            });
            cell.push(RenderCommand::PopBlendGroup);
        }

        let tiling = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 16.0, 8.0),
            x_step: 8.0,
            y_step: 8.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });
        assert_eq!(tiling.blend_segments().unwrap().len(), 33);

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 64.0, 32.0));
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 64.0, 32.0),
            rule: FillRule::NonZero,
            paint: Paint::Tiling(tiling),
            alpha: 1.0,
            overprint: None,
        });

        let svg = display_list_to_svg(
            &dl,
            &FontCache::new(),
            &ImageCache::new(),
            &SvgOptions::default(),
        );
        let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default())
            .expect("valid SVG");
        let mut pixmap = resvg::tiny_skia::Pixmap::new(64, 32).expect("pixmap");
        let sx = 64.0 / tree.size().width();
        let sy = 32.0 / tree.size().height();
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(sx, sy),
            &mut pixmap.as_mut(),
        );
        // Interior pixel in spanning strip y_pdf=2 -> svg_y = 32 - 2 = 30, x=28.
        let p = pixmap.pixel(28, 30).expect("in bounds");
        let rgb = [p.red(), p.green(), p.blue()];
        assert!(
            rgb.iter().all(|&c| (58..=70).contains(&c)),
            "SVG overlapping 2x33 (>64) copies must preserve both 50%-gray Multiply marks (~64, not ~128), got {rgb:?}"
        );
    }

    #[test]
    fn tiling_pattern_mixed_blend_five_copies_preserve_object_opacity_in_svg() {
        // Non-associative mixed cell: Normal red followed by Multiply black.
        // At 100% -> black; at 50% over white -> ~[128, 128, 128] (NOT [128, 64, 64]).
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0));
        cell.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 10.0, 10.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        cell.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Multiply,
            isolated: true,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
            alpha: 1.0,
            mask: None,
        });
        cell.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 10.0, 10.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
        });
        cell.push(RenderCommand::PopBlendGroup);
        let tiling = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 10.0, 10.0),
            x_step: 10.0,
            y_step: 10.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 200.0, 120.0));
        // Five disjoint 50%-opacity fills in the top row y=[80..110].
        for i in 0..5 {
            let x0 = 10.0 + i as f64 * 35.0;
            dl.push(RenderCommand::FillPath {
                path: rect_path(x0, 80.0, x0 + 25.0, 110.0),
                rule: FillRule::NonZero,
                paint: Paint::Tiling(std::sync::Arc::clone(&tiling)),
                alpha: 0.5,
                overprint: None,
            });
        }
        // Five disjoint 50%-opacity strokes in the middle row y=[40..70].
        for i in 0..5 {
            let x = 22.0 + i as f64 * 35.0;
            let mut line = Path::new();
            line.move_to(Point::new(x, 40.0));
            line.line_to(Point::new(x, 70.0));
            dl.push(RenderCommand::StrokePath {
                path: line,
                style: StrokeStyle {
                    width: 16.0,
                    ..StrokeStyle::default()
                },
                paint: Paint::Tiling(std::sync::Arc::clone(&tiling)),
                alpha: 0.5,
                overprint: None,
            });
        }
        // Five *overlapping* 50%-opacity fills in the bottom row y=[5..30]
        // (forces > MAX_BACKDROP_SNAPSHOTS = 4 chained snapshots onto the bounded fallback).
        for i in 0..5 {
            let x0 = 10.0 + i as f64 * 8.0;
            dl.push(RenderCommand::FillPath {
                path: rect_path(x0, 5.0, x0 + 40.0, 30.0),
                rule: FillRule::NonZero,
                paint: Paint::Tiling(std::sync::Arc::clone(&tiling)),
                alpha: 0.5,
                overprint: None,
            });
        }

        let svg = display_list_to_svg(
            &dl,
            &FontCache::new(),
            &ImageCache::new(),
            &SvgOptions::default(),
        );
        let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default())
            .expect("valid SVG");
        let mut pixmap = resvg::tiny_skia::Pixmap::new(200, 120).expect("pixmap");
        let sx = 200.0 / tree.size().width();
        let sy = 120.0 / tree.size().height();
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(sx, sy),
            &mut pixmap.as_mut(),
        );
        let px = |x: u32, y_pdf: u32| -> [u8; 3] {
            let p = pixmap.pixel(x, 119 - y_pdf).expect("in bounds");
            [p.red(), p.green(), p.blue()]
        };

        let fill_1 = px(22, 95);
        let fill_5 = px(162, 95);
        assert!(
            fill_1.iter().all(|&c| (124..=132).contains(&c)),
            "1st disjoint mixed fill must be ~[128,128,128], got {fill_1:?}"
        );
        assert!(
            fill_5.iter().all(|&c| (124..=132).contains(&c)),
            "5th disjoint mixed fill must agree (~[128,128,128], not [128,64,64]), got {fill_5:?}"
        );

        let stroke_1 = px(22, 55);
        let stroke_5 = px(162, 55);
        assert!(
            stroke_1.iter().all(|&c| (124..=132).contains(&c)),
            "1st disjoint mixed stroke must be ~[128,128,128], got {stroke_1:?}"
        );
        assert!(
            stroke_5.iter().all(|&c| (124..=132).contains(&c)),
            "5th disjoint mixed stroke must agree (~[128,128,128], not [128,64,64]), got {stroke_5:?}"
        );

        // Non-overlapping tail of the 5th overlapping fill (x=76, y_pdf=18) on white:
        // exercises the bounded post-cap fallback and must STILL be gray ~[128,128,128].
        let overlap_5_tail = px(76, 18);
        assert!(
            overlap_5_tail.iter().all(|&c| (124..=132).contains(&c)),
            "5th overlapping mixed fill tail (past snapshot cap) must apply opacity once (~[128,128,128], not [128,64,64]), got {overlap_5_tail:?}"
        );
    }

    #[test]
    fn tiling_pattern_mixed_blend_preserves_translucent_backdrop_alpha_in_gap() {
        // Cell 20×20 with Normal red + Multiply black ONLY on x=[0..10], leaving
        // x=[10..20] as an unpainted gap. Over a 50%-opaque blue backdrop with
        // SvgOptions.background = None, a 50%-opacity patterned fill or stroke
        // must keep backdrop alpha = 0.5 (~128, NOT 0.625 / ~159) inside the gap.
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        cell.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 10.0, 20.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        cell.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Multiply,
            isolated: true,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 20.0, 20.0),
            alpha: 1.0,
            mask: None,
        });
        cell.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 10.0, 20.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
        });
        cell.push(RenderCommand::PopBlendGroup);
        let tiling = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 20.0, 20.0),
            x_step: 20.0,
            y_step: 20.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 120.0, 60.0));
        // 50%-opaque blue rectangle across the whole page on a transparent canvas.
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 120.0, 60.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 0.5,
            overprint: None,
        });
        // 50%-opacity mixed-mode patterned fill covering x=[0..40].
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 40.0, 60.0),
            rule: FillRule::NonZero,
            paint: Paint::Tiling(std::sync::Arc::clone(&tiling)),
            alpha: 0.5,
            overprint: None,
        });
        // 50%-opacity mixed-mode patterned stroke covering x=[60..100] (center x=80, width=40).
        let mut line = Path::new();
        line.move_to(Point::new(80.0, 0.0));
        line.line_to(Point::new(80.0, 60.0));
        dl.push(RenderCommand::StrokePath {
            path: line,
            style: StrokeStyle {
                width: 40.0,
                ..StrokeStyle::default()
            },
            paint: Paint::Tiling(tiling),
            alpha: 0.5,
            overprint: None,
        });

        let svg = display_list_to_svg(
            &dl,
            &FontCache::new(),
            &ImageCache::new(),
            &SvgOptions {
                background: None,
                ..SvgOptions::default()
            },
        );
        let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default())
            .expect("valid SVG");
        let mut pixmap = resvg::tiny_skia::Pixmap::new(120, 60).expect("pixmap");
        let sx = 120.0 / tree.size().width();
        let sy = 60.0 / tree.size().height();
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(sx, sy),
            &mut pixmap.as_mut(),
        );
        let px_rgba = |x: u32, y: u32| -> [u8; 4] {
            let p = pixmap.pixel(x, y).expect("in bounds");
            [p.red(), p.green(), p.blue(), p.alpha()]
        };

        let outside = px_rgba(50, 30);
        let fill_gap = px_rgba(15, 30);
        let stroke_gap = px_rgba(75, 30);
        assert!(
            (124..=132).contains(&outside[3]),
            "sanity check: 50%-opaque blue backdrop outside must have alpha ~128, got {outside:?}"
        );
        assert!(
            (fill_gap[3] as i32 - outside[3] as i32).abs() <= 2
                && (fill_gap[2] as i32 - outside[2] as i32).abs() <= 2
                && fill_gap[0] <= 2
                && fill_gap[1] <= 2,
            "fill unpainted gap must preserve translucent backdrop ({outside:?}, NOT alpha ~159), got {fill_gap:?}"
        );
        assert!(
            (stroke_gap[3] as i32 - outside[3] as i32).abs() <= 2
                && (stroke_gap[2] as i32 - outside[2] as i32).abs() <= 2
                && stroke_gap[0] <= 2
                && stroke_gap[1] <= 2,
            "stroke unpainted gap must preserve translucent backdrop ({outside:?}, NOT alpha ~159), got {stroke_gap:?}"
        );
        // Painted half of cell (x=5 in fill, x=65 in stroke): 50% black over 50%-opaque blue
        // gives alpha = 0.5 + 0.5*(1 - 0.5) = 0.75 (~191), premul blue = 0.5*(1 - 0.5) = 0.25 (~64).
        let fill_painted = px_rgba(5, 30);
        let stroke_painted = px_rgba(65, 30);
        assert!(
            (187..=195).contains(&fill_painted[3]) && fill_painted[0] <= 2,
            "fill painted half must composite once (~alpha 191, no red leak), got {fill_painted:?}"
        );
        assert!(
            (187..=195).contains(&stroke_painted[3]) && stroke_painted[0] <= 2,
            "stroke painted half must composite once (~alpha 191, no red leak), got {stroke_painted:?}"
        );
    }

    #[test]
    fn tiling_pattern_five_overlapping_translucent_copies_preserve_live_backdrop_in_svg() {
        // Cell 20×20: opaque Normal red on left [0..10], opaque white Multiply
        // on right [10..20]. Painted FIVE times overlapping at 50% opacity over
        // opaque blue (both fills and strokes, plus a third row with intervening
        // solid draws between each copy forcing 5 separate snapshots). The right
        // half must stay pure blue (0, 0, 255) throughout — never mixing white
        // onto blue (~(128, 128, 255)).
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        cell.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 10.0, 20.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        cell.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Multiply,
            isolated: true,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 20.0, 20.0),
            alpha: 1.0,
            mask: None,
        });
        cell.push(RenderCommand::FillPath {
            path: rect_path(10.0, 0.0, 20.0, 20.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        cell.push(RenderCommand::PopBlendGroup);
        let tiling = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 20.0, 20.0),
            x_step: 20.0,
            y_step: 20.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 120.0, 120.0));
        // Opaque blue backdrop across the whole page.
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 120.0, 120.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        // Row 1 (y=[80..120]): five overlapping 50%-opacity fills on x=[0..40].
        for _ in 0..5 {
            dl.push(RenderCommand::FillPath {
                path: rect_path(0.0, 80.0, 40.0, 120.0),
                rule: FillRule::NonZero,
                paint: Paint::Tiling(std::sync::Arc::clone(&tiling)),
                alpha: 0.5,
                overprint: None,
            });
        }
        // Row 2 (y=[40..80]): five overlapping 50%-opacity strokes covering x=[0..40].
        for _ in 0..5 {
            let mut line = Path::new();
            line.move_to(Point::new(20.0, 40.0));
            line.line_to(Point::new(20.0, 80.0));
            dl.push(RenderCommand::StrokePath {
                path: line,
                style: StrokeStyle {
                    width: 40.0,
                    ..StrokeStyle::default()
                },
                paint: Paint::Tiling(std::sync::Arc::clone(&tiling)),
                alpha: 0.5,
                overprint: None,
            });
        }
        // Row 3 (y=[0..40]): five overlapping 50%-opacity fills interleaved with
        // an unrelated solid draw (forcing 5 separate sequential snapshots).
        for i in 0..5 {
            dl.push(RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 40.0, 40.0),
                rule: FillRule::NonZero,
                paint: Paint::Tiling(std::sync::Arc::clone(&tiling)),
                alpha: 0.5,
                overprint: None,
            });
            dl.push(RenderCommand::FillPath {
                path: rect_path(80.0 + i as f64 * 4.0, 5.0, 82.0 + i as f64 * 4.0, 10.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::rgb(0.0, 1.0, 0.0)),
                alpha: 1.0,
                overprint: None,
            });
        }

        let svg = display_list_to_svg(
            &dl,
            &FontCache::new(),
            &ImageCache::new(),
            &SvgOptions::default(),
        );
        let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default())
            .expect("valid SVG");
        let mut pixmap = resvg::tiny_skia::Pixmap::new(120, 120).expect("pixmap");
        let sx = 120.0 / tree.size().width();
        let sy = 120.0 / tree.size().height();
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(sx, sy),
            &mut pixmap.as_mut(),
        );
        let px = |x: u32, y_pdf: u32| -> [u8; 3] {
            let p = pixmap.pixel(x, 119 - y_pdf).expect("in bounds");
            [p.red(), p.green(), p.blue()]
        };

        // Right half (x=15): white Multiply over blue must remain pure blue (0,0,255) across all 5 copies.
        for (label, y_pdf) in [
            ("5 overlapping fills", 100),
            ("5 overlapping strokes", 60),
            ("5 interleaved snapshots", 20),
        ] {
            let right_px = px(15, y_pdf);
            assert!(
                right_px[0] <= 2 && right_px[1] <= 2 && right_px[2] >= 253,
                "{label}: white Multiply right half must stay pure blue (NOT ~(128,128,255)), got {right_px:?}"
            );
            // Left half (x=5): 5 layers of 50% red over blue -> (1 - 0.5^5)*255 ≈ 247 red, ~8 blue.
            let left_px = px(5, y_pdf);
            assert!(
                (240..=255).contains(&left_px[0]) && left_px[1] <= 2 && left_px[2] <= 15,
                "{label}: 5x 50% red left half over blue must be ~(247, 0, 8), got {left_px:?}"
            );
        }
    }

    #[test]
    fn tiling_pattern_dense_pass_budget_fallback_preserves_repeated_multiply() {
        // Cell B exceeding the 1,024 per-copy pass budget (175 alternating
        // Multiply/Darken spanning 6 horizontal steps = 1,050 > 1,024), with
        // an opaque 50%-gray Multiply mark followed immediately by an opaque
        // 50%-gray Screen mark plus a trailing black detail confined to step 0.
        // Six Multiply–Screen pairs over blue yield ~170 (NOT ~251 from running
        // six Multiplies followed by six Screens), and the detail stays black.
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 12.0, 2.0));
        for k in 0..175 {
            let (mode, color) = if k == 20 {
                (BlendMode::Multiply, Color::gray(0.5))
            } else if k == 21 {
                (BlendMode::Screen, Color::gray(0.5))
            } else if k % 2 == 0 {
                (BlendMode::Multiply, Color::white())
            } else {
                (BlendMode::Darken, Color::white())
            };
            cell.push(RenderCommand::PushBlendGroup {
                blend_mode: mode,
                isolated: true,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 12.0, 2.0),
                alpha: 1.0,
                mask: None,
            });
            cell.push(RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 12.0, 2.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(color),
                alpha: 1.0,
                overprint: None,
            });
            cell.push(RenderCommand::PopBlendGroup);
        }
        cell.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Multiply,
            isolated: true,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 12.0, 2.0),
            alpha: 1.0,
            mask: None,
        });
        cell.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 1.0, 2.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
        });
        cell.push(RenderCommand::PopBlendGroup);
        let tiling = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 12.0, 2.0),
            x_step: 2.0,
            y_step: 2.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 20.0, 20.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 20.0, 20.0),
            rule: FillRule::NonZero,
            paint: Paint::Tiling(tiling),
            alpha: 1.0,
            overprint: None,
        });

        let svg = display_list_to_svg(
            &dl,
            &FontCache::new(),
            &ImageCache::new(),
            &SvgOptions::default(),
        );
        let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default())
            .expect("valid SVG");
        // Rasterize at exact 4 px/pt (80×80: `80 / (20 * 96/72) == 3.0` exact) so
        // the 2pt×2pt cell maps to an exact 8×8 pixel tile with zero resvg
        // `ceil(2.0000001) = 3` pattern-resampling blur across the 1pt stripe edge.
        let mut pixmap = resvg::tiny_skia::Pixmap::new(80, 80).expect("pixmap");
        let sx = 80.0 / tree.size().width();
        let sy = 80.0 / tree.size().height();
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(sx, sy),
            &mut pixmap.as_mut(),
        );
        let px = |x: u32, y_pdf: u32| -> [u8; 3] {
            let p = pixmap.pixel(x, 79 - y_pdf).expect("in bounds");
            [p.red(), p.green(), p.blue()]
        };

        // Even stripe x_pdf in [10..11] -> px x=41: black detail.
        let detail = px(41, 40);
        assert!(
            detail[0] <= 2 && detail[1] <= 2 && detail[2] <= 2,
            "SVG dense pass-budget fallback must preserve trailing black detail, got {detail:?}"
        );
        // Odd stripe x_pdf in [11..12] -> px x=45: 6x Multiply–Screen pairs over blue -> ~[170,170,170] (NOT ~251).
        let repeated = px(45, 40);
        assert!(
            (165..=175).contains(&repeated[2])
                && (165..=175).contains(&repeated[0])
                && (165..=175).contains(&repeated[1]),
            "SVG dense pass-budget fallback must preserve 6x Multiply–Screen copy order (~[170,170,170], NOT blue ~251), got {repeated:?}"
        );
    }

    #[test]
    fn tiling_pattern_passthrough_form_and_interleaved_draws_preserve_backdrop_in_svg() {
        // Red Normal left half [0..10], white Multiply right half [10..20].
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        cell.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 10.0, 20.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        cell.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Multiply,
            isolated: true,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 20.0, 20.0),
            alpha: 1.0,
            mask: None,
        });
        cell.push(RenderCommand::FillPath {
            path: rect_path(10.0, 0.0, 20.0, 20.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        cell.push(RenderCommand::PopBlendGroup);
        let tiling = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 20.0, 20.0),
            x_step: 20.0,
            y_step: 20.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 120.0, 120.0));
        // Opaque blue backdrop across the entire page.
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 120.0, 120.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });

        // 1. Passthrough form (full-opacity, non-isolated Normal, no soft mask)
        //    wrapping a 50%-opacity patterned fill (y=[80..120]) and stroke (y=[40..80]).
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Normal,
            isolated: false,
            knockout: false,
            bounds: Rect::new(0.0, 40.0, 120.0, 120.0),
            alpha: 1.0,
            mask: None,
        });
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 80.0, 40.0, 120.0),
            rule: FillRule::NonZero,
            paint: Paint::Tiling(std::sync::Arc::clone(&tiling)),
            alpha: 0.5,
            overprint: None,
        });
        let mut line = Path::new();
        line.move_to(Point::new(20.0, 40.0));
        line.line_to(Point::new(20.0, 80.0));
        dl.push(RenderCommand::StrokePath {
            path: line,
            style: StrokeStyle {
                width: 40.0,
                ..StrokeStyle::default()
            },
            paint: Paint::Tiling(std::sync::Arc::clone(&tiling)),
            alpha: 0.5,
            overprint: None,
        });
        dl.push(RenderCommand::PopBlendGroup);

        // 2. 70 interleaved solid + translucent patterned fills (would exceed
        //    MAX_BACKDROP_SNAPSHOTS = 64 if each solid draw forced a new snapshot),
        //    followed by a 71st fill on x=[0..40], y=[0..40] over clean blue.
        for i in 0..70 {
            let x = 60.0 + (i % 10) as f64 * 4.0;
            dl.push(RenderCommand::FillPath {
                path: rect_path(x, 0.0, x + 2.0, 10.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::rgb(0.0, 1.0, 0.0)),
                alpha: 1.0,
                overprint: None,
            });
            dl.push(RenderCommand::FillPath {
                path: rect_path(x, 12.0, x + 2.0, 20.0),
                rule: FillRule::NonZero,
                paint: Paint::Tiling(std::sync::Arc::clone(&tiling)),
                alpha: 0.5,
                overprint: None,
            });
        }
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 40.0, 40.0),
            rule: FillRule::NonZero,
            paint: Paint::Tiling(tiling),
            alpha: 0.5,
            overprint: None,
        });

        let svg = display_list_to_svg(
            &dl,
            &FontCache::new(),
            &ImageCache::new(),
            &SvgOptions::default(),
        );
        let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default())
            .expect("valid SVG");
        let mut pixmap = resvg::tiny_skia::Pixmap::new(120, 120).expect("pixmap");
        let sx = 120.0 / tree.size().width();
        let sy = 120.0 / tree.size().height();
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(sx, sy),
            &mut pixmap.as_mut(),
        );
        let px = |x: u32, y_pdf: u32| -> [u8; 3] {
            let p = pixmap.pixel(x, 119 - y_pdf).expect("in bounds");
            [p.red(), p.green(), p.blue()]
        };

        for (label, y_pdf) in [
            ("passthrough form fill", 100),
            ("passthrough form stroke", 60),
            ("71st interleaved fill past old snapshot cap", 20),
        ] {
            let right_px = px(15, y_pdf);
            assert!(
                right_px[0] <= 2 && right_px[1] <= 2 && right_px[2] >= 253,
                "{label}: white Multiply right half over blue must stay pure blue (NOT ~(128,128,255)), got {right_px:?}"
            );
            let left_px = px(5, y_pdf);
            assert!(
                (124..=132).contains(&left_px[0])
                    && left_px[1] <= 2
                    && (124..=132).contains(&left_px[2]),
                "{label}: 50% red left half over blue must be ~(128,0,128), got {left_px:?}"
            );
        }
    }

    #[test]
    fn tiling_pattern_direct_glyph_run_renders_visible_text_in_svg() {
        // Exercise the exact direct `RenderCommand::DrawGlyphRun` with `Paint::Tiling`
        // (two opaque black Multiply segments) fixture via SVG export + resvg.
        let mut objects: Vec<Vec<u8>> = Vec::new();
        let push_stream = |dict: &str, data: &[u8]| {
            let mut out = Vec::new();
            out.extend_from_slice(
                format!("<< {dict} /Length {} >>\nstream\n", data.len()).as_bytes(),
            );
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let gs_mul = b"<< /Type /ExtGState /BM /Multiply >>".to_vec();
        let cell = push_stream(
            "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
             /BBox [0 0 10 10] /XStep 10 /YStep 10 \
             /Resources << /ExtGState << /GM 7 0 R >> >>",
            b"/GM gs 0 0 0 rg 0 0 10 10 re f\n/GM gs 0 0 0 rg 0 0 10 10 re f",
        );
        let font = b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec();
        let content = push_stream(
            "",
            b"/Pattern cs /P0 scn 0 0 10 10 re f\n\
              0 0 0 rg BT /F1 60 Tf 70 25 Td (O) Tj ET",
        );
        objects.push(b"<< /Type /Catalog /Pages 2 0 R >>".to_vec());
        objects.push(b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec());
        objects.push(
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 120 120] /Contents 4 0 R \
              /Resources << /Pattern << /P0 5 0 R >> /Font << /F1 6 0 R >> >> >>"
                .to_vec(),
        );
        objects.push(content);
        objects.push(cell);
        objects.push(font);
        objects.push(gs_mul);

        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = vec![0usize];
        for (i, body) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            pdf.extend_from_slice(body);
            pdf.extend_from_slice(b"\nendobj\n");
        }
        let xref = pdf.len();
        pdf.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for &off in &offsets[1..] {
            pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );

        let doc = zpdf::PdfDocument::open(pdf).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let content_bytes = doc.page_content_bytes(&page).expect("content bytes");
        let mut images = ImageCache::new();
        let dl = zpdf_content::interpreter::ContentInterpreter::new(page.media_box)
            .with_fonts(&mut fonts)
            .with_document(doc.file(), &page.resources)
            .with_images(&mut images)
            .interpret(&content_bytes);
        let tiling_paint = dl
            .commands
            .iter()
            .find_map(|c| match c {
                RenderCommand::FillPath {
                    paint: p @ Paint::Tiling(_),
                    ..
                } => Some(p.clone()),
                _ => None,
            })
            .expect("tiling paint");
        let mut solid_glyph_dl = DisplayList::new(page.media_box);
        let mut tiling_glyph_dl = DisplayList::new(page.media_box);
        for cmd in &dl.commands {
            if let RenderCommand::DrawGlyphRun(run) = cmd {
                solid_glyph_dl.push(RenderCommand::DrawGlyphRun(run.clone()));
                let mut tiled = run.clone();
                tiled.paint = tiling_paint.clone();
                tiling_glyph_dl.push(RenderCommand::DrawGlyphRun(tiled));
            }
        }
        assert!(
            !tiling_glyph_dl.commands.is_empty(),
            "expected DrawGlyphRun"
        );

        let render_svg = |list: &DisplayList| {
            let svg = display_list_to_svg(list, &fonts, &images, &SvgOptions::default());
            let tree =
                resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default())
                    .expect("valid SVG");
            let mut pixmap = resvg::tiny_skia::Pixmap::new(120, 120).expect("pixmap");
            let sx = 120.0 / tree.size().width();
            let sy = 120.0 / tree.size().height();
            resvg::render(
                &tree,
                resvg::tiny_skia::Transform::from_scale(sx, sy),
                &mut pixmap.as_mut(),
            );
            pixmap
        };
        let solid_pm = render_svg(&solid_glyph_dl);
        let tiling_pm = render_svg(&tiling_glyph_dl);

        let mut dark_pixels = 0usize;
        let mut aa_pixels = 0usize;
        for y in 20..110u32 {
            for x in 65..118u32 {
                let r_ref = solid_pm.pixel(x, y).unwrap().red();
                let r_pat = tiling_pm.pixel(x, y).unwrap().red();
                if r_pat < 30 {
                    dark_pixels += 1;
                }
                if (60..=200).contains(&r_ref) {
                    aa_pixels += 1;
                    assert!(
                        (r_pat as i32 - r_ref as i32).abs() <= 4,
                        "SVG DrawGlyphRun AA pixel ({x},{y}) diverged: solid={r_ref}, tiling={r_pat}"
                    );
                }
            }
        }
        assert!(
            dark_pixels >= 100,
            "SVG DrawGlyphRun with Paint::Tiling must render visible 'O', found {dark_pixels} dark pixels"
        );
        assert!(
            aa_pixels >= 10,
            "expected anti-aliased edge pixels in SVG 'O', found {aa_pixels}"
        );
    }
}
