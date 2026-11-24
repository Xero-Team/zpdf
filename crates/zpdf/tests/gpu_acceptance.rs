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
fn gpu_clear_matches_cpu_and_matching_fill_exactly() {
    use zpdf::display_list::{Color, FillRule, Paint, Path, RenderCommand};
    use zpdf::Rect;
    use zpdf_render::PageRenderInfo;

    let mut renderer = zpdf::gpu::WgpuRenderer::new();
    match renderer.adapter_identity() {
        Ok(_) => {}
        Err(zpdf::gpu::WgpuRenderError::NoAdapter) => {
            eprintln!("skipping GPU clear-colour acceptance (no adapter)");
            return;
        }
        Err(e) => panic!("initialize GPU clear-colour acceptance: {e}"),
    }
    for (color, expected) in [
        (Color::gray(0.0), [0, 0, 0, 255]),
        (Color::gray(0.25), [64, 64, 64, 255]),
        (Color::gray(0.5), [128, 128, 128, 255]),
        (Color::gray(0.75), [191, 191, 191, 255]),
        (Color::gray(1.0), [255, 255, 255, 255]),
        (Color::rgb(0.5, 0.25, 0.75), [128, 64, 191, 255]),
    ] {
        let info = PageRenderInfo {
            page_rect: Rect::new(0.0, 0.0, 4.0, 2.0),
            scale: 1.0,
            background: color,
        };
        let mut path = Path::new();
        path.rect(Rect::new(2.0, 0.0, 4.0, 2.0));
        let fill = RenderCommand::FillPath {
            path,
            rule: FillRule::NonZero,
            paint: Paint::Solid(color),
            alpha: 1.0,
            overprint: None,
        };
        let mut cpu = zpdf::cpu::CpuRenderer::new();
        cpu.begin_page(&info).expect("CPU begin page");
        cpu.execute(&fill).expect("CPU matching fill");
        let cpu = cpu.end_page().expect("CPU end page");
        renderer.begin_page(&info).expect("GPU begin page");
        renderer.execute(&fill).expect("GPU matching fill");
        let gpu = renderer.end_page().expect("GPU end page");
        assert_eq!((cpu.width, cpu.height), (4, 2));
        assert_eq!((gpu.width, gpu.height), (4, 2));
        // x=0 samples the clear; x=3 samples the matching rectangle.
        for x in [0, 3] {
            let offset = x * 4;
            assert_eq!(
                &cpu.data[offset..offset + 4],
                &expected,
                "CPU {color:?} x={x}"
            );
            assert_eq!(
                &gpu.data[offset..offset + 4],
                &expected,
                "GPU {color:?} x={x}"
            );
        }
    }

    // Blank-page translucent clears premultiply clamped RGB by clamped alpha
    // in floating point before rounding once to bytes, matching CPU Pixmap::fill.
    for (color, expected) in [
        (Color::rgba(0.1, 0.0, 0.0, 254.0 / 255.0), [25, 0, 0, 254]),
        (Color::rgba(1.0, 0.5, 0.0, 0.5), [128, 64, 0, 128]),
        (Color::rgba(1.0, 0.5, 0.25, 0.0), [0, 0, 0, 0]),
    ] {
        let info = PageRenderInfo {
            page_rect: Rect::new(0.0, 0.0, 4.0, 2.0),
            scale: 1.0,
            background: color,
        };
        let mut cpu = zpdf::cpu::CpuRenderer::new();
        cpu.begin_page(&info).expect("CPU begin page");
        let cpu = cpu.end_page().expect("CPU end page");
        renderer.begin_page(&info).expect("GPU begin page");
        let gpu = renderer.end_page().expect("GPU end page");
        assert_eq!((cpu.width, cpu.height), (4, 2));
        assert_eq!((gpu.width, gpu.height), (4, 2));
        assert_eq!(cpu.data.len(), 4 * 2 * 4);
        assert_eq!(gpu.data.len(), 4 * 2 * 4);
        for px in 0..(4 * 2) {
            let offset = px * 4;
            assert_eq!(
                &cpu.data[offset..offset + 4],
                &expected,
                "CPU translucent clear {color:?} px={px}"
            );
            assert_eq!(
                &gpu.data[offset..offset + 4],
                &expected,
                "GPU translucent clear {color:?} px={px}"
            );
        }
    }
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

/// Regression: a fill covering 65×65 = 4,225 opaque tiling cells (just above
/// `MAX_GPU_TILES = 4,096`) and a dense patterned stroke must paint via a
/// repeating GPU-rasterized cell texture instead of leaving the region blank,
/// and match the CPU oracle within <1%.
#[test]
fn gpu_dense_tiling_pattern_above_4096_tiles_matches_cpu() {
    // 4×4 pt opaque two-color cell (red left half [0..2], blue right half [2..4]),
    // tiled over a 260×260 pt rect = 65×65 = 4,225 visible tiles.
    let cell: &[u8] = b"1 0 0 rg 0 0 2 4 re f\n0 0 1 rg 2 0 2 4 re f";
    let pat = "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
               /BBox [0 0 4 4] /XStep 4 /YStep 4 /Resources << >>";
    // Also draw a dense patterned stroke (width 20, spanning 260 pt on a 2×2
    // dense pattern, 130×10 = 1,300 or with 1×1 pattern -> 260×20 = 5,200 tiles!).
    let fine_cell: &[u8] = b"0 1 0 rg 0 0 0.5 1 re f\n1 1 0 rg 0.5 0 0.5 1 re f";
    let fine_pat = "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
                    /BBox [0 0 1 1] /XStep 1 /YStep 1 /Resources << >>";
    let content: &[u8] = b"/Pattern cs /P0 scn 0 0 260 260 re f\n\
                           /Pattern CS /P1 SCN 20 w 0 130 m 260 130 l S";
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 260 260] /Contents 4 0 R \
          /Resources << /Pattern << /P0 5 0 R /P1 6 0 R >> >> >>"
            .to_vec(),
        stream_obj("", content),
        stream_obj(pat, cell),
        stream_obj(fine_pat, fine_cell),
    ]);

    let doc = PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let content_bytes = doc.page_content_bytes(&page).expect("content bytes");
    let mut images = ImageCache::new();
    let dl = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images)
        .interpret(&content_bytes);

    let gpu = match zpdf::gpu::WgpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, SCALE)
    {
        Ok(t) => t,
        Err(e) => {
            eprintln!("skipping GPU dense tiling acceptance (no adapter?): {e}");
            return;
        }
    };
    let cpu = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, SCALE)
        .expect("cpu render");

    assert_eq!((cpu.width, cpu.height), (gpu.width, gpu.height));
    let px = |data: &[u8], x: f64, y: f64| -> [u8; 3] {
        let ix = (x * SCALE as f64) as u32;
        let iy = ((260.0 - y) * SCALE as f64) as u32;
        let off = ((iy * gpu.width + ix) * 4) as usize;
        [data[off], data[off + 1], data[off + 2]]
    };

    // Sample opposite corners of the 65×65 fill (must NOT be blank white):
    // tile (0,0) and tile (64,64) both have red left half and blue right half.
    let g_red_bl = px(&gpu.data, 1.0, 2.0);
    let g_blue_bl = px(&gpu.data, 3.0, 2.0);
    let g_red_tr = px(&gpu.data, 257.0, 258.0);
    let g_blue_tr = px(&gpu.data, 259.0, 258.0);
    assert!(
        g_red_bl[0] > 200 && g_red_bl[1] < 40 && g_red_bl[2] < 40,
        "GPU tile (0,0) left half must be red, got {g_red_bl:?}"
    );
    assert!(
        g_blue_bl[0] < 40 && g_blue_bl[1] < 40 && g_blue_bl[2] > 200,
        "GPU tile (0,0) right half must be blue, got {g_blue_bl:?}"
    );
    assert!(
        g_red_tr[0] > 200 && g_red_tr[1] < 40 && g_red_tr[2] < 40,
        "GPU tile (64,64) left half must be red, got {g_red_tr:?}"
    );
    assert!(
        g_blue_tr[0] < 40 && g_blue_tr[1] < 40 && g_blue_tr[2] > 200,
        "GPU tile (64,64) right half must be blue, got {g_blue_tr:?}"
    );

    // Sample inside the dense patterned stroke band (y=130): green left / yellow right.
    let g_stroke_l = px(&gpu.data, 100.25, 130.0);
    let g_stroke_r = px(&gpu.data, 100.75, 130.0);
    assert!(
        g_stroke_l[1] > 200 && g_stroke_l[2] < 40,
        "GPU dense patterned stroke must paint green/yellow, got {g_stroke_l:?}"
    );
    assert!(
        g_stroke_r[0] > 200 && g_stroke_r[1] > 200 && g_stroke_r[2] < 40,
        "GPU dense patterned stroke right half must paint yellow, got {g_stroke_r:?}"
    );

    // Overall GPU vs CPU parity across the entire 520×520 raster.
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
    let pct = diff as f64 / total as f64 * 100.0;
    println!("  dense_tiling_65x65: {pct:.3}% differing");
    assert!(
        pct < MAX_DIFF_PCT,
        "dense 65x65 tiling GPU vs CPU {pct:.3}% exceeds {MAX_DIFF_PCT}%"
    );
}

