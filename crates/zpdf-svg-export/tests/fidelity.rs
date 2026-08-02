//! SVG↔CPU acceptance harness. Builds the same style of synthetic
//! single-feature corpus as `zpdf/tests/gpu_acceptance.rs`, runs each page
//! through the full pipeline, exports SVG, rasterizes it with resvg (pure
//! Rust, tiny-skia based — an independent SVG implementation), and asserts
//! the result matches the tiny-skia CPU oracle within a small differing-pixel
//! budget.

use zpdf::{ContentInterpreter, ImageCache, PdfDocument, RenderBackend};
use zpdf_svg_export::{display_list_to_svg, SvgOptions};

const SCALE: f32 = 2.0;
const THRESHOLD: u8 = 16;
const MAX_DIFF_PCT: f64 = 1.5;

/// Real embedded TrueType font (not Type3) — its Unicode cmap maps 'A' (0x41)
/// to GID 1, a filled-rectangle outline.
const TEST_TTF: &[u8] = include_bytes!("../../zpdf-font/tests/fixtures/var.ttf");

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

/// A 4-object PDF (catalog/pages/page/content) with an empty resource dict.
fn simple_pdf(content: &[u8]) -> Vec<u8> {
    assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R /Resources << >> >>"
            .to_vec(),
        stream_obj("", content),
    ])
}

fn inline_img(w: u32, h: u32, rgb: &[u8]) -> Vec<u8> {
    let mut v = format!("BI /W {w} /H {h} /CS /RGB /BPC 8 ID ").into_bytes();
    v.extend_from_slice(rgb);
    v.extend_from_slice(b" EI");
    v
}

/// A Type3 font PDF: glyph 'sq' (code 65) is a filled square; the page paints "AAA".
fn type3_pdf() -> Vec<u8> {
    let glyph = b"1000 0 d0\n150 150 700 700 re\nf";
    let content = b"0 0 0 rg\nBT /F1 60 Tf 15 70 Td (AAA) Tj ET";
    assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /Font << /F1 5 0 R >> >> >>"
            .to_vec(),
        stream_obj("", content),
        b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
          /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
          /FirstChar 65 /LastChar 65 /Widths [1000] /Resources << >> >>"
            .to_vec(),
        b"<< /sq 8 0 R >>".to_vec(),
        b"<< /Type /Encoding /Differences [65 /sq] >>".to_vec(),
        stream_obj("", glyph),
    ])
}

/// Real outline glyph ('A' → GID 1 in `TEST_TTF`) repeated across a grid.
fn outline_text_pdf(font_size: f32) -> Vec<u8> {
    let content = format!(
        "BT /F1 {font_size} Tf 0 0 0 rg\n\
        20 150 Td (A) Tj 40 0 Td (A) Tj 40 0 Td (A) Tj 40 0 Td (A) Tj\n\
        -120 -50 Td (A) Tj 40 0 Td (A) Tj 40 0 Td (A) Tj 40 0 Td (A) Tj\n\
        -120 -50 Td (A) Tj 40 0 Td (A) Tj 40 0 Td (A) Tj 40 0 Td (A) Tj\nET"
    );
    assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /Font << /F1 5 0 R >> >> >>"
            .to_vec(),
        stream_obj("", content.as_bytes()),
        b"<< /Type /Font /Subtype /TrueType /BaseFont /ZpdfSans /FirstChar 65 \
          /LastChar 65 /Widths [500] /FontDescriptor 6 0 R >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /ZpdfSans /Flags 32 \
          /FontBBox [0 -200 700 800] /FontFile2 7 0 R >>"
            .to_vec(),
        stream_obj(&format!("/Length1 {}", TEST_TTF.len()), TEST_TTF),
    ])
}

/// Transparency-group Form XObjects composited with a Multiply blend.
fn blend_groups_pdf(count: usize) -> Vec<u8> {
    let mut content = b"0.9 0.9 0.2 rg 0 0 200 200 re f\n".to_vec();
    for i in 0..count {
        let x = 10 + (i % 8) * 20;
        let y = 10 + (i % 6) * 25;
        content.extend_from_slice(format!("q 1 0 0 1 {x} {y} cm /GS1 gs /Fm1 Do Q\n").as_bytes());
    }
    let form = b"0 0 1 rg 0 0 60 60 re f\n0.2 0.8 0.2 rg 20 20 50 50 re f";
    assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /XObject << /Fm1 5 0 R >> /ExtGState << /GS1 6 0 R >> >> >>"
            .to_vec(),
        stream_obj("", &content),
        stream_obj(
            "/Type /XObject /Subtype /Form /BBox [0 0 60 60] \
             /Group << /Type /Group /S /Transparency /I true >>",
            form,
        ),
        b"<< /Type /ExtGState /BM /Multiply /ca 0.8 /CA 0.8 >>".to_vec(),
    ])
}

