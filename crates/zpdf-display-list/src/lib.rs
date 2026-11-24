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

/// One top-level blend-mode slice extracted from a backdrop-dependent
/// [`TilingPattern`] cell so backends can rasterize each slice onto a
/// transparent tile and composite it onto the live destination with its
/// [`BlendMode`].
#[derive(Debug, Clone)]
pub struct TilingBlendSegment {
    pub blend_mode: BlendMode,
    pub commands: std::sync::Arc<DisplayList>,
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

    /// True when any mark inside the cell reads the destination page backdrop
    /// (a top-level non-`Normal` [`BlendMode`], active [`Overprint`], or a
    /// nested backdrop-dependent [`TilingPattern`]). Such cells cannot be
    /// pre-baked onto a single transparent tile and composited with `Normal`
    /// source-over (ISO 32000-1 §11.6.7).
    ///
    /// Memoizes nested cell checks by `(Arc::as_ptr, depth)` so shared child
    /// cells in a DAG are classified at most once per depth.
    pub fn is_backdrop_dependent(&self) -> bool {
        let mut memo = std::collections::HashMap::new();
        dl_is_backdrop_dependent_memo(&self.commands, 0, &mut memo)
    }

    /// Split a backdrop-dependent cell into sequential top-level blend-mode
    /// slices when every mark can be rasterized into a tile pixmap and
    /// composited onto the destination with its own [`BlendMode`]. Recursively
    /// lifts non-overlapping nested backdrop-dependent [`TilingPattern`] paints
    /// so dense nested `Multiply` (etc.) patterns stay representable. Returns
    /// `None` when the cell uses active overprint, non-passthrough nested
    /// backdrop-dependent groups, overlapping nested backdrop-dependent tiling
    /// patterns, or exceeds the segment cap.
    pub fn blend_segments(&self) -> Option<Vec<TilingBlendSegment>> {
        let mut memo = std::collections::HashMap::new();
        self.blend_segments_at(0, &mut memo)
    }