/// Regression: anisotropic tiling pattern `/Matrix [4 0 0 1 0 0]` at 72 DPI
/// must transform stroked outlines (and stroke clips) in pattern space on wgpu,
/// producing a 4px-wide vertical stroke and a 1px-tall horizontal stroke
/// (matching CPU, rather than scaling both widths by sqrt(det)=2px).
#[test]
fn gpu_anisotropic_tiling_stroke_and_clip_preserve_directional_widths() {
    // 10×10 cell mapped by [4 0 0 1 0 0] -> 40×10 px per tile at scale=1.0:
    //  - Red 1pt vertical stroke at x=5 -> 4px wide on screen
    //  - Blue 1pt horizontal stroke at y=5 -> 1px tall on screen
    //  - Green nested-pattern 1pt vertical stroke at x=2 -> tests PushClipStroke
    //    transformed in pattern space (4px wide).
    let inner_green = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 10 10] /XStep 10 /YStep 10 /Resources << >>",
        b"0 1 0 rg 0 0 10 10 re f",
    );
    let cell = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 10 10] /XStep 10 /YStep 10 \
         /Matrix [4 0 0 1 0 0] \
         /Resources << /Pattern << /PG 6 0 R >> >>",
        b"1 w\n\
          1 0 0 RG 5 0 m 5 10 l S\n\
          0 0 1 RG 0 5 m 10 5 l S\n\
          /Pattern CS /PG SCN 2 0 m 2 10 l S",
    );
    let content = stream_obj("", b"/Pattern cs /P0 scn 0 0 80 40 re f");
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 80 40] /Contents 4 0 R \
          /Resources << /Pattern << /P0 5 0 R >> >> >>"
            .to_vec(), // 3
        content,                                       // 4
        cell,                                          // 5
        inner_green,                                   // 6
    ]);

    let doc = PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let content_bytes = doc.page_content_bytes(&page).expect("content bytes");
    let mut images = ImageCache::new();
    let dl = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images)
        .interpret(&content_bytes);

    let gpu = match zpdf::gpu::WgpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, 1.0)
    {
        Ok(t) => t,
        Err(e) => {
            eprintln!("skipping GPU anisotropic stroke test (no adapter?): {e}");
            return;
        }
    };
    let cpu = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, 1.0)
        .expect("cpu render");

    assert_eq!((cpu.width, cpu.height), (gpu.width, gpu.height));
    let px = |data: &[u8], x: u32, y_pdf: u32| -> [u8; 3] {
        let iy = 39 - y_pdf;
        let off = ((iy * gpu.width + x) * 4) as usize;
        [data[off], data[off + 1], data[off + 2]]
    };

    // Integrate red vertical stroke across x in [14..26] at y=2 (away from blue y=5).
    let mut red_ink_px = 0.0f64;
    for x in 14..26 {
        let c = px(&gpu.data, x, 2);
        red_ink_px += (255 - c[1]) as f64 / 255.0;
    }
    // Integrate green stroke-clipped vertical stroke across x in [2..14] at y=2.
    let mut green_ink_px = 0.0f64;
    for x in 2..14 {
        let c = px(&gpu.data, x, 2);
        green_ink_px += (255 - c[0]) as f64 / 255.0;
    }
    // Integrate blue horizontal stroke down y in [0..10] at x=32 (away from vertical strokes).
    let mut blue_ink_px = 0.0f64;
    for y in 0..10 {
        let c = px(&gpu.data, 32, y);
        blue_ink_px += (255 - c[0]) as f64 / 255.0;
    }
    assert!(
        (3.5..=4.5).contains(&red_ink_px),
        "GPU anisotropic vertical stroke must integrate to ~4px wide, measured {red_ink_px:.2}px"
    );
    assert!(
        (3.5..=4.5).contains(&green_ink_px),
        "GPU anisotropic vertical stroke clip must integrate to ~4px wide, measured {green_ink_px:.2}px"
    );
    assert!(
        (0.8..=1.2).contains(&blue_ink_px),
        "GPU anisotropic horizontal stroke must integrate to ~1px tall, measured {blue_ink_px:.2}px"
    );

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
    let pct = diff as f64 / total as f64 * 100.0;
    println!("  anisotropic_tiling_stroke: {pct:.3}% differing");
    assert!(
        pct < MAX_DIFF_PCT,
        "anisotropic tiling stroke GPU vs CPU {pct:.3}% exceeds {MAX_DIFF_PCT}%"
    );
}

