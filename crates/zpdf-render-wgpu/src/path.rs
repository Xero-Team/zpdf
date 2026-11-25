//! Path tessellation: display-list paths -> triangle meshes via lyon, in
//! device-pixel space. Mirrors the CPU oracle's fill-rule and stroke semantics.
//! The geometry arena and draw ordering live in [`crate::record`].

use lyon::path::Path as LyonPath;
use lyon::tessellation::{
    BuffersBuilder, FillOptions, FillRule as LyonFillRule, FillTessellator, FillVertex,
    FillVertexConstructor, LineCap as LyonCap, LineJoin as LyonJoin, StrokeOptions,
    StrokeTessellator, StrokeVertex, StrokeVertexConstructor, VertexBuffers,
};

use zpdf_display_list::{FillRule, LineCap, LineJoin, Path as DlPath, PathElement, StrokeStyle};

use crate::transform::{PageMap, SolidVertex};

/// Flattening tolerance in device pixels. Sub-pixel; keeps curve facets invisible.
pub const TOLERANCE: f32 = 0.1;

/// Matches the CPU backend's guard for coordinates that can overflow rasterizer
/// edge arithmetic. Real off-page/clipped content remains far inside this bound.
const MAX_DEVICE_COORD: f32 = 1_000_000.0;

/// A tessellated triangle mesh (device-pixel positions, u32 indices).
pub type Mesh = VertexBuffers<SolidVertex, u32>;

/// Tessellate a filled display-list path. `None` for empty/degenerate paths
/// (mirrors the CPU returning `None` from `build_skia_path`).
pub fn fill_mesh(path: &DlPath, rule: FillRule, color: [f32; 4], map: &PageMap) -> Option<Mesh> {
    let lyon_path = build_lyon_path(path, map)?;
    fill_lyon_path(&lyon_path, rule, color)
}

/// Tessellate a filled display-list path transformed by `ctm` into page space.
pub fn fill_mesh_transformed(
    path: &DlPath,
    rule: FillRule,
    color: [f32; 4],
    ctm: &zpdf_core::Matrix,
    map: &PageMap,
) -> Option<Mesh> {
    if *ctm == zpdf_core::Matrix::identity() {
        return fill_mesh(path, rule, color, map);
    }
    let lyon_path = build_lyon_path_transformed(path, ctm, map)?;
    fill_lyon_path(&lyon_path, rule, color)
}

/// Tessellate a stroked display-list path. The device-space width is clamped to
/// one device pixel (pdfium's hairline boost, mirrored by the CPU backend): PDF
/// zero-width strokes are 1px hairlines, and sub-pixel widths stay legible.
pub fn stroke_mesh(
    path: &DlPath,
    style: &StrokeStyle,
    color: [f32; 4],
    map: &PageMap,
) -> Option<Mesh> {
    let width = (style.width * map.scale).max(1.0);
    let mut lyon_path = build_lyon_path(path, map)?;
    if let Some(dash) = &style.dash {
        if !zpdf_render::dash::is_degenerate(&dash.array) {
            // lyon has no dashing: flatten into solid sub-segments (each gets
            // start/end caps from the stroke options, like a real dash).
            lyon_path = dash_lyon_path(&lyon_path, &dash.array, dash.phase, map.scale, TOLERANCE)?;
        }
    }
    let opts = StrokeOptions::tolerance(TOLERANCE)
        .with_line_width(width)
        .with_start_cap(map_cap(style.cap))
        .with_end_cap(map_cap(style.cap))
        .with_line_join(map_join(style.join))
        // lyon asserts miter_limit >= 1.0; PDF limits are >= 1 by spec, so this
        // only guards malformed files (documented oracle divergence).
        .with_miter_limit(style.miter_limit.max(1.0));
    stroke_lyon_path(&lyon_path, &opts, color)
}

/// Exact largest singular value of the 2×2 linear part of `ctm` scaled by
/// `scale` (the maximum stretch in any direction from pattern space to device
/// pixels).
pub fn max_linear_scale(ctm: &zpdf_core::Matrix, scale: f32) -> f32 {
    let s = scale as f64;
    let (a, b, c, d) = (ctm.a * s, ctm.b * s, ctm.c * s, ctm.d * s);
    let sum_sq = a * a + b * b + c * c + d * d;
    let det = a * d - b * c;
    let disc = (sum_sq * sum_sq - 4.0 * det * det).max(0.0).sqrt();
    (0.5 * (sum_sq + disc)).sqrt() as f32
}

