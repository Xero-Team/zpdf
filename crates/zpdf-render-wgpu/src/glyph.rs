//! Glyph rendering: outline glyphs (vector-fill baseline) and Type3 glyphs.
//!
//! Outline glyphs are tessellated through the same `solid_fill` path as regular
//! fills, at the CPU oracle's exact `outline_to_pixel` coordinates — correct by
//! construction (same coordinates, same NonZero winding; only the AA kernel
//! differs). Type3 glyphs are content streams: interpreted to a sub-display-list
//! whose fills/strokes are transformed by FontMatrix and routed through the same
//! pipeline. Both honor the active clip (geometry is recorded at the current clip
//! depth, so the stencil test applies).

use lyon::math::point;
use lyon::path::Path as LyonPath;
use lyon::tessellation::StrokeOptions;

use zpdf_core::Matrix;
use zpdf_display_list::{FillRule, GlyphRun, Paint, Path as DlPath, PathElement, RenderCommand};
use zpdf_font::{FontCache, GlyphOutline, LoadedFont, OutlineCommand};

use crate::glyph_atlas::{self, GlyphKey};
use crate::path::{fill_lyon_path, stroke_lyon_path, TOLERANCE};
use crate::record::PageRecorder;
use crate::transform::{quantize_premul, PageMap};

/// Run-level constants for the glyph-space -> device-pixel transform.
struct GlyphXform {
    upem: f32,
    font_size: f32,
    map: PageMap,
    /// Horizontal text-scaling factor (Tz/100); scales the glyph shape x only.
    h_scale: f32,
}

/// Render one glyph run into the recorder. Dispatch guards mirror the CPU exactly:
/// every early-return here corresponds to a CPU no-op, so the GPU emits the same
/// set of glyphs (a missing guard would add/drop pixels and fail `compare`).
pub fn render_glyph_run(rec: &mut PageRecorder, fonts: &FontCache, map: &PageMap, run: &GlyphRun) {
    let Some(font) = fonts.get(run.font_id) else {
        return;
    };
    if !font.has_font_data() {
        return;
    }
    let Paint::Solid(c) = &run.paint else {
        return;
    };
    let color = quantize_premul(c, run.alpha);
    if color[3] == 0.0 {
        return; // fully transparent — contributes nothing (CPU draws invisibly)
    }

    let tm = &run.transform;

    if font.is_type3() {
        render_type3(rec, font, run, color, map, run.h_scale);
        return;
    }

    let x = GlyphXform {
        upem: font.units_per_em as f32,
        font_size: run.font_size,
        map: *map,
        h_scale: run.h_scale,
    };

    // Atlas fast path: only for axis-aligned, non-mirrored runs (rotation,
    // shear, or mirroring would need per-instance-rotated resampling of an
    // upright raster, which risks drifting from the CPU oracle's AA — those
    // runs always take the vector-fill path below, unchanged). `None` means
    // "run not atlas-eligible"; a `Some` glyph can still individually fall
    // back (degenerate outline, atlas full) via `get_or_rasterize` -> `None`.
    let atlas_key_size = if glyph_atlas_enabled() && is_axis_aligned(tm, run.h_scale) {
        axis_aligned_px_per_em(tm, run.h_scale, run.font_size, map.scale)
    } else {
        None
    };

    for g in &run.glyphs {
        let Some(outline) = font.glyph_outline(g.glyph_id) else {
            continue;
        };

        if let Some((x_millipx_per_em, y_millipx_per_em)) = atlas_key_size {
            let key = GlyphKey {
                font_id: run.font_id,
                glyph_id: g.glyph_id,
                x_millipx_per_em,
                y_millipx_per_em,
            };
            if let Some(entry) = rec.glyph_atlas.get_or_rasterize(key, &outline, x.upem) {
                let origin = outline_to_pixel(0.0, 0.0, g.x, tm, &x);
                let quad = glyph_atlas::glyph_quad(
                    entry,
                    rec.glyph_atlas.size(),
                    (origin.x, origin.y),
                    color,
                );
                rec.add_glyph_quad(quad);
                continue;
            }
        }

        if let Some(path) = build_outline_path(&outline, g.x, tm, &x) {
            if let Some(mesh) = fill_lyon_path(&path, FillRule::NonZero, color) {
                rec.add_mesh(mesh);
            }
        }
    }
}

