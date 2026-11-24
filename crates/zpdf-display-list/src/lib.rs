use zpdf_core::{Matrix, Rect};

mod path;

#[derive(Debug, Clone)]
pub struct DisplayList {
    pub page_rect: Rect,
    /// Visual commands executed in order by render backends.
    ///
    /// Callers that retain [`Self::expanded_glyph_runs`],
    /// [`Self::pattern_ranges`], or [`Self::type3_glyph_ranges`] must treat
    /// `commands` as append-only while those ranges are live (do not insert,
    /// remove, or reorder entries before a recorded range). Call
    /// [`Self::clear_command_ranges`] (or all three of
    /// [`Self::clear_expanded_glyph_runs`], [`Self::clear_pattern_ranges`],
    /// and [`Self::clear_type3_glyph_ranges`]) before mutating `commands`
    /// in-place so recorded ranges cannot silently refer to different commands.
    pub commands: Vec<RenderCommand>,
    expanded_glyph_runs: Vec<ExpandedGlyphRun>,
    pattern_ranges: Vec<(usize, usize)>,
    type3_glyph_ranges: Vec<(usize, usize)>,
}

impl DisplayList {
    pub fn new(page_rect: Rect) -> Self {
        Self {
            page_rect,
            commands: Vec::new(),
            expanded_glyph_runs: Vec::new(),
            pattern_ranges: Vec::new(),
            type3_glyph_ranges: Vec::new(),
        }
    }

    pub fn push(&mut self, cmd: RenderCommand) {
        self.commands.push(cmd);
    }

    /// Truncate `self.commands` to `len`, dropping incomplete expanded glyph
    /// runs and clipping pattern and Type 3 glyph ranges to the surviving
    /// command prefix.
    pub fn truncate_commands(&mut self, len: usize) {
        self.commands.truncate(len);
        self.expanded_glyph_runs.retain(|e| e.cmd_end <= len);
        for ranges in [&mut self.pattern_ranges, &mut self.type3_glyph_ranges] {
            ranges.retain_mut(|(start, end)| {
                if *start >= len {
                    return false;
                }
                *end = (*end).min(len);
                *start < *end
            });
        }
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

    /// Discard all command-index-keyed ranges (`expanded_glyph_runs`,
    /// `pattern_ranges`, and `type3_glyph_ranges`). Call before removing,
    /// inserting, or reordering entries in `self.commands` in-place so
    /// recorded ranges cannot silently refer to different commands.
    pub fn clear_command_ranges(&mut self) {
        self.expanded_glyph_runs.clear();
        self.pattern_ranges.clear();
        self.type3_glyph_ranges.clear();
    }

    /// Discard all recorded expanded glyph runs. When mutating `self.commands`
    /// in-place, prefer [`Self::clear_command_ranges`] so pattern and Type 3
    /// glyph ranges are invalidated as well.
    pub fn clear_expanded_glyph_runs(&mut self) {
        self.expanded_glyph_runs.clear();
    }

    /// Semantic glyph runs already expanded into `commands`, ordered by
    /// `cmd_start` with non-overlapping `[cmd_start, cmd_end)` ranges.
    pub fn expanded_glyph_runs(&self) -> &[ExpandedGlyphRun] {
        &self.expanded_glyph_runs
    }

    /// Record a command range `[cmd_start, cmd_end)` emitted while painting a
    /// tiling pattern cell. Glyphs inside a pattern cell are decorative paint
    /// artwork rather than logical page text.
    ///
    /// # Panics
    ///
    /// Panics if `cmd_start > cmd_end`, `cmd_end > self.commands.len()`, or
    /// `cmd_start` overlaps a previously recorded range.
    pub fn push_pattern_range(&mut self, cmd_start: usize, cmd_end: usize) {
        assert!(
            cmd_start <= cmd_end && cmd_end <= self.commands.len(),
            "pattern command range {cmd_start}..{cmd_end} out of bounds for {} commands",
            self.commands.len()
        );
        if let Some(&(_, prev_end)) = self.pattern_ranges.last() {
            assert!(
                cmd_start >= prev_end,
                "pattern command range {cmd_start}..{cmd_end} overlaps previous range ending at {prev_end}",
            );
        }
        if cmd_start < cmd_end {
            self.pattern_ranges.push((cmd_start, cmd_end));
        }
    }

    /// Discard all recorded pattern command ranges.
    pub fn clear_pattern_ranges(&mut self) {
        self.pattern_ranges.clear();
    }

    /// Non-overlapping `[cmd_start, cmd_end)` ranges of commands emitted while
    /// painting tiling pattern cells.
    pub fn pattern_ranges(&self) -> &[(usize, usize)] {
        &self.pattern_ranges
    }

    /// Whether `commands[cmd_idx]` originated inside a tiling pattern cell.
    pub fn is_in_pattern_range(&self, cmd_idx: usize) -> bool {
        let pos = self
            .pattern_ranges
            .partition_point(|&(start, _)| start <= cmd_idx);
        pos > 0 && cmd_idx < self.pattern_ranges[pos - 1].1
    }

    /// Record a command range `[cmd_start, cmd_end)` emitted while interpreting
    /// an outermost Type 3 CharProc. Commands inside this range are glyph
    /// artwork (not independent page-level text runs) when not substituted by
    /// an [`ExpandedGlyphRun`].
    ///
    /// # Panics
    ///
    /// Panics if `cmd_start > cmd_end`, `cmd_end > self.commands.len()`, or
    /// `cmd_start` overlaps a previously recorded range.
    pub fn push_type3_glyph_range(&mut self, cmd_start: usize, cmd_end: usize) {
        assert!(
            cmd_start <= cmd_end && cmd_end <= self.commands.len(),
            "type3 glyph command range {cmd_start}..{cmd_end} out of bounds for {} commands",
            self.commands.len()
        );
        if let Some(&(_, prev_end)) = self.type3_glyph_ranges.last() {
            assert!(
                cmd_start >= prev_end,
                "type3 glyph command range {cmd_start}..{cmd_end} overlaps previous range ending at {prev_end}",
            );
        }
        if cmd_start < cmd_end {
            self.type3_glyph_ranges.push((cmd_start, cmd_end));
        }
    }

    /// Discard all recorded Type 3 glyph command ranges.
    pub fn clear_type3_glyph_ranges(&mut self) {
        self.type3_glyph_ranges.clear();
    }

    /// Non-overlapping `[cmd_start, cmd_end)` ranges of commands emitted while
    /// interpreting outermost Type 3 CharProcs.
    pub fn type3_glyph_ranges(&self) -> &[(usize, usize)] {
        &self.type3_glyph_ranges
    }

    /// Whether `commands[cmd_idx]` originated inside a Type 3 CharProc.
    pub fn is_in_type3_glyph_range(&self, cmd_idx: usize) -> bool {
        let pos = self
            .type3_glyph_ranges
            .partition_point(|&(start, _)| start <= cmd_idx);
        pos > 0 && cmd_idx < self.type3_glyph_ranges[pos - 1].1
    }

    /// Heap bytes exclusively owned by this display list (command buffer, range
    /// vectors, and per-command owned geometry such as path point/verb buffers,
    /// dash arrays, and glyph vectors), excluding ref-counted payloads such as
    /// `Arc<InlineImage>` and nested `SoftMask` lists.
    pub fn owned_heap_bytes(&self) -> u64 {
        let mut bytes = (std::mem::size_of::<Self>() as u64)
            .saturating_add(
                (self.commands.capacity() as u64)
                    .saturating_mul(std::mem::size_of::<RenderCommand>() as u64),
            )
            .saturating_add(
                (self.expanded_glyph_runs.capacity() as u64)
                    .saturating_mul(std::mem::size_of::<ExpandedGlyphRun>() as u64),
            )
            .saturating_add(
                ((self.pattern_ranges.capacity() + self.type3_glyph_ranges.capacity()) as u64)
                    .saturating_mul(std::mem::size_of::<(usize, usize)>() as u64),
            );
        for cmd in &self.commands {
            bytes = bytes.saturating_add(cmd.owned_heap_bytes());
        }
        for expanded in &self.expanded_glyph_runs {
            bytes = bytes.saturating_add(
                (expanded.run.glyphs.capacity() as u64)
                    .saturating_mul(std::mem::size_of::<PositionedGlyph>() as u64),
            );
        }
        bytes
    }
}

/// Deduplication state tracking ref-counted allocations currently retained by a
/// renderer or exporter's soft-mask cache (`DisplayList`, `TilingPattern`,
/// `InlineImage`, and transfer-LUT allocations tracked independently by their
/// allocation identities).
#[derive(Debug, Default, Clone)]
pub struct RetainedMaskAccounting {
    display_list_ptrs: std::collections::HashSet<usize>,
    tiling_ptrs: std::collections::HashSet<usize>,
    inline_image_ptrs: std::collections::HashSet<usize>,
    transfer_ptrs: std::collections::HashSet<usize>,
}

/// Incremental retention delta computed for admitting a `DisplayList` (or
/// `SoftMask`) into a cache without mutating [`RetainedMaskAccounting`] until
/// admission succeeds.
#[derive(Debug, Default)]
pub struct RetainedMaskDelta {
    pub added_bytes: u64,
    new_display_lists: Vec<usize>,
    new_tilings: Vec<usize>,
    new_inline_images: Vec<usize>,
    new_transfers: Vec<usize>,
}

impl RetainedMaskAccounting {
    pub fn clear(&mut self) {
        self.display_list_ptrs.clear();
        self.tiling_ptrs.clear();
        self.inline_image_ptrs.clear();
        self.transfer_ptrs.clear();
    }

