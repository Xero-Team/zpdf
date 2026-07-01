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

/// Real embedded TrueType font (not Type3) — its Unicode cmap maps 'A' (0x41)
/// to GID 1, a filled-rectangle outline (see `crates/zpdf/tests/variable_font_pdf.rs`,
/// which exercises the same fixture/GID via `glyph_outline(1)` directly).
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

/// A PDF with the SAME real outline glyph ('A' -> GID 1 in `TEST_TTF`) repeated
/// 12 times across a grid, all under one plain (translation-only) text
/// transform — axis-aligned, unscaled, unmirrored. Exercises the wgpu glyph
/// atlas's cache-reuse path (P3.4/M6b): every repeat after the first hits the
/// same `GlyphKey` and blits a quad instead of re-rasterizing. `text_type3`
/// above never exercises this — Type3 glyphs always take the content-stream
/// path, never the outline-glyph atlas.
///
/// `font_size` is a parameter rather than a constant: the atlas buckets to
/// the nearest *integer* device-pixel em-size
/// (`glyph.rs::axis_aligned_px_per_em`) while the CPU oracle rasterizes at
/// the exact continuous size, so a size that happens to round cleanly (e.g.
/// 40pt at 2x = a whole 80px) makes an axis-aligned rectangle glyph agree
/// bit-for-bit between tiny-skia and lyon's MSAA *regardless of which path is
/// used* — verified empirically: at 40pt, forcing `ZPDF_GPU_GLYPH_ATLAS=0`
/// also diffs 0.000% against CPU either way, so it can't discriminate
/// "did the atlas actually run." The corpus regression case below uses a
/// round size (comfortable, stable margin vs the 1% gate); the separate
/// atlas-engagement test uses a fractional size (genuinely exercises the
/// size-rounding delta) precisely because that test asserts a *relative*
/// property (atlas output != forced-fallback output) rather than an absolute
/// diff-% budget, so it doesn't need a safety margin against a fixed gate.
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

/// A PDF that paints `count` transparency-group Form XObjects in sequence, each
/// composited with a blend mode. Exercises the wgpu layered path + its recycling
/// LayerPool (many groups must not allocate one full-page layer apiece).
fn blend_groups_pdf(count: usize) -> Vec<u8> {
    let mut content = b"0.9 0.9 0.2 rg 0 0 200 200 re f\n".to_vec();
    for i in 0..count {
        // Stagger each group's position so they overlap differently.
        let x = 10 + (i % 8) * 20;
        let y = 10 + (i % 6) * 25;
        content.extend_from_slice(format!("q 1 0 0 1 {x} {y} cm /GS1 gs /Fm1 Do Q\n").as_bytes());
    }
    // Group form: two overlapping translucent rects.
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
        ("text_outline_repeated", outline_text_pdf(40.0)),
        // One blend group, then many — the latter forces the LayerPool to recycle
        // rather than allocate a layer per group.
        ("blend_group_single", blend_groups_pdf(1)),
        ("blend_group_many", blend_groups_pdf(40)),
    ]
}

/// Render one PDF with both backends; returns (differing %, CPU dims, GPU dims),
/// or `None` if no GPU adapter is available.
type BackendDiff = (f64, (u32, u32), (u32, u32));

fn compare_backends(pdf: Vec<u8>) -> Option<BackendDiff> {
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

/// A page exercising generated markup/geometric annotation appearances (no
/// `/AP`): a Multiply highlight over a black/white split, a filled+stroked
/// Square, a Line, and a filled Polygon. Drives the synthetic-form-stream
/// annotation path through both backends.
fn markup_annots_pdf() -> Vec<u8> {
    let content: &[u8] = b"1 1 1 rg 0 0 200 200 re f\n0 0 0 rg 0 0 100 200 re f";
    assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << >> /Annots [5 0 R 6 0 R 7 0 R 8 0 R] >>"
            .to_vec(),
        stream_obj("", content),
        b"<< /Type /Annot /Subtype /Highlight /Rect [20 150 180 175] /F 4 \
          /QuadPoints [20 175 180 175 20 150 180 150] /C [1 1 0] >>"
            .to_vec(),
        b"<< /Type /Annot /Subtype /Square /Rect [30 30 90 90] /F 4 \
          /IC [0 1 0] /C [1 0 0] /BS << /W 4 >> >>"
            .to_vec(),
        b"<< /Type /Annot /Subtype /Line /Rect [110 25 190 95] /F 4 \
          /L [110 30 190 90] /C [0 0 1] /BS << /W 3 >> >>"
            .to_vec(),
        b"<< /Type /Annot /Subtype /Polygon /Rect [110 105 190 145] /F 4 \
          /Vertices [120 110 180 110 150 140] /IC [1 0 1] >>"
            .to_vec(),
    ])
}