/// A glyph run is atlas-eligible when its transform has no rotation/shear
/// (`tm.b`/`tm.c` ≈ 0) and no mirroring (`tm.a`, `tm.d`, `h_scale` all > 0).
/// The atlas rasterizes each glyph once, upright, at a fixed device-pixel
/// size, then blits a *translated* quad (see `glyph_atlas::glyph_quad`) — it
/// never rotates or flips the raster, so anything that needs true
/// per-instance rotation/mirroring must keep using the vector-fill path.
fn is_axis_aligned(tm: &Matrix, h_scale: f32) -> bool {
    const EPS: f64 = 1e-6;
    tm.b.abs() < EPS && tm.c.abs() < EPS && tm.a > EPS && tm.d > EPS && h_scale > 0.0
}

/// Device pixels per font em, in millipixels (see [`GlyphKey::MILLI`]) for
/// atlas bucketing — independent x/y scales (rather than a single "size") so
/// `Tz` (horizontal-only scaling) is captured exactly instead of forcing
/// square pixels. Reproduces `outline_to_pixel`'s effective per-axis scale
/// factor under the axis-aligned assumption (`tm.b == tm.c == 0`), so a
/// raster built at this size lands pixel-for-pixel under the real
/// device-pixel transform. Millipixel (not whole-pixel) precision matters: at
/// whole-pixel rounding, a real-world text page measured ~3x worse GPU-vs-CPU
/// divergence than the pre-existing lyon-vs-tiny-skia AA baseline, since a
/// ≤0.5px rounding error is a large *relative* distortion for typical
/// 9-15px body-text em-sizes (see `GlyphKey` doc comment for the measurement).
/// `None` when the effective size rounds to zero or negative — not atlas-able.
fn axis_aligned_px_per_em(
    tm: &Matrix,
    h_scale: f32,
    font_size: f32,
    scale: f32,
) -> Option<(i32, i32)> {
    let milli = GlyphKey::MILLI as f32;
    let x_px = (tm.a as f32 * font_size * h_scale * scale * milli).round() as i32;
    let y_px = (tm.d as f32 * font_size * scale * milli).round() as i32;
    (x_px > 0 && y_px > 0).then_some((x_px, y_px))
}

/// Debug escape hatch mirroring `ZPDF_GPU_FORCE_FALLBACK` (`context.rs`):
/// `ZPDF_GPU_GLYPH_ATLAS=0` forces every glyph run through the vector-fill
/// path, isolating the atlas's AA delta from the pre-existing GPU-vs-CPU MSAA
/// delta when diffing a page rendered both ways.
fn glyph_atlas_enabled() -> bool {
    std::env::var("ZPDF_GPU_GLYPH_ATLAS")
        .map(|v| v != "0")
        .unwrap_or(true)
}

/// Reproduce `CpuRenderer::outline_to_pixel`: font units -> text space (f32) ->
/// page space (f64 via the matrix) -> device pixels (f32), with the one fixed
/// page Y-flip. The f32->f64->f32 precision order is parity-critical.
fn outline_to_pixel(
    gx: f64,
    gy: f64,
    glyph_x: f32,
    tm: &Matrix,
    x: &GlyphXform,
) -> lyon::math::Point {
    // Th (h_scale) multiplies the glyph shape x only; the advance in glyph_x
    // already includes it.
    let tx = (gx as f32 / x.upem * x.font_size * x.h_scale + glyph_x) as f64;
    let ty = (gy as f32 / x.upem * x.font_size) as f64;
    let page_x = tm.a * tx + tm.c * ty + tm.e;
    let page_y = tm.b * tx + tm.d * ty + tm.f;
    let px = (page_x as f32 - x.map.x0) * x.map.scale;
    let py = (x.map.y1 - page_y as f32) * x.map.scale;
    point(px, py)
}

