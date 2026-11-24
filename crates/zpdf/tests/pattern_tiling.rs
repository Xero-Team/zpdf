//! Tiling-pattern acceptance tests (CPU backend): colored cell replication,
//! pattern /Matrix, non-rect clipping, and uncolored (PaintType 2) cells
//! painted with the `scn` color. Mirrors the synthetic corpus fixtures
//! (tests/gen_corpus.py pattern_tiling*.pdf).
#![cfg(feature = "cpu-render")]

use zpdf::{ContentInterpreter, ImageCache, PdfDocument, RenderBackend};

const SCALE: f32 = 2.0;

/// Concatenate 1-based objects with a classic xref table + trailer.
fn assemble(objs: &[Vec<u8>]) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = Vec::with_capacity(objs.len());
    for (i, body) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref_pos = out.len();
    let n = objs.len() + 1;
    out.extend_from_slice(format!("xref\n0 {n}\n").as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for off in &offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<< /Size {n} /Root 1 0 R >>\nstartxref\n{xref_pos}\n%%EOF\n").as_bytes(),
    );
    out
}

fn stream_obj(dict: &str, content: &[u8]) -> Vec<u8> {
    let mut v = format!("<< {dict} /Length {} >>\nstream\n", content.len()).into_bytes();
    v.extend_from_slice(content);
    v.extend_from_slice(b"\nendstream");
    v
}

fn render(pdf: Vec<u8>) -> zpdf::cpu::RenderedPage {
    let doc = PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let content = doc.page_content_bytes(&page).expect("content bytes");
    let mut images = ImageCache::new();
    let dl = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images)
        .interpret(&content);
    zpdf::cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, SCALE)
        .expect("cpu render")
}

/// RGB at page-space (x, y) on a 200pt-high page rendered at SCALE.
fn px(page: &zpdf::cpu::RenderedPage, x: f64, y: f64) -> [u8; 3] {
    let ix = (x * SCALE as f64) as u32;
    let iy = ((200.0 - y) * SCALE as f64) as u32;
    let off = ((iy * page.width + ix) * 4) as usize;
    [page.data[off], page.data[off + 1], page.data[off + 2]]
}

fn assert_rgbish(c: [u8; 3], want: [u8; 3], what: &str) {
    let close = |a: u8, b: u8| {
        (a as i32 - b as i32).abs() <= 40 || (a > 200) == (b > 200) && (a < 60) == (b < 60)
    };
    assert!(
        close(c[0], want[0]) && close(c[1], want[1]) && close(c[2], want[2]),
        "{what}: got {c:?}, want ≈{want:?}"
    );
}

/// Colored tiling pattern: red square at [2,9]², blue at [11,18]² in a 20×20
/// cell, tiled over 10..90 × 110..190 with an identity pattern matrix.
#[test]
fn colored_tiling_pattern_replicates_cells() {
    let cell: &[u8] = b"1 0 0 rg 2 2 7 7 re f\n0 0 1 rg 11 11 7 7 re f";
    let pat = "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
               /BBox [0 0 20 20] /XStep 20 /YStep 20 /Resources << >>";
    let content: &[u8] = b"/Pattern cs /P0 scn 10 110 80 80 re f";
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /Pattern << /P0 5 0 R >> >> >>"
            .to_vec(),
        stream_obj("", content),
        stream_obj(pat, cell),
    ]);
    let page = render(pdf);

    // (25, 125): cell-space (5, 5) → inside the red square.
    assert_rgbish(px(&page, 25.0, 125.0), [255, 0, 0], "red cell square");
    // (35, 135): cell-space (15, 15) → inside the blue square.
    assert_rgbish(px(&page, 35.0, 135.0), [0, 0, 255], "blue cell square");
    // (30, 125): cell-space (10, 5) → between squares: white background.
    assert_rgbish(px(&page, 30.0, 125.0), [255, 255, 255], "cell gap");
    // (150, 150): outside the filled rect entirely.
    assert_rgbish(px(&page, 150.0, 150.0), [255, 255, 255], "outside fill");
    // Same cell positions one tile over (+20) replicate.
    assert_rgbish(px(&page, 45.0, 125.0), [255, 0, 0], "red square tile +x");
    assert_rgbish(px(&page, 25.0, 145.0), [255, 0, 0], "red square tile +y");
}

/// Uncolored (PaintType 2) pattern: the same colorless cell painted red then
/// blue via the scn operands; cell-gap stays background.
#[test]
fn uncolored_tiling_pattern_takes_scn_color() {
    let cell: &[u8] = b"2 2 16 16 re f";
    let pat = "/Type /Pattern /PatternType 1 /PaintType 2 /TilingType 1 \
               /BBox [0 0 20 20] /XStep 20 /YStep 20 /Resources << >>";
    let content: &[u8] =
        b"/CS0 cs\n1 0 0 /P0 scn 10 10 80 180 re f\n0 0 1 /P0 scn 110 10 80 180 re f";
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /Pattern << /P0 5 0 R >> /ColorSpace << /CS0 6 0 R >> >> >>"
            .to_vec(),
        stream_obj("", content),
        stream_obj(pat, cell),
        b"[/Pattern /DeviceRGB]".to_vec(),
    ]);
    let page = render(pdf);

    // (30, 30): cell-space (10, 10) → inside the square → scn red.
    assert_rgbish(
        px(&page, 30.0, 30.0),
        [255, 0, 0],
        "uncolored cell, red scn",
    );
    // (130, 30): same cell in the right rect → scn blue.
    assert_rgbish(
        px(&page, 130.0, 30.0),
        [0, 0, 255],
        "uncolored cell, blue scn",
    );
    // (20.5, 20.5): cell-space (0.5, 0.5) → in the 2pt gap → background.
    assert_rgbish(px(&page, 20.5, 20.5), [255, 255, 255], "uncolored cell gap");
}