/// Regression (ISO 32000-1 §11.6.7): a dense (> 4,096 tiles, synthetic-tile GPU
/// path) tiling pattern whose cell paints an opaque white mark and a cyan mark
/// with `/BM /Multiply` must composite against the live page backdrop across
/// distinct background colors (blue left, yellow right) instead of baking white
/// into an opaque tile texture.
#[test]
fn gpu_dense_tiling_multiply_preserves_backdrop_across_backgrounds() {
    let gs_mul = b"<< /Type /ExtGState /BM /Multiply >>".to_vec();
    // 4×4 cell tiled across 260×260 -> 65×65 = 4,225 tiles (> MAX_GPU_TILES = 4,096).
    let cell = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 4 4] /XStep 4 /YStep 4 \
         /Resources << /ExtGState << /GM 6 0 R >> >>",
        b"/GM gs 1 1 1 rg 0 0 2 4 re f\n0 1 1 rg 2 0 2 4 re f",
    );
    let content = stream_obj(
        "",
        b"0 0 1 rg 0 0 130 260 re f\n\
          1 1 0 rg 130 0 130 260 re f\n\
          /Pattern cs /P0 scn 0 0 260 260 re f",
    );
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 260 260] /Contents 4 0 R \
          /Resources << /Pattern << /P0 5 0 R >> >> >>"
            .to_vec(), // 3
        content,                                       // 4
        cell,                                          // 5
        gs_mul,                                        // 6
    ]);

    let doc = PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let content_bytes = doc.page_content_bytes(&page).expect("content bytes");
    let mut images = ImageCache::new();
    let dl = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images)
        .interpret(&content_bytes);

    let gpu = match zpdf::gpu::WgpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, SCALE)
    {
        Ok(t) => t,
        Err(e) => {
            eprintln!("skipping GPU dense Multiply tiling test (no adapter?): {e}");
            return;
        }
    };
    let cpu = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, SCALE)
        .expect("cpu render");

    assert_eq!((cpu.width, cpu.height), (gpu.width, gpu.height));
    let px = |data: &[u8], x: f64, y: f64| -> [u8; 3] {
        let ix = (x * SCALE as f64) as u32;
        let iy = ((260.0 - y) * SCALE as f64) as u32;
        let off = ((iy * gpu.width + ix) * 4) as usize;
        [data[off], data[off + 1], data[off + 2]]
    };

    // Blue left background: white Multiply (x=21 -> cell-x=1) stays blue.
    let blue_white = px(&gpu.data, 21.0, 130.0);
    assert!(
        blue_white[0] < 30 && blue_white[1] < 30 && blue_white[2] > 220,
        "GPU dense white Multiply over blue must stay blue, got {blue_white:?}"
    );
    // Yellow right background: white Multiply (x=141 -> cell-x=1) stays yellow.
    let yellow_white = px(&gpu.data, 141.0, 130.0);
    assert!(
        yellow_white[0] > 220 && yellow_white[1] > 220 && yellow_white[2] < 30,
        "GPU dense white Multiply over yellow must stay yellow, got {yellow_white:?}"
    );
    // Yellow right background: cyan Multiply (x=143 -> cell-x=3) becomes green.
    let yellow_cyan = px(&gpu.data, 143.0, 130.0);
    assert!(
        yellow_cyan[0] < 30 && yellow_cyan[1] > 220 && yellow_cyan[2] < 30,
        "GPU dense cyan Multiply over yellow must turn green, got {yellow_cyan:?}"
    );

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
    let pct = diff as f64 / total as f64 * 100.0;
    println!("  dense_tiling_multiply_65x65: {pct:.3}% differing");
    assert!(
        pct < MAX_DIFF_PCT,
        "dense Multiply tiling GPU vs CPU {pct:.3}% exceeds {MAX_DIFF_PCT}%"
    );
}