fn build_outline_path(
    outline: &GlyphOutline,
    glyph_x: f32,
    tm: &Matrix,
    x: &GlyphXform,
) -> Option<LyonPath> {
    if outline.commands.is_empty() {
        return None;
    }
    let p = |gx: f64, gy: f64| outline_to_pixel(gx, gy, glyph_x, tm, x);
    let mut b = LyonPath::builder();
    let mut open = false;
    for cmd in &outline.commands {
        match *cmd {
            OutlineCommand::MoveTo(gx, gy) => {
                if open {
                    b.end(false);
                }
                b.begin(p(gx, gy));
                open = true;
            }
            OutlineCommand::LineTo(gx, gy) => {
                if !open {
                    b.begin(p(gx, gy));
                    open = true;
                } else {
                    b.line_to(p(gx, gy));
                }
            }
            OutlineCommand::QuadTo(cx, cy, gx, gy) => {
                if !open {
                    b.begin(p(cx, cy));
                    open = true;
                }
                b.quadratic_bezier_to(p(cx, cy), p(gx, gy));
            }
            OutlineCommand::CurveTo(c1x, c1y, c2x, c2y, gx, gy) => {
                if !open {
                    b.begin(p(c1x, c1y));
                    open = true;
                }
                b.cubic_bezier_to(p(c1x, c1y), p(c2x, c2y), p(gx, gy));
            }
            OutlineCommand::Close => {
                if open {
                    b.end(true);
                    open = false;
                }
            }
        }
    }
    if open {
        b.end(false);
    }
    Some(b.build())
}

