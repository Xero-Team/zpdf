//! What a page is actually made of — measured from its display list.
//!
//! **Why this module exists.** The previous round's corpus was labelled by hand,
//! and the labels were wrong: the file recorded as "image-heavy" was a 346-glyph
//! text page, and the one recorded as "text-heavy" was a two-image page with zero
//! glyphs. Every conclusion that round drew about "text-bound" and "image-bound"
//! pages inherited those errors. So composition is measured here, from the
//! display list the renderer will actually consume, and the class is derived from
//! those numbers rather than from a filename.
//!
//! ## Commensurable units
//!
//! The three families are compared in **estimated device-pixel coverage**, not in
//! counts. Counting alone is wrong in a way that matters: the corpus's `test8`
//! page has 1163 glyphs and one image, and a count-based rule calls it
//! image-bound on the strength of that single image — which is 81x61 pixels
//! (4811 device px, the trivial one). Coverage puts the glyphs at ~250k device px
//! and classifies it as text, which is what it is.
//!
//! Coverage is exact for images (the unit square's area times the transform
//! determinant), an em-box proxy for glyphs, and a bounding-box proxy for vector
//! paths — an upper bound for thin strokes. The proxies are documented at their
//! use sites; the classification only needs the families to be the same order of
//! magnitude of comparable, which they now are.
//!
//! The counts are also the deterministic half of a benchmark: integers are
//! reproducible on any machine, which makes them the only safe thing to assert in
//! CI (wall-clock thresholds in CI are a false-positive generator).

use std::path::Path;

use zpdf::display_list::{DisplayList, Paint, Path as DlPath, PathElement, RenderCommand};
use zpdf_core::Matrix;

use crate::pipeline;

/// Fraction of a glyph's em box assumed to be inked, for the coverage proxy.
/// Typical Latin/CJK text sits around 0.4–0.6 of the em box; the exact value
/// only shifts the text/vector/image balance by a constant factor, and a 2x
/// dominance margin absorbs it.
const GLYPH_EM_COVERAGE: f64 = 0.5;

/// How many work items of each kind a page asks for, plus comparable coverage
/// estimates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Composition {
    /// Total display-list commands.
    pub commands: u64,
    pub fills: u64,
    pub strokes: u64,
    /// Glyph *runs* (each carries a transform and a paint).
    pub glyph_runs: u64,
    /// Glyph *instances* — one run can carry 800 glyphs, and it is glyphs that
    /// get outlined and rasterized.
    pub glyphs: u64,
    pub images: u64,
    pub clips: u64,
    pub blend_groups: u64,

    /// Estimated device-pixel coverage of glyph work (em-box proxy).
    pub glyph_device_px: u64,
    /// Estimated device-pixel coverage of vector work (path-bbox proxy; an upper
    /// bound for thin strokes).
    pub vector_device_px: u64,
    /// Exact destination area of image draws, in device pixels (Σ |det| · scale²).
    /// On the corpus's image-bound page three images each cover a whole page —
    /// ~5.4 s of bilinear resampling — and an image *count* of three would have
    /// said nothing about it.
    pub image_device_px: u64,

    /// Fills/strokes whose paint is not a solid colour. Both render backends
    /// paint only `Paint::Solid`, so anything counted here is a paint the
    /// backends will silently drop — a fidelity bug, not a metric. Expected to
    /// stay at zero; `summary()` flags it loudly.
    pub non_solid_paints: u64,
}

