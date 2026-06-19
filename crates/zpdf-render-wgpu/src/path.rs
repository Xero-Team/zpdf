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

/// A tessellated triangle mesh (device-pixel positions, u32 indices).
pub type Mesh = VertexBuffers<SolidVertex, u32>;

/// Tessellate a filled display-list path. `None` for empty/degenerate paths
/// (mirrors the CPU returning `None` from `build_skia_path`).
pub fn fill_mesh(path: &DlPath, rule: FillRule, color: [f32; 4], map: &PageMap) -> Option<Mesh> {
    let lyon_path = build_lyon_path(path, map)?;
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
            lyon_path = dash_lyon_path(&lyon_path, &dash.array, dash.phase, map.scale)?;
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

/// Flatten a (device-pixel) lyon path into the "on" runs of a dash pattern and
/// rebuild it as open polyline sub-paths. The dash array/phase are page-unit
/// values, scaled into device pixels here. Returns `None` when no run survives
/// (e.g. a short path entirely inside an "off" interval).
fn dash_lyon_path(path: &LyonPath, array: &[f32], phase: f32, scale: f32) -> Option<LyonPath> {
    use lyon::path::iterator::PathIterator;
    use lyon::path::PathEvent;

    let scaled: Vec<f32> = array.iter().map(|v| v * scale).collect();
    let phase = phase * scale;

    // Collect flattened polylines; closed sub-paths repeat their first point so
    // the dash walks the closing edge too (the pattern restarts per sub-path).
    let mut polylines: Vec<Vec<[f32; 2]>> = Vec::new();
    let mut cur: Vec<[f32; 2]> = Vec::new();
    for ev in path.iter().flattened(TOLERANCE) {
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
    if path.elements.is_empty() {
        return None;
    }
    let mut b = LyonPath::builder();
    let mut open = false;
    for el in &path.elements {
        match *el {
            PathElement::MoveTo(p) => {
                if open {
                    b.end(false);
                }
                b.begin(map.pt(p));
                open = true;
            }
            PathElement::LineTo(p) => {
                if !open {
                    b.begin(map.pt(p));
                    open = true;
                } else {
                    b.line_to(map.pt(p));
                }
            }
            PathElement::CurveTo(c1, c2, e) => {
                if !open {
                    b.begin(map.pt(c1));
                    open = true;
                }
                b.cubic_bezier_to(map.pt(c1), map.pt(c2), map.pt(e));
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

#[cfg(test)]
mod tests {
    use super::*;
    use zpdf_core::Point;
    use zpdf_display_list::Path as DlPath;

    fn map() -> PageMap {
        PageMap {
            scale: 1.0,
            x0: 0.0,
            y1: 100.0,
        }
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
}