/// Regression (ISO 32000-1 §11.6.7): painting a tiling pattern at 50% object
/// opacity (`/ca 0.5`) whose cell contains two overlapping opaque black
/// `/BM /Multiply` fills must preserve the live backdrop while evaluating the
/// cell and apply the 50% opacity ONCE (~128 over white, NOT ~64), across BOTH
/// GPU replay paths (<= 4,096 vector tiles on the left; > 4,096 dense
/// synthetic-tile segments on the right) and matching CPU.
#[test]
fn gpu_translucent_overlapping_multiply_tiling_applies_outer_opacity_once() {
    let gs_mul = b"<< /Type /ExtGState /BM /Multiply >>".to_vec();
    let gs_half = b"<< /Type /ExtGState /ca 0.5 >>".to_vec();
    // Vector path (left half 130×260 with 20×20 cell -> ~7×13 = 91 tiles <= 4,096).
    let pat_vec = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 20 20] /XStep 20 /YStep 20 \
         /Resources << /ExtGState << /GM 7 0 R >> >>",
        b"/GM gs\n\
          0 0 0 rg 0 0 10 20 re f\n\
          0 0 0 rg 0 0 10 20 re f\n\
          1 1 1 rg 10 0 10 20 re f",
    );
    // Dense synthetic-tile path (right half 130×260 with 2×2 cell -> 65×130 = 8,450 tiles > 4,096).
    let pat_dense = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 2 2] /XStep 2 /YStep 2 \
         /Resources << /ExtGState << /GM 7 0 R >> >>",
        b"/GM gs\n\
          0 0 0 rg 0 0 1 2 re f\n\
          0 0 0 rg 0 0 1 2 re f\n\
          1 1 1 rg 1 0 1 2 re f",
    );
    // Bottom half [0..130]: blue background; top half [130..260]: white background.
    let content = stream_obj(
        "",
        b"0 0 1 rg 0 0 260 130 re f\n\
          q /GH gs /Pattern cs /PV scn 0 0 130 260 re f Q\n\
          q /GH gs /Pattern cs /PD scn 130 0 130 260 re f Q",
    );
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 260 260] /Contents 4 0 R \
          /Resources << /Pattern << /PV 5 0 R /PD 6 0 R >> /ExtGState << /GH 8 0 R >> >> >>"
            .to_vec(), // 3
        content,                                       // 4
        pat_vec,                                       // 5
        pat_dense,                                     // 6
        gs_mul,                                        // 7
        gs_half,                                       // 8
    ]);

    let doc = PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let content_bytes = doc.page_content_bytes(&page).expect("content bytes");
    let mut images = ImageCache::new();
    let dl = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images)
        .interpret(&content_bytes);

    let gpu = match zpdf::gpu::WgpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, SCALE)
    {
        Ok(t) => t,
        Err(e) => {
            eprintln!("skipping GPU translucent overlapping Multiply test (no adapter?): {e}");
            return;
        }
    };
    let cpu = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, SCALE)
        .expect("cpu render");

    assert_eq!((cpu.width, cpu.height), (gpu.width, gpu.height));
    let px = |data: &[u8], x: f64, y: f64| -> [u8; 3] {
        let ix = (x * SCALE as f64) as u32;
        let iy = ((260.0 - y) * SCALE as f64) as u32;
        let off = ((iy * gpu.width + ix) * 4) as usize;
        [data[off], data[off + 1], data[off + 2]]
    };

    // Left (GPU vector replay): overlapping black Multiply over white at 50% -> ~[128, 128, 128].
    let vec_white = px(&gpu.data, 25.0, 200.0);
    assert!(
        vec_white.iter().all(|&c| (122..=134).contains(&c)),
        "GPU vector overlapping Multiply over white at 50% should be ~128, got {vec_white:?}"
    );
    // Left (GPU vector replay): white Multiply over blue at 50% -> stays blue.
    let vec_blue_id = px(&gpu.data, 35.0, 60.0);
    assert!(
        vec_blue_id[0] < 10 && vec_blue_id[1] < 10 && vec_blue_id[2] > 245,
        "GPU vector white Multiply over blue at 50% must stay blue, got {vec_blue_id:?}"
    );
    // Left (GPU vector replay): overlapping black Multiply over blue at 50% -> ~[0, 0, 128].
    let vec_blue = px(&gpu.data, 25.0, 60.0);
    assert!(
        vec_blue[0] < 10 && vec_blue[1] < 10 && (122..=134).contains(&vec_blue[2]),
        "GPU vector overlapping Multiply over blue at 50% should be ~[0, 0, 128], got {vec_blue:?}"
    );

    // Right (GPU dense synthetic-tile segments): overlapping black Multiply over white at 50% -> ~[128, 128, 128].
    let dense_white = px(&gpu.data, 140.5, 200.0);
    assert!(
        dense_white.iter().all(|&c| (122..=134).contains(&c)),
        "GPU dense overlapping Multiply over white at 50% should be ~128, got {dense_white:?}"
    );
    // Right (GPU dense synthetic-tile segments): white Multiply over blue at 50% -> stays blue.
    let dense_blue_id = px(&gpu.data, 141.5, 60.0);
    assert!(
        dense_blue_id[0] < 10 && dense_blue_id[1] < 10 && dense_blue_id[2] > 245,
        "GPU dense white Multiply over blue at 50% must stay blue, got {dense_blue_id:?}"
    );
    // Right (GPU dense synthetic-tile segments): overlapping black Multiply over blue at 50% -> ~[0, 0, 128].
    let dense_blue = px(&gpu.data, 140.5, 60.0);
    assert!(
        dense_blue[0] < 10 && dense_blue[1] < 10 && (122..=134).contains(&dense_blue[2]),
        "GPU dense overlapping Multiply over blue at 50% should be ~[0, 0, 128], got {dense_blue:?}"
    );

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
    let pct = diff as f64 / total as f64 * 100.0;
    println!("  translucent_overlapping_multiply_tiling: {pct:.3}% differing");
    assert!(
        pct < MAX_DIFF_PCT,
        "translucent overlapping Multiply tiling GPU vs CPU {pct:.3}% exceeds {MAX_DIFF_PCT}%"
    );
}