/// Like `compare_backends`, but wires the page's annotations (so generated
/// markup appearances are painted by both backends).
fn compare_backends_with_annots(pdf: Vec<u8>) -> Option<BackendDiff> {
    let doc = PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let content = doc.page_content_bytes(&page).expect("content bytes");
    let mut images = ImageCache::new();
    let annots = doc.page_annotations(&page);
    let dl = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images)
        .with_annotations(&annots)
        .interpret(&content);

    let gpu = match zpdf::gpu::WgpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, SCALE)
    {
        Ok(t) => t,
        Err(e) => {
            eprintln!("skipping GPU markup acceptance (no adapter?): {e}");
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
fn gpu_matches_cpu_on_markup_annotations() {
    match compare_backends_with_annots(markup_annots_pdf()) {
        None => eprintln!("GPU markup acceptance skipped (no adapter)."),
        Some((pct, cdim, gdim)) => {
            assert_eq!(cdim, gdim, "dimension mismatch {cdim:?} vs {gdim:?}");
            println!("  markup_annotations: {pct:.3}% differing");
            assert!(
                pct < MAX_DIFF_PCT,
                "markup GPU vs CPU {pct:.3}% exceeds {MAX_DIFF_PCT}%"
            );
        }
    }
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

/// Guards against `text_outline_repeated` passing `gpu_matches_cpu_on_corpus`
/// as a false positive (e.g. a blank page trivially diffs at 0%, or the atlas
/// path silently never engages and the case is really just re-testing
/// vector-fill): (1) the CPU reference must contain real ink — some pixels
/// darker than the white background — proving the glyphs actually resolved
/// and rendered, not a blank page; (2) forcing `ZPDF_GPU_GLYPH_ATLAS=0` (pure
/// vector-fill, mirroring every other text run before this phase) must
/// produce *some* pixel difference from the default atlas-enabled render —
/// proving the atlas code path is genuinely engaged and not a no-op, since
/// two different rasterizers (tiny-skia analytic AA vs lyon MSAA tessellation)
/// essentially never agree on every single pixel of a real glyph's AA edge.
#[test]
fn glyph_atlas_path_is_genuinely_exercised() {
    let pdf = outline_text_pdf(37.3); // fractional: see `outline_text_pdf` doc comment
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
    let ink_pixels = (0..cpu.data.len() / 4)
        .filter(|&i| cpu.data[i * 4] < 200) // red channel well below white
        .count();
    assert!(
        ink_pixels > 200,
        "CPU reference has almost no ink ({ink_pixels} px) — glyphs likely \
         failed to resolve to real outlines; this test's premise is broken"
    );

    // SAFETY-ISH: std::env::set_var in a test is racy against other tests
    // touching this var if run in parallel; this is the only test that does,
    // and it restores the var afterward.
    let render = || {
        zpdf::gpu::WgpuRenderer::new()
            .with_fonts(&fonts)
            .with_images(&images)
            .render_display_list(&dl, SCALE)
    };
    unsafe {
        std::env::set_var("ZPDF_GPU_GLYPH_ATLAS", "0");
    }
    let fallback = render();
    unsafe {
        std::env::remove_var("ZPDF_GPU_GLYPH_ATLAS");
    }
    let atlas = render();

    let (Ok(fallback), Ok(atlas)) = (fallback, atlas) else {
        eprintln!("glyph_atlas_path_is_genuinely_exercised skipped (no adapter).");
        return;
    };
    assert_eq!(
        (fallback.width, fallback.height),
        (atlas.width, atlas.height)
    );
    let differs = fallback
        .data
        .iter()
        .zip(atlas.data.iter())
        .any(|(a, b)| a != b);
    assert!(
        differs,
        "atlas-enabled and forced-fallback GPU renders are byte-identical — \
         the glyph atlas path does not appear to be engaging"
    );
}