/// A pattern cell containing an unmatched `BDC /OC` over a hidden layer must
/// not leak marked-content suppression into later tiles or page content.
#[test]
fn tiling_cell_oc_leak_does_not_suppress_page() {
    // The cell paints red inside a never-closed hidden /OC block.
    let cell: &[u8] = b"/OC /L0 BDC 1 0 0 rg 2 2 16 16 re f";
    let pat = "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
               /BBox [0 0 20 20] /XStep 20 /YStep 20 \
               /Resources << /Properties << /L0 6 0 R >> >>";
    let content: &[u8] = b"/Pattern cs /P0 scn 10 110 80 80 re f\n0 0 1 rg 120 20 60 60 re f";
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R /OCProperties << /OCGs [6 0 R] \
          /D << /OFF [6 0 R] >> >> >>"
            .to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /Pattern << /P0 5 0 R >> >> >>"
            .to_vec(),
        stream_obj("", content),
        stream_obj(pat, cell),
        b"<< /Type /OCG /Name (off) >>".to_vec(),
    ]);
    let doc = zpdf::PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let content_bytes = doc.page_content_bytes(&page).expect("content");
    let mut images = zpdf::ImageCache::new();
    let oc = doc.oc_config().expect("oc config");
    let dl = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images)
        .with_optional_content(&oc)
        .interpret(&content_bytes);
    let page = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, SCALE)
        .expect("cpu render");

    // The hidden cell content stays hidden…
    assert_rgbish(
        px(&page, 25.0, 125.0),
        [255, 255, 255],
        "hidden cell content",
    );
    // …but the page content AFTER the patterned fill must still paint.
    assert_rgbish(
        px(&page, 150.0, 50.0),
        [0, 0, 255],
        "post-pattern page fill",
    );
}

/// A tiling pattern applied to a STROKE paints the pattern clipped to the
/// stroke outline (not a solid average colour, and not the path interior).
/// Cell paints red in the left half (cell-x 0..10), white in the right half,
/// so a position landing in the red sub-cell proves the real pattern is used.
#[test]
fn tiling_pattern_strokes_outline() {
    // Red left half of a 20×20 cell; the right half shows the backdrop.
    let cell: &[u8] = b"1 0 0 rg 0 0 10 20 re f";
    let pat = "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
               /BBox [0 0 20 20] /XStep 20 /YStep 20 /Resources << >>";
    // 20pt-wide horizontal stroke along y=100 from x=20 to x=180. The stroke
    // colour space/pattern use the uppercase (stroking) operators.
    let content: &[u8] = b"/Pattern CS /P0 SCN 20 w 20 100 m 180 100 l S";
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /Pattern << /P0 5 0 R >> >> >>"
            .to_vec(),
        stream_obj("", content),
        stream_obj(pat, cell),
    ]);
    let page = render(pdf);

    // On the stroke band (y∈[90,110]) at x=45 → cell-x 5 → red sub-cell.
    assert_rgbish(
        px(&page, 45.0, 100.0),
        [255, 0, 0],
        "pattern on stroke (red)",
    );
    // On the stroke at x=55 → cell-x 15 → white sub-cell (NOT the solid avg).
    assert_rgbish(
        px(&page, 55.0, 100.0),
        [255, 255, 255],
        "pattern on stroke (white sub-cell)",
    );
    // Off the stroke (y=140) the pattern must not paint, even at a red cell-x.
    assert_rgbish(px(&page, 45.0, 140.0), [255, 255, 255], "off-stroke clip");
}

/// A pattern /Matrix offsets the tiling grid: shifting the pattern by (10, 10)
/// moves the red square from cell-space [2,9]² to page [12,19]².
#[test]
fn tiling_pattern_matrix_offsets_grid() {
    let cell: &[u8] = b"1 0 0 rg 2 2 7 7 re f";
    let pat = "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
               /BBox [0 0 20 20] /XStep 20 /YStep 20 /Resources << >> \
               /Matrix [1 0 0 1 10 10]";
    let content: &[u8] = b"/Pattern cs /P0 scn 0 0 200 200 re f";
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /Pattern << /P0 5 0 R >> >> >>"
            .to_vec(),
        stream_obj("", content),
        stream_obj(pat, cell),
    ]);
    let page = render(pdf);

    // Page (15, 15) = pattern (5, 5) → red square.
    assert_rgbish(px(&page, 15.0, 15.0), [255, 0, 0], "offset red square");
    // Page (5, 5) = pattern (-5, -5) = cell-space (15, 15) → empty.
    assert_rgbish(px(&page, 5.0, 5.0), [255, 255, 255], "offset cell gap");
}

/// Helper returning the display list, interpret stats, and rendered page at
/// custom page dimensions and scale.
fn render_with_stats(
    pdf: Vec<u8>,
    scale: f32,
) -> (
    zpdf::DisplayList,
    zpdf::InterpretStats,
    zpdf::cpu::RenderedPage,
) {
    let doc = PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let content = doc.page_content_bytes(&page).expect("content bytes");
    let mut images = ImageCache::new();
    let (dl, stats) = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images)
        .interpret_with_stats(&content);
    let rendered = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, scale)
        .expect("cpu render");
    (dl, stats, rendered)
}

fn px_on(page: &zpdf::cpu::RenderedPage, page_h: f64, scale: f32, x: f64, y: f64) -> [u8; 3] {
    let ix = (x * scale as f64) as u32;
    let iy = ((page_h - y) * scale as f64) as u32;
    let off = ((iy * page.width + ix) * 4) as usize;
    [page.data[off], page.data[off + 1], page.data[off + 2]]
}

