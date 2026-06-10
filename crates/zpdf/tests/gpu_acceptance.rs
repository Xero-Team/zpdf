//! GPU<->CPU acceptance harness. Builds the synthetic single-feature corpus in
//! Rust (the Python generator + its output live under the gitignored `tests/`),
//! runs each through the full pipeline with both backends, and asserts the GPU
//! matches the tiny-skia CPU oracle within <1% differing pixels.
//!
//! Gated on `gpu-render`; run with `cargo test -p zpdf --features gpu-render`.
//! Skips gracefully when no GPU adapter is available (headless CI without a
//! software rasterizer).
#![cfg(feature = "gpu-render")]

use zpdf::{ContentInterpreter, ImageCache, PdfDocument, RenderBackend};

const SCALE: f32 = 2.0;
const THRESHOLD: u8 = 16;
const MAX_DIFF_PCT: f64 = 1.0;

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
    ]
}

/// Render one PDF with both backends; returns (differing %, CPU dims, GPU dims),
/// or `None` if no GPU adapter is available.
fn compare_backends(pdf: Vec<u8>) -> Option<(f64, (u32, u32), (u32, u32))> {
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

    // GPU first: bail out (skip) if there's no adapter.
    let gpu = match zpdf::gpu::WgpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, SCALE)
    {
        Ok(t) => t,
        Err(e) => {
            eprintln!("skipping GPU acceptance (no adapter?): {e}");
            return None;
        }
    };
    let cpu = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, SCALE)
        .expect("cpu render");

    if (cpu.width, cpu.height) != (gpu.width, gpu.height) {
        return Some((100.0, (cpu.width, cpu.height), (gpu.width, gpu.height)));
    }
    let total = (cpu.width * cpu.height) as u64;
    let mut diff = 0u64;
    for i in 0..total as usize {
        let b = i * 4;
        let dr = (gpu.data[b] as i32 - cpu.data[b] as i32).unsigned_abs();
        let dg = (gpu.data[b + 1] as i32 - cpu.data[b + 1] as i32).unsigned_abs();
        let db = (gpu.data[b + 2] as i32 - cpu.data[b + 2] as i32).unsigned_abs();
        if dr.max(dg).max(db) > THRESHOLD as u32 {
            diff += 1;
        }
    }
    Some((
        diff as f64 / total as f64 * 100.0,
        (cpu.width, cpu.height),
        (gpu.width, gpu.height),
    ))
}

#[test]
fn gpu_matches_cpu_on_corpus() {
    let mut skipped = false;
    for (name, pdf) in corpus() {
        match compare_backends(pdf) {
            None => {
                skipped = true;
                break;
            }
            Some((pct, cdim, gdim)) => {
                assert_eq!(
                    cdim, gdim,
                    "{name}: dimension mismatch {cdim:?} vs {gdim:?}"
                );
                println!("  {name}: {pct:.3}% differing");
                assert!(
                    pct < MAX_DIFF_PCT,
                    "{name}: GPU vs CPU {pct:.3}% exceeds {MAX_DIFF_PCT}%"
                );
            }
        }
    }
    if skipped {
        eprintln!("GPU acceptance harness skipped (no adapter).");
    }
}
