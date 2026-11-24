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

/// Regression (ISO 32000-1 §11.6.7): a tiling pattern painted with object
/// opacity `< 1` (`ca = 0.5`) whose cell contains two overlapping opaque black
/// `/BM /Multiply` fills must evaluate the cell at full opacity against the
/// live backdrop and apply the object's 50% opacity ONCE (~128 over white, NOT
/// ~64 from compounding 50% per segment/mark). Covers both the non-overlapping
/// CPU segmented path and the overlapping CPU direct-replay path.
#[test]
fn tiling_cell_overlapping_multiply_applies_outer_opacity_once() {
    let gs_mul = b"<< /Type /ExtGState /BM /Multiply >>".to_vec();
    let gs_half = b"<< /Type /ExtGState /ca 0.5 >>".to_vec();
    // Two overlapping opaque black Multiply fills in [0..10], plus opaque white
    // Multiply fill in [10..20] (identity on any backdrop).
    let cell_body: &[u8] = b"/GM gs\n\
        0 0 0 rg 0 0 10 20 re f\n\
        0 0 0 rg 0 0 10 20 re f\n\
        1 1 1 rg 10 0 10 20 re f";
    // Non-overlapping cell -> CPU segmented tile path.
    let pat_non_overlap = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 20 20] /XStep 20 /YStep 20 \
         /Resources << /ExtGState << /GM 7 0 R >> >>",
        cell_body,
    );
    // Overlapping cell (`/BBox` wider than `/XStep`) -> CPU direct per-tile replay path.
    let pat_overlap = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 24 20] /XStep 20 /YStep 20 \
         /Resources << /ExtGState << /GM 7 0 R >> >>",
        cell_body,
    );
    // Top half (y in [100..200]): white page background.
    // Bottom half (y in [0..100]): blue background.
    // Paint non-overlapping pattern at 50% opacity on left [0..100],
    // overlapping pattern at 50% opacity on right [100..200].
    let content = stream_obj(
        "",
        b"0 0 1 rg 0 0 200 100 re f\n\
          q /GH gs /Pattern cs /P1 scn 0 0 100 200 re f Q\n\
          q /GH gs /Pattern cs /P2 scn 100 0 100 200 re f Q",
    );
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /Pattern << /P1 5 0 R /P2 6 0 R >> /ExtGState << /GH 8 0 R >> >> >>"
            .to_vec(), // 3
        content,                                       // 4
        pat_non_overlap,                               // 5
        pat_overlap,                                   // 6
        gs_mul,                                        // 7
        gs_half,                                       // 8
    ]);
    let page = render(pdf);

    // Left (segmented path): overlapping black Multiply over white at 50% -> ~[128, 128, 128] (NOT ~64).
    let seg_white = px(&page, 25.0, 150.0);
    assert!(
        seg_white.iter().all(|&c| (122..=134).contains(&c)),
        "CPU segmented overlapping Multiply over white at 50% should be ~128, got {seg_white:?}"
    );
    // Left (segmented path): white Multiply over blue at 50% -> stays blue [0, 0, 255].
    assert_rgbish(
        px(&page, 35.0, 50.0),
        [0, 0, 255],
        "CPU segmented white Multiply over blue at 50% stays blue",
    );
    // Left (segmented path): overlapping black Multiply over blue at 50% -> ~[0, 0, 128].
    let seg_blue = px(&page, 25.0, 50.0);
    assert!(
        seg_blue[0] < 8 && seg_blue[1] < 8 && (122..=134).contains(&seg_blue[2]),
        "CPU segmented overlapping Multiply over blue at 50% should be ~[0, 0, 128], got {seg_blue:?}"
    );

    // Right (direct per-tile replay path): overlapping black Multiply over white at 50% -> ~[128, 128, 128].
    let dir_white = px(&page, 125.0, 150.0);
    assert!(
        dir_white.iter().all(|&c| (122..=134).contains(&c)),
        "CPU direct-replay overlapping Multiply over white at 50% should be ~128, got {dir_white:?}"
    );
    // Right (direct per-tile replay path): white Multiply over blue at 50% -> stays blue [0, 0, 255].
    assert_rgbish(
        px(&page, 135.0, 50.0),
        [0, 0, 255],
        "CPU direct-replay white Multiply over blue at 50% stays blue",
    );
    // Right (direct per-tile replay path): overlapping black Multiply over blue at 50% -> ~[0, 0, 128].
    let dir_blue = px(&page, 125.0, 50.0);
    assert!(
        dir_blue[0] < 8 && dir_blue[1] < 8 && (122..=134).contains(&dir_blue[2]),
        "CPU direct-replay overlapping Multiply over blue at 50% should be ~[0, 0, 128], got {dir_blue:?}"
    );
}

/// Regression: during CPU direct per-tile replay of an overlapping
/// backdrop-dependent pattern, translated tiles with a spatially varying soft
/// mask must NOT alias freed `Arc<DisplayList>` addresses in `soft_mask_planes`
/// and drop or mis-position later tiles' masked marks.
#[test]
fn direct_replay_translated_tiles_with_spatially_varying_soft_mask() {
    // Spatially varying luminosity mask: white (visible) on left half [0..10],
    // black (masked out) on right half [10..20].
    let mask_group = stream_obj(
        "/Type /XObject /Subtype /Form /BBox [0 0 24 20] \
         /Group << /Type /Group /S /Transparency /CS /DeviceGray >>",
        b"1 g 0 0 10 20 re f",
    );
    let gs_mask_mul =
        b"<< /Type /ExtGState /BM /Multiply /SMask << /Type /Mask /S /Luminosity /G 6 0 R >> >>"
            .to_vec();
    // Overlapping cell (`/BBox` 24 > `/XStep` 20) + `/BM /Multiply` forces CPU direct replay.
    let cell = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 24 20] /XStep 20 /YStep 20 \
         /Resources << /ExtGState << /GM 7 0 R >> >>",
        b"/GM gs 0 1 1 rg 0 0 20 20 re f",
    );
    // Yellow background across 200×200 (10×10 = 100 translated tiles).
    let content = stream_obj(
        "",
        b"1 1 0 rg 0 0 200 200 re f\n/Pattern cs /P0 scn 0 0 200 200 re f",
    );
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /Pattern << /P0 5 0 R >> >> >>"
            .to_vec(), // 3
        content,                                       // 4
        cell,                                          // 5
        mask_group,                                    // 6
        gs_mask_mul,                                   // 7
    ]);
    let page = render(pdf);

    // Every translated tile must reveal cyan*yellow = green in its left half
    // [0..10] and stay yellow in its masked-out right half [10..20].
    for &(tx, ty) in &[(0, 0), (1, 0), (3, 2), (7, 5), (9, 9)] {
        let bx = tx as f64 * 20.0;
        let by = ty as f64 * 20.0;
        assert_rgbish(
            px(&page, bx + 5.0, by + 10.0),
            [0, 255, 0],
            &format!("tile ({tx},{ty}) unmasked left half should be cyan*yellow=green"),
        );
        assert_rgbish(
            px(&page, bx + 15.0, by + 10.0),
            [255, 255, 0],
            &format!("tile ({tx},{ty}) masked right half should stay yellow"),
        );
    }
}

