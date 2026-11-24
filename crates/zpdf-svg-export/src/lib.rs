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
        prev_bounds: Option<SvgRect>,
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
        prev_non_isolated_depth: usize,
    },
    NonIsolatedFilterGroup {
        blend_mode: BlendMode,
        alpha: f32,
        mask_id: Option<u32>,
    },
}

struct SvgWriter<'a> {
    defs: String,
    out: String,
    has_opaque_background: bool,
    blend_depth: usize,
    isolated_depth: usize,
    non_isolated_filter_depth: usize,
    backdrop_snapshots: usize,
    /// Cumulative multiplicative fan-out across backdrop `<use>` snapshots that
    /// reference the prior page more than once; capped by [`MAX_BACKDROP_FANOUT`]
    /// so sequential dense patterns cannot trigger exponential `<use>` expansion.
    backdrop_fanout: usize,
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
    /// Conservative SVG-space bounding box of the active clip chain (used to
    /// scope dense SVG `<filter>` subregions to the affected area).
    clip_bounds: Option<SvgRect>,
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
/// overlapping affine backdrop-dependent tiling fill or stroke before
/// collapsing into the order-preserving 3-seed affine filter fallback.
const MAX_SVG_TILING_BLEND_PASSES: usize = 1024;
/// Total pass ceiling across bounded filter chunks for overlapping non-affine
/// (or opacity-grouped) backdrop-dependent cells that must keep exact per-copy
/// draw order against the live backdrop rather than using the affine shortcut.
const MAX_SVG_NON_AFFINE_TOTAL_PASSES: usize = 65_536;
/// Cap on per-copy step evaluations inside the SVG affine tile collapse
/// (`p_trans`, `f(0)`, `f(1)`), matching the `MAX_TILING_OVERLAP_COPIES × MAX_SEGMENTS` budget.
const MAX_AFFINE_TILE_PASSES: usize = 65_536;
/// Cap on sequential page-backdrop `<g id="b...">` snapshots when filter
/// batches reach [`MAX_BACKDROP_FILTER_PASSES`] or [`MAX_FILTER_INTERMEDIATE_PIXELS`].
const MAX_BACKDROP_SNAPSHOTS: usize = 64;
/// Cap on total filter primitives batched inside a single SVG `<filter>` chain.
const MAX_BACKDROP_FILTER_PASSES: usize = 8192;
/// Peak intermediate pixel budget for one SVG `<filter>` element at 1× SVG
/// user units (72 dpi): 16M RGBA pixels = 64 MiB. Renderers such as `resvg`
/// retain every primitive result in a `<filter>` until it completes, so filter
/// batches and chunks enforce `primitives * region_pixels <= MAX_FILTER_INTERMEDIATE_PIXELS`.
const MAX_FILTER_INTERMEDIATE_PIXELS: u64 = 16 * 1024 * 1024;
/// Cap on multiplicative `<use>` fan-out for multi-reference backdrop snapshots
/// (scoped filters / in-place blended groups), preventing exponential expansion.
const MAX_BACKDROP_FANOUT: usize = 32;
/// Flat (non-`OpacityGroup`) multi-pass tiling plans above this op count are
/// evaluated in-place via sibling `mix-blend-mode` rects (0 filter primitives
/// on opaque backdrops, 5 on translucent backdrops) instead of retaining one
/// `<feImage>` buffer per pass inside an SVG `<filter>`.
const FLAT_BLEND_GROUP_OP_THRESHOLD: usize = 64;

/// Integer-snapped SVG user-space bounding box clamped to the page viewport.
#[derive(Clone, Copy, Debug)]
struct SvgRect {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
}

impl SvgRect {
    fn full_page(w: f64, h: f64) -> Self {
        Self {
            x0: 0.0,
            y0: 0.0,
            x1: w.max(1.0),
            y1: h.max(1.0),
        }
    }

    fn from_bounds_clamped(
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
        pad: f64,
        page_w: f64,
        page_h: f64,
    ) -> Option<Self> {
        if ![x0, y0, x1, y1, pad].iter().all(|v| v.is_finite()) {
            return None;
        }
        let nx0 = (x0 - pad).floor().clamp(0.0, page_w);
        let ny0 = (y0 - pad).floor().clamp(0.0, page_h);
        let nx1 = (x1 + pad).ceil().clamp(0.0, page_w);
        let ny1 = (y1 + pad).ceil().clamp(0.0, page_h);
        (nx1 > nx0 && ny1 > ny0).then_some(Self {
            x0: nx0,
            y0: ny0,
            x1: nx1,
            y1: ny1,
        })
    }

    fn width(self) -> f64 {
        (self.x1 - self.x0).max(0.0)
    }

    fn height(self) -> f64 {
        (self.y1 - self.y0).max(0.0)
    }

    fn pixels(self) -> u64 {
        let rw = self.width().ceil().max(1.0) as u64;
        let rh = self.height().ceil().max(1.0) as u64;
        rw.saturating_mul(rh).max(1)
    }

    fn intersect(self, other: Self) -> Option<Self> {
        let x0 = self.x0.max(other.x0);
        let y0 = self.y0.max(other.y0);
        let x1 = self.x1.min(other.x1);
        let y1 = self.y1.min(other.y1);
        (x1 > x0 && y1 > y0).then_some(Self { x0, y0, x1, y1 })
    }