/// Regression: an overlapping cell twice as wide as its horizontal step
/// (`/BBox [0 0 8 4] /XStep 4 /YStep 4`, opaque 50%-gray `/BM /Multiply`) covers
/// every interior pixel twice (`255 * 0.5 * 0.5 ≈ 64` on white) and must agree
/// below (10×10 = 100 vector tiles) and above (65×65 = 4,225 dense synthetic
/// tiles) the 4,096 tile cap on both GPU and CPU.
#[test]
fn gpu_overlapping_multiply_cell_agrees_above_and_below_4096_tile_cap() {
    let gs_mul = b"<< /Type /ExtGState /BM /Multiply >>".to_vec();
    let cell = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 8 4] /XStep 4 /YStep 4 \
         /Resources << /ExtGState << /GM 6 0 R >> >>",
        b"/GM gs 0.5 0.5 0.5 rg 0 0 8 4 re f",
    );
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

    let doc = PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let content_bytes = doc.page_content_bytes(&page).expect("content bytes");
    let mut images = ImageCache::new();
    let dl = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images)
        .interpret(&content_bytes);

    let gpu = match zpdf::gpu::WgpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, 1.0)
    {
        Ok(t) => t,
        Err(e) => {
            eprintln!("skipping GPU overlapping Multiply cap parity test (no adapter?): {e}");
            return;
        }
    };
    let cpu = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, 1.0)
        .expect("cpu render");

    assert_eq!((cpu.width, cpu.height), (gpu.width, gpu.height));
    let px = |data: &[u8], x: u32, y_pdf: u32| -> [u8; 3] {
        let iy = 259 - y_pdf;
        let off = ((iy * gpu.width + x) * 4) as usize;
        [data[off], data[off + 1], data[off + 2]]
    };

    let gpu_below = px(&gpu.data, 22, 22);
    let gpu_above = px(&gpu.data, 190, 130);
    assert!(
        gpu_below.iter().all(|&c| (58..=70).contains(&c)),
        "GPU below-cap overlapping 50%-gray Multiply must be ~64 on white, got {gpu_below:?}"
    );
    assert!(
        gpu_above.iter().all(|&c| (58..=70).contains(&c)),
        "GPU above-cap overlapping 50%-gray Multiply must be ~64 on white (not ~128), got {gpu_above:?}"
    );
    for ch in 0..3 {
        assert!(
            (gpu_below[ch] as i32 - gpu_above[ch] as i32).abs() <= 2,
            "GPU interior pixel must agree below ({gpu_below:?}) and above ({gpu_above:?}) cap"
        );
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
    let pct = diff as f64 / total as f64 * 100.0;
    println!("  overlapping_multiply_above_below_cap: {pct:.3}% differing");
    assert!(
        pct < MAX_DIFF_PCT,
        "overlapping Multiply above/below cap GPU vs CPU {pct:.3}% exceeds {MAX_DIFF_PCT}%"
    );
}

/// Regression: a dense (> 4,096 tiles) outer tiling pattern whose cell fills
/// with an inner `Multiply` pattern must composite cleanly against distinct
/// page backgrounds on both GPU and CPU.
#[test]
fn gpu_dense_nested_multiply_tiling_preserves_backdrop() {
    let gs_mul = b"<< /Type /ExtGState /BM /Multiply >>".to_vec();
    let p_inner = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 2 2] /XStep 2 /YStep 2 \
         /Resources << /ExtGState << /GM 7 0 R >> >>",
        b"/GM gs 1 1 1 rg 0 0 1 2 re f\n0 1 1 rg 1 0 1 2 re f",
    );
    let p_outer = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 4 4] /XStep 4 /YStep 4 \
         /Resources << /Pattern << /PI 5 0 R >> >>",
        b"/Pattern cs /PI scn 0 0 4 4 re f",
    );
    let content = stream_obj(
        "",
        b"0 0 1 rg 0 0 130 260 re f\n\
          1 1 0 rg 130 0 130 260 re f\n\
          /Pattern cs /PO scn 0 0 260 260 re f",
    );
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 260 260] /Contents 4 0 R \
          /Resources << /Pattern << /PO 6 0 R >> >> >>"
            .to_vec(), // 3
        content,                                       // 4
        p_inner,                                       // 5
        p_outer,                                       // 6
        gs_mul,                                        // 7
    ]);

    let doc = PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let content_bytes = doc.page_content_bytes(&page).expect("content bytes");
    let mut images = ImageCache::new();
    let dl = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images)
        .interpret(&content_bytes);

    let gpu = match zpdf::gpu::WgpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, SCALE)
    {
        Ok(t) => t,
        Err(e) => {
            eprintln!("skipping GPU dense nested Multiply test (no adapter?): {e}");
            return;
        }
    };
    let cpu = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, SCALE)
        .expect("cpu render");

    assert_eq!((cpu.width, cpu.height), (gpu.width, gpu.height));
    let px = |data: &[u8], x: f64, y: f64| -> [u8; 3] {
        let ix = (x * SCALE as f64) as u32;
        let iy = ((260.0 - y) * SCALE as f64) as u32;
        let off = ((iy * gpu.width + ix) * 4) as usize;
        [data[off], data[off + 1], data[off + 2]]
    };

    let y_white = px(&gpu.data, 140.5, 130.0);
    assert!(
        y_white[0] > 220 && y_white[1] > 220 && y_white[2] < 30,
        "GPU dense nested white Multiply over yellow must stay yellow, got {y_white:?}"
    );
    let y_cyan = px(&gpu.data, 141.5, 130.0);
    assert!(
        y_cyan[0] < 30 && y_cyan[1] > 220 && y_cyan[2] < 30,
        "GPU dense nested cyan Multiply over yellow must turn green, got {y_cyan:?}"
    );
    let b_cyan = px(&gpu.data, 21.5, 130.0);
    assert!(
        b_cyan[0] < 30 && b_cyan[1] < 30 && b_cyan[2] > 220,
        "GPU dense nested cyan Multiply over blue must stay blue, got {b_cyan:?}"
    );

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
    let pct = diff as f64 / total as f64 * 100.0;
    println!("  dense_nested_multiply_65x65: {pct:.3}% differing");
    assert!(
        pct < MAX_DIFF_PCT,
        "dense nested Multiply tiling GPU vs CPU {pct:.3}% exceeds {MAX_DIFF_PCT}%"
    );
}