/// Regression for #155: tiling patterns covering > 4,096 tiles (100×100 =
/// 10,000 tiles each) must emit compact `Paint::Tiling` commands without
/// falling back to a solid colour or hitting the command budget — for both
/// coloured (`/PaintType 1`) and uncoloured (`/PaintType 2`) patterns.
#[test]
fn over_4096_tiles_renders_pattern_without_solid_fallback() {
    // 600×600 pt page with 6×6 pt cells = 10,000 tiles per half (50×100 = 5,000
    // on each side; 100×100 across the whole page).
    let colored_cell: &[u8] = b"1 0 0 rg 0 0 3 3 re f\n0 0 1 rg 3 3 3 3 re f";
    let colored_pat = "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
                       /BBox [0 0 6 6] /XStep 6 /YStep 6 /Resources << >>";
    let uncolored_cell: &[u8] = b"0 0 3 3 re f";
    let uncolored_pat = "/Type /Pattern /PatternType 1 /PaintType 2 /TilingType 3 \
                         /BBox [0 0 6 6] /XStep 6 /YStep 6 /Resources << >>";
    // Full 600×600 coloured fill (10,000 tiles), then right half (300..600)
    // cleared white and filled with uncoloured green pattern (5,000 tiles).
    let content: &[u8] = b"/Pattern cs /P1 scn 0 0 600 600 re f\n\
                           1 1 1 rg 300 0 300 600 re f\n\
                           /CS0 cs 0 1 0 /P2 scn 300 0 300 600 re f";
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 600] /Contents 4 0 R \
          /Resources << /Pattern << /P1 5 0 R /P2 6 0 R >> /ColorSpace << /CS0 7 0 R >> >> >>"
            .to_vec(),
        stream_obj("", content),
        stream_obj(colored_pat, colored_cell),
        stream_obj(uncolored_pat, uncolored_cell),
        b"[/Pattern /DeviceRGB]".to_vec(),
    ]);

    let (dl, stats, page) = render_with_stats(pdf, 1.0);
    assert!(
        stats.diagnostics.is_complete(),
        "must not hit resource/tile cap: {:?}",
        stats.diagnostics
    );
    assert_eq!(
        dl.commands.len(),
        3,
        "display list stays O(1) in tile count, got {} commands",
        dl.commands.len()
    );

    // Coloured pattern (> 4,096 tiles): sample near bottom-left and near top-center.
    assert_rgbish(
        px_on(&page, 600.0, 1.0, 1.5, 1.5),
        [255, 0, 0],
        "colored tile (0,0) red",
    );
    assert_rgbish(
        px_on(&page, 600.0, 1.0, 295.5, 595.5),
        [255, 0, 0],
        "colored tile (49,99) red",
    );
    assert_rgbish(
        px_on(&page, 600.0, 1.0, 298.5, 598.5),
        [0, 0, 255],
        "colored tile (49,99) blue",
    );
    assert_rgbish(
        px_on(&page, 600.0, 1.0, 298.5, 595.5),
        [255, 255, 255],
        "colored tile (49,99) white gap",
    );

    // Uncoloured pattern (5,000 tiles): sample near far top-right corner.
    assert_rgbish(
        px_on(&page, 600.0, 1.0, 595.5, 595.5),
        [0, 255, 0],
        "uncolored tile (99,99) green",
    );
    assert_rgbish(
        px_on(&page, 600.0, 1.0, 598.5, 598.5),
        [255, 255, 255],
        "uncolored tile (99,99) white gap",
    );
}

/// Negative `/YStep` with fractional non-zero `/BBox` origin (as emitted by
/// Adobe Illustrator in PDFBOX-1094-5) must repeat cleanly across many tiles
/// without phase drift.
#[test]
fn negative_ystep_and_fractional_step_keep_exact_phase() {
    // 8.5pt step with negative YStep = -8.5 and bbox shifted by -0.5 in Y,
    // compensated by /Matrix [1 0 0 1 0 0.5].
    let cell: &[u8] = b"1 0 0 rg 1 0.5 3 3 re f";
    let pat = "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 2 \
               /BBox [0 -0.5 8.5 8.0] /XStep 8.5 /YStep -8.5 \
               /Matrix [1 0 0 1 0 0.5] /Resources << >>";
    let content: &[u8] = b"/Pattern cs /P0 scn 0 0 425 425 re f";
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 425 425] /Contents 4 0 R \
          /Resources << /Pattern << /P0 5 0 R >> >> >>"
            .to_vec(),
        stream_obj("", content),
        stream_obj(pat, cell),
    ]);

    let (_dl, stats, page) = render_with_stats(pdf, 2.0);
    assert!(stats.diagnostics.is_complete());
    // In tile (0, 0), red rect sits at page [1..4] × [1..4].
    // After 45 tiles (+382.5 pt in both axes), tile (45, 45) sits at
    // [383.5..386.5] × [383.5..386.5]; center (385.0, 385.0) must still be red,
    // and (389.0, 389.0) must be white gap (proving zero cumulative drift).
    assert_rgbish(
        px_on(&page, 425.0, 2.0, 2.5, 2.5),
        [255, 0, 0],
        "tile (0,0) red center",
    );
    assert_rgbish(
        px_on(&page, 425.0, 2.0, 385.0, 385.0),
        [255, 0, 0],
        "tile (45,45) red center without phase drift",
    );
    assert_rgbish(
        px_on(&page, 425.0, 2.0, 389.0, 389.0),
        [255, 255, 255],
        "tile (45,45) gap",
    );
}

/// Overlapping cells (`/BBox` larger than `/XStep`, `/YStep`, as in
/// MOZILLA-1244729-0) must wrap their protruding region seamlessly into the
/// start of the next period.
#[test]
fn overlapping_bbox_larger_than_step_wraps_into_neighbor_period() {
    // Cell bbox is 12×12 while XStep = YStep = 10. A red strip at x=[10.5..11.8]
    // protrudes past XStep=10 and must appear at x=[0.5..1.8] of every period.
    let cell: &[u8] = b"1 0 0 rg 10.5 2 1.3 6 re f";
    let pat = "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 3 \
               /BBox [0 0 12 12] /XStep 10 /YStep 10 /Resources << >>";
    let content: &[u8] = b"/Pattern cs /P0 scn 0 0 200 200 re f";
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /Pattern << /P0 5 0 R >> >> >>"
            .to_vec(),
        stream_obj("", content),
        stream_obj(pat, cell),
    ]);
    let page = render(pdf);
    // Page (21.1, 25.0) = period-offset (1.1, 5.0), painted by tile (1, 2)'s protrusion.
    assert_rgbish(
        px(&page, 21.1, 25.0),
        [255, 0, 0],
        "wrapped protrusion from neighbor tile",
    );
    // Page (25.0, 25.0) = period-offset (5.0, 5.0), unpainted gap.
    assert_rgbish(px(&page, 25.0, 25.0), [255, 255, 255], "interior gap");
}

