//! Draw-call batching (P3.7): collapse consecutive [`PageOp`]s that share the
//! same pipeline + clip (+ image) state into a single `draw_indexed` call.
//!
//! This is a pure run-length merge over the ALREADY-SEQUENTIAL op list — never
//! a sort or reorder. Reordering is unsafe here: overlapping alpha-blended
//! draws are order-dependent under "over" compositing, and clip state is
//! stencil-sequential (`record.rs`: "Clips interleave with draws ... order
//! matters"). Merging strictly-adjacent ops with an identical key is safe by
//! construction: consecutive `PageRecorder::append`s land in contiguous index
//! ranges (indices are pre-rebased to absolute, so `base_vertex` is always 0 —
//! see `record.rs`), so replacing N sequential `draw_indexed(range_i, 0, 0..1)`
//! calls with one covering their concatenation draws the exact same triangles
//! in the exact same order — bit-identical output, not an approximation.

use crate::record::{MeshRange, PageOp};

/// The state a `Draw`/`Image`/`StampClip` op renders under. Two adjacent ops
/// with an equal key (and contiguous ranges) are merge-eligible. `Other`
/// covers `ResetStencil`/`PushBlend`/`PopBlend`, which never merge — including
/// with each other, since they're structurally distinct even when both map to
/// this bucket (`batch_ops` never attempts to merge an `Other`-keyed op).
#[derive(PartialEq, Eq, Clone, Copy)]
enum BatchKey {
    Draw(u32),       // clip_ref
    Image(u32, u32), // image_id, clip_ref
    Glyph(u32),      // clip_ref — a page has exactly one atlas, no id needed
    StampClip(u32),  // ref_value
    Other,
}

fn key_of(op: &PageOp) -> BatchKey {
    match op {
        PageOp::Draw { clip_ref, .. } => BatchKey::Draw(*clip_ref),
        PageOp::Image {
            image_id, clip_ref, ..
        } => BatchKey::Image(*image_id, *clip_ref),
        PageOp::Glyph { clip_ref, .. } => BatchKey::Glyph(*clip_ref),
        PageOp::StampClip { ref_value, .. } => BatchKey::StampClip(*ref_value),
        PageOp::ResetStencil | PageOp::PushBlend { .. } | PageOp::PopBlend => BatchKey::Other,
    }
}

/// Two ranges are mergeable when `a` immediately precedes `b` in the shared
/// arena (same base_vertex — always 0 post-rebase — and `b` starts exactly
/// where `a` ends).
fn contiguous(a: &MeshRange, b: &MeshRange) -> bool {
    a.base_vertex == b.base_vertex && a.first_index + a.index_count == b.first_index
}

/// Extend `op`'s range to cover `next` too, if they're the same op kind with
/// equal keys and contiguous ranges. Returns `false` (no merge) otherwise.
fn try_merge(op: &mut PageOp, next: &PageOp) -> bool {
    match (op, next) {
        (
            PageOp::Draw { range, clip_ref },
            PageOp::Draw {
                range: r2,
                clip_ref: c2,
            },
        ) if clip_ref == c2 && contiguous(range, r2) => {
            range.index_count += r2.index_count;
            true
        }
        (
            PageOp::Image {
                range,
                image_id,
                clip_ref,
            },
            PageOp::Image {
                range: r2,
                image_id: i2,
                clip_ref: c2,
            },
        ) if image_id == i2 && clip_ref == c2 && contiguous(range, r2) => {
            range.index_count += r2.index_count;
            true
        }
        (
            PageOp::Glyph { range, clip_ref },
            PageOp::Glyph {
                range: r2,
                clip_ref: c2,
            },
        ) if clip_ref == c2 && contiguous(range, r2) => {
            range.index_count += r2.index_count;
            true
        }
        (
            PageOp::StampClip { range, ref_value },
            PageOp::StampClip {
                range: r2,
                ref_value: v2,
            },
        ) if ref_value == v2 && contiguous(range, r2) => {
            range.index_count += r2.index_count;
            true
        }
        _ => false,
    }
}