/// Regression: an overlapping backdrop-dependent cell twice as wide as its
/// horizontal step (`/BBox [0 0 8 4] /XStep 4 /YStep 4`, opaque 50%-gray
/// `/BM /Multiply`) covers every interior point twice (`255 * 0.5 * 0.5 ≈ 64`
/// on white) and must produce the same ~64 interior value both below the 4,096
/// tile cap (direct replay) and above it (dense per-copy segment compositing).
#[test]
fn overlapping_multiply_cell_agrees_above_and_below_4096_tile_cap() {
    let gs_mul = b"<< /Type /ExtGState /BM /Multiply >>".to_vec();
    let cell = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 8 4] /XStep 4 /YStep 4 \
         /Resources << /ExtGState << /GM 6 0 R >> >>",
        b"/GM gs 0.5 0.5 0.5 rg 0 0 8 4 re f",
    );
    // Left fill [0..40]×[0..40] = 10×10 = 100 tiles (below 4,096 cap).
    // Right fill [60..320]×[0..260] = 65×65 = 4,225 tiles (above 4,096 cap).
    let content = stream_obj("", b"/Pattern cs /P0 scn 0 0 40 40 re f\n60 0 260 260 re f");
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 320 260] /Contents 4 0 R \
          /Resources << /Pattern << /P0 5 0 R >> >> >>"
            .to_vec(), // 3
        content,                                       // 4
        cell,                                          // 5
        gs_mul,                                        // 6
    ]);
    let (_dl, _stats, page) = render_with_stats(pdf, 1.0);

    let below = px_on(&page, 260.0, 1.0, 22.5, 22.5);
    let above = px_on(&page, 260.0, 1.0, 190.5, 130.5);
    assert!(
        below.iter().all(|&c| (58..=70).contains(&c)),
        "below-cap overlapping 50%-gray Multiply must be ~64 on white, got {below:?}"
    );
    assert!(
        above.iter().all(|&c| (58..=70).contains(&c)),
        "above-cap overlapping 50%-gray Multiply must be ~64 on white (not ~128), got {above:?}"
    );
    for ch in 0..3 {
        assert!(
            (below[ch] as i32 - above[ch] as i32).abs() <= 2,
            "interior pixel must agree below ({below:?}) and above ({above:?}) the 4,096 cap"
        );
    }
}

/// Regression: a dense (> 4,096 tiles) outer tiling pattern whose cell fills or
/// strokes with an inner `Multiply` pattern must remain visible and composite
/// against the live page backdrop on both the fill and stroke paths.
#[test]
fn dense_nested_multiply_tiling_pattern_above_4096_tiles_preserves_backdrop() {
    let gs_mul = b"<< /Type /ExtGState /BM /Multiply >>".to_vec();
    // Inner pattern (2×2): white Multiply on left half [0..1], cyan Multiply on right half [1..2].
    let p_inner = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 2 2] /XStep 2 /YStep 2 \
         /Resources << /ExtGState << /GM 9 0 R >> >>",
        b"/GM gs 1 1 1 rg 0 0 1 2 re f\n0 1 1 rg 1 0 1 2 re f",
    );
    // Outer fill pattern (4×4 -> 65×65 = 4,225 tiles over 260×260) fills with PInner.
    let p_outer_fill = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 4 4] /XStep 4 /YStep 4 \
         /Resources << /Pattern << /PI 5 0 R >> >>",
        b"/Pattern cs /PI scn 0 0 4 4 re f",
    );
    // Inner magenta Multiply pattern (1×1) used by a dense stroke pattern (> 4,096 tiles).
    let p_inner_mag = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 1 1] /XStep 1 /YStep 1 \
         /Resources << /ExtGState << /GM 9 0 R >> >>",
        b"/GM gs 1 0 1 rg 0 0 1 1 re f",
    );
    // Outer stroke pattern (1×1 -> 260×20 = 5,200 tiles on a 20pt stroke) fills with PInnerMag.
    let p_outer_stroke = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 1 1] /XStep 1 /YStep 1 \
         /Resources << /Pattern << /PM 7 0 R >> >>",
        b"/Pattern cs /PM scn 0 0 1 1 re f",
    );
    // Blue left [0..130], yellow right [130..260]; dense nested fill across whole
    // page, then dense nested stroke along y=200 (20pt tall).
    let content = stream_obj(
        "",
        b"0 0 1 rg 0 0 130 260 re f\n\
          1 1 0 rg 130 0 130 260 re f\n\
          /Pattern cs /POF scn 0 0 260 260 re f\n\
          /Pattern CS /POS SCN 20 w 0 200 m 260 200 l S",
    );
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 260 260] /Contents 4 0 R \
          /Resources << /Pattern << /POF 6 0 R /POS 8 0 R >> >> >>"
            .to_vec(), // 3
        content,                                       // 4
        p_inner,                                       // 5
        p_outer_fill,                                  // 6
        p_inner_mag,                                   // 7
        p_outer_stroke,                                // 8
        gs_mul,                                        // 9
    ]);
    let (_dl, _stats, page) = render_with_stats(pdf, 2.0);

    // Dense nested fill over yellow right half (y=100):
    // - cell-x=0.5 (white Multiply) -> stays yellow [255, 255, 0]
    // - cell-x=1.5 (cyan Multiply) -> becomes green [0, 255, 0] (NOT dropped/blank!)
    assert_rgbish(
        px_on(&page, 260.0, 2.0, 140.5, 100.0),
        [255, 255, 0],
        "dense nested white Multiply fill over yellow stays yellow",
    );
    assert_rgbish(
        px_on(&page, 260.0, 2.0, 141.5, 100.0),
        [0, 255, 0],
        "dense nested cyan Multiply fill over yellow turns green",
    );
    // Dense nested fill over blue left half: stays blue.
    assert_rgbish(
        px_on(&page, 260.0, 2.0, 21.5, 100.0),
        [0, 0, 255],
        "dense nested cyan Multiply fill over blue stays blue",
    );
    // Dense nested stroke (5,200 tiles) along y=200:
    // - over yellow (x=140.5, previously yellow after white Multiply): magenta*yellow = red!
    // - over blue (x=20.5): magenta*blue = blue!
    assert_rgbish(
        px_on(&page, 260.0, 2.0, 140.5, 200.0),
        [255, 0, 0],
        "dense nested magenta Multiply stroke over yellow turns red",
    );
    assert_rgbish(
        px_on(&page, 260.0, 2.0, 20.5, 200.0),
        [0, 0, 255],
        "dense nested magenta Multiply stroke over blue stays blue",
    );
}