/// Regression: opaque outer soft mask on a backdrop-dependent tiling pattern
/// (white + cyan `Multiply` over blue and yellow backdrops) must preserve the
/// live page backdrop on both CPU and GPU through the interpreter
/// (ISO 32000-1 §11.6.7).
#[test]
fn gpu_masked_backdrop_dependent_tiling_preserves_page_backdrop() {
    let gs_mul = b"<< /Type /ExtGState /BM /Multiply >>".to_vec();
    let mask_form = stream_obj(
        "/Type /XObject /Subtype /Form /BBox [0 0 100 100] \
         /Group << /S /Transparency /CS /DeviceRGB >>",
        b"1 1 1 rg 0 0 50 100 re f",
    );
    let gs_smask =
        b"<< /Type /ExtGState /SMask << /Type /Mask /S /Luminosity /G 7 0 R >> >>".to_vec();
    let cell = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 20 20] /XStep 20 /YStep 20 \
         /Resources << /ExtGState << /GM 6 0 R >> >>",
        b"/GM gs 1 1 1 rg 0 0 10 20 re f\n0 1 1 rg 10 0 10 20 re f",
    );
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

    let doc = PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let content_bytes = doc.page_content_bytes(&page).expect("content bytes");
    let mut images = ImageCache::new();
    let dl = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images)
        .interpret(&content_bytes);

    let gpu = match zpdf::gpu::WgpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, 1.0)
    {
        Ok(t) => t,
        Err(e) => {
            eprintln!("skipping GPU masked tiling test (no adapter?): {e}");
            return;
        }
    };
    let cpu = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, 1.0)
        .expect("cpu render");

    let px = |data: &[u8], x: u32, y_pdf: u32| -> [u8; 3] {
        let iy = 99 - y_pdf;
        let off = ((iy * gpu.width + x) * 4) as usize;
        [data[off], data[off + 1], data[off + 2]]
    };
    let blue_px = px(&gpu.data, 5, 75);
    assert!(
        blue_px[0] < 10 && blue_px[1] < 10 && blue_px[2] > 245,
        "GPU opaque-masked white Multiply over blue must stay blue, got {blue_px:?}"
    );
    let green_px = px(&gpu.data, 15, 25);
    assert!(
        green_px[0] < 10 && green_px[1] > 245 && green_px[2] < 10,
        "GPU opaque-masked cyan Multiply over yellow must turn green, got {green_px:?}"
    );
    let masked_out = px(&gpu.data, 75, 25);
    assert!(
        masked_out[0] > 245 && masked_out[1] > 245 && masked_out[2] < 10,
        "GPU zero-masked area over yellow must stay yellow, got {masked_out:?}"
    );

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
    let pct = diff as f64 / total as f64 * 100.0;
    println!("  masked_backdrop_tiling: {pct:.3}% differing");
    assert!(
        pct < MAX_DIFF_PCT,
        "masked backdrop tiling GPU vs CPU {pct:.3}% exceeds {MAX_DIFF_PCT}%"
    );
}

/// Regression: dense (>4,096 tiles) outer cell painting a translucent (50%)
/// inner pattern with two white `Multiply` marks must preserve a blue backdrop
/// on BOTH sides of the tile cap on GPU (matching CPU).
#[test]
fn gpu_dense_translucent_multi_segment_nested_tiling_preserves_backdrop() {
    let gs_mul = b"<< /Type /ExtGState /BM /Multiply >>".to_vec();
    let gs_half = b"<< /Type /ExtGState /ca 0.5 >>".to_vec();
    let p_inner = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 2 2] /XStep 2 /YStep 2 \
         /Resources << /ExtGState << /GM 7 0 R >> >>",
        b"/GM gs 1 1 1 rg 0 0 2 2 re f\n/GM gs 1 1 1 rg 0 0 2 2 re f",
    );
    let p_outer = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 4 4] /XStep 4 /YStep 4 \
         /Resources << /Pattern << /PI 5 0 R >> /ExtGState << /GH 8 0 R >> >>",
        b"/GH gs /Pattern cs /PI scn 0 0 4 4 re f",
    );
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

    let doc = PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let content_bytes = doc.page_content_bytes(&page).expect("content bytes");
    let mut images = ImageCache::new();
    let dl = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images)
        .interpret(&content_bytes);

    let gpu = match zpdf::gpu::WgpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, 1.0)
    {
        Ok(t) => t,
        Err(e) => {
            eprintln!("skipping GPU translucent nested tiling test (no adapter?): {e}");
            return;
        }
    };
    let cpu = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, 1.0)
        .expect("cpu render");

    let px = |data: &[u8], x: u32, y_pdf: u32| -> [u8; 3] {
        let iy = 259 - y_pdf;
        let off = ((iy * gpu.width + x) * 4) as usize;
        [data[off], data[off + 1], data[off + 2]]
    };
    let below = px(&gpu.data, 20, 20);
    let above = px(&gpu.data, 190, 130);
    assert!(
        below[0] < 10 && below[1] < 10 && below[2] > 245,
        "GPU below-cap translucent nested white Multiply over blue must stay blue, got {below:?}"
    );
    assert!(
        above[0] < 10 && above[1] < 10 && above[2] > 245,
        "GPU above-cap translucent nested white Multiply over blue must stay blue, got {above:?}"
    );

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
    let pct = diff as f64 / total as f64 * 100.0;
    println!("  dense_translucent_nested_white_multiply: {pct:.3}% differing");
    assert!(
        pct < MAX_DIFF_PCT,
        "dense translucent nested white Multiply GPU vs CPU {pct:.3}% exceeds {MAX_DIFF_PCT}%"
    );
}

/// Regression: 2 overlapping copies × 33 segments (= 66 > 64) on the dense GPU
/// path must still keep both copies separate so the spanning 50%-gray Multiply
/// strip evaluates to ~64 on white (matching below-cap direct replay and CPU).
#[test]
fn gpu_overlapping_multiply_above_64_passes_preserves_both_copies() {
    let gs_mul = b"<< /Type /ExtGState /BM /Multiply >>".to_vec();
    let gs_dark = b"<< /Type /ExtGState /BM /Darken >>".to_vec();
    let mut cell_stream = String::from("/GM gs 0.5 0.5 0.5 rg 0 0 8 2 re f\n");
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

    let doc = PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let content_bytes = doc.page_content_bytes(&page).expect("content bytes");
    let mut images = ImageCache::new();
    let dl = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images)
        .interpret(&content_bytes);

    let gpu = match zpdf::gpu::WgpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, 1.0)
    {
        Ok(t) => t,
        Err(e) => {
            eprintln!("skipping GPU overlapping >64 passes test (no adapter?): {e}");
            return;
        }
    };
    let cpu = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, 1.0)
        .expect("cpu render");

    let px = |data: &[u8], x: u32, y_pdf: u32| -> [u8; 3] {
        let iy = 259 - y_pdf;
        let off = ((iy * gpu.width + x) * 4) as usize;
        [data[off], data[off + 1], data[off + 2]]
    };
    let below = px(&gpu.data, 22, 21);
    let above = px(&gpu.data, 190, 129);
    assert!(
        below.iter().all(|&c| (58..=70).contains(&c)),
        "GPU below-cap 2x33 overlapping Multiply must be ~64, got {below:?}"
    );
    assert!(
        above.iter().all(|&c| (58..=70).contains(&c)),
        "GPU above-cap 2x33 (>64) overlapping Multiply must stay ~64 (not ~128), got {above:?}"
    );

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
    let pct = diff as f64 / total as f64 * 100.0;
    println!("  overlapping_multiply_2x33_above_64: {pct:.3}% differing");
    assert!(
        pct < MAX_DIFF_PCT,
        "overlapping Multiply 2x33 GPU vs CPU {pct:.3}% exceeds {MAX_DIFF_PCT}%"
    );
}