    pub fn commit(&mut self, delta: RetainedMaskDelta) {
        self.display_list_ptrs.extend(delta.new_display_lists);
        self.tiling_ptrs.extend(delta.new_tilings);
        self.inline_image_ptrs.extend(delta.new_inline_images);
        self.transfer_ptrs.extend(delta.new_transfers);
    }

    /// Estimate newly-retained heap bytes reachable from `root` (following all
    /// nested `SoftMask` lists, `Paint::Tiling` pattern objects and their cell
    /// display lists, and `DrawInlineImage` pixel buffers) via an iterative
    /// visited-set DAG worklist with no nesting cutoff. Every distinct
    /// `TilingPattern` allocation is charged separately from its shared cell
    /// `DisplayList`, and every reachable allocation is charged at most once.
    pub fn delta_for_display_list(&self, root: &std::sync::Arc<DisplayList>) -> RetainedMaskDelta {
        let mut delta = RetainedMaskDelta::default();
        let mut seen_dls = std::collections::HashSet::new();
        let mut seen_tilings = std::collections::HashSet::new();
        let mut seen_inlines = std::collections::HashSet::new();
        let mut seen_transfers = std::collections::HashSet::new();
        let mut stack: Vec<&DisplayList> = vec![root.as_ref()];

        while let Some(dl) = stack.pop() {
            let dl_ptr = dl as *const DisplayList as usize;
            if !seen_dls.insert(dl_ptr) {
                continue;
            }
            if !self.display_list_ptrs.contains(&dl_ptr) {
                delta.added_bytes = delta.added_bytes.saturating_add(dl.owned_heap_bytes());
                delta.new_display_lists.push(dl_ptr);
            }
            for cmd in &dl.commands {
                match cmd {
                    RenderCommand::DrawInlineImage(draw) => {
                        let ptr = std::sync::Arc::as_ptr(&draw.image) as usize;
                        if seen_inlines.insert(ptr) && !self.inline_image_ptrs.contains(&ptr) {
                            let img_bytes = (draw.image.data.len() as u64)
                                .saturating_add(std::mem::size_of::<InlineImage>() as u64);
                            delta.added_bytes = delta.added_bytes.saturating_add(img_bytes);
                            delta.new_inline_images.push(ptr);
                        }
                    }
                    RenderCommand::PushBlendGroup {
                        mask: Some(sub), ..
                    } => {
                        if let Some(tr) = &sub.transfer {
                            let tr_ptr = std::sync::Arc::as_ptr(tr) as usize;
                            if seen_transfers.insert(tr_ptr)
                                && !self.transfer_ptrs.contains(&tr_ptr)
                            {
                                delta.added_bytes = delta.added_bytes.saturating_add(256);
                                delta.new_transfers.push(tr_ptr);
                            }
                        }
                        stack.push(sub.commands.as_ref());
                    }
                    RenderCommand::FillPath {
                        paint: Paint::Tiling(tiling),
                        ..
                    }
                    | RenderCommand::StrokePath {
                        paint: Paint::Tiling(tiling),
                        ..
                    }
                    | RenderCommand::DrawGlyphRun(GlyphRun {
                        paint: Paint::Tiling(tiling),
                        ..
                    }) => {
                        let t_ptr = std::sync::Arc::as_ptr(tiling) as usize;
                        if seen_tilings.insert(t_ptr) && !self.tiling_ptrs.contains(&t_ptr) {
                            delta.added_bytes = delta
                                .added_bytes
                                .saturating_add(std::mem::size_of::<TilingPattern>() as u64);
                            delta.new_tilings.push(t_ptr);
                        }
                        stack.push(tiling.commands.as_ref());
                    }
                    _ => {}
                }
            }
            for expanded in &dl.expanded_glyph_runs {
                if let Paint::Tiling(tiling) = &expanded.run.paint {
                    let t_ptr = std::sync::Arc::as_ptr(tiling) as usize;
                    if seen_tilings.insert(t_ptr) && !self.tiling_ptrs.contains(&t_ptr) {
                        delta.added_bytes =
                            delta
                                .added_bytes
                                .saturating_add(std::mem::size_of::<TilingPattern>() as u64);
                        delta.new_tilings.push(t_ptr);
                    }
                    stack.push(tiling.commands.as_ref());
                }
            }
        }
        delta
    }
}

impl RenderCommand {
    /// Heap bytes exclusively owned by this command (path buffers, dash arrays,
    /// and glyph vectors), excluding ref-counted payloads.
    pub fn owned_heap_bytes(&self) -> u64 {
        let bytes = match self {
            Self::FillPath { path, .. } | Self::PushClip { path, .. } => path.heap_bytes(),
            Self::StrokePath { path, style, .. } | Self::PushClipStroke { path, style } => path
                .heap_bytes()
                .saturating_add(style.dash.as_ref().map_or(0, |d| {
                    d.array
                        .capacity()
                        .saturating_mul(std::mem::size_of::<f32>())
                })),
            Self::DrawGlyphRun(run) => run
                .glyphs
                .capacity()
                .saturating_mul(std::mem::size_of::<PositionedGlyph>()),
            Self::DrawImage(_)
            | Self::DrawInlineImage(_)
            | Self::PopClip
            | Self::PushBlendGroup { .. }
            | Self::PopBlendGroup => 0,
        };
        bytes as u64
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

/// Result of mapping a local-space stroke (`StrokePath` or `PushClipStroke`)
/// through an affine transform into destination coordinates.
#[derive(Debug, Clone)]
pub enum TransformedStroke {
    /// Positive-width stroke expanded into a filled outline in destination
    /// coordinates (emitted as `FillPath` / `PushClip` with `FillRule::NonZero`)
    /// so anisotropic scales and shears preserve directional stroke widths,
    /// elliptical caps/joins, and directional dash lengths.
    Filled(Path),
    /// Centerline path and adjusted stroke style in destination coordinates
    /// (used for uniform similarity transforms or 0-width PDF hairlines).
    Stroked(Path, StrokeStyle),
    /// Degenerate or empty stroke producing no geometry.
    Empty,
}

impl SoftMask {
    /// Map a soft mask's commands, bounds, and offset by `matrix` (e.g. from
    /// Type 3 glyph coordinates into page space), embedding any referenced
    /// `ImageId` via `resolve_image` and mapping stroked paths / stroke clips
    /// via `transform_stroke` (given the active local-space clip bounds) so
    /// directional stroke widths and clipped dashes are preserved under
    /// nonuniform transforms. Shared nested mask `Arc<DisplayList>` references
    /// at the same depth reuse one transformed allocation.
    pub fn transform_with_images<F, S>(
        &self,
        matrix: &Matrix,
        resolve_image: &F,
        transform_stroke: &S,
    ) -> Self
    where
        F: Fn(ImageId) -> Option<std::sync::Arc<InlineImage>>,
        S: Fn(&Path, &StrokeStyle, &Matrix, Option<Rect>) -> TransformedStroke,
    {
        let mut memo = std::collections::HashMap::new();
        self.transform_with_images_depth(matrix, resolve_image, transform_stroke, 0, &mut memo)
    }

    fn transform_with_images_depth<F, S>(
        &self,
        matrix: &Matrix,
        resolve_image: &F,
        transform_stroke: &S,
        depth: u8,
        memo: &mut std::collections::HashMap<(*const DisplayList, u8), std::sync::Arc<DisplayList>>,
    ) -> Self
    where
        F: Fn(ImageId) -> Option<std::sync::Arc<InlineImage>>,
        S: Fn(&Path, &StrokeStyle, &Matrix, Option<Rect>) -> TransformedStroke,
    {
        fn transform_rect(r: Rect, m: &Matrix) -> Rect {
            let corners = [
                zpdf_core::Point::new(r.x0, r.y0).transform(m),
                zpdf_core::Point::new(r.x1, r.y0).transform(m),
                zpdf_core::Point::new(r.x0, r.y1).transform(m),
                zpdf_core::Point::new(r.x1, r.y1).transform(m),
            ];
            let (mut x0, mut y0) = (f64::INFINITY, f64::INFINITY);
            let (mut x1, mut y1) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
            for p in corners {
                x0 = x0.min(p.x);
                y0 = y0.min(p.y);
                x1 = x1.max(p.x);
                y1 = y1.max(p.y);
            }
            if x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite() {
                Rect::new(x0, y0, x1, y1)
            } else {
                r
            }
        }
        fn path_local_bounds(path: &Path) -> Option<Rect> {
            let mut x0 = f64::INFINITY;
            let mut y0 = f64::INFINITY;
            let mut x1 = f64::NEG_INFINITY;
            let mut y1 = f64::NEG_INFINITY;
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
                .then(|| Rect::new(x0, y0, x1, y1))
        }
        fn intersect_clip(cur: Option<Rect>, next: Rect) -> Rect {
            let n = next.normalize();
            match cur {
                Some(c) => {
                    let c = c.normalize();
                    let x0 = c.x0.max(n.x0);
                    let y0 = c.y0.max(n.y0);
                    Rect::new(x0, y0, c.x1.min(n.x1).max(x0), c.y1.min(n.y1).max(y0))
                }
                None => n,
            }
        }
        fn transform_paint(paint: &Paint, m: &Matrix) -> Paint {
            match paint {
                Paint::Tiling(t) => Paint::Tiling(std::sync::Arc::new(TilingPattern {
                    commands: std::sync::Arc::clone(&t.commands),
                    bbox: t.bbox,
                    x_step: t.x_step,
                    y_step: t.y_step,
                    matrix: m.concat(&t.matrix),
                    tiling_type: t.tiling_type,
                })),
                other => other.clone(),
            }
        }

        let ox = matrix.a * self.offset.0 as f64 + matrix.c * self.offset.1 as f64;
        let oy = matrix.b * self.offset.0 as f64 + matrix.d * self.offset.1 as f64;
        let memo_key = (std::sync::Arc::as_ptr(&self.commands), depth);
        if let Some(cached) = memo.get(&memo_key) {
            return SoftMask {
                kind: self.kind,
                commands: std::sync::Arc::clone(cached),
                offset: (ox as f32, oy as f32),
                backdrop_luma: self.backdrop_luma,
                transfer: self.transfer.clone(),
            };
        }

        let mut dl = DisplayList::new(transform_rect(self.commands.page_rect, matrix));
        if depth < 8 {
            let mut clip_stack: Vec<Option<Rect>> = Vec::new();
            let mut cur_clip: Option<Rect> = None;
            for cmd in &self.commands.commands {
                match cmd {
                    RenderCommand::FillPath {
                        path,
                        rule,
                        paint,
                        alpha,
                        overprint,
                    } => dl.push(RenderCommand::FillPath {
                        path: path.map_points(|p| p.transform(matrix)),
                        rule: *rule,
                        paint: transform_paint(paint, matrix),
                        alpha: *alpha,
                        overprint: *overprint,
                    }),
                    RenderCommand::StrokePath {
                        path,
                        style,
                        paint,
                        alpha,
                        overprint,
                    } => match transform_stroke(path, style, matrix, cur_clip) {
                        TransformedStroke::Filled(outline) => dl.push(RenderCommand::FillPath {
                            path: outline,
                            rule: FillRule::NonZero,
                            paint: transform_paint(paint, matrix),
                            alpha: *alpha,
                            overprint: *overprint,
                        }),
                        TransformedStroke::Stroked(mapped_path, mapped_style) => {
                            dl.push(RenderCommand::StrokePath {
                                path: mapped_path,
                                style: mapped_style,
                                paint: transform_paint(paint, matrix),
                                alpha: *alpha,
                                overprint: *overprint,
                            })
                        }
                        TransformedStroke::Empty => {}
                    },
                    RenderCommand::PushClip { path, rule } => {
                        clip_stack.push(cur_clip);
                        if let Some(b) = path_local_bounds(path) {
                            cur_clip = Some(intersect_clip(cur_clip, b));
                        }
                        dl.push(RenderCommand::PushClip {
                            path: path.map_points(|p| p.transform(matrix)),
                            rule: *rule,
                        });
                    }
                    RenderCommand::PushClipStroke { path, style } => {
                        clip_stack.push(cur_clip);
                        let mapped = transform_stroke(path, style, matrix, cur_clip);
                        if let Some(b) = path_local_bounds(path) {
                            let reach = match style.join {
                                LineJoin::Miter => style.miter_limit.clamp(1.0, 100.0) as f64,
                                _ => std::f64::consts::SQRT_2,
                            };
                            let w = if style.width.is_finite() && style.width > 0.0 {
                                style.width as f64
                            } else {
                                1.0
                            };
                            let pad = w * 0.5 * reach.max(std::f64::consts::SQRT_2) + 1.0;
                            let padded = Rect::new(b.x0 - pad, b.y0 - pad, b.x1 + pad, b.y1 + pad);
                            cur_clip = Some(intersect_clip(cur_clip, padded));
                        }
                        match mapped {
                            TransformedStroke::Filled(outline) => {
                                dl.push(RenderCommand::PushClip {
                                    path: outline,
                                    rule: FillRule::NonZero,
                                })
                            }
                            TransformedStroke::Stroked(mapped_path, mapped_style) => {
                                dl.push(RenderCommand::PushClipStroke {
                                    path: mapped_path,
                                    style: mapped_style,
                                })
                            }
                            TransformedStroke::Empty => dl.push(RenderCommand::PushClip {
                                path: Path::new(),
                                rule: FillRule::NonZero,
                            }),
                        }
                    }
                    RenderCommand::PopClip => {
                        cur_clip = clip_stack.pop().flatten();
                        dl.push(RenderCommand::PopClip);
                    }
                    RenderCommand::DrawImage(draw) => {
                        if let Some(image) = resolve_image(draw.image_id) {
                            dl.push(RenderCommand::DrawInlineImage(InlineImageDraw {
                                image,
                                transform: matrix.concat(&draw.transform),
                                alpha: draw.alpha,
                            }));
                        } else {
                            dl.push(RenderCommand::DrawImage(ImageDraw {
                                image_id: draw.image_id,
                                transform: matrix.concat(&draw.transform),
                                alpha: draw.alpha,
                                is_image_mask: draw.is_image_mask,
                            }));
                        }
                    }
                    RenderCommand::DrawInlineImage(draw) => {
                        dl.push(RenderCommand::DrawInlineImage(InlineImageDraw {
                            image: std::sync::Arc::clone(&draw.image),
                            transform: matrix.concat(&draw.transform),
                            alpha: draw.alpha,
                        }))
                    }
                    RenderCommand::DrawGlyphRun(run) => {
                        let mut mapped = run.clone();
                        mapped.transform = matrix.concat(&run.transform);
                        mapped.paint = transform_paint(&run.paint, matrix);
                        dl.push(RenderCommand::DrawGlyphRun(mapped));
                    }
                    RenderCommand::PushBlendGroup {
                        blend_mode,
                        isolated,
                        knockout,
                        bounds,
                        alpha,
                        mask,
                    } => dl.push(RenderCommand::PushBlendGroup {
                        blend_mode: *blend_mode,
                        isolated: *isolated,
                        knockout: *knockout,
                        bounds: transform_rect(*bounds, matrix),
                        alpha: *alpha,
                        mask: mask.as_ref().map(|sub| {
                            sub.transform_with_images_depth(
                                matrix,
                                resolve_image,
                                transform_stroke,
                                depth + 1,
                                memo,
                            )
                        }),
                    }),
                    RenderCommand::PopBlendGroup => dl.push(RenderCommand::PopBlendGroup),
                }
            }
        }
        let commands = std::sync::Arc::new(dl);
        memo.insert(memo_key, std::sync::Arc::clone(&commands));
        SoftMask {
            kind: self.kind,
            commands,
            offset: (ox as f32, oy as f32),
            backdrop_luma: self.backdrop_luma,
            transfer: self.transfer.clone(),
        }
    }
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
    /// Stable 1-based tile cache index unique within the parent
    /// [`TilingPattern::blend_steps`] plan.
    pub segment_id: usize,
    /// Conservative local cell-space bounding box of painted marks in this
    /// segment (`None` when unbounded/unknown). Used to skip `(di, dj)`
    /// neighbor copies that cannot touch the fundamental tile period.
    pub bounds: Option<Rect>,
}

impl TilingBlendSegment {
    /// True when this segment shifted by `(di * x_step, dj * y_step)` can touch
    /// `tiling`'s fundamental period `[bx0, bx0 + |x_step|] × [by0, by0 + |y_step|]`.
    pub fn intersects_period(&self, tiling: &TilingPattern, di: i64, dj: i64) -> bool {
        let Some(b) = self.bounds else {
            return true;
        };
        let bbox = tiling.bbox.normalize();
        let abs_x = tiling.x_step.abs();
        let abs_y = tiling.y_step.abs();
        if ![
            bbox.x0,
            bbox.y0,
            abs_x,
            abs_y,
            b.x0,
            b.y0,
            b.x1,
            b.y1,
            tiling.x_step,
            tiling.y_step,
        ]
        .iter()
        .all(|v| v.is_finite())
        {
            return true;
        }
        let dx = di as f64 * tiling.x_step;
        let dy = dj as f64 * tiling.y_step;
        let eps = 1e-6;
        let sx0 = b.x0 + dx;
        let sx1 = b.x1 + dx;
        let sy0 = b.y0 + dy;
        let sy1 = b.y1 + dy;
        sx1 > bbox.x0 - eps
            && sx0 < bbox.x0 + abs_x + eps
            && sy1 > bbox.y0 - eps
            && sy0 < bbox.y0 + abs_y + eps
    }

    /// True when this segment is a provable exact identity over any opaque
    /// backdrop (`B_a == 1`): pure-white `Multiply`/`Darken` or pure-black
    /// `Screen`/`Lighten` solid path fills/strokes with no overprint or nested
    /// groups.
    pub fn is_identity_on_opaque(&self) -> bool {
        let want_white = match self.blend_mode {
            BlendMode::Multiply | BlendMode::Darken => true,
            BlendMode::Screen | BlendMode::Lighten => false,
            _ => return false,
        };
        self.commands.commands.iter().all(|cmd| match cmd {
            RenderCommand::FillPath {
                paint: Paint::Solid(c),
                overprint: None,
                ..
            }
            | RenderCommand::StrokePath {
                paint: Paint::Solid(c),
                overprint: None,
                ..
            } => {
                if want_white {
                    c.r >= 1.0 && c.g >= 1.0 && c.b >= 1.0
                } else {
                    c.r <= 0.0 && c.g <= 0.0 && c.b <= 0.0
                }
            }
            RenderCommand::PushClip { .. }
            | RenderCommand::PushClipStroke { .. }
            | RenderCommand::PopClip => true,
            _ => false,
        })
    }
}

/// One step in a decomposed backdrop-dependent [`TilingPattern`] plan.
#[derive(Debug, Clone)]
pub enum TilingBlendStep {
    /// Rasterize a single blend-mode slice into a period tile and composite it
    /// against the live destination with `blend_mode`.
    Segment(TilingBlendSegment),
    /// Evaluate `steps` at full outer opacity (`1.0`) against a copy of the
    /// live destination backdrop, then mix the finished result back into the
    /// destination with constant `alpha` (and optional per-pixel `mask`
    /// coverage from a lifted nested shape) ONCE (ISO 32000-1 §11.4.5 / §11.6.7).
    OpacityGroup {
        alpha: f32,
        /// Optional coverage mask segment (opaque white shape under active clips)
        /// for a lifted multi-step nested tiling paint, ensuring the nested
        /// shape's anti-aliased edge coverage is applied once across all child
        /// steps rather than once per segment.
        mask: Option<TilingBlendSegment>,
        steps: Vec<TilingBlendStep>,
    },
}

impl TilingBlendStep {
    /// Total number of leaf [`TilingBlendSegment`]s in `steps`.
    pub fn count_segments(steps: &[Self]) -> usize {
        steps
            .iter()
            .map(|s| match s {
                Self::Segment(_) => 1,
                Self::OpacityGroup { steps, .. } => Self::count_segments(steps),
            })
            .sum()
    }

    /// True when at least one leaf segment in `steps` can touch `tiling`'s
    /// fundamental period under neighbor shift `(di, dj)`.
    pub fn any_intersects_period(steps: &[Self], tiling: &TilingPattern, di: i64, dj: i64) -> bool {
        steps.iter().any(|s| match s {
            Self::Segment(seg) => seg.intersects_period(tiling, di, dj),
            Self::OpacityGroup { mask, steps, .. } => {
                mask.as_ref()
                    .is_none_or(|m| m.intersects_period(tiling, di, dj))
                    && Self::any_intersects_period(steps, tiling, di, dj)
            }
        })
    }

    /// Count how many leaf [`TilingBlendSegment`] passes in `steps` intersect
    /// `tiling`'s fundamental period under neighbor shift `(di, dj)`.
    pub fn count_intersecting_passes(
        steps: &[Self],
        tiling: &TilingPattern,
        di: i64,
        dj: i64,
    ) -> usize {
        steps
            .iter()
            .map(|s| match s {
                Self::Segment(seg) => usize::from(seg.intersects_period(tiling, di, dj)),
                Self::OpacityGroup { mask, steps, .. } => {
                    if mask
                        .as_ref()
                        .is_some_and(|m| !m.intersects_period(tiling, di, dj))
                    {
                        0
                    } else {
                        Self::count_intersecting_passes(steps, tiling, di, dj)
                    }
                }
            })
            .sum()
    }

    /// If every leaf segment in `steps` shares the same [`BlendMode`] and there
    /// is at least one segment, return that mode.
    pub fn uniform_blend_mode(steps: &[Self]) -> Option<BlendMode> {
        let mut mode: Option<BlendMode> = None;
        fn visit(steps: &[TilingBlendStep], mode: &mut Option<BlendMode>) -> bool {
            for s in steps {
                match s {
                    TilingBlendStep::Segment(seg) => match *mode {
                        None => *mode = Some(seg.blend_mode),
                        Some(m) if m == seg.blend_mode => {}
                        Some(_) => return false,
                    },
                    TilingBlendStep::OpacityGroup { steps, .. } => {
                        if !visit(steps, mode) {
                            return false;
                        }
                    }
                }
            }
            true
        }
        if visit(steps, &mut mode) {
            mode
        } else {
            None
        }
    }

    /// True when every leaf blend operation in `steps` is strictly affine in
    /// the premultiplied backdrop `(B_rgb, B_a)` per channel (`Normal`,
    /// `Multiply`, `Screen`, and `OpacityGroup` wrappers around them), so the
    /// combined effect of arbitrary overlapping copies is exactly represented
    /// by transparent (`T, a_t`), black (`K`), and white (`K + M`) samples:
    ///
    /// - `out_a = a_t + (1 - a_t) * B_a`
    /// - `out_rgb = (1 - B_a) * T + B_a * K + M * B_rgb`
    ///
    /// Non-affine blend modes such as `Darken` (`min(B, c)`), `Lighten`,
    /// `Overlay`, `ColorDodge`, `Difference`, or non-separable HSL modes return
    /// `false` so backends preserve copy order against the actual backdrop.
    pub fn is_affine_in_backdrop(steps: &[Self]) -> bool {
        steps.iter().all(|step| match step {
            Self::Segment(seg) => {
                matches!(
                    seg.blend_mode,
                    BlendMode::Normal | BlendMode::Multiply | BlendMode::Screen
                ) || seg.is_identity_on_opaque()
            }
            Self::OpacityGroup { steps, .. } => Self::is_affine_in_backdrop(steps),
        })
    }
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
        commands_ink_bounds(&self.commands.commands, self.bbox)
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
    /// composited onto the destination with its own [`BlendMode`]. Returns
    /// `None` when the cell requires a non-isolated [`TilingBlendStep::OpacityGroup`]
    /// (use [`Self::blend_steps`] to handle those too) or cannot be segmented.
    pub fn blend_segments(&self) -> Option<Vec<TilingBlendSegment>> {
        let steps = self.blend_steps()?;
        let mut out = Vec::with_capacity(steps.len());
        for step in steps {
            match step {
                TilingBlendStep::Segment(seg) => out.push(seg),
                TilingBlendStep::OpacityGroup { .. } => return None,
            }
        }
        Some(out)
    }

    /// Split a backdrop-dependent cell into sequential [`TilingBlendStep`]s
    /// (blend-mode slices and non-isolated constant-opacity groups). Recursively
    /// lifts non-overlapping nested backdrop-dependent [`TilingPattern`] paints
    /// — including translucent multi-segment inner patterns — and non-isolated
    /// `Normal` opacity groups so dense cells stay backdrop-aware. Returns
    /// `None` when the cell uses active overprint, non-passthrough masked
    /// non-isolated groups, overlapping nested backdrop-dependent tiling
    /// patterns, or exceeds the segment/depth cap.
    pub fn blend_steps(&self) -> Option<Vec<TilingBlendStep>> {
        let mut memo = std::collections::HashMap::new();
        let mut next_seg_id = 1usize;
        self.blend_steps_for_slice(&self.commands.commands, &[], 0, &mut memo, &mut next_seg_id)
    }

    fn blend_steps_for_slice(
        &self,
        cmds: &[RenderCommand],
        initial_clips: &[RenderCommand],
        depth: u8,
        memo: &mut std::collections::HashMap<(usize, u8), bool>,
        next_seg_id: &mut usize,
    ) -> Option<Vec<TilingBlendStep>> {
        const MAX_SEGMENTS: usize = 256;
        if depth > MAX_TILING_BACKDROP_SCAN_DEPTH {
            return None;
        }
        let page_rect = self.commands.page_rect;
        let bbox = self.bbox;
        type AssocRun = (BlendMode, Vec<Vec<RenderCommand>>, Option<Rect>);
        let mut steps: Vec<TilingBlendStep> = Vec::new();
        let mut clip_stack: Vec<RenderCommand> = initial_clips.to_vec();
        let mut cur_normal: Vec<RenderCommand> = Vec::new();
        let mut cur_assoc: Option<AssocRun> = None;
        let mut passthrough_pops: Vec<usize> = Vec::new();

        let make_segment = |blend_mode: BlendMode,
                            seg_cmds: Vec<RenderCommand>,
                            next_seg_id: &mut usize|
         -> Option<TilingBlendSegment> {
            if *next_seg_id > MAX_SEGMENTS {
                return None;
            }
            let segment_id = *next_seg_id;
            *next_seg_id += 1;
            let bounds = commands_ink_bounds(&seg_cmds, bbox);
            let mut dl = DisplayList::new(page_rect);
            dl.commands = seg_cmds;
            Some(TilingBlendSegment {
                blend_mode,
                commands: std::sync::Arc::new(dl),
                segment_id,
                bounds,
            })
        };

        let flush_assoc = |cur_assoc: &mut Option<AssocRun>,
                           steps: &mut Vec<TilingBlendStep>,
                           next_seg_id: &mut usize|
         -> Option<()> {
            let Some((mode, mut items, bounds)) = cur_assoc.take() else {
                return Some(());
            };
            if items.is_empty() {
                return Some(());
            }
            if *next_seg_id > MAX_SEGMENTS {
                return None;
            }
            let segment_id = *next_seg_id;
            *next_seg_id += 1;
            let seg_cmds = if items.len() == 1 {
                items.pop().unwrap()
            } else {
                // Porter-Duff premultiplied Multiply and Screen are strictly
                // associative over ALL colors and alphas starting from
                // transparent (0,0,0,0), so consecutive runs coalesce into one
                // backdrop-independent tile segment wrapped in an isolated
                // Normal group.
                let mut v = Vec::new();
                v.push(RenderCommand::PushBlendGroup {
                    blend_mode: BlendMode::Normal,
                    isolated: true,
                    knockout: false,
                    bounds: bbox,
                    alpha: 1.0,
                    mask: None,
                });
                let mut iter = items.into_iter();
                if let Some(first) = iter.next() {
                    v.extend(first);
                }
                for item in iter {
                    v.push(RenderCommand::PushBlendGroup {
                        blend_mode: mode,
                        isolated: true,
                        knockout: false,
                        bounds: bbox,
                        alpha: 1.0,
                        mask: None,
                    });
                    v.extend(item);
                    v.push(RenderCommand::PopBlendGroup);
                }
                v.push(RenderCommand::PopBlendGroup);
                v
            };
            let mut dl = DisplayList::new(page_rect);
            dl.commands = seg_cmds;
            steps.push(TilingBlendStep::Segment(TilingBlendSegment {
                blend_mode: mode,
                commands: std::sync::Arc::new(dl),
                segment_id,
                bounds,
            }));
            Some(())
        };

        let flush_normal = |cur_normal: &mut Vec<RenderCommand>,
                            cur_assoc: &mut Option<AssocRun>,
                            clip_stack: &[RenderCommand],
                            steps: &mut Vec<TilingBlendStep>,
                            next_seg_id: &mut usize|
         -> Option<()> {
            flush_assoc(cur_assoc, steps, next_seg_id)?;
            if cur_normal.is_empty() {
                return Some(());
            }
            for _ in 0..clip_stack.len() {
                cur_normal.push(RenderCommand::PopClip);
            }
            let seg = make_segment(BlendMode::Normal, std::mem::take(cur_normal), next_seg_id)?;
            steps.push(TilingBlendStep::Segment(seg));
            Some(())
        };

        let wrap_cmd_in_clips = |cmd: RenderCommand, clip_stack: &[RenderCommand]| {
            let mut seg_cmds = Vec::with_capacity(clip_stack.len() * 2 + 1);
            seg_cmds.extend(clip_stack.iter().cloned());
            seg_cmds.push(cmd);
            for _ in 0..clip_stack.len() {
                seg_cmds.push(RenderCommand::PopClip);
            }
            seg_cmds
        };

        #[allow(clippy::too_many_arguments)]
        fn lift_inner_steps<F>(
            inner: &TilingPattern,
            inner_steps: Vec<TilingBlendStep>,
            leaf_alpha: f32,
            clip_stack: &[RenderCommand],
            page_rect: Rect,
            bbox: Rect,
            bounds_override: Option<Option<Rect>>,
            next_seg_id: &mut usize,
            build_cmd: &F,
        ) -> Option<Vec<TilingBlendStep>>
        where
            F: Fn(std::sync::Arc<TilingPattern>, f32) -> RenderCommand,
        {
            let mut out = Vec::with_capacity(inner_steps.len());
            for step in inner_steps {
                match step {
                    TilingBlendStep::Segment(seg) => {
                        if *next_seg_id > MAX_SEGMENTS {
                            return None;
                        }
                        let segment_id = *next_seg_id;
                        *next_seg_id += 1;
                        let mut lifted = inner.clone();
                        lifted.commands = seg.commands;
                        let cmd = build_cmd(std::sync::Arc::new(lifted), leaf_alpha);
                        let mut seg_cmds = Vec::with_capacity(clip_stack.len() * 2 + 1);
                        seg_cmds.extend(clip_stack.iter().cloned());
                        seg_cmds.push(cmd);
                        for _ in 0..clip_stack.len() {
                            seg_cmds.push(RenderCommand::PopClip);
                        }
                        let bounds =
                            bounds_override.unwrap_or_else(|| commands_ink_bounds(&seg_cmds, bbox));
                        let mut dl = DisplayList::new(page_rect);
                        dl.commands = seg_cmds;
                        out.push(TilingBlendStep::Segment(TilingBlendSegment {
                            blend_mode: seg.blend_mode,
                            commands: std::sync::Arc::new(dl),
                            segment_id,
                            bounds,
                        }));
                    }
                    TilingBlendStep::OpacityGroup { alpha, mask, steps } => {
                        let lifted_mask = match mask {
                            Some(m) => {
                                if *next_seg_id > MAX_SEGMENTS {
                                    return None;
                                }
                                let segment_id = *next_seg_id;
                                *next_seg_id += 1;
                                let mut lifted = inner.clone();
                                lifted.commands = m.commands;
                                let cmd = build_cmd(std::sync::Arc::new(lifted), 1.0);
                                let mut seg_cmds = Vec::with_capacity(clip_stack.len() * 2 + 1);
                                seg_cmds.extend(clip_stack.iter().cloned());
                                seg_cmds.push(cmd);
                                for _ in 0..clip_stack.len() {
                                    seg_cmds.push(RenderCommand::PopClip);
                                }
                                let bounds = bounds_override
                                    .unwrap_or_else(|| commands_ink_bounds(&seg_cmds, bbox));
                                let mut dl = DisplayList::new(page_rect);
                                dl.commands = seg_cmds;
                                Some(TilingBlendSegment {
                                    blend_mode: BlendMode::Normal,
                                    commands: std::sync::Arc::new(dl),
                                    segment_id,
                                    bounds,
                                })
                            }
                            None => None,
                        };
                        let lifted_children = lift_inner_steps(
                            inner,
                            steps,
                            1.0,
                            clip_stack,
                            page_rect,
                            bbox,
                            bounds_override,
                            next_seg_id,
                            build_cmd,
                        )?;
                        out.push(TilingBlendStep::OpacityGroup {
                            alpha,
                            mask: lifted_mask,
                            steps: lifted_children,
                        });
                    }
                }
            }
            Some(out)
        }

        let rect_cover_path = |r: Rect| -> Path {
            let r = r.normalize();
            let pad = 1.0;
            let mut p = Path::new();
            p.move_to(zpdf_core::Point::new(r.x0 - pad, r.y0 - pad));
            p.line_to(zpdf_core::Point::new(r.x1 + pad, r.y0 - pad));
            p.line_to(zpdf_core::Point::new(r.x1 + pad, r.y1 + pad));
            p.line_to(zpdf_core::Point::new(r.x0 - pad, r.y1 + pad));
            p.close();
            p
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
                            let mut inner_seg_id = 1usize;
                            let inner_steps = inner.blend_steps_for_slice(
                                &inner.commands.commands,
                                &[],
                                depth + 1,
                                memo,
                                &mut inner_seg_id,
                            )?;
                            let multi_step = inner_steps.len() > 1
                                || matches!(
                                    inner_steps.first(),
                                    Some(TilingBlendStep::OpacityGroup { .. })
                                );
                            if multi_step {
                                flush_normal(
                                    &mut cur_normal,
                                    &mut cur_assoc,
                                    &clip_stack,
                                    &mut steps,
                                    next_seg_id,
                                )?;
                                // Capture the nested fill path + active clip coverage ONCE in
                                // `mask_seg`, and lift every child step onto a full-coverage
                                // rect so anti-aliased nested path edges are never blended
                                // multiple times across child segments.
                                let mask_cmd = RenderCommand::FillPath {
                                    path: path.clone(),
                                    rule: *rule,
                                    paint: Paint::Solid(Color::white()),
                                    alpha: 1.0,
                                    overprint: None,
                                };
                                let mask_seg = make_segment(
                                    BlendMode::Normal,
                                    wrap_cmd_in_clips(mask_cmd, &clip_stack),
                                    next_seg_id,
                                )?;
                                let cover_path = rect_cover_path(mask_seg.bounds.unwrap_or(bbox));
                                let build_cover = |pat: std::sync::Arc<TilingPattern>, a: f32| {
                                    RenderCommand::FillPath {
                                        path: cover_path.clone(),
                                        rule: FillRule::NonZero,
                                        paint: Paint::Tiling(pat),
                                        alpha: a,
                                        overprint: *overprint,
                                    }
                                };
                                let lifted = lift_inner_steps(
                                    inner,
                                    inner_steps,
                                    1.0,
                                    &[],
                                    page_rect,
                                    bbox,
                                    Some(mask_seg.bounds),
                                    next_seg_id,
                                    &build_cover,
                                )?;
                                steps.push(TilingBlendStep::OpacityGroup {
                                    alpha: *alpha,
                                    mask: Some(mask_seg),
                                    steps: lifted,
                                });
                            } else {
                                let build = |pat: std::sync::Arc<TilingPattern>, a: f32| {
                                    RenderCommand::FillPath {
                                        path: path.clone(),
                                        rule: *rule,
                                        paint: Paint::Tiling(pat),
                                        alpha: a,
                                        overprint: *overprint,
                                    }
                                };
                                let lifted = lift_inner_steps(
                                    inner,
                                    inner_steps,
                                    *alpha,
                                    &clip_stack,
                                    page_rect,
                                    bbox,
                                    None,
                                    next_seg_id,
                                    &build,
                                )?;
                                for step in lifted {
                                    flush_normal(
                                        &mut cur_normal,
                                        &mut cur_assoc,
                                        &clip_stack,
                                        &mut steps,
                                        next_seg_id,
                                    )?;
                                    steps.push(step);
                                }
                            }
                            idx += 1;
                            continue;
                        }
                    }
                    if cur_normal.is_empty() {
                        flush_assoc(&mut cur_assoc, &mut steps, next_seg_id)?;
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
                            let mut inner_seg_id = 1usize;
                            let inner_steps = inner.blend_steps_for_slice(
                                &inner.commands.commands,
                                &[],
                                depth + 1,
                                memo,
                                &mut inner_seg_id,
                            )?;
                            let multi_step = inner_steps.len() > 1
                                || matches!(
                                    inner_steps.first(),
                                    Some(TilingBlendStep::OpacityGroup { .. })
                                );
                            flush_normal(
                                &mut cur_normal,
                                &mut cur_assoc,
                                &clip_stack,
                                &mut steps,
                                next_seg_id,
                            )?;
                            if multi_step {
                                let mask_cmd = RenderCommand::StrokePath {
                                    path: path.clone(),
                                    style: style.clone(),
                                    paint: Paint::Solid(Color::white()),
                                    alpha: 1.0,
                                    overprint: None,
                                };
                                let mask_seg = make_segment(
                                    BlendMode::Normal,
                                    wrap_cmd_in_clips(mask_cmd, &clip_stack),
                                    next_seg_id,
                                )?;
                                let cover_path = rect_cover_path(mask_seg.bounds.unwrap_or(bbox));
                                let build_cover = |pat: std::sync::Arc<TilingPattern>, a: f32| {
                                    RenderCommand::FillPath {
                                        path: cover_path.clone(),
                                        rule: FillRule::NonZero,
                                        paint: Paint::Tiling(pat),
                                        alpha: a,
                                        overprint: *overprint,
                                    }
                                };
                                let lifted = lift_inner_steps(
                                    inner,
                                    inner_steps,
                                    1.0,
                                    &[],
                                    page_rect,
                                    bbox,
                                    Some(mask_seg.bounds),
                                    next_seg_id,
                                    &build_cover,
                                )?;
                                steps.push(TilingBlendStep::OpacityGroup {
                                    alpha: *alpha,
                                    mask: Some(mask_seg),
                                    steps: lifted,
                                });
                            } else {
                                let build = |pat: std::sync::Arc<TilingPattern>, a: f32| {
                                    RenderCommand::StrokePath {
                                        path: path.clone(),
                                        style: style.clone(),
                                        paint: Paint::Tiling(pat),
                                        alpha: a,
                                        overprint: *overprint,
                                    }
                                };
                                let lifted = lift_inner_steps(
                                    inner,
                                    inner_steps,
                                    *alpha,
                                    &clip_stack,
                                    page_rect,
                                    bbox,
                                    None,
                                    next_seg_id,
                                    &build,
                                )?;
                                steps.extend(lifted);
                            }
                            idx += 1;
                            continue;
                        }
                    }
                    if cur_normal.is_empty() {
                        flush_assoc(&mut cur_assoc, &mut steps, next_seg_id)?;
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
                            let mut inner_seg_id = 1usize;
                            let inner_steps = inner.blend_steps_for_slice(
                                &inner.commands.commands,
                                &[],
                                depth + 1,
                                memo,
                                &mut inner_seg_id,
                            )?;
                            let multi_step = inner_steps.len() > 1
                                || matches!(
                                    inner_steps.first(),
                                    Some(TilingBlendStep::OpacityGroup { .. })
                                );
                            flush_normal(
                                &mut cur_normal,
                                &mut cur_assoc,
                                &clip_stack,
                                &mut steps,
                                next_seg_id,
                            )?;
                            if multi_step {
                                let mut mask_run = run.clone();
                                mask_run.paint = Paint::Solid(Color::white());
                                mask_run.alpha = 1.0;
                                mask_run.overprint = None;
                                let mask_seg = make_segment(
                                    BlendMode::Normal,
                                    wrap_cmd_in_clips(
                                        RenderCommand::DrawGlyphRun(mask_run),
                                        &clip_stack,
                                    ),
                                    next_seg_id,
                                )?;
                                let cover_path = rect_cover_path(mask_seg.bounds.unwrap_or(bbox));
                                let build_cover = |pat: std::sync::Arc<TilingPattern>, a: f32| {
                                    RenderCommand::FillPath {
                                        path: cover_path.clone(),
                                        rule: FillRule::NonZero,
                                        paint: Paint::Tiling(pat),
                                        alpha: a,
                                        overprint: run.overprint,
                                    }
                                };
                                let lifted = lift_inner_steps(
                                    inner,
                                    inner_steps,
                                    1.0,
                                    &[],
                                    page_rect,
                                    bbox,
                                    Some(mask_seg.bounds),
                                    next_seg_id,
                                    &build_cover,
                                )?;
                                steps.push(TilingBlendStep::OpacityGroup {
                                    alpha: run.alpha,
                                    mask: Some(mask_seg),
                                    steps: lifted,
                                });
                            } else {
                                let build = |pat: std::sync::Arc<TilingPattern>, a: f32| {
                                    let mut lifted_run = run.clone();
                                    lifted_run.paint = Paint::Tiling(pat);
                                    lifted_run.alpha = a;
                                    RenderCommand::DrawGlyphRun(lifted_run)
                                };
                                let lifted = lift_inner_steps(
                                    inner,
                                    inner_steps,
                                    run.alpha,
                                    &clip_stack,
                                    page_rect,
                                    bbox,
                                    None,
                                    next_seg_id,
                                    &build,
                                )?;
                                steps.extend(lifted);
                            }
                            idx += 1;
                            continue;
                        }
                    }
                    if cur_normal.is_empty() {
                        flush_assoc(&mut cur_assoc, &mut steps, next_seg_id)?;
                        cur_normal.extend(clip_stack.iter().cloned());
                    }
                    cur_normal.push(cmds[idx].clone());
                    idx += 1;
                }
                RenderCommand::DrawImage(_) | RenderCommand::DrawInlineImage(_) => {
                    if cur_normal.is_empty() {
                        flush_assoc(&mut cur_assoc, &mut steps, next_seg_id)?;
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
                                flush_assoc(&mut cur_assoc, &mut steps, next_seg_id)?;
                                cur_normal.extend(clip_stack.iter().cloned());
                            }
                            cur_normal.extend(cmds[idx..=end].iter().cloned());
                            idx = end + 1;
                        } else if !*knockout && *alpha >= 1.0 && mask.is_none() {
                            // Passthrough non-isolated group: flatten into outer stream.
                            passthrough_pops.push(end);
                            idx += 1;
                        } else if !*knockout && mask.is_none() {
                            // Non-isolated constant-opacity group: keep live backdrop
                            // across its decomposed steps and apply `alpha` once.
                            flush_normal(
                                &mut cur_normal,
                                &mut cur_assoc,
                                &clip_stack,
                                &mut steps,
                                next_seg_id,
                            )?;
                            let inner_steps = self.blend_steps_for_slice(
                                inner,
                                &clip_stack,
                                depth + 1,
                                memo,
                                next_seg_id,
                            )?;
                            steps.push(TilingBlendStep::OpacityGroup {
                                alpha: *alpha,
                                mask: None,
                                steps: inner_steps,
                            });
                            idx = end + 1;
                        } else {
                            return None;
                        }
                    } else {
                        if !*isolated && inner_backdrop {
                            return None;
                        }
                        let is_assoc =
                            matches!(*blend_mode, BlendMode::Multiply | BlendMode::Screen);
                        if !is_assoc
                            || cur_assoc
                                .as_ref()
                                .is_some_and(|(m, _, _)| *m != *blend_mode)
                        {
                            flush_assoc(&mut cur_assoc, &mut steps, next_seg_id)?;
                        }
                        if !cur_normal.is_empty() {
                            for _ in 0..clip_stack.len() {
                                cur_normal.push(RenderCommand::PopClip);
                            }
                            let seg = make_segment(
                                BlendMode::Normal,
                                std::mem::take(&mut cur_normal),
                                next_seg_id,
                            )?;
                            steps.push(TilingBlendStep::Segment(seg));
                        }
                        let needs_wrapper = *alpha < 1.0 || mask.is_some() || *knockout;
                        let seg_cmds = if needs_wrapper {
                            let mut v = Vec::with_capacity(clip_stack.len() * 2 + inner.len() + 2);
                            v.extend(clip_stack.iter().cloned());
                            v.push(RenderCommand::PushBlendGroup {
                                blend_mode: BlendMode::Normal,
                                isolated: true,
                                knockout: *knockout,
                                bounds: *bounds,
                                alpha: *alpha,
                                mask: mask.clone(),
                            });
                            v.extend(inner.iter().cloned());
                            v.push(RenderCommand::PopBlendGroup);
                            for _ in 0..clip_stack.len() {
                                v.push(RenderCommand::PopClip);
                            }
                            v
                        } else if inner.len() == 1 {
                            wrap_cmd_in_clips(inner[0].clone(), &clip_stack)
                        } else {
                            let mut v = Vec::with_capacity(clip_stack.len() * 2 + inner.len());
                            v.extend(clip_stack.iter().cloned());
                            v.extend(inner.iter().cloned());
                            for _ in 0..clip_stack.len() {
                                v.push(RenderCommand::PopClip);
                            }
                            v
                        };
                        if is_assoc {
                            let item_bounds = commands_ink_bounds(&seg_cmds, bbox);
                            match cur_assoc.as_mut() {
                                Some((_, items, acc_bounds)) => {
                                    items.push(seg_cmds);
                                    *acc_bounds = match (*acc_bounds, item_bounds) {
                                        (Some(a), Some(b)) => Some(Rect::new(
                                            a.x0.min(b.x0),
                                            a.y0.min(b.y0),
                                            a.x1.max(b.x1),
                                            a.y1.max(b.y1),
                                        )),
                                        (a, b) => a.or(b),
                                    };
                                }
                                None => {
                                    cur_assoc = Some((*blend_mode, vec![seg_cmds], item_bounds));
                                }
                            }
                        } else {
                            let seg = make_segment(*blend_mode, seg_cmds, next_seg_id)?;
                            steps.push(TilingBlendStep::Segment(seg));
                        }
                        idx = end + 1;
                    }
                }
                RenderCommand::PopBlendGroup => {
                    idx += 1;
                }
            }
        }
        flush_normal(
            &mut cur_normal,
            &mut cur_assoc,
            &clip_stack,
            &mut steps,
            next_seg_id,
        )?;
        Some(steps)
    }
}