impl Composition {
    /// Measure a display list at the given device scale.
    pub fn of(dl: &DisplayList, scale: f32) -> Self {
        let mut c = Self::default();
        let s2 = (scale as f64) * (scale as f64);
        for cmd in &dl.commands {
            c.commands += 1;
            match cmd {
                RenderCommand::FillPath { path, paint, .. } => {
                    c.fills += 1;
                    c.vector_device_px += path_area_device(path, s2);
                    if !matches!(paint, Paint::Solid(_)) {
                        c.non_solid_paints += 1;
                    }
                }
                RenderCommand::StrokePath { path, paint, .. } => {
                    c.strokes += 1;
                    c.vector_device_px += path_area_device(path, s2);
                    if !matches!(paint, Paint::Solid(_)) {
                        c.non_solid_paints += 1;
                    }
                }
                RenderCommand::DrawGlyphRun(run) => {
                    c.glyph_runs += 1;
                    let n = run.glyphs.len() as u64;
                    c.glyphs += n;
                    // Glyph em box: font_size is in text space, `tm` maps text to
                    // page space, `scale` page to device.
                    let tm_det = (run.transform.a * run.transform.d
                        - run.transform.b * run.transform.c)
                        .abs();
                    let em_box = (run.font_size as f64).powi(2) * tm_det * s2;
                    let per_glyph = (em_box * GLYPH_EM_COVERAGE).round().max(1.0) as u64;
                    c.glyph_device_px += n * per_glyph;
                }
                RenderCommand::DrawImage(draw) => {
                    c.images += 1;
                    c.image_device_px += matrix_area_device(&draw.transform, s2);
                }
                RenderCommand::PushClip { .. } | RenderCommand::PushClipStroke { .. } => {
                    c.clips += 1;
                }
                RenderCommand::PushBlendGroup { .. } => c.blend_groups += 1,
                RenderCommand::PopClip | RenderCommand::PopBlendGroup => {}
            }
        }
        c
    }

    /// The three families, as comparable device-pixel coverage.
    pub fn weights(&self) -> (u64, u64, u64) {
        (
            self.glyph_device_px,
            self.vector_device_px,
            self.image_device_px,
        )
    }

    /// Measured load class: whichever family dominates, or `Mixed` when none
    /// does.
    ///
    /// Dominance is "at least double the runner-up", which keeps a page with 250k
    /// px of glyphs and 30k px of vector work classified as text, while a page
    /// with 250k of each is honestly mixed.
    pub fn class(&self) -> LoadClass {
        let (text, vector, image) = self.weights();
        if text == 0 && vector == 0 && image == 0 {
            return LoadClass::Empty;
        }
        let mut ranked = [
            (LoadClass::Text, text),
            (LoadClass::Vector, vector),
            (LoadClass::Image, image),
        ];
        ranked.sort_by_key(|b| std::cmp::Reverse(b.1));
        let (top_class, top) = ranked[0];
        let second = ranked[1].1;
        if top >= second.saturating_mul(2) || second == 0 {
            top_class
        } else {
            LoadClass::Mixed
        }
    }

    /// One-line breakdown, for a report.
    pub fn summary(&self) -> String {
        let mut s = format!(
            "{} cmds: fills={} strokes={} glyphs={} ({} runs) images={} clips={} blend_groups={} | \
             coverage(device px): glyphs={} vector={} images={}",
            self.commands,
            self.fills,
            self.strokes,
            self.glyphs,
            self.glyph_runs,
            self.images,
            self.clips,
            self.blend_groups,
            self.glyph_device_px,
            self.vector_device_px,
            self.image_device_px,
        );
        if self.non_solid_paints > 0 {
            s.push_str(&format!(
                "  !! {} non-solid paints — backends paint only Paint::Solid and will drop these",
                self.non_solid_paints
            ));
        }
        s
    }
}

