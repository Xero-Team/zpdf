//! Page recorder: a single geometry arena plus an ordered op list.
//!
//! Clips interleave with draws, so we cannot batch all solid geometry and draw it
//! at the end — order matters. `execute` appends ops; `end_page` replays them into
//! one render pass. Clip state is a stencil counter: stencil[pixel] = number of
//! active clip paths covering it; content draws test `stencil == clip_depth`
//! (inside the intersection of all active clips).

use crate::glyph_atlas::GlyphAtlas;
use crate::path::{fill_mesh, stroke_mesh, Mesh};
use crate::transform::{PageMap, SolidVertex, TexturedVertex};
use zpdf_core::{time::Instant, ParseLimits};
use zpdf_display_list::{
    BlendMode, FillRule, Overprint, Path as DlPath, SoftMaskKind, StrokeStyle,
};

/// A soft mask's recorded geometry (ops referencing the shared page arena) plus
/// the reduction parameters needed to turn it into per-pixel coverage.
pub struct MaskOps {
    /// Ops replayed into an offscreen mask layer (reference the shared buffers).
    /// May itself contain `PushBlend`/`PopBlend` (a transparency group nested
    /// inside the mask group), composited through the layered path.
    pub ops: Vec<PageOp>,
    pub kind: SoftMaskKind,
    /// /BC backdrop luminosity for areas the mask group leaves unpainted.
    pub backdrop_luma: f32,
    /// Device-pixel translation of the coverage plane (a tiling pattern builds
    /// one mask and reuses it at every cell; the cell CTMs differ only by this
    /// page-space shift, here pre-scaled to device pixels — `dy` already carries
    /// the device Y flip). The apply pass samples the mask at `coord − (dx, dy)`.
    pub dx: i32,
    pub dy: i32,
    /// /TR transfer function pre-sampled over [0,1] into 256 steps, applied to
    /// the reduced coverage before it modulates the group (identity when absent).
    pub transfer: Option<std::sync::Arc<[u8; 256]>>,
}

/// A range of indices/vertices in the shared arena. Indices are always
/// absolute (rebased at append time), so `base_vertex` is always `0` — kept as
/// a field for `draw_indexed`'s call shape rather than removed, to minimize
/// churn at call sites.
#[derive(Clone, Copy)]
pub struct MeshRange {
    pub first_index: u32,
    pub index_count: u32,
    pub base_vertex: i32,
}

/// A clip path to (re-)stamp into a layer's stencil, with its nesting ref value.
#[derive(Clone, Copy)]
pub struct ClipStamp {
    pub range: MeshRange,
    pub ref_value: u32,
}

/// One recorded operation, replayed in order into the page pass(es).
pub enum PageOp {
    /// Draw solid geometry, testing `stencil == clip_ref`.
    Draw { range: MeshRange, clip_ref: u32 },
    /// Stamp a clip path: where `stencil == ref_value`, increment (intersection).
    StampClip { range: MeshRange, ref_value: u32 },
    /// Reset the whole stencil to 0 (fullscreen quad) before re-stamping on pop.
    ResetStencil,
    /// Draw an image quad (from the textured arena), testing `stencil == clip_ref`.
    Image {
        range: MeshRange,
        image_id: u32,
        clip_ref: u32,
    },
    /// Draw a rasterized-glyph quad (from the glyph arena, sampling the
    /// page's single atlas texture), testing `stencil == clip_ref`. Unlike
    /// `Image`, no texture id is carried — a page has exactly one atlas.
    Glyph { range: MeshRange, clip_ref: u32 },
    /// Begin a transparency group: subsequent ops render to a fresh offscreen layer.
    /// `clips` are the clip paths active at push time, re-stamped into the new layer.
    /// `alpha` is the group constant alpha applied at composite time; `mask` is an
    /// optional ExtGState /SMask, pre-multiplied into the group before compositing.
    PushBlend {
        mode: BlendMode,
        alpha: f32,
        clips: Vec<ClipStamp>,
        mask: Option<MaskOps>,
        /// When set, the group is composited with the overprint formula (PDF
        /// 8.6.7) instead of `mode`: a single element rendered into the layer,
        /// then merged per-colorant against the backdrop. `mode`/`alpha` are
        /// unused (Normal / 1.0).
        overprint: Option<Overprint>,
    },
    /// End the current group: composite it onto the parent with `mode` (carried by
    /// the matching PushBlend), then re-stamp `clips` into the resulting layer.
    PopBlend,
}

#[derive(Clone, Copy)]
struct ClipEntry {
    /// `None` for an empty/degenerate clip path (still occupies a depth level).
    range: Option<MeshRange>,
    ref_value: u32,
    /// False once the Stencil8 representable depth (255) is saturated.
    advanced: bool,
}