/// Regression: when overlapping tiles mix `Normal` and `Multiply` marks
/// (`XStep = 4`, Normal red at `x = 0.5..2.0`, Multiply cyan at `x = 4.5..6.0`),
/// at period offset `x = 1.0` the previous copy paints Multiply cyan *before*
/// the current copy paints Normal red, leaving red — both below and above the
/// 4,096 tile cap (applying all Normal copies before all Multiply copies would
/// turn the pixel black).
#[test]
fn overlapping_mixed_normal_and_multiply_preserves_copy_order_above_cap() {
    let gs_norm = b"<< /Type /ExtGState /BM /Normal >>".to_vec();
    let gs_mul = b"<< /Type /ExtGState /BM /Multiply >>".to_vec();
    let cell = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 8 4] /XStep 4 /YStep 4 \
         /Resources << /ExtGState << /GN 6 0 R /GM 7 0 R >> >>",
        b"/GN gs 1 0 0 rg 0.5 0 1.5 4 re f\n/GM gs 0 1 1 rg 4.5 0 1.5 4 re f",
    );
    // Left fill [0..40]×[0..40] = 10×10 = 100 tiles (below cap).
    // Right fill [60..320]×[0..260] = 65×65 = 4,225 tiles (above cap).
    let content = stream_obj("", b"/Pattern cs /P0 scn 0 0 40 40 re f\n60 0 260 260 re f");
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 320 260] /Contents 4 0 R \
          /Resources << /Pattern << /P0 5 0 R >> >> >>"
            .to_vec(), // 3
        content,                                       // 4
        cell,                                          // 5
        gs_norm,                                       // 6
        gs_mul,                                        // 7
    ]);
    let (_dl, _stats, page) = render_with_stats(pdf, 2.0);

    // At period offset x = 1.25 (inside x = 0.5..2.0 of current copy and x = 4.5..6.0
    // of previous copy), both below cap (x = 21.25) and above cap (x = 181.25) must be RED.
    assert_rgbish(
        px_on(&page, 260.0, 2.0, 21.25, 22.0),
        [255, 0, 0],
        "below-cap overlapping Normal-over-Multiply copy order must yield red",
    );
    assert_rgbish(
        px_on(&page, 260.0, 2.0, 181.25, 130.0),
        [255, 0, 0],
        "above-cap overlapping Normal-over-Multiply copy order must yield red (not black)",
    );
}

/// Regression: an object with a fully opaque outer soft mask painting a
/// backdrop-dependent tiling pattern (opaque white `Multiply` over a blue page)
/// must evaluate the pattern against the live blue page backdrop before
/// applying the mask, leaving the pixel blue — NOT white (ISO 32000-1 §11.6.7).
#[test]
fn masked_backdrop_dependent_tiling_pattern_preserves_page_backdrop() {
    let gs_mul = b"<< /Type /ExtGState /BM /Multiply >>".to_vec();
    let mask_form = stream_obj(
        "/Type /XObject /Subtype /Form /BBox [0 0 100 100] \
         /Group << /S /Transparency /CS /DeviceRGB >>",
        b"1 1 1 rg 0 0 50 100 re f",
    );
    let gs_smask =
        b"<< /Type /ExtGState /SMask << /Type /Mask /S /Luminosity /G 7 0 R >> >>".to_vec();
    // Cell: left half white Multiply, right half cyan Multiply.
    let cell = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 20 20] /XStep 20 /YStep 20 \
         /Resources << /ExtGState << /GM 6 0 R >> >>",
        b"/GM gs 1 1 1 rg 0 0 10 20 re f\n0 1 1 rg 10 0 10 20 re f",
    );
    // Blue top half [0..100]×[50..100], yellow bottom half [0..100]×[0..50].
    // Soft mask is opaque white on left [0..50] and black (masked out) on right [50..100].
    let content = stream_obj(
        "",
        b"0 0 1 rg 0 50 100 50 re f\n\
          1 1 0 rg 0 0 100 50 re f\n\
          /GSM gs /Pattern cs /P0 scn 0 0 100 100 re f",
    );
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R \
          /Resources << /Pattern << /P0 5 0 R >> /ExtGState << /GSM 8 0 R >> >> >>"
            .to_vec(), // 3
        content,                                       // 4
        cell,                                          // 5
        gs_mul,                                        // 6
        mask_form,                                     // 7
        gs_smask,                                      // 8
    ]);
    let (_dl, _stats, page) = render_with_stats(pdf, 1.0);

    // Unmasked left half [0..50], blue top half: white Multiply over blue MUST stay blue!
    assert_rgbish(
        px_on(&page, 100.0, 1.0, 5.0, 75.0),
        [0, 0, 255],
        "opaque-masked white Multiply tiling over blue must stay blue (not white)",
    );
    // Unmasked left half [0..50], yellow bottom half: white stays yellow, cyan turns green.
    assert_rgbish(
        px_on(&page, 100.0, 1.0, 5.0, 25.0),
        [255, 255, 0],
        "opaque-masked white Multiply tiling over yellow must stay yellow",
    );
    assert_rgbish(
        px_on(&page, 100.0, 1.0, 15.0, 25.0),
        [0, 255, 0],
        "opaque-masked cyan Multiply tiling over yellow must turn green",
    );
    // Masked-out right half [50..100], yellow bottom half: cyan Multiply masked out -> stays yellow.
    assert_rgbish(
        px_on(&page, 100.0, 1.0, 75.0, 25.0),
        [255, 255, 0],
        "zero-masked cyan Multiply tiling over yellow must leave backdrop untouched",
    );
}