/// Regression: GPU outer shape and clip coverage (fill edge, stroke edge, and
/// anti-aliased glyph clip) must be applied ONCE across all segments of a
/// multi-segment `Multiply` tiling pattern — matching a solid opaque black
/// reference (~128 on half-covered edges, NOT ~64).
#[test]
fn gpu_multi_segment_tiling_applies_outer_shape_coverage_once() {
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

    let render_gpu = |pdf: Vec<u8>| {
        let doc = PdfDocument::open(pdf).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let content_bytes = doc.page_content_bytes(&page).expect("content bytes");
        let mut images = ImageCache::new();
        let dl = ContentInterpreter::new(page.media_box)
            .with_fonts(&mut fonts)
            .with_document(doc.file(), &page.resources)
            .with_images(&mut images)
            .interpret(&content_bytes);
        zpdf::gpu::WgpuRenderer::new()
            .with_fonts(&fonts)
            .with_images(&images)
            .render_display_list(&dl, 1.0)
    };
    let pat_gpu = match render_gpu(pat_pdf) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("skipping GPU outer-shape coverage test (no adapter?): {e}");
            return;
        }
    };
    let ref_gpu = render_gpu(ref_pdf).expect("gpu solid reference render");

    let px = |data: &[u8], x: u32, y_pdf: u32| -> [u8; 3] {
        let iy = 119 - y_pdf;
        let off = ((iy * pat_gpu.width + x) * 4) as usize;
        [data[off], data[off + 1], data[off + 2]]
    };
    let fill_pat = px(&pat_gpu.data, 10, 50);
    let fill_ref = px(&ref_gpu.data, 10, 50);
    assert!(
        (110..=145).contains(&fill_ref[0]),
        "GPU sanity check: solid fill edge must be ~128, got {fill_ref:?}"
    );
    assert!(
        (fill_pat[0] as i32 - fill_ref[0] as i32).abs() <= 4,
        "GPU multi-segment fill edge must match solid ref ({fill_ref:?}), got {fill_pat:?}"
    );

    let stroke_pat = px(&pat_gpu.data, 45, 50);
    let stroke_ref = px(&ref_gpu.data, 45, 50);
    assert!(
        (110..=145).contains(&stroke_ref[0]),
        "GPU sanity check: solid stroke edge must be ~128, got {stroke_ref:?}"
    );
    assert!(
        (stroke_pat[0] as i32 - stroke_ref[0] as i32).abs() <= 4,
        "GPU multi-segment stroke edge must match solid ref ({stroke_ref:?}), got {stroke_pat:?}"
    );

    let mut aa_glyph_pixels = 0usize;
    for y in 20..110u32 {
        for x in 65..118u32 {
            let off = ((y * pat_gpu.width + x) * 4) as usize;
            let r_ref = ref_gpu.data[off];
            if (60..=200).contains(&r_ref) {
                aa_glyph_pixels += 1;
                let r_pat = pat_gpu.data[off];
                assert!(
                    (r_pat as i32 - r_ref as i32).abs() <= 4,
                    "GPU glyph-clip AA pixel ({x},{y}) diverged: solid ref={r_ref}, tiling={r_pat}"
                );
            }
        }
    }
    assert!(
        aa_glyph_pixels >= 10,
        "expected anti-aliased glyph-clip pixels on GPU, found {aa_glyph_pixels}"
    );
}

/// Regression (ISO 32000-1 §11.6.7): GPU must evaluate a backdrop-dependent
/// tiling cell against the live page backdrop BEFORE applying an outer
/// `/BM /Screen` blend mode (matching CPU).
#[test]
fn gpu_outer_screen_blend_mode_on_backdrop_dependent_tiling_evaluates_cell_first() {
    let gs_mul = b"<< /Type /ExtGState /BM /Multiply >>".to_vec();
    let gs_screen = b"<< /Type /ExtGState /BM /Screen >>".to_vec();
    let cell = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 10 10] /XStep 10 /YStep 10 \
         /Resources << /ExtGState << /GM 6 0 R >> >>",
        b"/GM gs 1 1 1 rg 0 0 10 10 re f",
    );
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
        cell,                                          // 5
        gs_mul,                                        // 6
        gs_screen,                                     // 7
    ]);

    let doc = PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let content_bytes = doc.page_content_bytes(&page).expect("content bytes");
    let mut images = ImageCache::new();
    let dl = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images)
        .interpret(&content_bytes);

    let gpu = match zpdf::gpu::WgpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, 1.0)
    {
        Ok(t) => t,
        Err(e) => {
            eprintln!("skipping GPU outer-Screen tiling test (no adapter?): {e}");
            return;
        }
    };
    let cpu = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, 1.0)
        .expect("cpu render");

    let px = |data: &[u8], x: u32, y_pdf: u32| -> [u8; 3] {
        let iy = 99 - y_pdf;
        let off = ((iy * gpu.width + x) * 4) as usize;
        [data[off], data[off + 1], data[off + 2]]
    };
    let blue_in = px(&gpu.data, 25, 50);
    assert!(
        blue_in[0] < 10 && blue_in[1] < 10 && blue_in[2] > 245,
        "GPU white Multiply cell under outer /BM /Screen over blue must stay blue, got {blue_in:?}"
    );
    let gray_in = px(&gpu.data, 70, 50);
    assert!(
        gray_in.iter().all(|&c| (186..=197).contains(&c)),
        "GPU white Multiply cell under outer /BM /Screen over 50% gray must be ~191, got {gray_in:?}"
    );
    let gray_out = px(&gpu.data, 95, 50);
    assert!(
        gray_out.iter().all(|&c| (124..=132).contains(&c)),
        "GPU 50% gray outside fill must stay ~128, got {gray_out:?}"
    );

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
    let pct = diff as f64 / total as f64 * 100.0;
    println!("  outer_screen_backdrop_tiling: {pct:.3}% differing");
    assert!(
        pct < MAX_DIFF_PCT,
        "outer Screen backdrop tiling GPU vs CPU {pct:.3}% exceeds {MAX_DIFF_PCT}%"
    );
}