/// Rotated pattern `/Matrix` and translucent `ca = 0.5` apply cleanly to
/// `Paint::Tiling`.
#[test]
fn rotated_and_translucent_tiling_pattern_renders() {
    // Solid red left half of a 20×20 cell, rotated 90° CCW via /Matrix [0 1 -1 0 0 0]
    // so cell +x maps to page +y (bottom half red, top half empty in every 20×20 tile),
    // painted at 50% fill alpha.
    let cell: &[u8] = b"1 0 0 rg 0 0 10 20 re f";
    let pat = "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
               /BBox [0 0 20 20] /XStep 20 /YStep 20 \
               /Matrix [0 1 -1 0 0 0] /Resources << >>";
    let content: &[u8] = b"/GS1 gs /Pattern cs /P0 scn 0 0 200 200 re f";
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /Pattern << /P0 5 0 R >> /ExtGState << /GS1 6 0 R >> >> >>"
            .to_vec(),
        stream_obj("", content),
        stream_obj(pat, cell),
        b"<< /Type /ExtGState /ca 0.5 >>".to_vec(),
    ]);
    let page = render(pdf);
    // Under 90° CCW rotation, cell x∈[0,10] maps to page y mod 20 ∈ [0,10].
    // Red at ca=0.5 over white gives ~[255, 128, 128].
    let tinted = px(&page, 45.0, 25.0);
    assert!(
        tinted[0] > 240 && (110..=150).contains(&tinted[1]) && (110..=150).contains(&tinted[2]),
        "expected translucent red band at rotated y mod 20 = 5, got {tinted:?}"
    );
    // y mod 20 = 15 falls in the empty half of the rotated cell → white.
    assert_rgbish(px(&page, 45.0, 35.0), [255, 255, 255], "rotated empty band");
}

/// Self-referential tiling pattern terminates immediately without recursing
/// into a stack overflow or hanging.
#[test]
fn self_referential_tiling_pattern_terminates_safely() {
    let cell: &[u8] = b"/Pattern cs /P0 scn 0 0 10 10 re f";
    let pat = "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
               /BBox [0 0 10 10] /XStep 10 /YStep 10 \
               /Resources << /Pattern << /P0 5 0 R >> >>";
    let content: &[u8] = b"/Pattern cs /P0 scn 0 0 100 100 re f";
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R \
          /Resources << /Pattern << /P0 5 0 R >> >> >>"
            .to_vec(),
        stream_obj("", content),
        stream_obj(pat, cell),
    ]);
    let (_dl, stats, _page) = render_with_stats(pdf, 1.0);
    assert!(
        stats.diagnostics.malformed_content > 0,
        "self-referential tiling pattern must be flagged as malformed_content"
    );
}

/// Regression: two sibling Form XObjects inside one tiling pattern cell that
/// apply the same indirect `/ExtGState` (whose `/SMask /G` form inherits `/Im`
/// from its enclosing form's `/Resources`) must NOT share a cached soft mask
/// when the two sibling forms bind `/Im` to different images.
#[test]
fn sibling_forms_in_tiling_cell_do_not_alias_inherited_soft_mask_resources() {
    // 1x1 DeviceGray image: white (255 -> luma 1.0, fully visible).
    let img_white = stream_obj(
        "/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8",
        &[255],
    );
    // 1x1 DeviceGray image: black (0 -> luma 0.0, fully masked out).
    let img_black = stream_obj(
        "/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8",
        &[0],
    );
    // Soft-mask transparency group /G with NO /Resources of its own; it draws
    // /Im inherited from whichever sibling form invoked /GS1 gs.
    let mask_group = stream_obj(
        "/Type /XObject /Subtype /Form /BBox [0 0 20 20] \
         /Group << /Type /Group /S /Transparency /CS /DeviceGray >>",
        b"20 0 0 20 0 0 cm /Im Do",
    );
    // Shared indirect ExtGState referencing mask_group (obj 10).
    let gs_shared =
        b"<< /Type /ExtGState /SMask << /Type /Mask /S /Luminosity /G 10 0 R >> >>".to_vec();
    // Sibling Form 1 binds /Im -> img_white (obj 8), applies /GS1 gs, paints red on left half [0..10].
    let form1 = stream_obj(
        "/Type /XObject /Subtype /Form /BBox [0 0 20 20] \
         /Resources << /XObject << /Im 8 0 R >> /ExtGState << /GS1 11 0 R >> >>",
        b"/GS1 gs 1 0 0 rg 0 0 10 20 re f",
    );
    // Sibling Form 2 binds /Im -> img_black (obj 9), applies the SAME /GS1 gs
    // under the exact same CTM/clip/paint/depth, and tries to paint blue on right half [10..20].
    let form2 = stream_obj(
        "/Type /XObject /Subtype /Form /BBox [0 0 20 20] \
         /Resources << /XObject << /Im 9 0 R >> /ExtGState << /GS1 11 0 R >> >>",
        b"/GS1 gs 0 0 1 rg 10 0 10 20 re f",
    );
    let cell = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 20 20] /XStep 20 /YStep 20 \
         /Resources << /XObject << /Fm1 6 0 R /Fm2 7 0 R >> >>",
        b"/Fm1 Do /Fm2 Do",
    );
    let content = stream_obj("", b"/Pattern cs /P0 scn 0 0 200 200 re f");
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /Pattern << /P0 5 0 R >> >> >>"
            .to_vec(), // 3
        content,                                       // 4
        cell,                                          // 5
        form1,                                         // 6
        form2,                                         // 7
        img_white,                                     // 8
        img_black,                                     // 9
        mask_group,                                    // 10
        gs_shared,                                     // 11
    ]);

    let page = render(pdf);
    // Left half of tile (cell-x = 5): masked by white image -> red must paint.
    assert_rgbish(
        px(&page, 25.0, 25.0),
        [255, 0, 0],
        "Fm1 left half visible through white mask",
    );
    // Right half of tile (cell-x = 15): masked by black image -> blue must be fully masked out (white bg).
    assert_rgbish(
        px(&page, 35.0, 25.0),
        [255, 255, 255],
        "Fm2 right half masked out by black mask (must not alias Fm1's cached mask)",
    );
}