/// Regression: an outer cell painting a translucent (50%) inner pattern whose
/// cell has two opaque white `Multiply` marks must preserve a blue page
/// backdrop on BOTH sides of the 4,096 tile cap (neither pasting half-white
/// over blue nor diverging above vs below the cap).
#[test]
fn dense_translucent_multi_segment_nested_tiling_preserves_backdrop_above_cap() {
    let gs_mul = b"<< /Type /ExtGState /BM /Multiply >>".to_vec();
    let gs_half = b"<< /Type /ExtGState /ca 0.5 >>".to_vec();
    // Inner pattern (2×2): two opaque white Multiply marks covering [0..2]×[0..2].
    let p_inner = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 2 2] /XStep 2 /YStep 2 \
         /Resources << /ExtGState << /GM 7 0 R >> >>",
        b"/GM gs 1 1 1 rg 0 0 2 2 re f\n/GM gs 1 1 1 rg 0 0 2 2 re f",
    );
    // Outer pattern (4×4): paints inner pattern at 50% opacity.
    let p_outer = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 4 4] /XStep 4 /YStep 4 \
         /Resources << /Pattern << /PI 5 0 R >> /ExtGState << /GH 8 0 R >> >>",
        b"/GH gs /Pattern cs /PI scn 0 0 4 4 re f",
    );
    // Blue page [0..320]×[0..260].
    // Left fill [0..40]×[0..40] = 100 tiles (below 4,096 cap).
    // Right fill [60..320]×[0..260] = 65×65 = 4,225 tiles (above 4,096 cap).
    let content = stream_obj(
        "",
        b"0 0 1 rg 0 0 320 260 re f\n\
          /Pattern cs /PO scn 0 0 40 40 re f\n\
          60 0 260 260 re f",
    );
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 320 260] /Contents 4 0 R \
          /Resources << /Pattern << /PO 6 0 R >> >> >>"
            .to_vec(), // 3
        content,                                       // 4
        p_inner,                                       // 5
        p_outer,                                       // 6
        gs_mul,                                        // 7
        gs_half,                                       // 8
    ]);
    let (_dl, _stats, page) = render_with_stats(pdf, 1.0);

    let below = px_on(&page, 260.0, 1.0, 20.5, 20.5);
    let above = px_on(&page, 260.0, 1.0, 190.5, 130.5);
    assert_rgbish(
        below,
        [0, 0, 255],
        "below-cap translucent nested white Multiply over blue must stay blue",
    );
    assert_rgbish(
        above,
        [0, 0, 255],
        "above-cap translucent nested white Multiply over blue must stay blue (not pale blue)",
    );
}

/// Regression: when overlapping copies × segments exceeds 64 (2 copies × 33
/// segments = 66), dense compositing must STILL keep overlapping copies
/// separate in painter's order so two 50%-gray `Multiply` marks give ~64 on
/// white (not ~128).
#[test]
fn overlapping_multiply_above_64_passes_preserves_both_copies() {
    let gs_mul = b"<< /Type /ExtGState /BM /Multiply >>".to_vec();
    let gs_dark = b"<< /Type /ExtGState /BM /Darken >>".to_vec();
    let mut cell_stream = String::from("/GM gs 0.5 0.5 0.5 rg 0 0 8 2 re f\n");
    // 32 additional alternating Multiply/Darken marks in a separate horizontal
    // strip y=[2..4] so the cell has 33 non-coalesceable segments (2 copies × 33 = 66 > 64).
    for k in 0..32 {
        let gs = if k % 2 == 0 { "/GD" } else { "/GM" };
        cell_stream.push_str(&format!("{gs} gs 0.99 0.99 0.99 rg 0 2 8 2 re f\n"));
    }
    let cell = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 8 4] /XStep 4 /YStep 4 \
         /Resources << /ExtGState << /GM 6 0 R /GD 7 0 R >> >>",
        cell_stream.as_bytes(),
    );
    // Left [0..40]×[0..40] = 100 tiles (below cap); right [60..320]×[0..260] = 4,225 tiles (above cap).
    let content = stream_obj("", b"/Pattern cs /P0 scn 0 0 40 40 re f\n60 0 260 260 re f");
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 320 260] /Contents 4 0 R \
          /Resources << /Pattern << /P0 5 0 R >> >> >>"
            .to_vec(), // 3
        content,                                       // 4
        cell,                                          // 5
        gs_mul,                                        // 6
        gs_dark,                                       // 7
    ]);
    let (_dl, _stats, page) = render_with_stats(pdf, 1.0);

    // Sample inside the spanning strip y mod 4 = 1.0 (e.g. y = 21.0 and y = 129.0).
    let below = px_on(&page, 260.0, 1.0, 22.5, 21.0);
    let above = px_on(&page, 260.0, 1.0, 190.5, 129.0);
    assert!(
        below.iter().all(|&c| (58..=70).contains(&c)),
        "below-cap 2x33 overlapping 50%-gray Multiply must be ~64 on white, got {below:?}"
    );
    assert!(
        above.iter().all(|&c| (58..=70).contains(&c)),
        "above-cap 2x33 (>64) overlapping 50%-gray Multiply must stay ~64 on white (not ~128), got {above:?}"
    );
}