/// Device-pixel area of a display-list path's bounding box, times `s2` (scale²).
///
/// Path points are already in page space (the interpreter applies the CTM), so
/// only the device scale is needed. Curve control points are included, which
/// makes this a conservative over-estimate of a curve's true extent.
fn path_area_device(path: &DlPath, s2: f64) -> u64 {
    let (mut x0, mut y0) = (f64::INFINITY, f64::INFINITY);
    let (mut x1, mut y1) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
    for el in &path.elements {
        let mut visit = |p: &zpdf_core::Point| {
            if p.x.is_finite() && p.y.is_finite() {
                x0 = x0.min(p.x);
                y0 = y0.min(p.y);
                x1 = x1.max(p.x);
                y1 = y1.max(p.y);
            }
        };
        match el {
            PathElement::MoveTo(p) | PathElement::LineTo(p) => visit(p),
            PathElement::CurveTo(a, b, c) => {
                visit(a);
                visit(b);
                visit(c);
            }
            PathElement::Close => {}
        }
    }
    if !(x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite()) {
        return 0;
    }
    ((x1 - x0).max(0.0) * (y1 - y0).max(0.0) * s2).round() as u64
}

/// Device-pixel area of the unit square under `t`, times `s2`.
fn matrix_area_device(t: &Matrix, s2: f64) -> u64 {
    let det = (t.a * t.d - t.b * t.c).abs();
    (det * s2).round() as u64
}

/// Measured load class of a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadClass {
    /// Glyph coverage dominates.
    Text,
    /// Vector fill/stroke coverage dominates.
    Vector,
    /// Image destination area dominates.
    Image,
    /// No single family reaches 2x the runner-up.
    Mixed,
    /// No paint commands at all (a cover page, or a blank page).
    Empty,
}

impl LoadClass {
    /// Lowercase name, matching the manifest's `class` column.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Vector => "vector",
            Self::Image => "image",
            Self::Mixed => "mixed",
            Self::Empty => "empty",
        }
    }

    /// Parse the manifest's `class` column.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "text" => Some(Self::Text),
            "vector" => Some(Self::Vector),
            "image" => Some(Self::Image),
            "mixed" => Some(Self::Mixed),
            "empty" => Some(Self::Empty),
            _ => None,
        }
    }
}

/// Measure one page.
pub fn classify_page(
    path: &Path,
    index: usize,
    dpi: f32,
) -> Result<(LoadClass, Composition), String> {
    let setup = pipeline::load_page(path, index, dpi)?;
    let comp = Composition::of(&setup.dl, setup.scale);
    Ok((comp.class(), comp))
}

/// Measure several pages of one document, for reporting how heterogeneous it is.
///
/// Returns `(page_index, class, composition)` for first/middle/last page. The
/// manifest records the class of the page the benches actually measure (page 0),
/// not a majority vote: a label that describes a different page than the one
/// measured is precisely the failure mode this module was written to end.
pub fn classify_pages(
    path: &Path,
    dpi: f32,
    max_samples: usize,
) -> Result<Vec<(usize, LoadClass, Composition)>, String> {
    let parsed = pipeline::parse(path, 0, dpi)?;
    let page_count = parsed.doc.page_count();
    if page_count == 0 {
        return Err(format!("{} has no pages", path.display()));
    }
    let mut indices: Vec<usize> = Vec::new();
    for i in [0, page_count / 2, page_count.saturating_sub(1)] {
        if !indices.contains(&i) {
            indices.push(i);
        }
    }
    indices.truncate(max_samples.max(1));

    let mut out = Vec::new();
    for idx in indices {
        let (class, comp) = classify_page(path, idx, dpi)?;
        out.push((idx, class, comp));
    }
    Ok(out)
}

/// Print a `ZPDF_BENCH_DEBUG`-style breakdown when that variable is set.
///
/// Kept as a function (rather than inline in a bench) so every target reports
/// composition the same way, and so the probe cannot rot unnoticed the way the
/// previous round's temporary env-var probes did.
pub fn maybe_report(label: &str, comp: &Composition) {
    if std::env::var("ZPDF_BENCH_DEBUG").as_deref() == Ok("1") {
        eprintln!(
            "zpdf-benches: {label} [{}] {}",
            comp.class().as_str(),
            comp.summary()
        );
    }
}

