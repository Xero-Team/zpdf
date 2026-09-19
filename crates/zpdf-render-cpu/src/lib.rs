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
    /// Page rect bounds (supports CropBox / nonzero MediaBox origins):
    /// device x = (x - rect_x0) * scale, device y = (rect_y1 - y) * scale.
    rect_x0: f32,
    rect_y0: f32,
    rect_y1: f32,
    font_cache: Option<&'a FontCache>,
    image_cache: Option<&'a ImageCache>,
    /// Maximum pixels in one output raster. Oversized pages are uniformly
    /// downscaled before allocation so the complete page still renders.
    max_page_pixels: u64,
    /// Maximum live transparency-group nesting, including groups inside masks.
    max_blend_group_depth: usize,
    /// Maximum retained one-byte soft-mask coverage planes per page.
    max_soft_mask_cache_bytes: u64,
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
    /// Bytes retained by the active clip plus saved masks on the clip stack.
    clip_mask_bytes: u64,
    blend_stack: Vec<BlendEntry>,
    /// Bytes retained by the live page/group pixmap plus parked blend parents.
    blend_surface_bytes: u64,
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
    /// keyed by mask identity. Tiling patterns attach the same mask (modulo
    /// offset) to every painted command of every tile; without this the full
    /// page-sized mask would re-rasterize per command. Cleared per page —
    /// the keys are Arc pointers, only stable while the display list lives.
    soft_mask_planes: HashMap<SoftMaskPlaneKey, Arc<[u8]>>,
    soft_mask_cache_bytes: u64,
    /// Current recursive soft-mask depth (guards malicious/cyclic mask graphs).
    soft_mask_depth: usize,
    /// Box-filtered image variants reused by repeated draws at the same device
    /// footprint (common for patterns and repeated XObjects).
    downscaled_images: HashMap<(ImageId, u32, u32), Arc<[u8]>>,
    /// Opt-in per-stage timings. `None` means "not collecting": every [`Self::tick`]
    /// then returns `None` and the hot path pays one branch, no clock read.
    /// See [`zpdf_render::StageStats`] for why collection is opt-in, and for the
    /// caveat that it makes the measured path slightly slower than production.
    stage_stats: Option<zpdf_render::StageStats>,
    /// Page start, read only while collecting, for `StageStats::total_ns`.
    page_start: Option<zpdf_core::time::Instant>,
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

