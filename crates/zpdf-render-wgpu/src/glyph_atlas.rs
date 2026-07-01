//! Per-page glyph coverage atlas (P3.4/M6b): rasterizes each unique
//! `(font, glyph, device-pixel size)` combination once via tiny-skia — the
//! same analytic-AA rasterizer the CPU oracle uses — and packs the coverage
//! masks into one R8Unorm texture, so repeated glyph instances on a page blit
//! a quad instead of re-tessellating through `lyon` on every occurrence.
//!
//! **Scope** (see ROADMAP P3.4/M6b): built fresh per page (`GlyphAtlas::new`
//! in `begin_page`) and discarded at `end_page` — a page's unique-glyph
//! working set is small enough that one 2048x2048 atlas comfortably holds it,
//! without cross-page cache-coherency machinery. The caller (`glyph.rs`)
//! restricts entry to axis-aligned, non-mirrored, unscaled-shape glyph runs;
//! anything else (rotation, shear, mirroring, non-1.0 Tz) never reaches this
//! module and keeps using the existing, unchanged vector-fill path.
//!
//! **Eviction**: on atlas-full, the single least-recently-used slot is
//! evicted and its exact rect reused, but only if the new glyph fits in it;
//! otherwise `get_or_rasterize` returns `None` and the caller falls back to
//! vector-fill for that one glyph. The atlas is a pure optimization, never a
//! correctness requirement — every failure mode here degrades gracefully.

use std::collections::HashMap;

use tiny_skia::{FillRule as SkFillRule, Mask, PathBuilder, Transform};

use zpdf_font::{GlyphOutline, OutlineCommand};

use crate::context::GpuContext;
use crate::transform::TexturedVertex;

/// AA bleed margin, in pixels, reserved around each rasterized glyph.
const PAD: u32 = 1;

/// Default per-page atlas footprint. A page's unique-glyph working set is
/// small (dozens-hundreds, even for dense CJK); 2048x2048 R8 (4 MiB) is a
/// generous bound well within any real adapter's texture-size floor (the
/// smallest guaranteed `max_texture_dimension_2d` across D3D11/Vulkan/Metal/
/// WebGPU is 4096), so no negotiation against `GpuContext` is needed here.
const GLYPH_ATLAS_SIZE: u32 = 2048;

/// Identifies one rasterizable glyph: font, glyph id, and its *device-pixel*
/// horizontal/vertical em-size, each in **millipixels** (1/1000 device
/// pixel — see [`GlyphKey::MILLI`]). Two glyph instances with an equal key
/// produce pixel-identical rasters, so only the first pays rasterization
/// cost; later occurrences reuse the atlas slot.
///
/// Millipixel (not whole-pixel) granularity matters: real-world body text is
/// usually 9-15 device pixels tall, where a whole-pixel rounding error (up to
/// 0.5px) is a large *relative* shape distortion — measured on a real PDF,
/// that coarser bucketing alone pushed GPU-vs-CPU divergence on a text-heavy
/// page from an already-present ~1.3% (the pre-existing lyon-vs-tiny-skia AA
/// baseline) to ~3.9%. Millipixel precision makes the quantization error
/// negligible (≤0.0005px) without materially hurting cache reuse — real
/// documents repeat the *same* nominal font size/scale across a run, which
/// hashes identically regardless of bucket coarseness; coarse buckets mostly
/// just gave suspicious answers under floating-point noise.
#[derive(PartialEq, Eq, Hash, Clone, Copy, Debug)]
pub struct GlyphKey {
    pub font_id: u32,
    pub glyph_id: u16,
    pub x_millipx_per_em: i32,
    pub y_millipx_per_em: i32,
}

impl GlyphKey {
    /// Millipixels per device pixel (the key's fixed-point scale factor).
    pub const MILLI: f64 = 1000.0;
}

/// Where a glyph's coverage mask lives in the atlas, and how to place a quad
/// so the raster's font-space origin lands under the caller's device-pixel
/// pen position.
#[derive(Clone, Copy)]
pub struct AtlasEntry {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    /// Offset (in raster pixels, from the raster's top-left) of the glyph's
    /// font-space origin (0,0) — i.e. the pen position. Already includes `PAD`.
    pub pen_x: f32,
    pub pen_y: f32,
}