/// Regression: outer anti-aliased shape coverage (fills, stroked outlines, and
/// outline glyph runs) must be applied ONCE to the completed multi-segment
/// pattern rather than once per segment — matching a solid opaque black
/// reference on half-covered edge pixels (~128 on white, NOT ~64).
#[test]
fn multi_segment_tiling_applies_outer_shape_coverage_once_for_fill_stroke_and_glyph() {
    let gs_mul = b"<< /Type /ExtGState /BM /Multiply >>".to_vec();
    let cell = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 10 10] /XStep 10 /YStep 10 \
         /Resources << /ExtGState << /GM 7 0 R >> >>",
        b"/GM gs 0 0 0 rg 0 0 10 10 re f\n/GM gs 0 0 0 rg 0 0 10 10 re f",
    );
    let font = b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec();
    let build_pdf = |paint_cmds: &[u8]| {
        let content = stream_obj("", paint_cmds);
        assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 120 120] /Contents 4 0 R \
              /Resources << /Pattern << /P0 5 0 R >> /Font << /F1 6 0 R >> >> >>"
                .to_vec(), // 3
            content,                                       // 4
            cell.clone(),                                  // 5
            font.clone(),                                  // 6
            gs_mul.clone(),                                // 7
        ])
    };

    // 1. Half-pixel fill edge at x = 10.5 (pixel x=10 has ~50% coverage).
    // 2. Half-pixel stroke edges at x = 45.5 and 54.5 (stroke at x=50, width=9).
    // 3. Anti-aliased glyph clip 'O' at (70, 25), 60pt Helvetica. Using `7 Tr`
    //    (clip) + solid black rect in `ref_pdf` exercises the exact same
    //    `PushClip(glyph_clip)` geometry as pattern-painted text so every AA
    //    edge pixel is directly comparable without outline-curve flattening drift.
    let pat_pdf = build_pdf(
        b"/Pattern cs /P0 scn 10.5 10 20 80 re f\n\
          /Pattern CS /P0 SCN 9 w 50 10 m 50 90 l S\n\
          /Pattern cs /P0 scn BT /F1 60 Tf 70 25 Td (O) Tj ET",
    );
    let ref_pdf = build_pdf(
        b"0 0 0 rg 10.5 10 20 80 re f\n\
          0 0 0 RG 9 w 50 10 m 50 90 l S\n\
          q BT /F1 60 Tf 7 Tr 70 25 Td (O) Tj ET 0 0 0 rg 65 20 55 60 re f Q",
    );

    let (pat_dl, _, pat_page) = render_with_stats(pat_pdf, 1.0);
    let (_, _, ref_page) = render_with_stats(ref_pdf, 1.0);

    // Fill edge pixel at x=10, y=50: must match solid black reference (~128, NOT ~64).
    let fill_edge_pat = px_on(&pat_page, 120.0, 1.0, 10.2, 50.0);
    let fill_edge_ref = px_on(&ref_page, 120.0, 1.0, 10.2, 50.0);
    assert!(
        (120..=136).contains(&fill_edge_ref[0]),
        "sanity check: solid fill edge must be ~128, got {fill_edge_ref:?}"
    );
    assert!(
        (fill_edge_pat[0] as i32 - fill_edge_ref[0] as i32).abs() <= 2,
        "multi-segment fill edge must match solid reference ({fill_edge_ref:?}), got {fill_edge_pat:?}"
    );

    // Stroke edge pixel at x=45, y=50: must match solid black stroke reference (~128, NOT ~64).
    let stroke_edge_pat = px_on(&pat_page, 120.0, 1.0, 45.2, 50.0);
    let stroke_edge_ref = px_on(&ref_page, 120.0, 1.0, 45.2, 50.0);
    assert!(
        (120..=136).contains(&stroke_edge_ref[0]),
        "sanity check: solid stroke edge must be ~128, got {stroke_edge_ref:?}"
    );
    assert!(
        (stroke_edge_pat[0] as i32 - stroke_edge_ref[0] as i32).abs() <= 2,
        "multi-segment stroke edge must match solid reference ({stroke_edge_ref:?}), got {stroke_edge_pat:?}"
    );

    // Anti-aliased glyph-clip 'O' region x=[65..118], y_dev=[20..110]: every
    // partial-coverage edge pixel in the solid reference (60..=200) must match
    // the multi-segment tiling pattern within <= 2 (single coverage pass).
    let mut aa_clip_pixels = 0usize;
    for y in 20..110u32 {
        for x in 65..118u32 {
            let off = ((y * pat_page.width + x) * 4) as usize;
            let r_ref = ref_page.data[off];
            if (60..=200).contains(&r_ref) {
                aa_clip_pixels += 1;
                let r_pat = pat_page.data[off];
                assert!(
                    (r_pat as i32 - r_ref as i32).abs() <= 2,
                    "glyph-clip AA edge pixel ({x},{y}) diverged: solid ref={r_ref}, tiling={r_pat}"
                );
            }
        }
    }
    assert!(
        aa_clip_pixels >= 20,
        "expected anti-aliased glyph-clip edge pixels in 'O', found {aa_clip_pixels}"
    );

    // 4. Direct `RenderCommand::DrawGlyphRun` with `Paint::Tiling` (2 black
    //    Multiply segments) vs `Paint::Solid(black)`: proves the glyph-run
    //    renderer itself also applies outline glyph AA coverage once.
    let tiling_paint = pat_dl
        .commands
        .iter()
        .find_map(|c| match c {
            zpdf::display_list::RenderCommand::FillPath {
                paint: p @ zpdf::display_list::Paint::Tiling(_),
                ..
            } => Some(p.clone()),
            _ => None,
        })
        .expect("tiling paint in pat_dl");
    let glyph_pdf = build_pdf(b"0 0 0 rg BT /F1 60 Tf 70 25 Td (O) Tj ET");
    let doc = PdfDocument::open(glyph_pdf).expect("open glyph pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let content = doc.page_content_bytes(&page).expect("content bytes");
    let mut images = ImageCache::new();
    let solid_glyph_dl = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images)
        .interpret(&content);
    let mut tiling_glyph_dl = solid_glyph_dl.clone();
    let mut patched_runs = 0usize;
    for cmd in &mut tiling_glyph_dl.commands {
        if let zpdf::display_list::RenderCommand::DrawGlyphRun(run) = cmd {
            run.paint = tiling_paint.clone();
            patched_runs += 1;
        }
    }
    assert!(patched_runs >= 1, "expected at least one DrawGlyphRun");
    let solid_glyph_page = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&solid_glyph_dl, 1.0)
        .expect("solid glyph render");
    let tiling_glyph_page = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&tiling_glyph_dl, 1.0)
        .expect("tiling glyph render");
    let mut aa_run_pixels = 0usize;
    for y in 20..110u32 {
        for x in 65..118u32 {
            let off = ((y * solid_glyph_page.width + x) * 4) as usize;
            let r_ref = solid_glyph_page.data[off];
            if (60..=200).contains(&r_ref) {
                aa_run_pixels += 1;
                let r_pat = tiling_glyph_page.data[off];
                assert!(
                    (r_pat as i32 - r_ref as i32).abs() <= 2,
                    "DrawGlyphRun AA edge pixel ({x},{y}) diverged: solid ref={r_ref}, tiling={r_pat}"
                );
            }
        }
    }
    assert!(
        aa_run_pixels >= 20,
        "expected anti-aliased DrawGlyphRun edge pixels in 'O', found {aa_run_pixels}"
    );
}

