use std::collections::HashMap;
use std::sync::Arc;

use zpdf_core::ParseLimits;
use zpdf_display_list::*;
use zpdf_font::{FontCache, GlyphOutline, OutlineCommand};
use zpdf_image::ImageCache;
use zpdf_render::{PageRenderInfo, RenderBackend};

pub struct CpuRenderer<'a> {
    pixmap: Option<tiny_skia::Pixmap>,
    /// Device region the current target surface has been painted in since it
    /// became the target — see [`PaintedBounds`]. `None` == painted nothing.
    ///
    /// A blend group installs a fresh surface (region reset to nothing) and
    /// claims its region back at pop, which is what confines the group's
    /// composite and mask fold to the part of the page the group used.
    dirty: Option<PaintedBounds>,
    scale: f32,
    /// Vertical device scale; matches `scale` on page surfaces and allows
    /// anisotropic tile-pixel snapping when rasterizing a [`TilingPattern`] cell.
    scale_y: f32,
    /// Page rect bounds (supports CropBox / nonzero MediaBox origins):
    /// device x = (x - rect_x0) * scale - dev_origin_x,
    /// device y = (rect_y1 - y) * scale_y - dev_origin_y.
    rect_x0: f32,
    rect_y0: f32,
    rect_y1: f32,
    /// Integer device-pixel origin of the active target surface relative to the
    /// page raster (`(0, 0)` on the page itself; non-zero while rendering into a
    /// sub-region or horizontal/vertical band in `with_backdrop_shape_coverage`).
    dev_origin_x: u32,
    dev_origin_y: u32,
    font_cache: Option<&'a FontCache>,
    image_cache: Option<&'a ImageCache>,
    /// Maximum pixels in one output raster. Oversized pages are uniformly
    /// downscaled before allocation so the complete page still renders.
    max_page_pixels: u64,
    /// Maximum live transparency-group nesting, including groups inside masks.
    max_blend_group_depth: usize,
    /// Maximum retained one-byte soft-mask coverage planes per page.
    max_soft_mask_cache_bytes: u64,
    /// Security budgets used while interpreting nested Type3 glyph streams.
    parse_limits: ParseLimits,
    clip_stack: Vec<ClipFrame>,
    current_clip: Option<tiny_skia::Mask>,
    /// Device region of the active clip, tracking `current_clip` frame for
    /// frame. Intersecting a draw's extent with it keeps the painted region of a
    /// surface inside the clip that constrains it, without having to inspect the
    /// mask's pixels. `None` while no clip is active.
    clip_bounds: Option<PaintedBounds>,
    /// Cumulative full-raster clip-mask work (Σ width·height over built clip
    /// masks). Bounds the O(clips × raster) cost of the raster-mask clip model:
    /// pathological pages (100k+ tiny W clips on a large raster) would otherwise
    /// allocate/zero gigabytes of mask memory and hang. Past the budget, further
    /// clips are skipped (the page renders complete but slightly over-painted).
    clip_pixel_spent: u64,
    /// Bytes retained by clip masks: the active one, those saved on the clip
    /// stack and the zeroed ones in `mask_pool`.
    clip_mask_bytes: u64,
    /// Zeroed page-sized clip masks left by popped clips. A popped mask is
    /// cleared over its own `clip_bounds` — the only pixels it can be non-zero
    /// in — so the next clip reuses it instead of allocating and zeroing a full
    /// raster.
    mask_pool: Vec<tiny_skia::Mask>,
    blend_stack: Vec<BlendEntry>,
    /// Active non-isolated non-`Normal` transparency group currently recording
    /// its inner commands so `PopBlendGroup` can evaluate both live-backdrop
    /// color `(C_n, α_n)` and transparent group alpha `α_g` for exact
    /// ISO 32000-1 §11.4.5 backdrop removal before compositing with its outer
    /// blend mode.
    recording_non_isolated: Option<RecordedNonIsolatedGroup>,
    /// Number of enclosing recorded non-isolated non-`Normal` groups currently
    /// being replayed (counted against `max_blend_group_depth`).
    non_isolated_depth: usize,
    /// True while replaying Pass 2 of a recorded non-isolated non-`Normal`
    /// group over a transparent surface to compute accumulated group alpha
    /// `α_g`. In this mode, nested non-isolated non-`Normal` groups contribute
    /// identical Porter-Duff alpha when evaluated as single-pass isolated
    /// `Normal` groups, avoiding `2^depth` recursive replays.
    alpha_only_pass: bool,
    /// True while replaying Pass 3 of a recorded non-isolated child group
    /// inside a knockout parent to capture its ISO 32000-1 §11.4.6 geometric
    /// shape `f_G` at full opacity.
    shape_only_pass: bool,
    /// Bytes retained by the live page/group pixmap plus parked blend parents.
    blend_surface_bytes: u64,
    /// Transparent page-sized surfaces left by finished blend groups and
    /// overprint/knockout scratch passes, cleared over their painted region
    /// (see [`PaintedBounds`]). Reusing one replaces a full-raster allocation
    /// per group — pages wrap every element in its own blend group.
    surface_pool: Vec<tiny_skia::Pixmap>,
    /// Bytes of `surface_pool`. They count against [`MAX_BLEND_SURFACE_BYTES`]
    /// beside `blend_surface_bytes`, and are released when room is needed.
    pooled_surface_bytes: u64,
    /// Blend pushes skipped after reaching a configured depth limit or the
    /// render deadline. Keeping a separate depth lets matching pops unwind
    /// without accidentally popping a real outer group.
    skipped_blend_depth: usize,
    /// Wall-clock ceiling per page, an anti-hang backstop for adversarial pages
    /// that emit hundreds of thousands of expensive primitives (strokes, large
    /// fills, glyphs) — too many to render in bounded time yet individually
    /// cheap, so neither the command nor clip-pixel budget catches them. `None`
    /// disables it. Legit pages finish in well under the default and never hit it.
    render_budget: Option<std::time::Duration>,
    /// Absolute deadline for the current page (set in `begin_page`).
    deadline: Option<zpdf_core::time::Instant>,
    /// Set once the page deadline passes; further draws are skipped so the
    /// command loop drains quickly and the page returns partially rendered.
    over_budget: bool,
    /// Soft-mask coverage planes rasterized at build position (offset 0),
    /// keyed by mask identity. Retains the source mask `Arc<DisplayList>` and
    /// optional `/TR` LUT alongside the coverage bytes so pointer keys stay
    /// unique for as long as the entry lives. Cleared per page.
    soft_mask_planes: HashMap<SoftMaskPlaneKey, SoftMaskPlaneEntry>,
    soft_mask_cache_bytes: u64,
    /// Current recursive soft-mask depth (guards malicious/cyclic mask graphs).
    soft_mask_depth: usize,
    /// Rasterized [`TilingPattern`] cell pixmaps reused across fills/strokes
    /// sharing the same pattern cell and tile pixel dimensions. Stores the cell
    /// `Arc<DisplayList>` alongside the pixmap so the pointer key stays unique
    /// for as long as the entry lives, plus a cached uniform-opaque color flag.
    /// Cleared per page.
    tiling_cache: HashMap<TilingCacheKey, CachedTilingPixmap>,
    tiling_cache_bytes: u64,
    /// Per-page memo of `TilingPattern::is_backdrop_dependent()`, keeping each
    /// cell `Arc<DisplayList>` alive so pointer keys never alias freed memory.
    tiling_backdrop_cache: HashMap<usize, (Arc<DisplayList>, bool)>,
    /// Current recursive tiling-pattern rasterization depth.
    tiling_depth: usize,
    /// Box-filtered image variants reused by repeated draws at the same device
    /// footprint (common for patterns and repeated XObjects).
    downscaled_images: HashMap<(ImageId, u32, u32), Arc<[u8]>>,
    /// Outlines extracted from font programs this page, by font and glyph id
    /// (`None` for a glyph without one), so a glyph drawn many times is parsed
    /// once. Cleared per page: font ids are only stable within one font cache.
    glyph_outlines: HashMap<(FontId, u16), Option<Arc<GlyphOutline>>>,
    /// Outline commands held by `glyph_outlines` (see
    /// [`MAX_CACHED_OUTLINE_COMMANDS`]).
    glyph_outline_commands: usize,
    /// Interpreted Type 3 glyph programs this page, by font and glyph id
    /// (`None` for a code without one), so a glyph drawn many times is
    /// interpreted once. Cleared per page like `glyph_outlines`.
    type3_glyphs: HashMap<(FontId, u16), Option<Arc<Type3Glyph>>>,
    /// Commands and image bytes held by `type3_glyphs` (see
    /// [`MAX_CACHED_TYPE3_COMMANDS`], [`MAX_CACHED_TYPE3_IMAGE_BYTES`]).
    type3_glyph_commands: usize,
    type3_glyph_image_bytes: u64,
    /// Opt-in per-stage timings. `None` means "not collecting": every [`Self::tick`]
    /// then returns `None` and the hot path pays one branch, no clock read.
    /// See [`zpdf_render::StageStats`] for why collection is opt-in, and for the
    /// caveat that it makes the measured path slightly slower than production.
    stage_stats: Option<zpdf_render::StageStats>,
    /// Page start, read only while collecting, for `StageStats::total_ns`.
    page_start: Option<zpdf_core::time::Instant>,
}

/// Cached rasterized tiling cell pixmap with its owning command list (keeping
/// `Arc::as_ptr` cache keys unique) and precomputed uniform-opaque color.
struct CachedTilingPixmap {
    _commands: Arc<DisplayList>,
    pixmap: Arc<tiny_skia::Pixmap>,
    uniform_opaque: Option<[u8; 4]>,
}

type PreparedTilingPaint = (
    Arc<tiny_skia::Pixmap>,
    tiny_skia::Transform,
    tiny_skia::FilterQuality,
    f32,
    Option<[u8; 4]>,
);

fn pixmap_uniform_opaque(pixmap: &tiny_skia::Pixmap) -> Option<[u8; 4]> {
    let (pxs, _) = pixmap.data().as_chunks::<4>();
    let first = *pxs.first()?;
    (first[3] == 255 && pxs.iter().all(|&p| p == first)).then_some(first)
}

/// Identity of a [`SoftMask`] up to its offset: command list and transfer
/// LUT by Arc pointer, plus the scalar parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SoftMaskPlaneKey {
    commands: usize,
    kind: SoftMaskKind,
    backdrop_luma_bits: u32,
    transfer: usize,
}

impl SoftMaskPlaneKey {
    fn new(mask: &SoftMask) -> Self {
        Self {
            commands: std::sync::Arc::as_ptr(&mask.commands) as *const () as usize,
            kind: mask.kind,
            backdrop_luma_bits: mask.backdrop_luma.to_bits(),
            transfer: mask
                .transfer
                .as_ref()
                .map(|lut| std::sync::Arc::as_ptr(lut) as *const () as usize)
                .unwrap_or(0),
        }
    }
}

/// Cached soft-mask coverage plane plus strong references to the `Arc`s whose
/// addresses form [`SoftMaskPlaneKey`], preventing freed-address aliasing.
type SoftMaskPlaneEntry = (Arc<DisplayList>, Option<Arc<[u8; 256]>>, Arc<[u8]>);

/// Identity of a rasterized [`TilingPattern`] cell pixmap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct TilingCacheKey {
    commands: usize,
    segment_index: usize,
    overlap_shift: Option<(i64, i64)>,
    copy_blend_mode: u8,
    bbox: [u64; 4],
    steps: [u64; 2],
    tile_w: u32,
    tile_h: u32,
    blend_depth: usize,
}

impl TilingCacheKey {
    #[allow(clippy::too_many_arguments)]
    fn new(
        tiling: &TilingPattern,
        segment_index: usize,
        overlap_shift: Option<(i64, i64)>,
        copy_blend_mode: BlendMode,
        tile_w: u32,
        tile_h: u32,
        blend_depth: usize,
    ) -> Self {
        Self {
            commands: Arc::as_ptr(&tiling.commands) as *const () as usize,
            segment_index,
            overlap_shift,
            copy_blend_mode: copy_blend_mode as u8,
            bbox: [
                tiling.bbox.x0.to_bits(),
                tiling.bbox.y0.to_bits(),
                tiling.bbox.x1.to_bits(),
                tiling.bbox.y1.to_bits(),
            ],
            steps: [tiling.x_step.to_bits(), tiling.y_step.to_bits()],
            tile_w,
            tile_h,
            blend_depth,
        }
    }
}

/// Guard limits for [`TilingPattern`] cell rasterization in the CPU backend.
const MAX_TILING_DEPTH: usize = 16;
const MAX_TILING_TILE_DIM: u32 = 4096;
const MAX_TILING_TILE_PIXELS: u64 = 4_194_304; // 2048×2048 = 16 MiB RGBA
const MAX_TILING_CACHE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_DIRECT_TILING_TILES: i64 = 4096;
const MAX_DENSE_TILING_BLEND_PASSES: usize = 1024;

struct PreparedAffineTiles {
    trans: Arc<tiny_skia::Pixmap>,
    k: Arc<tiny_skia::Pixmap>,
    f1: Arc<tiny_skia::Pixmap>,
    transform: tiny_skia::Transform,
    quality: tiny_skia::FilterQuality,
}

fn unit(v: f32) -> f32 {
    if v.is_finite() {
        v.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// `round(v * a / 255)` for two 8-bit factors, matching zpdf-image.
#[inline]
fn mul255(v: u8, a: u8) -> u8 {
    ((v as u32 * a as u32 + 127) / 255) as u8
}

/// Coverage where the mask group painted nothing — what a fresh raster
/// produces outside the group, used to fill strips vacated by a plane shift.
fn unpainted_value(mask: &SoftMask) -> u8 {
    let v = match mask.kind {
        SoftMaskKind::Luminosity => (unit(mask.backdrop_luma) * 255.0).round() as u8,
        SoftMaskKind::Alpha => 0,
    };
    match &mask.transfer {
        Some(lut) => lut[v as usize],
        None => v,
    }
}

/// One mask pixel's coverage: the kind's reduction followed by the `/TR` LUT.
///
/// Shared by the scoped reduce and — under stats — by the leak check that
/// verifies the reduced region was enough, so the two definitions cannot drift
/// apart. `#[inline]` because the reduce calls it once per plane pixel, where a
/// call would otherwise cost more than the arithmetic.
#[inline]
fn mask_plane_value(px: tiny_skia::PremultipliedColorU8, mask: &SoftMask) -> u8 {
    let v = match mask.kind {
        SoftMaskKind::Luminosity => {
            let a = px.alpha();
            if a == 0 {
                (unit(mask.backdrop_luma) * 255.0).round() as u8
            } else {
                let d = px.demultiply();
                // Rec. 601 luma.
                (0.299 * d.red() as f32 + 0.587 * d.green() as f32 + 0.114 * d.blue() as f32)
                    .round()
                    .min(255.0) as u8
            }
        }
        SoftMaskKind::Alpha => px.alpha(),
    };
    match &mask.transfer {
        Some(lut) => lut[v as usize],
        None => v,
    }
}

/// Translate a `w`×`h` coverage plane by whole device pixels, filling vacated
/// areas with `fill`. Offsets at or beyond the plane size yield all-`fill`.
fn shift_plane(base: &[u8], w: u32, h: u32, dx: i64, dy: i64, fill: u8) -> Vec<u8> {
    let (w, h) = (w as usize, h as usize);
    let mut out = vec![fill; w * h];
    if base.len() != w * h {
        return out;
    }
    if dx.unsigned_abs() as usize >= w || dy.unsigned_abs() as usize >= h {
        return out;
    }
    let copy_w = w - dx.unsigned_abs() as usize;
    let copy_h = h - dy.unsigned_abs() as usize;
    let (src_x, dst_x) = if dx >= 0 {
        (0, dx as usize)
    } else {
        ((-dx) as usize, 0)
    };
    let (src_y, dst_y) = if dy >= 0 {
        (0, dy as usize)
    } else {
        ((-dy) as usize, 0)
    };
    for row in 0..copy_h {
        let src = (src_y + row) * w + src_x;
        let dst = (dst_y + row) * w + dst_x;
        out[dst..dst + copy_w].copy_from_slice(&base[src..src + copy_w]);
    }
    out
}

/// A half-open device-pixel region `[x0, x1) × [y0, y1)` of the current target
/// surface: the union of everything painted onto it since it became the target.
///
/// This is what lets a blend group's composite be confined to the part of the
/// page the group actually used. The region must always be a **superset** of
/// the painted pixels: outside it a group is fully transparent, and a blend of
/// zero source alpha leaves the backdrop pixel-identical for every blend mode
/// (PDF 11.4.7 Porter-Duff semantics), so skipping those pixels is exact. Every
/// draw site therefore grows its own extent conservatively — anti-aliased edges,
/// stroke miter overshoot, the transformed corners of an image — and any extent
/// that cannot be established with certainty degrades to the whole surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PaintedBounds {
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
}

impl PaintedBounds {
    fn whole(width: u32, height: u32) -> Self {
        Self {
            x0: 0,
            y0: 0,
            x1: width,
            y1: height,
        }
    }

    fn union(self, other: Self) -> Self {
        Self {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }

    /// Intersection; may be empty (inverted), which [`Self::is_empty`] reports.
    fn intersect(self, other: Self) -> Self {
        Self {
            x0: self.x0.max(other.x0),
            y0: self.y0.max(other.y0),
            x1: self.x1.min(other.x1),
            y1: self.y1.min(other.y1),
        }
    }

    fn is_empty(self) -> bool {
        self.x1 <= self.x0 || self.y1 <= self.y0
    }

    fn area(self) -> u64 {
        self.x1.saturating_sub(self.x0) as u64 * self.y1.saturating_sub(self.y0) as u64
    }

    /// The region as a device-space rectangle, or `None` when it is empty.
    ///
    /// Axes above the exact-integer range of `f32` can round here. Integer
    /// pixel ranges must still be clamped to the original surface dimensions.
    fn to_rect(self) -> Option<tiny_skia::Rect> {
        if self.is_empty() {
            return None;
        }
        tiny_skia::Rect::from_ltrb(
            self.x0 as f32,
            self.y0 as f32,
            self.x1 as f32,
            self.y1 as f32,
        )
    }
}

/// One pass over a finished group raster, returning its non-transparent
/// bounding box and the number of painted pixels that fall *outside* `region`.
///
/// The second number is what makes the scoped composite trustworthy: if any
/// painted pixel lies outside the region the composite is confined to, the
/// region is not the superset the "zero alpha changes nothing" argument needs,
/// and the page would come out wrong. It is expected to be 0 on every page, and
/// being a deterministic integer it is assertable in tests rather than trusted.
///
/// Costs a full pass, so callers gate it on stats collection being enabled —
/// in production neither number is computed.
fn group_coverage(
    pixmap: &tiny_skia::Pixmap,
    region: Option<PaintedBounds>,
) -> (Option<PaintedBounds>, u64) {
    let (w, h) = (pixmap.width(), pixmap.height());
    let data = pixmap.data();
    let mut content: Option<PaintedBounds> = None;
    let mut leak = 0u64;
    for y in 0..h {
        let row = (y as usize) * (w as usize) * 4;
        for x in 0..w {
            // Premultiplied RGBA; alpha is the fourth byte.
            if data[row + (x as usize) * 4 + 3] == 0 {
                continue;
            }
            let inside = region.is_some_and(|r| x >= r.x0 && x < r.x1 && y >= r.y0 && y < r.y1);
            if !inside {
                leak += 1;
            }
            let pixel = PaintedBounds {
                x0: x,
                y0: y,
                x1: x + 1,
                y1: y + 1,
            };
            content = Some(match content {
                Some(c) => c.union(pixel),
                None => pixel,
            });
        }
    }
    (content, leak)
}

/// How far a stroke's ink can reach beyond its centerline's bounding box: a
/// miter join is the worst case, at `miter_limit/2 × width` from the vertex
/// (PDF's default limit is 10, so this is not the `width/2` a round join needs),
/// plus one device pixel for the anti-aliased edge.
///
/// `width` and `miter_limit` come from the built [`tiny_skia::Stroke`], whose
/// width is already floored to one device pixel and finite.
fn stroke_overshoot(width: f32, miter_limit: f32) -> f32 {
    width * miter_limit.max(2.0) * 0.5 + 1.0
}

/// Default [`CpuRenderer::render_budget`]. Generous enough that no realistic page
/// approaches it (complex pages render in well under a second) while bounding
/// pathological inputs to a few seconds.
pub const DEFAULT_RENDER_BUDGET: std::time::Duration = std::time::Duration::from_secs(8);

/// Outline commands [`CpuRenderer`] keeps per page for reuse (about 28 MiB);
/// glyphs past it are extracted from the font program on every draw.
const MAX_CACHED_OUTLINE_COMMANDS: usize = 512 * 1024;

/// Display commands and decoded image bytes of interpreted Type 3 glyphs
/// [`CpuRenderer`] keeps per page; glyphs past either are interpreted on every
/// draw.
const MAX_CACHED_TYPE3_COMMANDS: usize = 256 * 1024;
const MAX_CACHED_TYPE3_IMAGE_BYTES: u64 = 64 * 1024 * 1024;

/// A Type 3 glyph program interpreted in glyph space.
struct Type3Glyph {
    list: DisplayList,
    /// Images its `DrawImage` commands reference.
    images: ImageCache,
    /// Shared `Arc<InlineImage>` wrappers for glyph-local `ImageId`s referenced
    /// inside soft masks so repeated glyph placements reuse one pixel buffer.
    inline_images: HashMap<ImageId, Arc<zpdf_display_list::InlineImage>>,
    font_matrix: [f64; 6],
}

/// A saved clip state for the clip stack. `Skipped` records a budget-dropped
/// `PushClip` so its matching `PopClip` leaves `current_clip` untouched.
///
/// A `Mask` frame carries the device region of the clip it restores, so
/// `clip_bounds` can be unwound in lockstep with `current_clip`.
enum ClipFrame {
    Empty,
    Mask(tiny_skia::Mask, Option<PaintedBounds>),
    Skipped,
}

/// Ceiling on cumulative clip-mask pixel-work per page (see `clip_pixel_spent`).
/// ~2 Gpx keeps every realistic clip-heavy page intact while bounding adversarial
/// ones to roughly a second of mask work.
const MAX_CLIP_PIXEL_WORK: u64 = 2_000_000_000;
const MAX_CLIP_MASK_BYTES: u64 = 128 * 1024 * 1024;

const MAX_DOWNSCALED_IMAGE_CACHE_BYTES: usize = 64 * 1024 * 1024;
const MAX_BLEND_SURFACE_BYTES: u64 = 512 * 1024 * 1024;
/// Maximum combined byte size of the `(shape_buf, scratch)` band pair in
/// [`CpuRenderer::with_backdrop_shape_coverage`] (32 MiB = 4M pixels per band).
/// Small marks allocate only their clipped extent; huge marks on large rasters
/// process in bands so they stay well inside [`MAX_BLEND_SURFACE_BYTES`].
const MAX_COVERAGE_BAND_BYTES: u64 = 32 * 1024 * 1024;
/// Surfaces [`CpuRenderer::surface_pool`] keeps at most.
const MAX_POOLED_SURFACES: usize = 4;

/// Uniformly reduce `scale` until ceil-rounded dimensions fit an exact pixel
/// budget. Recomputing after every reduction matters for very thin pages: one
/// dimension bottoms out at one pixel, so a single square-root estimate can
/// otherwise remain well above the requested total.
fn fit_raster_to_pixel_limit(
    width_points: f64,
    height_points: f64,
    requested_scale: f32,
    max_pixels: u64,
) -> Option<(f32, u32, u32)> {
    if max_pixels == 0 {
        return None;
    }
    let dimensions = |scale: f32| {
        let width = (width_points * scale as f64).ceil().max(1.0) as u32;
        let height = (height_points * scale as f64).ceil().max(1.0) as u32;
        (width, height, width as u64 * height as u64)
    };

    let mut scale = requested_scale;
    for _ in 0..96 {
        let (width, height, pixels) = dimensions(scale);
        if pixels <= max_pixels {
            return Some((scale, width, height));
        }
        let shrink = (max_pixels as f64 / pixels as f64).sqrt() * (1.0 - f64::EPSILON);
        let candidate = (scale as f64 * shrink) as f32;
        scale = if candidate > 0.0 && candidate < scale {
            candidate
        } else if scale.to_bits() > 1 {
            f32::from_bits(scale.to_bits() - 1)
        } else {
            return None;
        };
    }
    None
}

struct BlendEntry {
    /// The parked parent raster (the group's backdrop), restored and composited
    /// onto at `pop`. A 1×1 placeholder for a `passthrough` entry.
    pixmap: tiny_skia::Pixmap,
    /// The backdrop surface's painted region, carried across the group so `pop`
    /// can put it back and extend it by what the composite touches. `None` for
    /// a `passthrough` entry, which never leaves the backdrop surface.
    dirty: Option<PaintedBounds>,
    blend_mode: BlendMode,
    /// Group constant alpha applied at composite time.
    alpha: f32,
    /// Rasterized soft-mask coverage (one byte per pixel), multiplied into the
    /// group before compositing.
    mask: Option<Arc<[u8]>>,
    /// Knockout group: each element composites against the group's initial
    /// backdrop rather than the accumulation of preceding elements.
    knockout: bool,
    /// The group's initial backdrop (`b0`) for per-element knockout compositing
    /// (`Some` only for knockout groups).
    backdrop: Option<tiny_skia::Pixmap>,
    /// A non-isolated group with no group-level effect (Normal blend, alpha 1,
    /// no mask, no knockout): its elements draw straight onto the canvas, so no
    /// buffer is parked and `pop` is a no-op.
    passthrough: bool,
    /// Non-isolated Normal group with constant alpha and/or soft mask: the
    /// group surface was seeded with a copy of `pixmap` so blend modes inside
    /// see the live backdrop, and `pop` applies `alpha * mask` once via
    /// `mix(backdrop, group, alpha * mask)` (ISO 32000-1 §11.4.5 / §11.6.7).
    non_isolated: bool,
}

struct RecordedNonIsolatedGroup {
    blend_mode: BlendMode,
    alpha: f32,
    mask: Option<Arc<[u8]>>,
    nesting: usize,
    cmds: Vec<RenderCommand>,
}

#[derive(Debug, thiserror::Error)]
pub enum CpuRenderError {
    #[error("not initialized")]
    NotInitialized,
    #[error("failed to create pixmap")]
    PixmapCreation,
    #[error("invalid page render info: {0}")]
    InvalidPage(#[from] zpdf_render::PageGeometryError),
    #[error("render limit exceeded: {0}")]
    LimitExceeded(String),
    #[error("png encode error: {0}")]
    PngEncode(String),
}

impl<'a> CpuRenderer<'a> {
    pub fn new() -> Self {
        let limits = ParseLimits::default();
        Self {
            pixmap: None,
            dirty: None,
            scale: 1.0,
            scale_y: 1.0,
            rect_x0: 0.0,
            rect_y0: 0.0,
            rect_y1: 0.0,
            dev_origin_x: 0,
            dev_origin_y: 0,
            font_cache: None,
            image_cache: None,
            max_page_pixels: limits.max_page_pixels,
            max_blend_group_depth: limits.max_blend_group_depth as usize,
            max_soft_mask_cache_bytes: limits.max_softmask_cache_bytes,
            parse_limits: limits,
            clip_stack: Vec::new(),
            current_clip: None,
            clip_bounds: None,
            clip_pixel_spent: 0,
            clip_mask_bytes: 0,
            mask_pool: Vec::new(),
            blend_stack: Vec::new(),
            recording_non_isolated: None,
            non_isolated_depth: 0,
            alpha_only_pass: false,
            shape_only_pass: false,
            blend_surface_bytes: 0,
            surface_pool: Vec::new(),
            pooled_surface_bytes: 0,
            skipped_blend_depth: 0,
            render_budget: Some(DEFAULT_RENDER_BUDGET),
            deadline: None,
            over_budget: false,
            soft_mask_planes: HashMap::new(),
            soft_mask_cache_bytes: 0,
            soft_mask_depth: 0,
            tiling_cache: HashMap::new(),
            tiling_cache_bytes: 0,
            tiling_backdrop_cache: HashMap::new(),
            tiling_depth: 0,
            downscaled_images: HashMap::new(),
            glyph_outlines: HashMap::new(),
            glyph_outline_commands: 0,
            type3_glyphs: HashMap::new(),
            type3_glyph_commands: 0,
            type3_glyph_image_bytes: 0,
            stage_stats: None,
            page_start: None,
        }
    }

    /// Enable per-stage timing collection (off by default).
    ///
    /// Collection adds clock reads to the render hot path, so a measurement with
    /// it enabled is a slightly slower path than a consumer runs. Benches report
    /// the timing-off run as the headline number and this as a labelled
    /// diagnostic. Retrieve with [`RenderBackend::stage_stats`].
    pub fn with_stage_timing(mut self, enabled: bool) -> Self {
        self.stage_stats = enabled.then(zpdf_render::StageStats::default);
        self
    }

    /// Borrow the in-progress root page surface `(width, height, premultiplied_rgba)`
    /// while a page is open (between `begin_page` and `end_page`) and no
    /// non-passthrough blend group is currently parked on `blend_stack`.
    pub fn current_surface(&self) -> Option<(u32, u32, &[u8])> {
        if self.blend_stack.iter().any(|entry| !entry.passthrough) {
            return None;
        }
        let pixmap = self.pixmap.as_ref()?;
        Some((pixmap.width(), pixmap.height(), pixmap.data()))
    }

    /// Effective device pixel scale currently installed by `begin_page`.
    pub fn current_scale(&self) -> f32 {
        self.scale
    }

    /// Whether the current page exceeded its configured wall-clock
    /// `render_budget` and skipped subsequent paint commands.
    pub fn is_over_budget(&self) -> bool {
        self.over_budget
    }

    /// Update the active page's wall-clock deadline (or `None` to pause
    /// budget tracking while the caller performs unrelated work between
    /// command batches on a long-lived page surface).
    pub fn set_deadline(&mut self, deadline: Option<zpdf_core::time::Instant>) {
        self.deadline = deadline;
    }

    /// Drop all cached soft-mask coverage planes on the current page. Callers
    /// that pass temporary rebased `Arc<DisplayList>` masks to a long-lived
    /// renderer should clear this cache before dropping those temporary `Arc`s
    /// so subsequent allocations cannot reuse an old `Arc::as_ptr` key.
    pub fn clear_soft_mask_cache(&mut self) {
        self.soft_mask_planes.clear();
        self.soft_mask_cache_bytes = 0;
    }

    /// Start a stopwatch, or `None` when not collecting. Pair with [`Self::tock`].
    fn tick(&self) -> Option<zpdf_core::time::Instant> {
        self.stage_stats
            .as_ref()
            .map(|_| zpdf_core::time::Instant::now())
    }

    /// Add `start..now` to one bucket. A no-op when `start` is `None` (timing
    /// off), so instrumentation never branches per call site.
    fn tock(
        &mut self,
        start: Option<zpdf_core::time::Instant>,
        bucket: impl FnOnce(&mut zpdf_render::StageStats) -> &mut u64,
    ) {
        if let (Some(start), Some(stats)) = (start, self.stage_stats.as_mut()) {
            *bucket(stats) += start.elapsed().as_nanos() as u64;
        }
    }

    /// Add `n` to one counter, or nothing when not collecting.
    fn count(&mut self, n: u64, counter: impl FnOnce(&mut zpdf_render::StageStats) -> &mut u64) {
        if let Some(stats) = self.stage_stats.as_mut() {
            *counter(stats) += n;
        }
    }

    /// Override the per-page wall-clock render budget (`None` disables it).
    pub fn with_render_budget(mut self, budget: Option<std::time::Duration>) -> Self {
        self.render_budget = budget;
        self
    }

    /// Apply the render-time security budgets from the document being rendered.
    /// The values are copied, so the renderer does not borrow the limits object.
    pub fn with_limits(mut self, limits: &ParseLimits) -> Self {
        self.parse_limits = limits.clone();
        self.max_page_pixels = limits.max_page_pixels;
        self.max_blend_group_depth = limits.max_blend_group_depth as usize;
        self.max_soft_mask_cache_bytes = limits.max_softmask_cache_bytes;
        self
    }

    pub fn with_fonts(mut self, cache: &'a FontCache) -> Self {
        self.font_cache = Some(cache);
        self
    }

    pub fn with_images(mut self, cache: &'a ImageCache) -> Self {
        self.image_cache = Some(cache);
        self
    }

    /// Pixel dimensions of the active page raster (`None` before `begin_page`
    /// or after `end_page`).
    pub fn pixmap_dimensions(&self) -> Option<(u32, u32)> {
        self.pixmap.as_ref().map(|p| (p.width(), p.height()))
    }

    /// Actual raster scale chosen by `begin_page` (after any page-pixel clamp).
    pub fn actual_scale(&self) -> f32 {
        self.scale
    }

    /// True while the renderer is deferring commands inside a recorded
    /// non-isolated non-`Normal` transparency group until its matching
    /// `PopBlendGroup` replays them.
    pub fn is_recording_non_isolated(&self) -> bool {
        self.recording_non_isolated.is_some()
    }

    /// Copy premultiplied RGBA8 pixels from `[x0..x0+width] × [y0..y0+height]`
    /// of the active page raster without ending the page.
    pub fn copy_subrect(&self, x0: u32, y0: u32, width: u32, height: u32) -> Option<Vec<u8>> {
        let pixmap = self.pixmap.as_ref()?;
        let (pw, ph) = (pixmap.width(), pixmap.height());
        if width == 0 || height == 0 || x0.checked_add(width)? > pw || y0.checked_add(height)? > ph
        {
            return None;
        }
        let row_bytes = (width as usize).checked_mul(4)?;
        let total = row_bytes.checked_mul(height as usize)?;
        let stride = (pw as usize).checked_mul(4)?;
        let x_off = (x0 as usize).checked_mul(4)?;
        let src = pixmap.data();
        let mut out = vec![0u8; total];
        for row in 0..height as usize {
            let src_start = (y0 as usize + row) * stride + x_off;
            let dst_start = row * row_bytes;
            out[dst_start..dst_start + row_bytes]
                .copy_from_slice(&src[src_start..src_start + row_bytes]);
        }
        Some(out)
    }

    /// Replace the active page raster's pixels with `premul_rgba` (exact
    /// dimensions required) and mark the whole raster dirty so subsequent blend
    /// groups composite against the seeded backdrop.
    pub fn seed_pixmap(&mut self, width: u32, height: u32, premul_rgba: &[u8]) -> bool {
        let Some(pixmap) = self.pixmap.as_mut() else {
            return false;
        };
        if pixmap.width() != width
            || pixmap.height() != height
            || premul_rgba.len() != pixmap.data().len()
        {
            return false;
        }
        pixmap.data_mut().copy_from_slice(premul_rgba);
        self.dirty = Some(PaintedBounds::whole(width, height));
        true
    }

    /// The scale [`RenderBackend::render_display_list`] rasterizes a page of
    /// `page_rect` at when asked for `scale`: the request itself, or the
    /// reduced scale that fits this renderer's page-pixel budget. `None` when
    /// the page cannot be rendered at all.
    ///
    /// Pass it to `ContentInterpreter::with_image_downscale` when the display
    /// list is interpreted for this renderer: images then arrive already
    /// box-filtered to the size this renderer would filter them to on draw.
    pub fn raster_scale(&self, page_rect: zpdf_core::Rect, scale: f32) -> Option<f32> {
        let info = PageRenderInfo {
            page_rect,
            scale,
            background: Color::white(),
        };
        self.fit_raster(&info).ok().map(|(scale, _, _)| scale)
    }

    /// Raster scale and dimensions for `info` within the page-pixel budget.
    ///
    /// A pathological MediaBox (or a high DPI on a huge page) otherwise demands
    /// a multi-gigabyte Pixmap that aborts the process on the allocation alone,
    /// or makes per-pixel work (fills, blits, per-clip masks) hang. Scaling BOTH
    /// the dimensions and the scale by the same factor keeps geometry
    /// consistent, so the output is a smaller-but-complete render instead of a
    /// crash/timeout.
    fn fit_raster(&self, info: &PageRenderInfo) -> Result<(f32, u32, u32), CpuRenderError> {
        // ceil(), not truncation: a 595x842pt page at 110 DPI is 909.03x1286.6px
        // and must produce a 910x1287 raster (pdfium semantics) so no content is
        // sliced off the right/bottom edges.
        let (w, h) = info.raster_dimensions()?;
        let pixels = w as u64 * h as u64;
        if pixels <= self.max_page_pixels {
            return Ok((info.scale, w, h));
        }
        fit_raster_to_pixel_limit(
            info.page_rect.width(),
            info.page_rect.height(),
            info.scale,
            self.max_page_pixels,
        )
        .ok_or_else(|| {
            CpuRenderError::LimitExceeded(format!(
                "page raster {w}x{h} cannot fit max_page_pixels={}",
                self.max_page_pixels
            ))
        })
    }

    /// Convert PDF Y coordinate (origin bottom-left) to pixel Y (origin top-left),
    /// relative to the active surface's top edge.
    fn flip_y(&self, y: f32) -> f32 {
        (self.rect_y1 - y) * self.scale_y - self.dev_origin_y as f32
    }

    fn to_pixel_x(&self, x: f32) -> f32 {
        (x - self.rect_x0) * self.scale - self.dev_origin_x as f32
    }

    fn stroke_scale(&self) -> f32 {
        if self.scale == self.scale_y {
            self.scale
        } else {
            (self.scale * self.scale_y).sqrt()
        }
    }

    /// Finish a built path, rejecting it (caller then skips the draw) when its
    /// bounds are non-finite or astronomically larger than the target pixmap.
    /// Such geometry — from a degenerate text/CTM matrix or a corrupt coordinate
    /// — overflows tiny-skia's AA blitter row-offset arithmetic and panics with
    /// an out-of-range slice index instead of being clipped. A generous overscan
    /// (64× the longest pixmap side, min 1e6 px) keeps all legitimately
    /// off-page-but-clipped content while dropping only crash-inducing extents.
    fn finish_within_limits(&self, pb: tiny_skia::PathBuilder) -> Option<tiny_skia::Path> {
        let path = pb.finish()?;
        self.check_path_limits(path)
    }

    fn check_path_limits(&self, path: tiny_skia::Path) -> Option<tiny_skia::Path> {
        let b = path.bounds();
        let vals = [b.left(), b.top(), b.right(), b.bottom()];
        if vals.iter().any(|v| !v.is_finite()) {
            tracing::warn!("skipping non-finite path");
            return None;
        }
        let span = self
            .pixmap
            .as_ref()
            .map(|p| p.width().max(p.height()) as f32)
            .unwrap_or(0.0);
        let limit = (span * 64.0).max(1_000_000.0);
        if vals.iter().any(|v| v.abs() > limit) {
            tracing::warn!("skipping out-of-range path bounds={b:?}");
            return None;
        }
        Some(path)
    }

    /// Build a path in unscaled Y-down cell space (`(x - rect_x0, rect_y1 - y)`),
    /// used when `self.scale != self.scale_y` so stroke expansion and dashing
    /// happen in isotropic user space before applying the anisotropic scale.
    fn build_unscaled_skia_path(&self, path: &Path) -> Option<tiny_skia::Path> {
        let mut pb = tiny_skia::PathBuilder::new();
        for elem in path {
            match elem {
                PathElement::MoveTo(p) => {
                    pb.move_to(p.x as f32 - self.rect_x0, self.rect_y1 - p.y as f32);
                }
                PathElement::LineTo(p) => {
                    pb.line_to(p.x as f32 - self.rect_x0, self.rect_y1 - p.y as f32);
                }
                PathElement::CurveTo(c1, c2, end) => {
                    pb.cubic_to(
                        c1.x as f32 - self.rect_x0,
                        self.rect_y1 - c1.y as f32,
                        c2.x as f32 - self.rect_x0,
                        self.rect_y1 - c2.y as f32,
                        end.x as f32 - self.rect_x0,
                        self.rect_y1 - end.y as f32,
                    );
                }
                PathElement::Close => pb.close(),
            }
        }
        let built = pb.finish()?;
        let b = built.bounds();
        if ![b.left(), b.top(), b.right(), b.bottom()]
            .iter()
            .all(|v| v.is_finite())
        {
            return None;
        }
        Some(built)
    }

    /// Expand a positive-width stroke in unscaled user space and transform the
    /// resulting filled outline by `(self.scale, self.scale_y)` so anisotropic
    /// pattern-cell transforms produce exact axis-dependent stroke widths and
    /// elliptical caps/joins without perturbing pattern paint shaders.
    fn build_anisotropic_stroke_outline(
        &self,
        path: &Path,
        style: &StrokeStyle,
    ) -> Option<tiny_skia::Path> {
        let unscaled = self.build_unscaled_skia_path(path)?;
        let max_scale = self.scale.max(self.scale_y);
        let res_scale = max_scale.clamp(0.01, 1024.0);
        let unscaled_stroke = Self::skia_stroke(style, 1.0, 1.0 / max_scale);
        let dashed;
        let source = if let Some(ref dash) = unscaled_stroke.dash {
            dashed = unscaled.dash(dash, res_scale)?;
            &dashed
        } else {
            &unscaled
        };
        let stroked = source.stroke(&unscaled_stroke, res_scale)?;
        let scale_xf = tiny_skia::Transform::from_row(
            self.scale,
            0.0,
            0.0,
            self.scale_y,
            -(self.dev_origin_x as f32),
            -(self.dev_origin_y as f32),
        );
        let device = stroked.transform(scale_xf)?;
        self.check_path_limits(device)
    }

    /// Dash a 0-width hairline in unscaled user space when `self.scale != self.scale_y`,
    /// then transform the centerline to device space for a 1px device hairline.
    fn build_anisotropic_hairline_path(
        &self,
        path: &Path,
        style: &StrokeStyle,
    ) -> Option<tiny_skia::Path> {
        let Some(dash) = Self::stroke_dash(style, 1.0) else {
            return self.build_skia_path(path);
        };
        let unscaled = self.build_unscaled_skia_path(path)?;
        let res_scale = self.scale.max(self.scale_y).clamp(0.01, 1024.0);
        let dashed = unscaled.dash(&dash, res_scale)?;
        let scale_xf = tiny_skia::Transform::from_row(
            self.scale,
            0.0,
            0.0,
            self.scale_y,
            -(self.dev_origin_x as f32),
            -(self.dev_origin_y as f32),
        );
        let device = dashed.transform(scale_xf)?;
        self.check_path_limits(device)
    }

    /// Record that the current target surface may have been painted inside
    /// `bounds` (device space), grown by `grow` device pixels to cover what the
    /// rasterizer legitimately overshoots the geometry by.
    ///
    /// Called after every successful draw onto the target, never for drawing
    /// into a scratch raster (whose extent the caller re-establishes itself).
    fn mark_painted(&mut self, bounds: tiny_skia::Rect, grow: f32) {
        let Some((width, height)) = self.pixmap.as_ref().map(|p| (p.width(), p.height())) else {
            return;
        };
        let mut region = Self::drawn_extent(bounds, grow, width, height);
        if let Some(clip) = self.clip_bounds {
            region = region.intersect(clip);
        }
        if region.is_empty() {
            // Provably nothing: entirely off-surface, or entirely outside the clip.
            return;
        }
        self.dirty = Some(match self.dirty {
            Some(dirty) => dirty.union(region),
            None => region,
        });
    }

    /// Whether a draw with this geometry can change any pixel of the target:
    /// false when its extent ([`Self::drawn_extent`]) lies entirely off the
    /// surface or outside the active clip, whose mask is zero there.
    fn reaches_target(&self, bounds: tiny_skia::Rect, grow: f32) -> bool {
        let Some((width, height)) = self.pixmap.as_ref().map(|p| (p.width(), p.height())) else {
            return false;
        };
        let mut region = Self::drawn_extent(bounds, grow, width, height);
        if let Some(clip) = self.clip_bounds {
            region = region.intersect(clip);
        }
        !region.is_empty()
    }

    /// Record that the whole target surface may have been painted — for a draw
    /// whose extent cannot be established, or one that merges over the full
    /// raster by construction. Still narrowed by the active clip, which is the
    /// only bound that is known to hold.
    fn mark_all_painted(&mut self) {
        let Some((width, height)) = self.pixmap.as_ref().map(|p| (p.width(), p.height())) else {
            return;
        };
        let mut region = PaintedBounds::whole(width, height);
        if let Some(clip) = self.clip_bounds {
            region = region.intersect(clip);
        }
        if region.is_empty() {
            return;
        }
        self.dirty = Some(match self.dirty {
            Some(dirty) => dirty.union(region),
            None => region,
        });
    }

    /// The conservative device extent of a draw with this geometry: `bounds`
    /// grown by `grow`, clamped to the surface.
    ///
    /// Anything that cannot be bounded — a non-finite coordinate or grow, an
    /// inverted rectangle — degrades to the whole surface, because an extent
    /// that is too small silently drops pixels while one that is too large only
    /// costs time.
    fn drawn_extent(bounds: tiny_skia::Rect, grow: f32, width: u32, height: u32) -> PaintedBounds {
        let (left, top, right, bottom) =
            (bounds.left(), bounds.top(), bounds.right(), bounds.bottom());
        let bounded = left.is_finite()
            && top.is_finite()
            && right.is_finite()
            && bottom.is_finite()
            && grow.is_finite()
            && right >= left
            && bottom >= top;
        if !bounded {
            return PaintedBounds::whole(width, height);
        }
        // Floor/ceil, not round: a pixel whose *cell* the geometry touches
        // receives coverage even when the path's edge lies inside it.
        let grow = grow.max(0.0);
        // Float-to-integer casts saturate at zero and u32::MAX. Apply the
        // surface limit afterwards: converting an integer axis to f32 first
        // can round it either above or below the actual allocated dimension.
        PaintedBounds {
            x0: ((left - grow).floor() as u32).min(width),
            y0: ((top - grow).floor() as u32).min(height),
            x1: ((right + grow).ceil() as u32).min(width),
            y1: ((bottom + grow).ceil() as u32).min(height),
        }
    }

    fn build_skia_path(&self, path: &Path) -> Option<tiny_skia::Path> {
        let mut pb = tiny_skia::PathBuilder::new();
        for elem in path {
            match elem {
                PathElement::MoveTo(p) => {
                    pb.move_to(self.to_pixel_x(p.x as f32), self.flip_y(p.y as f32));
                }
                PathElement::LineTo(p) => {
                    pb.line_to(self.to_pixel_x(p.x as f32), self.flip_y(p.y as f32));
                }
                PathElement::CurveTo(c1, c2, end) => {
                    pb.cubic_to(
                        self.to_pixel_x(c1.x as f32),
                        self.flip_y(c1.y as f32),
                        self.to_pixel_x(c2.x as f32),
                        self.flip_y(c2.y as f32),
                        self.to_pixel_x(end.x as f32),
                        self.flip_y(end.y as f32),
                    );
                }
                PathElement::Close => {
                    pb.close();
                }
            }
        }
        self.finish_within_limits(pb)
    }

    fn color_to_paint(color: &Color, alpha: f32) -> tiny_skia::Paint<'static> {
        let mut paint = tiny_skia::Paint::default();
        paint.set_color_rgba8(
            (color.r * 255.0).round() as u8,
            (color.g * 255.0).round() as u8,
            (color.b * 255.0).round() as u8,
            (color.a * alpha * 255.0).round() as u8,
        );
        paint.anti_alias = true;
        paint
    }

    fn fill_rule_to_skia(rule: &FillRule) -> tiny_skia::FillRule {
        match rule {
            FillRule::NonZero => tiny_skia::FillRule::Winding,
            FillRule::EvenOdd => tiny_skia::FillRule::EvenOdd,
        }
    }

    /// Cached per-page check of `tiling.is_backdrop_dependent()`, keyed by
    /// `Arc::as_ptr(&tiling.commands)` while keeping the `Arc` alive.
    fn tiling_is_backdrop_dependent(&mut self, tiling: &TilingPattern) -> bool {
        let ptr = Arc::as_ptr(&tiling.commands) as *const () as usize;
        if let Some(&(_, dep)) = self.tiling_backdrop_cache.get(&ptr) {
            return dep;
        }
        let dep = tiling.is_backdrop_dependent();
        if self.tiling_backdrop_cache.len() >= 256 {
            self.tiling_backdrop_cache.clear();
        }
        self.tiling_backdrop_cache
            .insert(ptr, (Arc::clone(&tiling.commands), dep));
        dep
    }

    /// Rasterize (or look up in `tiling_cache`) the single cell of `tiling`
    /// into a tile pixmap snapped to whole device pixels, and return the
    /// pattern shader parameters (`pixmap`, `transform`, `quality`, `opacity`).
    fn prepare_tiling_paint(
        &mut self,
        tiling: &TilingPattern,
        alpha: f32,
    ) -> Option<(
        Arc<tiny_skia::Pixmap>,
        tiny_skia::Transform,
        tiny_skia::FilterQuality,
        f32,
    )> {
        self.prepare_tiling_paint_for(
            tiling,
            &tiling.commands,
            0,
            None,
            BlendMode::Normal,
            None,
            alpha,
        )
        .map(|(pixmap, transform, quality, opacity, _)| (pixmap, transform, quality, opacity))
    }

    /// Rasterize (or look up in `tiling_cache`) either the whole cell
    /// (`segment_index == 0`) or one blend-mode slice (`segment_index == i + 1`)
    /// of `tiling` into a tile pixmap snapped to whole device pixels. When
    /// `overlap_shift` is `Some((di, dj))`, only that single neighbor copy is
    /// rasterized so overlapping backdrop-dependent cells can composite each
    /// neighbor copy separately in painter's order. When `overlap_shift` is
    /// `None` and `copy_blend_mode` is an associative per-copy mode
    /// (`Multiply`/`Screen`/`Darken`/`Lighten`), overlapping copies are
    /// composited into the tile with `copy_blend_mode` so repeated per-copy
    /// blending is preserved even when flattening past the pass budget.
    #[allow(clippy::too_many_arguments)]
    fn prepare_tiling_paint_for(
        &mut self,
        tiling: &TilingPattern,
        commands: &DisplayList,
        segment_index: usize,
        overlap_shift: Option<(i64, i64)>,
        copy_blend_mode: BlendMode,
        copy_filter_bounds: Option<zpdf_core::Rect>,
        alpha: f32,
    ) -> Option<PreparedTilingPaint> {
        if self.tiling_depth >= MAX_TILING_DEPTH || commands.commands.is_empty() {
            return None;
        }
        let opacity = unit(alpha);
        if opacity <= 0.0 {
            return None;
        }
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

        // Compose page→device (PDF Y-up → device Y-down) with pattern→page.
        let page_to_dev = zpdf_core::Matrix::new(
            self.scale as f64,
            0.0,
            0.0,
            -(self.scale_y as f64),
            -(self.rect_x0 as f64) * self.scale as f64 - self.dev_origin_x as f64,
            (self.rect_y1 as f64) * self.scale_y as f64 - self.dev_origin_y as f64,
        );
        let pat_to_dev = page_to_dev.concat(&tiling.matrix);
        if ![
            pat_to_dev.a,
            pat_to_dev.b,
            pat_to_dev.c,
            pat_to_dev.d,
            pat_to_dev.e,
            pat_to_dev.f,
        ]
        .iter()
        .all(|v| v.is_finite())
        {
            return None;
        }
        let det = pat_to_dev.a * pat_to_dev.d - pat_to_dev.b * pat_to_dev.c;
        if !det.is_finite() || det.abs() < 1e-12 {
            return None;
        }

        let len_x = (pat_to_dev.a * abs_x).hypot(pat_to_dev.b * abs_x);
        let len_y = (pat_to_dev.c * abs_y).hypot(pat_to_dev.d * abs_y);
        if !len_x.is_finite() || !len_y.is_finite() || len_x <= 0.0 || len_y <= 0.0 {
            return None;
        }

        // Snap the repeating period to whole device pixels within safety caps.
        let mut tw_f = len_x.round().max(1.0);
        let mut th_f = len_y.round().max(1.0);
        let max_dim = tw_f.max(th_f);
        if max_dim > MAX_TILING_TILE_DIM as f64 {
            let s = MAX_TILING_TILE_DIM as f64 / max_dim;
            tw_f = (tw_f * s).floor().max(1.0);
            th_f = (th_f * s).floor().max(1.0);
        }
        if tw_f * th_f > MAX_TILING_TILE_PIXELS as f64 {
            let s = (MAX_TILING_TILE_PIXELS as f64 / (tw_f * th_f)).sqrt();
            tw_f = (tw_f * s).floor().max(1.0);
            th_f = (th_f * s).floor().max(1.0);
        }
        let tile_w = tw_f as u32;
        let tile_h = th_f as u32;

        // Map tile-pixel space [0, tile_w] × [0, tile_h] back to the fundamental
        // pattern-space period [bx0, bx0 + abs_x] × [by0, by0 + abs_y], then to
        // device pixels. Keeps the exact PDF repeat period across huge pages
        // (zero cumulative phase drift) while filling the snapped tile edge-to-edge.
        let tile_to_pat = zpdf_core::Matrix::new(
            abs_x / tile_w as f64,
            0.0,
            0.0,
            -abs_y / tile_h as f64,
            bx0,
            by0 + abs_y,
        );
        let m = pat_to_dev.concat(&tile_to_pat);
        let transform = tiny_skia::Transform::from_row(
            m.a as f32, m.b as f32, m.c as f32, m.d as f32, m.e as f32, m.f as f32,
        );
        if !transform.is_finite() || transform.invert().is_none() {
            return None;
        }
        let exact_1to1 = (m.a - 1.0).abs() < 1e-4
            && m.b.abs() < 1e-4
            && m.c.abs() < 1e-4
            && (m.d - 1.0).abs() < 1e-4
            && (m.e - m.e.round()).abs() < 1e-4
            && (m.f - m.f.round()).abs() < 1e-4;
        let quality = if exact_1to1 {
            tiny_skia::FilterQuality::Nearest
        } else {
            tiny_skia::FilterQuality::Bilinear
        };

        let combine_copies = overlap_shift.is_none()
            && matches!(
                copy_blend_mode,
                BlendMode::Multiply | BlendMode::Screen | BlendMode::Darken | BlendMode::Lighten
            );
        let key_mode = if combine_copies {
            copy_blend_mode
        } else {
            BlendMode::Normal
        };
        let blend_depth = self
            .soft_mask_depth
            .saturating_add(self.blend_stack.len())
            .saturating_add(self.non_isolated_depth);
        let key = TilingCacheKey::new(
            tiling,
            segment_index,
            overlap_shift,
            key_mode,
            tile_w,
            tile_h,
            blend_depth,
        );
        let (tile_pixmap, uniform_opaque) = if let Some(cached) = self.tiling_cache.get(&key) {
            (Arc::clone(&cached.pixmap), cached.uniform_opaque)
        } else {
            let tile_bytes = tile_w as u64 * tile_h as u64 * 4;
            if !self.surface_room(tile_bytes) {
                tracing::warn!("tiling pattern cell {tile_w}x{tile_h} exceeds memory budget");
                return None;
            }
            let target = tiny_skia::Pixmap::new(tile_w, tile_h)?;
            let cell_scale_x = (tile_w as f64 / abs_x) as f32;
            let cell_scale_y = (tile_h as f64 / abs_y) as f32;
            if !cell_scale_x.is_finite()
                || !cell_scale_y.is_finite()
                || cell_scale_x <= 0.0
                || cell_scale_y <= 0.0
            {
                return None;
            }

            // When `/BBox` exceeds `|XStep|` or `|YStep|`, draw every neighbor
            // copy whose painted ink overlaps the fundamental period (or just
            // `overlap_shift` when compositing overlapping blend copies).
            let (di_range, dj_range) = match overlap_shift {
                Some((di, dj)) => (di..=di, dj..=dj),
                None => {
                    let overlap = tiling.overlap_copy_ranges();
                    if overlap.truncated {
                        tracing::warn!(
                            "tiling pattern overlapping cell copies exceed {}; clamping",
                            zpdf_display_list::MAX_TILING_OVERLAP_COPIES
                        );
                    }
                    (overlap.di, overlap.dj)
                }
            };

            let mut sub = CpuRenderer {
                pixmap: Some(target),
                dirty: None,
                scale: cell_scale_x,
                scale_y: cell_scale_y,
                rect_x0: bx0 as f32,
                rect_y0: by0 as f32,
                rect_y1: (by0 + abs_y) as f32,
                dev_origin_x: 0,
                dev_origin_y: 0,
                font_cache: self.font_cache,
                image_cache: self.image_cache,
                max_page_pixels: self.max_page_pixels,
                max_blend_group_depth: self.max_blend_group_depth,
                max_soft_mask_cache_bytes: self.max_soft_mask_cache_bytes,
                parse_limits: self.parse_limits.clone(),
                clip_stack: Vec::new(),
                current_clip: None,
                clip_bounds: None,
                clip_pixel_spent: 0,
                clip_mask_bytes: 0,
                mask_pool: Vec::new(),
                blend_stack: Vec::new(),
                recording_non_isolated: None,
                non_isolated_depth: 0,
                alpha_only_pass: false,
                shape_only_pass: false,
                blend_surface_bytes: self
                    .blend_surface_bytes
                    .saturating_add(self.pooled_surface_bytes)
                    .saturating_add(tile_bytes),
                surface_pool: Vec::new(),
                pooled_surface_bytes: 0,
                skipped_blend_depth: 0,
                render_budget: self.render_budget,
                deadline: self.deadline,
                over_budget: self.over_budget,
                soft_mask_planes: HashMap::new(),
                soft_mask_cache_bytes: 0,
                soft_mask_depth: blend_depth,
                tiling_cache: HashMap::new(),
                tiling_cache_bytes: 0,
                tiling_backdrop_cache: HashMap::new(),
                tiling_depth: self.tiling_depth.saturating_add(1),
                downscaled_images: HashMap::new(),
                glyph_outlines: HashMap::new(),
                glyph_outline_commands: 0,
                type3_glyphs: HashMap::new(),
                type3_glyph_commands: 0,
                type3_glyph_image_bytes: 0,
                stage_stats: None,
                page_start: None,
            };

            let mut drew_any = false;
            for dj in dj_range {
                for di in di_range.clone() {
                    if sub.over_budget {
                        break;
                    }
                    if let Some(b) = copy_filter_bounds {
                        let dx = di as f64 * x_step;
                        let dy = dj as f64 * y_step;
                        if b.x1 + dx <= bx0 + 1e-9
                            || b.x0 + dx >= bx0 + abs_x - 1e-9
                            || b.y1 + dy <= by0 + 1e-9
                            || b.y0 + dy >= by0 + abs_y - 1e-9
                        {
                            continue;
                        }
                    }
                    // Soft-mask planes are rasterized in `sub`'s device coordinates
                    // (`rect_x0`, `rect_y1`); invalidate before shifting the origin.
                    if !sub.soft_mask_planes.is_empty() {
                        sub.soft_mask_planes.clear();
                        sub.soft_mask_cache_bytes = 0;
                    }
                    sub.rect_x0 = (bx0 - di as f64 * x_step) as f32;
                    sub.rect_y0 = (by0 - dj as f64 * y_step) as f32;
                    sub.rect_y1 = (by0 + abs_y - dj as f64 * y_step) as f32;
                    let wrapped = combine_copies && drew_any;
                    if wrapped {
                        sub.push_blend_group(copy_blend_mode, true, false, 1.0, None);
                    }
                    let base_blends = sub.blend_stack.len();
                    for cmd in &commands.commands {
                        let _ = sub.execute(cmd);
                    }
                    while !sub.clip_stack.is_empty() {
                        sub.pop_clip();
                    }
                    while sub.blend_stack.len() > base_blends {
                        sub.pop_blend_group();
                    }
                    if wrapped {
                        if sub.skipped_blend_depth > 0 {
                            sub.skipped_blend_depth -= 1;
                        } else {
                            sub.pop_blend_group();
                        }
                    }
                    drew_any = true;
                }
            }
            if sub.over_budget {
                self.over_budget = true;
            }
            let rendered = Arc::new(sub.pixmap.take()?);
            let uniform_opaque = pixmap_uniform_opaque(&rendered);
            if !sub.over_budget && tile_bytes <= MAX_TILING_CACHE_BYTES {
                if self.tiling_cache_bytes.saturating_add(tile_bytes) > MAX_TILING_CACHE_BYTES {
                    self.tiling_cache.clear();
                    self.tiling_cache_bytes = 0;
                }
                self.tiling_cache.insert(
                    key,
                    CachedTilingPixmap {
                        _commands: Arc::clone(&tiling.commands),
                        pixmap: Arc::clone(&rendered),
                        uniform_opaque,
                    },
                );
                self.tiling_cache_bytes = self.tiling_cache_bytes.saturating_add(tile_bytes);
            }
            (rendered, uniform_opaque)
        };

        Some((tile_pixmap, transform, quality, opacity, uniform_opaque))
    }

    /// Collapse an affine overlapping dense backdrop-dependent tiling pattern
    /// whose per-copy pass count exceeds [`MAX_DENSE_TILING_BLEND_PASSES`] into
    /// three repeating sample tiles `(tile_trans, tile0, tile1)` evaluated in
    /// **exact per-copy draw order** onto transparent (`(T, a_t)`), opaque black
    /// (`K`), and opaque white (`K + M`). Applying:
    ///
    /// - `out_a = a_t + (1 - a_t) * B_a`
    /// - `out_rgb = (1 - B_a) * T + B_a * K + M * B_rgb`
    ///
    /// reproduces any sequence of `Normal` / `Multiply` / `Screen` /
    /// `OpacityGroup` steps in a single pass across arbitrary target alpha
    /// (transparent, translucent, and opaque) without reordering copies.
    /// Returns `None` without caching if pass or wall-clock budgets are
    /// exhausted so partial tiles are never cached as complete.
    fn prepare_tiling_affine_tiles(
        &mut self,
        tiling: &TilingPattern,
        steps: &[TilingBlendStep],
    ) -> Option<PreparedAffineTiles> {
        const MAX_AFFINE_TILE_PASSES: usize = 65_536;
        if self.tiling_depth >= MAX_TILING_DEPTH || steps.is_empty() {
            return None;
        }
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
        let page_to_dev = zpdf_core::Matrix::new(
            self.scale as f64,
            0.0,
            0.0,
            -(self.scale_y as f64),
            -(self.rect_x0 as f64) * self.scale as f64 - self.dev_origin_x as f64,
            (self.rect_y1 as f64) * self.scale_y as f64 - self.dev_origin_y as f64,
        );
        let pat_to_dev = page_to_dev.concat(&tiling.matrix);
        if ![
            pat_to_dev.a,
            pat_to_dev.b,
            pat_to_dev.c,
            pat_to_dev.d,
            pat_to_dev.e,
            pat_to_dev.f,
        ]
        .iter()
        .all(|v| v.is_finite())
        {
            return None;
        }
        let det = pat_to_dev.a * pat_to_dev.d - pat_to_dev.b * pat_to_dev.c;
        if !det.is_finite() || det.abs() < 1e-12 {
            return None;
        }
        let len_x = (pat_to_dev.a * abs_x).hypot(pat_to_dev.b * abs_x);
        let len_y = (pat_to_dev.c * abs_y).hypot(pat_to_dev.d * abs_y);
        if !len_x.is_finite() || !len_y.is_finite() || len_x <= 0.0 || len_y <= 0.0 {
            return None;
        }
        let mut tw_f = len_x.round().max(1.0);
        let mut th_f = len_y.round().max(1.0);
        let max_dim = tw_f.max(th_f);
        if max_dim > MAX_TILING_TILE_DIM as f64 {
            let s = MAX_TILING_TILE_DIM as f64 / max_dim;
            tw_f = (tw_f * s).floor().max(1.0);
            th_f = (th_f * s).floor().max(1.0);
        }
        if tw_f * th_f > MAX_TILING_TILE_PIXELS as f64 {
            let s = (MAX_TILING_TILE_PIXELS as f64 / (tw_f * th_f)).sqrt();
            tw_f = (tw_f * s).floor().max(1.0);
            th_f = (th_f * s).floor().max(1.0);
        }
        let tile_w = tw_f as u32;
        let tile_h = th_f as u32;

        let tile_to_pat = zpdf_core::Matrix::new(
            abs_x / tile_w as f64,
            0.0,
            0.0,
            -abs_y / tile_h as f64,
            bx0,
            by0 + abs_y,
        );
        let m = pat_to_dev.concat(&tile_to_pat);
        let transform = tiny_skia::Transform::from_row(
            m.a as f32, m.b as f32, m.c as f32, m.d as f32, m.e as f32, m.f as f32,
        );
        if !transform.is_finite() || transform.invert().is_none() {
            return None;
        }
        let exact_1to1 = (m.a - 1.0).abs() < 1e-4
            && m.b.abs() < 1e-4
            && m.c.abs() < 1e-4
            && (m.d - 1.0).abs() < 1e-4
            && (m.e - m.e.round()).abs() < 1e-4
            && (m.f - m.f.round()).abs() < 1e-4;
        let quality = if exact_1to1 {
            tiny_skia::FilterQuality::Nearest
        } else {
            tiny_skia::FilterQuality::Bilinear
        };

        let blend_depth = self
            .soft_mask_depth
            .saturating_add(self.blend_stack.len())
            .saturating_add(self.non_isolated_depth);
        let trans_key = TilingCacheKey::new(
            tiling,
            usize::MAX - 2,
            None,
            BlendMode::Normal,
            tile_w,
            tile_h,
            blend_depth,
        );
        let k_key = TilingCacheKey::new(
            tiling,
            usize::MAX - 1,
            None,
            BlendMode::Multiply,
            tile_w,
            tile_h,
            blend_depth,
        );
        let f1_key = TilingCacheKey::new(
            tiling,
            usize::MAX,
            None,
            BlendMode::Screen,
            tile_w,
            tile_h,
            blend_depth,
        );
        if let (Some(trans_entry), Some(k_entry), Some(f1_entry)) = (
            self.tiling_cache.get(&trans_key),
            self.tiling_cache.get(&k_key),
            self.tiling_cache.get(&f1_key),
        ) {
            return Some(PreparedAffineTiles {
                trans: Arc::clone(&trans_entry.pixmap),
                k: Arc::clone(&k_entry.pixmap),
                f1: Arc::clone(&f1_entry.pixmap),
                transform,
                quality,
            });
        }

        let tile_bytes = tile_w as u64 * tile_h as u64 * 4;
        if !self.surface_room(tile_bytes.saturating_mul(3)) {
            return None;
        }
        let cell_scale_x = (tile_w as f64 / abs_x) as f32;
        let cell_scale_y = (tile_h as f64 / abs_y) as f32;
        if !cell_scale_x.is_finite()
            || !cell_scale_y.is_finite()
            || cell_scale_x <= 0.0
            || cell_scale_y <= 0.0
        {
            return None;
        }

        let overlap = tiling.overlap_copy_ranges();
        let make_sub = |target: tiny_skia::Pixmap| CpuRenderer {
            pixmap: Some(target),
            dirty: None,
            scale: cell_scale_x,
            scale_y: cell_scale_y,
            rect_x0: bx0 as f32,
            rect_y0: by0 as f32,
            rect_y1: (by0 + abs_y) as f32,
            dev_origin_x: 0,
            dev_origin_y: 0,
            font_cache: self.font_cache,
            image_cache: self.image_cache,
            max_page_pixels: self.max_page_pixels,
            max_blend_group_depth: self.max_blend_group_depth,
            max_soft_mask_cache_bytes: self.max_soft_mask_cache_bytes,
            parse_limits: self.parse_limits.clone(),
            clip_stack: Vec::new(),
            current_clip: None,
            clip_bounds: None,
            clip_pixel_spent: 0,
            clip_mask_bytes: 0,
            mask_pool: Vec::new(),
            blend_stack: Vec::new(),
            recording_non_isolated: None,
            non_isolated_depth: 0,
            alpha_only_pass: false,
            shape_only_pass: false,
            blend_surface_bytes: self
                .blend_surface_bytes
                .saturating_add(self.pooled_surface_bytes)
                .saturating_add(tile_bytes.saturating_mul(3)),
            surface_pool: Vec::new(),
            pooled_surface_bytes: 0,
            skipped_blend_depth: 0,
            render_budget: self.render_budget,
            deadline: self.deadline,
            over_budget: self.over_budget,
            soft_mask_planes: HashMap::new(),
            soft_mask_cache_bytes: 0,
            soft_mask_depth: blend_depth,
            tiling_cache: HashMap::new(),
            tiling_cache_bytes: 0,
            tiling_backdrop_cache: HashMap::new(),
            tiling_depth: self.tiling_depth.saturating_add(1),
            downscaled_images: HashMap::new(),
            glyph_outlines: HashMap::new(),
            glyph_outline_commands: 0,
            type3_glyphs: HashMap::new(),
            type3_glyph_commands: 0,
            type3_glyph_image_bytes: 0,
            stage_stats: None,
            page_start: None,
        };

        #[allow(clippy::too_many_arguments)]
        fn exec_steps_on_tile(
            sub: &mut CpuRenderer,
            tiling: &TilingPattern,
            steps: &[TilingBlendStep],
            di: i64,
            dj: i64,
            seed_is_opaque: bool,
            remaining: &mut usize,
            exhausted: &mut bool,
        ) {
            for step in steps {
                if sub.over_budget || *exhausted {
                    return;
                }
                match step {
                    TilingBlendStep::Segment(seg) => {
                        if !seg.intersects_period(tiling, di, dj) {
                            continue;
                        }
                        if seed_is_opaque && seg.is_identity_on_opaque() {
                            continue;
                        }
                        if *remaining == 0 {
                            *exhausted = true;
                            return;
                        }
                        *remaining -= 1;
                        let wrap = seg.blend_mode != BlendMode::Normal;
                        if wrap {
                            sub.push_blend_group(seg.blend_mode, true, false, 1.0, None);
                        }
                        let base_blends = sub.blend_stack.len();
                        let base_clips = sub.clip_stack.len();
                        for cmd in &seg.commands.commands {
                            if sub.over_budget {
                                break;
                            }
                            let _ = sub.execute(cmd);
                        }
                        while sub.clip_stack.len() > base_clips {
                            sub.pop_clip();
                        }
                        while sub.blend_stack.len() > base_blends {
                            sub.pop_blend_group();
                        }
                        if wrap {
                            if sub.skipped_blend_depth > 0 {
                                sub.skipped_blend_depth -= 1;
                            } else {
                                sub.pop_blend_group();
                            }
                        }
                    }
                    TilingBlendStep::OpacityGroup {
                        alpha,
                        mask,
                        steps: inner,
                    } => {
                        if mask
                            .as_ref()
                            .is_some_and(|m| !m.intersects_period(tiling, di, dj))
                            || !TilingBlendStep::any_intersects_period(inner, tiling, di, dj)
                        {
                            continue;
                        }
                        let a = unit(*alpha);
                        if a <= 0.0 {
                            continue;
                        }
                        let whole = sub
                            .pixmap
                            .as_ref()
                            .map(|p| PaintedBounds::whole(p.width(), p.height()));
                        if let Some(m_seg) = mask {
                            let _ = sub.with_backdrop_shape_coverage(
                                a,
                                whole,
                                |r, _| {
                                    for cmd in &m_seg.commands.commands {
                                        let _ = r.execute(cmd);
                                    }
                                },
                                |r, _| {
                                    exec_steps_on_tile(
                                        r,
                                        tiling,
                                        inner,
                                        di,
                                        dj,
                                        seed_is_opaque,
                                        remaining,
                                        exhausted,
                                    );
                                    true
                                },
                            );
                        } else {
                            let _ = sub.with_backdrop_opacity(a, whole, |r| {
                                exec_steps_on_tile(
                                    r,
                                    tiling,
                                    inner,
                                    di,
                                    dj,
                                    seed_is_opaque,
                                    remaining,
                                    exhausted,
                                );
                                true
                            });
                        }
                    }
                }
            }
        }

        let run_sample =
            |seed: Option<tiny_skia::Color>| -> Option<(tiny_skia::Pixmap, bool, bool)> {
                let mut target = tiny_skia::Pixmap::new(tile_w, tile_h)?;
                let seed_is_opaque = if let Some(c) = seed {
                    target.fill(c);
                    true
                } else {
                    false
                };
                let mut sub = make_sub(target);
                let mut rem = MAX_AFFINE_TILE_PASSES;
                let mut exhausted = false;
                'copies: for dj in overlap.dj.clone() {
                    for di in overlap.di.clone() {
                        if sub.over_budget || exhausted {
                            break 'copies;
                        }
                        if !sub.soft_mask_planes.is_empty() {
                            sub.soft_mask_planes.clear();
                            sub.soft_mask_cache_bytes = 0;
                        }
                        sub.rect_x0 = (bx0 - di as f64 * x_step) as f32;
                        sub.rect_y0 = (by0 - dj as f64 * y_step) as f32;
                        sub.rect_y1 = (by0 + abs_y - dj as f64 * y_step) as f32;
                        exec_steps_on_tile(
                            &mut sub,
                            tiling,
                            steps,
                            di,
                            dj,
                            seed_is_opaque,
                            &mut rem,
                            &mut exhausted,
                        );
                    }
                }
                let ob = sub.over_budget;
                Some((sub.pixmap.take()?, ob, exhausted))
            };

        let (tile_trans, obt, ext) = run_sample(None)?;
        let (tile0, ob0, ex0) = run_sample(Some(tiny_skia::Color::BLACK))?;
        let (tile1, ob1, ex1) = run_sample(Some(tiny_skia::Color::WHITE))?;
        if obt || ob0 || ob1 {
            self.over_budget = true;
            return None;
        }
        if ext || ex0 || ex1 {
            tracing::warn!(
                limit = MAX_AFFINE_TILE_PASSES,
                "dense CPU affine tiling sample exceeded pass budget; declining partial tile"
            );
            return None;
        }

        let trans_arc = Arc::new(tile_trans);
        let k_arc = Arc::new(tile0);
        let f1_arc = Arc::new(tile1);
        let cache_bytes = tile_bytes.saturating_mul(3);
        if cache_bytes <= MAX_TILING_CACHE_BYTES {
            if self.tiling_cache_bytes.saturating_add(cache_bytes) > MAX_TILING_CACHE_BYTES {
                self.tiling_cache.clear();
                self.tiling_cache_bytes = 0;
            }
            self.tiling_cache.insert(
                trans_key,
                CachedTilingPixmap {
                    _commands: Arc::clone(&tiling.commands),
                    pixmap: Arc::clone(&trans_arc),
                    uniform_opaque: None,
                },
            );
            self.tiling_cache.insert(
                k_key,
                CachedTilingPixmap {
                    _commands: Arc::clone(&tiling.commands),
                    pixmap: Arc::clone(&k_arc),
                    uniform_opaque: None,
                },
            );
            self.tiling_cache.insert(
                f1_key,
                CachedTilingPixmap {
                    _commands: Arc::clone(&tiling.commands),
                    pixmap: Arc::clone(&f1_arc),
                    uniform_opaque: None,
                },
            );
            self.tiling_cache_bytes = self.tiling_cache_bytes.saturating_add(cache_bytes);
        }
        Some(PreparedAffineTiles {
            trans: trans_arc,
            k: k_arc,
            f1: f1_arc,
            transform,
            quality,
        })
    }

    fn tiling_is_non_overlapping(tiling: &TilingPattern) -> bool {
        let bbox = tiling.bbox.normalize();
        bbox.width() <= tiling.x_step.abs() + 1e-9 && bbox.height() <= tiling.y_step.abs() + 1e-9
    }

    /// Conservative page-space bounds of `path`.
    fn page_path_bounds(path: &Path) -> Option<zpdf_core::Rect> {
        let (mut x0, mut y0, mut x1, mut y1) = (
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        );
        for p in path.points() {
            if !p.x.is_finite() || !p.y.is_finite() {
                return None;
            }
            x0 = x0.min(p.x);
            y0 = y0.min(p.y);
            x1 = x1.max(p.x);
            y1 = y1.max(p.y);
        }
        (x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite())
            .then(|| zpdf_core::Rect::new(x0, y0, x1, y1))
    }

    /// Evaluate a backdrop-dependent pattern draw at full outer opacity (`1.0`)
    /// against a copy of the live destination backdrop, then apply the object's
    /// constant `alpha` ONCE via the exact premultiplied non-isolated group
    /// identity `dst = (1 - opacity) * backdrop + opacity * result`
    /// (ISO 32000-1 §11.4.5 / §11.6.7). When `opacity >= 1.0`, evaluates
    /// directly onto `self.pixmap` with zero extra copies.
    fn with_backdrop_opacity(
        &mut self,
        alpha: f32,
        scope_region: Option<PaintedBounds>,
        draw: impl FnOnce(&mut Self) -> bool,
    ) -> bool {
        let opacity = unit(alpha);
        if opacity <= 0.0 {
            return false;
        }
        if opacity >= 1.0 {
            return draw(self);
        }
        let Some(mut backdrop) = self.pixmap.take() else {
            return false;
        };
        let (w, h) = (backdrop.width(), backdrop.height());
        let copy_region = match (scope_region, self.clip_bounds) {
            (Some(r), Some(c)) => r.intersect(c),
            (Some(r), None) => r,
            (None, Some(c)) => c,
            (None, None) => PaintedBounds::whole(w, h),
        };
        if copy_region.is_empty() {
            self.pixmap = Some(backdrop);
            return false;
        }
        let surface_bytes = w as u64 * h as u64 * 4;
        let pooled = self
            .surface_pool
            .iter()
            .any(|p| (p.width(), p.height()) == (w, h));
        if !pooled && !self.surface_room(surface_bytes) {
            self.pixmap = Some(backdrop);
            tracing::warn!("tiling backdrop scratch exceeds memory budget; drawing directly");
            return draw(self);
        }
        let Some(mut scratch) = self.take_surface(w, h) else {
            self.pixmap = Some(backdrop);
            return draw(self);
        };

        // Seed `scratch` with the live backdrop across `copy_region` so
        // non-Normal blend modes / overprint inside the cell see the exact
        // destination pixels while evaluating at opacity 1.0.
        for_region_rows(copy_region, w, |range| {
            scratch.data_mut()[range.clone()].copy_from_slice(&backdrop.data()[range]);
        });

        let backdrop_dirty = self.dirty.take();
        self.pixmap = Some(scratch);
        self.blend_surface_bytes = self.blend_surface_bytes.saturating_add(surface_bytes);

        let painted = draw(self);

        let scratch = self.pixmap.take();
        let draw_dirty = self.dirty.take();
        self.blend_surface_bytes = self.blend_surface_bytes.saturating_sub(surface_bytes);

        if let Some(scratch) = scratch {
            let merge_region = draw_dirty
                .map(|r| r.intersect(copy_region))
                .filter(|r| !r.is_empty());
            if let Some(region) = merge_region {
                let inv = 1.0 - opacity;
                for_region_rows(region, w, |range| {
                    let dst = &mut backdrop.data_mut()[range.clone()];
                    let src = &scratch.data()[range];
                    for (d, s) in dst.iter_mut().zip(src.iter()) {
                        *d = (*d as f32 * inv + *s as f32 * opacity)
                            .round()
                            .clamp(0.0, 255.0) as u8;
                    }
                });
                self.dirty = Some(match backdrop_dirty {
                    Some(b) => b.union(region),
                    None => region,
                });
            } else {
                self.dirty = backdrop_dirty;
            }
            let recycle_region = match draw_dirty {
                Some(d) if !d.is_empty() => copy_region.union(d),
                _ => copy_region,
            };
            self.recycle_surface(scratch, Some(recycle_region));
        } else {
            self.dirty = backdrop_dirty;
        }
        self.pixmap = Some(backdrop);
        painted
    }

    /// Evaluate a backdrop-dependent tiling pattern at full coverage (`1.0`)
    /// onto a backdrop-seeded scratch surface, while rasterizing the outer
    /// shape (fill path, stroke outline, or glyph run) ONCE into a coverage
    /// surface, then merge the completed pattern back into `self.pixmap` ONCE
    /// with `w = opacity * shape_coverage * clip_coverage`
    /// (ISO 32000-1 §11.4.5 / §11.6.7). Prevents anti-aliased outer edges from
    /// double-blending across multiple segments, overlapping copies, or tiles.
    ///
    /// Scratch surfaces are sized to the clipped affected region (and split
    /// into bands when large) so even a small patterned mark or a full-page
    /// mark on a large raster (e.g. 7,000 × 7,000) stays well inside
    /// [`MAX_BLEND_SURFACE_BYTES`].
    fn with_backdrop_shape_coverage(
        &mut self,
        alpha: f32,
        scope_region: Option<PaintedBounds>,
        rasterize_shape: impl FnMut(&mut Self, tiny_skia::Transform),
        draw_pattern: impl FnMut(&mut Self, PaintedBounds) -> bool,
    ) -> bool {
        self.with_backdrop_shape_coverage_budgeted(
            alpha,
            scope_region,
            MAX_COVERAGE_BAND_BYTES,
            rasterize_shape,
            draw_pattern,
        )
    }

    fn with_backdrop_shape_coverage_budgeted(
        &mut self,
        alpha: f32,
        scope_region: Option<PaintedBounds>,
        max_band_bytes: u64,
        mut rasterize_shape: impl FnMut(&mut Self, tiny_skia::Transform),
        mut draw_pattern: impl FnMut(&mut Self, PaintedBounds) -> bool,
    ) -> bool {
        let opacity = unit(alpha);
        if opacity <= 0.0 {
            return false;
        }
        let Some(mut backdrop) = self.pixmap.take() else {
            return false;
        };
        let (w, h) = (backdrop.width(), backdrop.height());
        let initial_region = match (scope_region, self.clip_bounds) {
            (Some(r), Some(c)) => r.intersect(c),
            (Some(r), None) => r,
            (None, Some(c)) => c,
            (None, None) => PaintedBounds::whole(w, h),
        };
        if initial_region.is_empty() {
            self.pixmap = Some(backdrop);
            return false;
        }

        let rw = initial_region.x1 - initial_region.x0;
        let rh = initial_region.y1 - initial_region.y0;
        let max_live_avail = MAX_BLEND_SURFACE_BYTES.saturating_sub(self.blend_surface_bytes);
        if max_live_avail < 8 {
            self.pixmap = Some(backdrop);
            tracing::warn!("tiling shape coverage scratch exceeds memory budget");
            return false;
        }
        // Leave headroom inside `MAX_BLEND_SURFACE_BYTES` for nested opacity
        // groups and cell tiles drawn inside the band.
        let cap = max_band_bytes.max(8);
        let preferred = (max_live_avail / 2).min(cap);
        let band_budget = if preferred >= 8 {
            preferred
        } else {
            max_live_avail.min(cap)
        };
        let max_band_pixels = (band_budget / 8).max(1);
        let (band_w, band_h) = if (rw as u64) <= max_band_pixels {
            (rw, (max_band_pixels / rw as u64).clamp(1, rh as u64) as u32)
        } else {
            (max_band_pixels.min(rw as u64) as u32, 1)
        };

        let backdrop_dirty = self.dirty.take();
        let saved_clip = self.current_clip.take();
        let saved_clip_bounds = self.clip_bounds.take();
        let saved_clip_stack = std::mem::take(&mut self.clip_stack);
        let saved_mask_pool = std::mem::take(&mut self.mask_pool);
        let saved_soft_masks = std::mem::take(&mut self.soft_mask_planes);
        let saved_soft_mask_bytes = std::mem::replace(&mut self.soft_mask_cache_bytes, 0);
        let saved_ox = self.dev_origin_x;
        let saved_oy = self.dev_origin_y;

        let mut shape_buf: Option<tiny_skia::Pixmap> = None;
        let mut scratch: Option<tiny_skia::Pixmap> = None;
        let mut live_bytes: u64 = 0;
        let mut any_painted = false;
        let mut merged_dirty: Option<PaintedBounds> = None;

        'bands: for by0 in (initial_region.y0..initial_region.y1).step_by(band_h as usize) {
            let by1 = (by0 + band_h).min(initial_region.y1);
            let cur_h = by1 - by0;
            for bx0 in (initial_region.x0..initial_region.x1).step_by(band_w as usize) {
                if self.over_budget {
                    break 'bands;
                }
                let bx1 = (bx0 + band_w).min(initial_region.x1);
                let cur_w = bx1 - bx0;

                // Reuse the band pair across equal-sized bands; reallocate only
                // on the first band or a narrower/shorter trailing edge band.
                let dims_match = shape_buf
                    .as_ref()
                    .is_some_and(|p| p.width() == cur_w && p.height() == cur_h)
                    && scratch
                        .as_ref()
                        .is_some_and(|p| p.width() == cur_w && p.height() == cur_h);
                if dims_match {
                    if let Some(ref mut sb) = shape_buf {
                        sb.data_mut().fill(0);
                    }
                    if let Some(ref mut sc) = scratch {
                        sc.data_mut().fill(0);
                    }
                } else {
                    shape_buf = None;
                    scratch = None;
                    self.blend_surface_bytes = self.blend_surface_bytes.saturating_sub(live_bytes);
                    live_bytes = 0;
                    let pair_bytes = cur_w as u64 * cur_h as u64 * 8;
                    if !self.surface_room(pair_bytes) {
                        tracing::warn!("tiling shape coverage band exceeds memory budget");
                        break 'bands;
                    }
                    let Some(sb) = tiny_skia::Pixmap::new(cur_w, cur_h) else {
                        break 'bands;
                    };
                    let Some(sc) = tiny_skia::Pixmap::new(cur_w, cur_h) else {
                        break 'bands;
                    };
                    self.blend_surface_bytes = self.blend_surface_bytes.saturating_add(pair_bytes);
                    live_bytes = pair_bytes;
                    shape_buf = Some(sb);
                    scratch = Some(sc);
                }

                let Some(sbuf) = shape_buf.take() else {
                    break 'bands;
                };
                let Some(mut sc) = scratch.take() else {
                    shape_buf = Some(sbuf);
                    break 'bands;
                };

                self.dev_origin_x = saved_ox.saturating_add(bx0);
                self.dev_origin_y = saved_oy.saturating_add(by0);
                // Band-local soft-mask planes are origin-dependent.
                if !self.soft_mask_planes.is_empty() {
                    self.soft_mask_planes.clear();
                    self.soft_mask_cache_bytes = 0;
                }

                // 1. Rasterize the outer shape into `shape_buf` (unclipped;
                //    `saved_clip` is folded once in step 5 with zero extra mask copy).
                self.pixmap = Some(sbuf);
                self.dirty = None;
                let band_offset =
                    tiny_skia::Transform::from_translate(-(bx0 as f32), -(by0 as f32));
                rasterize_shape(self, band_offset);
                let Some(sbuf) = self.pixmap.take() else {
                    scratch = Some(sc);
                    break 'bands;
                };
                let _ = self.dirty.take();

                // 2. Locate exact tight bounds in band coordinates where both
                //    outer shape coverage and active clip coverage are non-zero.
                let s_data = sbuf.data();
                let mut min_x = cur_w;
                let mut min_y = cur_h;
                let mut max_x = 0u32;
                let mut max_y = 0u32;
                let mut found = false;
                let page_w = w as usize;
                let bw_usize = cur_w as usize;
                for ly in 0..cur_h {
                    let s_row = ly as usize * bw_usize;
                    let c_row = (by0 + ly) as usize * page_w;
                    for lx in 0..cur_w {
                        if s_data[(s_row + lx as usize) * 4 + 3] == 0 {
                            continue;
                        }
                        if let Some(ref cm) = saved_clip {
                            if cm.data()[c_row + (bx0 + lx) as usize] == 0 {
                                continue;
                            }
                        }
                        found = true;
                        min_x = min_x.min(lx);
                        min_y = min_y.min(ly);
                        max_x = max_x.max(lx);
                        max_y = max_y.max(ly);
                    }
                }
                if !found {
                    shape_buf = Some(sbuf);
                    scratch = Some(sc);
                    continue;
                }
                let active_local = PaintedBounds {
                    x0: min_x,
                    y0: min_y,
                    x1: max_x + 1,
                    y1: max_y + 1,
                };

                // 3. Seed `scratch` from `backdrop` over `active_local`.
                let row_bytes = (active_local.x1 - active_local.x0) as usize * 4;
                let backdrop_data = backdrop.data();
                let sc_data = sc.data_mut();
                for ly in active_local.y0..active_local.y1 {
                    let dst_off = (ly as usize * bw_usize + active_local.x0 as usize) * 4;
                    let src_off =
                        ((by0 + ly) as usize * page_w + (bx0 + active_local.x0) as usize) * 4;
                    sc_data[dst_off..dst_off + row_bytes]
                        .copy_from_slice(&backdrop_data[src_off..src_off + row_bytes]);
                }

                // 4. Evaluate the full pattern onto `scratch` at 1.0 coverage.
                self.pixmap = Some(sc);
                self.dirty = None;
                let band_painted = draw_pattern(self, active_local);
                let Some(sc) = self.pixmap.take() else {
                    shape_buf = Some(sbuf);
                    break 'bands;
                };
                let _ = self.dirty.take();

                // 5. Merge `scratch` back into `backdrop` ONCE with
                //    `w_eff = opacity * mul255(shape_alpha, clip_alpha) / 255`.
                if band_painted {
                    any_painted = true;
                    let src_data = sc.data();
                    let dst_data = backdrop.data_mut();
                    for ly in active_local.y0..active_local.y1 {
                        let s_row = ly as usize * bw_usize;
                        let d_row = (by0 + ly) as usize * page_w;
                        for lx in active_local.x0..active_local.x1 {
                            let s_base = (s_row + lx as usize) * 4;
                            let s_a = s_data[s_base + 3];
                            if s_a == 0 {
                                continue;
                            }
                            let page_x = (bx0 + lx) as usize;
                            let cov = match saved_clip.as_ref() {
                                Some(cm) => mul255(s_a, cm.data()[d_row + page_x]),
                                None => s_a,
                            };
                            if cov == 0 {
                                continue;
                            }
                            let w_eff = opacity * (cov as f32 / 255.0);
                            let d_base = (d_row + page_x) * 4;
                            if w_eff >= 1.0 {
                                dst_data[d_base..d_base + 4]
                                    .copy_from_slice(&src_data[s_base..s_base + 4]);
                            } else {
                                let inv = 1.0 - w_eff;
                                for ch in 0..4 {
                                    dst_data[d_base + ch] = (dst_data[d_base + ch] as f32 * inv
                                        + src_data[s_base + ch] as f32 * w_eff)
                                        .round()
                                        .clamp(0.0, 255.0)
                                        as u8;
                                }
                            }
                        }
                    }
                    let page_active = PaintedBounds {
                        x0: bx0 + active_local.x0,
                        y0: by0 + active_local.y0,
                        x1: bx0 + active_local.x1,
                        y1: by0 + active_local.y1,
                    };
                    merged_dirty = Some(match merged_dirty {
                        Some(m) => m.union(page_active),
                        None => page_active,
                    });
                }

                shape_buf = Some(sbuf);
                scratch = Some(sc);
            }
        }

        drop(shape_buf);
        drop(scratch);
        self.blend_surface_bytes = self.blend_surface_bytes.saturating_sub(live_bytes);

        // Drop any band-sized surfaces left in `surface_pool` by nested
        // `with_backdrop_opacity` calls so they do not occupy page pool slots.
        let mut kept_bytes = 0u64;
        self.surface_pool.retain(|p| {
            let keep = p.width() == w && p.height() == h;
            if keep {
                kept_bytes = kept_bytes.saturating_add(surface_bytes(p));
            }
            keep
        });
        self.pooled_surface_bytes = kept_bytes;

        // Drop any band-local clip masks and restore the caller's clip/origin state.
        let band_mask_bytes: u64 = self
            .mask_pool
            .iter()
            .map(|m| m.width() as u64 * m.height() as u64)
            .sum();
        self.clip_mask_bytes = self.clip_mask_bytes.saturating_sub(band_mask_bytes);
        self.mask_pool = saved_mask_pool;
        self.clip_stack = saved_clip_stack;
        self.current_clip = saved_clip;
        self.clip_bounds = saved_clip_bounds;
        self.soft_mask_planes = saved_soft_masks;
        self.soft_mask_cache_bytes = saved_soft_mask_bytes;
        self.dev_origin_x = saved_ox;
        self.dev_origin_y = saved_oy;

        self.dirty = match (backdrop_dirty, merged_dirty) {
            (Some(b), Some(m)) => Some(b.union(m)),
            (b, m) => b.or(m),
        };
        self.pixmap = Some(backdrop);
        any_painted
    }

    /// Execute a [`TilingBlendStep`] tree onto `self.pixmap` across `region`
    /// at full coverage (no outer anti-aliasing), respecting per-copy overlap
    /// shifts, `intersects_period` culling, and nested `OpacityGroup`s.
    #[allow(clippy::too_many_arguments)]
    fn paint_tiling_steps_in_region(
        &mut self,
        tiling: &TilingPattern,
        steps: &[TilingBlendStep],
        overlap_shift: Option<(i64, i64)>,
        region: PaintedBounds,
        region_opaque: bool,
        remaining_passes: &mut usize,
    ) -> bool {
        let Some(rect) = region.to_rect() else {
            return false;
        };
        let mut painted = false;
        for step in steps {
            if self.over_budget || *remaining_passes == 0 {
                break;
            }
            match step {
                TilingBlendStep::Segment(seg) => {
                    if let Some((di, dj)) = overlap_shift {
                        if !seg.intersects_period(tiling, di, dj) {
                            continue;
                        }
                    }
                    if region_opaque && seg.is_identity_on_opaque() {
                        continue;
                    }
                    *remaining_passes = remaining_passes.saturating_sub(1);
                    let Some((pixmap, transform, quality, opacity, tile_uniform_opaque)) = self
                        .prepare_tiling_paint_for(
                            tiling,
                            &seg.commands,
                            seg.segment_id,
                            overlap_shift,
                            if overlap_shift.is_none() {
                                seg.blend_mode
                            } else {
                                BlendMode::Normal
                            },
                            seg.bounds,
                            1.0,
                        )
                    else {
                        continue;
                    };
                    let uniform_opaque = (opacity >= 1.0).then_some(tile_uniform_opaque).flatten();
                    if let (Some(c), Some(ref mut target)) = (uniform_opaque, self.pixmap.as_mut())
                    {
                        if region_opaque
                            && matches!(seg.blend_mode, BlendMode::Darken | BlendMode::Lighten)
                        {
                            let stride = target.width() as usize;
                            let data = target.data_mut();
                            let x0 = region.x0 as usize;
                            let x1 = region.x1 as usize;
                            let is_darken = seg.blend_mode == BlendMode::Darken;
                            for y in region.y0 as usize..region.y1 as usize {
                                let row = &mut data[(y * stride + x0) * 4..(y * stride + x1) * 4];
                                let (dst_pxs, _) = row.as_chunks_mut::<4>();
                                if is_darken {
                                    for p in dst_pxs {
                                        p[0] = p[0].min(c[0]);
                                        p[1] = p[1].min(c[1]);
                                        p[2] = p[2].min(c[2]);
                                    }
                                } else {
                                    for p in dst_pxs {
                                        p[0] = p[0].max(c[0]);
                                        p[1] = p[1].max(c[1]);
                                        p[2] = p[2].max(c[2]);
                                    }
                                }
                            }
                            painted = true;
                            continue;
                        }
                        let solid = tiny_skia::Paint {
                            shader: tiny_skia::Shader::SolidColor(tiny_skia::Color::from_rgba8(
                                c[0], c[1], c[2], 255,
                            )),
                            blend_mode: Self::blend_mode_to_skia(seg.blend_mode),
                            anti_alias: false,
                            ..Default::default()
                        };
                        target.fill_rect(rect, &solid, tiny_skia::Transform::identity(), None);
                        painted = true;
                        continue;
                    }
                    let paint = tiny_skia::Paint {
                        shader: tiny_skia::Pattern::new(
                            (*pixmap).as_ref(),
                            tiny_skia::SpreadMode::Repeat,
                            quality,
                            opacity,
                            transform,
                        ),
                        blend_mode: Self::blend_mode_to_skia(seg.blend_mode),
                        anti_alias: false,
                        ..Default::default()
                    };
                    if let Some(ref mut target) = self.pixmap {
                        target.fill_rect(rect, &paint, tiny_skia::Transform::identity(), None);
                        painted = true;
                    }
                }
                TilingBlendStep::OpacityGroup {
                    alpha: inner_alpha,
                    mask: inner_mask,
                    steps: inner_steps,
                } => {
                    if let Some((di, dj)) = overlap_shift {
                        if inner_mask
                            .as_ref()
                            .is_some_and(|m| !m.intersects_period(tiling, di, dj))
                            || !TilingBlendStep::any_intersects_period(inner_steps, tiling, di, dj)
                        {
                            continue;
                        }
                    }
                    if let Some(m_seg) = inner_mask {
                        let ok = self.with_backdrop_shape_coverage(
                            *inner_alpha,
                            Some(region),
                            |r, _band_offset| {
                                if let Some((pixmap, transform, quality, tile_opacity, _)) = r
                                    .prepare_tiling_paint_for(
                                        tiling,
                                        &m_seg.commands,
                                        m_seg.segment_id,
                                        overlap_shift,
                                        BlendMode::Normal,
                                        m_seg.bounds,
                                        1.0,
                                    )
                                {
                                    let paint = tiny_skia::Paint {
                                        shader: tiny_skia::Pattern::new(
                                            (*pixmap).as_ref(),
                                            tiny_skia::SpreadMode::Repeat,
                                            quality,
                                            tile_opacity,
                                            transform,
                                        ),
                                        blend_mode: tiny_skia::BlendMode::SourceOver,
                                        anti_alias: false,
                                        ..Default::default()
                                    };
                                    if let Some(ref mut target) = r.pixmap {
                                        if let Some(r_rect) =
                                            PaintedBounds::whole(target.width(), target.height())
                                                .to_rect()
                                        {
                                            target.fill_rect(
                                                r_rect,
                                                &paint,
                                                tiny_skia::Transform::identity(),
                                                None,
                                            );
                                        }
                                    }
                                }
                            },
                            |r, local_region| {
                                r.paint_tiling_steps_in_region(
                                    tiling,
                                    inner_steps,
                                    overlap_shift,
                                    local_region,
                                    region_opaque,
                                    remaining_passes,
                                )
                            },
                        );
                        if ok {
                            painted = true;
                        }
                    } else if self.with_backdrop_opacity(*inner_alpha, Some(region), |r| {
                        let ok = r.paint_tiling_steps_in_region(
                            tiling,
                            inner_steps,
                            overlap_shift,
                            region,
                            region_opaque,
                            remaining_passes,
                        );
                        if ok {
                            r.dirty = Some(match r.dirty {
                                Some(d) => d.union(region),
                                None => region,
                            });
                        }
                        ok
                    }) {
                        painted = true;
                    }
                }
            }
        }
        painted
    }

    fn paint_tiling_plan_in_region(
        &mut self,
        tiling: &TilingPattern,
        steps: &[TilingBlendStep],
        region: PaintedBounds,
    ) -> bool {
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
        let mut remaining_passes = if overlap_count > 1 {
            MAX_DENSE_TILING_BLEND_PASSES
        } else {
            65_536
        };
        let separate_copies = if overlap_count > 1 {
            let mut total_passes = 0usize;
            'count: for dj in overlap.dj.clone() {
                for di in overlap.di.clone() {
                    total_passes = total_passes.saturating_add(
                        TilingBlendStep::count_intersecting_passes(steps, tiling, di, dj),
                    );
                    if total_passes > MAX_DENSE_TILING_BLEND_PASSES {
                        break 'count;
                    }
                }
            }
            if total_passes > MAX_DENSE_TILING_BLEND_PASSES {
                if TilingBlendStep::is_affine_in_backdrop(steps) {
                    tracing::warn!(
                        total_passes,
                        limit = MAX_DENSE_TILING_BLEND_PASSES,
                        "dense CPU tiling blend plan exceeds pass budget; collapsing affine overlapping copies into 3-seed tiles"
                    );
                    if let Some(PreparedAffineTiles {
                        trans: trans_pm,
                        k: k_pm,
                        f1: f1_pm,
                        transform,
                        quality,
                    }) = self.prepare_tiling_affine_tiles(tiling, steps)
                    {
                        if let (Some(inv), Some(ref mut target)) =
                            (transform.invert(), self.pixmap.as_mut())
                        {
                            let tw = trans_pm.width() as usize;
                            let th = trans_pm.height() as usize;
                            let td = trans_pm.data();
                            let kd = k_pm.data();
                            let f1d = f1_pm.data();
                            let dst_w = target.width() as usize;
                            let dst = target.data_mut();
                            let sample_bilinear = |data: &[u8], tx: f32, ty: f32| -> [f32; 4] {
                                let u = tx - 0.5;
                                let v = ty - 0.5;
                                let x0f = u.floor();
                                let y0f = v.floor();
                                let fx = u - x0f;
                                let fy = v - y0f;
                                let x0 = (x0f as i64).rem_euclid(tw as i64) as usize;
                                let y0 = (y0f as i64).rem_euclid(th as i64) as usize;
                                let x1 = (x0 + 1) % tw;
                                let y1 = (y0 + 1) % th;
                                let o00 = (y0 * tw + x0) * 4;
                                let o10 = (y0 * tw + x1) * 4;
                                let o01 = (y1 * tw + x0) * 4;
                                let o11 = (y1 * tw + x1) * 4;
                                let w00 = (1.0 - fx) * (1.0 - fy);
                                let w10 = fx * (1.0 - fy);
                                let w01 = (1.0 - fx) * fy;
                                let w11 = fx * fy;
                                let mut out = [0.0f32; 4];
                                for ch in 0..4 {
                                    out[ch] = (data[o00 + ch] as f32 * w00
                                        + data[o10 + ch] as f32 * w10
                                        + data[o01 + ch] as f32 * w01
                                        + data[o11 + ch] as f32 * w11)
                                        / 255.0;
                                }
                                out
                            };
                            let nearest = quality == tiny_skia::FilterQuality::Nearest;
                            for y in region.y0..region.y1 {
                                let py = y as f32 + 0.5;
                                let row = y as usize * dst_w;
                                for x in region.x0..region.x1 {
                                    let px = x as f32 + 0.5;
                                    let tx = inv.sx * px + inv.kx * py + inv.tx;
                                    let ty = inv.ky * px + inv.sy * py + inv.ty;
                                    let (t, k, f1) = if nearest {
                                        let ix = (tx.floor() as i64).rem_euclid(tw as i64) as usize;
                                        let iy = (ty.floor() as i64).rem_euclid(th as i64) as usize;
                                        let o = (iy * tw + ix) * 4;
                                        (
                                            [
                                                td[o] as f32 / 255.0,
                                                td[o + 1] as f32 / 255.0,
                                                td[o + 2] as f32 / 255.0,
                                                td[o + 3] as f32 / 255.0,
                                            ],
                                            [
                                                kd[o] as f32 / 255.0,
                                                kd[o + 1] as f32 / 255.0,
                                                kd[o + 2] as f32 / 255.0,
                                                kd[o + 3] as f32 / 255.0,
                                            ],
                                            [
                                                f1d[o] as f32 / 255.0,
                                                f1d[o + 1] as f32 / 255.0,
                                                f1d[o + 2] as f32 / 255.0,
                                                f1d[o + 3] as f32 / 255.0,
                                            ],
                                        )
                                    } else {
                                        (
                                            sample_bilinear(td, tx, ty),
                                            sample_bilinear(kd, tx, ty),
                                            sample_bilinear(f1d, tx, ty),
                                        )
                                    };
                                    let p = (row + x as usize) * 4;
                                    let ba = dst[p + 3] as f32 / 255.0;
                                    let ta = t[3].clamp(0.0, 1.0);
                                    let out_a = (ta + (1.0 - ta) * ba).clamp(0.0, 1.0);
                                    let inv_ba = 1.0 - ba;
                                    for ch in 0..3 {
                                        let m = (f1[ch] - k[ch]).clamp(0.0, 1.0 - k[ch]);
                                        let br = dst[p + ch] as f32 / 255.0;
                                        let out_c = (inv_ba * t[ch] + ba * k[ch] + m * br)
                                            .clamp(0.0, out_a);
                                        dst[p + ch] =
                                            (out_c * 255.0).round().clamp(0.0, 255.0) as u8;
                                    }
                                    dst[p + 3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
                                }
                            }
                            self.dirty = Some(match self.dirty {
                                Some(d) => d.union(region),
                                None => region,
                            });
                            return true;
                        }
                    }
                    if self.over_budget {
                        return false;
                    }
                }
                // Non-affine blend modes (e.g. Darken/Lighten) cannot be
                // interpolated from black/white samples: preserve exact per-copy
                // draw order against the actual backdrop up to the bounded
                // `MAX_TILING_OVERLAP_COPIES × MAX_SEGMENTS` ceiling.
                remaining_passes = 65_536;
                true
            } else {
                true
            }
        } else {
            false
        };
        let region_opaque = !self.alpha_only_pass
            && self.pixmap.as_ref().is_some_and(|p| {
                let stride = p.width() as usize;
                let data = p.data();
                for y in region.y0 as usize..region.y1 as usize {
                    let row = y * stride;
                    for x in region.x0 as usize..region.x1 as usize {
                        if data[(row + x) * 4 + 3] != 255 {
                            return false;
                        }
                    }
                }
                true
            });
        let mut painted = false;
        if separate_copies {
            'copies: for dj in overlap.dj.clone() {
                for di in overlap.di.clone() {
                    if self.over_budget || remaining_passes == 0 {
                        break 'copies;
                    }
                    if self.paint_tiling_steps_in_region(
                        tiling,
                        steps,
                        Some((di, dj)),
                        region,
                        region_opaque,
                        &mut remaining_passes,
                    ) {
                        painted = true;
                    }
                }
            }
        } else {
            painted = self.paint_tiling_steps_in_region(
                tiling,
                steps,
                None,
                region,
                region_opaque,
                &mut remaining_passes,
            );
        }
        if painted {
            self.dirty = Some(match self.dirty {
                Some(d) => d.union(region),
                None => region,
            });
        }
        painted
    }

    /// Replay a backdrop-dependent [`TilingPattern`] directly onto `self`'s live
    /// destination surface (already clipped to the painted shape) at full outer
    /// opacity, up to [`MAX_DIRECT_TILING_TILES`]. Returns `true` if handled
    /// directly, or `false` if the visible grid exceeded the direct-replay cap.
    fn render_tiling_direct(
        &mut self,
        tiling: &TilingPattern,
        mut page_bounds: zpdf_core::Rect,
    ) -> bool {
        if self.tiling_depth >= MAX_TILING_DEPTH || tiling.commands.commands.is_empty() {
            return true;
        }
        if let Some(p) = self.pixmap.as_ref() {
            let surf_x0 =
                self.rect_x0 as f64 + self.dev_origin_x as f64 / self.scale.max(1e-6) as f64;
            let surf_x1 = surf_x0 + p.width() as f64 / self.scale.max(1e-6) as f64;
            let surf_y1 =
                self.rect_y1 as f64 - self.dev_origin_y as f64 / self.scale_y.max(1e-6) as f64;
            let surf_y0 = surf_y1 - p.height() as f64 / self.scale_y.max(1e-6) as f64;
            let pad = 1.0 / self.scale.min(self.scale_y).max(1.0) as f64;
            let x0 = page_bounds.x0.max(surf_x0 - pad);
            let y0 = page_bounds.y0.max(surf_y0 - pad);
            let x1 = page_bounds.x1.min(surf_x1 + pad);
            let y1 = page_bounds.y1.min(surf_y1 + pad);
            if x1 <= x0 || y1 <= y0 {
                return true;
            }
            page_bounds = zpdf_core::Rect::new(x0, y0, x1, y1);
        }
        let Some(from_page) = tiling.matrix.inverse() else {
            return true;
        };
        let corners = [
            zpdf_core::Point::new(page_bounds.x0, page_bounds.y0),
            zpdf_core::Point::new(page_bounds.x1, page_bounds.y0),
            zpdf_core::Point::new(page_bounds.x0, page_bounds.y1),
            zpdf_core::Point::new(page_bounds.x1, page_bounds.y1),
        ];
        let (mut px0, mut py0) = (f64::INFINITY, f64::INFINITY);
        let (mut px1, mut py1) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
        for c in &corners {
            let p = c.transform(&from_page);
            px0 = px0.min(p.x);
            py0 = py0.min(p.y);
            px1 = px1.max(p.x);
            py1 = py1.max(p.y);
        }
        if ![px0, py0, px1, py1].iter().all(|v| v.is_finite()) {
            return true;
        }
        let bbox = tiling.bbox.normalize();
        if ![
            bbox.x0,
            bbox.y0,
            bbox.x1,
            bbox.y1,
            tiling.x_step,
            tiling.y_step,
        ]
        .iter()
        .all(|v| v.is_finite())
            || tiling.x_step == 0.0
            || tiling.y_step == 0.0
        {
            return true;
        }
        let range = |lo: f64, hi: f64, cell_lo: f64, cell_hi: f64, step: f64| -> (i64, i64) {
            let t0 = (lo - cell_hi) / step;
            let t1 = (hi - cell_lo) / step;
            let (a, b) = if t0 <= t1 { (t0, t1) } else { (t1, t0) };
            (
                (a.floor() as i64).saturating_add(1),
                (b.ceil() as i64).saturating_sub(1),
            )
        };
        let (i0, i1) = range(px0, px1, bbox.x0, bbox.x1, tiling.x_step);
        let (j0, j1) = range(py0, py1, bbox.y0, bbox.y1, tiling.y_step);
        let nx = i1.saturating_sub(i0).saturating_add(1);
        let ny = j1.saturating_sub(j0).saturating_add(1);
        if nx <= 0 || ny <= 0 {
            return true;
        }
        if nx.saturating_mul(ny) > MAX_DIRECT_TILING_TILES {
            return false;
        }

        self.tiling_depth = self.tiling_depth.saturating_add(1);
        let base_clips = self.clip_stack.len();
        let base_blends = self.blend_stack.len();
        let base_skipped = self.skipped_blend_depth;
        // Scope soft-mask plane caching to each tile's live transformed
        // command list so per-tile transformed mask Arcs never alias freed
        // heap addresses across translated tiles or pollute the page cache.
        let saved_mask_planes = std::mem::take(&mut self.soft_mask_planes);
        let saved_mask_bytes = std::mem::replace(&mut self.soft_mask_cache_bytes, 0);

        'tiles: for j in j0..=j1 {
            for i in i0..=i1 {
                if self.over_budget {
                    break 'tiles;
                }
                let tile_ctm = tiling.matrix.concat(&zpdf_core::Matrix::translate(
                    i as f64 * tiling.x_step,
                    j as f64 * tiling.y_step,
                ));
                let mut mask_memo = HashMap::new();
                let transformed = transform_display_list_for_cpu(
                    &tiling.commands,
                    &tile_ctm,
                    self.scale.max(self.scale_y),
                    &mut mask_memo,
                );
                for cmd in &transformed.commands {
                    if self.over_budget {
                        break;
                    }
                    let _ = self.execute(cmd);
                }
                while self.skipped_blend_depth > base_skipped {
                    self.skipped_blend_depth -= 1;
                }
                while self.blend_stack.len() > base_blends {
                    self.pop_blend_group();
                }
                while self.clip_stack.len() > base_clips {
                    self.pop_clip();
                }
                // Evict tile-local mask planes before `transformed` drops.
                self.soft_mask_planes.clear();
                self.soft_mask_cache_bytes = 0;
            }
        }
        self.soft_mask_planes = saved_mask_planes;
        self.soft_mask_cache_bytes = saved_mask_bytes;
        self.tiling_depth = self.tiling_depth.saturating_sub(1);
        true
    }

    fn render_tiling_fill_backdrop_dependent(
        &mut self,
        path: &Path,
        skia_path: &tiny_skia::Path,
        rule: &FillRule,
        tiling: &TilingPattern,
        alpha: f32,
    ) -> bool {
        let opacity = unit(alpha);
        if opacity <= 0.0 {
            return false;
        }
        let fill_rule = Self::fill_rule_to_skia(rule);
        let steps = tiling.blend_steps();

        // Fast path: a single non-overlapping segment at full opacity applies
        // outer shape coverage once directly in a single `fill_path` call.
        if opacity >= 1.0 && Self::tiling_is_non_overlapping(tiling) {
            if let Some([TilingBlendStep::Segment(seg)]) = steps.as_deref() {
                if let Some((pixmap, transform, quality, tile_opacity, _)) = self
                    .prepare_tiling_paint_for(
                        tiling,
                        &seg.commands,
                        seg.segment_id,
                        None,
                        BlendMode::Normal,
                        seg.bounds,
                        1.0,
                    )
                {
                    let paint = tiny_skia::Paint {
                        shader: tiny_skia::Pattern::new(
                            (*pixmap).as_ref(),
                            tiny_skia::SpreadMode::Repeat,
                            quality,
                            tile_opacity,
                            transform,
                        ),
                        blend_mode: Self::blend_mode_to_skia(seg.blend_mode),
                        anti_alias: true,
                        ..Default::default()
                    };
                    if let Some(ref mut target) = self.pixmap {
                        target.fill_path(
                            skia_path,
                            &paint,
                            fill_rule,
                            tiny_skia::Transform::identity(),
                            self.current_clip.as_ref(),
                        );
                        self.mark_painted(skia_path.bounds(), 1.0);
                        return true;
                    }
                }
            }
        }

        let scope = self
            .pixmap
            .as_ref()
            .map(|p| Self::drawn_extent(skia_path.bounds(), 1.0, p.width(), p.height()));
        let white = Self::color_to_paint(&Color::white(), 1.0);
        self.with_backdrop_shape_coverage(
            opacity,
            scope,
            |renderer, band_offset| {
                if let Some(ref mut target) = renderer.pixmap {
                    target.fill_path(skia_path, &white, fill_rule, band_offset, None);
                }
            },
            |renderer, region| {
                if Self::tiling_is_non_overlapping(tiling) {
                    if let Some(ref steps) = steps {
                        if renderer.paint_tiling_plan_in_region(tiling, steps, region) {
                            return true;
                        }
                    }
                }
                if let Some(bounds) = Self::page_path_bounds(path) {
                    if renderer.render_tiling_direct(tiling, bounds) {
                        return true;
                    }
                }
                if let Some(ref steps) = steps {
                    if renderer.paint_tiling_plan_in_region(tiling, steps, region) {
                        return true;
                    }
                }
                // Bounded backstop when both direct replay (>4,096 tiles) and
                // blend_steps() decline (e.g., active overprint inside a dense
                // cell): paint the single rasterized cell rather than dropping blank.
                if let (Some(rect), Some((pixmap, transform, quality, tile_opacity, _))) = (
                    region.to_rect(),
                    renderer.prepare_tiling_paint_for(
                        tiling,
                        &tiling.commands,
                        0,
                        None,
                        BlendMode::Normal,
                        None,
                        1.0,
                    ),
                ) {
                    let paint = tiny_skia::Paint {
                        shader: tiny_skia::Pattern::new(
                            (*pixmap).as_ref(),
                            tiny_skia::SpreadMode::Repeat,
                            quality,
                            tile_opacity,
                            transform,
                        ),
                        blend_mode: tiny_skia::BlendMode::SourceOver,
                        anti_alias: false,
                        ..Default::default()
                    };
                    if let Some(ref mut target) = renderer.pixmap {
                        target.fill_rect(rect, &paint, tiny_skia::Transform::identity(), None);
                        renderer.dirty = Some(match renderer.dirty {
                            Some(d) => d.union(region),
                            None => region,
                        });
                        return true;
                    }
                }
                false
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn render_tiling_stroke_backdrop_dependent(
        &mut self,
        path: &Path,
        skia_path: &tiny_skia::Path,
        style: &StrokeStyle,
        stroke: &tiny_skia::Stroke,
        grow: f32,
        anisotropic_outline: Option<&tiny_skia::Path>,
        anisotropic_hairline: Option<&tiny_skia::Path>,
        tiling: &TilingPattern,
        alpha: f32,
    ) -> bool {
        let opacity = unit(alpha);
        if opacity <= 0.0 {
            return false;
        }
        let steps = tiling.blend_steps();

        let draw_stroke_shape = |target: &mut tiny_skia::Pixmap,
                                 paint: &tiny_skia::Paint<'_>,
                                 xf: tiny_skia::Transform,
                                 clip: Option<&tiny_skia::Mask>| {
            if let Some(outline) = anisotropic_outline {
                target.fill_path(outline, paint, tiny_skia::FillRule::Winding, xf, clip);
            } else if let Some(hairline) = anisotropic_hairline {
                let mut hairline_stroke = Self::skia_stroke(style, 1.0, 1.0);
                hairline_stroke.dash = None;
                target.stroke_path(hairline, paint, &hairline_stroke, xf, clip);
            } else {
                target.stroke_path(skia_path, paint, stroke, xf, clip);
            }
        };

        // Fast path: single non-overlapping segment at full opacity.
        if opacity >= 1.0 && Self::tiling_is_non_overlapping(tiling) {
            if let Some([TilingBlendStep::Segment(seg)]) = steps.as_deref() {
                if let Some((pixmap, transform, quality, tile_opacity, _)) = self
                    .prepare_tiling_paint_for(
                        tiling,
                        &seg.commands,
                        seg.segment_id,
                        None,
                        BlendMode::Normal,
                        seg.bounds,
                        1.0,
                    )
                {
                    let paint = tiny_skia::Paint {
                        shader: tiny_skia::Pattern::new(
                            (*pixmap).as_ref(),
                            tiny_skia::SpreadMode::Repeat,
                            quality,
                            tile_opacity,
                            transform,
                        ),
                        blend_mode: Self::blend_mode_to_skia(seg.blend_mode),
                        anti_alias: true,
                        ..Default::default()
                    };
                    if let Some(ref mut target) = self.pixmap {
                        draw_stroke_shape(
                            target,
                            &paint,
                            tiny_skia::Transform::identity(),
                            self.current_clip.as_ref(),
                        );
                        if let Some(outline) = anisotropic_outline {
                            self.mark_painted(outline.bounds(), 1.0);
                        } else {
                            self.mark_painted(skia_path.bounds(), grow);
                        }
                        return true;
                    }
                }
            }
        }

        let scope = self.pixmap.as_ref().map(|p| {
            if let Some(outline) = anisotropic_outline {
                Self::drawn_extent(outline.bounds(), 1.0, p.width(), p.height())
            } else {
                Self::drawn_extent(skia_path.bounds(), grow, p.width(), p.height())
            }
        });
        let white = Self::color_to_paint(&Color::white(), 1.0);
        self.with_backdrop_shape_coverage(
            opacity,
            scope,
            |renderer, band_offset| {
                if let Some(ref mut target) = renderer.pixmap {
                    draw_stroke_shape(target, &white, band_offset, None);
                }
            },
            |renderer, region| {
                if Self::tiling_is_non_overlapping(tiling) {
                    if let Some(ref steps) = steps {
                        if renderer.paint_tiling_plan_in_region(tiling, steps, region) {
                            return true;
                        }
                    }
                }
                if let Some(b) = Self::page_path_bounds(path) {
                    let reach = match style.join {
                        LineJoin::Miter => {
                            if style.miter_limit.is_finite() {
                                (style.miter_limit as f64).max(std::f64::consts::SQRT_2)
                            } else {
                                f64::INFINITY
                            }
                        }
                        _ => std::f64::consts::SQRT_2,
                    };
                    let width = if style.width.is_finite() {
                        style.width.max(1.0) as f64
                    } else {
                        f64::INFINITY
                    };
                    let pad = (width * 0.5 * reach).max(0.5);
                    let region_rect = zpdf_core::Rect::new(
                        renderer.rect_x0 as f64
                            + (region.x0 as f64 + renderer.dev_origin_x as f64)
                                / renderer.scale as f64,
                        renderer.rect_y1 as f64
                            - (region.y1 as f64 + renderer.dev_origin_y as f64)
                                / renderer.scale_y as f64,
                        renderer.rect_x0 as f64
                            + (region.x1 as f64 + renderer.dev_origin_x as f64)
                                / renderer.scale as f64,
                        renderer.rect_y1 as f64
                            - (region.y0 as f64 + renderer.dev_origin_y as f64)
                                / renderer.scale_y as f64,
                    );
                    let bounds = if pad.is_finite() {
                        zpdf_core::Rect::new(
                            (b.x0 - pad).max(region_rect.x0),
                            (b.y0 - pad).max(region_rect.y0),
                            (b.x1 + pad).min(region_rect.x1),
                            (b.y1 + pad).min(region_rect.y1),
                        )
                    } else {
                        region_rect
                    };
                    if bounds.x1 > bounds.x0
                        && bounds.y1 > bounds.y0
                        && renderer.render_tiling_direct(tiling, bounds)
                    {
                        return true;
                    }
                }
                if let Some(ref steps) = steps {
                    if renderer.paint_tiling_plan_in_region(tiling, steps, region) {
                        return true;
                    }
                }
                // Bounded backstop when both direct replay (>4,096 tiles) and
                // blend_steps() decline: paint the single rasterized cell.
                if let (Some(rect), Some((pixmap, transform, quality, tile_opacity, _))) = (
                    region.to_rect(),
                    renderer.prepare_tiling_paint_for(
                        tiling,
                        &tiling.commands,
                        0,
                        None,
                        BlendMode::Normal,
                        None,
                        1.0,
                    ),
                ) {
                    let paint = tiny_skia::Paint {
                        shader: tiny_skia::Pattern::new(
                            (*pixmap).as_ref(),
                            tiny_skia::SpreadMode::Repeat,
                            quality,
                            tile_opacity,
                            transform,
                        ),
                        blend_mode: tiny_skia::BlendMode::SourceOver,
                        anti_alias: false,
                        ..Default::default()
                    };
                    if let Some(ref mut target) = renderer.pixmap {
                        target.fill_rect(rect, &paint, tiny_skia::Transform::identity(), None);
                        renderer.dirty = Some(match renderer.dirty {
                            Some(d) => d.union(region),
                            None => region,
                        });
                        return true;
                    }
                }
                false
            },
        )
    }

    fn render_fill(&mut self, path: &Path, rule: &FillRule, paint_spec: &Paint, alpha: f32) {
        let t = self.tick();
        let Some(skia_path) = self.build_skia_path(path) else {
            return self.tock(t, |s| &mut s.fill_ns);
        };
        // One device pixel of slack: a pixel the path only clips the corner of
        // still receives anti-aliased coverage. Check reachability before
        // rasterizing any tiling cell.
        if !self.reaches_target(skia_path.bounds(), 1.0) {
            return self.tock(t, |s| &mut s.fill_ns);
        }
        if let Paint::Tiling(tiling) = paint_spec {
            if self.tiling_is_backdrop_dependent(tiling) {
                let painted = self
                    .render_tiling_fill_backdrop_dependent(path, &skia_path, rule, tiling, alpha);
                self.tock(t, |s| &mut s.fill_ns);
                self.count(painted as u64, |s| &mut s.fills);
                return;
            }
        }
        let tile_guard: Arc<tiny_skia::Pixmap>;
        let paint = match paint_spec {
            Paint::Solid(c) => Self::color_to_paint(c, alpha),
            Paint::Tiling(tiling) => {
                let Some((pixmap, transform, quality, opacity)) =
                    self.prepare_tiling_paint(tiling, alpha)
                else {
                    return self.tock(t, |s| &mut s.fill_ns);
                };
                tile_guard = pixmap;
                tiny_skia::Paint {
                    shader: tiny_skia::Pattern::new(
                        (*tile_guard).as_ref(),
                        tiny_skia::SpreadMode::Repeat,
                        quality,
                        opacity,
                        transform,
                    ),
                    anti_alias: true,
                    ..Default::default()
                }
            }
            // Shadings are rasterized by the interpreter into images before
            // reaching a backend.
            _ => return self.tock(t, |s| &mut s.fill_ns),
        };
        let fill_rule = Self::fill_rule_to_skia(rule);
        let mut painted = false;
        if let Some(ref mut pixmap) = self.pixmap {
            pixmap.fill_path(
                &skia_path,
                &paint,
                fill_rule,
                tiny_skia::Transform::identity(),
                self.current_clip.as_ref(),
            );
            painted = true;
        }
        if painted {
            self.mark_painted(skia_path.bounds(), 1.0);
        }
        self.tock(t, |s| &mut s.fill_ns);
        self.count(painted as u64, |s| &mut s.fills);
    }

    fn render_stroke(&mut self, path: &Path, style: &StrokeStyle, paint_spec: &Paint, alpha: f32) {
        let t = self.tick();
        let Some(skia_path) = self.build_skia_path(path) else {
            return self.tock(t, |s| &mut s.stroke_ns);
        };
        let anisotropic = self.scale != self.scale_y;
        let stroke = if anisotropic {
            // Conservative upper-bound stroke width for reachability culling.
            Self::skia_stroke(style, self.scale.max(self.scale_y), 1.0)
        } else {
            self.build_skia_stroke(style)
        };
        // The *centerline's* bounds say nothing about how far the ink reaches:
        // joins, caps and miters all extend past it.
        let grow = stroke_overshoot(stroke.width, stroke.miter_limit);
        if !self.reaches_target(skia_path.bounds(), grow) {
            return self.tock(t, |s| &mut s.stroke_ns);
        }
        let anisotropic_outline = if anisotropic && style.width.is_finite() && style.width > 0.0 {
            let Some(outline) = self.build_anisotropic_stroke_outline(path, style) else {
                return self.tock(t, |s| &mut s.stroke_ns);
            };
            Some(outline)
        } else {
            None
        };
        let anisotropic_hairline = if anisotropic && anisotropic_outline.is_none() {
            let Some(hairline) = self.build_anisotropic_hairline_path(path, style) else {
                return self.tock(t, |s| &mut s.stroke_ns);
            };
            Some(hairline)
        } else {
            None
        };
        if let Paint::Tiling(tiling) = paint_spec {
            if self.tiling_is_backdrop_dependent(tiling) {
                let painted = self.render_tiling_stroke_backdrop_dependent(
                    path,
                    &skia_path,
                    style,
                    &stroke,
                    grow,
                    anisotropic_outline.as_ref(),
                    anisotropic_hairline.as_ref(),
                    tiling,
                    alpha,
                );
                self.tock(t, |s| &mut s.stroke_ns);
                self.count(painted as u64, |s| &mut s.strokes);
                return;
            }
        }
        let tile_guard: Arc<tiny_skia::Pixmap>;
        let paint = match paint_spec {
            Paint::Solid(c) => Self::color_to_paint(c, alpha),
            Paint::Tiling(tiling) => {
                let Some((pixmap, transform, quality, opacity)) =
                    self.prepare_tiling_paint(tiling, alpha)
                else {
                    return self.tock(t, |s| &mut s.stroke_ns);
                };
                tile_guard = pixmap;
                tiny_skia::Paint {
                    shader: tiny_skia::Pattern::new(
                        (*tile_guard).as_ref(),
                        tiny_skia::SpreadMode::Repeat,
                        quality,
                        opacity,
                        transform,
                    ),
                    anti_alias: true,
                    ..Default::default()
                }
            }
            _ => return self.tock(t, |s| &mut s.stroke_ns),
        };
        let mut painted = false;
        if let Some(ref mut pixmap) = self.pixmap {
            if let Some(ref outline) = anisotropic_outline {
                pixmap.fill_path(
                    outline,
                    &paint,
                    tiny_skia::FillRule::Winding,
                    tiny_skia::Transform::identity(),
                    self.current_clip.as_ref(),
                );
            } else if let Some(ref hairline) = anisotropic_hairline {
                let mut hairline_stroke = Self::skia_stroke(style, 1.0, 1.0);
                hairline_stroke.dash = None;
                pixmap.stroke_path(
                    hairline,
                    &paint,
                    &hairline_stroke,
                    tiny_skia::Transform::identity(),
                    self.current_clip.as_ref(),
                );
            } else {
                pixmap.stroke_path(
                    &skia_path,
                    &paint,
                    &stroke,
                    tiny_skia::Transform::identity(),
                    self.current_clip.as_ref(),
                );
            }
            painted = true;
        }
        if painted {
            if let Some(ref outline) = anisotropic_outline {
                self.mark_painted(outline.bounds(), 1.0);
            } else {
                self.mark_painted(skia_path.bounds(), grow);
            }
        }
        self.tock(t, |s| &mut s.stroke_ns);
        self.count(painted as u64, |s| &mut s.strokes);
    }

    /// Device-space tiny-skia stroke parameters for `style`.
    fn build_skia_stroke(&self, style: &StrokeStyle) -> tiny_skia::Stroke {
        // Hairline boost: never let a stroke fall below one device pixel
        // (matches pdfium; keeps thin diagram strokes legible at low DPI, and
        // renders PDF zero-width strokes as 1px hairlines).
        Self::skia_stroke(style, self.stroke_scale(), 1.0)
    }

    /// tiny-skia stroke parameters for `style` with lengths scaled by `scale`
    /// and the width at least `min_width`.
    fn skia_stroke(style: &StrokeStyle, scale: f32, min_width: f32) -> tiny_skia::Stroke {
        tiny_skia::Stroke {
            width: (style.width * scale).max(min_width),
            line_cap: match style.cap {
                LineCap::Butt => tiny_skia::LineCap::Butt,
                LineCap::Round => tiny_skia::LineCap::Round,
                LineCap::Square => tiny_skia::LineCap::Square,
            },
            line_join: match style.join {
                LineJoin::Miter => tiny_skia::LineJoin::Miter,
                LineJoin::Round => tiny_skia::LineJoin::Round,
                LineJoin::Bevel => tiny_skia::LineJoin::Bevel,
            },
            miter_limit: style.miter_limit,
            dash: Self::stroke_dash(style, scale),
        }
    }

    /// Build a device-space tiny-skia dash from the stroke style. Degenerate
    /// patterns (empty/all-zero/negative entries) return `None` → solid stroke.
    /// PDF allows odd-length dash arrays (the pattern repeats, so `[3]` means
    /// 3 on / 3 off); tiny-skia requires an even count, so odd arrays are doubled.
    fn stroke_dash(style: &StrokeStyle, scale: f32) -> Option<tiny_skia::StrokeDash> {
        let dash = style.dash.as_ref()?;
        if zpdf_render::dash::is_degenerate(&dash.array) {
            return None;
        }
        let mut array: Vec<f32> = dash.array.iter().map(|v| v * scale).collect();
        if array.len() % 2 == 1 {
            // Avoid clone: reserve and extend from within the same vec
            array.reserve(array.len());
            let len = array.len();
            for i in 0..len {
                array.push(array[i]);
            }
        }
        tiny_skia::StrokeDash::new(array, dash.phase * scale)
    }

    fn path_to_skia_user_space(path: &Path) -> Option<tiny_skia::Path> {
        let mut pb = tiny_skia::PathBuilder::new();
        for elem in path {
            match elem {
                PathElement::MoveTo(p) => pb.move_to(p.x as f32, p.y as f32),
                PathElement::LineTo(p) => pb.line_to(p.x as f32, p.y as f32),
                PathElement::CurveTo(c1, c2, end) => pb.cubic_to(
                    c1.x as f32,
                    c1.y as f32,
                    c2.x as f32,
                    c2.y as f32,
                    end.x as f32,
                    end.y as f32,
                ),
                PathElement::Close => pb.close(),
            }
        }
        pb.finish()
    }

    fn skia_path_to_display_list(skia_path: &tiny_skia::Path) -> Path {
        let mut out = Path::new();
        let mut cur = zpdf_core::Point::new(0.0, 0.0);
        let mut subpath_start = cur;
        for seg in skia_path.segments() {
            match seg {
                tiny_skia::PathSegment::MoveTo(p) => {
                    let pt = zpdf_core::Point::new(p.x as f64, p.y as f64);
                    out.move_to(pt);
                    cur = pt;
                    subpath_start = pt;
                }
                tiny_skia::PathSegment::LineTo(p) => {
                    let pt = zpdf_core::Point::new(p.x as f64, p.y as f64);
                    out.line_to(pt);
                    cur = pt;
                }
                tiny_skia::PathSegment::QuadTo(c, end) => {
                    let q = zpdf_core::Point::new(c.x as f64, c.y as f64);
                    let p2 = zpdf_core::Point::new(end.x as f64, end.y as f64);
                    let c1 = zpdf_core::Point::new(
                        cur.x + (2.0 / 3.0) * (q.x - cur.x),
                        cur.y + (2.0 / 3.0) * (q.y - cur.y),
                    );
                    let c2 = zpdf_core::Point::new(
                        p2.x + (2.0 / 3.0) * (q.x - p2.x),
                        p2.y + (2.0 / 3.0) * (q.y - p2.y),
                    );
                    out.curve_to(c1, c2, p2);
                    cur = p2;
                }
                tiny_skia::PathSegment::CubicTo(c1, c2, end) => {
                    let p2 = zpdf_core::Point::new(end.x as f64, end.y as f64);
                    out.curve_to(
                        zpdf_core::Point::new(c1.x as f64, c1.y as f64),
                        zpdf_core::Point::new(c2.x as f64, c2.y as f64),
                        p2,
                    );
                    cur = p2;
                }
                tiny_skia::PathSegment::Close => {
                    out.close();
                    cur = subpath_start;
                }
            }
        }
        out
    }

    /// Apply `style`'s dash pattern to `path` in its own user coordinate space.
    ///
    /// Returns a clone of `path` unchanged when `style` has no active dash
    /// pattern, or an empty [`Path`] if dashing yields no visible segments.
    #[doc(hidden)]
    pub fn dash_path(path: &Path, style: &StrokeStyle, resolution_scale: f32) -> Path {
        let Some(dash) = Self::stroke_dash(style, 1.0) else {
            return path.clone();
        };
        let Some(skia_path) = Self::path_to_skia_user_space(path) else {
            return Path::new();
        };
        let res = resolution_scale.max(0.25);
        let Some(dashed) = skia_path.dash(&dash, res) else {
            return Path::new();
        };
        Self::skia_path_to_display_list(&dashed)
    }

    /// Expand a display-list path stroke (including caps, joins, miter limit,
    /// and dashes) into a filled outline in the path's own user coordinate
    /// space.
    ///
    /// `min_width` specifies the minimum user-space stroke width independently
    /// of `resolution_scale` (which only controls curve flattening).
    #[doc(hidden)]
    pub fn stroke_path_to_outline(
        path: &Path,
        style: &StrokeStyle,
        min_width: f32,
        resolution_scale: f32,
    ) -> Option<Path> {
        let skia_path = Self::path_to_skia_user_space(path)?;
        let res = resolution_scale.max(0.25);
        let skia_stroke = Self::skia_stroke(style, 1.0, min_width.max(0.0));
        // `tiny_skia::Path::stroke` does not apply `stroke.dash`; dash first
        // identically to `Pixmap::stroke_path`.
        let dashed_path;
        let path_to_stroke = if let Some(ref dash) = skia_stroke.dash {
            dashed_path = skia_path.dash(dash, res)?;
            &dashed_path
        } else {
            &skia_path
        };
        let outlined = path_to_stroke.stroke(&skia_stroke, res)?;
        let out = Self::skia_path_to_display_list(&outlined);
        (!out.is_empty()).then_some(out)
    }

    /// Render a glyph run with a specific alpha override (used by knockout groups).
    fn render_glyph_run_with_alpha(&mut self, run: &GlyphRun, alpha_override: f32) {
        let font_cache = match self.font_cache {
            Some(fc) => fc,
            None => return,
        };
        let font = match font_cache.get(run.font_id) {
            Some(f) => f,
            None => return,
        };
        if !font.has_font_data() {
            return;
        }

        if let Paint::Tiling(tiling) = &run.paint {
            if self.tiling_is_backdrop_dependent(tiling) {
                if let Some(steps) = tiling.blend_steps() {
                    let opacity = unit(alpha_override);
                    if opacity <= 0.0 {
                        return;
                    }
                    self.count(run.glyphs.len() as u64, |s| &mut s.glyphs);
                    // Fast path: single non-overlapping segment at full opacity
                    // for outline glyphs. Type 3 glyphs route through
                    // `render_type3_backdrop_tiling` so explicit `d0` colors,
                    // overlapping sub-shapes, and glyph-local clips composite
                    // in exact painter's order.
                    if !font.is_type3() && opacity >= 1.0 && Self::tiling_is_non_overlapping(tiling)
                    {
                        if let [TilingBlendStep::Segment(seg)] = steps.as_slice() {
                            if let Some((pixmap, transform, quality, tile_opacity, _)) = self
                                .prepare_tiling_paint_for(
                                    tiling,
                                    &seg.commands,
                                    seg.segment_id,
                                    None,
                                    BlendMode::Normal,
                                    seg.bounds,
                                    1.0,
                                )
                            {
                                let paint = tiny_skia::Paint {
                                    shader: tiny_skia::Pattern::new(
                                        (*pixmap).as_ref(),
                                        tiny_skia::SpreadMode::Repeat,
                                        quality,
                                        tile_opacity,
                                        transform,
                                    ),
                                    blend_mode: Self::blend_mode_to_skia(seg.blend_mode),
                                    anti_alias: true,
                                    ..Default::default()
                                };
                                self.render_outline_glyphs(run, font, &paint);
                                return;
                            }
                        }
                    }

                    // Multi-pass / overlapping / translucent glyph run: rasterize
                    // glyph coverage ONCE into a region-sized scratch alpha
                    // surface, evaluate the full pattern plan at 1.0 coverage,
                    // and apply glyph AA coverage × object opacity ONCE.
                    // For Type 3 fonts, preserve intrinsic colored (`d0`) non-mask
                    // images instead of feeding them into the tiling coverage mask.
                    if font.is_type3() {
                        self.render_type3_backdrop_tiling(run, font, tiling, &steps, opacity);
                        return;
                    }
                    let scope = self.outline_glyph_run_scope(run, font);
                    let white = Self::color_to_paint(&Color::white(), 1.0);
                    self.with_backdrop_shape_coverage(
                        opacity,
                        scope,
                        |renderer, _band_offset| {
                            renderer.render_outline_glyphs(run, font, &white);
                        },
                        |renderer, region| {
                            renderer.paint_tiling_plan_in_region(tiling, &steps, region)
                        },
                    );
                    return;
                }
            }
        }

        let tile_guard: Arc<tiny_skia::Pixmap>;
        let paint = match &run.paint {
            Paint::Solid(c) => Self::color_to_paint(c, alpha_override),
            Paint::Tiling(tiling) => {
                let Some((pixmap, transform, quality, opacity)) =
                    self.prepare_tiling_paint(tiling, alpha_override)
                else {
                    return;
                };
                tile_guard = pixmap;
                tiny_skia::Paint {
                    shader: tiny_skia::Pattern::new(
                        (*tile_guard).as_ref(),
                        tiny_skia::SpreadMode::Repeat,
                        quality,
                        opacity,
                        transform,
                    ),
                    anti_alias: true,
                    ..Default::default()
                }
            }
            _ => return,
        };

        // Counted once the run is known to actually paint, so the counter means
        // "glyph instances drawn" rather than "glyph runs seen".
        self.count(run.glyphs.len() as u64, |s| &mut s.glyphs);
        if font.is_type3() {
            // Type3 glyphs are content streams interpreted per glyph (see
            // `render_type3_glyphs`). Their cost lands in `glyph_raster_ns`:
            // they *are* glyph rasterization, just not outline-based.
            let t = self.tick();
            self.render_type3_glyphs(run, font, &paint, alpha_override);
            self.tock(t, |s| &mut s.glyph_raster_ns);
        } else {
            self.render_outline_glyphs(run, font, &paint);
        }
    }

    fn render_glyph_run(&mut self, run: &GlyphRun) {
        self.render_glyph_run_with_alpha(run, run.alpha)
    }

    fn push_clip(&mut self, path: &Path, rule: &FillRule) {
        let t = self.tick();
        let Some(pixmap) = self.pixmap.as_ref() else {
            return self.tock(t, |s| &mut s.clip_ns);
        };
        let (pw, ph) = (pixmap.width(), pixmap.height());

        // Budget guard: once the page has spent its clip-mask pixel-work, stop
        // building new masks (which would hang on 100k-clip pages). The
        // matching PopClip pops a `Skipped` frame and the previously-active
        // clip stays in force — strictly tighter than dropping clipping entirely.
        if self.clip_pixel_spent >= MAX_CLIP_PIXEL_WORK {
            self.clip_stack.push(ClipFrame::Skipped);
            return self.tock(t, |s| &mut s.clip_ns);
        }

        let skia_path = self.build_skia_path(path);
        if skia_path.is_none() && !path.is_empty() {
            // A non-finite/out-of-range clip path: keep push/pop balanced.
            self.clip_stack.push(ClipFrame::Skipped);
            return self.tock(t, |s| &mut s.clip_ns);
        }

        let Some(mut mask) = self.take_mask(pw, ph) else {
            self.clip_stack.push(ClipFrame::Skipped);
            return self.tock(t, |s| &mut s.clip_ns);
        };
        // The fill and the intersection below only touch the path's extent.
        let extent = skia_path.as_ref().map_or(0, |path| {
            Self::drawn_extent(path.bounds(), 1.0, pw, ph).area()
        });
        self.clip_pixel_spent = self.clip_pixel_spent.saturating_add(extent);

        if let Some(skia_path) = &skia_path {
            mask.fill_path(
                skia_path,
                Self::fill_rule_to_skia(rule),
                true, // anti-alias clip edges (fills/strokes are AA'd too)
                tiny_skia::Transform::identity(),
            );
        }

        // Intersect with the current clip, but only across the new clip path's
        // device bbox: outside it the new mask is already 0, so the intersection
        // there is 0 too and need not be touched. This turns the per-clip
        // intersection from O(raster) into O(clip bbox).
        if let (Some(current), Some(skia_path)) = (&self.current_clip, &skia_path) {
            let b = skia_path.bounds();
            let w = pw as usize;
            let h = ph as usize;
            let x0 = (b.left().floor().max(0.0) as usize).min(w);
            let x1 = (b.right().ceil().max(0.0) as usize).min(w);
            let y0 = (b.top().floor().max(0.0) as usize).min(h);
            let y1 = (b.bottom().ceil().max(0.0) as usize).min(h);
            let current_data = current.data();
            let mask_data = mask.data_mut();
            for y in y0..y1 {
                let row = y * w;
                for x in x0..x1 {
                    let i = row + x;
                    mask_data[i] = mul255(mask_data[i], current_data[i]);
                }
            }
        }

        let frame = match self.current_clip.take() {
            Some(old) => ClipFrame::Mask(old, self.clip_bounds),
            None => ClipFrame::Empty,
        };
        self.clip_stack.push(frame);
        self.current_clip = Some(mask);
        // The new clip's device extent: its path's bbox plus the anti-aliased
        // edge. It is deliberately *not* intersected with the outgoing clip.
        // The mask is only combined with that clip across a bbox-restricted
        // loop, so outside the loop the mask keeps the new path's own coverage;
        // a region that must contain every pixel the active mask can be
        // non-zero in therefore has to come from the new clip alone.
        self.clip_bounds = Some(skia_path.as_ref().map_or(
            PaintedBounds {
                x0: 0,
                y0: 0,
                x1: 0,
                y1: 0,
            },
            |path| Self::drawn_extent(path.bounds(), 1.0, pw, ph),
        ));
        self.tock(t, |s| &mut s.clip_ns);
        self.count(1, |s| &mut s.clips_pushed);
    }

    fn pop_clip(&mut self) {
        let t = self.tick();
        match self.clip_stack.pop() {
            // Budget-skipped push: leave the active clip untouched.
            Some(ClipFrame::Skipped) => {}
            Some(ClipFrame::Mask(prev, prev_bounds)) => {
                if let Some(removed) = self.current_clip.replace(prev) {
                    self.recycle_mask(removed, self.clip_bounds);
                }
                self.clip_bounds = prev_bounds;
            }
            Some(ClipFrame::Empty) => {
                if let Some(mask) = self.current_clip.take() {
                    self.recycle_mask(mask, self.clip_bounds);
                }
                self.clip_bounds = None;
            }
            None => {}
        }
        self.tock(t, |s| &mut s.clip_ns);
    }

    /// A zeroed `width`×`height` clip mask: a pooled one, else a new one
    /// within [`MAX_CLIP_MASK_BYTES`] (charged as full-raster clip work, since
    /// the allocation zeroes it all).
    fn take_mask(&mut self, width: u32, height: u32) -> Option<tiny_skia::Mask> {
        if let Some(at) = self
            .mask_pool
            .iter()
            .rposition(|m| (m.width(), m.height()) == (width, height))
        {
            return Some(self.mask_pool.swap_remove(at));
        }
        let bytes = width as u64 * height as u64;
        if self.clip_mask_bytes.saturating_add(bytes) > MAX_CLIP_MASK_BYTES {
            return None;
        }
        let mask = tiny_skia::Mask::new(width, height)?;
        self.clip_mask_bytes += bytes;
        self.clip_pixel_spent = self.clip_pixel_spent.saturating_add(bytes);
        Some(mask)
    }

    /// Zero a retired clip mask over `bounds`, which holds every pixel it can
    /// be non-zero in (the clip's `clip_bounds`), and keep it for reuse.
    fn recycle_mask(&mut self, mut mask: tiny_skia::Mask, bounds: Option<PaintedBounds>) {
        let width = mask.width() as usize;
        let height = mask.height() as usize;
        let data = mask.data_mut();
        match bounds {
            Some(b) => {
                let (x0, x1) = ((b.x0 as usize).min(width), (b.x1 as usize).min(width));
                for y in (b.y0 as usize).min(height)..(b.y1 as usize).min(height) {
                    if x0 < x1 {
                        data[y * width + x0..y * width + x1].fill(0);
                    }
                }
            }
            None => data.fill(0),
        }
        self.mask_pool.push(mask);
    }

    /// A fully transparent `width`×`height` surface: a pooled one, or a new
    /// allocation (`None` when that fails).
    fn take_surface(&mut self, width: u32, height: u32) -> Option<tiny_skia::Pixmap> {
        if let Some(at) = self
            .surface_pool
            .iter()
            .rposition(|p| (p.width(), p.height()) == (width, height))
        {
            let surface = self.surface_pool.swap_remove(at);
            self.pooled_surface_bytes = self
                .pooled_surface_bytes
                .saturating_sub(surface_bytes(&surface));
            return Some(surface);
        }
        tiny_skia::Pixmap::new(width, height)
    }

    /// Whether `bytes` more surface memory fit [`MAX_BLEND_SURFACE_BYTES`]
    /// beside the live surfaces, releasing pooled ones to make room.
    fn surface_room(&mut self, bytes: u64) -> bool {
        while self
            .blend_surface_bytes
            .saturating_add(self.pooled_surface_bytes)
            .saturating_add(bytes)
            > MAX_BLEND_SURFACE_BYTES
        {
            let Some(surface) = self.surface_pool.pop() else {
                return self.blend_surface_bytes.saturating_add(bytes) <= MAX_BLEND_SURFACE_BYTES;
            };
            self.pooled_surface_bytes = self
                .pooled_surface_bytes
                .saturating_sub(surface_bytes(&surface));
        }
        true
    }

    /// Clear a retired surface over `painted`, which holds every pixel it
    /// painted (see [`PaintedBounds`]), and pool it when the budget has room.
    fn recycle_surface(&mut self, mut surface: tiny_skia::Pixmap, painted: Option<PaintedBounds>) {
        let bytes = surface_bytes(&surface);
        if self.surface_pool.len() >= MAX_POOLED_SURFACES
            || self
                .blend_surface_bytes
                .saturating_add(self.pooled_surface_bytes)
                .saturating_add(bytes)
                > MAX_BLEND_SURFACE_BYTES
        {
            return;
        }
        if let Some(region) = painted.filter(|r| !r.is_empty()) {
            let stride = surface.width() as usize * 4;
            let (x0, x1) = (region.x0 as usize * 4, region.x1 as usize * 4);
            let data = surface.data_mut();
            for y in region.y0 as usize..region.y1 as usize {
                if let Some(row) = data.get_mut(y * stride + x0..y * stride + x1) {
                    row.fill(0);
                }
            }
        }
        self.pooled_surface_bytes += bytes;
        self.surface_pool.push(surface);
    }

    /// Intersect the clip with a stroked path's outline. tiny-skia masks cannot
    /// stroke directly, so the stroke is rasterized into a scratch pixmap and
    /// its alpha lifted into a coverage mask. Used to clip pattern/shading
    /// paints to a stroke.
    fn push_clip_stroke(&mut self, path: &Path, style: &StrokeStyle) {
        let Some(pixmap) = self.pixmap.as_ref() else {
            return;
        };
        let (pw, ph) = (pixmap.width(), pixmap.height());

        if self.clip_pixel_spent >= MAX_CLIP_PIXEL_WORK {
            self.clip_stack.push(ClipFrame::Skipped);
            return;
        }
        let Some(skia_path) = self.build_skia_path(path) else {
            self.clip_stack.push(ClipFrame::Skipped);
            return;
        };
        let anisotropic = self.scale != self.scale_y;
        let anisotropic_outline = if anisotropic && style.width.is_finite() && style.width > 0.0 {
            let Some(outline) = self.build_anisotropic_stroke_outline(path, style) else {
                self.clip_stack.push(ClipFrame::Skipped);
                return;
            };
            Some(outline)
        } else {
            None
        };
        let anisotropic_hairline = if anisotropic && anisotropic_outline.is_none() {
            let Some(hairline) = self.build_anisotropic_hairline_path(path, style) else {
                self.clip_stack.push(ClipFrame::Skipped);
                return;
            };
            Some(hairline)
        } else {
            None
        };
        let stroke = if anisotropic {
            Self::skia_stroke(style, self.scale.max(self.scale_y), 1.0)
        } else {
            self.build_skia_stroke(style)
        };
        // Where the stroked outline can reach: the centerline bbox says nothing
        // about joins, caps and miters (see `stroke_overshoot`).
        let extent = if let Some(ref outline) = anisotropic_outline {
            Self::drawn_extent(outline.bounds(), 1.0, pw, ph)
        } else {
            let grow = stroke_overshoot(stroke.width, stroke.miter_limit);
            Self::drawn_extent(skia_path.bounds(), grow, pw, ph)
        };
        let scratch_bytes = pw as u64 * ph as u64 * 4;
        if !self
            .surface_pool
            .iter()
            .any(|p| (p.width(), p.height()) == (pw, ph))
            && !self.surface_room(scratch_bytes)
        {
            self.clip_stack.push(ClipFrame::Skipped);
            return;
        }
        let Some(mut scratch) = self.take_surface(pw, ph) else {
            self.clip_stack.push(ClipFrame::Skipped);
            return;
        };
        let Some(mut mask) = self.take_mask(pw, ph) else {
            self.recycle_surface(scratch, None);
            self.clip_stack.push(ClipFrame::Skipped);
            return;
        };
        self.clip_pixel_spent = self.clip_pixel_spent.saturating_add(extent.area());

        let mut paint = tiny_skia::Paint::default();
        paint.set_color(tiny_skia::Color::WHITE);
        paint.anti_alias = true;
        if let Some(ref outline) = anisotropic_outline {
            scratch.fill_path(
                outline,
                &paint,
                tiny_skia::FillRule::Winding,
                tiny_skia::Transform::identity(),
                None,
            );
        } else if let Some(ref hairline) = anisotropic_hairline {
            let mut hairline_stroke = Self::skia_stroke(style, 1.0, 1.0);
            hairline_stroke.dash = None;
            scratch.stroke_path(
                hairline,
                &paint,
                &hairline_stroke,
                tiny_skia::Transform::identity(),
                None,
            );
        } else {
            scratch.stroke_path(
                &skia_path,
                &paint,
                &stroke,
                tiny_skia::Transform::identity(),
                None,
            );
        }

        // Lift the stroke's coverage into the mask, intersected with the
        // active clip, over the stroke's extent. Outside it the stroke covers
        // nothing, so the mask stays 0 there.
        let w = pw as usize;
        let (x0, x1) = (extent.x0 as usize, extent.x1 as usize);
        let current_data = self.current_clip.as_ref().map(|current| current.data());
        let pixels = scratch.pixels();
        let mask_data = mask.data_mut();
        for y in extent.y0 as usize..extent.y1 as usize {
            let row = y * w;
            for x in x0..x1 {
                let i = row + x;
                let coverage = pixels[i].alpha();
                mask_data[i] = match current_data {
                    Some(current) => mul255(coverage, current[i]),
                    None => coverage,
                };
            }
        }
        self.recycle_surface(scratch, Some(extent));

        let frame = match self.current_clip.take() {
            Some(old) => ClipFrame::Mask(old, self.clip_bounds),
            None => ClipFrame::Empty,
        };
        self.clip_stack.push(frame);
        self.current_clip = Some(mask);
        self.clip_bounds = Some(extent);
    }

    fn push_blend_group(
        &mut self,
        blend_mode: BlendMode,
        isolated: bool,
        knockout: bool,
        alpha: f32,
        mask: Option<&SoftMask>,
    ) {
        // Once a group is skipped, all of its nested groups must also be
        // structural no-ops until the matching pops drain the skipped depth.
        // Otherwise a nested pop could accidentally close the real outer group.
        if self.skipped_blend_depth > 0 {
            self.skipped_blend_depth = self.skipped_blend_depth.saturating_add(1);
            return;
        }
        let depth = self
            .soft_mask_depth
            .saturating_add(self.blend_stack.len())
            .saturating_add(self.non_isolated_depth);
        if depth >= self.max_blend_group_depth {
            self.skipped_blend_depth = 1;
            tracing::warn!(
                limit = self.max_blend_group_depth,
                "blend-group nesting exceeds configured limit; using passthrough"
            );
            return;
        }

        // Rasterize the soft mask before parking the base pixmap (needs dims).
        let mask_plane = mask.and_then(|m| self.soft_mask_plane(m));

        // During Pass 2/3 of an enclosing non-isolated non-`Normal` group we
        // only need accumulated Porter-Duff alpha `α_g` (or geometric shape
        // `f_G` at full opacity) over transparent. Because every PDF blend mode
        // shares `α_out = α_b + α_s * (1 - α_b)`, a nested non-isolated
        // non-`Normal` group contributes the exact same alpha/shape when
        // evaluated in a single pass as an isolated `Normal` group — avoiding
        // exponential `2^depth` recursive replays.
        let (blend_mode, isolated, alpha) = if self.shape_only_pass {
            (
                BlendMode::Normal,
                true,
                if unit(alpha) > 0.0 { 1.0 } else { 0.0 },
            )
        } else if self.alpha_only_pass && !isolated && !knockout && blend_mode != BlendMode::Normal
        {
            (BlendMode::Normal, true, alpha)
        } else {
            (blend_mode, isolated, alpha)
        };

        // A non-isolated group with no group-as-a-unit operation (Normal blend,
        // full alpha, no soft mask, not knockout) is exactly equivalent to
        // drawing its elements straight onto the backdrop — and that is the only
        // way element-level blend modes inside it correctly see the backdrop.
        // Park nothing; a 1×1 sentinel keeps push/pop balanced.
        let passthrough = !isolated
            && !knockout
            && blend_mode == BlendMode::Normal
            && alpha >= 1.0
            && mask_plane.is_none();
        if passthrough {
            if let Some(sentinel) = tiny_skia::Pixmap::new(1, 1) {
                self.blend_stack.push(BlendEntry {
                    pixmap: sentinel,
                    dirty: None,
                    blend_mode,
                    alpha,
                    mask: None,
                    knockout: false,
                    backdrop: None,
                    passthrough: true,
                    non_isolated: false,
                });
            }
            return;
        }

        // A non-isolated group with a non-Normal outer blend mode (e.g. a
        // backdrop-dependent tiling pattern painted under `/BM /Screen`,
        // ISO 32000-1 §11.4.5 / §11.6.7) must evaluate its elements against
        // the live backdrop before applying the outer blend mode, while
        // tracking accumulated group alpha α_g over transparent so untouched
        // pixels and cell gaps are left undisturbed. Record its inner commands
        // until the matching `PopBlendGroup`.
        if !isolated && !knockout && blend_mode != BlendMode::Normal {
            self.recording_non_isolated = Some(RecordedNonIsolatedGroup {
                blend_mode,
                alpha,
                mask: mask_plane,
                nesting: 0,
                cmds: Vec::new(),
            });
            return;
        }

        let pixmap = match self.pixmap.take() {
            Some(p) => p,
            None => return,
        };
        let w = pixmap.width();
        let h = pixmap.height();
        let surface_bytes = w as u64 * h as u64 * 4;

        let pooled = self
            .surface_pool
            .iter()
            .any(|p| (p.width(), p.height()) == (w, h));
        if !pooled && !self.surface_room(surface_bytes) {
            self.pixmap = Some(pixmap);
            tracing::warn!("blend-group surfaces exceed memory budget; using passthrough");
            if let Some(sentinel) = tiny_skia::Pixmap::new(1, 1) {
                self.blend_stack.push(BlendEntry {
                    pixmap: sentinel,
                    dirty: None,
                    blend_mode,
                    alpha,
                    mask: None,
                    knockout: false,
                    backdrop: None,
                    passthrough: true,
                    non_isolated: false,
                });
            }
            return;
        }

        // Non-isolated Normal groups carrying constant alpha and/or a soft mask
        // (e.g. a masked backdrop-dependent tiling pattern, ISO 32000-1 §11.6.7)
        // start from a copy of the live backdrop so inner blend modes evaluate
        // against the real destination, and `pop_blend_group` applies
        // `alpha * mask` once via `mix(backdrop, group, alpha * mask)`.
        let non_isolated = !isolated && !knockout && blend_mode == BlendMode::Normal;
        let mut group = self.take_surface(w, h);
        if non_isolated {
            if let Some(ref mut g) = group {
                g.data_mut().copy_from_slice(pixmap.data());
            }
        }
        let backdrop = None;

        // The backdrop's painted region is parked with it; the group gets a
        // fresh surface of its own.
        let backdrop_dirty = self.dirty.take();

        self.blend_stack.push(BlendEntry {
            pixmap,
            dirty: backdrop_dirty,
            blend_mode,
            alpha,
            mask: mask_plane,
            knockout,
            backdrop,
            passthrough: false,
            non_isolated,
        });

        self.pixmap = group;
        if self.pixmap.is_some() {
            self.blend_surface_bytes += surface_bytes;
        }
    }

    /// Conservative device-space bounding box (in current `self.pixmap`
    /// coordinates) of all drawing commands in `cmds`, intersected with
    /// `self.clip_bounds`. Falls back to `PaintedBounds::whole(w, h)` for
    /// commands whose bounds cannot be bounded cheaply (e.g. Type 3 glyphs).
    fn commands_device_scope(&mut self, cmds: &[RenderCommand], w: u32, h: u32) -> PaintedBounds {
        let mut scope: Option<PaintedBounds> = None;
        let mut extend = |b: PaintedBounds| {
            if !b.is_empty() {
                scope = Some(match scope {
                    Some(cur) => cur.union(b),
                    None => b,
                });
            }
        };
        for cmd in cmds {
            match cmd {
                RenderCommand::FillPath { path, .. } => {
                    if let Some(sp) = self.build_skia_path(path) {
                        extend(Self::drawn_extent(sp.bounds(), 1.0, w, h));
                    }
                }
                RenderCommand::StrokePath { path, style, .. } => {
                    if let Some(sp) = self.build_skia_path(path) {
                        let max_scale = self.scale.max(self.scale_y).max(1.0);
                        let stroke = Self::skia_stroke(style, max_scale, 1.0);
                        let grow = stroke_overshoot(stroke.width, stroke.miter_limit) + 2.0;
                        extend(Self::drawn_extent(sp.bounds(), grow, w, h));
                    }
                }
                RenderCommand::DrawImage(draw) => {
                    let t = &draw.transform;
                    let corners = [
                        (t.e, t.f),
                        (t.a + t.e, t.b + t.f),
                        (t.c + t.e, t.d + t.f),
                        (t.a + t.c + t.e, t.b + t.d + t.f),
                    ];
                    let (mut x0, mut y0) = (f32::INFINITY, f32::INFINITY);
                    let (mut x1, mut y1) = (f32::NEG_INFINITY, f32::NEG_INFINITY);
                    for (px, py) in corners {
                        let dx = self.to_pixel_x(px as f32);
                        let dy = self.flip_y(py as f32);
                        x0 = x0.min(dx);
                        y0 = y0.min(dy);
                        x1 = x1.max(dx);
                        y1 = y1.max(dy);
                    }
                    if x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite() {
                        if let Some(rect) =
                            tiny_skia::Rect::from_ltrb(x0, y0, x1.max(x0 + 1e-4), y1.max(y0 + 1e-4))
                        {
                            extend(Self::drawn_extent(rect, 1.0, w, h));
                        }
                    } else {
                        extend(PaintedBounds::whole(w, h));
                    }
                }
                RenderCommand::DrawInlineImage(draw) => {
                    let t = &draw.transform;
                    let corners = [
                        (t.e, t.f),
                        (t.a + t.e, t.b + t.f),
                        (t.c + t.e, t.d + t.f),
                        (t.a + t.c + t.e, t.b + t.d + t.f),
                    ];
                    let (mut x0, mut y0) = (f32::INFINITY, f32::INFINITY);
                    let (mut x1, mut y1) = (f32::NEG_INFINITY, f32::NEG_INFINITY);
                    for (px, py) in corners {
                        let dx = self.to_pixel_x(px as f32);
                        let dy = self.flip_y(py as f32);
                        x0 = x0.min(dx);
                        y0 = y0.min(dy);
                        x1 = x1.max(dx);
                        y1 = y1.max(dy);
                    }
                    if x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite() {
                        if let Some(rect) =
                            tiny_skia::Rect::from_ltrb(x0, y0, x1.max(x0 + 1e-4), y1.max(y0 + 1e-4))
                        {
                            extend(Self::drawn_extent(rect, 1.0, w, h));
                        }
                    } else {
                        extend(PaintedBounds::whole(w, h));
                    }
                }
                RenderCommand::DrawGlyphRun(run) => {
                    let font = self.font_cache.and_then(|fc| fc.get(run.font_id));
                    let b = font.and_then(|f| self.outline_glyph_run_scope(run, f));
                    extend(b.unwrap_or_else(|| PaintedBounds::whole(w, h)));
                }
                RenderCommand::PushClip { .. }
                | RenderCommand::PushClipStroke { .. }
                | RenderCommand::PopClip
                | RenderCommand::PushBlendGroup { .. }
                | RenderCommand::PopBlendGroup => {}
            }
        }
        let region = scope.unwrap_or(PaintedBounds {
            x0: 0,
            y0: 0,
            x1: 0,
            y1: 0,
        });
        match self.clip_bounds {
            Some(clip) => region.intersect(clip),
            None => region,
        }
    }

    /// Replay a recorded non-isolated non-`Normal` group against both the live
    /// backdrop (producing `(C_n, α_n)`) and a transparent surface (producing
    /// accumulated group alpha `α_g`), perform exact ISO 32000-1 §11.4.5
    /// backdrop removal, fold any soft mask, and composite onto `backdrop` with
    /// `rec.blend_mode` and `rec.alpha`.
    ///
    /// Scopes and bands the `(group_color, group_alpha)` scratch pair to the
    /// affected region so small (and full-page) groups on large rasters (e.g.
    /// 7,000 × 7,000) preserve the live backdrop within [`MAX_BLEND_SURFACE_BYTES`].
    fn finish_recorded_non_isolated_group(&mut self, rec: RecordedNonIsolatedGroup) {
        self.finish_recorded_non_isolated_group_budgeted(rec, MAX_COVERAGE_BAND_BYTES);
    }

    fn finish_recorded_non_isolated_group_budgeted(
        &mut self,
        rec: RecordedNonIsolatedGroup,
        max_band_bytes: u64,
    ) {
        if self.over_budget || rec.cmds.is_empty() {
            return;
        }
        // Compute affected region BEFORE taking `self.pixmap` so
        // `outline_glyph_run_scope` and `build_skia_path`'s extent guard see
        // the real target dimensions (e.g. 20,000 × 16 rasters).
        let Some((w, h)) = self.pixmap.as_ref().map(|p| (p.width(), p.height())) else {
            return;
        };
        let initial_region = self.commands_device_scope(&rec.cmds, w, h);
        if initial_region.is_empty() {
            return;
        }
        let Some(mut backdrop) = self.pixmap.take() else {
            return;
        };
        let parent_knockout = self.in_knockout();
        let rec_alpha = unit(rec.alpha);
        let rec_alpha_u8 = (rec_alpha * 255.0).round().clamp(0.0, 255.0) as u8;

        // Helper: push a non-knockout sentinel frame onto `blend_stack` while
        // replaying `rec.cmds` so: (a) a knockout parent never erases paint
        // accumulating inside this non-knockout child, (b) nested knockout
        // grandchildren still see `in_knockout() == true`, and (c) `blend_stack.len()`
        // accurately counts this group toward `max_blend_group_depth`.
        let run_recorded_pass = |this: &mut Self, target: tiny_skia::Pixmap| {
            let sentinel_pushed = if let Some(sentinel) = tiny_skia::Pixmap::new(1, 1) {
                this.blend_stack.push(BlendEntry {
                    pixmap: sentinel,
                    dirty: None,
                    blend_mode: rec.blend_mode,
                    alpha: rec.alpha,
                    mask: None,
                    knockout: false,
                    backdrop: None,
                    passthrough: true,
                    non_isolated: false,
                });
                true
            } else {
                this.non_isolated_depth = this.non_isolated_depth.saturating_add(1);
                false
            };
            let base_clips = this.clip_stack.len();
            let base_blends = this.blend_stack.len();
            let base_skipped = this.skipped_blend_depth;
            this.pixmap = Some(target);
            this.dirty = None;
            for cmd in &rec.cmds {
                if this.over_budget {
                    break;
                }
                let _ = this.execute(cmd);
            }
            while this.skipped_blend_depth > base_skipped {
                this.skipped_blend_depth -= 1;
            }
            while this.blend_stack.len() > base_blends {
                this.pop_blend_group();
            }
            while this.clip_stack.len() > base_clips {
                this.pop_clip();
            }
            if sentinel_pushed {
                let _ = this.blend_stack.pop();
            } else {
                this.non_isolated_depth = this.non_isolated_depth.saturating_sub(1);
            }
            (this.pixmap.take(), this.dirty.take())
        };

        let page_surface_bytes = w as u64 * h as u64 * 4;
        let needed_surfaces = if parent_knockout { 3u64 } else { 2u64 };
        let full_scratch_bytes = page_surface_bytes.saturating_mul(needed_surfaces);
        let use_full_page =
            initial_region == PaintedBounds::whole(w, h) && full_scratch_bytes <= max_band_bytes;

        if use_full_page {
            let pooled_count = self
                .surface_pool
                .iter()
                .filter(|p| (p.width(), p.height()) == (w, h))
                .count();
            let needed_new_bytes =
                (needed_surfaces as usize).saturating_sub(pooled_count) as u64 * page_surface_bytes;
            if needed_new_bytes == 0 || self.surface_room(needed_new_bytes) {
                if let Some(mut group_color) = self.take_surface(w, h) {
                    if let Some(group_alpha) = self.take_surface(w, h) {
                        let group_shape_opt = if parent_knockout {
                            self.take_surface(w, h)
                        } else {
                            None
                        };
                        if !parent_knockout || group_shape_opt.is_some() {
                            let backdrop_dirty = self.dirty.take();
                            self.blend_surface_bytes =
                                self.blend_surface_bytes.saturating_add(full_scratch_bytes);

                            if parent_knockout {
                                if let Some(b0) =
                                    self.blend_stack.last().and_then(|e| e.backdrop.as_ref())
                                {
                                    group_color.data_mut().copy_from_slice(b0.data());
                                }
                            } else {
                                group_color.data_mut().copy_from_slice(backdrop.data());
                            }
                            let (group_color, _color_dirty) = run_recorded_pass(self, group_color);

                            let prev_alpha_only =
                                std::mem::replace(&mut self.alpha_only_pass, true);
                            let (group_alpha, alpha_dirty) = run_recorded_pass(self, group_alpha);
                            let (group_shape, shape_dirty) = if let Some(gs) = group_shape_opt {
                                let prev_shape_only =
                                    std::mem::replace(&mut self.shape_only_pass, true);
                                let res = run_recorded_pass(self, gs);
                                self.shape_only_pass = prev_shape_only;
                                res
                            } else {
                                (None, None)
                            };
                            self.alpha_only_pass = prev_alpha_only;

                            self.blend_surface_bytes =
                                self.blend_surface_bytes.saturating_sub(full_scratch_bytes);

                            let (mut group_color, group_alpha) = match (group_color, group_alpha) {
                                (Some(gc), Some(ga)) => (gc, ga),
                                (gc_opt, ga_opt) => {
                                    if let Some(gc) = gc_opt {
                                        let whole = PaintedBounds::whole(gc.width(), gc.height());
                                        self.recycle_surface(gc, Some(whole));
                                    }
                                    if let Some(ga) = ga_opt {
                                        self.recycle_surface(ga, alpha_dirty);
                                    }
                                    if let Some(gs) = group_shape {
                                        self.recycle_surface(gs, shape_dirty);
                                    }
                                    self.dirty = backdrop_dirty;
                                    self.pixmap = Some(backdrop);
                                    return;
                                }
                            };

                            let region = match (alpha_dirty, shape_dirty) {
                                (Some(a), Some(b)) => Some(a.union(b)),
                                (a, b) => a.or(b),
                            }
                            .filter(|r| !r.is_empty());
                            if let Some(region) = region {
                                let stride = w as usize;
                                let parent_b0_data = if parent_knockout {
                                    self.blend_stack
                                        .last()
                                        .and_then(|e| e.backdrop.as_ref())
                                        .map(|p| p.data())
                                } else {
                                    Some(backdrop.data())
                                };
                                let alpha_data = group_alpha.data();
                                let col_data = group_color.data_mut();
                                let (x0, x1) = (region.x0 as usize, region.x1 as usize);
                                for y in region.y0 as usize..region.y1 as usize {
                                    let row = y * stride;
                                    let m_row = rec
                                        .mask
                                        .as_ref()
                                        .and_then(|plane| plane.get(row + x0..row + x1));
                                    for (dx, x) in (x0..x1).enumerate() {
                                        let p = (row + x) * 4;
                                        let ga_u8 = alpha_data[p + 3];
                                        if ga_u8 == 0 {
                                            col_data[p..p + 4].fill(0);
                                            continue;
                                        }
                                        let m_u8 = m_row.map_or(255, |r| r[dx]);
                                        let eff_m = if parent_knockout {
                                            mul255(m_u8, rec_alpha_u8)
                                        } else {
                                            m_u8
                                        };
                                        if eff_m == 0 {
                                            col_data[p..p + 4].fill(0);
                                            continue;
                                        }
                                        let a_g = ga_u8 as f32 / 255.0;
                                        let a_n = col_data[p + 3] as f32 / 255.0;
                                        let a_0 =
                                            parent_b0_data.map_or(0.0, |b| b[p + 3] as f32 / 255.0);
                                        let factor = a_0 / a_g - a_0;
                                        for ch in 0..3 {
                                            let c_n = if a_n > 0.0 {
                                                (col_data[p + ch] as f32 / 255.0) / a_n
                                            } else {
                                                0.0
                                            };
                                            let c_0 = if a_0 > 0.0 {
                                                parent_b0_data.map_or(0.0, |b| {
                                                    (b[p + ch] as f32 / 255.0) / a_0
                                                })
                                            } else {
                                                0.0
                                            };
                                            let c_g = (c_n + (c_n - c_0) * factor).clamp(0.0, 1.0);
                                            let premul =
                                                (c_g * a_g * 255.0).round().clamp(0.0, 255.0) as u8;
                                            col_data[p + ch] = if eff_m == 255 {
                                                premul
                                            } else {
                                                mul255(premul, eff_m)
                                            };
                                        }
                                        col_data[p + 3] = if eff_m == 255 {
                                            ga_u8
                                        } else {
                                            mul255(ga_u8, eff_m)
                                        };
                                    }
                                }

                                if parent_knockout {
                                    if let Some(ref gs) = group_shape {
                                        let paint = tiny_skia::PixmapPaint {
                                            blend_mode: Self::blend_mode_to_skia(rec.blend_mode),
                                            opacity: 1.0,
                                            ..Default::default()
                                        };
                                        let b0_ref = self
                                            .blend_stack
                                            .last()
                                            .and_then(|e| e.backdrop.as_ref());
                                        if let Some(b0_pix) = b0_ref {
                                            if let Some(mut x_pix) = tiny_skia::Pixmap::new(w, h) {
                                                x_pix.data_mut().copy_from_slice(b0_pix.data());
                                                if let Some(rect) = region.to_rect() {
                                                    composite_group_region(
                                                        &mut x_pix,
                                                        &group_color,
                                                        &paint,
                                                        rect,
                                                    );
                                                }
                                                for_region_rows(region, w, |range| {
                                                    knockout_merge_premultiplied(
                                                        &mut backdrop.data_mut()[range.clone()],
                                                        &x_pix.data()[range.clone()],
                                                        &gs.data()[range.clone()],
                                                        Some(&b0_pix.data()[range]),
                                                    );
                                                });
                                            }
                                        } else {
                                            for_region_rows(region, w, |range| {
                                                knockout_merge_premultiplied(
                                                    &mut backdrop.data_mut()[range.clone()],
                                                    &group_color.data()[range.clone()],
                                                    &gs.data()[range],
                                                    None,
                                                );
                                            });
                                        }
                                    }
                                } else {
                                    let paint = tiny_skia::PixmapPaint {
                                        blend_mode: Self::blend_mode_to_skia(rec.blend_mode),
                                        opacity: rec_alpha,
                                        ..Default::default()
                                    };
                                    if let Some(rect) = region.to_rect() {
                                        composite_group_region(
                                            &mut backdrop,
                                            &group_color,
                                            &paint,
                                            rect,
                                        );
                                    }
                                }
                                self.dirty = Some(match backdrop_dirty {
                                    Some(b) => b.union(region),
                                    None => region,
                                });
                            } else {
                                self.dirty = backdrop_dirty;
                            }

                            let whole =
                                PaintedBounds::whole(group_color.width(), group_color.height());
                            self.recycle_surface(group_color, Some(whole));
                            self.recycle_surface(group_alpha, alpha_dirty);
                            if let Some(gs) = group_shape {
                                self.recycle_surface(gs, shape_dirty);
                            }
                            self.pixmap = Some(backdrop);
                            return;
                        }
                        self.recycle_surface(group_alpha, None);
                    }
                    self.recycle_surface(group_color, None);
                }
            }
        }

        // Region-scoped / banded path: size the scratch surfaces to
        // `initial_region` (and split into bands when large) so small or large
        // non-isolated groups on 7,000 × 7,000 rasters stay well within
        // `MAX_BLEND_SURFACE_BYTES` while preserving the live backdrop.
        while !self.surface_pool.is_empty()
            && self
                .blend_surface_bytes
                .saturating_add(self.pooled_surface_bytes)
                .saturating_add(max_band_bytes)
                > MAX_BLEND_SURFACE_BYTES
        {
            if let Some(s) = self.surface_pool.pop() {
                self.pooled_surface_bytes =
                    self.pooled_surface_bytes.saturating_sub(surface_bytes(&s));
            }
        }
        let avail_bytes = MAX_BLEND_SURFACE_BYTES.saturating_sub(self.blend_surface_bytes);
        let budget_bytes = avail_bytes.min(max_band_bytes);
        let bytes_per_pixel = needed_surfaces * 4;
        if budget_bytes < bytes_per_pixel {
            tracing::warn!(
                "non-isolated blend group scratch exceeds memory budget; falling back to isolated group"
            );
            self.pixmap = Some(backdrop);
            self.push_blend_group(rec.blend_mode, true, false, rec.alpha, None);
            if let Some(last) = self.blend_stack.last_mut() {
                last.mask = rec.mask;
            }
            for cmd in &rec.cmds {
                let _ = self.execute(cmd);
            }
            self.pop_blend_group();
            return;
        }

        let reg_w = initial_region.x1 - initial_region.x0;
        let reg_h = initial_region.y1 - initial_region.y0;
        let max_band_pixels = (budget_bytes / bytes_per_pixel).max(1);
        let (band_w, band_h) = if reg_w as u64 * reg_h as u64 <= max_band_pixels {
            (reg_w, reg_h)
        } else {
            let bw = (reg_w as u64).min(max_band_pixels) as u32;
            let bh = (max_band_pixels / bw as u64).clamp(1, reg_h as u64) as u32;
            (bw, bh)
        };

        let backdrop_dirty = self.dirty.take();
        let saved_ox = self.dev_origin_x;
        let saved_oy = self.dev_origin_y;
        let saved_clip = self.current_clip.take();
        let saved_clip_bounds = self.clip_bounds.take();
        let saved_clip_stack = std::mem::take(&mut self.clip_stack);
        let saved_mask_pool = std::mem::take(&mut self.mask_pool);
        let saved_soft_masks = std::mem::take(&mut self.soft_mask_planes);
        let saved_soft_mask_bytes = std::mem::replace(&mut self.soft_mask_cache_bytes, 0);

        let mut merged_dirty: Option<PaintedBounds> = None;
        let mut color_buf: Option<tiny_skia::Pixmap> = None;
        let mut alpha_buf: Option<tiny_skia::Pixmap> = None;
        let mut shape_buf: Option<tiny_skia::Pixmap> = None;
        let mut live_bytes = 0u64;
        let page_w = w as usize;
        let paint = tiny_skia::PixmapPaint {
            blend_mode: Self::blend_mode_to_skia(rec.blend_mode),
            opacity: if parent_knockout { 1.0 } else { rec_alpha },
            ..Default::default()
        };

        'bands: for by0 in (initial_region.y0..initial_region.y1).step_by(band_h as usize) {
            let cur_h = (initial_region.y1 - by0).min(band_h);
            for bx0 in (initial_region.x0..initial_region.x1).step_by(band_w as usize) {
                if self.over_budget {
                    break 'bands;
                }
                let cur_w = (initial_region.x1 - bx0).min(band_w);
                let bw_usize = cur_w as usize;

                // Slice outer clip mask into band coordinates (skip band if completely clipped out).
                let band_clip = if let Some(ref cm) = saved_clip {
                    let mut m = match tiny_skia::Mask::new(cur_w, cur_h) {
                        Some(m) => m,
                        None => break 'bands,
                    };
                    let src = cm.data();
                    let dst = m.data_mut();
                    let mut any_nonzero = false;
                    for ly in 0..cur_h as usize {
                        let s_off = (by0 as usize + ly) * page_w + bx0 as usize;
                        let d_off = ly * bw_usize;
                        let row_src = &src[s_off..s_off + bw_usize];
                        dst[d_off..d_off + bw_usize].copy_from_slice(row_src);
                        if !any_nonzero && row_src.iter().any(|&v| v != 0) {
                            any_nonzero = true;
                        }
                    }
                    if !any_nonzero {
                        continue;
                    }
                    Some(m)
                } else {
                    None
                };

                let reuse = color_buf
                    .as_ref()
                    .is_some_and(|p| p.width() == cur_w && p.height() == cur_h);
                if reuse {
                    if let Some(ref mut a) = alpha_buf {
                        a.data_mut().fill(0);
                    }
                    if let Some(ref mut s) = shape_buf {
                        s.data_mut().fill(0);
                    }
                } else {
                    color_buf = None;
                    alpha_buf = None;
                    shape_buf = None;
                    self.blend_surface_bytes = self.blend_surface_bytes.saturating_sub(live_bytes);
                    live_bytes = 0;
                    let band_bytes = cur_w as u64 * cur_h as u64 * bytes_per_pixel;
                    if !self.surface_room(band_bytes) {
                        break 'bands;
                    }
                    let Some(gc) = tiny_skia::Pixmap::new(cur_w, cur_h) else {
                        break 'bands;
                    };
                    let Some(ga) = tiny_skia::Pixmap::new(cur_w, cur_h) else {
                        break 'bands;
                    };
                    let gs = if parent_knockout {
                        match tiny_skia::Pixmap::new(cur_w, cur_h) {
                            Some(s) => Some(s),
                            None => break 'bands,
                        }
                    } else {
                        None
                    };
                    self.blend_surface_bytes = self.blend_surface_bytes.saturating_add(band_bytes);
                    live_bytes = band_bytes;
                    color_buf = Some(gc);
                    alpha_buf = Some(ga);
                    shape_buf = gs;
                }

                let Some(mut gc) = color_buf.take() else {
                    break 'bands;
                };
                let Some(ga) = alpha_buf.take() else {
                    color_buf = Some(gc);
                    break 'bands;
                };
                let gs_in = shape_buf.take();

                self.dev_origin_x = saved_ox.saturating_add(bx0);
                self.dev_origin_y = saved_oy.saturating_add(by0);
                if !self.soft_mask_planes.is_empty() {
                    self.soft_mask_planes.clear();
                    self.soft_mask_cache_bytes = 0;
                }
                self.current_clip = band_clip;
                self.clip_bounds = self
                    .current_clip
                    .as_ref()
                    .map(|_| PaintedBounds::whole(cur_w, cur_h));

                // Seed `gc` from the active initial backdrop (`parent_b0` if inside
                // a knockout parent, else `backdrop`) over this band.
                let row_bytes = bw_usize * 4;
                if parent_knockout {
                    if let Some(b0_pix) = self.blend_stack.last().and_then(|e| e.backdrop.as_ref())
                    {
                        let b0_data = b0_pix.data();
                        let gc_data = gc.data_mut();
                        for ly in 0..cur_h as usize {
                            let dst_off = ly * row_bytes;
                            let src_off = ((by0 as usize + ly) * page_w + bx0 as usize) * 4;
                            gc_data[dst_off..dst_off + row_bytes]
                                .copy_from_slice(&b0_data[src_off..src_off + row_bytes]);
                        }
                    } else {
                        gc.data_mut().fill(0);
                    }
                } else {
                    let base_data = backdrop.data();
                    let gc_data = gc.data_mut();
                    for ly in 0..cur_h as usize {
                        let dst_off = ly * row_bytes;
                        let src_off = ((by0 as usize + ly) * page_w + bx0 as usize) * 4;
                        gc_data[dst_off..dst_off + row_bytes]
                            .copy_from_slice(&base_data[src_off..src_off + row_bytes]);
                    }
                }

                let (gc_opt, _color_dirty) = run_recorded_pass(self, gc);
                let prev_alpha_only = std::mem::replace(&mut self.alpha_only_pass, true);
                let (ga_opt, alpha_dirty) = run_recorded_pass(self, ga);
                let (gs_opt, shape_dirty) = if let Some(gs) = gs_in {
                    let prev_shape_only = std::mem::replace(&mut self.shape_only_pass, true);
                    let res = run_recorded_pass(self, gs);
                    self.shape_only_pass = prev_shape_only;
                    res
                } else {
                    (None, None)
                };
                self.alpha_only_pass = prev_alpha_only;

                self.current_clip = None;
                self.clip_bounds = None;
                self.clip_stack.clear();

                let (Some(mut gc), Some(ga)) = (gc_opt, ga_opt) else {
                    break 'bands;
                };

                let band_region = match (alpha_dirty, shape_dirty) {
                    (Some(a), Some(b)) => Some(a.union(b)),
                    (a, b) => a.or(b),
                }
                .filter(|r| !r.is_empty());

                if let Some(local_region) = band_region {
                    let parent_b0_data = if parent_knockout {
                        self.blend_stack
                            .last()
                            .and_then(|e| e.backdrop.as_ref())
                            .map(|p| p.data())
                    } else {
                        Some(backdrop.data())
                    };
                    let alpha_data = ga.data();
                    let col_data = gc.data_mut();
                    let (lx0, lx1) = (local_region.x0 as usize, local_region.x1 as usize);
                    let (ly0, ly1) = (local_region.y0 as usize, local_region.y1 as usize);

                    for ly in 0..cur_h as usize {
                        let row_start = ly * bw_usize;
                        if ly < ly0 || ly >= ly1 {
                            col_data[row_start * 4..(row_start + bw_usize) * 4].fill(0);
                            continue;
                        }
                        if lx0 > 0 {
                            col_data[row_start * 4..(row_start + lx0) * 4].fill(0);
                        }
                        if lx1 < bw_usize {
                            col_data[(row_start + lx1) * 4..(row_start + bw_usize) * 4].fill(0);
                        }
                        let page_row = (by0 as usize + ly) * page_w + bx0 as usize;
                        let m_row = rec
                            .mask
                            .as_ref()
                            .and_then(|plane| plane.get(page_row + lx0..page_row + lx1));
                        for (dx, lx) in (lx0..lx1).enumerate() {
                            let lp = (row_start + lx) * 4;
                            let ga_u8 = alpha_data[lp + 3];
                            if ga_u8 == 0 {
                                col_data[lp..lp + 4].fill(0);
                                continue;
                            }
                            let m_u8 = m_row.map_or(255, |r| r[dx]);
                            let eff_m = if parent_knockout {
                                mul255(m_u8, rec_alpha_u8)
                            } else {
                                m_u8
                            };
                            if eff_m == 0 {
                                col_data[lp..lp + 4].fill(0);
                                continue;
                            }
                            let bp = (page_row + lx) * 4;
                            let a_g = ga_u8 as f32 / 255.0;
                            let a_n = col_data[lp + 3] as f32 / 255.0;
                            let a_0 = parent_b0_data.map_or(0.0, |b| b[bp + 3] as f32 / 255.0);
                            let factor = a_0 / a_g - a_0;
                            for ch in 0..3 {
                                let c_n = if a_n > 0.0 {
                                    (col_data[lp + ch] as f32 / 255.0) / a_n
                                } else {
                                    0.0
                                };
                                let c_0 = if a_0 > 0.0 {
                                    parent_b0_data
                                        .map_or(0.0, |b| (b[bp + ch] as f32 / 255.0) / a_0)
                                } else {
                                    0.0
                                };
                                let c_g = (c_n + (c_n - c_0) * factor).clamp(0.0, 1.0);
                                let premul = (c_g * a_g * 255.0).round().clamp(0.0, 255.0) as u8;
                                col_data[lp + ch] = if eff_m == 255 {
                                    premul
                                } else {
                                    mul255(premul, eff_m)
                                };
                            }
                            col_data[lp + 3] = if eff_m == 255 {
                                ga_u8
                            } else {
                                mul255(ga_u8, eff_m)
                            };
                        }
                    }

                    if parent_knockout {
                        if let Some(ref gs) = gs_opt {
                            let b0_ref = self.blend_stack.last().and_then(|e| e.backdrop.as_ref());
                            let x_band = if let Some(b0_pix) = b0_ref {
                                if let Some(mut xb) = tiny_skia::Pixmap::new(cur_w, cur_h) {
                                    let b0_data = b0_pix.data();
                                    let xb_data = xb.data_mut();
                                    for ly in 0..cur_h as usize {
                                        let dst_off = ly * row_bytes;
                                        let src_off =
                                            ((by0 as usize + ly) * page_w + bx0 as usize) * 4;
                                        xb_data[dst_off..dst_off + row_bytes].copy_from_slice(
                                            &b0_data[src_off..src_off + row_bytes],
                                        );
                                    }
                                    xb.draw_pixmap(
                                        0,
                                        0,
                                        gc.as_ref(),
                                        &paint,
                                        tiny_skia::Transform::identity(),
                                        None,
                                    );
                                    Some(xb)
                                } else {
                                    None
                                }
                            } else {
                                None
                            };
                            let elem_data = x_band.as_ref().map_or(gc.data(), |xb| xb.data());
                            let b0_data = x_band.as_ref().and(b0_ref).map(|p| p.data());
                            let shape_data = gs.data();
                            let dst_data = backdrop.data_mut();
                            for ly in ly0..ly1 {
                                let d_start =
                                    ((by0 as usize + ly) * page_w + bx0 as usize + lx0) * 4;
                                let d_end = ((by0 as usize + ly) * page_w + bx0 as usize + lx1) * 4;
                                let s_start = (ly * bw_usize + lx0) * 4;
                                let s_end = (ly * bw_usize + lx1) * 4;
                                knockout_merge_premultiplied(
                                    &mut dst_data[d_start..d_end],
                                    &elem_data[s_start..s_end],
                                    &shape_data[s_start..s_end],
                                    b0_data.map(|b| &b[d_start..d_end]),
                                );
                            }
                        }
                    } else {
                        backdrop.draw_pixmap(
                            bx0 as i32,
                            by0 as i32,
                            gc.as_ref(),
                            &paint,
                            tiny_skia::Transform::identity(),
                            None,
                        );
                    }
                    let page_active = PaintedBounds {
                        x0: bx0 + local_region.x0,
                        y0: by0 + local_region.y0,
                        x1: bx0 + local_region.x1,
                        y1: by0 + local_region.y1,
                    };
                    merged_dirty = Some(match merged_dirty {
                        Some(m) => m.union(page_active),
                        None => page_active,
                    });
                }

                color_buf = Some(gc);
                alpha_buf = Some(ga);
                shape_buf = gs_opt;
            }
        }

        drop(color_buf);
        drop(alpha_buf);
        drop(shape_buf);
        self.blend_surface_bytes = self.blend_surface_bytes.saturating_sub(live_bytes);

        let mut kept_bytes = 0u64;
        self.surface_pool.retain(|p| {
            let keep = p.width() == w && p.height() == h;
            if keep {
                kept_bytes = kept_bytes.saturating_add(surface_bytes(p));
            }
            keep
        });
        self.pooled_surface_bytes = kept_bytes;

        let band_mask_bytes: u64 = self
            .mask_pool
            .iter()
            .map(|m| m.width() as u64 * m.height() as u64)
            .sum();
        self.clip_mask_bytes = self.clip_mask_bytes.saturating_sub(band_mask_bytes);
        self.mask_pool = saved_mask_pool;
        self.clip_stack = saved_clip_stack;
        self.current_clip = saved_clip;
        self.clip_bounds = saved_clip_bounds;
        self.soft_mask_planes = saved_soft_masks;
        self.soft_mask_cache_bytes = saved_soft_mask_bytes;
        self.dev_origin_x = saved_ox;
        self.dev_origin_y = saved_oy;

        self.dirty = match (backdrop_dirty, merged_dirty) {
            (Some(b), Some(m)) => Some(b.union(m)),
            (b, m) => b.or(m),
        };
        self.pixmap = Some(backdrop);
    }

    fn pop_blend_group(&mut self) {
        let entry = match self.blend_stack.pop() {
            Some(e) => e,
            None => return,
        };

        // Passthrough group: elements already drew onto the live canvas.
        if entry.passthrough {
            return;
        }

        // Timed from here: folding the mask and compositing the group is the
        // actual blend-group cost, while the guards above do no work.
        let t = self.tick();

        let mut group_pixmap = match self.pixmap.take() {
            Some(p) => p,
            None => {
                self.pixmap = Some(entry.pixmap);
                // Nothing was composited, so the backdrop's painted region is
                // exactly what it was before the group opened.
                self.dirty = entry.dirty;
                self.tock(t, |s| &mut s.soft_mask_ns);
                return;
            }
        };

        // The group's painted region, claimed from the surface being retired.
        // `None` (or an empty region) means it painted nothing, which makes the
        // fold and the composite below no-ops: the backdrop is left untouched
        // and cannot have been affected.
        let group_dirty = self.dirty.take();
        let region = group_dirty.filter(|r| !r.is_empty());

        if entry.non_isolated {
            let mut base = entry.pixmap;
            if let Some(region) = region {
                let alpha = unit(entry.alpha);
                let stride = base.width() as usize;
                let dst_data = base.data_mut();
                let src_data = group_pixmap.data();
                for y in region.y0 as usize..region.y1 as usize {
                    let row = y * stride;
                    let m_row = entry.mask.as_ref().and_then(|plane| {
                        plane.get(row + region.x0 as usize..row + region.x1 as usize)
                    });
                    for (dx, x) in (region.x0 as usize..region.x1 as usize).enumerate() {
                        let m = m_row.map_or(1.0, |r| r[dx] as f32 / 255.0);
                        let w_eff = (alpha * m).clamp(0.0, 1.0);
                        if w_eff <= 0.0 {
                            continue;
                        }
                        let p = (row + x) * 4;
                        if w_eff >= 1.0 {
                            dst_data[p..p + 4].copy_from_slice(&src_data[p..p + 4]);
                        } else {
                            let inv = 1.0 - w_eff;
                            for ch in 0..4 {
                                dst_data[p + ch] = (dst_data[p + ch] as f32 * inv
                                    + src_data[p + ch] as f32 * w_eff)
                                    .round()
                                    .clamp(0.0, 255.0)
                                    as u8;
                            }
                        }
                    }
                }
            }
            self.pixmap = Some(base);
            self.dirty = match (entry.dirty, group_dirty) {
                (Some(backdrop), Some(group)) => Some(backdrop.union(group)),
                (backdrop, group) => backdrop.or(group),
            };
            self.blend_surface_bytes = self
                .blend_surface_bytes
                .saturating_sub(surface_bytes(&group_pixmap));
            // The entire group surface was seeded with `base`, so clear all of
            // it when returning it to `surface_pool`.
            let whole = PaintedBounds::whole(group_pixmap.width(), group_pixmap.height());
            self.recycle_surface(group_pixmap, Some(whole));
            self.tock(t, |s| &mut s.soft_mask_ns);
            return;
        }

        // Fold the soft mask into the group: premultiplied RGBA scales
        // uniformly by the per-pixel mask coverage. Confined to the composite
        // region — outside it the group is transparent, and a transparent pixel
        // stays transparent under any fold, so the composite (confined to the
        // same region) cannot observe the difference.
        if let (Some(plane), Some(region)) = (&entry.mask, region) {
            let t_fold = self.tick();
            let stride = group_pixmap.width() as usize;
            debug_assert_eq!(
                plane.len(),
                stride * group_pixmap.height() as usize,
                "a mask plane is page-sized, like every surface it is folded into"
            );
            let (x0, x1) = (region.x0 as usize, region.x1 as usize);
            let data = group_pixmap.data_mut();
            for y in region.y0 as usize..region.y1 as usize {
                let row = y * stride;
                let Some(m_row) = plane.get(row + x0..row + x1) else {
                    continue;
                };
                let px_row = &mut data[(row + x0) * 4..(row + x1) * 4];
                for (px, &m) in px_row.as_chunks_mut::<4>().0.iter_mut().zip(m_row.iter()) {
                    if m == 255 {
                        continue;
                    }
                    px[0] = mul255(px[0], m);
                    px[1] = mul255(px[1], m);
                    px[2] = mul255(px[2], m);
                    px[3] = mul255(px[3], m);
                }
            }
            self.tock(t_fold, |s| &mut s.mask_fold_ns);
        }

        let mut base = entry.pixmap;
        let blend = Self::blend_mode_to_skia(entry.blend_mode);

        let paint = tiny_skia::PixmapPaint {
            blend_mode: blend,
            opacity: unit(entry.alpha),
            ..Default::default()
        };

        // Diagnostics, and a full pass — so only while collecting stats, and
        // written before the composite timer starts so they cannot inflate that
        // bucket. `content` is the ideal scope, `region` what was actually
        // processed, and `leak` must be zero: any painted pixel outside the
        // region would mean the region is not the superset the composite's
        // correctness rests on.
        if self.stage_stats.is_some() {
            let (content, leak) = group_coverage(&group_pixmap, region);
            self.count(content.map_or(0, PaintedBounds::area), |s| {
                &mut s.mask_composite_px
            });
            self.count(region.map_or(0, PaintedBounds::area), |s| {
                &mut s.mask_composite_region_px
            });
            self.count(leak, |s| &mut s.mask_composite_leak_px);
        }

        let t_composite = self.tick();
        if let Some(rect) = region.and_then(PaintedBounds::to_rect) {
            composite_group_region(&mut base, &group_pixmap, &paint, rect);
        }
        self.tock(t_composite, |s| &mut s.mask_composite_ns);

        self.pixmap = Some(base);
        // The composite can only have touched the group's region, so the
        // backdrop's region is its own plus that.
        self.dirty = match (entry.dirty, group_dirty) {
            (Some(backdrop), Some(group)) => Some(backdrop.union(group)),
            (backdrop, group) => backdrop.or(group),
        };
        self.blend_surface_bytes = self
            .blend_surface_bytes
            .saturating_sub(surface_bytes(&group_pixmap));
        self.recycle_surface(group_pixmap, group_dirty);
        self.tock(t, |s| &mut s.soft_mask_ns);
    }

    /// Dispatch the four painting commands to their renderers (shared by the
    /// normal path and the knockout path).
    fn render_paint_cmd(&mut self, cmd: &RenderCommand) {
        if self.shape_only_pass {
            self.render_shape_cmd(cmd);
            return;
        }
        // Overprint is intercepted in `execute` before this; the plain
        // renderers below ignore it (so do the scratch passes that reuse them).
        match cmd {
            RenderCommand::FillPath {
                path,
                rule,
                paint,
                alpha,
                ..
            } => self.render_fill(path, rule, paint, *alpha),
            RenderCommand::StrokePath {
                path,
                style,
                paint,
                alpha,
                ..
            } => self.render_stroke(path, style, paint, *alpha),
            RenderCommand::DrawGlyphRun(run) => self.render_glyph_run(run),
            RenderCommand::DrawImage(draw) => self.render_image(draw),
            RenderCommand::DrawInlineImage(draw) => self.render_inline_image(draw),
            _ => {}
        }
    }

    /// True while the innermost open transparency group is a knockout group.
    fn in_knockout(&self) -> bool {
        self.blend_stack.last().map(|e| e.knockout).unwrap_or(false)
    }

    /// Render a painting command at full opacity to capture its *shape* (the
    /// geometric/anti-aliased coverage), independent of its opacity.
    fn render_shape_cmd(&mut self, cmd: &RenderCommand) {
        let opaque_paint = |paint: &Paint| -> Paint {
            match paint {
                Paint::Solid(c) if c.a > 0.0 => Paint::Solid(Color::rgba(c.r, c.g, c.b, 1.0)),
                other => other.clone(),
            }
        };
        match cmd {
            RenderCommand::FillPath {
                path, rule, paint, ..
            } => {
                let p = opaque_paint(paint);
                self.render_fill(path, rule, &p, 1.0);
            }
            RenderCommand::StrokePath {
                path, style, paint, ..
            } => {
                let p = opaque_paint(paint);
                self.render_stroke(path, style, &p, 1.0);
            }
            RenderCommand::DrawGlyphRun(run) => {
                if let Paint::Solid(c) = &run.paint {
                    if c.a > 0.0 && c.a < 1.0 {
                        let mut opaque_run = run.clone();
                        opaque_run.paint = Paint::Solid(Color::rgba(c.r, c.g, c.b, 1.0));
                        self.render_glyph_run_with_alpha(&opaque_run, 1.0);
                        return;
                    }
                }
                self.render_glyph_run_with_alpha(run, 1.0);
            }
            RenderCommand::DrawImage(draw) => self.render_image_with_alpha(draw, 1.0),
            RenderCommand::DrawInlineImage(draw) => self.render_inline_image_with_alpha(draw, 1.0),
            _ => {}
        }
    }

    /// Paint one element inside a knockout group: render it alone, composite it
    /// over the group's initial backdrop, and replace the group buffer in
    /// proportion to the element's *shape* (so it knocks out preceding elements
    /// rather than accumulating over them). The shape — captured by a full-
    /// opacity pass — is what PDF 11.4.9 uses, so a semi-transparent solid fill
    /// still fully knocks out (it shows the backdrop through itself, not the
    /// elements beneath).
    fn knockout_paint(&mut self, cmd: &RenderCommand) {
        let (w, h) = match self.pixmap.as_ref() {
            Some(p) => (p.width(), p.height()),
            None => return,
        };
        let scratch_bytes = w as u64 * h as u64 * 4 * 2;
        if !self.surface_room(scratch_bytes) {
            tracing::warn!("knockout scratch surfaces exceed memory budget; using accumulation");
            self.render_paint_cmd(cmd);
            return;
        }

        // Pass 1: the element as painted (real colour and opacity).
        // The scratch passes below draw into throwaway surfaces, so the region
        // the *canvas* has been painted in is put aside and restored after them;
        // each pass starts from an empty region so it reports its own extent.
        let canvas_dirty = self.dirty.take();
        let group = self.pixmap.take();
        self.pixmap = self.take_surface(w, h);
        if self.pixmap.is_none() {
            self.pixmap = group;
            self.dirty = canvas_dirty;
            self.render_paint_cmd(cmd); // OOM: fall back to accumulation
            return;
        }
        self.render_paint_cmd(cmd);
        let elem = self.pixmap.take();
        let elem_dirty = self.dirty.take();

        // Pass 2: the element at full opacity → its shape (alpha = coverage).
        self.pixmap = self.take_surface(w, h);
        if self.pixmap.is_none() {
            self.pixmap = group;
            self.dirty = canvas_dirty;
            if let Some(elem) = elem {
                self.recycle_surface(elem, elem_dirty);
            }
            return;
        }
        self.render_shape_cmd(cmd);
        let shape = self.pixmap.take();
        let shape_dirty = self.dirty.take();

        self.pixmap = group;
        self.dirty = canvas_dirty;
        let (elem, shape) = match (elem, shape) {
            (Some(e), Some(s)) => (e, s),
            _ => return,
        };
        // The merge changes only pixels the shape covers, all inside the
        // passes' regions.
        let region = match (elem_dirty, shape_dirty) {
            (Some(a), Some(b)) => Some(a.union(b)),
            (a, b) => a.or(b),
        };
        if let (Some(region), Some(g), Some(entry)) =
            (region, self.pixmap.as_mut(), self.blend_stack.last())
        {
            let b0 = entry.backdrop.as_ref().map(|p| p.data());
            for_region_rows(region, w, |range| {
                knockout_merge(
                    &mut g.data_mut()[range.clone()],
                    &elem.data()[range.clone()],
                    &shape.data()[range.clone()],
                    b0.map(|b| &b[range]),
                );
            });
            self.dirty = Some(match self.dirty {
                Some(dirty) => dirty.union(region),
                None => region,
            });
        }
        self.recycle_surface(elem, elem_dirty);
        self.recycle_surface(shape, shape_dirty);
    }

    /// Paint an overprinting element (PDF 8.6.7): render it alone to capture
    /// per-pixel coverage·alpha, then merge it onto the canvas in naïve
    /// subtractive CMYK — painting only the `active` colorants and reading the
    /// rest from the backdrop, so it never disturbs colorants it does not name.
    fn overprint_paint(&mut self, cmd: &RenderCommand, op: &Overprint) {
        // No colorants painted → the op leaves every channel as the backdrop, a
        // pure no-op. Skip it (also avoids spuriously raising a non-opaque
        // backdrop's alpha and the full-page scratch work).
        if op.active == 0 {
            return;
        }
        let (w, h) = match self.pixmap.as_ref() {
            Some(p) => (p.width(), p.height()),
            None => return,
        };
        let scratch_bytes = w as u64 * h as u64 * 4;
        if !self.surface_room(scratch_bytes) {
            tracing::warn!("overprint scratch surface exceeds memory budget; using source-over");
            self.render_paint_cmd(cmd);
            return;
        }
        // Render the element (real colour/opacity, current clip) into a scratch
        // buffer; only its alpha channel — the covered fraction — is consumed.
        // As in `knockout_paint`, the canvas's painted region is put aside while
        // the scratch surface is the target, which reports the element's extent.
        let canvas_dirty = self.dirty.take();
        let canvas = self.pixmap.take();
        self.pixmap = self.take_surface(w, h);
        if self.pixmap.is_none() {
            // OOM: fall back to a normal paint so content still appears.
            self.pixmap = canvas;
            self.dirty = canvas_dirty;
            self.render_paint_cmd(cmd);
            return;
        }
        self.render_paint_cmd(cmd);
        let elem = self.pixmap.take();
        let elem_dirty = self.dirty.take();
        self.pixmap = canvas;
        self.dirty = canvas_dirty;
        let Some(elem) = elem else {
            return;
        };
        // The merge changes only pixels the element covers.
        if let (Some(region), Some(canvas)) = (elem_dirty, self.pixmap.as_mut()) {
            for_region_rows(region, w, |range| {
                overprint_merge(
                    &mut canvas.data_mut()[range.clone()],
                    &elem.data()[range],
                    op,
                );
            });
            self.dirty = Some(match self.dirty {
                Some(dirty) => dirty.union(region),
                None => region,
            });
        }
        self.recycle_surface(elem, elem_dirty);
    }

    /// Coverage plane for `mask`, honoring its page-space offset. The base
    /// raster (at build position) is cached per mask identity; offset uses are
    /// derived by shifting it — exact wherever the base raster painted, and
    /// vacated strips take the mask's unpainted value, which is what a fresh
    /// raster yields outside the group too.
    fn soft_mask_plane(&mut self, mask: &SoftMask) -> Option<Arc<[u8]>> {
        if self.soft_mask_depth >= self.max_blend_group_depth {
            tracing::warn!(
                limit = self.max_blend_group_depth,
                "soft-mask nesting exceeds configured blend-group limit; skipping mask"
            );
            return None;
        }
        let (w, h) = {
            let p = self.pixmap.as_ref()?;
            (p.width(), p.height())
        };
        let mask_work_bytes = w as u64 * h as u64 * 5; // RGBA scratch + 1-byte plane
        if !self.surface_room(mask_work_bytes) {
            tracing::warn!("soft-mask scratch exceeds memory budget; skipping mask");
            return None;
        }

        // Timed from here: the guards above do no real work, while everything
        // below is either a plane shift or a full mask-group sub-render.
        let t = self.tick();

        // Page-space offset → device pixels (device y grows downward).
        let dx = (mask.offset.0 * self.scale).round() as i64;
        let dy = (-mask.offset.1 * self.scale_y).round() as i64;

        let key = SoftMaskPlaneKey::new(mask);
        if let Some((_, _, base)) = self.soft_mask_planes.get(&key) {
            // Clone out of the map before any `&mut self` accounting call.
            let base = Arc::clone(base);
            if dx == 0 && dy == 0 {
                self.tock(t, |s| &mut s.soft_mask_ns);
                return Some(base);
            }
            let shifted = Arc::from(shift_plane(&base, w, h, dx, dy, unpainted_value(mask)));
            self.tock(t, |s| &mut s.soft_mask_ns);
            return Some(shifted);
        }

        let base: Arc<[u8]> = match self.rasterize_soft_mask(mask) {
            Some(plane) => {
                self.count(1, |s| &mut s.soft_mask_planes);
                Arc::from(plane)
            }
            None => {
                self.tock(t, |s| &mut s.soft_mask_ns);
                return None;
            }
        };
        let base_bytes = base.len() as u64;
        if base_bytes <= self.max_soft_mask_cache_bytes {
            if self.soft_mask_cache_bytes.saturating_add(base_bytes)
                > self.max_soft_mask_cache_bytes
            {
                self.soft_mask_planes.clear();
                self.soft_mask_cache_bytes = 0;
            }
            self.soft_mask_planes.insert(
                key,
                (
                    Arc::clone(&mask.commands),
                    mask.transfer.clone(),
                    Arc::clone(&base),
                ),
            );
            self.soft_mask_cache_bytes = self.soft_mask_cache_bytes.saturating_add(base_bytes);
        }
        let plane = if dx == 0 && dy == 0 {
            base
        } else {
            Arc::from(shift_plane(&base, w, h, dx, dy, unpainted_value(mask)))
        };
        self.tock(t, |s| &mut s.soft_mask_ns);
        Some(plane)
    }

    /// Render a soft mask's group commands offscreen (same page geometry as
    /// the current target) and reduce to a per-pixel coverage plane, ignoring
    /// the offset (callers shift the result).
    fn rasterize_soft_mask(&mut self, mask: &SoftMask) -> Option<Vec<u8>> {
        // Everything up to `rendered` — the full-page scratch allocation, its
        // backdrop fill, the group's sub-render, and the blend drain — is one
        // sub-bucket. The per-pixel reduction is the other.
        let t_render = self.tick();
        let (w, h) = {
            let p = self.pixmap.as_ref()?;
            (p.width(), p.height())
        };

        let mut target = tiny_skia::Pixmap::new(w, h)?;
        match mask.kind {
            // Luminosity masks composite the group over the /BC backdrop; the
            // result stays opaque, so luminance reads are exact.
            SoftMaskKind::Luminosity => {
                let l = unit(mask.backdrop_luma);
                target.fill(tiny_skia::Color::from_rgba(l, l, l, 1.0)?);
            }
            // Alpha masks read group coverage; start fully transparent.
            SoftMaskKind::Alpha => {}
        }

        let mut sub = CpuRenderer {
            pixmap: Some(target),
            // The sub-renderer's own target starts as the mask group's backdrop
            // and is composited nowhere, so only its nested groups' regions
            // matter — and those start empty.
            dirty: None,
            scale: self.scale,
            scale_y: self.scale_y,
            rect_x0: self.rect_x0,
            rect_y0: self.rect_y0,
            rect_y1: self.rect_y1,
            dev_origin_x: self.dev_origin_x,
            dev_origin_y: self.dev_origin_y,
            font_cache: self.font_cache,
            image_cache: self.image_cache,
            max_page_pixels: self.max_page_pixels,
            max_blend_group_depth: self.max_blend_group_depth,
            max_soft_mask_cache_bytes: self.max_soft_mask_cache_bytes,
            parse_limits: self.parse_limits.clone(),
            clip_stack: Vec::new(),
            current_clip: None,
            clip_bounds: None,
            clip_pixel_spent: 0,
            clip_mask_bytes: 0,
            mask_pool: Vec::new(),
            blend_stack: Vec::new(),
            recording_non_isolated: None,
            non_isolated_depth: 0,
            alpha_only_pass: false,
            shape_only_pass: false,
            // Everything the page holds stays allocated while the mask group
            // renders, so its groups share the page's budget rather than
            // starting a fresh one per nesting level.
            blend_surface_bytes: self
                .blend_surface_bytes
                .saturating_add(self.pooled_surface_bytes)
                .saturating_add(w as u64 * h as u64 * 5),
            surface_pool: Vec::new(),
            pooled_surface_bytes: 0,
            skipped_blend_depth: 0,
            // Share the parent page's deadline so a soft-mask group cannot hang
            // past the budget the top-level render already started counting.
            render_budget: self.render_budget,
            deadline: self.deadline,
            over_budget: self.over_budget,
            soft_mask_planes: HashMap::new(),
            soft_mask_cache_bytes: 0,
            // The mask belongs to the group currently being pushed, so carry
            // both enclosing mask depth and live page-group depth into the
            // sub-renderer. Nested groups inside the mask then share one exact
            // max_blend_group_depth budget with the outer page.
            soft_mask_depth: self
                .soft_mask_depth
                .saturating_add(self.blend_stack.len())
                .saturating_add(self.non_isolated_depth)
                .saturating_add(1),
            tiling_cache: HashMap::new(),
            tiling_cache_bytes: 0,
            tiling_backdrop_cache: HashMap::new(),
            tiling_depth: self.tiling_depth,
            downscaled_images: HashMap::new(),
            glyph_outlines: HashMap::new(),
            glyph_outline_commands: 0,
            type3_glyphs: HashMap::new(),
            type3_glyph_commands: 0,
            type3_glyph_image_bytes: 0,
            // The sub-renderer's work is attributed to the *parent's*
            // `soft_mask_ns` bucket (the parent times this whole call), so
            // collecting here too would double-count it.
            stage_stats: None,
            page_start: None,
        };
        for cmd in &mask.commands.commands {
            let _ = sub.execute(cmd);
        }
        while !sub.blend_stack.is_empty() {
            sub.pop_blend_group();
        }
        let rendered = sub.pixmap.take();
        self.tock(t_render, |s| &mut s.mask_render_ns);
        let rendered = rendered?;

        let t_reduce = self.tick();
        // Only the region the mask's own group painted can hold anything but the
        // value a fresh raster carries there (`/BC` luminance for a luminosity
        // mask, transparency for an alpha one) — which is exactly what the
        // transfer LUT maps `unpainted_value` to. So the plane is filled with
        // that value and only the painted region is paid for pixel by pixel.
        // `sub.dirty` is the same conservative superset the blend-group
        // composite relies on, tracked by the sub-renderer as it drew.
        let fill = unpainted_value(mask);
        let painted = sub.dirty.filter(|r| !r.is_empty());
        let mut plane = vec![fill; (w * h) as usize];
        if let Some(region) = painted {
            let width = rendered.width() as usize;
            let pixels = rendered.pixels();
            for y in region.y0 as usize..region.y1 as usize {
                let row = y * width;
                let (x0, x1) = (region.x0 as usize, region.x1 as usize);
                let (Some(src), Some(out)) = (
                    pixels.get(row + x0..row + x1),
                    plane.get_mut(row + x0..row + x1),
                ) else {
                    continue;
                };
                for (px, out) in src.iter().zip(out.iter_mut()) {
                    *out = mask_plane_value(*px, mask);
                }
            }
        }
        self.tock(t_reduce, |s| &mut s.mask_reduce_ns);

        // Diagnostics, and a full pass — so only while collecting stats, and
        // after the reduce's timer has closed so they cannot inflate the bucket
        // they explain (their cost lands in `soft_mask_ns`, like the composite's
        // coverage scan). The leak count is the invariant this shortcut rests
        // on, and it has to be tested on the *value*, not on the raster's alpha:
        // a luminosity mask's target starts out opaque, so every pixel there has
        // non-zero alpha and an alpha-based check would report the whole page as
        // leaking. What matters is whether a pixel *outside* the region would
        // have reduced to something other than what the fill put there.
        if self.stage_stats.is_some() {
            let width = rendered.width() as usize;
            let pixels = rendered.pixels();
            let mut leak = 0u64;
            for y in 0..h as usize {
                let row = y * width;
                for x in 0..width {
                    let inside = painted.is_some_and(|r| {
                        (r.x0 as usize..r.x1 as usize).contains(&x)
                            && (r.y0 as usize..r.y1 as usize).contains(&y)
                    });
                    if inside {
                        continue;
                    }
                    if pixels
                        .get(row + x)
                        .is_some_and(|px| mask_plane_value(*px, mask) != fill)
                    {
                        leak += 1;
                    }
                }
            }
            self.count(painted.map_or(0, PaintedBounds::area), |s| {
                &mut s.mask_reduce_px
            });
            self.count(leak, |s| &mut s.mask_reduce_leak_px);
        }
        Some(plane)
    }

    fn blend_mode_to_skia(mode: BlendMode) -> tiny_skia::BlendMode {
        match mode {
            BlendMode::Normal => tiny_skia::BlendMode::SourceOver,
            BlendMode::Multiply => tiny_skia::BlendMode::Multiply,
            BlendMode::Screen => tiny_skia::BlendMode::Screen,
            BlendMode::Overlay => tiny_skia::BlendMode::Overlay,
            BlendMode::Darken => tiny_skia::BlendMode::Darken,
            BlendMode::Lighten => tiny_skia::BlendMode::Lighten,
            BlendMode::ColorDodge => tiny_skia::BlendMode::ColorDodge,
            BlendMode::ColorBurn => tiny_skia::BlendMode::ColorBurn,
            BlendMode::HardLight => tiny_skia::BlendMode::HardLight,
            BlendMode::SoftLight => tiny_skia::BlendMode::SoftLight,
            BlendMode::Difference => tiny_skia::BlendMode::Difference,
            BlendMode::Exclusion => tiny_skia::BlendMode::Exclusion,
            BlendMode::Hue => tiny_skia::BlendMode::Hue,
            BlendMode::Saturation => tiny_skia::BlendMode::Saturation,
            BlendMode::Color => tiny_skia::BlendMode::Color,
            BlendMode::Luminosity => tiny_skia::BlendMode::Luminosity,
        }
    }

    fn render_image_with_alpha(&mut self, draw: &ImageDraw, alpha_override: f32) {
        let Some(image_cache) = self.image_cache else {
            return;
        };
        self.render_image_from_cache(image_cache, draw, alpha_override, true);
    }

    fn render_inline_image(&mut self, draw: &InlineImageDraw) {
        self.render_inline_image_with_alpha(draw, draw.alpha);
    }

    fn render_inline_image_with_alpha(&mut self, draw: &InlineImageDraw, alpha_override: f32) {
        if !draw.image.premultiplied
            && draw
                .image
                .data
                .as_chunks::<4>()
                .0
                .iter()
                .any(|px| px[3] < 255)
        {
            let mut premul = draw.image.data.clone();
            Self::premultiply_rgba(&mut premul);
            self.render_rgba_image(
                draw.image.width,
                draw.image.height,
                &premul,
                &draw.transform,
                alpha_override,
                None,
            );
            return;
        }
        self.render_rgba_image(
            draw.image.width,
            draw.image.height,
            &draw.image.data,
            &draw.transform,
            alpha_override,
            None,
        );
    }

    fn premultiply_rgba(data: &mut [u8]) {
        for px in data.as_chunks_mut::<4>().0 {
            let a = px[3] as u32;
            if a == 0 {
                px[0] = 0;
                px[1] = 0;
                px[2] = 0;
            } else if a < 255 {
                for c in &mut px[..3] {
                    *c = ((*c as u32 * a + 127) / 255) as u8;
                }
            }
        }
    }

    fn render_image_from_cache(
        &mut self,
        image_cache: &ImageCache,
        draw: &ImageDraw,
        alpha_override: f32,
        cache_downscaled: bool,
    ) {
        let image = match image_cache.get(draw.image_id) {
            Some(img) => img,
            None => return,
        };
        self.render_rgba_image(
            image.width,
            image.height,
            &image.data,
            &draw.transform,
            alpha_override,
            cache_downscaled.then_some(draw.image_id),
        );
    }

    fn render_rgba_image(
        &mut self,
        width: u32,
        height: u32,
        data: &[u8],
        draw_transform: &zpdf_core::Matrix,
        alpha_override: f32,
        downscale_cache_id: Option<ImageId>,
    ) {
        let Some(expected_len) = (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(4))
        else {
            tracing::warn!(
                image_id = ?downscale_cache_id,
                "skipping overflowing image dimensions"
            );
            return;
        };
        if width == 0 || height == 0 || data.len() != expected_len {
            tracing::warn!(
                image_id = ?downscale_cache_id,
                width,
                height,
                bytes = data.len(),
                "skipping malformed decoded image"
            );
            return;
        }

        // Image work is the largest single measured CPU cost on the image-bound
        // page (three full-page bilinear upsamples, ~5.4 s), so it gets its own
        // bucket. Started after validation, so trivial rejections are not
        // counted as image work.
        let t = self.tick();
        // PDF images occupy the unit square [0,1]×[0,1] in user space, mapped by
        // the CTM. Image sample space has its origin at the TOP-left with y
        // pointing DOWN (PDF spec §8.9.5.2), so sample row 0 maps to the top
        // edge of the unit square (v = 1). The renderer then applies its fixed
        // page Y-flip — the same one used for every path and glyph — so the
        // CTM's own orientation (including a negative `d`, common in scanned
        // PDFs that store the JPEG upside down) is honored as ordinary geometry.
        //
        // Full chain for image pixel (ix, iy):
        //   ux = ix/iw,  uy = 1 - iy/ih              (sample → unit square)
        //   px = a*ux + c*uy + e                     (unit square → user space)
        //   py = b*ux + d*uy + f
        //   screen_x = (px - x0) * s                 (user space → screen px)
        //   screen_y = (y1 - py) * s                 (fixed page flip)
        //
        // Composed into a single affine in image-pixel coordinates:
        //   screen_x = (a*s/iw)*ix + (-c*s/ih)*iy + (c + e - x0)*s
        //   screen_y = (-b*s/iw)*ix + (d*s/ih)*iy + (y1 - d - f)*s
        let tm = draw_transform;
        let sx = self.scale;
        let sy = self.scale_y;
        let (a, b, c, d, e, f) = (
            tm.a as f32,
            tm.b as f32,
            tm.c as f32,
            tm.d as f32,
            tm.e as f32,
            tm.f as f32,
        );

        // Below ~0.5x per axis, bilinear sampling starts skipping source pixels
        // entirely (thin strokes in downscaled scans break up). Pre-downscale
        // with a box filter (plain average) to the target device scale first.
        // An interpreter given `with_image_downscale` at this scale has already
        // applied the same rule, so its images arrive at their target size.
        let mut downscaled: Option<(Arc<[u8]>, u32, u32)> = None;
        if let Some((tw, th)) =
            zpdf_image::box_filter_size(draw_transform, sx.max(sy), width, height)
        {
            let scaled = if let Some(image_id) = downscale_cache_id {
                let key = (image_id, tw, th);
                if let Some(cached) = self.downscaled_images.get(&key) {
                    Arc::clone(cached)
                } else {
                    let scaled: Arc<[u8]> =
                        Arc::from(zpdf_image::box_downscale_rgba(data, width, height, tw, th));
                    let retained: usize = self.downscaled_images.values().map(|d| d.len()).sum();
                    if scaled.len() <= MAX_DOWNSCALED_IMAGE_CACHE_BYTES {
                        if retained.saturating_add(scaled.len()) > MAX_DOWNSCALED_IMAGE_CACHE_BYTES
                        {
                            self.downscaled_images.clear();
                        }
                        self.downscaled_images.insert(key, Arc::clone(&scaled));
                    }
                    scaled
                }
            } else {
                Arc::from(zpdf_image::box_downscale_rgba(data, width, height, tw, th))
            };
            downscaled = Some((scaled, tw, th));
        }
        let (data, w, h) = match &downscaled {
            Some((scaled, w, h)) => (scaled.as_ref(), *w, *h),
            None => (data, width, height),
        };
        let src = match tiny_skia::PixmapRef::from_bytes(data, w, h) {
            Some(p) => p,
            None => {
                self.tock(t, |s| &mut s.image_ns);
                return;
            }
        };
        let iw = w as f32;
        let ih = h as f32;

        // tiny-skia `Transform::from_row(sx, ky, kx, sy, tx, ty)` maps
        // (x,y) → (sx*x + kx*y + tx, ky*x + sy*y + ty).
        let transform = tiny_skia::Transform::from_row(
            a * sx / iw,                                            // sx: screen_x per ix
            -b * sy / iw,                                           // ky: screen_y per ix
            -c * sx / ih,                                           // kx: screen_x per iy
            d * sy / ih,                                            // sy: screen_y per iy
            (c + e - self.rect_x0) * sx - self.dev_origin_x as f32, // tx
            (self.rect_y1 - d - f) * sy - self.dev_origin_y as f32, // ty
        );

        let paint = tiny_skia::PixmapPaint {
            opacity: if alpha_override.is_finite() {
                alpha_override.clamp(0.0, 1.0)
            } else {
                0.0
            },
            // Bilinear sampling (matches pdfium); nearest leaves blocky upscales
            // and aliased downscales.
            quality: tiny_skia::FilterQuality::Bilinear,
            ..Default::default()
        };

        let mut drawn = false;
        if let Some(pixmap) = self.pixmap.as_mut() {
            pixmap.draw_pixmap(0, 0, src, &paint, transform, self.current_clip.as_ref());
            drawn = true;
        }
        if drawn {
            // The image occupies the unit square in its own space, so its device
            // footprint is that square's transformed corners — the same thing the
            // CTM above maps the samples through, not the source rectangle.
            let footprint = tiny_skia::Rect::from_ltrb(0.0, 0.0, iw, ih)
                .and_then(|unit| unit.transform(transform));
            match footprint {
                Some(footprint) => self.mark_painted(footprint, 1.0),
                // A non-finite transform: the footprint cannot be established.
                None => self.mark_all_painted(),
            }
        }
        self.tock(t, |s| &mut s.image_ns);
        self.count(drawn as u64, |s| &mut s.images);
    }

    fn render_image(&mut self, draw: &ImageDraw) {
        self.render_image_with_alpha(draw, draw.alpha)
    }

    /// Conservative device-space bounding box of an outline [`GlyphRun`] on the
    /// active target surface, so `with_backdrop_shape_coverage` sizes its scratch
    /// buffers to the run rather than the whole page. Returns `None` for Type 3
    /// fonts (whose arbitrary glyph streams fall back to clipped/banded bounds).
    fn outline_glyph_run_scope(
        &mut self,
        run: &GlyphRun,
        font: &zpdf_font::LoadedFont,
    ) -> Option<PaintedBounds> {
        if font.is_type3() {
            return None;
        }
        let (pw, ph) = self.pixmap.as_ref().map(|p| (p.width(), p.height()))?;
        let upem = font.units_per_em as f32;
        let mut x0 = f32::INFINITY;
        let mut y0 = f32::INFINITY;
        let mut x1 = f32::NEG_INFINITY;
        let mut y1 = f32::NEG_INFINITY;
        let mut any = false;
        for glyph in &run.glyphs {
            let Some(outline) = self.glyph_outline(run.font_id, font, glyph.glyph_id) else {
                continue;
            };
            if let Some(path) = self.build_outline_transformed_path(
                &outline,
                upem,
                run.font_size,
                run.h_scale,
                &run.transform,
                glyph.x,
                glyph.y,
            ) {
                let b = path.bounds();
                x0 = x0.min(b.left());
                y0 = y0.min(b.top());
                x1 = x1.max(b.right());
                y1 = y1.max(b.bottom());
                any = true;
            }
        }
        if !any {
            return Some(PaintedBounds {
                x0: 0,
                y0: 0,
                x1: 0,
                y1: 0,
            });
        }
        let rect = tiny_skia::Rect::from_ltrb(x0, y0, x1, y1)?;
        Some(Self::drawn_extent(rect, 1.0, pw, ph))
    }

    fn render_outline_glyphs(
        &mut self,
        run: &GlyphRun,
        font: &zpdf_font::LoadedFont,
        paint: &tiny_skia::Paint<'_>,
    ) {
        let tm = &run.transform;
        let font_size = run.font_size;
        let h_scale = run.h_scale;
        let upem = font.units_per_em as f32;

        for glyph in &run.glyphs {
            // The split that §10.1/§10.2 of the performance notes could only
            // infer: outline *extraction* (font charstring/glyf work) versus
            // turning that outline into a device path and rasterizing it. Cold
            // rendering measured 20x warm without this boundary, so the two are
            // timed separately.
            let t_parse = self.tick();
            let outline = self.glyph_outline(run.font_id, font, glyph.glyph_id);
            self.tock(t_parse, |s| &mut s.outline_parse_ns);
            let Some(outline) = outline else {
                continue;
            };

            let t_raster = self.tick();
            // Transform each glyph outline point:
            // glyph_coord (font units) → text space → user space → page space → pixel space
            let skia_path = self.build_outline_transformed_path(
                &outline, upem, font_size, h_scale, tm, glyph.x, glyph.y,
            );
            if let Some(path) = skia_path {
                let painted = if let Some(ref mut pixmap) = self.pixmap {
                    pixmap.fill_path(
                        &path,
                        paint,
                        tiny_skia::FillRule::Winding,
                        tiny_skia::Transform::identity(),
                        self.current_clip.as_ref(),
                    );
                    true
                } else {
                    false
                };
                if painted {
                    // Marked from the *transformed* outline's own bounds rather
                    // than an em-box estimate: advance widths, italic overshoot
                    // and a rotated text matrix all move the ink off the box.
                    self.mark_painted(path.bounds(), 1.0);
                }
            }
            self.tock(t_raster, |s| &mut s.glyph_raster_ns);
        }
    }

    /// The outline of `glyph_id` in `font` (the font cache's `font_id`),
    /// extracted from the font program once per page.
    fn glyph_outline(
        &mut self,
        font_id: FontId,
        font: &zpdf_font::LoadedFont,
        glyph_id: u16,
    ) -> Option<Arc<GlyphOutline>> {
        if let Some(outline) = self.glyph_outlines.get(&(font_id, glyph_id)) {
            return outline.clone();
        }
        let outline = font.glyph_outline(glyph_id).map(Arc::new);
        self.count(outline.is_some() as u64, |s| &mut s.glyph_outlines_parsed);
        let commands = outline.as_ref().map_or(1, |o| o.commands.len().max(1));
        let total = self.glyph_outline_commands.saturating_add(commands);
        if total <= MAX_CACHED_OUTLINE_COMMANDS {
            self.glyph_outline_commands = total;
            self.glyph_outlines
                .insert((font_id, glyph_id), outline.clone());
        }
        outline
    }

    #[allow(clippy::too_many_arguments)]
    fn build_outline_transformed_path(
        &self,
        outline: &GlyphOutline,
        upem: f32,
        font_size: f32,
        h_scale: f32,
        tm: &zpdf_core::Matrix,
        glyph_x_offset: f32,
        glyph_y_offset: f32,
    ) -> Option<tiny_skia::Path> {
        let mut pb = tiny_skia::PathBuilder::new();
        let off = (glyph_x_offset, glyph_y_offset);

        for cmd in &outline.commands {
            match *cmd {
                OutlineCommand::MoveTo(x, y) => {
                    let (px, py) = self.outline_to_pixel(x, y, upem, font_size, h_scale, tm, off);
                    pb.move_to(px, py);
                }
                OutlineCommand::LineTo(x, y) => {
                    let (px, py) = self.outline_to_pixel(x, y, upem, font_size, h_scale, tm, off);
                    pb.line_to(px, py);
                }
                OutlineCommand::QuadTo(x1, y1, x, y) => {
                    let (px1, py1) =
                        self.outline_to_pixel(x1, y1, upem, font_size, h_scale, tm, off);
                    let (px, py) = self.outline_to_pixel(x, y, upem, font_size, h_scale, tm, off);
                    pb.quad_to(px1, py1, px, py);
                }
                OutlineCommand::CurveTo(x1, y1, x2, y2, x, y) => {
                    let (px1, py1) =
                        self.outline_to_pixel(x1, y1, upem, font_size, h_scale, tm, off);
                    let (px2, py2) =
                        self.outline_to_pixel(x2, y2, upem, font_size, h_scale, tm, off);
                    let (px, py) = self.outline_to_pixel(x, y, upem, font_size, h_scale, tm, off);
                    pb.cubic_to(px1, py1, px2, py2, px, py);
                }
                OutlineCommand::Close => pb.close(),
            }
        }
        self.finish_within_limits(pb)
    }

    #[allow(clippy::too_many_arguments)]
    fn outline_to_pixel(
        &self,
        gx: f64,
        gy: f64,
        upem: f32,
        font_size: f32,
        h_scale: f32,
        tm: &zpdf_core::Matrix,
        glyph_offset: (f32, f32),
    ) -> (f32, f32) {
        // font units → text space. The shape x carries the horizontal-scaling
        // factor (Th = Tz/100); the offset already includes it (accumulated
        // advance), so Th multiplies only the outline term.
        let tx = (gx as f32 / upem * font_size * h_scale + glyph_offset.0) as f64;
        let ty = (gy as f32 / upem * font_size + glyph_offset.1) as f64;

        // text space → page space via combined CTM*Tm*rise
        let page_x = tm.a * tx + tm.c * ty + tm.e;
        let page_y = tm.b * tx + tm.d * ty + tm.f;

        // page space → pixel space. The CTM's own orientation (including a
        // negative `d`) is honored as ordinary geometry, then the one fixed page
        // Y-flip is applied — the same affine used for paths and images.
        let px = (page_x as f32 - self.rect_x0) * self.scale - self.dev_origin_x as f32;
        let py = (self.rect_y1 - page_y as f32) * self.scale_y - self.dev_origin_y as f32;
        (px, py)
    }

    /// True when a Type 3 CharProc command paints an explicit (`d0`) color or
    /// RGB image rather than inheriting the enclosing text paint.
    fn is_type3_explicit_colored(cmd: &RenderCommand) -> bool {
        matches!(cmd, RenderCommand::DrawImage(d) if !d.is_image_mask)
            || matches!(
                cmd,
                RenderCommand::FillPath {
                    paint: Paint::Solid(_),
                    ..
                } | RenderCommand::StrokePath {
                    paint: Paint::Solid(_),
                    ..
                }
            )
    }

    fn type3_glyph_to_page(
        tm: &zpdf_core::Matrix,
        glyph: &PositionedGlyph,
        font_size: f32,
        h_scale: f32,
        font_matrix: &[f64; 6],
    ) -> zpdf_core::Matrix {
        tm.concat(&zpdf_core::Matrix::translate(
            glyph.x as f64,
            glyph.y as f64,
        ))
        .concat(&zpdf_core::Matrix::scale(
            font_size as f64 * h_scale as f64,
            font_size as f64,
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

    fn build_raw_skia_path(path: &Path) -> Option<tiny_skia::Path> {
        let mut pb = tiny_skia::PathBuilder::new();
        for elem in path {
            match elem {
                PathElement::MoveTo(p) => {
                    if !p.x.is_finite() || !p.y.is_finite() {
                        return None;
                    }
                    pb.move_to(p.x as f32, p.y as f32);
                }
                PathElement::LineTo(p) => {
                    if !p.x.is_finite() || !p.y.is_finite() {
                        return None;
                    }
                    pb.line_to(p.x as f32, p.y as f32);
                }
                PathElement::CurveTo(c1, c2, end) => {
                    if ![c1.x, c1.y, c2.x, c2.y, end.x, end.y]
                        .iter()
                        .all(|v| v.is_finite())
                    {
                        return None;
                    }
                    pb.cubic_to(
                        c1.x as f32,
                        c1.y as f32,
                        c2.x as f32,
                        c2.y as f32,
                        end.x as f32,
                        end.y as f32,
                    );
                }
                PathElement::Close => pb.close(),
            }
        }
        pb.finish()
    }

    fn max_linear_scale_2x2(a: f64, b: f64, c: f64, d: f64) -> f64 {
        let sum_sq = a * a + b * b + c * c + d * d;
        let det = a * d - b * c;
        let disc = (sum_sq * sum_sq - 4.0 * det * det).max(0.0).sqrt();
        (0.5 * (sum_sq + disc)).sqrt()
    }

    fn type3_glyph_to_device(
        &self,
        glyph_to_page: &zpdf_core::Matrix,
    ) -> Option<tiny_skia::Transform> {
        let sx = self.scale as f64;
        let sy = self.scale_y as f64;
        let a = glyph_to_page.a * sx;
        let ky = -glyph_to_page.b * sy;
        let kx = glyph_to_page.c * sx;
        let d = -glyph_to_page.d * sy;
        let tx = (glyph_to_page.e - self.rect_x0 as f64) * sx - self.dev_origin_x as f64;
        let ty = (self.rect_y1 as f64 - glyph_to_page.f) * sy - self.dev_origin_y as f64;
        if ![a, ky, kx, d, tx, ty].iter().all(|v| v.is_finite()) {
            return None;
        }
        Some(tiny_skia::Transform::from_row(
            a as f32, ky as f32, kx as f32, d as f32, tx as f32, ty as f32,
        ))
    }

    /// Expand a Type 3 glyph stroke in glyph space and map its filled outline
    /// through `glyph_to_page` + device scaling so nonuniform text transforms
    /// preserve directional stroke widths, elliptical caps/joins, and dashes.
    /// Returns `(device_path, true)` for positive-width filled stroke outlines
    /// or `(device_centerline, false)` for 0-width PDF hairlines.
    fn build_type3_stroke_geometry(
        &self,
        path: &Path,
        style: &StrokeStyle,
        glyph_to_page: &zpdf_core::Matrix,
    ) -> Option<(tiny_skia::Path, bool)> {
        let dev_xf = self.type3_glyph_to_device(glyph_to_page)?;
        let max_scale = Self::max_linear_scale_2x2(
            dev_xf.sx as f64,
            dev_xf.ky as f64,
            dev_xf.kx as f64,
            dev_xf.sy as f64,
        ) as f32;
        if !max_scale.is_finite() || max_scale <= 1e-9 {
            return None;
        }
        let unscaled = Self::build_raw_skia_path(path)?;
        let res_scale = max_scale.clamp(0.01, 1024.0);
        if style.width.is_finite() && style.width > 0.0 {
            let glyph_stroke = Self::skia_stroke(style, 1.0, 0.5 / max_scale);
            let dashed;
            let source = if let Some(ref dash) = glyph_stroke.dash {
                dashed = unscaled.dash(dash, res_scale)?;
                &dashed
            } else {
                &unscaled
            };
            let stroked = source.stroke(&glyph_stroke, res_scale)?;
            let device = stroked.transform(dev_xf)?;
            self.check_path_limits(device).map(|p| (p, true))
        } else {
            let source = if let Some(dash) = Self::stroke_dash(style, 1.0) {
                unscaled.dash(&dash, res_scale)?
            } else {
                unscaled
            };
            let device = source.transform(dev_xf)?;
            self.check_path_limits(device).map(|p| (p, false))
        }
    }

    fn push_type3_clip_stroke(
        &mut self,
        path: &Path,
        style: &StrokeStyle,
        glyph_to_page: &zpdf_core::Matrix,
    ) {
        let page_max_scale = Self::max_linear_scale_2x2(
            glyph_to_page.a,
            glyph_to_page.b,
            glyph_to_page.c,
            glyph_to_page.d,
        ) as f32;
        let dev_max_scale = page_max_scale * self.scale.max(self.scale_y);
        if style.width.is_finite()
            && style.width > 0.0
            && dev_max_scale.is_finite()
            && dev_max_scale > 1e-9
        {
            if let Some(unscaled) = Self::build_raw_skia_path(path) {
                let res_scale = dev_max_scale.clamp(0.01, 1024.0);
                let glyph_stroke = Self::skia_stroke(style, 1.0, 0.5 / dev_max_scale);
                let dashed = glyph_stroke
                    .dash
                    .as_ref()
                    .and_then(|d| unscaled.dash(d, res_scale));
                let source = match (&glyph_stroke.dash, &dashed) {
                    (Some(_), Some(d)) => Some(d),
                    (Some(_), None) => None,
                    (None, _) => Some(&unscaled),
                };
                if let Some(stroked) = source.and_then(|s| s.stroke(&glyph_stroke, res_scale)) {
                    let page_outline = skia_path_to_dl_transformed(&stroked, glyph_to_page);
                    self.push_clip(&page_outline, &FillRule::NonZero);
                    return;
                }
                self.push_clip(&Path::new(), &FillRule::NonZero);
                return;
            }
        }
        let page_path = Self::type3_path_to_page(path, glyph_to_page);
        let scaled = Self::scale_type3_stroke_style(style, page_max_scale);
        self.push_clip_stroke(&page_path, &scaled);
    }

    fn scale_type3_stroke_style(style: &StrokeStyle, scale: f32) -> StrokeStyle {
        let s = if scale.is_finite() && scale >= 0.0 {
            scale
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

    /// Render a Type 3 glyph run with a backdrop-dependent tiling pattern,
    /// keeping explicit (`d0`) colored vectors and non-mask images in their own
    /// colors while compositing inherited fills, strokes, and `/ImageMask true`
    /// stencils through `tiling`, honoring glyph-local clips in painter's order.
    fn render_type3_backdrop_tiling(
        &mut self,
        run: &GlyphRun,
        font: &zpdf_font::LoadedFont,
        tiling: &TilingPattern,
        steps: &[TilingBlendStep],
        opacity: f32,
    ) {
        let mut has_explicit_colored = false;
        for glyph in &run.glyphs {
            if let Some(program) = self.type3_glyph(run.font_id, font, glyph.glyph_id) {
                if program
                    .list
                    .commands
                    .iter()
                    .any(Self::is_type3_explicit_colored)
                {
                    has_explicit_colored = true;
                    break;
                }
            }
        }
        let white = Self::color_to_paint(&Color::white(), 1.0);
        if !has_explicit_colored {
            self.with_backdrop_shape_coverage(
                opacity,
                None,
                |renderer, _band_offset| {
                    let t = renderer.tick();
                    renderer.render_type3_glyphs(run, font, &white, 1.0);
                    renderer.tock(t, |s| &mut s.glyph_raster_ns);
                },
                |renderer, region| renderer.paint_tiling_plan_in_region(tiling, steps, region),
            );
            return;
        }
        for glyph in &run.glyphs {
            if self
                .deadline
                .is_some_and(|deadline| zpdf_core::time::Instant::now() >= deadline)
            {
                self.over_budget = true;
                return;
            }
            let Some(program) = self.type3_glyph(run.font_id, font, glyph.glyph_id) else {
                continue;
            };
            let cmds = &program.list.commands;
            if !cmds.iter().any(Self::is_type3_explicit_colored) {
                self.with_backdrop_shape_coverage(
                    opacity,
                    None,
                    |renderer, _band_offset| {
                        let t = renderer.tick();
                        renderer
                            .render_type3_glyph_commands(glyph, &program, cmds, run, &white, 1.0);
                        renderer.tock(t, |s| &mut s.glyph_raster_ns);
                    },
                    |renderer, region| renderer.paint_tiling_plan_in_region(tiling, steps, region),
                );
                continue;
            }
            let glyph_to_page = Self::type3_glyph_to_page(
                &run.transform,
                glyph,
                run.font_size,
                run.h_scale,
                &program.font_matrix,
            );
            let resolve_glyph_img = |id: ImageId| program.inline_images.get(&id).cloned();
            let device_scale = self.scale.max(self.scale_y);
            let transform_mask_stroke =
                |path: &Path, style: &StrokeStyle, m: &zpdf_core::Matrix| {
                    transform_stroke_to_page(path, style, m, device_scale)
                };
            let mut seg_start = 0usize;
            let mut glyph_clip_depth = 0usize;
            let mut glyph_blend_depth = 0usize;
            let flush_inherited = |r: &mut Self, start: usize, end: usize| {
                if start >= end {
                    return;
                }
                r.with_backdrop_shape_coverage(
                    opacity,
                    None,
                    |renderer, _band_offset| {
                        let t = renderer.tick();
                        renderer.render_type3_glyph_commands(
                            glyph,
                            &program,
                            &cmds[start..end],
                            run,
                            &white,
                            1.0,
                        );
                        renderer.tock(t, |s| &mut s.glyph_raster_ns);
                    },
                    |renderer, region| renderer.paint_tiling_plan_in_region(tiling, steps, region),
                );
            };
            for (i, cmd) in cmds.iter().enumerate() {
                match cmd {
                    RenderCommand::PushClip { path, rule } => {
                        flush_inherited(self, seg_start, i);
                        let page_path = Self::type3_path_to_page(path, &glyph_to_page);
                        self.push_clip(&page_path, rule);
                        glyph_clip_depth += 1;
                        seg_start = i + 1;
                    }
                    RenderCommand::PushClipStroke { path, style } => {
                        flush_inherited(self, seg_start, i);
                        self.push_type3_clip_stroke(path, style, &glyph_to_page);
                        glyph_clip_depth += 1;
                        seg_start = i + 1;
                    }
                    RenderCommand::PopClip => {
                        flush_inherited(self, seg_start, i);
                        if glyph_clip_depth > 0 {
                            self.pop_clip();
                            glyph_clip_depth -= 1;
                        }
                        seg_start = i + 1;
                    }
                    RenderCommand::PushBlendGroup {
                        blend_mode,
                        isolated,
                        knockout,
                        alpha,
                        mask,
                        ..
                    } => {
                        flush_inherited(self, seg_start, i);
                        let page_mask = mask.as_ref().map(|m| {
                            m.transform_with_images(
                                &glyph_to_page,
                                &resolve_glyph_img,
                                &transform_mask_stroke,
                            )
                        });
                        self.push_blend_group(
                            *blend_mode,
                            *isolated,
                            *knockout,
                            *alpha,
                            page_mask.as_ref(),
                        );
                        glyph_blend_depth += 1;
                        seg_start = i + 1;
                    }
                    RenderCommand::PopBlendGroup => {
                        flush_inherited(self, seg_start, i);
                        if glyph_blend_depth > 0 {
                            self.pop_blend_group();
                            glyph_blend_depth -= 1;
                        }
                        seg_start = i + 1;
                    }
                    _ if Self::is_type3_explicit_colored(cmd) => {
                        flush_inherited(self, seg_start, i);
                        let t = self.tick();
                        self.render_type3_glyph_commands(
                            glyph,
                            &program,
                            &cmds[i..=i],
                            run,
                            &white,
                            opacity,
                        );
                        self.tock(t, |s| &mut s.glyph_raster_ns);
                        seg_start = i + 1;
                    }
                    _ => {}
                }
            }
            flush_inherited(self, seg_start, cmds.len());
            while glyph_blend_depth > 0 {
                self.pop_blend_group();
                glyph_blend_depth -= 1;
            }
            while glyph_clip_depth > 0 {
                self.pop_clip();
                glyph_clip_depth -= 1;
            }
        }
    }

    fn render_type3_glyph_commands(
        &mut self,
        glyph: &PositionedGlyph,
        program: &Type3Glyph,
        commands: &[RenderCommand],
        run: &GlyphRun,
        paint: &tiny_skia::Paint<'_>,
        alpha_override: f32,
    ) {
        let tm = &run.transform;
        let font_size = run.font_size;
        let h_scale = run.h_scale;
        let font_matrix = program.font_matrix;
        let glyph_images = &program.images;
        let resolve_glyph_img = |id: ImageId| program.inline_images.get(&id).cloned();
        let device_scale = self.scale.max(self.scale_y);
        let transform_mask_stroke = |path: &Path, style: &StrokeStyle, m: &zpdf_core::Matrix| {
            transform_stroke_to_page(path, style, m, device_scale)
        };
        let glyph_to_page = Self::type3_glyph_to_page(tm, glyph, font_size, h_scale, &font_matrix);
        let mut glyph_clip_depth = 0usize;
        let mut glyph_blend_depth = 0usize;

        for cmd in commands {
            match cmd {
                RenderCommand::PushClip { path, rule } => {
                    let page_path = Self::type3_path_to_page(path, &glyph_to_page);
                    self.push_clip(&page_path, rule);
                    glyph_clip_depth += 1;
                }
                RenderCommand::PushClipStroke { path, style } => {
                    self.push_type3_clip_stroke(path, style, &glyph_to_page);
                    glyph_clip_depth += 1;
                }
                RenderCommand::PopClip => {
                    if glyph_clip_depth > 0 {
                        self.pop_clip();
                        glyph_clip_depth -= 1;
                    }
                }
                RenderCommand::PushBlendGroup {
                    blend_mode,
                    isolated,
                    knockout,
                    alpha,
                    mask,
                    ..
                } => {
                    let page_mask = mask.as_ref().map(|m| {
                        m.transform_with_images(
                            &glyph_to_page,
                            &resolve_glyph_img,
                            &transform_mask_stroke,
                        )
                    });
                    self.push_blend_group(
                        *blend_mode,
                        *isolated,
                        *knockout,
                        *alpha,
                        page_mask.as_ref(),
                    );
                    glyph_blend_depth += 1;
                }
                RenderCommand::PopBlendGroup => {
                    if glyph_blend_depth > 0 {
                        self.pop_blend_group();
                        glyph_blend_depth -= 1;
                    }
                }
                RenderCommand::FillPath {
                    path,
                    rule,
                    paint: cmd_paint,
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
                    let explicit_paint;
                    let draw_paint = match cmd_paint {
                        Paint::Solid(c) => {
                            explicit_paint = Self::color_to_paint(c, alpha_override * cmd_alpha);
                            &explicit_paint
                        }
                        Paint::Inherited => {
                            if cmd_alpha < 1.0 {
                                if let tiny_skia::Shader::SolidColor(sc) = paint.shader {
                                    let mut p = paint.clone();
                                    let mut c = sc;
                                    c.set_alpha(c.alpha() * cmd_alpha);
                                    p.shader = tiny_skia::Shader::SolidColor(c);
                                    explicit_paint = p;
                                    &explicit_paint
                                } else {
                                    paint
                                }
                            } else {
                                paint
                            }
                        }
                        _ => continue,
                    };
                    if let Some(skia_path) = self.build_type3_transformed_path(
                        path,
                        &font_matrix,
                        font_size,
                        h_scale,
                        tm,
                        (glyph.x, glyph.y),
                    ) {
                        if let Some(ref mut pixmap) = self.pixmap {
                            pixmap.fill_path(
                                &skia_path,
                                draw_paint,
                                Self::fill_rule_to_skia(rule),
                                tiny_skia::Transform::identity(),
                                self.current_clip.as_ref(),
                            );
                        }
                        self.mark_painted(skia_path.bounds(), 1.0);
                    }
                }
                RenderCommand::StrokePath {
                    path,
                    style,
                    paint: cmd_paint,
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
                    let explicit_paint;
                    let draw_paint = match cmd_paint {
                        Paint::Solid(c) => {
                            explicit_paint = Self::color_to_paint(c, alpha_override * cmd_alpha);
                            &explicit_paint
                        }
                        Paint::Inherited => {
                            if cmd_alpha < 1.0 {
                                if let tiny_skia::Shader::SolidColor(sc) = paint.shader {
                                    let mut p = paint.clone();
                                    let mut c = sc;
                                    c.set_alpha(c.alpha() * cmd_alpha);
                                    p.shader = tiny_skia::Shader::SolidColor(c);
                                    explicit_paint = p;
                                    &explicit_paint
                                } else {
                                    paint
                                }
                            } else {
                                paint
                            }
                        }
                        _ => continue,
                    };
                    if let Some((skia_path, is_filled_outline)) =
                        self.build_type3_stroke_geometry(path, style, &glyph_to_page)
                    {
                        if is_filled_outline {
                            if let Some(ref mut pixmap) = self.pixmap {
                                pixmap.fill_path(
                                    &skia_path,
                                    draw_paint,
                                    tiny_skia::FillRule::Winding,
                                    tiny_skia::Transform::identity(),
                                    self.current_clip.as_ref(),
                                );
                            }
                            self.mark_painted(skia_path.bounds(), 1.0);
                        } else {
                            let mut hairline = Self::skia_stroke(style, 0.0, 0.5);
                            hairline.dash = None;
                            if let Some(ref mut pixmap) = self.pixmap {
                                pixmap.stroke_path(
                                    &skia_path,
                                    draw_paint,
                                    &hairline,
                                    tiny_skia::Transform::identity(),
                                    self.current_clip.as_ref(),
                                );
                            }
                            let grow = stroke_overshoot(hairline.width, hairline.miter_limit);
                            self.mark_painted(skia_path.bounds(), grow);
                        }
                    }
                }
                RenderCommand::DrawImage(draw) => {
                    let img_to_page = glyph_to_page.concat(&draw.transform);
                    let draw = ImageDraw {
                        image_id: draw.image_id,
                        transform: img_to_page,
                        alpha: draw.alpha,
                        is_image_mask: draw.is_image_mask,
                    };
                    self.render_type3_image(glyph_images, &draw, paint, alpha_override);
                }
                _ => {}
            }
        }
        while glyph_blend_depth > 0 {
            self.pop_blend_group();
            glyph_blend_depth -= 1;
        }
        while glyph_clip_depth > 0 {
            self.pop_clip();
            glyph_clip_depth -= 1;
        }
    }

    fn render_type3_glyphs(
        &mut self,
        run: &GlyphRun,
        font: &zpdf_font::LoadedFont,
        paint: &tiny_skia::Paint<'_>,
        alpha_override: f32,
    ) {
        for glyph in &run.glyphs {
            // One run can hold many glyphs, each a whole content program, so
            // the page deadline is checked per glyph, not just per command.
            if self
                .deadline
                .is_some_and(|deadline| zpdf_core::time::Instant::now() >= deadline)
            {
                self.over_budget = true;
                tracing::warn!("render exceeded time budget inside a Type 3 run; truncating page");
                return;
            }
            let Some(program) = self.type3_glyph(run.font_id, font, glyph.glyph_id) else {
                continue;
            };
            self.render_type3_glyph_commands(
                glyph,
                &program,
                &program.list.commands,
                run,
                paint,
                alpha_override,
            );
        }
    }

    /// The interpreted program of Type 3 glyph `glyph_id` of `font` (the font
    /// cache's `font_id`), interpreted once per page.
    fn type3_glyph(
        &mut self,
        font_id: FontId,
        font: &zpdf_font::LoadedFont,
        glyph_id: u16,
    ) -> Option<Arc<Type3Glyph>> {
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
                    .with_images(&mut images)
                    .with_limits(&self.parse_limits);
                if let Some(deadline) = self.deadline {
                    interp = interp.with_deadline(deadline);
                }
                let list = interp.interpret(stream);
                let inline_images = collect_type3_mask_inline_images(&list, &images);
                Arc::new(Type3Glyph {
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

    /// Type 3 glyphs in Beamer PDFs commonly use `/ImageMask true` inline
    /// images as glyph stencils. The nested interpreter decodes those images
    /// with its default black fill colour, so recolour their opaque samples
    /// with the text paint (or mask a tiling pattern shader through the
    /// stencil's alpha coverage) before compositing onto the page.
    fn render_type3_image(
        &mut self,
        image_cache: &ImageCache,
        draw: &ImageDraw,
        paint: &tiny_skia::Paint<'_>,
        alpha: f32,
    ) {
        let Some(source) = image_cache.get(draw.image_id) else {
            return;
        };
        let eff_alpha = alpha * draw.alpha;
        if !draw.is_image_mask {
            self.render_image_from_cache(image_cache, draw, eff_alpha, false);
            return;
        }

        match &paint.shader {
            tiny_skia::Shader::SolidColor(tint) => {
                let mut tinted = source.clone();
                let (pixels, _) = tinted.data.as_chunks_mut::<4>();
                for pixel in pixels {
                    let pixel_alpha = pixel[3];
                    if pixel_alpha == 0 {
                        continue;
                    }
                    // ImageCache pixels are premultiplied RGBA. Type 3 masks currently
                    // have binary alpha, but multiplying here also keeps this correct
                    // for anti-aliased mask decoders.
                    pixel[0] = (tint.red() * pixel_alpha as f32).round() as u8;
                    pixel[1] = (tint.green() * pixel_alpha as f32).round() as u8;
                    pixel[2] = (tint.blue() * pixel_alpha as f32).round() as u8;
                }

                let mut tinted_cache = ImageCache::new();
                let image_id = tinted_cache.insert(tinted);
                let draw = ImageDraw {
                    image_id,
                    transform: draw.transform,
                    alpha: draw.alpha,
                    is_image_mask: true,
                };
                self.render_image_from_cache(&tinted_cache, &draw, eff_alpha, false);
            }
            tiny_skia::Shader::Pattern(_) => {
                let Some(expected_len) = (source.width as usize)
                    .checked_mul(source.height as usize)
                    .and_then(|n| n.checked_mul(4))
                else {
                    return;
                };
                if source.width == 0 || source.height == 0 || source.data.len() != expected_len {
                    return;
                }
                let Some((pw, ph)) = self.pixmap.as_ref().map(|p| (p.width(), p.height())) else {
                    return;
                };
                let Some(src_ref) =
                    tiny_skia::PixmapRef::from_bytes(&source.data, source.width, source.height)
                else {
                    return;
                };

                let tm = &draw.transform;
                let sx = self.scale;
                let sy = self.scale_y;
                let iw = source.width as f32;
                let ih = source.height as f32;
                let (a, b, c, d, e, f) = (
                    tm.a as f32,
                    tm.b as f32,
                    tm.c as f32,
                    tm.d as f32,
                    tm.e as f32,
                    tm.f as f32,
                );
                let transform = tiny_skia::Transform::from_row(
                    a * sx / iw,
                    -b * sy / iw,
                    -c * sx / ih,
                    d * sy / ih,
                    (c + e - self.rect_x0) * sx - self.dev_origin_x as f32,
                    (self.rect_y1 - d - f) * sy - self.dev_origin_y as f32,
                );
                let Some(footprint) = tiny_skia::Rect::from_ltrb(0.0, 0.0, iw, ih)
                    .and_then(|unit_r| unit_r.transform(transform))
                else {
                    return;
                };
                let rx0 = (footprint.left().floor().max(0.0) as u32).min(pw);
                let ry0 = (footprint.top().floor().max(0.0) as u32).min(ph);
                let rx1 = (footprint.right().ceil().max(0.0) as u32).min(pw);
                let ry1 = (footprint.bottom().ceil().max(0.0) as u32).min(ph);
                if rx1 <= rx0 || ry1 <= ry0 {
                    return;
                }
                let stencil_alpha = if draw.alpha.is_finite() {
                    draw.alpha.clamp(0.0, 1.0)
                } else {
                    0.0
                };
                if stencil_alpha <= 0.0 {
                    return;
                }
                self.render_type3_pattern_stencil_budgeted(
                    src_ref,
                    transform,
                    footprint,
                    rx0,
                    ry0,
                    rx1,
                    ry1,
                    stencil_alpha,
                    paint,
                    MAX_COVERAGE_BAND_BYTES,
                );
            }
            _ => {}
        }
    }

    /// Rasterize a Type 3 bitmap stencil masked through a `Shader::Pattern`
    /// in horizontal/vertical bands sized to fit the remaining live surface
    /// budget (`MAX_BLEND_SURFACE_BYTES - self.blend_surface_bytes`) so a
    /// page-sized stencil on a large raster (e.g. 7,000 × 7,000) never exceeds
    /// the 512 MiB ceiling.
    #[allow(clippy::too_many_arguments)]
    fn render_type3_pattern_stencil_budgeted(
        &mut self,
        src_ref: tiny_skia::PixmapRef<'_>,
        transform: tiny_skia::Transform,
        footprint: tiny_skia::Rect,
        rx0: u32,
        ry0: u32,
        rx1: u32,
        ry1: u32,
        stencil_alpha: f32,
        paint: &tiny_skia::Paint<'_>,
        max_band_bytes: u64,
    ) {
        let Some((pw, _ph)) = self.pixmap.as_ref().map(|p| (p.width(), p.height())) else {
            return;
        };
        let rw = rx1 - rx0;
        let rh = ry1 - ry0;
        if rw == 0 || rh == 0 {
            return;
        }
        let max_live_avail = MAX_BLEND_SURFACE_BYTES.saturating_sub(self.blend_surface_bytes);
        if max_live_avail < 8 {
            tracing::warn!("type3 stencil scratch exceeds live surface memory budget");
            return;
        }
        let band_budget = max_live_avail.min(max_band_bytes.max(8));
        let max_band_pixels = (band_budget / 8).max(1);
        let (band_w, band_h) = if (rw as u64) <= max_band_pixels {
            (rw, (max_band_pixels / rw as u64).clamp(1, rh as u64) as u32)
        } else {
            (max_band_pixels.min(rw as u64) as u32, 1)
        };

        let mut src_paint = paint.clone();
        src_paint.blend_mode = tiny_skia::BlendMode::Source;
        src_paint.anti_alias = false;
        let pw_usize = pw as usize;

        let mut mask_pix: Option<tiny_skia::Pixmap> = None;
        let mut pat_pix: Option<tiny_skia::Pixmap> = None;
        let mut live_bytes: u64 = 0;
        let mut any_painted = false;

        'bands: for by0 in (ry0..ry1).step_by(band_h as usize) {
            let by1 = (by0 + band_h).min(ry1);
            let cur_h = by1 - by0;
            for bx0 in (rx0..rx1).step_by(band_w as usize) {
                if self.over_budget {
                    break 'bands;
                }
                let bx1 = (bx0 + band_w).min(rx1);
                let cur_w = bx1 - bx0;

                let dims_match = mask_pix
                    .as_ref()
                    .is_some_and(|p| p.width() == cur_w && p.height() == cur_h)
                    && pat_pix
                        .as_ref()
                        .is_some_and(|p| p.width() == cur_w && p.height() == cur_h);
                if dims_match {
                    if let Some(ref mut mp) = mask_pix {
                        mp.data_mut().fill(0);
                    }
                    if let Some(ref mut pp) = pat_pix {
                        pp.data_mut().fill(0);
                    }
                } else {
                    mask_pix = None;
                    pat_pix = None;
                    self.blend_surface_bytes = self.blend_surface_bytes.saturating_sub(live_bytes);
                    live_bytes = 0;
                    let pair_bytes = cur_w as u64 * cur_h as u64 * 8;
                    if !self.surface_room(pair_bytes) {
                        tracing::warn!("type3 stencil band exceeds live surface memory budget");
                        break 'bands;
                    }
                    let Some(mp) = tiny_skia::Pixmap::new(cur_w, cur_h) else {
                        break 'bands;
                    };
                    let Some(pp) = tiny_skia::Pixmap::new(cur_w, cur_h) else {
                        break 'bands;
                    };
                    self.blend_surface_bytes = self.blend_surface_bytes.saturating_add(pair_bytes);
                    live_bytes = pair_bytes;
                    mask_pix = Some(mp);
                    pat_pix = Some(pp);
                }

                let (Some(ref mut mp), Some(ref mut pp)) = (mask_pix.as_mut(), pat_pix.as_mut())
                else {
                    break 'bands;
                };

                // Rasterize stencil coverage into `mp` shifted by (-bx0, -by0).
                // Note: `paint.shader` (Pattern) already carries `alpha_override`,
                // so only `draw.alpha` modulates the stencil here.
                let shift = tiny_skia::Transform::from_translate(-(bx0 as f32), -(by0 as f32));
                mp.draw_pixmap(
                    0,
                    0,
                    src_ref,
                    &tiny_skia::PixmapPaint {
                        opacity: stencil_alpha,
                        quality: tiny_skia::FilterQuality::Bilinear,
                        ..Default::default()
                    },
                    transform.post_concat(shift),
                    None,
                );

                // Sample the pattern shader across [bx0..bx1]×[by0..by1].
                if let Some(fill_rect) =
                    tiny_skia::Rect::from_ltrb(bx0 as f32, by0 as f32, bx1 as f32, by1 as f32)
                {
                    pp.fill_rect(fill_rect, &src_paint, shift, None);
                }

                // Modulate premultiplied pattern samples by stencil alpha (and active clip).
                let clip_ref = self.current_clip.as_ref();
                let clip_data = clip_ref.map(|m| m.data());
                let cw_usize = cur_w as usize;
                let (pat_px, _) = pp.data_mut().as_chunks_mut::<4>();
                let (mask_px, _) = mp.data().as_chunks::<4>();
                for y in 0..cur_h as usize {
                    let row_off = y * cw_usize;
                    let clip_row = (by0 as usize + y) * pw_usize + bx0 as usize;
                    for x in 0..cw_usize {
                        let i = row_off + x;
                        let mut cov = mask_px[i][3];
                        if let Some(cd) = clip_data {
                            cov = mul255(cov, cd[clip_row + x]);
                        }
                        if cov == 0 {
                            pat_px[i] = [0, 0, 0, 0];
                        } else if cov < 255 {
                            pat_px[i][0] = mul255(pat_px[i][0], cov);
                            pat_px[i][1] = mul255(pat_px[i][1], cov);
                            pat_px[i][2] = mul255(pat_px[i][2], cov);
                            pat_px[i][3] = mul255(pat_px[i][3], cov);
                        }
                    }
                }

                if let Some(pixmap) = self.pixmap.as_mut() {
                    pixmap.draw_pixmap(
                        bx0 as i32,
                        by0 as i32,
                        pp.as_ref(),
                        &tiny_skia::PixmapPaint {
                            opacity: 1.0,
                            blend_mode: paint.blend_mode,
                            quality: tiny_skia::FilterQuality::Nearest,
                        },
                        tiny_skia::Transform::identity(),
                        None,
                    );
                    any_painted = true;
                }
            }
        }

        drop(mask_pix);
        drop(pat_pix);
        self.blend_surface_bytes = self.blend_surface_bytes.saturating_sub(live_bytes);
        if any_painted {
            self.mark_painted(footprint, 1.0);
        }
    }

    /// Transform a Type3 glyph path through: FontMatrix → text position → CTM → pixels.
    #[allow(clippy::too_many_arguments)]
    fn build_type3_transformed_path(
        &self,
        path: &Path,
        font_matrix: &[f64; 6],
        font_size: f32,
        h_scale: f32,
        tm: &zpdf_core::Matrix,
        glyph_offset: (f32, f32),
    ) -> Option<tiny_skia::Path> {
        let mut pb = tiny_skia::PathBuilder::new();

        for elem in path {
            match elem {
                PathElement::MoveTo(p) => {
                    let (px, py) = self.type3_to_pixel(
                        p.x,
                        p.y,
                        font_matrix,
                        font_size,
                        h_scale,
                        tm,
                        glyph_offset,
                    );
                    pb.move_to(px, py);
                }
                PathElement::LineTo(p) => {
                    let (px, py) = self.type3_to_pixel(
                        p.x,
                        p.y,
                        font_matrix,
                        font_size,
                        h_scale,
                        tm,
                        glyph_offset,
                    );
                    pb.line_to(px, py);
                }
                PathElement::CurveTo(c1, c2, end) => {
                    let (x1, y1) = self.type3_to_pixel(
                        c1.x,
                        c1.y,
                        font_matrix,
                        font_size,
                        h_scale,
                        tm,
                        glyph_offset,
                    );
                    let (x2, y2) = self.type3_to_pixel(
                        c2.x,
                        c2.y,
                        font_matrix,
                        font_size,
                        h_scale,
                        tm,
                        glyph_offset,
                    );
                    let (x, y) = self.type3_to_pixel(
                        end.x,
                        end.y,
                        font_matrix,
                        font_size,
                        h_scale,
                        tm,
                        glyph_offset,
                    );
                    pb.cubic_to(x1, y1, x2, y2, x, y);
                }
                PathElement::Close => pb.close(),
            }
        }
        self.finish_within_limits(pb)
    }

    /// Transform a point from Type3 glyph space all the way to pixel space.
    #[allow(clippy::too_many_arguments)]
    fn type3_to_pixel(
        &self,
        gx: f64,
        gy: f64,
        font_matrix: &[f64; 6],
        font_size: f32,
        h_scale: f32,
        tm: &zpdf_core::Matrix,
        glyph_offset: (f32, f32),
    ) -> (f32, f32) {
        // Step 1: FontMatrix * glyph_coord → text space
        let tx = font_matrix[0] * gx + font_matrix[2] * gy + font_matrix[4];
        let ty = font_matrix[1] * gx + font_matrix[3] * gy + font_matrix[5];

        // Step 2: scale by font_size; the shape x also carries Th (Tz/100).
        let tx = tx * font_size as f64 * h_scale as f64;
        let ty = ty * font_size as f64;

        // Step 3: add glyph offset (text space, advance already incl. Th)
        let tx = tx + glyph_offset.0 as f64;
        let ty = ty + glyph_offset.1 as f64;

        // Step 4: apply text matrix (= CTM * Tm) to get page-space coords
        let page_x = tm.a * tx + tm.c * ty + tm.e;
        let page_y = tm.b * tx + tm.d * ty + tm.f;

        // Step 5: page space → pixel space. The CTM's orientation is ordinary
        // geometry; the one fixed page Y-flip then maps to device pixels.
        let px = (page_x as f32 - self.rect_x0) * self.scale - self.dev_origin_x as f32;
        let py = (self.rect_y1 - page_y as f32) * self.scale_y - self.dev_origin_y as f32;

        (px, py)
    }
}

impl<'a> Default for CpuRenderer<'a> {
    fn default() -> Self {
        Self::new()
    }
}

/// Build shared `Arc<InlineImage>` wrappers once per Type 3 glyph program for
/// every glyph-local `ImageId` referenced inside its soft masks so repeated
/// glyph placements share one decoded pixel buffer instead of cloning per draw.
pub fn collect_type3_mask_inline_images(
    list: &DisplayList,
    images: &ImageCache,
) -> HashMap<ImageId, Arc<zpdf_display_list::InlineImage>> {
    fn visit(
        commands: &[RenderCommand],
        in_mask: bool,
        depth: u8,
        ids: &mut std::collections::HashSet<ImageId>,
    ) {
        if depth > 8 {
            return;
        }
        for cmd in commands {
            match cmd {
                RenderCommand::DrawImage(draw) if in_mask => {
                    ids.insert(draw.image_id);
                }
                RenderCommand::PushBlendGroup { mask: Some(m), .. } => {
                    visit(&m.commands.commands, true, depth + 1, ids);
                }
                _ => {}
            }
        }
    }

    let mut ids = std::collections::HashSet::new();
    visit(&list.commands, false, 0, &mut ids);
    let mut out = HashMap::with_capacity(ids.len());
    for id in ids {
        if let Some(img) = images.get(id) {
            out.insert(
                id,
                Arc::new(zpdf_display_list::InlineImage {
                    width: img.width,
                    height: img.height,
                    data: img.data.clone(),
                    has_alpha: img.has_alpha,
                    is_image_mask: img.is_image_mask,
                    premultiplied: img.premultiplied,
                }),
            );
        }
    }
    out
}

/// Map a local-space stroke (`StrokePath` or `PushClipStroke`) through `matrix`
/// into destination coordinates while preserving directional stroke widths,
/// elliptical caps/joins, and directional dash lengths under nonuniform scales
/// and shears.
pub fn transform_stroke_to_page(
    path: &Path,
    style: &StrokeStyle,
    matrix: &zpdf_core::Matrix,
    device_scale: f32,
) -> zpdf_display_list::TransformedStroke {
    let sx = matrix.a.hypot(matrix.b);
    let sy = matrix.c.hypot(matrix.d);
    let max_scale = CpuRenderer::max_linear_scale_2x2(matrix.a, matrix.b, matrix.c, matrix.d);
    if !sx.is_finite() || !sy.is_finite() || !max_scale.is_finite() || max_scale <= 1e-9 {
        return zpdf_display_list::TransformedStroke::Empty;
    }
    let dot = matrix.a * matrix.c + matrix.b * matrix.d;
    let is_uniform = (sx - sy).abs() <= 1e-5 * sx.max(sy).max(1.0) && dot.abs() <= 1e-5 * sx * sy;
    if is_uniform {
        let s = sx as f32;
        let scaled = StrokeStyle {
            width: if style.width > 0.0 && style.width.is_finite() {
                style.width * s
            } else {
                0.0
            },
            cap: style.cap,
            join: style.join,
            miter_limit: style.miter_limit,
            dash: style.dash.as_ref().map(|d| zpdf_display_list::DashPattern {
                array: d.array.iter().map(|v| v * s).collect(),
                phase: d.phase * s,
            }),
        };
        return zpdf_display_list::TransformedStroke::Stroked(
            path.map_points(|p| p.transform(matrix)),
            scaled,
        );
    }

    // Normalize local geometry by `norm` to page-point magnitude so tiny
    // glyph coordinates scaled up by a large matrix keep full stroking accuracy.
    let mut max_coord = 0.0_f64;
    for p in path.points() {
        if !p.x.is_finite() || !p.y.is_finite() {
            return zpdf_display_list::TransformedStroke::Empty;
        }
        max_coord = max_coord.max(p.x.abs()).max(p.y.abs());
    }
    let norm = if max_coord * max_scale <= 1e7 && (style.width as f64) * max_scale <= 1e7 {
        max_scale
    } else {
        1.0
    };
    let norm_f32 = norm as f32;
    let mut pb = tiny_skia::PathBuilder::new();
    for elem in path {
        match elem {
            PathElement::MoveTo(p) => pb.move_to((p.x * norm) as f32, (p.y * norm) as f32),
            PathElement::LineTo(p) => pb.line_to((p.x * norm) as f32, (p.y * norm) as f32),
            PathElement::CurveTo(c1, c2, end) => pb.cubic_to(
                (c1.x * norm) as f32,
                (c1.y * norm) as f32,
                (c2.x * norm) as f32,
                (c2.y * norm) as f32,
                (end.x * norm) as f32,
                (end.y * norm) as f32,
            ),
            PathElement::Close => pb.close(),
        }
    }
    let Some(norm_path) = pb.finish() else {
        return zpdf_display_list::TransformedStroke::Empty;
    };
    let dev = if device_scale.is_finite() && device_scale > 0.0 {
        device_scale
    } else {
        1.0
    };
    let res_scale = ((max_scale / norm) as f32 * dev).clamp(1.0, 1024.0);
    let post_matrix = zpdf_core::Matrix {
        a: matrix.a / norm,
        b: matrix.b / norm,
        c: matrix.c / norm,
        d: matrix.d / norm,
        e: matrix.e,
        f: matrix.f,
    };

    let safe_dash = style.dash.as_ref().and_then(|d| {
        let period: f64 = d
            .array
            .iter()
            .filter(|v| v.is_finite() && **v >= 0.0)
            .map(|&v| v as f64)
            .sum();
        if !period.is_finite() || period <= 0.0 {
            return None;
        }
        let mut chord_len = 0.0_f64;
        let mut start = zpdf_core::Point::new(0.0, 0.0);
        let mut cur = start;
        for elem in path {
            match elem {
                PathElement::MoveTo(p) => {
                    start = p;
                    cur = p;
                }
                PathElement::LineTo(p) => {
                    chord_len += (p.x - cur.x).hypot(p.y - cur.y);
                    cur = p;
                }
                PathElement::CurveTo(c1, c2, end) => {
                    chord_len += (c1.x - cur.x).hypot(c1.y - cur.y)
                        + (c2.x - c1.x).hypot(c2.y - c1.y)
                        + (end.x - c2.x).hypot(end.y - c2.y);
                    cur = end;
                }
                PathElement::Close => {
                    chord_len += (start.x - cur.x).hypot(start.y - cur.y);
                    cur = start;
                }
            }
        }
        let repeats = if d.array.len() % 2 == 1 {
            chord_len / (2.0 * period)
        } else {
            chord_len / period
        };
        if !repeats.is_finite() || repeats > 100_000.0 {
            return None;
        }
        CpuRenderer::stroke_dash(style, norm_f32)
    });

    if style.width.is_finite() && style.width > 0.0 {
        let mut glyph_stroke = CpuRenderer::skia_stroke(style, norm_f32, 0.0);
        glyph_stroke.dash = None;
        let dashed;
        let source = if let Some(ref dash) = safe_dash {
            let Some(d) = norm_path.dash(dash, res_scale) else {
                return zpdf_display_list::TransformedStroke::Empty;
            };
            dashed = d;
            &dashed
        } else {
            &norm_path
        };
        let Some(stroked) = source.stroke(&glyph_stroke, res_scale) else {
            return zpdf_display_list::TransformedStroke::Empty;
        };
        zpdf_display_list::TransformedStroke::Filled(skia_path_to_dl_transformed(
            &stroked,
            &post_matrix,
        ))
    } else {
        // 0-width PDF hairline: always 1 device pixel wide, while dashes still
        // follow glyph-space directional lengths under `matrix`.
        let hairline_style = StrokeStyle {
            width: 0.0,
            cap: style.cap,
            join: style.join,
            miter_limit: style.miter_limit,
            dash: None,
        };
        if let Some(ref dash) = safe_dash {
            let Some(dashed) = norm_path.dash(dash, res_scale) else {
                return zpdf_display_list::TransformedStroke::Empty;
            };
            zpdf_display_list::TransformedStroke::Stroked(
                skia_path_to_dl_transformed(&dashed, &post_matrix),
                hairline_style,
            )
        } else {
            zpdf_display_list::TransformedStroke::Stroked(
                path.map_points(|p| p.transform(matrix)),
                hairline_style,
            )
        }
    }
}

/// Convert a `tiny_skia::Path` (e.g. a pattern-space stroked outline) into a
/// [`Path`] transformed by `ctm` into page space.
fn skia_path_to_dl_transformed(src: &tiny_skia::Path, ctm: &zpdf_core::Matrix) -> Path {
    let mut out = Path::new();
    let mut cur = zpdf_core::Point::new(0.0, 0.0);
    for seg in src.segments() {
        match seg {
            tiny_skia::PathSegment::MoveTo(p) => {
                cur = zpdf_core::Point::new(p.x as f64, p.y as f64);
                out.move_to(cur.transform(ctm));
            }
            tiny_skia::PathSegment::LineTo(p) => {
                cur = zpdf_core::Point::new(p.x as f64, p.y as f64);
                out.line_to(cur.transform(ctm));
            }
            tiny_skia::PathSegment::QuadTo(p1, p2) => {
                let q1 = zpdf_core::Point::new(p1.x as f64, p1.y as f64);
                let q2 = zpdf_core::Point::new(p2.x as f64, p2.y as f64);
                let c1 = zpdf_core::Point::new(
                    cur.x + (2.0 / 3.0) * (q1.x - cur.x),
                    cur.y + (2.0 / 3.0) * (q1.y - cur.y),
                );
                let c2 = zpdf_core::Point::new(
                    q2.x + (2.0 / 3.0) * (q1.x - q2.x),
                    q2.y + (2.0 / 3.0) * (q1.y - q2.y),
                );
                out.curve_to(c1.transform(ctm), c2.transform(ctm), q2.transform(ctm));
                cur = q2;
            }
            tiny_skia::PathSegment::CubicTo(p1, p2, p3) => {
                let c1 = zpdf_core::Point::new(p1.x as f64, p1.y as f64);
                let c2 = zpdf_core::Point::new(p2.x as f64, p2.y as f64);
                let end = zpdf_core::Point::new(p3.x as f64, p3.y as f64);
                out.curve_to(c1.transform(ctm), c2.transform(ctm), end.transform(ctm));
                cur = end;
            }
            tiny_skia::PathSegment::Close => out.close(),
        }
    }
    out
}

/// Expand a positive-width stroke in pattern space and transform its outline by
/// `ctm` into page space so anisotropic direct-replay tiles preserve exact
/// directional stroke widths.
fn expand_anisotropic_stroke_to_page_path(
    path: &Path,
    style: &StrokeStyle,
    ctm: &zpdf_core::Matrix,
    device_scale: f32,
) -> Option<Path> {
    let mut pb = tiny_skia::PathBuilder::new();
    for elem in path {
        match elem {
            PathElement::MoveTo(p) => pb.move_to(p.x as f32, p.y as f32),
            PathElement::LineTo(p) => pb.line_to(p.x as f32, p.y as f32),
            PathElement::CurveTo(c1, c2, end) => pb.cubic_to(
                c1.x as f32,
                c1.y as f32,
                c2.x as f32,
                c2.y as f32,
                end.x as f32,
                end.y as f32,
            ),
            PathElement::Close => pb.close(),
        }
    }
    let unscaled = pb.finish()?;
    let s = device_scale.max(1e-4) as f64;
    let (a, b, c, d) = (ctm.a * s, ctm.b * s, ctm.c * s, ctm.d * s);
    let sum_sq = a * a + b * b + c * c + d * d;
    let det = a * d - b * c;
    let disc = (sum_sq * sum_sq - 4.0 * det * det).max(0.0).sqrt();
    let max_dev_scale = (0.5 * (sum_sq + disc)).sqrt() as f32;
    if !max_dev_scale.is_finite() || max_dev_scale <= 1e-6 {
        return None;
    }
    let res_scale = max_dev_scale.clamp(0.01, 1024.0);
    let pat_stroke = CpuRenderer::skia_stroke(style, 1.0, 1.0 / max_dev_scale);
    let dashed;
    let source = if let Some(ref dash) = pat_stroke.dash {
        dashed = unscaled.dash(dash, res_scale)?;
        &dashed
    } else {
        &unscaled
    };
    let stroked = source.stroke(&pat_stroke, res_scale)?;
    Some(skia_path_to_dl_transformed(&stroked, ctm))
}

fn scale_stroke_style_for_cpu(style: &StrokeStyle, scale: f32) -> StrokeStyle {
    let mut out = style.clone();
    if scale.is_finite() && scale > 0.0 {
        out.width *= scale;
        if let Some(dash) = &mut out.dash {
            for v in &mut dash.array {
                *v *= scale;
            }
            dash.phase *= scale;
        }
    }
    out
}

/// Transform a tiling cell's [`DisplayList`] into page space under `ctm` for
/// direct per-tile replay against the live CPU destination surface (used only
/// for overlapping backdrop-dependent cells or cells with active overprint).
/// Reuses `mask_memo` within one tile so multiple marks referencing the same
/// source soft-mask `Arc<DisplayList>` share one transformed copy.
fn transform_display_list_for_cpu(
    dl: &DisplayList,
    ctm: &zpdf_core::Matrix,
    device_scale: f32,
    mask_memo: &mut HashMap<usize, Arc<DisplayList>>,
) -> DisplayList {
    let col_x_sq = ctm.a * ctm.a + ctm.b * ctm.b;
    let col_y_sq = ctm.c * ctm.c + ctm.d * ctm.d;
    let dot = ctm.a * ctm.c + ctm.b * ctm.d;
    let ref_sq = col_x_sq.max(col_y_sq).max(1.0);
    let anisotropic = (col_x_sq - col_y_sq).abs() > 1e-4 * ref_sq || dot.abs() > 1e-4 * ref_sq;
    let stroke_scale = (ctm.a * ctm.d - ctm.b * ctm.c).abs().sqrt() as f32;

    let mut out = DisplayList::new(dl.page_rect);
    out.commands.reserve(dl.commands.len());
    for cmd in &dl.commands {
        let mapped = match cmd {
            RenderCommand::FillPath {
                path,
                rule,
                paint,
                alpha,
                overprint,
            } => RenderCommand::FillPath {
                path: path.map_points(|p| p.transform(ctm)),
                rule: *rule,
                paint: match paint {
                    Paint::Tiling(inner) => {
                        let mut nested = (**inner).clone();
                        nested.matrix = ctm.concat(&nested.matrix);
                        Paint::Tiling(Arc::new(nested))
                    }
                    other => other.clone(),
                },
                alpha: *alpha,
                overprint: *overprint,
            },
            RenderCommand::StrokePath {
                path,
                style,
                paint,
                alpha,
                overprint,
            } => {
                let mapped_paint = match paint {
                    Paint::Tiling(inner) => {
                        let mut nested = (**inner).clone();
                        nested.matrix = ctm.concat(&nested.matrix);
                        Paint::Tiling(Arc::new(nested))
                    }
                    other => other.clone(),
                };
                if anisotropic && style.width.is_finite() && style.width > 0.0 {
                    if let Some(expanded) =
                        expand_anisotropic_stroke_to_page_path(path, style, ctm, device_scale)
                    {
                        RenderCommand::FillPath {
                            path: expanded,
                            rule: FillRule::NonZero,
                            paint: mapped_paint,
                            alpha: *alpha,
                            overprint: *overprint,
                        }
                    } else {
                        continue;
                    }
                } else {
                    RenderCommand::StrokePath {
                        path: path.map_points(|p| p.transform(ctm)),
                        style: scale_stroke_style_for_cpu(style, stroke_scale),
                        paint: mapped_paint,
                        alpha: *alpha,
                        overprint: *overprint,
                    }
                }
            }
            RenderCommand::DrawGlyphRun(run) => {
                let mut cloned = run.clone();
                cloned.transform = ctm.concat(&cloned.transform);
                if let Paint::Tiling(inner) = &cloned.paint {
                    let mut nested = (**inner).clone();
                    nested.matrix = ctm.concat(&nested.matrix);
                    cloned.paint = Paint::Tiling(Arc::new(nested));
                }
                RenderCommand::DrawGlyphRun(cloned)
            }
            RenderCommand::DrawImage(draw) => {
                let mut cloned = draw.clone();
                cloned.transform = ctm.concat(&cloned.transform);
                RenderCommand::DrawImage(cloned)
            }
            RenderCommand::DrawInlineImage(draw) => {
                let mut cloned = draw.clone();
                cloned.transform = ctm.concat(&cloned.transform);
                RenderCommand::DrawInlineImage(cloned)
            }
            RenderCommand::PushClip { path, rule } => RenderCommand::PushClip {
                path: path.map_points(|p| p.transform(ctm)),
                rule: *rule,
            },
            RenderCommand::PushClipStroke { path, style } => {
                if anisotropic && style.width.is_finite() && style.width > 0.0 {
                    let expanded =
                        expand_anisotropic_stroke_to_page_path(path, style, ctm, device_scale)
                            .unwrap_or_default();
                    RenderCommand::PushClip {
                        path: expanded,
                        rule: FillRule::NonZero,
                    }
                } else {
                    RenderCommand::PushClipStroke {
                        path: path.map_points(|p| p.transform(ctm)),
                        style: scale_stroke_style_for_cpu(style, stroke_scale),
                    }
                }
            }
            RenderCommand::PopClip => RenderCommand::PopClip,
            RenderCommand::PushBlendGroup {
                blend_mode,
                isolated,
                knockout,
                bounds,
                alpha,
                mask,
            } => RenderCommand::PushBlendGroup {
                blend_mode: *blend_mode,
                isolated: *isolated,
                knockout: *knockout,
                bounds: *bounds,
                alpha: *alpha,
                mask: mask.as_ref().map(|m| {
                    let mut mm = m.clone();
                    let (ox, oy) = (m.offset.0 as f64, m.offset.1 as f64);
                    mm.offset = (
                        (ctm.a * ox + ctm.c * oy) as f32,
                        (ctm.b * ox + ctm.d * oy) as f32,
                    );
                    let src_ptr = Arc::as_ptr(&m.commands) as *const () as usize;
                    mm.commands = if let Some(cached) = mask_memo.get(&src_ptr) {
                        Arc::clone(cached)
                    } else {
                        let transformed = Arc::new(transform_display_list_for_cpu(
                            &m.commands,
                            ctm,
                            device_scale,
                            mask_memo,
                        ));
                        mask_memo.insert(src_ptr, Arc::clone(&transformed));
                        transformed
                    };
                    mm
                }),
            },
            RenderCommand::PopBlendGroup => RenderCommand::PopBlendGroup,
        };
        out.commands.push(mapped);
    }
    out
}

/// Composite a finished blend group onto its backdrop, confined to `region`
/// (device pixels, half-open) instead of the whole raster.
///
/// Equivalent to `base.draw_pixmap(0, 0, group, paint, identity, None)` and
/// pixel-identical to it inside the region: same pattern shader, same filter
/// quality, same blend pipeline, same integer alignment. The only difference is
/// that the blit stops at the region, which is exact wherever the group is
/// transparent — a zero source alpha leaves the backdrop unchanged under every
/// blend mode, since the general Porter-Duff form `s·(1−da) + d·(1−sa) +
/// B(s,d)·sa·da` with `sa = 0` reduces to `d`. `region` must therefore contain
/// every pixel the group painted (see [`PaintedBounds`]).
fn composite_group_region(
    base: &mut tiny_skia::Pixmap,
    group: &tiny_skia::Pixmap,
    paint: &tiny_skia::PixmapPaint,
    region: tiny_skia::Rect,
) {
    let scoped = tiny_skia::Paint {
        shader: tiny_skia::Pattern::new(
            group.as_ref(),
            tiny_skia::SpreadMode::Pad,
            paint.quality,
            paint.opacity,
            tiny_skia::Transform::identity(),
        ),
        blend_mode: paint.blend_mode,
        // A pixmap blit is not anti-aliased in Skia either, and the region is
        // aligned to whole pixels, so there is nothing to anti-alias.
        anti_alias: false,
        ..Default::default()
    };
    base.fill_rect(region, &scoped, tiny_skia::Transform::identity(), None);
}

/// Bytes of an RGBA surface.
fn surface_bytes(surface: &tiny_skia::Pixmap) -> u64 {
    surface.width() as u64 * surface.height() as u64 * 4
}

/// Call `f` with the byte range of each row of `region` in an RGBA surface
/// `width` pixels wide.
fn for_region_rows(region: PaintedBounds, width: u32, mut f: impl FnMut(std::ops::Range<usize>)) {
    let stride = width as usize * 4;
    let (x0, x1) = (region.x0 as usize * 4, region.x1 as usize * 4);
    for y in region.y0 as usize..region.y1 as usize {
        f(y * stride + x0..y * stride + x1);
    }
}

/// Knockout merge: `group = lerp(group, elem OVER b0, shape)`, in place,
/// premultiplied RGBA8. `shape` carries the element's geometric coverage in its
/// alpha channel (a full-opacity render of the same element), while `elem`
/// carries its real premultiplied colour/opacity. `b0` is the group's initial
/// backdrop (`None` == transparent, where `elem OVER transparent == elem`).
/// Where shape is full, the element fully knocks out preceding elements.
fn knockout_merge(group: &mut [u8], elem: &[u8], shape: &[u8], b0: Option<&[u8]>) {
    let n = group.len() / 4;
    for i in 0..n {
        let o = i * 4;
        let f = shape[o + 3]; // geometric coverage of this element
        if f == 0 {
            continue;
        }
        let ea = elem[o + 3];
        let inv_e = (255 - ea) as u32; // 1 − elem.alpha
                                       // X = elem OVER b0 (premultiplied), using the element's real opacity.
        let x = match b0 {
            Some(b) => [
                elem[o] as u32 + (inv_e * b[o] as u32 + 127) / 255,
                elem[o + 1] as u32 + (inv_e * b[o + 1] as u32 + 127) / 255,
                elem[o + 2] as u32 + (inv_e * b[o + 2] as u32 + 127) / 255,
                ea as u32 + (inv_e * b[o + 3] as u32 + 127) / 255,
            ],
            None => [
                elem[o] as u32,
                elem[o + 1] as u32,
                elem[o + 2] as u32,
                ea as u32,
            ],
        };
        // group = group·(1 − shape) + X·shape.
        let inv_f = (255 - f) as u32;
        for c in 0..4 {
            let v = (group[o + c] as u32 * inv_f + x[c] * f as u32 + 127) / 255;
            group[o + c] = v.min(255) as u8;
        }
    }
}

/// Knockout merge for a child whose premultiplied `x_covered` ALREADY carries
/// the element's antialiased geometric coverage `f_k` inside its premultiplied
/// color and alpha (ISO 32000-1 §11.4.6):
///
/// `out = x_covered + (group - b0) * (1 - f_k)`
///
/// where `x_covered` is `composite(b0, child_premul)` (or `child_premul` when
/// `b0` is `None` / transparent). Uses `shape` (`f_k`) ONLY to attenuate
/// preceding siblings (and remove the uncovered `(1 - f_k) * b0` portion
/// baked into `x_covered`) without double-multiplying the child's antialiased
/// edge coverage.
fn knockout_merge_premultiplied(
    group: &mut [u8],
    x_covered: &[u8],
    shape: &[u8],
    b0: Option<&[u8]>,
) {
    let n = group.len() / 4;
    for i in 0..n {
        let o = i * 4;
        let f = shape[o + 3];
        if f == 0 {
            continue;
        }
        let inv_f = (255 - f) as f32 / 255.0;
        for c in 0..4 {
            let g = group[o + c] as f32;
            let x = x_covered[o + c] as f32;
            let b = b0.map_or(0.0, |b0| b0[o + c] as f32);
            let v = (x + (g - b) * inv_f).round().clamp(0.0, 255.0) as u8;
            group[o + c] = v;
        }
    }
}

/// The overprint descriptor of a painting command, if any (`None` for images
/// and non-overprinting paints).
fn paint_overprint(cmd: &RenderCommand) -> Option<Overprint> {
    match cmd {
        RenderCommand::FillPath { overprint, .. } | RenderCommand::StrokePath { overprint, .. } => {
            *overprint
        }
        RenderCommand::DrawGlyphRun(run) => run.overprint,
        _ => None,
    }
}

/// Merge an overprinting element onto the canvas in naïve subtractive CMYK.
/// `canvas`/`elem` are premultiplied RGBA8; `elem`'s alpha is the element's
/// coverage·opacity. For each covered pixel the backdrop is decomposed into
/// colorants, the `active` channels are taken from `op.cmyk` (others kept), the
/// result recomposed to RGB, and written back premultiplied. A colorant the
/// element does not paint round-trips exactly (rgb→cmyk→rgb is the identity for
/// the naïve model), so the backdrop is undisturbed where the element is silent.
fn overprint_merge(canvas: &mut [u8], elem: &[u8], op: &Overprint) {
    for (c, e) in canvas
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(elem.as_chunks::<4>().0)
    {
        let a = e[3] as f64 / 255.0; // coverage · opacity
        if a <= 0.0 {
            continue;
        }
        let ba = c[3] as f64 / 255.0;
        // Decompose the backdrop into colorants. A transparent backdrop carries
        // no ink (white paper), so retained colorants stay 0.
        let (br, bg, bb) = if ba > 0.0 {
            (
                c[0] as f64 / 255.0 / ba,
                c[1] as f64 / 255.0 / ba,
                c[2] as f64 / 255.0 / ba,
            )
        } else {
            (1.0, 1.0, 1.0)
        };
        let (bc, bm, by, bk) = zpdf_core::rgb_to_cmyk_naive(br, bg, bb);
        let bd = [bc, bm, by, bk];
        let mut res = [0f64; 4];
        for i in 0..4 {
            let tgt = if op.active & (1 << i) != 0 {
                unit(op.cmyk[i]) as f64
            } else {
                bd[i]
            };
            res[i] = bd[i] + a * (tgt - bd[i]);
        }
        let (rr, rg, rb) = zpdf_core::cmyk_to_rgb_naive(res[0], res[1], res[2], res[3]);
        let out_a = ba + a * (1.0 - ba);
        let enc = |v: f64| (v * out_a * 255.0).round().clamp(0.0, 255.0) as u8;
        c[0] = enc(rr);
        c[1] = enc(rg);
        c[2] = enc(rb);
        c[3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
    }
}

/// RGBA pixel buffer output.
pub struct RenderedPage {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl RenderedPage {
    /// Save the rendered page to a PNG file. Consumes the page to avoid cloning
    /// the pixel buffer (eliminates 512MB copy on 64Mpx pages).
    pub fn save_png(self, path: &str) -> Result<(), CpuRenderError> {
        let img = image::RgbaImage::from_raw(self.width, self.height, self.data)
            .ok_or(CpuRenderError::PixmapCreation)?;
        img.save(path)
            .map_err(|e| CpuRenderError::PngEncode(e.to_string()))
    }
}

impl<'a> RenderBackend for CpuRenderer<'a> {
    type Target = RenderedPage;
    type Error = CpuRenderError;

    fn begin_page(&mut self, info: &PageRenderInfo) -> Result<(), Self::Error> {
        // Start the page clock and clear the previous page's buckets first, so
        // `total_ns` spans allocation through `end_page` — per-page raster
        // allocation is real cost on a multi-page run, not just the drawing.
        self.page_start = self
            .stage_stats
            .as_ref()
            .map(|_| zpdf_core::time::Instant::now());
        if self.stage_stats.is_some() {
            self.stage_stats = Some(zpdf_render::StageStats::default());
        }
        // A failed replacement must not leave the previous page available to a
        // later `end_page` call.
        self.pixmap = None;
        self.dirty = None;
        self.rect_x0 = info.page_rect.x0 as f32;
        self.rect_y0 = info.page_rect.y0 as f32;
        self.rect_y1 = info.page_rect.y1 as f32;
        self.dev_origin_x = 0;
        self.dev_origin_y = 0;
        // Fresh clip + blend state + budget for each page (a renderer may be
        // reused). An unbalanced group from a prior page must not leak through.
        self.clip_stack.clear();
        self.current_clip = None;
        self.clip_bounds = None;
        self.blend_stack.clear();
        self.recording_non_isolated = None;
        self.non_isolated_depth = 0;
        self.alpha_only_pass = false;
        self.shape_only_pass = false;
        self.blend_surface_bytes = 0;
        self.surface_pool.clear();
        self.pooled_surface_bytes = 0;
        self.skipped_blend_depth = 0;
        self.clip_pixel_spent = 0;
        self.clip_mask_bytes = 0;
        self.mask_pool.clear();
        self.over_budget = false;
        self.soft_mask_depth = 0;
        self.soft_mask_planes.clear();
        self.soft_mask_cache_bytes = 0;
        self.tiling_depth = 0;
        self.tiling_cache.clear();
        self.tiling_cache_bytes = 0;
        self.tiling_backdrop_cache.clear();
        self.downscaled_images.clear();
        self.glyph_outlines.clear();
        self.glyph_outline_commands = 0;
        self.type3_glyphs.clear();
        self.type3_glyph_commands = 0;
        self.type3_glyph_image_bytes = 0;
        self.deadline = self
            .render_budget
            .map(|d| zpdf_core::time::Instant::now() + d);

        let (scale, w, h) = self.fit_raster(info)?;
        if scale != info.scale {
            tracing::warn!(
                limit = self.max_page_pixels,
                "raster exceeds configured budget; clamped to {w}x{h} (scale {:.4} -> {:.4})",
                info.scale,
                scale
            );
        }
        self.scale = scale;
        self.scale_y = scale;

        let mut pixmap = tiny_skia::Pixmap::new(w, h).ok_or(CpuRenderError::PixmapCreation)?;

        let bg = &info.background;
        pixmap.fill(
            tiny_skia::Color::from_rgba(
                bg.r.clamp(0.0, 1.0),
                bg.g.clamp(0.0, 1.0),
                bg.b.clamp(0.0, 1.0),
                bg.a.clamp(0.0, 1.0),
            )
            .unwrap_or(tiny_skia::Color::WHITE),
        );

        self.pixmap = Some(pixmap);
        self.blend_surface_bytes = w as u64 * h as u64 * 4;
        Ok(())
    }

    fn execute(&mut self, cmd: &RenderCommand) -> Result<(), Self::Error> {
        if self.pixmap.is_none() {
            return Err(CpuRenderError::NotInitialized);
        }
        // Anti-hang backstop: once the page's wall-clock budget is spent, skip
        // remaining draws so the command loop drains and the page returns
        // partially rendered instead of hanging. Checked before EVERY command:
        // a page may have only a handful of commands that are each individually
        // very expensive (e.g. dozens of large images scaled across a huge
        // raster), so a coarse stride would never sample the clock in time.
        // Instant::now is cheap enough to call per command.
        if let Some(deadline) = self.deadline {
            if zpdf_core::time::Instant::now() >= deadline {
                self.over_budget = true;
                tracing::warn!("render exceeded time budget; truncating page");
            }
        }

        if let Some(ref mut rec) = self.recording_non_isolated {
            if self.over_budget && !rec.cmds.is_empty() {
                rec.cmds.clear();
                rec.cmds.shrink_to_fit();
            }
            match cmd {
                RenderCommand::PushBlendGroup { .. } => {
                    rec.nesting += 1;
                    if !self.over_budget {
                        rec.cmds.push(cmd.clone());
                    }
                }
                RenderCommand::PopBlendGroup if rec.nesting > 0 => {
                    rec.nesting -= 1;
                    if !self.over_budget {
                        rec.cmds.push(cmd.clone());
                    }
                }
                RenderCommand::PopBlendGroup => {
                    if let Some(finished) = self.recording_non_isolated.take() {
                        self.finish_recorded_non_isolated_group(finished);
                    }
                }
                _ => {
                    if !self.over_budget {
                        rec.cmds.push(cmd.clone());
                    }
                }
            }
            return Ok(());
        }

        if self.over_budget {
            // Continue only structural commands so a deadline reached inside a
            // clip/blend group cannot leak that group's offscreen target into
            // `end_page` or accidentally pop an outer group.
            match cmd {
                RenderCommand::PushClip { .. } | RenderCommand::PushClipStroke { .. } => {
                    self.clip_stack.push(ClipFrame::Skipped);
                }
                RenderCommand::PopClip => self.pop_clip(),
                RenderCommand::PushBlendGroup { .. } => self.skipped_blend_depth += 1,
                RenderCommand::PopBlendGroup if self.skipped_blend_depth > 0 => {
                    self.skipped_blend_depth -= 1;
                }
                RenderCommand::PopBlendGroup => self.pop_blend_group(),
                _ => {}
            }
            return Ok(());
        }

        // Inside a knockout group each painting element composites against the
        // group's initial backdrop rather than the accumulation of preceding
        // elements: route paints through the per-element knockout path.
        if self.in_knockout()
            && matches!(
                cmd,
                RenderCommand::FillPath { .. }
                    | RenderCommand::StrokePath { .. }
                    | RenderCommand::DrawGlyphRun(_)
                    | RenderCommand::DrawImage(_)
                    | RenderCommand::DrawInlineImage(_)
            )
        {
            self.knockout_paint(cmd);
            return Ok(());
        }

        // Overprint (PDF 8.6.7): composite the element in subtractive CMYK so it
        // leaves the colorants it does not paint untouched. (Inside a knockout
        // group the knockout path above takes precedence — a rare combination.)
        if let Some(op) = paint_overprint(cmd) {
            self.overprint_paint(cmd, &op);
            return Ok(());
        }

        match cmd {
            RenderCommand::FillPath { .. }
            | RenderCommand::StrokePath { .. }
            | RenderCommand::DrawGlyphRun(_)
            | RenderCommand::DrawImage(_)
            | RenderCommand::DrawInlineImage(_) => {
                self.render_paint_cmd(cmd);
            }
            RenderCommand::PushClip { path, rule } => {
                self.push_clip(path, rule);
            }
            RenderCommand::PushClipStroke { path, style } => {
                self.push_clip_stroke(path, style);
            }
            RenderCommand::PopClip => {
                self.pop_clip();
            }
            RenderCommand::PushBlendGroup {
                blend_mode,
                isolated,
                knockout,
                alpha,
                mask,
                ..
            } => {
                self.push_blend_group(*blend_mode, *isolated, *knockout, *alpha, mask.as_ref());
            }
            RenderCommand::PopBlendGroup if self.skipped_blend_depth > 0 => {
                self.skipped_blend_depth -= 1;
            }
            RenderCommand::PopBlendGroup => self.pop_blend_group(),
        }
        Ok(())
    }

    fn end_page(&mut self) -> Result<Self::Target, Self::Error> {
        // Gracefully close a malformed/truncated display list. This also restores
        // the real page target if the deadline fired inside an open blend group.
        self.recording_non_isolated = None;
        self.non_isolated_depth = 0;
        self.alpha_only_pass = false;
        self.shape_only_pass = false;
        while !self.blend_stack.is_empty() {
            self.pop_blend_group();
        }
        let pixmap = self.pixmap.take().ok_or(CpuRenderError::NotInitialized)?;
        let width = pixmap.width();
        let height = pixmap.height();
        let data = pixmap.take();
        self.clip_stack.clear();
        self.current_clip = None;
        self.clip_bounds = None;
        self.clip_mask_bytes = 0;
        self.mask_pool.clear();
        self.soft_mask_planes.clear();
        self.soft_mask_cache_bytes = 0;
        self.tiling_cache.clear();
        self.tiling_cache_bytes = 0;
        self.tiling_backdrop_cache.clear();
        self.downscaled_images.clear();
        self.deadline = None;
        self.skipped_blend_depth = 0;
        self.blend_surface_bytes = 0;
        self.surface_pool.clear();
        self.pooled_surface_bytes = 0;
        // Close the page clock last, so `total_ns` includes any blend groups
        // drained above and any over-budget unwinding.
        if let (Some(start), Some(stats)) = (self.page_start.take(), self.stage_stats.as_mut()) {
            stats.total_ns = start.elapsed().as_nanos() as u64;
        }
        Ok(RenderedPage {
            width,
            height,
            data,
        })
    }

    fn stage_stats(&self) -> Option<zpdf_render::StageStats> {
        self.stage_stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zpdf_core::{Matrix, Point, Rect};
    use zpdf_image::DecodedImage;

    fn px(page: &RenderedPage, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * page.width + x) * 4) as usize;
        [
            page.data[i],
            page.data[i + 1],
            page.data[i + 2],
            page.data[i + 3],
        ]
    }

    fn line_path(x0: f64, y0: f64, x1: f64, y1: f64) -> Path {
        let mut p = Path::new();
        p.move_to(Point::new(x0, y0));
        p.line_to(Point::new(x1, y1));
        p
    }

    #[test]
    fn paint_rounds_colour_and_alpha_to_nearest_byte() {
        for (value, expected) in [
            (-1.0, 0),
            (0.0, 0),
            (0.49 / 255.0, 0),
            (0.5 / 255.0, 1),
            (0.499, 127),
            (0.5, 128),
            (254.49 / 255.0, 254),
            (254.5 / 255.0, 255),
            (1.0, 255),
            (2.0, 255),
            (f32::NAN, 0),
            (f32::NEG_INFINITY, 0),
            (f32::INFINITY, 255),
        ] {
            let paint = CpuRenderer::color_to_paint(&Color::rgba(value, value, value, value), 1.0);
            let tiny_skia::Shader::SolidColor(color) = paint.shader else {
                panic!("expected solid paint");
            };
            assert_eq!(
                color.to_color_u8(),
                tiny_skia::ColorU8::from_rgba(expected, expected, expected, expected),
                "component {value}"
            );
        }
        let paint = CpuRenderer::color_to_paint(&Color::rgba(0.0, 0.5, 1.0, 0.5), 0.5);
        let tiny_skia::Shader::SolidColor(color) = paint.shader else {
            panic!("expected solid paint");
        };
        assert_eq!(
            color.to_color_u8(),
            tiny_skia::ColorU8::from_rgba(0, 128, 255, 64)
        );
    }

    #[test]
    fn half_gray_pdf_fill_rounds_to_128() {
        let list =
            zpdf_content::interpreter::ContentInterpreter::new(Rect::new(0.0, 0.0, 2.0, 2.0))
                .interpret(b"0.5 g 0 0 2 2 re f");
        let page = CpuRenderer::new().render_display_list(&list, 1.0).unwrap();
        assert!(page
            .data
            .as_chunks::<4>()
            .0
            .iter()
            .all(|p| *p == [128, 128, 128, 255]));
    }

    #[test]
    fn mask_products_round_for_every_byte_pair() {
        for v in 0..=255 {
            for a in 0..=255 {
                let expected = (f64::from(v) * f64::from(a) / 255.0).round() as u8;
                assert_eq!(mul255(v, a), expected, "{v} * {a} / 255");
            }
        }
    }

    #[test]
    fn clip_intersections_round_mask_products() {
        for stroked in [false, true] {
            let coverage = |current: u8| {
                let mut renderer = CpuRenderer::new();
                renderer
                    .begin_page(&PageRenderInfo {
                        page_rect: Rect::new(0.0, 0.0, 3.0, 3.0),
                        scale: 1.0,
                        background: Color::white(),
                    })
                    .unwrap();
                renderer.push_clip(&rect_path(0.0, 0.0, 3.0, 3.0), &FillRule::NonZero);
                renderer
                    .current_clip
                    .as_mut()
                    .unwrap()
                    .data_mut()
                    .fill(current);
                if stroked {
                    renderer
                        .push_clip_stroke(&line_path(0.5, 1.5, 2.5, 1.5), &StrokeStyle::default());
                } else {
                    renderer.push_clip(&rect_path(0.5, 0.5, 2.5, 2.5), &FillRule::NonZero);
                }
                renderer.current_clip.unwrap().data().to_vec()
            };
            let unmasked = coverage(255);
            assert!(unmasked.iter().any(|&v| v > 0 && v < 255));
            for current in [0, 1, 127, 128, 254, 255] {
                let expected: Vec<u8> = unmasked
                    .iter()
                    .map(|&v| (f64::from(v) * f64::from(current) / 255.0).round() as u8)
                    .collect();
                assert_eq!(
                    coverage(current),
                    expected,
                    "stroked={stroked}, current={current}"
                );
            }
        }
    }

    fn transparent_pixel_renderer() -> CpuRenderer<'static> {
        let mut renderer = CpuRenderer::new();
        renderer
            .begin_page(&PageRenderInfo {
                page_rect: Rect::new(0.0, 0.0, 1.0, 1.0),
                scale: 1.0,
                background: Color::rgba(0.0, 0.0, 0.0, 0.0),
            })
            .unwrap();
        renderer
    }

    #[test]
    fn soft_mask_rounds_premultiplied_colour_and_alpha_products() {
        for (coverage, expected) in [(0, 0), (127, 64), (128, 65), (255, 129)] {
            let mask = SoftMask {
                kind: SoftMaskKind::Alpha,
                commands: Arc::new(DisplayList::new(Rect::new(0.0, 0.0, 1.0, 1.0))),
                offset: (0.0, 0.0),
                backdrop_luma: 0.0,
                transfer: Some(Arc::new([coverage; 256])),
            };
            let mut renderer = transparent_pixel_renderer();
            renderer.push_blend_group(BlendMode::Normal, true, false, 1.0, Some(&mask));
            renderer
                .execute(&solid_rect(
                    Color::rgba(1.0, 1.0, 1.0, 129.0 / 255.0),
                    0.0,
                    0.0,
                    1.0,
                    1.0,
                ))
                .unwrap();
            renderer.pop_blend_group();
            assert_eq!(
                px(&renderer.end_page().unwrap(), 0, 0),
                [expected; 4],
                "coverage {coverage}"
            );
        }
    }

    #[test]
    fn type3_image_mask_rounds_tinted_channels() {
        let mut images = ImageCache::new();
        let image_id = images.insert(DecodedImage {
            width: 1,
            height: 1,
            data: vec![0, 0, 0, 128],
            has_alpha: true,
            is_image_mask: true,
            premultiplied: true,
        });
        let draw = ImageDraw {
            image_id,
            transform: Matrix::identity(),
            alpha: 1.0,
            is_image_mask: true,
        };
        let mut paint = tiny_skia::Paint::default();
        paint.set_color_rgba8(1, 127, 254, 255);
        let mut renderer = transparent_pixel_renderer();
        renderer.render_type3_image(&images, &draw, &paint, 1.0);
        assert_eq!(px(&renderer.end_page().unwrap(), 0, 0), [1, 64, 127, 128]);
    }

    #[test]
    fn execute_requires_an_active_page() {
        let mut renderer = CpuRenderer::new();
        assert!(matches!(
            renderer.execute(&RenderCommand::PopClip),
            Err(CpuRenderError::NotInitialized)
        ));
    }

    #[test]
    fn invalid_begin_drops_the_previous_page() {
        let mut renderer = CpuRenderer::new();
        renderer
            .begin_page(&PageRenderInfo {
                page_rect: Rect::new(0.0, 0.0, 2.0, 2.0),
                scale: 1.0,
                background: Color::white(),
            })
            .unwrap();
        assert!(matches!(
            renderer.begin_page(&PageRenderInfo {
                page_rect: Rect::new(0.0, 0.0, f64::INFINITY, 2.0),
                scale: 1.0,
                background: Color::white(),
            }),
            Err(CpuRenderError::InvalidPage(_))
        ));
        assert!(matches!(
            renderer.end_page(),
            Err(CpuRenderError::NotInitialized)
        ));
    }

    #[test]
    fn document_page_pixel_limit_uniformly_downscales_before_allocation() {
        let limits = ParseLimits {
            max_page_pixels: 25,
            ..ParseLimits::default()
        };
        let mut renderer = CpuRenderer::new().with_limits(&limits);
        renderer
            .begin_page(&PageRenderInfo {
                page_rect: Rect::new(0.0, 0.0, 10.0, 10.0),
                scale: 1.0,
                background: Color::white(),
            })
            .unwrap();
        let page = renderer.end_page().unwrap();
        assert_eq!((page.width, page.height), (5, 5));
        assert!(page.width as u64 * page.height as u64 <= limits.max_page_pixels);
    }

    #[test]
    fn page_pixel_limit_is_exact_for_extreme_aspect_ratios() {
        let (scale, width, height) = fit_raster_to_pixel_limit(1.0, 1000.0, 1.0, 100).unwrap();
        assert!(scale > 0.0);
        assert_eq!(width, 1);
        assert!(width as u64 * height as u64 <= 100);
    }

    #[test]
    fn drawn_extent_clamps_to_integer_axes_above_f32_precision() {
        // Exercise both rounding directions without allocating a large raster.
        for long_axis in [16_777_217, 16_777_219, 33_554_431, 63_999_999] {
            for (width, height) in [(long_axis, 1), (1, long_axis)] {
                let right = width as f32 + 16.0;
                let bottom = height as f32 + 16.0;
                let covering = tiny_skia::Rect::from_ltrb(0.0, 0.0, right, bottom).unwrap();
                assert_eq!(
                    CpuRenderer::drawn_extent(covering, 0.0, width, height),
                    PaintedBounds::whole(width, height),
                );

                let outside =
                    tiny_skia::Rect::from_ltrb(right, bottom, right + 16.0, bottom + 16.0).unwrap();
                assert_eq!(
                    CpuRenderer::drawn_extent(outside, 0.0, width, height),
                    PaintedBounds {
                        x0: width,
                        y0: height,
                        x1: width,
                        y1: height,
                    },
                );
            }
        }
    }

    #[test]
    fn drawn_extent_keeps_conservative_small_raster_rounding() {
        let bounds = tiny_skia::Rect::from_ltrb(2.25, 3.75, 7.25, 8.25).unwrap();
        assert_eq!(
            CpuRenderer::drawn_extent(bounds, 0.5, 10, 10),
            PaintedBounds {
                x0: 1,
                y0: 3,
                x1: 8,
                y1: 9,
            },
        );
        assert_eq!(
            CpuRenderer::drawn_extent(bounds, f32::INFINITY, 10, 10),
            PaintedBounds::whole(10, 10),
        );
    }

    #[test]
    fn zero_page_pixel_limit_rejects_before_allocation() {
        let limits = ParseLimits {
            max_page_pixels: 0,
            ..ParseLimits::default()
        };
        let mut renderer = CpuRenderer::new().with_limits(&limits);
        let err = renderer
            .begin_page(&PageRenderInfo {
                page_rect: Rect::new(0.0, 0.0, 1.0, 1.0),
                scale: 1.0,
                background: Color::white(),
            })
            .unwrap_err();
        assert!(matches!(err, CpuRenderError::LimitExceeded(_)));
        assert!(renderer.pixmap.is_none());
    }

    #[test]
    fn document_blend_depth_limit_keeps_structural_pops_balanced() {
        let limits = ParseLimits {
            max_blend_group_depth: 1,
            ..ParseLimits::default()
        };
        let mut renderer = CpuRenderer::new().with_limits(&limits);
        renderer
            .begin_page(&PageRenderInfo {
                page_rect: Rect::new(0.0, 0.0, 4.0, 4.0),
                scale: 1.0,
                background: Color::white(),
            })
            .unwrap();
        let push = RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Normal,
            isolated: true,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 4.0, 4.0),
            alpha: 1.0,
            mask: None,
        };
        renderer.execute(&push).unwrap();
        renderer.execute(&push).unwrap();
        assert_eq!(renderer.blend_stack.len(), 1);
        assert_eq!(renderer.skipped_blend_depth, 1);
        renderer.execute(&RenderCommand::PopBlendGroup).unwrap();
        assert_eq!(renderer.blend_stack.len(), 1);
        assert_eq!(renderer.skipped_blend_depth, 0);
        renderer.execute(&RenderCommand::PopBlendGroup).unwrap();
        assert!(renderer.blend_stack.is_empty());

        // A recorded non-isolated Screen parent also occupies depth 1, so with
        // max_blend_group_depth = 1 a nested child group inside it must be
        // skipped to passthrough rather than allocating/compositing a second
        // blend group (verified by alpha = 0.0 on the child: if not skipped it
        // would hide the inner red fill; when skipped the red fill draws through).
        let mut full = Path::new();
        full.rect(Rect::new(0.0, 0.0, 4.0, 4.0));
        renderer
            .execute(&RenderCommand::FillPath {
                path: full.clone(),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::black()),
                alpha: 1.0,
                overprint: None,
            })
            .unwrap();
        renderer
            .execute(&RenderCommand::PushBlendGroup {
                blend_mode: BlendMode::Screen,
                isolated: false,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 4.0, 4.0),
                alpha: 1.0,
                mask: None,
            })
            .unwrap();
        renderer
            .execute(&RenderCommand::PushBlendGroup {
                blend_mode: BlendMode::Normal,
                isolated: true,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 4.0, 4.0),
                alpha: 0.0,
                mask: None,
            })
            .unwrap();
        renderer
            .execute(&RenderCommand::FillPath {
                path: full,
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
                alpha: 1.0,
                overprint: None,
            })
            .unwrap();
        renderer.execute(&RenderCommand::PopBlendGroup).unwrap();
        renderer.execute(&RenderCommand::PopBlendGroup).unwrap();
        assert!(renderer.blend_stack.is_empty());
        assert_eq!(renderer.skipped_blend_depth, 0);
        assert_eq!(renderer.non_isolated_depth, 0);
        let page = renderer.end_page().unwrap();
        let px = &page.data[0..3];
        assert!(
            px[0] > 240 && px[1] < 10 && px[2] < 10,
            "nested child group inside recorded non-isolated Screen parent at depth limit 1 must be skipped to passthrough, got {px:?}"
        );
    }

    #[test]
    fn recording_non_isolated_group_drops_saved_draws_after_timeout() {
        let mut renderer = CpuRenderer::new();
        renderer
            .begin_page(&PageRenderInfo {
                page_rect: Rect::new(0.0, 0.0, 16.0, 16.0),
                scale: 1.0,
                background: Color::white(),
            })
            .unwrap();
        renderer
            .execute(&RenderCommand::PushBlendGroup {
                blend_mode: BlendMode::Screen,
                isolated: false,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 16.0, 16.0),
                alpha: 1.0,
                mask: None,
            })
            .unwrap();
        let mut rect = Path::new();
        rect.rect(Rect::new(0.0, 0.0, 8.0, 8.0));
        renderer
            .execute(&RenderCommand::FillPath {
                path: rect,
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::black()),
                alpha: 1.0,
                overprint: None,
            })
            .unwrap();
        assert_eq!(
            renderer
                .recording_non_isolated
                .as_ref()
                .expect("recording active")
                .cmds
                .len(),
            1
        );

        // Expire the page deadline and execute a glyph run; previously saved
        // draws must be released and the glyph run must not be cloned into cmds.
        renderer.deadline = Some(zpdf_core::time::Instant::now());
        renderer.over_budget = true;
        let glyph_run = GlyphRun {
            font_id: 0,
            font_size: 12.0,
            glyphs: vec![zpdf_display_list::PositionedGlyph {
                glyph_id: 1,
                code_len: 1,
                code: 65,
                x: 1.0,
                y: 1.0,
                advance: 6.0,
            }],
            transform: zpdf_core::Matrix::identity(),
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
            h_scale: 1.0,
        };
        renderer
            .execute(&RenderCommand::DrawGlyphRun(glyph_run))
            .unwrap();
        let rec = renderer
            .recording_non_isolated
            .as_ref()
            .expect("still tracking nesting until matching pop");
        assert!(
            rec.cmds.is_empty() && rec.cmds.capacity() == 0,
            "saved draws must be released once deadline fires (len={}, cap={})",
            rec.cmds.len(),
            rec.cmds.capacity()
        );
        renderer.execute(&RenderCommand::PopBlendGroup).unwrap();
        assert!(renderer.recording_non_isolated.is_none());
        assert!(renderer.end_page().is_ok());
    }

    #[test]
    fn document_soft_mask_cache_limit_is_enforced() {
        let limits = ParseLimits {
            max_softmask_cache_bytes: 3,
            ..ParseLimits::default()
        };
        let mut renderer = CpuRenderer::new().with_limits(&limits);
        renderer
            .begin_page(&PageRenderInfo {
                page_rect: Rect::new(0.0, 0.0, 2.0, 2.0),
                scale: 1.0,
                background: Color::white(),
            })
            .unwrap();
        let mask = SoftMask {
            kind: SoftMaskKind::Alpha,
            commands: Arc::new(DisplayList::new(Rect::new(0.0, 0.0, 2.0, 2.0))),
            offset: (0.0, 0.0),
            backdrop_luma: 0.0,
            transfer: None,
        };
        assert_eq!(renderer.soft_mask_plane(&mask).unwrap().len(), 4);
        assert!(renderer.soft_mask_planes.is_empty());
        assert_eq!(renderer.soft_mask_cache_bytes, 0);
    }

    #[test]
    fn end_page_transfers_pixels_without_a_full_raster_copy() {
        let mut renderer = CpuRenderer::new();
        renderer
            .begin_page(&PageRenderInfo {
                page_rect: Rect::new(0.0, 0.0, 8.0, 8.0),
                scale: 1.0,
                background: Color::white(),
            })
            .unwrap();
        let before = renderer.pixmap.as_ref().unwrap().data().as_ptr();
        let page = renderer.end_page().unwrap();
        assert_eq!(
            before,
            page.data.as_ptr(),
            "Pixmap allocation should be moved"
        );
    }

    #[test]
    fn malformed_image_is_ignored_without_panicking() {
        let mut images = ImageCache::new();
        let image_id = images.insert(DecodedImage {
            width: 10,
            height: 10,
            data: vec![0; 3],
            has_alpha: false,
            is_image_mask: false,
            premultiplied: false,
        });
        let mut renderer = CpuRenderer::new().with_images(&images);
        renderer
            .begin_page(&PageRenderInfo {
                page_rect: Rect::new(0.0, 0.0, 10.0, 10.0),
                scale: 1.0,
                background: Color::white(),
            })
            .unwrap();
        renderer
            .execute(&RenderCommand::DrawImage(ImageDraw {
                image_id,
                transform: Matrix::identity(),
                alpha: 1.0,
                is_image_mask: false,
            }))
            .unwrap();
        let page = renderer.end_page().unwrap();
        assert_eq!(px(&page, 5, 5), [255, 255, 255, 255]);
    }

    #[test]
    fn repeated_image_minification_reuses_box_filter_result() {
        let mut images = ImageCache::new();
        let image_id = images.insert(DecodedImage {
            width: 100,
            height: 100,
            data: vec![255; 100 * 100 * 4],
            has_alpha: false,
            is_image_mask: false,
            premultiplied: false,
        });
        let mut renderer = CpuRenderer::new().with_images(&images);
        renderer
            .begin_page(&PageRenderInfo {
                page_rect: Rect::new(0.0, 0.0, 10.0, 10.0),
                scale: 1.0,
                background: Color::white(),
            })
            .unwrap();
        let draw = RenderCommand::DrawImage(ImageDraw {
            image_id,
            transform: Matrix::identity(),
            alpha: 1.0,
            is_image_mask: false,
        });
        renderer.execute(&draw).unwrap();
        renderer.execute(&draw).unwrap();
        assert_eq!(renderer.downscaled_images.len(), 1);
    }

    #[test]
    fn clip_memory_budget_degrades_to_a_skipped_frame() {
        let mut renderer = CpuRenderer::new();
        renderer
            .begin_page(&PageRenderInfo {
                page_rect: Rect::new(0.0, 0.0, 10.0, 10.0),
                scale: 1.0,
                background: Color::white(),
            })
            .unwrap();
        renderer.clip_mask_bytes = MAX_CLIP_MASK_BYTES;
        let mut path = Path::new();
        path.rect(Rect::new(0.0, 0.0, 5.0, 5.0));
        renderer.push_clip(&path, &FillRule::NonZero);
        assert!(matches!(
            renderer.clip_stack.last(),
            Some(ClipFrame::Skipped)
        ));
    }

    fn render_with_clip_budget(
        dl: &DisplayList,
        scale: f32,
        initial_clip_mask_bytes: u64,
        initial_clip_pixel_spent: u64,
    ) -> RenderedPage {
        let mut renderer = CpuRenderer::new();
        renderer
            .begin_page(&PageRenderInfo {
                page_rect: dl.page_rect,
                scale,
                background: Color::white(),
            })
            .expect("begin_page");
        renderer.clip_mask_bytes = initial_clip_mask_bytes;
        renderer.clip_pixel_spent = initial_clip_pixel_spent;
        for cmd in &dl.commands {
            renderer.execute(cmd).expect("execute");
        }
        renderer.end_page().expect("end_page")
    }

    #[test]
    fn clip_elision_preserves_clip_memory_and_pixel_work_budgets_for_later_clips() {
        use zpdf_content::interpreter::ContentInterpreter;

        let page = Rect::new(0.0, 0.0, 100.0, 100.0);
        // Outer full-page clip + redundant covering full-page clip + genuine
        // half-page clip (x = 0..50) + full-page red fill.
        let content =
            b"q 0 0 100 100 re W n 0 0 100 100 re W n 0 0 50 100 re W n 1 0 0 rg 0 0 100 100 re f Q";
        let retained_dl = ContentInterpreter::new(page).interpret(content);
        let elided_dl = ContentInterpreter::new(page)
            .with_clip_elision_scale(1.0)
            .interpret(content);

        let retained_clips = retained_dl
            .commands
            .iter()
            .filter(|c| matches!(c, RenderCommand::PushClip { .. }))
            .count();
        let elided_clips = elided_dl
            .commands
            .iter()
            .filter(|c| matches!(c, RenderCommand::PushClip { .. }))
            .count();
        assert_eq!(retained_clips, 3);
        assert_eq!(elided_clips, 2);

        // Within full budgets, retained and elided renders are byte-identical
        // and both enforce the half-page clip (left red, right white).
        let unconstrained_retained = render_with_clip_budget(&retained_dl, 1.0, 0, 0);
        let unconstrained_elided = render_with_clip_budget(&elided_dl, 1.0, 0, 0);
        assert_eq!(unconstrained_elided.data, unconstrained_retained.data);
        assert_eq!(px(&unconstrained_elided, 25, 50), [255, 0, 0, 255]);
        assert_eq!(px(&unconstrained_elided, 75, 50), [255, 255, 255, 255]);

        // Mask-memory budget boundary (analogous to 8000x8000 where 2 live
        // masks fit in MAX_CLIP_MASK_BYTES and a 3rd does not): 100x100 =
        // 10_000 bytes/mask, so leaving 20_000 bytes lets 2 live masks succeed.
        // Retained spends both on clips 1 & 2 and skips clip 3 (overdrawing
        // the right half); elided avoids allocating clip 2, enforces clip 3,
        // and matches the unconstrained PDF-spec render byte-for-byte.
        let mask_bytes = 100 * 100;
        let mem_retained =
            render_with_clip_budget(&retained_dl, 1.0, MAX_CLIP_MASK_BYTES - 2 * mask_bytes, 0);
        let mem_elided =
            render_with_clip_budget(&elided_dl, 1.0, MAX_CLIP_MASK_BYTES - 2 * mask_bytes, 0);
        assert_eq!(px(&mem_retained, 75, 50), [255, 0, 0, 255]);
        assert_eq!(px(&mem_elided, 25, 50), [255, 0, 0, 255]);
        assert_eq!(px(&mem_elided, 75, 50), [255, 255, 255, 255]);
        assert_eq!(mem_elided.data, unconstrained_retained.data);

        // Cumulative pixel-work budget boundary: each new full-page clip
        // charges 10_000 (mask zero/alloc) + 10_000 (fill extent) = 20_000 px.
        // Leaving 40_000 px exhausts MAX_CLIP_PIXEL_WORK after clips 1 & 2 in
        // the retained stream so clip 3 is skipped, while eliding clip 2 leaves
        // room for clip 3 and matches the unconstrained render byte-for-byte.
        let full_clip_work = 2 * mask_bytes;
        let work_retained = render_with_clip_budget(
            &retained_dl,
            1.0,
            0,
            MAX_CLIP_PIXEL_WORK - 2 * full_clip_work,
        );
        let work_elided =
            render_with_clip_budget(&elided_dl, 1.0, 0, MAX_CLIP_PIXEL_WORK - 2 * full_clip_work);
        assert_eq!(px(&work_retained, 75, 50), [255, 0, 0, 255]);
        assert_eq!(px(&work_elided, 25, 50), [255, 0, 0, 255]);
        assert_eq!(px(&work_elided, 75, 50), [255, 255, 255, 255]);
        assert_eq!(work_elided.data, unconstrained_retained.data);

        // Once the elided stream itself exhausts either budget on clip 2
        // (`0 0 50 100`), a later nested clip covering clip 2 (`0 0 60 100`)
        // would also be skipped in the retained stream, so both streams stay
        // byte-identical (elision never drops a clip that retained enforces).
        let post_budget_content =
            b"q 0 0 100 100 re W n 0 0 50 100 re W n 0 0 60 100 re W n 1 0 0 rg 0 0 100 100 re f Q";
        let post_retained_dl = ContentInterpreter::new(page).interpret(post_budget_content);
        let post_elided_dl = ContentInterpreter::new(page)
            .with_clip_elision_scale(1.0)
            .interpret(post_budget_content);

        let post_mem_retained =
            render_with_clip_budget(&post_retained_dl, 1.0, MAX_CLIP_MASK_BYTES - mask_bytes, 0);
        let post_mem_elided =
            render_with_clip_budget(&post_elided_dl, 1.0, MAX_CLIP_MASK_BYTES - mask_bytes, 0);
        assert_eq!(post_mem_elided.data, post_mem_retained.data);

        let post_work_retained = render_with_clip_budget(
            &post_retained_dl,
            1.0,
            0,
            MAX_CLIP_PIXEL_WORK - full_clip_work,
        );
        let post_work_elided = render_with_clip_budget(
            &post_elided_dl,
            1.0,
            0,
            MAX_CLIP_PIXEL_WORK - full_clip_work,
        );
        assert_eq!(post_work_elided.data, post_work_retained.data);
    }

    #[test]
    fn budgeted_renderer_still_unwinds_an_open_blend_group() {
        let mut renderer = CpuRenderer::new();
        renderer
            .begin_page(&PageRenderInfo {
                page_rect: Rect::new(0.0, 0.0, 10.0, 10.0),
                scale: 1.0,
                background: Color::white(),
            })
            .unwrap();
        renderer
            .execute(&RenderCommand::PushBlendGroup {
                blend_mode: BlendMode::Normal,
                isolated: true,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
                alpha: 1.0,
                mask: None,
            })
            .unwrap();
        renderer.over_budget = true;
        renderer.execute(&RenderCommand::PopBlendGroup).unwrap();
        assert!(renderer.blend_stack.is_empty());
        assert!(renderer.end_page().is_ok());
    }

    fn stroke_cmd(path: Path, style: StrokeStyle) -> RenderCommand {
        RenderCommand::StrokePath {
            path,
            style,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        }
    }

    fn render(dl: &DisplayList, scale: f32) -> RenderedPage {
        CpuRenderer::new()
            .render_display_list(dl, scale)
            .expect("render")
    }

    #[test]
    fn hairline_stroke_clamps_to_one_device_pixel() {
        // 0.05pt stroke at scale 1 → clamped to 1px. Centered at device y=10.5,
        // it covers row 10 fully; without the clamp the row stays ~95% white.
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        let style = StrokeStyle {
            width: 0.05,
            ..Default::default()
        };
        dl.push(stroke_cmd(line_path(2.0, 9.5, 18.0, 9.5), style));
        let page = render(&dl, 1.0);
        let p = px(&page, 10, 10);
        assert!(p[0] < 60, "hairline row should be dark, got {p:?}");
    }

    #[test]
    fn dash_pattern_produces_gaps() {
        // [4 on, 4 off] along y=9.5 (device row 10): on [0,4), [8,12), [16,20).
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        let style = StrokeStyle {
            width: 2.0,
            dash: Some(DashPattern {
                array: vec![4.0, 4.0],
                phase: 0.0,
            }),
            ..Default::default()
        };
        dl.push(stroke_cmd(line_path(0.0, 9.5, 20.0, 9.5), style));
        let page = render(&dl, 1.0);
        let on = px(&page, 2, 10);
        let off = px(&page, 6, 10);
        assert!(on[0] < 60, "dash 'on' segment should be dark, got {on:?}");
        assert!(
            off[0] > 200,
            "dash 'off' gap should stay white, got {off:?}"
        );
    }

    #[test]
    fn odd_dash_array_repeats_like_doubled() {
        // PDF [4] == 4 on / 4 off (tiny-skia needs the doubled even array).
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        let style = StrokeStyle {
            width: 2.0,
            dash: Some(DashPattern {
                array: vec![4.0],
                phase: 0.0,
            }),
            ..Default::default()
        };
        dl.push(stroke_cmd(line_path(0.0, 9.5, 20.0, 9.5), style));
        let page = render(&dl, 1.0);
        assert!(px(&page, 2, 10)[0] < 60);
        assert!(px(&page, 6, 10)[0] > 200);
    }

    #[test]
    fn degenerate_dash_strokes_solid() {
        // All-zero array is invalid → solid stroke, no gaps.
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        let style = StrokeStyle {
            width: 2.0,
            dash: Some(DashPattern {
                array: vec![0.0, 0.0],
                phase: 0.0,
            }),
            ..Default::default()
        };
        dl.push(stroke_cmd(line_path(0.0, 9.5, 20.0, 9.5), style));
        let page = render(&dl, 1.0);
        assert!(px(&page, 6, 10)[0] < 60, "degenerate dash must draw solid");
    }

    #[test]
    fn raster_dims_use_ceil() {
        // 595x842pt (A4) at 110 DPI: 909.03x1286.6 → 910x1287 (pdfium parity).
        let dl = DisplayList::new(Rect::new(0.0, 0.0, 595.0, 842.0));
        let page = render(&dl, 110.0 / 72.0);
        assert_eq!((page.width, page.height), (910, 1287));

        // Exact integer sizes are unchanged by ceil().
        let dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 50.0));
        let page = render(&dl, 2.0);
        assert_eq!((page.width, page.height), (200, 100));
    }

    #[test]
    fn page_rect_origin_offsets_geometry() {
        // CropBox-style rect (100,50)-(120,70): raster is 20x20 and content is
        // positioned relative to the rect origin.
        let mut dl = DisplayList::new(Rect::new(100.0, 50.0, 120.0, 70.0));
        let mut path = Path::new();
        path.rect(Rect::new(105.0, 55.0, 115.0, 65.0));
        dl.push(RenderCommand::FillPath {
            path,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        let page = render(&dl, 1.0);
        assert_eq!((page.width, page.height), (20, 20));
        let center = px(&page, 10, 10);
        assert!(
            center[0] > 200 && center[1] < 60,
            "center red, got {center:?}"
        );
        let corner = px(&page, 2, 2);
        assert!(
            corner[0] > 200 && corner[1] > 200,
            "corner white, got {corner:?}"
        );
    }

    #[test]
    fn clip_mask_is_antialiased() {
        // Clip to the lower-left triangle, fill black: the diagonal edge passes
        // through pixel (10,10) at 50% coverage — AA must leave it mid-gray.
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        let mut tri = Path::new();
        tri.move_to(Point::new(0.0, 0.0));
        tri.line_to(Point::new(20.0, 0.0));
        tri.line_to(Point::new(0.0, 20.0));
        tri.close();
        dl.push(RenderCommand::PushClip {
            path: tri,
            rule: FillRule::NonZero,
        });
        let mut full = Path::new();
        full.rect(Rect::new(0.0, 0.0, 20.0, 20.0));
        dl.push(RenderCommand::FillPath {
            path: full,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PopClip);
        let page = render(&dl, 1.0);
        let edge = px(&page, 10, 10);
        assert!(
            edge[0] > 30 && edge[0] < 225,
            "clip edge should be AA gray, got {edge:?}"
        );
    }

    #[test]
    fn image_upscale_is_bilinear() {
        // 2x2 black/white checker scaled to 20x20: bilinear leaves the center
        // mid-gray; nearest would snap to pure black/white.
        let mut images = ImageCache::new();
        #[rustfmt::skip]
        let data = vec![
            0, 0, 0, 255,        255, 255, 255, 255,
            255, 255, 255, 255,  0, 0, 0, 255,
        ];
        let id = images.insert(DecodedImage {
            width: 2,
            height: 2,
            data,
            has_alpha: false,
            is_image_mask: false,
            premultiplied: true,
        });
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        dl.push(RenderCommand::DrawImage(ImageDraw {
            image_id: id,
            transform: Matrix::new(20.0, 0.0, 0.0, 20.0, 0.0, 0.0),
            alpha: 1.0,
            is_image_mask: false,
        }));
        let page = CpuRenderer::new()
            .with_images(&images)
            .render_display_list(&dl, 1.0)
            .expect("render");
        let center = px(&page, 10, 10);
        assert!(
            center[0] > 60 && center[0] < 200,
            "center should interpolate to gray, got {center:?}"
        );
    }

    #[test]
    fn strong_minification_box_filters() {
        // 16x16 image of alternating 1px black/white columns drawn into 4x4
        // device pixels (0.25x): the box pre-downscale averages every column to
        // ~50% gray; nearest/bilinear at that ratio would skip columns entirely.
        let mut images = ImageCache::new();
        let mut data = Vec::with_capacity(16 * 16 * 4);
        for _y in 0..16 {
            for x in 0..16u32 {
                let v = if x % 2 == 0 { 0u8 } else { 255u8 };
                data.extend_from_slice(&[v, v, v, 255]);
            }
        }
        let id = images.insert(DecodedImage {
            width: 16,
            height: 16,
            data,
            has_alpha: false,
            is_image_mask: false,
            premultiplied: true,
        });
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 8.0, 8.0));
        dl.push(RenderCommand::DrawImage(ImageDraw {
            image_id: id,
            transform: Matrix::new(4.0, 0.0, 0.0, 4.0, 2.0, 2.0),
            alpha: 1.0,
            is_image_mask: false,
        }));
        let page = CpuRenderer::new()
            .with_images(&images)
            .render_display_list(&dl, 1.0)
            .expect("render");
        let inside = px(&page, 4, 4);
        assert!(
            inside[0] > 60 && inside[0] < 200,
            "minified stripes should average to gray, got {inside:?}"
        );
    }

    #[test]
    fn box_downscale_averages_blocks() {
        // 4x2 → 2x1: each output pixel averages a 2x2 block.
        let data = vec![
            0, 0, 0, 255, 255, 255, 255, 255, 100, 100, 100, 255, 200, 200, 200, 255, //
            0, 0, 0, 255, 255, 255, 255, 255, 100, 100, 100, 255, 200, 200, 200, 255,
        ];
        let out = zpdf_image::box_downscale_rgba(&data, 4, 2, 2, 1);
        assert_eq!(out.len(), 8);
        assert_eq!(out[0], 128); // round((0+255+0+255)/4)
        assert_eq!(out[4], 150); // (100+200+100+200)/4
        assert_eq!(out[3], 255);
    }

    #[test]
    fn shift_plane_right_down() {
        // 3x3 with a marker at (0,0); shift +1,+1 moves it to (1,1).
        let mut base = vec![10u8; 9];
        base[0] = 200;
        let out = shift_plane(&base, 3, 3, 1, 1, 7);
        assert_eq!(out[3 + 1], 200);
        // Vacated top row and left column take the fill value.
        assert_eq!(&out[0..3], &[7, 7, 7]);
        assert_eq!(out[3], 7);
        assert_eq!(out[6], 7);
        // Copied region keeps base values.
        assert_eq!(out[5], 10);
    }

    #[test]
    fn shift_plane_left_up() {
        let mut base = vec![10u8; 9];
        base[2 * 3 + 2] = 200; // marker at (2,2)
        let out = shift_plane(&base, 3, 3, -1, -1, 7);
        assert_eq!(out[3 + 1], 200);
        // Vacated bottom row and right column take the fill value.
        assert_eq!(&out[6..9], &[7, 7, 7]);
        assert_eq!(out[2], 7);
        assert_eq!(out[5], 7);
    }

    #[test]
    fn shift_plane_out_of_range_is_all_fill() {
        let base = vec![10u8; 9];
        assert_eq!(shift_plane(&base, 3, 3, 3, 0, 7), vec![7u8; 9]);
        assert_eq!(shift_plane(&base, 3, 3, 0, -3, 7), vec![7u8; 9]);
    }

    #[test]
    fn unpainted_value_modes() {
        let mask = |kind, backdrop_luma, transfer| SoftMask {
            kind,
            commands: std::sync::Arc::new(DisplayList::new(Rect::new(0.0, 0.0, 1.0, 1.0))),
            offset: (0.0, 0.0),
            backdrop_luma,
            transfer,
        };
        assert_eq!(unpainted_value(&mask(SoftMaskKind::Alpha, 0.5, None)), 0);
        assert_eq!(
            unpainted_value(&mask(SoftMaskKind::Luminosity, 0.5, None)),
            128
        );
        let inverting = std::sync::Arc::new(std::array::from_fn(|i| 255 - i as u8));
        assert_eq!(
            unpainted_value(&mask(SoftMaskKind::Alpha, 0.0, Some(inverting))),
            255
        );
    }

    // -- Opt-in stage timing (`StageStats`) --------------------------------
    //
    // The point of these tests is that stage timing cannot silently rot the way
    // the previous round's env-var probes did (they were deleted after the
    // investigation, so nothing caught their drift). Counters are asserted
    // exactly; wall-clock buckets are asserted non-zero on targets that have a
    // clock.

    /// zpdf-font's own fixture: glyph 1 is a rectangle, so it always yields a
    /// real outline to extract and rasterize.
    const VAR_TTF: &[u8] = include_bytes!("../../zpdf-font/tests/fixtures/var.ttf");

    fn var_fonts() -> (FontCache, zpdf_font::FontId) {
        let mut fonts = FontCache::new();
        let font = zpdf_font::LoadedFont::new_with_data(
            zpdf_font::PdfFontType::TrueType,
            "ZpdfVar".into(),
            VAR_TTF.to_vec(),
            zpdf_font::CidWidths::new(1000.0),
        );
        let id = fonts.insert("ZpdfVar".into(), font);
        (fonts, id)
    }

    /// A Type3 font whose glyph 1 is a single filled square, drawn by a content
    /// stream. Type3 glyph streams are interpreted *inside* the renderer, so they
    /// are the one content family whose painted extent is not obvious from the
    /// display list.
    fn type3_fonts() -> (FontCache, zpdf_font::FontId) {
        let mut char_procs: HashMap<String, Arc<[u8]>> = HashMap::new();
        // A 600×600 square in glyph space (the FontMatrix scales by 1/1000).
        char_procs.insert("g1".to_string(), Arc::from(&b"100 100 600 600 re f\n"[..]));
        let font = zpdf_font::LoadedFont::new_with_data(
            zpdf_font::PdfFontType::Type3 {
                font_matrix: [0.001, 0.0, 0.0, 0.001, 0.0, 0.0],
                char_procs,
                encoding: vec!["g0".into(), "g1".into()],
                widths: vec![1000.0, 1000.0],
                first_char: 0,
            },
            "ZpdfType3".into(),
            Vec::new(),
            zpdf_font::CidWidths::new(1000.0),
        );
        assert!(font.is_type3(), "the fixture must reach the Type3 path");
        let mut fonts = FontCache::new();
        let id = fonts.insert("ZpdfType3".into(), font);
        (fonts, id)
    }

    fn type3_image_fonts(stream: &[u8]) -> (FontCache, zpdf_font::FontId) {
        let mut char_procs: HashMap<String, Arc<[u8]>> = HashMap::new();
        char_procs.insert("g1".to_string(), Arc::from(stream));
        let font = zpdf_font::LoadedFont::new_with_data(
            zpdf_font::PdfFontType::Type3 {
                font_matrix: [0.001, 0.0, 0.0, 0.001, 0.0, 0.0],
                char_procs,
                encoding: vec!["g0".into(), "g1".into()],
                widths: vec![1000.0, 1000.0],
                first_char: 0,
            },
            "ZpdfType3Image".into(),
            Vec::new(),
            zpdf_font::CidWidths::new(1000.0),
        );
        let mut fonts = FontCache::new();
        let id = fonts.insert("ZpdfType3Image".into(), font);
        (fonts, id)
    }

    /// A Type 3 glyph run of `count` glyphs 1 under text matrix `tm`, 12pt.
    fn type3_run(font_id: zpdf_font::FontId, count: usize, tm: Matrix) -> RenderCommand {
        RenderCommand::DrawGlyphRun(GlyphRun {
            font_id,
            font_size: 12.0,
            glyphs: (0..count)
                .map(|k| PositionedGlyph {
                    glyph_id: 1,
                    code_len: 0,
                    code: 0,
                    x: k as f32 * 12.0,
                    y: 0.0,
                    advance: 12.0,
                })
                .collect(),
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
            transform: tm,
            h_scale: 1.0,
        })
    }

    #[test]
    fn repeated_type3_glyphs_are_interpreted_once_per_page() {
        let (fonts, font_id) = type3_fonts();
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 20.0));
        dl.push(type3_run(
            font_id,
            5,
            Matrix::new(1.0, 0.0, 0.0, 1.0, 2.0, 2.0),
        ));
        let mut renderer = CpuRenderer::new().with_fonts(&fonts);
        let page = renderer.render_display_list(&dl, 1.0).expect("render");
        assert_eq!(renderer.type3_glyphs.len(), 1);
        // Every instance still paints: the last glyph's square at x ≈ 50..57.
        let ink = |x: u32| page.data[((14 * page.width + x) * 4) as usize] < 128;
        assert!(ink(4) && ink(53));
    }

    #[test]
    fn type3_stroke_width_follows_the_text_matrix() {
        // A 100-unit stroke in glyph space: 1.2 user units at 12pt, 12 under a
        // 10x text matrix.
        let (fonts, font_id) = type3_image_fonts(b"100 w 0 500 m 1000 500 l S");
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 200.0, 200.0));
        dl.push(type3_run(
            font_id,
            1,
            Matrix::new(10.0, 0.0, 0.0, 10.0, 20.0, 20.0),
        ));
        let page = render_backend(&dl, 1.0, Some(&fonts), None, false).0;
        // The stroke runs along page y = 20 + 60, device row 120; count its rows.
        let rows = (0..page.height)
            .filter(|&y| page.data[((y * page.width + 80) * 4) as usize] < 128)
            .count();
        assert!((11..=14).contains(&rows), "stroke covers {rows} rows");
    }

    fn rect_path(x0: f64, y0: f64, x1: f64, y1: f64) -> Path {
        let mut p = Path::new();
        p.move_to(Point::new(x0, y0));
        p.line_to(Point::new(x1, y0));
        p.line_to(Point::new(x1, y1));
        p.line_to(Point::new(x0, y1));
        p.close();
        p
    }

    fn fill_cmd(x0: f64, y0: f64, x1: f64, y1: f64) -> RenderCommand {
        RenderCommand::FillPath {
            path: rect_path(x0, y0, x1, y1),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        }
    }

    fn glyph_run_cmd(font_id: zpdf_font::FontId) -> RenderCommand {
        RenderCommand::DrawGlyphRun(GlyphRun {
            font_id,
            font_size: 12.0,
            glyphs: vec![PositionedGlyph {
                glyph_id: 1,
                code_len: 0,
                code: 0,
                x: 0.0,
                y: 0.0,
                advance: 12.0,
            }],
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
            transform: Matrix::new(1.0, 0.0, 0.0, 1.0, 2.0, 2.0),
            h_scale: 1.0,
        })
    }

    #[test]
    fn type3_glyph_images_use_document_pixel_limits() {
        let glyph_stream = b"BI /W 8 /H 8 /BPC 1 /IM true ID \0\0\0\0\0\0\0\0 EI";
        let (fonts, font_id) = type3_image_fonts(glyph_stream);
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        dl.push(glyph_run_cmd(font_id));

        let limits = ParseLimits {
            max_image_pixels: 1,
            ..ParseLimits::default()
        };
        let page = CpuRenderer::new()
            .with_fonts(&fonts)
            .with_limits(&limits)
            .render_display_list(&dl, 1.0)
            .expect("render");
        let (pixels, remainder) = page.data.as_chunks::<4>();
        assert!(
            remainder.is_empty() && pixels.iter().all(|pixel| *pixel == [255, 255, 255, 255]),
            "the nested Type3 image must honor max_image_pixels"
        );
    }

    #[test]
    fn type3_bitmap_glyph_renders_under_normal_and_multiply_tiling_patterns() {
        // 8×8 checkerboard/solid 1-bit `/ImageMask true` glyph: top 4 rows ink (0x00 in
        // 1-bit image mask means painted sample; 0xFF means transparent), bottom 4 rows empty.
        let glyph_stream =
            b"q 1000 0 0 1000 0 0 cm BI /W 8 /H 8 /BPC 1 /IM true ID \x00\x00\x00\x00\xff\xff\xff\xff EI Q";
        let (fonts, font_id) = type3_image_fonts(glyph_stream);

        let make_tiling = |color: Color, mode: BlendMode| {
            let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 4.0, 4.0));
            let mut sq = Path::new();
            sq.rect(Rect::new(0.0, 0.0, 4.0, 4.0));
            if mode != BlendMode::Normal {
                cell.push(RenderCommand::PushBlendGroup {
                    blend_mode: mode,
                    isolated: true,
                    knockout: false,
                    bounds: Rect::new(0.0, 0.0, 4.0, 4.0),
                    alpha: 1.0,
                    mask: None,
                });
            }
            cell.push(RenderCommand::FillPath {
                path: sq,
                rule: FillRule::NonZero,
                paint: Paint::Solid(color),
                alpha: 1.0,
                overprint: None,
            });
            if mode != BlendMode::Normal {
                cell.push(RenderCommand::PopBlendGroup);
            }
            Arc::new(TilingPattern {
                commands: Arc::new(cell),
                bbox: Rect::new(0.0, 0.0, 4.0, 4.0),
                x_step: 4.0,
                y_step: 4.0,
                matrix: Matrix::identity(),
                tiling_type: 1,
            })
        };

        // 1) Single-segment black Multiply tile at full opacity (fast single-pass path).
        let mul_tile = make_tiling(Color::black(), BlendMode::Multiply);
        let mut dl_mul = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        dl_mul.push(RenderCommand::DrawGlyphRun(GlyphRun {
            font_id,
            font_size: 12.0,
            glyphs: vec![PositionedGlyph {
                glyph_id: 1,
                code_len: 1,
                code: b'A' as u32,
                x: 0.0,
                y: 0.0,
                advance: 12.0,
            }],
            paint: Paint::Tiling(mul_tile),
            alpha: 1.0,
            overprint: None,
            transform: Matrix::new(1.0, 0.0, 0.0, 1.0, 2.0, 2.0),
            h_scale: 1.0,
        }));
        let mul_page = CpuRenderer::new()
            .with_fonts(&fonts)
            .render_display_list(&dl_mul, 1.0)
            .expect("render multiply bitmap Type3");
        // Glyph spans PDF x=[2..14], y=[2..14] -> pixel x=[2..14], y=[6..18].
        // Top half of the 8×8 mask (pixel y=[6..12]) must be painted black;
        // bottom half (pixel y=[13..18]) must stay white.
        let top_mul = px(&mul_page, 8, 8);
        let bot_mul = px(&mul_page, 8, 15);
        assert!(
            top_mul[0] < 10 && top_mul[1] < 10 && top_mul[2] < 10,
            "full-opacity Multiply tiling on bitmap Type 3 glyph must paint black silhouette, got {top_mul:?}"
        );
        assert_eq!(
            bot_mul,
            [255, 255, 255, 255],
            "unmasked bottom half of bitmap Type 3 glyph must remain white"
        );

        // 2) Plain Normal red tile at full opacity (regular pattern shader path).
        let norm_tile = make_tiling(Color::rgb(1.0, 0.0, 0.0), BlendMode::Normal);
        let mut dl_norm = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        dl_norm.push(RenderCommand::DrawGlyphRun(GlyphRun {
            font_id,
            font_size: 12.0,
            glyphs: vec![PositionedGlyph {
                glyph_id: 1,
                code_len: 1,
                code: b'A' as u32,
                x: 0.0,
                y: 0.0,
                advance: 12.0,
            }],
            paint: Paint::Tiling(norm_tile),
            alpha: 1.0,
            overprint: None,
            transform: Matrix::new(1.0, 0.0, 0.0, 1.0, 2.0, 2.0),
            h_scale: 1.0,
        }));
        let norm_page = CpuRenderer::new()
            .with_fonts(&fonts)
            .render_display_list(&dl_norm, 1.0)
            .expect("render normal bitmap Type3");
        let top_norm = px(&norm_page, 8, 8);
        let bot_norm = px(&norm_page, 8, 15);
        assert!(
            top_norm[0] > 245 && top_norm[1] < 10 && top_norm[2] < 10,
            "Normal red tiling on bitmap Type 3 glyph must paint red silhouette, got {top_norm:?}"
        );
        assert_eq!(
            bot_norm,
            [255, 255, 255, 255],
            "unmasked bottom half under Normal tiling must remain white"
        );
    }

    #[test]
    fn type3_non_mask_transparency_keeps_source_colour() {
        let mut images = ImageCache::new();
        let image_id = images.insert(DecodedImage {
            width: 1,
            height: 1,
            data: vec![0, 128, 0, 128],
            has_alpha: true,
            is_image_mask: false,
            premultiplied: true,
        });
        let draw = ImageDraw {
            image_id,
            transform: Matrix::identity(),
            alpha: 1.0,
            is_image_mask: false,
        };
        let paint = CpuRenderer::color_to_paint(&Color::rgb(1.0, 0.0, 0.0), 1.0);
        let mut renderer = CpuRenderer::new().with_images(&images);
        renderer
            .begin_page(&PageRenderInfo {
                page_rect: Rect::new(0.0, 0.0, 1.0, 1.0),
                scale: 1.0,
                background: Color::white(),
            })
            .unwrap();
        renderer.render_type3_image(&images, &draw, &paint, 1.0);
        let pixel = px(&renderer.end_page().unwrap(), 0, 0);
        assert!(
            pixel[1] > pixel[0],
            "source green must not be tinted red: {pixel:?}"
        );
    }

    #[test]
    fn type3_mask_combines_image_and_outer_alpha() {
        let mut images = ImageCache::new();
        let image_id = images.insert(DecodedImage {
            width: 1,
            height: 1,
            data: vec![0, 0, 0, 255],
            has_alpha: true,
            is_image_mask: true,
            premultiplied: true,
        });
        let draw = ImageDraw {
            image_id,
            transform: Matrix::identity(),
            alpha: 0.5,
            is_image_mask: true,
        };
        let paint = CpuRenderer::color_to_paint(&Color::rgb(1.0, 0.0, 0.0), 1.0);
        let mut renderer = CpuRenderer::new().with_images(&images);
        renderer
            .begin_page(&PageRenderInfo {
                page_rect: Rect::new(0.0, 0.0, 1.0, 1.0),
                scale: 1.0,
                background: Color::white(),
            })
            .unwrap();
        renderer.render_type3_image(&images, &draw, &paint, 0.5);
        let pixel = px(&renderer.end_page().unwrap(), 0, 0);
        assert!(
            (180..=205).contains(&pixel[1]),
            "both alpha values must be applied: {pixel:?}"
        );
    }

    /// One page exercising every command family, rendered with timing enabled.
    fn mixed_page_stats() -> zpdf_render::StageStats {
        let (fonts, font_id) = var_fonts();
        let mut images = ImageCache::new();
        let image_id = images.insert(DecodedImage {
            width: 4,
            height: 4,
            data: vec![128; 4 * 4 * 4],
            has_alpha: false,
            is_image_mask: false,
            premultiplied: true,
        });

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 40.0, 40.0));
        dl.push(fill_cmd(2.0, 2.0, 12.0, 12.0));
        dl.push(fill_cmd(14.0, 2.0, 24.0, 12.0));
        dl.push(stroke_cmd(
            line_path(2.0, 20.0, 30.0, 20.0),
            StrokeStyle {
                width: 1.0,
                ..Default::default()
            },
        ));
        dl.push(RenderCommand::PushClip {
            path: rect_path(0.0, 0.0, 30.0, 30.0),
            rule: FillRule::NonZero,
        });
        dl.push(glyph_run_cmd(font_id));
        dl.push(RenderCommand::DrawImage(ImageDraw {
            image_id,
            transform: Matrix::new(10.0, 0.0, 0.0, 10.0, 2.0, 30.0),
            alpha: 1.0,
            is_image_mask: false,
        }));
        dl.push(RenderCommand::PopClip);

        let mut renderer = CpuRenderer::new()
            .with_fonts(&fonts)
            .with_images(&images)
            .with_stage_timing(true);
        renderer.render_display_list(&dl, 1.0).expect("render");
        renderer.stage_stats().expect("timing was enabled")
    }

    #[test]
    fn stage_timing_is_off_by_default() {
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        dl.push(fill_cmd(2.0, 2.0, 10.0, 10.0));
        let mut renderer = CpuRenderer::new();
        renderer.render_display_list(&dl, 1.0).expect("render");
        assert_eq!(
            renderer.stage_stats(),
            None,
            "collection must be opt-in; the production path pays nothing"
        );
    }

    #[test]
    fn stage_timing_counts_each_command_family() {
        let s = mixed_page_stats();
        assert_eq!(s.fills, 2, "two fills");
        assert_eq!(s.strokes, 1, "one stroke");
        assert_eq!(s.images, 1, "one image");
        assert_eq!(s.clips_pushed, 1, "one clip push");
        assert_eq!(s.glyphs, 1, "one glyph instance");
        assert_eq!(
            s.glyph_outlines_parsed, 1,
            "an outline font extracts one outline per glyph instance"
        );
        assert!(s.total_ns > 0, "end_page must stamp total_ns");
        assert!(!s.is_empty());
    }

    /// The split the performance notes could only infer: extracting an outline
    /// from the font program vs rasterizing it. Both must land in their own
    /// bucket, and neither may swallow the other.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn stage_timing_buckets_are_non_zero() {
        let s = mixed_page_stats();
        assert!(s.fill_ns > 0, "fill bucket: {s:?}");
        assert!(s.stroke_ns > 0, "stroke bucket: {s:?}");
        assert!(s.image_ns > 0, "image bucket: {s:?}");
        assert!(s.clip_ns > 0, "clip bucket: {s:?}");
        assert!(s.outline_parse_ns > 0, "outline-parse bucket: {s:?}");
        assert!(s.glyph_raster_ns > 0, "glyph-raster bucket: {s:?}");
        assert_eq!(s.glyph_ns(), s.outline_parse_ns + s.glyph_raster_ns);
        // Buckets are attribution, not a partition: they may overlap and need
        // not sum to the page total, but each must be smaller than it.
        for (name, ns) in [
            ("fill", s.fill_ns),
            ("stroke", s.stroke_ns),
            ("image", s.image_ns),
            ("clip", s.clip_ns),
            ("glyph", s.glyph_ns()),
            ("soft_mask", s.soft_mask_ns),
        ] {
            assert!(
                ns <= s.total_ns,
                "{name} bucket {ns}ns exceeds total {}ns",
                s.total_ns
            );
        }
    }

    /// A page with no soft mask or blend group must report zero there — the
    /// negative control, so a non-zero bucket means real work rather than a
    /// mis-attributed neighbour.
    #[test]
    fn stage_timing_soft_mask_is_zero_without_transparency() {
        let s = mixed_page_stats();
        assert_eq!(s.soft_mask_ns, 0, "no transparency on this page");
        assert_eq!(s.soft_mask_planes, 0, "no masks rasterized");
    }

    #[test]
    fn repeated_glyphs_extract_their_outline_once_per_page() {
        let (fonts, font_id) = var_fonts();
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 40.0, 40.0));
        dl.push(glyph_run_cmd(font_id));
        dl.push(glyph_run_cmd(font_id));
        dl.push(glyph_run_cmd(font_id));
        let mut renderer = CpuRenderer::new()
            .with_fonts(&fonts)
            .with_stage_timing(true);
        let first = renderer.render_display_list(&dl, 1.0).expect("first page");
        let s = renderer.stage_stats().unwrap();
        assert_eq!(s.glyphs, 3);
        assert_eq!(s.glyph_outlines_parsed, 1);

        // The next page extracts it again, and draws the same pixels.
        let second = renderer.render_display_list(&dl, 1.0).expect("second page");
        assert_eq!(renderer.stage_stats().unwrap().glyph_outlines_parsed, 1);
        assert_eq!(first.data, second.data);
    }

    /// Stats describe exactly one page: a second page must not inherit the
    /// first page's counters.
    #[test]
    fn stage_timing_resets_between_pages() {
        let (fonts, font_id) = var_fonts();
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 40.0, 40.0));
        dl.push(glyph_run_cmd(font_id));
        let mut renderer = CpuRenderer::new()
            .with_fonts(&fonts)
            .with_stage_timing(true);

        renderer.render_display_list(&dl, 1.0).expect("first page");
        assert_eq!(renderer.stage_stats().unwrap().glyphs, 1);

        // A page with only a fill: the glyph counter must come back to zero.
        let mut dl2 = DisplayList::new(Rect::new(0.0, 0.0, 40.0, 40.0));
        dl2.push(fill_cmd(2.0, 2.0, 10.0, 10.0));
        renderer
            .render_display_list(&dl2, 1.0)
            .expect("second page");
        let s = renderer.stage_stats().unwrap();
        assert_eq!(
            s.glyphs, 0,
            "second page must not inherit page one's glyphs"
        );
        assert_eq!(s.fills, 1);
        assert_eq!(s.glyph_outlines_parsed, 0);
    }

    // --- scoped blend-group composites (performance notes §14.3) -------------

    /// Device pixels of a display list's page raster.
    fn raster_pixels(dl: &DisplayList, scale: f32) -> u64 {
        let w = (dl.page_rect.width() * scale as f64).ceil().max(1.0) as u64;
        let h = (dl.page_rect.height() * scale as f64).ceil().max(1.0) as u64;
        w * h
    }

    /// How many blend groups the display list opens.
    fn group_count(dl: &DisplayList) -> u64 {
        dl.commands
            .iter()
            .filter(|c| matches!(c, RenderCommand::PushBlendGroup { .. }))
            .count() as u64
    }

    fn solid_rect(color: Color, x0: f64, y0: f64, x1: f64, y1: f64) -> RenderCommand {
        RenderCommand::FillPath {
            path: rect_path(x0, y0, x1, y1),
            rule: FillRule::NonZero,
            paint: Paint::Solid(color),
            alpha: 1.0,
            overprint: None,
        }
    }

    fn group_cmd(
        mode: BlendMode,
        isolated: bool,
        alpha: f32,
        mask: Option<SoftMask>,
    ) -> RenderCommand {
        RenderCommand::PushBlendGroup {
            blend_mode: mode,
            isolated,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
            alpha,
            mask,
        }
    }

    /// A fully transparent fill of the whole page.
    ///
    /// It paints nothing — a zero-alpha source leaves the destination unchanged
    /// under every blend mode — but it *is* a real draw, so a correct
    /// painted-region tracker has to widen the group's region to the whole
    /// surface. That is the lever the equivalence tests below turn: the same
    /// scene with and without this command composites the group over the whole
    /// raster and over its content alone, and the two must agree byte for byte.
    fn invisible_page_fill(dl: &DisplayList) -> RenderCommand {
        solid_rect(
            Color::rgba(0.0, 0.0, 0.0, 0.0),
            dl.page_rect.x0,
            dl.page_rect.y0,
            dl.page_rect.x1,
            dl.page_rect.y1,
        )
    }

    fn render_backend(
        dl: &DisplayList,
        scale: f32,
        fonts: Option<&FontCache>,
        images: Option<&ImageCache>,
        timing: bool,
    ) -> (RenderedPage, Option<zpdf_render::StageStats>) {
        let mut renderer = CpuRenderer::new().with_stage_timing(timing);
        if let Some(fonts) = fonts {
            renderer = renderer.with_fonts(fonts);
        }
        if let Some(images) = images {
            renderer = renderer.with_images(images);
        }
        let page = renderer.render_display_list(dl, scale).expect("render");
        (page, renderer.stage_stats())
    }

    /// Render `build(false)` and `build(true)` — one scene, with and without an
    /// invisible full-page fill inside its blend groups — and require
    /// byte-identical rasters.
    ///
    /// The fill widens every group's tracked region to the whole surface, so the
    /// `true` render composites each group the *unscoped* way (a full-raster
    /// blit: exactly the path the backend took before the region existed) while
    /// `false` takes the scoped one. If a group's tracked region ever excludes a
    /// pixel that group painted, that pixel is dropped and the rasters differ —
    /// which is what pins scoping to pixel identity rather than to "the
    /// difference is small".
    ///
    /// Also the two invariants that make such a comparison worth anything: the
    /// leak counter is zero (no painted pixel outside the composited region) and
    /// the composited region is smaller than the full raster the reference used
    /// (so the two renders really did take different paths).
    fn assert_scoped_is_pixel_identical(
        build: impl Fn(bool) -> DisplayList,
        scale: f32,
        fonts: Option<&FontCache>,
        images: Option<&ImageCache>,
    ) -> zpdf_render::StageStats {
        let scoped_dl = build(false);
        let (scoped, stats) = render_backend(&scoped_dl, scale, fonts, images, true);
        let (reference, _) = render_backend(&build(true), scale, fonts, images, false);
        assert_eq!(
            (scoped.width, scoped.height),
            (reference.width, reference.height),
            "the two renders produced different raster sizes"
        );
        if scoped.data != reference.data {
            let byte = scoped
                .data
                .iter()
                .zip(reference.data.iter())
                .position(|(a, b)| a != b)
                .unwrap_or(0);
            let pixel = byte as u32 / 4;
            panic!(
                "scoped composite differs from the full-raster composite at ({}, {}) — \
                 a painted pixel fell outside the tracked region",
                pixel % scoped.width,
                pixel / scoped.width
            );
        }

        let stats = stats.expect("timing was enabled");
        assert_eq!(
            stats.mask_composite_leak_px, 0,
            "painted pixels outside the region the composite was confined to"
        );
        // A scene where nothing composites reports a region of 0, which satisfies
        // this too — harmlessly, since there is no region to get wrong. Where the
        // scene *does* composite, this is what proves the scoping engaged.
        assert!(
            stats.mask_composite_region_px
                < raster_pixels(&scoped_dl, scale) * group_count(&scoped_dl),
            "no group was scoped (region {} px) — the comparison above is vacuous",
            stats.mask_composite_region_px
        );
        stats
    }

    /// An opaque backdrop, one transparency group, and two disjoint small rects
    /// inside it — so the region is a union, not a single draw's bounds.
    fn two_rect_group_scene(
        mode: BlendMode,
        isolated: bool,
        alpha: f32,
        filler: bool,
    ) -> DisplayList {
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        dl.push(solid_rect(
            Color::rgb(0.0, 0.0, 1.0),
            0.0,
            0.0,
            100.0,
            100.0,
        ));
        dl.push(group_cmd(mode, isolated, alpha, None));
        dl.push(solid_rect(
            Color::rgb(1.0, 0.0, 0.0),
            10.0,
            60.0,
            40.0,
            90.0,
        ));
        dl.push(solid_rect(
            Color::rgb(0.0, 1.0, 0.0),
            50.0,
            20.0,
            65.0,
            35.0,
        ));
        if filler {
            dl.push(invisible_page_fill(&dl));
        }
        dl.push(RenderCommand::PopBlendGroup);
        dl
    }

    /// Every blend mode, group alpha, isolation flag and scale must come out
    /// identical whether the composite is confined to the group's content or run
    /// over the whole raster. That holds because the general Porter-Duff blend
    /// `s·(1−da) + d·(1−sa) + B(s,d)·sa·da` collapses to the destination where
    /// `sa = 0` — including the HSL family, whose blend term is not separable per
    /// channel, and which is why they are all in the list.
    #[test]
    fn scoped_composite_matches_full_raster_for_every_blend_mode() {
        for mode in [
            BlendMode::Normal,
            BlendMode::Multiply,
            BlendMode::Screen,
            BlendMode::Overlay,
            BlendMode::Darken,
            BlendMode::Lighten,
            BlendMode::ColorDodge,
            BlendMode::ColorBurn,
            BlendMode::HardLight,
            BlendMode::SoftLight,
            BlendMode::Difference,
            BlendMode::Exclusion,
            BlendMode::Hue,
            BlendMode::Saturation,
            BlendMode::Color,
            BlendMode::Luminosity,
        ] {
            for isolated in [true, false] {
                for alpha in [0.25, 1.0] {
                    for scale in [1.0, 2.0] {
                        assert_scoped_is_pixel_identical(
                            |filler| two_rect_group_scene(mode, isolated, alpha, filler),
                            scale,
                            None,
                            None,
                        );
                    }
                }
            }
        }
    }

    /// A page-wide fill *clipped* to a corner, inside the group: the group's ink
    /// is a 40×40 square, so the region has to follow the clip rather than the
    /// path. The invisible fill sits outside the clip, which is what makes the
    /// reference render unscoped.
    fn clip_group_scene(filler: bool) -> DisplayList {
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        dl.push(solid_rect(
            Color::rgb(0.0, 0.0, 1.0),
            0.0,
            0.0,
            100.0,
            100.0,
        ));
        dl.push(group_cmd(BlendMode::Multiply, true, 1.0, None));
        dl.push(RenderCommand::PushClip {
            path: rect_path(0.0, 0.0, 40.0, 40.0),
            rule: FillRule::NonZero,
        });
        dl.push(solid_rect(
            Color::rgb(1.0, 1.0, 0.0),
            0.0,
            0.0,
            100.0,
            100.0,
        ));
        dl.push(RenderCommand::PopClip);
        if filler {
            dl.push(invisible_page_fill(&dl));
        }
        dl.push(RenderCommand::PopBlendGroup);
        dl
    }

    #[test]
    fn scoped_composite_is_intersected_with_the_clip() {
        let stats = assert_scoped_is_pixel_identical(clip_group_scene, 1.0, None, None);
        assert!(
            (40 * 40..=41 * 41).contains(&stats.mask_composite_region_px),
            "the region must be the clip's 40×40 corner — give or take the one pixel \
             its anti-aliased edge can reach — and not the 100×100 path, got {}",
            stats.mask_composite_region_px
        );
    }

    /// A hairpin stroke inside a group: the join throws a miter spike well past
    /// the centerline's bounding box, so a region grown only by the stroke width
    /// would cut the tip off.
    fn miter_group_scene(filler: bool) -> DisplayList {
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        dl.push(solid_rect(
            Color::rgb(0.0, 0.0, 1.0),
            0.0,
            0.0,
            100.0,
            100.0,
        ));
        dl.push(group_cmd(BlendMode::Multiply, true, 1.0, None));
        let mut path = Path::new();
        path.move_to(Point::new(10.0, 24.0));
        path.line_to(Point::new(30.0, 20.0));
        path.line_to(Point::new(10.0, 16.0));
        dl.push(stroke_cmd(
            path,
            StrokeStyle {
                width: 2.0,
                miter_limit: 10.0,
                ..Default::default()
            },
        ));
        if filler {
            dl.push(invisible_page_fill(&dl));
        }
        dl.push(RenderCommand::PopBlendGroup);
        dl
    }

    #[test]
    fn scoped_composite_covers_a_miter_spike_past_the_path_bounds() {
        // The spike: ~5 px past the vertex at x = 30 (page y = 20 → device y = 80).
        let page = render_backend(&miter_group_scene(false), 1.0, None, None, false).0;
        assert!(
            px(&page, 34, 80)[0] < 100,
            "expected the miter spike to reach x=34, got {:?}",
            px(&page, 34, 80)
        );
        assert_scoped_is_pixel_identical(miter_group_scene, 1.0, None, None);
    }

    /// Nested groups: the inner group's region has to flow into the outer one's,
    /// or the outer composite would drop everything the inner one contributed.
    fn nested_group_scene(filler: bool) -> DisplayList {
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        dl.push(solid_rect(
            Color::rgb(1.0, 1.0, 1.0),
            0.0,
            0.0,
            100.0,
            100.0,
        ));
        dl.push(group_cmd(BlendMode::Multiply, true, 1.0, None));
        dl.push(solid_rect(Color::rgb(1.0, 0.0, 0.0), 5.0, 5.0, 20.0, 20.0));
        dl.push(group_cmd(BlendMode::Screen, true, 0.75, None));
        dl.push(solid_rect(
            Color::rgb(0.0, 0.0, 1.0),
            60.0,
            60.0,
            90.0,
            90.0,
        ));
        if filler {
            dl.push(invisible_page_fill(&dl));
        }
        dl.push(RenderCommand::PopBlendGroup);
        if filler {
            dl.push(invisible_page_fill(&dl));
        }
        dl.push(RenderCommand::PopBlendGroup);
        dl
    }

    #[test]
    fn scoped_composite_carries_a_nested_groups_region_into_its_parent() {
        let stats = assert_scoped_is_pixel_identical(nested_group_scene, 1.0, None, None);
        // Two groups, each scoped to its own small corner (the outermost one also
        // covers the inner one's): the sum can never approach two full pages.
        assert!(
            stats.mask_composite_region_px < 2 * 100 * 100,
            "region {} px — the outer group did not stay scoped",
            stats.mask_composite_region_px
        );
    }

    /// A soft-masked group: both the scoped mask fold and the scoped composite
    /// are exercised, and the mask's coverage varies across the region.
    fn soft_mask_group_scene(filler: bool) -> DisplayList {
        let mut mask_dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        mask_dl.push(solid_rect(Color::rgb(1.0, 1.0, 1.0), 0.0, 0.0, 50.0, 100.0));
        let mask = SoftMask {
            kind: SoftMaskKind::Alpha,
            commands: Arc::new(mask_dl),
            offset: (0.0, 0.0),
            backdrop_luma: 0.0,
            transfer: None,
        };

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        dl.push(solid_rect(
            Color::rgb(0.0, 0.0, 1.0),
            0.0,
            0.0,
            100.0,
            100.0,
        ));
        dl.push(group_cmd(BlendMode::Normal, true, 1.0, Some(mask)));
        dl.push(solid_rect(
            Color::rgb(1.0, 0.0, 0.0),
            20.0,
            20.0,
            60.0,
            60.0,
        ));
        if filler {
            dl.push(invisible_page_fill(&dl));
        }
        dl.push(RenderCommand::PopBlendGroup);
        dl
    }

    #[test]
    fn scoped_composite_matches_full_raster_with_a_soft_mask() {
        let stats = assert_scoped_is_pixel_identical(soft_mask_group_scene, 1.0, None, None);
        assert!(
            stats.mask_fold_ns > 0 || stats.soft_mask_ns > 0,
            "the mask path must have run"
        );
    }

    /// A mask whose own group paints only a small square: the mask raster is
    /// page-sized, but only that square can hold anything but "unpainted", so the
    /// reduce is confined to it. The invisible fill sits inside the *mask's*
    /// command list, which is what forces the reference render to reduce the
    /// whole plane — so the two renders differ in exactly the work under test.
    ///
    /// Both mask kinds are built because they fail differently: a luminosity
    /// mask's target starts *opaque*, so its every pixel has non-zero alpha and a
    /// paintedness test based on alpha would call the whole page painted.
    fn small_mask_group_scene(kind: SoftMaskKind, filler: bool) -> DisplayList {
        let mut mask_dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        mask_dl.push(solid_rect(
            Color::rgb(1.0, 1.0, 1.0),
            20.0,
            20.0,
            40.0,
            40.0,
        ));
        if filler {
            mask_dl.push(invisible_page_fill(&mask_dl));
        }
        let mask = SoftMask {
            kind,
            commands: Arc::new(mask_dl),
            offset: (0.0, 0.0),
            backdrop_luma: 0.0,
            transfer: None,
        };

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        dl.push(solid_rect(
            Color::rgb(0.0, 0.0, 1.0),
            0.0,
            0.0,
            100.0,
            100.0,
        ));
        dl.push(group_cmd(BlendMode::Normal, true, 1.0, Some(mask)));
        dl.push(solid_rect(
            Color::rgb(1.0, 0.0, 0.0),
            10.0,
            10.0,
            90.0,
            90.0,
        ));
        dl.push(RenderCommand::PopBlendGroup);
        dl
    }

    #[test]
    fn mask_reduce_is_confined_to_what_the_mask_group_painted() {
        for kind in [SoftMaskKind::Alpha, SoftMaskKind::Luminosity] {
            let stats = assert_scoped_is_pixel_identical(
                |filler| small_mask_group_scene(kind, filler),
                1.0,
                None,
                None,
            );
            assert_eq!(
                stats.mask_reduce_leak_px, 0,
                "{kind:?}: no pixel outside the reduced region may differ from \
                 what the fill put there"
            );
            // The 20×20 square plus one pixel of anti-aliased edge on each side.
            assert!(
                (20 * 20..=22 * 22).contains(&stats.mask_reduce_px),
                "{kind:?}: the reduce must cover the mask group's 20×20 (± its AA \
                 edge), not the whole 100×100 plane — got {}",
                stats.mask_reduce_px
            );
        }
    }

    /// Glyph ink inside a group. Marked from each *transformed* outline's own
    /// bounds, so a group of text is scoped to the text and not to an em box.
    #[test]
    fn scoped_composite_matches_full_raster_with_glyphs_inside_the_group() {
        let (fonts, font_id) = var_fonts();
        let build = |filler: bool| {
            let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
            dl.push(solid_rect(
                Color::rgb(0.0, 0.0, 1.0),
                0.0,
                0.0,
                100.0,
                100.0,
            ));
            dl.push(group_cmd(BlendMode::Multiply, true, 1.0, None));
            dl.push(glyph_run_cmd(font_id));
            if filler {
                dl.push(invisible_page_fill(&dl));
            }
            dl.push(RenderCommand::PopBlendGroup);
            dl
        };
        assert_scoped_is_pixel_identical(build, 1.0, Some(&fonts), None);
    }

    /// Type3 ink is an ordinary device path by the time the backend sees it, so
    /// it bounds like any other fill — asserted rather than assumed, because a
    /// Type3 glyph stream is the one place content is interpreted *inside* the
    /// renderer.
    #[test]
    fn scoped_composite_matches_full_raster_with_type3_glyphs_inside_the_group() {
        let (fonts, font_id) = type3_fonts();
        let build = |filler: bool| {
            let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
            dl.push(solid_rect(
                Color::rgb(0.0, 0.0, 1.0),
                0.0,
                0.0,
                100.0,
                100.0,
            ));
            dl.push(group_cmd(BlendMode::Multiply, true, 1.0, None));
            dl.push(glyph_run_cmd(font_id));
            if filler {
                dl.push(invisible_page_fill(&dl));
            }
            dl.push(RenderCommand::PopBlendGroup);
            dl
        };
        assert_scoped_is_pixel_identical(build, 1.0, Some(&fonts), None);
    }

    /// An image inside a group: marked from the transformed unit square, which
    /// is what the CTM maps the samples through — not the source rectangle.
    #[test]
    fn scoped_composite_matches_full_raster_with_an_image_inside_the_group() {
        let mut images = ImageCache::new();
        let image_id = images.insert(DecodedImage {
            width: 4,
            height: 4,
            data: vec![200; 4 * 4 * 4],
            has_alpha: false,
            is_image_mask: false,
            premultiplied: true,
        });
        let build = |filler: bool| {
            let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
            dl.push(solid_rect(
                Color::rgb(0.0, 0.0, 1.0),
                0.0,
                0.0,
                100.0,
                100.0,
            ));
            dl.push(group_cmd(BlendMode::Multiply, true, 1.0, None));
            dl.push(RenderCommand::DrawImage(ImageDraw {
                image_id,
                transform: Matrix::new(30.0, 0.0, 0.0, 30.0, 10.0, 10.0),
                alpha: 1.0,
                is_image_mask: false,
            }));
            if filler {
                dl.push(invisible_page_fill(&dl));
            }
            dl.push(RenderCommand::PopBlendGroup);
            dl
        };
        assert_scoped_is_pixel_identical(build, 1.0, None, Some(&images));
    }

    /// A group whose content is entirely clipped away paints nothing, so there
    /// is no composite at all. The reference (whose invisible fill sits outside
    /// the clip) instead composites a fully transparent group over the whole
    /// page — and the two must still agree, which is the "outside the region the
    /// source is transparent" argument stated the other way round.
    #[test]
    fn a_group_whose_content_is_clipped_away_composites_nothing() {
        fn build(filler: bool) -> DisplayList {
            let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
            dl.push(solid_rect(
                Color::rgb(0.0, 0.0, 1.0),
                0.0,
                0.0,
                100.0,
                100.0,
            ));
            dl.push(group_cmd(BlendMode::Multiply, true, 1.0, None));
            dl.push(RenderCommand::PushClip {
                path: rect_path(0.0, 0.0, 10.0, 10.0),
                rule: FillRule::NonZero,
            });
            // Entirely outside the clip above: nothing of this reaches the group.
            dl.push(solid_rect(
                Color::rgb(1.0, 0.0, 0.0),
                50.0,
                50.0,
                90.0,
                90.0,
            ));
            dl.push(RenderCommand::PopClip);
            if filler {
                dl.push(invisible_page_fill(&dl));
            }
            dl.push(RenderCommand::PopBlendGroup);
            dl
        }
        let stats = assert_scoped_is_pixel_identical(build, 1.0, None, None);
        assert_eq!(
            stats.mask_composite_region_px, 0,
            "a group that painted nothing owns no region, so its composite is skipped"
        );
    }

    /// Knockout and overprint elements render alone into scratch surfaces whose
    /// painted regions bound the merge, so their groups composite over just
    /// those regions — and the leak count shows the regions are still *sound*.
    #[test]
    fn knockout_and_overprint_groups_are_scoped_and_sound() {
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        dl.push(solid_rect(
            Color::rgb(0.0, 0.0, 1.0),
            0.0,
            0.0,
            100.0,
            100.0,
        ));
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Normal,
            isolated: true,
            knockout: true,
            bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
            alpha: 1.0,
            mask: None,
        });
        // Two overlapping elements: the second knocks the first out where they
        // overlap, so a missed mark would show up as a wrong pixel, not just a
        // smaller region.
        dl.push(solid_rect(
            Color::rgba(1.0, 0.0, 0.0, 0.5),
            10.0,
            10.0,
            60.0,
            60.0,
        ));
        dl.push(solid_rect(
            Color::rgba(0.0, 1.0, 0.0, 0.5),
            40.0,
            40.0,
            90.0,
            90.0,
        ));
        dl.push(RenderCommand::PopBlendGroup);

        // Overprint inside a normal group.
        dl.push(group_cmd(BlendMode::Multiply, true, 1.0, None));
        let mut op = solid_rect(Color::rgb(1.0, 1.0, 0.0), 5.0, 70.0, 45.0, 95.0);
        if let RenderCommand::FillPath { overprint, .. } = &mut op {
            *overprint = Some(Overprint {
                cmyk: [1.0, 0.0, 0.0, 0.0],
                active: Overprint::C,
            });
        }
        dl.push(op);
        dl.push(RenderCommand::PopBlendGroup);

        let (page, stats) = render_backend(&dl, 1.0, None, None, true);
        assert_eq!(page.width, 100);
        let stats = stats.expect("timing was enabled");
        assert_eq!(
            stats.mask_composite_leak_px, 0,
            "the conservative marks must still cover every painted pixel"
        );
        // Each element's extent carries one pixel of anti-aliasing slack. The
        // knockout elements span x, y ∈ [9, 91); the overprinted rect (device
        // y = 100 − PDF y) spans x ∈ [4, 46), y ∈ [4, 31).
        assert_eq!(
            stats.mask_composite_region_px,
            82 * 82 + 42 * 27,
            "each group composites over its elements' extent only"
        );
    }

    /// RGBA of the device pixel at (x, y).
    fn pixel(page: &RenderedPage, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * page.width + x) * 4) as usize;
        page.data[i..i + 4].try_into().unwrap()
    }

    /// Sequential clips are charged their own extent, not the raster's: at 300
    /// DPI a page with thousands of small clips still honours the last one
    /// (charged per raster, clips stopped being built after about 240).
    #[test]
    fn many_sequential_clips_all_stay_in_force() {
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 612.0, 792.0));
        for i in 0..3000 {
            let x = (i % 60) as f64 * 10.0;
            let y = (i / 60) as f64 * 10.0;
            dl.push(RenderCommand::PushClip {
                path: rect_path(x, y, x + 5.0, y + 5.0),
                rule: FillRule::NonZero,
            });
            dl.push(solid_rect(
                Color::rgb(0.0, 0.0, 1.0),
                x,
                y,
                x + 5.0,
                y + 5.0,
            ));
            dl.push(RenderCommand::PopClip);
        }
        dl.push(RenderCommand::PushClip {
            path: rect_path(600.0, 700.0, 610.0, 710.0),
            rule: FillRule::NonZero,
        });
        dl.push(solid_rect(
            Color::rgb(1.0, 0.0, 0.0),
            0.0,
            0.0,
            612.0,
            792.0,
        ));
        dl.push(RenderCommand::PopClip);

        let (page, stats) = render_backend(&dl, 300.0 / 72.0, None, None, true);
        assert_eq!(stats.expect("timing").clips_pushed, 3001);
        // Device (10, 10) is PDF (2.4, 789.6): outside every clip.
        assert_eq!(pixel(&page, 10, 10), [255; 4], "the last clip held");
        // Inside the last clip: PDF (605, 705).
        let (x, y) = (605.0 * 300.0 / 72.0, (792.0 - 705.0) * 300.0 / 72.0);
        assert_eq!(pixel(&page, x as u32, y as u32), [255, 0, 0, 255]);
    }

    #[test]
    fn strokes_outside_the_clip_are_culled() {
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        dl.push(RenderCommand::PushClip {
            path: rect_path(0.0, 0.0, 20.0, 20.0),
            rule: FillRule::NonZero,
        });
        let mut line = Path::new();
        line.move_to(Point::new(50.0, 10.0));
        line.line_to(Point::new(90.0, 10.0));
        dl.push(stroke_cmd(line, StrokeStyle::default()));
        dl.push(RenderCommand::PopClip);
        let (page, stats) = render_backend(&dl, 1.0, None, None, true);
        assert_eq!(stats.expect("timing").strokes, 0);
        assert!(page.data.iter().all(|&b| b == 255), "page stays white");
    }

    #[test]
    fn recycled_surfaces_and_masks_come_back_clear() {
        let mut renderer = CpuRenderer::new();
        renderer
            .begin_page(&PageRenderInfo {
                page_rect: Rect::new(0.0, 0.0, 40.0, 40.0),
                scale: 1.0,
                background: Color::white(),
            })
            .expect("page");
        let mut surface = renderer.take_surface(40, 40).expect("surface");
        surface.fill(tiny_skia::Color::BLACK);
        let mut mask = renderer.take_mask(40, 40).expect("mask");
        mask.data_mut().fill(255);
        // Both claim the whole raster as their painted region.
        renderer.recycle_surface(surface, Some(PaintedBounds::whole(40, 40)));
        renderer.recycle_mask(mask, None);
        let surface = renderer.take_surface(40, 40).expect("pooled surface");
        assert!(surface.data().iter().all(|&b| b == 0));
        let mask = renderer.take_mask(40, 40).expect("pooled mask");
        assert!(mask.data().iter().all(|&b| b == 0));
        assert_eq!(renderer.pooled_surface_bytes, 0);
    }

    /// The leak counter is the assertion the whole optimization rests on, so it
    /// must be able to fail: a checker that cannot fire is not a check. Feed the
    /// coverage scan a region deliberately smaller than the content and require
    /// it to name the pixels left outside.
    #[test]
    fn leak_counter_reports_painted_pixels_outside_the_region() {
        let mut pixmap = tiny_skia::Pixmap::new(4, 4).unwrap();
        for y in 2..4u32 {
            for x in 2..4u32 {
                let i = ((y * 4 + x) * 4) as usize;
                pixmap.data_mut()[i..i + 4].copy_from_slice(&[255, 0, 0, 255]);
            }
        }
        let content = PaintedBounds {
            x0: 2,
            y0: 2,
            x1: 4,
            y1: 4,
        };
        assert_eq!(group_coverage(&pixmap, Some(content)), (Some(content), 0));

        let too_small = PaintedBounds {
            x0: 2,
            y0: 2,
            x1: 3,
            y1: 3,
        };
        assert_eq!(
            group_coverage(&pixmap, Some(too_small)),
            (Some(content), 3),
            "three of the four painted pixels lie outside the region"
        );
        assert_eq!(
            group_coverage(&pixmap, None),
            (Some(content), 4),
            "no region at all leaves every painted pixel unaccounted for"
        );

        let empty = tiny_skia::Pixmap::new(4, 4).unwrap();
        assert_eq!(
            group_coverage(&empty, None),
            (None, 0),
            "an untouched raster has no content and nothing to leak"
        );
    }

    /// Banding `with_backdrop_shape_coverage_budgeted` into tiny horizontal and
    /// vertical tiles (under an active anti-aliased clip and a half-pixel shape
    /// edge) must produce pixel-for-pixel identical RGBA output to a single band.
    #[test]
    fn with_backdrop_shape_coverage_banded_matches_single_band_pixel_for_pixel() {
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 8.0, 8.0));
        let mut r1 = Path::new();
        r1.move_to(Point::new(0.0, 0.0));
        r1.line_to(Point::new(8.0, 0.0));
        r1.line_to(Point::new(8.0, 8.0));
        r1.line_to(Point::new(0.0, 8.0));
        r1.close();
        cell.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Multiply,
            isolated: false,
            knockout: false,
            alpha: 1.0,
            mask: None,
            bounds: Rect::new(0.0, 0.0, 8.0, 8.0),
        });
        cell.push(RenderCommand::FillPath {
            path: r1.clone(),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.5, 0.8, 0.2)),
            alpha: 1.0,
            overprint: None,
        });
        cell.push(RenderCommand::PopBlendGroup);
        cell.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Multiply,
            isolated: false,
            knockout: false,
            alpha: 1.0,
            mask: None,
            bounds: Rect::new(0.0, 0.0, 8.0, 8.0),
        });
        cell.push(RenderCommand::FillPath {
            path: r1,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.4, 0.6, 0.9)),
            alpha: 1.0,
            overprint: None,
        });
        cell.push(RenderCommand::PopBlendGroup);

        let tiling = TilingPattern {
            bbox: Rect::new(0.0, 0.0, 8.0, 8.0),
            x_step: 8.0,
            y_step: 8.0,
            matrix: zpdf_core::Matrix::identity(),
            commands: Arc::new(cell),
            tiling_type: 1,
        };
        let steps = tiling.blend_steps().expect("blend steps");

        let mut clip_path = Path::new();
        clip_path.move_to(Point::new(12.5, 10.5));
        clip_path.line_to(Point::new(56.5, 14.5));
        clip_path.line_to(Point::new(52.5, 58.5));
        clip_path.line_to(Point::new(8.5, 54.5));
        clip_path.close();

        let mut shape_path = Path::new();
        shape_path.move_to(Point::new(10.5, 12.5));
        shape_path.line_to(Point::new(54.5, 12.5));
        shape_path.line_to(Point::new(54.5, 56.5));
        shape_path.line_to(Point::new(10.5, 56.5));
        shape_path.close();

        let render_with_budget = |max_band_bytes: u64| -> Vec<u8> {
            let mut renderer = CpuRenderer::new();
            renderer
                .begin_page(&PageRenderInfo {
                    page_rect: Rect::new(0.0, 0.0, 64.0, 64.0),
                    scale: 1.0,
                    background: Color::rgb(0.9, 0.7, 0.5),
                })
                .expect("page");
            renderer.push_clip(&clip_path, &FillRule::NonZero);
            let skia_path = renderer.build_skia_path(&shape_path).expect("path");
            let scope = renderer
                .pixmap
                .as_ref()
                .map(|p| CpuRenderer::drawn_extent(skia_path.bounds(), 1.0, p.width(), p.height()));
            let white = CpuRenderer::color_to_paint(&Color::white(), 1.0);
            let painted = renderer.with_backdrop_shape_coverage_budgeted(
                0.5,
                scope,
                max_band_bytes,
                |r, band_offset| {
                    if let Some(ref mut target) = r.pixmap {
                        target.fill_path(
                            &skia_path,
                            &white,
                            tiny_skia::FillRule::Winding,
                            band_offset,
                            None,
                        );
                    }
                },
                |r, region| r.paint_tiling_plan_in_region(&tiling, &steps, region),
            );
            assert!(painted, "expected banded coverage pass to paint");
            renderer.pop_clip();
            renderer.end_page().expect("end").data
        };

        let single = render_with_budget(MAX_COVERAGE_BAND_BYTES);
        // 512 bytes = 64 pixels per band → forces horizontal strips (width 46, height 1).
        let horizontal_bands = render_with_budget(512);
        // 128 bytes = 16 pixels per band → forces both X and Y tiling (16×1 tiles).
        let xy_tiles = render_with_budget(128);

        assert_eq!(single, horizontal_bands);
        assert_eq!(single, xy_tiles);
        // Confirm the pattern actually modified pixels inside the clipped region.
        let sample = ((32 * 64 + 32) * 4) as usize;
        assert_ne!(&single[sample..sample + 3], &[230, 179, 128]);
    }

    #[test]
    fn nested_non_isolated_blend_groups_avoid_exponential_replay_and_match_spec() {
        // 15 nested non-isolated Screen groups + 1 inner Multiply white fill
        // over opaque blue must complete quickly (no 2^15 replay blowup) and
        // preserve exact blue.
        let page_rect = Rect::new(0.0, 0.0, 32.0, 32.0);
        let mut dl = DisplayList::new(page_rect);
        let mut full = Path::new();
        full.rect(page_rect);
        dl.push(RenderCommand::FillPath {
            path: full.clone(),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        for _ in 0..15 {
            dl.push(RenderCommand::PushBlendGroup {
                blend_mode: BlendMode::Screen,
                isolated: false,
                knockout: false,
                bounds: page_rect,
                alpha: 1.0,
                mask: None,
            });
        }
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Multiply,
            isolated: true,
            knockout: false,
            bounds: page_rect,
            alpha: 1.0,
            mask: None,
        });
        dl.push(RenderCommand::FillPath {
            path: full.clone(),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PopBlendGroup);
        for _ in 0..15 {
            dl.push(RenderCommand::PopBlendGroup);
        }

        let mut renderer = CpuRenderer::new();
        let page = renderer.render_display_list(&dl, 1.0).expect("cpu render");
        assert!(
            !renderer.over_budget,
            "nested non-isolated groups must not hit time budget"
        );
        let off = ((16 * page.width + 16) * 4) as usize;
        let px = &page.data[off..off + 4];
        assert!(
            px[0] < 10 && px[1] < 10 && px[2] > 245 && px[3] > 245,
            "15 nested non-isolated Screen groups over blue with white Multiply must stay blue, got {px:?}"
        );

        // Analytical ISO 32000-1 §11.4.5 check:
        // Backdrop = opaque gray (0.5, 0.5, 0.5).
        // Outer non-isolated Multiply (alpha 1.0) wraps inner non-isolated
        // Screen (alpha 0.5) containing opaque red (1, 0, 0).
        // Inner Screen onto gray at 0.5 -> (0.75, 0.5, 0.5), α_g = 0.5.
        // Backdrop removal of outer against gray -> c_g = (1.0, 0.5, 0.5), α_g = 0.5.
        // Outer Multiply onto gray -> 0.5*(0.5,0.5,0.5) + 0.5*(0.5,0.25,0.25)
        //                           = (0.5, 0.375, 0.375) ≈ (128, 96, 96).
        let mut dl2 = DisplayList::new(page_rect);
        dl2.push(RenderCommand::FillPath {
            path: full.clone(),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.5, 0.5, 0.5)),
            alpha: 1.0,
            overprint: None,
        });
        dl2.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Multiply,
            isolated: false,
            knockout: false,
            bounds: page_rect,
            alpha: 1.0,
            mask: None,
        });
        dl2.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Screen,
            isolated: false,
            knockout: false,
            bounds: page_rect,
            alpha: 0.5,
            mask: None,
        });
        dl2.push(RenderCommand::FillPath {
            path: full,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        dl2.push(RenderCommand::PopBlendGroup);
        dl2.push(RenderCommand::PopBlendGroup);

        let page2 = renderer
            .render_display_list(&dl2, 1.0)
            .expect("cpu render 2");
        let px2 = &page2.data[off..off + 3];
        assert!(
            (px2[0] as i32 - 128).abs() <= 2
                && (px2[1] as i32 - 96).abs() <= 2
                && (px2[2] as i32 - 96).abs() <= 2,
            "nested non-isolated Multiply(Screen(0.5, red)) over gray must be ~(128, 96, 96), got {px2:?}"
        );
    }

    #[test]
    fn banded_non_isolated_screen_group_matches_single_pass() {
        // Verify multi-band slicing in `finish_recorded_non_isolated_group_budgeted`
        // (including active clip mask and soft mask) matches a single full pass
        // bit-for-bit.
        let page_rect = Rect::new(0.0, 0.0, 48.0, 48.0);
        let mut clip_path = Path::new();
        clip_path.rect(Rect::new(4.0, 4.0, 44.0, 44.0));
        let mut fill_path = Path::new();
        fill_path.rect(Rect::new(8.0, 8.0, 40.0, 40.0));

        let run = |max_band_bytes: u64| -> Vec<u8> {
            let mut r = CpuRenderer::new();
            r.begin_page(&PageRenderInfo {
                page_rect,
                scale: 1.0,
                background: Color::rgb(0.0, 0.0, 1.0),
            })
            .unwrap();
            // Right half gray so Screen(gray, gray) = ~191 while left stays blue.
            let mut right = Path::new();
            right.rect(Rect::new(24.0, 0.0, 48.0, 48.0));
            r.execute(&RenderCommand::FillPath {
                path: right,
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::rgb(0.5, 0.5, 0.5)),
                alpha: 1.0,
                overprint: None,
            })
            .unwrap();
            r.push_clip(&clip_path, &FillRule::NonZero);
            let rec = RecordedNonIsolatedGroup {
                blend_mode: BlendMode::Screen,
                alpha: 1.0,
                mask: None,
                nesting: 0,
                cmds: vec![
                    RenderCommand::PushBlendGroup {
                        blend_mode: BlendMode::Multiply,
                        isolated: true,
                        knockout: false,
                        bounds: page_rect,
                        alpha: 1.0,
                        mask: None,
                    },
                    RenderCommand::FillPath {
                        path: fill_path.clone(),
                        rule: FillRule::NonZero,
                        paint: Paint::Solid(Color::white()),
                        alpha: 1.0,
                        overprint: None,
                    },
                    RenderCommand::PopBlendGroup,
                ],
            };
            r.finish_recorded_non_isolated_group_budgeted(rec, max_band_bytes);
            r.pop_clip();
            r.end_page().unwrap().data
        };

        let single = run(MAX_COVERAGE_BAND_BYTES);
        let banded = run(512);
        let tiled = run(128);
        assert_eq!(single, banded);
        assert_eq!(single, tiled);
        // Left half inside fill (x=16, y=24) must stay pure blue; right half (x=32, y=24) must be ~191 gray.
        let left_off = ((24 * 48 + 16) * 4) as usize;
        assert_eq!(&single[left_off..left_off + 3], &[0, 0, 255]);
        let right_off = ((24 * 48 + 32) * 4) as usize;
        assert!(single[right_off..right_off + 3]
            .iter()
            .all(|&c| (186..=197).contains(&c)));
    }

    #[test]
    fn knockout_parent_does_not_erase_inside_recorded_non_isolated_child() {
        // Isolated knockout parent whose only child is a non-knockout Screen
        // form containing two overlapping 50%-opaque red fills. The two fills
        // inside the child must accumulate to 75% alpha (NOT knock each other
        // out to 50%), and the completed child must still knock out a prior
        // sibling inside the knockout parent as one element.
        let page_rect = Rect::new(0.0, 0.0, 40.0, 20.0);
        let mut dl = DisplayList::new(page_rect);

        // Left half [0..20]: isolated knockout parent with ONLY the non-knockout
        // Screen child (two overlapping 50%-opaque red fills) over transparent
        // canvas -> exact alpha 0.75 (~191), NOT 0.5 (~128).
        let mut left = Path::new();
        left.rect(Rect::new(0.0, 0.0, 20.0, 20.0));
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Normal,
            isolated: true,
            knockout: true,
            bounds: Rect::new(0.0, 0.0, 20.0, 20.0),
            alpha: 1.0,
            mask: None,
        });
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Screen,
            isolated: false,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 20.0, 20.0),
            alpha: 1.0,
            mask: None,
        });
        for _ in 0..2 {
            dl.push(RenderCommand::FillPath {
                path: left.clone(),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
                alpha: 0.5,
                overprint: None,
            });
        }
        dl.push(RenderCommand::PopBlendGroup);
        dl.push(RenderCommand::PopBlendGroup);

        // Right half [20..40]: isolated knockout parent with a prior opaque
        // green sibling followed by the same non-knockout Screen child. The
        // child must knock out the prior green sibling completely as ONE element
        // leaving 75%-alpha red (zero green).
        let mut right = Path::new();
        right.rect(Rect::new(20.0, 0.0, 40.0, 20.0));
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Normal,
            isolated: true,
            knockout: true,
            bounds: Rect::new(20.0, 0.0, 40.0, 20.0),
            alpha: 1.0,
            mask: None,
        });
        dl.push(RenderCommand::FillPath {
            path: right.clone(),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 1.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Screen,
            isolated: false,
            knockout: false,
            bounds: Rect::new(20.0, 0.0, 40.0, 20.0),
            alpha: 1.0,
            mask: None,
        });
        for _ in 0..2 {
            dl.push(RenderCommand::FillPath {
                path: right.clone(),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
                alpha: 0.5,
                overprint: None,
            });
        }
        dl.push(RenderCommand::PopBlendGroup);
        dl.push(RenderCommand::PopBlendGroup);

        let mut r = CpuRenderer::new();
        r.begin_page(&PageRenderInfo {
            page_rect,
            scale: 1.0,
            background: Color::rgba(0.0, 0.0, 0.0, 0.0),
        })
        .unwrap();
        for cmd in &dl.commands {
            r.execute(cmd).unwrap();
        }
        let page = r.end_page().unwrap();
        let left_px = &page.data[((10 * 40 + 10) * 4) as usize..((10 * 40 + 10) * 4 + 4) as usize];
        assert!(
            (188..=194).contains(&left_px[0])
                && left_px[1] == 0
                && left_px[2] == 0
                && (188..=194).contains(&left_px[3]),
            "two 50% fills inside non-knockout Screen child of isolated knockout parent must accumulate to 75% alpha (~191, NOT ~128), got {left_px:?}"
        );
        let right_px = &page.data[((10 * 40 + 30) * 4) as usize..((10 * 40 + 30) * 4 + 4) as usize];
        assert!(
            (188..=194).contains(&right_px[0])
                && right_px[1] == 0
                && right_px[2] == 0
                && (188..=194).contains(&right_px[3]),
            "completed non-knockout Screen child must knock out prior green sibling as one element (~[191,0,0,191]), got {right_px:?}"
        );
    }

    #[test]
    fn recorded_non_isolated_group_scopes_with_live_pixmap_dimensions() {
        // On a 20,000 × 16 raster, the path-extent guard is max(20_000 * 64, 1_000_000)
        // = 1,280,000. A rectangle covering the page and extending to x = 1,100,000
        // passes the normal path guard and must NOT be dropped during scoping
        // (which previously saw `self.pixmap == None` and fell back to 1,000,000).
        let page_rect = Rect::new(0.0, 0.0, 20_000.0, 16.0);
        let mut wide = Path::new();
        wide.rect(Rect::new(0.0, 0.0, 1_100_000.0, 16.0));
        let mut dl = DisplayList::new(page_rect);
        dl.push(RenderCommand::FillPath {
            path: wide.clone(),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.5, 0.5, 0.5)),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Screen,
            isolated: false,
            knockout: false,
            bounds: page_rect,
            alpha: 1.0,
            mask: None,
        });
        dl.push(RenderCommand::FillPath {
            path: wide,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.5, 0.5, 0.5)),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PopBlendGroup);

        let page = CpuRenderer::new()
            .render_display_list(&dl, 1.0)
            .expect("wide raster render");
        assert_eq!((page.width, page.height), (20_000, 16));
        let off = ((8 * 20_000 + 10_000) * 4) as usize;
        let px = &page.data[off..off + 3];
        assert!(
            px.iter().all(|&c| (186..=197).contains(&c)),
            "Screen(0.5, 0.5) with rect extending to x=1,100,000 on 20000x16 raster must paint ~191 (not be dropped as empty), got {px:?}"
        );
    }

    #[test]
    fn knockout_parent_applies_antialiased_edge_coverage_once_for_recorded_child() {
        // On a transparent page, an isolated knockout parent containing a single
        // non-isolated Screen child with an opaque red rectangle whose left edge
        // lands at x = 10.5 (half-pixel coverage) must yield ~128 edge alpha (NOT
        // ~64 from multiplying premultiplied coverage by shape a second time), in
        // BOTH the full-page and banded paths. Also verify a preceding opaque blue
        // sibling is attenuated by (1 - f) = 0.5 at the same edge (~[128, 0, 128, 255]).
        let page_rect = Rect::new(0.0, 0.0, 32.0, 20.0);
        let mut rect_half_edge = Path::new();
        rect_half_edge.rect(Rect::new(10.5, 2.0, 22.0, 18.0));
        let mut full_bg = Path::new();
        full_bg.rect(page_rect);

        let run = |with_sibling: bool, max_band_bytes: u64| -> Vec<u8> {
            let mut r = CpuRenderer::new();
            r.begin_page(&PageRenderInfo {
                page_rect,
                scale: 1.0,
                background: Color::rgba(0.0, 0.0, 0.0, 0.0),
            })
            .unwrap();
            r.push_blend_group(BlendMode::Normal, true, true, 1.0, None);
            if with_sibling {
                r.execute(&RenderCommand::FillPath {
                    path: full_bg.clone(),
                    rule: FillRule::NonZero,
                    paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
                    alpha: 1.0,
                    overprint: None,
                })
                .unwrap();
            }
            let rec = RecordedNonIsolatedGroup {
                blend_mode: BlendMode::Screen,
                alpha: 1.0,
                mask: None,
                nesting: 0,
                cmds: vec![RenderCommand::FillPath {
                    path: rect_half_edge.clone(),
                    rule: FillRule::NonZero,
                    paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
                    alpha: 1.0,
                    overprint: None,
                }],
            };
            r.finish_recorded_non_isolated_group_budgeted(rec, max_band_bytes);
            r.pop_blend_group();
            r.end_page().unwrap().data
        };

        let edge_off = ((10 * 32 + 10) * 4) as usize;
        let interior_off = ((10 * 32 + 15) * 4) as usize;

        for (label, budget) in [
            ("full-page", MAX_COVERAGE_BAND_BYTES),
            ("banded-512", 512),
            ("banded-128", 128),
        ] {
            let solo = run(false, budget);
            let edge = &solo[edge_off..edge_off + 4];
            assert!(
                (122..=134).contains(&edge[0])
                    && edge[1] == 0
                    && edge[2] == 0
                    && (122..=134).contains(&edge[3]),
                "{label}: half-pixel edge on transparent knockout parent must have alpha ~128 (NOT ~64), got {edge:?}"
            );
            assert_eq!(
                &solo[interior_off..interior_off + 4],
                &[255, 0, 0, 255],
                "{label}: interior must be opaque red"
            );

            let over_blue = run(true, budget);
            let edge_b = &over_blue[edge_off..edge_off + 4];
            assert!(
                (122..=134).contains(&edge_b[0])
                    && edge_b[1] == 0
                    && (122..=134).contains(&edge_b[2])
                    && edge_b[3] >= 253,
                "{label}: half-pixel edge over prior blue sibling must be ~[128,0,128,255], got {edge_b:?}"
            );
            assert_eq!(
                &over_blue[interior_off..interior_off + 4],
                &[255, 0, 0, 255],
                "{label}: interior must fully knock out prior blue sibling"
            );
        }
    }

    #[test]
    fn type3_bitmap_pattern_stencil_bands_within_live_surface_budget() {
        // 8×8 stencil: top half opaque, bottom half transparent.
        let mut stencil = tiny_skia::Pixmap::new(8, 8).unwrap();
        for y in 0..4 {
            for x in 0..8 {
                let off = ((y * 8 + x) * 4) as usize;
                stencil.data_mut()[off..off + 4].copy_from_slice(&[0, 0, 0, 255]);
            }
        }
        // Two-color 4×4 repeating tile: left red, right blue.
        let mut tile = tiny_skia::Pixmap::new(4, 4).unwrap();
        for y in 0..4 {
            for x in 0..4 {
                let off = ((y * 4 + x) * 4) as usize;
                let px = if x < 2 {
                    [255, 0, 0, 255]
                } else {
                    [0, 0, 255, 255]
                };
                tile.data_mut()[off..off + 4].copy_from_slice(&px);
            }
        }
        let paint = tiny_skia::Paint {
            shader: tiny_skia::Pattern::new(
                tile.as_ref(),
                tiny_skia::SpreadMode::Repeat,
                tiny_skia::FilterQuality::Nearest,
                1.0,
                tiny_skia::Transform::identity(),
            ),
            ..Default::default()
        };
        let page_rect = Rect::new(0.0, 0.0, 32.0, 32.0);
        let footprint = tiny_skia::Rect::from_ltrb(4.0, 4.0, 28.0, 28.0).unwrap();
        let transform = tiny_skia::Transform::from_scale(3.0, 3.0).post_translate(4.0, 4.0);

        let run = |extra_live_bytes: u64, max_band_bytes: u64| -> (Vec<u8>, u64) {
            let mut r = CpuRenderer::new();
            r.begin_page(&PageRenderInfo {
                page_rect,
                scale: 1.0,
                background: Color::white(),
            })
            .unwrap();
            // Active clip to exercise clip indexing across vertical and horizontal bands.
            let mut clip = Path::new();
            clip.rect(Rect::new(6.0, 6.0, 26.0, 26.0));
            r.execute(&RenderCommand::PushClip {
                path: clip,
                rule: FillRule::NonZero,
            })
            .unwrap();
            let base_bytes = r.blend_surface_bytes;
            r.blend_surface_bytes = base_bytes.saturating_add(extra_live_bytes);
            let before = r.blend_surface_bytes;
            r.render_type3_pattern_stencil_budgeted(
                stencil.as_ref(),
                transform,
                footprint,
                4,
                4,
                28,
                28,
                0.75,
                &paint,
                max_band_bytes,
            );
            let after = r.blend_surface_bytes;
            assert_eq!(
                after, before,
                "live blend_surface_bytes must be released after stencil banding"
            );
            r.blend_surface_bytes = base_bytes;
            (r.end_page().unwrap().data, after)
        };

        let (full, _) = run(0, MAX_COVERAGE_BAND_BYTES);
        // Tight vertical + horizontal banding via `max_band_bytes = 64` (8 px/band).
        let (tight_band, _) = run(0, 64);
        assert_eq!(
            tight_band, full,
            "banded Type 3 pattern stencil must match unbanded output pixel-for-pixel"
        );

        // Simulate a 7000×7000 page where remaining live surface budget is only
        // 512 bytes (< full 24×24×8 = 4608 bytes): must still band and match!
        let remaining = 512u64;
        let page_bytes = 32u64 * 32 * 4;
        let extra = MAX_BLEND_SURFACE_BYTES - page_bytes - remaining;
        let (tight_live, _) = run(extra, MAX_COVERAGE_BAND_BYTES);
        assert_eq!(
            tight_live, full,
            "stencil near MAX_BLEND_SURFACE_BYTES must band against remaining live room and match pixel-for-pixel"
        );

        // Sanity-check that the stencil actually painted the clipped top half and left
        // the bottom half white.
        let top_red = &full[((8 * 32 + 8) * 4) as usize..((8 * 32 + 8) * 4 + 4) as usize];
        let top_blue = &full[((8 * 32 + 10) * 4) as usize..((8 * 32 + 10) * 4 + 4) as usize];
        let bot_white = &full[((20 * 32 + 8) * 4) as usize..((20 * 32 + 8) * 4 + 4) as usize];
        assert!(
            top_red[0] > 240 && top_red[1] < 80 && top_red[2] < 80,
            "{top_red:?}"
        );
        assert!(
            top_blue[0] < 80 && top_blue[1] < 80 && top_blue[2] > 240,
            "{top_blue:?}"
        );
        assert_eq!(bot_white, &[255, 255, 255, 255]);
    }

    #[test]
    fn tiling_cache_stores_uniform_opaque_color_once_and_reuses_it() {
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 16.0, 16.0));
        let mut r = Path::new();
        r.rect(Rect::new(0.0, 0.0, 16.0, 16.0));
        cell.push(RenderCommand::FillPath {
            path: r,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.2, 0.4, 0.8)),
            alpha: 1.0,
            overprint: None,
        });
        let tiling = Arc::new(TilingPattern {
            commands: Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 16.0, 16.0),
            x_step: 16.0,
            y_step: 16.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });

        let mut renderer = CpuRenderer::new();
        renderer
            .begin_page(&PageRenderInfo {
                page_rect: Rect::new(0.0, 0.0, 32.0, 32.0),
                scale: 1.0,
                background: Color::white(),
            })
            .unwrap();
        let (_, _, _, _, u1) = renderer
            .prepare_tiling_paint_for(
                &tiling,
                &tiling.commands,
                0,
                None,
                BlendMode::Normal,
                None,
                1.0,
            )
            .expect("first prepare");
        assert!(u1.is_some(), "uniform opaque tile must cache Some(rgba)");
        assert_eq!(renderer.tiling_cache.len(), 1);
        let cached_entry = renderer.tiling_cache.values().next().unwrap();
        assert_eq!(cached_entry.uniform_opaque, u1);
        let (_, _, _, _, u2) = renderer
            .prepare_tiling_paint_for(
                &tiling,
                &tiling.commands,
                0,
                None,
                BlendMode::Normal,
                None,
                1.0,
            )
            .expect("cached prepare");
        assert_eq!(u2, u1, "cache hit must reuse stored uniform_opaque color");
    }

    #[test]
    fn type3_d0_colored_image_preserves_rgb_under_backdrop_dependent_tiling() {
        let d0_stream =
            b"1 0 d0 BI /W 2 /H 2 /CS /RGB /BPC 8 ID \xff\x00\x00\xff\x00\x00\xff\x00\x00\xff\x00\x00 EI";
        let d1_stream =
            b"1 0 0 0 1 1 d1 BI /W 8 /H 8 /BPC 1 /IM true ID \x00\x00\x00\x00\x00\x00\x00\x00 EI";
        let font = zpdf_font::LoadedFont::new_with_data(
            zpdf_font::PdfFontType::Type3 {
                font_matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
                char_procs: HashMap::from([
                    ("A".into(), Arc::<[u8]>::from(&d0_stream[..])),
                    ("B".into(), Arc::<[u8]>::from(&d1_stream[..])),
                ]),
                encoding: vec!["A".into(), "B".into()],
                widths: vec![1.0, 1.0],
                first_char: 0,
            },
            "Type3MixedBackdrop".into(),
            Vec::new(),
            zpdf_font::CidWidths::new(1.0),
        );
        let mut fonts = FontCache::new();
        let font_id = fonts.insert("F1".into(), font);

        // Backdrop-dependent Multiply blue tiling pattern with 0.5 glyph opacity
        // so it enters the multi-pass coverage branch.
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 8.0, 8.0));
        cell.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Multiply,
            isolated: false,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 8.0, 8.0),
            alpha: 1.0,
            mask: None,
        });
        let mut r = Path::new();
        r.rect(Rect::new(0.0, 0.0, 8.0, 8.0));
        cell.push(RenderCommand::FillPath {
            path: r,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        cell.push(RenderCommand::PopBlendGroup);
        let tiling = Arc::new(TilingPattern {
            commands: Arc::new(cell),
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
            alpha: 0.5,
            overprint: None,
            transform: zpdf_core::Matrix::translate(8.0, 10.0),
            h_scale: 1.0,
        }));

        let rendered = CpuRenderer::new()
            .with_fonts(&fonts)
            .render_display_list(&dl, 1.0)
            .expect("cpu render");
        let px = |x: u32, y: u32| -> [u8; 3] {
            let off = ((y * rendered.width + x) * 4) as usize;
            [
                rendered.data[off],
                rendered.data[off + 1],
                rendered.data[off + 2],
            ]
        };
        // 'A' (d0 opaque red image at alpha 0.5 over white) = pink [255, ~128, ~128].
        let a_px = px(18, 20);
        assert!(
            a_px[0] > 245 && (118..=138).contains(&a_px[1]) && (118..=138).contains(&a_px[2]),
            "d0 red image at 0.5 alpha under backdrop-dependent blue tiling must stay pink, got {a_px:?}"
        );
        // 'B' (d1 stencil with Multiply blue at alpha 0.5 over white) = light blue [~128, ~128, 255].
        let b_px = px(38, 20);
        assert!(
            (118..=138).contains(&b_px[0]) && (118..=138).contains(&b_px[1]) && b_px[2] > 245,
            "d1 stencil with backdrop-dependent blue tiling at 0.5 alpha must be light blue, got {b_px:?}"
        );
    }

    #[test]
    fn type3_d0_explicit_vector_colors_and_glyph_clips_under_backdrop_tiling() {
        // 'A' (`d0`): explicit red fill + explicit green stroke under backdrop-dependent blue tiling.
        let d0_vec = b"1 0 d0 1 0 0 rg 0 0 0.5 1 re f 0 1 0 RG 0.3 w 0.75 0.1 m 0.75 0.9 l S";
        // 'B' (`d1`): glyph-local clip on full-width image mask, restored with Q before second draw.
        let d1_clip = b"1 0 0 0 1 1 d1 \
            q 0 0 0.4 1 re W n \
            BI /W 8 /H 8 /BPC 1 /IM true ID \x00\x00\x00\x00\x00\x00\x00\x00 EI \
            Q 0.7 0 0.3 1 re f";
        let font = zpdf_font::LoadedFont::new_with_data(
            zpdf_font::PdfFontType::Type3 {
                font_matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
                char_procs: HashMap::from([
                    ("A".into(), Arc::<[u8]>::from(&d0_vec[..])),
                    ("B".into(), Arc::<[u8]>::from(&d1_clip[..])),
                ]),
                encoding: vec!["A".into(), "B".into()],
                widths: vec![1.0, 1.0],
                first_char: 0,
            },
            "Type3VecAndClip".into(),
            Vec::new(),
            zpdf_font::CidWidths::new(1.0),
        );
        let mut fonts = FontCache::new();
        let font_id = fonts.insert("F1".into(), font);

        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 8.0, 8.0));
        cell.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Multiply,
            isolated: false,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 8.0, 8.0),
            alpha: 1.0,
            mask: None,
        });
        let mut r = Path::new();
        r.rect(Rect::new(0.0, 0.0, 8.0, 8.0));
        cell.push(RenderCommand::FillPath {
            path: r,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        cell.push(RenderCommand::PopBlendGroup);
        let tiling = Arc::new(TilingPattern {
            commands: Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 8.0, 8.0),
            x_step: 8.0,
            y_step: 8.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 40.0));
        dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
            font_id,
            font_size: 40.0,
            glyphs: vec![
                PositionedGlyph {
                    glyph_id: 0,
                    code_len: 1,
                    code: b'A' as u32,
                    x: 0.0,
                    y: 0.0,
                    advance: 40.0,
                },
                PositionedGlyph {
                    glyph_id: 1,
                    code_len: 1,
                    code: b'B' as u32,
                    x: 40.0,
                    y: 0.0,
                    advance: 40.0,
                },
            ],
            paint: Paint::Tiling(tiling),
            alpha: 1.0,
            overprint: None,
            transform: zpdf_core::Matrix::translate(10.0, 0.0),
            h_scale: 1.0,
        }));

        let rendered = CpuRenderer::new()
            .with_fonts(&fonts)
            .render_display_list(&dl, 1.0)
            .expect("cpu render");
        let px = |x: u32, y: u32| -> [u8; 3] {
            let off = ((y * rendered.width + x) * 4) as usize;
            [
                rendered.data[off],
                rendered.data[off + 1],
                rendered.data[off + 2],
            ]
        };
        // 'A' at x=[10..50]: red fill at x=20, green stroke at x=40.
        assert!(
            px(20, 20)[0] > 240 && px(20, 20)[2] < 15,
            "{:?}",
            px(20, 20)
        );
        assert!(
            px(40, 20)[1] > 240 && px(40, 20)[2] < 15,
            "{:?}",
            px(40, 20)
        );
        // 'B' at x=[50..90]: clipped left=[50..66] blue, gap=[66..78] white, restored right=[78..90] blue.
        assert!(
            px(58, 20)[2] > 240 && px(58, 20)[0] < 15,
            "{:?}",
            px(58, 20)
        );
        assert_eq!(px(72, 20), [255, 255, 255]);
        assert!(
            px(84, 20)[2] > 240 && px(84, 20)[0] < 15,
            "{:?}",
            px(84, 20)
        );
    }

    #[test]
    fn inline_image_straight_alpha_matches_premultiplied_on_white() {
        let render_inline = |data: Vec<u8>, premultiplied: bool| -> [u8; 4] {
            let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 8.0, 8.0));
            dl.push(RenderCommand::DrawInlineImage(
                zpdf_display_list::InlineImageDraw {
                    image: Arc::new(zpdf_display_list::InlineImage {
                        width: 1,
                        height: 1,
                        data,
                        has_alpha: true,
                        is_image_mask: false,
                        premultiplied,
                    }),
                    transform: zpdf_core::Matrix::new(8.0, 0.0, 0.0, 8.0, 0.0, 0.0),
                    alpha: 1.0,
                },
            ));
            let page = CpuRenderer::new()
                .render_display_list(&dl, 1.0)
                .expect("render");
            let idx = ((4 * page.width + 4) * 4) as usize;
            page.data[idx..idx + 4].try_into().unwrap()
        };

        let straight = render_inline(vec![128, 0, 0, 128], false);
        let premul = render_inline(vec![64, 0, 0, 128], true);
        assert_eq!(
            straight, premul,
            "straight-alpha [128,0,0,128] must composite identically to premultiplied [64,0,0,128]"
        );
        assert!(
            (187..=195).contains(&straight[0])
                && (123..=131).contains(&straight[1])
                && (123..=131).contains(&straight[2])
                && straight[3] == 255,
            "expected ~[191, 127, 127, 255] on white, got {straight:?}"
        );
    }

    #[test]
    fn type3_soft_mask_shares_decoded_inline_images_across_placements() {
        use zpdf_font::{LoadedFont, PdfFontType, Type3FormXObject, Type3Resources, Type3SoftMask};

        // 2x1 mask image: left pixel white (visible), right pixel black (masked).
        let mask_img_stream = b"q 20 0 0 20 0 0 cm BI /W 2 /H 1 /BPC 8 /CS /G ID \xff\x00 EI Q";
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
            data: Arc::from(&mask_img_stream[..]),
            resources: None,
        });
        let mut t3_res = Type3Resources::default();
        t3_res.ext_g_state_smasks.insert(
            "GSImgMask".into(),
            Type3SoftMask::Dict {
                is_alpha: false,
                group,
                backdrop_luma: 0.0,
                transfer: None,
            },
        );
        // Include both an explicit colored fill and an inherited fill so
        // `render_type3_backdrop_tiling` exercises its explicit-colored branch too.
        let charproc = b"20 0 d0 /GSImgMask gs 0 0 20 20 re f 1 0 0 rg 0 0 20 20 re f";
        let font = LoadedFont::new_with_data(
            PdfFontType::Type3 {
                font_matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
                char_procs: HashMap::from([("A".into(), Arc::<[u8]>::from(&charproc[..]))]),
                encoding: vec!["A".into()],
                widths: vec![20.0],
                first_char: 0,
            },
            "Type3SharedMaskImg".into(),
            Vec::new(),
            zpdf_font::CidWidths::new(20.0),
        )
        .with_type3_resources(Arc::new(t3_res));
        let mut fonts = FontCache::new();
        let font_id = fonts.insert("F1".into(), font);

        let glyph_at = |x: f32| PositionedGlyph {
            glyph_id: 0,
            code_len: 1,
            code: b'A' as u32,
            x,
            y: 0.0,
            advance: 20.0,
        };
        let extract_mask_inline_arcs = |renderer: &CpuRenderer<'_>| {
            let mut arcs: Vec<Arc<zpdf_display_list::InlineImage>> = Vec::new();
            for (dl, _, _) in renderer.soft_mask_planes.values() {
                for cmd in &dl.commands {
                    if let RenderCommand::DrawInlineImage(draw) = cmd {
                        arcs.push(Arc::clone(&draw.image));
                    }
                }
            }
            arcs
        };

        // 1. Solid Type 3 path (`render_type3_glyph_commands`).
        let mut solid_renderer = CpuRenderer::new().with_fonts(&fonts);
        solid_renderer
            .begin_page(&PageRenderInfo {
                page_rect: Rect::new(0.0, 0.0, 60.0, 20.0),
                scale: 1.0,
                background: Color::white(),
            })
            .expect("begin solid");
        solid_renderer
            .execute(&RenderCommand::DrawGlyphRun(GlyphRun {
                font_id,
                font_size: 1.0,
                glyphs: vec![glyph_at(0.0), glyph_at(30.0)],
                paint: Paint::Solid(Color::black()),
                alpha: 1.0,
                overprint: None,
                transform: zpdf_core::Matrix::identity(),
                h_scale: 1.0,
            }))
            .expect("execute solid");
        let solid_arcs = extract_mask_inline_arcs(&solid_renderer);
        assert_eq!(
            solid_arcs.len(),
            4,
            "two separated placements × two masked fills should retain four mask entries"
        );
        assert!(
            solid_arcs.iter().all(|a| Arc::ptr_eq(&solid_arcs[0], a)),
            "solid Type 3 replay must share one Arc<InlineImage> across all placements"
        );
        let solid_page = solid_renderer.end_page().expect("end solid");
        for bx in [0u32, 30] {
            let left_idx = ((10 * solid_page.width + bx + 5) * 4) as usize;
            let right_idx = ((10 * solid_page.width + bx + 15) * 4) as usize;
            assert!(solid_page.data[left_idx] > 240 && solid_page.data[left_idx + 1] < 15);
            assert!(solid_page.data[right_idx] > 240 && solid_page.data[right_idx + 1] > 240);
        }

        // 2. Backdrop-dependent tiling Type 3 path (`render_type3_backdrop_tiling`).
        let mut tile_dl = DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0));
        tile_dl.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Multiply,
            isolated: false,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
            alpha: 1.0,
            mask: None,
        });
        tile_dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 10.0, 10.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        tile_dl.push(RenderCommand::PopBlendGroup);
        let tiling = Arc::new(TilingPattern {
            commands: Arc::new(tile_dl),
            bbox: Rect::new(0.0, 0.0, 10.0, 10.0),
            x_step: 10.0,
            y_step: 10.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });
        let mut tiling_renderer = CpuRenderer::new().with_fonts(&fonts);
        tiling_renderer
            .begin_page(&PageRenderInfo {
                page_rect: Rect::new(0.0, 0.0, 60.0, 20.0),
                scale: 1.0,
                background: Color::white(),
            })
            .expect("begin tiling");
        tiling_renderer
            .execute(&RenderCommand::DrawGlyphRun(GlyphRun {
                font_id,
                font_size: 1.0,
                glyphs: vec![glyph_at(0.0), glyph_at(30.0)],
                paint: Paint::Tiling(tiling),
                alpha: 1.0,
                overprint: None,
                transform: zpdf_core::Matrix::identity(),
                h_scale: 1.0,
            }))
            .expect("execute tiling");
        let tiling_arcs = extract_mask_inline_arcs(&tiling_renderer);
        assert!(
            tiling_arcs.len() >= 2,
            "expected at least 2 retained mask entries across separated placements, got {}",
            tiling_arcs.len()
        );
        assert!(
            tiling_arcs.iter().all(|a| Arc::ptr_eq(&tiling_arcs[0], a)),
            "tiling Type 3 replay must share one Arc<InlineImage> across all placements"
        );
        let _ = tiling_renderer.end_page().expect("end tiling");
    }

    #[test]
    fn type3_soft_mask_nonuniform_text_scale_preserves_directional_stroke_and_clip_stroke() {
        use zpdf_font::{LoadedFont, PdfFontType, Type3FormXObject, Type3Resources, Type3SoftMask};

        // Mask draws a 12-unit-wide white cross centered at glyph (40, 40):
        //   vertical bar x=40, y=[10..70] -> under 0.5x H-scale, becomes x=20 with width 6 (x=[17..23])
        //   horizontal bar y=40, x=[10..70] -> under 1.0x V-scale, stays y=40 with thickness 12 (y=[34..46])
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
            PdfFontType::Type3 {
                font_matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
                char_procs: HashMap::from([("A".into(), Arc::<[u8]>::from(&charproc[..]))]),
                encoding: vec!["A".into()],
                widths: vec![80.0],
                first_char: 0,
            },
            "Type3NonUniformMask".into(),
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
        let page = CpuRenderer::new()
            .with_fonts(&fonts)
            .render_display_list(&dl, 1.0)
            .expect("render");
        let px = |x: u32, pdf_y: u32| -> [u8; 3] {
            let py = page.height - 1 - pdf_y;
            let idx = ((py * page.width + x) * 4) as usize;
            [page.data[idx], page.data[idx + 1], page.data[idx + 2]]
        };
        // Vertical stroke at x=20, width 6 -> x=[17..23] at y=60:
        // x=22 is inside (red), x=24 is outside (white; averaged 75% width=9 would reach 24.5!).
        let v_in = px(22, 60);
        let v_out = px(24, 60);
        assert!(
            v_in[0] > 240 && v_in[1] < 15 && v_in[2] < 15,
            "vertical mask stroke at x=22 must be red (width=6), got {v_in:?}"
        );
        assert!(
            v_out[0] > 240 && v_out[1] > 240 && v_out[2] > 240,
            "vertical mask stroke at x=24 must be white (not averaged width=9), got {v_out:?}"
        );
        // Horizontal stroke at y=40, thickness 12 -> y=[34..46] at x=10:
        // y=45 is inside (red; averaged 75% thickness=9 would stop at 44.5!), y=47 is outside (white).
        let h_in = px(10, 45);
        let h_out = px(10, 47);
        assert!(
            h_in[0] > 240 && h_in[1] < 15 && h_in[2] < 15,
            "horizontal mask stroke at y=45 must be red (thickness=12), got {h_in:?}"
        );
        assert!(
            h_out[0] > 240 && h_out[1] > 240 && h_out[2] > 240,
            "horizontal mask stroke at y=47 must be white, got {h_out:?}"
        );

        // Also verify `PushClipStroke` preserves directional widths under 0.5x H-scale.
        let mut cross = Path::new();
        cross.move_to(Point::new(40.0, 10.0));
        cross.line_to(Point::new(40.0, 70.0));
        cross.move_to(Point::new(10.0, 40.0));
        cross.line_to(Point::new(70.0, 40.0));
        let mut clip_mask_dl = DisplayList::new(Rect::new(0.0, 0.0, 80.0, 80.0));
        clip_mask_dl.push(RenderCommand::PushClipStroke {
            path: cross,
            style: StrokeStyle {
                width: 12.0,
                ..StrokeStyle::default()
            },
        });
        clip_mask_dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 80.0, 80.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        clip_mask_dl.push(RenderCommand::PopClip);
        let src_mask = SoftMask {
            kind: SoftMaskKind::Luminosity,
            commands: Arc::new(clip_mask_dl),
            offset: (0.0, 0.0),
            backdrop_luma: 0.0,
            transfer: None,
        };
        let mapped = src_mask.transform_with_images(
            &zpdf_core::Matrix::scale(0.5, 1.0),
            &|_| None,
            &|p, s, m| transform_stroke_to_page(p, s, m, 1.0),
        );
        let mut clip_dl = DisplayList::new(Rect::new(0.0, 0.0, 60.0, 80.0));
        clip_dl.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Normal,
            isolated: true,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 60.0, 80.0),
            alpha: 1.0,
            mask: Some(mapped),
        });
        clip_dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 60.0, 80.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        clip_dl.push(RenderCommand::PopBlendGroup);
        let clip_page = CpuRenderer::new()
            .render_display_list(&clip_dl, 1.0)
            .expect("render clip stroke mask");
        let cpx = |x: u32, pdf_y: u32| -> [u8; 3] {
            let py = clip_page.height - 1 - pdf_y;
            let idx = ((py * clip_page.width + x) * 4) as usize;
            [
                clip_page.data[idx],
                clip_page.data[idx + 1],
                clip_page.data[idx + 2],
            ]
        };
        assert!(cpx(22, 60)[0] > 240 && cpx(22, 60)[1] < 15);
        assert!(cpx(24, 60)[0] > 240 && cpx(24, 60)[1] > 240);
        assert!(cpx(10, 45)[0] > 240 && cpx(10, 45)[1] < 15);
        assert!(cpx(10, 47)[0] > 240 && cpx(10, 47)[1] > 240);
    }
}