/// Tessellate a stroked display-list path under an arbitrary affine `ctm`
/// (e.g. a tiling pattern's pattern-to-page matrix). Positive-width strokes
/// (and their dashes) are expanded in pattern space and the resulting outline
/// vertices are mapped through `ctm` + `map`, preserving anisotropic stroke
/// widths, elliptical round caps/joins, and directional dash scaling.
/// Zero-width hairlines are dashed in pattern space (if dashed), transformed
/// to device space, and stroked at 1 device pixel.
pub fn stroke_mesh_transformed(
    path: &DlPath,
    style: &StrokeStyle,
    color: [f32; 4],
    ctm: &zpdf_core::Matrix,
    map: &PageMap,
) -> Option<Mesh> {
    stroke_mesh_transformed_with_min_width(path, style, color, ctm, map, 1.0)
}

/// Like [`stroke_mesh_transformed`], with a configurable device-pixel minimum
/// width floor (`1.0` for regular paths, `0.5` for Type 3 glyph strokes).
pub(crate) fn stroke_mesh_transformed_with_min_width(
    path: &DlPath,
    style: &StrokeStyle,
    color: [f32; 4],
    ctm: &zpdf_core::Matrix,
    map: &PageMap,
    min_width: f32,
) -> Option<Mesh> {
    if *ctm == zpdf_core::Matrix::identity() && min_width == 1.0 {
        return stroke_mesh(path, style, color, map);
    }
    if ![ctm.a, ctm.b, ctm.c, ctm.d, ctm.e, ctm.f]
        .iter()
        .all(|v| v.is_finite())
    {
        return None;
    }
    let max_scale = max_linear_scale(ctm, map.scale);
    if !max_scale.is_finite() || max_scale <= 1e-9 {
        return None;
    }
    let pat_tol = (TOLERANCE / max_scale).clamp(1e-5, TOLERANCE);

    if !style.width.is_finite() || style.width <= 0.0 {
        // PDF zero-width hairline: `min_width` device pixels regardless of anisotropic CTM.
        let device_path = match &style.dash {
            Some(dash) if !zpdf_render::dash::is_degenerate(&dash.array) => {
                let raw = build_raw_lyon_path(path, ctm, map)?;
                let dashed = dash_lyon_path(&raw, &dash.array, dash.phase, 1.0, pat_tol)?;
                transform_lyon_path(&dashed, ctm, map)?
            }
            _ => build_lyon_path_transformed(path, ctm, map)?,
        };
        let opts = StrokeOptions::tolerance(TOLERANCE)
            .with_line_width(min_width)
            .with_start_cap(map_cap(style.cap))
            .with_end_cap(map_cap(style.cap))
            .with_line_join(map_join(style.join))
            .with_miter_limit(style.miter_limit.max(1.0));
        return stroke_lyon_path(&device_path, &opts, color);
    }

    let mut pat_path = build_raw_lyon_path(path, ctm, map)?;
    if let Some(dash) = &style.dash {
        if !zpdf_render::dash::is_degenerate(&dash.array) {
            pat_path = dash_lyon_path(&pat_path, &dash.array, dash.phase, 1.0, pat_tol)?;
        }
    }
    // Pattern/glyph-space width with a `min_width` device-pixel floor along
    // the maximally stretched axis.
    let pat_width = style.width.max(min_width / max_scale);
    let opts = StrokeOptions::tolerance(pat_tol)
        .with_line_width(pat_width)
        .with_start_cap(map_cap(style.cap))
        .with_end_cap(map_cap(style.cap))
        .with_line_join(map_join(style.join))
        .with_miter_limit(style.miter_limit.max(1.0));

    let mut mesh = Mesh::new();
    let mut tess = StrokeTessellator::new();
    if let Err(e) = tess.tessellate_path(
        &pat_path,
        &opts,
        &mut BuffersBuilder::new(&mut mesh, TransformedSolidCtor { color, ctm, map }),
    ) {
        tracing::debug!("transformed stroke tessellation failed: {e:?}");
        return None;
    }
    if mesh.indices.is_empty()
        || !mesh
            .vertices
            .iter()
            .all(|v| safe_point(lyon::math::point(v.pos[0], v.pos[1])))
    {
        return None;
    }
    Some(mesh)
}

