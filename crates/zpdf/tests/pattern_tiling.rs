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