    fn union(self, other: Self) -> Self {
        Self {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }

    fn is_strict_subregion(self, page_w: f64, page_h: f64) -> bool {
        self.width() * self.height() < page_w * page_h * 0.9
    }
}

/// One operation inside an SVG backdrop-dependent tiling pattern plan.
#[derive(Clone)]
enum TilingPlanOp {
    /// Composite tiling pattern `p{pat_id}` onto the running backdrop using `blend_mode`.
    Pattern { pat_id: u32, blend_mode: BlendMode },
    /// Save the running backdrop (and any active group-alpha track) before
    /// evaluating a cell-internal non-isolated `OpacityGroup`.
    PushOpacityGroup,
    /// Mix the completed cell-internal `OpacityGroup` back into the saved
    /// backdrop with constant `alpha` and optional mask pattern `p{mask_pat_id}`.
    PopOpacityGroup {
        alpha: f32,
        mask_pat_id: Option<u32>,
    },
}

enum TilingPassPlan {
    Ops(Vec<TilingPlanOp>),
    Affine { t_id: u32, k_id: u32, mk_id: u32 },
}

enum BackdropStep {
    /// Intervening Normal-blended content wrapped in `<g id="ov{id}">` and
    /// composited with Porter-Duff `over` onto the running filter backdrop.
    Overlay(u32),
    /// Intervening non-Normal blended element/group wrapped in `<g id="ov{id}">`
    /// and composited via `<feBlend mode="..."/>` onto the running backdrop.
    BlendOverlay { ov_id: u32, blend_mode: BlendMode },
    /// Save the running filter backdrop (and start a fresh transparent group
    /// alpha track when `blend_mode != Normal`) before evaluating a
    /// non-isolated group against the live parent.
    PushNonIsolatedGroup { blend_mode: BlendMode },
    /// Merge the completed non-isolated group back into the saved backdrop:
    /// for `Normal`, mixes via `weight_id` (`cur * w + saved * (1 - w)`); for
    /// non-`Normal` (ISO 32000-1 §11.4.5), removes `(1 - α_g) * saved` from
    /// `cur`, applies optional `weight_id`, and blends onto `saved`.
    PopNonIsolatedGroup {
        blend_mode: BlendMode,
        weight_id: Option<u32>,
    },
    /// Multi-pass / opacity-grouped tiling pattern evaluated against the
    /// running backdrop and mixed once through `<g id="s{shape_id}">`.
    MultiPass {
        ops: Vec<TilingPlanOp>,
        shape_id: u32,
    },
    /// Exact 3-seed affine collapse evaluated from transparent-seeded
    /// `p{t_id}` (`(T, a_t)`), black-seeded `p{k_id}` (`K`), and white-seeded
    /// `p{mk_id}` (`K + M`), mixed once through `<g id="s{shape_id}">`.
    Affine {
        t_id: u32,
        k_id: u32,
        mk_id: u32,
        shape_id: u32,
    },
}

struct PendingBackdrop {
    bg_id: u32,
    steps: Vec<BackdropStep>,
    total_primitives: usize,
    region: SvgRect,
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
            non_isolated_filter_depth: 0,
            backdrop_snapshots: 0,
            backdrop_fanout: 1,
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
            clip_bounds: None,
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
                        prev_bounds: self.clip_bounds,
                    };
                    self.push_clip(path, rule);
                    frames.push(frame);
                }
                RenderCommand::PushClipStroke { path, style } => {
                    let frame = Frame::Clip {
                        prev_clip: self.clip,
                        prev_mask: self.stroke_mask,
                        prev_bounds: self.clip_bounds,
                    };
                    self.push_clip_stroke(path, style);
                    frames.push(frame);
                }
                RenderCommand::PopClip => match frames.last() {
                    Some(Frame::Clip {
                        prev_clip,
                        prev_mask,
                        prev_bounds,
                    }) => {
                        self.clip = *prev_clip;
                        self.stroke_mask = *prev_mask;
                        self.clip_bounds = *prev_bounds;
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
                    let is_non_isolated_group =
                        !*isolated && (eff_alpha < 1.0 || mask_id.is_some() || blend.is_some());
                    if depth == 0
                        && is_non_isolated_group
                        && (self.pending_backdrop.is_some() || !self.out.is_empty())
                        && self.enqueue_backdrop_step(
                            BackdropStep::PushNonIsolatedGroup {
                                blend_mode: *blend_mode,
                            },
                            if *blend_mode != BlendMode::Normal {
                                6
                            } else {
                                4
                            },
                            None,
                            false,
                        )
                    {
                        self.blend_depth += 1;
                        self.non_isolated_filter_depth += 1;
                        if *blend_mode != BlendMode::Normal {
                            self.isolated_depth += 1;
                        }
                        frames.push(Frame::NonIsolatedFilterGroup {
                            blend_mode: *blend_mode,
                            alpha: eff_alpha,
                            mask_id,
                        });
                    } else if !isolates {
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
                            prev_non_isolated_depth: 0,
                        });
                    } else {
                        let prev_out = std::mem::take(&mut self.out);
                        let prev_pending = self.pending_backdrop.take();
                        let prev_non_isolated_depth =
                            std::mem::replace(&mut self.non_isolated_filter_depth, 0);
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
                            prev_non_isolated_depth,
                        });
                    }
                }
                RenderCommand::PopBlendGroup => match frames.last() {
                    Some(Frame::NonIsolatedFilterGroup { .. }) => {
                        if let Some(Frame::NonIsolatedFilterGroup {
                            blend_mode,
                            alpha,
                            mask_id,
                        }) = frames.pop()
                        {
                            self.close_non_isolated_filter_group(blend_mode, alpha, mask_id);
                        }
                    }
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
                            prev_non_isolated_depth,
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
                                prev_non_isolated_depth,
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
                Frame::NonIsolatedFilterGroup {
                    blend_mode,
                    alpha,
                    mask_id,
                } => {
                    self.close_non_isolated_filter_group(blend_mode, alpha, mask_id);
                }
                Frame::Blend {
                    open,
                    isolates,
                    blend_mode,
                    isolated,
                    alpha,
                    mask_id,
                    prev_out,
                    prev_pending,
                    prev_non_isolated_depth,
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
                        prev_non_isolated_depth,
                        depth,
                    );
                }
                Frame::Clip {
                    prev_clip,
                    prev_mask,
                    prev_bounds,
                } => {
                    self.clip = prev_clip;
                    self.stroke_mask = prev_mask;
                    self.clip_bounds = prev_bounds;
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
        let saved_non_isolated = std::mem::replace(&mut self.non_isolated_filter_depth, 0);
        let saved_x0 = self.x0;
        let saved_y1 = self.y1;
        let saved_scale = self.scale;
        let saved_w = self.page_width;
        let saved_h = self.page_height;
        let saved_blend_depth = std::mem::replace(&mut self.blend_depth, 0);
        let saved_isolated_depth = std::mem::replace(&mut self.isolated_depth, 0);
        let (saved_clip, saved_mask, saved_clip_bounds) = (
            self.clip.take(),
            self.stroke_mask.take(),
            self.clip_bounds.take(),
        );
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
        self.non_isolated_filter_depth = saved_non_isolated;
        self.x0 = saved_x0;
        self.y1 = saved_y1;
        self.scale = saved_scale;
        self.page_width = saved_w;
        self.page_height = saved_h;
        self.blend_depth = saved_blend_depth;
        self.isolated_depth = saved_isolated_depth;
        self.clip = saved_clip;
        self.stroke_mask = saved_mask;
        self.clip_bounds = saved_clip_bounds;
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
        let saved_non_isolated = std::mem::replace(&mut self.non_isolated_filter_depth, 0);
        let saved_x0 = self.x0;
        let saved_y1 = self.y1;
        let saved_scale = self.scale;
        let saved_w = self.page_width;
        let saved_h = self.page_height;
        let saved_blend_depth = std::mem::replace(&mut self.blend_depth, 0);
        let saved_isolated_depth = std::mem::replace(&mut self.isolated_depth, 0);
        let (saved_clip, saved_mask, saved_clip_bounds) = (
            self.clip.take(),
            self.stroke_mask.take(),
            self.clip_bounds.take(),
        );
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
        self.non_isolated_filter_depth = saved_non_isolated;
        self.x0 = saved_x0;
        self.y1 = saved_y1;
        self.scale = saved_scale;
        self.page_width = saved_w;
        self.page_height = saved_h;
        self.blend_depth = saved_blend_depth;
        self.isolated_depth = saved_isolated_depth;
        self.clip = saved_clip;
        self.stroke_mask = saved_mask;
        self.clip_bounds = saved_clip_bounds;
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
        remaining: &mut usize,
        exhausted: &mut bool,
        depth: u8,
    ) {
        for step in steps {
            if *exhausted || self.check_over_budget() {
                break;
            }
            match step {
                zpdf_display_list::TilingBlendStep::Segment(seg) => {
                    if !seg.intersects_period(tiling, di, dj) {
                        continue;
                    }
                    if *remaining == 0 {
                        *exhausted = true;
                        break;
                    }
                    *remaining -= 1;
                    if let Some(css_name) = blend_css(seg.blend_mode) {
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
                    steps: inner,
                    ..
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
                    self.emit_affine_steps(inner, tiling, di, dj, remaining, exhausted, depth);
                }
            }
        }
    }

    /// Order-preserving bounded fallback for affine overlapping
    /// backdrop-dependent cells exceeding [`MAX_SVG_TILING_BLEND_PASSES`]:
    /// evaluate all `(dj, di)` overlap copies in exact copy order and all
    /// intersecting steps in exact draw order onto transparent (`p_t = (T, a_t)`),
    /// black-seeded (`p_k = K`), and white-seeded (`p_mk = K + M`) `<pattern>`
    /// defs so the page `<filter>` computes the exact premultiplied affine map
    /// across arbitrary backdrop alpha without reordering copies. Returns `None`
    /// without caching if the pass or wall-clock budget is exhausted.
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
        let key_t = make_key(usize::MAX - 3);
        let key_k = make_key(usize::MAX - 1);
        let key_mk = make_key(usize::MAX - 2);
        if let (Some(&(_, t_id)), Some(&(_, k_id)), Some(&(_, mk_id))) = (
            self.pattern_defs.get(&key_t),
            self.pattern_defs.get(&key_k),
            self.pattern_defs.get(&key_mk),
        ) {
            return Some((t_id, k_id, mk_id));
        }

        let overlap = tiling.overlap_copy_ranges();
        let saved_out = std::mem::take(&mut self.out);
        let saved_pending = self.pending_backdrop.take();
        let saved_non_isolated = std::mem::replace(&mut self.non_isolated_filter_depth, 0);
        let saved_x0 = self.x0;
        let saved_y1 = self.y1;
        let saved_scale = self.scale;
        let saved_w = self.page_width;
        let saved_h = self.page_height;
        let saved_blend_depth = std::mem::replace(&mut self.blend_depth, 0);
        let saved_isolated_depth = std::mem::replace(&mut self.isolated_depth, 0);
        let (saved_clip, saved_mask, saved_clip_bounds) = (
            self.clip.take(),
            self.stroke_mask.take(),
            self.clip_bounds.take(),
        );
        self.scale = g.cell_scale;
        self.page_width = g.tile_w;
        self.page_height = g.tile_h;

        let mut remaining = MAX_AFFINE_TILE_PASSES;
        let mut exhausted = false;
        'copies: for dj in overlap.dj.clone() {
            for di in overlap.di.clone() {
                if exhausted || self.check_over_budget() {
                    break 'copies;
                }
                if !zpdf_display_list::TilingBlendStep::any_intersects_period(steps, tiling, di, dj)
                {
                    continue;
                }
                self.x0 = g.bx0 - di as f64 * g.x_step;
                self.y1 = g.by0 + g.abs_y - dj as f64 * g.y_step;
                self.emit_affine_steps(
                    steps,
                    tiling,
                    di,
                    dj,
                    &mut remaining,
                    &mut exhausted,
                    depth,
                );
            }
        }
        self.flush_pending_backdrop();
        let blended_steps = std::mem::replace(&mut self.out, saved_out);
        self.pending_backdrop = saved_pending;
        self.non_isolated_filter_depth = saved_non_isolated;
        self.x0 = saved_x0;
        self.y1 = saved_y1;
        self.scale = saved_scale;
        self.page_width = saved_w;
        self.page_height = saved_h;
        self.blend_depth = saved_blend_depth;
        self.isolated_depth = saved_isolated_depth;
        self.clip = saved_clip;
        self.stroke_mask = saved_mask;
        self.clip_bounds = saved_clip_bounds;

        if exhausted || self.over_budget {
            tracing::warn!(
                limit = MAX_AFFINE_TILE_PASSES,
                "svg affine tiling sample exceeded budget; declining partial tile"
            );
            return None;
        }

        let t_id = self.fresh_id();
        let k_id = self.fresh_id();
        let mk_id = self.fresh_id();
        self.pattern_defs
            .insert(key_t, (std::sync::Arc::clone(&tiling.commands), t_id));
        self.pattern_defs
            .insert(key_k, (std::sync::Arc::clone(&tiling.commands), k_id));
        self.pattern_defs
            .insert(key_mk, (std::sync::Arc::clone(&tiling.commands), mk_id));

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

        self.write_pattern_def(t_id, &g, &blended_steps);
        self.write_pattern_def(k_id, &g, &body_k);
        self.write_pattern_def(mk_id, &g, &body_mk);
        Some((t_id, k_id, mk_id))
    }

    fn has_real_opacity_group(steps: &[zpdf_display_list::TilingBlendStep]) -> bool {
        steps.iter().any(|step| match step {
            zpdf_display_list::TilingBlendStep::Segment(_) => false,
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
                a < 1.0 || mask.is_some() || Self::has_real_opacity_group(inner)
            }
        })
    }

    fn ops_has_opacity_group(ops: &[TilingPlanOp]) -> bool {
        ops.iter().any(|op| {
            matches!(
                op,
                TilingPlanOp::PushOpacityGroup | TilingPlanOp::PopOpacityGroup { .. }
            )
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_tiling_step_ops(
        &mut self,
        tiling: &zpdf_display_list::TilingPattern,
        steps: &[zpdf_display_list::TilingBlendStep],
        overlap_shift: Option<(i64, i64)>,
        opaque_bg: bool,
        depth: u8,
        remaining: &mut usize,
        exhausted: &mut bool,
        ops: &mut Vec<TilingPlanOp>,
    ) {
        for step in steps {
            if *exhausted || self.check_over_budget() {
                return;
            }
            match step {
                zpdf_display_list::TilingBlendStep::Segment(seg) => {
                    if let Some((di, dj)) = overlap_shift {
                        if !seg.intersects_period(tiling, di, dj) {
                            continue;
                        }
                    }
                    if opaque_bg && seg.is_identity_on_opaque() {
                        continue;
                    }
                    if *remaining == 0 {
                        *exhausted = true;
                        return;
                    }
                    *remaining -= 1;
                    let copy_mode = if overlap_shift.is_none() {
                        seg.blend_mode
                    } else {
                        BlendMode::Normal
                    };
                    if let Some(pat_id) = self.emit_tiling_pattern_def_for(
                        tiling,
                        &seg.commands,
                        seg.segment_id,
                        overlap_shift,
                        copy_mode,
                        seg.bounds,
                        depth,
                    ) {
                        ops.push(TilingPlanOp::Pattern {
                            pat_id,
                            blend_mode: seg.blend_mode,
                        });
                    }
                }
                zpdf_display_list::TilingBlendStep::OpacityGroup {
                    alpha,
                    mask,
                    steps: inner,
                } => {
                    if let Some((di, dj)) = overlap_shift {
                        if mask
                            .as_ref()
                            .is_some_and(|m| !m.intersects_period(tiling, di, dj))
                            || !zpdf_display_list::TilingBlendStep::any_intersects_period(
                                inner, tiling, di, dj,
                            )
                        {
                            continue;
                        }
                    }
                    let a = if alpha.is_finite() {
                        alpha.clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    if a <= 0.0 {
                        continue;
                    }
                    if a >= 1.0 && mask.is_none() {
                        self.collect_tiling_step_ops(
                            tiling,
                            inner,
                            overlap_shift,
                            opaque_bg,
                            depth,
                            remaining,
                            exhausted,
                            ops,
                        );
                        continue;
                    }
                    let start_len = ops.len();
                    ops.push(TilingPlanOp::PushOpacityGroup);
                    self.collect_tiling_step_ops(
                        tiling,
                        inner,
                        overlap_shift,
                        opaque_bg,
                        depth,
                        remaining,
                        exhausted,
                        ops,
                    );
                    if *exhausted || self.over_budget {
                        return;
                    }
                    if ops.len() == start_len + 1 {
                        ops.pop();
                        continue;
                    }
                    let mask_pat_id = match mask {
                        Some(m_seg) => self.emit_tiling_pattern_def_for(
                            tiling,
                            &m_seg.commands,
                            m_seg.segment_id,
                            overlap_shift,
                            BlendMode::Normal,
                            m_seg.bounds,
                            depth,
                        ),
                        None => None,
                    };
                    if mask.is_some() && mask_pat_id.is_none() {
                        ops.truncate(start_len);
                        continue;
                    }
                    ops.push(TilingPlanOp::PopOpacityGroup {
                        alpha: a,
                        mask_pat_id,
                    });
                }
            }
        }
    }

    /// Resolve a tiling pattern into either per-segment / opacity-grouped
    /// [`TilingPlanOp`] steps or an order-preserving affine collapse
    /// `(t_id, k_id, mk_id)` when overlapping affine copies exceed
    /// [`MAX_SVG_TILING_BLEND_PASSES`].
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
                let has_opacity_group = Self::has_real_opacity_group(&steps);

                let uniform_assoc = zpdf_display_list::TilingBlendStep::uniform_blend_mode(&steps)
                    .filter(|&m| matches!(m, BlendMode::Multiply | BlendMode::Screen));
                if (total_segs > 1 || overlap_count > 1 || has_opacity_group)
                    && self.opaque_backdrop(depth)
                {
                    if let Some(mode) = uniform_assoc {
                        if let Some(id) =
                            self.emit_tiling_pattern_def_collapsed(tiling, &steps, mode, depth)
                        {
                            return TilingPassPlan::Ops(vec![TilingPlanOp::Pattern {
                                pat_id: id,
                                blend_mode: mode,
                            }]);
                        }
                    }
                }

                let mut remaining = MAX_SVG_NON_AFFINE_TOTAL_PASSES;
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
                        if depth == 0
                            && !has_opacity_group
                            && zpdf_display_list::TilingBlendStep::is_affine_in_backdrop(&steps)
                        {
                            if let Some((t_id, k_id, mk_id)) =
                                self.emit_tiling_pattern_def_affine(tiling, &steps, depth)
                            {
                                return TilingPassPlan::Affine { t_id, k_id, mk_id };
                            }
                        }
                        if self.over_budget {
                            return TilingPassPlan::Ops(Vec::new());
                        }
                        // Non-affine blend modes (e.g. Darken/Lighten) and cells
                        // with real OpacityGroups cannot use the flat affine
                        // pattern shortcut: preserve exact per-copy draw order
                        // across bounded filter chunks up to the total ceiling.
                        remaining = MAX_SVG_NON_AFFINE_TOTAL_PASSES;
                        true
                    } else {
                        true
                    }
                } else {
                    false
                };

                let opaque_bg = self.opaque_backdrop(depth);
                let mut ops = Vec::new();
                let mut exhausted = false;
                if separate_copies {
                    'copies: for dj in overlap.dj.clone() {
                        for di in overlap.di.clone() {
                            if exhausted || self.check_over_budget() {
                                break 'copies;
                            }
                            self.collect_tiling_step_ops(
                                tiling,
                                &steps,
                                Some((di, dj)),
                                opaque_bg,
                                depth,
                                &mut remaining,
                                &mut exhausted,
                                &mut ops,
                            );
                        }
                    }
                } else {
                    self.collect_tiling_step_ops(
                        tiling,
                        &steps,
                        None,
                        opaque_bg,
                        depth,
                        &mut remaining,
                        &mut exhausted,
                        &mut ops,
                    );
                }
                if self.over_budget {
                    return TilingPassPlan::Ops(Vec::new());
                }
                if !exhausted {
                    return TilingPassPlan::Ops(ops);
                }
                tracing::warn!(
                    limit = MAX_SVG_NON_AFFINE_TOTAL_PASSES,
                    "svg tiling blend plan exceeded total pass ceiling; falling back to complete isolated pattern"
                );
            } else {
                tracing::warn!(
                    "svg tiling pattern has unsegmentable backdrop-dependent marks; falling back to isolated pattern"
                );
            }
        }
        TilingPassPlan::Ops(
            self.emit_tiling_pattern_def_for(
                tiling,
                &tiling.commands,
                0,
                None,
                BlendMode::Normal,
                None,
                depth,
            )
            .map(|pat_id| {
                vec![TilingPlanOp::Pattern {
                    pat_id,
                    blend_mode: BlendMode::Normal,
                }]
            })
            .unwrap_or_default(),
        )
    }

    /// Emit filter primitives evaluating `ops` against `init_color` (and
    /// optionally tracking accumulated group alpha against `init_alpha`),
    /// returning the resulting `(color_result, alpha_result)` names.
    #[allow(clippy::too_many_arguments)]
    fn emit_filter_tiling_ops(
        defs: &mut String,
        w: f64,
        h: f64,
        pat_rects: &HashMap<u32, u32>,
        tag: usize,
        ops: &[TilingPlanOp],
        init_color: &str,
        init_alpha: Option<&str>,
    ) -> (String, Option<String>) {
        let mut acc = init_color.to_string();
        let mut acc_a = init_alpha.map(str::to_string);
        let mut stack: Vec<(String, Option<String>)> = Vec::new();
        for (p_idx, op) in ops.iter().enumerate() {
            match op {
                TilingPlanOp::Pattern { pat_id, blend_mode } => {
                    let rid = pat_rects[pat_id];
                    let pat_res = format!("p{tag}_{p_idx}");
                    let next_acc = format!("a{tag}_{p_idx}");
                    let _ = write!(defs, "<feImage href=\"#pr{rid}\" x=\"0\" y=\"0\" width=\"");
                    push_num(defs, w);
                    defs.push_str("\" height=\"");
                    push_num(defs, h);
                    let _ = writeln!(defs, "\" result=\"{pat_res}\"/>");
                    if let Some(name) = blend_css(*blend_mode) {
                        let _ = writeln!(
                            defs,
                            "<feBlend in=\"{pat_res}\" in2=\"{acc}\" mode=\"{name}\" result=\"{next_acc}\"/>"
                        );
                    } else {
                        let _ = writeln!(
                            defs,
                            "<feComposite in=\"{pat_res}\" in2=\"{acc}\" operator=\"over\" result=\"{next_acc}\"/>"
                        );
                    }
                    acc = next_acc;
                    if let Some(ref cur_a) = acc_a {
                        let next_a = format!("aa{tag}_{p_idx}");
                        let _ = writeln!(
                            defs,
                            "<feComposite in=\"{pat_res}\" in2=\"{cur_a}\" operator=\"over\" result=\"{next_a}\"/>"
                        );
                        acc_a = Some(next_a);
                    }
                }
                TilingPlanOp::PushOpacityGroup => {
                    stack.push((acc.clone(), acc_a.clone()));
                }
                TilingPlanOp::PopOpacityGroup { alpha, mask_pat_id } => {
                    let (s_acc, s_acc_a) =
                        stack.pop().unwrap_or_else(|| (acc.clone(), acc_a.clone()));
                    let next_acc = format!("a{tag}_{p_idx}");
                    if let Some(m_pat_id) = mask_pat_id {
                        let m_rid = pat_rects[m_pat_id];
                        let m_raw = format!("om{tag}_{p_idx}");
                        let _ = write!(
                            defs,
                            "<feImage href=\"#pr{m_rid}\" x=\"0\" y=\"0\" width=\""
                        );
                        push_num(defs, w);
                        defs.push_str("\" height=\"");
                        push_num(defs, h);
                        let _ = writeln!(defs, "\" result=\"{m_raw}\"/>");
                        let wt_name = if *alpha < 1.0 {
                            let m_scaled = format!("omw{tag}_{p_idx}");
                            let _ = write!(
                                defs,
                                "<feComponentTransfer in=\"{m_raw}\" result=\"{m_scaled}\"><feFuncA type=\"linear\" slope=\""
                            );
                            push_num(defs, *alpha as f64);
                            defs.push_str("\"/></feComponentTransfer>\n");
                            m_scaled
                        } else {
                            m_raw
                        };
                        let rw = format!("orw{tag}_{p_idx}");
                        let bw = format!("obw{tag}_{p_idx}");
                        let _ = writeln!(
                            defs,
                            "<feComposite in=\"{acc}\" in2=\"{wt_name}\" operator=\"in\" result=\"{rw}\"/>"
                        );
                        let _ = writeln!(
                            defs,
                            "<feComposite in=\"{s_acc}\" in2=\"{wt_name}\" operator=\"out\" result=\"{bw}\"/>"
                        );
                        let _ = writeln!(
                            defs,
                            "<feComposite in=\"{rw}\" in2=\"{bw}\" operator=\"arithmetic\" k1=\"0\" k2=\"1\" k3=\"1\" k4=\"0\" result=\"{next_acc}\"/>"
                        );
                        acc = next_acc;
                        if let (Some(ca), Some(sa)) = (acc_a.as_ref(), s_acc_a.as_ref()) {
                            let rwa = format!("orwa{tag}_{p_idx}");
                            let bwa = format!("obwa{tag}_{p_idx}");
                            let next_a = format!("aa{tag}_{p_idx}");
                            let _ = writeln!(
                                defs,
                                "<feComposite in=\"{ca}\" in2=\"{wt_name}\" operator=\"in\" result=\"{rwa}\"/>"
                            );
                            let _ = writeln!(
                                defs,
                                "<feComposite in=\"{sa}\" in2=\"{wt_name}\" operator=\"out\" result=\"{bwa}\"/>"
                            );
                            let _ = writeln!(
                                defs,
                                "<feComposite in=\"{rwa}\" in2=\"{bwa}\" operator=\"arithmetic\" k1=\"0\" k2=\"1\" k3=\"1\" k4=\"0\" result=\"{next_a}\"/>"
                            );
                            acc_a = Some(next_a);
                        }
                    } else {
                        let a = alpha.clamp(0.0, 1.0) as f64;
                        let inv = 1.0 - a;
                        let _ = write!(
                            defs,
                            "<feComposite in=\"{acc}\" in2=\"{s_acc}\" operator=\"arithmetic\" k1=\"0\" k2=\""
                        );
                        push_num(defs, a);
                        defs.push_str("\" k3=\"");
                        push_num(defs, inv);
                        let _ = writeln!(defs, "\" k4=\"0\" result=\"{next_acc}\"/>");
                        acc = next_acc;
                        if let (Some(ca), Some(sa)) = (acc_a.as_ref(), s_acc_a.as_ref()) {
                            let next_a = format!("aa{tag}_{p_idx}");
                            let _ = write!(
                                defs,
                                "<feComposite in=\"{ca}\" in2=\"{sa}\" operator=\"arithmetic\" k1=\"0\" k2=\""
                            );
                            push_num(defs, a);
                            defs.push_str("\" k3=\"");
                            push_num(defs, inv);
                            let _ = writeln!(defs, "\" k4=\"0\" result=\"{next_a}\"/>");
                            acc_a = Some(next_a);
                        }
                    }
                }
            }
        }
        (acc, acc_a)
    }

    fn full_page_rect(&self) -> SvgRect {
        SvgRect::full_page(self.page_width, self.page_height)
    }

    /// Conservative SVG-space bounding box of `path` expanded by `pad` user
    /// units and intersected with the active clip chain and page viewport.
    fn path_svg_bounds(&self, path: &Path, pad: f64) -> Option<SvgRect> {
        let (mut min_x, mut min_y) = (f64::INFINITY, f64::INFINITY);
        let (mut max_x, mut max_y) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
        let mut touch = |px: f64, py: f64| {
            let (sx, sy) = self.map(px, py);
            if coord_ok(sx) && coord_ok(sy) {
                min_x = min_x.min(sx);
                min_y = min_y.min(sy);
                max_x = max_x.max(sx);
                max_y = max_y.max(sy);
            }
        };
        for el in path {
            match el {
                PathElement::MoveTo(p) | PathElement::LineTo(p) => touch(p.x, p.y),
                PathElement::CurveTo(c1, c2, p) => {
                    touch(c1.x, c1.y);
                    touch(c2.x, c2.y);
                    touch(p.x, p.y);
                }
                PathElement::Close => {}
            }
        }
        if !(min_x.is_finite() && min_y.is_finite() && max_x.is_finite() && max_y.is_finite()) {
            return None;
        }
        let rect = SvgRect::from_bounds_clamped(
            min_x,
            min_y,
            max_x,
            max_y,
            pad,
            self.page_width,
            self.page_height,
        )?;
        match self.clip_bounds {
            Some(cb) => rect.intersect(cb),
            None => Some(rect),
        }
    }

    fn stroke_svg_pad(&self, style: &StrokeStyle) -> f64 {
        let width = (style.width as f64 * self.scale).max(0.05);
        let half = width * 0.5;
        let miter = match style.join {
            LineJoin::Miter => (style.miter_limit as f64).clamp(1.0, 20.0),
            LineJoin::Round | LineJoin::Bevel => 1.5,
        };
        (half * miter + 2.0).min(self.page_width.max(self.page_height))
    }

    fn tiling_op_filter_primitives(op: &TilingPlanOp, track_alpha: bool) -> usize {
        match op {
            TilingPlanOp::Pattern { .. } => {
                if track_alpha {
                    3
                } else {
                    2
                }
            }
            TilingPlanOp::PushOpacityGroup => 0,
            TilingPlanOp::PopOpacityGroup {
                mask_pat_id: None, ..
            } => {
                if track_alpha {
                    2
                } else {
                    1
                }
            }
            TilingPlanOp::PopOpacityGroup {
                mask_pat_id: Some(_),
                ..
            } => {
                if track_alpha {
                    8
                } else {
                    5
                }
            }
        }
    }

    fn tiling_ops_filter_primitives(ops: &[TilingPlanOp], track_alpha: bool) -> usize {
        ops.iter()
            .map(|op| Self::tiling_op_filter_primitives(op, track_alpha))
            .sum()
    }

    /// Maximum filter primitive count allowed in one `<filter>` covering
    /// `region` without exceeding [`MAX_FILTER_INTERMEDIATE_PIXELS`].
    fn max_filter_primitives_for_region(region: SvgRect) -> usize {
        ((MAX_FILTER_INTERMEDIATE_PIXELS / region.pixels()) as usize)
            .min(MAX_BACKDROP_FILTER_PASSES * 2)
    }

    fn write_filter_open_tag(defs: &mut String, fid: u32, region: SvgRect) {
        let _ = write!(
            defs,
            "<filter id=\"f{fid}\" filterUnits=\"userSpaceOnUse\" x=\""
        );
        push_num(defs, region.x0);
        defs.push_str("\" y=\"");
        push_num(defs, region.y0);
        defs.push_str("\" width=\"");
        push_num(defs, region.width());
        defs.push_str("\" height=\"");
        push_num(defs, region.height());
        defs.push_str("\" color-interpolation-filters=\"sRGB\">\n");
    }

    /// Emit the unmodified background outside a scoped filter subregion `region`:
    /// on opaque pages draw `#b{bg_id}` directly underneath (zero seam at any
    /// fractional scale); on translucent pages clip `#b{bg_id}` to the exact
    /// complement of `region` so translucent pixels outside aren't doubled.
    fn emit_scoped_bg_restore(&mut self, bg_id: u32, region: SvgRect) {
        if self.has_opaque_background && self.isolated_depth == 0 {
            let _ = writeln!(self.out, "<use href=\"#b{bg_id}\"/>");
        } else {
            let out_clip = self.fresh_id();
            let _ = write!(self.defs, "<clipPath id=\"c{out_clip}\"><path d=\"M0 0H");
            push_num(&mut self.defs, self.page_width);
            self.defs.push('V');
            push_num(&mut self.defs, self.page_height);
            self.defs.push_str("H0ZM");
            push_pair(&mut self.defs, region.x0, region.y0);
            self.defs.push('H');
            push_num(&mut self.defs, region.x1);
            self.defs.push('V');
            push_num(&mut self.defs, region.y1);
            self.defs.push('H');
            push_num(&mut self.defs, region.x0);
            self.defs
                .push_str("Z\" clip-rule=\"evenodd\"/></clipPath>\n");
            let _ = writeln!(
                self.out,
                "<use href=\"#b{bg_id}\" clip-path=\"url(#c{out_clip})\"/>"
            );
        }
    }

    /// Evaluate a dense flat (non-`OpacityGroup`) multi-pass tiling plan in
    /// O(1) peak raster memory by blending sibling `<rect style="mix-blend-mode:..."/>`
    /// elements in-place onto a single copy of the backdrop, then masking ONCE
    /// with `shape_body`:
    /// - Opaque backdrop: `<g mask="url(#m...)">` with ZERO `<filter>` primitives.
    /// - Translucent backdrop: one 5-primitive `<filter>` scoped to `affected`.
    fn emit_flat_blend_group_tiling_ops(
        &mut self,
        ops: &[TilingPlanOp],
        shape_body: &str,
        affected: SvgRect,
    ) -> bool {
        if self.non_isolated_filter_depth > 0 || ops.is_empty() || Self::ops_has_opacity_group(ops)
        {
            return false;
        }
        let w = self.page_width;
        let h = self.page_height;
        let opaque = self.has_opaque_background && self.isolated_depth == 0;
        let mut scoped = affected.is_strict_subregion(w, h);
        let refs = if opaque {
            2
        } else if scoped {
            if self.backdrop_fanout.saturating_mul(3) <= MAX_BACKDROP_FANOUT {
                3
            } else {
                scoped = false;
                2
            }
        } else {
            2
        };
        if self.backdrop_fanout.saturating_mul(refs) > MAX_BACKDROP_FANOUT {
            return false;
        }
        let reg = if scoped {
            affected
        } else {
            self.full_page_rect()
        };
        if !opaque && Self::max_filter_primitives_for_region(reg) < 5 {
            return false;
        }
        self.flush_pending_backdrop();
        if self.backdrop_snapshots >= MAX_BACKDROP_SNAPSHOTS {
            return false;
        }
        let Some(orig_bg_id) = self.snapshot_page_backdrop(true) else {
            return false;
        };
        self.backdrop_fanout = self.backdrop_fanout.saturating_mul(refs);

        let reg_clip = if scoped {
            let cid = self.fresh_id();
            let _ = write!(self.defs, "<clipPath id=\"c{cid}\"><rect x=\"");
            push_num(&mut self.defs, reg.x0);
            self.defs.push_str("\" y=\"");
            push_num(&mut self.defs, reg.y0);
            self.defs.push_str("\" width=\"");
            push_num(&mut self.defs, reg.width());
            self.defs.push_str("\" height=\"");
            push_num(&mut self.defs, reg.height());
            self.defs.push_str("\"/></clipPath>\n");
            Some(cid)
        } else {
            None
        };

        let write_blend_rects = |dst: &mut String, ops: &[TilingPlanOp], reg: SvgRect| {
            for op in ops {
                if let TilingPlanOp::Pattern { pat_id, blend_mode } = op {
                    dst.push_str("<rect x=\"");
                    push_num(dst, reg.x0);
                    dst.push_str("\" y=\"");
                    push_num(dst, reg.y0);
                    dst.push_str("\" width=\"");
                    push_num(dst, reg.width());
                    dst.push_str("\" height=\"");
                    push_num(dst, reg.height());
                    let _ = write!(dst, "\" fill=\"url(#p{pat_id})\"");
                    if let Some(name) = blend_css(*blend_mode) {
                        let _ = write!(dst, " style=\"mix-blend-mode:{name};\"");
                    }
                    dst.push_str("/>\n");
                }
            }
        };

        if opaque {
            let mask_id = self.fresh_id();
            let _ = write!(
                self.defs,
                "<mask id=\"m{mask_id}\" maskUnits=\"userSpaceOnUse\" style=\"mask-type:alpha\" x=\""
            );
            push_num(&mut self.defs, reg.x0);
            self.defs.push_str("\" y=\"");
            push_num(&mut self.defs, reg.y0);
            self.defs.push_str("\" width=\"");
            push_num(&mut self.defs, reg.width());
            self.defs.push_str("\" height=\"");
            push_num(&mut self.defs, reg.height());
            let _ = write!(self.defs, "\">\n{shape_body}</mask>\n");

            let _ = writeln!(self.out, "<use href=\"#b{orig_bg_id}\"/>");
            let _ = writeln!(self.out, "<g mask=\"url(#m{mask_id})\">");
            if let Some(cid) = reg_clip {
                let _ = writeln!(
                    self.out,
                    "<use href=\"#b{orig_bg_id}\" clip-path=\"url(#c{cid})\"/>"
                );
            } else {
                let _ = writeln!(self.out, "<use href=\"#b{orig_bg_id}\"/>");
            }
            write_blend_rects(&mut self.out, ops, reg);
            self.out.push_str("</g>\n");
            return true;
        }

        // Translucent backdrop: evaluate all passes in-place into `<g id="b{eval_id}">`,
        // then run ONE 5-primitive filter scoped to `reg` to compute exact
        // `eval * w + orig * (1 - w)`.
        let shape_id = self.fresh_id();
        let _ = write!(self.defs, "<g id=\"s{shape_id}\">\n{shape_body}</g>\n");
        let eval_id = self.fresh_id();
        let _ = writeln!(self.defs, "<g id=\"b{eval_id}\">");
        if let Some(cid) = reg_clip {
            let _ = writeln!(
                self.defs,
                "<use href=\"#b{orig_bg_id}\" clip-path=\"url(#c{cid})\"/>"
            );
        } else {
            let _ = writeln!(self.defs, "<use href=\"#b{orig_bg_id}\"/>");
        }
        write_blend_rects(&mut self.defs, ops, reg);
        self.defs.push_str("</g>\n");

        let fid = self.fresh_id();
        Self::write_filter_open_tag(&mut self.defs, fid, reg);
        let _ = write!(
            self.defs,
            "<feImage href=\"#b{orig_bg_id}\" x=\"0\" y=\"0\" width=\""
        );
        push_num(&mut self.defs, w);
        self.defs.push_str("\" height=\"");
        push_num(&mut self.defs, h);
        self.defs.push_str("\" result=\"orig\"/>\n");
        let _ = write!(
            self.defs,
            "<feImage href=\"#s{shape_id}\" x=\"0\" y=\"0\" width=\""
        );
        push_num(&mut self.defs, w);
        self.defs.push_str("\" height=\"");
        push_num(&mut self.defs, h);
        self.defs.push_str("\" result=\"wt\"/>\n");
        self.defs.push_str(
            "<feComposite in=\"SourceGraphic\" in2=\"wt\" operator=\"in\" result=\"rw\"/>\n\
             <feComposite in=\"orig\" in2=\"wt\" operator=\"out\" result=\"bw\"/>\n\
             <feComposite in=\"rw\" in2=\"bw\" operator=\"arithmetic\" k1=\"0\" k2=\"1\" k3=\"1\" k4=\"0\"/>\n\
             </filter>\n",
        );

        if scoped {
            self.emit_scoped_bg_restore(orig_bg_id, reg);
        }
        let _ = writeln!(
            self.out,
            "<use href=\"#b{eval_id}\" filter=\"url(#f{fid})\"/>"
        );
        true
    }

    /// Evaluate a dense opacity-grouped tiling op stream exceeding one filter's
    /// intermediate-pixel budget in bounded sequential `<filter>` chunks scoped
    /// to `affected` (splitting only at top-level `OpacityGroup` depth 0) while
    /// applying `shape_id` (coverage + object opacity) ONCE on the final chunk
    /// against the original pre-fill backdrop snapshot.
    fn emit_chunked_tiling_ops(
        &mut self,
        ops: &[TilingPlanOp],
        shape_id: u32,
        affected: SvgRect,
    ) -> bool {
        if self.non_isolated_filter_depth > 0 || ops.is_empty() {
            return false;
        }
        let w = self.page_width;
        let h = self.page_height;
        let mut scoped = affected.is_strict_subregion(w, h);
        let refs = if scoped {
            if self.backdrop_fanout.saturating_mul(3) <= MAX_BACKDROP_FANOUT {
                3
            } else {
                scoped = false;
                2
            }
        } else {
            2
        };
        if self.backdrop_fanout.saturating_mul(refs) > MAX_BACKDROP_FANOUT {
            return false;
        }
        let reg = if scoped {
            affected
        } else {
            self.full_page_rect()
        };
        // Reserve 5 primitives for the final `orig`/`wt`/`in`/`out`/`arithmetic` mix.
        let max_chunk_prims = Self::max_filter_primitives_for_region(reg).saturating_sub(5);
        if max_chunk_prims < 2 {
            return false;
        }

        let mut chunks: Vec<&[TilingPlanOp]> = Vec::new();
        let mut chunk_start = 0usize;
        let mut chunk_prims = 0usize;
        let mut group_depth = 0usize;
        let mut last_top_boundary = 0usize;
        let mut prims_at_last_boundary = 0usize;
        for (i, op) in ops.iter().enumerate() {
            let op_prims = Self::tiling_op_filter_primitives(op, false);
            chunk_prims = chunk_prims.saturating_add(op_prims);
            match op {
                TilingPlanOp::PushOpacityGroup => group_depth += 1,
                TilingPlanOp::PopOpacityGroup { .. } => {
                    group_depth = group_depth.saturating_sub(1);
                }
                TilingPlanOp::Pattern { .. } => {}
            }
            if group_depth == 0 {
                let end = i + 1;
                if chunk_prims > max_chunk_prims {
                    if last_top_boundary > chunk_start {
                        chunks.push(&ops[chunk_start..last_top_boundary]);
                        chunk_start = last_top_boundary;
                        chunk_prims = chunk_prims.saturating_sub(prims_at_last_boundary);
                    }
                    if chunk_prims > max_chunk_prims {
                        // One indivisible top-level OpacityGroup exceeds the pixel budget.
                        return false;
                    }
                }
                last_top_boundary = end;
                prims_at_last_boundary = chunk_prims;
            }
        }
        if chunk_start < ops.len() {
            if chunk_prims > max_chunk_prims {
                return false;
            }
            chunks.push(&ops[chunk_start..]);
        }
        if chunks.is_empty() {
            return false;
        }
        // Each nested chunk holds one `SourceGraphic` buffer of `reg` on the
        // filter call stack while the innermost chunk executes; bound both
        // snapshot count and total peak intermediate pixels across the chain.
        let peak_chain_px = (chunks
            .len()
            .saturating_add(max_chunk_prims)
            .saturating_add(5) as u64)
            .saturating_mul(reg.pixels());
        if peak_chain_px > MAX_FILTER_INTERMEDIATE_PIXELS.saturating_mul(2) {
            tracing::warn!(
                chunks = chunks.len(),
                region_px = reg.pixels(),
                "svg chunked tiling filter chain exceeds peak pixel budget; falling back to isolated group"
            );
            return false;
        }

        self.flush_pending_backdrop();
        if self.backdrop_snapshots.saturating_add(chunks.len()) > MAX_BACKDROP_SNAPSHOTS {
            tracing::warn!(
                chunks = chunks.len(),
                region_px = reg.pixels(),
                "svg chunked tiling filter exceeds intermediate-pixel snapshot budget; falling back to isolated group"
            );
            return false;
        }
        let Some(orig_bg_id) = self.snapshot_page_backdrop(true) else {
            return false;
        };
        self.backdrop_fanout = self.backdrop_fanout.saturating_mul(refs);

        let mut pat_rects: HashMap<u32, u32> = HashMap::new();
        for op in ops {
            let pat_id = match op {
                TilingPlanOp::Pattern { pat_id, .. } => Some(*pat_id),
                TilingPlanOp::PopOpacityGroup { mask_pat_id, .. } => *mask_pat_id,
                TilingPlanOp::PushOpacityGroup => None,
            };
            if let Some(pid) = pat_id {
                if let std::collections::hash_map::Entry::Vacant(e) = pat_rects.entry(pid) {
                    self.next_id += 1;
                    let rid = self.next_id;
                    let _ = write!(self.defs, "<rect id=\"pr{rid}\" x=\"0\" y=\"0\" width=\"");
                    push_num(&mut self.defs, w);
                    self.defs.push_str("\" height=\"");
                    push_num(&mut self.defs, h);
                    let _ = writeln!(self.defs, "\" fill=\"url(#p{pid})\"/>");
                    e.insert(rid);
                }
            }
        }

        let mut prev_g = orig_bg_id;
        for (chunk_idx, chunk) in chunks.iter().enumerate() {
            let is_last = chunk_idx + 1 == chunks.len();
            let fid = self.fresh_id();
            Self::write_filter_open_tag(&mut self.defs, fid, reg);
            let (acc, _) = Self::emit_filter_tiling_ops(
                &mut self.defs,
                w,
                h,
                &pat_rects,
                chunk_idx,
                chunk,
                "SourceGraphic",
                None,
            );
            if is_last {
                let _ = write!(
                    self.defs,
                    "<feImage href=\"#b{orig_bg_id}\" x=\"0\" y=\"0\" width=\""
                );
                push_num(&mut self.defs, w);
                self.defs.push_str("\" height=\"");
                push_num(&mut self.defs, h);
                self.defs.push_str("\" result=\"orig\"/>\n");
                let _ = write!(
                    self.defs,
                    "<feImage href=\"#s{shape_id}\" x=\"0\" y=\"0\" width=\""
                );
                push_num(&mut self.defs, w);
                self.defs.push_str("\" height=\"");
                push_num(&mut self.defs, h);
                self.defs.push_str("\" result=\"wt\"/>\n");
                let _ = writeln!(
                    self.defs,
                    "<feComposite in=\"{acc}\" in2=\"wt\" operator=\"in\" result=\"rw\"/>"
                );
                let _ = writeln!(
                    self.defs,
                    "<feComposite in=\"orig\" in2=\"wt\" operator=\"out\" result=\"bw\"/>"
                );
                self.defs.push_str(
                    "<feComposite in=\"rw\" in2=\"bw\" operator=\"arithmetic\" k1=\"0\" k2=\"1\" k3=\"1\" k4=\"0\"/>\n",
                );
            }
            self.defs.push_str("</filter>\n");
            if is_last {
                if scoped {
                    self.emit_scoped_bg_restore(orig_bg_id, reg);
                }
                let _ = writeln!(
                    self.out,
                    "<use href=\"#b{prev_g}\" filter=\"url(#f{fid})\"/>"
                );
            } else {
                let next_g = self.fresh_id();
                self.backdrop_snapshots += 1;
                let _ = writeln!(
                    self.defs,
                    "<g id=\"b{next_g}\"><use href=\"#b{prev_g}\" filter=\"url(#f{fid})\"/></g>"
                );
                prev_g = next_g;
            }
        }
        true
    }

    /// Emit `ops` inside an already-open isolated `<g>` wrapper (used only when
    /// nested inside a `<pattern>`/`<mask>` or after snapshot exhaustion).
    fn emit_isolated_tiling_ops(&mut self, ops: &[TilingPlanOp]) {
        let mut stack: Vec<String> = Vec::new();
        for op in ops {
            let target = stack.last_mut().unwrap_or(&mut self.out);
            match op {
                TilingPlanOp::Pattern { pat_id, blend_mode } => {
                    target.push_str("<rect x=\"0\" y=\"0\" width=\"");
                    push_num(target, self.page_width);
                    target.push_str("\" height=\"");
                    push_num(target, self.page_height);
                    let _ = write!(target, "\" fill=\"url(#p{pat_id})\"");
                    if let Some(name) = blend_css(*blend_mode) {
                        let _ = write!(target, " style=\"mix-blend-mode:{name};\"");
                    }
                    target.push_str("/>\n");
                }
                TilingPlanOp::PushOpacityGroup => {
                    stack.push(String::new());
                }
                TilingPlanOp::PopOpacityGroup { alpha, .. } => {
                    if let Some(inner) = stack.pop() {
                        let parent = stack.last_mut().unwrap_or(&mut self.out);
                        parent.push_str("<g");
                        push_opacity(parent, "opacity", *alpha);
                        parent.push_str(">\n");
                        parent.push_str(&inner);
                        parent.push_str("</g>\n");
                    }
                }
            }
        }
        for leftover in stack {
            self.out.push_str(&leftover);
        }
    }

    /// Flush any open batch of backdrop-dependent filter steps into `self.out`
    /// (preserving any trailing elements already accumulated in `self.out` in
    /// exact painter's order). References `#b{bg_id}` only ONCE (zero
    /// exponential `<use>` expansion).
    fn flush_pending_backdrop(&mut self) {
        if self.non_isolated_filter_depth > 0 {
            return;
        }
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
                BackdropStep::Overlay(_)
                | BackdropStep::BlendOverlay { .. }
                | BackdropStep::PushNonIsolatedGroup { .. }
                | BackdropStep::PopNonIsolatedGroup { .. } => {}
                BackdropStep::MultiPass { ops, .. } => {
                    for op in ops {
                        match op {
                            TilingPlanOp::Pattern { pat_id, .. } => {
                                register_pat(&mut self.defs, &mut self.next_id, *pat_id);
                            }
                            TilingPlanOp::PopOpacityGroup {
                                mask_pat_id: Some(m_id),
                                ..
                            } => {
                                register_pat(&mut self.defs, &mut self.next_id, *m_id);
                            }
                            TilingPlanOp::PushOpacityGroup
                            | TilingPlanOp::PopOpacityGroup {
                                mask_pat_id: None, ..
                            } => {}
                        }
                    }
                }
                BackdropStep::Affine {
                    t_id, k_id, mk_id, ..
                } => {
                    register_pat(&mut self.defs, &mut self.next_id, *t_id);
                    register_pat(&mut self.defs, &mut self.next_id, *k_id);
                    register_pat(&mut self.defs, &mut self.next_id, *mk_id);
                }
            }
        }

        let scoped = pending.region.is_strict_subregion(w, h)
            && self.backdrop_fanout.saturating_mul(2) <= MAX_BACKDROP_FANOUT;
        let filter_region = if scoped {
            self.backdrop_fanout = self.backdrop_fanout.saturating_mul(2);
            pending.region
        } else {
            self.full_page_rect()
        };

        let fid = self.fresh_id();
        Self::write_filter_open_tag(&mut self.defs, fid, filter_region);

        let mut cur = String::from("SourceGraphic");
        let mut ga: Option<String> = None;
        let mut group_stack: Vec<(String, Option<String>)> = Vec::new();
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
                    if let Some(ref cur_ga) = ga {
                        let next_ga = format!("ga{idx}");
                        let _ = writeln!(
                            self.defs,
                            "<feComposite in=\"{ov_res}\" in2=\"{cur_ga}\" operator=\"over\" result=\"{next_ga}\"/>"
                        );
                        ga = Some(next_ga);
                    }
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
                    if let Some(ref cur_ga) = ga {
                        let next_ga = format!("ga{idx}");
                        let _ = writeln!(
                            self.defs,
                            "<feComposite in=\"{ov_res}\" in2=\"{cur_ga}\" operator=\"over\" result=\"{next_ga}\"/>"
                        );
                        ga = Some(next_ga);
                    }
                }
                BackdropStep::PushNonIsolatedGroup { blend_mode } => {
                    group_stack.push((cur.clone(), ga.clone()));
                    if *blend_mode != BlendMode::Normal {
                        let zero_a = format!("gaz{idx}");
                        let _ = writeln!(
                            self.defs,
                            "<feFlood flood-color=\"#000000\" flood-opacity=\"0\" result=\"{zero_a}\"/>"
                        );
                        ga = Some(zero_a);
                    }
                }
                BackdropStep::PopNonIsolatedGroup {
                    blend_mode,
                    weight_id,
                } => {
                    let (saved, saved_ga) =
                        group_stack.pop().unwrap_or_else(|| (cur.clone(), None));
                    if *blend_mode == BlendMode::Normal {
                        if let Some(wid) = weight_id {
                            let wt_res = format!("gw{idx}");
                            let rw_res = format!("grw{idx}");
                            let bw_res = format!("gbw{idx}");
                            let next_cur = format!("c{idx}");
                            let _ = write!(
                                self.defs,
                                "<feImage href=\"#s{wid}\" x=\"0\" y=\"0\" width=\""
                            );
                            push_num(&mut self.defs, w);
                            self.defs.push_str("\" height=\"");
                            push_num(&mut self.defs, h);
                            let _ = writeln!(self.defs, "\" result=\"{wt_res}\"/>");
                            let _ = writeln!(
                                self.defs,
                                "<feComposite in=\"{cur}\" in2=\"{wt_res}\" operator=\"in\" result=\"{rw_res}\"/>"
                            );
                            let _ = writeln!(
                                self.defs,
                                "<feComposite in=\"{saved}\" in2=\"{wt_res}\" operator=\"out\" result=\"{bw_res}\"/>"
                            );
                            let _ = writeln!(
                                self.defs,
                                "<feComposite in=\"{rw_res}\" in2=\"{bw_res}\" operator=\"arithmetic\" k1=\"0\" k2=\"1\" k3=\"1\" k4=\"0\" result=\"{next_cur}\"/>"
                            );
                            cur = next_cur;
                            if let (Some(cur_ga), Some(sg)) = (ga.as_ref(), saved_ga.as_ref()) {
                                let rwa = format!("grwa{idx}");
                                let bwa = format!("gbwa{idx}");
                                let next_ga = format!("ga{idx}");
                                let _ = writeln!(
                                    self.defs,
                                    "<feComposite in=\"{cur_ga}\" in2=\"{wt_res}\" operator=\"in\" result=\"{rwa}\"/>"
                                );
                                let _ = writeln!(
                                    self.defs,
                                    "<feComposite in=\"{sg}\" in2=\"{wt_res}\" operator=\"out\" result=\"{bwa}\"/>"
                                );
                                let _ = writeln!(
                                    self.defs,
                                    "<feComposite in=\"{rwa}\" in2=\"{bwa}\" operator=\"arithmetic\" k1=\"0\" k2=\"1\" k3=\"1\" k4=\"0\" result=\"{next_ga}\"/>"
                                );
                                ga = Some(next_ga);
                            } else {
                                ga = saved_ga;
                            }
                        } else {
                            ga = saved_ga;
                        }
                    } else {
                        // ISO 32000-1 §11.4.5 backdrop removal for a non-isolated
                        // non-Normal group:
                        //   P_g = P_n - (1 - α_g) * P_0
                        // then apply optional constant opacity + soft mask weight
                        // and composite onto `saved` with `blend_mode`.
                        let cur_ga = ga.unwrap_or_else(|| cur.clone());
                        let bg_rem = format!("br{idx}");
                        let pg = format!("pg{idx}");
                        let next_cur = format!("c{idx}");
                        let _ = writeln!(
                            self.defs,
                            "<feComposite in=\"{saved}\" in2=\"{cur_ga}\" operator=\"out\" result=\"{bg_rem}\"/>"
                        );
                        let _ = writeln!(
                            self.defs,
                            "<feComposite in=\"{cur}\" in2=\"{bg_rem}\" operator=\"arithmetic\" k1=\"0\" k2=\"1\" k3=\"-1\" k4=\"0\" result=\"{pg}\"/>"
                        );
                        let pg_final = if let Some(wid) = weight_id {
                            let wt_res = format!("gw{idx}");
                            let pg_w = format!("pgw{idx}");
                            let _ = write!(
                                self.defs,
                                "<feImage href=\"#s{wid}\" x=\"0\" y=\"0\" width=\""
                            );
                            push_num(&mut self.defs, w);
                            self.defs.push_str("\" height=\"");
                            push_num(&mut self.defs, h);
                            let _ = writeln!(self.defs, "\" result=\"{wt_res}\"/>");
                            let _ = writeln!(
                                self.defs,
                                "<feComposite in=\"{pg}\" in2=\"{wt_res}\" operator=\"in\" result=\"{pg_w}\"/>"
                            );
                            pg_w
                        } else {
                            pg
                        };
                        if let Some(name) = blend_css(*blend_mode) {
                            let _ = writeln!(
                                self.defs,
                                "<feBlend in=\"{pg_final}\" in2=\"{saved}\" mode=\"{name}\" result=\"{next_cur}\"/>"
                            );
                        } else {
                            let _ = writeln!(
                                self.defs,
                                "<feComposite in=\"{pg_final}\" in2=\"{saved}\" operator=\"over\" result=\"{next_cur}\"/>"
                            );
                        }
                        cur = next_cur;
                        if let Some(sg) = saved_ga {
                            let next_ga = format!("ga{idx}");
                            let _ = writeln!(
                                self.defs,
                                "<feComposite in=\"{pg_final}\" in2=\"{sg}\" operator=\"over\" result=\"{next_ga}\"/>"
                            );
                            ga = Some(next_ga);
                        } else {
                            ga = None;
                        }
                    }
                }
                BackdropStep::MultiPass { ops, shape_id } => {
                    let (acc, acc_a) = Self::emit_filter_tiling_ops(
                        &mut self.defs,
                        w,
                        h,
                        &pat_rects,
                        idx,
                        ops,
                        &cur,
                        ga.as_deref(),
                    );
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
                    if let (Some(aa), Some(cur_ga)) = (acc_a, ga.as_ref()) {
                        let rwa = format!("rwa{idx}");
                        let bwa = format!("bwa{idx}");
                        let next_ga = format!("ga{idx}");
                        let _ = writeln!(
                            self.defs,
                            "<feComposite in=\"{aa}\" in2=\"{wt_res}\" operator=\"in\" result=\"{rwa}\"/>"
                        );
                        let _ = writeln!(
                            self.defs,
                            "<feComposite in=\"{cur_ga}\" in2=\"{wt_res}\" operator=\"out\" result=\"{bwa}\"/>"
                        );
                        let _ = writeln!(
                            self.defs,
                            "<feComposite in=\"{rwa}\" in2=\"{bwa}\" operator=\"arithmetic\" k1=\"0\" k2=\"1\" k3=\"1\" k4=\"0\" result=\"{next_ga}\"/>"
                        );
                        ga = Some(next_ga);
                    }
                }
                BackdropStep::Affine {
                    t_id,
                    k_id,
                    mk_id,
                    shape_id,
                } => {
                    let rt = pat_rects[t_id];
                    let rk = pat_rects[k_id];
                    let rmk = pat_rects[mk_id];
                    let pt = format!("pt{idx}");
                    let pk = format!("pk{idx}");
                    let pmk = format!("pmk{idx}");
                    let pkinv = format!("pki{idx}");
                    let pm = format!("pm{idx}");
                    let cur_s = format!("cs{idx}");
                    let mb_s = format!("mb{idx}");
                    let res_s = format!("rs{idx}");
                    let res_on_b = format!("rb{idx}");
                    let res = format!("ar{idx}");
                    let wt_res = format!("w{idx}");
                    let rw_res = format!("rw{idx}");
                    let bw_res = format!("bw{idx}");
                    let next_cur = format!("c{idx}");
                    for (rid, out_name) in [(rt, &pt), (rk, &pk), (rmk, &pmk)] {
                        let _ = write!(
                            self.defs,
                            "<feImage href=\"#pr{rid}\" x=\"0\" y=\"0\" width=\""
                        );
                        push_num(&mut self.defs, w);
                        self.defs.push_str("\" height=\"");
                        push_num(&mut self.defs, h);
                        let _ = writeln!(self.defs, "\" result=\"{out_name}\"/>");
                    }
                    // Exact 3-seed premultiplied affine composite for any backdrop alpha B_a:
                    //   M = clamp(f(1) - K, 0, 1 - K)          (kept at alpha = 1)
                    //   cur_s = (B_rgb / B_a, 1)               (unpremultiplied straight backdrop)
                    //   res_s = (M * c_b + K, 1)
                    //   res_on_b = res_s IN cur = (M * B_rgb + K * B_a, B_a)
                    //   res = res_on_b OVER pt
                    //       = ((1 - B_a) * T + B_a * K + M * B_rgb, a_t + (1 - a_t) * B_a)
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
                        "<feComponentTransfer in=\"{cur}\" result=\"{cur_s}\">\
                         <feFuncA type=\"linear\" slope=\"0\" intercept=\"1\"/>\
                         </feComponentTransfer>"
                    );
                    let _ = writeln!(
                        self.defs,
                        "<feComposite in=\"{pm}\" in2=\"{cur_s}\" operator=\"arithmetic\" k1=\"1\" k2=\"0\" k3=\"0\" k4=\"0\" result=\"{mb_s}\"/>"
                    );
                    let _ = writeln!(
                        self.defs,
                        "<feComposite in=\"{mb_s}\" in2=\"{pk}\" operator=\"arithmetic\" k1=\"0\" k2=\"1\" k3=\"1\" k4=\"0\" result=\"{res_s}\"/>"
                    );
                    let _ = writeln!(
                        self.defs,
                        "<feComposite in=\"{res_s}\" in2=\"{cur}\" operator=\"in\" result=\"{res_on_b}\"/>"
                    );
                    let _ = writeln!(
                        self.defs,
                        "<feComposite in=\"{res_on_b}\" in2=\"{pt}\" operator=\"over\" result=\"{res}\"/>"
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
                        "<feComposite in=\"{res}\" in2=\"{wt_res}\" operator=\"in\" result=\"{rw_res}\"/>"
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
                    if let Some(ref cur_ga) = ga {
                        let aa = format!("aa{idx}");
                        let rwa = format!("rwa{idx}");
                        let bwa = format!("bwa{idx}");
                        let next_ga = format!("ga{idx}");
                        let _ = writeln!(
                            self.defs,
                            "<feComposite in=\"{pt}\" in2=\"{cur_ga}\" operator=\"over\" result=\"{aa}\"/>"
                        );
                        let _ = writeln!(
                            self.defs,
                            "<feComposite in=\"{aa}\" in2=\"{wt_res}\" operator=\"in\" result=\"{rwa}\"/>"
                        );
                        let _ = writeln!(
                            self.defs,
                            "<feComposite in=\"{cur_ga}\" in2=\"{wt_res}\" operator=\"out\" result=\"{bwa}\"/>"
                        );
                        let _ = writeln!(
                            self.defs,
                            "<feComposite in=\"{rwa}\" in2=\"{bwa}\" operator=\"arithmetic\" k1=\"0\" k2=\"1\" k3=\"1\" k4=\"0\" result=\"{next_ga}\"/>"
                        );
                        ga = Some(next_ga);
                    }
                }
            }
        }

        self.defs.push_str("</filter>\n");
        if scoped {
            self.emit_scoped_bg_restore(pending.bg_id, filter_region);
        }
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
            let full = self.full_page_rect();
            if let Some(ref mut p) = self.pending_backdrop {
                p.steps.push(BackdropStep::Overlay(ov_id));
                p.total_primitives = p.total_primitives.saturating_add(3);
                p.region = full;
            }
        }
    }

    /// Enqueue one backdrop-dependent filter step onto the active linear
    /// `<filter>` chain, absorbing intervening normal draws as `Overlay` steps
    /// and enforcing [`MAX_FILTER_INTERMEDIATE_PIXELS`] on the batch's
    /// `(total_primitives × region_pixels)` before starting a new snapshot.
    fn enqueue_backdrop_step(
        &mut self,
        step: BackdropStep,
        step_primitives: usize,
        region: Option<SvgRect>,
        allow_empty: bool,
    ) -> bool {
        let full = self.full_page_rect();
        let step_region = region.unwrap_or(full);
        if let Some(ref p) = self.pending_backdrop {
            let has_out = !self.out.is_empty();
            let extra_prims = if has_out { 3 } else { 0 };
            let candidate_region = if has_out || self.non_isolated_filter_depth > 0 {
                full
            } else {
                p.region.union(step_region)
            };
            let effective_region = if candidate_region
                .is_strict_subregion(self.page_width, self.page_height)
                && self.backdrop_fanout.saturating_mul(2) <= MAX_BACKDROP_FANOUT
            {
                candidate_region
            } else {
                full
            };
            let prim_limit = Self::max_filter_primitives_for_region(effective_region);
            if p.total_primitives
                .saturating_add(extra_prims)
                .saturating_add(step_primitives)
                <= prim_limit
            {
                self.absorb_out_into_pending();
                if let Some(ref mut p) = self.pending_backdrop {
                    p.total_primitives = p.total_primitives.saturating_add(step_primitives);
                    p.region = candidate_region;
                    p.steps.push(step);
                }
                return true;
            }
            if self.non_isolated_filter_depth > 0 {
                return false;
            }
            self.flush_pending_backdrop();
        }
        let init_region = if self.non_isolated_filter_depth > 0 {
            full
        } else {
            step_region
        };
        let effective_region = if init_region.is_strict_subregion(self.page_width, self.page_height)
            && self.backdrop_fanout.saturating_mul(2) <= MAX_BACKDROP_FANOUT
        {
            init_region
        } else {
            full
        };
        let prim_limit = Self::max_filter_primitives_for_region(effective_region);
        if (allow_empty || !self.out.is_empty())
            && self.backdrop_snapshots < MAX_BACKDROP_SNAPSHOTS
            && step_primitives <= prim_limit
        {
            if let Some(bg_id) = self.snapshot_page_backdrop(allow_empty) {
                self.pending_backdrop = Some(PendingBackdrop {
                    bg_id,
                    steps: vec![step],
                    total_primitives: step_primitives,
                    region: init_region,
                });
                return true;
            }
        }
        false
    }

    /// Dispatch a backdrop-dependent multi-pass tiling op stream (for fills,
    /// strokes, or Type 3 runs) using bounded raster memory:
    /// - Dense flat plans evaluate in-place via sibling `mix-blend-mode` rects
    ///   (0 filter primitives on opaque pages, 5 scoped primitives on translucent pages).
    /// - Small/medium plans batch into the active `<filter>` scoped to `affected`.
    /// - Oversized `OpacityGroup` plans split into pixel-budgeted scoped chunks,
    ///   returning `false` to the isolated single-opacity fallback if over budget.
    fn emit_tiling_multi_pass_backdrop(
        &mut self,
        ops: &[TilingPlanOp],
        shape_body: &str,
        affected: SvgRect,
    ) -> bool {
        let has_opacity_group = Self::ops_has_opacity_group(ops);
        if !(self.pending_backdrop.is_some() || !self.out.is_empty() || has_opacity_group) {
            return false;
        }
        let track_alpha = self.non_isolated_filter_depth > 0;
        let step_prims = Self::tiling_ops_filter_primitives(ops, track_alpha)
            .saturating_add(if track_alpha { 8 } else { 5 });
        let effective_region = if !track_alpha
            && affected.is_strict_subregion(self.page_width, self.page_height)
            && self.backdrop_fanout.saturating_mul(2) <= MAX_BACKDROP_FANOUT
        {
            affected
        } else {
            self.full_page_rect()
        };
        let prim_limit = Self::max_filter_primitives_for_region(effective_region);

        if !has_opacity_group
            && self.non_isolated_filter_depth == 0
            && (ops.len() > FLAT_BLEND_GROUP_OP_THRESHOLD || step_prims > prim_limit)
            && self.emit_flat_blend_group_tiling_ops(ops, shape_body, affected)
        {
            return true;
        }

        let shape_id = self.fresh_id();
        let _ = writeln!(self.defs, "<g id=\"s{shape_id}\">{shape_body}</g>");
        if self.enqueue_backdrop_step(
            BackdropStep::MultiPass {
                ops: ops.to_vec(),
                shape_id,
            },
            step_prims,
            Some(affected),
            has_opacity_group,
        ) {
            return true;
        }
        if self.non_isolated_filter_depth == 0
            && self.emit_chunked_tiling_ops(ops, shape_id, affected)
        {
            return true;
        }
        false
    }

    fn emit_tiling_fill_path_data(
        &mut self,
        d: &str,
        rule: &FillRule,
        tiling: &zpdf_display_list::TilingPattern,
        opacity: f32,
        affected: SvgRect,
        depth: u8,
    ) {
        let plan = self.emit_tiling_plan(tiling, depth);
        match plan {
            TilingPassPlan::Affine { t_id, k_id, mk_id } => {
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
                            t_id,
                            k_id,
                            mk_id,
                            shape_id,
                        },
                        14,
                        Some(affected),
                        true,
                    ) {
                        return;
                    }
                    self.flush_pending_backdrop();
                }
                let placement = self.placement_attrs();
                let _ = write!(self.out, "<path d=\"{d}\" fill=\"url(#p{t_id})\"");
                if *rule == FillRule::EvenOdd {
                    self.out.push_str(" fill-rule=\"evenodd\"");
                }
                push_opacity(&mut self.out, "fill-opacity", opacity);
                self.out.push_str(&placement);
                self.out.push_str("/>\n");
            }
            TilingPassPlan::Ops(ops) => {
                if ops.is_empty() {
                    return;
                }
                let single_pattern = match ops.as_slice() {
                    [TilingPlanOp::Pattern { pat_id, blend_mode }] => Some((*pat_id, *blend_mode)),
                    _ => None,
                };
                if single_pattern.is_none() {
                    if depth == 0 {
                        let placement = self.placement_attrs();
                        let mut shape_body = String::with_capacity(d.len() + placement.len() + 64);
                        let _ = write!(shape_body, "<path d=\"{d}\" fill=\"#ffffff\"");
                        if *rule == FillRule::EvenOdd {
                            shape_body.push_str(" fill-rule=\"evenodd\"");
                        }
                        push_opacity(&mut shape_body, "fill-opacity", opacity);
                        shape_body.push_str(&placement);
                        shape_body.push_str("/>");
                        if self.emit_tiling_multi_pass_backdrop(&ops, &shape_body, affected) {
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
                    self.emit_isolated_tiling_ops(&ops);
                    self.out.push_str("</g>\n");
                    return;
                }
                let Some((id, blend_mode)) = single_pattern else {
                    return;
                };
                if depth == 0 && self.pending_backdrop.is_some() && blend_mode != BlendMode::Normal
                {
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
                        BackdropStep::BlendOverlay { ov_id, blend_mode },
                        3,
                        Some(affected),
                        false,
                    ) {
                        return;
                    }
                    self.flush_pending_backdrop();
                }
                let placement = self.placement_attrs();
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
                let affected = self
                    .path_svg_bounds(path, 2.0)
                    .unwrap_or_else(|| self.clip_bounds.unwrap_or_else(|| self.full_page_rect()));
                self.emit_tiling_fill_path_data(&d, rule, tiling, opacity, affected, depth);
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
                let pad = self.stroke_svg_pad(style);
                let affected = self
                    .path_svg_bounds(path, pad)
                    .unwrap_or_else(|| self.clip_bounds.unwrap_or_else(|| self.full_page_rect()));
                let plan = self.emit_tiling_plan(tiling, depth);
                match plan {
                    TilingPassPlan::Affine { t_id, k_id, mk_id } => {
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
                                    t_id,
                                    k_id,
                                    mk_id,
                                    shape_id,
                                },
                                14,
                                Some(affected),
                                true,
                            ) {
                                return;
                            }
                            self.flush_pending_backdrop();
                        }
                        let placement = self.placement_attrs();
                        let _ = write!(
                            self.out,
                            "<path d=\"{d}\" fill=\"none\" stroke=\"url(#p{t_id})\""
                        );
                        push_opacity(&mut self.out, "stroke-opacity", opacity);
                        push_stroke_attrs(&mut self.out, style, self.scale);
                        self.out.push_str(&placement);
                        self.out.push_str("/>\n");
                    }
                    TilingPassPlan::Ops(ops) => {
                        if ops.is_empty() {
                            return;
                        }
                        let single_pattern = match ops.as_slice() {
                            [TilingPlanOp::Pattern { pat_id, blend_mode }] => {
                                Some((*pat_id, *blend_mode))
                            }
                            _ => None,
                        };
                        if single_pattern.is_none() {
                            if depth == 0 {
                                let placement = self.placement_attrs();
                                let mut shape_body =
                                    String::with_capacity(d.len() + placement.len() + 96);
                                let _ = write!(
                                    shape_body,
                                    "<path d=\"{d}\" fill=\"none\" stroke=\"#ffffff\""
                                );
                                push_opacity(&mut shape_body, "stroke-opacity", opacity);
                                push_stroke_attrs(&mut shape_body, style, self.scale);
                                shape_body.push_str(&placement);
                                shape_body.push_str("/>");
                                if self.emit_tiling_multi_pass_backdrop(&ops, &shape_body, affected)
                                {
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
                            self.emit_isolated_tiling_ops(&ops);
                            self.out.push_str("</g>\n");
                            return;
                        }
                        let Some((id, blend_mode)) = single_pattern else {
                            return;
                        };
                        if depth == 0
                            && self.pending_backdrop.is_some()
                            && blend_mode != BlendMode::Normal
                        {
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
                                BackdropStep::BlendOverlay { ov_id, blend_mode },
                                3,
                                Some(affected),
                                false,
                            ) {
                                return;
                            }
                            self.flush_pending_backdrop();
                        }
                        let placement = self.placement_attrs();
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
        if let Some(cb) = self.path_svg_bounds(path, 2.0) {
            self.clip_bounds = Some(cb);
        }
    }

    /// A stroked clip has no SVG `<clipPath>` equivalent; a luminance mask with
    /// a white stroke is exactly the coverage the CPU backend rasterizes. The
    /// def bakes in the enclosing clip and mask so intersection nests.
    fn push_clip_stroke(&mut self, path: &Path, style: &StrokeStyle) {
        let Some(d) = self.path_data(path) else {
            return;
        };
        let pad = self.stroke_svg_pad(style);
        let next_bounds = self.path_svg_bounds(path, pad);
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
        if let Some(cb) = next_bounds {
            self.clip_bounds = Some(cb);
        }
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

    fn close_non_isolated_filter_group(
        &mut self,
        blend_mode: BlendMode,
        alpha: f32,
        mask_id: Option<u32>,
    ) {
        self.absorb_out_into_pending();
        self.blend_depth = self.blend_depth.saturating_sub(1);
        self.non_isolated_filter_depth = self.non_isolated_filter_depth.saturating_sub(1);
        if blend_mode != BlendMode::Normal {
            self.isolated_depth = self.isolated_depth.saturating_sub(1);
        }
        let weight_id = if alpha < 1.0 || mask_id.is_some() || blend_mode == BlendMode::Normal {
            let id = self.fresh_id();
            let _ = write!(self.defs, "<g id=\"s{id}\"><rect x=\"0\" y=\"0\" width=\"");
            push_num(&mut self.defs, self.page_width);
            self.defs.push_str("\" height=\"");
            push_num(&mut self.defs, self.page_height);
            self.defs.push_str("\" fill=\"#ffffff\"");
            push_opacity(&mut self.defs, "fill-opacity", alpha);
            if let Some(mid) = mask_id {
                let _ = write!(self.defs, " mask=\"url(#m{mid})\"");
            }
            self.defs.push_str("/></g>\n");
            Some(id)
        } else {
            None
        };
        if let Some(ref mut p) = self.pending_backdrop {
            p.steps.push(BackdropStep::PopNonIsolatedGroup {
                blend_mode,
                weight_id,
            });
            p.total_primitives = p.total_primitives.saturating_add(6);
        }
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
        prev_non_isolated_depth: usize,
        depth: u8,
    ) {
        if !open {
            return;
        }
        self.flush_pending_backdrop();
        let inner_body = std::mem::replace(&mut self.out, prev_out);
        self.pending_backdrop = prev_pending;
        self.non_isolated_filter_depth = prev_non_isolated_depth;
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
                3,
                None,
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
        let saved_non_isolated = std::mem::replace(&mut self.non_isolated_filter_depth, 0);
        let saved_blend_depth = std::mem::replace(&mut self.blend_depth, 0);
        let saved_isolated_depth = std::mem::replace(&mut self.isolated_depth, 0);
        // Mask content is self-contained (geometry fixed at `gs` time); it must
        // not inherit the page's active clip chain.
        let (saved_clip, saved_mask, saved_clip_bounds) = (
            self.clip.take(),
            self.stroke_mask.take(),
            self.clip_bounds.take(),
        );
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
        self.non_isolated_filter_depth = saved_non_isolated;
        self.blend_depth = saved_blend_depth;
        self.isolated_depth = saved_isolated_depth;
        self.clip = saved_clip;
        self.stroke_mask = saved_mask;
        self.clip_bounds = saved_clip_bounds;

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
                } else if let Some((d, glyph_bounds)) = self.outline_run_path_data(run, font) {
                    let affected = match (glyph_bounds, self.clip_bounds) {
                        (Some(gb), Some(cb)) => gb.intersect(cb).unwrap_or(gb),
                        (Some(gb), None) => gb,
                        (None, Some(cb)) => cb,
                        (None, None) => self.full_page_rect(),
                    };
                    self.emit_tiling_fill_path_data(
                        &d,
                        &FillRule::NonZero,
                        tiling,
                        opacity,
                        affected,
                        depth,
                    );
                }
            }
            _ => {
                tracing::debug!("skipping non-solid text paint (rasterized upstream)");
            }
        }
    }

    /// Outline fonts: build one merged SVG path string and its conservative
    /// SVG-space bounding box for all glyphs of `run`.
    fn outline_run_path_data(
        &mut self,
        run: &GlyphRun,
        font: &LoadedFont,
    ) -> Option<(String, Option<SvgRect>)> {
        let upem = font.units_per_em;
        if !(upem.is_finite() && upem > 0.0) {
            return None;
        }
        let mut d = String::new();
        let (x0, y1, scale) = (self.x0, self.y1, self.scale);
        let (mut min_x, mut min_y) = (f64::INFINITY, f64::INFINITY);
        let (mut max_x, mut max_y) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
        let mut record_bounds = |pts: &[(f64, f64)], off: (f32, f32)| {
            let tm = &run.transform;
            for &(gx, gy) in pts {
                let tx = gx / upem * run.font_size as f64 * run.h_scale as f64 + off.0 as f64;
                let ty = gy / upem * run.font_size as f64 + off.1 as f64;
                let px = tm.a * tx + tm.c * ty + tm.e;
                let py = tm.b * tx + tm.d * ty + tm.f;
                let (sx, sy) = ((px - x0) * scale, (y1 - py) * scale);
                if coord_ok(sx) && coord_ok(sy) {
                    min_x = min_x.min(sx);
                    min_y = min_y.min(sy);
                    max_x = max_x.max(sx);
                    max_y = max_y.max(sy);
                }
            }
        };
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
                        record_bounds(&[(x, y)], off);
                    }
                    OutlineCommand::LineTo(x, y) => {
                        ok &= self.push_glyph_point(&mut part, 'L', &[(x, y)], upem, run, off);
                        record_bounds(&[(x, y)], off);
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
                        record_bounds(&[(x1, y1), (x, y)], off);
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
                        record_bounds(&[(x1, y1), (x2, y2), (x, y)], off);
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
        if d.is_empty() {
            return None;
        }
        let bounds = SvgRect::from_bounds_clamped(
            min_x,
            min_y,
            max_x,
            max_y,
            2.0,
            self.page_width,
            self.page_height,
        );
        Some((d, bounds))
    }

    /// Outline fonts: all glyphs of the run merge into one nonzero-filled path.
    fn emit_outline_run(&mut self, run: &GlyphRun, font: &LoadedFont, color: &Color, opacity: f32) {
        let Some((d, _)) = self.outline_run_path_data(run, font) else {
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
        let saved_non_isolated = std::mem::replace(&mut self.non_isolated_filter_depth, 0);
        self.emit_type3_run(run, font, &Color::white(), 1.0);
        let shape_body = std::mem::replace(&mut self.out, saved_out);
        self.pending_backdrop = saved_pending;
        self.non_isolated_filter_depth = saved_non_isolated;
        if shape_body.is_empty() {
            return;
        }
        let affected = self.clip_bounds.unwrap_or_else(|| self.full_page_rect());
        let plan = self.emit_tiling_plan(tiling, depth);
        match plan {
            TilingPassPlan::Affine { t_id, k_id, mk_id } => {
                if depth == 0 {
                    let shape_id = self.fresh_id();
                    let _ = write!(self.defs, "<g id=\"s{shape_id}\"");
                    push_opacity(&mut self.defs, "opacity", opacity);
                    self.defs.push_str(">\n");
                    self.defs.push_str(&shape_body);
                    self.defs.push_str("</g>\n");
                    if self.enqueue_backdrop_step(
                        BackdropStep::Affine {
                            t_id,
                            k_id,
                            mk_id,
                            shape_id,
                        },
                        14,
                        Some(affected),
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
                    "\" fill=\"url(#p{t_id})\" mask=\"url(#m{mask_id})\""
                );
                push_opacity(&mut self.out, "fill-opacity", opacity);
                self.out.push_str("/>\n");
            }
            TilingPassPlan::Ops(ops) => {
                if ops.is_empty() {
                    return;
                }
                let single_pattern = match ops.as_slice() {
                    [TilingPlanOp::Pattern { pat_id, blend_mode }] => Some((*pat_id, *blend_mode)),
                    _ => None,
                };
                if depth == 0
                    && (single_pattern.is_none()
                        || (self.pending_backdrop.is_some()
                            && single_pattern.is_some_and(|(_, m)| m != BlendMode::Normal)))
                {
                    let mut wrapped_shape = String::with_capacity(shape_body.len() + 48);
                    wrapped_shape.push_str("<g");
                    push_opacity(&mut wrapped_shape, "opacity", opacity);
                    wrapped_shape.push_str(">\n");
                    wrapped_shape.push_str(&shape_body);
                    wrapped_shape.push_str("</g>\n");
                    if self.emit_tiling_multi_pass_backdrop(&ops, &wrapped_shape, affected) {
                        return;
                    }
                    self.flush_pending_backdrop();
                }
                let mask_id = self.fresh_id();
                let _ = write!(
                    self.defs,
                    "<mask id=\"m{mask_id}\" maskUnits=\"userSpaceOnUse\">\n{shape_body}</mask>\n"
                );
                if let Some((id, blend_mode)) = single_pattern {
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
                } else {
                    self.out.push_str("<g");
                    push_opacity(&mut self.out, "opacity", opacity);
                    let _ = writeln!(
                        self.out,
                        " style=\"isolation:isolate;\" mask=\"url(#m{mask_id})\">"
                    );
                    self.emit_isolated_tiling_ops(&ops);
                    self.out.push_str("</g>\n");
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
        const NOTO_SC_TTF: &[u8] =
            include_bytes!("../../zpdf-font/tests/fixtures/noto-sans-sc-subset.ttf");
        let font = b"<< /Type /Font /Subtype /TrueType /BaseFont /NotoSansSubset \
                     /Encoding /WinAnsiEncoding /FirstChar 65 /LastChar 65 /Widths [608] \
                     /FontDescriptor 8 0 R >>"
            .to_vec();
        let font_desc = b"<< /Type /FontDescriptor /FontName /NotoSansSubset /Flags 32 \
                          /FontBBox [0 -120 1000 880] /FontFile2 9 0 R >>"
            .to_vec();
        let font_stream = push_stream(&format!("/Length1 {}", NOTO_SC_TTF.len()), NOTO_SC_TTF);
        let content = push_stream(
            "",
            b"/Pattern cs /P0 scn 0 0 10 10 re f\n\
              0 0 0 rg BT /F1 60 Tf 70 25 Td (A) Tj ET",
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
        objects.push(font_desc);
        objects.push(font_stream);

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
            "SVG DrawGlyphRun with Paint::Tiling must render visible 'A', found {dark_pixels} dark pixels"
        );
        assert!(
            aa_pixels >= 10,
            "expected anti-aliased edge pixels in SVG 'A', found {aa_pixels}"
        );
    }

    fn render_svg_resvg(
        dl: &DisplayList,
        options: &SvgOptions,
        w_px: u32,
        h_px: u32,
    ) -> resvg::tiny_skia::Pixmap {
        let svg = display_list_to_svg(dl, &FontCache::new(), &ImageCache::new(), options);
        let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default())
            .expect("valid SVG");
        let mut pixmap = resvg::tiny_skia::Pixmap::new(w_px, h_px).expect("pixmap");
        let sx = w_px as f32 / tree.size().width();
        let sy = h_px as f32 / tree.size().height();
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(sx, sy),
            &mut pixmap.as_mut(),
        );
        pixmap
    }

    #[test]
    fn dense_darken_tiling_preserves_non_affine_backdrop_and_copy_order_in_svg() {
        // Opaque 50% gray Darken maps backdrop B to min(B, 0.5). On a 0.25-gray
        // page (~64), affine interpolation from black (0) and white (0.5) would
        // erroneously give 0.125 (~32). Even above 1,024 overlapping passes
        // (6 copies × 175 segments = 1,050), SVG must decline the affine
        // shortcut and preserve Darken against the real 0.25-gray backdrop.
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 12.0, 4.0));
        for k in 0..175 {
            let mode = if k % 2 == 0 {
                BlendMode::Darken
            } else {
                BlendMode::Multiply
            };
            let (rect, color) = if k == 0 {
                (Rect::new(0.0, 0.0, 12.0, 2.0), Color::gray(0.5))
            } else {
                (Rect::new(0.0, 2.0, 12.0, 4.0), Color::white())
            };
            cell.push(RenderCommand::PushBlendGroup {
                blend_mode: mode,
                isolated: true,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 12.0, 4.0),
                alpha: 1.0,
                mask: None,
            });
            cell.push(RenderCommand::FillPath {
                path: rect_path(rect.x0, rect.y0, rect.x1, rect.y1),
                rule: FillRule::NonZero,
                paint: Paint::Solid(color),
                alpha: 1.0,
                overprint: None,
            });
            cell.push(RenderCommand::PopBlendGroup);
        }
        let tiling = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 12.0, 4.0),
            x_step: 2.0,
            y_step: 4.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });
        let page_rect = Rect::new(0.0, 0.0, 20.0, 20.0);
        let mut dl = DisplayList::new(page_rect);
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 20.0, 20.0),
            rule: FillRule::NonZero,
            paint: Paint::Tiling(tiling),
            alpha: 1.0,
            overprint: None,
        });

        let pixmap = render_svg_resvg(
            &dl,
            &SvgOptions {
                background: Some(Color::gray(0.25)),
                ..SvgOptions::default()
            },
            80,
            80,
        );
        // Sample inside bottom half of a cell (y_pdf = 5.0 -> cell y = 1.0, in Darken 0.5 strip):
        // y_px = (20 - 5) * 4 = 60, x_px = 40.
        let p = pixmap.pixel(40, 60).expect("in bounds");
        let rgb = [p.red(), p.green(), p.blue()];
        assert!(
            rgb.iter().all(|&c| (60..=68).contains(&c)),
            "SVG dense Darken(0.5) over 0.25-gray page must stay ~64 (NOT ~32 from affine shortcut), got {rgb:?}"
        );
    }

    #[test]
    fn dense_affine_tiling_preserves_accumulated_alpha_on_transparent_target_in_svg() {
        // Bottom strip y=[0..2] has a single 50%-opaque white Normal mark in
        // x=[0..2] (exact accumulated alpha 0.5). Top strip y=[2..4] has 175
        // spanning Multiply/Screen segments across 6 overlapping copies (1,050
        // > 1,024) to force the affine collapse path. On a transparent canvas,
        // the bottom strip must retain alpha ~128 (NOT 255 or 191).
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 12.0, 4.0));
        cell.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 2.0, 2.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 0.5,
            overprint: None,
        });
        for k in 0..175 {
            let mode = if k % 2 == 0 {
                BlendMode::Multiply
            } else {
                BlendMode::Screen
            };
            cell.push(RenderCommand::PushBlendGroup {
                blend_mode: mode,
                isolated: true,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 12.0, 4.0),
                alpha: 1.0,
                mask: None,
            });
            cell.push(RenderCommand::FillPath {
                path: rect_path(0.0, 2.0, 12.0, 4.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::gray(0.5)),
                alpha: 1.0,
                overprint: None,
            });
            cell.push(RenderCommand::PopBlendGroup);
        }
        let tiling = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 12.0, 4.0),
            x_step: 2.0,
            y_step: 4.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });
        let page_rect = Rect::new(0.0, 0.0, 20.0, 20.0);
        let mut dl = DisplayList::new(page_rect);
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 20.0, 20.0),
            rule: FillRule::NonZero,
            paint: Paint::Tiling(tiling),
            alpha: 1.0,
            overprint: None,
        });

        let pixmap = render_svg_resvg(
            &dl,
            &SvgOptions {
                background: None,
                ..SvgOptions::default()
            },
            80,
            80,
        );
        // Sample inside bottom strip (y_pdf = 5.0 -> y_px = 60, x_px = 40):
        let p = pixmap.pixel(40, 60).expect("in bounds");
        let rgba = [p.red(), p.green(), p.blue(), p.alpha()];
        assert!(
            rgba.iter().all(|&c| (122..=134).contains(&c)),
            "SVG dense affine tiling 50% white Normal mark on transparent target must retain alpha ~128 (NOT 255 or 191), got {rgba:?}"
        );
    }

    #[test]
    fn dense_affine_tiling_past_4096_passes_preserves_trailing_detail_in_svg() {
        // 24 overlapping copies (BBox [0..48]×[0..2], XStep=2) × 176 spanning
        // alternating Multiply(0.5)/Screen(0.5) segments = 4,224 (> 4,096) plus
        // a trailing black detail confined to [0..1]×[0..2] (visited only in the
        // 24th/final copy di=0). Both the black detail and the ~170 paired strip
        // must survive completely.
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 48.0, 2.0));
        for k in 0..176 {
            let mode = if k % 2 == 0 {
                BlendMode::Multiply
            } else {
                BlendMode::Screen
            };
            cell.push(RenderCommand::PushBlendGroup {
                blend_mode: mode,
                isolated: true,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 48.0, 2.0),
                alpha: 1.0,
                mask: None,
            });
            cell.push(RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 48.0, 2.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::gray(0.5)),
                alpha: 1.0,
                overprint: None,
            });
            cell.push(RenderCommand::PopBlendGroup);
        }
        cell.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Multiply,
            isolated: true,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 48.0, 2.0),
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
            bbox: Rect::new(0.0, 0.0, 48.0, 2.0),
            x_step: 2.0,
            y_step: 2.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });
        let page_rect = Rect::new(0.0, 0.0, 20.0, 20.0);
        let mut dl = DisplayList::new(page_rect);
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 20.0, 20.0),
            rule: FillRule::NonZero,
            paint: Paint::Tiling(tiling),
            alpha: 1.0,
            overprint: None,
        });

        let pixmap = render_svg_resvg(
            &dl,
            &SvgOptions {
                background: Some(Color::rgb(0.0, 0.0, 1.0)),
                ..SvgOptions::default()
            },
            80,
            80,
        );
        // At 4 px/pt: cell x=[0..1] maps to x_px mod 8 in [0..4), x=[1..2] to [4..8).
        let black_p = pixmap.pixel(42, 40).expect("in bounds");
        let gray_p = pixmap.pixel(46, 40).expect("in bounds");
        assert!(
            black_p.red() <= 4 && black_p.green() <= 4 && black_p.blue() <= 4,
            "SVG trailing black detail past 4,096 affine evaluations must remain black, got ({},{},{})",
            black_p.red(),
            black_p.green(),
            black_p.blue()
        );
        let gray = [gray_p.red(), gray_p.green(), gray_p.blue()];
        assert!(
            gray.iter().all(|&c| (165..=176).contains(&c)),
            "SVG paired Multiply/Screen strip past 4,096 evaluations must be ~170, got {gray:?}"
        );
    }

    #[test]
    fn full_opacity_multi_pass_tiling_applies_edge_coverage_once_in_svg() {
        // Cell paints opaque Normal red followed by opaque Multiply black (two
        // passes, net result = black). At full object opacity (1.0) on white, a
        // half-covered edge (x = 10.5 for fill, x = 54.5 for stroke) must be
        // neutral gray ~(128, 128, 128) — NOT reddish ~(128, 64, 64) from
        // emitting two separate antialiased paths.
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

        let page_rect = Rect::new(0.0, 0.0, 80.0, 40.0);
        let mut dl = DisplayList::new(page_rect);
        // Fill with left edge at fractional x = 10.5.
        dl.push(RenderCommand::FillPath {
            path: rect_path(10.5, 4.0, 36.0, 36.0),
            rule: FillRule::NonZero,
            paint: Paint::Tiling(std::sync::Arc::clone(&tiling)),
            alpha: 1.0,
            overprint: None,
        });
        // Stroke centered at x = 60.0 with width = 11.0 -> left edge at x = 54.5.
        let mut line = Path::new();
        line.move_to(Point::new(60.0, 4.0));
        line.line_to(Point::new(60.0, 36.0));
        dl.push(RenderCommand::StrokePath {
            path: line,
            style: StrokeStyle {
                width: 11.0,
                ..StrokeStyle::default()
            },
            paint: Paint::Tiling(tiling),
            alpha: 1.0,
            overprint: None,
        });

        let pixmap = render_svg_resvg(&dl, &SvgOptions::default(), 80, 40);
        for (label, x) in [
            ("fill half-pixel edge", 10u32),
            ("stroke half-pixel edge", 54u32),
        ] {
            let p = pixmap.pixel(x, 20).expect("in bounds");
            let rgb = [p.red(), p.green(), p.blue()];
            assert!(
                rgb.iter().all(|&c| (120..=136).contains(&c))
                    && (rgb[0] as i32 - rgb[1] as i32).abs() <= 4
                    && (rgb[1] as i32 - rgb[2] as i32).abs() <= 4,
                "SVG full-opacity multi-pass {label} must be neutral gray ~(128,128,128) (NOT ~(128,64,64)), got {rgb:?}"
            );
        }
    }

    #[test]
    fn non_isolated_normal_form_with_opacity_preserves_parent_backdrop_in_svg() {
        // A non-isolated Normal form at 50% opacity containing a full-opacity
        // white Multiply tiling pattern over opaque blue must evaluate Multiply
        // against the live blue parent backdrop (leaving blue unchanged, NOT
        // ~(128, 128, 255)), while still applying the 50% form opacity once to
        // opaque red on the right half (~(128, 0, 128)).
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0));
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
            paint: Paint::Solid(Color::white()),
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

        let page_rect = Rect::new(0.0, 0.0, 80.0, 40.0);
        let mut dl = DisplayList::new(page_rect);
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 80.0, 40.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Normal,
            isolated: false,
            knockout: false,
            bounds: page_rect,
            alpha: 0.5,
            mask: None,
        });
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 40.0, 40.0),
            rule: FillRule::NonZero,
            paint: Paint::Tiling(tiling),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::FillPath {
            path: rect_path(40.0, 0.0, 80.0, 40.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PopBlendGroup);

        let pixmap = render_svg_resvg(&dl, &SvgOptions::default(), 80, 40);
        let left = pixmap.pixel(20, 20).expect("in bounds");
        assert!(
            left.red() <= 2 && left.green() <= 2 && left.blue() >= 253,
            "SVG 50%-opacity non-isolated Normal form with white Multiply pattern over blue must stay blue (NOT ~(128,128,255)), got ({},{},{})",
            left.red(),
            left.green(),
            left.blue()
        );
        let right = pixmap.pixel(60, 20).expect("in bounds");
        assert!(
            (124..=132).contains(&right.red())
                && right.green() <= 2
                && (124..=132).contains(&right.blue()),
            "SVG 50%-opacity non-isolated Normal form with opaque red over blue must be ~(128,0,128), got ({},{},{})",
            right.red(),
            right.green(),
            right.blue()
        );
    }

    #[test]
    fn outer_screen_blend_mode_on_backdrop_dependent_tiling_evaluates_cell_first_in_svg() {
        // Matches `outer_screen_blend_mode_on_backdrop_dependent_tiling_evaluates_cell_first`:
        // left half [0..50] opaque blue, right half [50..100] opaque 50% gray,
        // white Multiply tiling cell painted under outer non-isolated Screen
        // over [10..90]×[10..90].
        // - Over blue: Multiply(white, blue) = blue -> Screen(blue, blue) = blue (NOT white!).
        // - Over 50% gray: Multiply(white, 0.5) = 0.5 -> Screen(0.5, 0.5) = 0.75 (~191).
        // - Outside [10..90]: untouched page backdrop.
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0));
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
            paint: Paint::Solid(Color::white()),
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

        let page_rect = Rect::new(0.0, 0.0, 100.0, 100.0);
        let mut dl = DisplayList::new(page_rect);
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 50.0, 100.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::FillPath {
            path: rect_path(50.0, 0.0, 100.0, 100.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::gray(0.5)),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Screen,
            isolated: false,
            knockout: false,
            bounds: Rect::new(10.0, 10.0, 90.0, 90.0),
            alpha: 1.0,
            mask: None,
        });
        dl.push(RenderCommand::FillPath {
            path: rect_path(10.0, 10.0, 90.0, 90.0),
            rule: FillRule::NonZero,
            paint: Paint::Tiling(tiling),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PopBlendGroup);

        let pixmap = render_svg_resvg(&dl, &SvgOptions::default(), 100, 100);
        let blue_in = pixmap.pixel(25, 50).expect("in bounds");
        assert!(
            blue_in.red() <= 4 && blue_in.green() <= 4 && blue_in.blue() >= 251,
            "SVG white Multiply under non-isolated Screen over blue must stay blue (NOT white), got ({},{},{})",
            blue_in.red(),
            blue_in.green(),
            blue_in.blue()
        );
        let gray_in = pixmap.pixel(75, 50).expect("in bounds");
        let g_rgb = [gray_in.red(), gray_in.green(), gray_in.blue()];
        assert!(
            g_rgb.iter().all(|&c| (186..=196).contains(&c)),
            "SVG white Multiply under non-isolated Screen over 50% gray must be ~191, got {g_rgb:?}"
        );
        let gray_out = pixmap.pixel(95, 50).expect("in bounds");
        let go_rgb = [gray_out.red(), gray_out.green(), gray_out.blue()];
        assert!(
            go_rgb.iter().all(|&c| (124..=132).contains(&c)),
            "SVG untouched 50% gray outside non-isolated Screen fill must stay ~128, got {go_rgb:?}"
        );
    }

    #[test]
    fn cell_opacity_group_around_darken_mark_preserves_backdrop_in_svg() {
        // A pattern cell containing a 50%-opacity non-isolated group around an
        // opaque 50%-gray Darken mark produces a valid `OpacityGroup` whose mode
        // (`Darken`) is excluded from `uniform_assoc`. It must stay in the live
        // backdrop blend plan:
        // - Over 25% gray (~64): Darken(0.5, 0.25) = 0.25 -> 50% group keeps 0.25 (~64, NOT ~96).
        // - Over 80% gray (~204): Darken(0.5, 0.80) = 0.50 -> 50% group blends to 0.65 (~166).
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0));
        cell.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Normal,
            isolated: false,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
            alpha: 0.5,
            mask: None,
        });
        cell.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Darken,
            isolated: true,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
            alpha: 1.0,
            mask: None,
        });
        cell.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 10.0, 10.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::gray(0.5)),
            alpha: 1.0,
            overprint: None,
        });
        cell.push(RenderCommand::PopBlendGroup);
        cell.push(RenderCommand::PopBlendGroup);
        let tiling = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 10.0, 10.0),
            x_step: 10.0,
            y_step: 10.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });

        let page_rect = Rect::new(0.0, 0.0, 80.0, 40.0);
        let mut dl = DisplayList::new(page_rect);
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 40.0, 40.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::gray(0.25)),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::FillPath {
            path: rect_path(40.0, 0.0, 80.0, 40.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::gray(0.8)),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 80.0, 40.0),
            rule: FillRule::NonZero,
            paint: Paint::Tiling(tiling),
            alpha: 1.0,
            overprint: None,
        });

        let pixmap = render_svg_resvg(&dl, &SvgOptions::default(), 80, 40);
        let left = pixmap.pixel(20, 20).expect("in bounds");
        let l_rgb = [left.red(), left.green(), left.blue()];
        assert!(
            l_rgb.iter().all(|&c| (60..=68).contains(&c)),
            "SVG cell OpacityGroup(0.5) around Darken(0.5) over 25% gray must stay ~64 (NOT ~96), got {l_rgb:?}"
        );
        let right = pixmap.pixel(60, 20).expect("in bounds");
        let r_rgb = [right.red(), right.green(), right.blue()];
        assert!(
            r_rgb.iter().all(|&c| (160..=172).contains(&c)),
            "SVG cell OpacityGroup(0.5) around Darken(0.5) over 80% gray must blend to ~166, got {r_rgb:?}"
        );
    }

    #[test]
    fn dense_non_affine_65_copies_128_segments_preserves_last_copy_detail_in_svg() {
        // 65 overlapping copies (BBox [0..130]×[0..2], XStep=2, YStep=2) × 128
        // spanning Darken/Lighten gray segments consume 64*128 = 8,192 passes
        // after copy 64; a black Darken detail confined to [0..1]×[0..2] (only
        // visited in copy 65) must be preserved across bounded chunks, and
        // 50% object opacity must still apply once across chunks.
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 130.0, 2.0));
        for k in 0..128 {
            let (mode, gray) = if k % 2 == 0 {
                (BlendMode::Darken, 0.6)
            } else {
                (BlendMode::Lighten, 0.4)
            };
            cell.push(RenderCommand::PushBlendGroup {
                blend_mode: mode,
                isolated: true,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 130.0, 2.0),
                alpha: 1.0,
                mask: None,
            });
            cell.push(RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 130.0, 2.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::gray(gray)),
                alpha: 1.0,
                overprint: None,
            });
            cell.push(RenderCommand::PopBlendGroup);
        }
        cell.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Darken,
            isolated: true,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 1.0, 2.0),
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
            bbox: Rect::new(0.0, 0.0, 130.0, 2.0),
            x_step: 2.0,
            y_step: 2.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });

        // Compact 16×8 page rendered at 16×8 px keeps resvg fast while testing
        // both 100% opacity (left half [0..8]) and 50% opacity (right half [8..16]).
        let page_rect = Rect::new(0.0, 0.0, 16.0, 8.0);
        let mut dl = DisplayList::new(page_rect);
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 8.0, 8.0),
            rule: FillRule::NonZero,
            paint: Paint::Tiling(std::sync::Arc::clone(&tiling)),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::FillPath {
            path: rect_path(8.0, 0.0, 16.0, 8.0),
            rule: FillRule::NonZero,
            paint: Paint::Tiling(tiling),
            alpha: 0.5,
            overprint: None,
        });

        let pixmap = render_svg_resvg(
            &dl,
            &SvgOptions {
                background: Some(Color::white()),
                ..SvgOptions::default()
            },
            16,
            8,
        );
        // x=4 (even -> cell x=[0..1], copy-65 black detail at 100% opacity): ~0.
        let opaque_black = pixmap.pixel(4, 4).expect("in bounds");
        let ob_rgb = [
            opaque_black.red(),
            opaque_black.green(),
            opaque_black.blue(),
        ];
        assert!(
            ob_rgb.iter().all(|&c| c <= 4),
            "SVG 65th-copy black detail past 8,192 non-affine passes must remain black, got {ob_rgb:?}"
        );
        // x=5 (odd -> cell x=[1..2], clamped to 0.6 by Darken(0.6)/Lighten(0.4) over white): ~153.
        let opaque_gray = pixmap.pixel(5, 4).expect("in bounds");
        let og_rgb = [opaque_gray.red(), opaque_gray.green(), opaque_gray.blue()];
        assert!(
            og_rgb.iter().all(|&c| (148..=158).contains(&c)),
            "SVG 65-copy Darken(0.6)/Lighten(0.4) over white must be ~153, got {og_rgb:?}"
        );
        // x=10 (even -> cell x=[0..1], copy-65 black detail at 50% opacity over white): ~128.
        let half_black = pixmap.pixel(10, 4).expect("in bounds");
        let hb_rgb = [half_black.red(), half_black.green(), half_black.blue()];
        assert!(
            hb_rgb.iter().all(|&c| (122..=134).contains(&c)),
            "SVG 65th-copy black detail at 50% object opacity across chunks must be ~128, got {hb_rgb:?}"
        );
    }

    #[test]
    fn dense_non_affine_65_copies_on_letter_page_bounds_filter_memory_in_svg() {
        // 65 overlapping copies × 128 spanning Darken/Lighten gray segments plus
        // a copy-65 black detail painted into a small 20×20 box on a full
        // 612×792 Letter page must NOT emit an 8,192-feImage full-page `<filter>`
        // (which would allocate ~15+ GiB in resvg). It evaluates in-place with
        // O(1) raster memory and renders accurately through resvg.
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 130.0, 2.0));
        for k in 0..128 {
            let (mode, gray) = if k % 2 == 0 {
                (BlendMode::Darken, 0.6)
            } else {
                (BlendMode::Lighten, 0.4)
            };
            cell.push(RenderCommand::PushBlendGroup {
                blend_mode: mode,
                isolated: true,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 130.0, 2.0),
                alpha: 1.0,
                mask: None,
            });
            cell.push(RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 130.0, 2.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::gray(gray)),
                alpha: 1.0,
                overprint: None,
            });
            cell.push(RenderCommand::PopBlendGroup);
        }
        cell.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Darken,
            isolated: true,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 1.0, 2.0),
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
            bbox: Rect::new(0.0, 0.0, 130.0, 2.0),
            x_step: 2.0,
            y_step: 2.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });

        let page_rect = Rect::new(0.0, 0.0, 612.0, 792.0);
        let mut dl = DisplayList::new(page_rect);
        // Small 8×8 box at (100, 100) at 50% opacity on a white 612×792 Letter page.
        dl.push(RenderCommand::FillPath {
            path: rect_path(100.0, 100.0, 108.0, 108.0),
            rule: FillRule::NonZero,
            paint: Paint::Tiling(std::sync::Arc::clone(&tiling)),
            alpha: 0.5,
            overprint: None,
        });

        let svg = display_list_to_svg(
            &dl,
            &FontCache::new(),
            &ImageCache::new(),
            &SvgOptions {
                background: Some(Color::white()),
                ..SvgOptions::default()
            },
        );
        // Flat dense pattern on an opaque Letter page must not emit any `<feImage>`
        // filter buffers (evaluates in-place in O(1) memory).
        assert_eq!(
            svg.matches("<feImage").count(),
            0,
            "flat dense non-affine pattern on opaque Letter page must evaluate in-place with 0 <feImage> filter buffers"
        );

        let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default())
            .expect("valid Letter SVG");
        let mut pixmap = resvg::tiny_skia::Pixmap::new(612, 792).expect("pixmap");
        let sx = 612.0 / tree.size().width();
        let sy = 792.0 / tree.size().height();
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(sx, sy),
            &mut pixmap.as_mut(),
        );

        // PDF y=104 maps to SVG y = 792 - 104 = 688.
        // x=104 (even -> copy-65 black detail at 50% opacity over white): ~128.
        let black_50 = pixmap.pixel(104, 688).expect("in bounds");
        let b_rgb = [black_50.red(), black_50.green(), black_50.blue()];
        assert!(
            b_rgb.iter().all(|&c| (122..=134).contains(&c)),
            "Letter-page 8×8 copy-65 black detail at 50% opacity must be ~128, got {b_rgb:?}"
        );
        // x=105 (odd -> Darken(0.6)/Lighten(0.4) = 0.6 at 50% over white -> 0.8 = ~204).
        let gray_50 = pixmap.pixel(105, 688).expect("in bounds");
        let g_rgb = [gray_50.red(), gray_50.green(), gray_50.blue()];
        assert!(
            g_rgb.iter().all(|&c| (198..=210).contains(&c)),
            "Letter-page 8×8 Darken(0.6) strip at 50% opacity over white must be ~204, got {g_rgb:?}"
        );
        // Outside box must stay pure white.
        let outside = pixmap.pixel(50, 50).expect("in bounds");
        assert_eq!(
            [outside.red(), outside.green(), outside.blue()],
            [255, 255, 255],
            "pixels outside 8×8 box on Letter page must stay untouched white"
        );

        // Also verify the translucent-page path (background: None with a semi-transparent
        // backdrop rect): emits at most 2 `<feImage>` primitives inside ONE tightly
        // scoped 12×12 `<filter>` rather than 8,320 full-page `<feImage>` buffers.
        let mut dl_trans = DisplayList::new(page_rect);
        dl_trans.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 612.0, 792.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgba(1.0, 1.0, 1.0, 0.5)),
            alpha: 1.0,
            overprint: None,
        });
        dl_trans.push(RenderCommand::FillPath {
            path: rect_path(100.0, 100.0, 108.0, 108.0),
            rule: FillRule::NonZero,
            paint: Paint::Tiling(std::sync::Arc::clone(&tiling)),
            alpha: 0.5,
            overprint: None,
        });
        let svg_trans = display_list_to_svg(
            &dl_trans,
            &FontCache::new(),
            &ImageCache::new(),
            &SvgOptions {
                background: None,
                ..SvgOptions::default()
            },
        );
        assert!(
            svg_trans.matches("<feImage").count() <= 2,
            "translucent Letter-page flat dense pattern must use <= 2 <feImage> primitives, got {}",
            svg_trans.matches("<feImage").count()
        );
        assert!(
            svg_trans.contains("width=\"12\" height=\"12\" color-interpolation-filters"),
            "translucent Letter-page 8×8 box filter must be scoped to 12×12"
        );
        let tree_t =
            resvg::usvg::Tree::from_data(svg_trans.as_bytes(), &resvg::usvg::Options::default())
                .expect("valid translucent Letter SVG");
        let mut pixmap_t = resvg::tiny_skia::Pixmap::new(612, 792).expect("pixmap");
        let sx_t = 612.0 / tree_t.size().width();
        let sy_t = 792.0 / tree_t.size().height();
        resvg::render(
            &tree_t,
            resvg::tiny_skia::Transform::from_scale(sx_t, sy_t),
            &mut pixmap_t.as_mut(),
        );
        // Outside box must keep the untouched 50%-alpha white backdrop (~128 premultiplied).
        let out_t = pixmap_t.pixel(50, 50).expect("in bounds");
        assert!(
            (122..=134).contains(&out_t.alpha()),
            "translucent backdrop outside 8×8 box must stay ~128 alpha, got {}",
            out_t.alpha()
        );
        // Inside box at x=104 (copy-65 opaque black Darken over 50%-white, blended at 50% fill alpha):
        // eval has premul (0, 0, 0, 1); orig has premul (0.5, 0.5, 0.5, 0.5);
        // 0.5 * eval + 0.5 * orig = premul (0.25, 0.25, 0.25, 0.75) => alpha ~191, rgb ~64.
        let in_t = pixmap_t.pixel(104, 688).expect("in bounds");
        assert!(
            (184..=198).contains(&in_t.alpha()) && (58..=70).contains(&in_t.red()),
            "translucent copy-65 black detail at 50% must have premul ~64/191, got {:?}",
            [in_t.red(), in_t.green(), in_t.blue(), in_t.alpha()]
        );
    }

    #[test]
    fn dense_opacity_group_on_letter_page_scopes_filter_to_affected_bounds_in_svg() {
        // A small 20×20 box on a 612×792 Letter page painted with an `OpacityGroup`
        // (which requires `<filter>` evaluation rather than flat `<rect>` blending)
        // must scope its `<filter>` region tightly to the affected box instead of
        // allocating full 612×792 intermediate buffers.
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 2.0));
        cell.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Normal,
            isolated: false,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 20.0, 2.0),
            alpha: 0.5,
            mask: None,
        });
        for _ in 0..8 {
            cell.push(RenderCommand::PushBlendGroup {
                blend_mode: BlendMode::Darken,
                isolated: true,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 20.0, 2.0),
                alpha: 1.0,
                mask: None,
            });
            cell.push(RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 20.0, 2.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::gray(0.5)),
                alpha: 1.0,
                overprint: None,
            });
            cell.push(RenderCommand::PopBlendGroup);
        }
        cell.push(RenderCommand::PopBlendGroup);
        let tiling = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 20.0, 2.0),
            x_step: 2.0,
            y_step: 2.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });

        let page_rect = Rect::new(0.0, 0.0, 612.0, 792.0);
        let mut dl = DisplayList::new(page_rect);
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 612.0, 792.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::gray(0.25)),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::FillPath {
            path: rect_path(100.0, 100.0, 120.0, 120.0),
            rule: FillRule::NonZero,
            paint: Paint::Tiling(std::sync::Arc::clone(&tiling)),
            alpha: 1.0,
            overprint: None,
        });

        let svg = display_list_to_svg(
            &dl,
            &FontCache::new(),
            &ImageCache::new(),
            &SvgOptions::default(),
        );
        // Filter must be scoped tightly to the 20×20 box (+2pt AA pad => 24×24),
        // NOT the full 612×792 Letter page.
        assert!(
            svg.contains("width=\"24\" height=\"24\""),
            "expected `<filter>` scoped to 24×24 affected bounds on 612×792 page"
        );
        assert!(
            !svg.contains("<filter id=\"f")
                || !svg.contains("width=\"612\" height=\"792\" color-interpolation-filters"),
            "must not emit a full-page 612×792 <filter> for a 20×20 patterned box"
        );

        let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default())
            .expect("valid scoped Letter SVG");
        let mut pixmap = resvg::tiny_skia::Pixmap::new(612, 792).expect("pixmap");
        let sx = 612.0 / tree.size().width();
        let sy = 792.0 / tree.size().height();
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(sx, sy),
            &mut pixmap.as_mut(),
        );

        // Inside 20×20 box (PDF y=110 -> SVG y=682): Darken(0.5, 0.25) = 0.25 (~64).
        let inside = pixmap.pixel(110, 682).expect("in bounds");
        let in_rgb = [inside.red(), inside.green(), inside.blue()];
        assert!(
            in_rgb.iter().all(|&c| (60..=68).contains(&c)),
            "scoped OpacityGroup around Darken(0.5) over 25% gray must stay ~64, got {in_rgb:?}"
        );
        // Outside box must keep the 25% gray page background (~64).
        let outside = pixmap.pixel(50, 50).expect("in bounds");
        let out_rgb = [outside.red(), outside.green(), outside.blue()];
        assert!(
            out_rgb.iter().all(|&c| (60..=68).contains(&c)),
            "background outside scoped filter region must remain ~64, got {out_rgb:?}"
        );

        // Also verify a full-page 612×792 box with the same OpacityGroup cell:
        // must split into bounded chunks where every `<filter>` stays within the
        // intermediate-pixel budget (<= 34 primitives per full-page filter).
        let mut dl_full = DisplayList::new(page_rect);
        dl_full.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 612.0, 792.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::gray(0.25)),
            alpha: 1.0,
            overprint: None,
        });
        dl_full.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 612.0, 792.0),
            rule: FillRule::NonZero,
            paint: Paint::Tiling(tiling),
            alpha: 1.0,
            overprint: None,
        });
        let svg_full = display_list_to_svg(
            &dl_full,
            &FontCache::new(),
            &ImageCache::new(),
            &SvgOptions::default(),
        );
        let filter_count = svg_full.matches("<filter ").count();
        assert!(
            filter_count >= 2,
            "full-page 612×792 OpacityGroup exceeding 34 primitives must split into bounded chunks, got {filter_count}"
        );
        for chunk in svg_full.split("<filter ").skip(1) {
            let body = chunk.split("</filter>").next().unwrap_or("");
            let prims = body.matches("<feImage").count()
                + body.matches("<feBlend").count()
                + body.matches("<feComposite").count()
                + body.matches("<feComponentTransfer").count()
                + body.matches("<feFlood").count();
            assert!(
                prims <= 34,
                "full-page 612×792 <filter> exceeded intermediate-pixel primitive budget (34): got {prims}"
            );
        }
        let tree_f =
            resvg::usvg::Tree::from_data(svg_full.as_bytes(), &resvg::usvg::Options::default())
                .expect("valid full-page chunked Letter SVG");
        // Render at 1/4 scale (153×198) so debug-mode resvg completes in <1s while
        // still exercising the full multi-chunk `<filter>` chain end-to-end.
        let mut pixmap_f = resvg::tiny_skia::Pixmap::new(153, 198).expect("pixmap");
        let sx_f = 153.0 / tree_f.size().width();
        let sy_f = 198.0 / tree_f.size().height();
        resvg::render(
            &tree_f,
            resvg::tiny_skia::Transform::from_scale(sx_f, sy_f),
            &mut pixmap_f.as_mut(),
        );
        let px_f = pixmap_f.pixel(76, 100).expect("in bounds");
        let f_rgb = [px_f.red(), px_f.green(), px_f.blue()];
        assert!(
            f_rgb.iter().all(|&c| (60..=68).contains(&c)),
            "full-page chunked OpacityGroup on Letter page must stay ~64, got {f_rgb:?}"
        );
    }
}