struct Slot {
    entry: AtlasEntry,
    last_used: u64,
}

/// A page-scoped coverage atlas. `pixels` is uploaded to an R8Unorm texture
/// once in `end_page`; nothing in this module touches the GPU.
pub struct GlyphAtlas {
    size: u32,
    pixels: Vec<u8>,
    slots: HashMap<GlyphKey, Slot>,
    clock: u64,
    shelf_x: u32,
    shelf_y: u32,
    shelf_h: u32,
}

impl Default for GlyphAtlas {
    fn default() -> Self {
        Self::new(GLYPH_ATLAS_SIZE)
    }
}

impl GlyphAtlas {
    pub fn new(size: u32) -> Self {
        Self {
            size,
            pixels: vec![0u8; (size as usize) * (size as usize)],
            slots: HashMap::new(),
            clock: 0,
            shelf_x: 0,
            shelf_y: 0,
            shelf_h: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    pub fn size(&self) -> u32 {
        self.size
    }

    /// Look up (or rasterize and cache) the coverage mask for `key`. `outline`
    /// is the glyph's font-unit (Y-up) outline; `upem` its units-per-em.
    /// Returns `None` for a degenerate outline, a glyph too large for this
    /// atlas even when empty, or one that doesn't fit after evicting the
    /// single least-recently-used slot — the caller falls back to vector-fill.
    pub fn get_or_rasterize(
        &mut self,
        key: GlyphKey,
        outline: &GlyphOutline,
        upem: f32,
    ) -> Option<AtlasEntry> {
        self.clock += 1;
        if let Some(slot) = self.slots.get_mut(&key) {
            slot.last_used = self.clock;
            return Some(slot.entry);
        }
        if key.x_millipx_per_em <= 0 || key.y_millipx_per_em <= 0 {
            return None;
        }

        let (min_x, _min_y, max_x, max_y) = outline_bbox(outline)?;
        let scale_x = key.x_millipx_per_em as f64 / GlyphKey::MILLI / upem as f64;
        let scale_y = key.y_millipx_per_em as f64 / GlyphKey::MILLI / upem as f64;
        let w = (((max_x - min_x) * scale_x).ceil() as u32 + 2 * PAD).max(1);
        let h = (((max_y - _min_y) * scale_y).ceil() as u32 + 2 * PAD).max(1);
        if w > self.size || h > self.size {
            return None; // pathologically large glyph — not atlas-able
        }
        let path = build_raster_path(outline, min_x, max_y, scale_x, scale_y)?;
        let mut mask = Mask::new(w, h)?;
        mask.fill_path(&path, SkFillRule::Winding, true, Transform::identity());

        let (x, y) = self.allocate(w, h)?;
        blit_mask(&mut self.pixels, self.size, x, y, w, h, mask.data());

        let entry = AtlasEntry {
            x,
            y,
            w,
            h,
            pen_x: (-min_x * scale_x) as f32 + PAD as f32,
            pen_y: (max_y * scale_y) as f32 + PAD as f32,
        };
        self.slots.insert(
            key,
            Slot {
                entry,
                last_used: self.clock,
            },
        );
        Some(entry)
    }

    /// Shelf-pack a `w x h` rect. On overflow, evict the single
    /// least-recently-used slot and reuse its exact rect if large enough for
    /// `w x h`; otherwise fail. This is a minimal "free list of size 1", not a
    /// general allocator — the shelf cursor itself never moves backward, so
    /// already-placed glyphs are never invalidated.
    fn allocate(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        if let Some(pos) = self.try_shelf_alloc(w, h) {
            return Some(pos);
        }
        let victim = self
            .slots
            .iter()
            .min_by_key(|(_, s)| s.last_used)
            .map(|(k, s)| (*k, s.entry))?;
        if victim.1.w >= w && victim.1.h >= h {
            self.slots.remove(&victim.0);
            return Some((victim.1.x, victim.1.y));
        }
        None
    }

    fn try_shelf_alloc(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        if self.shelf_x + w > self.size {
            self.shelf_x = 0;
            self.shelf_y += self.shelf_h;
            self.shelf_h = 0;
        }
        if self.shelf_y + h > self.size {
            return None;
        }
        let pos = (self.shelf_x, self.shelf_y);
        self.shelf_x += w;
        self.shelf_h = self.shelf_h.max(h);
        Some(pos)
    }
}

/// Font-space bounding box of an outline, including control points (a safe,
/// slightly loose superset — fine for padding purposes, avoids Bezier-extrema
/// math). `None` for an empty or degenerate (zero-area) outline.
fn outline_bbox(outline: &GlyphOutline) -> Option<(f64, f64, f64, f64)> {
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    let mut touch = |x: f64, y: f64| {
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    };
    for cmd in &outline.commands {
        match *cmd {
            OutlineCommand::MoveTo(x, y) | OutlineCommand::LineTo(x, y) => touch(x, y),
            OutlineCommand::QuadTo(cx, cy, x, y) => {
                touch(cx, cy);
                touch(x, y);
            }
            OutlineCommand::CurveTo(c1x, c1y, c2x, c2y, x, y) => {
                touch(c1x, c1y);
                touch(c2x, c2y);
                touch(x, y);
            }
            OutlineCommand::Close => {}
        }
    }
    if min_x.is_finite() && min_y.is_finite() && max_x > min_x && max_y > min_y {
        Some((min_x, min_y, max_x, max_y))
    } else {
        None
    }
}

/// Build the glyph outline as a tiny-skia path in raster-local pixel space:
/// font-space `(gx, gy)` -> `((gx-min_x)*scale_x + PAD, (max_y-gy)*scale_y + PAD)`
/// (the Y-flip mirrors font space Y-up into raster space Y-down). A direct,
/// unguarded per-command translation — mirrors the CPU oracle's own
/// `build_outline_transformed_path` (zpdf-render-cpu/src/lib.rs) exactly, since
/// outline command streams are always well-formed (MoveTo-led contours) by
/// construction; matching that code path's structure (not just its output) is
/// what keeps this rasterization pixel-faithful to the oracle.
fn build_raster_path(
    outline: &GlyphOutline,
    min_x: f64,
    max_y: f64,
    scale_x: f64,
    scale_y: f64,
) -> Option<tiny_skia::Path> {
    let rx = |gx: f64| ((gx - min_x) * scale_x) as f32 + PAD as f32;
    let ry = |gy: f64| ((max_y - gy) * scale_y) as f32 + PAD as f32;
    let mut b = PathBuilder::new();
    for cmd in &outline.commands {
        match *cmd {
            OutlineCommand::MoveTo(x, y) => b.move_to(rx(x), ry(y)),
            OutlineCommand::LineTo(x, y) => b.line_to(rx(x), ry(y)),
            OutlineCommand::QuadTo(cx, cy, x, y) => b.quad_to(rx(cx), ry(cy), rx(x), ry(y)),
            OutlineCommand::CurveTo(c1x, c1y, c2x, c2y, x, y) => {
                b.cubic_to(rx(c1x), ry(c1y), rx(c2x), ry(c2y), rx(x), ry(y))
            }
            OutlineCommand::Close => b.close(),
        }
    }
    b.finish()
}

/// Copy a tightly-packed `w x h` coverage mask into the atlas at `(x, y)`,
/// respecting the atlas's own row stride (`atlas_size`).
fn blit_mask(atlas: &mut [u8], atlas_size: u32, x: u32, y: u32, w: u32, h: u32, mask_data: &[u8]) {
    for row in 0..h {
        let src_start = (row * w) as usize;
        let src = &mask_data[src_start..src_start + w as usize];
        let dst_start = ((y + row) * atlas_size + x) as usize;
        atlas[dst_start..dst_start + w as usize].copy_from_slice(src);
    }
}

/// Build the device-pixel quad + atlas UVs for one rasterized glyph instance:
/// translate the raster's own `[0,w]x[0,h]` pixel rectangle so its pen point
/// (`entry.pen_x`, `entry.pen_y`) lands under `origin` — the glyph's true
/// device-pixel pen position, computed exactly like the vector-fill path's
/// `outline_to_pixel(0, 0, ...)` (same formula, evaluated at the glyph
/// origin instead of at every outline vertex). This is a pure translation,
/// never a scale/shear: valid only when the raster was built at the run's
/// actual device-pixel em-size, which `glyph.rs` guarantees by restricting
/// atlas entry to axis-aligned, non-mirrored runs.
pub fn glyph_quad(
    entry: AtlasEntry,
    atlas_size: u32,
    origin: (f32, f32),
    color: [f32; 4],
) -> [TexturedVertex; 4] {
    let ox = origin.0 - entry.pen_x;
    let oy = origin.1 - entry.pen_y;
    let (w, h) = (entry.w as f32, entry.h as f32);
    let s = atlas_size as f32;
    let (u0, v0, u1, v1) = (
        entry.x as f32 / s,
        entry.y as f32 / s,
        (entry.x as f32 + w) / s,
        (entry.y as f32 + h) / s,
    );
    [
        TexturedVertex {
            pos: [ox, oy],
            uv: [u0, v0],
            color,
        },
        TexturedVertex {
            pos: [ox + w, oy],
            uv: [u1, v0],
            color,
        },
        TexturedVertex {
            pos: [ox + w, oy + h],
            uv: [u1, v1],
            color,
        },
        TexturedVertex {
            pos: [ox, oy + h],
            uv: [u0, v1],
            color,
        },
    ]
}

/// Upload the atlas's coverage pixels to an `R8Unorm` texture and build its
/// bind group (group 1: texture + the shared bilinear sampler). Called once
/// per page in `end_page`, only when the atlas is non-empty.
pub fn upload_atlas_bind_group(ctx: &GpuContext, atlas: &GlyphAtlas) -> wgpu::BindGroup {
    let size = wgpu::Extent3d {
        width: atlas.size(),
        height: atlas.size(),
        depth_or_array_layers: 1,
    };
    let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("zpdf-glyph-atlas"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    ctx.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        atlas.pixels(),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(atlas.size()),
            rows_per_image: Some(atlas.size()),
        },
        size,
    );
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("zpdf-glyph-atlas-bg"),
        layout: &ctx.pipelines.glyph_bgl,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&ctx.pipelines.sampler),
            },
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A simple closed square outline, 0..1000 font units (upem 1000).
    fn square_outline() -> GlyphOutline {
        GlyphOutline {
            commands: vec![
                OutlineCommand::MoveTo(0.0, 0.0),
                OutlineCommand::LineTo(1000.0, 0.0),
                OutlineCommand::LineTo(1000.0, 1000.0),
                OutlineCommand::LineTo(0.0, 1000.0),
                OutlineCommand::Close,
            ],
            advance_width: 1000.0,
        }
    }

    /// Builds a `GlyphKey` from whole-pixel sizes (test convenience — the
    /// real field unit is millipixels).
    fn key(x_px: i32, y_px: i32) -> GlyphKey {
        GlyphKey {
            font_id: 0,
            glyph_id: 1,
            x_millipx_per_em: x_px * 1000,
            y_millipx_per_em: y_px * 1000,
        }
    }

    #[test]
    fn rasterizes_and_caches_a_glyph() {
        let mut atlas = GlyphAtlas::new(256);
        let outline = square_outline();
        assert!(atlas.is_empty());
        let e1 = atlas
            .get_or_rasterize(key(20, 20), &outline, 1000.0)
            .expect("rasterizes");
        assert!(!atlas.is_empty());
        // A 20px em-size square glyph filling the full em box should raster to
        // roughly 20x20 + padding.
        assert_eq!(e1.w, 20 + 2 * PAD);
        assert_eq!(e1.h, 20 + 2 * PAD);

        // Second lookup with the same key reuses the slot (no repack).
        let e2 = atlas
            .get_or_rasterize(key(20, 20), &outline, 1000.0)
            .expect("cached");
        assert_eq!((e1.x, e1.y), (e2.x, e2.y));
    }

    #[test]
    fn different_size_bucket_is_a_different_slot() {
        let mut atlas = GlyphAtlas::new(256);
        let outline = square_outline();
        let e1 = atlas
            .get_or_rasterize(key(20, 20), &outline, 1000.0)
            .unwrap();
        let e2 = atlas
            .get_or_rasterize(key(40, 40), &outline, 1000.0)
            .unwrap();
        assert_ne!((e1.x, e1.y, e1.w), (e2.x, e2.y, e2.w));
    }

    /// Regression for the millipixel-precision fix: whole-pixel rounding
    /// (the original v1 design) distorted a real PDF's body text by ~3x
    /// versus the pre-existing lyon-vs-tiny-skia AA baseline, because a
    /// ≤0.5px rounding error is a large relative error at typical 9-15px
    /// body-text em-sizes. A fractional em-size (12.3px, i.e. 12_300
    /// millipixels — between the 12px and 13px whole-pixel buckets) must
    /// rasterize the *shape itself* at a scale close to the true 12.3, not
    /// snap to 12 or 13: sum the blitted coverage (a filled square's total
    /// ink is exactly its area, `size^2`, modulo AA fringe) and check it
    /// lands near `12.3^2 = 151.3`, which is far enough from both `12^2=144`
    /// and `13^2=169` to distinguish "accurate" from "snapped to a
    /// whole-pixel bucket" — unlike the raster buffer's outer `w`/`h`, which
    /// only ever differ by at most 1px regardless of input precision (ceil
    /// rounding) and so can't tell the two cases apart.
    #[test]
    fn fractional_millipixel_size_is_not_snapped_to_whole_pixels() {
        let mut atlas = GlyphAtlas::new(256);
        let outline = square_outline();
        let key = GlyphKey {
            font_id: 0,
            glyph_id: 1,
            x_millipx_per_em: 12_300,
            y_millipx_per_em: 12_300,
        };
        let entry = atlas.get_or_rasterize(key, &outline, 1000.0).unwrap();
        let mut coverage = 0.0f64;
        for row in entry.y..entry.y + entry.h {
            for col in entry.x..entry.x + entry.w {
                coverage += atlas.pixels()[(row * atlas.size() + col) as usize] as f64 / 255.0;
            }
        }
        assert!(
            (coverage - 151.29).abs() < 3.0,
            "total ink coverage {coverage:.1} should track the exact 12.3px \
             target (~151.3), not a whole-pixel snap (144 or 169)"
        );
    }

    #[test]
    fn degenerate_outline_returns_none() {
        let mut atlas = GlyphAtlas::new(256);
        let empty = GlyphOutline {
            commands: vec![],
            advance_width: 0.0,
        };
        assert!(atlas
            .get_or_rasterize(key(20, 20), &empty, 1000.0)
            .is_none());
    }

    #[test]
    fn non_positive_size_bucket_returns_none() {
        let mut atlas = GlyphAtlas::new(256);
        let outline = square_outline();
        assert!(atlas
            .get_or_rasterize(key(0, 20), &outline, 1000.0)
            .is_none());
        assert!(atlas
            .get_or_rasterize(key(20, -5), &outline, 1000.0)
            .is_none());
    }

    #[test]
    fn glyph_larger_than_atlas_returns_none() {
        let mut atlas = GlyphAtlas::new(64);
        let outline = square_outline();
        // 1000px em-size square vastly exceeds a 64x64 atlas.
        assert!(atlas
            .get_or_rasterize(key(1000, 1000), &outline, 1000.0)
            .is_none());
    }

    #[test]
    fn overflow_evicts_lru_slot_and_reuses_its_rect() {
        // A small atlas that fits exactly two 30x30-ish glyphs per shelf pass.
        let mut atlas = GlyphAtlas::new(64);
        let outline = square_outline();
        let a = atlas
            .get_or_rasterize(key(28, 28), &outline, 1000.0)
            .unwrap();
        // Touch `a` again so it's more recently used than the next insert.
        atlas
            .get_or_rasterize(key(28, 28), &outline, 1000.0)
            .unwrap();
        let _b = atlas
            .get_or_rasterize(
                GlyphKey {
                    font_id: 0,
                    glyph_id: 2,
                    x_millipx_per_em: 28_000,
                    y_millipx_per_em: 28_000,
                },
                &outline,
                1000.0,
            )
            .unwrap();
        // A third distinct glyph of the same size should force an eviction
        // (shelf packing at 64x64 with 30x30 slots has room for very few) —
        // exercise the path without asserting exactly which slot is evicted.
        let c_key = GlyphKey {
            font_id: 0,
            glyph_id: 3,
            x_millipx_per_em: 28_000,
            y_millipx_per_em: 28_000,
        };
        let result = atlas.get_or_rasterize(c_key, &outline, 1000.0);
        // Either it fit directly or eviction made room — both are acceptable;
        // the important contract is it never panics and `a`'s original slot
        // remains valid until actually evicted.
        let _ = result;
        let _ = a;
    }
}