/// Regression: a 7,000 × 7,000 raster (49M pixels, within the default 64M-pixel
/// limit) already uses 196 MB, so allocating two full-size RGBA scratch buffers
/// (588 MB total) would exceed the 512 MiB blend budget and silently drop even a
/// small 50%-opacity `Multiply` fill or stroke. Sizing the coverage buffers to
/// the clipped affected region (and banding large regions) keeps these marks
/// visible and accurate within the existing budget.
#[test]
fn small_patterned_mark_on_7000x7000_raster_fits_blend_budget() {
    let gs_mul = b"<< /Type /ExtGState /BM /Multiply >>".to_vec();
    let gs_half = b"<< /Type /ExtGState /ca 0.5 /CA 0.5 >>".to_vec();
    let cell_black = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 10 10] /XStep 10 /YStep 10 \
         /Resources << /ExtGState << /GM 7 0 R >> >>",
        b"/GM gs 0 0 0 rg 0 0 10 10 re f\n/GM gs 0 0 0 rg 0 0 10 10 re f",
    );
    let cell_white = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 10 10] /XStep 10 /YStep 10 \
         /Resources << /ExtGState << /GM 7 0 R >> >>",
        b"/GM gs 1 1 1 rg 0 0 10 10 re f",
    );
    // 1. Small 20×20 50%-opacity 2-seg black Multiply fill on white at (100.5, 100).
    // 2. Small 20×20 50%-opacity white Multiply fill over a blue patch at (200, 100).
    // 3. Small 50%-opacity 2-seg black Multiply stroke on white at x=300, y=100..120.
    let content = stream_obj(
        "",
        b"0 0 1 rg 200 100 20 20 re f\n\
          /GH gs /Pattern cs /PB scn 100.5 100 20 20 re f\n\
          /Pattern cs /PW scn 200 100 20 20 re f\n\
          /Pattern CS /PB SCN 10 w 300 100 m 300 120 l S",
    );
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 7000 7000] /Contents 4 0 R \
          /Resources << /Pattern << /PB 5 0 R /PW 6 0 R >> /ExtGState << /GH 8 0 R >> >> >>"
            .to_vec(), // 3
        content,                                       // 4
        cell_black,                                    // 5
        cell_white,                                    // 6
        gs_mul,                                        // 7
        gs_half,                                       // 8
    ]);

    let (_dl, _stats, page) = render_with_stats(pdf, 1.0);
    assert_eq!((page.width, page.height), (7000, 7000));

    // 1. Interior of 50%-opacity 2-seg black Multiply on white: ~128 (NOT 255 dropped, NOT ~64).
    let fill_mid = px_on(&page, 7000.0, 1.0, 110.5, 110.5);
    assert!(
        fill_mid.iter().all(|&c| (122..=134).contains(&c)),
        "50%-opacity black Multiply fill on 7000x7000 raster must paint ~128, got {fill_mid:?}"
    );
    // Half-covered edge at x=100.5 with 50% opacity: 0.5 * 0.5 = 0.25 black over white → ~191.
    let fill_edge = px_on(&page, 7000.0, 1.0, 100.2, 110.5);
    assert!(
        fill_edge.iter().all(|&c| (185..=197).contains(&c)),
        "half-covered 50%-opacity fill edge on 7000x7000 raster must be ~191, got {fill_edge:?}"
    );

    // 2. White Multiply over blue patch must stay pure blue (backdrop preserved).
    let blue_mid = px_on(&page, 7000.0, 1.0, 210.5, 110.5);
    assert_rgbish(
        blue_mid,
        [0, 0, 255],
        "50%-opacity white Multiply over blue on 7000x7000 raster must stay blue",
    );

    // 3. 50%-opacity 2-seg black Multiply stroke on white: ~128.
    let stroke_mid = px_on(&page, 7000.0, 1.0, 300.5, 110.5);
    assert!(
        stroke_mid.iter().all(|&c| (122..=134).contains(&c)),
        "50%-opacity black Multiply stroke on 7000x7000 raster must paint ~128, got {stroke_mid:?}"
    );
}

