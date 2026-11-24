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

use zpdf_core::Matrix;
use zpdf_display_list::{FillRule, GlyphRun, Paint, Path as DlPath, PathElement, RenderCommand};
use zpdf_font::{FontCache, GlyphOutline, LoadedFont, OutlineCommand};

use crate::glyph_atlas::{self, GlyphKey};
use crate::path::{fill_lyon_path, stroke_mesh_transformed_with_min_width};
use crate::record::{PageOp, PageRecorder, Type3ImageKey};
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
    render_glyph_run_with_tm(rec, fonts, map, run, &run.transform);
}

/// Render one glyph run transformed by an outer `ctm` (e.g. a tiling cell's
/// pattern-to-page matrix) without cloning the run's glyph list.
pub fn render_glyph_run_transformed(
    rec: &mut PageRecorder,
    fonts: &FontCache,
    map: &PageMap,
    run: &GlyphRun,
    ctm: &Matrix,
) {
    if *ctm == Matrix::identity() {
        render_glyph_run_with_tm(rec, fonts, map, run, &run.transform);
    } else {
        let tm = ctm.concat(&run.transform);
        render_glyph_run_with_tm(rec, fonts, map, run, &tm);
    }
}

/// Render one glyph run's coverage as opaque white (`[1.0; 4]`) under `ctm`,
/// ignoring `run.paint` and `run.alpha`, so `Paint::Tiling` glyph runs can use
/// the glyph outlines / atlas / Type 3 shapes as an alpha coverage mask.
///
/// Simultaneously computes the conservative page-space bounding box of outline
/// glyphs during the budgeted pass (honoring `rec.can_record()` and avoiding
/// duplicate `font.glyph_outline` reparsing on warm atlas hits). Returns `None`
/// for Type 3 fonts, empty runs, or when recording stops early (callers fall
/// back to `rec.page_rect()`).
pub fn render_glyph_run_mask_and_bounds_transformed(
    rec: &mut PageRecorder,
    fonts: &FontCache,
    map: &PageMap,
    run: &GlyphRun,
    ctm: &Matrix,
) -> Option<zpdf_core::Rect> {
    let tm = if *ctm == Matrix::identity() {
        run.transform
    } else {
        ctm.concat(&run.transform)
    };
    render_glyph_run_colored(rec, fonts, map, run, &tm, [1.0, 1.0, 1.0, 1.0], 1.0, true)
}

