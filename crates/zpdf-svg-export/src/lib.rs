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
    BlendMode, Color, DisplayList, FillRule, GlyphRun, ImageDraw, InlineImageDraw, LineCap,
    LineJoin, Paint, Path, PathElement, RenderCommand, SoftMask, SoftMaskKind, StrokeStyle,
};
use zpdf_font::{FontCache, FontId, GlyphOutline, LoadedFont, OutlineCommand};
use zpdf_image::{DecodedImage, ImageCache, ImageId};
use zpdf_render::{PageRenderInfo, RenderBackend};
use zpdf_render_cpu::CpuRenderer;

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
/// Tiling patterns nest up to the interpreter's `MAX_FORM_DEPTH` (16).
const MAX_TILING_DEPTH: u8 = 16;
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

    let mut writer = SvgWriter::new(
        rect,
        width,
        height,
        &display_list.commands,
        fonts,
        images,
        options,
    );

    if let Some(bg) = &options.background {
        writer.out.push_str("<rect width=\"");
        push_num(&mut writer.out, width);
        writer.out.push_str("\" height=\"");
        push_num(&mut writer.out, height);
        writer.out.push_str("\" fill=\"");
        writer.out.push_str(&hex_color(bg));
        writer.out.push_str("\"/>\n");
    }

    writer.emit_commands(&display_list.commands, EmitDepth::default());
    writer.flush_pending_backdrop();
    writer.flush_disjoint_backdrop();

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
enum Frame<'a> {
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
        prev_non_isolated_ga_depth: usize,
        prev_backdrop_cpu: Option<Box<SvgBackdropCpu<'a>>>,
        prev_backdrop_cpu_start_idx: usize,
        prev_backdrop_cpu_transparent: bool,
        swapped_backdrop_cpu: bool,
    },
    NonIsolatedFilterGroup {
        blend_mode: BlendMode,
        alpha: f32,
        mask_id: Option<u32>,
        trailing_reserve: usize,
    },
}

/// Lazily initialized running CPU backdrop used as a bounded O(1)-memory
/// raster fallback when overlapping dense patterns exhaust `MAX_BACKDROP_FANOUT`
/// or the SVG `<filter>` intermediate-pixel budget.
struct SvgBackdropCpu<'a> {
    renderer: CpuRenderer<'a>,
    cursor: usize,
    scale: f64,
    width: u32,
    height: u32,
    exhausted: bool,
}

struct SvgWriter<'a> {
    defs: String,
    out: String,
    has_opaque_background: bool,
    page_rect: zpdf_core::Rect,
    page_commands: &'a [RenderCommand],
    page_background: Color,
    current_page_cmd_idx: Option<usize>,
    svg_backdrop_diverged_depth: usize,
    backdrop_cpu: Option<Box<SvgBackdropCpu<'a>>>,
    backdrop_cpu_start_idx: usize,
    backdrop_cpu_transparent: bool,
    active_clip_cmd_indices: Vec<usize>,
    blend_depth: usize,
    isolated_depth: usize,
    non_isolated_filter_depth: usize,
    /// Number of enclosing `NonIsolatedFilterGroup` frames (NOT reset across
    /// isolated children, unlike `non_isolated_filter_depth`), used to detect
    /// when `CpuRenderer` is deferring commands inside an outer non-isolated group.
    non_isolated_ancestor_depth: usize,
    /// Number of active non-`Normal` `NonIsolatedFilterGroup` frames in the
    /// current `pending_backdrop` filter chain (which require `ga` alpha-tracking
    /// primitives on every step inside them).
    non_isolated_ga_depth: usize,
    backdrop_snapshots: usize,
    /// Cumulative multiplicative fan-out across backdrop `<use>` snapshots that
    /// reference the prior page more than once; capped by [`MAX_BACKDROP_FANOUT`]
    /// so sequential dense patterns cannot trigger exponential `<use>` expansion.
    backdrop_fanout: usize,
    /// Open batch of pairwise-disjoint translucent multi-pass tiling items
    /// sharing one base backdrop `<g id="b{bg_id}">` snapshot, flushed as a
    /// single premultiplied `R * w + B * (1 - w)` SVG filter composition.
    pending_backdrop: Option<PendingBackdrop>,
    /// Shared pre-run backdrop snapshot reused across consecutive pairwise-
    /// disjoint dense/chunked tiling fills so repeated fills never wrap or
    /// re-evaluate earlier dense fills.
    disjoint_backdrop: Option<DisjointBackdropBase>,
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
    /// Retains `Arc<InlineImage>` allocations keyed by address in `image_defs`
    /// (`ImageDefKey::Inline`) so freed temporary inline images (e.g. from
    /// transformed Type 3 soft masks) cannot alias a later image.
    retained_inline_images: Vec<std::sync::Arc<zpdf_display_list::InlineImage>>,
    /// Soft-mask defs already written. A soft mask the interpreter reuses
    /// (one `gs` applied to many objects shares one mask list) is written
    /// once and referenced by every group it masks. Retains the keyed
    /// `Arc<DisplayList>` so raw pointer keys can never alias freed temporary
    /// allocations (such as uncached Type 3 glyph masks or nested transformed
    /// soft masks).
    mask_defs: HashMap<MaskDefKey, (std::sync::Arc<DisplayList>, u32)>,
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
    Inline(usize),
    Type3Stencil {
        font: FontId,
        glyph: u16,
        image: ImageId,
        tint: [u32; 4],
    },
}

/// Independent nesting depths for soft-mask (`MAX_MASK_DEPTH`) and
/// tiling-pattern (`MAX_TILING_DEPTH`) definitions so nested tiles up to depth
/// 16 do not exhaust the 8-level soft-mask budget.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
struct EmitDepth {
    mask: u8,
    tile: u8,
}

impl EmitDepth {
    fn is_top_level(self) -> bool {
        self.mask == 0 && self.tile == 0
    }

    fn enter_mask(self) -> Self {
        Self {
            mask: self.mask.saturating_add(1),
            tile: self.tile,
        }
    }

    fn enter_tile(self) -> Self {
        Self {
            mask: self.mask,
            tile: self.tile.saturating_add(1),
        }
    }
}

/// Identity of a soft-mask def: the retained source mask list plus any
/// placement transform applied to it (for Type 3 glyph-local masks), the
/// parameters written around it, and the nesting depths it was written at
/// (masks nested past [`MAX_MASK_DEPTH`] or tiles past [`MAX_TILING_DEPTH`]
/// are left out).
#[derive(PartialEq, Eq, Hash)]
struct MaskDefKey {
    commands: *const DisplayList,
    placement: Option<[u64; 6]>,
    kind: SoftMaskKind,
    offset: [u32; 2],
    backdrop_luma: u32,
    origin_scale: [u64; 3],
    depth: EmitDepth,
}

fn canonical_f64_bits(v: f64) -> u64 {
    if v == 0.0 {
        0.0f64.to_bits()
    } else if v.is_nan() {
        f64::NAN.to_bits()
    } else {
        v.to_bits()
    }
}

fn canonical_f32_bits(v: f32) -> u32 {
    if v == 0.0 {
        0.0f32.to_bits()
    } else if v.is_nan() {
        f32::NAN.to_bits()
    } else {
        v.to_bits()
    }
}