/// One cooperative deadline covers page recording, including child programs
/// and soft masks. A single tessellation call is not preemptible.
const RECORD_BUDGET: std::time::Duration = std::time::Duration::from_secs(8);

fn geometry_bytes<V>(vertices: usize, indices: usize) -> Option<u64> {
    let vertices = u64::try_from(vertices)
        .ok()?
        .checked_mul(std::mem::size_of::<V>() as u64)?;
    let indices = u64::try_from(indices)
        .ok()?
        .checked_mul(std::mem::size_of::<u32>() as u64)?;
    vertices.checked_add(indices)
}

fn arena_fits_u32(current: usize, additional: usize) -> bool {
    current
        .checked_add(additional)
        .and_then(|total| u32::try_from(total).ok())
        .is_some()
}

/// Accumulates geometry + ops for one page.
#[derive(Default)]
pub struct PageRecorder {
    pub vertices: Vec<SolidVertex>,
    pub indices: Vec<u32>,
    /// Separate arena for image quads (different vertex format).
    pub tex_vertices: Vec<TexturedVertex>,
    pub tex_indices: Vec<u32>,
    /// Separate arena for glyph-atlas quads. Indices are absolute (rebased at
    /// append time, like `vertices`/`indices`), so consecutive same-clip glyph
    /// quads batch into one draw call (`batch.rs`) — the dominant case for
    /// text-heavy pages, unlike per-image quads (`tex_indices`), which each
    /// carry a distinct texture and so never batch with one another.
    pub glyph_vertices: Vec<TexturedVertex>,
    pub glyph_indices: Vec<u32>,
    pub glyph_atlas: GlyphAtlas,
    pub ops: Vec<PageOp>,
    clip_stack: Vec<ClipEntry>,
    clip_depth: u32,
    /// `None` keeps the standalone recorder utility unlimited; renderers set a
    /// concrete document-derived limit at page creation.
    max_blend_depth: Option<usize>,
    /// Logical outer depth while recording a soft-mask sub-list.
    blend_depth_offset: usize,
    blend_depth: usize,
    /// Groups omitted after reaching the depth cap. Matching pops consume this
    /// counter instead of closing a real outer group.
    skipped_blend_depth: usize,
    /// Shared by the page and every soft-mask sub-list; never saved/restored
    /// with the local clip/blend state in `record_subops`.
    limits: ParseLimits,
    /// Actual child operators, with a minimum charge of one per dispatch so
    /// empty/operand-only programs cannot replay for free (including on wasm).
    type3_work_used: u64,
    geometry_bytes: u64,
    deadline: Option<Instant>,
    /// A failed child or append invalidates the whole page. Latching the error
    /// lets sub-list recording unwind its state before the renderer drops it.
    recording_error: Option<&'static str>,
}

impl PageRecorder {
    pub fn with_limits(mut self, limits: &ParseLimits) -> Self {
        self.limits = limits.clone();
        self.deadline = Some(Instant::now() + RECORD_BUDGET);
        self.with_max_blend_depth(limits.max_blend_group_depth as usize)
    }