fn commands_ink_bounds(cmds: &[RenderCommand], bbox: Rect) -> Option<Rect> {
    let bbox = bbox.normalize();
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

    for cmd in cmds {
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
                        let factor = match style.join {
                            LineJoin::Miter => {
                                if style.miter_limit.is_finite() {
                                    (style.miter_limit as f64).max(1.0)
                                } else {
                                    return Some(bbox);
                                }
                            }
                            LineJoin::Round | LineJoin::Bevel => 1.0,
                        };
                        let p = (w * 0.5 * factor).max(w);
                        if !p.is_finite() {
                            return Some(bbox);
                        }
                        p
                    } else {
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
            RenderCommand::DrawInlineImage(draw) => {
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
    /// Inherit the enclosing paint (used inside Type 3 glyph CharProcs before
    /// an explicit `d0` color operator is executed, and for all marks inside a
    /// `d1` CharProc per ISO 32000-1 §9.6.5).
    Inherited,
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
    fn clear_command_ranges_invalidates_all_indexed_ranges_for_in_place_edits() {
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        for _ in 0..4 {
            dl.push(RenderCommand::PopClip);
        }
        dl.push_expanded_glyph_run(0, 1, sample_glyph_run(1));
        dl.push_type3_glyph_range(0, 1);
        dl.push_pattern_range(2, 4);

        dl.clear_command_ranges();
        dl.commands.remove(0);

        assert!(dl.expanded_glyph_runs().is_empty());
        assert!(dl.type3_glyph_ranges().is_empty());
        assert!(dl.pattern_ranges().is_empty());
        assert!(!dl.is_in_type3_glyph_range(0));
        assert!(!dl.is_in_pattern_range(2));
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

    #[test]
    fn pattern_ranges_track_membership_and_truncate_cleanly() {
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        for _ in 0..6 {
            dl.push(RenderCommand::PopClip);
        }
        dl.push_pattern_range(1, 3);
        dl.push_pattern_range(4, 6);
        assert!(!dl.is_in_pattern_range(0));
        assert!(dl.is_in_pattern_range(1));
        assert!(dl.is_in_pattern_range(2));
        assert!(!dl.is_in_pattern_range(3));
        assert!(dl.is_in_pattern_range(4));
        assert!(dl.is_in_pattern_range(5));
        assert!(!dl.is_in_pattern_range(6));

        dl.truncate_commands(4);
        assert_eq!(dl.commands.len(), 4);
        assert_eq!(dl.pattern_ranges(), &[(1, 3)]);
        assert!(!dl.is_in_pattern_range(4));

        // Partial truncation inside `[1, 3)` preserves the surviving `[1, 2)` prefix.
        dl.truncate_commands(2);
        assert_eq!(dl.commands.len(), 2);
        assert_eq!(dl.pattern_ranges(), &[(1, 2)]);
        assert!(dl.is_in_pattern_range(1));
        assert!(!dl.is_in_pattern_range(2));

        // Truncating to the start of the range drops it completely.
        dl.truncate_commands(1);
        assert_eq!(dl.commands.len(), 1);
        assert!(dl.pattern_ranges().is_empty());
        assert!(!dl.is_in_pattern_range(1));
    }

    #[test]
    fn type3_glyph_ranges_track_membership_and_truncate_cleanly() {
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        for _ in 0..5 {
            dl.push(RenderCommand::PopClip);
        }
        dl.push_type3_glyph_range(1, 3);
        dl.push_type3_glyph_range(3, 5);
        assert!(!dl.is_in_type3_glyph_range(0));
        assert!(dl.is_in_type3_glyph_range(1));
        assert!(dl.is_in_type3_glyph_range(2));
        assert!(dl.is_in_type3_glyph_range(3));
        assert!(dl.is_in_type3_glyph_range(4));
        assert!(!dl.is_in_type3_glyph_range(5));

        dl.truncate_commands(2);
        assert_eq!(dl.type3_glyph_ranges(), &[(1, 2)]);
        assert!(dl.is_in_type3_glyph_range(1));
        assert!(!dl.is_in_type3_glyph_range(2));
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

    #[test]
    fn blend_steps_wraps_translucent_multi_segment_inner_tiling_in_opacity_group() {
        // Two non-coalesceable segments (Multiply + Darken) inside `inner` so
        // lifting must wrap them in a single OpacityGroup carrying the nested
        // shape mask once.
        let mut inner_cmds = Vec::new();
        for mode in [BlendMode::Multiply, BlendMode::Darken] {
            inner_cmds.push(RenderCommand::PushBlendGroup {
                blend_mode: mode,
                isolated: true,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
                alpha: 1.0,
                mask: None,
            });
            inner_cmds.push(RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 10.0, 10.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::white()),
                alpha: 1.0,
                overprint: None,
            });
            inner_cmds.push(RenderCommand::PopBlendGroup);
        }
        let inner = Arc::new(tiling_with(inner_cmds));
        let outer = tiling_with(vec![RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 10.0, 10.0),
            rule: FillRule::NonZero,
            paint: Paint::Tiling(inner),
            alpha: 0.5,
            overprint: None,
        }]);
        let steps = outer
            .blend_steps()
            .expect("translucent multi-segment inner tiling must decompose into OpacityGroup");
        assert_eq!(steps.len(), 1);
        match &steps[0] {
            TilingBlendStep::OpacityGroup { alpha, mask, steps } => {
                assert!((*alpha - 0.5).abs() < 1e-6);
                assert!(mask.is_some(), "nested shape mask must be captured once");
                assert_eq!(steps.len(), 2);
                assert!(matches!(
                    &steps[0],
                    TilingBlendStep::Segment(TilingBlendSegment {
                        blend_mode: BlendMode::Multiply,
                        ..
                    })
                ));
                assert!(matches!(
                    &steps[1],
                    TilingBlendStep::Segment(TilingBlendSegment {
                        blend_mode: BlendMode::Darken,
                        ..
                    })
                ));
            }
            other => panic!("expected OpacityGroup, got {other:?}"),
        }
    }

    #[test]
    fn blend_steps_coalesces_consecutive_multiply_and_screen_marks() {
        let mut cmds = Vec::new();
        for _ in 0..205 {
            cmds.push(RenderCommand::PushBlendGroup {
                blend_mode: BlendMode::Multiply,
                isolated: true,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
                alpha: 1.0,
                mask: None,
            });
            cmds.push(RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 10.0, 10.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::white()),
                alpha: 1.0,
                overprint: None,
            });
            cmds.push(RenderCommand::PopBlendGroup);
        }
        let pat = tiling_with(cmds);
        let segs = pat
            .blend_segments()
            .expect("205 consecutive Multiply marks must coalesce");
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].blend_mode, BlendMode::Multiply);
        // The coalesced segment commands must remain backdrop-independent.
        assert!(!TilingPattern {
            commands: Arc::clone(&segs[0].commands),
            bbox: pat.bbox,
            x_step: pat.x_step,
            y_step: pat.y_step,
            matrix: pat.matrix,
            tiling_type: pat.tiling_type,
        }
        .is_backdrop_dependent());
    }

    #[test]
    fn blend_segment_intersects_period_skips_disjoint_neighbor_copy() {
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 8.0, 4.0));
        // Mark 1 spans [0..8] × [0..2] (intersects both di=-1 and di=0).
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Multiply,
            isolated: true,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 8.0, 4.0),
            alpha: 1.0,
            mask: None,
        });
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 8.0, 2.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::gray(0.5)),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PopBlendGroup);
        // Mark 2 (Darken, non-coalesceable with Multiply) occupies only [0..3] × [2..4]
        // (intersects di=0, NOT di=-1).
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Darken,
            isolated: true,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 8.0, 4.0),
            alpha: 1.0,
            mask: None,
        });
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 2.0, 3.0, 4.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::gray(0.5)),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PopBlendGroup);

        let pat = TilingPattern {
            commands: Arc::new(dl),
            bbox: Rect::new(0.0, 0.0, 8.0, 4.0),
            x_step: 4.0,
            y_step: 4.0,
            matrix: Matrix::identity(),
            tiling_type: 1,
        };
        let segs = pat.blend_segments().expect("segments");
        assert_eq!(segs.len(), 2);
        assert!(segs[0].intersects_period(&pat, -1, 0));
        assert!(segs[0].intersects_period(&pat, 0, 0));
        assert!(!segs[1].intersects_period(&pat, -1, 0));
        assert!(segs[1].intersects_period(&pat, 0, 0));
    }

    #[test]
    fn is_affine_in_backdrop_accepts_only_affine_blend_modes() {
        let make_mode = |mode: BlendMode| -> TilingPattern {
            let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 4.0, 4.0));
            dl.push(RenderCommand::PushBlendGroup {
                blend_mode: mode,
                isolated: true,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 4.0, 4.0),
                alpha: 1.0,
                mask: None,
            });
            dl.push(RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 4.0, 4.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::gray(0.5)),
                alpha: 1.0,
                overprint: None,
            });
            dl.push(RenderCommand::PopBlendGroup);
            tiling_with(dl.commands)
        };

        for affine in [BlendMode::Normal, BlendMode::Multiply, BlendMode::Screen] {
            let steps = make_mode(affine).blend_steps().expect("steps");
            assert!(
                TilingBlendStep::is_affine_in_backdrop(&steps),
                "{affine:?} must be classified as affine in backdrop"
            );
        }
        for non_affine in [
            BlendMode::Darken,
            BlendMode::Lighten,
            BlendMode::Overlay,
            BlendMode::Difference,
            BlendMode::Exclusion,
            BlendMode::ColorDodge,
            BlendMode::ColorBurn,
            BlendMode::HardLight,
            BlendMode::SoftLight,
            BlendMode::Hue,
            BlendMode::Saturation,
            BlendMode::Color,
            BlendMode::Luminosity,
        ] {
            let steps = make_mode(non_affine).blend_steps().expect("steps");
            assert!(
                !TilingBlendStep::is_affine_in_backdrop(&steps),
                "{non_affine:?} must NOT be classified as affine in backdrop"
            );
        }
    }

    #[test]
    fn stroke_ink_bounds_honor_large_finite_miter_limit() {
        // A width-2 stroke with miter_limit 200 can project a miter tip ~150
        // units beyond its vertex; clamping at 100 would drop neighboring copies
        // at 25-unit steps (e.g. di = -6).
        let mut sharp = Path::new();
        sharp.move_to(zpdf_core::Point::new(10.0, 0.0));
        sharp.line_to(zpdf_core::Point::new(0.0, 0.0));
        sharp.line_to(zpdf_core::Point::new(10.0, 0.13));

        let mut dl = DisplayList::new(Rect::new(-200.0, -20.0, 25.0, 25.0));
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Multiply,
            isolated: true,
            knockout: false,
            bounds: Rect::new(-200.0, -20.0, 25.0, 25.0),
            alpha: 1.0,
            mask: None,
        });
        dl.push(RenderCommand::StrokePath {
            path: sharp,
            style: StrokeStyle {
                width: 2.0,
                join: LineJoin::Miter,
                miter_limit: 200.0,
                ..StrokeStyle::default()
            },
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PopBlendGroup);

        let pat = TilingPattern {
            commands: Arc::new(dl),
            bbox: Rect::new(-200.0, -20.0, 25.0, 25.0),
            x_step: 25.0,
            y_step: 25.0,
            matrix: Matrix::identity(),
            tiling_type: 1,
        };
        let segs = pat.blend_segments().expect("segments");
        assert_eq!(segs.len(), 1);
        let bounds = segs[0].bounds.expect("stroke ink bounds");
        assert!(
            bounds.x0 <= -150.0,
            "miter_limit=200 stroke bound must reach <= -150, got {bounds:?}"
        );
        // Copy shifted +150 (+6 steps) still intersects period [-200..-175] or [0..25]
        // via its miter tip at x = -150.
        assert!(
            segs[0].intersects_period(&pat, -1, 0),
            "copy reaching miter tip must intersect period"
        );
    }

    #[test]
    fn soft_mask_transform_preserves_shared_nested_mask_allocation() {
        let mut inner_dl = DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0));
        inner_dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 10.0, 10.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        let inner_arc = Arc::new(inner_dl);

        let make_nested = || SoftMask {
            kind: SoftMaskKind::Luminosity,
            commands: Arc::clone(&inner_arc),
            offset: (0.0, 0.0),
            backdrop_luma: 0.0,
            transfer: None,
        };

        let mut outer_dl = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        for x in [0.0, 10.0] {
            outer_dl.push(RenderCommand::PushBlendGroup {
                blend_mode: BlendMode::Normal,
                isolated: true,
                knockout: false,
                bounds: Rect::new(x, 0.0, x + 10.0, 10.0),
                alpha: 1.0,
                mask: Some(make_nested()),
            });
            outer_dl.push(RenderCommand::FillPath {
                path: rect_path(x, 0.0, x + 10.0, 10.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::white()),
                alpha: 1.0,
                overprint: None,
            });
            outer_dl.push(RenderCommand::PopBlendGroup);
        }

        let outer_mask = SoftMask {
            kind: SoftMaskKind::Luminosity,
            commands: Arc::new(outer_dl),
            offset: (0.0, 0.0),
            backdrop_luma: 0.0,
            transfer: None,
        };
        let mapped = outer_mask.transform_with_images(
            &Matrix::translate(15.0, 25.0),
            &|_| None,
            &|path, style, m, _| {
                TransformedStroke::Stroked(path.map_points(|p| p.transform(m)), style.clone())
            },
        );
        let nested_masks: Vec<&SoftMask> = mapped
            .commands
            .commands
            .iter()
            .filter_map(|cmd| match cmd {
                RenderCommand::PushBlendGroup {
                    mask: Some(sub), ..
                } => Some(sub),
                _ => None,
            })
            .collect();
        assert_eq!(nested_masks.len(), 2);
        assert!(
            Arc::ptr_eq(&nested_masks[0].commands, &nested_masks[1].commands),
            "two references to the same nested mask must share one transformed Arc<DisplayList>"
        );
        assert!(
            !Arc::ptr_eq(&nested_masks[0].commands, &inner_arc),
            "transformed nested mask must be distinct from the untransformed source"
        );
        let Some(RenderCommand::FillPath { path, .. }) = nested_masks[0].commands.commands.first()
        else {
            panic!("expected FillPath inside transformed nested mask");
        };
        let first_pt = path.points().first().expect("translated point");
        assert!((first_pt.x - 15.0).abs() < 1e-6 && (first_pt.y - 25.0).abs() < 1e-6);
    }

    #[test]
    fn retained_mask_accounting_counts_oversized_descendant_beyond_depth_16() {
        let leaf_img = Arc::new(InlineImage {
            width: 32,
            height: 64,
            data: vec![255u8; 32 * 64 * 4],
            has_alpha: true,
            is_image_mask: false,
            premultiplied: true,
        });
        let mut current = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        current.push(RenderCommand::DrawInlineImage(InlineImageDraw {
            image: Arc::clone(&leaf_img),
            transform: Matrix::new(20.0, 0.0, 0.0, 20.0, 0.0, 0.0),
            alpha: 1.0,
        }));
        let mut current = Arc::new(current);
        for _ in 0..20 {
            let mut parent = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
            parent.push(RenderCommand::PushBlendGroup {
                blend_mode: BlendMode::Normal,
                isolated: true,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 20.0, 20.0),
                alpha: 1.0,
                mask: Some(SoftMask {
                    kind: SoftMaskKind::Luminosity,
                    commands: Arc::clone(&current),
                    offset: (0.0, 0.0),
                    backdrop_luma: 0.0,
                    transfer: None,
                }),
            });
            parent.push(RenderCommand::PopBlendGroup);
            current = Arc::new(parent);
        }

        let mut accounting = RetainedMaskAccounting::default();
        let delta = accounting.delta_for_display_list(&current);
        assert!(
            delta.added_bytes >= 8_192,
            "deeply nested 8 KB inline image at depth 20 must be counted, got {} bytes",
            delta.added_bytes
        );
        accounting.commit(delta);
        let repeat = accounting.delta_for_display_list(&current);
        assert_eq!(repeat.added_bytes, 0);
    }

    #[test]
    fn retained_mask_accounting_distinguishes_tiling_patterns_sharing_one_cell() {
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0));
        cell.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 10.0, 10.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        let cell_arc = Arc::new(cell);
        let cell_bytes = cell_arc.owned_heap_bytes();
        let tiling_struct_bytes = std::mem::size_of::<TilingPattern>() as u64;

        let t1 = Arc::new(TilingPattern {
            commands: Arc::clone(&cell_arc),
            bbox: Rect::new(0.0, 0.0, 10.0, 10.0),
            x_step: 10.0,
            y_step: 10.0,
            matrix: Matrix::identity(),
            tiling_type: 1,
        });
        let t2 = Arc::new(TilingPattern {
            commands: Arc::clone(&cell_arc),
            bbox: Rect::new(0.0, 0.0, 10.0, 10.0),
            x_step: 10.0,
            y_step: 10.0,
            matrix: Matrix::translate(5.0, 5.0),
            tiling_type: 1,
        });

        let mut same_dl = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        same_dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 10.0, 20.0),
            rule: FillRule::NonZero,
            paint: Paint::Tiling(Arc::clone(&t1)),
            alpha: 1.0,
            overprint: None,
        });
        same_dl.push(RenderCommand::FillPath {
            path: rect_path(10.0, 0.0, 20.0, 20.0),
            rule: FillRule::NonZero,
            paint: Paint::Tiling(Arc::clone(&t1)),
            alpha: 1.0,
            overprint: None,
        });
        let same_outer = same_dl.owned_heap_bytes();
        let same_arc = Arc::new(same_dl);

        let mut distinct_dl = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        distinct_dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 10.0, 20.0),
            rule: FillRule::NonZero,
            paint: Paint::Tiling(Arc::clone(&t1)),
            alpha: 1.0,
            overprint: None,
        });
        distinct_dl.push(RenderCommand::FillPath {
            path: rect_path(10.0, 0.0, 20.0, 20.0),
            rule: FillRule::NonZero,
            paint: Paint::Tiling(Arc::clone(&t2)),
            alpha: 1.0,
            overprint: None,
        });
        let distinct_outer = distinct_dl.owned_heap_bytes();
        let distinct_arc = Arc::new(distinct_dl);

        let acct_same = RetainedMaskAccounting::default();
        let d_same = acct_same.delta_for_display_list(&same_arc);
        assert_eq!(
            d_same.added_bytes,
            same_outer + tiling_struct_bytes + cell_bytes,
            "two references to one Arc<TilingPattern> must charge one pattern + one cell"
        );

        let acct_distinct = RetainedMaskAccounting::default();
        let d_distinct = acct_distinct.delta_for_display_list(&distinct_arc);
        assert_eq!(
            d_distinct.added_bytes,
            distinct_outer + 2 * tiling_struct_bytes + cell_bytes,
            "two distinct Arc<TilingPattern> objects sharing one cell must charge both patterns + one cell"
        );
    }
}