fn unit(v: f32) -> f32 {
    if v.is_finite() {
        v.clamp(0.0, 1.0)
    } else {
        0.0
    }
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
    /// The coordinates are integers below 2^24 (a raster dimension), so the
    /// `u32` → `f32` conversion is exact.
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
const DEFAULT_RENDER_BUDGET: std::time::Duration = std::time::Duration::from_secs(8);

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
            rect_x0: 0.0,
            rect_y0: 0.0,
            rect_y1: 0.0,
            font_cache: None,
            image_cache: None,
            max_page_pixels: limits.max_page_pixels,
            max_blend_group_depth: limits.max_blend_group_depth as usize,
            max_soft_mask_cache_bytes: limits.max_softmask_cache_bytes,
            clip_stack: Vec::new(),
            current_clip: None,
            clip_bounds: None,
            clip_pixel_spent: 0,
            clip_mask_bytes: 0,
            blend_stack: Vec::new(),
            blend_surface_bytes: 0,
            skipped_blend_depth: 0,
            render_budget: Some(DEFAULT_RENDER_BUDGET),
            deadline: None,
            over_budget: false,
            soft_mask_planes: HashMap::new(),
            soft_mask_cache_bytes: 0,
            soft_mask_depth: 0,
            downscaled_images: HashMap::new(),
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

    /// Convert PDF Y coordinate (origin bottom-left) to pixel Y (origin top-left),
    /// relative to the page rect's top edge.
    fn flip_y(&self, y: f32) -> f32 {
        (self.rect_y1 - y) * self.scale
    }

    fn to_pixel_x(&self, x: f32) -> f32 {
        (x - self.rect_x0) * self.scale
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
        PaintedBounds {
            x0: (left - grow).floor().clamp(0.0, width as f32) as u32,
            y0: (top - grow).floor().clamp(0.0, height as f32) as u32,
            x1: (right + grow).ceil().clamp(0.0, width as f32) as u32,
            y1: (bottom + grow).ceil().clamp(0.0, height as f32) as u32,
        }
    }

    fn build_skia_path(&self, path: &Path) -> Option<tiny_skia::Path> {
        let mut pb = tiny_skia::PathBuilder::new();
        for elem in &path.elements {
            match *elem {
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
            (color.r * 255.0) as u8,
            (color.g * 255.0) as u8,
            (color.b * 255.0) as u8,
            (color.a * alpha * 255.0) as u8,
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

    fn render_fill(&mut self, path: &Path, rule: &FillRule, paint_spec: &Paint, alpha: f32) {
        let t = self.tick();
        let Some(skia_path) = self.build_skia_path(path) else {
            return self.tock(t, |s| &mut s.fill_ns);
        };
        let paint = match paint_spec {
            Paint::Solid(c) => Self::color_to_paint(c, alpha),
            // Non-solid paints are resolved by the interpreter into images
            // before they reach a backend, so Pattern/Shading here paints
            // nothing (see `StageStats`: shading is an interpret-stage cost).
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
            // One device pixel of slack: a pixel the path only clips the corner
            // of still receives anti-aliased coverage.
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
        let paint = match paint_spec {
            Paint::Solid(c) => Self::color_to_paint(c, alpha),
            _ => return self.tock(t, |s| &mut s.stroke_ns),
        };
        let stroke = self.build_skia_stroke(style);
        let mut painted = false;
        if let Some(ref mut pixmap) = self.pixmap {
            pixmap.stroke_path(
                &skia_path,
                &paint,
                &stroke,
                tiny_skia::Transform::identity(),
                self.current_clip.as_ref(),
            );
            painted = true;
        }
        if painted {
            // The *centerline's* bounds say nothing about how far the ink
            // reaches: joins, caps and miters all extend past it.
            let grow = stroke_overshoot(stroke.width, stroke.miter_limit);
            self.mark_painted(skia_path.bounds(), grow);
        }
        self.tock(t, |s| &mut s.stroke_ns);
        self.count(painted as u64, |s| &mut s.strokes);
    }

    /// Device-space tiny-skia stroke parameters for `style`.
    fn build_skia_stroke(&self, style: &StrokeStyle) -> tiny_skia::Stroke {
        tiny_skia::Stroke {
            // Hairline boost: never let a stroke fall below one device pixel
            // (matches pdfium; keeps thin diagram strokes legible at low DPI,
            // and renders PDF zero-width strokes as 1px hairlines).
            width: (style.width * self.scale).max(1.0),
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
            dash: self.build_stroke_dash(style),
        }
    }

    /// Build a device-space tiny-skia dash from the stroke style. Degenerate
    /// patterns (empty/all-zero/negative entries) return `None` → solid stroke.
    /// PDF allows odd-length dash arrays (the pattern repeats, so `[3]` means
    /// 3 on / 3 off); tiny-skia requires an even count, so odd arrays are doubled.
    fn build_stroke_dash(&self, style: &StrokeStyle) -> Option<tiny_skia::StrokeDash> {
        let dash = style.dash.as_ref()?;
        if zpdf_render::dash::is_degenerate(&dash.array) {
            return None;
        }
        let mut array: Vec<f32> = dash.array.iter().map(|v| v * self.scale).collect();
        if array.len() % 2 == 1 {
            // Avoid clone: reserve and extend from within the same vec
            array.reserve(array.len());
            let len = array.len();
            for i in 0..len {
                array.push(array[i]);
            }
        }
        tiny_skia::StrokeDash::new(array, dash.phase * self.scale)
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

        let paint = match &run.paint {
            Paint::Solid(c) => Self::color_to_paint(c, alpha_override),
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
            self.render_type3_glyphs(run, font, &paint);
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
        let mask_bytes = pw as u64 * ph as u64;

        // Budget guard: once the page has spent its clip-mask pixel-work, stop
        // building new full-raster masks (which would hang on 100k-clip pages).
        // The matching PopClip pops a `Skipped` frame and the previously-active
        // clip stays in force — strictly tighter than dropping clipping entirely.
        if self.clip_pixel_spent >= MAX_CLIP_PIXEL_WORK
            || self.clip_mask_bytes.saturating_add(mask_bytes) > MAX_CLIP_MASK_BYTES
        {
            self.clip_stack.push(ClipFrame::Skipped);
            return self.tock(t, |s| &mut s.clip_ns);
        }

        let Some(skia_path) = self.build_skia_path(path) else {
            // A non-finite/out-of-range clip path: keep push/pop balanced.
            self.clip_stack.push(ClipFrame::Skipped);
            return self.tock(t, |s| &mut s.clip_ns);
        };

        let mut mask = match tiny_skia::Mask::new(pw, ph) {
            Some(m) => m,
            None => {
                self.clip_stack.push(ClipFrame::Skipped);
                return self.tock(t, |s| &mut s.clip_ns);
            }
        };
        self.clip_pixel_spent += pw as u64 * ph as u64;
        self.clip_mask_bytes += mask_bytes;

        mask.fill_path(
            &skia_path,
            Self::fill_rule_to_skia(rule),
            true, // anti-alias clip edges (fills/strokes are AA'd too)
            tiny_skia::Transform::identity(),
        );

        // Intersect with the current clip, but only across the new clip path's
        // device bbox: outside it the new mask is already 0, so the intersection
        // there is 0 too and need not be touched. This turns the per-clip
        // intersection from O(raster) into O(clip bbox).
        if let Some(ref current) = self.current_clip {
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
                    mask_data[i] = ((mask_data[i] as u16 * current_data[i] as u16) / 255) as u8;
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
        self.clip_bounds = Some(Self::drawn_extent(skia_path.bounds(), 1.0, pw, ph));
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
                    self.clip_mask_bytes = self
                        .clip_mask_bytes
                        .saturating_sub(removed.width() as u64 * removed.height() as u64);
                }
                self.clip_bounds = prev_bounds;
            }
            Some(ClipFrame::Empty) => {
                if let Some(mask) = self.current_clip.take() {
                    self.clip_mask_bytes = self
                        .clip_mask_bytes
                        .saturating_sub(mask.width() as u64 * mask.height() as u64);
                }
                self.clip_bounds = None;
            }
            None => {}
        }
        self.tock(t, |s| &mut s.clip_ns);
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
        let mask_bytes = pw as u64 * ph as u64;

        if self.clip_pixel_spent >= MAX_CLIP_PIXEL_WORK
            || self.clip_mask_bytes.saturating_add(mask_bytes) > MAX_CLIP_MASK_BYTES
        {
            self.clip_stack.push(ClipFrame::Skipped);
            return;
        }
        let Some(skia_path) = self.build_skia_path(path) else {
            self.clip_stack.push(ClipFrame::Skipped);
            return;
        };
        let Some(mut scratch) = tiny_skia::Pixmap::new(pw, ph) else {
            self.clip_stack.push(ClipFrame::Skipped);
            return;
        };
        self.clip_pixel_spent += pw as u64 * ph as u64;
        self.clip_mask_bytes += mask_bytes;

        let mut paint = tiny_skia::Paint::default();
        paint.set_color(tiny_skia::Color::WHITE);
        paint.anti_alias = true;
        let stroke = self.build_skia_stroke(style);
        scratch.stroke_path(
            &skia_path,
            &paint,
            &stroke,
            tiny_skia::Transform::identity(),
            None,
        );
        let mut mask = tiny_skia::Mask::from_pixmap(scratch.as_ref(), tiny_skia::MaskType::Alpha);

        // Intersect with the active clip over the stroke's device bbox (the
        // centerline bounds grown by the stroke width). Outside that box the
        // stroke mask is already 0, so the intersection is 0 there too.
        if let Some(ref current) = self.current_clip {
            let b = skia_path.bounds();
            let grow = stroke.width.max(1.0);
            let w = pw as usize;
            let h = ph as usize;
            let x0 = ((b.left() - grow).floor().max(0.0) as usize).min(w);
            let x1 = ((b.right() + grow).ceil().max(0.0) as usize).min(w);
            let y0 = ((b.top() - grow).floor().max(0.0) as usize).min(h);
            let y1 = ((b.bottom() + grow).ceil().max(0.0) as usize).min(h);
            let current_data = current.data();
            let mask_data = mask.data_mut();
            for y in y0..y1 {
                let row = y * w;
                for x in x0..x1 {
                    let i = row + x;
                    mask_data[i] = ((mask_data[i] as u16 * current_data[i] as u16) / 255) as u8;
                }
            }
        }

        let frame = match self.current_clip.take() {
            Some(old) => ClipFrame::Mask(old, self.clip_bounds),
            None => ClipFrame::Empty,
        };
        self.clip_stack.push(frame);
        self.current_clip = Some(mask);
        // Same reasoning as `push_clip`, with the stroke's own overshoot: the
        // centerline bbox says nothing about where the stroked outline reaches.
        let grow = stroke_overshoot(stroke.width, stroke.miter_limit);
        self.clip_bounds = Some(Self::drawn_extent(skia_path.bounds(), grow, pw, ph));
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
        let depth = self.soft_mask_depth.saturating_add(self.blend_stack.len());
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
                });
            }
            return;
        }

        let pixmap = match self.pixmap.take() {
            Some(p) => p,
            None => return,
        };
        let w = pixmap.width();
        let h = pixmap.height();
        let surface_bytes = w as u64 * h as u64 * 4;

        if self.blend_surface_bytes.saturating_add(surface_bytes) > MAX_BLEND_SURFACE_BYTES {
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
                });
            }
            return;
        }

        // The group renders into its own transparent buffer (isolated). A
        // non-isolated group that reached this point carries a group-level
        // effect (alpha/mask/blend) and is approximated as isolated — element
        // blends against the backdrop are the rare loss. `None` on allocation
        // failure degrades to a no-op group, which `pop_blend_group` handles.
        let group = tiny_skia::Pixmap::new(w, h);
        // Per-element knockout composites against the (transparent) initial
        // backdrop preserved here.
        // This backend currently approximates every non-passthrough group as
        // isolated, so its initial backdrop is transparent. Do not clone the
        // full-page transparent pixmap merely to represent that fact.
        let backdrop = None;

        // The backdrop's painted region is parked with it; the group gets a
        // fresh, empty surface of its own.
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
        });

        self.pixmap = group;
        if self.pixmap.is_some() {
            self.blend_surface_bytes += surface_bytes;
        }
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
                    let m = m as u16;
                    px[0] = ((px[0] as u16 * m) / 255) as u8;
                    px[1] = ((px[1] as u16 * m) / 255) as u8;
                    px[2] = ((px[2] as u16 * m) / 255) as u8;
                    px[3] = ((px[3] as u16 * m) / 255) as u8;
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
            .saturating_sub(group_pixmap.width() as u64 * group_pixmap.height() as u64 * 4);
        self.tock(t, |s| &mut s.soft_mask_ns);
    }

    /// Dispatch the four painting commands to their renderers (shared by the
    /// normal path and the knockout path).
    fn render_paint_cmd(&mut self, cmd: &RenderCommand) {
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
        match cmd {
            RenderCommand::FillPath {
                path, rule, paint, ..
            } => self.render_fill(path, rule, paint, 1.0),
            RenderCommand::StrokePath {
                path, style, paint, ..
            } => self.render_stroke(path, style, paint, 1.0),
            RenderCommand::DrawGlyphRun(run) => self.render_glyph_run_with_alpha(run, 1.0),
            RenderCommand::DrawImage(draw) => self.render_image_with_alpha(draw, 1.0),
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
        if self.blend_surface_bytes.saturating_add(scratch_bytes) > MAX_BLEND_SURFACE_BYTES {
            tracing::warn!("knockout scratch surfaces exceed memory budget; using accumulation");
            self.render_paint_cmd(cmd);
            return;
        }

        // Pass 1: the element as painted (real colour and opacity).
        // The scratch passes below draw into throwaway surfaces, so the region
        // the *canvas* has been painted in is put aside and restored after them.
        let canvas_dirty = self.dirty;
        let group = self.pixmap.take();
        self.pixmap = tiny_skia::Pixmap::new(w, h);
        if self.pixmap.is_none() {
            self.pixmap = group;
            self.render_paint_cmd(cmd); // OOM: fall back to accumulation
            return;
        }
        self.render_paint_cmd(cmd);
        let elem = self.pixmap.take();

        // Pass 2: the element at full opacity → its shape (alpha = coverage).
        self.pixmap = tiny_skia::Pixmap::new(w, h);
        if self.pixmap.is_none() {
            self.pixmap = group;
            return;
        }
        self.render_shape_cmd(cmd);
        let shape = self.pixmap.take();

        self.pixmap = group;
        self.dirty = canvas_dirty;
        let (elem, shape) = match (elem, shape) {
            (Some(e), Some(s)) => (e, s),
            _ => return,
        };
        if let (Some(g), Some(entry)) = (self.pixmap.as_mut(), self.blend_stack.last()) {
            let b0 = entry.backdrop.as_ref().map(|p| p.data());
            knockout_merge(g.data_mut(), elem.data(), shape.data(), b0);
        }
        // The merge walks the whole raster (it reads each pixel's coverage to
        // decide whether to replace or keep), so no useful bound survives it.
        self.mark_all_painted();
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
        if self.blend_surface_bytes.saturating_add(scratch_bytes) > MAX_BLEND_SURFACE_BYTES {
            tracing::warn!("overprint scratch surface exceeds memory budget; using source-over");
            self.render_paint_cmd(cmd);
            return;
        }
        // Render the element (real colour/opacity, current clip) into a scratch
        // buffer; only its alpha channel — the covered fraction — is consumed.
        // As in `knockout_paint`, the canvas's painted region is put aside while
        // the scratch surface is the target.
        let canvas_dirty = self.dirty;
        let canvas = self.pixmap.take();
        self.pixmap = tiny_skia::Pixmap::new(w, h);
        if self.pixmap.is_none() {
            // OOM: fall back to a normal paint so content still appears.
            self.pixmap = canvas;
            self.render_paint_cmd(cmd);
            return;
        }
        self.render_paint_cmd(cmd);
        let elem = self.pixmap.take();
        self.pixmap = canvas;
        self.dirty = canvas_dirty;
        if let (Some(canvas), Some(elem)) = (self.pixmap.as_mut(), elem) {
            overprint_merge(canvas.data_mut(), elem.data(), op);
        }
        // The merge reads and writes whole pixels across the raster.
        self.mark_all_painted();
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
        if self.blend_surface_bytes.saturating_add(mask_work_bytes) > MAX_BLEND_SURFACE_BYTES {
            tracing::warn!("soft-mask scratch exceeds memory budget; skipping mask");
            return None;
        }

        // Timed from here: the guards above do no real work, while everything
        // below is either a plane shift or a full mask-group sub-render.
        let t = self.tick();

        // Page-space offset → device pixels (device y grows downward).
        let dx = (mask.offset.0 * self.scale).round() as i64;
        let dy = (-mask.offset.1 * self.scale).round() as i64;

        let key = SoftMaskPlaneKey::new(mask);
        if let Some(base) = self.soft_mask_planes.get(&key) {
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
            self.soft_mask_planes.insert(key, Arc::clone(&base));
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
            rect_x0: self.rect_x0,
            rect_y0: self.rect_y0,
            rect_y1: self.rect_y1,
            font_cache: self.font_cache,
            image_cache: self.image_cache,
            max_page_pixels: self.max_page_pixels,
            max_blend_group_depth: self.max_blend_group_depth,
            max_soft_mask_cache_bytes: self.max_soft_mask_cache_bytes,
            clip_stack: Vec::new(),
            current_clip: None,
            clip_bounds: None,
            clip_pixel_spent: 0,
            clip_mask_bytes: 0,
            blend_stack: Vec::new(),
            blend_surface_bytes: w as u64 * h as u64 * 4,
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
                .saturating_add(1),
            downscaled_images: HashMap::new(),
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
        let Some(expected_len) = (image.width as usize)
            .checked_mul(image.height as usize)
            .and_then(|n| n.checked_mul(4))
        else {
            tracing::warn!(
                image_id = draw.image_id,
                "skipping overflowing image dimensions"
            );
            return;
        };
        if image.width == 0 || image.height == 0 || image.data.len() != expected_len {
            tracing::warn!(
                image_id = draw.image_id,
                width = image.width,
                height = image.height,
                bytes = image.data.len(),
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
        let tm = &draw.transform;
        let s = self.scale;
        let (a, b, c, d, e, f) = (
            tm.a as f32,
            tm.b as f32,
            tm.c as f32,
            tm.d as f32,
            tm.e as f32,
            tm.f as f32,
        );

        // Device-space footprint of the unit square's axes: how many device
        // pixels one image axis spans (column lengths of the scaled CTM).
        let dev_w = (a * a + b * b).sqrt() * s;
        let dev_h = (c * c + d * d).sqrt() * s;
        let fx = dev_w / image.width as f32;
        let fy = dev_h / image.height as f32;

        // Below ~0.5x per axis, bilinear sampling starts skipping source pixels
        // entirely (thin strokes in downscaled scans break up). Pre-downscale
        // with a box filter (plain average) to the target device scale first.
        let mut downscaled: Option<(Arc<[u8]>, u32, u32)> = None;
        if fx < 0.5 || fy < 0.5 {
            let tw = ((image.width as f32 * fx.min(1.0)).ceil() as u32).clamp(1, image.width);
            let th = ((image.height as f32 * fy.min(1.0)).ceil() as u32).clamp(1, image.height);
            if tw < image.width || th < image.height {
                let key = (draw.image_id, tw, th);
                let data = if cache_downscaled {
                    if let Some(data) = self.downscaled_images.get(&key) {
                        Arc::clone(data)
                    } else {
                        let data: Arc<[u8]> = Arc::from(box_downscale_rgba(
                            &image.data,
                            image.width,
                            image.height,
                            tw,
                            th,
                        ));
                        let retained: usize =
                            self.downscaled_images.values().map(|d| d.len()).sum();
                        if data.len() <= MAX_DOWNSCALED_IMAGE_CACHE_BYTES {
                            if retained.saturating_add(data.len())
                                > MAX_DOWNSCALED_IMAGE_CACHE_BYTES
                            {
                                self.downscaled_images.clear();
                            }
                            self.downscaled_images.insert(key, Arc::clone(&data));
                        }
                        data
                    }
                } else {
                    Arc::from(box_downscale_rgba(
                        &image.data,
                        image.width,
                        image.height,
                        tw,
                        th,
                    ))
                };
                downscaled = Some((data, tw, th));
            }
        }
        let (data, w, h) = match &downscaled {
            Some((data, w, h)) => (data.as_ref(), *w, *h),
            None => (image.data.as_slice(), image.width, image.height),
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
            a * s / iw,                 // sx: screen_x per ix
            -b * s / iw,                // ky: screen_y per ix
            -c * s / ih,                // kx: screen_x per iy
            d * s / ih,                 // sy: screen_y per iy
            (c + e - self.rect_x0) * s, // tx
            (self.rect_y1 - d - f) * s, // ty
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
            let outline = font.glyph_outline(glyph.glyph_id);
            self.tock(t_parse, |s| &mut s.outline_parse_ns);
            let Some(outline) = outline else {
                continue;
            };
            self.count(1, |s| &mut s.glyph_outlines_parsed);

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
        let px = (page_x as f32 - self.rect_x0) * self.scale;
        let py = (self.rect_y1 - page_y as f32) * self.scale;
        (px, py)
    }

    fn render_type3_glyphs(
        &mut self,
        run: &GlyphRun,
        font: &zpdf_font::LoadedFont,
        paint: &tiny_skia::Paint<'_>,
    ) {
        use zpdf_content::interpreter::ContentInterpreter;

        // run.transform = CTM * Tm (already includes all coordinate transforms)
        // The CTM may already flip Y (e.g., Skia PDFs use negative y scale).
        // We must apply the full transform chain: FontMatrix * glyph_coords → text space,
        // then Tm * text_coords → CTM-transformed space, then to pixels.
        let tm = &run.transform;
        let font_size = run.font_size;
        let h_scale = run.h_scale;

        for glyph in &run.glyphs {
            let (stream, font_matrix) = match font.type3_glyph_stream(glyph.glyph_id) {
                Some(s) => s,
                None => continue,
            };

            let glyph_rect = zpdf_core::Rect::new(0.0, -1000.0, 1000.0, 1000.0);
            let mut glyph_images = ImageCache::new();
            let glyph_dl = ContentInterpreter::new(glyph_rect)
                .with_images(&mut glyph_images)
                .interpret(stream);

            // Build the full transform: glyph_space → page_space → pixel_space
            // FontMatrix transforms glyph coords to text space (typically 0.001 scale)
            // Then the text matrix (tm) transforms text space to page space (already in run.transform)
            // glyph.x is the accumulated horizontal offset in page space from the interpreter

            for cmd in &glyph_dl.commands {
                match cmd {
                    RenderCommand::FillPath { path, rule, .. } => {
                        if let Some(skia_path) = self.build_type3_transformed_path(
                            path,
                            &font_matrix,
                            font_size,
                            h_scale,
                            tm,
                            glyph.x,
                        ) {
                            if let Some(ref mut pixmap) = self.pixmap {
                                pixmap.fill_path(
                                    &skia_path,
                                    paint,
                                    Self::fill_rule_to_skia(rule),
                                    tiny_skia::Transform::identity(),
                                    self.current_clip.as_ref(),
                                );
                            }
                            // Type3 ink is an ordinary device path here, so it
                            // bounds exactly like any other fill's.
                            self.mark_painted(skia_path.bounds(), 1.0);
                        }
                    }
                    RenderCommand::StrokePath { path, style, .. } => {
                        if let Some(skia_path) = self.build_type3_transformed_path(
                            path,
                            &font_matrix,
                            font_size,
                            h_scale,
                            tm,
                            glyph.x,
                        ) {
                            let stroke = tiny_skia::Stroke {
                                width: (style.width
                                    * font_matrix[0].abs() as f32
                                    * font_size
                                    * self.scale)
                                    .max(0.5),
                                ..Default::default()
                            };
                            if let Some(ref mut pixmap) = self.pixmap {
                                pixmap.stroke_path(
                                    &skia_path,
                                    paint,
                                    &stroke,
                                    tiny_skia::Transform::identity(),
                                    self.current_clip.as_ref(),
                                );
                            }
                            let grow = stroke_overshoot(stroke.width, stroke.miter_limit);
                            self.mark_painted(skia_path.bounds(), grow);
                        }
                    }
                    RenderCommand::DrawImage(draw) => {
                        let glyph_to_page = tm
                            .concat(&zpdf_core::Matrix::translate(glyph.x as f64, 0.0))
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
                            .concat(&draw.transform);
                        let draw = ImageDraw {
                            image_id: draw.image_id,
                            transform: glyph_to_page,
                            alpha: draw.alpha,
                        };
                        self.render_type3_image(&glyph_images, &draw, paint, run.alpha);
                    }
                    _ => {}
                }
            }
        }
    }

    /// Type 3 glyphs in Beamer PDFs commonly use `/ImageMask true` inline
    /// images as glyph stencils. The nested interpreter decodes those images
    /// with its default black fill colour, so recolour their opaque samples
    /// with the text paint before sending them through the normal image path.
    fn render_type3_image(
        &mut self,
        image_cache: &ImageCache,
        draw: &ImageDraw,
        paint: &tiny_skia::Paint<'_>,
        alpha: f32,
    ) {
        let Some(tint) = (match &paint.shader {
            tiny_skia::Shader::SolidColor(color) => Some(*color),
            _ => None,
        }) else {
            return;
        };
        let Some(source) = image_cache.get(draw.image_id) else {
            return;
        };
        if !source.has_alpha {
            self.render_image_from_cache(image_cache, draw, alpha, false);
            return;
        }

        let mut tinted = source.clone();
        for pixel in tinted.data.chunks_exact_mut(4) {
            let pixel_alpha = pixel[3] as u16;
            if pixel_alpha == 0 {
                continue;
            }
            // ImageCache pixels are premultiplied RGBA. Type 3 masks currently
            // have binary alpha, but multiplying here also keeps this correct
            // for anti-aliased mask decoders.
            pixel[0] = (tint.red() * 255.0 * pixel_alpha as f32 / 255.0) as u8;
            pixel[1] = (tint.green() * 255.0 * pixel_alpha as f32 / 255.0) as u8;
            pixel[2] = (tint.blue() * 255.0 * pixel_alpha as f32 / 255.0) as u8;
        }

        let mut tinted_cache = ImageCache::new();
        let image_id = tinted_cache.insert(tinted);
        let draw = ImageDraw {
            image_id,
            transform: draw.transform,
            alpha: draw.alpha,
        };
        self.render_image_from_cache(&tinted_cache, &draw, alpha, false);
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
        glyph_x_offset: f32,
    ) -> Option<tiny_skia::Path> {
        let mut pb = tiny_skia::PathBuilder::new();

        for elem in &path.elements {
            match *elem {
                PathElement::MoveTo(p) => {
                    let (px, py) = self.type3_to_pixel(
                        p.x,
                        p.y,
                        font_matrix,
                        font_size,
                        h_scale,
                        tm,
                        glyph_x_offset,
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
                        glyph_x_offset,
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
                        glyph_x_offset,
                    );
                    let (x2, y2) = self.type3_to_pixel(
                        c2.x,
                        c2.y,
                        font_matrix,
                        font_size,
                        h_scale,
                        tm,
                        glyph_x_offset,
                    );
                    let (x, y) = self.type3_to_pixel(
                        end.x,
                        end.y,
                        font_matrix,
                        font_size,
                        h_scale,
                        tm,
                        glyph_x_offset,
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
        glyph_x_offset: f32,
    ) -> (f32, f32) {
        // Step 1: FontMatrix * glyph_coord → text space
        let tx = font_matrix[0] * gx + font_matrix[2] * gy + font_matrix[4];
        let ty = font_matrix[1] * gx + font_matrix[3] * gy + font_matrix[5];

        // Step 2: scale by font_size; the shape x also carries Th (Tz/100).
        let tx = tx * font_size as f64 * h_scale as f64;
        let ty = ty * font_size as f64;

        // Step 3: add glyph horizontal offset (text space, advance already incl. Th)
        let tx = tx + glyph_x_offset as f64;

        // Step 4: apply text matrix (= CTM * Tm) to get page-space coords
        let page_x = tm.a * tx + tm.c * ty + tm.e;
        let page_y = tm.b * tx + tm.d * ty + tm.f;

        // Step 5: page space → pixel space. The CTM's orientation is ordinary
        // geometry; the one fixed page Y-flip then maps to device pixels.
        let px = (page_x as f32 - self.rect_x0) * self.scale;
        let py = (self.rect_y1 - page_y as f32) * self.scale;

        (px, py)
    }
}

impl<'a> Default for CpuRenderer<'a> {
    fn default() -> Self {
        Self::new()
    }
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

/// Box-filter (area-average) downscale of a tight RGBA8 buffer. Each target
/// pixel averages its covering source block, so no source pixel is dropped —
/// unlike point/bilinear sampling at strong minification. Channels are averaged
/// independently, which is correct for the premultiplied data tiny-skia consumes.
fn box_downscale_rgba(data: &[u8], sw: u32, sh: u32, tw: u32, th: u32) -> Vec<u8> {
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
        self.scale = info.scale;
        self.rect_x0 = info.page_rect.x0 as f32;
        self.rect_y0 = info.page_rect.y0 as f32;
        self.rect_y1 = info.page_rect.y1 as f32;
        // Fresh clip + blend state + budget for each page (a renderer may be
        // reused). An unbalanced group from a prior page must not leak through.
        self.clip_stack.clear();
        self.current_clip = None;
        self.clip_bounds = None;
        self.blend_stack.clear();
        self.blend_surface_bytes = 0;
        self.skipped_blend_depth = 0;
        self.clip_pixel_spent = 0;
        self.clip_mask_bytes = 0;
        self.over_budget = false;
        self.soft_mask_depth = 0;
        self.soft_mask_planes.clear();
        self.soft_mask_cache_bytes = 0;
        self.downscaled_images.clear();
        self.deadline = self
            .render_budget
            .map(|d| zpdf_core::time::Instant::now() + d);

        // ceil(), not truncation: a 595x842pt page at 110 DPI is 909.03x1286.6px
        // and must produce a 910x1287 raster (pdfium semantics) so no content is
        // sliced off the right/bottom edges.
        let (mut w, mut h) = info.raster_dimensions()?;

        // Clamp the raster to a total-pixel budget. A pathological MediaBox (or
        // a high DPI on a huge page) otherwise demands a multi-gigabyte Pixmap
        // that aborts the process on the allocation alone, or makes per-pixel
        // work (fills, blits, per-clip masks) hang. Scaling BOTH the dimensions
        // and `self.scale` by the same factor keeps geometry consistent, so the
        // output is a smaller-but-complete render instead of a crash/timeout.
        let pixels = w as u64 * h as u64;
        if pixels > self.max_page_pixels {
            let requested_scale = self.scale;
            let (limited_scale, limited_width, limited_height) = fit_raster_to_pixel_limit(
                info.page_rect.width(),
                info.page_rect.height(),
                requested_scale,
                self.max_page_pixels,
            )
            .ok_or_else(|| {
                CpuRenderError::LimitExceeded(format!(
                    "page raster {w}x{h} cannot fit max_page_pixels={}",
                    self.max_page_pixels
                ))
            })?;
            self.scale = limited_scale;
            w = limited_width;
            h = limited_height;
            tracing::warn!(
                limit = self.max_page_pixels,
                "raster {pixels} px exceeds configured budget; clamped to {w}x{h} (scale {:.4} -> {:.4})",
                requested_scale,
                self.scale
            );
        }

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
            | RenderCommand::DrawImage(_) => {
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
        self.soft_mask_planes.clear();
        self.soft_mask_cache_bytes = 0;
        self.downscaled_images.clear();
        self.deadline = None;
        self.skipped_blend_depth = 0;
        self.blend_surface_bytes = 0;
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
            premultiplied: true,
        });
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        dl.push(RenderCommand::DrawImage(ImageDraw {
            image_id: id,
            transform: Matrix::new(20.0, 0.0, 0.0, 20.0, 0.0, 0.0),
            alpha: 1.0,
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
            premultiplied: true,
        });
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 8.0, 8.0));
        dl.push(RenderCommand::DrawImage(ImageDraw {
            image_id: id,
            transform: Matrix::new(4.0, 0.0, 0.0, 4.0, 2.0, 2.0),
            alpha: 1.0,
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
        let out = box_downscale_rgba(&data, 4, 2, 2, 1);
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

    /// One page exercising every command family, rendered with timing enabled.
    fn mixed_page_stats() -> zpdf_render::StageStats {
        let (fonts, font_id) = var_fonts();
        let mut images = ImageCache::new();
        let image_id = images.insert(DecodedImage {
            width: 4,
            height: 4,
            data: vec![128; 4 * 4 * 4],
            has_alpha: false,
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

    /// Knockout and overprint element painting goes through full-raster scratch
    /// passes and merges over the whole raster, so nothing finer than the whole
    /// surface survives them. Assert that conservative outcome (and that it is
    /// still *sound*) rather than leaving it to be inferred from the code.
    #[test]
    fn knockout_and_overprint_groups_are_conservative_and_sound() {
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
        assert_eq!(
            stats.mask_composite_region_px,
            100 * 100 * 2,
            "both groups composite over the full surface — knockout and overprint \
             cannot be scoped"
        );
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
}