fn matrix_key_bits(m: &zpdf_core::Matrix) -> [u64; 6] {
    [
        canonical_f64_bits(m.a),
        canonical_f64_bits(m.b),
        canonical_f64_bits(m.c),
        canonical_f64_bits(m.d),
        canonical_f64_bits(m.e),
        canonical_f64_bits(m.f),
    ]
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
    depth: EmitDepth,
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

#[derive(Clone)]
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

#[derive(Clone)]
struct PendingBackdrop {
    bg_id: u32,
    steps: Vec<BackdropStep>,
    total_primitives: usize,
    region: SvgRect,
}

#[derive(Clone)]
struct DisjointBackdropBase {
    bg_id: u32,
    opaque: bool,
    /// For translucent scoped runs, the `<clipPath id="c{id}">` referenced by
    /// the base `<use href="#b{bg_id}" clip-path="url(#c{id})"/>` in `self.out`,
    /// materialized into `self.defs` on flush with `evenodd` holes for every
    /// `painted_regions` rect.
    comp_clip_id: Option<u32>,
    /// Byte length of `self.out` right after the last disjoint dense overlay.
    /// Any intervening append into `self.out` invalidates reuse.
    out_len: usize,
    painted_regions: Vec<SvgRect>,
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
    /// Shared `Arc<InlineImage>` wrappers for glyph-local `ImageId`s referenced
    /// inside soft masks so repeated placements reuse one decoded image and one
    /// SVG `<image>` def.
    inline_images: HashMap<ImageId, std::sync::Arc<zpdf_display_list::InlineImage>>,
    font_matrix: [f64; 6],
}

impl<'a> SvgWriter<'a> {
    fn new(
        rect: zpdf_core::Rect,
        width: f64,
        height: f64,
        page_commands: &'a [RenderCommand],
        fonts: &'a FontCache,
        images: &'a ImageCache,
        options: &SvgOptions,
    ) -> Self {
        Self {
            defs: String::new(),
            out: String::new(),
            has_opaque_background: options.background.is_some_and(|c| c.a >= 1.0),
            page_rect: rect,
            page_commands,
            page_background: options
                .background
                .unwrap_or(Color::rgba(0.0, 0.0, 0.0, 0.0)),
            current_page_cmd_idx: None,
            svg_backdrop_diverged_depth: 0,
            backdrop_cpu: None,
            backdrop_cpu_start_idx: 0,
            backdrop_cpu_transparent: false,
            active_clip_cmd_indices: Vec::new(),
            blend_depth: 0,
            isolated_depth: 0,
            non_isolated_filter_depth: 0,
            non_isolated_ancestor_depth: 0,
            non_isolated_ga_depth: 0,
            backdrop_snapshots: 0,
            backdrop_fanout: 1,
            pending_backdrop: None,
            disjoint_backdrop: None,
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
            retained_inline_images: Vec::new(),
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

    /// Exact filter primitive count needed to open a `NonIsolatedFilterGroup`,
    /// absorb one trailing overlay inside it, and close it. Reserving all three
    /// upfront guarantees `close_non_isolated_filter_group` never exceeds the
    /// active `<filter>` intermediate-pixel budget.
    fn non_isolated_group_primitives(
        blend_mode: BlendMode,
        alpha: f32,
        has_mask: bool,
        outer_tracks_ga: bool,
    ) -> (usize, usize, usize) {
        let push_prims = if blend_mode != BlendMode::Normal {
            1
        } else {
            0
        };
        let inner_tracks_ga = outer_tracks_ga || blend_mode != BlendMode::Normal;
        let trailing_reserve = if inner_tracks_ga { 3 } else { 2 };
        let pop_prims = if blend_mode == BlendMode::Normal {
            4 + if outer_tracks_ga { 3 } else { 0 }
        } else {
            3 + if alpha < 1.0 || has_mask { 2 } else { 0 } + if outer_tracks_ga { 1 } else { 0 }
        };
        (push_prims, trailing_reserve, pop_prims)
    }

    fn emit_commands(&mut self, commands: &[RenderCommand], depth: EmitDepth) {
        let mut frames: Vec<Frame<'a>> = Vec::new();
        let mut idx = 0usize;
        while idx < commands.len() {
            let cmd = &commands[idx];
            if depth.is_top_level() {
                self.current_page_cmd_idx = Some(idx);
            }
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
                RenderCommand::DrawInlineImage(draw) => {
                    self.emit_inline_image(draw);
                }
                RenderCommand::PushClip { path, rule } => {
                    let frame = Frame::Clip {
                        prev_clip: self.clip,
                        prev_mask: self.stroke_mask,
                        prev_bounds: self.clip_bounds,
                    };
                    self.push_clip(path, rule);
                    if depth.is_top_level() {
                        self.active_clip_cmd_indices.push(idx);
                    }
                    frames.push(frame);
                }
                RenderCommand::PushClipStroke { path, style } => {
                    let frame = Frame::Clip {
                        prev_clip: self.clip,
                        prev_mask: self.stroke_mask,
                        prev_bounds: self.clip_bounds,
                    };
                    self.push_clip_stroke(path, style);
                    if depth.is_top_level() {
                        self.active_clip_cmd_indices.push(idx);
                    }
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
                        if depth.is_top_level() {
                            self.active_clip_cmd_indices.pop();
                        }
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
                    let (push_prims, trailing_reserve, pop_prims) =
                        Self::non_isolated_group_primitives(
                            *blend_mode,
                            eff_alpha,
                            mask_id.is_some(),
                            self.non_isolated_ga_depth > 0,
                        );
                    let group_prims = push_prims
                        .saturating_add(trailing_reserve)
                        .saturating_add(pop_prims);
                    if depth.is_top_level()
                        && is_non_isolated_group
                        && (self.pending_backdrop.is_some() || !self.out.is_empty())
                        && self.enqueue_backdrop_step(
                            BackdropStep::PushNonIsolatedGroup {
                                blend_mode: *blend_mode,
                            },
                            group_prims,
                            None,
                            false,
                        )
                    {
                        self.blend_depth += 1;
                        self.non_isolated_filter_depth += 1;
                        self.non_isolated_ancestor_depth += 1;
                        if *blend_mode != BlendMode::Normal {
                            self.isolated_depth += 1;
                            self.non_isolated_ga_depth += 1;
                        }
                        frames.push(Frame::NonIsolatedFilterGroup {
                            blend_mode: *blend_mode,
                            alpha: eff_alpha,
                            mask_id,
                            trailing_reserve,
                        });
                    } else if depth.is_top_level()
                        && is_non_isolated_group
                        && self.non_isolated_filter_depth == 0
                        && self.svg_backdrop_diverged_depth == 0
                        && (self.pending_backdrop.is_some() || !self.out.is_empty())
                        && {
                            let end_idx = svg_find_matching_pop_blend(commands, idx);
                            let affected =
                                self.clip_bounds.unwrap_or_else(|| self.full_page_rect());
                            let handled =
                                svg_slice_needs_non_isolated_backdrop(&commands[idx + 1..end_idx])
                                    && self.emit_raster_backdrop_range_fallback(
                                        idx, end_idx, affected,
                                    );
                            if handled {
                                idx = end_idx + 1;
                            }
                            handled
                        }
                    {
                        continue;
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
                            prev_non_isolated_ga_depth: 0,
                            prev_backdrop_cpu: None,
                            prev_backdrop_cpu_start_idx: 0,
                            prev_backdrop_cpu_transparent: false,
                            swapped_backdrop_cpu: false,
                        });
                    } else {
                        self.flush_disjoint_backdrop();
                        let prev_out = std::mem::take(&mut self.out);
                        let prev_pending = self.pending_backdrop.take();
                        let prev_non_isolated_depth =
                            std::mem::replace(&mut self.non_isolated_filter_depth, 0);
                        let prev_non_isolated_ga_depth =
                            std::mem::replace(&mut self.non_isolated_ga_depth, 0);
                        let (
                            swapped_backdrop_cpu,
                            prev_backdrop_cpu,
                            prev_backdrop_cpu_start_idx,
                            prev_backdrop_cpu_transparent,
                        ) = if depth.is_top_level()
                            && *isolated
                            && self.non_isolated_ancestor_depth > 0
                        {
                            (
                                true,
                                self.backdrop_cpu.take(),
                                std::mem::replace(&mut self.backdrop_cpu_start_idx, idx + 1),
                                std::mem::replace(&mut self.backdrop_cpu_transparent, true),
                            )
                        } else {
                            (false, None, 0, false)
                        };
                        self.blend_depth += 1;
                        self.isolated_depth += 1;
                        if !*isolated {
                            self.svg_backdrop_diverged_depth += 1;
                        }
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
                            prev_non_isolated_ga_depth,
                            prev_backdrop_cpu,
                            prev_backdrop_cpu_start_idx,
                            prev_backdrop_cpu_transparent,
                            swapped_backdrop_cpu,
                        });
                    }
                }
                RenderCommand::PopBlendGroup => match frames.last() {
                    Some(Frame::NonIsolatedFilterGroup { .. }) => {
                        if let Some(Frame::NonIsolatedFilterGroup {
                            blend_mode,
                            alpha,
                            mask_id,
                            trailing_reserve,
                        }) = frames.pop()
                        {
                            self.close_non_isolated_filter_group(
                                blend_mode,
                                alpha,
                                mask_id,
                                trailing_reserve,
                            );
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
                            prev_non_isolated_ga_depth,
                            prev_backdrop_cpu,
                            prev_backdrop_cpu_start_idx,
                            prev_backdrop_cpu_transparent,
                            swapped_backdrop_cpu,
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
                                prev_non_isolated_ga_depth,
                                prev_backdrop_cpu,
                                prev_backdrop_cpu_start_idx,
                                prev_backdrop_cpu_transparent,
                                swapped_backdrop_cpu,
                                depth,
                            );
                        }
                    }
                    _ => tracing::warn!("unbalanced PopBlendGroup ignored"),
                },
            }
            idx += 1;
        }
        // A malformed stream can leave frames unclosed; keep the SVG well-formed.
        for frame in frames.drain(..).rev() {
            match frame {
                Frame::NonIsolatedFilterGroup {
                    blend_mode,
                    alpha,
                    mask_id,
                    trailing_reserve,
                } => {
                    self.close_non_isolated_filter_group(
                        blend_mode,
                        alpha,
                        mask_id,
                        trailing_reserve,
                    );
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
                    prev_non_isolated_ga_depth,
                    prev_backdrop_cpu,
                    prev_backdrop_cpu_start_idx,
                    prev_backdrop_cpu_transparent,
                    swapped_backdrop_cpu,
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
                        prev_non_isolated_ga_depth,
                        prev_backdrop_cpu,
                        prev_backdrop_cpu_start_idx,
                        prev_backdrop_cpu_transparent,
                        swapped_backdrop_cpu,
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
                    if depth.is_top_level() {
                        self.active_clip_cmd_indices.pop();
                    }
                }
            }
        }
        if depth.is_top_level() {
            self.current_page_cmd_idx = None;
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
    fn opaque_backdrop(&self, depth: EmitDepth) -> bool {
        depth.is_top_level() && self.has_opaque_background && self.isolated_depth == 0
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
        depth: EmitDepth,
    ) -> Option<u32> {
        if depth.tile >= MAX_TILING_DEPTH || commands.commands.is_empty() {
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
        let saved_disjoint = self.disjoint_backdrop.take();
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
                    self.emit_commands(&commands.commands, depth.enter_tile());
                    self.out.push_str("</g>\n");
                } else {
                    self.emit_commands(&commands.commands, depth.enter_tile());
                }
                drew_any = true;
            }
        }

        self.flush_pending_backdrop();
        self.flush_disjoint_backdrop();
        let body = std::mem::replace(&mut self.out, saved_out);
        self.pending_backdrop = saved_pending;
        self.disjoint_backdrop = saved_disjoint;
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
        depth: EmitDepth,
    ) {
        for step in steps {
            match step {
                zpdf_display_list::TilingBlendStep::Segment(seg) => {
                    let _ = writeln!(
                        self.out,
                        "<g style=\"mix-blend-mode:{css_name};isolation:isolate;\">"
                    );
                    self.emit_commands(&seg.commands.commands, depth.enter_tile());
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
                        self.emit_commands(&m_seg.commands.commands, depth.enter_tile().enter_mask());
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
        depth: EmitDepth,
    ) -> Option<u32> {
        if depth.tile >= MAX_TILING_DEPTH || steps.is_empty() {
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
        let saved_disjoint = self.disjoint_backdrop.take();
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
        self.flush_disjoint_backdrop();
        let body = std::mem::replace(&mut self.out, saved_out);
        self.pending_backdrop = saved_pending;
        self.disjoint_backdrop = saved_disjoint;
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
        depth: EmitDepth,
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
                        self.emit_commands(&seg.commands.commands, depth.enter_tile());
                        self.out.push_str("</g>\n");
                    } else {
                        self.emit_commands(&seg.commands.commands, depth.enter_tile());
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
        depth: EmitDepth,
    ) -> Option<(u32, u32, u32)> {
        if depth.tile >= MAX_TILING_DEPTH || steps.is_empty() {
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
        let saved_disjoint = self.disjoint_backdrop.take();
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
        self.flush_disjoint_backdrop();
        let blended_steps = std::mem::replace(&mut self.out, saved_out);
        self.pending_backdrop = saved_pending;
        self.disjoint_backdrop = saved_disjoint;
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
        depth: EmitDepth,
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
        depth: EmitDepth,
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
                        if depth.is_top_level()
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
        let max_pad = self.page_width.max(self.page_height);
        let miter = match style.join {
            LineJoin::Miter => {
                if style.miter_limit.is_finite() {
                    (style.miter_limit as f64).max(1.0)
                } else {
                    max_pad
                }
            }
            LineJoin::Round | LineJoin::Bevel => 1.5,
        };
        (half * miter + 2.0).min(max_pad)
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

    /// Finalize any open [`DisjointBackdropBase`] by emitting its deferred
    /// complement `<clipPath>` (with `evenodd` holes for every overlaid region
    /// on translucent backdrops).
    fn flush_disjoint_backdrop(&mut self) {
        let Some(base) = self.disjoint_backdrop.take() else {
            return;
        };
        if let Some(cid) = base.comp_clip_id {
            let _ = write!(self.defs, "<clipPath id=\"c{cid}\"><path d=\"M0 0H");
            push_num(&mut self.defs, self.page_width);
            self.defs.push('V');
            push_num(&mut self.defs, self.page_height);
            self.defs.push_str("H0Z");
            for r in &base.painted_regions {
                self.defs.push('M');
                push_pair(&mut self.defs, r.x0, r.y0);
                self.defs.push('H');
                push_num(&mut self.defs, r.x1);
                self.defs.push('V');
                push_num(&mut self.defs, r.y1);
                self.defs.push('H');
                push_num(&mut self.defs, r.x0);
                self.defs.push('Z');
            }
            self.defs
                .push_str("\" clip-rule=\"evenodd\"/></clipPath>\n");
        }
    }

    /// Reuse the active pre-run backdrop snapshot when `check_rect` is disjoint
    /// from every fill already overlaid on it (avoiding wrapping or re-rendering
    /// earlier dense fills), or snapshot `self.out` once and start a fresh run.
    fn acquire_disjoint_or_new_backdrop(
        &mut self,
        check_rect: SvgRect,
        opaque: bool,
        scoped: bool,
        new_fanout_refs: usize,
    ) -> Option<u32> {
        if self.pending_backdrop.is_none() {
            if let Some(ref mut base) = self.disjoint_backdrop {
                if base.out_len == self.out.len()
                    && base.opaque == opaque
                    && (opaque || (scoped && base.comp_clip_id.is_some()))
                    && base
                        .painted_regions
                        .iter()
                        .all(|r| r.intersect(check_rect).is_none())
                {
                    base.painted_regions.push(check_rect);
                    return Some(base.bg_id);
                }
            }
        }
        if self.backdrop_fanout.saturating_mul(new_fanout_refs) > MAX_BACKDROP_FANOUT {
            return None;
        }
        self.flush_pending_backdrop();
        self.flush_disjoint_backdrop();
        if self.backdrop_snapshots >= MAX_BACKDROP_SNAPSHOTS {
            return None;
        }
        let orig_bg_id = self.snapshot_page_backdrop(true)?;
        self.backdrop_fanout = self.backdrop_fanout.saturating_mul(new_fanout_refs);
        if opaque {
            let _ = writeln!(self.out, "<use href=\"#b{orig_bg_id}\"/>");
            self.disjoint_backdrop = Some(DisjointBackdropBase {
                bg_id: orig_bg_id,
                opaque: true,
                comp_clip_id: None,
                out_len: self.out.len(),
                painted_regions: vec![check_rect],
            });
        } else if scoped {
            let cid = self.fresh_id();
            let _ = writeln!(
                self.out,
                "<use href=\"#b{orig_bg_id}\" clip-path=\"url(#c{cid})\"/>"
            );
            self.disjoint_backdrop = Some(DisjointBackdropBase {
                bg_id: orig_bg_id,
                opaque: false,
                comp_clip_id: Some(cid),
                out_len: self.out.len(),
                painted_regions: vec![check_rect],
            });
        }
        Some(orig_bg_id)
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
        disjoint_rect: SvgRect,
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
        let reg = if scoped {
            affected
        } else {
            self.full_page_rect()
        };
        if !opaque && Self::max_filter_primitives_for_region(reg) < 5 {
            return false;
        }
        let check_rect = if opaque { disjoint_rect } else { reg };
        let Some(orig_bg_id) =
            self.acquire_disjoint_or_new_backdrop(check_rect, opaque, scoped, refs)
        else {
            return false;
        };

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
            if let Some(ref mut base) = self.disjoint_backdrop {
                base.out_len = self.out.len();
            }
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

        let _ = writeln!(
            self.out,
            "<use href=\"#b{eval_id}\" filter=\"url(#f{fid})\"/>"
        );
        if let Some(ref mut base) = self.disjoint_backdrop {
            base.out_len = self.out.len();
        }
        true
    }

    /// Evaluate a dense opacity-grouped tiling op stream exceeding one filter's
    /// intermediate-pixel budget as a space-separated filter chain
    /// (`filter="url(#f0) url(#f1) ..."`) scoped to `affected` (splitting only
    /// at top-level `OpacityGroup` depth 0). Each filter applies in-place so
    /// peak raster memory stays bounded to one chunk, and pairwise-disjoint
    /// fills share the pre-run backdrop snapshot so earlier fills are never
    /// re-rendered.
    fn emit_chunked_tiling_ops(
        &mut self,
        ops: &[TilingPlanOp],
        shape_body: &str,
        affected: SvgRect,
        disjoint_rect: SvgRect,
    ) -> bool {
        if self.non_isolated_filter_depth > 0 || ops.is_empty() {
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
        let reg = if scoped {
            affected
        } else {
            self.full_page_rect()
        };
        // On opaque backdrops, shape masking happens via `<g mask="url(#m...)">`
        // outside the filter chain (0 extra filter primitives and 0 `<feImage>`
        // re-renders of `#b{orig_bg_id}`). On translucent backdrops, reserve 5
        // primitives on the final chunk for the `orig`/`wt`/`in`/`out`/`arithmetic` mix.
        let reserve = if opaque { 0 } else { 5 };
        let max_chunk_prims = Self::max_filter_primitives_for_region(reg).saturating_sub(reserve);
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
        if chunks.is_empty() || chunks.len() > MAX_BACKDROP_SNAPSHOTS {
            return false;
        }

        let check_rect = if opaque { disjoint_rect } else { reg };
        let Some(orig_bg_id) =
            self.acquire_disjoint_or_new_backdrop(check_rect, opaque, scoped, refs)
        else {
            return false;
        };

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

        let shape_id = if opaque {
            None
        } else {
            let sid = self.fresh_id();
            let _ = writeln!(self.defs, "<g id=\"s{sid}\">{shape_body}</g>");
            Some(sid)
        };

        let mut filter_urls = String::new();
        for (chunk_idx, chunk) in chunks.iter().enumerate() {
            let is_last = chunk_idx + 1 == chunks.len();
            let fid = self.fresh_id();
            if !filter_urls.is_empty() {
                filter_urls.push(' ');
            }
            let _ = write!(filter_urls, "url(#f{fid})");
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
                if let Some(sid) = shape_id {
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
                        "<feImage href=\"#s{sid}\" x=\"0\" y=\"0\" width=\""
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
            }
            self.defs.push_str("</filter>\n");
        }

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
            let _ = writeln!(
                self.out,
                "<g mask=\"url(#m{mask_id})\"><use href=\"#b{orig_bg_id}\" filter=\"{filter_urls}\"/></g>"
            );
        } else {
            let _ = writeln!(
                self.out,
                "<use href=\"#b{orig_bg_id}\" filter=\"{filter_urls}\"/>"
            );
        }
        if let Some(ref mut base) = self.disjoint_backdrop {
            base.out_len = self.out.len();
        }
        true
    }

    /// Emit `ops` inside an already-open isolated `<g>` wrapper (used only when
    /// nested inside a `<pattern>`/`<mask>` or after snapshot exhaustion).
    fn emit_isolated_tiling_ops(&mut self, ops: &[TilingPlanOp]) {
        let mut stack: Vec<String> = Vec::new();
        for op in ops {
            match op {
                TilingPlanOp::Pattern { pat_id, blend_mode } => {
                    let target = stack.last_mut().unwrap_or(&mut self.out);
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
                TilingPlanOp::PopOpacityGroup { alpha, mask_pat_id } => {
                    if let Some(inner) = stack.pop() {
                        let mask_id = mask_pat_id.map(|m_pat_id| {
                            let mid = self.fresh_id();
                            let _ = write!(
                                self.defs,
                                "<mask id=\"m{mid}\" maskUnits=\"userSpaceOnUse\" style=\"mask-type:alpha\"><rect x=\"0\" y=\"0\" width=\""
                            );
                            push_num(&mut self.defs, self.page_width);
                            self.defs.push_str("\" height=\"");
                            push_num(&mut self.defs, self.page_height);
                            let _ = writeln!(self.defs, "\" fill=\"url(#p{m_pat_id})\"/></mask>");
                            mid
                        });
                        let parent = stack.last_mut().unwrap_or(&mut self.out);
                        parent.push_str("<g");
                        push_opacity(parent, "opacity", *alpha);
                        if let Some(mid) = mask_id {
                            let _ = write!(parent, " mask=\"url(#m{mid})\"");
                        }
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

    /// Backdrop-aware bounded raster overlay fallback used when overlapping
    /// dense patterns exhaust `MAX_BACKDROP_FANOUT` or the SVG `<filter>`
    /// intermediate-pixel budget:
    /// - Advances a single running `CpuRenderer` monotonically across page
    ///   commands in O(N) total work.
    /// - Preserves the live backdrop and applies object opacity, soft masks,
    ///   `OpacityGroup`s, and antialiased edges ONCE to the completed blend.
    /// - Emits a single PNG `<image>` covering `affected` with exact incremental
    ///   alpha (never duplicating path geometry per pass, zero `<use>` fan-out,
    ///   and O(1) raster memory).
    fn emit_raster_backdrop_tiling_fallback(&mut self, affected: SvgRect) -> bool {
        let Some(cmd_idx) = self.current_page_cmd_idx else {
            return false;
        };
        self.emit_raster_backdrop_range_fallback(cmd_idx, cmd_idx, affected)
    }

    fn emit_raster_backdrop_range_fallback(
        &mut self,
        start_idx: usize,
        end_idx: usize,
        affected: SvgRect,
    ) -> bool {
        const MAX_RASTER_DIM: u32 = 1024;
        const MAX_RASTER_PIXELS: u64 = 1_048_576;

        if self.non_isolated_filter_depth > 0
            || self.svg_backdrop_diverged_depth > 0
            || start_idx > end_idx
            || end_idx >= self.page_commands.len()
        {
            return false;
        }

        if self.backdrop_cpu.is_none() {
            let page = self.page_rect.normalize();
            let Some(scale) = svg_clamp_raster_scale(page, MAX_RASTER_DIM, MAX_RASTER_PIXELS)
            else {
                return false;
            };
            let mut renderer = CpuRenderer::new()
                .with_fonts(self.fonts)
                .with_images(self.images);
            if let Some(deadline) = self.deadline {
                let now = zpdf_core::time::Instant::now();
                let remaining = if deadline > now {
                    deadline.duration_since(now)
                } else {
                    std::time::Duration::ZERO
                };
                renderer = renderer.with_render_budget(Some(remaining));
            } else {
                renderer = renderer.with_render_budget(None);
            }
            let bg = if self.backdrop_cpu_transparent {
                Color::rgba(0.0, 0.0, 0.0, 0.0)
            } else {
                self.page_background
            };
            let info = PageRenderInfo {
                page_rect: page,
                scale,
                background: bg,
            };
            if renderer.begin_page(&info).is_err() {
                return false;
            }
            let Some((width, height)) = renderer.pixmap_dimensions() else {
                return false;
            };
            if width > MAX_RASTER_DIM
                || height > MAX_RASTER_DIM
                || (width as u64) * (height as u64) > MAX_RASTER_PIXELS
            {
                return false;
            }
            let actual_scale = renderer.actual_scale() as f64;
            if !actual_scale.is_finite() || actual_scale <= 0.0 {
                return false;
            }
            if self.backdrop_cpu_start_idx > 0 {
                for &clip_idx in &self.active_clip_cmd_indices {
                    if clip_idx < self.backdrop_cpu_start_idx {
                        let _ = renderer.execute(&self.page_commands[clip_idx]);
                    }
                }
            }
            self.backdrop_cpu = Some(Box::new(SvgBackdropCpu {
                renderer,
                cursor: self.backdrop_cpu_start_idx,
                scale: actual_scale,
                width,
                height,
                exhausted: false,
            }));
        }

        let (before, after, ix0, iy0, rw, rh, bs, state_width, state_height, covers_whole_page) = {
            let Some(state) = self.backdrop_cpu.as_mut() else {
                return false;
            };
            if state.exhausted || state.renderer.is_over_budget() {
                state.exhausted = true;
                self.over_budget = true;
                return true;
            }
            if state.cursor > start_idx {
                return false;
            }
            while state.cursor < start_idx {
                let _ = state.renderer.execute(&self.page_commands[state.cursor]);
                state.cursor += 1;
                if state.renderer.is_over_budget() {
                    state.exhausted = true;
                    self.over_budget = true;
                    return true;
                }
            }
            if state.renderer.is_recording_non_isolated() {
                return false;
            }
            let bs = state.scale;
            let state_w = state.width;
            let state_h = state.height;
            let ix0 = (affected.x0 * bs).floor().clamp(0.0, state_w as f64) as u32;
            let ix1 = (affected.x1 * bs).ceil().clamp(0.0, state_w as f64) as u32;
            let iy0 = (affected.y0 * bs).floor().clamp(0.0, state_h as f64) as u32;
            let iy1 = (affected.y1 * bs).ceil().clamp(0.0, state_h as f64) as u32;
            if ix1 <= ix0 || iy1 <= iy0 {
                for c in &self.page_commands[start_idx..=end_idx] {
                    let _ = state.renderer.execute(c);
                }
                state.cursor = end_idx + 1;
                return true;
            }
            let rw = ix1 - ix0;
            let rh = iy1 - iy0;
            let covers_whole_page = ix0 == 0 && iy0 == 0 && rw == state_w && rh == state_h;
            let Some(before) = state.renderer.copy_subrect(ix0, iy0, rw, rh) else {
                return false;
            };
            let mut open_blends = 0usize;
            for c in &self.page_commands[start_idx..=end_idx] {
                match c {
                    RenderCommand::PushBlendGroup { .. } => open_blends += 1,
                    RenderCommand::PopBlendGroup => open_blends = open_blends.saturating_sub(1),
                    _ => {}
                }
                let _ = state.renderer.execute(c);
                if state.renderer.is_over_budget() {
                    state.exhausted = true;
                    self.over_budget = true;
                    return true;
                }
            }
            for _ in 0..open_blends {
                let _ = state.renderer.execute(&RenderCommand::PopBlendGroup);
            }
            state.cursor = end_idx + 1;
            if state.renderer.is_over_budget() {
                state.exhausted = true;
                self.over_budget = true;
                return true;
            }
            if state.renderer.is_recording_non_isolated() {
                return false;
            }
            let Some(after) = state.renderer.copy_subrect(ix0, iy0, rw, rh) else {
                return false;
            };
            (
                before,
                after,
                ix0,
                iy0,
                rw,
                rh,
                bs,
                state_w,
                state_h,
                covers_whole_page,
            )
        };

        // Tighten to the actual changed pixel bounding box so conservative
        // full-page bounds (e.g. unclipped Type 3 glyphs) never replace
        // untouched earlier vector text/linework with a full-page raster.
        let (mut rel_x0, mut rel_y0) = (rw, rh);
        let (mut rel_x1, mut rel_y1) = (0u32, 0u32);
        let mut changed_count = 0usize;
        let rw_usize = rw as usize;
        for y in 0..rh as usize {
            let row_start = y * rw_usize * 4;
            let row_end = row_start + rw_usize * 4;
            let b_row = before[row_start..row_end].as_chunks::<4>().0;
            let f_row = after[row_start..row_end].as_chunks::<4>().0;
            for (x, (bp, fp)) in b_row.iter().zip(f_row.iter()).enumerate() {
                if fp != bp {
                    changed_count += 1;
                    let xu = x as u32;
                    let yu = y as u32;
                    rel_x0 = rel_x0.min(xu);
                    rel_y0 = rel_y0.min(yu);
                    rel_x1 = rel_x1.max(xu + 1);
                    rel_y1 = rel_y1.max(yu + 1);
                }
            }
        }
        if changed_count == 0 {
            return true;
        }
        let cx0 = ix0 + rel_x0;
        let cy0 = iy0 + rel_y0;
        let cw = rel_x1 - rel_x0;
        let ch = rel_y1 - rel_y0;
        let (before_crop, after_crop) = if cw == rw && ch == rh {
            (before, after)
        } else {
            let cw_bytes = (cw as usize) * 4;
            let mut b_sub = vec![0u8; (ch as usize) * cw_bytes];
            let mut f_sub = vec![0u8; (ch as usize) * cw_bytes];
            for row in 0..ch as usize {
                let src_off = ((rel_y0 as usize + row) * rw_usize + rel_x0 as usize) * 4;
                let dst_off = row * cw_bytes;
                b_sub[dst_off..dst_off + cw_bytes]
                    .copy_from_slice(&before[src_off..src_off + cw_bytes]);
                f_sub[dst_off..dst_off + cw_bytes]
                    .copy_from_slice(&after[src_off..src_off + cw_bytes]);
            }
            (b_sub, f_sub)
        };

        // Only replace the entire vector page when this mark demonstrably
        // painted/changed EVERY pixel of the page with opaque output (so an
        // incremental overlay would already be 100% opaque everywhere).
        let all_page_pixels_changed = covers_whole_page
            && cw == state_width
            && ch == state_height
            && changed_count == (state_width as usize) * (state_height as usize);
        let after_all_opaque = after_crop.as_chunks::<4>().0.iter().all(|px| px[3] == 255);
        let replace_whole_page = all_page_pixels_changed
            && self.blend_depth == 0
            && self.has_opaque_background
            && after_all_opaque;

        let (img, replace_hole) = if replace_whole_page {
            (
                DecodedImage {
                    width: cw,
                    height: ch,
                    data: after_crop,
                    has_alpha: false,
                    is_image_mask: false,
                    premultiplied: false,
                },
                false,
            )
        } else if let Some(overlay) = svg_compute_incremental_overlay(&before_crop, &after_crop) {
            (
                DecodedImage {
                    width: cw,
                    height: ch,
                    data: overlay,
                    has_alpha: true,
                    is_image_mask: false,
                    premultiplied: false,
                },
                false,
            )
        } else {
            // At least one changed pixel cannot be expressed as SourceOver on
            // the translucent backdrop (e.g. unchanged/reduced alpha): clip out
            // ONLY the tight changed region from `self.out` and emit the actual
            // unpremultiplied `after` RGBA preserving its true alpha.
            (
                DecodedImage {
                    width: cw,
                    height: ch,
                    data: after_crop,
                    has_alpha: true,
                    is_image_mask: false,
                    premultiplied: true,
                },
                true,
            )
        };
        let Some(png) = encode_png(&img) else {
            return false;
        };

        let x = cx0 as f64 / bs;
        let y = cy0 as f64 / bs;
        let w = if replace_whole_page {
            self.page_width
        } else {
            cw as f64 / bs
        };
        let h = if replace_whole_page {
            self.page_height
        } else {
            ch as f64 / bs
        };

        if replace_whole_page {
            self.pending_backdrop = None;
            self.disjoint_backdrop = None;
            self.out.clear();
            self.backdrop_fanout = 1;
        } else {
            self.flush_pending_backdrop();
            self.flush_disjoint_backdrop();
            if replace_hole && !self.out.is_empty() {
                if cx0 == 0 && cy0 == 0 && cw == state_width && ch == state_height {
                    self.out.clear();
                } else {
                    let hx1 = if cx0 + cw == state_width {
                        self.page_width
                    } else {
                        (x + w).min(self.page_width)
                    };
                    let hy1 = if cy0 + ch == state_height {
                        self.page_height
                    } else {
                        (y + h).min(self.page_height)
                    };
                    let hole_clip = self.fresh_id();
                    let _ = write!(self.defs, "<clipPath id=\"c{hole_clip}\"><path d=\"M0 0H");
                    push_num(&mut self.defs, self.page_width);
                    self.defs.push('V');
                    push_num(&mut self.defs, self.page_height);
                    self.defs.push_str("H0ZM");
                    push_pair(&mut self.defs, x, y);
                    self.defs.push('H');
                    push_num(&mut self.defs, hx1);
                    self.defs.push('V');
                    push_num(&mut self.defs, hy1);
                    self.defs.push('H');
                    push_num(&mut self.defs, x);
                    self.defs
                        .push_str("Z\" clip-rule=\"evenodd\"/></clipPath>\n");
                    let prior = std::mem::take(&mut self.out);
                    let _ = writeln!(self.out, "<g clip-path=\"url(#c{hole_clip})\">");
                    self.out.push_str(&prior);
                    self.out.push_str("</g>\n");
                }
            }
        }
        self.out.push_str("<image x=\"");
        push_num(&mut self.out, x);
        self.out.push_str("\" y=\"");
        push_num(&mut self.out, y);
        self.out.push_str("\" width=\"");
        push_num(&mut self.out, w);
        self.out.push_str("\" height=\"");
        push_num(&mut self.out, h);
        self.out
            .push_str("\" preserveAspectRatio=\"none\" href=\"data:image/png;base64,");
        push_base64(&mut self.out, &png);
        self.out.push_str("\"/>\n");
        true
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
        self.flush_disjoint_backdrop();
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
        self.flush_disjoint_backdrop();
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
    /// backdrop steps see it without starting a new snapshot. When `charge` is
    /// `false`, the caller already reserved primitive capacity for this overlay.
    fn absorb_out_into_pending(&mut self, charge: bool) {
        if self.pending_backdrop.is_some() && !self.out.is_empty() {
            let mid = std::mem::take(&mut self.out);
            let ov_id = self.fresh_id();
            let _ = write!(self.defs, "<g id=\"ov{ov_id}\">\n{mid}</g>\n");
            let full = self.full_page_rect();
            let ov_prims = if self.non_isolated_ga_depth > 0 { 3 } else { 2 };
            if let Some(ref mut p) = self.pending_backdrop {
                p.steps.push(BackdropStep::Overlay(ov_id));
                if charge {
                    p.total_primitives = p.total_primitives.saturating_add(ov_prims);
                }
                p.region = full;
            }
        }
    }

    /// Read-only check mirroring [`Self::enqueue_backdrop_step`] to test whether
    /// an additional filter step fits within the active batch or a new snapshot
    /// without mutating `self.out` or `self.pending_backdrop`.
    fn can_enqueue_backdrop_step(
        &self,
        step_primitives: usize,
        region: Option<SvgRect>,
        allow_empty: bool,
        has_out: bool,
    ) -> bool {
        let full = self.full_page_rect();
        let step_region = region.unwrap_or(full);
        let mut fanout = self.backdrop_fanout;
        if let Some(ref p) = self.pending_backdrop {
            let extra_prims = if has_out {
                if self.non_isolated_ga_depth > 0 {
                    3
                } else {
                    2
                }
            } else {
                0
            };
            let candidate_region = if has_out || self.non_isolated_filter_depth > 0 {
                full
            } else {
                p.region.union(step_region)
            };
            let effective_region = if candidate_region
                .is_strict_subregion(self.page_width, self.page_height)
                && fanout.saturating_mul(2) <= MAX_BACKDROP_FANOUT
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
                return true;
            }
            if self.non_isolated_filter_depth > 0 {
                return false;
            }
            if !p.steps.is_empty()
                && p.region
                    .is_strict_subregion(self.page_width, self.page_height)
                && fanout.saturating_mul(2) <= MAX_BACKDROP_FANOUT
            {
                fanout = fanout.saturating_mul(2);
            }
        }
        let has_out_after_flush =
            has_out || self.pending_backdrop.is_some() || self.disjoint_backdrop.is_some();
        let init_region = if self.non_isolated_filter_depth > 0 {
            full
        } else {
            step_region
        };
        let effective_region = if init_region.is_strict_subregion(self.page_width, self.page_height)
            && fanout.saturating_mul(2) <= MAX_BACKDROP_FANOUT
        {
            init_region
        } else {
            full
        };
        let prim_limit = Self::max_filter_primitives_for_region(effective_region);
        (allow_empty || has_out_after_flush)
            && self.backdrop_snapshots < MAX_BACKDROP_SNAPSHOTS
            && step_primitives <= prim_limit
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
            let extra_prims = if has_out {
                if self.non_isolated_ga_depth > 0 {
                    3
                } else {
                    2
                }
            } else {
                0
            };
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
                self.absorb_out_into_pending(true);
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
        disjoint_rect: SvgRect,
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
            && self.emit_flat_blend_group_tiling_ops(ops, shape_body, affected, disjoint_rect)
        {
            return true;
        }

        if step_prims <= prim_limit || self.non_isolated_filter_depth > 0 {
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
        }
        self.non_isolated_filter_depth == 0
            && self.emit_chunked_tiling_ops(ops, shape_body, affected, disjoint_rect)
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_tiling_fill_path_data(
        &mut self,
        d: &str,
        rule: &FillRule,
        tiling: &zpdf_display_list::TilingPattern,
        opacity: f32,
        affected: SvgRect,
        disjoint_rect: SvgRect,
        depth: EmitDepth,
    ) {
        let plan = self.emit_tiling_plan(tiling, depth);
        match plan {
            TilingPassPlan::Affine { t_id, k_id, mk_id } => {
                if depth.is_top_level() {
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
                    let affine_prims = if self.non_isolated_ga_depth > 0 {
                        18
                    } else {
                        14
                    };
                    if self.enqueue_backdrop_step(
                        BackdropStep::Affine {
                            t_id,
                            k_id,
                            mk_id,
                            shape_id,
                        },
                        affine_prims,
                        Some(affected),
                        true,
                    ) {
                        return;
                    }
                    if (self.pending_backdrop.is_some() || !self.out.is_empty())
                        && self.emit_raster_backdrop_tiling_fallback(affected)
                    {
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
                    if depth.is_top_level() {
                        let placement = self.placement_attrs();
                        let mut shape_body = String::with_capacity(d.len() + placement.len() + 64);
                        let _ = write!(shape_body, "<path d=\"{d}\" fill=\"#ffffff\"");
                        if *rule == FillRule::EvenOdd {
                            shape_body.push_str(" fill-rule=\"evenodd\"");
                        }
                        push_opacity(&mut shape_body, "fill-opacity", opacity);
                        shape_body.push_str(&placement);
                        shape_body.push_str("/>");
                        if self.emit_tiling_multi_pass_backdrop(
                            &ops,
                            &shape_body,
                            affected,
                            disjoint_rect,
                        ) {
                            return;
                        }
                        if (self.pending_backdrop.is_some() || !self.out.is_empty())
                            && self.emit_raster_backdrop_tiling_fallback(affected)
                        {
                            return;
                        }
                    }

                    // Bounded isolated single-opacity fallback (when there is no
                    // backdrop yet, nested inside a pattern/mask, or past the
                    // filter/snapshot budget): evaluate all passes at 1.0 inside
                    // one isolated group and apply `opacity` ONCE.
                    if depth.is_top_level() {
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
                if depth.is_top_level()
                    && self.pending_backdrop.is_some()
                    && blend_mode != BlendMode::Normal
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

    fn emit_fill(
        &mut self,
        path: &Path,
        rule: &FillRule,
        paint: &Paint,
        alpha: f32,
        depth: EmitDepth,
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
                let disjoint_rect = self.path_svg_bounds(path, 0.0).unwrap_or(affected);
                self.emit_tiling_fill_path_data(
                    &d,
                    rule,
                    tiling,
                    opacity,
                    affected,
                    disjoint_rect,
                    depth,
                );
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
        depth: EmitDepth,
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
                        if depth.is_top_level() {
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
                            let affine_prims = if self.non_isolated_ga_depth > 0 {
                                18
                            } else {
                                14
                            };
                            if self.enqueue_backdrop_step(
                                BackdropStep::Affine {
                                    t_id,
                                    k_id,
                                    mk_id,
                                    shape_id,
                                },
                                affine_prims,
                                Some(affected),
                                true,
                            ) {
                                return;
                            }
                            if (self.pending_backdrop.is_some() || !self.out.is_empty())
                                && self.emit_raster_backdrop_tiling_fallback(affected)
                            {
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
                            if depth.is_top_level() {
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
                                if self.emit_tiling_multi_pass_backdrop(
                                    &ops,
                                    &shape_body,
                                    affected,
                                    affected,
                                ) {
                                    return;
                                }
                                if (self.pending_backdrop.is_some() || !self.out.is_empty())
                                    && self.emit_raster_backdrop_tiling_fallback(affected)
                                {
                                    return;
                                }
                            }

                            // Bounded isolated single-opacity fallback for strokes.
                            if depth.is_top_level() {
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
                        if depth.is_top_level()
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
        trailing_reserve: usize,
    ) {
        if !self.out.is_empty() {
            self.absorb_out_into_pending(false);
        } else if let Some(ref mut p) = self.pending_backdrop {
            p.total_primitives = p.total_primitives.saturating_sub(trailing_reserve);
        }
        self.blend_depth = self.blend_depth.saturating_sub(1);
        self.non_isolated_filter_depth = self.non_isolated_filter_depth.saturating_sub(1);
        self.non_isolated_ancestor_depth = self.non_isolated_ancestor_depth.saturating_sub(1);
        if blend_mode != BlendMode::Normal {
            self.isolated_depth = self.isolated_depth.saturating_sub(1);
            self.non_isolated_ga_depth = self.non_isolated_ga_depth.saturating_sub(1);
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
        prev_non_isolated_ga_depth: usize,
        prev_backdrop_cpu: Option<Box<SvgBackdropCpu<'a>>>,
        prev_backdrop_cpu_start_idx: usize,
        prev_backdrop_cpu_transparent: bool,
        swapped_backdrop_cpu: bool,
        depth: EmitDepth,
    ) {
        if !open {
            return;
        }
        self.flush_pending_backdrop();
        self.flush_disjoint_backdrop();
        let inner_body = std::mem::replace(&mut self.out, prev_out);
        self.pending_backdrop = prev_pending;
        self.non_isolated_filter_depth = prev_non_isolated_depth;
        self.non_isolated_ga_depth = prev_non_isolated_ga_depth;
        if swapped_backdrop_cpu {
            self.backdrop_cpu = prev_backdrop_cpu;
            self.backdrop_cpu_start_idx = prev_backdrop_cpu_start_idx;
            self.backdrop_cpu_transparent = prev_backdrop_cpu_transparent;
        }
        self.blend_depth = self.blend_depth.saturating_sub(1);
        if isolates {
            self.isolated_depth = self.isolated_depth.saturating_sub(1);
        }
        if !isolated {
            self.svg_backdrop_diverged_depth = self.svg_backdrop_diverged_depth.saturating_sub(1);
        }
        if depth.is_top_level()
            && blend_css(blend_mode).is_some()
            && self.pending_backdrop.is_some()
        {
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
            let ov_prims = if self.non_isolated_ga_depth > 0 { 3 } else { 2 };
            if self.enqueue_backdrop_step(
                BackdropStep::BlendOverlay { ov_id, blend_mode },
                ov_prims,
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
    fn emit_soft_mask_def(&mut self, mask: &SoftMask, depth: EmitDepth) -> Option<u32> {
        self.emit_soft_mask_def_inner(mask, None, depth, || None)
    }

    /// Emit a Type 3 glyph-local soft mask placed at `placement`, keyed by the
    /// retained source mask and placement transform so separate glyph positions
    /// get distinct `<mask>` defs while identical placements deduplicate.
    fn emit_transformed_soft_mask_def<F>(
        &mut self,
        source: &SoftMask,
        placement: &zpdf_core::Matrix,
        resolve_image: &F,
        depth: EmitDepth,
    ) -> Option<u32>
    where
        F: Fn(ImageId) -> Option<std::sync::Arc<zpdf_display_list::InlineImage>>,
    {
        let stroke_scale = (self.scale as f32).max(4.0);
        let transform_stroke = |path: &Path, style: &StrokeStyle, m: &zpdf_core::Matrix| {
            zpdf_render_cpu::transform_stroke_to_page(path, style, m, stroke_scale)
        };
        self.emit_soft_mask_def_inner(source, Some(matrix_key_bits(placement)), depth, || {
            Some(source.transform_with_images(placement, resolve_image, &transform_stroke))
        })
    }

    fn emit_soft_mask_def_inner(
        &mut self,
        source: &SoftMask,
        placement: Option<[u64; 6]>,
        depth: EmitDepth,
        materialize: impl FnOnce() -> Option<SoftMask>,
    ) -> Option<u32> {
        if depth.mask >= MAX_MASK_DEPTH {
            tracing::warn!("soft-mask nesting exceeds depth budget; mask dropped");
            return None;
        }
        if source.transfer.is_some() {
            self.warn_transfer();
        }
        let key = MaskDefKey {
            commands: std::sync::Arc::as_ptr(&source.commands),
            placement,
            kind: source.kind,
            offset: [
                canonical_f32_bits(source.offset.0),
                canonical_f32_bits(source.offset.1),
            ],
            backdrop_luma: canonical_f32_bits(source.backdrop_luma),
            origin_scale: [
                canonical_f64_bits(self.x0),
                canonical_f64_bits(self.y1),
                canonical_f64_bits(self.scale),
            ],
            depth,
        };
        if let Some(&(_, id)) = self.mask_defs.get(&key) {
            return Some(id);
        }
        let id = self.fresh_id();
        self.mask_defs
            .insert(key, (std::sync::Arc::clone(&source.commands), id));

        let transformed = materialize();
        let mask = transformed.as_ref().unwrap_or(source);

        let saved_out = std::mem::take(&mut self.out);
        let saved_pending = self.pending_backdrop.take();
        let saved_disjoint = self.disjoint_backdrop.take();
        let saved_non_isolated = std::mem::replace(&mut self.non_isolated_filter_depth, 0);
        let saved_non_isolated_ancestor =
            std::mem::replace(&mut self.non_isolated_ancestor_depth, 0);
        let saved_non_isolated_ga = std::mem::replace(&mut self.non_isolated_ga_depth, 0);
        let saved_clip_indices = std::mem::take(&mut self.active_clip_cmd_indices);
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
        self.emit_commands(&mask.commands.commands, depth.enter_mask());
        self.flush_pending_backdrop();
        self.flush_disjoint_backdrop();
        if translated {
            self.out.push_str("</g>\n");
        }
        let body = std::mem::replace(&mut self.out, saved_out);
        self.pending_backdrop = saved_pending;
        self.disjoint_backdrop = saved_disjoint;
        self.non_isolated_filter_depth = saved_non_isolated;
        self.non_isolated_ancestor_depth = saved_non_isolated_ancestor;
        self.non_isolated_ga_depth = saved_non_isolated_ga;
        self.active_clip_cmd_indices = saved_clip_indices;
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

    fn emit_glyph_run(&mut self, run: &GlyphRun, depth: EmitDepth) {
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
                    self.emit_type3_run(run, font, color, opacity, depth);
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

    fn type3_glyph_to_page(
        run: &GlyphRun,
        glyph: &zpdf_display_list::PositionedGlyph,
        font_matrix: &[f64; 6],
    ) -> zpdf_core::Matrix {
        run.transform
            .concat(&zpdf_core::Matrix::translate(
                glyph.x as f64,
                glyph.y as f64,
            ))
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
            ))
    }

    fn type3_path_to_page(path: &Path, glyph_to_page: &zpdf_core::Matrix) -> Path {
        let mut out = Path::new();
        for elem in path {
            match elem {
                PathElement::MoveTo(p) => out.move_to(p.transform(glyph_to_page)),
                PathElement::LineTo(p) => out.line_to(p.transform(glyph_to_page)),
                PathElement::CurveTo(c1, c2, end) => out.curve_to(
                    c1.transform(glyph_to_page),
                    c2.transform(glyph_to_page),
                    end.transform(glyph_to_page),
                ),
                PathElement::Close => out.close(),
            }
        }
        out
    }

    fn max_linear_scale_2x2(a: f64, b: f64, c: f64, d: f64) -> f64 {
        let sum_sq = a * a + b * b + c * c + d * d;
        let det = a * d - b * c;
        let disc = (sum_sq * sum_sq - 4.0 * det * det).max(0.0).sqrt();
        (0.5 * (sum_sq + disc)).sqrt()
    }

    fn type3_glyph_to_svg_matrix(&self, glyph_to_page: &zpdf_core::Matrix) -> [f64; 6] {
        let s = self.scale;
        [
            glyph_to_page.a * s,
            -glyph_to_page.b * s,
            glyph_to_page.c * s,
            -glyph_to_page.d * s,
            (glyph_to_page.e - self.x0) * s,
            (self.y1 - glyph_to_page.f) * s,
        ]
    }

    fn type3_scaled_path_data(
        &mut self,
        path: &Path,
        glyph_to_page: &zpdf_core::Matrix,
        norm: f64,
    ) -> Option<String> {
        let (x0, y1, scale) = (self.x0, self.y1, self.scale);
        let map = |gx: f64, gy: f64| -> Option<(f64, f64)> {
            let p = zpdf_core::Point::new(gx, gy).transform(glyph_to_page);
            let (sx, sy) = ((p.x - x0) * scale, (y1 - p.y) * scale);
            let (nx, ny) = (gx * norm, gy * norm);
            (coord_ok(sx) && coord_ok(sy) && coord_ok(nx) && coord_ok(ny)).then_some((nx, ny))
        };
        self.path_data_with(path, map)
    }

    /// Format a Type 3 stroked path preserving nonuniform glyph-to-SVG scaling
    /// (via `transform="matrix(...)"` on a norm-scaled glyph-space path so
    /// coordinates, stroke width, and dash lengths stay at SVG pixel scale) and
    /// the full stroke style (`stroke-width`, cap, join, miter limit, dash
    /// array/offset). When `placement` carries an enclosing `clip-path`/`mask`,
    /// wraps the transformed path in `<g{placement}>` so the clip stays in
    /// page/SVG space.
    #[allow(clippy::too_many_arguments)]
    fn format_type3_stroke_element(
        &mut self,
        path: &Path,
        style: &StrokeStyle,
        glyph_to_page: &zpdf_core::Matrix,
        color_hex: &str,
        stroke_opacity: f32,
        placement: &str,
    ) -> Option<String> {
        let svg_m = self.type3_glyph_to_svg_matrix(glyph_to_page);
        let max_scale = Self::max_linear_scale_2x2(svg_m[0], svg_m[1], svg_m[2], svg_m[3]);
        if !max_scale.is_finite() || max_scale <= 1e-9 {
            return None;
        }
        if style.width.is_finite() && style.width > 0.0 && svg_m.iter().all(|v| coord_ok(*v)) {
            let norm = max_scale;
            let norm_d = self.type3_scaled_path_data(path, glyph_to_page, norm)?;
            let eff_style = StrokeStyle {
                width: style.width.max((0.05 / norm) as f32),
                cap: style.cap,
                join: style.join,
                miter_limit: style.miter_limit,
                dash: style.dash.clone(),
            };
            let linear = [
                svg_m[0] / norm,
                svg_m[1] / norm,
                svg_m[2] / norm,
                svg_m[3] / norm,
            ];
            let mut elem = String::with_capacity(norm_d.len() + 144);
            let wrapped = !placement.is_empty();
            if wrapped {
                let _ = write!(elem, "<g{placement}>");
            }
            elem.push_str("<path d=\"");
            elem.push_str(&norm_d);
            elem.push_str("\" transform=\"matrix(");
            for (i, &v) in linear.iter().enumerate() {
                if i > 0 {
                    elem.push(' ');
                }
                push_matrix_num(&mut elem, v);
            }
            elem.push(' ');
            push_num(&mut elem, svg_m[4]);
            elem.push(' ');
            push_num(&mut elem, svg_m[5]);
            elem.push_str(")\" fill=\"none\" stroke=\"");
            elem.push_str(color_hex);
            elem.push('"');
            push_stroke_attrs(&mut elem, &eff_style, norm);
            push_opacity(&mut elem, "stroke-opacity", stroke_opacity);
            elem.push_str("/>");
            if wrapped {
                elem.push_str("</g>");
            }
            elem.push('\n');
            return Some(elem);
        }
        let d = self.type3_path_data(path, glyph_to_page)?;
        let det_scale = (svg_m[0] * svg_m[3] - svg_m[1] * svg_m[2]).abs().sqrt();
        let mut elem = String::with_capacity(d.len() + 96);
        elem.push_str("<path d=\"");
        elem.push_str(&d);
        elem.push_str("\" fill=\"none\" stroke=\"");
        elem.push_str(color_hex);
        elem.push('"');
        push_stroke_attrs(&mut elem, style, det_scale);
        push_opacity(&mut elem, "stroke-opacity", stroke_opacity);
        elem.push_str(placement);
        elem.push_str("/>\n");
        Some(elem)
    }

    fn push_type3_clip_stroke(
        &mut self,
        path: &Path,
        style: &StrokeStyle,
        glyph_to_page: &zpdf_core::Matrix,
    ) {
        let clip_attr = self
            .clip
            .map(|c| format!(" clip-path=\"url(#c{c})\""))
            .unwrap_or_default();
        let Some(stroke_elem) = self.format_type3_stroke_element(
            path,
            style,
            glyph_to_page,
            "#ffffff",
            1.0,
            &clip_attr,
        ) else {
            return;
        };
        let svg_m = self.type3_glyph_to_svg_matrix(glyph_to_page);
        let max_scale = Self::max_linear_scale_2x2(svg_m[0], svg_m[1], svg_m[2], svg_m[3]);
        let page_path = Self::type3_path_to_page(path, glyph_to_page);
        let scaled = Self::scale_type3_stroke_style(style, max_scale / self.scale.max(1e-9));
        let pad = self.stroke_svg_pad(&scaled);
        let next_bounds = self.path_svg_bounds(&page_path, pad);
        let id = self.fresh_id();
        let _ = write!(
            self.defs,
            "<mask id=\"m{id}\" maskUnits=\"userSpaceOnUse\">"
        );
        if let Some(outer) = self.stroke_mask {
            let _ = write!(self.defs, "<g mask=\"url(#m{outer})\">");
        }
        self.defs.push_str(&stroke_elem);
        if self.stroke_mask.is_some() {
            self.defs.push_str("</g>");
        }
        self.defs.push_str("</mask>\n");
        self.stroke_mask = Some(id);
        if let Some(cb) = next_bounds {
            self.clip_bounds = Some(cb);
        }
    }

    fn scale_type3_stroke_style(style: &StrokeStyle, scale: f64) -> StrokeStyle {
        let s = if scale.is_finite() && scale >= 0.0 {
            scale as f32
        } else {
            0.0
        };
        StrokeStyle {
            width: style.width * s,
            cap: style.cap,
            join: style.join,
            miter_limit: style.miter_limit,
            dash: style.dash.as_ref().map(|d| zpdf_display_list::DashPattern {
                array: d.array.iter().map(|v| v * s).collect(),
                phase: d.phase * s,
            }),
        }
    }

    /// Type 3 glyph runs painted with a [`TilingPattern`]: keep explicit (`d0`)
    /// colored vectors and non-mask images in their own colors while rendering
    /// inherited fills, strokes, and `/ImageMask true` stencils as opaque white
    /// masks composited with `tiling` in exact painter's order, honoring
    /// glyph-local clips.
    fn emit_tiling_type3_run(
        &mut self,
        run: &GlyphRun,
        font: &LoadedFont,
        tiling: &zpdf_display_list::TilingPattern,
        opacity: f32,
        depth: EmitDepth,
    ) {
        enum Type3TilingPart {
            InheritedShape {
                body: String,
                bounds: Option<SvgRect>,
            },
            ColoredImage {
                key: ImageDefKey,
                program: Rc<Type3Glyph>,
                image_id: ImageId,
                transform: zpdf_core::Matrix,
                alpha: f32,
                clip: Option<u32>,
                stroke_mask: Option<u32>,
                bounds: Option<SvgRect>,
            },
            ColoredVector {
                body: String,
                bounds: Option<SvgRect>,
            },
            BlendGroup {
                blend_mode: BlendMode,
                isolated: bool,
                alpha: f32,
                mask_id: Option<u32>,
                children: Vec<Type3TilingPart>,
                bounds: Option<SvgRect>,
            },
        }

        fn part_bounds(part: &Type3TilingPart) -> Option<SvgRect> {
            match part {
                Type3TilingPart::InheritedShape { bounds, .. }
                | Type3TilingPart::ColoredImage { bounds, .. }
                | Type3TilingPart::ColoredVector { bounds, .. }
                | Type3TilingPart::BlendGroup { bounds, .. } => *bounds,
            }
        }

        fn union_part_bounds(parts: &[Type3TilingPart]) -> Option<SvgRect> {
            let mut acc: Option<SvgRect> = None;
            for part in parts {
                if let Some(r) = part_bounds(part) {
                    acc = Some(match acc {
                        Some(cur) => cur.union(r),
                        None => r,
                    });
                }
            }
            acc
        }

        fn parts_have_inherited(parts: &[Type3TilingPart]) -> bool {
            parts.iter().any(|p| match p {
                Type3TilingPart::InheritedShape { .. } => true,
                Type3TilingPart::BlendGroup { children, .. } => parts_have_inherited(children),
                Type3TilingPart::ColoredImage { .. } | Type3TilingPart::ColoredVector { .. } => {
                    false
                }
            })
        }

        fn count_inherited_shapes(parts: &[Type3TilingPart]) -> usize {
            parts
                .iter()
                .map(|p| match p {
                    Type3TilingPart::InheritedShape { .. } => 1,
                    Type3TilingPart::BlendGroup { children, .. } => {
                        count_inherited_shapes(children)
                    }
                    Type3TilingPart::ColoredImage { .. }
                    | Type3TilingPart::ColoredVector { .. } => 0,
                })
                .sum()
        }

        fn parts_have_non_isolated_backdrop_group(
            parts: &[Type3TilingPart],
            plan_is_backdrop: bool,
            in_non_isolated: bool,
        ) -> bool {
            for part in parts {
                match part {
                    Type3TilingPart::InheritedShape { .. } => {
                        if in_non_isolated && plan_is_backdrop {
                            return true;
                        }
                    }
                    Type3TilingPart::BlendGroup {
                        blend_mode,
                        isolated,
                        children,
                        ..
                    } => {
                        if in_non_isolated && *blend_mode != BlendMode::Normal {
                            return true;
                        }
                        if parts_have_non_isolated_backdrop_group(
                            children,
                            plan_is_backdrop,
                            !*isolated,
                        ) {
                            return true;
                        }
                    }
                    Type3TilingPart::ColoredImage { .. }
                    | Type3TilingPart::ColoredVector { .. } => {}
                }
            }
            false
        }

        fn push_blend_group_part(
            parts: &mut Vec<Type3TilingPart>,
            blend_mode: BlendMode,
            isolated: bool,
            alpha: f32,
            mask_id: Option<u32>,
            children: Vec<Type3TilingPart>,
        ) {
            if children.is_empty() {
                return;
            }
            if blend_mode == BlendMode::Normal && !isolated && alpha >= 1.0 && mask_id.is_none() {
                parts.extend(children);
                return;
            }
            let bounds = union_part_bounds(&children);
            parts.push(Type3TilingPart::BlendGroup {
                blend_mode,
                isolated,
                alpha,
                mask_id,
                children,
                bounds,
            });
        }

        fn emit_colored_parts(writer: &mut SvgWriter<'_>, parts: &[Type3TilingPart], opacity: f32) {
            for part in parts {
                match part {
                    Type3TilingPart::ColoredImage {
                        key,
                        program,
                        image_id,
                        transform,
                        alpha,
                        clip,
                        stroke_mask,
                        ..
                    } => {
                        if let Some(image) = program.images.get(*image_id) {
                            let prev_clip = writer.clip;
                            let prev_mask = writer.stroke_mask;
                            writer.clip = *clip;
                            writer.stroke_mask = *stroke_mask;
                            writer.place_image(*key, image, None, transform, *alpha * opacity);
                            writer.clip = prev_clip;
                            writer.stroke_mask = prev_mask;
                        }
                    }
                    Type3TilingPart::ColoredVector { body, .. } => {
                        writer.out.push_str(body);
                    }
                    Type3TilingPart::BlendGroup {
                        blend_mode,
                        isolated,
                        alpha,
                        mask_id,
                        children,
                        ..
                    } => {
                        writer.flush_pending_backdrop();
                        writer.flush_disjoint_backdrop();
                        let prev_out = std::mem::take(&mut writer.out);
                        emit_colored_parts(writer, children, opacity);
                        let inner_body = std::mem::replace(&mut writer.out, prev_out);
                        writer.write_blend_group_tag(
                            *blend_mode,
                            *isolated,
                            *alpha,
                            *mask_id,
                            &inner_body,
                        );
                    }
                    Type3TilingPart::InheritedShape { .. } => {}
                }
            }
        }

        let mut parts: Vec<Type3TilingPart> = Vec::new();
        let mut cur_body = String::new();
        let (mut min_x, mut min_y) = (f64::INFINITY, f64::INFINITY);
        let (mut max_x, mut max_y) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
        let mut cur_pad = 2.0_f64;

        let saved_out = std::mem::take(&mut self.out);
        let saved_pending = self.pending_backdrop.take();
        let saved_disjoint = self.disjoint_backdrop.take();
        let saved_non_isolated = std::mem::replace(&mut self.non_isolated_filter_depth, 0);
        let saved_non_isolated_ga = std::mem::replace(&mut self.non_isolated_ga_depth, 0);

        let flush_shape = |writer: &mut Self,
                           parts: &mut Vec<Type3TilingPart>,
                           cur_body: &mut String,
                           min_x: &mut f64,
                           min_y: &mut f64,
                           max_x: &mut f64,
                           max_y: &mut f64,
                           cur_pad: &mut f64| {
            writer.flush_pending_backdrop();
            writer.flush_disjoint_backdrop();
            cur_body.push_str(&std::mem::take(&mut writer.out));
            if !cur_body.is_empty() {
                let bounds = SvgRect::from_bounds_clamped(
                    *min_x,
                    *min_y,
                    *max_x,
                    *max_y,
                    *cur_pad,
                    writer.page_width,
                    writer.page_height,
                );
                parts.push(Type3TilingPart::InheritedShape {
                    body: std::mem::take(cur_body),
                    bounds,
                });
            }
            *min_x = f64::INFINITY;
            *min_y = f64::INFINITY;
            *max_x = f64::NEG_INFINITY;
            *max_y = f64::NEG_INFINITY;
            *cur_pad = 2.0;
        };

        let white = Color::white();
        for glyph in &run.glyphs {
            if self.check_over_budget() {
                break;
            }
            let Some(program) = self.type3_glyph(run.font_id, font, glyph.glyph_id) else {
                continue;
            };
            let font_matrix = program.font_matrix;
            let glyph_to_page = Self::type3_glyph_to_page(run, glyph, &font_matrix);

            let (x0, y1, scale) = (self.x0, self.y1, self.scale);
            let map_glyph_pt = |gx: f64, gy: f64| -> Option<(f64, f64)> {
                let p = zpdf_core::Point::new(gx, gy).transform(&glyph_to_page);
                let (sx, sy) = ((p.x - x0) * scale, (y1 - p.y) * scale);
                (coord_ok(sx) && coord_ok(sy)).then_some((sx, sy))
            };
            struct Type3BlendFrame {
                blend_mode: BlendMode,
                isolated: bool,
                alpha: f32,
                mask_id: Option<u32>,
                prev_parts: Vec<Type3TilingPart>,
            }
            let mut glyph_clips: Vec<(Option<u32>, Option<u32>, Option<SvgRect>)> = Vec::new();
            let mut glyph_blends: Vec<Type3BlendFrame> = Vec::new();
            let resolve_glyph_img = |id: ImageId| program.inline_images.get(&id).cloned();

            for cmd in &program.list.commands {
                if self.check_over_budget() {
                    break;
                }
                match cmd {
                    RenderCommand::PushClip { path, rule } => {
                        let prev = (self.clip, self.stroke_mask, self.clip_bounds);
                        glyph_clips.push(prev);
                        let page_path = Self::type3_path_to_page(path, &glyph_to_page);
                        self.push_clip(&page_path, rule);
                        if let (Some(outer_b), Some(new_b)) = (prev.2, self.clip_bounds) {
                            self.clip_bounds = outer_b.intersect(new_b).or(Some(new_b));
                        }
                    }
                    RenderCommand::PushClipStroke { path, style } => {
                        let prev = (self.clip, self.stroke_mask, self.clip_bounds);
                        glyph_clips.push(prev);
                        self.push_type3_clip_stroke(path, style, &glyph_to_page);
                        if let (Some(outer_b), Some(new_b)) = (prev.2, self.clip_bounds) {
                            self.clip_bounds = outer_b.intersect(new_b).or(Some(new_b));
                        }
                    }
                    RenderCommand::PopClip => {
                        if let Some((clip, stroke_mask, clip_bounds)) = glyph_clips.pop() {
                            self.clip = clip;
                            self.stroke_mask = stroke_mask;
                            self.clip_bounds = clip_bounds;
                        }
                    }
                    RenderCommand::PushBlendGroup {
                        blend_mode,
                        isolated,
                        alpha,
                        mask,
                        ..
                    } => {
                        flush_shape(
                            self,
                            &mut parts,
                            &mut cur_body,
                            &mut min_x,
                            &mut min_y,
                            &mut max_x,
                            &mut max_y,
                            &mut cur_pad,
                        );
                        let eff_alpha = if alpha.is_finite() {
                            alpha.clamp(0.0, 1.0)
                        } else {
                            1.0
                        };
                        let mask_id = mask.as_ref().and_then(|m| {
                            self.emit_transformed_soft_mask_def(
                                m,
                                &glyph_to_page,
                                &resolve_glyph_img,
                                depth,
                            )
                        });
                        let prev_parts = std::mem::take(&mut parts);
                        glyph_blends.push(Type3BlendFrame {
                            blend_mode: *blend_mode,
                            isolated: *isolated,
                            alpha: eff_alpha,
                            mask_id,
                            prev_parts,
                        });
                    }
                    RenderCommand::PopBlendGroup => {
                        if let Some(frame) = glyph_blends.pop() {
                            flush_shape(
                                self,
                                &mut parts,
                                &mut cur_body,
                                &mut min_x,
                                &mut min_y,
                                &mut max_x,
                                &mut max_y,
                                &mut cur_pad,
                            );
                            let children = std::mem::replace(&mut parts, frame.prev_parts);
                            push_blend_group_part(
                                &mut parts,
                                frame.blend_mode,
                                frame.isolated,
                                frame.alpha,
                                frame.mask_id,
                                children,
                            );
                        }
                    }
                    RenderCommand::FillPath {
                        path,
                        rule,
                        paint,
                        alpha,
                        ..
                    } => {
                        let cmd_alpha = if alpha.is_finite() {
                            alpha.clamp(0.0, 1.0)
                        } else {
                            0.0
                        };
                        if cmd_alpha <= 0.0 {
                            continue;
                        }
                        let Some(d) = self.type3_path_data(path, &glyph_to_page) else {
                            continue;
                        };
                        let (mut px0, mut py0) = (f64::INFINITY, f64::INFINITY);
                        let (mut px1, mut py1) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
                        for p in path.points() {
                            if let Some((sx, sy)) = map_glyph_pt(p.x, p.y) {
                                px0 = px0.min(sx);
                                py0 = py0.min(sy);
                                px1 = px1.max(sx);
                                py1 = py1.max(sy);
                            }
                        }
                        match paint {
                            Paint::Solid(c) => {
                                flush_shape(
                                    self,
                                    &mut parts,
                                    &mut cur_body,
                                    &mut min_x,
                                    &mut min_y,
                                    &mut max_x,
                                    &mut max_y,
                                    &mut cur_pad,
                                );
                                let mut elem = String::with_capacity(d.len() + 64);
                                elem.push_str("<path d=\"");
                                elem.push_str(&d);
                                elem.push_str("\" fill=\"");
                                elem.push_str(&hex_color(c));
                                elem.push('"');
                                if *rule == FillRule::EvenOdd {
                                    elem.push_str(" fill-rule=\"evenodd\"");
                                }
                                push_opacity(&mut elem, "fill-opacity", opacity * cmd_alpha);
                                let placement = self.placement_attrs();
                                elem.push_str(&placement);
                                elem.push_str("/>\n");
                                let bounds = SvgRect::from_bounds_clamped(
                                    px0,
                                    py0,
                                    px1,
                                    py1,
                                    2.0,
                                    self.page_width,
                                    self.page_height,
                                );
                                parts.push(Type3TilingPart::ColoredVector { body: elem, bounds });
                            }
                            Paint::Inherited => {
                                min_x = min_x.min(px0);
                                min_y = min_y.min(py0);
                                max_x = max_x.max(px1);
                                max_y = max_y.max(py1);
                                self.out.push_str("<path d=\"");
                                self.out.push_str(&d);
                                self.out.push_str("\" fill=\"#ffffff\"");
                                if *rule == FillRule::EvenOdd {
                                    self.out.push_str(" fill-rule=\"evenodd\"");
                                }
                                push_opacity(&mut self.out, "fill-opacity", cmd_alpha);
                                let placement = self.placement_attrs();
                                self.out.push_str(&placement);
                                self.out.push_str("/>\n");
                            }
                            _ => {}
                        }
                    }
                    RenderCommand::StrokePath {
                        path,
                        style,
                        paint,
                        alpha,
                        ..
                    } => {
                        let cmd_alpha = if alpha.is_finite() {
                            alpha.clamp(0.0, 1.0)
                        } else {
                            0.0
                        };
                        if cmd_alpha <= 0.0 {
                            continue;
                        }
                        let svg_m = self.type3_glyph_to_svg_matrix(&glyph_to_page);
                        let max_scale =
                            Self::max_linear_scale_2x2(svg_m[0], svg_m[1], svg_m[2], svg_m[3]);
                        if !max_scale.is_finite() || !style.width.is_finite() {
                            continue;
                        }
                        let scaled_style =
                            Self::scale_type3_stroke_style(style, max_scale / self.scale.max(1e-9));
                        let stroke_pad = self.stroke_svg_pad(&scaled_style) + 2.0;
                        let (mut px0, mut py0) = (f64::INFINITY, f64::INFINITY);
                        let (mut px1, mut py1) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
                        for p in path.points() {
                            if let Some((sx, sy)) = map_glyph_pt(p.x, p.y) {
                                px0 = px0.min(sx);
                                py0 = py0.min(sy);
                                px1 = px1.max(sx);
                                py1 = py1.max(sy);
                            }
                        }
                        match paint {
                            Paint::Solid(c) => {
                                flush_shape(
                                    self,
                                    &mut parts,
                                    &mut cur_body,
                                    &mut min_x,
                                    &mut min_y,
                                    &mut max_x,
                                    &mut max_y,
                                    &mut cur_pad,
                                );
                                let placement = self.placement_attrs();
                                let color_hex = hex_color(c);
                                let Some(elem) = self.format_type3_stroke_element(
                                    path,
                                    style,
                                    &glyph_to_page,
                                    &color_hex,
                                    opacity * cmd_alpha,
                                    &placement,
                                ) else {
                                    continue;
                                };
                                let bounds = SvgRect::from_bounds_clamped(
                                    px0,
                                    py0,
                                    px1,
                                    py1,
                                    stroke_pad,
                                    self.page_width,
                                    self.page_height,
                                );
                                parts.push(Type3TilingPart::ColoredVector { body: elem, bounds });
                            }
                            Paint::Inherited => {
                                let placement = self.placement_attrs();
                                let Some(elem) = self.format_type3_stroke_element(
                                    path,
                                    style,
                                    &glyph_to_page,
                                    "#ffffff",
                                    cmd_alpha,
                                    &placement,
                                ) else {
                                    continue;
                                };
                                cur_pad = cur_pad.max(stroke_pad);
                                min_x = min_x.min(px0);
                                min_y = min_y.min(py0);
                                max_x = max_x.max(px1);
                                max_y = max_y.max(py1);
                                self.out.push_str(&elem);
                            }
                            _ => {}
                        }
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
                        if alpha <= 0.0 {
                            continue;
                        }
                        let (mut ix0, mut iy0) = (f64::INFINITY, f64::INFINITY);
                        let (mut ix1, mut iy1) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
                        for &(ux, uy) in &[(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)] {
                            let p = zpdf_core::Point::new(ux, uy).transform(&transform);
                            let (sx, sy) = ((p.x - x0) * scale, (y1 - p.y) * scale);
                            if coord_ok(sx) && coord_ok(sy) {
                                ix0 = ix0.min(sx);
                                iy0 = iy0.min(sy);
                                ix1 = ix1.max(sx);
                                iy1 = iy1.max(sy);
                            }
                        }
                        if draw.is_image_mask {
                            min_x = min_x.min(ix0);
                            min_y = min_y.min(iy0);
                            max_x = max_x.max(ix1);
                            max_y = max_y.max(iy1);
                            let tint = [white.r, white.g, white.b, white.a].map(f32::to_bits);
                            let key = ImageDefKey::Type3Stencil {
                                font: run.font_id,
                                glyph: glyph.glyph_id,
                                image: draw.image_id,
                                tint,
                            };
                            self.place_image(key, image, Some(&white), &transform, alpha);
                        } else {
                            flush_shape(
                                self,
                                &mut parts,
                                &mut cur_body,
                                &mut min_x,
                                &mut min_y,
                                &mut max_x,
                                &mut max_y,
                                &mut cur_pad,
                            );
                            let key = ImageDefKey::Type3Stencil {
                                font: run.font_id,
                                glyph: glyph.glyph_id,
                                image: draw.image_id,
                                tint: [u32::MAX; 4],
                            };
                            let bounds = SvgRect::from_bounds_clamped(
                                ix0,
                                iy0,
                                ix1,
                                iy1,
                                2.0,
                                self.page_width,
                                self.page_height,
                            );
                            parts.push(Type3TilingPart::ColoredImage {
                                key,
                                program: Rc::clone(&program),
                                image_id: draw.image_id,
                                transform,
                                alpha,
                                clip: self.clip,
                                stroke_mask: self.stroke_mask,
                                bounds,
                            });
                        }
                    }
                    _ => {}
                }
            }
            while let Some(frame) = glyph_blends.pop() {
                flush_shape(
                    self,
                    &mut parts,
                    &mut cur_body,
                    &mut min_x,
                    &mut min_y,
                    &mut max_x,
                    &mut max_y,
                    &mut cur_pad,
                );
                let children = std::mem::replace(&mut parts, frame.prev_parts);
                push_blend_group_part(
                    &mut parts,
                    frame.blend_mode,
                    frame.isolated,
                    frame.alpha,
                    frame.mask_id,
                    children,
                );
            }
            while let Some((clip, stroke_mask, clip_bounds)) = glyph_clips.pop() {
                self.clip = clip;
                self.stroke_mask = stroke_mask;
                self.clip_bounds = clip_bounds;
            }
        }
        flush_shape(
            self,
            &mut parts,
            &mut cur_body,
            &mut min_x,
            &mut min_y,
            &mut max_x,
            &mut max_y,
            &mut cur_pad,
        );
        self.out = saved_out;
        self.pending_backdrop = saved_pending;
        self.disjoint_backdrop = saved_disjoint;
        self.non_isolated_filter_depth = saved_non_isolated;
        self.non_isolated_ga_depth = saved_non_isolated_ga;

        if parts.is_empty() {
            return;
        }
        let run_union = union_part_bounds(&parts);
        let run_affected = match (run_union, self.clip_bounds) {
            (Some(gb), Some(cb)) => gb.intersect(cb).unwrap_or(gb),
            (Some(gb), None) => gb,
            (None, Some(cb)) => cb,
            (None, None) => self.full_page_rect(),
        };
        if !parts_have_inherited(&parts) {
            if depth.is_top_level()
                && (self.pending_backdrop.is_some() || !self.out.is_empty())
                && parts_have_non_isolated_backdrop_group(&parts, false, false)
                && self.emit_raster_backdrop_tiling_fallback(run_affected)
            {
                return;
            }
            emit_colored_parts(self, &parts, opacity);
            return;
        }

        let plan = self.emit_tiling_plan(tiling, depth);
        let plan_is_backdrop = match &plan {
            TilingPassPlan::Affine { .. } => true,
            TilingPassPlan::Ops(ops) => match ops.as_slice() {
                [] => false,
                [TilingPlanOp::Pattern { blend_mode, .. }] => *blend_mode != BlendMode::Normal,
                _ => true,
            },
        };
        if depth.is_top_level()
            && (self.pending_backdrop.is_some() || !self.out.is_empty())
            && parts_have_non_isolated_backdrop_group(&parts, plan_is_backdrop, false)
            && self.emit_raster_backdrop_tiling_fallback(run_affected)
        {
            return;
        }
        let plan_can_fallback = depth.is_top_level()
            && match &plan {
                TilingPassPlan::Affine { .. } => true,
                TilingPassPlan::Ops(ops) => {
                    !ops.is_empty() && !matches!(ops.as_slice(), [TilingPlanOp::Pattern { .. }])
                }
            };
        let total_inherited = count_inherited_shapes(&parts);
        let first_is_only_inherited = parts.len() == 1
            && matches!(parts.first(), Some(Type3TilingPart::InheritedShape { .. }));
        // Only plans that can fall back AND emit parts around or nested inside
        // groups need rollback staging. Ordinary single-pass patterns and
        // single-shape runs incur zero staging or checkpoint overhead.
        let mut can_rollback = plan_can_fallback && !first_is_only_inherited;
        let mut fallback_allowed = plan_can_fallback;

        enum EmitPartsOutcome {
            Done,
            NeedFallback,
        }

        struct TilingEmitState {
            staged_prior_out: Option<String>,
            prior_def_id: Option<u32>,
            can_rollback: bool,
            fallback_allowed: bool,
            inherited_remaining: usize,
        }

        #[allow(clippy::too_many_arguments)]
        fn emit_tiling_parts(
            writer: &mut SvgWriter<'_>,
            parts: &[Type3TilingPart],
            plan: &TilingPassPlan,
            opacity: f32,
            depth: EmitDepth,
            in_blend_group: bool,
            state: &mut TilingEmitState,
        ) -> EmitPartsOutcome {
            for part in parts {
                match part {
                    Type3TilingPart::ColoredImage {
                        key,
                        program,
                        image_id,
                        transform,
                        alpha,
                        clip,
                        stroke_mask,
                        ..
                    } => {
                        if let Some(image) = program.images.get(*image_id) {
                            let prev_clip = writer.clip;
                            let prev_mask = writer.stroke_mask;
                            writer.clip = *clip;
                            writer.stroke_mask = *stroke_mask;
                            writer.place_image(*key, image, None, transform, *alpha * opacity);
                            writer.clip = prev_clip;
                            writer.stroke_mask = prev_mask;
                        }
                    }
                    Type3TilingPart::ColoredVector { body, .. } => {
                        writer.out.push_str(body);
                    }
                    Type3TilingPart::BlendGroup {
                        blend_mode,
                        isolated,
                        alpha,
                        mask_id,
                        children,
                        ..
                    } => {
                        writer.flush_pending_backdrop();
                        writer.flush_disjoint_backdrop();
                        let prev_out = std::mem::take(&mut writer.out);
                        let prev_pending = writer.pending_backdrop.take();
                        let prev_disjoint = writer.disjoint_backdrop.take();
                        let prev_non_isolated =
                            std::mem::replace(&mut writer.non_isolated_filter_depth, 0);
                        let prev_non_isolated_ga =
                            std::mem::replace(&mut writer.non_isolated_ga_depth, 0);
                        let prev_blend_depth = writer.blend_depth;
                        let prev_isolated_depth = writer.isolated_depth;
                        writer.blend_depth += 1;
                        writer.isolated_depth += 1;
                        let outcome =
                            emit_tiling_parts(writer, children, plan, opacity, depth, true, state);
                        match outcome {
                            EmitPartsOutcome::Done => {
                                writer.flush_pending_backdrop();
                                writer.flush_disjoint_backdrop();
                                let inner_body = std::mem::replace(&mut writer.out, prev_out);
                                writer.pending_backdrop = prev_pending;
                                writer.disjoint_backdrop = prev_disjoint;
                                writer.non_isolated_filter_depth = prev_non_isolated;
                                writer.non_isolated_ga_depth = prev_non_isolated_ga;
                                writer.blend_depth = prev_blend_depth;
                                writer.isolated_depth = prev_isolated_depth;
                                writer.write_blend_group_tag(
                                    *blend_mode,
                                    *isolated,
                                    *alpha,
                                    *mask_id,
                                    &inner_body,
                                );
                            }
                            EmitPartsOutcome::NeedFallback => {
                                writer.out = prev_out;
                                writer.pending_backdrop = prev_pending;
                                writer.disjoint_backdrop = prev_disjoint;
                                writer.non_isolated_filter_depth = prev_non_isolated;
                                writer.non_isolated_ga_depth = prev_non_isolated_ga;
                                writer.blend_depth = prev_blend_depth;
                                writer.isolated_depth = prev_isolated_depth;
                                return EmitPartsOutcome::NeedFallback;
                            }
                        }
                    }
                    Type3TilingPart::InheritedShape {
                        body: shape_body,
                        bounds,
                    } => {
                        state.inherited_remaining = state.inherited_remaining.saturating_sub(1);
                        let is_last_inherited = state.inherited_remaining == 0;
                        let affected = match (*bounds, writer.clip_bounds) {
                            (Some(gb), Some(cb)) => gb.intersect(cb).unwrap_or(gb),
                            (Some(gb), None) => gb,
                            (None, Some(cb)) => cb,
                            (None, None) => writer.full_page_rect(),
                        };
                        let prior_has_content = !in_blend_group
                            && state
                                .staged_prior_out
                                .as_ref()
                                .is_some_and(|p| !p.is_empty());
                        let has_any_backdrop = writer.pending_backdrop.is_some()
                            || prior_has_content
                            || !writer.out.is_empty();
                        match plan {
                            TilingPassPlan::Affine { t_id, k_id, mk_id } => {
                                let (t_id, k_id, mk_id) = (*t_id, *k_id, *mk_id);
                                if depth.is_top_level() {
                                    let affine_prims = if writer.non_isolated_ga_depth > 0 {
                                        18
                                    } else {
                                        14
                                    };
                                    if state.can_rollback && !in_blend_group {
                                        let will_fit = writer.can_enqueue_backdrop_step(
                                            affine_prims,
                                            Some(affected),
                                            true,
                                            prior_has_content || !writer.out.is_empty(),
                                        );
                                        if !will_fit {
                                            if state.fallback_allowed && has_any_backdrop {
                                                return EmitPartsOutcome::NeedFallback;
                                            }
                                        } else if let Some(mut prior) =
                                            state.staged_prior_out.take()
                                        {
                                            if is_last_inherited || prior.is_empty() {
                                                prior.push_str(&writer.out);
                                                writer.out = prior;
                                            } else {
                                                let rid = writer.fresh_id();
                                                let _ = write!(
                                                    writer.defs,
                                                    "<g id=\"r{rid}\">\n{prior}</g>\n"
                                                );
                                                state.prior_def_id = Some(rid);
                                                let run_tail = std::mem::take(&mut writer.out);
                                                let _ =
                                                    writeln!(writer.out, "<use href=\"#r{rid}\"/>");
                                                writer.out.push_str(&run_tail);
                                            }
                                        }
                                    }
                                    let shape_id = writer.fresh_id();
                                    let _ = write!(writer.defs, "<g id=\"s{shape_id}\"");
                                    push_opacity(&mut writer.defs, "opacity", opacity);
                                    writer.defs.push_str(">\n");
                                    writer.defs.push_str(shape_body);
                                    writer.defs.push_str("</g>\n");
                                    if writer.enqueue_backdrop_step(
                                        BackdropStep::Affine {
                                            t_id,
                                            k_id,
                                            mk_id,
                                            shape_id,
                                        },
                                        affine_prims,
                                        Some(affected),
                                        true,
                                    ) {
                                        continue;
                                    }
                                    if state.fallback_allowed && has_any_backdrop {
                                        return EmitPartsOutcome::NeedFallback;
                                    }
                                    if !in_blend_group {
                                        if let Some(mut prior) = state.staged_prior_out.take() {
                                            prior.push_str(&writer.out);
                                            writer.out = prior;
                                            state.can_rollback = false;
                                        }
                                    }
                                    writer.flush_pending_backdrop();
                                }
                                let mask_id = writer.fresh_id();
                                let _ = write!(
                                    writer.defs,
                                    "<mask id=\"m{mask_id}\" maskUnits=\"userSpaceOnUse\">\n{shape_body}</mask>\n"
                                );
                                writer.out.push_str("<rect x=\"0\" y=\"0\" width=\"");
                                push_num(&mut writer.out, writer.page_width);
                                writer.out.push_str("\" height=\"");
                                push_num(&mut writer.out, writer.page_height);
                                let _ = write!(
                                    writer.out,
                                    "\" fill=\"url(#p{t_id})\" mask=\"url(#m{mask_id})\""
                                );
                                push_opacity(&mut writer.out, "fill-opacity", opacity);
                                writer.out.push_str("/>\n");
                            }
                            TilingPassPlan::Ops(ops) => {
                                if ops.is_empty() {
                                    continue;
                                }
                                let single_pattern = match ops.as_slice() {
                                    [TilingPlanOp::Pattern { pat_id, blend_mode }] => {
                                        Some((*pat_id, *blend_mode))
                                    }
                                    _ => None,
                                };
                                if depth.is_top_level()
                                    && (single_pattern.is_none()
                                        || (writer.pending_backdrop.is_some()
                                            && single_pattern
                                                .is_some_and(|(_, m)| m != BlendMode::Normal)))
                                {
                                    if state.can_rollback && !in_blend_group {
                                        if let Some(prior) = state.staged_prior_out.take() {
                                            if !prior.is_empty() {
                                                let rid = writer.fresh_id();
                                                let _ = write!(
                                                    writer.defs,
                                                    "<g id=\"r{rid}\">\n{prior}</g>\n"
                                                );
                                                state.prior_def_id = Some(rid);
                                                let run_tail = std::mem::take(&mut writer.out);
                                                let _ =
                                                    writeln!(writer.out, "<use href=\"#r{rid}\"/>");
                                                writer.out.push_str(&run_tail);
                                            }
                                        }
                                    }
                                    let mut wrapped_shape =
                                        String::with_capacity(shape_body.len() + 48);
                                    wrapped_shape.push_str("<g");
                                    push_opacity(&mut wrapped_shape, "opacity", opacity);
                                    wrapped_shape.push_str(">\n");
                                    wrapped_shape.push_str(shape_body);
                                    wrapped_shape.push_str("</g>\n");
                                    if writer.emit_tiling_multi_pass_backdrop(
                                        ops,
                                        &wrapped_shape,
                                        affected,
                                        affected,
                                    ) {
                                        continue;
                                    }
                                    if single_pattern.is_none()
                                        && state.fallback_allowed
                                        && (writer.pending_backdrop.is_some()
                                            || !writer.out.is_empty()
                                            || prior_has_content)
                                    {
                                        return EmitPartsOutcome::NeedFallback;
                                    }
                                    writer.flush_pending_backdrop();
                                }
                                let mask_id = writer.fresh_id();
                                let _ = write!(
                                    writer.defs,
                                    "<mask id=\"m{mask_id}\" maskUnits=\"userSpaceOnUse\">\n{shape_body}</mask>\n"
                                );
                                if let Some((id, blend_mode)) = single_pattern {
                                    writer.out.push_str("<rect x=\"0\" y=\"0\" width=\"");
                                    push_num(&mut writer.out, writer.page_width);
                                    writer.out.push_str("\" height=\"");
                                    push_num(&mut writer.out, writer.page_height);
                                    let _ = write!(
                                        writer.out,
                                        "\" fill=\"url(#p{id})\" mask=\"url(#m{mask_id})\""
                                    );
                                    push_opacity(&mut writer.out, "fill-opacity", opacity);
                                    if let Some(name) = blend_css(blend_mode) {
                                        let _ =
                                            write!(writer.out, " style=\"mix-blend-mode:{name};\"");
                                    }
                                    writer.out.push_str("/>\n");
                                } else {
                                    writer.out.push_str("<g");
                                    push_opacity(&mut writer.out, "opacity", opacity);
                                    let _ = writeln!(
                                        writer.out,
                                        " style=\"isolation:isolate;\" mask=\"url(#m{mask_id})\">"
                                    );
                                    writer.emit_isolated_tiling_ops(ops);
                                    writer.out.push_str("</g>\n");
                                }
                            }
                        }
                    }
                }
            }
            EmitPartsOutcome::Done
        }

        'emit_parts: loop {
            let mut saved_meta = None;
            let mut state = TilingEmitState {
                staged_prior_out: None,
                prior_def_id: None,
                can_rollback,
                fallback_allowed,
                inherited_remaining: total_inherited,
            };
            if can_rollback {
                state.staged_prior_out = Some(std::mem::take(&mut self.out));
                saved_meta = Some((
                    self.pending_backdrop.clone(),
                    self.disjoint_backdrop.clone(),
                    self.backdrop_fanout,
                    self.backdrop_snapshots,
                ));
            }
            match emit_tiling_parts(self, &parts, &plan, opacity, depth, false, &mut state) {
                EmitPartsOutcome::Done => {
                    if let Some(mut prior) = state.staged_prior_out.take() {
                        prior.push_str(&self.out);
                        self.out = prior;
                    }
                    break;
                }
                EmitPartsOutcome::NeedFallback => {
                    fallback_allowed = false;
                    let retry = can_rollback;
                    can_rollback = false;
                    if let Some((pending, disjoint, fanout, snaps)) = saved_meta.take() {
                        self.pending_backdrop = pending;
                        self.disjoint_backdrop = disjoint;
                        self.backdrop_fanout = fanout;
                        self.backdrop_snapshots = snaps;
                    }
                    if let Some(prior) = state.staged_prior_out.take() {
                        self.out = prior;
                    } else if let Some(rid) = state.prior_def_id.take() {
                        self.out.clear();
                        let _ = writeln!(self.out, "<use href=\"#r{rid}\"/>");
                    }
                    if self.emit_raster_backdrop_tiling_fallback(run_affected) {
                        return;
                    }
                    if retry {
                        continue 'emit_parts;
                    }
                    break;
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
                let mut interp = ContentInterpreter::new(glyph_rect)
                    .for_type3_glyph()
                    .with_type3_resources(font.type3_resources())
                    .with_images(&mut images);
                if let Some(deadline) = self.deadline {
                    interp = interp.with_deadline(deadline);
                }
                let list = interp.interpret(stream);
                let inline_images =
                    zpdf_render_cpu::collect_type3_mask_inline_images(&list, &images);
                Rc::new(Type3Glyph {
                    list,
                    images,
                    inline_images,
                    font_matrix,
                })
            });
        let (commands, image_bytes) = glyph.as_ref().map_or((1, 0), |g| {
            let inline_bytes: u64 = g
                .inline_images
                .values()
                .map(|img| img.data.len() as u64)
                .sum();
            (
                g.list.commands.len().max(1),
                g.images.bytes_used().saturating_add(inline_bytes),
            )
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
    /// does) and emit its fills, strokes, glyph-local clips, blend/soft-mask
    /// groups, and images through the Type3 transform chain.
    fn emit_type3_run(
        &mut self,
        run: &GlyphRun,
        font: &LoadedFont,
        color: &Color,
        opacity: f32,
        depth: EmitDepth,
    ) {
        for glyph in &run.glyphs {
            if self.check_over_budget() {
                break;
            }
            let Some(program) = self.type3_glyph(run.font_id, font, glyph.glyph_id) else {
                continue;
            };
            let font_matrix = program.font_matrix;
            let glyph_to_page = Self::type3_glyph_to_page(run, glyph, &font_matrix);
            let mut glyph_clips: Vec<(Option<u32>, Option<u32>, Option<SvgRect>)> = Vec::new();
            let mut glyph_blends: Vec<(BlendMode, bool, f32, Option<u32>, String)> = Vec::new();
            let resolve_glyph_img = |id: ImageId| program.inline_images.get(&id).cloned();

            for cmd in &program.list.commands {
                if self.check_over_budget() {
                    break;
                }
                match cmd {
                    RenderCommand::PushClip { path, rule } => {
                        let prev = (self.clip, self.stroke_mask, self.clip_bounds);
                        glyph_clips.push(prev);
                        let page_path = Self::type3_path_to_page(path, &glyph_to_page);
                        self.push_clip(&page_path, rule);
                        if let (Some(outer_b), Some(new_b)) = (prev.2, self.clip_bounds) {
                            self.clip_bounds = outer_b.intersect(new_b).or(Some(new_b));
                        }
                    }
                    RenderCommand::PushClipStroke { path, style } => {
                        let prev = (self.clip, self.stroke_mask, self.clip_bounds);
                        glyph_clips.push(prev);
                        self.push_type3_clip_stroke(path, style, &glyph_to_page);
                        if let (Some(outer_b), Some(new_b)) = (prev.2, self.clip_bounds) {
                            self.clip_bounds = outer_b.intersect(new_b).or(Some(new_b));
                        }
                    }
                    RenderCommand::PopClip => {
                        if let Some((clip, stroke_mask, clip_bounds)) = glyph_clips.pop() {
                            self.clip = clip;
                            self.stroke_mask = stroke_mask;
                            self.clip_bounds = clip_bounds;
                        }
                    }
                    RenderCommand::PushBlendGroup {
                        blend_mode,
                        isolated,
                        alpha,
                        mask,
                        ..
                    } => {
                        let eff_alpha = if alpha.is_finite() {
                            alpha.clamp(0.0, 1.0)
                        } else {
                            1.0
                        };
                        let mask_id = mask.as_ref().and_then(|m| {
                            self.emit_transformed_soft_mask_def(
                                m,
                                &glyph_to_page,
                                &resolve_glyph_img,
                                depth,
                            )
                        });
                        let prev_out = std::mem::take(&mut self.out);
                        glyph_blends.push((*blend_mode, *isolated, eff_alpha, mask_id, prev_out));
                    }
                    RenderCommand::PopBlendGroup => {
                        if let Some((blend_mode, isolated, group_alpha, mask_id, prev_out)) =
                            glyph_blends.pop()
                        {
                            let inner_body = std::mem::replace(&mut self.out, prev_out);
                            self.write_blend_group_tag(
                                blend_mode,
                                isolated,
                                group_alpha,
                                mask_id,
                                &inner_body,
                            );
                        }
                    }
                    RenderCommand::FillPath {
                        path,
                        rule,
                        paint,
                        alpha,
                        ..
                    } => {
                        let cmd_alpha = if alpha.is_finite() {
                            alpha.clamp(0.0, 1.0)
                        } else {
                            0.0
                        };
                        if cmd_alpha <= 0.0 {
                            continue;
                        }
                        let draw_color = match paint {
                            Paint::Solid(c) => c,
                            Paint::Inherited => color,
                            _ => continue,
                        };
                        let Some(d) = self.type3_path_data(path, &glyph_to_page) else {
                            continue;
                        };
                        self.out.push_str("<path d=\"");
                        self.out.push_str(&d);
                        self.out.push_str("\" fill=\"");
                        self.out.push_str(&hex_color(draw_color));
                        self.out.push('"');
                        if *rule == FillRule::EvenOdd {
                            self.out.push_str(" fill-rule=\"evenodd\"");
                        }
                        push_opacity(&mut self.out, "fill-opacity", opacity * cmd_alpha);
                        let placement = self.placement_attrs();
                        self.out.push_str(&placement);
                        self.out.push_str("/>\n");
                    }
                    RenderCommand::StrokePath {
                        path,
                        style,
                        paint,
                        alpha,
                        ..
                    } => {
                        let cmd_alpha = if alpha.is_finite() {
                            alpha.clamp(0.0, 1.0)
                        } else {
                            0.0
                        };
                        if cmd_alpha <= 0.0 {
                            continue;
                        }
                        let draw_color = match paint {
                            Paint::Solid(c) => c,
                            Paint::Inherited => color,
                            _ => continue,
                        };
                        let placement = self.placement_attrs();
                        if let Some(elem) = self.format_type3_stroke_element(
                            path,
                            style,
                            &glyph_to_page,
                            &hex_color(draw_color),
                            opacity * cmd_alpha,
                            &placement,
                        ) {
                            self.out.push_str(&elem);
                        }
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
            while let Some((blend_mode, isolated, group_alpha, mask_id, prev_out)) =
                glyph_blends.pop()
            {
                let inner_body = std::mem::replace(&mut self.out, prev_out);
                self.write_blend_group_tag(blend_mode, isolated, group_alpha, mask_id, &inner_body);
            }
            while let Some((clip, stroke_mask, clip_bounds)) = glyph_clips.pop() {
                self.clip = clip;
                self.stroke_mask = stroke_mask;
                self.clip_bounds = clip_bounds;
            }
        }
    }

    fn type3_path_data(
        &mut self,
        path: &Path,
        glyph_to_page: &zpdf_core::Matrix,
    ) -> Option<String> {
        let (x0, y1, scale) = (self.x0, self.y1, self.scale);
        let map = |gx: f64, gy: f64| -> Option<(f64, f64)> {
            let p = zpdf_core::Point::new(gx, gy).transform(glyph_to_page);
            let (sx, sy) = ((p.x - x0) * scale, (y1 - p.y) * scale);
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

    fn emit_inline_image(&mut self, draw: &InlineImageDraw) {
        let alpha = if draw.alpha.is_finite() {
            draw.alpha.clamp(0.0, 1.0)
        } else {
            0.0
        };
        if alpha <= 0.0 {
            return;
        }
        let key = ImageDefKey::Inline(std::sync::Arc::as_ptr(&draw.image) as usize);
        let was_cached = self.image_defs.contains_key(&key);
        self.place_rgba_image(
            key,
            draw.image.width,
            draw.image.height,
            &draw.image.data,
            draw.image.premultiplied,
            None,
            &draw.transform,
            alpha,
        );
        if !was_cached && self.image_defs.contains_key(&key) {
            self.retained_inline_images
                .push(std::sync::Arc::clone(&draw.image));
        }
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
        self.place_rgba_image(
            key,
            image.width,
            image.height,
            &image.data,
            image.premultiplied,
            tint,
            transform,
            alpha,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn place_rgba_image(
        &mut self,
        key: ImageDefKey,
        width: u32,
        height: u32,
        data: &[u8],
        premultiplied: bool,
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
        let iw = width as f64;
        let ih = height as f64;
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
                    Some(color) => {
                        let tinted_bytes = tinted_rgba(data, premultiplied, color);
                        encode_png_rgba(width, height, &tinted_bytes, premultiplied)
                    }
                    None => encode_png_rgba(width, height, data, premultiplied),
                };
                let id = png.map(|png| {
                    let id = self.fresh_id();
                    let _ = write!(
                        self.defs,
                        "<image id=\"i{id}\" width=\"{width}\" height=\"{height}\" \
                         preserveAspectRatio=\"none\" href=\"data:image/png;base64,"
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

/// Format a normalized linear matrix entry (magnitude <= 1) with up to 6 decimals.
fn push_matrix_num(out: &mut String, v: f64) {
    let r = (v * 1_000_000.0).round() / 1_000_000.0;
    if r == r.trunc() && r.abs() < 1e15 {
        let _ = write!(out, "{}", r as i64);
    } else {
        let mut s = format!("{r:.6}");
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
fn tinted_rgba(data: &[u8], premultiplied: bool, color: &Color) -> Vec<u8> {
    let mut out = data.to_vec();
    let rgb = [color.r, color.g, color.b].map(|c| c.clamp(0.0, 1.0));
    for px in out.as_chunks_mut::<4>().0 {
        let a = px[3] as f32 / 255.0;
        let scale = if premultiplied { a } else { 1.0 };
        for (dst, c) in px[..3].iter_mut().zip(rgb) {
            *dst = (c * scale * 255.0).round() as u8;
        }
    }
    out
}

fn svg_find_matching_pop_blend(commands: &[RenderCommand], start_idx: usize) -> usize {
    let mut depth = 0usize;
    for (i, cmd) in commands.iter().enumerate().skip(start_idx) {
        match cmd {
            RenderCommand::PushBlendGroup { .. } => depth += 1,
            RenderCommand::PopBlendGroup => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
    }
    commands.len().saturating_sub(1)
}

fn svg_slice_needs_non_isolated_backdrop(commands: &[RenderCommand]) -> bool {
    commands.iter().any(|cmd| match cmd {
        RenderCommand::FillPath {
            paint: Paint::Tiling(_),
            ..
        }
        | RenderCommand::StrokePath {
            paint: Paint::Tiling(_),
            ..
        }
        | RenderCommand::DrawGlyphRun(GlyphRun {
            paint: Paint::Tiling(_),
            ..
        }) => true,
        RenderCommand::PushBlendGroup {
            blend_mode,
            knockout,
            ..
        } => *blend_mode != BlendMode::Normal || *knockout,
        _ => false,
    })
}

fn svg_clamp_raster_scale(page: zpdf_core::Rect, max_dim: u32, max_pixels: u64) -> Option<f32> {
    let page = page.normalize();
    let width_pt = page.width();
    let height_pt = page.height();
    if !(page.x0.is_finite()
        && page.y0.is_finite()
        && page.x1.is_finite()
        && page.y1.is_finite()
        && width_pt.is_finite()
        && height_pt.is_finite()
        && width_pt > 0.0
        && height_pt > 0.0)
    {
        return None;
    }
    let mut scale = 1.0_f64;
    let max_side = (width_pt * scale).max(height_pt * scale);
    if max_side > max_dim as f64 {
        scale *= max_dim as f64 / max_side;
    }
    let pixels = (width_pt * scale) * (height_pt * scale);
    if pixels > max_pixels as f64 {
        scale *= (max_pixels as f64 / pixels).sqrt();
    }
    let mut scale_f32 = scale as f32;
    for _ in 0..8 {
        if !scale_f32.is_finite() || scale_f32 <= 0.0 {
            return None;
        }
        let info = PageRenderInfo {
            page_rect: page,
            scale: scale_f32,
            background: Color::white(),
        };
        if let Ok((w, h)) = info.raster_dimensions() {
            if w <= max_dim && h <= max_dim && (w as u64) * (h as u64) <= max_pixels {
                return Some(scale_f32);
            }
        }
        scale_f32 *= 0.99;
    }
    None
}

/// Solve for the straight RGBA8 overlay pixel `X` such that Porter-Duff
/// `SourceOver(X, B) == F` (premultiplied `before`/`after`), leaving unchanged
/// pixels transparent so surrounding SVG vector content stays untouched.
///
/// Returns `None` when any changed pixel cannot be represented via `SourceOver`
/// (e.g., unchanged/reduced alpha on a translucent backdrop such as
/// `[0, 0, 0, 204] -> [1, 1, 1, 204]`), signalling the caller to replace that
/// bounded subregion directly with its true RGBA and alpha instead of baking
/// opaque white.
fn svg_compute_incremental_overlay(before_premul: &[u8], after_premul: &[u8]) -> Option<Vec<u8>> {
    let mut out = vec![0u8; before_premul.len()];
    for ((b, f), dst) in before_premul
        .as_chunks::<4>()
        .0
        .iter()
        .zip(after_premul.as_chunks::<4>().0.iter())
        .zip(out.as_chunks_mut::<4>().0.iter_mut())
    {
        if f == b {
            continue;
        }
        let ba = b[3];
        let fa = f[3];
        if ba == 0 {
            if fa == 0 {
                continue;
            }
            if fa == 255 {
                *dst = *f;
            } else {
                let a = fa as u32;
                *dst = [
                    ((f[0] as u32 * 255 + a / 2) / a).min(255) as u8,
                    ((f[1] as u32 * 255 + a / 2) / a).min(255) as u8,
                    ((f[2] as u32 * 255 + a / 2) / a).min(255) as u8,
                    fa,
                ];
            }
            continue;
        }
        if ba == 255 && fa == 255 {
            *dst = *f;
            continue;
        }
        if fa > ba {
            let bf = ba as f32 / 255.0;
            let ff = fa as f32 / 255.0;
            let ax = ((ff - bf) / (1.0 - bf)).clamp(0.0, 1.0);
            let inv_ax = 1.0 - ax;
            let mut straight = [0u8; 3];
            let mut valid = ax > 0.0;
            const EPS: f32 = 2.0 / 255.0;
            for c in 0..3 {
                let bp = b[c] as f32 / 255.0;
                let fp = f[c] as f32 / 255.0;
                let px = fp - inv_ax * bp;
                if px < -EPS || px > ax + EPS {
                    valid = false;
                    break;
                }
                let sc = (px.clamp(0.0, ax) / ax).clamp(0.0, 1.0);
                straight[c] = (sc * 255.0 + 0.5).floor().min(255.0) as u8;
            }
            if valid {
                let ax_u8 = (ax * 255.0 + 0.5).floor().clamp(1.0, 255.0) as u8;
                *dst = [straight[0], straight[1], straight[2], ax_u8];
                continue;
            }
        }
        return None;
    }
    Some(out)
}

fn encode_png(image: &DecodedImage) -> Option<Vec<u8>> {
    encode_png_rgba(image.width, image.height, &image.data, image.premultiplied)
}

fn encode_png_rgba(width: u32, height: u32, data: &[u8], premultiplied: bool) -> Option<Vec<u8>> {
    let expected = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)?;
    if width == 0 || height == 0 || data.len() != expected {
        return None;
    }
    let data: Cow<'_, [u8]> = if premultiplied {
        let mut d = data.to_vec();
        unpremultiply(&mut d);
        Cow::Owned(d)
    } else {
        Cow::Borrowed(data)
    };
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(&data, width, height, image::ExtendedColorType::Rgba8)
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
            let mut writer = SvgWriter::new(
                page().page_rect,
                100.0,
                200.0,
                &[],
                &fonts,
                &images,
                &options,
            );
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

            let mut writer = SvgWriter::new(
                page().page_rect,
                100.0,
                200.0,
                &[],
                &fonts,
                &images,
                &options,
            );
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
                    inline_images: HashMap::new(),
                    font_matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
                })),
            );
            writer.budget_checks = 62;
            writer.emit_type3_run(&run, &font, &Color::black(), 1.0, EmitDepth::default());
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
        let mut writer = SvgWriter::new(
            page().page_rect,
            100.0,
            200.0,
            &[],
            &fonts,
            &images,
            &options,
        );
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
    fn inline_image_command_embeds_and_deduplicates_png() {
        let mut dl = page();
        let img = std::sync::Arc::new(zpdf_display_list::InlineImage {
            width: 1,
            height: 1,
            data: vec![255, 0, 0, 255],
            has_alpha: false,
            is_image_mask: true,
            premultiplied: true,
        });
        dl.push(RenderCommand::DrawInlineImage(InlineImageDraw {
            image: std::sync::Arc::clone(&img),
            transform: Matrix::scale(20.0, 20.0),
            alpha: 1.0,
        }));
        dl.push(RenderCommand::DrawInlineImage(InlineImageDraw {
            image: img,
            transform: Matrix::translate(30.0, 0.0).concat(&Matrix::scale(20.0, 20.0)),
            alpha: 1.0,
        }));
        let svg = to_svg(&dl);
        assert_eq!(
            svg.matches("<image ").count(),
            1,
            "shared Arc emits one def: {svg}"
        );
        assert_eq!(
            svg.matches("<use ").count(),
            2,
            "both draws emit <use>: {svg}"
        );
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

    #[test]
    fn repeated_dense_chunked_fills_evaluate_prior_scene_once_per_fill_in_svg() {
        // Five repeated disjoint dense fills on a Letter page must reuse the
        // pre-run backdrop snapshot rather than nesting each fill inside the
        // next snapshot (which would double earlier fills on every repeat) or
        // referencing `#b...` a second time through `<feImage>` on opaque pages.
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

        // Use five wide bands (each 612×120 = 73,440 px -> max ~228 prims per
        // filter; make each cell take > 250 prims by putting 16 copies of 8
        // marks so EVERY band triggers chunking!).
        let mut big_cell = DisplayList::new(Rect::new(0.0, 0.0, 32.0, 2.0));
        big_cell.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Normal,
            isolated: false,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 32.0, 2.0),
            alpha: 0.5,
            mask: None,
        });
        for _ in 0..8 {
            big_cell.push(RenderCommand::PushBlendGroup {
                blend_mode: BlendMode::Darken,
                isolated: true,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 32.0, 2.0),
                alpha: 1.0,
                mask: None,
            });
            big_cell.push(RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 32.0, 2.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::gray(0.5)),
                alpha: 1.0,
                overprint: None,
            });
            big_cell.push(RenderCommand::PopBlendGroup);
        }
        big_cell.push(RenderCommand::PopBlendGroup);
        let chunked_tiling = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(big_cell),
            bbox: Rect::new(0.0, 0.0, 32.0, 2.0),
            x_step: 2.0,
            y_step: 2.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });

        let _ = tiling; // keep cell builder reference clear
        let page_rect = Rect::new(0.0, 0.0, 612.0, 792.0);
        let mut dl = DisplayList::new(page_rect);
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 612.0, 792.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::gray(0.8)),
            alpha: 1.0,
            overprint: None,
        });
        for i in 0..5 {
            let y0 = 20.0 + i as f64 * 150.0;
            dl.push(RenderCommand::FillPath {
                path: rect_path(0.0, y0, 612.0, y0 + 120.0),
                rule: FillRule::NonZero,
                paint: Paint::Tiling(std::sync::Arc::clone(&chunked_tiling)),
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
        // Only ONE backdrop `<g id="b...">` snapshot may exist (shared by all 5
        // disjoint chunked fills), and ZERO `<feImage href="#b` re-renders.
        assert_eq!(
            svg.matches("<g id=\"b").count(),
            1,
            "five disjoint chunked fills must share 1 pre-run backdrop snapshot, got {}",
            svg.matches("<g id=\"b").count()
        );
        assert_eq!(
            svg.matches("<feImage href=\"#b").count(),
            0,
            "chunked fills on opaque pages must not re-render prior scene via <feImage href=\"#b...\">"
        );
        // Must have emitted multi-filter chains (`url(#f...) url(#f...)`) for the chunked bands.
        assert!(
            svg.contains(") url(#f"),
            "expected space-separated multi-filter chunks on 612×120 bands"
        );

        let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default())
            .expect("valid repeated chunked SVG");
        // Render at 1/4 scale (153×198) to exercise all 5 chunked bands quickly.
        let mut pixmap = resvg::tiny_skia::Pixmap::new(153, 198).expect("pixmap");
        let sx = 153.0 / tree.size().width();
        let sy = 198.0 / tree.size().height();
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(sx, sy),
            &mut pixmap.as_mut(),
        );
        // Inside band 0 (PDF y=80 -> SVG y=712 -> scaled y=178) and band 4 (PDF y=680 -> SVG y=112 -> scaled y=28):
        // Over 0.8 gray, 16 nested 50%-opacity Darken(0.5) groups converge to ~0.5;
        // at 50% fill alpha over 0.8 gray -> 0.5 * 0.5 + 0.5 * 0.8 = 0.65 (~166).
        for (label, sy_px) in [("band 0", 178u32), ("band 4", 28u32)] {
            let px = pixmap.pixel(76, sy_px).expect("in bounds");
            let rgb = [px.red(), px.green(), px.blue()];
            assert!(
                rgb.iter().all(|&c| (158..=174).contains(&c)),
                "{label} must blend to ~166 across chunked bands, got {rgb:?}"
            );
        }
        // Gap between bands (PDF y=155 -> SVG y=637 -> scaled y=159): stays 0.8 gray (~204).
        let gap = pixmap.pixel(76, 159).expect("in bounds");
        let gap_rgb = [gap.red(), gap.green(), gap.blue()];
        assert!(
            gap_rgb.iter().all(|&c| (198..=210).contains(&c)),
            "gap between repeated chunked fills must remain 0.8 gray (~204), got {gap_rgb:?}"
        );
    }

    #[test]
    fn nested_soft_mask_tiling_opacity_group_preserves_shape_mask_in_svg() {
        // An Alpha soft mask containing a tiling pattern whose cell paints a
        // TRIANGLE with a two-segment inner pattern produces a nested
        // `PopOpacityGroup { mask_pat_id: Some(...) }` routed through
        // `emit_isolated_tiling_ops` (depth == 1). Pixels inside the triangle
        // must pass through the mask; pixels inside the triangle's bounding box
        // but outside the triangle itself must be masked out.
        let mut inner_cell = DisplayList::new(Rect::new(0.0, 0.0, 40.0, 40.0));
        inner_cell.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 40.0, 40.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        inner_cell.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Multiply,
            isolated: true,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 40.0, 40.0),
            alpha: 1.0,
            mask: None,
        });
        inner_cell.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 40.0, 40.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        inner_cell.push(RenderCommand::PopBlendGroup);
        let inner_tiling = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(inner_cell),
            bbox: Rect::new(0.0, 0.0, 40.0, 40.0),
            x_step: 40.0,
            y_step: 40.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });

        // Outer cell: lower-left triangle (0,0)-(40,0)-(0,40) painted with the
        // two-segment inner pattern at 100% alpha.
        let mut tri = Path::new();
        tri.move_to(zpdf_core::Point::new(0.0, 0.0));
        tri.line_to(zpdf_core::Point::new(40.0, 0.0));
        tri.line_to(zpdf_core::Point::new(0.0, 40.0));
        tri.close();

        let mut outer_cell = DisplayList::new(Rect::new(0.0, 0.0, 40.0, 40.0));
        outer_cell.push(RenderCommand::FillPath {
            path: tri,
            rule: FillRule::NonZero,
            paint: Paint::Tiling(inner_tiling),
            alpha: 1.0,
            overprint: None,
        });
        let outer_tiling = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(outer_cell),
            bbox: Rect::new(0.0, 0.0, 40.0, 40.0),
            x_step: 40.0,
            y_step: 40.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });

        let mut mask_dl = DisplayList::new(Rect::new(0.0, 0.0, 40.0, 40.0));
        mask_dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 40.0, 40.0),
            rule: FillRule::NonZero,
            paint: Paint::Tiling(outer_tiling),
            alpha: 1.0,
            overprint: None,
        });
        let soft_mask = SoftMask {
            commands: std::sync::Arc::new(mask_dl),
            kind: SoftMaskKind::Alpha,
            offset: (0.0, 0.0),
            backdrop_luma: 0.0,
            transfer: None,
        };

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 40.0, 40.0));
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Normal,
            isolated: true,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 40.0, 40.0),
            alpha: 1.0,
            mask: Some(soft_mask),
        });
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 40.0, 40.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PopBlendGroup);

        let pixmap = render_svg_resvg(&dl, &SvgOptions::default(), 40, 40);
        // PDF (8, 8) -> SVG (8, 32): strictly INSIDE triangle (x + y = 16 < 40) -> red!
        let inside = pixmap.pixel(8, 32).expect("in bounds");
        assert!(
            inside.red() > 240 && inside.green() < 15 && inside.blue() < 15,
            "pixel inside nested soft-mask triangle must be red, got {:?}",
            [inside.red(), inside.green(), inside.blue()]
        );
        // PDF (32, 32) -> SVG (32, 8): inside [0..40]×[0..40] bbox but OUTSIDE
        // triangle (x + y = 64 > 40) -> must stay pure white, NOT exposed red!
        let outside = pixmap.pixel(32, 8).expect("in bounds");
        assert!(
            outside.red() > 245 && outside.green() > 245 && outside.blue() > 245,
            "pixel inside triangle bbox but outside triangle must stay masked white, got {:?}",
            [outside.red(), outside.green(), outside.blue()]
        );
    }

    #[test]
    fn six_overlapping_dense_full_page_fills_preserve_live_backdrop_past_fanout_cap() {
        // On an opaque Letter page (612×792), 5 overlapping dense (>64-pass)
        // full-page fills exhaust `MAX_BACKDROP_FANOUT = 32` (`1 -> 2 -> 4 -> 8 -> 16 -> 32`).
        // The 6th overlapping full-page fill (> full-page filter primitive budget)
        // must keep the resource cap AND preserve the live backdrop instead of
        // falling back to an isolated group (which would blend Darken(0.6)
        // against transparent -> 0.6 and jump from 0.4 (~102) to 0.6 (~153)).
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 150.0, 150.0));
        // 36 alternating Darken(0.6) + Lighten(0.4) pairs = 72 passes (> 64 flat
        // threshold and > full-page Letter filter primitive budget of 138 prims).
        for _ in 0..36 {
            cell.push(RenderCommand::PushBlendGroup {
                blend_mode: BlendMode::Darken,
                isolated: true,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 150.0, 150.0),
                alpha: 1.0,
                mask: None,
            });
            cell.push(RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 150.0, 150.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::rgb(0.6, 0.6, 0.6)),
                alpha: 1.0,
                overprint: None,
            });
            cell.push(RenderCommand::PopBlendGroup);

            cell.push(RenderCommand::PushBlendGroup {
                blend_mode: BlendMode::Lighten,
                isolated: true,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 150.0, 150.0),
                alpha: 1.0,
                mask: None,
            });
            cell.push(RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 150.0, 150.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::rgb(0.4, 0.4, 0.4)),
                alpha: 1.0,
                overprint: None,
            });
            cell.push(RenderCommand::PopBlendGroup);
        }
        // Finish with one extra Darken(0.6) so isolated evaluation against
        // transparent would produce 0.6 (~153), whereas live-backdrop evaluation
        // against 0.4 produces min(0.4, 0.6) = 0.4 (~102)!
        cell.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Darken,
            isolated: true,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 150.0, 150.0),
            alpha: 1.0,
            mask: None,
        });
        cell.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 150.0, 150.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.6, 0.6, 0.6)),
            alpha: 1.0,
            overprint: None,
        });
        cell.push(RenderCommand::PopBlendGroup);

        let tiling = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 150.0, 150.0),
            x_step: 150.0,
            y_step: 150.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 612.0, 792.0));
        // Dark gray (0.25) opaque backdrop over the whole Letter page.
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 612.0, 792.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.25, 0.25, 0.25)),
            alpha: 1.0,
            overprint: None,
        });
        // 6 overlapping dense full-page fills!
        for _ in 0..6 {
            dl.push(RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 612.0, 792.0),
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
        // Fan-out cap must still limit `<g id="b...">` snapshots to <= 5 (2^5 = 32).
        let bg_snapshots = svg.matches("<g id=\"b").count();
        assert!(
            bg_snapshots <= 5,
            "backdrop snapshots must stay capped at MAX_BACKDROP_FANOUT (<= 5), got {bg_snapshots}"
        );
        // The 6th fill must NOT wrap in an isolated group.
        assert!(
            !svg.contains("isolation:isolate"),
            "6th overlapping dense fill must use live-backdrop direct fallback, not isolation:isolate"
        );

        // Render downscaled (61×79) with resvg to verify live-backdrop result (~0.4 = 102).
        let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default())
            .expect("valid SVG");
        let mut pixmap = resvg::tiny_skia::Pixmap::new(61, 79).expect("pixmap");
        let sx = 61.0 / tree.size().width();
        let sy = 79.0 / tree.size().height();
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(sx, sy),
            &mut pixmap.as_mut(),
        );
        let mid = pixmap.pixel(30, 40).expect("in bounds");
        let rgb = [mid.red(), mid.green(), mid.blue()];
        assert!(
            rgb.iter().all(|&c| (96..=108).contains(&c)),
            "6th overlapping dense full-page fill must stay at ~0.4 (~102), NOT jump to isolated 0.6 (~153), got {rgb:?}"
        );
    }

    #[test]
    fn dense_fallback_applies_fractional_opacity_once_and_does_not_duplicate_path() {
        // On a white Letter page (612×792), 5 overlapping dense full-page fills
        // exhaust `MAX_BACKDROP_FANOUT = 32`. The 6th fill is a 50%-opaque fill
        // whose cell alternates 36 pairs of Darken(black) + Lighten(black) (72 passes)
        // over a detailed 500-vertex boundary path.
        // 6th fill: 50%-opaque fill with 70 alternating Darken(black) + Lighten(0.01)
        // pairs = 140 non-identity passes (> full-page filter budget of 138 prims)
        // over a detailed 500-vertex boundary path.
        // - Per-pass opacity compounding would turn white into near-black (0.5^70 ≈ 0).
        // - Applying opacity ONCE to the completed blend must yield ~50% gray (~128).
        // - The detailed path must NOT be duplicated across the 140 passes.
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 150.0, 150.0));
        for _ in 0..70 {
            cell.push(RenderCommand::PushBlendGroup {
                blend_mode: BlendMode::Darken,
                isolated: true,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 150.0, 150.0),
                alpha: 1.0,
                mask: None,
            });
            cell.push(RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 150.0, 150.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::black()),
                alpha: 1.0,
                overprint: None,
            });
            cell.push(RenderCommand::PopBlendGroup);

            cell.push(RenderCommand::PushBlendGroup {
                blend_mode: BlendMode::Lighten,
                isolated: true,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 150.0, 150.0),
                alpha: 1.0,
                mask: None,
            });
            cell.push(RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 150.0, 150.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::rgb(0.01, 0.01, 0.01)),
                alpha: 1.0,
                overprint: None,
            });
            cell.push(RenderCommand::PopBlendGroup);
        }
        let dark_light_black = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 150.0, 150.0),
            x_step: 150.0,
            y_step: 150.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });

        // 72 non-identity near-white Darken(0.99)/Lighten(1.0) passes to genuinely
        // exhaust MAX_BACKDROP_FANOUT = 32 across 5 overlapping full-page fills while
        // leaving the Letter page white before the 6th fill.
        let mut white_cell = DisplayList::new(Rect::new(0.0, 0.0, 150.0, 150.0));
        for _ in 0..36 {
            white_cell.push(RenderCommand::PushBlendGroup {
                blend_mode: BlendMode::Darken,
                isolated: true,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 150.0, 150.0),
                alpha: 1.0,
                mask: None,
            });
            white_cell.push(RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 150.0, 150.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::rgb(0.99, 0.99, 0.99)),
                alpha: 1.0,
                overprint: None,
            });
            white_cell.push(RenderCommand::PopBlendGroup);

            white_cell.push(RenderCommand::PushBlendGroup {
                blend_mode: BlendMode::Lighten,
                isolated: true,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 150.0, 150.0),
                alpha: 1.0,
                mask: None,
            });
            white_cell.push(RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 150.0, 150.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::white()),
                alpha: 1.0,
                overprint: None,
            });
            white_cell.push(RenderCommand::PopBlendGroup);
        }
        let white_dense = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(white_cell),
            bbox: Rect::new(0.0, 0.0, 150.0, 150.0),
            x_step: 150.0,
            y_step: 150.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });

        // Build a detailed 500-vertex boundary covering the interior of the page.
        let mut detailed = Path::new();
        detailed.move_to(Point::new(20.0, 20.0));
        for i in 1..=250 {
            let x = 20.0 + (i as f64 / 250.0) * 572.0;
            let y = 20.0 + if i % 2 == 0 { 0.0 } else { 2.0 };
            detailed.line_to(Point::new(x, y));
        }
        for i in (0..=250).rev() {
            let x = 20.0 + (i as f64 / 250.0) * 572.0;
            let y = 772.0 - if i % 2 == 0 { 0.0 } else { 2.0 };
            detailed.line_to(Point::new(x, y));
        }
        detailed.close();

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 612.0, 792.0));
        // 5 overlapping dense fills in [20..80]×[20..80] exhaust MAX_BACKDROP_FANOUT = 32.
        for _ in 0..5 {
            dl.push(RenderCommand::FillPath {
                path: rect_path(20.0, 20.0, 80.0, 80.0),
                rule: FillRule::NonZero,
                paint: Paint::Tiling(std::sync::Arc::clone(&white_dense)),
                alpha: 1.0,
                overprint: None,
            });
        }
        dl.push(RenderCommand::FillPath {
            path: detailed,
            rule: FillRule::NonZero,
            paint: Paint::Tiling(dark_light_black),
            alpha: 0.5,
            overprint: None,
        });

        let svg = display_list_to_svg(
            &dl,
            &FontCache::new(),
            &ImageCache::new(),
            &SvgOptions::default(),
        );
        // Must hit the bounded raster overlay fallback (not repeat the 500-vertex path 140 times).
        assert!(
            svg.contains("href=\"data:image/png;base64,"),
            "6th overlapping dense fill past MAX_BACKDROP_FANOUT must emit bounded raster overlay"
        );
        assert!(
            svg.len() < 150_000,
            "dense fallback must not duplicate detailed path across 140 passes (svg len = {})",
            svg.len()
        );
        assert!(
            !svg.contains("isolation:isolate"),
            "fallback must not isolate against transparent"
        );

        let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default())
            .expect("valid SVG");
        let mut pixmap = resvg::tiny_skia::Pixmap::new(61, 79).expect("pixmap");
        let sx = 61.0 / tree.size().width();
        let sy = 79.0 / tree.size().height();
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(sx, sy),
            &mut pixmap.as_mut(),
        );
        let mid = pixmap.pixel(30, 40).expect("in bounds");
        let rgb = [mid.red(), mid.green(), mid.blue()];
        assert!(
            rgb.iter().all(|&c| (120..=136).contains(&c)),
            "50%-opaque alternating Darken(black)/Lighten(black) over white must produce ~50% gray (~128), not compounded black, got {rgb:?}"
        );
        // Margin outside the detailed path (x=5, y=5) must stay pure white.
        let margin = pixmap.pixel(1, 1).expect("in bounds");
        assert!(
            margin.red() > 245 && margin.green() > 245 && margin.blue() > 245,
            "margin outside detailed path must stay white, got {:?}",
            [margin.red(), margin.green(), margin.blue()]
        );
    }

    #[test]
    fn svg_transparent_fallback_preserves_alpha_when_rgb_changes_at_equal_alpha() {
        let rect_path = |x0, y0, x1, y1| {
            let mut p = Path::new();
            p.rect(Rect::new(x0, y0, x1, y1));
            p
        };
        // Dense pattern (74 alternating Darken(white)/Lighten(white) passes > 64):
        // at fill alpha = 0.005 over premultiplied [0, 0, 0, 204], full-coverage
        // Lighten(white) yields [255, 255, 255, 255], and 0.5% mix takes
        // [0, 0, 0, 204] -> [1, 1, 1, 204] (color changes while alpha stays 204).
        let mut faint_cell = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        for _ in 0..37 {
            faint_cell.push(RenderCommand::PushBlendGroup {
                blend_mode: BlendMode::Darken,
                isolated: false,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 20.0, 20.0),
                alpha: 1.0,
                mask: None,
            });
            faint_cell.push(RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 20.0, 20.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::white()),
                alpha: 1.0,
                overprint: None,
            });
            faint_cell.push(RenderCommand::PopBlendGroup);
            faint_cell.push(RenderCommand::PushBlendGroup {
                blend_mode: BlendMode::Lighten,
                isolated: false,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 20.0, 20.0),
                alpha: 1.0,
                mask: None,
            });
            faint_cell.push(RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 20.0, 20.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::white()),
                alpha: 1.0,
                overprint: None,
            });
            faint_cell.push(RenderCommand::PopBlendGroup);
        }
        let faint_dense = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(faint_cell),
            bbox: Rect::new(0.0, 0.0, 20.0, 20.0),
            x_step: 20.0,
            y_step: 20.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });

        // Direct unit check on svg_compute_incremental_overlay: [0,0,0,204] -> [1,1,1,204]
        // must return None instead of baking opaque white [52,52,52,255].
        assert!(
            svg_compute_incremental_overlay(&[0, 0, 0, 204], &[1, 1, 1, 204]).is_none(),
            "unchanged translucent alpha with RGB change must not bake opaque white"
        );

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 400.0, 400.0));
        // 80%-opaque black backdrop over transparent SVG canvas -> [0, 0, 0, 204].
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 400.0, 400.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::black()),
            alpha: 0.8,
            overprint: None,
        });
        // Three small overlapping fills on [10..20]² exhaust MAX_BACKDROP_FANOUT
        // (1 -> 3 -> 9 -> 27) via `emit_flat_blend_group_tiling_ops` (74 > 64 ops).
        for _ in 0..3 {
            dl.push(RenderCommand::FillPath {
                path: rect_path(10.0, 10.0, 20.0, 20.0),
                rule: FillRule::NonZero,
                paint: Paint::Tiling(std::sync::Arc::clone(&faint_dense)),
                alpha: 0.005,
                overprint: None,
            });
        }
        // 4th fill on [40..380]² (prim_limit = 141 < 153) exceeds MAX_BACKDROP_FANOUT
        // and enters `emit_raster_backdrop_tiling_fallback`, taking [0,0,0,204] -> [1,1,1,204].
        dl.push(RenderCommand::FillPath {
            path: rect_path(40.0, 40.0, 380.0, 380.0),
            rule: FillRule::NonZero,
            paint: Paint::Tiling(faint_dense),
            alpha: 0.005,
            overprint: None,
        });

        let opts = SvgOptions {
            background: None,
            budget: None,
        };
        let svg = display_list_to_svg(&dl, &FontCache::new(), &ImageCache::new(), &opts);
        assert!(
            svg.contains("href=\"data:image/png;base64,"),
            "expected raster fallback PNG on translucent canvas"
        );
        let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default())
            .expect("valid SVG");
        let mut pixmap = resvg::tiny_skia::Pixmap::new(100, 100).expect("pixmap");
        let s = 100.0 / tree.size().width();
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(s, s),
            &mut pixmap.as_mut(),
        );
        // Inside [40..380]² (scaled to [10..95]²): must keep alpha ~204 and dark RGB ~1
        // (NEVER opaque [52, 52, 52, 255]).
        let inside = pixmap.pixel(50, 50).expect("in bounds");
        // Untouched corner PDF [0..40]×[360..400] -> SVG [0..40]×[0..40] -> pixel (2, 2).
        let outside = pixmap.pixel(2, 2).expect("in bounds");
        assert!(
            (200..=208).contains(&inside.alpha()) && (1..=5).contains(&inside.red()),
            "transparent SVG fallback must preserve ~204 alpha and faint mark ~[1,1,1,204] (not opaque ~52), got {:?}",
            [inside.red(), inside.green(), inside.blue(), inside.alpha()]
        );
        assert!(
            (200..=208).contains(&outside.alpha()) && outside.red() == 0,
            "surrounding vector backdrop must stay ~204 alpha black, got {:?}",
            [
                outside.red(),
                outside.green(),
                outside.blue(),
                outside.alpha()
            ]
        );
    }

    #[test]
    fn svg_unclipped_type3_fallback_preserves_earlier_vector_content() {
        let rect_path = |x0, y0, x1, y1| {
            let mut p = Path::new();
            p.rect(Rect::new(x0, y0, x1, y1));
            p
        };
        let charproc =
            b"1 0 0 0 1 1 d1 BI /W 8 /H 8 /BPC 1 /IM true ID \x00\x00\x00\x00\x00\x00\x00\x00 EI";
        let font = LoadedFont::new_with_data(
            zpdf_font::PdfFontType::Type3 {
                font_matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
                char_procs: HashMap::from([(
                    "A".into(),
                    std::sync::Arc::<[u8]>::from(&charproc[..]),
                )]),
                encoding: vec!["A".into()],
                widths: vec![1.0],
                first_char: 0,
            },
            "Type3KeepVectors".into(),
            Vec::new(),
            zpdf_font::CidWidths::new(1.0),
        );
        let mut fonts = FontCache::new();
        let font_id = fonts.insert("F1".into(), font);

        let mut dense_cell = DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0));
        for _ in 0..70 {
            dense_cell.push(RenderCommand::PushBlendGroup {
                blend_mode: BlendMode::Darken,
                isolated: false,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
                alpha: 1.0,
                mask: None,
            });
            dense_cell.push(RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 10.0, 10.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
                alpha: 1.0,
                overprint: None,
            });
            dense_cell.push(RenderCommand::PopBlendGroup);
            dense_cell.push(RenderCommand::PushBlendGroup {
                blend_mode: BlendMode::Lighten,
                isolated: false,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
                alpha: 1.0,
                mask: None,
            });
            dense_cell.push(RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 10.0, 10.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::black()),
                alpha: 1.0,
                overprint: None,
            });
            dense_cell.push(RenderCommand::PopBlendGroup);
        }
        let dense_red = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(dense_cell),
            bbox: Rect::new(0.0, 0.0, 10.0, 10.0),
            x_step: 10.0,
            y_step: 10.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 200.0, 200.0));
        // Distinct earlier vector artwork at [10..50]×[10..50].
        dl.push(RenderCommand::FillPath {
            path: rect_path(10.0, 10.0, 50.0, 50.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.5, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        // Exhaust MAX_BACKDROP_FANOUT on overlapping fills at [100..150]×[100..150].
        for _ in 0..5 {
            dl.push(RenderCommand::FillPath {
                path: rect_path(100.0, 100.0, 150.0, 150.0),
                rule: FillRule::NonZero,
                paint: Paint::Tiling(std::sync::Arc::clone(&dense_red)),
                alpha: 1.0,
                overprint: None,
            });
        }
        // Small unclipped Type 3 glyph at [160..176]×[160..176] hitting raster fallback.
        dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
            font_id,
            font_size: 16.0,
            glyphs: vec![PositionedGlyph {
                glyph_id: 0,
                code_len: 1,
                code: b'A' as u32,
                x: 0.0,
                y: 0.0,
                advance: 16.0,
            }],
            paint: Paint::Tiling(dense_red),
            alpha: 1.0,
            overprint: None,
            transform: zpdf_core::Matrix::translate(160.0, 160.0),
            h_scale: 1.0,
        }));

        let svg = display_list_to_svg(&dl, &fonts, &ImageCache::new(), &SvgOptions::default());
        assert!(
            svg.contains("fill=\"#0080ff\""),
            "small unclipped Type 3 glyph fallback must NOT wipe earlier vector artwork from the SVG"
        );
        assert!(
            svg.contains("href=\"data:image/png;base64,"),
            "Type 3 glyph past MAX_BACKDROP_FANOUT must emit bounded raster overlay"
        );
    }

    #[test]
    fn svg_isolated_child_inside_non_isolated_multiply_preserves_fallback_mark() {
        let rect_path = |x0, y0, x1, y1| {
            let mut p = Path::new();
            p.rect(Rect::new(x0, y0, x1, y1));
            p
        };
        let make_dense = |darken_color: Color| {
            let mut dense_cell = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
            for _ in 0..37 {
                dense_cell.push(RenderCommand::PushBlendGroup {
                    blend_mode: BlendMode::Darken,
                    isolated: false,
                    knockout: false,
                    bounds: Rect::new(0.0, 0.0, 20.0, 20.0),
                    alpha: 1.0,
                    mask: None,
                });
                dense_cell.push(RenderCommand::FillPath {
                    path: rect_path(0.0, 0.0, 20.0, 20.0),
                    rule: FillRule::NonZero,
                    paint: Paint::Solid(darken_color),
                    alpha: 1.0,
                    overprint: None,
                });
                dense_cell.push(RenderCommand::PopBlendGroup);
                dense_cell.push(RenderCommand::PushBlendGroup {
                    blend_mode: BlendMode::Lighten,
                    isolated: false,
                    knockout: false,
                    bounds: Rect::new(0.0, 0.0, 20.0, 20.0),
                    alpha: 1.0,
                    mask: None,
                });
                dense_cell.push(RenderCommand::FillPath {
                    path: rect_path(0.0, 0.0, 20.0, 20.0),
                    rule: FillRule::NonZero,
                    paint: Paint::Solid(Color::black()),
                    alpha: 1.0,
                    overprint: None,
                });
                dense_cell.push(RenderCommand::PopBlendGroup);
            }
            std::sync::Arc::new(zpdf_display_list::TilingPattern {
                commands: std::sync::Arc::new(dense_cell),
                bbox: Rect::new(0.0, 0.0, 20.0, 20.0),
                x_step: 20.0,
                y_step: 20.0,
                matrix: zpdf_core::Matrix::identity(),
                tiling_type: 1,
            })
        };
        let dense_white = make_dense(Color::white());
        let dense_cyan = make_dense(Color::rgb(0.0, 1.0, 1.0));

        // 400×400 page: outer non-isolated Multiply fits in `NonIsolatedFilterGroup`
        // (prim_limit = 104 >= 7). Inside the isolated child, 3 small overlapping
        // fills on [10..20]² exhaust MAX_BACKDROP_FANOUT (1 -> 3 -> 9 -> 27) via
        // `emit_flat_blend_group_tiling_ops` (74 > 64 ops), and fill 4 on [40..380]²
        // (prim_limit = 141 < 153) MUST enter `emit_raster_backdrop_tiling_fallback`
        // and paint cyan (never silently omitted while outer CPU group is deferred).
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 400.0, 400.0));
        // Yellow outer backdrop [255, 255, 0].
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 400.0, 400.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 1.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        // Non-isolated Multiply outer group (causes CpuRenderer to enter recording_non_isolated).
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Multiply,
            isolated: false,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 400.0, 400.0),
            alpha: 1.0,
            mask: None,
        });
        // Isolated Normal child inside the non-isolated Multiply group.
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Normal,
            isolated: true,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 400.0, 400.0),
            alpha: 1.0,
            mask: None,
        });
        // White fill inside isolated child so Darken(cyan) yields cyan inside the child.
        dl.push(RenderCommand::FillPath {
            path: rect_path(40.0, 40.0, 380.0, 380.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        // Exhaust MAX_BACKDROP_FANOUT inside the isolated child using small fills.
        for _ in 0..3 {
            dl.push(RenderCommand::FillPath {
                path: rect_path(10.0, 10.0, 20.0, 20.0),
                rule: FillRule::NonZero,
                paint: Paint::Tiling(std::sync::Arc::clone(&dense_white)),
                alpha: 1.0,
                overprint: None,
            });
        }
        // 4th fill hits raster fallback inside the isolated child and turns [40..380]² cyan.
        dl.push(RenderCommand::FillPath {
            path: rect_path(40.0, 40.0, 380.0, 380.0),
            rule: FillRule::NonZero,
            paint: Paint::Tiling(dense_cyan),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PopBlendGroup);
        dl.push(RenderCommand::PopBlendGroup);

        let svg = display_list_to_svg(
            &dl,
            &FontCache::new(),
            &ImageCache::new(),
            &SvgOptions {
                background: Some(Color::white()),
                budget: None,
            },
        );
        assert!(
            svg.contains("href=\"data:image/png;base64,"),
            "4th dense fill inside isolated child must exercise raster fallback"
        );
        let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default())
            .expect("valid SVG");
        let mut pixmap = resvg::tiny_skia::Pixmap::new(100, 100).expect("pixmap");
        let s = 100.0 / tree.size().width();
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(s, s),
            &mut pixmap.as_mut(),
        );
        // Inside [20..380]²: cyan [0,255,255] Multiply over yellow [255,255,0] = green [0,255,0].
        // If the 4th fill had been omitted while the outer Multiply group was deferred,
        // the isolated child would have stayed white and the pixel would be yellow [255,255,0].
        let mid = pixmap.pixel(50, 50).expect("in bounds");
        let rgb = [mid.red(), mid.green(), mid.blue()];
        assert!(
            rgb[0] < 15 && rgb[1] > 240 && rgb[2] < 15,
            "fallback mark inside isolated child of non-isolated Multiply group must produce green [0,255,0], not omitted yellow, got {rgb:?}"
        );
    }

    #[test]
    fn svg_affine_filter_over_budget_uses_live_backdrop_instead_of_transparent_seed() {
        let rect_path = |x0, y0, x1, y1| {
            let mut p = Path::new();
            p.rect(Rect::new(x0, y0, x1, y1));
            p
        };
        // Six overlapping copies (bbox 60×10, step 10×10) × 176 alternating white Multiply
        // and black Screen marks -> 1056 > MAX_SVG_TILING_BLEND_PASSES (1024), selecting
        // the 3-seed affine plan. On a 1200×1200 page, max_filter_primitives = 11 < 14,
        // so enqueue fails and must use the live CPU backdrop (keeping opaque blue unchanged)
        // instead of painting the opaque-white transparent seed over blue.
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 60.0, 10.0));
        for _ in 0..88 {
            cell.push(RenderCommand::PushBlendGroup {
                blend_mode: BlendMode::Multiply,
                isolated: false,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 60.0, 10.0),
                alpha: 1.0,
                mask: None,
            });
            cell.push(RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 60.0, 10.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::white()),
                alpha: 1.0,
                overprint: None,
            });
            cell.push(RenderCommand::PopBlendGroup);
            cell.push(RenderCommand::PushBlendGroup {
                blend_mode: BlendMode::Screen,
                isolated: false,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 60.0, 10.0),
                alpha: 1.0,
                mask: None,
            });
            cell.push(RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 60.0, 10.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::black()),
                alpha: 1.0,
                overprint: None,
            });
            cell.push(RenderCommand::PopBlendGroup);
        }
        let tiling = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 60.0, 10.0),
            x_step: 10.0,
            y_step: 10.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 1200.0, 1200.0));
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 1200.0, 1200.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 1200.0, 1200.0),
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
        let mut pixmap = resvg::tiny_skia::Pixmap::new(60, 60).expect("pixmap");
        let s = 60.0 / tree.size().width();
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(s, s),
            &mut pixmap.as_mut(),
        );
        let mid = pixmap.pixel(30, 30).expect("in bounds");
        let rgb = [mid.red(), mid.green(), mid.blue()];
        assert!(
            rgb[0] < 15 && rgb[1] < 15 && rgb[2] > 240,
            "over-budget affine filter on 1200x1200 blue page must keep blue via CPU backdrop, not paint white seed, got {rgb:?}"
        );
    }

    #[test]
    fn svg_non_isolated_group_reserves_close_primitives_within_budget() {
        let rect_path = |x0, y0, x1, y1| {
            let mut p = Path::new();
            p.rect(Rect::new(x0, y0, x1, y1));
            p
        };
        // On a 2048×2048 page, max_filter_primitives = 4. A non-isolated Normal
        // 50%-opacity group containing a solid fill needs 6 primitives (2 for the
        // trailing overlay + 4 for PopNonIsolatedGroup), so it must NOT emit a
        // filter exceeding the 4-primitive budget.
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 2048.0, 2048.0));
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Normal,
            isolated: false,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 2048.0, 2048.0),
            alpha: 0.5,
            mask: None,
        });
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 2048.0, 2048.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PopBlendGroup);

        let svg = display_list_to_svg(
            &dl,
            &FontCache::new(),
            &ImageCache::new(),
            &SvgOptions::default(),
        );
        assert!(
            !svg.contains("<filter "),
            "2048x2048 non-isolated Normal group requiring 6 primitives must not emit a filter exceeding the 4-primitive budget"
        );
        let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default())
            .expect("valid SVG");
        let mut pixmap = resvg::tiny_skia::Pixmap::new(32, 32).expect("pixmap");
        let s = 32.0 / tree.size().width();
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(s, s),
            &mut pixmap.as_mut(),
        );
        let mid = pixmap.pixel(16, 16).expect("in bounds");
        let rgb = [mid.red(), mid.green(), mid.blue()];
        assert!(
            rgb[0] > 245 && (118..=138).contains(&rgb[1]) && (118..=138).contains(&rgb[2]),
            "50%-opaque red non-isolated group over white must render pink [255,~128,~128], got {rgb:?}"
        );
    }

    #[test]
    fn svg_clamp_raster_scale_honors_fallback_limits_without_floor() {
        let huge = Rect::new(0.0, 0.0, 40_000_000.0, 40_000_000.0);
        let scale = svg_clamp_raster_scale(huge, 1024, 1_048_576)
            .expect("40M×40M page fits 1024×1024 without 1e-4 floor");
        let (w, h) = PageRenderInfo {
            page_rect: huge,
            scale,
            background: Color::white(),
        }
        .raster_dimensions()
        .expect("valid raster dimensions");
        assert!(
            w <= 1024 && h <= 1024 && (w as u64) * (h as u64) <= 1_048_576,
            "raster dimensions must stay within 1024×1024 / 1M pixels, got {w}×{h} at scale {scale}"
        );
        // Unrepresentable scale (underflows f32 to 0) declines fallback cleanly.
        let absurd = Rect::new(0.0, 0.0, 1e45, 1e45);
        assert!(svg_clamp_raster_scale(absurd, 1024, 1_048_576).is_none());
    }

    #[test]
    fn svg_type3_d0_colored_image_preserves_rgb_under_tiling_pattern() {
        let d0_stream =
            b"1 0 d0 BI /W 2 /H 2 /CS /RGB /BPC 8 ID \xff\x00\x00\xff\x00\x00\xff\x00\x00\xff\x00\x00 EI";
        let d1_stream =
            b"1 0 0 0 1 1 d1 BI /W 8 /H 8 /BPC 1 /IM true ID \x00\x00\x00\x00\x00\x00\x00\x00 EI";
        let d0_vec_stream =
            b"1 0 d0 1 0 0 rg 0 0 0.5 1 re f 0 1 0 RG 0.3 w 0.75 0.1 m 0.75 0.9 l S";
        let font = LoadedFont::new_with_data(
            zpdf_font::PdfFontType::Type3 {
                font_matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
                char_procs: HashMap::from([
                    ("A".into(), std::sync::Arc::<[u8]>::from(&d0_stream[..])),
                    ("B".into(), std::sync::Arc::<[u8]>::from(&d1_stream[..])),
                    ("C".into(), std::sync::Arc::<[u8]>::from(&d0_vec_stream[..])),
                ]),
                encoding: vec!["A".into(), "B".into(), "C".into()],
                widths: vec![1.0, 1.0, 1.0],
                first_char: 0,
            },
            "Type3MixedSvg".into(),
            Vec::new(),
            zpdf_font::CidWidths::new(1.0),
        );
        let mut fonts = FontCache::new();
        let font_id = fonts.insert("F1".into(), font);

        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 8.0, 8.0));
        let mut r = Path::new();
        r.rect(Rect::new(0.0, 0.0, 8.0, 8.0));
        cell.push(RenderCommand::FillPath {
            path: r,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        let tiling = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 8.0, 8.0),
            x_step: 8.0,
            y_step: 8.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 80.0, 40.0));
        dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
            font_id,
            font_size: 20.0,
            glyphs: vec![
                PositionedGlyph {
                    glyph_id: 0,
                    code_len: 1,
                    code: b'A' as u32,
                    x: 0.0,
                    y: 0.0,
                    advance: 20.0,
                },
                PositionedGlyph {
                    glyph_id: 1,
                    code_len: 1,
                    code: b'B' as u32,
                    x: 20.0,
                    y: 0.0,
                    advance: 20.0,
                },
                PositionedGlyph {
                    glyph_id: 2,
                    code_len: 1,
                    code: b'C' as u32,
                    x: 40.0,
                    y: 0.0,
                    advance: 20.0,
                },
            ],
            paint: Paint::Tiling(tiling),
            alpha: 1.0,
            overprint: None,
            transform: zpdf_core::Matrix::translate(8.0, 10.0),
            h_scale: 1.0,
        }));

        let svg = display_list_to_svg(&dl, &fonts, &ImageCache::new(), &SvgOptions::default());
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
        let a = pixmap.pixel(18, 20).expect("in bounds");
        let b = pixmap.pixel(38, 20).expect("in bounds");
        let c_fill = pixmap.pixel(53, 20).expect("in bounds");
        let c_stroke = pixmap.pixel(63, 20).expect("in bounds");
        assert!(
            a.red() > 240 && a.green() < 15 && a.blue() < 15,
            "SVG d0 colored RGB Type 3 image must stay red under blue tiling pattern, got {:?}",
            [a.red(), a.green(), a.blue()]
        );
        assert!(
            b.red() < 15 && b.green() < 15 && b.blue() > 240,
            "SVG d1 stencil Type 3 glyph must inherit blue tiling pattern, got {:?}",
            [b.red(), b.green(), b.blue()]
        );
        assert!(
            c_fill.red() > 240 && c_fill.green() < 15 && c_fill.blue() < 15,
            "SVG d0 explicit red vector fill must stay red under blue tiling pattern, got {:?}",
            [c_fill.red(), c_fill.green(), c_fill.blue()]
        );
        assert!(
            c_stroke.red() < 15 && c_stroke.green() > 240 && c_stroke.blue() < 15,
            "SVG d0 explicit green vector stroke must stay green under blue tiling pattern, got {:?}",
            [c_stroke.red(), c_stroke.green(), c_stroke.blue()]
        );
    }

    #[test]
    fn svg_type3_glyph_local_clip_restores_before_second_draw() {
        let clip_stream = b"1 0 0 0 1 1 d1 \
            q 0 0 0.4 1 re W n \
            BI /W 8 /H 8 /BPC 1 /IM true ID \x00\x00\x00\x00\x00\x00\x00\x00 EI \
            Q 0.7 0 0.3 1 re f";
        let font = LoadedFont::new_with_data(
            zpdf_font::PdfFontType::Type3 {
                font_matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
                char_procs: HashMap::from([(
                    "A".into(),
                    std::sync::Arc::<[u8]>::from(&clip_stream[..]),
                )]),
                encoding: vec!["A".into()],
                widths: vec![1.0],
                first_char: 0,
            },
            "Type3ClipSvg".into(),
            Vec::new(),
            zpdf_font::CidWidths::new(1.0),
        );
        let mut fonts = FontCache::new();
        let font_id = fonts.insert("F1".into(), font);

        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 8.0, 8.0));
        let mut r = Path::new();
        r.rect(Rect::new(0.0, 0.0, 8.0, 8.0));
        cell.push(RenderCommand::FillPath {
            path: r,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        let tiling = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 8.0, 8.0),
            x_step: 8.0,
            y_step: 8.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });

        for (label, paint) in [
            ("solid", Paint::Solid(Color::rgb(0.0, 0.0, 1.0))),
            ("tiling", Paint::Tiling(tiling)),
        ] {
            let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 60.0, 40.0));
            dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
                font_id,
                font_size: 40.0,
                glyphs: vec![PositionedGlyph {
                    glyph_id: 0,
                    code_len: 1,
                    code: b'A' as u32,
                    x: 0.0,
                    y: 0.0,
                    advance: 40.0,
                }],
                paint,
                alpha: 1.0,
                overprint: None,
                transform: zpdf_core::Matrix::translate(10.0, 0.0),
                h_scale: 1.0,
            }));
            let svg = display_list_to_svg(&dl, &fonts, &ImageCache::new(), &SvgOptions::default());
            let tree =
                resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default())
                    .expect("valid SVG");
            let mut pixmap = resvg::tiny_skia::Pixmap::new(60, 40).expect("pixmap");
            let sx = 60.0 / tree.size().width();
            let sy = 40.0 / tree.size().height();
            resvg::render(
                &tree,
                resvg::tiny_skia::Transform::from_scale(sx, sy),
                &mut pixmap.as_mut(),
            );
            let left = pixmap.pixel(18, 20).expect("in bounds");
            let gap = pixmap.pixel(32, 20).expect("in bounds");
            let right = pixmap.pixel(44, 20).expect("in bounds");
            assert!(
                left.red() < 15 && left.green() < 15 && left.blue() > 240,
                "SVG ({label}) clipped left half must be blue, got {:?}",
                [left.red(), left.green(), left.blue()]
            );
            assert_eq!(
                [gap.red(), gap.green(), gap.blue()],
                [255, 255, 255],
                "SVG ({label}) clipped-out middle gap must stay white"
            );
            assert!(
                right.red() < 15 && right.green() < 15 && right.blue() > 240,
                "SVG ({label}) second draw after Q must be blue, got {:?}",
                [right.red(), right.green(), right.blue()]
            );
        }
    }

    #[test]
    fn svg_mixed_type3_run_over_budget_affine_uses_live_backdrop() {
        let rect_path = |x0, y0, x1, y1| {
            let mut p = Path::new();
            p.rect(Rect::new(x0, y0, x1, y1));
            p
        };
        // Same over-budget 3-seed affine tiling pattern on a 1200×1200 opaque blue page:
        // alternating white Multiply + black Screen is an exact identity on opaque blue.
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 60.0, 10.0));
        for _ in 0..88 {
            cell.push(RenderCommand::PushBlendGroup {
                blend_mode: BlendMode::Multiply,
                isolated: false,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 60.0, 10.0),
                alpha: 1.0,
                mask: None,
            });
            cell.push(RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 60.0, 10.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::white()),
                alpha: 1.0,
                overprint: None,
            });
            cell.push(RenderCommand::PopBlendGroup);
            cell.push(RenderCommand::PushBlendGroup {
                blend_mode: BlendMode::Screen,
                isolated: false,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 60.0, 10.0),
                alpha: 1.0,
                mask: None,
            });
            cell.push(RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 60.0, 10.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::black()),
                alpha: 1.0,
                overprint: None,
            });
            cell.push(RenderCommand::PopBlendGroup);
        }
        let tiling = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 60.0, 10.0),
            x_step: 10.0,
            y_step: 10.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });

        // Mixed Type 3 run: 'A' is a `d0` red RGB image on the left [0..0.25],
        // 'B' is a `d1` full-page inherited shape [0.25..1.0].
        let d0_img =
            b"1 0 d0 0.25 0 0 1 0 0 cm BI /W 2 /H 2 /CS /RGB /BPC 8 ID \xff\x00\x00\xff\x00\x00\xff\x00\x00\xff\x00\x00 EI";
        let d1_box = b"1 0 0 0 1 1 d1 0.25 0 0.75 1 re f";
        let font = LoadedFont::new_with_data(
            zpdf_font::PdfFontType::Type3 {
                font_matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
                char_procs: HashMap::from([
                    ("A".into(), std::sync::Arc::<[u8]>::from(&d0_img[..])),
                    ("B".into(), std::sync::Arc::<[u8]>::from(&d1_box[..])),
                ]),
                encoding: vec!["A".into(), "B".into()],
                widths: vec![1.0, 1.0],
                first_char: 0,
            },
            "Type3MixedAffineLiveBackdrop".into(),
            Vec::new(),
            zpdf_font::CidWidths::new(1.0),
        );
        let mut fonts = FontCache::new();
        let font_id = fonts.insert("F1".into(), font);

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 1200.0, 1200.0));
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 1200.0, 1200.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
            font_id,
            font_size: 1200.0,
            glyphs: vec![
                PositionedGlyph {
                    glyph_id: 0,
                    code_len: 1,
                    code: b'A' as u32,
                    x: 0.0,
                    y: 0.0,
                    advance: 0.0,
                },
                PositionedGlyph {
                    glyph_id: 1,
                    code_len: 1,
                    code: b'B' as u32,
                    x: 0.0,
                    y: 0.0,
                    advance: 0.0,
                },
            ],
            paint: Paint::Tiling(tiling),
            alpha: 1.0,
            overprint: None,
            transform: zpdf_core::Matrix::identity(),
            h_scale: 1.0,
        }));

        let svg = display_list_to_svg(&dl, &fonts, &ImageCache::new(), &SvgOptions::default());
        let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default())
            .expect("valid SVG");
        let mut pixmap = resvg::tiny_skia::Pixmap::new(60, 60).expect("pixmap");
        let s = 60.0 / tree.size().width();
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(s, s),
            &mut pixmap.as_mut(),
        );
        let red_img = pixmap.pixel(7, 30).expect("in bounds");
        let blue_pat = pixmap.pixel(40, 30).expect("in bounds");
        assert!(
            red_img.red() > 240 && red_img.green() < 15 && red_img.blue() < 15,
            "mixed Type 3 run fallback must preserve d0 red RGB image, got {:?}",
            [red_img.red(), red_img.green(), red_img.blue()]
        );
        assert!(
            blue_pat.red() < 15 && blue_pat.green() < 15 && blue_pat.blue() > 240,
            "mixed Type 3 run over-budget affine glyph must keep live blue backdrop instead of transparent white seed, got {:?}",
            [blue_pat.red(), blue_pat.green(), blue_pat.blue()]
        );
    }

    #[test]
    fn svg_type3_named_resource_stencil_do_renders_with_tiling() {
        let mut img_dict = zpdf_core::PdfDict::new();
        img_dict.insert(
            zpdf_core::PdfName::new("Subtype"),
            zpdf_core::PdfObject::Name(zpdf_core::PdfName::new("Image")),
        );
        img_dict.insert(
            zpdf_core::PdfName::new("Width"),
            zpdf_core::PdfObject::Integer(8),
        );
        img_dict.insert(
            zpdf_core::PdfName::new("Height"),
            zpdf_core::PdfObject::Integer(8),
        );
        img_dict.insert(
            zpdf_core::PdfName::new("BitsPerComponent"),
            zpdf_core::PdfObject::Integer(1),
        );
        img_dict.insert(
            zpdf_core::PdfName::new("ImageMask"),
            zpdf_core::PdfObject::Bool(true),
        );
        let mut resources = zpdf_font::Type3Resources::default();
        resources.xobjects.insert(
            "Ink".into(),
            zpdf_font::Type3XObject::Image(std::sync::Arc::new(zpdf_font::Type3ImageXObject {
                dict: img_dict,
                data: std::sync::Arc::from([0u8; 8].as_slice()),
                smask: None,
                mask_stream: None,
            })),
        );

        let inline_stream =
            b"1 0 0 0 1 1 d1 BI /W 8 /H 8 /BPC 1 /IM true ID \x00\x00\x00\x00\x00\x00\x00\x00 EI";
        let named_stream = b"1 0 0 0 1 1 d1 /Ink Do";
        let font = LoadedFont::new_with_data(
            zpdf_font::PdfFontType::Type3 {
                font_matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
                char_procs: HashMap::from([
                    ("A".into(), std::sync::Arc::<[u8]>::from(&inline_stream[..])),
                    ("B".into(), std::sync::Arc::<[u8]>::from(&named_stream[..])),
                ]),
                encoding: vec!["A".into(), "B".into()],
                widths: vec![1.0, 1.0],
                first_char: 0,
            },
            "Type3NamedSvg".into(),
            Vec::new(),
            zpdf_font::CidWidths::new(1.0),
        )
        .with_type3_resources(std::sync::Arc::new(resources));
        let mut fonts = FontCache::new();
        let font_id = fonts.insert("F1".into(), font);

        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 8.0, 8.0));
        let mut r = Path::new();
        r.rect(Rect::new(0.0, 0.0, 8.0, 8.0));
        cell.push(RenderCommand::FillPath {
            path: r,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        let tiling = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 8.0, 8.0),
            x_step: 8.0,
            y_step: 8.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 60.0, 40.0));
        dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
            font_id,
            font_size: 20.0,
            glyphs: vec![
                PositionedGlyph {
                    glyph_id: 0,
                    code_len: 1,
                    code: b'A' as u32,
                    x: 0.0,
                    y: 0.0,
                    advance: 20.0,
                },
                PositionedGlyph {
                    glyph_id: 1,
                    code_len: 1,
                    code: b'B' as u32,
                    x: 20.0,
                    y: 0.0,
                    advance: 20.0,
                },
            ],
            paint: Paint::Tiling(tiling),
            alpha: 1.0,
            overprint: None,
            transform: zpdf_core::Matrix::translate(8.0, 10.0),
            h_scale: 1.0,
        }));

        let svg = display_list_to_svg(&dl, &fonts, &ImageCache::new(), &SvgOptions::default());
        let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default())
            .expect("valid SVG");
        let mut pixmap = resvg::tiny_skia::Pixmap::new(60, 40).expect("pixmap");
        let sx = 60.0 / tree.size().width();
        let sy = 40.0 / tree.size().height();
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(sx, sy),
            &mut pixmap.as_mut(),
        );
        let a = pixmap.pixel(18, 20).expect("in bounds");
        let b = pixmap.pixel(38, 20).expect("in bounds");
        let bg = pixmap.pixel(4, 20).expect("in bounds");
        assert!(
            a.red() < 15 && a.green() < 15 && a.blue() > 240,
            "SVG inline stencil Type 3 glyph must be blue, got {:?}",
            [a.red(), a.green(), a.blue()]
        );
        assert!(
            b.red() < 15 && b.green() < 15 && b.blue() > 240,
            "SVG named `/Ink Do` stencil Type 3 glyph must be blue, got {:?}",
            [b.red(), b.green(), b.blue()]
        );
        assert_eq!([bg.red(), bg.green(), bg.blue()], [255, 255, 255]);
    }

    #[test]
    fn svg_type3_nonuniform_4x1_stroke_preserves_anisotropic_widths() {
        let charproc = b"30 0 0 0 30 40 d1 2 w 4 2 m 4 38 l S 12 20 m 22 20 l S";
        let font = LoadedFont::new_with_data(
            zpdf_font::PdfFontType::Type3 {
                font_matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
                char_procs: HashMap::from([(
                    "A".into(),
                    std::sync::Arc::<[u8]>::from(&charproc[..]),
                )]),
                encoding: vec!["A".into()],
                widths: vec![30.0],
                first_char: 0,
            },
            "Type3AnisoSvg".into(),
            Vec::new(),
            zpdf_font::CidWidths::new(30.0),
        );
        let mut fonts = FontCache::new();
        let font_id = fonts.insert("F1".into(), font);

        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 8.0, 8.0));
        let mut r = Path::new();
        r.rect(Rect::new(0.0, 0.0, 8.0, 8.0));
        cell.push(RenderCommand::FillPath {
            path: r,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        let tiling = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 8.0, 8.0),
            x_step: 8.0,
            y_step: 8.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });

        for (label, paint) in [
            ("solid", Paint::Solid(Color::rgb(1.0, 0.0, 0.0))),
            ("tiling", Paint::Tiling(tiling)),
        ] {
            let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 110.0, 56.0));
            dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
                font_id,
                font_size: 1.0,
                glyphs: vec![PositionedGlyph {
                    glyph_id: 0,
                    code_len: 1,
                    code: b'A' as u32,
                    x: 0.0,
                    y: 0.0,
                    advance: 30.0,
                }],
                paint,
                alpha: 1.0,
                overprint: None,
                transform: zpdf_core::Matrix::new(4.0, 0.0, 0.0, 1.0, 8.0, 8.0),
                h_scale: 1.0,
            }));

            let svg = display_list_to_svg(&dl, &fonts, &ImageCache::new(), &SvgOptions::default());
            let tree =
                resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default())
                    .expect("valid SVG");
            let mut pixmap = resvg::tiny_skia::Pixmap::new(110, 56).expect("pixmap");
            let sx = 110.0 / tree.size().width();
            let sy = 56.0 / tree.size().height();
            resvg::render(
                &tree,
                resvg::tiny_skia::Transform::from_scale(sx, sy),
                &mut pixmap.as_mut(),
            );
            let vert_width = (10..40)
                .filter(|&x| {
                    let p = pixmap.pixel(x, 28).expect("in bounds");
                    p.red() > 200 && p.green() < 80 && p.blue() < 80
                })
                .count();
            let horiz_height = (0..56)
                .filter(|&y| {
                    let p = pixmap.pixel(76, y).expect("in bounds");
                    p.red() > 200 && p.green() < 80 && p.blue() < 80
                })
                .count();
            assert!(
                (7..=9).contains(&vert_width),
                "SVG ({label}) 4x1 vertical Type 3 stroke (w=2) must be ~8px wide, got {vert_width}px"
            );
            assert!(
                (1..=3).contains(&horiz_height),
                "SVG ({label}) 4x1 horizontal Type 3 stroke (w=2) must be ~2px tall, got {horiz_height}px"
            );
        }
    }

    #[test]
    fn svg_type3_dashed_stroke_leaves_gaps_transparent() {
        // `d1` dashed stroke `[6 6] 0 d` with width 4, round cap & join:
        // painted dashes at x=[4..10], [16..22], [28..34]; transparent gaps at x=[10..16], [22..28].
        // `d0` explicit red dashed stroke at y=28 with identical dash pattern.
        let d1_dash = b"40 0 0 0 40 40 d1 4 w 0 J 1 j [6 6] 0 d 4 12 m 34 12 l S";
        let d0_dash = b"40 0 d0 1 0 0 RG 4 w 0 J 1 j [6 6] 0 d 4 28 m 34 28 l S";
        let font = LoadedFont::new_with_data(
            zpdf_font::PdfFontType::Type3 {
                font_matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
                char_procs: HashMap::from([
                    ("A".into(), std::sync::Arc::<[u8]>::from(&d1_dash[..])),
                    ("B".into(), std::sync::Arc::<[u8]>::from(&d0_dash[..])),
                ]),
                encoding: vec!["A".into(), "B".into()],
                widths: vec![40.0, 40.0],
                first_char: 0,
            },
            "Type3DashedSvg".into(),
            Vec::new(),
            zpdf_font::CidWidths::new(40.0),
        );
        let mut fonts = FontCache::new();
        let font_id = fonts.insert("F1".into(), font);

        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 8.0, 8.0));
        let mut r = Path::new();
        r.rect(Rect::new(0.0, 0.0, 8.0, 8.0));
        cell.push(RenderCommand::FillPath {
            path: r,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        let tiling = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 8.0, 8.0),
            x_step: 8.0,
            y_step: 8.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 40.0, 40.0));
        dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
            font_id,
            font_size: 1.0,
            glyphs: vec![
                PositionedGlyph {
                    glyph_id: 0,
                    code_len: 1,
                    code: b'A' as u32,
                    x: 0.0,
                    y: 0.0,
                    advance: 0.0,
                },
                PositionedGlyph {
                    glyph_id: 1,
                    code_len: 1,
                    code: b'B' as u32,
                    x: 0.0,
                    y: 0.0,
                    advance: 0.0,
                },
            ],
            paint: Paint::Tiling(tiling),
            alpha: 1.0,
            overprint: None,
            transform: zpdf_core::Matrix::identity(),
            h_scale: 1.0,
        }));

        let svg = display_list_to_svg(&dl, &fonts, &ImageCache::new(), &SvgOptions::default());
        assert!(
            svg.contains("stroke-dasharray=\"6 6\""),
            "SVG Type 3 stroke must serialize dash array"
        );
        assert!(
            svg.contains("stroke-linejoin=\"round\""),
            "SVG Type 3 stroke must serialize line join"
        );
        let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default())
            .expect("valid SVG");
        let mut pixmap = resvg::tiny_skia::Pixmap::new(40, 40).expect("pixmap");
        let sx = 40.0 / tree.size().width();
        let sy = 40.0 / tree.size().height();
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(sx, sy),
            &mut pixmap.as_mut(),
        );
        // PDF y=12 -> pixel y=28 (d1 blue tiling); PDF y=28 -> pixel y=12 (d0 explicit red).
        for &(x, expect_blue) in &[(7, true), (13, false), (19, true), (25, false), (31, true)] {
            let p1 = pixmap.pixel(x, 28).expect("in bounds");
            let p0 = pixmap.pixel(x, 12).expect("in bounds");
            if expect_blue {
                assert!(
                    p1.red() < 15 && p1.green() < 15 && p1.blue() > 240,
                    "d1 dashed tiling segment at x={x} must be blue, got {:?}",
                    [p1.red(), p1.green(), p1.blue()]
                );
                assert!(
                    p0.red() > 240 && p0.green() < 15 && p0.blue() < 15,
                    "d0 dashed explicit red segment at x={x} must be red, got {:?}",
                    [p0.red(), p0.green(), p0.blue()]
                );
            } else {
                assert_eq!(
                    [p1.red(), p1.green(), p1.blue()],
                    [255, 255, 255],
                    "d1 dashed tiling gap at x={x} must remain white/transparent"
                );
                assert_eq!(
                    [p0.red(), p0.green(), p0.blue()],
                    [255, 255, 255],
                    "d0 dashed explicit red gap at x={x} must remain white/transparent"
                );
            }
        }
    }

    #[test]
    fn svg_many_single_pass_tiling_type3_runs_export_without_checkpoint_copy() {
        let charproc = b"1 0 0 0 1 1 d1 0 0 1 1 re f";
        let font = LoadedFont::new_with_data(
            zpdf_font::PdfFontType::Type3 {
                font_matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
                char_procs: HashMap::from([(
                    "A".into(),
                    std::sync::Arc::<[u8]>::from(&charproc[..]),
                )]),
                encoding: vec!["A".into()],
                widths: vec![1.0],
                first_char: 0,
            },
            "Type3ManyNormal".into(),
            Vec::new(),
            zpdf_font::CidWidths::new(1.0),
        );
        let mut fonts = FontCache::new();
        let font_id = fonts.insert("F1".into(), font);

        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 8.0, 8.0));
        let mut r = Path::new();
        r.rect(Rect::new(0.0, 0.0, 8.0, 8.0));
        cell.push(RenderCommand::FillPath {
            path: r,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        let tiling = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 8.0, 8.0),
            x_step: 8.0,
            y_step: 8.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 400.0, 200.0));
        for i in 0..200 {
            let mut p = Path::new();
            let x = (i % 40) as f64 * 10.0;
            let y = (i / 40) as f64 * 10.0;
            p.rect(Rect::new(x, y, x + 5.0, y + 5.0));
            dl.push(RenderCommand::FillPath {
                path: p,
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::rgb(0.8, 0.8, 0.8)),
                alpha: 1.0,
                overprint: None,
            });
        }
        for i in 0..150 {
            let x = (i % 30) as f64 * 12.0 + 4.0;
            let y = (i / 30) as f64 * 20.0 + 80.0;
            dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
                font_id,
                font_size: 8.0,
                glyphs: vec![PositionedGlyph {
                    glyph_id: 0,
                    code_len: 1,
                    code: b'A' as u32,
                    x: 0.0,
                    y: 0.0,
                    advance: 8.0,
                }],
                paint: Paint::Tiling(std::sync::Arc::clone(&tiling)),
                alpha: 1.0,
                overprint: None,
                transform: zpdf_core::Matrix::translate(x, y),
                h_scale: 1.0,
            }));
        }

        let svg = display_list_to_svg(&dl, &fonts, &ImageCache::new(), &SvgOptions::default());
        assert_eq!(
            svg.matches("<mask id=\"m").count(),
            150,
            "all 150 single-pass Normal patterned Type 3 runs must emit cleanly"
        );
        assert!(
            !svg.contains("<g id=\"r"),
            "single-pass Normal patterned Type 3 runs must not create rollback staging defs"
        );
    }

    #[test]
    fn svg_soft_mask_inside_deep_nested_tiling_keeps_transparent_half() {
        // 9-level nested tiling pattern (tile depth reaches 9 > MAX_MASK_DEPTH = 8,
        // still within MAX_TILING_DEPTH = 16). Inside the innermost tile, a
        // luminosity soft mask paints only the left half white and leaves the
        // right half black/transparent over a full-tile red fill. Because mask
        // depth is tracked independently of tile depth, the soft mask must be
        // emitted and the right half must remain transparent (white background).
        let tile_rect = Rect::new(0.0, 0.0, 20.0, 20.0);
        let mut mask_dl = DisplayList::new(tile_rect);
        mask_dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 10.0, 20.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        let half_mask = SoftMask {
            kind: SoftMaskKind::Luminosity,
            commands: std::sync::Arc::new(mask_dl),
            offset: (0.0, 0.0),
            backdrop_luma: 0.0,
            transfer: None,
        };

        let mut inner_cell = DisplayList::new(tile_rect);
        inner_cell.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Normal,
            isolated: true,
            knockout: false,
            bounds: tile_rect,
            alpha: 1.0,
            mask: Some(half_mask),
        });
        inner_cell.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 20.0, 20.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        inner_cell.push(RenderCommand::PopBlendGroup);

        let mut tiling = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(inner_cell),
            bbox: tile_rect,
            x_step: 20.0,
            y_step: 20.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });
        for _ in 0..8 {
            let mut parent = DisplayList::new(tile_rect);
            parent.push(RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 20.0, 20.0),
                rule: FillRule::NonZero,
                paint: Paint::Tiling(std::sync::Arc::clone(&tiling)),
                alpha: 1.0,
                overprint: None,
            });
            tiling = std::sync::Arc::new(zpdf_display_list::TilingPattern {
                commands: std::sync::Arc::new(parent),
                bbox: tile_rect,
                x_step: 20.0,
                y_step: 20.0,
                matrix: zpdf_core::Matrix::identity(),
                tiling_type: 1,
            });
        }

        let mut dl = DisplayList::new(tile_rect);
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
        assert!(
            svg.contains("<mask id=\"m"),
            "soft mask inside 9th-level nested tiling pattern must not be dropped at MAX_MASK_DEPTH: {svg}"
        );

        let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default())
            .expect("valid SVG");
        let mut pixmap = resvg::tiny_skia::Pixmap::new(40, 40).expect("pixmap");
        let sx = 40.0 / tree.size().width();
        let sy = 40.0 / tree.size().height();
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(sx, sy),
            &mut pixmap.as_mut(),
        );
        let left = pixmap.pixel(10, 20).expect("left half in bounds");
        let right = pixmap.pixel(30, 20).expect("right half in bounds");
        assert!(
            left.red() > 240 && left.green() < 15 && left.blue() < 15,
            "masked left half inside 9-level nested tile must be red, got {:?}",
            [left.red(), left.green(), left.blue(), left.alpha()]
        );
        assert!(
            right.red() > 240 && right.green() > 240 && right.blue() > 240,
            "transparent right half of soft mask inside 9-level nested tile must keep white background, got {:?}",
            [right.red(), right.green(), right.blue(), right.alpha()]
        );
    }

    #[test]
    fn svg_type3_ext_gstate_smask_keeps_masked_half_transparent() {
        use std::sync::Arc;
        use zpdf_font::{Type3FormXObject, Type3Resources, Type3SoftMask};

        let mut form_dict = zpdf_core::PdfDict::new();
        form_dict.insert(
            zpdf_core::PdfName::new("BBox"),
            zpdf_core::PdfObject::Array(vec![
                zpdf_core::PdfObject::Real(0.0),
                zpdf_core::PdfObject::Real(0.0),
                zpdf_core::PdfObject::Real(20.0),
                zpdf_core::PdfObject::Real(20.0),
            ]),
        );
        let group = Arc::new(Type3FormXObject {
            dict: form_dict,
            data: Arc::from(&b"1 g 0 0 10 20 re f"[..]),
            resources: None,
        });
        let mut t3_res = Type3Resources::default();
        t3_res.ext_g_state_smasks.insert(
            "GSMask".into(),
            Type3SoftMask::Dict {
                is_alpha: false,
                group,
                backdrop_luma: 0.0,
                transfer: None,
            },
        );
        let charproc = b"20 0 d0 /GSMask gs 1 0 0 rg 0 0 20 20 re f";
        let font = LoadedFont::new_with_data(
            zpdf_font::PdfFontType::Type3 {
                font_matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
                char_procs: HashMap::from([("A".into(), Arc::<[u8]>::from(&charproc[..]))]),
                encoding: vec!["A".into()],
                widths: vec![20.0],
                first_char: 0,
            },
            "Type3SMaskSvg".into(),
            Vec::new(),
            zpdf_font::CidWidths::new(20.0),
        )
        .with_type3_resources(Arc::new(t3_res));
        let mut fonts = FontCache::new();
        let font_id = fonts.insert("F1".into(), font);

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
            font_id,
            font_size: 1.0,
            glyphs: vec![PositionedGlyph {
                glyph_id: 0,
                code_len: 1,
                code: b'A' as u32,
                x: 0.0,
                y: 0.0,
                advance: 20.0,
            }],
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
            transform: zpdf_core::Matrix::identity(),
            h_scale: 1.0,
        }));

        let svg = display_list_to_svg(&dl, &fonts, &ImageCache::new(), &SvgOptions::default());
        assert!(
            svg.contains("<mask id=\"m"),
            "SVG standalone Type 3 replay must emit <mask> for /ExtGState /SMask: {svg}"
        );
        let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default())
            .expect("valid SVG");
        let mut pixmap = resvg::tiny_skia::Pixmap::new(40, 40).expect("pixmap");
        let sx = 40.0 / tree.size().width();
        let sy = 40.0 / tree.size().height();
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(sx, sy),
            &mut pixmap.as_mut(),
        );
        let left = pixmap.pixel(10, 20).expect("left half");
        let right = pixmap.pixel(30, 20).expect("right half");
        assert!(
            left.red() > 240 && left.green() < 15 && left.blue() < 15,
            "SVG Type 3 /SMask left half must be red, got {:?}",
            [left.red(), left.green(), left.blue()]
        );
        assert!(
            right.red() > 240 && right.green() > 240 && right.blue() > 240,
            "SVG Type 3 /SMask right half must remain white, got {:?}",
            [right.red(), right.green(), right.blue()]
        );
    }

    #[test]
    fn svg_type3_ext_gstate_smask_separated_placements_keep_distinct_masks() {
        use std::sync::Arc;
        use zpdf_font::{Type3FormXObject, Type3Resources, Type3SoftMask};

        let mut form_dict = zpdf_core::PdfDict::new();
        form_dict.insert(
            zpdf_core::PdfName::new("BBox"),
            zpdf_core::PdfObject::Array(vec![
                zpdf_core::PdfObject::Real(0.0),
                zpdf_core::PdfObject::Real(0.0),
                zpdf_core::PdfObject::Real(20.0),
                zpdf_core::PdfObject::Real(20.0),
            ]),
        );
        let group = Arc::new(Type3FormXObject {
            dict: form_dict,
            data: Arc::from(&b"1 g 0 0 10 20 re f"[..]),
            resources: None,
        });
        let mut t3_res = Type3Resources::default();
        t3_res.ext_g_state_smasks.insert(
            "GSMask".into(),
            Type3SoftMask::Dict {
                is_alpha: false,
                group,
                backdrop_luma: 0.0,
                transfer: None,
            },
        );
        // Colored glyph (`d0`) for solid-run replay and stencil glyph (`d1`)
        // for tiling-run replay, both using the same /ExtGState /SMask.
        let d0_proc = b"20 0 d0 /GSMask gs 1 0 0 rg 0 0 20 20 re f";
        let d1_proc = b"20 0 0 0 20 20 d1 /GSMask gs 0 0 20 20 re f";
        let font = LoadedFont::new_with_data(
            zpdf_font::PdfFontType::Type3 {
                font_matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
                char_procs: HashMap::from([
                    ("A".into(), Arc::<[u8]>::from(&d0_proc[..])),
                    ("B".into(), Arc::<[u8]>::from(&d1_proc[..])),
                ]),
                encoding: vec!["A".into(), "B".into()],
                widths: vec![20.0, 20.0],
                first_char: 0,
            },
            "Type3SMaskSeparated".into(),
            Vec::new(),
            zpdf_font::CidWidths::new(20.0),
        )
        .with_type3_resources(Arc::new(t3_res));
        let mut fonts = FontCache::new();
        let font_id = fonts.insert("F1".into(), font);

        // 1. Solid Type 3 run: three separated placements plus one identical
        // repeat at x = 30.0. The three distinct positions must each emit
        // their own <mask> def (no freed-Arc address aliasing), while the
        // identical repeat reuses the existing def.
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 90.0, 20.0));
        let glyph_at = |glyph_id: u16, x: f32| PositionedGlyph {
            glyph_id,
            code_len: 1,
            code: if glyph_id == 0 { b'A' } else { b'B' } as u32,
            x,
            y: 0.0,
            advance: 20.0,
        };
        dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
            font_id,
            font_size: 1.0,
            glyphs: vec![
                glyph_at(0, 0.0),
                glyph_at(0, 30.0),
                glyph_at(0, 60.0),
                glyph_at(0, 30.0),
            ],
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
            transform: zpdf_core::Matrix::identity(),
            h_scale: 1.0,
        }));

        let svg = display_list_to_svg(&dl, &fonts, &ImageCache::new(), &SvgOptions::default());
        assert_eq!(
            svg.matches("<mask ").count(),
            3,
            "3 separated placements + 1 identical repeat must emit exactly 3 <mask> defs: {svg}"
        );
        let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default())
            .expect("valid SVG");
        let mut pixmap = resvg::tiny_skia::Pixmap::new(180, 40).expect("pixmap");
        let sx = 180.0 / tree.size().width();
        let sy = 40.0 / tree.size().height();
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(sx, sy),
            &mut pixmap.as_mut(),
        );
        for base_x in [0u32, 30, 60] {
            let left = pixmap.pixel((base_x + 5) * 2, 20).expect("left half");
            let right = pixmap.pixel((base_x + 15) * 2, 20).expect("right half");
            assert!(
                left.red() > 240 && left.green() < 15 && left.blue() < 15,
                "placement at x={base_x} left half must be red, got {:?}",
                [left.red(), left.green(), left.blue()]
            );
            assert!(
                right.red() > 240 && right.green() > 240 && right.blue() > 240,
                "placement at x={base_x} masked right half must stay white, got {:?}",
                [right.red(), right.green(), right.blue()]
            );
        }

        // 2. Tiling-patterned Type 3 run (`emit_tiling_type3_run`): two
        // separated placements painted with a blue tiling pattern.
        let mut tile_dl = DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0));
        tile_dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 10.0, 10.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        let tiling = Arc::new(zpdf_display_list::TilingPattern {
            commands: Arc::new(tile_dl),
            bbox: Rect::new(0.0, 0.0, 10.0, 10.0),
            x_step: 10.0,
            y_step: 10.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });
        let mut tiling_page = DisplayList::new(Rect::new(0.0, 0.0, 60.0, 20.0));
        tiling_page.push(RenderCommand::DrawGlyphRun(GlyphRun {
            font_id,
            font_size: 1.0,
            glyphs: vec![glyph_at(1, 0.0), glyph_at(1, 30.0)],
            paint: Paint::Tiling(tiling),
            alpha: 1.0,
            overprint: None,
            transform: zpdf_core::Matrix::identity(),
            h_scale: 1.0,
        }));

        let tiling_svg = display_list_to_svg(
            &tiling_page,
            &fonts,
            &ImageCache::new(),
            &SvgOptions::default(),
        );
        let tiling_tree =
            resvg::usvg::Tree::from_data(tiling_svg.as_bytes(), &resvg::usvg::Options::default())
                .expect("valid tiling SVG");
        let mut tiling_px = resvg::tiny_skia::Pixmap::new(120, 40).expect("tiling pixmap");
        resvg::render(
            &tiling_tree,
            resvg::tiny_skia::Transform::from_scale(
                120.0 / tiling_tree.size().width(),
                40.0 / tiling_tree.size().height(),
            ),
            &mut tiling_px.as_mut(),
        );
        for base_x in [0u32, 30] {
            let left = tiling_px.pixel((base_x + 5) * 2, 20).expect("tiling left");
            let right = tiling_px
                .pixel((base_x + 15) * 2, 20)
                .expect("tiling right");
            assert!(
                left.red() < 15 && left.green() < 15 && left.blue() > 240,
                "tiling placement at x={base_x} left half must be blue, got {:?}",
                [left.red(), left.green(), left.blue()]
            );
            assert!(
                right.red() > 240 && right.green() > 240 && right.blue() > 240,
                "tiling placement at x={base_x} masked right half must stay white, got {:?}",
                [right.red(), right.green(), right.blue()]
            );
        }
    }

    #[test]
    fn svg_tiling_type3_transparency_group_composites_overlap_once_matching_cpu() {
        use std::sync::Arc;
        use zpdf_font::{Type3FormXObject, Type3Resources, Type3XObject};

        let make_form = |data: &[u8]| {
            let mut form_dict = zpdf_core::PdfDict::new();
            form_dict.insert(
                zpdf_core::PdfName::new("BBox"),
                zpdf_core::PdfObject::Array(vec![
                    zpdf_core::PdfObject::Real(0.0),
                    zpdf_core::PdfObject::Real(0.0),
                    zpdf_core::PdfObject::Real(30.0),
                    zpdf_core::PdfObject::Real(20.0),
                ]),
            );
            let mut grp = zpdf_core::PdfDict::new();
            grp.insert(
                zpdf_core::PdfName::new("S"),
                zpdf_core::PdfObject::Name(zpdf_core::PdfName::new("Transparency")),
            );
            grp.insert(
                zpdf_core::PdfName::new("I"),
                zpdf_core::PdfObject::Bool(true),
            );
            form_dict.insert(
                zpdf_core::PdfName::new("Group"),
                zpdf_core::PdfObject::Dict(grp),
            );
            Arc::new(Type3FormXObject {
                dict: form_dict,
                data: Arc::from(data),
                resources: None,
            })
        };
        let mut gs_half = zpdf_core::PdfDict::new();
        gs_half.insert(
            zpdf_core::PdfName::new("ca"),
            zpdf_core::PdfObject::Real(0.5),
        );
        let mut t3_res = Type3Resources::default();
        t3_res.ext_g_states.insert("GSHalf".into(), gs_half);
        // Form A: two overlapping opaque colored fills (red [0..20], green [10..30]).
        t3_res.xobjects.insert(
            "FmColored".into(),
            Type3XObject::Form(make_form(
                b"1 0 0 rg 0 0 20 20 re f 0 1 0 rg 10 0 20 20 re f",
            )),
        );
        // Form B: overlapping opaque colored fill (red [0..20]) + inherited pattern fill ([10..30]).
        t3_res.xobjects.insert(
            "FmMixed".into(),
            Type3XObject::Form(make_form(b"q 1 0 0 rg 0 0 20 20 re f Q 10 0 20 20 re f")),
        );

        let proc_colored = b"30 0 d0 /GSHalf gs /FmColored Do";
        let proc_mixed = b"30 0 d0 /GSHalf gs /FmMixed Do";
        let font = LoadedFont::new_with_data(
            zpdf_font::PdfFontType::Type3 {
                font_matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
                char_procs: HashMap::from([
                    ("A".into(), Arc::<[u8]>::from(&proc_colored[..])),
                    ("B".into(), Arc::<[u8]>::from(&proc_mixed[..])),
                ]),
                encoding: vec!["A".into(), "B".into()],
                widths: vec![30.0, 30.0],
                first_char: 0,
            },
            "Type3TilingGroupOverlap".into(),
            Vec::new(),
            zpdf_font::CidWidths::new(30.0),
        )
        .with_type3_resources(Arc::new(t3_res));
        let mut fonts = FontCache::new();
        let font_id = fonts.insert("F1".into(), font);

        let mut tile_dl = DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0));
        tile_dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 10.0, 10.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        let tiling = Arc::new(zpdf_display_list::TilingPattern {
            commands: Arc::new(tile_dl),
            bbox: Rect::new(0.0, 0.0, 10.0, 10.0),
            x_step: 10.0,
            y_step: 10.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });

        for (label, glyph_id, code) in [
            ("colored+colored", 0u16, b'A'),
            ("colored+inherited", 1, b'B'),
        ] {
            let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 30.0, 20.0));
            dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
                font_id,
                font_size: 1.0,
                glyphs: vec![PositionedGlyph {
                    glyph_id,
                    code_len: 1,
                    code: code as u32,
                    x: 0.0,
                    y: 0.0,
                    advance: 30.0,
                }],
                paint: Paint::Tiling(Arc::clone(&tiling)),
                alpha: 1.0,
                overprint: None,
                transform: zpdf_core::Matrix::identity(),
                h_scale: 1.0,
            }));

            let cpu = CpuRenderer::new()
                .with_fonts(&fonts)
                .render_display_list(&dl, 1.0)
                .expect("cpu render");
            let svg = display_list_to_svg(&dl, &fonts, &ImageCache::new(), &SvgOptions::default());
            let tree =
                resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default())
                    .expect("valid SVG");
            let mut pixmap = resvg::tiny_skia::Pixmap::new(30, 20).expect("pixmap");
            resvg::render(
                &tree,
                resvg::tiny_skia::Transform::from_scale(
                    30.0 / tree.size().width(),
                    20.0 / tree.size().height(),
                ),
                &mut pixmap.as_mut(),
            );

            for &(region, x) in &[
                ("left-only", 5u32),
                ("overlap", 15u32),
                ("right-only", 25u32),
            ] {
                let s = pixmap.pixel(x, 10).expect("svg pixel");
                let svg_rgb = [s.red(), s.green(), s.blue()];
                let idx = ((10 * cpu.width + x) * 4) as usize;
                let cpu_rgb = [cpu.data[idx], cpu.data[idx + 1], cpu.data[idx + 2]];
                for ch in 0..3 {
                    assert!(
                        svg_rgb[ch].abs_diff(cpu_rgb[ch]) <= 2,
                        "{label} {region} (x={x}): SVG {svg_rgb:?} must match CPU {cpu_rgb:?} (group opacity 0.5 applied once, not 0.75): {svg}"
                    );
                }
            }
        }
    }

    #[test]
    fn svg_type3_soft_mask_nonuniform_scale_preserves_directional_stroke() {
        use std::sync::Arc;
        use zpdf_font::{Type3FormXObject, Type3Resources, Type3SoftMask};

        let mask_stream = b"1 G 12 w 40 10 m 40 70 l S 10 40 m 70 40 l S";
        let mut form_dict = zpdf_core::PdfDict::new();
        form_dict.insert(
            zpdf_core::PdfName::new("BBox"),
            zpdf_core::PdfObject::Array(vec![
                zpdf_core::PdfObject::Real(0.0),
                zpdf_core::PdfObject::Real(0.0),
                zpdf_core::PdfObject::Real(80.0),
                zpdf_core::PdfObject::Real(80.0),
            ]),
        );
        let group = Arc::new(Type3FormXObject {
            dict: form_dict,
            data: Arc::from(&mask_stream[..]),
            resources: None,
        });
        let mut t3_res = Type3Resources::default();
        t3_res.ext_g_state_smasks.insert(
            "GSStroke".into(),
            Type3SoftMask::Dict {
                is_alpha: false,
                group,
                backdrop_luma: 0.0,
                transfer: None,
            },
        );
        let charproc = b"80 0 d0 /GSStroke gs 1 0 0 rg 0 0 80 80 re f";
        let font = LoadedFont::new_with_data(
            zpdf_font::PdfFontType::Type3 {
                font_matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
                char_procs: HashMap::from([("A".into(), Arc::<[u8]>::from(&charproc[..]))]),
                encoding: vec!["A".into()],
                widths: vec![80.0],
                first_char: 0,
            },
            "Type3NonUniformMaskSvg".into(),
            Vec::new(),
            zpdf_font::CidWidths::new(80.0),
        )
        .with_type3_resources(Arc::new(t3_res));
        let mut fonts = FontCache::new();
        let font_id = fonts.insert("F1".into(), font);

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 60.0, 80.0));
        dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
            font_id,
            font_size: 1.0,
            glyphs: vec![PositionedGlyph {
                glyph_id: 0,
                code_len: 1,
                code: b'A' as u32,
                x: 0.0,
                y: 0.0,
                advance: 80.0,
            }],
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
            transform: zpdf_core::Matrix::identity(),
            h_scale: 0.5,
        }));

        let svg = display_list_to_svg(&dl, &fonts, &ImageCache::new(), &SvgOptions::default());
        let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default())
            .expect("valid SVG");
        let mut pixmap = resvg::tiny_skia::Pixmap::new(60, 80).expect("pixmap");
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(
                60.0 / tree.size().width(),
                80.0 / tree.size().height(),
            ),
            &mut pixmap.as_mut(),
        );
        let px = |x: u32, pdf_y: u32| -> [u8; 3] {
            let p = pixmap.pixel(x, 79 - pdf_y).expect("in bounds");
            [p.red(), p.green(), p.blue()]
        };
        assert!(px(22, 60)[0] > 240 && px(22, 60)[1] < 15, "{svg}");
        assert!(px(24, 60)[0] > 240 && px(24, 60)[1] > 240, "{svg}");
        assert!(px(10, 45)[0] > 240 && px(10, 45)[1] < 15, "{svg}");
        assert!(px(10, 47)[0] > 240 && px(10, 47)[1] > 240, "{svg}");
    }
}