/// Regression: dense GPU (>4,096 tiles) nested half-pixel path painted with a
/// 2-segment black `Multiply` inner pattern must apply the nested path's
/// anti-aliased coverage ONCE (~128 on white, matching solid reference).
#[test]
fn gpu_dense_nested_half_pixel_path_applies_inner_shape_coverage_once() {
    let gs_mul = b"<< /Type /ExtGState /BM /Multiply >>".to_vec();
    let gs_dark = b"<< /Type /ExtGState /BM /Darken >>".to_vec();
    let p_inner = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 2 2] /XStep 2 /YStep 2 \
         /Resources << /ExtGState << /GM 8 0 R /GD 9 0 R >> >>",
        b"/GM gs 0 0 0 rg 0 0 2 2 re f\n/GD gs 0 0 0 rg 0 0 2 2 re f",
    );
    let p_outer = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 4 4] /XStep 4 /YStep 4 \
         /Resources << /Pattern << /PI 5 0 R >> >>",
        b"/Pattern cs /PI scn 1.5 0 2 4 re f",
    );
    let p_ref = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 4 4] /XStep 4 /YStep 4",
        b"0 0 0 rg 1.5 0 2 4 re f",
    );
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

    let doc = PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let content_bytes = doc.page_content_bytes(&page).expect("content bytes");
    let mut images = ImageCache::new();
    let dl = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images)
        .interpret(&content_bytes);

    let gpu = match zpdf::gpu::WgpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, 1.0)
    {
        Ok(t) => t,
        Err(e) => {
            eprintln!("skipping GPU dense nested half-pixel test (no adapter?): {e}");
            return;
        }
    };
    let px = |x: u32, y_pdf: u32| -> [u8; 3] {
        let iy = 259 - y_pdf;
        let off = ((iy * gpu.width + x) * 4) as usize;
        [gpu.data[off], gpu.data[off + 1], gpu.data[off + 2]]
    };
    let pat_edge = px(121, 130);
    let ref_edge = px(401, 130);
    assert!(
        (110..=145).contains(&ref_edge[0]),
        "GPU sanity check: solid nested edge must be ~128, got {ref_edge:?}"
    );
    assert!(
        (pat_edge[0] as i32 - ref_edge[0] as i32).abs() <= 4,
        "GPU dense nested half-pixel edge must match solid ref ({ref_edge:?}, NOT ~64), got {pat_edge:?}"
    );
}

/// Regression: dense GPU (>4,096 tiles) overlapping cell exceeding the 1,024
/// per-copy pass budget (both 6 steps × 205 spanning white Multiply marks and
/// 6 steps × 175 non-coalesceable alternating Multiply/Darken white marks +
/// trailing black detail in step 0) must keep the trailing black detail visible
/// and preserve the blue page backdrop.
#[test]
fn gpu_dense_overlapping_cell_exceeding_pass_budget_preserves_trailing_detail() {
    let gs_mul = b"<< /Type /ExtGState /BM /Multiply >>".to_vec();
    let gs_dark = b"<< /Type /ExtGState /BM /Darken >>".to_vec();
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
    let mut cell_alt_stream = String::new();
    for k in 0..175 {
        let gs = if k % 2 == 0 { "/GM" } else { "/GD" };
        cell_alt_stream.push_str(&format!("{gs} gs 1 1 1 rg 0 0 12 2 re f\n"));
    }
    cell_alt_stream.push_str("/GM gs 0 0 0 rg 0 0 1 2 re f\n");
    let cell_alt = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 12 2] /XStep 2 /YStep 2 \
         /Resources << /ExtGState << /GM 7 0 R /GD 8 0 R >> >>",
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
    ]);

    let doc = PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let content_bytes = doc.page_content_bytes(&page).expect("content bytes");
    let mut images = ImageCache::new();
    let dl = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images)
        .interpret(&content_bytes);

    let gpu = match zpdf::gpu::WgpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, 1.0)
    {
        Ok(t) => t,
        Err(e) => {
            eprintln!("skipping GPU dense pass-budget fallback test (no adapter?): {e}");
            return;
        }
    };
    let px = |x: u32, y_pdf: u32| -> [u8; 3] {
        let iy = 259 - y_pdf;
        let off = ((iy * gpu.width + x) * 4) as usize;
        [gpu.data[off], gpu.data[off + 1], gpu.data[off + 2]]
    };
    let detail_a = px(121, 130);
    assert!(
        detail_a[0] < 10 && detail_a[1] < 10 && detail_a[2] < 10,
        "GPU Cell A trailing black detail must remain black, got {detail_a:?}"
    );
    let bg_a = px(123, 130);
    assert!(
        bg_a[0] < 10 && bg_a[1] < 10 && bg_a[2] > 245,
        "GPU Cell A white Multiply marks must preserve blue backdrop, got {bg_a:?}"
    );
    let detail_b = px(340, 66);
    assert!(
        detail_b[0] < 10 && detail_b[1] < 10 && detail_b[2] < 10,
        "GPU Cell B trailing black detail past 1,024-pass budget must remain black, got {detail_b:?}"
    );
    let bg_b = px(341, 66);
    assert!(
        bg_b[0] < 10 && bg_b[1] < 10 && bg_b[2] > 245,
        "GPU Cell B white Multiply/Darken marks in fallback must preserve blue backdrop, got {bg_b:?}"
    );
}