    fn blend_segments_at(
        &self,
        depth: u8,
        memo: &mut std::collections::HashMap<(usize, u8), bool>,
    ) -> Option<Vec<TilingBlendSegment>> {
        const MAX_SEGMENTS: usize = 64;
        if depth > MAX_TILING_BACKDROP_SCAN_DEPTH {
            return None;
        }
        let cmds = &self.commands.commands;
        let page_rect = self.commands.page_rect;
        let mut segments: Vec<TilingBlendSegment> = Vec::new();
        let mut clip_stack: Vec<RenderCommand> = Vec::new();
        let mut cur_normal: Vec<RenderCommand> = Vec::new();
        let mut passthrough_pops: Vec<usize> = Vec::new();

        let flush_normal = |cur_normal: &mut Vec<RenderCommand>,
                            clip_stack: &[RenderCommand],
                            segments: &mut Vec<TilingBlendSegment>|
         -> Option<()> {
            if cur_normal.is_empty() {
                return Some(());
            }
            if segments.len() >= MAX_SEGMENTS {
                return None;
            }
            for _ in 0..clip_stack.len() {
                cur_normal.push(RenderCommand::PopClip);
            }
            let mut dl = DisplayList::new(page_rect);
            dl.commands = std::mem::take(cur_normal);
            segments.push(TilingBlendSegment {
                blend_mode: BlendMode::Normal,
                commands: std::sync::Arc::new(dl),
            });
            Some(())
        };

        let emit_lifted_cmd = |blend_mode: BlendMode,
                               cmd: RenderCommand,
                               cur_normal: &mut Vec<RenderCommand>,
                               clip_stack: &[RenderCommand],
                               segments: &mut Vec<TilingBlendSegment>|
         -> Option<()> {
            if blend_mode == BlendMode::Normal {
                if cur_normal.is_empty() {
                    cur_normal.extend(clip_stack.iter().cloned());
                }
                cur_normal.push(cmd);
                return Some(());
            }
            flush_normal(cur_normal, clip_stack, segments)?;
            if segments.len() >= MAX_SEGMENTS {
                return None;
            }
            let mut seg_cmds = Vec::with_capacity(clip_stack.len() * 2 + 1);
            seg_cmds.extend(clip_stack.iter().cloned());
            seg_cmds.push(cmd);
            for _ in 0..clip_stack.len() {
                seg_cmds.push(RenderCommand::PopClip);
            }
            let mut dl = DisplayList::new(page_rect);
            dl.commands = seg_cmds;
            segments.push(TilingBlendSegment {
                blend_mode,
                commands: std::sync::Arc::new(dl),
            });
            Some(())
        };

        let mut idx = 0usize;
        while idx < cmds.len() {
            if passthrough_pops.last().copied() == Some(idx) {
                passthrough_pops.pop();
                idx += 1;
                continue;
            }
            match &cmds[idx] {
                RenderCommand::PushClip { .. } | RenderCommand::PushClipStroke { .. } => {
                    if !cur_normal.is_empty() {
                        cur_normal.push(cmds[idx].clone());
                    }
                    clip_stack.push(cmds[idx].clone());
                    idx += 1;
                }
                RenderCommand::PopClip => {
                    if !cur_normal.is_empty() {
                        cur_normal.push(RenderCommand::PopClip);
                    }
                    clip_stack.pop();
                    idx += 1;
                }
                RenderCommand::FillPath {
                    path,
                    rule,
                    paint,
                    alpha,
                    overprint,
                } => {
                    if overprint.is_some_and(|op| op.active != 0) {
                        return None;
                    }
                    if let Paint::Tiling(inner) = paint {
                        if dl_is_backdrop_dependent_memo(&inner.commands, depth + 1, memo) {
                            let inner_bbox = inner.bbox.normalize();
                            if inner_bbox.width() > inner.x_step.abs() + 1e-9
                                || inner_bbox.height() > inner.y_step.abs() + 1e-9
                            {
                                return None;
                            }
                            let inner_segs = inner.blend_segments_at(depth + 1, memo)?;
                            if inner_segs.len() > 1 && *alpha < 1.0 {
                                return None;
                            }
                            for seg in inner_segs {
                                let mut lifted = (**inner).clone();
                                lifted.commands = seg.commands;
                                emit_lifted_cmd(
                                    seg.blend_mode,
                                    RenderCommand::FillPath {
                                        path: path.clone(),
                                        rule: *rule,
                                        paint: Paint::Tiling(std::sync::Arc::new(lifted)),
                                        alpha: *alpha,
                                        overprint: *overprint,
                                    },
                                    &mut cur_normal,
                                    &clip_stack,
                                    &mut segments,
                                )?;
                            }
                            idx += 1;
                            continue;
                        }
                    }
                    if cur_normal.is_empty() {
                        cur_normal.extend(clip_stack.iter().cloned());
                    }
                    cur_normal.push(cmds[idx].clone());
                    idx += 1;
                }
                RenderCommand::StrokePath {
                    path,
                    style,
                    paint,
                    alpha,
                    overprint,
                } => {
                    if overprint.is_some_and(|op| op.active != 0) {
                        return None;
                    }
                    if let Paint::Tiling(inner) = paint {
                        if dl_is_backdrop_dependent_memo(&inner.commands, depth + 1, memo) {
                            let inner_bbox = inner.bbox.normalize();
                            if inner_bbox.width() > inner.x_step.abs() + 1e-9
                                || inner_bbox.height() > inner.y_step.abs() + 1e-9
                            {
                                return None;
                            }
                            let inner_segs = inner.blend_segments_at(depth + 1, memo)?;
                            if inner_segs.len() > 1 && *alpha < 1.0 {
                                return None;
                            }
                            for seg in inner_segs {
                                let mut lifted = (**inner).clone();
                                lifted.commands = seg.commands;
                                emit_lifted_cmd(
                                    seg.blend_mode,
                                    RenderCommand::StrokePath {
                                        path: path.clone(),
                                        style: style.clone(),
                                        paint: Paint::Tiling(std::sync::Arc::new(lifted)),
                                        alpha: *alpha,
                                        overprint: *overprint,
                                    },
                                    &mut cur_normal,
                                    &clip_stack,
                                    &mut segments,
                                )?;
                            }
                            idx += 1;
                            continue;
                        }
                    }
                    if cur_normal.is_empty() {
                        cur_normal.extend(clip_stack.iter().cloned());
                    }
                    cur_normal.push(cmds[idx].clone());
                    idx += 1;
                }
                RenderCommand::DrawGlyphRun(run) => {
                    if run.overprint.is_some_and(|op| op.active != 0) {
                        return None;
                    }
                    if let Paint::Tiling(inner) = &run.paint {
                        if dl_is_backdrop_dependent_memo(&inner.commands, depth + 1, memo) {
                            let inner_bbox = inner.bbox.normalize();
                            if inner_bbox.width() > inner.x_step.abs() + 1e-9
                                || inner_bbox.height() > inner.y_step.abs() + 1e-9
                            {
                                return None;
                            }
                            let inner_segs = inner.blend_segments_at(depth + 1, memo)?;
                            if inner_segs.len() > 1 && run.alpha < 1.0 {
                                return None;
                            }
                            for seg in inner_segs {
                                let mut lifted = (**inner).clone();
                                lifted.commands = seg.commands;
                                let mut lifted_run = run.clone();
                                lifted_run.paint = Paint::Tiling(std::sync::Arc::new(lifted));
                                emit_lifted_cmd(
                                    seg.blend_mode,
                                    RenderCommand::DrawGlyphRun(lifted_run),
                                    &mut cur_normal,
                                    &clip_stack,
                                    &mut segments,
                                )?;
                            }
                            idx += 1;
                            continue;
                        }
                    }
                    if cur_normal.is_empty() {
                        cur_normal.extend(clip_stack.iter().cloned());
                    }
                    cur_normal.push(cmds[idx].clone());
                    idx += 1;
                }
                RenderCommand::DrawImage(_) => {
                    if cur_normal.is_empty() {
                        cur_normal.extend(clip_stack.iter().cloned());
                    }
                    cur_normal.push(cmds[idx].clone());
                    idx += 1;
                }
                RenderCommand::PushBlendGroup {
                    blend_mode,
                    isolated,
                    knockout,
                    bounds,
                    alpha,
                    mask,
                } => {
                    let end = find_matching_pop_blend(cmds, idx)?;
                    let inner = &cmds[idx + 1..end];
                    let inner_backdrop = slice_is_backdrop_dependent_memo(inner, depth, memo);
                    if *blend_mode == BlendMode::Normal {
                        if *isolated || !inner_backdrop {
                            if cur_normal.is_empty() {
                                cur_normal.extend(clip_stack.iter().cloned());
                            }
                            cur_normal.extend(cmds[idx..=end].iter().cloned());
                            idx = end + 1;
                        } else if !*knockout && *alpha >= 1.0 && mask.is_none() {
                            // Passthrough non-isolated group: flatten into outer stream.
                            passthrough_pops.push(end);
                            idx += 1;
                        } else {
                            return None;
                        }
                    } else {
                        if !*isolated && inner_backdrop {
                            return None;
                        }
                        flush_normal(&mut cur_normal, &clip_stack, &mut segments)?;
                        if segments.len() >= MAX_SEGMENTS {
                            return None;
                        }
                        let mut seg_cmds =
                            Vec::with_capacity(clip_stack.len() * 2 + inner.len() + 2);
                        seg_cmds.extend(clip_stack.iter().cloned());
                        let needs_wrapper = *alpha < 1.0 || mask.is_some() || *knockout;
                        if needs_wrapper {
                            seg_cmds.push(RenderCommand::PushBlendGroup {
                                blend_mode: BlendMode::Normal,
                                isolated: true,
                                knockout: *knockout,
                                bounds: *bounds,
                                alpha: *alpha,
                                mask: mask.clone(),
                            });
                        }
                        seg_cmds.extend(inner.iter().cloned());
                        if needs_wrapper {
                            seg_cmds.push(RenderCommand::PopBlendGroup);
                        }
                        for _ in 0..clip_stack.len() {
                            seg_cmds.push(RenderCommand::PopClip);
                        }
                        let mut dl = DisplayList::new(page_rect);
                        dl.commands = seg_cmds;
                        segments.push(TilingBlendSegment {
                            blend_mode: *blend_mode,
                            commands: std::sync::Arc::new(dl),
                        });
                        idx = end + 1;
                    }
                }
                RenderCommand::PopBlendGroup => {
                    idx += 1;
                }
            }
        }
        flush_normal(&mut cur_normal, &clip_stack, &mut segments)?;
        Some(segments)
    }
}