fn render_glyph_run_with_tm(
    rec: &mut PageRecorder,
    fonts: &FontCache,
    map: &PageMap,
    run: &GlyphRun,
    tm: &Matrix,
) {
    let Paint::Solid(c) = &run.paint else {
        return;
    };
    let color = quantize_premul(c, run.alpha);
    if color[3] == 0.0 {
        return; // fully transparent — contributes nothing (CPU draws invisibly)
    }
    let image_alpha = if run.alpha.is_finite() {
        run.alpha.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let _ = render_glyph_run_colored(rec, fonts, map, run, tm, color, image_alpha, false);
}

#[allow(clippy::too_many_arguments)]
fn render_glyph_run_colored(
    rec: &mut PageRecorder,
    fonts: &FontCache,
    map: &PageMap,
    run: &GlyphRun,
    tm: &Matrix,
    color: [f32; 4],
    image_alpha: f32,
    track_bounds: bool,
) -> Option<zpdf_core::Rect> {
    let font = fonts.get(run.font_id)?;
    if !font.has_font_data() {
        return None;
    }

    if font.is_type3() {
        render_type3(
            rec,
            fonts,
            font,
            run,
            tm,
            color,
            image_alpha,
            map,
            run.h_scale,
        );
        return None;
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

    let (mut min_px, mut min_py) = (f32::INFINITY, f32::INFINITY);
    let (mut max_px, mut max_py) = (f32::NEG_INFINITY, f32::NEG_INFINITY);
    let mut touch_px = |px: f32, py: f32| {
        if track_bounds && px.is_finite() && py.is_finite() {
            min_px = min_px.min(px);
            min_py = min_py.min(py);
            max_px = max_px.max(px);
            max_py = max_py.max(py);
        }
    };

    for g in &run.glyphs {
        if !rec.can_record() {
            return None;
        }
        let atlas_key = atlas_key_size.map(|(x_millipx_per_em, y_millipx_per_em)| GlyphKey {
            font_id: run.font_id,
            glyph_id: g.glyph_id,
            x_millipx_per_em,
            y_millipx_per_em,
        });
        if let Some(entry) = atlas_key.and_then(|key| rec.glyph_atlas.lookup(key)) {
            let origin = outline_to_pixel(0.0, 0.0, (g.x, g.y), tm, &x);
            let quad =
                glyph_atlas::glyph_quad(entry, rec.glyph_atlas.size(), (origin.x, origin.y), color);
            touch_px(quad[0].pos[0], quad[0].pos[1]);
            touch_px(quad[2].pos[0], quad[2].pos[1]);
            rec.add_glyph_quad(quad);
            continue;
        }
        let Some(outline) = font.glyph_outline(g.glyph_id) else {
            continue;
        };

        if let Some(key) = atlas_key {
            if let Some(entry) = rec.glyph_atlas.get_or_rasterize(key, &outline, x.upem) {
                let origin = outline_to_pixel(0.0, 0.0, (g.x, g.y), tm, &x);
                let quad = glyph_atlas::glyph_quad(
                    entry,
                    rec.glyph_atlas.size(),
                    (origin.x, origin.y),
                    color,
                );
                touch_px(quad[0].pos[0], quad[0].pos[1]);
                touch_px(quad[2].pos[0], quad[2].pos[1]);
                rec.add_glyph_quad(quad);
                continue;
            }
        }

        if let Some(path) = build_outline_path(&outline, (g.x, g.y), tm, &x) {
            if let Some(mesh) = fill_lyon_path(&path, FillRule::NonZero, color) {
                if track_bounds {
                    for v in &mesh.vertices {
                        touch_px(v.pos[0], v.pos[1]);
                    }
                }
                rec.add_mesh(mesh);
            }
        }
    }

    if track_bounds
        && map.scale.is_finite()
        && map.scale > 0.0
        && min_px.is_finite()
        && min_py.is_finite()
        && max_px > min_px
        && max_py > min_py
    {
        let inv = 1.0 / map.scale as f64;
        let x0 = min_px as f64 * inv + map.x0 as f64 - 1.0;
        let x1 = max_px as f64 * inv + map.x0 as f64 + 1.0;
        let y0 = map.y1 as f64 - max_py as f64 * inv - 1.0;
        let y1 = map.y1 as f64 - min_py as f64 * inv + 1.0;
        if x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite() {
            return Some(zpdf_core::Rect::new(x0, y0, x1, y1));
        }
    }
    None
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
    glyph_offset: (f32, f32),
    tm: &Matrix,
    x: &GlyphXform,
) -> lyon::math::Point {
    // Th (h_scale) multiplies the glyph shape x only; the positioned x offset
    // already includes it.
    let tx = (gx as f32 / x.upem * x.font_size * x.h_scale + glyph_offset.0) as f64;
    let ty = (gy as f32 / x.upem * x.font_size + glyph_offset.1) as f64;
    let page_x = tm.a * tx + tm.c * ty + tm.e;
    let page_y = tm.b * tx + tm.d * ty + tm.f;
    let px = (page_x as f32 - x.map.x0) * x.map.scale;
    let py = (x.map.y1 - page_y as f32) * x.map.scale;
    point(px, py)
}

fn build_outline_path(
    outline: &GlyphOutline,
    glyph_offset: (f32, f32),
    tm: &Matrix,
    x: &GlyphXform,
) -> Option<LyonPath> {
    if outline.commands.is_empty() {
        return None;
    }
    let p = |gx: f64, gy: f64| outline_to_pixel(gx, gy, glyph_offset, tm, x);
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

fn type3_glyph_to_page(
    tm: &Matrix,
    glyph_offset: (f32, f32),
    font_size: f32,
    h_scale: f32,
    font_matrix: &[f64; 6],
) -> Matrix {
    tm.concat(&Matrix::translate(
        glyph_offset.0 as f64,
        glyph_offset.1 as f64,
    ))
    .concat(&Matrix::scale(
        font_size as f64 * h_scale as f64,
        font_size as f64,
    ))
    .concat(&Matrix::new(
        font_matrix[0],
        font_matrix[1],
        font_matrix[2],
        font_matrix[3],
        font_matrix[4],
        font_matrix[5],
    ))
}

fn type3_placement_bounds_key(r: Option<zpdf_core::Rect>) -> [u64; 4] {
    let bits = |v: f64| {
        if v == 0.0 {
            0.0f64.to_bits()
        } else {
            v.to_bits()
        }
    };
    match r {
        Some(r) => [bits(r.x0), bits(r.y0), bits(r.x1), bits(r.y1)],
        None => [u64::MAX; 4],
    }
}

fn scale_premul_color(color: [f32; 4], alpha: f32) -> [f32; 4] {
    if alpha >= 1.0 {
        color
    } else if alpha <= 0.0 {
        [0.0; 4]
    } else {
        [
            color[0] * alpha,
            color[1] * alpha,
            color[2] * alpha,
            color[3] * alpha,
        ]
    }
}

/// Render Type3 glyphs: each glyph is a content stream. Interpret it to a
/// sub-display-list (same glyph_rect as the CPU), routing fills, strokes,
/// glyph-local clips, and inline image masks/RGB images transformed by
/// FontMatrix. Mirrors `CpuRenderer::render_type3_glyphs`.
#[allow(clippy::too_many_arguments)]
fn render_type3(
    rec: &mut PageRecorder,
    fonts: &FontCache,
    font: &LoadedFont,
    run: &GlyphRun,
    tm: &Matrix,
    color: [f32; 4],
    image_alpha: f32,
    map: &PageMap,
    h_scale: f32,
) {
    let font_size = run.font_size;

    for g in &run.glyphs {
        if !rec.can_record() {
            return;
        }
        let Some((stream, font_matrix)) = font.type3_glyph_stream(g.glyph_id) else {
            continue;
        };
        let glyph_to_page = type3_glyph_to_page(tm, (g.x, g.y), font_size, h_scale, &font_matrix);
        let font_bbox = font.type3_font_bbox();
        let fallback_bounds = font_bbox
            .is_none()
            .then(|| {
                zpdf_content::interpreter::ContentInterpreter::inverse_map_rect(
                    map.page_rect,
                    &glyph_to_page,
                )
            })
            .flatten();
        let Some((glyph_dl, glyph_images, used_dest_bounds)) = rec
            .interpret_type3_stream_with_bbox(
                stream,
                font.type3_resources(),
                font_bbox,
                fallback_bounds,
            )
        else {
            return;
        };
        let placement_bounds =
            used_dest_bounds.then(|| type3_placement_bounds_key(fallback_bounds));
        let mut glyph_clip_depth = 0usize;
        let mut glyph_blend_depth = 0usize;

        for cmd in &glyph_dl.commands {
            if !rec.can_record() {
                break;
            }
            match cmd {
                RenderCommand::PushClip { path, rule } => {
                    rec.push_clip_transformed(path, *rule, &glyph_to_page, map);
                    glyph_clip_depth += 1;
                }
                RenderCommand::PushClipStroke { path, style } => {
                    rec.push_clip_stroke_transformed(path, style, &glyph_to_page, map);
                    glyph_clip_depth += 1;
                }
                RenderCommand::PopClip => {
                    if glyph_clip_depth > 0 {
                        rec.pop_clip();
                        glyph_clip_depth -= 1;
                    }
                }
                RenderCommand::PushBlendGroup {
                    blend_mode,
                    isolated,
                    knockout,
                    alpha,
                    mask,
                    ..
                } => {
                    let sub = if rec.can_push_blend() {
                        mask.as_ref().map(|m| {
                            crate::build_mask_ops(
                                rec,
                                m,
                                &glyph_to_page,
                                Some(fonts),
                                Some(crate::RecordImages::Type3 {
                                    font_id: run.font_id,
                                    glyph_id: g.glyph_id,
                                    images: &glyph_images,
                                    placement_bounds,
                                }),
                                map,
                                0,
                            )
                        })
                    } else {
                        None
                    };
                    if !*isolated && !*knockout {
                        if *blend_mode == zpdf_display_list::BlendMode::Normal
                            && *alpha >= 1.0
                            && sub.is_none()
                        {
                            rec.push_passthrough_blend();
                        } else {
                            rec.push_non_isolated_with_mode(*blend_mode, *alpha, sub);
                        }
                    } else {
                        rec.push_blend(*blend_mode, *alpha, sub);
                    }
                    glyph_blend_depth += 1;
                }
                RenderCommand::PopBlendGroup => {
                    if glyph_blend_depth > 0 {
                        rec.pop_blend();
                        glyph_blend_depth -= 1;
                    }
                }
                RenderCommand::FillPath {
                    path,
                    rule,
                    paint,
                    alpha,
                    ..
                } => {
                    let cmd_alpha = if alpha.is_finite() {
                        alpha.clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    if cmd_alpha <= 0.0 {
                        continue;
                    }
                    let draw_color = match paint {
                        Paint::Solid(c) => quantize_premul(c, image_alpha * cmd_alpha),
                        Paint::Inherited => scale_premul_color(color, cmd_alpha),
                        _ => continue,
                    };
                    if draw_color[3] <= 0.0 {
                        continue;
                    }
                    if let Some(p) = build_type3_path(
                        path,
                        &font_matrix,
                        font_size,
                        h_scale,
                        tm,
                        (g.x, g.y),
                        map,
                    ) {
                        if let Some(mesh) = fill_lyon_path(&p, *rule, draw_color) {
                            rec.add_mesh(mesh);
                        }
                    }
                }
                RenderCommand::StrokePath {
                    path,
                    style,
                    paint,
                    alpha,
                    ..
                } => {
                    let cmd_alpha = if alpha.is_finite() {
                        alpha.clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    if cmd_alpha <= 0.0 {
                        continue;
                    }
                    let draw_color = match paint {
                        Paint::Solid(c) => quantize_premul(c, image_alpha * cmd_alpha),
                        Paint::Inherited => scale_premul_color(color, cmd_alpha),
                        _ => continue,
                    };
                    if draw_color[3] <= 0.0 {
                        continue;
                    }
                    if let Some(mesh) = stroke_mesh_transformed_with_min_width(
                        path,
                        style,
                        draw_color,
                        &glyph_to_page,
                        map,
                        0.5,
                    ) {
                        rec.add_mesh(mesh);
                    }
                }
                RenderCommand::DrawImage(draw) => {
                    let Some(src_img) = glyph_images.get(draw.image_id) else {
                        continue;
                    };
                    if !crate::image::is_valid_decoded_image(src_img) {
                        continue;
                    }
                    let img_to_page = glyph_to_page.concat(&draw.transform);
                    if draw.is_image_mask {
                        let a_scale = color[3].clamp(0.0, 1.0);
                        if a_scale <= 0.0 {
                            continue;
                        }
                        let (r_s, g_s, b_s) = (
                            (color[0] / a_scale).clamp(0.0, 1.0),
                            (color[1] / a_scale).clamp(0.0, 1.0),
                            (color[2] / a_scale).clamp(0.0, 1.0),
                        );
                        let eff_alpha = draw.alpha * a_scale;
                        if eff_alpha <= 0.0 {
                            continue;
                        }
                        let quad = crate::image::image_quad(
                            src_img.width as f32,
                            src_img.height as f32,
                            &img_to_page,
                            map,
                            eff_alpha,
                        );
                        if crate::image::is_safe_quad(&quad) {
                            let (is_dep, slot) = glyph_images.type3_image_slot(draw.image_id);
                            let key = Type3ImageKey {
                                font_id: run.font_id,
                                glyph_id: g.glyph_id,
                                image_id: slot,
                                stencil_tint: Some([r_s.to_bits(), g_s.to_bits(), b_s.to_bits()]),
                                placement_bounds: is_dep.then_some(placement_bounds).flatten(),
                            };
                            rec.add_type3_image_cached(key, quad, || {
                                let mut tinted = src_img.clone();
                                for px in tinted.data.as_chunks_mut::<4>().0 {
                                    let pa = px[3];
                                    if pa == 0 {
                                        px[0] = 0;
                                        px[1] = 0;
                                        px[2] = 0;
                                        continue;
                                    }
                                    let paf = pa as f32;
                                    px[0] = (r_s * paf).round() as u8;
                                    px[1] = (g_s * paf).round() as u8;
                                    px[2] = (b_s * paf).round() as u8;
                                }
                                tinted
                            });
                        }
                    } else {
                        let eff_alpha = draw.alpha * image_alpha;
                        if eff_alpha <= 0.0 {
                            continue;
                        }
                        let quad = crate::image::image_quad(
                            src_img.width as f32,
                            src_img.height as f32,
                            &img_to_page,
                            map,
                            eff_alpha,
                        );
                        if crate::image::is_safe_quad(&quad) {
                            let (is_dep, slot) = glyph_images.type3_image_slot(draw.image_id);
                            let key = Type3ImageKey {
                                font_id: run.font_id,
                                glyph_id: g.glyph_id,
                                image_id: slot,
                                stencil_tint: None,
                                placement_bounds: is_dep.then_some(placement_bounds).flatten(),
                            };
                            rec.add_type3_image_cached(key, quad, || src_img.clone());
                        }
                    }
                }
                _ => {}
            }
        }
        while glyph_blend_depth > 0 {
            rec.pop_blend();
            glyph_blend_depth -= 1;
        }
        while glyph_clip_depth > 0 {
            rec.pop_clip();
            glyph_clip_depth -= 1;
        }
    }
}

/// Record a Type 3 glyph run whose active paint is `Paint::Tiling`, keeping
/// explicit (`d0`) colored vectors and non-mask images in their own colors
/// while batching inherited fills, strokes, and `/ImageMask true` stencils into
/// alpha coverage masks passed to `paint_masked_batch` in exact painter's order,
/// preserving glyph-local clips.
#[allow(clippy::too_many_arguments)]
pub fn render_type3_tiling_transformed(
    rec: &mut PageRecorder,
    fonts: &FontCache,
    map: &PageMap,
    run: &GlyphRun,
    ctm: &Matrix,
    eff_opacity: f32,
    mut paint_masked_batch: impl FnMut(&mut PageRecorder, Vec<PageOp>, Option<zpdf_core::Rect>),
) {
    let Some(font) = fonts.get(run.font_id) else {
        return;
    };
    if !font.has_font_data() || !font.is_type3() {
        return;
    }
    let tm = if *ctm == Matrix::identity() {
        run.transform
    } else {
        ctm.concat(&run.transform)
    };
    let font_size = run.font_size;
    let h_scale = run.h_scale;
    let white_tint = Some([1.0f32.to_bits(); 3]);

    let mut mask_ops: Vec<PageOp> = Vec::new();
    let mut mask_has_draws = false;
    let (mut min_px, mut min_py) = (f32::INFINITY, f32::INFINITY);
    let (mut max_px, mut max_py) = (f32::NEG_INFINITY, f32::NEG_INFINITY);

    let finish_bounds = |min_px: f32, min_py: f32, max_px: f32, max_py: f32| {
        if map.scale.is_finite()
            && map.scale > 0.0
            && min_px.is_finite()
            && min_py.is_finite()
            && max_px > min_px
            && max_py > min_py
        {
            let inv = 1.0 / map.scale as f64;
            let x0 = min_px as f64 * inv + map.x0 as f64 - 1.0;
            let x1 = max_px as f64 * inv + map.x0 as f64 + 1.0;
            let y0 = map.y1 as f64 - max_py as f64 * inv - 1.0;
            let y1 = map.y1 as f64 - min_py as f64 * inv + 1.0;
            if x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite() {
                return Some(zpdf_core::Rect::new(x0, y0, x1, y1));
            }
        }
        None
    };

    macro_rules! flush_mask_batch {
        () => {
            if mask_has_draws {
                let bounds = finish_bounds(min_px, min_py, max_px, max_py);
                paint_masked_batch(rec, std::mem::take(&mut mask_ops), bounds);
                mask_has_draws = false;
                min_px = f32::INFINITY;
                min_py = f32::INFINITY;
                max_px = f32::NEG_INFINITY;
                max_py = f32::NEG_INFINITY;
            } else {
                mask_ops.clear();
            }
        };
    }

    for g in &run.glyphs {
        if !rec.can_record() {
            return;
        }
        let Some((stream, font_matrix)) = font.type3_glyph_stream(g.glyph_id) else {
            continue;
        };
        let glyph_to_page = type3_glyph_to_page(&tm, (g.x, g.y), font_size, h_scale, &font_matrix);
        let font_bbox = font.type3_font_bbox();
        let fallback_bounds = font_bbox
            .is_none()
            .then(|| {
                zpdf_content::interpreter::ContentInterpreter::inverse_map_rect(
                    map.page_rect,
                    &glyph_to_page,
                )
            })
            .flatten();
        let Some((glyph_dl, glyph_images, used_dest_bounds)) = rec
            .interpret_type3_stream_with_bbox(
                stream,
                font.type3_resources(),
                font_bbox,
                fallback_bounds,
            )
        else {
            return;
        };
        let placement_bounds =
            used_dest_bounds.then(|| type3_placement_bounds_key(fallback_bounds));
        let mut glyph_clip_depth = 0usize;
        let mut glyph_blend_depth = 0usize;

        for cmd in &glyph_dl.commands {
            if !rec.can_record() {
                break;
            }
            match cmd {
                RenderCommand::PushClip { path, rule } => {
                    let depth_before = rec.clip_depth();
                    rec.push_clip_transformed(path, *rule, &glyph_to_page, map);
                    if rec.clip_depth() > depth_before {
                        rec.mirror_top_clip_into(&mut mask_ops);
                    }
                    glyph_clip_depth += 1;
                }
                RenderCommand::PushClipStroke { path, style } => {
                    let depth_before = rec.clip_depth();
                    rec.push_clip_stroke_transformed(path, style, &glyph_to_page, map);
                    if rec.clip_depth() > depth_before {
                        rec.mirror_top_clip_into(&mut mask_ops);
                    }
                    glyph_clip_depth += 1;
                }
                RenderCommand::PopClip => {
                    if glyph_clip_depth > 0 {
                        let depth_before = rec.clip_depth();
                        rec.pop_clip();
                        if rec.clip_depth() != depth_before {
                            rec.mirror_clip_rebuild_into(&mut mask_ops);
                        }
                        glyph_clip_depth -= 1;
                    }
                }
                RenderCommand::PushBlendGroup {
                    blend_mode,
                    isolated,
                    knockout,
                    alpha,
                    mask,
                    ..
                } => {
                    flush_mask_batch!();
                    if !rec.can_record() {
                        break;
                    }
                    let sub = if rec.can_push_blend() {
                        mask.as_ref().map(|m| {
                            crate::build_mask_ops(
                                rec,
                                m,
                                &glyph_to_page,
                                Some(fonts),
                                Some(crate::RecordImages::Type3 {
                                    font_id: run.font_id,
                                    glyph_id: g.glyph_id,
                                    images: &glyph_images,
                                    placement_bounds,
                                }),
                                map,
                                0,
                            )
                        })
                    } else {
                        None
                    };
                    if !*isolated && !*knockout {
                        if *blend_mode == zpdf_display_list::BlendMode::Normal
                            && *alpha >= 1.0
                            && sub.is_none()
                        {
                            rec.push_passthrough_blend();
                        } else {
                            rec.push_non_isolated_with_mode(*blend_mode, *alpha, sub);
                        }
                    } else {
                        rec.push_blend(*blend_mode, *alpha, sub);
                    }
                    glyph_blend_depth += 1;
                }
                RenderCommand::PopBlendGroup => {
                    flush_mask_batch!();
                    if glyph_blend_depth > 0 {
                        rec.pop_blend();
                        glyph_blend_depth -= 1;
                    }
                }
                RenderCommand::FillPath {
                    path,
                    rule,
                    paint,
                    alpha,
                    ..
                } => {
                    let cmd_alpha = if alpha.is_finite() {
                        alpha.clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    if cmd_alpha <= 0.0 {
                        continue;
                    }
                    match paint {
                        Paint::Solid(c) => {
                            flush_mask_batch!();
                            if !rec.can_record() {
                                break;
                            }
                            let draw_color = quantize_premul(c, eff_opacity * cmd_alpha);
                            if draw_color[3] <= 0.0 {
                                continue;
                            }
                            if let Some(p) = build_type3_path(
                                path,
                                &font_matrix,
                                font_size,
                                h_scale,
                                &tm,
                                (g.x, g.y),
                                map,
                            ) {
                                if let Some(mesh) = fill_lyon_path(&p, *rule, draw_color) {
                                    rec.add_mesh(mesh);
                                }
                            }
                        }
                        Paint::Inherited => {
                            let mask_color = [cmd_alpha; 4];
                            if let Some(p) = build_type3_path(
                                path,
                                &font_matrix,
                                font_size,
                                h_scale,
                                &tm,
                                (g.x, g.y),
                                map,
                            ) {
                                if let Some(mesh) = fill_lyon_path(&p, *rule, mask_color) {
                                    for v in &mesh.vertices {
                                        if v.pos[0].is_finite() && v.pos[1].is_finite() {
                                            min_px = min_px.min(v.pos[0]);
                                            min_py = min_py.min(v.pos[1]);
                                            max_px = max_px.max(v.pos[0]);
                                            max_py = max_py.max(v.pos[1]);
                                        }
                                    }
                                    if rec.record_subops_clipped_into(&mut mask_ops, |r| {
                                        r.add_mesh(mesh)
                                    }) {
                                        mask_has_draws = true;
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                RenderCommand::StrokePath {
                    path,
                    style,
                    paint,
                    alpha,
                    ..
                } => {
                    let cmd_alpha = if alpha.is_finite() {
                        alpha.clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    if cmd_alpha <= 0.0 {
                        continue;
                    }
                    match paint {
                        Paint::Solid(c) => {
                            flush_mask_batch!();
                            if !rec.can_record() {
                                break;
                            }
                            let draw_color = quantize_premul(c, eff_opacity * cmd_alpha);
                            if draw_color[3] <= 0.0 {
                                continue;
                            }
                            if let Some(mesh) = stroke_mesh_transformed_with_min_width(
                                path,
                                style,
                                draw_color,
                                &glyph_to_page,
                                map,
                                0.5,
                            ) {
                                rec.add_mesh(mesh);
                            }
                        }
                        Paint::Inherited => {
                            let mask_color = [cmd_alpha; 4];
                            if let Some(mesh) = stroke_mesh_transformed_with_min_width(
                                path,
                                style,
                                mask_color,
                                &glyph_to_page,
                                map,
                                0.5,
                            ) {
                                for v in &mesh.vertices {
                                    if v.pos[0].is_finite() && v.pos[1].is_finite() {
                                        min_px = min_px.min(v.pos[0]);
                                        min_py = min_py.min(v.pos[1]);
                                        max_px = max_px.max(v.pos[0]);
                                        max_py = max_py.max(v.pos[1]);
                                    }
                                }
                                if rec
                                    .record_subops_clipped_into(&mut mask_ops, |r| r.add_mesh(mesh))
                                {
                                    mask_has_draws = true;
                                }
                            }
                        }
                        _ => {}
                    }
                }
                RenderCommand::DrawImage(draw) => {
                    let Some(src_img) = glyph_images.get(draw.image_id) else {
                        continue;
                    };
                    if !crate::image::is_valid_decoded_image(src_img) {
                        continue;
                    }
                    let img_to_page = glyph_to_page.concat(&draw.transform);

                    if draw.is_image_mask {
                        let eff_alpha = if draw.alpha.is_finite() {
                            draw.alpha.clamp(0.0, 1.0)
                        } else {
                            0.0
                        };
                        if eff_alpha <= 0.0 {
                            continue;
                        }
                        let quad = crate::image::image_quad(
                            src_img.width as f32,
                            src_img.height as f32,
                            &img_to_page,
                            map,
                            eff_alpha,
                        );
                        if crate::image::is_safe_quad(&quad) {
                            for v in &quad {
                                min_px = min_px.min(v.pos[0]);
                                min_py = min_py.min(v.pos[1]);
                                max_px = max_px.max(v.pos[0]);
                                max_py = max_py.max(v.pos[1]);
                            }
                            let (is_dep, slot) = glyph_images.type3_image_slot(draw.image_id);
                            let key = Type3ImageKey {
                                font_id: run.font_id,
                                glyph_id: g.glyph_id,
                                image_id: slot,
                                stencil_tint: white_tint,
                                placement_bounds: is_dep.then_some(placement_bounds).flatten(),
                            };
                            if rec.record_subops_clipped_into(&mut mask_ops, |r| {
                                r.add_type3_image_cached(key, quad, || {
                                    let mut tinted = src_img.clone();
                                    for px in tinted.data.as_chunks_mut::<4>().0 {
                                        let pa = px[3];
                                        px[0] = pa;
                                        px[1] = pa;
                                        px[2] = pa;
                                    }
                                    tinted
                                });
                            }) {
                                mask_has_draws = true;
                            }
                        }
                    } else {
                        // Intrinsic colored (`d0`) image: flush any pending
                        // inherited-paint mask batch first to keep painter's
                        // order, then draw the RGB image directly.
                        flush_mask_batch!();
                        if !rec.can_record() {
                            break;
                        }
                        let eff_alpha = draw.alpha * eff_opacity;
                        if !(eff_alpha.is_finite() && eff_alpha > 0.0) {
                            continue;
                        }
                        let quad = crate::image::image_quad(
                            src_img.width as f32,
                            src_img.height as f32,
                            &img_to_page,
                            map,
                            eff_alpha,
                        );
                        if crate::image::is_safe_quad(&quad) {
                            let (is_dep, slot) = glyph_images.type3_image_slot(draw.image_id);
                            let key = Type3ImageKey {
                                font_id: run.font_id,
                                glyph_id: g.glyph_id,
                                image_id: slot,
                                stencil_tint: None,
                                placement_bounds: is_dep.then_some(placement_bounds).flatten(),
                            };
                            rec.add_type3_image_cached(key, quad, || src_img.clone());
                        }
                    }
                }
                _ => {}
            }
        }
        while glyph_blend_depth > 0 {
            flush_mask_batch!();
            rec.pop_blend();
            glyph_blend_depth -= 1;
        }
        while glyph_clip_depth > 0 {
            let depth_before = rec.clip_depth();
            rec.pop_clip();
            if rec.clip_depth() != depth_before {
                rec.mirror_clip_rebuild_into(&mut mask_ops);
            }
            glyph_clip_depth -= 1;
        }
    }

    if mask_has_draws && rec.can_record() {
        let bounds = finish_bounds(min_px, min_py, max_px, max_py);
        paint_masked_batch(rec, mask_ops, bounds);
    }
}

/// Reproduce `CpuRenderer::type3_to_pixel`: glyph space -> text space (FontMatrix)
/// -> scale by font size -> add glyph offsets -> page space -> device pixels.
#[allow(clippy::too_many_arguments)]
fn type3_to_pixel(
    gx: f64,
    gy: f64,
    font_matrix: &[f64; 6],
    font_size: f32,
    h_scale: f32,
    tm: &Matrix,
    glyph_offset: (f32, f32),
    map: &PageMap,
) -> lyon::math::Point {
    let tx = font_matrix[0] * gx + font_matrix[2] * gy + font_matrix[4];
    let ty = font_matrix[1] * gx + font_matrix[3] * gy + font_matrix[5];
    let tx = tx * font_size as f64 * h_scale as f64 + glyph_offset.0 as f64;
    let ty = ty * font_size as f64 + glyph_offset.1 as f64;
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
    glyph_offset: (f32, f32),
    map: &PageMap,
) -> Option<LyonPath> {
    if path.is_empty() {
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
            glyph_offset,
            map,
        )
    };
    let mut b = LyonPath::builder();
    let mut open = false;
    for el in path {
        match el {
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
        PageMap::new(zpdf_core::Rect::new(0.0, 0.0, 1000.0, y1 as f64), scale)
    }

    #[test]
    fn type3_run_stops_at_shared_operator_budget_without_recording_partial_child() {
        use zpdf_core::ParseLimits;
        use zpdf_display_list::{Color, PositionedGlyph};
        use zpdf_font::{CidWidths, PdfFontType};

        let font = LoadedFont::new_with_data(
            PdfFontType::Type3 {
                font_matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
                char_procs: std::collections::HashMap::from([(
                    "A".into(),
                    std::sync::Arc::<[u8]>::from(&b"0 0 1 1 re f"[..]),
                )]),
                encoding: vec!["A".into()],
                widths: vec![1.0],
                first_char: 0,
            },
            "Type3BudgetTest".into(),
            Vec::new(),
            CidWidths::new(1.0),
        );
        let mut fonts = FontCache::new();
        let font_id = fonts.insert("F1".into(), font);
        let run = GlyphRun {
            font_id,
            font_size: 1.0,
            glyphs: (0..3)
                .map(|i| PositionedGlyph {
                    glyph_id: 0,
                    code_len: 0,
                    code: 0,
                    x: i as f32 * 2.0,
                    y: 0.0,
                    advance: 2.0,
                })
                .collect(),
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
            transform: Matrix::identity(),
            h_scale: 1.0,
        };
        let limits = ParseLimits {
            max_page_operators: 3,
            ..ParseLimits::default()
        };
        let mut limited = PageRecorder::default().with_limits(&limits);
        render_glyph_run(&mut limited, &fonts, &map(1.0, 10.0), &run);
        assert_eq!(
            limited.ops.len(),
            1,
            "only the complete first glyph was recorded"
        );
        assert!(limited.ensure_within_budget().is_err());

        let mut normal = PageRecorder::default().with_limits(&ParseLimits::default());
        render_glyph_run(&mut normal, &fonts, &map(1.0, 10.0), &run);
        assert_eq!(normal.ops.len(), 3);
        assert!(normal.ensure_within_budget().is_ok());
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
        let p = outline_to_pixel(500.0, 200.0, (0.0, 0.0), &tm, &x);
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
        let p = outline_to_pixel(500.0, 200.0, (0.0, 0.0), &tm, &x);
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
        let p = outline_to_pixel(500.0, 200.0, (0.0, 0.0), &tm, &x);
        assert_eq!(p.x, 90.0);
        assert_eq!(p.y, 96.0);
    }

    #[test]
    fn outline_to_pixel_honors_page_rect_origin() {
        // Page rect (20, 10)-(.., 110): device = ((page_x-20)*2, (110-page_y)*2).
        let x = GlyphXform {
            upem: 1000.0,
            font_size: 10.0,
            map: PageMap::new(zpdf_core::Rect::new(20.0, 10.0, 120.0, 110.0), 2.0),
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
        let p = outline_to_pixel(500.0, 200.0, (0.0, 0.0), &tm, &x);
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

    #[test]
    fn type3_colored_inline_image_applies_run_alpha_for_solid_and_keeps_full_alpha_for_mask() {
        use std::sync::Arc;
        use zpdf_display_list::{Color, PositionedGlyph};
        use zpdf_font::{CidWidths, PdfFontType};

        // 2×2 opaque red colored (`/CS /RGB /BPC 8`, non-mask) inline Type 3 image.
        let glyph_stream =
            b"BI /W 2 /H 2 /CS /RGB /BPC 8 ID \xff\x00\x00\xff\x00\x00\xff\x00\x00\xff\x00\x00 EI";
        let font = LoadedFont::new_with_data(
            PdfFontType::Type3 {
                font_matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
                char_procs: std::collections::HashMap::from([(
                    "A".into(),
                    Arc::<[u8]>::from(&glyph_stream[..]),
                )]),
                encoding: vec!["A".into()],
                widths: vec![1.0],
                first_char: 0,
            },
            "Type3ColoredUnit".into(),
            Vec::new(),
            CidWidths::new(1.0),
        );
        let mut fonts = FontCache::new();
        let font_id = fonts.insert("F1".into(), font);
        let map = PageMap::new(zpdf_core::Rect::new(0.0, 0.0, 40.0, 40.0), 1.0);

        let run = GlyphRun {
            font_id,
            font_size: 20.0,
            glyphs: vec![PositionedGlyph {
                glyph_id: 0,
                code_len: 1,
                code: b'A' as u32,
                x: 0.0,
                y: 0.0,
                advance: 20.0,
            }],
            paint: Paint::Solid(Color::black()),
            alpha: 0.5,
            overprint: None,
            transform: Matrix::translate(10.0, 10.0),
            h_scale: 1.0,
        };

        // 1) Solid text draw must multiply non-mask Type 3 image alpha by `run.alpha` (0.5).
        let mut solid_rec = PageRecorder::default();
        render_glyph_run(&mut solid_rec, &fonts, &map, &run);
        assert!(
            !solid_rec.tex_vertices.is_empty(),
            "colored Type 3 inline image must emit textured quad vertices"
        );
        for v in &solid_rec.tex_vertices {
            assert!(
                (v.color[3] - 0.5).abs() < 1e-5,
                "colored Type 3 inline image must carry run.alpha=0.5, got {}",
                v.color[3]
            );
        }

        // 2) Tiling coverage mask pass must keep image alpha at 1.0 so `run.alpha`
        //    is not applied twice when compositing the mask.
        let mut mask_rec = PageRecorder::default();
        let _ = render_glyph_run_mask_and_bounds_transformed(
            &mut mask_rec,
            &fonts,
            &map,
            &run,
            &Matrix::identity(),
        );
        assert!(!mask_rec.tex_vertices.is_empty());
        for v in &mask_rec.tex_vertices {
            assert!(
                (v.color[3] - 1.0).abs() < 1e-5,
                "mask pass must keep image alpha at 1.0, got {}",
                v.color[3]
            );
        }
    }
}