/// Regression: two sibling Form XObjects with distinct `/Im` bindings that both
/// paint using the SAME tiling pattern (whose own `/Resources` dict is present
/// but omits `/Im`, falling back to the enclosing form's `/Resources`) must NOT
/// alias each other's pre-interpreted tiling cell in `tiling_cell_cache`.
#[test]
fn sibling_forms_do_not_alias_tiling_cell_cache_on_inherited_resources() {
    let img_red = stream_obj(
        "/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceRGB /BitsPerComponent 8",
        &[255, 0, 0],
    );
    let img_blue = stream_obj(
        "/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceRGB /BitsPerComponent 8",
        &[0, 0, 255],
    );
    // Pattern has an explicit (empty) `/Resources << >>`, so `/Im` falls back
    // to whichever enclosing Form XObject invoked the pattern.
    let pat = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 20 20] /XStep 20 /YStep 20 /Resources << >>",
        b"20 0 0 20 0 0 cm /Im Do",
    );
    let form1 = stream_obj(
        "/Type /XObject /Subtype /Form /BBox [0 0 200 200] \
         /Resources << /XObject << /Im 8 0 R >> /Pattern << /P0 5 0 R >> >>",
        b"/Pattern cs /P0 scn 0 0 100 200 re f",
    );
    let form2 = stream_obj(
        "/Type /XObject /Subtype /Form /BBox [0 0 200 200] \
         /Resources << /XObject << /Im 9 0 R >> /Pattern << /P0 5 0 R >> >>",
        b"/Pattern cs /P0 scn 100 0 100 200 re f",
    );
    let content = stream_obj("", b"/Fm1 Do /Fm2 Do");
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /XObject << /Fm1 6 0 R /Fm2 7 0 R >> >> >>"
            .to_vec(), // 3
        content,                                       // 4
        pat,                                           // 5
        form1,                                         // 6
        form2,                                         // 7
        img_red,                                       // 8
        img_blue,                                      // 9
    ]);

    let page = render(pdf);
    assert_rgbish(
        px(&page, 50.0, 100.0),
        [255, 0, 0],
        "Fm1 left half paints red inherited /Im",
    );
    assert_rgbish(
        px(&page, 150.0, 100.0),
        [0, 0, 255],
        "Fm2 right half paints blue inherited /Im (must not alias Fm1 cell cache)",
    );
}

/// Regression: commands retained inside distinct detached tiling-pattern cells
/// must count toward `with_command_limit(...)` so multiple cells cannot bypass
/// the page command budget.
#[test]
fn detached_tiling_cell_commands_count_toward_command_limit() {
    let mut cell_body = Vec::new();
    for i in 0..60 {
        cell_body.extend_from_slice(format!("1 0 0 rg {i} 0 1 1 re f\n").as_bytes());
    }
    let pat1 = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 100 10] /XStep 100 /YStep 10 /Resources << >>",
        &cell_body,
    );
    let pat2 = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 100 10] /XStep 100 /YStep 10 /Resources << >>",
        &cell_body,
    );
    let content = stream_obj(
        "",
        b"/Pattern cs /P1 scn 0 0 100 100 re f\n/P2 scn 100 0 100 100 re f",
    );
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /Pattern << /P1 5 0 R /P2 6 0 R >> >> >>"
            .to_vec(),
        content,
        pat1,
        pat2,
    ]);
    let doc = PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let content_bytes = doc.page_content_bytes(&page).expect("content");
    let mut images = ImageCache::new();
    let (_dl, stats) = ContentInterpreter::new(page.media_box)
        .with_command_limit(100)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images)
        .interpret_with_stats(&content_bytes);
    assert!(
        stats.diagnostics.resource_limit_reached,
        "two distinct 60-command tiling cells must exceed max_commands=100"
    );
}

/// Regression: a tiling pattern cell that executes its own `BT ... ET` block
/// while being built for a text-clipping render mode (`4 Tr`) must not corrupt
/// or clear the outer text block's `text_clip_accum` / `text_active` state.
#[test]
fn tiling_pattern_cell_text_block_preserves_outer_text_clip() {
    // Cell runs its own `BT ... ET` before painting a red square.
    let cell = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 20 20] /XStep 20 /YStep 20 \
         /Resources << /Font << /F1 6 0 R >> >>",
        b"BT /F1 8 Tf 2 2 Td (x) Tj ET\n1 0 0 rg 0 0 20 20 re f",
    );
    // Outer page uses mode 4 (fill + add to clip) painted with `/P0`, then
    // fills the entire page with blue after `ET`. Because the outer text clip
    // must remain intact, pixels outside the glyph outline stay white.
    let content = stream_obj(
        "",
        b"q\nBT /F1 120 Tf 4 Tr /Pattern cs /P0 scn 20 40 Td (I) Tj ET\n\
          0 0 1 rg 0 0 200 200 re f\nQ",
    );
    let font = b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec();
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /Pattern << /P0 5 0 R >> /Font << /F1 6 0 R >> >> >>"
            .to_vec(),
        content,
        cell,
        font,
    ]);
    let (dl, _stats, page) = render_with_stats(pdf, SCALE);
    let has_push_clip = dl
        .commands
        .iter()
        .any(|c| matches!(c, zpdf::display_list::RenderCommand::PushClip { .. }));
    assert!(
        has_push_clip,
        "outer mode-4 text block must still install its glyph PushClip after ET"
    );
    // Far right side (x=180, y=100) is outside the 'I' glyph clip, so the
    // subsequent full-page blue fill must NOT leak there.
    assert_rgbish(
        px(&page, 180.0, 100.0),
        [255, 255, 255],
        "pixels outside outer glyph clip must stay white",
    );
}

/// Regression: interpreting a nested tiling pattern `P_outer` (which paints
/// `P_inner`) near `MAX_FORM_DEPTH` so `P_inner` hits the nesting limit must
/// NOT poison `tiling_cell_cache` for a later shallow page-level draw of
/// `P_outer`.
#[test]
fn deep_nested_tiling_pattern_hit_does_not_poison_shallow_cache() {
    // Object layout:
    //  1: Catalog
    //  2: Pages
    //  3: Page (draws /F1 at depth 0, then fills right half with /POuter at depth 0)
    //  4: Page content
    //  5: PInner (solid green 20x20 cell)
    //  6: POuter (fills 20x20 cell with PInner; solid fallback would be black)
    //  7..=21: 15 nested Form XObjects F1 -> F2 -> ... -> F15, where F15 (at
    //          form_depth=15) paints left half with /POuter (so POuter enters
    //          at form_depth=15 -> increments to 16 -> PInner hits MAX_FORM_DEPTH=16).
    let p_inner = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 20 20] /XStep 20 /YStep 20 /Resources << >>",
        b"0 1 0 rg 0 0 20 20 re f",
    );
    let p_outer = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 20 20] /XStep 20 /YStep 20 \
         /Resources << /Pattern << /PInner 5 0 R >> >>",
        b"/Pattern cs /PInner scn 0 0 20 20 re f",
    );
    let content = stream_obj("", b"/F1 Do\n/Pattern cs /POuter scn 100 0 100 200 re f");
    let mut objs = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /XObject << /F1 7 0 R >> /Pattern << /POuter 6 0 R >> >> >>"
            .to_vec(),
        content,
        p_inner,
        p_outer,
    ];
    for level in 1..=15usize {
        let obj_id = 6 + level;
        if level < 15 {
            let next_id = obj_id + 1;
            objs.push(stream_obj(
                &format!(
                    "/Type /XObject /Subtype /Form /BBox [0 0 200 200] \
                     /Resources << /XObject << /FNext {next_id} 0 R >> >>"
                ),
                b"/FNext Do",
            ));
        } else {
            objs.push(stream_obj(
                "/Type /XObject /Subtype /Form /BBox [0 0 200 200] \
                 /Resources << /Pattern << /POuter 6 0 R >> >>",
                b"/Pattern cs /POuter scn 0 0 100 200 re f",
            ));
        }
    }

    let page = render(assemble(&objs));
    // Right half is drawn at page depth 0: POuter must render PInner's green
    // (not the degraded depth-15 fallback cached from inside F15).
    assert_rgbish(
        px(&page, 150.0, 100.0),
        [0, 255, 0],
        "shallow page-level POuter draw must render inner green pattern",
    );
}