fn corpus() -> Vec<(&'static str, Vec<u8>)> {
    let img_a = [255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 0];
    let img_b = [0, 255, 255, 255, 0, 255, 255, 255, 255, 0, 0, 0];
    let mut image_rgb = Vec::new();
    image_rgb.extend_from_slice(b"q 70 0 0 70 20 110 cm ");
    image_rgb.extend_from_slice(&inline_img(2, 2, &img_a));
    image_rgb.extend_from_slice(b" Q\nq 70 0 0 70 110 110 cm ");
    image_rgb.extend_from_slice(&inline_img(2, 2, &img_b));
    image_rgb.extend_from_slice(b" Q\nq 80 0 0 -80 60 90 cm ");
    image_rgb.extend_from_slice(&inline_img(2, 2, &img_a));
    image_rgb.extend_from_slice(b" Q");

    let mut img_clip =
        b"1 1 0 rg 0 0 200 200 re f\nq 50 50 100 100 re W n\nq 200 0 0 200 0 0 cm ".to_vec();
    img_clip.extend_from_slice(&inline_img(2, 2, &img_b));
    img_clip.extend_from_slice(b" Q\nQ");

    vec![
        (
            "rect_fills",
            simple_pdf(
                b"0 0 1 rg 20 20 80 80 re f\n0 1 0 rg 110 20 70 70 re f\n\
                  1 0 0 rg 40 120 m 100 190 l 160 120 l h f\n\
                  0 0 0 rg 30 140 140 40 re 60 150 80 20 re f*",
            ),
        ),
        (
            "strokes",
            simple_pdf(
                b"0 0 0 RG 8 w 1 J 1 j 20 30 m 100 170 l 180 30 l S\n\
                  2 w 0 J 0 j 1 0 0 RG 20 100 m 180 100 l S\n\
                  14 w 2 J 0 0 1 RG 40 60 m 160 60 l S",
            ),
        ),
        (
            "strokes_dashed",
            simple_pdf(
                b"0 0 0 RG 4 w [8 4] 0 d 20 30 m 180 30 l S\n\
                  1 0 0 RG 3 w [6] 3 d 20 80 m 180 80 l S\n\
                  0 0 1 RG 5 w [10 5 2 5] 0 d 20 130 m 180 130 l 180 180 l S",
            ),
        ),
        (
            "curves",
            simple_pdf(
                b"0.2 0.4 0.8 rg 30 100 m 30 170 90 170 100 100 c 110 30 170 30 170 100 c f\n\
                  0 0 0 RG 1 w 20 50 m 60 10 140 190 180 150 c S",
            ),
        ),
        (
            "clip",
            simple_pdf(
                b"1 1 0 rg 0 0 200 200 re f\nq 30 30 140 140 re W n\n\
                  0 0 1 rg 0 0 200 200 re f\nq 60 60 120 60 re W n\n\
                  1 0 0 rg 0 0 200 200 re f\nQ\n0 1 0 rg 0 0 200 45 re f\nQ\n\
                  0 0 0 rg 175 175 20 20 re f",
            ),
        ),
        ("image_rgb", simple_pdf(&image_rgb)),
        ("image_under_clip", simple_pdf(&img_clip)),
        ("text_type3", type3_pdf()),
        ("text_outline", outline_text_pdf(40.0)),
        ("blend_group", blend_groups_pdf(6)),
    ]
}

/// Render one PDF via CPU backend and via SVG-export→resvg; return the
/// percentage of pixels whose max channel delta exceeds `THRESHOLD`.
fn compare_svg_vs_cpu(name: &str, pdf: Vec<u8>) -> f64 {
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

    let cpu = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, SCALE)
        .expect("cpu render");

    let svg = display_list_to_svg(&dl, &fonts, &images, &SvgOptions::default());

    let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default())
        .unwrap_or_else(|e| panic!("{name}: resvg failed to parse exported SVG: {e}"));
    let mut pixmap = resvg::tiny_skia::Pixmap::new(cpu.width, cpu.height).expect("pixmap alloc");
    let sx = cpu.width as f32 / tree.size().width();
    let sy = cpu.height as f32 / tree.size().height();
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(sx, sy),
        &mut pixmap.as_mut(),
    );

    let svg_px = pixmap.data();
    let total = (cpu.width * cpu.height) as u64;
    let mut diff = 0u64;
    for i in 0..total as usize {
        let b = i * 4;
        let dr = (svg_px[b] as i32 - cpu.data[b] as i32).unsigned_abs();
        let dg = (svg_px[b + 1] as i32 - cpu.data[b + 1] as i32).unsigned_abs();
        let db = (svg_px[b + 2] as i32 - cpu.data[b + 2] as i32).unsigned_abs();
        if dr.max(dg).max(db) > THRESHOLD as u32 {
            diff += 1;
        }
    }
    diff as f64 / total as f64 * 100.0
}

#[test]
fn svg_export_matches_cpu_oracle() {
    let mut failures = Vec::new();
    for (name, pdf) in corpus() {
        let pct = compare_svg_vs_cpu(name, pdf);
        eprintln!("svg fidelity {name}: {pct:.3}% differing pixels");
        if pct > MAX_DIFF_PCT {
            failures.push(format!("{name}: {pct:.3}% > {MAX_DIFF_PCT}%"));
        }
    }
    assert!(failures.is_empty(), "SVG fidelity failures: {failures:?}");
}