    /// Check at command/glyph boundaries and before appends. All nested work
    /// observes the same deadline and the first failure remains authoritative.
    pub fn can_record(&mut self) -> bool {
        if self.recording_error.is_none()
            && self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.recording_error = Some("GPU page recording exceeded time budget");
        }
        self.recording_error.is_none()
    }

    pub fn ensure_within_budget(&mut self) -> Result<(), crate::WgpuRenderError> {
        self.can_record();
        match self.recording_error {
            Some(reason) => Err(crate::WgpuRenderError::Unsupported(reason.into())),
            None => Ok(()),
        }
    }

    /// Interpret a Type3 occurrence with the document's content policy and
    /// debit max(1, operators executed) from one render-page allowance. The
    /// minimum charge bounds children with no operators as well.
    /// Work timing is optional; the total operator counter is always available.
    pub fn interpret_type3_stream(
        &mut self,
        stream: &[u8],
    ) -> Option<zpdf_display_list::DisplayList> {
        if !self.can_record() {
            return None;
        }
        let mut limits = self.limits.clone();
        limits.max_page_operators = limits
            .max_page_operators
            .saturating_sub(self.type3_work_used);
        if limits.max_page_operators == 0 {
            self.recording_error = Some("GPU Type3 interpretation exceeded content limits");
            return None;
        }
        let deadline = *self
            .deadline
            .get_or_insert_with(|| Instant::now() + RECORD_BUDGET);
        let rect = zpdf_core::Rect::new(0.0, -1000.0, 1000.0, 1000.0);
        let (dl, stats) = zpdf_content::interpreter::ContentInterpreter::new(rect)
            .with_limits(&limits)
            .with_deadline(deadline)
            .interpret_with_stats(stream);
        self.type3_work_used = self
            .type3_work_used
            .saturating_add(stats.operators_executed.max(1));
        if stats.diagnostics.time_limit_reached {
            self.recording_error = Some("GPU Type3 interpretation exceeded page time budget");
        } else if stats.diagnostics.resource_limit_reached {
            self.recording_error = Some("GPU Type3 interpretation exceeded content limits");
        }
        // Reject a partial child before any of its geometry is recorded. The
        // post-check also catches short children below the interpreter's clock
        // sampling interval.
        if !self.can_record() {
            return None;
        }
        Some(dl)
    }

    fn reserve_geometry<V>(
        &mut self,
        current_vertices: usize,
        current_indices: usize,
        vertices: usize,
        indices: usize,
    ) -> bool {
        if !self.can_record() {
            return false;
        }
        if !arena_fits_u32(current_vertices, vertices) || !arena_fits_u32(current_indices, indices)
        {
            self.recording_error = Some("GPU page geometry exceeds u32 indexing");
            return false;
        }
        let total = geometry_bytes::<V>(vertices, indices)
            .and_then(|bytes| self.geometry_bytes.checked_add(bytes))
            .filter(|&bytes| bytes <= self.limits.max_gpu_geometry_bytes);
        let Some(total) = total else {
            self.recording_error = Some("GPU page geometry exceeds max_gpu_geometry_bytes");
            return false;
        };
        self.geometry_bytes = total;
        true
    }

    pub fn with_max_blend_depth(mut self, max_depth: usize) -> Self {
        self.max_blend_depth = Some(max_depth);
        self
    }

    pub fn can_push_blend(&self) -> bool {
        self.skipped_blend_depth == 0
            && self.blend_depth_offset.saturating_add(self.blend_depth)
                < self.max_blend_depth.unwrap_or(usize::MAX)
    }

    fn append(&mut self, mesh: Mesh) -> Option<MeshRange> {
        if !self.reserve_geometry::<SolidVertex>(
            self.vertices.len(),
            self.indices.len(),
            mesh.vertices.len(),
            mesh.indices.len(),
        ) {
            return None;
        }
        // Indices are rebased to absolute (base_vertex baked in) so every draw
        // can use base_vertex 0 uniformly. This makes a contiguous run of
        // same-key ops trivially concatenable into one draw_indexed call
        // (`batch.rs`): consecutive appends are already contiguous in
        // `indices`, so merging is just extending index_count — no per-draw
        // base_vertex bookkeeping needed at replay time.
        let base_vertex = self.vertices.len() as u32;
        let first_index = self.indices.len() as u32;
        let index_count = mesh.indices.len() as u32;
        self.vertices.extend_from_slice(&mesh.vertices);
        self.indices
            .extend(mesh.indices.iter().map(|&i| i + base_vertex));
        Some(MeshRange {
            first_index,
            index_count,
            base_vertex: 0,
        })
    }

    /// Record a pre-tessellated mesh as a content draw at the current clip depth.
    /// Used by fills/strokes and by glyph rendering (which builds its own meshes).
    pub fn add_mesh(&mut self, mesh: Mesh) {
        if mesh.indices.is_empty() {
            return;
        }
        let Some(range) = self.append(mesh) else {
            return;
        };
        self.ops.push(PageOp::Draw {
            range,
            clip_ref: self.clip_depth,
        });
    }

    /// Record an image quad (4 textured vertices) at the current clip depth.
    pub fn add_image(&mut self, quad: [TexturedVertex; 4], image_id: u32) {
        if !self.reserve_geometry::<TexturedVertex>(
            self.tex_vertices.len(),
            self.tex_indices.len(),
            4,
            6,
        ) {
            return;
        }
        let base_vertex = self.tex_vertices.len() as u32;
        let first_index = self.tex_indices.len() as u32;
        self.tex_vertices.extend_from_slice(&quad);
        self.tex_indices.extend_from_slice(&[
            base_vertex,
            base_vertex + 1,
            base_vertex + 2,
            base_vertex,
            base_vertex + 2,
            base_vertex + 3,
        ]);
        self.ops.push(PageOp::Image {
            range: MeshRange {
                first_index,
                index_count: 6,
                base_vertex: 0,
            },
            image_id,
            clip_ref: self.clip_depth,
        });
    }

    /// Record a glyph-atlas quad (4 textured vertices) at the current clip
    /// depth. Indices are rebased to absolute (mirroring `append()`, unlike
    /// `add_image`'s per-quad `base_vertex`) so consecutive glyph quads are
    /// trivially batchable.
    pub fn add_glyph_quad(&mut self, quad: [TexturedVertex; 4]) {
        if !self.reserve_geometry::<TexturedVertex>(
            self.glyph_vertices.len(),
            self.glyph_indices.len(),
            4,
            6,
        ) {
            return;
        }
        let base_vertex = self.glyph_vertices.len() as u32;
        let first_index = self.glyph_indices.len() as u32;
        self.glyph_vertices.extend_from_slice(&quad);
        self.glyph_indices.extend_from_slice(&[
            base_vertex,
            base_vertex + 1,
            base_vertex + 2,
            base_vertex,
            base_vertex + 2,
            base_vertex + 3,
        ]);
        self.ops.push(PageOp::Glyph {
            range: MeshRange {
                first_index,
                index_count: 6,
                base_vertex: 0,
            },
            clip_ref: self.clip_depth,
        });
    }

    pub fn add_fill(&mut self, path: &DlPath, rule: FillRule, color: [f32; 4], map: &PageMap) {
        if !self.can_record() {
            return;
        }
        if let Some(mesh) = fill_mesh(path, rule, color, map) {
            self.add_mesh(mesh);
        }
    }

    pub fn add_stroke(
        &mut self,
        path: &DlPath,
        style: &StrokeStyle,
        color: [f32; 4],
        map: &PageMap,
    ) {
        if !self.can_record() {
            return;
        }
        if let Some(mesh) = stroke_mesh(path, style, color, map) {
            self.add_mesh(mesh);
        }
    }

    /// Push a clip path (tessellated as a fill with the given rule). Always
    /// occupies a depth level so the matching pop stays balanced, even when the
    /// path is empty (an empty clip ⇒ nothing passes at the deeper level, which
    /// is the correct "clip to nothing" behavior).
    pub fn push_clip(&mut self, path: &DlPath, rule: FillRule, map: &PageMap) {
        if !self.can_record() {
            return;
        }
        let range = fill_mesh(path, rule, [0.0; 4], map).and_then(|m| self.append(m));
        self.push_clip_range(range);
    }

    /// Push a clip to a *stroked* path's outline (tessellated as a stroke mesh).
    pub fn push_clip_stroke(&mut self, path: &DlPath, style: &StrokeStyle, map: &PageMap) {
        if !self.can_record() {
            return;
        }
        let range = stroke_mesh(path, style, [0.0; 4], map).and_then(|m| self.append(m));
        self.push_clip_range(range);
    }

    /// Common tail of the clip-push variants: stamp the (optional) mesh as a clip
    /// path and advance the clip depth (an empty mesh still occupies a level).
    fn push_clip_range(&mut self, range: Option<MeshRange>) {
        if !self.can_record() {
            return;
        }
        // Stencil8 can encode references 0..=255. Saturate deeper malformed
        // nesting instead of wrapping the reference to zero and leaking draws.
        if self.clip_depth == u8::MAX as u32 {
            self.clip_stack.push(ClipEntry {
                range: None,
                ref_value: self.clip_depth,
                advanced: false,
            });
            tracing::warn!("GPU clip nesting exceeds Stencil8 capacity; ignoring deeper clip");
            return;
        }
        let ref_value = self.clip_depth;
        if let Some(r) = range {
            self.ops.push(PageOp::StampClip {
                range: r,
                ref_value,
            });
        }
        self.clip_stack.push(ClipEntry {
            range,
            ref_value,
            advanced: true,
        });
        self.clip_depth += 1;
    }

    /// Pop a clip: rebuild the stencil from scratch (reset to 0, re-stamp the
    /// remaining entries). Decrementing the popped region is unsafe at shared
    /// MSAA boundaries; PDFs nest shallowly so rebuild is cheap. Unbalanced pop
    /// is a no-op (mirrors the CPU).
    pub fn pop_clip(&mut self) {
        let Some(popped) = self.clip_stack.pop() else {
            return;
        };
        if !popped.advanced {
            return;
        }
        self.clip_depth -= 1;
        self.ops.push(PageOp::ResetStencil);
        // Collect first to avoid borrowing self while pushing ops.
        let remaining: Vec<(MeshRange, u32)> = self
            .clip_stack
            .iter()
            .filter_map(|e| e.range.map(|r| (r, e.ref_value)))
            .collect();
        for (range, ref_value) in remaining {
            self.ops.push(PageOp::StampClip { range, ref_value });
        }
    }

    /// The clip paths currently active (for re-stamping into a fresh layer).
    fn active_clips(&self) -> Vec<ClipStamp> {
        self.clip_stack
            .iter()
            .filter_map(|e| {
                e.range.map(|range| ClipStamp {
                    range,
                    ref_value: e.ref_value,
                })
            })
            .collect()
    }

    /// Begin a transparency group. The active clips are captured so the group's
    /// fresh layer (and, on pop, the composited result) reproduce the same clip.
    pub fn push_blend(&mut self, mode: BlendMode, alpha: f32, mask: Option<MaskOps>) {
        if !self.can_push_blend() {
            if self.skipped_blend_depth == 0 {
                tracing::warn!(
                    limit = self.max_blend_depth.unwrap_or(usize::MAX),
                    "GPU blend-group nesting exceeds configured limit; using passthrough"
                );
            }
            self.skipped_blend_depth = self.skipped_blend_depth.saturating_add(1);
            return;
        }
        let clips = self.active_clips();
        self.ops.push(PageOp::PushBlend {
            mode,
            alpha,
            clips,
            mask,
            overprint: None,
        });
        self.blend_depth += 1;
    }

    /// Begin an overprint group (PDF 8.6.7): the next recorded draw renders into
    /// a fresh layer, composited onto the parent with the overprint formula.
    /// Forces the layered render path (like any blend group).
    pub fn push_overprint(&mut self, op: Overprint) {
        if !self.can_push_blend() {
            if self.skipped_blend_depth == 0 {
                tracing::warn!(
                    limit = self.max_blend_depth.unwrap_or(usize::MAX),
                    "GPU blend-group nesting exceeds configured limit; using source-over"
                );
            }
            self.skipped_blend_depth = self.skipped_blend_depth.saturating_add(1);
            return;
        }
        let clips = self.active_clips();
        self.ops.push(PageOp::PushBlend {
            mode: BlendMode::Normal,
            alpha: 1.0,
            clips,
            mask: None,
            overprint: Some(op),
        });
        self.blend_depth += 1;
    }

    /// Record a sub-sequence (a soft mask's commands) into the SHARED geometry
    /// arena, but collect its ops separately instead of appending them to the
    /// page op list. Clip state is isolated so the mask's clip nesting cannot
    /// leak into the page. Returns the mask's ops (which reference the shared
    /// vertex/index buffers, replayed later into a mask layer).
    pub fn record_subops(&mut self, f: impl FnOnce(&mut Self)) -> Vec<PageOp> {
        let nested_offset = self
            .blend_depth_offset
            .saturating_add(self.blend_depth)
            .saturating_add(1);
        let saved_ops = std::mem::take(&mut self.ops);
        let saved_stack = std::mem::take(&mut self.clip_stack);
        let saved_depth = std::mem::replace(&mut self.clip_depth, 0);
        let saved_blend_depth = std::mem::replace(&mut self.blend_depth, 0);
        let saved_blend_offset = std::mem::replace(&mut self.blend_depth_offset, nested_offset);
        let saved_skipped_blend_depth = std::mem::replace(&mut self.skipped_blend_depth, 0);
        f(self);
        self.close_open_blends();
        let sub = std::mem::replace(&mut self.ops, saved_ops);
        self.clip_stack = saved_stack;
        self.clip_depth = saved_depth;
        self.blend_depth = saved_blend_depth;
        self.blend_depth_offset = saved_blend_offset;
        self.skipped_blend_depth = saved_skipped_blend_depth;
        sub
    }

    pub fn pop_blend(&mut self) {
        if self.skipped_blend_depth > 0 {
            self.skipped_blend_depth -= 1;
            return;
        }
        if self.blend_depth == 0 {
            return;
        }
        self.ops.push(PageOp::PopBlend);
        self.blend_depth -= 1;
    }

    pub fn close_open_blends(&mut self) {
        while self.blend_depth > 0 || self.skipped_blend_depth > 0 {
            self.pop_blend();
        }
    }

    pub fn has_blend_groups(&self) -> bool {
        self.ops
            .iter()
            .any(|o| matches!(o, PageOp::PushBlend { .. }))
    }

    /// True if any `ResetStencil` op was recorded (i.e. clips were popped) and a
    /// fullscreen reset quad must be appended before replay.
    pub fn uses_reset(&self) -> bool {
        self.ops.iter().any(|o| matches!(o, PageOp::ResetStencil))
    }

    /// Append a fullscreen quad in device pixels (for `ResetStencil`).
    pub fn append_fullscreen(&mut self, w_px: f32, h_px: f32) -> Option<MeshRange> {
        let v = |x: f32, y: f32| SolidVertex {
            pos: [x, y],
            color: [0.0; 4],
        };
        let mut mesh = Mesh::new();
        mesh.vertices
            .extend_from_slice(&[v(0.0, 0.0), v(w_px, 0.0), v(w_px, h_px), v(0.0, h_px)]);
        mesh.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
        self.append(mesh)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zpdf_core::Point;

    fn map() -> PageMap {
        PageMap {
            scale: 1.0,
            x0: 0.0,
            y1: 100.0,
        }
    }

    fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> DlPath {
        let mut p = DlPath::new();
        p.move_to(Point::new(x0, y0));
        p.line_to(Point::new(x1, y0));
        p.line_to(Point::new(x1, y1));
        p.line_to(Point::new(x0, y1));
        p.close();
        p
    }

    #[test]
    fn type3_work_is_cumulative_across_occurrences_and_masks() {
        let limits = ParseLimits {
            max_page_operators: 5,
            ..ParseLimits::default()
        };
        let mut r = PageRecorder::default().with_limits(&limits);
        let deadline = r.deadline;
        let stream = b"0 0 1 1 re f"; // two operators, one visible command
        let first = r.interpret_type3_stream(stream).unwrap();
        assert_eq!(first.commands.len(), 1);
        let sub = r.record_subops(|nested| {
            let second = nested.interpret_type3_stream(stream).unwrap();
            assert_eq!(second.commands.len(), 1);
        });
        assert!(sub.is_empty());
        assert_eq!(r.type3_work_used, 4);
        assert_eq!(r.deadline, deadline);
        assert!(r.interpret_type3_stream(stream).is_none());
        assert_eq!(r.type3_work_used, 5);
        assert!(r.ensure_within_budget().is_err());
        assert!(r.interpret_type3_stream(b"q Q").is_none());
        assert_eq!(r.type3_work_used, 5, "failure stops later work");
    }

    #[test]
    fn empty_and_operand_only_type3_dispatches_are_not_free() {
        let limits = ParseLimits {
            max_page_operators: 2,
            ..ParseLimits::default()
        };
        let mut r = PageRecorder::default().with_limits(&limits);
        assert!(r.interpret_type3_stream(b"").is_some());
        assert!(r.interpret_type3_stream(b"1").is_some());
        assert_eq!(r.type3_work_used, 2);
        assert!(r.interpret_type3_stream(b"").is_none());
        assert_eq!(r.type3_work_used, 2);
        assert!(r.ensure_within_budget().is_err());
    }

    #[test]
    fn type3_children_enforce_custom_content_limits() {
        let cases: [(ParseLimits, &[u8]); 4] = [
            (
                ParseLimits {
                    max_page_operators: 1,
                    ..ParseLimits::default()
                },
                b"q Q",
            ),
            (
                ParseLimits {
                    max_operand_stack_depth: 1,
                    ..ParseLimits::default()
                },
                b"1 2",
            ),
            (
                ParseLimits {
                    max_graphics_state_depth: 1,
                    ..ParseLimits::default()
                },
                b"q q",
            ),
            (
                ParseLimits {
                    max_marked_content_depth: 1,
                    ..ParseLimits::default()
                },
                b"/Span BMC /Span BMC",
            ),
        ];
        for (limits, stream) in cases {
            let mut r = PageRecorder::default().with_limits(&limits);
            assert!(r.interpret_type3_stream(stream).is_none());
            assert!(r.ensure_within_budget().is_err());
            assert!(r.vertices.is_empty());
            assert!(r.ops.is_empty());
        }
    }

    #[test]
    fn type3_child_does_not_reset_an_expired_page_deadline() {
        let mut r = PageRecorder::default().with_limits(&ParseLimits::default());
        r.deadline = Some(Instant::now()); // deterministic expiry, no sleep
        let deadline = r.deadline;
        let sub = r.record_subops(|nested| {
            assert!(nested.interpret_type3_stream(b"0 0 1 1 re f").is_none());
        });
        assert!(sub.is_empty());
        assert_eq!(r.deadline, deadline);
        assert_eq!(r.type3_work_used, 0);
        assert!(r.ensure_within_budget().is_err());
    }

    fn triangle() -> Mesh {
        let mut mesh = Mesh::new();
        mesh.vertices.extend_from_slice(&[
            SolidVertex {
                pos: [0.0, 0.0],
                color: [1.0; 4],
            },
            SolidVertex {
                pos: [1.0, 0.0],
                color: [1.0; 4],
            },
            SolidVertex {
                pos: [0.0, 1.0],
                color: [1.0; 4],
            },
        ]);
        mesh.indices.extend_from_slice(&[0, 1, 2]);
        mesh
    }

    #[test]
    fn geometry_admission_is_shared_across_arenas_and_masks() {
        let limit = geometry_bytes::<SolidVertex>(3, 3).unwrap()
            + 2 * geometry_bytes::<TexturedVertex>(4, 6).unwrap();
        let limits = ParseLimits {
            max_gpu_geometry_bytes: limit,
            ..ParseLimits::default()
        };
        let mut r = PageRecorder::default().with_limits(&limits);
        let quad = [TexturedVertex {
            pos: [0.0; 2],
            uv: [0.0; 2],
            color: [1.0; 4],
        }; 4];
        r.add_mesh(triangle());
        r.add_image(quad, 1);
        let sub = r.record_subops(|nested| {
            nested.add_glyph_quad(quad);
            assert!(nested.ensure_within_budget().is_ok());
            // One more triangle would exceed the shared limit. Neither arena
            // data nor its draw op may be appended on failure.
            nested.add_mesh(triangle());
        });
        assert_eq!(r.geometry_bytes, limit);
        assert_eq!((r.vertices.len(), r.indices.len()), (3, 3));
        assert_eq!((r.tex_vertices.len(), r.tex_indices.len()), (4, 6));
        assert_eq!((r.glyph_vertices.len(), r.glyph_indices.len()), (4, 6));
        assert_eq!(r.ops.len(), 2, "outer ops restored after mask failure");
        assert_eq!(sub.len(), 1);
        assert_eq!(r.blend_depth_offset, 0);
        assert!(
            r.ensure_within_budget().is_err(),
            "mask error survives restore"
        );
    }

    #[test]
    fn clips_and_fullscreen_geometry_obey_admission() {
        let limits = ParseLimits {
            max_gpu_geometry_bytes: 0,
            ..ParseLimits::default()
        };
        let mut r = PageRecorder::default().with_limits(&limits);
        r.push_clip(&rect(0.0, 0.0, 1.0, 1.0), FillRule::NonZero, &map());
        assert!(r.ensure_within_budget().is_err());
        assert!(r.vertices.is_empty());
        assert!(r.indices.is_empty());
        assert!(r.clip_stack.is_empty());
        assert!(r.ops.is_empty());

        let mut r = PageRecorder::default().with_limits(&limits);
        assert!(r.append_fullscreen(1.0, 1.0).is_none());
        assert!(r.ensure_within_budget().is_err());
        assert!(r.vertices.is_empty());
        assert!(r.indices.is_empty());
    }

    #[test]
    fn geometry_count_checks_do_not_wrap() {
        assert!(arena_fits_u32(u32::MAX as usize - 1, 1));
        assert!(!arena_fits_u32(u32::MAX as usize, 1));
        assert!(!arena_fits_u32(usize::MAX, 1));
        #[cfg(target_pointer_width = "64")]
        assert!(geometry_bytes::<SolidVertex>(usize::MAX, 0).is_none());
    }

    #[test]
    fn nested_clips_increment_then_rebuild_on_pop() {
        let mut r = PageRecorder::default();
        let m = map();
        // push two clips, draw inside, pop both.
        r.push_clip(&rect(10.0, 10.0, 90.0, 90.0), FillRule::NonZero, &m);
        r.push_clip(&rect(20.0, 20.0, 80.0, 80.0), FillRule::NonZero, &m);
        r.add_fill(
            &rect(0.0, 0.0, 100.0, 100.0),
            FillRule::NonZero,
            [1.0, 0.0, 0.0, 1.0],
            &m,
        );
        r.pop_clip();
        r.pop_clip();

        // ops: StampClip(ref0), StampClip(ref1), Draw(clip_ref2),
        //      ResetStencil + StampClip(ref0) [restamp remaining after first pop],
        //      ResetStencil [second pop, nothing remaining]
        let draws: Vec<u32> = r
            .ops
            .iter()
            .filter_map(|o| match o {
                PageOp::Draw { clip_ref, .. } => Some(*clip_ref),
                _ => None,
            })
            .collect();
        assert_eq!(draws, vec![2], "content draw tests stencil == depth 2");
        let resets = r
            .ops
            .iter()
            .filter(|o| matches!(o, PageOp::ResetStencil))
            .count();
        assert_eq!(resets, 2, "each pop emits a reset");
        assert_eq!(r.clip_depth, 0, "depth balanced back to 0");
    }

    #[test]
    fn unbalanced_pop_is_noop() {
        let mut r = PageRecorder::default();
        r.pop_clip();
        assert!(r.ops.is_empty());
        assert_eq!(r.clip_depth, 0);
    }

    #[test]
    fn blend_stack_ignores_extra_pops_and_closes_open_groups() {
        let mut r = PageRecorder::default();
        r.pop_blend();
        assert!(r.ops.is_empty());
        r.push_blend(BlendMode::Normal, 1.0, None);
        r.close_open_blends();
        assert_eq!(r.blend_depth, 0);
        assert!(matches!(
            r.ops.as_slice(),
            [PageOp::PushBlend { .. }, PageOp::PopBlend]
        ));
    }

    #[test]
    fn blend_depth_limit_skips_nested_groups_without_popping_outer_group() {
        let mut r = PageRecorder::default().with_max_blend_depth(1);
        r.push_blend(BlendMode::Normal, 1.0, None);
        r.push_blend(BlendMode::Multiply, 1.0, None);
        assert_eq!(r.blend_depth, 1);
        assert_eq!(r.skipped_blend_depth, 1);
        assert_eq!(
            r.ops
                .iter()
                .filter(|op| matches!(op, PageOp::PushBlend { .. }))
                .count(),
            1
        );
        r.pop_blend();
        assert_eq!(r.blend_depth, 1);
        assert_eq!(r.skipped_blend_depth, 0);
        r.pop_blend();
        assert_eq!(r.blend_depth, 0);
        assert!(matches!(r.ops.last(), Some(PageOp::PopBlend)));
    }

    #[test]
    fn soft_mask_subops_count_toward_the_blend_depth_limit() {
        let mut r = PageRecorder::default().with_max_blend_depth(1);
        assert!(r.can_push_blend());
        let sub = r.record_subops(|nested| {
            assert!(!nested.can_push_blend());
            nested.push_blend(BlendMode::Normal, 1.0, None);
        });
        assert!(sub.is_empty());
        assert!(r.can_push_blend());
    }

    #[test]
    fn empty_clip_path_still_occupies_depth() {
        let mut r = PageRecorder::default();
        r.push_clip(&DlPath::new(), FillRule::NonZero, &map());
        assert_eq!(r.clip_depth, 1, "empty clip still advances depth");
        // No StampClip op emitted for the empty geometry.
        assert!(!r.ops.iter().any(|o| matches!(o, PageOp::StampClip { .. })));
    }

    /// End-to-end batching check through the real `add_fill`/`append` path
    /// (not hand-built `PageOp`s): four unclipped fills — the shape of the
    /// `rect_fills` GPU-acceptance corpus case — collapse into one draw call,
    /// and the merged range covers every vertex the four fills produced.
    #[test]
    fn unclipped_fills_batch_into_one_draw_call() {
        let mut r = PageRecorder::default();
        let m = map();
        let color = [1.0, 0.0, 0.0, 1.0];
        r.add_fill(&rect(0.0, 0.0, 10.0, 10.0), FillRule::NonZero, color, &m);
        r.add_fill(&rect(20.0, 0.0, 30.0, 10.0), FillRule::NonZero, color, &m);
        r.add_fill(&rect(40.0, 0.0, 50.0, 10.0), FillRule::NonZero, color, &m);
        r.add_fill(&rect(60.0, 0.0, 70.0, 10.0), FillRule::NonZero, color, &m);
        assert_eq!(r.ops.len(), 4, "recorded as four separate draws");

        let total_indices = r.indices.len() as u32;
        let batched = crate::batch::batch_ops(r.ops);
        assert_eq!(batched.len(), 1, "four unclipped fills batch into one draw");
        match &batched[0] {
            PageOp::Draw { range, clip_ref } => {
                assert_eq!(*clip_ref, 0);
                assert_eq!(range.first_index, 0);
                assert_eq!(range.index_count, total_indices, "covers every index");
            }
            _ => panic!("expected a merged Draw op"),
        }
    }

    #[test]
    fn repeated_image_quads_use_absolute_indices_and_batch() {
        let v = TexturedVertex {
            pos: [0.0, 0.0],
            uv: [0.0, 0.0],
            color: [1.0; 4],
        };
        let mut r = PageRecorder::default();
        r.add_image([v; 4], 7);
        r.add_image([v; 4], 7);
        assert_eq!(r.tex_indices, vec![0, 1, 2, 0, 2, 3, 4, 5, 6, 4, 6, 7]);
        let batched = crate::batch::batch_ops(r.ops);
        assert_eq!(batched.len(), 1);
        match &batched[0] {
            PageOp::Image { range, .. } => {
                assert_eq!(range.base_vertex, 0);
                assert_eq!(range.index_count, 12);
            }
            _ => panic!("expected a batched image draw"),
        }
    }

    #[test]
    fn clip_depth_saturates_at_stencil8_capacity() {
        let mut r = PageRecorder::default();
        for _ in 0..300 {
            r.push_clip(&DlPath::new(), FillRule::NonZero, &map());
        }
        assert_eq!(r.clip_depth, u8::MAX as u32);
        for _ in 0..300 {
            r.pop_clip();
        }
        assert_eq!(r.clip_depth, 0);
    }
}