/// Regression: an anisotropic pattern `/Matrix [4 0 0 1 0 0]` must scale a 1pt
/// vertical stroke to 4px wide and a 1pt horizontal stroke to 1px tall (not
/// an isotropic geometric-mean 2px for both).
#[test]
fn anisotropic_tiling_matrix_preserves_directional_stroke_widths() {
    // 10x10 pattern space mapped by [4 0 0 1 0 0] -> 40x10 device pixels at scale=1.
    // Red 1pt vertical stroke at cell x=5 -> x in [18..22] (4px wide).
    // Blue 1pt horizontal stroke at cell y=5 -> y in [4.5..5.5] (1px tall).
    let cell = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 10 10] /XStep 10 /YStep 10 \
         /Matrix [4 0 0 1 0 0] /Resources << >>",
        b"1 w\n1 0 0 RG 5 0 m 5 10 l S\n0 0 1 RG 0 5 m 10 5 l S",
    );
    let content = stream_obj("", b"/Pattern cs /P0 scn 0 0 80 40 re f");
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 80 40] /Contents 4 0 R \
          /Resources << /Pattern << /P0 5 0 R >> >> >>"
            .to_vec(),
        content,
        cell,
    ]);
    let (_dl, _stats, page) = render_with_stats(pdf, 1.0);
    // Integrate red ink thickness across one 40px tile row at y=2 (away from
    // the blue horizontal line at y=5): a 1pt stroke scaled by sx=4 integrates
    // to ~4.0 device pixels (vs ~2.0 under the old geometric-mean bug).
    let mut red_ink_px = 0.0f64;
    for x in 0..40 {
        let c = px_on(&page, 40.0, 1.0, x as f64 + 0.5, 2.5);
        red_ink_px += (255 - c[1]) as f64 / 255.0;
    }
    // Integrate blue ink thickness down one 10px tile column at x=5 (away from
    // the red vertical line at x=20): a 1pt stroke scaled by sy=1 integrates
    // to ~1.0 device pixel regardless of subpixel alignment (vs ~2.0 under the
    // old geometric-mean bug).
    let mut blue_ink_px = 0.0f64;
    for y in 0..10 {
        let c = px_on(&page, 40.0, 1.0, 5.5, y as f64 + 0.5);
        blue_ink_px += (255 - c[0]) as f64 / 255.0;
    }
    assert!(
        (3.5..=4.5).contains(&red_ink_px),
        "anisotropic vertical stroke should integrate to ~4px wide, measured {red_ink_px:.2}px"
    );
    assert!(
        (0.8..=1.2).contains(&blue_ink_px),
        "anisotropic horizontal stroke should integrate to ~1px tall, measured {blue_ink_px:.2}px"
    );
}

/// Regression: an overlapping cell whose painted ink sits more than 3 steps
/// past the fundamental period (`/BBox [0 0 50 10]`, `/XStep 10`, ink at
/// `x = 45..47`) must wrap copy `di = -4` into `[5..7]` instead of dropping it.
#[test]
fn wide_sparse_overlapping_bbox_beyond_four_steps_renders() {
    let cell = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 50 10] /XStep 10 /YStep 10 /Resources << >>",
        b"1 0 0 rg 45 2 2 6 re f",
    );
    let content = stream_obj("", b"/Pattern cs /P0 scn 0 0 200 200 re f");
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /Pattern << /P0 5 0 R >> >> >>"
            .to_vec(),
        content,
        cell,
    ]);
    let page = render(pdf);
    // Period offset (6.0, 5.0) comes from copy di=-4 (x=45..47 shifted by -40).
    assert_rgbish(
        px(&page, 26.0, 25.0),
        [255, 0, 0],
        "wrapped ink from di=-4 must paint at period x=6",
    );
    assert_rgbish(
        px(&page, 22.0, 25.0),
        [255, 255, 255],
        "gap outside wrapped ink must stay white",
    );
}

/// Regression: when an overlapping tiling cell (`/BBox [0 0 20 10]`, `/XStep 10`)
/// uses a soft mask, each shifted overlap copy `(di, dj)` changes the sub-renderer
/// origin and must NOT reuse a cached soft-mask plane rasterized at a previous
/// copy's origin.
#[test]
fn overlapping_tiling_cell_soft_mask_rebuilds_per_shifted_origin() {
    // Mask group paints white ONLY in x=[11..14] (outside [0..10] for di=0,
    // shifted into [1..4] for di=-1!).
    let mask_group = stream_obj(
        "/Type /XObject /Subtype /Form /BBox [0 0 20 10] \
         /Group << /Type /Group /S /Transparency /CS /DeviceGray >>",
        b"1 g 11 0 3 10 re f",
    );
    let gs = b"<< /Type /ExtGState /SMask << /Type /Mask /S /Luminosity /G 6 0 R >> >>".to_vec();
    let cell = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 20 10] /XStep 10 /YStep 10 \
         /Resources << /ExtGState << /GS1 7 0 R >> >>",
        b"/GS1 gs 1 0 0 rg 0 0 20 10 re f",
    );
    let content = stream_obj("", b"/Pattern cs /P0 scn 0 0 200 200 re f");
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /Pattern << /P0 5 0 R >> >> >>"
            .to_vec(), // 3
        content,                                       // 4
        cell,                                          // 5
        mask_group,                                    // 6
        gs,                                            // 7
    ]);
    let page = render(pdf);
    // Period offset x=2.5 (from di=-1 shifting [11..14] to [1..4]) must be red.
    assert_rgbish(
        px(&page, 22.5, 25.0),
        [255, 0, 0],
        "shifted overlap copy soft mask must reveal red at period x=2.5",
    );
    // Period offset x=7.5 is outside the mask in both di=-1 and di=0 -> white.
    assert_rgbish(
        px(&page, 27.5, 25.0),
        [255, 255, 255],
        "unmasked region at period x=7.5 must stay white",
    );
}