/// Regression (ISO 32000-1 §11.6.7): a backdrop-dependent tiling pattern painted
/// under an outer non-Normal blend mode (e.g. `/BM /Screen`) must evaluate its
/// cell against the live page backdrop BEFORE applying the outer blend mode.
/// Over opaque blue, white `Multiply` evaluates to blue, and `Screen(blue, blue)`
/// stays blue (isolating would evaluate to white and screen the page to white).
#[test]
fn outer_screen_blend_mode_on_backdrop_dependent_tiling_evaluates_cell_first() {
    let gs_mul = b"<< /Type /ExtGState /BM /Multiply >>".to_vec();
    let gs_screen = b"<< /Type /ExtGState /BM /Screen >>".to_vec();
    let cell = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 10 10] /XStep 10 /YStep 10 \
         /Resources << /ExtGState << /GM 6 0 R >> >>",
        b"/GM gs 1 1 1 rg 0 0 10 10 re f",
    );
    // Left half [0..50]: opaque blue; right half [50..100]: opaque 50% gray.
    // Paint patterned fill with `/BM /Screen` over [10..90]×[10..90].
    let content = stream_obj(
        "",
        b"0 0 1 rg 0 0 50 100 re f\n\
          0.5 0.5 0.5 rg 50 0 50 100 re f\n\
          /GS gs /Pattern cs /P0 scn 10 10 80 80 re f",
    );
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R \
          /Resources << /Pattern << /P0 5 0 R >> /ExtGState << /GS 7 0 R >> >> >>"
            .to_vec(), // 3
        content,                                       // 4
        cell.clone(),                                  // 5
        gs_mul.clone(),                                // 6
        gs_screen.clone(),                             // 7
    ]);
    let (_dl, _stats, page) = render_with_stats(pdf, 1.0);

    let blue_in = px_on(&page, 100.0, 1.0, 25.5, 50.5);
    assert_rgbish(
        blue_in,
        [0, 0, 255],
        "white Multiply cell under outer /BM /Screen over blue must stay blue (not screen to white)",
    );
    let gray_in = px_on(&page, 100.0, 1.0, 70.5, 50.5);
    assert!(
        gray_in.iter().all(|&c| (186..=197).contains(&c)),
        "white Multiply cell under outer /BM /Screen over 50% gray must screen(0.5,0.5)=~191, got {gray_in:?}"
    );
    let gray_out = px_on(&page, 100.0, 1.0, 95.5, 50.5);
    assert!(
        gray_out.iter().all(|&c| (124..=132).contains(&c)),
        "50% gray outside the patterned fill must stay untouched (~128), got {gray_out:?}"
    );

    // Same small white-Multiply pattern under outer /BM /Screen over opaque blue
    // on a 7,000 × 7,000 raster (page = 196 MB; two full-page scratch buffers
    // would exceed the 512 MiB blend budget). Scoped/banded replay must keep
    // the backdrop live so the pixel stays pure blue instead of isolating to white.
    let big_content = stream_obj(
        "",
        b"0 0 1 rg 0 0 7000 7000 re f\n\
          /GS gs /Pattern cs /P0 scn 100 100 80 80 re f",
    );
    let big_pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 7000 7000] /Contents 4 0 R \
          /Resources << /Pattern << /P0 5 0 R >> /ExtGState << /GS 7 0 R >> >> >>"
            .to_vec(), // 3
        big_content,                                   // 4
        cell,                                          // 5
        gs_mul,                                        // 6
        gs_screen,                                     // 7
    ]);
    let (_dl_big, _stats_big, big_page) = render_with_stats(big_pdf, 1.0);
    assert_eq!(big_page.width, 7000);
    assert_eq!(big_page.height, 7000);
    let big_blue_in = px_on(&big_page, 7000.0, 1.0, 140.5, 140.5);
    assert_rgbish(
        big_blue_in,
        [0, 0, 255],
        "white Multiply cell under outer /BM /Screen over blue on 7000x7000 raster must stay blue",
    );
}

/// Regression: when a dense outer cell (>4,096 tiles) paints a nested path with
/// a half-pixel edge using an inner pattern containing two opaque black
/// `Multiply` marks, the nested path's anti-aliased edge coverage must be
/// applied ONCE (~128 on white, matching a solid-black reference — NOT ~64).
#[test]
fn dense_nested_half_pixel_path_applies_inner_shape_coverage_once() {
    let gs_mul = b"<< /Type /ExtGState /BM /Multiply >>".to_vec();
    let gs_dark = b"<< /Type /ExtGState /BM /Darken >>".to_vec();
    // Non-coalesceable 2-segment inner pattern (Multiply + Darken black) so
    // lifting exercises `OpacityGroup { mask: Some(mask_seg), steps }` directly.
    let p_inner = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 2 2] /XStep 2 /YStep 2 \
         /Resources << /ExtGState << /GM 8 0 R /GD 9 0 R >> >>",
        b"/GM gs 0 0 0 rg 0 0 2 2 re f\n/GD gs 0 0 0 rg 0 0 2 2 re f",
    );
    // Outer cell 4×4 paints a rect with half-pixel left edge x = 1.5 using P_inner.
    let p_outer = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 4 4] /XStep 4 /YStep 4 \
         /Resources << /Pattern << /PI 5 0 R >> >>",
        b"/Pattern cs /PI scn 1.5 0 2 4 re f",
    );
    // Solid reference outer cell 4×4 painting the exact same half-pixel rect in solid black.
    let p_ref = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 4 4] /XStep 4 /YStep 4",
        b"0 0 0 rg 1.5 0 2 4 re f",
    );
    // 260×260 with 4×4 outer cells = 65×65 = 4,225 tiles (>4,096 dense path).
    let content = stream_obj(
        "",
        b"/Pattern cs /PO scn 0 0 260 260 re f\n\
          /Pattern cs /PR scn 280 0 260 260 re f",
    );
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 540 260] /Contents 4 0 R \
          /Resources << /Pattern << /PO 6 0 R /PR 7 0 R >> >> >>"
            .to_vec(), // 3
        content,                                       // 4
        p_inner,                                       // 5
        p_outer,                                       // 6
        p_ref,                                         // 7
        gs_mul,                                        // 8
        gs_dark,                                       // 9
    ]);
    let (_dl, _stats, page) = render_with_stats(pdf, 1.0);

    // Sample at the half-covered column x mod 4 == 1 (e.g. x=121 for PO, x=401 for PR).
    let pat_edge = px_on(&page, 260.0, 1.0, 121.2, 130.5);
    let ref_edge = px_on(&page, 260.0, 1.0, 401.2, 130.5);
    assert!(
        (120..=136).contains(&ref_edge[0]),
        "sanity check: solid nested edge must be ~128, got {ref_edge:?}"
    );
    assert!(
        (pat_edge[0] as i32 - ref_edge[0] as i32).abs() <= 3,
        "dense nested half-pixel edge must match solid ref ({ref_edge:?}, NOT ~64), got {pat_edge:?}"
    );
}

