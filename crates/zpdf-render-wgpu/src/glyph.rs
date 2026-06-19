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
    for g in &run.glyphs {
        let Some(outline) = font.glyph_outline(g.glyph_id) else {
            continue;
        };
        if let Some(path) = build_outline_path(&outline, g.x, tm, &x) {
            if let Some(mesh) = fill_lyon_path(&path, FillRule::NonZero, color) {
                rec.add_mesh(mesh);
            }
        }
    }
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
}