/// Regression: a small pattern cell (0.5×0.5 in pattern space) magnified by
/// `/Matrix [200 0 0 200 0 0]` to 100×100 pt on the page must size any shading
/// raster inside the cell from the transformed page-space footprint rather than
/// pattern-space units, so an axial gradient preserves distinct colors at
/// opposite ends instead of collapsing to a single 1×1 pixel.
#[test]
fn magnified_tiling_cell_preserves_axial_gradient_resolution() {
    // Type 2 axial shading across x in [0, 0.5]: red (1,0,0) -> blue (0,0,1).
    let fn_obj = b"<< /FunctionType 2 /Domain [0 1] /C0 [1 0 0] /C1 [0 0 1] /N 1 >>".to_vec();
    let shading_obj =
        b"<< /ShadingType 2 /ColorSpace /DeviceRGB /Coords [0 0 0.5 0] /Function 7 0 R /Extend [true true] >>"
            .to_vec();
    // 0.5×0.5 cell enlarged 200× to 100×100 pt per tile.
    let pat_large = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 0.5 0.5] /XStep 0.5 /YStep 0.5 \
         /Matrix [200 0 0 200 0 0] \
         /Resources << /Shading << /Sh1 6 0 R >> >>",
        b"/Sh1 sh",
    );
    // Also exercise the same cell object in a form at tiny 1× scale first, then
    // a form at 200× scale, to ensure tiling_cell_cache distinguishes cell scale.
    let pat_shared = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 0.5 0.5] /XStep 0.5 /YStep 0.5 \
         /Resources << /Shading << /Sh1 6 0 R >> >>",
        b"/Sh1 sh",
    );
    let form_tiny = stream_obj(
        "/Type /XObject /Subtype /Form /BBox [0 0 10 10] \
         /Resources << /Pattern << /PS 8 0 R >> >>",
        b"/Pattern cs /PS scn 0 0 1 1 re f",
    );
    let form_large = stream_obj(
        "/Type /XObject /Subtype /Form /BBox [0 0 1 1] \
         /Matrix [200 0 0 200 100 0] \
         /Resources << /Pattern << /PS 8 0 R >> >>",
        b"/Pattern cs /PS scn 0 0 0.5 0.5 re f",
    );
    // Left half [0..100] uses pat_large (/Matrix [200 0 0 200 0 0]);
    // right half [100..200] draws form_tiny then form_large with pat_shared.
    let content = stream_obj("", b"/Pattern cs /PL scn 0 0 100 100 re f\n/FT Do\n/FL Do");
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /Pattern << /PL 5 0 R >> /XObject << /FT 9 0 R /FL 10 0 R >> >> >>"
            .to_vec(), // 3
        content,                                       // 4
        pat_large,                                     // 5
        shading_obj,                                   // 6
        fn_obj,                                        // 7
        pat_shared,                                    // 8
        form_tiny,                                     // 9
        form_large,                                    // 10
    ]);
    let page = render(pdf);

    // Left tile [0..100]: left end (x=5) is red, middle (x=50) is purple, right end (x=95) is blue.
    assert_rgbish(
        px(&page, 5.0, 50.0),
        [242, 0, 13],
        "magnified cell left end red",
    );
    let mid = px(&page, 50.0, 50.0);
    assert!(
        (95..=160).contains(&mid[0]) && mid[1] < 20 && (95..=160).contains(&mid[2]),
        "magnified cell midpoint should blend red+blue, got {mid:?}"
    );
    assert_rgbish(
        px(&page, 95.0, 50.0),
        [13, 0, 242],
        "magnified cell right end blue",
    );

    // Right tile [100..200]: shared pattern magnified after a tiny draw must still preserve gradient.
    assert_rgbish(
        px(&page, 105.0, 50.0),
        [242, 0, 13],
        "shared magnified cell left end red",
    );
    assert_rgbish(
        px(&page, 195.0, 50.0),
        [13, 0, 242],
        "shared magnified cell right end blue",
    );
}

/// Regression (ISO 32000-1 §11.6.7): blend modes inside a tiling pattern cell
/// (such as `/BM /Multiply`) must composite against the live destination page
/// backdrop rather than a transparent cached tile. Paints the exact same cell
/// across both a blue left background and a yellow right background.
#[test]
fn tiling_cell_multiply_preserves_page_backdrop_across_backgrounds() {
    // Cell (20×20) paints two marks under `/BM /Multiply`:
    //  - left half [0..10]: opaque white (1 1 1 rg) -> Multiply identity (preserves backdrop!)
    //  - right half [10..20]: opaque cyan (0 1 1 rg) -> blue*cyan=blue, yellow*cyan=green.
    let gs_mul = b"<< /Type /ExtGState /BM /Multiply >>".to_vec();
    let cell = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 20 20] /XStep 20 /YStep 20 \
         /Resources << /ExtGState << /GM 6 0 R >> >>",
        b"/GM gs 1 1 1 rg 0 0 10 20 re f\n0 1 1 rg 10 0 10 20 re f",
    );
    // Blue left background [0..100], yellow right background [100..200], then
    // paint the same tiling pattern over the full 200×200 page.
    let content = stream_obj(
        "",
        b"0 0 1 rg 0 0 100 200 re f\n\
          1 1 0 rg 100 0 100 200 re f\n\
          /Pattern cs /P0 scn 0 0 200 200 re f",
    );
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /Pattern << /P0 5 0 R >> >> >>"
            .to_vec(), // 3
        content,                                       // 4
        cell,                                          // 5
        gs_mul,                                        // 6
    ]);
    let page = render(pdf);

    // Blue left half (x=25 -> cell-x=5, white Multiply): must stay blue, NOT turn white.
    assert_rgbish(
        px(&page, 25.0, 100.0),
        [0, 0, 255],
        "white Multiply cell over blue background must stay blue",
    );
    // Yellow right half (x=125 -> cell-x=5, white Multiply): must stay yellow, NOT turn white.
    assert_rgbish(
        px(&page, 125.0, 100.0),
        [255, 255, 0],
        "white Multiply cell over yellow background must stay yellow",
    );
    // Blue left half (x=35 -> cell-x=15, cyan Multiply): (0,0,1)*(0,1,1) = blue.
    assert_rgbish(
        px(&page, 35.0, 100.0),
        [0, 0, 255],
        "cyan Multiply cell over blue background stays blue",
    );
    // Yellow right half (x=135 -> cell-x=15, cyan Multiply): (1,1,0)*(0,1,1) = green.
    assert_rgbish(
        px(&page, 135.0, 100.0),
        [0, 255, 0],
        "cyan Multiply cell over yellow background turns green",
    );
}