/// Render Type3 glyphs: each glyph is a content stream. Interpret it to a
/// sub-display-list (same glyph_rect as the CPU), then route only its fills and
/// strokes through the solid pipeline, transformed by FontMatrix. Mirrors
/// `CpuRenderer::render_type3_glyphs`.
fn render_type3(
    rec: &mut PageRecorder,
    font: &LoadedFont,
    run: &GlyphRun,
    color: [f32; 4],
    map: &PageMap,
    h_scale: f32,
) {
    use zpdf_content::interpreter::ContentInterpreter;

    let tm = &run.transform;
    let font_size = run.font_size;

    for g in &run.glyphs {
        let Some((stream, font_matrix)) = font.type3_glyph_stream(g.glyph_id) else {
            continue;
        };
        let glyph_rect = zpdf_core::Rect::new(0.0, -1000.0, 1000.0, 1000.0);
        let glyph_dl = ContentInterpreter::new(glyph_rect).interpret(stream);

        for cmd in &glyph_dl.commands {
            match cmd {
                RenderCommand::FillPath { path, rule, .. } => {
                    if let Some(p) =
                        build_type3_path(path, &font_matrix, font_size, h_scale, tm, g.x, map)
                    {
                        if let Some(mesh) = fill_lyon_path(&p, *rule, color) {
                            rec.add_mesh(mesh);
                        }
                    }
                }
                RenderCommand::StrokePath { path, style, .. } => {
                    // Width matches the CPU: scaled by the FontMatrix x-scale and
                    // font size, with a 0.5px floor.
                    let width = (style.width * font_matrix[0].abs() as f32 * font_size * map.scale)
                        .max(0.5);
                    if let Some(p) =
                        build_type3_path(path, &font_matrix, font_size, h_scale, tm, g.x, map)
                    {
                        let opts = StrokeOptions::tolerance(TOLERANCE).with_line_width(width);
                        if let Some(mesh) = stroke_lyon_path(&p, &opts, color) {
                            rec.add_mesh(mesh);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// Reproduce `CpuRenderer::type3_to_pixel`: glyph space -> text space (FontMatrix)
/// -> scale by font size -> add glyph x -> page space (matrix) -> device pixels.
#[allow(clippy::too_many_arguments)]
fn type3_to_pixel(
    gx: f64,
    gy: f64,
    font_matrix: &[f64; 6],
    font_size: f32,
    h_scale: f32,
    tm: &Matrix,
    glyph_x: f32,
    map: &PageMap,
) -> lyon::math::Point {
    let tx = font_matrix[0] * gx + font_matrix[2] * gy + font_matrix[4];
    let ty = font_matrix[1] * gx + font_matrix[3] * gy + font_matrix[5];
    let tx = tx * font_size as f64 * h_scale as f64 + glyph_x as f64;
    let ty = ty * font_size as f64;
    let page_x = tm.a * tx + tm.c * ty + tm.e;
    let page_y = tm.b * tx + tm.d * ty + tm.f;
    let px = (page_x as f32 - map.x0) * map.scale;
    let py = (map.y1 - page_y as f32) * map.scale;
    point(px, py)
}

#[allow(clippy::too_many_arguments)]
fn build_type3_path(
    path: &DlPath,
    font_matrix: &[f64; 6],
    font_size: f32,
    h_scale: f32,
    tm: &Matrix,
    glyph_x: f32,
    map: &PageMap,
) -> Option<LyonPath> {
    if path.elements.is_empty() {
        return None;
    }
    let p = |pt: zpdf_core::Point| {
        type3_to_pixel(
            pt.x,
            pt.y,
            font_matrix,
            font_size,
            h_scale,
            tm,
            glyph_x,
            map,
        )
    };
    let mut b = LyonPath::builder();
    let mut open = false;
    for el in &path.elements {
        match *el {
            PathElement::MoveTo(pt) => {
                if open {
                    b.end(false);
                }
                b.begin(p(pt));
                open = true;
            }
            PathElement::LineTo(pt) => {
                if !open {
                    b.begin(p(pt));
                    open = true;
                } else {
                    b.line_to(p(pt));
                }
            }
            PathElement::CurveTo(c1, c2, e) => {
                if !open {
                    b.begin(p(c1));
                    open = true;
                }
                b.cubic_bezier_to(p(c1), p(c2), p(e));
            }
            PathElement::Close => {
                if open {
                    b.end(true);
                    open = false;
                }
            }
        }
    }
    if open {
        b.end(false);
    }
    Some(b.build())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(scale: f32, y1: f32) -> PageMap {
        PageMap { scale, x0: 0.0, y1 }
    }

    #[test]
    fn outline_to_pixel_matches_cpu_formula_no_flip() {
        // upem 1000, font_size 10, scale 2, page rect top 100, identity+translate CTM.
        let x = GlyphXform {
            upem: 1000.0,
            font_size: 10.0,
            map: map(2.0, 100.0),
            h_scale: 1.0,
        };
        let tm = Matrix {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: 50.0,
            f: 50.0,
        };
        // glyph (500, 200) font units: tx = 5, ty = 2; page = (55, 52);
        // px = 110; py = (100-52)*2 = 96.
        let p = outline_to_pixel(500.0, 200.0, 0.0, &tm, &x);
        assert_eq!(p.x, 110.0);
        assert_eq!(p.y, 96.0);
    }

    #[test]
    fn outline_to_pixel_flipped_ctm_uses_fixed_page_flip() {
        // The CTM's negative `d` is honored as ordinary geometry; the one fixed
        // page Y-flip still applies (the old conditional skip was the bug1
        // upside-down defect).
        let x = GlyphXform {
            upem: 1000.0,
            font_size: 10.0,
            map: map(2.0, 100.0),
            h_scale: 1.0,
        };
        let tm = Matrix {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: -1.0,
            e: 50.0,
            f: 50.0,
        };
        // ty = 2; page_y = -1*2 + 50 = 48; py = (100-48)*2 = 104.
        let p = outline_to_pixel(500.0, 200.0, 0.0, &tm, &x);
        assert_eq!(p.x, 110.0);
        assert_eq!(p.y, 104.0);
    }

    #[test]
    fn outline_to_pixel_h_scale_mirrors_shape_x_only() {
        // Th = -1 mirrors the glyph shape x about the pen origin, but never y.
        let x = GlyphXform {
            upem: 1000.0,
            font_size: 10.0,
            map: map(2.0, 100.0),
            h_scale: -1.0,
        };
        let tm = Matrix {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: 50.0,
            f: 50.0,
        };
        // tx = 500/1000*10*(-1) + 0 = -5; page_x = 45; px = 90.
        // ty unaffected: 2; page_y = 52; py = (100-52)*2 = 96.
        let p = outline_to_pixel(500.0, 200.0, 0.0, &tm, &x);
        assert_eq!(p.x, 90.0);
        assert_eq!(p.y, 96.0);
    }

    #[test]
    fn outline_to_pixel_honors_page_rect_origin() {
        // Page rect (20, 10)-(.., 110): device = ((page_x-20)*2, (110-page_y)*2).
        let x = GlyphXform {
            upem: 1000.0,
            font_size: 10.0,
            map: PageMap {
                scale: 2.0,
                x0: 20.0,
                y1: 110.0,
            },
            h_scale: 1.0,
        };
        let tm = Matrix {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: 50.0,
            f: 50.0,
        };
        // page = (55, 52): px = (55-20)*2 = 70; py = (110-52)*2 = 116.
        let p = outline_to_pixel(500.0, 200.0, 0.0, &tm, &x);
        assert_eq!(p.x, 70.0);
        assert_eq!(p.y, 116.0);
    }

    fn identity_tm() -> Matrix {
        Matrix {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: 0.0,
            f: 0.0,
        }
    }

    #[test]
    fn axis_aligned_true_for_identity_scale_and_translation() {
        assert!(is_axis_aligned(&identity_tm(), 1.0));
        let scaled = Matrix {
            a: 2.5,
            d: 0.5,
            ..identity_tm()
        };
        assert!(is_axis_aligned(&scaled, 1.0));
        let translated = Matrix {
            e: 100.0,
            f: -50.0,
            ..identity_tm()
        };
        assert!(is_axis_aligned(&translated, 1.0));
    }

    #[test]
    fn axis_aligned_false_for_rotation_or_shear() {
        let rotated = Matrix {
            a: 0.707,
            b: 0.707,
            c: -0.707,
            d: 0.707,
            ..identity_tm()
        };
        assert!(!is_axis_aligned(&rotated, 1.0));
        let sheared = Matrix {
            c: 0.3,
            ..identity_tm()
        };
        assert!(!is_axis_aligned(&sheared, 1.0));
    }

    #[test]
    fn axis_aligned_false_for_mirroring() {
        let flip_x = Matrix {
            a: -1.0,
            ..identity_tm()
        };
        assert!(!is_axis_aligned(&flip_x, 1.0));
        let flip_y = Matrix {
            d: -1.0,
            ..identity_tm()
        };
        assert!(!is_axis_aligned(&flip_y, 1.0));
        assert!(
            !is_axis_aligned(&identity_tm(), -1.0),
            "negative h_scale mirrors glyph shape"
        );
    }

    #[test]
    fn px_per_em_scales_with_font_size_and_device_scale() {
        // font_size 12, scale 2 (144 DPI-ish), identity tm -> 24 device px/em
        // = 24000 millipixels/em.
        let (x_px, y_px) = axis_aligned_px_per_em(&identity_tm(), 1.0, 12.0, 2.0).unwrap();
        assert_eq!((x_px, y_px), (24_000, 24_000));
    }

    #[test]
    fn px_per_em_captures_h_scale_independently_of_y() {
        // Tz = 50% (h_scale 0.5): x halves, y unaffected.
        let (x_px, y_px) = axis_aligned_px_per_em(&identity_tm(), 0.5, 12.0, 2.0).unwrap();
        assert_eq!((x_px, y_px), (12_000, 24_000));
    }

    #[test]
    fn px_per_em_rounding_is_monotonic_in_font_size() {
        let sizes: Vec<i32> = (1..40)
            .map(|pt| {
                axis_aligned_px_per_em(&identity_tm(), 1.0, pt as f32, 1.0)
                    .unwrap()
                    .0
            })
            .collect();
        for w in sizes.windows(2) {
            assert!(
                w[1] >= w[0],
                "px-per-em must not decrease as font_size grows"
            );
        }
    }

    #[test]
    fn px_per_em_none_for_vanishing_size() {
        assert!(axis_aligned_px_per_em(&identity_tm(), 1.0, 0.0, 2.0).is_none());
    }
}
