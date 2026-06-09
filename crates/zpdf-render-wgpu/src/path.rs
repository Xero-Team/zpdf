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

/// Tessellate a stroked display-list path. Returns `None` when the device-space
/// width is <= 0 (tiny-skia produces no geometry there — skip, don't hairline-clamp).
pub fn stroke_mesh(
    path: &DlPath,
    style: &StrokeStyle,
    color: [f32; 4],
    map: &PageMap,
) -> Option<Mesh> {
    let width = style.width * map.scale;
    if width <= 0.0 {
        return None;
    }
    let lyon_path = build_lyon_path(path, map)?;
    let opts = StrokeOptions::tolerance(TOLERANCE)
        .with_line_width(width)
        .with_start_cap(map_cap(style.cap))
        .with_end_cap(map_cap(style.cap))
        .with_line_join(map_join(style.join))
        // lyon asserts miter_limit >= 1.0; PDF limits are >= 1 by spec, so this
        // only guards malformed files (documented oracle divergence).
        .with_miter_limit(style.miter_limit.max(1.0));
    if style.dash.is_some() {
        // CPU drops dashes; stroking solid keeps parity.
        tracing::debug!("dash pattern ignored (parity with CPU)");
    }
    stroke_lyon_path(&lyon_path, &opts, color)
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
    if let Err(e) =
        tess.tessellate_path(path, &opts, &mut BuffersBuilder::new(&mut mesh, SolidCtor { color }))
    {
        tracing::debug!("fill tessellation failed: {e:?}");
        return None;
    }
    (!mesh.indices.is_empty()).then_some(mesh)
}

/// Tessellate an already-built lyon path as a stroke with the given options.
pub fn stroke_lyon_path(path: &LyonPath, opts: &StrokeOptions, color: [f32; 4]) -> Option<Mesh> {
    let mut mesh = Mesh::new();
    let mut tess = StrokeTessellator::new();
    if let Err(e) =
        tess.tessellate_path(path, opts, &mut BuffersBuilder::new(&mut mesh, SolidCtor { color }))
    {
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
            page_height: 100.0,
        }
    }

    #[test]
    fn empty_path_fills_to_none() {
        assert!(fill_mesh(&DlPath::new(), FillRule::NonZero, [0.0; 4], &map()).is_none());
    }

    #[test]
    fn zero_width_stroke_is_none() {
        let mut p = DlPath::new();
        p.move_to(Point::new(0.0, 0.0));
        p.line_to(Point::new(10.0, 10.0));
        let style = StrokeStyle {
            width: 0.0,
            ..Default::default()
        };
        assert!(stroke_mesh(&p, &style, [0.0; 4], &map()).is_none());
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