/// Merge consecutive same-key ops in a single linear pass. Consumes `ops`
/// (rather than borrowing) so non-mergeable variants — notably `PushBlend`,
/// which carries owned nested data (`MaskOps`, itself a `Vec<PageOp>`) — pass
/// through by move, with no need for `PageOp: Clone`.
pub fn batch_ops(ops: Vec<PageOp>) -> Vec<PageOp> {
    let mut out: Vec<PageOp> = Vec::with_capacity(ops.len());
    for op in ops {
        let key = key_of(&op);
        let mut merged = false;
        if key != BatchKey::Other {
            if let Some(prev) = out.last_mut() {
                if key_of(prev) == key {
                    merged = try_merge(prev, &op);
                }
            }
        }
        if !merged {
            out.push(op);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draw(first_index: u32, index_count: u32, clip_ref: u32) -> PageOp {
        PageOp::Draw {
            range: MeshRange {
                first_index,
                index_count,
                base_vertex: 0,
            },
            clip_ref,
        }
    }

    fn image(first_index: u32, index_count: u32, image_id: u32, clip_ref: u32) -> PageOp {
        PageOp::Image {
            range: MeshRange {
                first_index,
                index_count,
                base_vertex: 0,
            },
            image_id,
            clip_ref,
        }
    }

    fn stamp(first_index: u32, index_count: u32, ref_value: u32) -> PageOp {
        PageOp::StampClip {
            range: MeshRange {
                first_index,
                index_count,
                base_vertex: 0,
            },
            ref_value,
        }
    }

    fn glyph(first_index: u32, index_count: u32, clip_ref: u32) -> PageOp {
        PageOp::Glyph {
            range: MeshRange {
                first_index,
                index_count,
                base_vertex: 0,
            },
            clip_ref,
        }
    }

    fn draw_ranges(ops: &[PageOp]) -> Vec<(u32, u32, u32)> {
        ops.iter()
            .filter_map(|o| match o {
                PageOp::Draw { range, clip_ref } => {
                    Some((range.first_index, range.index_count, *clip_ref))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn adjacent_same_clip_draws_merge_into_one() {
        // Three contiguous fills at clip depth 0: indices [0,6) [6,12) [12,18).
        let ops = vec![draw(0, 6, 0), draw(6, 6, 0), draw(12, 6, 0)];
        let out = batch_ops(ops);
        assert_eq!(out.len(), 1, "three same-clip draws collapse to one");
        assert_eq!(draw_ranges(&out), vec![(0, 18, 0)]);
    }

    #[test]
    fn different_clip_ref_prevents_merge() {
        let ops = vec![draw(0, 6, 0), draw(6, 6, 1)];
        let out = batch_ops(ops);
        assert_eq!(out.len(), 2, "different clip_ref must not merge");
        assert_eq!(draw_ranges(&out), vec![(0, 6, 0), (6, 6, 1)]);
    }

    #[test]
    fn stamp_clip_or_reset_between_draws_breaks_the_run() {
        // Draw, then a StampClip (different op kind) at a *different* arena
        // position, then another draw with the same clip_ref as the first but
        // NOT contiguous with it (StampClip's own mesh sits in between).
        let ops = vec![draw(0, 6, 0), stamp(6, 3, 0), draw(9, 6, 0)];
        let out = batch_ops(ops);
        assert_eq!(
            out.len(),
            3,
            "an intervening op of a different kind must not be bridged"
        );
    }

    #[test]
    fn reset_stencil_and_blend_ops_never_merge_with_each_other() {
        let ops = vec![PageOp::ResetStencil, PageOp::ResetStencil, PageOp::PopBlend];
        let out = batch_ops(ops);
        assert_eq!(
            out.len(),
            3,
            "Other-keyed ops must never merge, even with an identical variant"
        );
    }

    #[test]
    fn image_ops_only_merge_with_matching_image_id() {
        let ops = vec![image(0, 6, 1, 0), image(6, 6, 2, 0), image(12, 6, 1, 0)];
        let out = batch_ops(ops);
        // First two differ by image_id (no merge); third also differs from the
        // (now-current) second, so nothing merges here — all three distinct.
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn image_ops_with_same_id_and_clip_merge() {
        let ops = vec![image(0, 6, 5, 2), image(6, 6, 5, 2)];
        let out = batch_ops(ops);
        assert_eq!(out.len(), 1);
        match &out[0] {
            PageOp::Image {
                range, image_id, ..
            } => {
                assert_eq!(range.index_count, 12);
                assert_eq!(*image_id, 5);
            }
            _ => panic!("expected Image"),
        }
    }

    #[test]
    fn non_contiguous_same_key_draws_do_not_merge() {
        // Same clip_ref, but a gap in the index range (as if something else
        // had appended between them without recording an op) — must not merge.
        let ops = vec![draw(0, 6, 0), draw(10, 6, 0)];
        let out = batch_ops(ops);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn empty_ops_list_is_unchanged() {
        assert!(batch_ops(Vec::new()).is_empty());
    }

    #[test]
    fn adjacent_same_clip_glyph_quads_merge_into_one() {
        // Six glyph quads (6 indices each) at clip depth 0, as `add_glyph_quad`
        // emits them (contiguous absolute ranges) — the common text-run shape.
        let ops = vec![
            glyph(0, 6, 0),
            glyph(6, 6, 0),
            glyph(12, 6, 0),
            glyph(18, 6, 0),
        ];
        let out = batch_ops(ops);
        assert_eq!(out.len(), 1, "four same-clip glyph quads collapse to one");
        match &out[0] {
            PageOp::Glyph { range, clip_ref } => {
                assert_eq!(*clip_ref, 0);
                assert_eq!(range.first_index, 0);
                assert_eq!(range.index_count, 24);
            }
            _ => panic!("expected a merged Glyph op"),
        }
    }

    #[test]
    fn glyph_ops_never_merge_with_draw_ops() {
        let ops = vec![draw(0, 6, 0), glyph(6, 6, 0)];
        let out = batch_ops(ops);
        assert_eq!(out.len(), 2, "different op kinds must not merge");
    }
}