fn find_matching_pop_blend(cmds: &[RenderCommand], push_idx: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, cmd) in cmds.iter().enumerate().skip(push_idx) {
        match cmd {
            RenderCommand::PushBlendGroup { .. } => depth += 1,
            RenderCommand::PopBlendGroup => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Matches the interpreter's tiling/form nesting depth cap (`MAX_FORM_DEPTH = 16`).
/// Subtrees nested beyond this depth are conservatively classified as
/// backdrop-dependent so uninspected subtrees are never treated as proven
/// backdrop-independent.
const MAX_TILING_BACKDROP_SCAN_DEPTH: u8 = 16;

fn dl_is_backdrop_dependent_memo(
    dl: &std::sync::Arc<DisplayList>,
    depth: u8,
    memo: &mut std::collections::HashMap<(usize, u8), bool>,
) -> bool {
    if depth > MAX_TILING_BACKDROP_SCAN_DEPTH {
        return true;
    }
    let key = (std::sync::Arc::as_ptr(dl) as *const () as usize, depth);
    if let Some(&cached) = memo.get(&key) {
        return cached;
    }
    let result = slice_is_backdrop_dependent_memo(&dl.commands, depth, memo);
    memo.insert(key, result);
    result
}

fn slice_is_backdrop_dependent_memo(
    cmds: &[RenderCommand],
    depth: u8,
    memo: &mut std::collections::HashMap<(usize, u8), bool>,
) -> bool {
    if depth > MAX_TILING_BACKDROP_SCAN_DEPTH {
        return true;
    }
    let mut isolated_depth = 0usize;
    for cmd in cmds {
        if isolated_depth == 0 {
            match cmd {
                RenderCommand::PushBlendGroup {
                    blend_mode,
                    isolated,
                    ..
                } => {
                    if *blend_mode != BlendMode::Normal {
                        return true;
                    }
                    if *isolated {
                        isolated_depth += 1;
                    }
                }
                RenderCommand::FillPath {
                    paint, overprint, ..
                }
                | RenderCommand::StrokePath {
                    paint, overprint, ..
                } => {
                    if overprint.is_some_and(|op| op.active != 0) {
                        return true;
                    }
                    if let Paint::Tiling(inner) = paint {
                        if dl_is_backdrop_dependent_memo(&inner.commands, depth + 1, memo) {
                            return true;
                        }
                    }
                }
                RenderCommand::DrawGlyphRun(run) => {
                    if run.overprint.is_some_and(|op| op.active != 0) {
                        return true;
                    }
                    if let Paint::Tiling(inner) = &run.paint {
                        if dl_is_backdrop_dependent_memo(&inner.commands, depth + 1, memo) {
                            return true;
                        }
                    }
                }
                _ => {}
            }
        } else {
            match cmd {
                RenderCommand::PushBlendGroup { .. } => isolated_depth += 1,
                RenderCommand::PopBlendGroup => isolated_depth -= 1,
                _ => {}
            }
        }
    }
    false
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

#[cfg(test)]
mod tiling_backdrop_tests {
    use super::*;
    use std::sync::Arc;

    fn rect_path(x0: f64, y0: f64, x1: f64, y1: f64) -> Path {
        let mut p = Path::new();
        p.rect(Rect::new(x0, y0, x1, y1));
        p
    }

    fn tiling_with(commands: Vec<RenderCommand>) -> TilingPattern {
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0));
        dl.commands = commands;
        TilingPattern {
            commands: Arc::new(dl),
            bbox: Rect::new(0.0, 0.0, 10.0, 10.0),
            x_step: 10.0,
            y_step: 10.0,
            matrix: Matrix::identity(),
            tiling_type: 1,
        }
    }

    #[test]
    fn backdrop_dependence_distinguishes_normal_isolated_multiply_and_overprint() {
        let normal = tiling_with(vec![RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 5.0, 5.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        }]);
        assert!(!normal.is_backdrop_dependent());

        // Multiply inside an isolated Normal group only reads the group's own backdrop.
        let isolated_mul = tiling_with(vec![
            RenderCommand::PushBlendGroup {
                blend_mode: BlendMode::Normal,
                isolated: true,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
                alpha: 1.0,
                mask: None,
            },
            RenderCommand::PushBlendGroup {
                blend_mode: BlendMode::Multiply,
                isolated: true,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
                alpha: 1.0,
                mask: None,
            },
            RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 5.0, 5.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::white()),
                alpha: 1.0,
                overprint: None,
            },
            RenderCommand::PopBlendGroup,
            RenderCommand::PopBlendGroup,
        ]);
        assert!(!isolated_mul.is_backdrop_dependent());

        // Active overprint reads the destination backdrop and cannot be split into blend segments.
        let overprint = tiling_with(vec![RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 5.0, 5.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: Some(Overprint {
                cmyk: [0.0, 0.0, 0.0, 1.0],
                active: Overprint::K,
            }),
        }]);
        assert!(overprint.is_backdrop_dependent());
        assert!(overprint.blend_segments().is_none());
    }

    #[test]
    fn blend_segments_splits_normal_and_multiply_preserving_clips() {
        let tiling = tiling_with(vec![
            RenderCommand::PushClip {
                path: rect_path(0.0, 0.0, 10.0, 10.0),
                rule: FillRule::NonZero,
            },
            RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 5.0, 10.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
                alpha: 1.0,
                overprint: None,
            },
            RenderCommand::PushBlendGroup {
                blend_mode: BlendMode::Multiply,
                isolated: true,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
                alpha: 1.0,
                mask: None,
            },
            RenderCommand::FillPath {
                path: rect_path(5.0, 0.0, 10.0, 10.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::white()),
                alpha: 1.0,
                overprint: None,
            },
            RenderCommand::PopBlendGroup,
            RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 2.0, 2.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::rgb(0.0, 1.0, 0.0)),
                alpha: 1.0,
                overprint: None,
            },
            RenderCommand::PopClip,
        ]);
        assert!(tiling.is_backdrop_dependent());
        let segs = tiling
            .blend_segments()
            .expect("normal+multiply+normal should split cleanly");
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[0].blend_mode, BlendMode::Normal);
        assert_eq!(segs[1].blend_mode, BlendMode::Multiply);
        assert_eq!(segs[2].blend_mode, BlendMode::Normal);
        // Each segment carries balanced PushClip/PopClip around its draw.
        for seg in &segs {
            let pushes = seg
                .commands
                .commands
                .iter()
                .filter(|c| matches!(c, RenderCommand::PushClip { .. }))
                .count();
            let pops = seg
                .commands
                .commands
                .iter()
                .filter(|c| matches!(c, RenderCommand::PopClip))
                .count();
            assert_eq!(pushes, 1);
            assert_eq!(pops, 1);
        }
    }

    #[test]
    fn is_backdrop_dependent_memoizes_shared_dag_without_exponential_blowup() {
        // 16-level diamond DAG where each level paints 16 references to the same
        // child Arc<DisplayList> (16^16 naive paths), all Normal.
        let mut child_arc = Arc::new(DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0)));
        Arc::make_mut(&mut child_arc).push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 10.0, 10.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
        });
        for _ in 0..16 {
            let child = Arc::new(TilingPattern {
                commands: Arc::clone(&child_arc),
                bbox: Rect::new(0.0, 0.0, 10.0, 10.0),
                x_step: 10.0,
                y_step: 10.0,
                matrix: Matrix::identity(),
                tiling_type: 1,
            });
            let mut parent = DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0));
            for _ in 0..16 {
                parent.push(RenderCommand::FillPath {
                    path: rect_path(0.0, 0.0, 10.0, 10.0),
                    rule: FillRule::NonZero,
                    paint: Paint::Tiling(Arc::clone(&child)),
                    alpha: 1.0,
                    overprint: None,
                });
            }
            child_arc = Arc::new(parent);
        }
        let root = TilingPattern {
            commands: child_arc,
            bbox: Rect::new(0.0, 0.0, 10.0, 10.0),
            x_step: 10.0,
            y_step: 10.0,
            matrix: Matrix::identity(),
            tiling_type: 1,
        };
        assert!(!root.is_backdrop_dependent());
    }

    #[test]
    fn is_backdrop_dependent_detects_multiply_at_depth_nine() {
        let mut child_arc = Arc::new(DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0)));
        {
            let dl = Arc::make_mut(&mut child_arc);
            dl.push(RenderCommand::PushBlendGroup {
                blend_mode: BlendMode::Multiply,
                isolated: true,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
                alpha: 1.0,
                mask: None,
            });
            dl.push(RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 10.0, 10.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::white()),
                alpha: 1.0,
                overprint: None,
            });
            dl.push(RenderCommand::PopBlendGroup);
        }
        for _ in 0..9 {
            let child = Arc::new(TilingPattern {
                commands: Arc::clone(&child_arc),
                bbox: Rect::new(0.0, 0.0, 10.0, 10.0),
                x_step: 10.0,
                y_step: 10.0,
                matrix: Matrix::identity(),
                tiling_type: 1,
            });
            let mut parent = DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0));
            parent.push(RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 10.0, 10.0),
                rule: FillRule::NonZero,
                paint: Paint::Tiling(child),
                alpha: 1.0,
                overprint: None,
            });
            child_arc = Arc::new(parent);
        }
        let root = TilingPattern {
            commands: child_arc,
            bbox: Rect::new(0.0, 0.0, 10.0, 10.0),
            x_step: 10.0,
            y_step: 10.0,
            matrix: Matrix::identity(),
            tiling_type: 1,
        };
        assert!(
            root.is_backdrop_dependent(),
            "Multiply mark 9 levels deep (within interpreter limit of 16) must be detected"
        );
        let segs = root
            .blend_segments()
            .expect("9-level nested non-overlapping Multiply should lift cleanly");
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].blend_mode, BlendMode::Multiply);
    }

    #[test]
    fn is_backdrop_dependent_is_conservative_past_scan_depth_cutoff() {
        // 18 nested levels (> MAX_TILING_BACKDROP_SCAN_DEPTH = 16) with only Normal
        // marks cannot be completely inspected, so it must conservatively report
        // backdrop-dependent rather than claiming proven backdrop-independence.
        let mut child_arc = Arc::new(DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0)));
        Arc::make_mut(&mut child_arc).push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 10.0, 10.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
        });
        for _ in 0..18 {
            let child = Arc::new(TilingPattern {
                commands: Arc::clone(&child_arc),
                bbox: Rect::new(0.0, 0.0, 10.0, 10.0),
                x_step: 10.0,
                y_step: 10.0,
                matrix: Matrix::identity(),
                tiling_type: 1,
            });
            let mut parent = DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0));
            parent.push(RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 10.0, 10.0),
                rule: FillRule::NonZero,
                paint: Paint::Tiling(child),
                alpha: 1.0,
                overprint: None,
            });
            child_arc = Arc::new(parent);
        }
        let root = TilingPattern {
            commands: child_arc,
            bbox: Rect::new(0.0, 0.0, 10.0, 10.0),
            x_step: 10.0,
            y_step: 10.0,
            matrix: Matrix::identity(),
            tiling_type: 1,
        };
        assert!(
            root.is_backdrop_dependent(),
            "uninspected subtree past depth cap must not be classified as backdrop-independent"
        );
        assert!(
            root.blend_segments().is_none(),
            "uninspected subtree past depth cap must decline segmentation and take fallback"
        );
    }

    #[test]
    fn blend_segments_lifts_nested_multiply_tiling_pattern() {
        let inner = Arc::new(tiling_with(vec![
            RenderCommand::PushBlendGroup {
                blend_mode: BlendMode::Multiply,
                isolated: true,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
                alpha: 1.0,
                mask: None,
            },
            RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 10.0, 10.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::rgb(0.0, 1.0, 1.0)),
                alpha: 1.0,
                overprint: None,
            },
            RenderCommand::PopBlendGroup,
        ]));
        let outer = tiling_with(vec![
            RenderCommand::PushClip {
                path: rect_path(0.0, 0.0, 10.0, 10.0),
                rule: FillRule::NonZero,
            },
            RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 10.0, 10.0),
                rule: FillRule::NonZero,
                paint: Paint::Tiling(inner),
                alpha: 1.0,
                overprint: None,
            },
            RenderCommand::PopClip,
        ]);
        assert!(outer.is_backdrop_dependent());
        let segs = outer
            .blend_segments()
            .expect("nested non-overlapping Multiply tiling must lift cleanly");
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].blend_mode, BlendMode::Multiply);
        // The lifted inner tiling inside the segment must itself be backdrop-independent.
        assert!(!TilingPattern {
            commands: Arc::clone(&segs[0].commands),
            bbox: outer.bbox,
            x_step: outer.x_step,
            y_step: outer.y_step,
            matrix: outer.matrix,
            tiling_type: outer.tiling_type,
        }
        .is_backdrop_dependent());
    }
}