/// Regression: on a dense grid (>4,096 tiles), an overlapping cell spanning 6
/// horizontal steps with 205 spanning white `Multiply` marks (or 175
/// non-coalesceable alternating `Multiply`/`Darken` white marks exceeding the
/// 1,024 per-copy pass budget) followed by a black detail confined to the first
/// step must NOT silently drop the trailing black detail and must still
/// preserve the page backdrop under the white marks.
#[test]
fn dense_overlapping_cell_exceeding_pass_budget_preserves_trailing_detail() {
    let gs_mul = b"<< /Type /ExtGState /BM /Multiply >>".to_vec();
    let gs_dark = b"<< /Type /ExtGState /BM /Darken >>".to_vec();

    // Cell A: 205 spanning white Multiply marks + trailing black detail in step 0.
    let mut cell_mul_stream = String::new();
    for _ in 0..205 {
        cell_mul_stream.push_str("/GM gs 1 1 1 rg 0 0 24 4 re f\n");
    }
    cell_mul_stream.push_str("0 0 0 rg 0 0 2 4 re f\n");
    let cell_mul = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 24 4] /XStep 4 /YStep 4 \
         /Resources << /ExtGState << /GM 7 0 R >> >>",
        cell_mul_stream.as_bytes(),
    );

    // Cell B: 175 non-coalesceable alternating Multiply/Darken spanning marks
    // across 6 steps (6 × 175 = 1,050 > 1,024 pass budget), with a spanning
    // opaque 50%-gray Multiply mark followed immediately by a spanning opaque
    // 50%-gray Screen mark (so six Multiply–Screen pairs over blue yield
    // ~170 in blue, whereas combining copies per segment yields ~251), plus a
    // trailing black detail in step 0, on a 2×2 step (132×132 fill = 66×66 =
    // 4,356 > 4,096 tiles).
    let gs_screen = b"<< /Type /ExtGState /BM /Screen >>".to_vec();
    let mut cell_alt_stream = String::new();
    for k in 0..175 {
        let (gs, rgb) = if k == 20 {
            ("/GM", "0.5 0.5 0.5")
        } else if k == 21 {
            ("/GS", "0.5 0.5 0.5")
        } else if k % 2 == 0 {
            ("/GM", "1 1 1")
        } else {
            ("/GD", "1 1 1")
        };
        cell_alt_stream.push_str(&format!("{gs} gs {rgb} rg 0 0 12 2 re f\n"));
    }
    cell_alt_stream.push_str("/GM gs 0 0 0 rg 0 0 1 2 re f\n");
    let cell_alt = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 12 2] /XStep 2 /YStep 2 \
         /Resources << /ExtGState << /GM 7 0 R /GD 8 0 R /GS 9 0 R >> >>",
        cell_alt_stream.as_bytes(),
    );

    let content = stream_obj(
        "",
        b"0 0 1 rg 0 0 420 260 re f\n\
          /Pattern cs /PM scn 0 0 260 260 re f\n\
          /Pattern cs /PA scn 280 0 132 132 re f",
    );
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 420 260] /Contents 4 0 R \
          /Resources << /Pattern << /PM 5 0 R /PA 6 0 R >> >> >>"
            .to_vec(), // 3
        content,                                       // 4
        cell_mul,                                      // 5
        cell_alt,                                      // 6
        gs_mul,                                        // 7
        gs_dark,                                       // 8
        gs_screen,                                     // 9
    ]);
    let (_dl, _stats, page) = render_with_stats(pdf, 1.0);

    // Cell A (205 Multiply): x mod 4 == 1 is black detail, x mod 4 == 3 stays blue.
    assert_rgbish(
        px_on(&page, 260.0, 1.0, 121.2, 130.5),
        [0, 0, 0],
        "Cell A trailing black detail past 205 white Multiply marks must remain visible",
    );
    assert_rgbish(
        px_on(&page, 260.0, 1.0, 123.2, 130.5),
        [0, 0, 255],
        "Cell A white Multiply marks must preserve blue backdrop",
    );

    // Cell B (175 alternating Multiply/Darken + gray Multiply/Screen pair > 1,024 passes):
    // x mod 2 == 0 is black detail; x mod 2 == 1 gets 6 Multiply–Screen pairs over blue -> ~170
    // (NOT ~251 from running 6 Multiplies followed by 6 Screens).
    assert_rgbish(
        px_on(&page, 260.0, 1.0, 340.2, 66.5),
        [0, 0, 0],
        "Cell B trailing black detail past 1,024-pass overlap budget must remain visible",
    );
    let bg_b = px_on(&page, 260.0, 1.0, 341.2, 66.5);
    assert!(
        (165..=175).contains(&bg_b[2])
            && (165..=175).contains(&bg_b[0])
            && (165..=175).contains(&bg_b[1]),
        "Cell B 6x Multiply–Screen pairs over blue must be ~[170,170,170] (NOT blue ~251), got {bg_b:?}"
    );
}

/// Regression: a small outline-font glyph inside a non-isolated `/BM /Screen`
/// group on a 7,000 × 7,000 raster must scope tightly to the glyph bounds (with
/// live target dimensions available before `pixmap.take()`) rather than falling
/// back to page-wide banding or dropping the group.
#[test]
fn small_outline_glyph_in_non_isolated_screen_group_on_7000x7000_raster() {
    let gs_screen = b"<< /Type /ExtGState /BM /Screen >>".to_vec();
    let font = b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec();
    let form = stream_obj(
        "/Type /XObject /Subtype /Form /BBox [0 0 7000 7000] \
         /Group << /S /Transparency /I false >> \
         /Resources << /Font << /F1 6 0 R >> >>",
        b"0.5 0.5 0.5 rg BT /F1 40 Tf 100 100 Td (I) Tj ET",
    );
    let content = stream_obj("", b"0.5 0.5 0.5 rg 0 0 7000 7000 re f\n/GS gs /FM Do");
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 7000 7000] /Contents 4 0 R \
          /Resources << /XObject << /FM 5 0 R >> /ExtGState << /GS 7 0 R >> >> >>"
            .to_vec(), // 3
        content,                                       // 4
        form,                                          // 5
        font,                                          // 6
        gs_screen,                                     // 7
    ]);
    let (_dl, _stats, page) = render_with_stats(pdf, 1.0);
    assert_eq!((page.width, page.height), (7000, 7000));
    // Check that the glyph 'I' painted visibly (Screen(0.5, 0.5) ≈ 191 over 128 gray).
    let mut brightened = 0usize;
    for y_pdf in 100..135 {
        for x in 100..120 {
            let p = px_on(&page, 7000.0, 1.0, x as f64 + 0.5, y_pdf as f64 + 0.5);
            if p[0] >= 180 && p[1] >= 180 && p[2] >= 180 {
                brightened += 1;
            }
        }
    }
    assert!(
        brightened >= 20,
        "outline glyph inside non-isolated Screen group on 7000x7000 raster must render ~191 pixels, found {brightened}"
    );
}
