//! Page recorder: a single geometry arena plus an ordered op list.
//!
//! Clips interleave with draws, so we cannot batch all solid geometry and draw it
//! at the end — order matters. `execute` appends ops; `end_page` replays them into
//! one render pass. Clip state is a stencil counter: stencil[pixel] = number of
//! active clip paths covering it; content draws test `stencil == clip_depth`
//! (inside the intersection of all active clips).

use crate::path::{fill_mesh, stroke_mesh, Mesh};
use crate::transform::{PageMap, SolidVertex, TexturedVertex};
use zpdf_display_list::{BlendMode, FillRule, Path as DlPath, StrokeStyle};

/// A range of indices/vertices in the shared arena.
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
    /// Begin a transparency group: subsequent ops render to a fresh offscreen layer.
    /// `clips` are the clip paths active at push time, re-stamped into the new layer.
    PushBlend {
        mode: BlendMode,
        clips: Vec<ClipStamp>,
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
}

/// Accumulates geometry + ops for one page.
#[derive(Default)]
pub struct PageRecorder {
    pub vertices: Vec<SolidVertex>,
    pub indices: Vec<u32>,
    /// Separate arena for image quads (different vertex format).
    pub tex_vertices: Vec<TexturedVertex>,
    pub tex_indices: Vec<u32>,
    pub ops: Vec<PageOp>,
    clip_stack: Vec<ClipEntry>,
    clip_depth: u32,
}

impl PageRecorder {
    fn append(&mut self, mesh: Mesh) -> MeshRange {
        let base_vertex = self.vertices.len() as i32;
        let first_index = self.indices.len() as u32;
        let index_count = mesh.indices.len() as u32;
        self.vertices.extend_from_slice(&mesh.vertices);
        // Indices stay mesh-local (0-based); draw_indexed applies base_vertex.
        self.indices.extend_from_slice(&mesh.indices);
        MeshRange {
            first_index,
            index_count,
            base_vertex,
        }
    }

    /// Record a pre-tessellated mesh as a content draw at the current clip depth.
    /// Used by fills/strokes and by glyph rendering (which builds its own meshes).
    pub fn add_mesh(&mut self, mesh: Mesh) {
        if mesh.indices.is_empty() {
            return;
        }
        let range = self.append(mesh);
        self.ops.push(PageOp::Draw {
            range,
            clip_ref: self.clip_depth,
        });
    }

    /// Record an image quad (4 textured vertices) at the current clip depth.
    pub fn add_image(&mut self, quad: [TexturedVertex; 4], image_id: u32) {
        let base_vertex = self.tex_vertices.len() as i32;
        let first_index = self.tex_indices.len() as u32;
        self.tex_vertices.extend_from_slice(&quad);
        self.tex_indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
        self.ops.push(PageOp::Image {
            range: MeshRange {
                first_index,
                index_count: 6,
                base_vertex,
            },
            image_id,
            clip_ref: self.clip_depth,
        });
    }

    pub fn add_fill(&mut self, path: &DlPath, rule: FillRule, color: [f32; 4], map: &PageMap) {
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
        if let Some(mesh) = stroke_mesh(path, style, color, map) {
            self.add_mesh(mesh);
        }
    }

    /// Push a clip path (tessellated as a fill with the given rule). Always
    /// occupies a depth level so the matching pop stays balanced, even when the
    /// path is empty (an empty clip ⇒ nothing passes at the deeper level, which
    /// is the correct "clip to nothing" behavior).
    pub fn push_clip(&mut self, path: &DlPath, rule: FillRule, map: &PageMap) {
        let range = fill_mesh(path, rule, [0.0; 4], map).map(|m| self.append(m));
        let ref_value = self.clip_depth;
        if let Some(r) = range {
            self.ops.push(PageOp::StampClip {
                range: r,
                ref_value,
            });
        }
        self.clip_stack.push(ClipEntry { range, ref_value });
        self.clip_depth += 1;
    }

    /// Pop a clip: rebuild the stencil from scratch (reset to 0, re-stamp the
    /// remaining entries). Decrementing the popped region is unsafe at shared
    /// MSAA boundaries; PDFs nest shallowly so rebuild is cheap. Unbalanced pop
    /// is a no-op (mirrors the CPU).
    pub fn pop_clip(&mut self) {
        if self.clip_stack.pop().is_none() {
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
    pub fn push_blend(&mut self, mode: BlendMode) {
        let clips = self.active_clips();
        self.ops.push(PageOp::PushBlend { mode, clips });
    }

    pub fn pop_blend(&mut self) {
        self.ops.push(PageOp::PopBlend);
    }

    pub fn has_blend_groups(&self) -> bool {
        self.ops.iter().any(|o| matches!(o, PageOp::PushBlend { .. }))
    }

    /// True if any `ResetStencil` op was recorded (i.e. clips were popped) and a
    /// fullscreen reset quad must be appended before replay.
    pub fn uses_reset(&self) -> bool {
        self.ops.iter().any(|o| matches!(o, PageOp::ResetStencil))
    }

    /// Append a fullscreen quad in device pixels (for `ResetStencil`).
    pub fn append_fullscreen(&mut self, w_px: f32, h_px: f32) -> MeshRange {
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
            page_height: 100.0,
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
    fn nested_clips_increment_then_rebuild_on_pop() {
        let mut r = PageRecorder::default();
        let m = map();
        // push two clips, draw inside, pop both.
        r.push_clip(&rect(10.0, 10.0, 90.0, 90.0), FillRule::NonZero, &m);
        r.push_clip(&rect(20.0, 20.0, 80.0, 80.0), FillRule::NonZero, &m);
        r.add_fill(&rect(0.0, 0.0, 100.0, 100.0), FillRule::NonZero, [1.0, 0.0, 0.0, 1.0], &m);
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
        let resets = r.ops.iter().filter(|o| matches!(o, PageOp::ResetStencil)).count();
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
    fn empty_clip_path_still_occupies_depth() {
        let mut r = PageRecorder::default();
        r.push_clip(&DlPath::new(), FillRule::NonZero, &map());
        assert_eq!(r.clip_depth, 1, "empty clip still advances depth");
        // No StampClip op emitted for the empty geometry.
        assert!(!r.ops.iter().any(|o| matches!(o, PageOp::StampClip { .. })));
    }
}