/// Flatten a lyon path into the "on" runs of a dash pattern and rebuild it as
/// open polyline sub-paths. The dash array/phase are multiplied by `scale`
/// (`map.scale` in device space, `1.0` in pattern space). Returns `None` when
/// no run survives (e.g. a short path entirely inside an "off" interval).
fn dash_lyon_path(
    path: &LyonPath,
    array: &[f32],
    phase: f32,
    scale: f32,
    tolerance: f32,
) -> Option<LyonPath> {
    use lyon::path::iterator::PathIterator;
    use lyon::path::PathEvent;

    let scaled: Vec<f32> = array.iter().map(|v| v * scale).collect();
    let phase = phase * scale;

    // Collect flattened polylines; closed sub-paths repeat their first point so
    // the dash walks the closing edge too (the pattern restarts per sub-path).
    let mut polylines: Vec<Vec<[f32; 2]>> = Vec::new();
    let mut cur: Vec<[f32; 2]> = Vec::new();
    for ev in path.iter().flattened(tolerance) {
        match ev {
            PathEvent::Begin { at } => {
                cur.clear();
                cur.push([at.x, at.y]);
            }
            PathEvent::Line { to, .. } => cur.push([to.x, to.y]),
            PathEvent::End { first, close, .. } => {
                if close {
                    cur.push([first.x, first.y]);
                }
                if cur.len() >= 2 {
                    polylines.push(std::mem::take(&mut cur));
                } else {
                    cur.clear();
                }
            }
            _ => {}
        }
    }

    let mut b = LyonPath::builder();
    let mut any = false;
    for polyline in &polylines {
        for run in zpdf_render::dash::dash_polyline(polyline, &scaled, phase) {
            // Skip zero-length runs (dots): lyon emits no geometry for them.
            if run.len() < 2 || run.iter().all(|p| *p == run[0]) {
                continue;
            }
            b.begin(lyon::math::point(run[0][0], run[0][1]));
            for p in &run[1..] {
                b.line_to(lyon::math::point(p[0], p[1]));
            }
            b.end(false);
            any = true;
        }
    }
    any.then(|| b.build())
}

/// Tessellate an already-built lyon path as a fill. Shared by display-list fills,
/// glyph outlines, and Type3 glyph paths (which build their own device-pixel paths).
pub fn fill_lyon_path(path: &LyonPath, rule: FillRule, color: [f32; 4]) -> Option<Mesh> {
    if !path_is_safe(path) {
        tracing::warn!("skipping non-finite or out-of-range GPU path");
        return None;
    }
    let opts = FillOptions::tolerance(TOLERANCE).with_fill_rule(match rule {
        FillRule::NonZero => LyonFillRule::NonZero,
        FillRule::EvenOdd => LyonFillRule::EvenOdd,
    });
    let mut mesh = Mesh::new();
    let mut tess = FillTessellator::new();
    if let Err(e) = tess.tessellate_path(
        path,
        &opts,
        &mut BuffersBuilder::new(&mut mesh, SolidCtor { color }),
    ) {
        tracing::debug!("fill tessellation failed: {e:?}");
        return None;
    }
    (!mesh.indices.is_empty()).then_some(mesh)
}

/// Tessellate an already-built lyon path as a stroke with the given options.
pub fn stroke_lyon_path(path: &LyonPath, opts: &StrokeOptions, color: [f32; 4]) -> Option<Mesh> {
    if !path_is_safe(path) {
        tracing::warn!("skipping non-finite or out-of-range GPU path");
        return None;
    }
    let mut mesh = Mesh::new();
    let mut tess = StrokeTessellator::new();
    if let Err(e) = tess.tessellate_path(
        path,
        opts,
        &mut BuffersBuilder::new(&mut mesh, SolidCtor { color }),
    ) {
        tracing::debug!("stroke tessellation failed: {e:?}");
        return None;
    }
    (!mesh.indices.is_empty()).then_some(mesh)
}