/// Print the interpret stage's per-category work split, behind the same
/// `ZPDF_BENCH_DEBUG=1` gate as [`maybe_report`].
///
/// `setup_ns` is the part of the criterion number that happens *before* the
/// interpreter runs — on a text page it is most of it, so a report that omitted
/// it would let the categories below be read as the whole stage.
///
/// The buckets themselves are attribution, not a partition — the formatting
/// (and that caveat) lives on [`zpdf::InterpretStats::describe`], next to the
/// numbers it describes.
pub fn maybe_report_interpret_work(
    label: &str,
    dpi: u32,
    setup_ns: u64,
    stats: &zpdf::InterpretStats,
) {
    if std::env::var("ZPDF_BENCH_DEBUG").as_deref() == Ok("1") {
        eprintln!(
            "zpdf-benches: {label}@{dpi} setup={:.2}ms | {}",
            setup_ns as f64 / 1e6,
            stats.describe()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zpdf::display_list::{
        Color, FillRule, GlyphRun, ImageDraw, Path, PositionedGlyph, StrokeStyle,
    };
    use zpdf_core::{Point, Rect};

    fn rect_path(w: f64, h: f64) -> Path {
        let mut p = Path::new();
        p.move_to(Point::new(0.0, 0.0));
        p.line_to(Point::new(w, 0.0));
        p.line_to(Point::new(w, h));
        p.line_to(Point::new(0.0, h));
        p.close();
        p
    }

    fn fill_cmd(w: f64, h: f64) -> RenderCommand {
        RenderCommand::FillPath {
            path: rect_path(w, h),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
        }
    }

    fn image_cmd(area_scale: f64) -> RenderCommand {
        RenderCommand::DrawImage(ImageDraw {
            image_id: 0,
            transform: Matrix::new(area_scale, 0.0, 0.0, area_scale, 0.0, 0.0),
            alpha: 1.0,
            is_image_mask: false,
        })
    }

    fn glyph_cmd(n: usize, font_size: f32, tm: Matrix) -> RenderCommand {
        RenderCommand::DrawGlyphRun(GlyphRun {
            font_id: 0,
            font_size,
            glyphs: (0..n)
                .map(|i| PositionedGlyph {
                    glyph_id: i as u16,
                    x: i as f32,
                    y: 0.0,
                    advance: 6.0,
                })
                .collect(),
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
            transform: tm,
            h_scale: 1.0,
        })
    }

    fn dl(cmds: Vec<RenderCommand>) -> DisplayList {
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        for c in cmds {
            dl.push(c);
        }
        dl
    }

    #[test]
    fn counts_each_command_family() {
        let d = dl(vec![
            fill_cmd(10.0, 10.0),
            fill_cmd(10.0, 10.0),
            RenderCommand::StrokePath {
                path: rect_path(4.0, 4.0),
                style: StrokeStyle::default(),
                paint: Paint::Solid(Color::black()),
                alpha: 1.0,
                overprint: None,
            },
            glyph_cmd(3, 10.0, Matrix::identity()),
            image_cmd(10.0),
            RenderCommand::PushClip {
                path: Path::new(),
                rule: FillRule::NonZero,
            },
            RenderCommand::PopClip,
        ]);
        let c = Composition::of(&d, 1.0);
        assert_eq!(c.commands, 7);
        assert_eq!(c.fills, 2);
        assert_eq!(c.strokes, 1);
        assert_eq!(c.glyph_runs, 1);
        assert_eq!(c.glyphs, 3);
        assert_eq!(c.images, 1);
        assert_eq!(c.clips, 1);
        assert_eq!(c.non_solid_paints, 0);
        assert_eq!(c.image_device_px, 100, "10x10 unit square at scale 1");
        assert_eq!(
            c.vector_device_px,
            100 + 100 + 16,
            "two 10x10 fills + a 4x4 stroke"
        );
    }

    #[test]
    fn areas_scale_with_scale_squared() {
        let d = dl(vec![image_cmd(10.0), fill_cmd(10.0, 10.0)]);
        let c = Composition::of(&d, 2.0);
        assert_eq!(c.image_device_px, 400);
        assert_eq!(c.vector_device_px, 400);
    }

    /// The bug this metric exists to avoid: a page with a thousand glyphs and one
    /// tiny image is text-bound, and a count-based rule calls it image-bound.
    #[test]
    fn a_tiny_image_does_not_outrank_a_thousand_glyphs() {
        let d = dl(vec![
            glyph_cmd(1000, 10.0, Matrix::identity()),
            // 81x61 device px, the trivial image on the real test8 page.
            image_cmd(1.0),
        ]);
        let c = Composition::of(&d, 1.0);
        assert_eq!(c.images, 1);
        assert!(
            c.glyph_device_px > c.image_device_px * 10,
            "glyph coverage {} should dwarf image coverage {}",
            c.glyph_device_px,
            c.image_device_px
        );
        assert_eq!(c.class(), LoadClass::Text);
    }

    /// ...and the converse: one full-page image with a caption's worth of text is
    /// image-bound.
    #[test]
    fn a_full_page_image_outranks_a_caption() {
        let d = dl(vec![
            image_cmd(600.0), // 360_000 device px
            glyph_cmd(20, 10.0, Matrix::identity()),
        ]);
        let c = Composition::of(&d, 1.0);
        assert_eq!(c.class(), LoadClass::Image);
    }

    #[test]
    fn class_is_derived_from_the_dominant_family() {
        let text = Composition::of(&dl(vec![glyph_cmd(500, 10.0, Matrix::identity())]), 1.0);
        assert_eq!(text.class(), LoadClass::Text);

        let vector = Composition::of(&dl(vec![fill_cmd(200.0, 200.0)]), 1.0);
        assert_eq!(vector.class(), LoadClass::Vector);

        // Comparable text and vector coverage -> mixed.
        // 40 glyphs at 10pt = 40 * (100 * 0.5) = 2000 px; a 45x45 fill = 2025 px.
        let mixed = Composition::of(
            &dl(vec![
                glyph_cmd(40, 10.0, Matrix::identity()),
                fill_cmd(45.0, 45.0),
            ]),
            1.0,
        );
        assert_eq!(mixed.class(), LoadClass::Mixed);
    }

    #[test]
    fn empty_page_is_empty_class() {
        let c = Composition::of(&dl(vec![RenderCommand::PopClip]), 1.0);
        assert_eq!(c.class(), LoadClass::Empty);
        assert_eq!(c.commands, 1, "PopClip is a command but paints nothing");
    }

    /// A degenerate path with no points must not produce a NaN area.
    #[test]
    fn degenerate_path_contributes_zero_area() {
        let c = Composition::of(&dl(vec![fill_cmd(0.0, 0.0)]), 1.0);
        assert_eq!(c.vector_device_px, 0);
        assert_eq!(c.fills, 1);
    }

    /// A non-solid paint is a fidelity red flag, not just a metric — the
    /// backends only paint `Paint::Solid`.
    #[test]
    fn non_solid_paints_are_flagged() {
        let cmd = RenderCommand::FillPath {
            path: rect_path(10.0, 10.0),
            rule: FillRule::NonZero,
            paint: Paint::Shading(0),
            alpha: 1.0,
            overprint: None,
        };
        let c = Composition::of(&dl(vec![cmd]), 1.0);
        assert_eq!(c.fills, 1);
        assert_eq!(c.non_solid_paints, 1);
        assert!(c.summary().contains("non-solid paints"), "{}", c.summary());
    }

    #[test]
    fn class_round_trips_through_its_manifest_name() {
        for class in [
            LoadClass::Text,
            LoadClass::Vector,
            LoadClass::Image,
            LoadClass::Mixed,
            LoadClass::Empty,
        ] {
            assert_eq!(LoadClass::parse(class.as_str()), Some(class));
        }
        assert_eq!(LoadClass::parse("nonsense"), None);
    }
}
