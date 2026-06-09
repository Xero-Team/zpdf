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
use crate::transform::quantize_premul;

/// Run-level constants for the glyph-space -> device-pixel transform.
struct GlyphXform {
    upem: f32,
    font_size: f32,
    scale: f32,
    page_height: f32,
    /// `tm.d < 0 || (tm.d == 0 && tm.b != 0)` — the CTM already maps Y downward.
    ctm_flips_y: bool,
}

/// Render one glyph run into the recorder. Dispatch guards mirror the CPU exactly:
/// every early-return here corresponds to a CPU no-op, so the GPU emits the same
/// set of glyphs (a missing guard would add/drop pixels and fail `compare`).
pub fn render_glyph_run(
    rec: &mut PageRecorder,
    fonts: &FontCache,
    scale: f32,
    page_height: f32,
    run: &GlyphRun,
) {
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
    let ctm_flips_y = tm.d < 0.0 || (tm.d == 0.0 && tm.b != 0.0);

    if font.is_type3() {
        render_type3(rec, font, run, color, scale, page_height, ctm_flips_y);
        return;
    }

    let x = GlyphXform {
        upem: font.units_per_em as f32,
        font_size: run.font_size,
        scale,
        page_height,
        ctm_flips_y,
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

/// Reproduce `CpuRenderer::outline_to_pixel`: font units -> user space (f32) ->
/// page space (f64 via the matrix) -> device pixels (f32), with the conditional
/// Y flip. The f32->f64->f32 precision order is parity-critical.
fn outline_to_pixel(gx: f64, gy: f64, glyph_x: f32, tm: &Matrix, x: &GlyphXform) -> lyon::math::Point {
    let tx = (gx as f32 / x.upem * x.font_size + glyph_x) as f64;
    let ty = (gy as f32 / x.upem * x.font_size) as f64;
    let page_x = tm.a * tx + tm.c * ty + tm.e;
    let page_y = tm.b * tx + tm.d * ty + tm.f;
    let px = page_x as f32 * x.scale;
    let py = if x.ctm_flips_y {
        page_y as f32 * x.scale
    } else {
        (x.page_height - page_y as f32) * x.scale
    };
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
    scale: f32,
    page_height: f32,
    ctm_flips_y: bool,
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
                    if let Some(p) = build_type3_path(
                        path,
                        &font_matrix,
                        font_size,
                        tm,
                        g.x,
                        scale,
                        page_height,
                        ctm_flips_y,
                    ) {
                        if let Some(mesh) = fill_lyon_path(&p, *rule, color) {
                            rec.add_mesh(mesh);
                        }
                    }
                }
                RenderCommand::StrokePath { path, style, .. } => {
                    // Width matches the CPU: scaled by the FontMatrix x-scale and
                    // font size, with a 0.5px floor.
                    let width = (style.width
                        * font_matrix[0].abs() as f32
                        * font_size
                        * scale)
                        .max(0.5);
                    if let Some(p) = build_type3_path(
                        path,
                        &font_matrix,
                        font_size,
                        tm,
                        g.x,
                        scale,
                        page_height,
                        ctm_flips_y,
                    ) {
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
    tm: &Matrix,
    glyph_x: f32,
    scale: f32,
    page_height: f32,
    ctm_flips_y: bool,
) -> lyon::math::Point {
    let tx = font_matrix[0] * gx + font_matrix[2] * gy + font_matrix[4];
    let ty = font_matrix[1] * gx + font_matrix[3] * gy + font_matrix[5];
    let tx = tx * font_size as f64 + glyph_x as f64;
    let ty = ty * font_size as f64;
    let page_x = tm.a * tx + tm.c * ty + tm.e;
    let page_y = tm.b * tx + tm.d * ty + tm.f;
    let px = page_x as f32 * scale;
    let py = if ctm_flips_y {
        page_y as f32 * scale
    } else {
        (page_height - page_y as f32) * scale
    };
    point(px, py)
}

#[allow(clippy::too_many_arguments)]
fn build_type3_path(
    path: &DlPath,
    font_matrix: &[f64; 6],
    font_size: f32,
    tm: &Matrix,
    glyph_x: f32,
    scale: f32,
    page_height: f32,
    ctm_flips_y: bool,
) -> Option<LyonPath> {
    if path.elements.is_empty() {
        return None;
    }
    let p = |pt: zpdf_core::Point| {
        type3_to_pixel(
            pt.x, pt.y, font_matrix, font_size, tm, glyph_x, scale, page_height, ctm_flips_y,
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

    #[test]
    fn outline_to_pixel_matches_cpu_formula_no_flip() {
        // upem 1000, font_size 10, scale 2, page_height 100, identity+translate CTM.
        let x = GlyphXform {
            upem: 1000.0,
            font_size: 10.0,
            scale: 2.0,
            page_height: 100.0,
            ctm_flips_y: false,
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
    fn outline_to_pixel_flipped_ctm_skips_page_flip() {
        let x = GlyphXform {
            upem: 1000.0,
            font_size: 10.0,
            scale: 2.0,
            page_height: 100.0,
            ctm_flips_y: true,
        };
        // d < 0: py uses page_y*scale directly (no page_height flip).
        let tm = Matrix {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: -1.0,
            e: 50.0,
            f: 50.0,
        };
        // ty = 2; page_y = -1*2 + 50 = 48; py = 48*2 = 96.
        let p = outline_to_pixel(500.0, 200.0, 0.0, &tm, &x);
        assert_eq!(p.x, 110.0);
        assert_eq!(p.y, 96.0);
    }
}