/// Build a lyon path from display-list elements, mapping each point to device
/// pixels. Balances sub-paths (lyon panics on unbalanced begin/end).
fn build_lyon_path(path: &DlPath, map: &PageMap) -> Option<LyonPath> {
    if path.is_empty() {
        return None;
    }
    let mut b = LyonPath::builder();
    let mut open = false;
    let map_point = |p| {
        let p = map.pt(p);
        safe_point(p).then_some(p)
    };
    for el in path {
        match el {
            PathElement::MoveTo(p) => {
                if open {
                    b.end(false);
                }
                b.begin(map_point(p)?);
                open = true;
            }
            PathElement::LineTo(p) => {
                if !open {
                    b.begin(map_point(p)?);
                    open = true;
                } else {
                    b.line_to(map_point(p)?);
                }
            }
            PathElement::CurveTo(c1, c2, e) => {
                if !open {
                    b.begin(map_point(c1)?);
                    open = true;
                }
                b.cubic_bezier_to(map_point(c1)?, map_point(c2)?, map_point(e)?);
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

/// Build a device-space lyon path by mapping each display-list point through
/// `ctm` (pattern/local space -> page space) and then `map.pt`.
fn build_lyon_path_transformed(
    path: &DlPath,
    ctm: &zpdf_core::Matrix,
    map: &PageMap,
) -> Option<LyonPath> {
    if path.is_empty() {
        return None;
    }
    let mut b = LyonPath::builder();
    let mut open = false;
    let map_point = |p: zpdf_core::Point| {
        let dev = map.pt(p.transform(ctm));
        safe_point(dev).then_some(dev)
    };
    for el in path {
        match el {
            PathElement::MoveTo(p) => {
                if open {
                    b.end(false);
                }
                b.begin(map_point(p)?);
                open = true;
            }
            PathElement::LineTo(p) => {
                if !open {
                    b.begin(map_point(p)?);
                    open = true;
                } else {
                    b.line_to(map_point(p)?);
                }
            }
            PathElement::CurveTo(c1, c2, e) => {
                if !open {
                    b.begin(map_point(c1)?);
                    open = true;
                }
                b.cubic_bezier_to(map_point(c1)?, map_point(c2)?, map_point(e)?);
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

/// Build a lyon path directly in pattern/local coordinates after checking that
/// every input point is finite and maps to a safe device coordinate under `ctm`.
fn build_raw_lyon_path(path: &DlPath, ctm: &zpdf_core::Matrix, map: &PageMap) -> Option<LyonPath> {
    if path.is_empty() {
        return None;
    }
    let mut b = LyonPath::builder();
    let mut open = false;
    let check_point = |p: zpdf_core::Point| {
        if !p.x.is_finite() || !p.y.is_finite() {
            return None;
        }
        let dev = map.pt(p.transform(ctm));
        safe_point(dev).then_some(lyon::math::point(p.x as f32, p.y as f32))
    };
    for el in path {
        match el {
            PathElement::MoveTo(p) => {
                if open {
                    b.end(false);
                }
                b.begin(check_point(p)?);
                open = true;
            }
            PathElement::LineTo(p) => {
                if !open {
                    b.begin(check_point(p)?);
                    open = true;
                } else {
                    b.line_to(check_point(p)?);
                }
            }
            PathElement::CurveTo(c1, c2, e) => {
                if !open {
                    b.begin(check_point(c1)?);
                    open = true;
                }
                b.cubic_bezier_to(check_point(c1)?, check_point(c2)?, check_point(e)?);
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

/// Map a pattern-space lyon path (typically open polyline runs produced by
/// `dash_lyon_path`) into device pixels via `ctm` + `map.pt`.
fn transform_lyon_path(
    path: &LyonPath,
    ctm: &zpdf_core::Matrix,
    map: &PageMap,
) -> Option<LyonPath> {
    use lyon::path::PathEvent;
    let map_pt = |p: lyon::math::Point| {
        let dev = map.pt(zpdf_core::Point::new(p.x as f64, p.y as f64).transform(ctm));
        safe_point(dev).then_some(dev)
    };
    let mut b = LyonPath::builder();
    let mut open = false;
    for ev in path.iter() {
        match ev {
            PathEvent::Begin { at } => {
                if open {
                    b.end(false);
                }
                b.begin(map_pt(at)?);
                open = true;
            }
            PathEvent::Line { to, .. } => {
                if open {
                    b.line_to(map_pt(to)?);
                }
            }
            PathEvent::Quadratic { ctrl, to, .. } => {
                if open {
                    b.quadratic_bezier_to(map_pt(ctrl)?, map_pt(to)?);
                }
            }
            PathEvent::Cubic {
                ctrl1, ctrl2, to, ..
            } => {
                if open {
                    b.cubic_bezier_to(map_pt(ctrl1)?, map_pt(ctrl2)?, map_pt(to)?);
                }
            }
            PathEvent::End { close, .. } => {
                if open {
                    b.end(close);
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

fn safe_point(p: lyon::math::Point) -> bool {
    p.x.is_finite()
        && p.y.is_finite()
        && p.x.abs() <= MAX_DEVICE_COORD
        && p.y.abs() <= MAX_DEVICE_COORD
}

fn path_is_safe(path: &LyonPath) -> bool {
    use lyon::path::PathEvent;
    path.iter().all(|event| match event {
        PathEvent::Begin { at } => safe_point(at),
        PathEvent::Line { from, to } => safe_point(from) && safe_point(to),
        PathEvent::Quadratic { from, ctrl, to } => {
            safe_point(from) && safe_point(ctrl) && safe_point(to)
        }
        PathEvent::Cubic {
            from,
            ctrl1,
            ctrl2,
            to,
        } => safe_point(from) && safe_point(ctrl1) && safe_point(ctrl2) && safe_point(to),
        PathEvent::End { last, first, .. } => safe_point(last) && safe_point(first),
    })
}

fn map_cap(c: LineCap) -> LyonCap {
    match c {
        LineCap::Butt => LyonCap::Butt,
        LineCap::Round => LyonCap::Round,
        LineCap::Square => LyonCap::Square,
    }
}

fn map_join(j: LineJoin) -> LyonJoin {
    match j {
        LineJoin::Miter => LyonJoin::Miter,
        LineJoin::Round => LyonJoin::Round,
        LineJoin::Bevel => LyonJoin::Bevel,
    }
}

/// Vertex constructor that stamps the (already premultiplied) draw color onto
/// every tessellated vertex. Clip tessellation passes a dummy color (unused).
struct SolidCtor {
    color: [f32; 4],
}

impl FillVertexConstructor<SolidVertex> for SolidCtor {
    fn new_vertex(&mut self, vertex: FillVertex) -> SolidVertex {
        let p = vertex.position();
        SolidVertex {
            pos: [p.x, p.y],
            color: self.color,
        }
    }
}

impl StrokeVertexConstructor<SolidVertex> for SolidCtor {
    fn new_vertex(&mut self, vertex: StrokeVertex) -> SolidVertex {
        let p = vertex.position();
        SolidVertex {
            pos: [p.x, p.y],
            color: self.color,
        }
    }
}

/// Vertex constructor that maps pattern-space stroked outline vertices through
/// an arbitrary affine `ctm` and page map into device pixels.
struct TransformedSolidCtor<'a> {
    color: [f32; 4],
    ctm: &'a zpdf_core::Matrix,
    map: &'a PageMap,
}

impl StrokeVertexConstructor<SolidVertex> for TransformedSolidCtor<'_> {
    fn new_vertex(&mut self, vertex: StrokeVertex) -> SolidVertex {
        let p = vertex.position();
        let dev = self
            .map
            .pt(zpdf_core::Point::new(p.x as f64, p.y as f64).transform(self.ctm));
        SolidVertex {
            pos: [dev.x, dev.y],
            color: self.color,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zpdf_core::Point;
    use zpdf_display_list::Path as DlPath;

    fn map() -> PageMap {
        PageMap::new(zpdf_core::Rect::new(0.0, 0.0, 100.0, 100.0), 1.0)
    }

    #[test]
    fn empty_path_fills_to_none() {
        assert!(fill_mesh(&DlPath::new(), FillRule::NonZero, [0.0; 4], &map()).is_none());
    }

    #[test]
    fn zero_width_stroke_renders_hairline() {
        // PDF zero-width strokes are 1-device-pixel hairlines (pdfium semantics,
        // mirrored by the CPU backend's 1px clamp) — geometry MUST be produced.
        let mut p = DlPath::new();
        p.move_to(Point::new(0.0, 0.0));
        p.line_to(Point::new(10.0, 10.0));
        let style = StrokeStyle {
            width: 0.0,
            ..Default::default()
        };
        let mesh = stroke_mesh(&p, &style, [0.0; 4], &map()).expect("hairline geometry");
        assert!(!mesh.indices.is_empty());
        // The quad spans ~1px across the line: vertex extents reflect width 1.
        let (min_x, max_x) = mesh
            .vertices
            .iter()
            .fold((f32::MAX, f32::MIN), |(lo, hi), v| {
                (lo.min(v.pos[0]), hi.max(v.pos[0]))
            });
        assert!(
            max_x - min_x <= 12.0,
            "hairline must not be wider than ~1px"
        );
    }

    #[test]
    fn dashed_stroke_splits_into_segments() {
        // Horizontal line of length 20 with [4 on, 4 off]: three on-runs, so the
        // mesh has vertices at the interior dash boundaries (x = 4 and x = 8).
        let mut p = DlPath::new();
        p.move_to(Point::new(0.0, 50.0));
        p.line_to(Point::new(20.0, 50.0));
        let style = StrokeStyle {
            width: 2.0,
            dash: Some(zpdf_display_list::DashPattern {
                array: vec![4.0, 4.0],
                phase: 0.0,
            }),
            ..Default::default()
        };
        let dashed = stroke_mesh(&p, &style, [0.0; 4], &map()).expect("dashed geometry");
        let solid = stroke_mesh(
            &p,
            &StrokeStyle {
                width: 2.0,
                ..Default::default()
            },
            [0.0; 4],
            &map(),
        )
        .expect("solid geometry");
        assert!(
            dashed.vertices.len() > solid.vertices.len(),
            "dash runs add segment endpoints"
        );
        let has_x = |x: f32| dashed.vertices.iter().any(|v| (v.pos[0] - x).abs() < 0.01);
        assert!(has_x(4.0) && has_x(8.0), "dash boundaries at x=4 and x=8");
        // Gap interiors carry no geometry.
        assert!(
            !dashed
                .vertices
                .iter()
                .any(|v| v.pos[0] > 4.01 && v.pos[0] < 7.99),
            "no vertices inside the off interval"
        );
    }

    #[test]
    fn degenerate_dash_strokes_solid() {
        let mut p = DlPath::new();
        p.move_to(Point::new(0.0, 50.0));
        p.line_to(Point::new(20.0, 50.0));
        let style = StrokeStyle {
            width: 2.0,
            dash: Some(zpdf_display_list::DashPattern {
                array: vec![0.0, 0.0],
                phase: 0.0,
            }),
            ..Default::default()
        };
        let dashed = stroke_mesh(&p, &style, [0.0; 4], &map()).expect("solid geometry");
        let solid = stroke_mesh(
            &p,
            &StrokeStyle {
                width: 2.0,
                ..Default::default()
            },
            [0.0; 4],
            &map(),
        )
        .expect("solid geometry");
        assert_eq!(dashed.vertices.len(), solid.vertices.len());
    }

    #[test]
    fn unclosed_subpaths_balance_and_fill() {
        // Two MoveTos without explicit Close must not panic and should tessellate.
        let mut p = DlPath::new();
        p.move_to(Point::new(10.0, 10.0));
        p.line_to(Point::new(90.0, 10.0));
        p.line_to(Point::new(50.0, 90.0));
        p.move_to(Point::new(20.0, 20.0));
        p.line_to(Point::new(40.0, 20.0));
        p.line_to(Point::new(30.0, 40.0));
        let mesh = fill_mesh(&p, FillRule::NonZero, [1.0, 0.0, 0.0, 1.0], &map());
        assert!(mesh.is_some());
        assert!(!mesh.unwrap().vertices.is_empty());
    }

    #[test]
    fn rejects_non_finite_and_extreme_coordinates() {
        let mut p = DlPath::new();
        p.move_to(Point::new(f64::NAN, 0.0));
        p.line_to(Point::new(1.0, 1.0));
        assert!(fill_mesh(&p, FillRule::NonZero, [1.0; 4], &map()).is_none());

        let mut p = DlPath::new();
        p.move_to(Point::new(0.0, 0.0));
        p.line_to(Point::new(2_000_000.0, 1.0));
        assert!(fill_mesh(&p, FillRule::NonZero, [1.0; 4], &map()).is_none());
    }

    #[test]
    fn anisotropic_transformed_stroke_preserves_directional_widths() {
        // Pattern matrix [4 0 0 1 0 0] at 72 DPI (scale=1):
        // - 1pt vertical stroke at x=5 -> x in [18, 22] (4.0 px wide)
        // - 1pt horizontal stroke at y=5 -> y in [94.5, 95.5] (1.0 px tall)
        let ctm = zpdf_core::Matrix::new(4.0, 0.0, 0.0, 1.0, 0.0, 0.0);
        let style = StrokeStyle {
            width: 1.0,
            ..Default::default()
        };

        let mut vert = DlPath::new();
        vert.move_to(Point::new(5.0, 0.0));
        vert.line_to(Point::new(5.0, 10.0));
        let vmesh = stroke_mesh_transformed(&vert, &style, [1.0, 0.0, 0.0, 1.0], &ctm, &map())
            .expect("vertical stroke mesh");
        let (vmin_x, vmax_x) = vmesh
            .vertices
            .iter()
            .fold((f32::MAX, f32::MIN), |(lo, hi), v| {
                (lo.min(v.pos[0]), hi.max(v.pos[0]))
            });
        assert!(
            ((vmax_x - vmin_x) - 4.0).abs() < 1e-3,
            "vertical stroke under [4 0 0 1] must be 4px wide, got {}",
            vmax_x - vmin_x
        );

        let mut horiz = DlPath::new();
        horiz.move_to(Point::new(0.0, 5.0));
        horiz.line_to(Point::new(10.0, 5.0));
        let hmesh = stroke_mesh_transformed(&horiz, &style, [0.0, 0.0, 1.0, 1.0], &ctm, &map())
            .expect("horizontal stroke mesh");
        let (hmin_y, hmax_y) = hmesh
            .vertices
            .iter()
            .fold((f32::MAX, f32::MIN), |(lo, hi), v| {
                (lo.min(v.pos[1]), hi.max(v.pos[1]))
            });
        assert!(
            ((hmax_y - hmin_y) - 1.0).abs() < 1e-3,
            "horizontal stroke under [4 0 0 1] must be 1px tall, got {}",
            hmax_y - hmin_y
        );
    }

    #[test]
    fn anisotropic_transformed_dashed_stroke_scales_dashes_directionally() {
        let ctm = zpdf_core::Matrix::new(4.0, 0.0, 0.0, 1.0, 0.0, 0.0);
        let style = StrokeStyle {
            width: 1.0,
            dash: Some(zpdf_display_list::DashPattern {
                array: vec![2.0, 2.0],
                phase: 0.0,
            }),
            ..Default::default()
        };
        let mut horiz = DlPath::new();
        horiz.move_to(Point::new(0.0, 5.0));
        horiz.line_to(Point::new(10.0, 5.0));
        let hmesh = stroke_mesh_transformed(&horiz, &style, [0.0; 4], &ctm, &map())
            .expect("dashed horizontal stroke");
        // Pattern dash [2 on, 2 off] scaled 4x along X -> [0..8] on, [8..16] off, [16..24] on.
        assert!(
            hmesh.vertices.iter().any(|v| (v.pos[0] - 8.0).abs() < 0.05)
                && hmesh
                    .vertices
                    .iter()
                    .any(|v| (v.pos[0] - 16.0).abs() < 0.05),
            "horizontal dash boundaries should stretch 4x to x=8 and x=16"
        );
        assert!(
            !hmesh
                .vertices
                .iter()
                .any(|v| v.pos[0] > 8.05 && v.pos[0] < 15.95),
            "off interval [8..16] must contain no vertices"
        );
    }
}