/// Regression (ISO 32000-1 §8.7.3.1): a tiling pattern defined/used inside a
/// Form XObject inherits its parent stream's entry graphics state (e.g. a 6 pt
/// line width set before invoking the form), and `tiling_cell_cache` must
/// distinguish invocations of the same form under different entry line widths.
#[test]
fn tiling_pattern_in_form_inherits_form_entry_line_width_and_cache_key() {
    // Pattern cell strokes a vertical red line at cell x=10 WITHOUT setting `w`.
    let pat = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 20 20] /XStep 20 /YStep 20 /Resources << >>",
        b"1 0 0 RG 10 0 m 10 20 l S",
    );
    // Form paints a 100×200 strip using /P0 (and even sets `2 w` locally after
    // entry to verify the pattern inherits the form's ENTRY state, not current).
    let form = stream_obj(
        "/Type /XObject /Subtype /Form /BBox [0 0 100 200] \
         /Resources << /Pattern << /P0 5 0 R >> >>",
        b"2 w /Pattern cs /P0 scn 0 0 100 200 re f",
    );
    // Invoke the SAME form twice: left half under 6 w, right half under 1 w.
    let content = stream_obj(
        "",
        b"q 6 w /Fm1 Do Q\n\
          q 1 0 0 1 100 0 cm 1 w /Fm1 Do Q",
    );
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /XObject << /Fm1 6 0 R >> >> >>"
            .to_vec(), // 3
        content,                                       // 4
        pat,                                           // 5
        form,                                          // 6
    ]);

    let (_dl, _stats, page) = render_with_stats(pdf, 1.0);
    // Integrate red ink thickness across one 20px tile on the left (6 w) vs right (1 w).
    let mut left_ink_px = 0.0f64;
    for x in 20..40 {
        let c = px_on(&page, 200.0, 1.0, x as f64 + 0.5, 100.5);
        left_ink_px += (255 - c[1]) as f64 / 255.0;
    }
    let mut right_ink_px = 0.0f64;
    for x in 120..140 {
        let c = px_on(&page, 200.0, 1.0, x as f64 + 0.5, 100.5);
        right_ink_px += (255 - c[1]) as f64 / 255.0;
    }
    assert!(
        (5.4..=6.6).contains(&left_ink_px),
        "form invoked under 6 w must produce ~6px strokes in cell, measured {left_ink_px:.2}px"
    );
    assert!(
        (0.8..=1.3).contains(&right_ink_px),
        "same form invoked under 1 w must produce ~1px strokes (no cache aliasing), measured {right_ink_px:.2}px"
    );
}

/// Regression: two `gs` calls inside a tiling cell that apply the same indirect
/// `/SMask` in the same resource scope and CTM with only the line dash pattern
/// changed must NOT alias the cached soft mask (a `/G` form that strokes a line
/// inherits the active dash state, so a dashed second mask must leave gaps).
#[test]
fn tiling_cell_soft_mask_cache_distinguishes_line_dash_state() {
    // Soft-mask `/G` form strokes two horizontal white lines (y=10 and y=30)
    // WITHOUT setting `d`, so both inherit the caller's active dash pattern.
    let mask_group = stream_obj(
        "/Type /XObject /Subtype /Form /BBox [0 0 40 40] \
         /Group << /Type /Group /S /Transparency /CS /DeviceGray >>",
        b"1 G 8 w 0 10 m 40 10 l S\n0 30 m 40 30 l S",
    );
    let gs_mask =
        b"<< /Type /ExtGState /SMask << /Type /Mask /S /Luminosity /G 6 0 R >> >>".to_vec();
    // Inside one cell and one resource scope, apply the SAME `/GS1` twice under
    // the exact same CTM/clip: first with solid line (`[] 0 d`) masking a red
    // bottom strip (y in [0..20]), then with dashed `[8 8] 0 d` masking a blue
    // top strip (y in [20..40]).
    let cell = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 40 40] /XStep 40 /YStep 40 \
         /Resources << /ExtGState << /GS1 7 0 R >> >>",
        b"q [] 0 d /GS1 gs 1 0 0 rg 0 0 40 20 re f Q\n\
          q [8 8] 0 d /GS1 gs 0 0 1 rg 0 20 40 20 re f Q",
    );
    let content = stream_obj("", b"/Pattern cs /P0 scn 0 0 200 200 re f");
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /Pattern << /P0 5 0 R >> >> >>"
            .to_vec(), // 3
        content,                                       // 4
        cell,                                          // 5
        mask_group,                                    // 6
        gs_mask,                                       // 7
    ]);
    let page = render(pdf);

    // Bottom red strip (y=10, solid mask): visible at both x=4 and x=12.
    assert_rgbish(
        px(&page, 44.0, 50.0),
        [255, 0, 0],
        "solid soft mask reveals red at x=4",
    );
    assert_rgbish(
        px(&page, 52.0, 50.0),
        [255, 0, 0],
        "solid soft mask reveals red at x=12",
    );
    // Top blue strip (y=30, [8 8] dashed mask): visible at x=4 (on-dash [0..8]),
    // masked out to white at x=12 (off-gap [8..16]).
    assert_rgbish(
        px(&page, 44.0, 70.0),
        [0, 0, 255],
        "dashed soft mask reveals blue inside on-dash [0..8]",
    );
    assert_rgbish(
        px(&page, 52.0, 70.0),
        [255, 255, 255],
        "dashed soft mask must NOT reuse solid mask or leak blue ink in off-gap [8..16]",
    );
}
