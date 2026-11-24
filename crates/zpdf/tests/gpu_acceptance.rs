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

/// Single shared headless [`zpdf::gpu::GpuContext`] for this integration test
/// binary. Running ~20 independent software DX12 (WARP) devices concurrently on
/// Windows CI exhausts D3D12 committed heap quota and triggers spurious
/// `Buffer with '...' label is invalid` panics inside `create_buffer_init`.
/// Sharing one context also serializes the `ZPDF_GPU_GLYPH_ATLAS` toggle test.
static SHARED_GPU_CTX: std::sync::Mutex<Option<zpdf::gpu::GpuContext>> =
    std::sync::Mutex::new(None);

fn with_shared_gpu_renderer<R>(
    f: impl FnOnce(&mut zpdf::gpu::WgpuRenderer<'_>) -> R,
) -> Result<R, zpdf::gpu::WgpuRenderError> {
    let mut guard = SHARED_GPU_CTX.lock().unwrap_or_else(|e| e.into_inner());
    let ctx = match guard.take() {
        Some(c) => c,
        None => zpdf::gpu::GpuContext::new_headless()?,
    };
    let mut renderer = zpdf::gpu::WgpuRenderer::new().with_context(ctx);
    let out = f(&mut renderer);
    if let Some(c) = renderer.take_context() {
        *guard = Some(c);
    }
    Ok(out)
}

fn render_gpu_shared(
    dl: &zpdf::display_list::DisplayList,
    fonts: Option<&zpdf::FontCache>,
    images: Option<&ImageCache>,
    scale: f32,
) -> Result<zpdf::gpu::GpuTexture, zpdf::gpu::WgpuRenderError> {
    let mut guard = SHARED_GPU_CTX.lock().unwrap_or_else(|e| e.into_inner());
    let ctx = match guard.take() {
        Some(c) => c,
        None => zpdf::gpu::GpuContext::new_headless()?,
    };
    let mut renderer = zpdf::gpu::WgpuRenderer::new().with_context(ctx);
    if let Some(f) = fonts {
        renderer = renderer.with_fonts(f);
    }
    if let Some(i) = images {
        renderer = renderer.with_images(i);
    }
    let res = renderer.render_display_list(dl, scale);
    if let Some(c) = renderer.take_context() {
        *guard = Some(c);
    }
    res
}

/// Real embedded TrueType font (not Type3) — its Unicode cmap maps 'A' (0x41)
/// to GID 1, a filled-rectangle outline (see `crates/zpdf/tests/variable_font_pdf.rs`,
/// which exercises the same fixture/GID via `glyph_outline(1)` directly).
const TEST_TTF: &[u8] = include_bytes!("../../zpdf-font/tests/fixtures/var.ttf");

/// Real embedded Noto Sans subset TrueType font whose 'A' (U+0041, GID 2) has
/// bold diagonal strokes and an inner triangular counter, giving deterministic
/// anti-aliased edges independent of host system fonts.
const NOTO_SC_TTF: &[u8] = include_bytes!("../../zpdf-font/tests/fixtures/noto-sans-sc-subset.ttf");

fn embedded_noto_font_objs(descriptor_id: u32, file_id: u32) -> [Vec<u8>; 3] {
    [
        format!(
            "<< /Type /Font /Subtype /TrueType /BaseFont /NotoSansSubset \
             /Encoding /WinAnsiEncoding /FirstChar 65 /LastChar 65 /Widths [608] \
             /FontDescriptor {descriptor_id} 0 R >>"
        )
        .into_bytes(),
        format!(
            "<< /Type /FontDescriptor /FontName /NotoSansSubset /Flags 32 \
             /FontBBox [0 -120 1000 880] /FontFile2 {file_id} 0 R >>"
        )
        .into_bytes(),
        stream_obj(&format!("/Length1 {}", NOTO_SC_TTF.len()), NOTO_SC_TTF),
    ]
}

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
    let gpu = match render_gpu_shared(&dl, Some(&fonts), Some(&images), SCALE) {
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

    let gpu = match render_gpu_shared(&dl, Some(&fonts), Some(&images), SCALE) {
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

    let res = with_shared_gpu_renderer(|renderer| {
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
    });
    match res {
        Ok(()) => {}
        Err(zpdf::gpu::WgpuRenderError::NoAdapter) => {
            eprintln!("skipping GPU clear-colour acceptance (no adapter)");
        }
        Err(e) => panic!("initialize GPU clear-colour acceptance: {e}"),
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

    // Hold SHARED_GPU_CTX across the env-var toggle so no parallel GPU test
    // observes `ZPDF_GPU_GLYPH_ATLAS=0`.
    let pair = (|| -> Result<_, zpdf::gpu::WgpuRenderError> {
        let mut guard = SHARED_GPU_CTX.lock().unwrap_or_else(|e| e.into_inner());
        let ctx = match guard.take() {
            Some(c) => c,
            None => zpdf::gpu::GpuContext::new_headless()?,
        };
        let mut renderer = zpdf::gpu::WgpuRenderer::new()
            .with_context(ctx)
            .with_fonts(&fonts)
            .with_images(&images);
        unsafe {
            std::env::set_var("ZPDF_GPU_GLYPH_ATLAS", "0");
        }
        let fallback = renderer.render_display_list(&dl, SCALE);
        unsafe {
            std::env::remove_var("ZPDF_GPU_GLYPH_ATLAS");
        }
        let atlas = renderer.render_display_list(&dl, SCALE);
        if let Some(c) = renderer.take_context() {
            *guard = Some(c);
        }
        Ok((fallback?, atlas?))
    })();

    let Ok((fallback, atlas)) = pair else {
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

    let gpu = match render_gpu_shared(&dl, Some(&fonts), Some(&images), SCALE) {
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

    let gpu = match render_gpu_shared(&dl, Some(&fonts), Some(&images), 1.0) {
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

    let gpu = match render_gpu_shared(&dl, Some(&fonts), Some(&images), SCALE) {
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

    let gpu = match render_gpu_shared(&dl, Some(&fonts), Some(&images), SCALE) {
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

    let gpu = match render_gpu_shared(&dl, Some(&fonts), Some(&images), 1.0) {
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

    let gpu = match render_gpu_shared(&dl, Some(&fonts), Some(&images), SCALE) {
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

    let gpu = match render_gpu_shared(&dl, Some(&fonts), Some(&images), 1.0) {
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

    let gpu = match render_gpu_shared(&dl, Some(&fonts), Some(&images), 1.0) {
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

    let gpu = match render_gpu_shared(&dl, Some(&fonts), Some(&images), 1.0) {
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
    let [font, font_desc, font_stream] = embedded_noto_font_objs(8, 9);
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
            font_desc.clone(),                             // 8
            font_stream.clone(),                           // 9
        ])
    };
    let pat_pdf = build_pdf(
        b"/Pattern cs /P0 scn 10.5 10 20 80 re f\n\
          /Pattern CS /P0 SCN 9 w 50 10 m 50 90 l S\n\
          /Pattern cs /P0 scn BT /F1 60 Tf 70 25 Td (A) Tj ET",
    );
    let ref_pdf = build_pdf(
        b"0 0 0 rg 10.5 10 20 80 re f\n\
          0 0 0 RG 9 w 50 10 m 50 90 l S\n\
          q BT /F1 60 Tf 7 Tr 70 25 Td (A) Tj ET 0 0 0 rg 65 20 55 60 re f Q",
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
        render_gpu_shared(&dl, Some(&fonts), Some(&images), 1.0)
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

    let gpu = match render_gpu_shared(&dl, Some(&fonts), Some(&images), 1.0) {
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

    let gpu = match render_gpu_shared(&dl, Some(&fonts), Some(&images), 1.0) {
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

    let gpu = match render_gpu_shared(&dl, Some(&fonts), Some(&images), 1.0) {
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
        (165..=175).contains(&bg_b[2])
            && (165..=175).contains(&bg_b[0])
            && (165..=175).contains(&bg_b[1]),
        "GPU Cell B 6x Multiply–Screen pairs over blue must be ~[170,170,170] (NOT blue ~251), got {bg_b:?}"
    );
}

/// Regression: direct `RenderCommand::DrawGlyphRun` with `Paint::Tiling` must
/// render visible patterned text on GPU for both page text and cell text
/// (matching CPU and solid-black reference on AA edge pixels).
#[test]
fn gpu_tiling_glyph_run_renders_page_and_cell_text() {
    let gs_mul = b"<< /Type /ExtGState /BM /Multiply >>".to_vec();
    let cell = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 10 10] /XStep 10 /YStep 10 \
         /Resources << /ExtGState << /GM 7 0 R >> >>",
        b"/GM gs 0 0 0 rg 0 0 10 10 re f\n/GM gs 0 0 0 rg 0 0 10 10 re f",
    );
    let [font, font_desc, font_stream] = embedded_noto_font_objs(8, 9);
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
            font_desc.clone(),                             // 8
            font_stream.clone(),                           // 9
        ])
    };

    let pat_pdf = build_pdf(b"/Pattern cs /P0 scn 0 0 10 10 re f");
    let doc_p = PdfDocument::open(pat_pdf).expect("open pat pdf");
    let page_p = doc_p.page(0).expect("page 0");
    let mut fonts_p = doc_p.load_page_fonts(&page_p);
    let content_p = doc_p.page_content_bytes(&page_p).expect("content");
    let mut images_p = ImageCache::new();
    let pat_dl = ContentInterpreter::new(page_p.media_box)
        .with_fonts(&mut fonts_p)
        .with_document(doc_p.file(), &page_p.resources)
        .with_images(&mut images_p)
        .interpret(&content_p);
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
        .expect("tiling paint");

    // 1. Direct page-level DrawGlyphRun with Paint::Tiling (matching pattern_tiling.rs fixture).
    let glyph_pdf = build_pdf(b"0 0 0 rg BT /F1 60 Tf 70 25 Td (A) Tj ET");
    let doc = PdfDocument::open(glyph_pdf).expect("open glyph pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let content = doc.page_content_bytes(&page).expect("content");
    let mut images = ImageCache::new();
    let solid_dl = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images)
        .interpret(&content);
    let mut tiling_dl = solid_dl.clone();
    let mut cell_glyph_run = None;
    for cmd in &mut tiling_dl.commands {
        if let zpdf::display_list::RenderCommand::DrawGlyphRun(run) = cmd {
            run.paint = tiling_paint.clone();
            let mut in_cell = run.clone();
            in_cell.font_size = 40.0;
            in_cell.transform = zpdf_core::Matrix::translate(5.0, 5.0);
            cell_glyph_run = Some(in_cell);
        }
    }
    let cell_glyph_run = cell_glyph_run.expect("glyph run");

    let solid_gpu = match render_gpu_shared(&solid_dl, Some(&fonts), Some(&images), 1.0) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("skipping GPU tiling DrawGlyphRun test (no adapter?): {e}");
            return;
        }
    };
    let tiling_gpu =
        render_gpu_shared(&tiling_dl, Some(&fonts), Some(&images), 1.0).expect("tiling glyph gpu");
    let mut dark_pixels = 0usize;
    let mut aa_pixels = 0usize;
    for y in 20..110u32 {
        for x in 65..118u32 {
            let off = ((y * solid_gpu.width + x) * 4) as usize;
            let r_ref = solid_gpu.data[off];
            let r_pat = tiling_gpu.data[off];
            if r_pat < 30 {
                dark_pixels += 1;
            }
            if (60..=200).contains(&r_ref) {
                aa_pixels += 1;
                assert!(
                    (r_pat as i32 - r_ref as i32).abs() <= 16,
                    "GPU DrawGlyphRun AA pixel ({x},{y}) diverged: solid={r_ref}, tiling={r_pat}"
                );
            }
        }
    }
    assert!(
        dark_pixels >= 100,
        "GPU DrawGlyphRun with Paint::Tiling must render visible 'A', found {dark_pixels} dark pixels"
    );
    assert!(
        aa_pixels >= 10,
        "expected anti-aliased edge pixels in solid 'A', found {aa_pixels}"
    );

    // 2. Direct cell-level DrawGlyphRun with Paint::Tiling inside a repeating tiling cell.
    let mut solid_cell_glyph_run = cell_glyph_run.clone();
    solid_cell_glyph_run.paint =
        zpdf::display_list::Paint::Solid(zpdf::display_list::Color::black());
    let mut outer_cell =
        zpdf::display_list::DisplayList::new(zpdf_core::Rect::new(0.0, 0.0, 60.0, 60.0));
    outer_cell.push(zpdf::display_list::RenderCommand::DrawGlyphRun(
        cell_glyph_run,
    ));
    let mut solid_outer_cell =
        zpdf::display_list::DisplayList::new(zpdf_core::Rect::new(0.0, 0.0, 60.0, 60.0));
    solid_outer_cell.push(zpdf::display_list::RenderCommand::DrawGlyphRun(
        solid_cell_glyph_run,
    ));
    let build_cell_dl = |cell_dl: zpdf::display_list::DisplayList| {
        let tiling = std::sync::Arc::new(zpdf::display_list::TilingPattern {
            commands: std::sync::Arc::new(cell_dl),
            bbox: zpdf_core::Rect::new(0.0, 0.0, 60.0, 60.0),
            x_step: 60.0,
            y_step: 60.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });
        let mut dl =
            zpdf::display_list::DisplayList::new(zpdf_core::Rect::new(0.0, 0.0, 120.0, 120.0));
        let mut full = zpdf::display_list::Path::new();
        full.rect(zpdf_core::Rect::new(0.0, 0.0, 120.0, 120.0));
        dl.push(zpdf::display_list::RenderCommand::FillPath {
            path: full,
            rule: zpdf::display_list::FillRule::NonZero,
            paint: zpdf::display_list::Paint::Tiling(tiling),
            alpha: 1.0,
            overprint: None,
        });
        dl
    };
    let cell_text_dl = build_cell_dl(outer_cell);
    let solid_cell_text_dl = build_cell_dl(solid_outer_cell);
    let cell_gpu =
        render_gpu_shared(&cell_text_dl, Some(&fonts), Some(&images), 1.0).expect("cell text gpu");
    let solid_cell_gpu = render_gpu_shared(&solid_cell_text_dl, Some(&fonts), Some(&images), 1.0)
        .expect("solid cell text gpu");
    let cell_cpu = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&cell_text_dl, 1.0)
        .expect("cell text cpu");
    let gpu_dark = cell_gpu
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|px| px[0] < 30 && px[1] < 30 && px[2] < 30)
        .count();
    assert!(
        gpu_dark >= 200,
        "GPU cell-level DrawGlyphRun with Paint::Tiling must render repeated 'A' cells, found {gpu_dark} dark pixels"
    );
    // Tiled-paint cell text must match solid-paint cell text across every pixel on GPU,
    // render dark ink in all 4 cells, and match CPU across every interior (non-AA-edge)
    // pixel (no missing or shifted cells; AA edges may vary slightly from bilinear glyph
    // atlas resampling).
    let (w, h) = (cell_cpu.width as usize, cell_cpu.height as usize);
    let total = w * h;
    let mut gpu_vs_solid_max = 0u32;
    let mut gpu_vs_cpu_gt64 = 0usize;
    let mut interior_mismatches = 0usize;
    let mut cell_dark = [0usize; 4];
    for y in 0..h {
        for x in 0..w {
            let b = (y * w + x) * 4;
            if cell_gpu.data[b] < 30 && cell_gpu.data[b + 1] < 30 && cell_gpu.data[b + 2] < 30 {
                let q = (y / 60).min(1) * 2 + (x / 60).min(1);
                cell_dark[q] += 1;
            }
            for c in 0..3 {
                let ds = (cell_gpu.data[b + c] as i32 - solid_cell_gpu.data[b + c] as i32)
                    .unsigned_abs();
                gpu_vs_solid_max = gpu_vs_solid_max.max(ds);
            }
            let dr = (cell_gpu.data[b] as i32 - cell_cpu.data[b] as i32).unsigned_abs();
            let dg = (cell_gpu.data[b + 1] as i32 - cell_cpu.data[b + 1] as i32).unsigned_abs();
            let db = (cell_gpu.data[b + 2] as i32 - cell_cpu.data[b + 2] as i32).unsigned_abs();
            let dmax = dr.max(dg).max(db);
            if dmax > 64 {
                gpu_vs_cpu_gt64 += 1;
            }
            if x > 0 && x + 1 < w && y > 0 && y + 1 < h {
                let mut all_dark = true;
                let mut all_white = true;
                for ny in (y - 1)..=(y + 1) {
                    for nx in (x - 1)..=(x + 1) {
                        let r = cell_cpu.data[(ny * w + nx) * 4];
                        if r >= 30 {
                            all_dark = false;
                        }
                        if r <= 225 {
                            all_white = false;
                        }
                    }
                }
                if (all_dark || all_white) && dmax > THRESHOLD as u32 {
                    interior_mismatches += 1;
                }
            }
        }
    }
    assert!(
        gpu_vs_solid_max <= 16,
        "GPU cell-level tiling DrawGlyphRun must match GPU solid cell text within <=16, max diff={gpu_vs_solid_max}"
    );
    assert!(
        cell_dark.iter().all(|&n| n >= 50),
        "GPU cell-level tiling DrawGlyphRun must render all 4 repeated 'A' cells, per-cell dark={cell_dark:?}"
    );
    assert_eq!(
        interior_mismatches, 0,
        "GPU cell-level tiling DrawGlyphRun must match CPU on every interior pixel across all 4 cells (found {interior_mismatches} mismatches)"
    );
    let gt64_pct = gpu_vs_cpu_gt64 as f64 / total as f64 * 100.0;
    assert!(
        gt64_pct < MAX_DIFF_PCT,
        "GPU cell-level tiling DrawGlyphRun diverged from CPU on {gpu_vs_cpu_gt64} pixels ({gt64_pct:.3}%)"
    );
}

/// Regression (ISO 32000-1 §11.4.5): deeply nested non-isolated groups with
/// non-`Normal` outer blend modes must stay linear in depth (no `2^depth`
/// recursive encoding blowup in `end_page`) and match CPU pixels exactly.
#[test]
fn gpu_deeply_nested_non_isolated_groups_stay_linear_and_match_cpu() {
    use zpdf_core::Rect;
    use zpdf_display_list::{
        BlendMode, Color, DisplayList, FillRule, Paint, Path, RenderCommand, SoftMask, SoftMaskKind,
    };
    let page_rect = Rect::new(0.0, 0.0, 96.0, 64.0);
    let mut dl = DisplayList::new(page_rect);

    // Left half [0..48]: 15 nested non-isolated Screen groups + 1 inner
    // Multiply white fill over opaque blue (hits full default blend depth 16).
    let mut left = Path::new();
    left.rect(Rect::new(0.0, 0.0, 48.0, 64.0));
    dl.push(RenderCommand::FillPath {
        path: left.clone(),
        rule: FillRule::NonZero,
        paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
        alpha: 1.0,
        overprint: None,
    });
    for _ in 0..15 {
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Screen,
            isolated: false,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 48.0, 64.0),
            alpha: 1.0,
            mask: None,
        });
    }
    dl.push(RenderCommand::PushBlendGroup {
        blend_mode: BlendMode::Multiply,
        isolated: true,
        knockout: false,
        bounds: Rect::new(0.0, 0.0, 48.0, 64.0),
        alpha: 1.0,
        mask: None,
    });
    dl.push(RenderCommand::FillPath {
        path: left,
        rule: FillRule::NonZero,
        paint: Paint::Solid(Color::white()),
        alpha: 1.0,
        overprint: None,
    });
    dl.push(RenderCommand::PopBlendGroup);
    for _ in 0..15 {
        dl.push(RenderCommand::PopBlendGroup);
    }

    // Right half [48..96]: opaque gray (0.5, 0.5, 0.5) backdrop with outer
    // non-isolated Multiply (alpha 1.0) wrapping:
    //   - top [48..96, 32..64]: inner non-isolated Screen (alpha 0.5) with red
    //     -> analytical ISO §11.4.5 result ~(128, 96, 96).
    //   - bottom [48..96, 0..32]: inner non-isolated Normal group with a 50%
    //     alpha soft mask painting opaque red -> evaluates red over gray,
    //     blends at 0.5 into outer group (c_n = (0.75,0.25,0.25), α_g = 0.5),
    //     backdrop removal recovers c_g = (1,0,0), outer Multiply onto gray ->
    //     0.5*(0.5,0.5,0.5) + 0.5*(0.5,0,0) = (0.5, 0.25, 0.25) ≈ (128, 64, 64).
    let mut right = Path::new();
    right.rect(Rect::new(48.0, 0.0, 96.0, 64.0));
    dl.push(RenderCommand::FillPath {
        path: right,
        rule: FillRule::NonZero,
        paint: Paint::Solid(Color::rgb(0.5, 0.5, 0.5)),
        alpha: 1.0,
        overprint: None,
    });
    dl.push(RenderCommand::PushBlendGroup {
        blend_mode: BlendMode::Multiply,
        isolated: false,
        knockout: false,
        bounds: Rect::new(48.0, 0.0, 96.0, 64.0),
        alpha: 1.0,
        mask: None,
    });

    // Top-right: nested non-isolated Screen (0.5)
    let mut top_right = Path::new();
    top_right.rect(Rect::new(48.0, 32.0, 96.0, 64.0));
    dl.push(RenderCommand::PushBlendGroup {
        blend_mode: BlendMode::Screen,
        isolated: false,
        knockout: false,
        bounds: Rect::new(48.0, 32.0, 96.0, 64.0),
        alpha: 0.5,
        mask: None,
    });
    dl.push(RenderCommand::FillPath {
        path: top_right,
        rule: FillRule::NonZero,
        paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
        alpha: 1.0,
        overprint: None,
    });
    dl.push(RenderCommand::PopBlendGroup);

    // Bottom-right: nested non-isolated Normal with 50% alpha soft mask
    let mut bot_right = Path::new();
    bot_right.rect(Rect::new(48.0, 0.0, 96.0, 32.0));
    let mut mask_dl = DisplayList::new(page_rect);
    mask_dl.push(RenderCommand::FillPath {
        path: bot_right.clone(),
        rule: FillRule::NonZero,
        paint: Paint::Solid(Color::white()),
        alpha: 0.5,
        overprint: None,
    });
    dl.push(RenderCommand::PushBlendGroup {
        blend_mode: BlendMode::Normal,
        isolated: false,
        knockout: false,
        bounds: Rect::new(48.0, 0.0, 96.0, 32.0),
        alpha: 1.0,
        mask: Some(SoftMask {
            kind: SoftMaskKind::Alpha,
            commands: std::sync::Arc::new(mask_dl),
            backdrop_luma: 0.0,
            offset: (0.0, 0.0),
            transfer: None,
        }),
    });
    dl.push(RenderCommand::FillPath {
        path: bot_right,
        rule: FillRule::NonZero,
        paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
        alpha: 1.0,
        overprint: None,
    });
    dl.push(RenderCommand::PopBlendGroup);
    dl.push(RenderCommand::PopBlendGroup);

    let gpu = match render_gpu_shared(&dl, None, None, 1.0) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("skipping GPU nested non-isolated groups test (no adapter?): {e}");
            return;
        }
    };
    let cpu = zpdf::cpu::CpuRenderer::new()
        .render_display_list(&dl, 1.0)
        .expect("cpu render");

    let px = |data: &[u8], x: u32, y_pdf: u32| -> [u8; 3] {
        let iy = 63 - y_pdf;
        let off = ((iy * gpu.width + x) * 4) as usize;
        [data[off], data[off + 1], data[off + 2]]
    };

    let left_gpu = px(&gpu.data, 24, 32);
    assert!(
        left_gpu[0] < 10 && left_gpu[1] < 10 && left_gpu[2] > 245,
        "GPU 15-deep nested non-isolated Screen groups over blue must stay blue, got {left_gpu:?}"
    );
    let tr_gpu = px(&gpu.data, 72, 48);
    assert!(
        (tr_gpu[0] as i32 - 128).abs() <= 2
            && (tr_gpu[1] as i32 - 96).abs() <= 2
            && (tr_gpu[2] as i32 - 96).abs() <= 2,
        "GPU nested non-isolated Multiply(Screen(0.5, red)) must be ~(128, 96, 96), got {tr_gpu:?}"
    );
    let br_gpu = px(&gpu.data, 72, 16);
    assert!(
        (br_gpu[0] as i32 - 128).abs() <= 2
            && (br_gpu[1] as i32 - 64).abs() <= 2
            && (br_gpu[2] as i32 - 64).abs() <= 2,
        "GPU nested non-isolated Multiply(Normal(mask=0.5, red)) must be ~(128, 64, 64), got {br_gpu:?}"
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
    println!("  nested_non_isolated_groups: {pct:.3}% differing");
    assert!(
        pct < MAX_DIFF_PCT,
        "GPU and CPU must match on nested non-isolated groups, got {pct:.3}% differing"
    );
}

/// Regression: opaque 50%-gray `Darken` over a 0.25-gray page must stay ~64
/// (NOT ~32 from affine black/white interpolation) even above 4,096 tiles AND
/// above 1,024 overlapping passes on GPU and CPU.
#[test]
fn gpu_dense_darken_tiling_preserves_non_affine_backdrop() {
    let gs_dark = b"<< /Type /ExtGState /BM /Darken >>".to_vec();
    let gs_mul = b"<< /Type /ExtGState /BM /Multiply >>".to_vec();
    let mut cell_stream = String::new();
    for k in 0..175 {
        if k == 0 {
            cell_stream.push_str("/GD gs 0.5 0.5 0.5 rg 0 0 12 2 re f\n");
        } else if k % 2 == 0 {
            cell_stream.push_str("/GD gs 1 1 1 rg 0 2 12 2 re f\n");
        } else {
            cell_stream.push_str("/GM gs 1 1 1 rg 0 2 12 2 re f\n");
        }
    }
    let cell = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 12 4] /XStep 2 /YStep 4 \
         /Resources << /ExtGState << /GD 6 0 R /GM 7 0 R >> >>",
        cell_stream.as_bytes(),
    );
    let content = stream_obj(
        "",
        b"0.25 0.25 0.25 rg 0 0 140 260 re f\n\
          /Pattern cs /P0 scn 0 0 140 260 re f",
    );
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 140 260] /Contents 4 0 R \
          /Resources << /Pattern << /P0 5 0 R >> >> >>"
            .to_vec(),
        content,
        cell,
        gs_dark,
        gs_mul,
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

    let gpu = match render_gpu_shared(&dl, Some(&fonts), Some(&images), 1.0) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("skipping GPU dense Darken test (no adapter?): {e}");
            return;
        }
    };
    let cpu = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, 1.0)
        .expect("cpu render");

    let iy = 259 - 129;
    let off = ((iy * gpu.width + 70) * 4) as usize;
    let px = &gpu.data[off..off + 3];
    assert!(
        px.iter().all(|&c| (60..=68).contains(&c)),
        "GPU dense Darken(0.5) over 0.25-gray page must stay ~64 (NOT ~32), got {px:?}"
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
    assert!(
        pct < MAX_DIFF_PCT,
        "GPU vs CPU dense Darken mismatch: {pct:.3}%"
    );
}

/// Regression: on a transparent GPU target, the dense 3-seed affine tiling path
/// must preserve the exact accumulated alpha (~128, NOT ~191) of a 50%-opaque
/// white Normal mark when overlapping Multiply/Screen marks in a separate strip
/// select the dense affine path.
#[test]
fn gpu_dense_affine_tiling_preserves_alpha_on_transparent_target() {
    use zpdf::display_list::{
        BlendMode, Color, DisplayList, FillRule, Paint, Path, RenderCommand, TilingPattern,
    };
    use zpdf_core::{Matrix, Rect};
    use zpdf_render::PageRenderInfo;

    let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 12.0, 4.0));
    let mut r_bot = Path::new();
    r_bot.rect(Rect::new(0.0, 0.0, 2.0, 2.0));
    cell.push(RenderCommand::FillPath {
        path: r_bot,
        rule: FillRule::NonZero,
        paint: Paint::Solid(Color::white()),
        alpha: 0.5,
        overprint: None,
    });
    let mut r_top = Path::new();
    r_top.rect(Rect::new(0.0, 2.0, 12.0, 4.0));
    for k in 0..175 {
        let mode = if k % 2 == 0 {
            BlendMode::Multiply
        } else {
            BlendMode::Screen
        };
        cell.push(RenderCommand::PushBlendGroup {
            blend_mode: mode,
            isolated: true,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 12.0, 4.0),
            alpha: 1.0,
            mask: None,
        });
        cell.push(RenderCommand::FillPath {
            path: r_top.clone(),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::gray(0.5)),
            alpha: 1.0,
            overprint: None,
        });
        cell.push(RenderCommand::PopBlendGroup);
    }
    let tiling = std::sync::Arc::new(TilingPattern {
        commands: std::sync::Arc::new(cell),
        bbox: Rect::new(0.0, 0.0, 12.0, 4.0),
        x_step: 2.0,
        y_step: 4.0,
        matrix: Matrix::identity(),
        tiling_type: 1,
    });
    let page_rect = Rect::new(0.0, 0.0, 140.0, 260.0);
    let mut full = Path::new();
    full.rect(page_rect);
    let cmd = RenderCommand::FillPath {
        path: full,
        rule: FillRule::NonZero,
        paint: Paint::Tiling(tiling),
        alpha: 1.0,
        overprint: None,
    };
    let info = PageRenderInfo {
        page_rect,
        scale: 1.0,
        background: Color::rgba(0.0, 0.0, 0.0, 0.0),
    };

    let gpu_res = with_shared_gpu_renderer(|r| {
        r.begin_page(&info)?;
        r.execute(&cmd)?;
        r.end_page()
    });
    let gpu = match gpu_res {
        Ok(Ok(t)) => t,
        Ok(Err(e)) | Err(e) => {
            eprintln!("skipping GPU dense transparent alpha test (no adapter?): {e}");
            return;
        }
    };
    let mut cpu_r = zpdf::cpu::CpuRenderer::new();
    cpu_r.begin_page(&info).unwrap();
    cpu_r.execute(&cmd).unwrap();
    let cpu = cpu_r.end_page().unwrap();

    let off = ((130 * gpu.width + 70) * 4) as usize;
    let gpu_px = &gpu.data[off..off + 4];
    let cpu_px = &cpu.data[off..off + 4];
    assert!(
        gpu_px.iter().all(|&c| (122..=134).contains(&c)),
        "GPU dense affine tiling 50% white Normal mark on transparent target must retain alpha ~128 (NOT ~191), got {gpu_px:?} (cpu={cpu_px:?})"
    );
    for ch in 0..4 {
        assert!(
            (gpu_px[ch] as i32 - cpu_px[ch] as i32).abs() <= 2,
            "GPU {gpu_px:?} must match CPU {cpu_px:?} on transparent dense affine strip"
        );
    }
}

/// Regression: a dense affine cell with 24 overlapping copies × 176 spanning
/// alternating Multiply/Screen segments (= 4,224 > 4,096) and a trailing black
/// detail confined to `[0..1]×[0..2]` (visited only in the final copy) must
/// preserve both the black detail and the ~170 paired strip on GPU and CPU.
#[test]
fn gpu_dense_affine_tiling_4200_evaluations_preserves_trailing_detail() {
    let gs_mul = b"<< /Type /ExtGState /BM /Multiply >>".to_vec();
    let gs_scr = b"<< /Type /ExtGState /BM /Screen >>".to_vec();
    let mut cell_stream = String::new();
    for k in 0..176 {
        let gs = if k % 2 == 0 { "/GM" } else { "/GS" };
        cell_stream.push_str(&format!("{gs} gs 0.5 0.5 0.5 rg 0 0 48 2 re f\n"));
    }
    cell_stream.push_str("/GM gs 0 0 0 rg 0 0 1 2 re f\n");
    let cell = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 48 2] /XStep 2 /YStep 2 \
         /Resources << /ExtGState << /GM 6 0 R /GS 7 0 R >> >>",
        cell_stream.as_bytes(),
    );
    let content = stream_obj(
        "",
        b"0 0 1 rg 0 0 132 132 re f\n\
          /Pattern cs /P0 scn 0 0 132 132 re f",
    );
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 132 132] /Contents 4 0 R \
          /Resources << /Pattern << /P0 5 0 R >> >> >>"
            .to_vec(),
        content,
        cell,
        gs_mul,
        gs_scr,
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

    let gpu = match render_gpu_shared(&dl, Some(&fonts), Some(&images), 1.0) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("skipping GPU dense 4200-pass affine test (no adapter?): {e}");
            return;
        }
    };
    let cpu = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, 1.0)
        .expect("cpu render");

    let px = |data: &[u8], x: u32, y_pdf: u32| -> [u8; 3] {
        let iy = 131 - y_pdf;
        let off = ((iy * gpu.width + x) * 4) as usize;
        [data[off], data[off + 1], data[off + 2]]
    };
    let black = px(&gpu.data, 66, 66);
    assert!(
        black.iter().all(|&c| c <= 6),
        "GPU trailing black detail past 4,096 affine evaluations must remain black, got {black:?}"
    );
    let gray = px(&gpu.data, 67, 66);
    assert!(
        gray.iter().all(|&c| (165..=176).contains(&c)),
        "GPU paired Multiply/Screen strip past 4,096 evaluations must be ~170, got {gray:?}"
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
    assert!(
        pct < MAX_DIFF_PCT,
        "GPU vs CPU 4200-pass affine mismatch: {pct:.3}%"
    );
}

/// Regression: a `DrawGlyphRun` painted with a non-backdrop-dependent (Normal)
/// solid black tiling pattern inside an inherited clip must apply the clip's
/// anti-aliased edge coverage ONCE — matching a clipped solid black glyph
/// (~128 on a 50%-coverage clip edge, NOT ~191 from multiplying clip coverage
/// in both the color layer and `shape_mask`).
#[test]
fn gpu_clipped_normal_tiling_glyph_applies_inherited_clip_once() {
    let cell = stream_obj(
        "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
         /BBox [0 0 10 10] /XStep 10 /YStep 10 /Resources << >>",
        b"0 0 0 rg 0 0 10 10 re f",
    );
    let build_pdf = |paint_cmds: &[u8]| {
        let content = stream_obj("", paint_cmds);
        assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 80 80] /Contents 4 0 R \
              /Resources << /Pattern << /P0 5 0 R >> /Font << /F1 6 0 R >> >> >>"
                .to_vec(), // 3
            content,                                       // 4
            cell.clone(),                                  // 5
            b"<< /Type /Font /Subtype /TrueType /BaseFont /ZpdfSans /FirstChar 65 \
              /LastChar 65 /Widths [500] /FontDescriptor 7 0 R >>"
                .to_vec(), // 6
            b"<< /Type /FontDescriptor /FontName /ZpdfSans /Flags 32 \
              /FontBBox [0 -200 700 800] /FontFile2 8 0 R >>"
                .to_vec(), // 7
            stream_obj(&format!("/Length1 {}", TEST_TTF.len()), TEST_TTF), // 8
        ])
    };

    // Extract the Normal Paint::Tiling and a solid-black clipped DrawGlyphRun
    // whose opaque square glyph covers x=[10..60], y=[10..70], clipped at the
    // half-pixel vertical edge x = 25.5 (pixel x=25 has ~50% clip coverage).
    let pat_pdf = build_pdf(b"/Pattern cs /P0 scn 0 0 10 10 re f");
    let doc_p = PdfDocument::open(pat_pdf).expect("open pat pdf");
    let page_p = doc_p.page(0).expect("page 0");
    let mut fonts_p = doc_p.load_page_fonts(&page_p);
    let content_p = doc_p.page_content_bytes(&page_p).expect("content");
    let mut images_p = ImageCache::new();
    let pat_dl = ContentInterpreter::new(page_p.media_box)
        .with_fonts(&mut fonts_p)
        .with_document(doc_p.file(), &page_p.resources)
        .with_images(&mut images_p)
        .interpret(&content_p);
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
        .expect("normal tiling paint");

    let solid_pdf = build_pdf(b"q 25.5 0 50 80 re W n 0 0 0 rg BT /F1 80 Tf 10 10 Td (A) Tj ET Q");
    let doc = PdfDocument::open(solid_pdf).expect("open solid pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let content = doc.page_content_bytes(&page).expect("content");
    let mut images = ImageCache::new();
    let solid_dl = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images)
        .interpret(&content);
    let mut tiling_dl = solid_dl.clone();
    let mut patched = 0usize;
    for cmd in &mut tiling_dl.commands {
        if let zpdf::display_list::RenderCommand::DrawGlyphRun(run) = cmd {
            run.paint = tiling_paint.clone();
            patched += 1;
        }
    }
    assert_eq!(patched, 1, "expected one DrawGlyphRun to patch");

    let solid_gpu = match render_gpu_shared(&solid_dl, Some(&fonts), Some(&images), 1.0) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("skipping GPU clipped tiling glyph test (no adapter?): {e}");
            return;
        }
    };
    let tiling_gpu =
        render_gpu_shared(&tiling_dl, Some(&fonts), Some(&images), 1.0).expect("tiling glyph gpu");

    let px = |data: &[u8], x: u32, y_pdf: u32| -> [u8; 3] {
        let iy = 79 - y_pdf;
        let off = ((iy * solid_gpu.width + x) * 4) as usize;
        [data[off], data[off + 1], data[off + 2]]
    };
    let interior = px(&tiling_gpu.data, 35, 35);
    assert!(
        interior.iter().all(|&c| c <= 4),
        "clipped Normal tiling glyph interior must be opaque black, got {interior:?}"
    );
    let edge_solid = px(&solid_gpu.data, 25, 35);
    let edge_tiling = px(&tiling_gpu.data, 25, 35);
    assert!(
        (110..=145).contains(&edge_solid[0]),
        "sanity check: clipped solid glyph edge at x=25.5 must be ~128, got {edge_solid:?}"
    );
    assert!(
        (edge_tiling[0] as i32 - edge_solid[0] as i32).abs() <= 8,
        "clipped Normal tiling glyph edge must match clipped solid glyph ({edge_solid:?}) rather than double-clipping to ~191, got {edge_tiling:?}"
    );
}

#[test]
fn gpu_type3_bitmap_glyph_tiling_preserves_silhouette() {
    use std::sync::Arc;
    use zpdf::display_list::{
        Color, DisplayList, FillRule, GlyphRun, Paint, Path, PositionedGlyph, RenderCommand,
        TilingPattern,
    };
    use zpdf_core::{Matrix, Rect};
    use zpdf_font::{CidWidths, FontCache, LoadedFont, PdfFontType};

    // 8×8 1-bit `/ImageMask true` CharProc: top 4 rows opaque ink (0x00),
    // bottom 4 rows transparent (0xFF).
    let glyph_stream = b"BI /W 8 /H 8 /BPC 1 /IM true ID \x00\x00\x00\x00\xff\xff\xff\xff EI";
    let font = LoadedFont::new_with_data(
        PdfFontType::Type3 {
            font_matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
            char_procs: std::collections::HashMap::from([(
                "A".into(),
                Arc::<[u8]>::from(&glyph_stream[..]),
            )]),
            encoding: vec!["A".into()],
            widths: vec![1.0],
            first_char: 0,
        },
        "Type3BitmapGpu".into(),
        Vec::new(),
        CidWidths::new(1.0),
    );
    let mut fonts = FontCache::new();
    let font_id = fonts.insert("F1".into(), font);

    let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 8.0, 8.0));
    let mut sq = Path::new();
    sq.rect(Rect::new(0.0, 0.0, 8.0, 8.0));
    cell.push(RenderCommand::FillPath {
        path: sq,
        rule: FillRule::NonZero,
        paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
        alpha: 1.0,
        overprint: None,
    });
    let tiling = Arc::new(TilingPattern {
        commands: Arc::new(cell),
        bbox: Rect::new(0.0, 0.0, 8.0, 8.0),
        x_step: 8.0,
        y_step: 8.0,
        matrix: Matrix::identity(),
        tiling_type: 1,
    });

    let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 40.0, 40.0));
    dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
        font_id,
        font_size: 24.0,
        glyphs: vec![PositionedGlyph {
            glyph_id: 0,
            code_len: 1,
            code: b'A' as u32,
            x: 0.0,
            y: 0.0,
            advance: 24.0,
        }],
        paint: Paint::Tiling(tiling),
        alpha: 1.0,
        overprint: None,
        transform: Matrix::translate(8.0, 8.0),
        h_scale: 1.0,
    }));

    let cpu = zpdf_render_cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .render_display_list(&dl, 1.0)
        .expect("cpu render");
    let gpu = match render_gpu_shared(&dl, Some(&fonts), None, 1.0) {
        Ok(p) => p,
        Err(zpdf::gpu::WgpuRenderError::NoAdapter) => {
            eprintln!("skipping GPU Type 3 bitmap glyph test (no adapter)");
            return;
        }
        Err(e) => panic!("GPU Type 3 bitmap glyph render failed: {e}"),
    };

    // Glyph spans PDF [8..32]×[8..32] -> pixel [8..32]×[8..32].
    // Top half (pixel y=14) must be opaque red; bottom half (pixel y=26) must stay white.
    let px = |data: &[u8], w: u32, x: u32, y: u32| -> [u8; 3] {
        let off = ((y * w + x) * 4) as usize;
        [data[off], data[off + 1], data[off + 2]]
    };
    let cpu_top = px(&cpu.data, cpu.width, 20, 14);
    let gpu_top = px(&gpu.data, gpu.width, 20, 14);
    let gpu_bot = px(&gpu.data, gpu.width, 20, 26);
    assert!(
        cpu_top[0] > 245 && cpu_top[1] < 10 && cpu_top[2] < 10,
        "CPU Type 3 bitmap glyph top half must be red, got {cpu_top:?}"
    );
    assert!(
        gpu_top[0] > 245 && gpu_top[1] < 10 && gpu_top[2] < 10,
        "GPU Type 3 bitmap glyph top half must be red, got {gpu_top:?}"
    );
    assert_eq!(
        gpu_bot,
        [255, 255, 255],
        "GPU Type 3 bitmap glyph transparent bottom half must remain white"
    );
}

#[test]
fn gpu_type3_colored_inline_image_applies_text_opacity() {
    use std::sync::Arc;
    use zpdf::display_list::{Color, DisplayList, GlyphRun, Paint, PositionedGlyph, RenderCommand};
    use zpdf_core::{Matrix, Rect};
    use zpdf_font::{CidWidths, FontCache, LoadedFont, PdfFontType};

    // 2×2 opaque red colored (non-mask, `/CS /RGB /BPC 8`) inline image CharProc.
    let glyph_stream =
        b"BI /W 2 /H 2 /CS /RGB /BPC 8 ID \xff\x00\x00\xff\x00\x00\xff\x00\x00\xff\x00\x00 EI";
    let font = LoadedFont::new_with_data(
        PdfFontType::Type3 {
            font_matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
            char_procs: std::collections::HashMap::from([(
                "A".into(),
                Arc::<[u8]>::from(&glyph_stream[..]),
            )]),
            encoding: vec!["A".into()],
            widths: vec![1.0],
            first_char: 0,
        },
        "Type3ColoredInlineOpacityGpu".into(),
        Vec::new(),
        CidWidths::new(1.0),
    );
    let mut fonts = FontCache::new();
    let font_id = fonts.insert("F1".into(), font);

    let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 40.0, 40.0));
    dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
        font_id,
        font_size: 24.0,
        glyphs: vec![PositionedGlyph {
            glyph_id: 0,
            code_len: 1,
            code: b'A' as u32,
            x: 0.0,
            y: 0.0,
            advance: 24.0,
        }],
        paint: Paint::Solid(Color::black()),
        alpha: 0.5,
        overprint: None,
        transform: Matrix::translate(8.0, 8.0),
        h_scale: 1.0,
    }));

    let cpu = zpdf_render_cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .render_display_list(&dl, 1.0)
        .expect("cpu render");
    let gpu = match render_gpu_shared(&dl, Some(&fonts), None, 1.0) {
        Ok(p) => p,
        Err(zpdf::gpu::WgpuRenderError::NoAdapter) => {
            eprintln!("skipping GPU Type 3 colored inline image opacity test (no adapter)");
            return;
        }
        Err(e) => panic!("GPU Type 3 colored inline image opacity render failed: {e}"),
    };

    let px = |data: &[u8], w: u32, x: u32, y: u32| -> [u8; 3] {
        let off = ((y * w + x) * 4) as usize;
        [data[off], data[off + 1], data[off + 2]]
    };
    let cpu_mid = px(&cpu.data, cpu.width, 20, 20);
    let gpu_mid = px(&gpu.data, gpu.width, 20, 20);
    // 50%-opacity opaque red over white = [255, ~128, ~128].
    assert!(
        cpu_mid[0] > 245 && (118..=138).contains(&cpu_mid[1]) && (118..=138).contains(&cpu_mid[2]),
        "CPU 50%-opacity colored Type 3 inline image must be pink ~[255,128,128], got {cpu_mid:?}"
    );
    assert!(
        gpu_mid[0] > 245 && (118..=138).contains(&gpu_mid[1]) && (118..=138).contains(&gpu_mid[2]),
        "GPU 50%-opacity colored Type 3 inline image must apply text opacity (~[255,128,128], not opaque red), got {gpu_mid:?}"
    );
}

#[test]
fn pdf_type3_bitmap_glyph_with_tiling_pattern_interprets_and_renders_cpu_and_gpu() {
    let stream = |dict: &str, bytes: &[u8]| -> Vec<u8> {
        let mut out = format!("<< {dict} /Length {} >>\nstream\n", bytes.len()).into_bytes();
        out.extend_from_slice(bytes);
        out.extend_from_slice(b"\nendstream");
        out
    };
    // 8×8 uncolored Type 3 bitmap glyph (`/IM true`): top 4 rows opaque ink, bottom 4 transparent.
    let charproc =
        b"1 0 0 0 1 1 d1 BI /W 8 /H 8 /BPC 1 /IM true ID \x00\x00\x00\x00\xff\xff\xff\xff EI";
    // 8×8 colored tiling pattern: left 4pt red, right 4pt blue.
    let pat_content = b"1 0 0 rg 0 0 4 8 re f 0 0 1 rg 4 0 4 8 re f";
    let page_content = b"/Pattern cs /P1 scn BT /F1 24 Tf 8 8 Td (A) Tj ET";

    let objects: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 40 40] \
          /Resources << /Font << /F1 5 0 R >> /Pattern << /P1 7 0 R >> >> \
          /Contents 4 0 R >>"
            .to_vec(),
        stream("", page_content),
        b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1 1] \
          /FontMatrix [1 0 0 1 0 0] /CharProcs << /A 6 0 R >> \
          /Encoding << /Type /Encoding /Differences [65 /A] >> \
          /FirstChar 65 /LastChar 65 /Widths [1] >>"
            .to_vec(),
        stream("", charproc),
        stream(
            "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 /BBox [0 0 8 8] /XStep 8 /YStep 8",
            pat_content,
        ),
    ];

    let mut pdf = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (idx, body) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n", idx + 1).as_bytes());
        pdf.extend_from_slice(body);
        pdf.extend_from_slice(b"\nendobj\n");
    }
    let xref = pdf.len();
    let count = objects.len() + 1;
    pdf.extend_from_slice(format!("xref\n0 {count}\n0000000000 65535 f \n").as_bytes());
    for off in offsets {
        pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!("trailer\n<< /Size {count} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
    );

    let doc = zpdf::PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let (content, _) = doc.page_content_with_diagnostics(&page).expect("content");
    let mut images = zpdf_image::ImageCache::new();
    let interpreter = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
        .with_document(doc.file(), &page.resources)
        .with_fonts(&mut fonts)
        .with_images(&mut images);
    let (dl, _) = interpreter.interpret_with_stats(&content);

    assert!(
        dl.commands.iter().any(|cmd| matches!(
            cmd,
            zpdf::display_list::RenderCommand::FillPath {
                paint: zpdf::display_list::Paint::Tiling(_),
                ..
            } | zpdf::display_list::RenderCommand::StrokePath {
                paint: zpdf::display_list::Paint::Tiling(_),
                ..
            } | zpdf::display_list::RenderCommand::DrawGlyphRun(zpdf::display_list::GlyphRun {
                paint: zpdf::display_list::Paint::Tiling(_),
                ..
            })
        )),
        "interpreted PDF display list must carry Paint::Tiling for the Type 3 bitmap glyph"
    );

    let cpu = zpdf_render_cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, 1.0)
        .expect("cpu render");
    let px = |data: &[u8], w: u32, x: u32, y: u32| -> [u8; 3] {
        let off = ((y * w + x) * 4) as usize;
        [data[off], data[off + 1], data[off + 2]]
    };
    // Glyph spans [8..32]×[8..32]; top half (pixel y=14) shows red at x=10 and blue at x=14;
    // bottom half (pixel y=26) stays white.
    let cpu_red = px(&cpu.data, cpu.width, 10, 14);
    let cpu_blue = px(&cpu.data, cpu.width, 14, 14);
    let cpu_bot = px(&cpu.data, cpu.width, 10, 26);
    assert!(
        cpu_red[0] > 240 && cpu_red[1] < 15 && cpu_red[2] < 15,
        "CPU Type 3 bitmap glyph top-left must show red pattern stripe, got {cpu_red:?}"
    );
    assert!(
        cpu_blue[0] < 15 && cpu_blue[1] < 15 && cpu_blue[2] > 240,
        "CPU Type 3 bitmap glyph top-right must show blue pattern stripe, got {cpu_blue:?}"
    );
    assert_eq!(cpu_bot, [255, 255, 255], "CPU bottom half must stay white");

    let gpu = match render_gpu_shared(&dl, Some(&fonts), Some(&images), 1.0) {
        Ok(p) => p,
        Err(zpdf::gpu::WgpuRenderError::NoAdapter) => {
            eprintln!("skipping GPU Type 3 PDF tiling bitmap glyph test (no adapter)");
            return;
        }
        Err(e) => panic!("GPU Type 3 PDF tiling bitmap glyph render failed: {e}"),
    };
    let gpu_red = px(&gpu.data, gpu.width, 10, 14);
    let gpu_blue = px(&gpu.data, gpu.width, 14, 14);
    let gpu_bot = px(&gpu.data, gpu.width, 10, 26);
    assert!(
        gpu_red[0] > 240 && gpu_red[1] < 15 && gpu_red[2] < 15,
        "GPU Type 3 bitmap glyph top-left must show red pattern stripe, got {gpu_red:?}"
    );
    assert!(
        gpu_blue[0] < 15 && gpu_blue[1] < 15 && gpu_blue[2] > 240,
        "GPU Type 3 bitmap glyph top-right must show blue pattern stripe, got {gpu_blue:?}"
    );
    assert_eq!(gpu_bot, [255, 255, 255], "GPU bottom half must stay white");
}

#[test]
fn pdf_type3_colored_d0_image_preserves_rgb_under_tiling_pattern() {
    let stream = |dict: &str, bytes: &[u8]| -> Vec<u8> {
        let mut out = format!("<< {dict} /Length {} >>\nstream\n", bytes.len()).into_bytes();
        out.extend_from_slice(bytes);
        out.extend_from_slice(b"\nendstream");
        out
    };
    // Colored `d0` glyph 'A': 2×2 opaque red DeviceRGB inline image.
    let d0_charproc =
        b"1 0 d0 BI /W 2 /H 2 /CS /RGB /BPC 8 ID \xff\x00\x00\xff\x00\x00\xff\x00\x00\xff\x00\x00 EI";
    // Uncolored `d1` glyph 'B': 8×8 stencil (`/IM true`), all opaque ink.
    let d1_charproc =
        b"1 0 0 0 1 1 d1 BI /W 8 /H 8 /BPC 1 /IM true ID \x00\x00\x00\x00\x00\x00\x00\x00 EI";
    // Colored `d0` vector glyph 'C': explicit red fill (`1 0 0 rg`) + explicit green stroke (`0 1 0 RG`).
    let d0_vec_charproc = b"1 0 d0 1 0 0 rg 0 0 0.5 1 re f 0 1 0 RG 0.3 w 0.75 0.1 m 0.75 0.9 l S";
    // 8×8 opaque blue tiling pattern.
    let pat_content = b"0 0 1 rg 0 0 8 8 re f";
    let page_content = b"/Pattern cs /P1 scn BT /F1 20 Tf 8 10 Td (ABC) Tj ET";

    let objects: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 80 40] \
          /Resources << /Font << /F1 5 0 R >> /Pattern << /P1 9 0 R >> >> \
          /Contents 4 0 R >>"
            .to_vec(),
        stream("", page_content),
        b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1 1] \
          /FontMatrix [1 0 0 1 0 0] /CharProcs << /A 6 0 R /B 7 0 R /C 8 0 R >> \
          /Encoding << /Type /Encoding /Differences [65 /A /B /C] >> \
          /FirstChar 65 /LastChar 67 /Widths [1 1 1] >>"
            .to_vec(),
        stream("", d0_charproc),
        stream("", d1_charproc),
        stream("", d0_vec_charproc),
        stream(
            "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 /BBox [0 0 8 8] /XStep 8 /YStep 8",
            pat_content,
        ),
    ];

    let mut pdf = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (idx, body) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n", idx + 1).as_bytes());
        pdf.extend_from_slice(body);
        pdf.extend_from_slice(b"\nendobj\n");
    }
    let xref = pdf.len();
    let count = objects.len() + 1;
    pdf.extend_from_slice(format!("xref\n0 {count}\n0000000000 65535 f \n").as_bytes());
    for off in offsets {
        pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!("trailer\n<< /Size {count} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
    );

    let doc = zpdf::PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let (content, _) = doc.page_content_with_diagnostics(&page).expect("content");
    let mut images = zpdf_image::ImageCache::new();
    let interpreter = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
        .with_document(doc.file(), &page.resources)
        .with_fonts(&mut fonts)
        .with_images(&mut images);
    let (dl, _) = interpreter.interpret_with_stats(&content);

    let cpu = zpdf_render_cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, 1.0)
        .expect("cpu render");
    let px = |data: &[u8], w: u32, x: u32, y: u32| -> [u8; 3] {
        let off = ((y * w + x) * 4) as usize;
        [data[off], data[off + 1], data[off + 2]]
    };
    // Glyph 'A' (d0 red RGB image) spans PDF x=[8..28], y=[10..30] -> pixel (18, 20).
    // Glyph 'B' (d1 stencil) spans PDF x=[28..48], y=[10..30] -> pixel (38, 20).
    // Glyph 'C' (d0 red vector fill x=[48..58], green stroke x=[63]) -> pixels (53, 20) and (63, 20).
    let cpu_a = px(&cpu.data, cpu.width, 18, 20);
    let cpu_b = px(&cpu.data, cpu.width, 38, 20);
    let cpu_c_fill = px(&cpu.data, cpu.width, 53, 20);
    let cpu_c_stroke = px(&cpu.data, cpu.width, 63, 20);
    assert!(
        cpu_a[0] > 240 && cpu_a[1] < 15 && cpu_a[2] < 15,
        "CPU colored d0 Type 3 RGB image must stay red under blue tiling pattern, got {cpu_a:?}"
    );
    assert!(
        cpu_b[0] < 15 && cpu_b[1] < 15 && cpu_b[2] > 240,
        "CPU uncolored d1 Type 3 stencil must inherit blue tiling pattern, got {cpu_b:?}"
    );
    assert!(
        cpu_c_fill[0] > 240 && cpu_c_fill[1] < 15 && cpu_c_fill[2] < 15,
        "CPU colored d0 Type 3 vector fill must stay red under blue tiling pattern, got {cpu_c_fill:?}"
    );
    assert!(
        cpu_c_stroke[0] < 15 && cpu_c_stroke[1] > 240 && cpu_c_stroke[2] < 15,
        "CPU colored d0 Type 3 vector stroke must stay green under blue tiling pattern, got {cpu_c_stroke:?}"
    );

    let gpu = match render_gpu_shared(&dl, Some(&fonts), Some(&images), 1.0) {
        Ok(p) => p,
        Err(zpdf::gpu::WgpuRenderError::NoAdapter) => {
            eprintln!("skipping GPU Type 3 d0/d1 tiling test (no adapter)");
            return;
        }
        Err(e) => panic!("GPU Type 3 d0/d1 tiling render failed: {e}"),
    };
    let gpu_a = px(&gpu.data, gpu.width, 18, 20);
    let gpu_b = px(&gpu.data, gpu.width, 38, 20);
    let gpu_c_fill = px(&gpu.data, gpu.width, 53, 20);
    let gpu_c_stroke = px(&gpu.data, gpu.width, 63, 20);
    assert!(
        gpu_a[0] > 240 && gpu_a[1] < 15 && gpu_a[2] < 15,
        "GPU colored d0 Type 3 RGB image must preserve red RGB under blue tiling pattern, got {gpu_a:?}"
    );
    assert!(
        gpu_b[0] < 15 && gpu_b[1] < 15 && gpu_b[2] > 240,
        "GPU uncolored d1 Type 3 stencil must inherit blue tiling pattern, got {gpu_b:?}"
    );
    assert!(
        gpu_c_fill[0] > 240 && gpu_c_fill[1] < 15 && gpu_c_fill[2] < 15,
        "GPU colored d0 Type 3 vector fill must preserve red under blue tiling pattern, got {gpu_c_fill:?}"
    );
    assert!(
        gpu_c_stroke[0] < 15 && gpu_c_stroke[1] > 240 && gpu_c_stroke[2] < 15,
        "GPU colored d0 Type 3 vector stroke must preserve green under blue tiling pattern, got {gpu_c_stroke:?}"
    );
}

#[test]
fn gpu_and_cpu_type3_glyph_local_clip_restores_before_second_draw() {
    use std::sync::Arc;
    use zpdf::display_list::{
        Color, DisplayList, FillRule, GlyphRun, Paint, Path, PositionedGlyph, RenderCommand,
        TilingPattern,
    };
    use zpdf_core::{Matrix, Rect};
    use zpdf_font::{CidWidths, FontCache, LoadedFont, PdfFontType};

    // `d1` glyph 'A': clips a full-width `/IM true` image mask to the left 40% (`[0..0.4]`),
    // pops the clip with `Q`, then fills `[0.7..1.0]` — verifying that:
    //  1. the glyph-local clip cuts the mask to `[0..0.4]` (leaving `[0.4..0.7]` white), and
    //  2. `Q` restores the clip before the second draw (`[0.7..1.0]` is painted).
    let clip_charproc = b"1 0 0 0 1 1 d1 \
        q 0 0 0.4 1 re W n \
        BI /W 8 /H 8 /BPC 1 /IM true ID \x00\x00\x00\x00\x00\x00\x00\x00 EI \
        Q \
        0.7 0 0.3 1 re f";
    let font = LoadedFont::new_with_data(
        PdfFontType::Type3 {
            font_matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
            char_procs: std::collections::HashMap::from([(
                "A".into(),
                Arc::<[u8]>::from(&clip_charproc[..]),
            )]),
            encoding: vec!["A".into()],
            widths: vec![1.0],
            first_char: 0,
        },
        "Type3ClipRestore".into(),
        Vec::new(),
        CidWidths::new(1.0),
    );
    let mut fonts = FontCache::new();
    let font_id = fonts.insert("F1".into(), font);

    let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 8.0, 8.0));
    let mut r = Path::new();
    r.rect(Rect::new(0.0, 0.0, 8.0, 8.0));
    cell.push(RenderCommand::FillPath {
        path: r,
        rule: FillRule::NonZero,
        paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
        alpha: 1.0,
        overprint: None,
    });
    let tiling = Arc::new(TilingPattern {
        commands: Arc::new(cell),
        bbox: Rect::new(0.0, 0.0, 8.0, 8.0),
        x_step: 8.0,
        y_step: 8.0,
        matrix: Matrix::identity(),
        tiling_type: 1,
    });

    for (label, paint) in [
        ("solid", Paint::Solid(Color::rgb(0.0, 0.0, 1.0))),
        ("tiling", Paint::Tiling(tiling)),
    ] {
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 60.0, 40.0));
        dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
            font_id,
            font_size: 40.0,
            glyphs: vec![PositionedGlyph {
                glyph_id: 0,
                code_len: 1,
                code: b'A' as u32,
                x: 0.0,
                y: 0.0,
                advance: 40.0,
            }],
            paint,
            alpha: 1.0,
            overprint: None,
            transform: Matrix::translate(10.0, 0.0),
            h_scale: 1.0,
        }));

        let cpu = zpdf_render_cpu::CpuRenderer::new()
            .with_fonts(&fonts)
            .render_display_list(&dl, 1.0)
            .expect("cpu render");
        let px = |data: &[u8], w: u32, x: u32, y: u32| -> [u8; 3] {
            let off = ((y * w + x) * 4) as usize;
            [data[off], data[off + 1], data[off + 2]]
        };
        // Glyph spans x=[10..50]: clipped left mask=[10..26], gap=[26..38], restored second draw=[38..50].
        let check = |backend: &str, data: &[u8], w: u32| {
            let left = px(data, w, 18, 20);
            let gap = px(data, w, 32, 20);
            let right = px(data, w, 44, 20);
            assert!(
                left[0] < 15 && left[1] < 15 && left[2] > 240,
                "{backend} ({label}) clipped left half must be blue, got {left:?}"
            );
            assert_eq!(
                gap,
                [255, 255, 255],
                "{backend} ({label}) clipped-out middle gap must remain white, got {gap:?}"
            );
            assert!(
                right[0] < 15 && right[1] < 15 && right[2] > 240,
                "{backend} ({label}) second draw after Q must be blue, got {right:?}"
            );
        };
        check("CPU", &cpu.data, cpu.width);

        match render_gpu_shared(&dl, Some(&fonts), None, 1.0) {
            Ok(gpu) => check("GPU", &gpu.data, gpu.width),
            Err(zpdf::gpu::WgpuRenderError::NoAdapter) => {
                eprintln!("skipping GPU Type 3 clip test ({label}, no adapter)");
            }
            Err(e) => panic!("GPU Type 3 clip test ({label}) failed: {e}"),
        }
    }
}

#[test]
fn gpu_and_cpu_type3_scaled_rotated_stroke_matches_cpu() {
    use std::sync::Arc;
    use zpdf::display_list::{
        Color, DisplayList, FillRule, GlyphRun, Paint, Path, PositionedGlyph, RenderCommand,
        TilingPattern,
    };
    use zpdf_core::{Matrix, Rect};
    use zpdf_font::{CidWidths, FontCache, LoadedFont, PdfFontType};

    // 90°-rotated FontMatrix (`font_matrix[0] == 0.0`): maps (x, y) -> (-y, x) scaled by 0.01.
    // CharProc draws a horizontal line from (10, 0) to (90, 0) with width 10 in glyph space.
    // With font_size = 10 and run.transform = 4× scale, total stroke_scale = 0.01 * 10 * 4 = 0.4,
    // so the stroked vertical line at page x = 40 spans y = [14..46] with width = 4.0 px.
    let stroke_charproc = b"100 0 0 0 100 100 d1 10 w 10 0 m 90 0 l S";
    let font = LoadedFont::new_with_data(
        PdfFontType::Type3 {
            font_matrix: [0.0, 0.01, -0.01, 0.0, 0.0, 0.0],
            char_procs: std::collections::HashMap::from([(
                "A".into(),
                Arc::<[u8]>::from(&stroke_charproc[..]),
            )]),
            encoding: vec!["A".into()],
            widths: vec![1.0],
            first_char: 0,
        },
        "Type3RotatedStroke".into(),
        Vec::new(),
        CidWidths::new(1.0),
    );
    let mut fonts = FontCache::new();
    let font_id = fonts.insert("F1".into(), font);

    let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 8.0, 8.0));
    let mut r = Path::new();
    r.rect(Rect::new(0.0, 0.0, 8.0, 8.0));
    cell.push(RenderCommand::FillPath {
        path: r,
        rule: FillRule::NonZero,
        paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
        alpha: 1.0,
        overprint: None,
    });
    let tiling = Arc::new(TilingPattern {
        commands: Arc::new(cell),
        bbox: Rect::new(0.0, 0.0, 8.0, 8.0),
        x_step: 8.0,
        y_step: 8.0,
        matrix: Matrix::identity(),
        tiling_type: 1,
    });

    for (label, paint) in [
        ("solid", Paint::Solid(Color::rgb(1.0, 0.0, 0.0))),
        ("tiling", Paint::Tiling(tiling)),
    ] {
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 80.0, 60.0));
        dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
            font_id,
            font_size: 10.0,
            glyphs: vec![PositionedGlyph {
                glyph_id: 0,
                code_len: 1,
                code: b'A' as u32,
                x: 0.0,
                y: 0.0,
                advance: 10.0,
            }],
            paint,
            alpha: 1.0,
            overprint: None,
            transform: Matrix::new(4.0, 0.0, 0.0, 4.0, 40.0, 10.0),
            h_scale: 1.0,
        }));

        let cpu = zpdf_render_cpu::CpuRenderer::new()
            .with_fonts(&fonts)
            .render_display_list(&dl, 1.0)
            .expect("cpu render");
        let gpu = match render_gpu_shared(&dl, Some(&fonts), None, 1.0) {
            Ok(p) => p,
            Err(zpdf::gpu::WgpuRenderError::NoAdapter) => {
                eprintln!("skipping GPU Type 3 rotated stroke test ({label}, no adapter)");
                return;
            }
            Err(e) => panic!("GPU Type 3 rotated stroke test ({label}) failed: {e}"),
        };

        // Count strongly red pixels on the horizontal midline (pixel y = 30).
        // A 4.0 px stroke produces 4 strongly red pixels (x = 38, 39, 40, 41);
        // the old `font_matrix[0]` bug collapsed this to a 0.5 px hairline (<= 1 pixel).
        let red_width = |data: &[u8], w: u32| -> usize {
            (0..w)
                .filter(|&x| {
                    let off = ((30 * w + x) * 4) as usize;
                    data[off] > 200 && data[off + 1] < 80 && data[off + 2] < 80
                })
                .count()
        };
        let cpu_w = red_width(&cpu.data, cpu.width);
        let gpu_w = red_width(&gpu.data, gpu.width);
        assert!(
            cpu_w >= 4,
            "CPU ({label}) 4x-scaled 90deg-rotated Type 3 stroke should span >= 4px, got {cpu_w}"
        );
        assert_eq!(
            gpu_w, cpu_w,
            "GPU ({label}) 4x-scaled 90deg-rotated Type 3 stroke width ({gpu_w}px) must match CPU ({cpu_w}px)"
        );
    }
}

#[test]
fn pdf_type3_named_resource_stencil_do_renders_cpu_and_gpu() {
    let stream = |dict: &str, bytes: &[u8]| -> Vec<u8> {
        let mut out = format!("<< {dict} /Length {} >>\nstream\n", bytes.len()).into_bytes();
        out.extend_from_slice(bytes);
        out.extend_from_slice(b"\nendstream");
        out
    };
    // Glyph 'A': inline `/IM true` 8×8 stencil (all 0x00 = opaque ink).
    let inline_charproc =
        b"1 0 0 0 1 1 d1 BI /W 8 /H 8 /BPC 1 /IM true ID \x00\x00\x00\x00\x00\x00\x00\x00 EI";
    // Glyph 'B': named `/Ink Do` stencil referencing the Type 3 font's `/Resources`.
    let named_charproc = b"1 0 0 0 1 1 d1 /Ink Do";
    let ink_mask_bytes = [0u8; 8];
    let pat_content = b"0 0 1 rg 0 0 8 8 re f";
    let page_content = b"/Pattern cs /P1 scn BT /F1 20 Tf 8 10 Td (AB) Tj ET";

    let objects: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 60 40] \
          /Resources << /Font << /F1 5 0 R >> /Pattern << /P1 9 0 R >> >> \
          /Contents 4 0 R >>"
            .to_vec(),
        stream("", page_content),
        b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1 1] \
          /FontMatrix [1 0 0 1 0 0] \
          /Resources << /XObject << /Ink 8 0 R >> >> \
          /CharProcs << /A 6 0 R /B 7 0 R >> \
          /Encoding << /Type /Encoding /Differences [65 /A /B] >> \
          /FirstChar 65 /LastChar 66 /Widths [1 1] >>"
            .to_vec(),
        stream("", inline_charproc),
        stream("", named_charproc),
        stream(
            "/Type /XObject /Subtype /Image /Width 8 /Height 8 /BitsPerComponent 1 /ImageMask true",
            &ink_mask_bytes,
        ),
        stream(
            "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 /BBox [0 0 8 8] /XStep 8 /YStep 8",
            pat_content,
        ),
    ];

    let mut pdf = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (idx, body) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n", idx + 1).as_bytes());
        pdf.extend_from_slice(body);
        pdf.extend_from_slice(b"\nendobj\n");
    }
    let xref = pdf.len();
    let count = objects.len() + 1;
    pdf.extend_from_slice(format!("xref\n0 {count}\n0000000000 65535 f \n").as_bytes());
    for off in offsets {
        pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!("trailer\n<< /Size {count} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
    );

    let doc = zpdf::PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let (content, _) = doc.page_content_with_diagnostics(&page).expect("content");
    let mut images = zpdf_image::ImageCache::new();
    let interpreter = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
        .with_document(doc.file(), &page.resources)
        .with_fonts(&mut fonts)
        .with_images(&mut images);
    let (dl, _) = interpreter.interpret_with_stats(&content);

    let px = |data: &[u8], w: u32, x: u32, y: u32| -> [u8; 3] {
        let off = ((y * w + x) * 4) as usize;
        [data[off], data[off + 1], data[off + 2]]
    };
    let check = |backend: &str, data: &[u8], w: u32| {
        // Inline 'A' spans x=[8..28], y=[10..30] -> (18, 20).
        // Named `/Ink Do` 'B' spans x=[28..48], y=[10..30] -> (38, 20).
        // Outside background at (4, 20) stays white.
        let a = px(data, w, 18, 20);
        let b = px(data, w, 38, 20);
        let bg = px(data, w, 4, 20);
        assert!(
            a[0] < 15 && a[1] < 15 && a[2] > 240,
            "{backend} inline stencil Type 3 glyph 'A' must be blue, got {a:?}"
        );
        assert!(
            b[0] < 15 && b[1] < 15 && b[2] > 240,
            "{backend} named `/Ink Do` stencil Type 3 glyph 'B' must be blue, got {b:?}"
        );
        assert_eq!(bg, [255, 255, 255], "{backend} margin must stay white");
    };

    let cpu = zpdf_render_cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, 1.0)
        .expect("cpu render");
    check("CPU", &cpu.data, cpu.width);

    match render_gpu_shared(&dl, Some(&fonts), Some(&images), 1.0) {
        Ok(gpu) => check("GPU", &gpu.data, gpu.width),
        Err(zpdf::gpu::WgpuRenderError::NoAdapter) => {
            eprintln!("skipping GPU named Type 3 `/Ink Do` test (no adapter)");
        }
        Err(e) => panic!("GPU named Type 3 `/Ink Do` test failed: {e}"),
    }
}

#[test]
fn gpu_and_cpu_type3_nonuniform_4x1_stroke_preserves_anisotropic_widths() {
    use std::sync::Arc;
    use zpdf::display_list::{
        Color, DisplayList, FillRule, GlyphRun, Paint, Path, PositionedGlyph, RenderCommand,
        TilingPattern,
    };
    use zpdf_core::{Matrix, Rect};
    use zpdf_font::{CidWidths, FontCache, LoadedFont, PdfFontType};

    // Glyph-space pen width 2 under 4× horizontal, 1× vertical text scaling:
    // - Vertical segment at glyph x=4, y=[2..38] -> page x=24, y=[10..46]: 8 px wide horizontally.
    // - Horizontal segment at glyph x=[12..22], y=20 -> page x=[56..96], y=28: 2 px tall vertically.
    let charproc = b"30 0 0 0 30 40 d1 2 w 4 2 m 4 38 l S 12 20 m 22 20 l S";
    let font = LoadedFont::new_with_data(
        PdfFontType::Type3 {
            font_matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
            char_procs: std::collections::HashMap::from([(
                "A".into(),
                Arc::<[u8]>::from(&charproc[..]),
            )]),
            encoding: vec!["A".into()],
            widths: vec![30.0],
            first_char: 0,
        },
        "Type3AnisotropicStroke".into(),
        Vec::new(),
        CidWidths::new(30.0),
    );
    let mut fonts = FontCache::new();
    let font_id = fonts.insert("F1".into(), font);

    let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 8.0, 8.0));
    let mut r = Path::new();
    r.rect(Rect::new(0.0, 0.0, 8.0, 8.0));
    cell.push(RenderCommand::FillPath {
        path: r,
        rule: FillRule::NonZero,
        paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
        alpha: 1.0,
        overprint: None,
    });
    let tiling = Arc::new(TilingPattern {
        commands: Arc::new(cell),
        bbox: Rect::new(0.0, 0.0, 8.0, 8.0),
        x_step: 8.0,
        y_step: 8.0,
        matrix: Matrix::identity(),
        tiling_type: 1,
    });

    for (label, paint) in [
        ("solid", Paint::Solid(Color::rgb(1.0, 0.0, 0.0))),
        ("tiling", Paint::Tiling(tiling)),
    ] {
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 110.0, 56.0));
        dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
            font_id,
            font_size: 1.0,
            glyphs: vec![PositionedGlyph {
                glyph_id: 0,
                code_len: 1,
                code: b'A' as u32,
                x: 0.0,
                y: 0.0,
                advance: 30.0,
            }],
            paint,
            alpha: 1.0,
            overprint: None,
            transform: Matrix::new(4.0, 0.0, 0.0, 1.0, 8.0, 8.0),
            h_scale: 1.0,
        }));

        let check = |backend: &str, data: &[u8], w: u32, h: u32| {
            // Vertical stroke crosses pixel row y = 28 near x = 24; expect ~8 px wide.
            let vert_width = (10..40)
                .filter(|&x| {
                    let off = ((28 * w + x) * 4) as usize;
                    data[off] > 200 && data[off + 1] < 80 && data[off + 2] < 80
                })
                .count();
            // Horizontal stroke crosses pixel col x = 76 near y = 28; expect ~2 px tall.
            let horiz_height = (0..h)
                .filter(|&y| {
                    let off = ((y * w + 76) * 4) as usize;
                    data[off] > 200 && data[off + 1] < 80 && data[off + 2] < 80
                })
                .count();
            assert!(
                (7..=9).contains(&vert_width),
                "{backend} ({label}) 4x1 scaled vertical stroke (glyph w=2) must be ~8px wide, got {vert_width}px"
            );
            assert!(
                (1..=3).contains(&horiz_height),
                "{backend} ({label}) 4x1 scaled horizontal stroke (glyph w=2) must be ~2px tall, got {horiz_height}px"
            );
        };

        let cpu = zpdf_render_cpu::CpuRenderer::new()
            .with_fonts(&fonts)
            .render_display_list(&dl, 1.0)
            .expect("cpu render");
        check("CPU", &cpu.data, cpu.width, cpu.height);

        match render_gpu_shared(&dl, Some(&fonts), None, 1.0) {
            Ok(gpu) => check("GPU", &gpu.data, gpu.width, gpu.height),
            Err(zpdf::gpu::WgpuRenderError::NoAdapter) => {
                eprintln!("skipping GPU 4x1 anisotropic Type 3 stroke test ({label}, no adapter)");
            }
            Err(e) => panic!("GPU 4x1 anisotropic Type 3 stroke test ({label}) failed: {e}"),
        }
    }
}

#[test]
fn pdf_type3_nonuniform_4x1_tiling_stroke_preserves_anisotropic_widths_cpu_and_gpu() {
    // Full PDF-level regression through `PdfDocument::open` + `ContentInterpreter`
    // (`paint_type3_glyph_run`) for a patterned `d1` Type 3 CharProc stroked with
    // a 2-unit pen under a 4×1 nonuniform text transform (`400 Tz` and `4 0 0 1 8 8 Tm`).
    // Vertical segment (glyph x=4, y=2..38 -> page x=24, y=10..46) must render
    // 8 px wide (red left half [20..24], blue right half [24..28]); horizontal
    // segment (glyph x=12..22, y=20 -> page x=56..96, y=28) must render 2 px tall.
    let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
        let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
        out.extend_from_slice(data);
        out.extend_from_slice(b"\nendstream");
        out
    };
    let build_pdf = |page_stream: &[u8]| -> Vec<u8> {
        let objects: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 110 56] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R >> /Pattern << /P1 9 0 R >> >> >>"
                .to_vec(),
            stream("", page_stream),
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 30 40] \
              /FontMatrix [1 0 0 1 0 0] /CharProcs 6 0 R /Encoding 7 0 R \
              /FirstChar 65 /LastChar 65 /Widths [30] /Resources << >> >>"
                .to_vec(),
            b"<< /A 8 0 R >>".to_vec(),
            b"<< /Type /Encoding /Differences [65 /A] >>".to_vec(),
            stream(
                "",
                b"30 0 0 0 30 40 d1 2 w 4 2 m 4 38 l S 12 20 m 22 20 l S",
            ),
            // Two-color 8×8 tiling cell: red [0..4] mod 8, blue [4..8] mod 8,
            // so page x=20..24 (4..8 mod 8) is blue and x=24..28 (0..4 mod 8) is red
            // (verifying both 8px width and page-space pattern alignment).
            stream(
                "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
                 /BBox [0 0 8 8] /XStep 8 /YStep 8 /Resources << >>",
                b"1 0 0 rg 0 0 4 8 re f 0 0 1 rg 4 0 4 8 re f",
            ),
        ];
        let mut pdf = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
        let mut offsets = Vec::with_capacity(objects.len());
        for (idx, body) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n", idx + 1).as_bytes());
            pdf.extend_from_slice(body);
            pdf.extend_from_slice(b"\nendobj\n");
        }
        let xref = pdf.len();
        let count = objects.len() + 1;
        pdf.extend_from_slice(format!("xref\n0 {count}\n0000000000 65535 f \n").as_bytes());
        for off in offsets {
            pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!("trailer\n<< /Size {count} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n")
                .as_bytes(),
        );
        pdf
    };

    for (label, page_stream) in [
        (
            "tiling_Tz400",
            &b"/Pattern cs /P1 scn BT /F1 1 Tf 400 Tz 8 8 Td (A) Tj ET"[..],
        ),
        (
            "tiling_Tm4x1",
            &b"/Pattern cs /P1 scn BT /F1 1 Tf 4 0 0 1 8 8 Tm (A) Tj ET"[..],
        ),
    ] {
        let doc = zpdf::PdfDocument::open(build_pdf(page_stream)).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let mut images = zpdf_image::ImageCache::new();
        let (content, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .interpret_with_stats(&content);

        let check = |backend: &str, data: &[u8], w: u32, h: u32| {
            let is_ink = |x: u32, y: u32| -> bool {
                let off = ((y * w + x) * 4) as usize;
                let (r, g, b) = (data[off], data[off + 1], data[off + 2]);
                g < 80 && (r > 200 || b > 200)
            };
            // Vertical stroke crosses row y = 28 across x = [20..28]: expect ~8 px wide.
            let vert_width = (10..40).filter(|&x| is_ink(x, 28)).count();
            // Horizontal stroke crosses col x = 74 (red stripe) and x = 78 (blue stripe)
            // near y = 28: expect ~2 px tall.
            let horiz_height_red = (0..h).filter(|&y| is_ink(74, y)).count();
            let horiz_height_blue = (0..h).filter(|&y| is_ink(78, y)).count();
            assert!(
                (7..=9).contains(&vert_width),
                "{backend} ({label}) PDF-interpreted 4x1 Type 3 vertical stroke must be ~8px wide, got {vert_width}px"
            );
            assert!(
                (1..=3).contains(&horiz_height_red) && (1..=3).contains(&horiz_height_blue),
                "{backend} ({label}) PDF-interpreted 4x1 Type 3 horizontal stroke must be ~2px tall, got red={horiz_height_red}px blue={horiz_height_blue}px"
            );
            // Verify the tiling pattern painted both blue (x=22, 6 mod 8) and red (x=26, 2 mod 8) halves of the 8px vertical bar.
            let left_off = ((28 * w + 22) * 4) as usize;
            let right_off = ((28 * w + 26) * 4) as usize;
            assert!(
                data[left_off + 2] > 200 && data[left_off] < 80,
                "{backend} ({label}) left half of vertical stroke (x=22) must be blue, got {:?}",
                &data[left_off..left_off + 4]
            );
            assert!(
                data[right_off] > 200 && data[right_off + 2] < 80,
                "{backend} ({label}) right half of vertical stroke (x=26) must be red, got {:?}",
                &data[right_off..right_off + 4]
            );
        };

        let cpu = zpdf_render_cpu::CpuRenderer::new()
            .with_fonts(&fonts)
            .with_images(&images)
            .render_display_list(&dl, 1.0)
            .expect("cpu render");
        check("CPU", &cpu.data, cpu.width, cpu.height);

        match render_gpu_shared(&dl, Some(&fonts), Some(&images), 1.0) {
            Ok(gpu) => check("GPU", &gpu.data, gpu.width, gpu.height),
            Err(zpdf::gpu::WgpuRenderError::NoAdapter) => {
                eprintln!(
                    "skipping GPU PDF-interpreted 4x1 Type 3 tiling stroke test ({label}, no adapter)"
                );
            }
            Err(e) => {
                panic!("GPU PDF-interpreted 4x1 Type 3 tiling stroke test ({label}) failed: {e}")
            }
        }
    }
}

#[test]
fn pdf_image_mask_with_white_multiply_tiling_preserves_blue_backdrop_cpu_and_gpu() {
    // Real PDF with an opaque blue page background and an `/ImageMask true`
    // stencil (both XObject `/MaskImg Do` and inline `BI /IM true ... EI`)
    // painted with a white Multiply tiling pattern. Because Multiply(white, blue)
    // == blue, pixels inside the stencil must stay pure blue (NOT white from
    // isolating against transparent black). Also verifies a 50%-opaque red
    // Multiply stencil darkens the top half once while leaving the bottom half
    // untouched pure blue.
    let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
        let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
        out.extend_from_slice(data);
        out.extend_from_slice(b"\nendstream");
        out
    };
    let build_pdf = |page_stream: &[u8], tile_stream: &[u8]| -> Vec<u8> {
        // 8×8 1-bpc `/ImageMask true`: top 4 rows painted (0x00), bottom 4 transparent (0xff).
        let mask_bits: [u8; 8] = [0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff];
        let objects: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 40 40] /Contents 4 0 R \
              /Resources << /XObject << /MaskImg 5 0 R >> /Pattern << /P1 6 0 R >> \
              /ExtGState << /GS50 << /Type /ExtGState /ca 0.5 >> >> >> >>"
                .to_vec(), // 3
            stream("", page_stream),                       // 4
            stream(
                "/Type /XObject /Subtype /Image /Width 8 /Height 8 /BitsPerComponent 1 /ImageMask true",
                &mask_bits,
            ), // 5
            stream(
                "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
                 /BBox [0 0 10 10] /XStep 10 /YStep 10 \
                 /Resources << /ExtGState << /GSM << /Type /ExtGState /BM /Multiply >> >> >>",
                tile_stream,
            ), // 6
        ];
        let mut pdf = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
        let mut offsets = Vec::with_capacity(objects.len());
        for (idx, body) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n", idx + 1).as_bytes());
            pdf.extend_from_slice(body);
            pdf.extend_from_slice(b"\nendobj\n");
        }
        let xref = pdf.len();
        let count = objects.len() + 1;
        pdf.extend_from_slice(format!("xref\n0 {count}\n0000000000 65535 f \n").as_bytes());
        for off in offsets {
            pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!("trailer\n<< /Size {count} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n")
                .as_bytes(),
        );
        pdf
    };

    let inline_stencil_page = b"0 0 1 rg 0 0 40 40 re f /Pattern cs /P1 scn q 20 0 0 20 10 10 cm \
          BI /W 8 /H 8 /BPC 1 /IM true ID \x00\x00\x00\x00\xff\xff\xff\xff EI Q";
    let xobj_stencil_page =
        b"0 0 1 rg 0 0 40 40 re f /Pattern cs /P1 scn q 20 0 0 20 10 10 cm /MaskImg Do Q";
    let xobj_half_alpha_page =
        b"0 0 1 rg 0 0 40 40 re f /GS50 gs /Pattern cs /P1 scn q 20 0 0 20 10 10 cm /MaskImg Do Q";

    let px = |data: &[u8], w: u32, x: u32, y: u32| -> [u8; 3] {
        let off = ((y * w + x) * 4) as usize;
        [data[off], data[off + 1], data[off + 2]]
    };

    // 1) White Multiply tile through both XObject and inline `/ImageMask true`
    //    over opaque blue -> stencil pixels stay pure blue [0, 0, 255].
    for (label, page_stream) in [
        ("xobject_white_multiply", &xobj_stencil_page[..]),
        ("inline_white_multiply", &inline_stencil_page[..]),
    ] {
        let doc =
            zpdf::PdfDocument::open(build_pdf(page_stream, b"/GSM gs 1 1 1 rg 0 0 10 10 re f"))
                .expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let mut images = zpdf_image::ImageCache::new();
        let (content, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .interpret_with_stats(&content);

        let check = |backend: &str, data: &[u8], w: u32| {
            // Stencil occupies page [10..30]×[10..30]; top half is pixel y=14, bottom half y=26.
            let top = px(data, w, 20, 14);
            let bot = px(data, w, 20, 26);
            assert!(
                top[0] < 10 && top[1] < 10 && top[2] > 245,
                "{backend} ({label}) white Multiply through image stencil over blue must stay blue, got {top:?}"
            );
            assert!(
                bot[0] < 10 && bot[1] < 10 && bot[2] > 245,
                "{backend} ({label}) transparent half of stencil must stay blue, got {bot:?}"
            );
        };

        let cpu = zpdf_render_cpu::CpuRenderer::new()
            .with_fonts(&fonts)
            .with_images(&images)
            .render_display_list(&dl, 1.0)
            .expect("cpu render");
        check("CPU", &cpu.data, cpu.width);

        match render_gpu_shared(&dl, Some(&fonts), Some(&images), 1.0) {
            Ok(gpu) => check("GPU", &gpu.data, gpu.width),
            Err(zpdf::gpu::WgpuRenderError::NoAdapter) => {
                eprintln!("skipping GPU image-mask backdrop test ({label}, no adapter)");
            }
            Err(e) => panic!("GPU image-mask backdrop test ({label}) failed: {e}"),
        }
    }

    // 2) 50%-opaque red Multiply tile through `/ImageMask true` over opaque blue:
    //    Multiply(red, blue) = black, blended at 50% opacity once -> [0, 0, ~128]
    //    in top half, untouched [0, 0, 255] in bottom half.
    {
        let doc = zpdf::PdfDocument::open(build_pdf(
            xobj_half_alpha_page,
            b"/GSM gs 1 0 0 rg 0 0 10 10 re f",
        ))
        .expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let mut images = zpdf_image::ImageCache::new();
        let (content, _) = doc.page_content_with_diagnostics(&page).expect("content");
        let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_images(&mut images)
            .interpret_with_stats(&content);

        let check = |backend: &str, data: &[u8], w: u32| {
            let top = px(data, w, 20, 14);
            let bot = px(data, w, 20, 26);
            assert!(
                top[0] < 10 && top[1] < 10 && (118..=138).contains(&top[2]),
                "{backend} 50% red Multiply through stencil over blue must yield [0,0,~128], got {top:?}"
            );
            assert!(
                bot[0] < 10 && bot[1] < 10 && bot[2] > 245,
                "{backend} transparent half of stencil must stay blue [0,0,255], got {bot:?}"
            );
        };

        let cpu = zpdf_render_cpu::CpuRenderer::new()
            .with_fonts(&fonts)
            .with_images(&images)
            .render_display_list(&dl, 1.0)
            .expect("cpu render");
        check("CPU", &cpu.data, cpu.width);

        match render_gpu_shared(&dl, Some(&fonts), Some(&images), 1.0) {
            Ok(gpu) => check("GPU", &gpu.data, gpu.width),
            Err(zpdf::gpu::WgpuRenderError::NoAdapter) => {
                eprintln!("skipping GPU 50% red Multiply image-mask test (no adapter)");
            }
            Err(e) => panic!("GPU 50% red Multiply image-mask test failed: {e}"),
        }
    }
}

#[test]
fn cpu_and_gpu_nine_level_nested_tiling_renders_innermost_color() {
    use std::sync::Arc;
    use zpdf::display_list::{
        Color, DisplayList, FillRule, Paint, Path, RenderCommand, TilingPattern,
    };
    use zpdf_core::{Matrix, Rect};

    // Build 9 nested opaque single-cell tiling patterns (exceeding the old
    // backend depth cap of 8 while within the interpreter's 16-level limit),
    // with bright green in the innermost cell.
    let mut inner_cell = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
    let mut sq = Path::new();
    sq.rect(Rect::new(0.0, 0.0, 20.0, 20.0));
    inner_cell.push(RenderCommand::FillPath {
        path: sq.clone(),
        rule: FillRule::NonZero,
        paint: Paint::Solid(Color::rgb(0.0, 1.0, 0.0)),
        alpha: 1.0,
        overprint: None,
    });
    let mut cur = Arc::new(TilingPattern {
        commands: Arc::new(inner_cell),
        bbox: Rect::new(0.0, 0.0, 20.0, 20.0),
        x_step: 20.0,
        y_step: 20.0,
        matrix: Matrix::identity(),
        tiling_type: 1,
    });
    for _ in 0..8 {
        let mut parent = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        parent.push(RenderCommand::FillPath {
            path: sq.clone(),
            rule: FillRule::NonZero,
            paint: Paint::Tiling(Arc::clone(&cur)),
            alpha: 1.0,
            overprint: None,
        });
        cur = Arc::new(TilingPattern {
            commands: Arc::new(parent),
            bbox: Rect::new(0.0, 0.0, 20.0, 20.0),
            x_step: 20.0,
            y_step: 20.0,
            matrix: Matrix::identity(),
            tiling_type: 1,
        });
    }

    let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 40.0, 40.0));
    let mut target = Path::new();
    target.rect(Rect::new(10.0, 10.0, 30.0, 30.0));
    dl.push(RenderCommand::FillPath {
        path: target,
        rule: FillRule::NonZero,
        paint: Paint::Tiling(cur),
        alpha: 1.0,
        overprint: None,
    });

    let check = |backend: &str, data: &[u8], w: u32| {
        let off = ((20 * w + 20) * 4) as usize;
        let px = &data[off..off + 4];
        assert!(
            px[0] < 10 && px[1] > 245 && px[2] < 10,
            "{backend} 9-level nested tiling pattern must render innermost bright green, got {px:?}"
        );
    };

    let cpu = zpdf_render_cpu::CpuRenderer::new()
        .render_display_list(&dl, 1.0)
        .expect("cpu render");
    check("CPU", &cpu.data, cpu.width);

    match render_gpu_shared(&dl, None, None, 1.0) {
        Ok(gpu) => check("GPU", &gpu.data, gpu.width),
        Err(zpdf::gpu::WgpuRenderError::NoAdapter) => {
            eprintln!("skipping GPU 9-level nested tiling test (no adapter)");
        }
        Err(e) => panic!("GPU 9-level nested tiling test failed: {e}"),
    }
}

#[test]
fn pdf_scaled_form_nested_hatch_and_translated_page_hatch_render_accurately() {
    let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
        let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
        out.extend_from_slice(data);
        out.extend_from_slice(b"\nendstream");
        out
    };
    // 1) Scaled 2× Form with /POut inheriting Form-local /PHatch (10×10 cell, red [0..5],
    //    blue [5..10]). At 2× Form scale, stripes MUST be 10px wide (red [0..10], blue [10..20]),
    //    NOT 20px wide (4× double-scaled).
    // 2) Two translated placements (dx=0 and dx=5) of /FormPageHatch whose Form-local tile
    //    inherits page-anchored /PageHatch (10×10 cell, green [0..5] mod 10, white [5..10] mod 10):
    //    at page x=22 (2 mod 10, inside dx=0) AND page x=62 (2 mod 10, inside dx=5 placement
    //    spanning x=[55..75]), BOTH must be green, and x=27 / x=67 (7 mod 10) must be white!
    let objects: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 60] /Contents 4 0 R \
          /Resources << /XObject << /FormScaled 5 0 R /FormTrans 8 0 R >> \
                        /Pattern << /PageHatch 10 0 R >> >> >>"
            .to_vec(), // 3
        stream(
            "",
            b"q 2 0 0 2 0 30 cm /FormScaled Do Q \
              q 1 0 0 1 20 0 cm /FormTrans Do Q \
              q 1 0 0 1 55 0 cm /FormTrans Do Q",
        ), // 4
        stream(
            "/Type /XObject /Subtype /Form /BBox [0 0 20 15] \
             /Resources << /Pattern << /POut 6 0 R /PHatch 7 0 R >> >>",
            b"/Pattern cs /POut scn 0 0 20 15 re f",
        ), // 5
        stream(
            "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
             /BBox [0 0 20 15] /XStep 20 /YStep 15 /Resources << >>",
            b"/Pattern cs /PHatch scn 0 0 20 15 re f",
        ), // 6
        stream(
            "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
             /BBox [0 0 10 10] /XStep 10 /YStep 10 /Resources << >>",
            b"1 0 0 rg 0 0 5 10 re f 0 0 1 rg 5 0 5 10 re f",
        ), // 7
        stream(
            "/Type /XObject /Subtype /Form /BBox [0 0 20 20] \
             /Resources << /Pattern << /PInherit 9 0 R >> >>",
            b"/Pattern cs /PInherit scn 0 0 20 20 re f",
        ), // 8
        stream(
            "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
             /BBox [0 0 20 20] /XStep 20 /YStep 20 /Resources << >>",
            b"/Pattern cs /PageHatch scn 0 0 20 20 re f",
        ), // 9
        stream(
            "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
             /BBox [0 0 10 10] /XStep 10 /YStep 10 /Resources << >>",
            b"0 1 0 rg 0 0 5 10 re f",
        ), // 10
    ];
    let mut pdf = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (idx, body) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n", idx + 1).as_bytes());
        pdf.extend_from_slice(body);
        pdf.extend_from_slice(b"\nendobj\n");
    }
    let xref = pdf.len();
    let count = objects.len() + 1;
    pdf.extend_from_slice(format!("xref\n0 {count}\n0000000000 65535 f \n").as_bytes());
    for off in offsets {
        pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!("trailer\n<< /Size {count} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
    );

    let doc = zpdf::PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let mut images = zpdf_image::ImageCache::new();
    let (content, _) = doc.page_content_with_diagnostics(&page).expect("content");
    let (dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
        .with_document(doc.file(), &page.resources)
        .with_fonts(&mut fonts)
        .with_images(&mut images)
        .interpret_with_stats(&content);

    let px = |data: &[u8], w: u32, x: u32, y: u32| -> [u8; 3] {
        let off = ((y * w + x) * 4) as usize;
        [data[off], data[off + 1], data[off + 2]]
    };
    let check = |backend: &str, data: &[u8], w: u32| {
        // 1) Scaled 2× Form covers PDF y=[30..60] (pixel y=[0..30]); sample y=15.
        //    At 2× scale: x=[0..10] red, x=[10..20] blue, x=[20..30] red!
        //    (If wrongly double-scaled to 4×, x=15 would still be red instead of blue.)
        let r1 = px(data, w, 5, 15);
        let b1 = px(data, w, 15, 15);
        let r2 = px(data, w, 25, 15);
        assert!(
            r1[0] > 240 && r1[2] < 15,
            "{backend} scaled Form stripe x=5 must be red, got {r1:?}"
        );
        assert!(
            b1[2] > 240 && b1[0] < 15,
            "{backend} scaled Form stripe x=15 must be blue (2x, not 4x), got {b1:?}"
        );
        assert!(
            r2[0] > 240 && r2[2] < 15,
            "{backend} scaled Form stripe x=25 must be red (2x period 20), got {r2:?}"
        );

        // 2) Translated Forms cover PDF y=[0..20] (pixel y=[40..60]); sample y=50.
        //    Placement 1 spans x=[20..40], Placement 2 spans x=[55..75] (shifted by 35 = 3.5 periods).
        //    Page-anchored /PageHatch is green at x mod 10 in [0..5], white in [5..10]:
        //    so x=22 & x=62 are green, and x=27 & x=67 are white!
        let g1 = px(data, w, 22, 50);
        let w1 = px(data, w, 27, 50);
        let g2 = px(data, w, 62, 50);
        let w2 = px(data, w, 67, 50);
        assert!(
            g1[1] > 240 && g1[0] < 15,
            "{backend} placement 1 x=22 (2 mod 10) must be green, got {g1:?}"
        );
        assert_eq!(
            w1,
            [255, 255, 255],
            "{backend} placement 1 x=27 must be white"
        );
        assert!(
            g2[1] > 240 && g2[0] < 15,
            "{backend} placement 2 x=62 (2 mod 10, half-period shifted Form) must stay green, got {g2:?}"
        );
        assert_eq!(
            w2,
            [255, 255, 255],
            "{backend} placement 2 x=67 (7 mod 10, half-period shifted Form) must stay white"
        );
    };

    let cpu = zpdf_render_cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, 1.0)
        .expect("cpu render");
    check("CPU", &cpu.data, cpu.width);

    match render_gpu_shared(&dl, Some(&fonts), Some(&images), 1.0) {
        Ok(gpu) => check("GPU", &gpu.data, gpu.width),
        Err(zpdf::gpu::WgpuRenderError::NoAdapter) => {
            eprintln!("skipping GPU scaled/translated nested hatch test (no adapter)");
        }
        Err(e) => panic!("GPU scaled/translated nested hatch test failed: {e}"),
    }
}

#[test]
fn type3_ext_gstate_smask_matches_between_live_and_standalone_replay() {
    use zpdf::display_list::{Color, DisplayList, GlyphRun, Paint, PositionedGlyph, RenderCommand};
    use zpdf_core::Matrix;

    // Colored (`d0`) Type 3 glyph 'A' activates `/GSMask gs` (a Luminosity soft
    // mask whose `/G` transparency group invokes `/HalfForm Do` from `/G`'s own
    // `/Resources`, painting white over the left half [0..20] and leaving the
    // right half [20..40] black/transparent), then fills the whole [0..40]x[0..40]
    // square in red.
    // Compare live document interpretation against standalone `DrawGlyphRun`
    // replay (no live PdfFile) across CPU and WGPU: in both paths, the left half
    // must be red and the right half must remain white.
    let stream_obj = |dict: &str, data: &[u8]| -> Vec<u8> {
        let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
        out.extend_from_slice(data);
        out.extend_from_slice(b"\nendstream");
        out
    };
    let objects: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 60 60] /Contents 4 0 R \
          /Resources << /Font << /F1 5 0 R >> >> >>"
            .to_vec(), // 3
        stream_obj("", b"BT /F1 1 Tf 10 10 Td (A) Tj ET"), // 4
        b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 40 40] \
          /FontMatrix [1 0 0 1 0 0] /CharProcs << /A 6 0 R >> \
          /Encoding << /Differences [65 /A] >> /FirstChar 65 /LastChar 65 /Widths [40] \
          /Resources << /ExtGState << /GSMask 7 0 R >> >> >>"
            .to_vec(), // 5
        stream_obj("", b"40 0 d0 /GSMask gs 1 0 0 rg 0 0 40 40 re f"), // 6
        b"<< /Type /ExtGState /SMask << /S /Luminosity /G 8 0 R >> >>".to_vec(), // 7
        stream_obj(
            "/Type /XObject /Subtype /Form /BBox [0 0 40 40] \
             /Group << /Type /Group /S /Transparency /CS /DeviceGray >> \
             /Resources << /XObject << /HalfForm 9 0 R >> >>",
            b"/HalfForm Do",
        ), // 8
        stream_obj(
            "/Type /XObject /Subtype /Form /BBox [0 0 40 40]",
            b"1 g 0 0 20 40 re f",
        ), // 9
    ];
    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (i, body) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        pdf.extend_from_slice(body);
        pdf.extend_from_slice(b"\nendobj\n");
    }
    let xref = pdf.len();
    pdf.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for off in offsets {
        pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );

    let doc = zpdf::PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let mut images = zpdf_image::ImageCache::new();
    let (content, _) = doc.page_content_with_diagnostics(&page).expect("content");

    // 1) Live document interpretation.
    let (live_dl, _) = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
        .with_document(doc.file(), &page.resources)
        .with_fonts(&mut fonts)
        .with_images(&mut images)
        .interpret_with_stats(&content);

    // 2) Standalone `DrawGlyphRun` replay (same loaded Type 3 font with
    //    `type3_resources`, NO live `PdfFile`).
    let (font_id, _) = fonts.get_by_name("F1").expect("F1 loaded");
    let mut standalone_dl = DisplayList::new(page.effective_box());
    standalone_dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
        font_id,
        font_size: 1.0,
        glyphs: vec![PositionedGlyph {
            glyph_id: 65,
            code_len: 1,
            code: 65,
            x: 0.0,
            y: 0.0,
            advance: 40.0,
        }],
        paint: Paint::Solid(Color::black()),
        alpha: 1.0,
        overprint: None,
        transform: Matrix::translate(10.0, 10.0),
        h_scale: 1.0,
    }));

    let px = |data: &[u8], w: u32, x: u32, y: u32| -> [u8; 3] {
        let off = ((y * w + x) * 4) as usize;
        [data[off], data[off + 1], data[off + 2]]
    };
    let assert_half_masked = |label: &str, data: &[u8], w: u32| {
        // Glyph covers PDF x=[10..50], y=[10..50] -> pixel y=30, left x=20, right x=40.
        let left = px(data, w, 20, 30);
        let right = px(data, w, 40, 30);
        assert!(
            left[0] > 240 && left[1] < 15 && left[2] < 15,
            "{label}: unmasked left half must be red, got {left:?}"
        );
        assert!(
            right[0] > 240 && right[1] > 240 && right[2] > 240,
            "{label}: masked right half must stay white, got {right:?}"
        );
    };

    let cpu_live = zpdf_render_cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&live_dl, 1.0)
        .expect("cpu live");
    let cpu_standalone = zpdf_render_cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&zpdf_image::ImageCache::new())
        .render_display_list(&standalone_dl, 1.0)
        .expect("cpu standalone");
    assert_half_masked("CPU live", &cpu_live.data, cpu_live.width);
    assert_half_masked("CPU standalone", &cpu_standalone.data, cpu_standalone.width);
    assert_eq!(
        cpu_live.data, cpu_standalone.data,
        "CPU live and standalone Type 3 /ExtGState /SMask renders must match pixel-for-pixel"
    );

    match render_gpu_shared(&live_dl, Some(&fonts), Some(&images), 1.0) {
        Ok(gpu_live) => {
            let gpu_standalone =
                render_gpu_shared(&standalone_dl, Some(&fonts), None, 1.0).expect("gpu standalone");
            assert_half_masked("GPU live", &gpu_live.data, gpu_live.width);
            assert_half_masked("GPU standalone", &gpu_standalone.data, gpu_standalone.width);
            assert_eq!(
                gpu_live.data, gpu_standalone.data,
                "GPU live and standalone Type 3 /ExtGState /SMask renders must match pixel-for-pixel"
            );
        }
        Err(zpdf::gpu::WgpuRenderError::NoAdapter) => {
            eprintln!("skipping GPU Type 3 /ExtGState /SMask test (no adapter)");
        }
        Err(e) => panic!("GPU Type 3 /ExtGState /SMask test failed: {e}"),
    }
}

#[test]
fn gpu_type3_image_mask_remaps_private_image_id_without_page_collision() {
    use std::collections::HashMap;
    use std::sync::Arc;
    use zpdf::display_list::{
        Color, DisplayList, FillRule, GlyphRun, ImageDraw, Paint, Path, PositionedGlyph,
        RenderCommand, TilingPattern,
    };
    use zpdf_core::{Matrix, PdfDict, PdfName, PdfObject, Rect};
    use zpdf_font::{
        CidWidths, FontCache, LoadedFont, PdfFontType, Type3FormXObject, Type3Resources,
        Type3SoftMask,
    };
    use zpdf_image::{DecodedImage, ImageCache};

    // Mask image (ImageId 0 in glyph's private cache): left white, right black.
    let mask_stream = b"q 20 0 0 20 0 0 cm BI /W 2 /H 1 /BPC 8 /CS /G ID \xff\x00 EI Q";
    let mut form_dict = PdfDict::new();
    form_dict.insert(
        PdfName::new("BBox"),
        PdfObject::Array(vec![
            PdfObject::Real(0.0),
            PdfObject::Real(0.0),
            PdfObject::Real(20.0),
            PdfObject::Real(20.0),
        ]),
    );
    let group = Arc::new(Type3FormXObject {
        dict: form_dict,
        data: Arc::from(&mask_stream[..]),
        resources: None,
    });
    let mut t3_res = Type3Resources::default();
    t3_res.ext_g_state_smasks.insert(
        "GSImg".into(),
        Type3SoftMask::Dict {
            is_alpha: false,
            group,
            backdrop_luma: 0.0,
            transfer: None,
        },
    );
    let d0_proc = b"20 0 d0 /GSImg gs 1 0 0 rg 0 0 20 20 re f";
    let d1_proc = b"20 0 0 0 20 20 d1 /GSImg gs 0 0 20 20 re f";
    let font = LoadedFont::new_with_data(
        PdfFontType::Type3 {
            font_matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
            char_procs: HashMap::from([
                ("A".into(), Arc::<[u8]>::from(&d0_proc[..])),
                ("B".into(), Arc::<[u8]>::from(&d1_proc[..])),
            ]),
            encoding: vec!["A".into(), "B".into()],
            widths: vec![20.0, 20.0],
            first_char: 0,
        },
        "Type3ImgMaskIdCollision".into(),
        Vec::new(),
        CidWidths::new(20.0),
    )
    .with_type3_resources(Arc::new(t3_res));
    let mut fonts = FontCache::new();
    let font_id = fonts.insert("F1".into(), font);

    // Page image also gets ImageId(0), with INVERTED pixels (left black, right white)
    // tinted green so any page/glyph ID collision immediately flips the mask halves.
    let mut page_images = ImageCache::new();
    let page_img_id = page_images.insert(DecodedImage {
        width: 2,
        height: 1,
        data: vec![0, 0, 0, 255, 0, 255, 0, 255],
        has_alpha: false,
        is_image_mask: false,
        premultiplied: true,
    });
    assert_eq!(page_img_id, 0);

    let mut tile_dl = DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0));
    let mut tile_rect = Path::new();
    tile_rect.rect(Rect::new(0.0, 0.0, 10.0, 10.0));
    tile_dl.push(RenderCommand::FillPath {
        path: tile_rect,
        rule: FillRule::NonZero,
        paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
        alpha: 1.0,
        overprint: None,
    });
    let tiling = Arc::new(TilingPattern {
        commands: Arc::new(tile_dl),
        bbox: Rect::new(0.0, 0.0, 10.0, 10.0),
        x_step: 10.0,
        y_step: 10.0,
        matrix: Matrix::identity(),
        tiling_type: 1,
    });

    // Layout across 80x20:
    //   x=[0..20]: solid Type 3 glyph 'A' (red left [0..10], white right [10..20])
    //   x=[25..45]: tiling Type 3 glyph 'B' (blue left [25..35], white right [35..45])
    //   x=[55..75]: page image ID 0 (black left [55..65], green right [65..75])
    let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 80.0, 20.0));
    dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
        font_id,
        font_size: 1.0,
        glyphs: vec![PositionedGlyph {
            glyph_id: 0,
            code_len: 1,
            code: b'A' as u32,
            x: 0.0,
            y: 0.0,
            advance: 20.0,
        }],
        paint: Paint::Solid(Color::black()),
        alpha: 1.0,
        overprint: None,
        transform: Matrix::identity(),
        h_scale: 1.0,
    }));
    dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
        font_id,
        font_size: 1.0,
        glyphs: vec![PositionedGlyph {
            glyph_id: 1,
            code_len: 1,
            code: b'B' as u32,
            x: 25.0,
            y: 0.0,
            advance: 20.0,
        }],
        paint: Paint::Tiling(tiling),
        alpha: 1.0,
        overprint: None,
        transform: Matrix::identity(),
        h_scale: 1.0,
    }));
    dl.push(RenderCommand::DrawImage(ImageDraw {
        image_id: page_img_id,
        transform: Matrix::new(20.0, 0.0, 0.0, 20.0, 55.0, 0.0),
        alpha: 1.0,
        is_image_mask: false,
    }));

    let px = |data: &[u8], w: u32, x: u32, y: u32| -> [u8; 3] {
        let off = ((y * w + x) * 4) as usize;
        [data[off], data[off + 1], data[off + 2]]
    };
    let assert_layout = |label: &str, data: &[u8], w: u32, expect_page_img: bool| {
        let solid_l = px(data, w, 5, 10);
        let solid_r = px(data, w, 15, 10);
        assert!(
            solid_l[0] > 240 && solid_l[1] < 15 && solid_l[2] < 15,
            "{label}: solid glyph left half must be red, got {solid_l:?}"
        );
        assert!(
            solid_r[0] > 240 && solid_r[1] > 240 && solid_r[2] > 240,
            "{label}: solid glyph right half must be white, got {solid_r:?}"
        );
        let tile_l = px(data, w, 30, 10);
        let tile_r = px(data, w, 40, 10);
        assert!(
            tile_l[0] < 15 && tile_l[1] < 15 && tile_l[2] > 240,
            "{label}: tiling glyph left half must be blue, got {tile_l:?}"
        );
        assert!(
            tile_r[0] > 240 && tile_r[1] > 240 && tile_r[2] > 240,
            "{label}: tiling glyph right half must be white, got {tile_r:?}"
        );
        if expect_page_img {
            let pg_l = px(data, w, 60, 10);
            let pg_r = px(data, w, 70, 10);
            assert!(
                pg_l[0] < 15 && pg_l[1] < 15 && pg_l[2] < 15,
                "{label}: page image left half must be black, got {pg_l:?}"
            );
            assert!(
                pg_r[0] < 15 && pg_r[1] > 240 && pg_r[2] < 15,
                "{label}: page image right half must be green, got {pg_r:?}"
            );
        }
    };

    let cpu = zpdf_render_cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&page_images)
        .render_display_list(&dl, 1.0)
        .expect("cpu render");
    assert_layout("CPU", &cpu.data, cpu.width, true);

    // Check GPU both with colliding page ImageId(0) AND with an empty page ImageCache.
    match render_gpu_shared(&dl, Some(&fonts), Some(&page_images), 1.0) {
        Ok(gpu_with_page_img) => {
            assert_layout(
                "GPU with colliding page ImageId(0)",
                &gpu_with_page_img.data,
                gpu_with_page_img.width,
                true,
            );
            let max_diff = gpu_with_page_img
                .data
                .iter()
                .zip(cpu.data.iter())
                .map(|(a, b)| a.abs_diff(*b))
                .max()
                .unwrap_or(0);
            assert!(
                max_diff <= 1,
                "GPU with colliding page ImageId(0) must match CPU within 1 LSB, max_diff={max_diff}"
            );
            let gpu_empty_page_cache =
                render_gpu_shared(&dl, Some(&fonts), None, 1.0).expect("gpu empty page cache");
            assert_layout(
                "GPU with empty page ImageCache",
                &gpu_empty_page_cache.data,
                gpu_empty_page_cache.width,
                false,
            );
        }
        Err(zpdf::gpu::WgpuRenderError::NoAdapter) => {
            eprintln!("skipping GPU Type 3 mask image ID test (no adapter)");
        }
        Err(e) => panic!("GPU Type 3 mask image ID test failed: {e}"),
    }
}

#[test]
fn type3_luminosity_mask_named_shading_matches_live_on_cpu_and_gpu() {
    use zpdf::display_list::{Color, DisplayList, GlyphRun, Paint, PositionedGlyph, RenderCommand};
    use zpdf_core::{Matrix, Rect};

    // Full PDF with a Type 3 font whose CharProc applies a luminosity soft mask
    // (`/GSFade gs`) whose `/G` transparency group paints an axial grayscale
    // gradient via `/Fade sh` (`0` at x=0 -> `1` at x=80) over a red rectangle.
    let page_content = b"BT /F1 1 Tf (A) Tj ET";
    let charproc = b"80 0 d0 /GSFade gs 1 0 0 rg 0 0 80 20 re f";
    let mask_group_stream = b"/Fade sh";
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 80 20] /Contents 4 0 R \
          /Resources << /Font << /F1 5 0 R >> >> >>"
            .to_vec(), // 3
        stream_obj("", page_content),                  // 4
        b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 80 20] \
          /FontMatrix [1 0 0 1 0 0] /CharProcs << /A 6 0 R >> \
          /Encoding << /Type /Encoding /Differences [65 /A] >> \
          /FirstChar 65 /LastChar 65 /Widths [80] \
          /Resources << /ExtGState << /GSFade 7 0 R >> >> >>"
            .to_vec(), // 5
        stream_obj("", charproc),                      // 6
        b"<< /Type /ExtGState /SMask << /S /Luminosity /G 8 0 R >> >>".to_vec(), // 7
        stream_obj(
            "/Type /XObject /Subtype /Form /BBox [0 0 80 20] \
             /Group << /Type /Group /S /Transparency /CS /DeviceGray >> \
             /Resources << /Shading << /Fade 9 0 R >> >>",
            mask_group_stream,
        ), // 8
        b"<< /ShadingType 2 /ColorSpace /DeviceGray /Coords [0 0 80 0] \
          /Function 10 0 R /Extend [true true] >>"
            .to_vec(), // 9
        b"<< /FunctionType 2 /Domain [0 1] /C0 [0] /C1 [1] /N 1 >>".to_vec(), // 10
    ]);

    let doc = PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let content = doc.page_content_bytes(&page).expect("content bytes");
    let mut live_images = ImageCache::new();
    let live_dl = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut live_images)
        .interpret(&content);
    assert!(
        !live_dl
            .commands
            .iter()
            .any(|c| matches!(c, RenderCommand::DrawGlyphRun(_))),
        "live document interpretation should inline Type 3 glyph rather than emit DrawGlyphRun"
    );
    let live_cpu = zpdf_render_cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&live_images)
        .render_display_list(&live_dl, 1.0)
        .expect("live CPU render");

    // Standalone `DrawGlyphRun` replay using ONLY the loaded `FontCache` (no
    // live `PdfFile` and no page `ImageCache`).
    let (font_id, _) = fonts.get_by_name("F1").expect("loaded F1 font id");
    let mut standalone_dl = DisplayList::new(Rect::new(0.0, 0.0, 80.0, 20.0));
    standalone_dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
        font_id,
        font_size: 1.0,
        glyphs: vec![PositionedGlyph {
            glyph_id: 65,
            code_len: 1,
            code: b'A' as u32,
            x: 0.0,
            y: 0.0,
            advance: 80.0,
        }],
        paint: Paint::Solid(Color::black()),
        alpha: 1.0,
        overprint: None,
        transform: Matrix::identity(),
        h_scale: 1.0,
    }));
    let standalone_cpu = zpdf_render_cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .render_display_list(&standalone_dl, 1.0)
        .expect("standalone CPU render");

    let px = |data: &[u8], w: u32, x: u32, y: u32| -> [u8; 3] {
        let off = ((y * w + x) * 4) as usize;
        [data[off], data[off + 1], data[off + 2]]
    };
    let assert_fade = |label: &str, data: &[u8], w: u32| {
        let left = px(data, w, 2, 10);
        let mid = px(data, w, 40, 10);
        let right = px(data, w, 78, 10);
        assert!(
            left[0] > 240 && left[1] > 235 && left[2] > 235,
            "{label}: left edge must fade to white, got {left:?}"
        );
        assert!(
            mid[0] > 240 && mid[1].abs_diff(128) <= 8 && mid[2].abs_diff(128) <= 8,
            "{label}: midpoint must be ~half-red, got {mid:?}"
        );
        assert!(
            right[0] > 240 && right[1] < 20 && right[2] < 20,
            "{label}: right edge must be solid red, got {right:?}"
        );
    };
    assert_fade("live CPU", &live_cpu.data, live_cpu.width);
    assert_fade("standalone CPU", &standalone_cpu.data, standalone_cpu.width);

    let cpu_max_diff = live_cpu
        .data
        .iter()
        .zip(standalone_cpu.data.iter())
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0);
    assert!(
        cpu_max_diff <= 3,
        "standalone CPU Type 3 shading mask must match live CPU within 3 LSB, max_diff={cpu_max_diff}"
    );

    match render_gpu_shared(&standalone_dl, Some(&fonts), None, 1.0) {
        Ok(standalone_gpu) => {
            assert_fade("standalone GPU", &standalone_gpu.data, standalone_gpu.width);
            let gpu_max_diff = live_cpu
                .data
                .iter()
                .zip(standalone_gpu.data.iter())
                .map(|(a, b)| a.abs_diff(*b))
                .max()
                .unwrap_or(0);
            assert!(
                gpu_max_diff <= 3,
                "standalone GPU Type 3 shading mask must match live CPU within 3 LSB, max_diff={gpu_max_diff}"
            );
        }
        Err(zpdf::gpu::WgpuRenderError::NoAdapter) => {
            eprintln!("skipping GPU Type 3 shading mask test (no adapter)");
        }
        Err(e) => panic!("GPU Type 3 shading mask test failed: {e}"),
    }
}

#[test]
fn type3_charproc_shading_negative_left_bearing_matches_live_on_cpu_and_gpu() {
    use zpdf::display_list::{Color, DisplayList, GlyphRun, Paint, PositionedGlyph, RenderCommand};
    use zpdf_core::{Matrix, Rect};

    // Colored Type 3 CharProc clipped to [-40..40, 0..20] (crossing x = 0)
    // painting a red->blue axial shading directly via `/Fade sh`. Placed at
    // x = 50 on a 100x20 page so page x = [10..90] is fully visible.
    let page_content = b"BT /F1 1 Tf 50 0 Td (A) Tj ET";
    let charproc = b"60 0 d0 -40 0 80 20 re W n /Fade sh";
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 20] /Contents 4 0 R \
          /Resources << /Font << /F1 5 0 R >> >> >>"
            .to_vec(), // 3
        stream_obj("", page_content),                  // 4
        b"<< /Type /Font /Subtype /Type3 /FontBBox [-40 0 40 20] \
          /FontMatrix [1 0 0 1 0 0] /CharProcs << /A 6 0 R >> \
          /Encoding << /Type /Encoding /Differences [65 /A] >> \
          /FirstChar 65 /LastChar 65 /Widths [60] \
          /Resources << /Shading << /Fade 7 0 R >> >> >>"
            .to_vec(), // 5
        stream_obj("", charproc),                      // 6
        b"<< /ShadingType 2 /ColorSpace /DeviceRGB /Coords [-40 0 40 0] \
          /Function 8 0 R /Extend [true true] >>"
            .to_vec(), // 7
        b"<< /FunctionType 2 /Domain [0 1] /C0 [1 0 0] /C1 [0 0 1] /N 1 >>".to_vec(), // 8
    ]);

    let doc = PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let content = doc.page_content_bytes(&page).expect("content bytes");
    let mut live_images = ImageCache::new();
    let live_dl = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut live_images)
        .interpret(&content);
    assert!(
        !live_dl
            .commands
            .iter()
            .any(|c| matches!(c, RenderCommand::DrawGlyphRun(_))),
        "live interpretation should inline Type 3 glyph"
    );
    let live_cpu = zpdf_render_cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&live_images)
        .render_display_list(&live_dl, 1.0)
        .expect("live CPU");

    let (font_id, _) = fonts.get_by_name("F1").expect("loaded F1");
    let mut standalone_dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 20.0));
    standalone_dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
        font_id,
        font_size: 1.0,
        glyphs: vec![PositionedGlyph {
            glyph_id: 65,
            code_len: 1,
            code: b'A' as u32,
            x: 50.0,
            y: 0.0,
            advance: 60.0,
        }],
        paint: Paint::Solid(Color::black()),
        alpha: 1.0,
        overprint: None,
        transform: Matrix::identity(),
        h_scale: 1.0,
    }));
    let standalone_cpu = zpdf_render_cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .render_display_list(&standalone_dl, 1.0)
        .expect("standalone CPU");

    let px = |data: &[u8], w: u32, x: u32, y: u32| -> [u8; 3] {
        let off = ((y * w + x) * 4) as usize;
        [data[off], data[off + 1], data[off + 2]]
    };
    let assert_overhang = |label: &str, data: &[u8], w: u32| {
        let left = px(data, w, 12, 10);
        let center = px(data, w, 50, 10);
        let right = px(data, w, 88, 10);
        assert!(
            left[0] > 235 && left[1] < 15 && left[2] < 20,
            "{label}: negative left overhang x=12 must be red, got {left:?}"
        );
        assert!(
            center[0].abs_diff(128) <= 8 && center[1] < 15 && center[2].abs_diff(128) <= 8,
            "{label}: center x=50 must be purple, got {center:?}"
        );
        assert!(
            right[0] < 20 && right[1] < 15 && right[2] > 235,
            "{label}: right side x=88 must be blue, got {right:?}"
        );
    };
    assert_overhang("live CPU", &live_cpu.data, live_cpu.width);
    assert_overhang("standalone CPU", &standalone_cpu.data, standalone_cpu.width);

    let cpu_max_diff = live_cpu
        .data
        .iter()
        .zip(standalone_cpu.data.iter())
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0);
    assert!(
        cpu_max_diff <= 3,
        "standalone CPU negative-left-bearing shading must match live CPU within 3 LSB, max_diff={cpu_max_diff}"
    );

    match render_gpu_shared(&standalone_dl, Some(&fonts), None, 1.0) {
        Ok(standalone_gpu) => {
            assert_overhang("standalone GPU", &standalone_gpu.data, standalone_gpu.width);
            let gpu_max_diff = live_cpu
                .data
                .iter()
                .zip(standalone_gpu.data.iter())
                .map(|(a, b)| a.abs_diff(*b))
                .max()
                .unwrap_or(0);
            assert!(
                gpu_max_diff <= 3,
                "standalone GPU negative-left-bearing shading must match live CPU within 3 LSB, max_diff={gpu_max_diff}"
            );
        }
        Err(zpdf::gpu::WgpuRenderError::NoAdapter) => {
            eprintln!("skipping GPU negative-left-bearing shading test (no adapter)");
        }
        Err(e) => panic!("GPU negative-left-bearing shading test failed: {e}"),
    }
}

#[test]
fn type3_soft_mask_stitching_transfer_with_sampled_child_matches_live() {
    use zpdf::display_list::{Color, DisplayList, GlyphRun, Paint, PositionedGlyph, RenderCommand};
    use zpdf_core::{Matrix, Rect};

    // Soft mask `/G` paints left half gray 0.25 and right half gray 0.75.
    // `/TR` is a Type 3 stitching function whose two `/Functions` children are
    // indirect Type 0 sampled streams:
    //   - `[0.0..0.5]` -> sampled `[255, 255]` (maps 0.25 -> 1.0 -> full red)
    //   - `[0.5..1.0]` -> sampled `[0, 0]`     (maps 0.75 -> 0.0 -> white)
    // If the sampled children are dropped, `/TR` falls back to identity and
    // paints pale red on left and dark red on right instead.
    let page_content = b"BT /F1 1 Tf (A) Tj ET";
    let charproc = b"40 0 d0 /GSMask gs 1 0 0 rg 0 0 40 20 re f";
    let mask_group_stream = b"0.25 g 0 0 20 20 re f 0.75 g 20 0 20 20 re f";
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 40 20] /Contents 4 0 R \
          /Resources << /Font << /F1 5 0 R >> >> >>"
            .to_vec(), // 3
        stream_obj("", page_content),                  // 4
        b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 40 20] \
          /FontMatrix [1 0 0 1 0 0] /CharProcs << /A 6 0 R >> \
          /Encoding << /Type /Encoding /Differences [65 /A] >> \
          /FirstChar 65 /LastChar 65 /Widths [40] \
          /Resources << /ExtGState << /GSMask 7 0 R >> >> >>"
            .to_vec(), // 5
        stream_obj("", charproc),                      // 6
        b"<< /Type /ExtGState /SMask << /S /Luminosity /G 8 0 R /TR 9 0 R >> >>".to_vec(), // 7
        stream_obj(
            "/Type /XObject /Subtype /Form /BBox [0 0 40 20] \
             /Group << /Type /Group /S /Transparency /CS /DeviceGray >>",
            mask_group_stream,
        ), // 8
        b"<< /FunctionType 3 /Domain [0 1] /Bounds [0.5] /Encode [0 1 0 1] \
          /Functions [10 0 R 11 0 R] >>"
            .to_vec(), // 9
        stream_obj(
            "/FunctionType 0 /Domain [0 1] /Range [0 1] /Size [2] /BitsPerSample 8",
            &[255, 255],
        ), // 10
        stream_obj(
            "/FunctionType 0 /Domain [0 1] /Range [0 1] /Size [2] /BitsPerSample 8",
            &[0, 0],
        ), // 11
    ]);

    let doc = PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let content = doc.page_content_bytes(&page).expect("content bytes");
    let mut live_images = ImageCache::new();
    let live_dl = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut live_images)
        .interpret(&content);
    let live_cpu = zpdf_render_cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&live_images)
        .render_display_list(&live_dl, 1.0)
        .expect("live CPU");

    let (font_id, _) = fonts.get_by_name("F1").expect("loaded F1");
    let mut standalone_dl = DisplayList::new(Rect::new(0.0, 0.0, 40.0, 20.0));
    standalone_dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
        font_id,
        font_size: 1.0,
        glyphs: vec![PositionedGlyph {
            glyph_id: 65,
            code_len: 1,
            code: b'A' as u32,
            x: 0.0,
            y: 0.0,
            advance: 40.0,
        }],
        paint: Paint::Solid(Color::black()),
        alpha: 1.0,
        overprint: None,
        transform: Matrix::identity(),
        h_scale: 1.0,
    }));
    let standalone_cpu = zpdf_render_cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .render_display_list(&standalone_dl, 1.0)
        .expect("standalone CPU");

    let px = |data: &[u8], w: u32, x: u32, y: u32| -> [u8; 3] {
        let off = ((y * w + x) * 4) as usize;
        [data[off], data[off + 1], data[off + 2]]
    };
    let assert_transfer = |label: &str, data: &[u8], w: u32| {
        let left = px(data, w, 10, 10);
        let right = px(data, w, 30, 10);
        assert!(
            left[0] > 240 && left[1] < 15 && left[2] < 15,
            "{label}: left half (0.25 -> 1.0 via sampled child) must be solid red, got {left:?}"
        );
        assert!(
            right[0] > 240 && right[1] > 240 && right[2] > 240,
            "{label}: right half (0.75 -> 0.0 via sampled child) must be white, got {right:?}"
        );
    };
    assert_transfer("live CPU", &live_cpu.data, live_cpu.width);
    assert_transfer("standalone CPU", &standalone_cpu.data, standalone_cpu.width);

    let cpu_max_diff = live_cpu
        .data
        .iter()
        .zip(standalone_cpu.data.iter())
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0);
    assert!(
        cpu_max_diff <= 2,
        "standalone CPU stitching /TR with sampled child must match live CPU within 2 LSB, max_diff={cpu_max_diff}"
    );

    match render_gpu_shared(&standalone_dl, Some(&fonts), None, 1.0) {
        Ok(standalone_gpu) => {
            assert_transfer("standalone GPU", &standalone_gpu.data, standalone_gpu.width);
            let gpu_max_diff = live_cpu
                .data
                .iter()
                .zip(standalone_gpu.data.iter())
                .map(|(a, b)| a.abs_diff(*b))
                .max()
                .unwrap_or(0);
            assert!(
                gpu_max_diff <= 2,
                "standalone GPU stitching /TR with sampled child must match live CPU within 2 LSB, max_diff={gpu_max_diff}"
            );
        }
        Err(zpdf::gpu::WgpuRenderError::NoAdapter) => {
            eprintln!("skipping GPU stitching /TR sampled-child test (no adapter)");
        }
        Err(e) => panic!("GPU stitching /TR sampled-child test failed: {e}"),
    }
}

#[test]
fn type3_stitching_shading_reusing_sampled_subfunction_renders_both_intervals_within_budget() {
    use zpdf::display_list::{Color, DisplayList, GlyphRun, Paint, PositionedGlyph, RenderCommand};
    use zpdf_core::{Matrix, ParseLimits, Rect};

    // Axial shading using a Type 3 stitching function whose two intervals
    // (`[0..0.5]` with Encode `[0 1]`, `[0.5..1]` with Encode `[1 0]`) BOTH
    // reference the SAME 16384-byte Type 0 sampled ramp `11 0 R` -> creates a
    // symmetric 0 -> 1 -> 0 luminosity fade under a tight 30000-byte font cache
    // budget (fits one 16384-byte payload + font/encoding/form/mask overhead,
    // strictly less than two copies = 32768+ bytes).
    let samples: Vec<u8> = (0..16_384).map(|i| ((i * 255) / 16_383) as u8).collect();
    let page_content = b"BT /F1 1 Tf (A) Tj ET";
    let charproc = b"80 0 d0 /GSFade gs 1 0 0 rg 0 0 80 20 re f";
    let mask_group_stream = b"/Fade sh";
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 80 20] /Contents 4 0 R \
          /Resources << /Font << /F1 5 0 R >> >> >>"
            .to_vec(), // 3
        stream_obj("", page_content),                  // 4
        b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 80 20] \
          /FontMatrix [1 0 0 1 0 0] /CharProcs << /A 6 0 R >> \
          /Encoding << /Type /Encoding /Differences [65 /A] >> \
          /FirstChar 65 /LastChar 65 /Widths [80] \
          /Resources << /ExtGState << /GSFade 7 0 R >> >> >>"
            .to_vec(), // 5
        stream_obj("", charproc),                      // 6
        b"<< /Type /ExtGState /SMask << /S /Luminosity /G 8 0 R >> >>".to_vec(), // 7
        stream_obj(
            "/Type /XObject /Subtype /Form /BBox [0 0 80 20] \
             /Group << /Type /Group /S /Transparency /CS /DeviceGray >> \
             /Resources << /Shading << /Fade 9 0 R >> >>",
            mask_group_stream,
        ), // 8
        b"<< /ShadingType 2 /ColorSpace /DeviceGray /Coords [0 0 80 0] \
          /Function 10 0 R /Extend [true true] >>"
            .to_vec(), // 9
        b"<< /FunctionType 3 /Domain [0 1] /Bounds [0.5] /Encode [0 1 1 0] \
          /Functions [11 0 R 11 0 R] >>"
            .to_vec(), // 10
        stream_obj(
            "/FunctionType 0 /Domain [0 1] /Range [0 1] /Size [16384] /BitsPerSample 8",
            &samples,
        ), // 11
    ]);

    let limits = ParseLimits {
        max_font_cache_bytes: 30_000,
        ..Default::default()
    };
    let doc = PdfDocument::open_with_limits(pdf, limits).expect("open pdf with tight budget");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let content = doc.page_content_bytes(&page).expect("content bytes");
    let mut live_images = ImageCache::new();
    let live_dl = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut live_images)
        .interpret(&content);
    let live_cpu = zpdf_render_cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&live_images)
        .render_display_list(&live_dl, 1.0)
        .expect("live CPU");

    let (font_id, _) = fonts.get_by_name("F1").expect("loaded F1");
    let mut standalone_dl = DisplayList::new(Rect::new(0.0, 0.0, 80.0, 20.0));
    standalone_dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
        font_id,
        font_size: 1.0,
        glyphs: vec![PositionedGlyph {
            glyph_id: 65,
            code_len: 1,
            code: b'A' as u32,
            x: 0.0,
            y: 0.0,
            advance: 80.0,
        }],
        paint: Paint::Solid(Color::black()),
        alpha: 1.0,
        overprint: None,
        transform: Matrix::identity(),
        h_scale: 1.0,
    }));
    let standalone_cpu = zpdf_render_cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .render_display_list(&standalone_dl, 1.0)
        .expect("standalone CPU");

    let px = |data: &[u8], w: u32, x: u32, y: u32| -> [u8; 3] {
        let off = ((y * w + x) * 4) as usize;
        [data[off], data[off + 1], data[off + 2]]
    };
    let assert_symmetric_fade = |label: &str, data: &[u8], w: u32| {
        let left = px(data, w, 2, 10);
        let q1 = px(data, w, 20, 10);
        let mid = px(data, w, 40, 10);
        let q3 = px(data, w, 60, 10);
        let right = px(data, w, 78, 10);
        assert!(
            left[0] > 240 && left[1] > 225 && left[2] > 225,
            "{label}: left edge must fade to white, got {left:?}"
        );
        assert!(
            q1[0] > 240 && q1[1].abs_diff(128) <= 12 && q1[2].abs_diff(128) <= 12,
            "{label}: first interval midpoint (x=20) must be ~half-red, got {q1:?}"
        );
        assert!(
            mid[0] > 240 && mid[1] < 20 && mid[2] < 20,
            "{label}: peak (x=40) must be solid red, got {mid:?}"
        );
        assert!(
            q3[0] > 240 && q3[1].abs_diff(128) <= 12 && q3[2].abs_diff(128) <= 12,
            "{label}: second interval midpoint (x=60) must be ~half-red, got {q3:?}"
        );
        assert!(
            right[0] > 240 && right[1] > 225 && right[2] > 225,
            "{label}: right edge must fade back to white, got {right:?}"
        );
    };
    assert_symmetric_fade("live CPU", &live_cpu.data, live_cpu.width);
    assert_symmetric_fade("standalone CPU", &standalone_cpu.data, standalone_cpu.width);

    let cpu_max_diff = live_cpu
        .data
        .iter()
        .zip(standalone_cpu.data.iter())
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0);
    assert!(
        cpu_max_diff <= 3,
        "standalone CPU reused stitching subfunction must match live CPU within 3 LSB, max_diff={cpu_max_diff}"
    );
}

#[test]
fn type3_unclipped_charproc_radial_shading_past_1000_units_matches_live() {
    use zpdf::display_list::{Color, DisplayList, GlyphRun, Paint, PositionedGlyph, RenderCommand};
    use zpdf_core::{Matrix, Rect};

    // Unclipped `d0` CharProc (no `W` clip!) painting a non-extended radial
    // gradient centered at (1800, 250) with radius 200 — completely outside the
    // old [-1000..1000] placeholder rect. Test BOTH declared `/FontBBox [0 0 2500 500]`
    // AND all-zero `/FontBBox [0 0 0 0]` (unspecified glyph extent per ISO 32000-1
    // Table 112). `/FontMatrix [0.04 0 0 0.04 0 0]` maps glyph [0..2500, 0..500]
    // onto the 100x20 page so the radial circle sits at page center (72, 10), radius 8.
    let page_content = b"BT /F1 1 Tf (A) Tj ET";
    let charproc = b"2500 0 d0 /Fade sh";
    let px = |data: &[u8], w: u32, x: u32, y: u32| -> [u8; 3] {
        let off = ((y * w + x) * 4) as usize;
        [data[off], data[off + 1], data[off + 2]]
    };

    for (case_label, font_bbox_pdf) in [
        ("declared /FontBBox [0 0 2500 500]", "[0 0 2500 500]"),
        ("all-zero /FontBBox [0 0 0 0]", "[0 0 0 0]"),
    ] {
        let font_dict = format!(
            "<< /Type /Font /Subtype /Type3 /FontBBox {font_bbox_pdf} \
             /FontMatrix [0.04 0 0 0.04 0 0] /CharProcs << /A 6 0 R >> \
             /Encoding << /Type /Encoding /Differences [65 /A] >> \
             /FirstChar 65 /LastChar 65 /Widths [2500] \
             /Resources << /Shading << /Fade 7 0 R >> >> >>"
        );
        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 20] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_vec(), // 3
            stream_obj("", page_content),                  // 4
            font_dict.into_bytes(),                        // 5
            stream_obj("", charproc),                      // 6
            b"<< /ShadingType 3 /ColorSpace /DeviceRGB \
              /Coords [1800 250 0 1800 250 200] /Function 8 0 R /Extend [false false] >>"
                .to_vec(), // 7
            b"<< /FunctionType 2 /Domain [0 1] /C0 [1 0 0] /C1 [0 0 1] /N 1 >>".to_vec(), // 8
        ]);

        let doc = PdfDocument::open(pdf).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let mut fonts = doc.load_page_fonts(&page);
        let content = doc.page_content_bytes(&page).expect("content bytes");
        let mut live_images = ImageCache::new();
        let live_dl = ContentInterpreter::new(page.media_box)
            .with_fonts(&mut fonts)
            .with_document(doc.file(), &page.resources)
            .with_images(&mut live_images)
            .interpret(&content);
        assert!(
            !live_dl
                .commands
                .iter()
                .any(|c| matches!(c, RenderCommand::DrawGlyphRun(_))),
            "{case_label}: live interpretation should inline Type 3 glyph"
        );
        let live_cpu = zpdf_render_cpu::CpuRenderer::new()
            .with_fonts(&fonts)
            .with_images(&live_images)
            .render_display_list(&live_dl, 1.0)
            .expect("live CPU");

        let (font_id, _) = fonts.get_by_name("F1").expect("loaded F1");
        let mut standalone_dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 20.0));
        standalone_dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
            font_id,
            font_size: 1.0,
            glyphs: vec![PositionedGlyph {
                glyph_id: 65,
                code_len: 1,
                code: b'A' as u32,
                x: 0.0,
                y: 0.0,
                advance: 100.0,
            }],
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
            transform: Matrix::identity(),
            h_scale: 1.0,
        }));
        let standalone_cpu = zpdf_render_cpu::CpuRenderer::new()
            .with_fonts(&fonts)
            .render_display_list(&standalone_dl, 1.0)
            .expect("standalone CPU");

        let assert_radial = |label: &str, data: &[u8], w: u32| {
            let center = px(data, w, 72, 10);
            let rim = px(data, w, 78, 10);
            let outside = px(data, w, 20, 10);
            assert!(
                center[0] > 210 && center[1] < 25 && center[2] < 60,
                "{case_label} ({label}): radial center (72, 10) must be red, got {center:?}"
            );
            assert!(
                rim[2] > 150 && rim[1] < 25,
                "{case_label} ({label}): radial rim (78, 10) must be bluish, got {rim:?}"
            );
            assert!(
                outside[0] > 240 && outside[1] > 240 && outside[2] > 240,
                "{case_label} ({label}): outside non-extended radial gradient must be white, got {outside:?}"
            );
        };
        assert_radial("live CPU", &live_cpu.data, live_cpu.width);
        assert_radial("standalone CPU", &standalone_cpu.data, standalone_cpu.width);

        let assert_matches_live = |label: &str, data: &[u8]| {
            // Standalone rasterizes the tight finite radial bounds at high
            // resolution (768x768) before downscaling onto the 16x16 page
            // circle; every pixel outside the 1.5px rim band around the sharp
            // non-extended boundary (r = 8.0 centered at (72, 10)) must closely
            // match live CPU (within 10 LSB for C0 cone-apex box downscaling).
            for &(x, y) in &[(72u32, 10u32), (75, 10), (78, 10), (20, 10)] {
                let a = px(&live_cpu.data, live_cpu.width, x, y);
                let b = px(data, live_cpu.width, x, y);
                for ch in 0..3 {
                    assert!(
                        a[ch].abs_diff(b[ch]) <= 10,
                        "{case_label} ({label}): sample ({x}, {y}) {b:?} must match live CPU {a:?}"
                    );
                }
            }
            for y in 0..live_cpu.height {
                for x in 0..live_cpu.width {
                    let dx = (x as f64 + 0.5) - 72.0;
                    let dy = (live_cpu.height as f64 - (y as f64 + 0.5)) - 10.0;
                    let r = dx.hypot(dy);
                    if (r - 8.0).abs() < 1.5 {
                        continue;
                    }
                    let a = px(&live_cpu.data, live_cpu.width, x, y);
                    let b = px(data, live_cpu.width, x, y);
                    for ch in 0..3 {
                        assert!(
                            a[ch].abs_diff(b[ch]) <= 10,
                            "{case_label} ({label}): non-rim pixel ({x}, {y}) (r={r:.2}) {b:?} diverged from live CPU {a:?}"
                        );
                    }
                }
            }
        };
        assert_matches_live("standalone CPU", &standalone_cpu.data);

        match render_gpu_shared(&standalone_dl, Some(&fonts), None, 1.0) {
            Ok(standalone_gpu) => {
                assert_radial("standalone GPU", &standalone_gpu.data, standalone_gpu.width);
                assert_matches_live("standalone GPU", &standalone_gpu.data);
            }
            Err(zpdf::gpu::WgpuRenderError::NoAdapter) => {
                eprintln!("skipping GPU unclipped radial shading test (no adapter)");
            }
            Err(e) => panic!("GPU unclipped radial shading test failed: {e}"),
        }
    }

    // Also verify an UNBOUNDED extended axial shading (`/Extend [true true]`,
    // no `/BBox`) in an unclipped `d0` CharProc with `/FontBBox [0 0 0 0]` uses
    // inverse-mapped destination page bounds to paint past x = 1000 glyph units.
    let axial_pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 20] /Contents 4 0 R \
          /Resources << /Font << /F1 5 0 R >> >> >>"
            .to_vec(), // 3
        stream_obj("", page_content),                  // 4
        b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 0 0] \
          /FontMatrix [0.04 0 0 0.04 0 0] /CharProcs << /A 6 0 R >> \
          /Encoding << /Type /Encoding /Differences [65 /A] >> \
          /FirstChar 65 /LastChar 65 /Widths [2500] \
          /Resources << /Shading << /Fade 7 0 R >> >> >>"
            .to_vec(), // 5
        stream_obj("", charproc),                      // 6
        b"<< /ShadingType 2 /ColorSpace /DeviceRGB \
          /Coords [0 0 500 0] /Function 8 0 R /Extend [true true] >>"
            .to_vec(), // 7
        b"<< /FunctionType 2 /Domain [0 1] /C0 [1 0 0] /C1 [0 0 1] /N 1 >>".to_vec(), // 8
    ]);
    let axial_doc = PdfDocument::open(axial_pdf).expect("open axial pdf");
    let axial_page = axial_doc.page(0).expect("page 0");
    let mut axial_fonts = axial_doc.load_page_fonts(&axial_page);
    let axial_content = axial_doc.page_content_bytes(&axial_page).expect("content");
    let mut axial_live_images = ImageCache::new();
    let axial_live_dl = ContentInterpreter::new(axial_page.media_box)
        .with_fonts(&mut axial_fonts)
        .with_document(axial_doc.file(), &axial_page.resources)
        .with_images(&mut axial_live_images)
        .interpret(&axial_content);
    let axial_live_cpu = zpdf_render_cpu::CpuRenderer::new()
        .with_fonts(&axial_fonts)
        .with_images(&axial_live_images)
        .render_display_list(&axial_live_dl, 1.0)
        .expect("axial live CPU");

    let (axial_fid, _) = axial_fonts.get_by_name("F1").expect("loaded F1");
    let mut axial_standalone_dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 20.0));
    axial_standalone_dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
        font_id: axial_fid,
        font_size: 1.0,
        glyphs: vec![PositionedGlyph {
            glyph_id: 65,
            code_len: 1,
            code: b'A' as u32,
            x: 0.0,
            y: 0.0,
            advance: 100.0,
        }],
        paint: Paint::Solid(Color::black()),
        alpha: 1.0,
        overprint: None,
        transform: Matrix::identity(),
        h_scale: 1.0,
    }));
    let axial_standalone_cpu = zpdf_render_cpu::CpuRenderer::new()
        .with_fonts(&axial_fonts)
        .render_display_list(&axial_standalone_dl, 1.0)
        .expect("axial standalone CPU");
    // x = 80 corresponds to glyph x = 2000 (past 1000!), extended to pure blue.
    for (label, data, w) in [
        ("axial live CPU", &axial_live_cpu.data, axial_live_cpu.width),
        (
            "axial standalone CPU",
            &axial_standalone_cpu.data,
            axial_standalone_cpu.width,
        ),
    ] {
        let far_right = px(data, w, 80, 10);
        assert!(
            far_right[0] < 15 && far_right[1] < 15 && far_right[2] > 240,
            "{label}: extended axial shading past x=1000 must be solid blue, got {far_right:?}"
        );
    }
    match render_gpu_shared(&axial_standalone_dl, Some(&axial_fonts), None, 1.0) {
        Ok(axial_gpu) => {
            let far_right = px(&axial_gpu.data, axial_gpu.width, 80, 10);
            assert!(
                far_right[0] < 15 && far_right[1] < 15 && far_right[2] > 240,
                "axial standalone GPU: extended axial shading past x=1000 must be solid blue, got {far_right:?}"
            );
        }
        Err(zpdf::gpu::WgpuRenderError::NoAdapter) => {
            eprintln!("skipping GPU extended axial shading test (no adapter)");
        }
        Err(e) => panic!("GPU extended axial shading test failed: {e}"),
    }
}

#[test]
fn type3_luminosity_mask_shading_bbox_clips_extended_gradient_matches_live() {
    use zpdf::display_list::{
        Color, DisplayList, FillRule, GlyphRun, Paint, Path, PositionedGlyph, RenderCommand,
    };
    use zpdf_core::{Matrix, Rect};

    // Page paints a solid blue backdrop across [0 0 80 20], then draws a Type 3
    // glyph whose luminosity soft mask invokes `/Fade sh`. `/Fade` is an axial
    // black->white gradient over Coords [20 0 60 0] with `/Extend [true true]`
    // AND shading `/BBox [20 0 60 20]`. Outside `/BBox` (x < 20 and x > 60),
    // the shading must NOT paint into the mask (leaving mask luminosity 0), so
    // the red glyph fill stays hidden and pixels outside `/BBox` keep their
    // original blue backdrop value.
    let page_content = b"0 0 1 rg 0 0 80 20 re f BT /F1 1 Tf (A) Tj ET";
    let charproc = b"80 0 d0 /GSFade gs 1 0 0 rg 0 0 80 20 re f";
    let mask_group_stream = b"/Fade sh";
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 80 20] /Contents 4 0 R \
          /Resources << /Font << /F1 5 0 R >> >> >>"
            .to_vec(), // 3
        stream_obj("", page_content),                  // 4
        b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 80 20] \
          /FontMatrix [1 0 0 1 0 0] /CharProcs << /A 6 0 R >> \
          /Encoding << /Type /Encoding /Differences [65 /A] >> \
          /FirstChar 65 /LastChar 65 /Widths [80] \
          /Resources << /ExtGState << /GSFade 7 0 R >> >> >>"
            .to_vec(), // 5
        stream_obj("", charproc),                      // 6
        b"<< /Type /ExtGState /SMask << /S /Luminosity /G 8 0 R >> >>".to_vec(), // 7
        stream_obj(
            "/Type /XObject /Subtype /Form /BBox [0 0 80 20] \
             /Group << /Type /Group /S /Transparency /CS /DeviceGray >> \
             /Resources << /Shading << /Fade 9 0 R >> >>",
            mask_group_stream,
        ), // 8
        b"<< /ShadingType 2 /ColorSpace /DeviceGray /Coords [20 0 60 0] \
          /BBox [20 0 60 20] /Function 10 0 R /Extend [true true] >>"
            .to_vec(), // 9
        b"<< /FunctionType 2 /Domain [0 1] /C0 [0] /C1 [1] /N 1 >>".to_vec(), // 10
    ]);

    let doc = PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let content = doc.page_content_bytes(&page).expect("content bytes");
    let mut live_images = ImageCache::new();
    let live_dl = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut live_images)
        .interpret(&content);
    let live_cpu = zpdf_render_cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&live_images)
        .render_display_list(&live_dl, 1.0)
        .expect("live CPU");

    let (font_id, _) = fonts.get_by_name("F1").expect("loaded F1");
    let mut standalone_dl = DisplayList::new(Rect::new(0.0, 0.0, 80.0, 20.0));
    let mut bg = Path::new();
    bg.rect(Rect::new(0.0, 0.0, 80.0, 20.0));
    standalone_dl.push(RenderCommand::FillPath {
        path: bg,
        rule: FillRule::NonZero,
        paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
        alpha: 1.0,
        overprint: None,
    });
    standalone_dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
        font_id,
        font_size: 1.0,
        glyphs: vec![PositionedGlyph {
            glyph_id: 65,
            code_len: 1,
            code: b'A' as u32,
            x: 0.0,
            y: 0.0,
            advance: 80.0,
        }],
        paint: Paint::Solid(Color::black()),
        alpha: 1.0,
        overprint: None,
        transform: Matrix::identity(),
        h_scale: 1.0,
    }));
    let standalone_cpu = zpdf_render_cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .render_display_list(&standalone_dl, 1.0)
        .expect("standalone CPU");

    let px = |data: &[u8], w: u32, x: u32, y: u32| -> [u8; 3] {
        let off = ((y * w + x) * 4) as usize;
        [data[off], data[off + 1], data[off + 2]]
    };
    let assert_bbox_clipped = |label: &str, data: &[u8], w: u32| {
        let left_out = px(data, w, 10, 10);
        let right_out = px(data, w, 70, 10);
        let mid_in = px(data, w, 40, 10);
        let high_in = px(data, w, 58, 10);
        assert!(
            left_out[0] < 5 && left_out[1] < 5 && left_out[2] > 250,
            "{label}: pixel left of shading /BBox (x=10) must keep original blue backdrop, got {left_out:?}"
        );
        assert!(
            right_out[0] < 5 && right_out[1] < 5 && right_out[2] > 250,
            "{label}: pixel right of shading /BBox (x=70) must keep original blue backdrop despite /Extend [true true], got {right_out:?}"
        );
        assert!(
            mid_in[0].abs_diff(128) <= 15 && mid_in[2].abs_diff(128) <= 15,
            "{label}: pixel inside shading /BBox (x=40) must blend ~50% red over blue, got {mid_in:?}"
        );
        assert!(
            high_in[0] > 220 && high_in[2] < 35,
            "{label}: pixel near right edge inside shading /BBox (x=58) must be mostly red, got {high_in:?}"
        );
    };
    assert_bbox_clipped("live CPU", &live_cpu.data, live_cpu.width);
    assert_bbox_clipped("standalone CPU", &standalone_cpu.data, standalone_cpu.width);

    let cpu_max_diff = live_cpu
        .data
        .iter()
        .zip(standalone_cpu.data.iter())
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0);
    assert!(
        cpu_max_diff <= 3,
        "standalone CPU shading /BBox clip must match live CPU within 3 LSB, max_diff={cpu_max_diff}"
    );

    match render_gpu_shared(&standalone_dl, Some(&fonts), None, 1.0) {
        Ok(standalone_gpu) => {
            assert_bbox_clipped("standalone GPU", &standalone_gpu.data, standalone_gpu.width);
            let gpu_max_diff = live_cpu
                .data
                .iter()
                .zip(standalone_gpu.data.iter())
                .map(|(a, b)| a.abs_diff(*b))
                .max()
                .unwrap_or(0);
            assert!(
                gpu_max_diff <= 3,
                "standalone GPU shading /BBox clip must match live CPU within 3 LSB, max_diff={gpu_max_diff}"
            );
        }
        Err(zpdf::gpu::WgpuRenderError::NoAdapter) => {
            eprintln!("skipping GPU shading /BBox test (no adapter)");
        }
        Err(e) => panic!("GPU shading /BBox test failed: {e}"),
    }
}

#[test]
fn type3_all_zero_bbox_axial_two_placements_and_zero_scale_prefix_matches_live() {
    use zpdf::display_list::{
        Color, DisplayList, FillRule, GlyphRun, Paint, Path, PositionedGlyph, RenderCommand,
    };
    use zpdf_core::{Matrix, Rect};

    let px = |data: &[u8], w: u32, x: u32, y: u32| -> [u8; 3] {
        let off = ((y * w + x) * 4) as usize;
        [data[off], data[off + 1], data[off + 2]]
    };

    // Case 1: Same all-zero `/FontBBox` unclipped axial-shading Type 3 glyph
    // rendered at two distinct x positions on one 100x20 page:
    //   - Placement 1 (clipped to [0..40]): x = 10 -> x=5 red, x=35 blue
    //   - Placement 2 (clipped to [50..100]): x = 70 -> x=55 red, x=95 blue
    // If GPU/SVG image caches reused Placement 1's texture under local
    // `image_id = 0`, Placement 2 at x = 55 would sample blue instead of red.
    let two_page_content = b"q 0 0 40 20 re W n BT /F1 1 Tf 10 0 Td (A) Tj ET Q \
          q 50 0 50 20 re W n BT /F1 1 Tf 70 0 Td (A) Tj ET Q";
    let charproc = b"2500 0 d0 /Fade sh";
    let pdf_two = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), // 1
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(), // 2
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 20] /Contents 4 0 R \
          /Resources << /Font << /F1 5 0 R >> >> >>"
            .to_vec(), // 3
        stream_obj("", two_page_content),              // 4
        b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 0 0] \
          /FontMatrix [0.04 0 0 0.04 0 0] /CharProcs << /A 6 0 R >> \
          /Encoding << /Type /Encoding /Differences [65 /A] >> \
          /FirstChar 65 /LastChar 65 /Widths [2500] \
          /Resources << /Shading << /Fade 7 0 R >> >> >>"
            .to_vec(), // 5
        stream_obj("", charproc),                      // 6
        b"<< /ShadingType 2 /ColorSpace /DeviceRGB \
          /Coords [0 0 500 0] /Function 8 0 R /Extend [true true] >>"
            .to_vec(), // 7
        b"<< /FunctionType 2 /Domain [0 1] /C0 [1 0 0] /C1 [0 0 1] /N 1 >>".to_vec(), // 8
    ]);

    let doc = PdfDocument::open(pdf_two).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let content = doc.page_content_bytes(&page).expect("content bytes");
    let mut live_images = ImageCache::new();
    let live_dl = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut live_images)
        .interpret(&content);
    let live_cpu = zpdf_render_cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&live_images)
        .render_display_list(&live_dl, 1.0)
        .expect("live CPU");

    let (font_id, _) = fonts.get_by_name("F1").expect("loaded F1");
    let mut clip1 = Path::new();
    clip1.rect(Rect::new(0.0, 0.0, 40.0, 20.0));
    let mut clip2 = Path::new();
    clip2.rect(Rect::new(50.0, 0.0, 100.0, 20.0));
    let mut standalone_two = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 20.0));
    standalone_two.push(RenderCommand::PushClip {
        path: clip1,
        rule: FillRule::NonZero,
    });
    standalone_two.push(RenderCommand::DrawGlyphRun(GlyphRun {
        font_id,
        font_size: 1.0,
        glyphs: vec![PositionedGlyph {
            glyph_id: 65,
            code_len: 1,
            code: b'A' as u32,
            x: 10.0,
            y: 0.0,
            advance: 100.0,
        }],
        paint: Paint::Solid(Color::black()),
        alpha: 1.0,
        overprint: None,
        transform: Matrix::identity(),
        h_scale: 1.0,
    }));
    standalone_two.push(RenderCommand::PopClip);
    standalone_two.push(RenderCommand::PushClip {
        path: clip2,
        rule: FillRule::NonZero,
    });
    standalone_two.push(RenderCommand::DrawGlyphRun(GlyphRun {
        font_id,
        font_size: 1.0,
        glyphs: vec![PositionedGlyph {
            glyph_id: 65,
            code_len: 1,
            code: b'A' as u32,
            x: 70.0,
            y: 0.0,
            advance: 100.0,
        }],
        paint: Paint::Solid(Color::black()),
        alpha: 1.0,
        overprint: None,
        transform: Matrix::identity(),
        h_scale: 1.0,
    }));
    standalone_two.push(RenderCommand::PopClip);

    let standalone_cpu_two = zpdf_render_cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .render_display_list(&standalone_two, 1.0)
        .expect("standalone CPU two placements");

    let assert_two_placements = |label: &str, data: &[u8], w: u32| {
        let p1_red = px(data, w, 5, 10);
        let p1_blue = px(data, w, 35, 10);
        let p2_red = px(data, w, 55, 10);
        let p2_blue = px(data, w, 95, 10);
        assert!(
            p1_red[0] > 240 && p1_red[2] < 15,
            "{label}: placement 1 left (x=5) must be red, got {p1_red:?}"
        );
        assert!(
            p1_blue[0] < 15 && p1_blue[2] > 240,
            "{label}: placement 1 right (x=35) must be blue, got {p1_blue:?}"
        );
        assert!(
            p2_red[0] > 240 && p2_red[2] < 15,
            "{label}: placement 2 left (x=55) must be red (not reused placement 1 texture), got {p2_red:?}"
        );
        assert!(
            p2_blue[0] < 15 && p2_blue[2] > 240,
            "{label}: placement 2 right (x=95) must be blue, got {p2_blue:?}"
        );
    };
    assert_two_placements("live CPU", &live_cpu.data, live_cpu.width);
    assert_two_placements(
        "standalone CPU",
        &standalone_cpu_two.data,
        standalone_cpu_two.width,
    );
    let cpu_max_diff = live_cpu
        .data
        .iter()
        .zip(standalone_cpu_two.data.iter())
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0);
    assert!(
        cpu_max_diff <= 4,
        "standalone CPU two placements must match live CPU within 4 LSB, max_diff={cpu_max_diff}"
    );

    match render_gpu_shared(&standalone_two, Some(&fonts), None, 1.0) {
        Ok(standalone_gpu_two) => {
            assert_two_placements(
                "standalone GPU",
                &standalone_gpu_two.data,
                standalone_gpu_two.width,
            );
            let gpu_max_diff = live_cpu
                .data
                .iter()
                .zip(standalone_gpu_two.data.iter())
                .map(|(a, b)| a.abs_diff(*b))
                .max()
                .unwrap_or(0);
            assert!(
                gpu_max_diff <= 4,
                "standalone GPU two placements must match live CPU within 4 LSB, max_diff={gpu_max_diff}"
            );
        }
        Err(zpdf::gpu::WgpuRenderError::NoAdapter) => {
            eprintln!("skipping GPU two-placement test (no adapter)");
        }
        Err(e) => panic!("GPU two-placement test failed: {e}"),
    }

    // Case 2: A zero-scale (singular) occurrence BEFORE a visible occurrence
    // must NOT cache the provisional placeholder program and crop the visible
    // glyph at x = 1000 glyph units (page x = 40).
    let mut standalone_zero = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 20.0));
    standalone_zero.push(RenderCommand::DrawGlyphRun(GlyphRun {
        font_id,
        font_size: 0.0,
        glyphs: vec![PositionedGlyph {
            glyph_id: 65,
            code_len: 1,
            code: b'A' as u32,
            x: 0.0,
            y: 0.0,
            advance: 0.0,
        }],
        paint: Paint::Solid(Color::black()),
        alpha: 1.0,
        overprint: None,
        transform: Matrix::scale(0.0, 0.0),
        h_scale: 1.0,
    }));
    standalone_zero.push(RenderCommand::DrawGlyphRun(GlyphRun {
        font_id,
        font_size: 1.0,
        glyphs: vec![PositionedGlyph {
            glyph_id: 65,
            code_len: 1,
            code: b'A' as u32,
            x: 0.0,
            y: 0.0,
            advance: 100.0,
        }],
        paint: Paint::Solid(Color::black()),
        alpha: 1.0,
        overprint: None,
        transform: Matrix::identity(),
        h_scale: 1.0,
    }));
    let cpu_zero = zpdf_render_cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .render_display_list(&standalone_zero, 1.0)
        .expect("standalone CPU zero-scale prefix");
    let cpu_far = px(&cpu_zero.data, cpu_zero.width, 80, 10);
    assert!(
        cpu_far[0] < 15 && cpu_far[1] < 15 && cpu_far[2] > 240,
        "standalone CPU visible glyph after zero-scale occurrence must paint blue past x=1000, got {cpu_far:?}"
    );
    match render_gpu_shared(&standalone_zero, Some(&fonts), None, 1.0) {
        Ok(gpu_zero) => {
            let gpu_far = px(&gpu_zero.data, gpu_zero.width, 80, 10);
            assert!(
                gpu_far[0] < 15 && gpu_far[1] < 15 && gpu_far[2] > 240,
                "standalone GPU visible glyph after zero-scale occurrence must paint blue past x=1000, got {gpu_far:?}"
            );
        }
        Err(zpdf::gpu::WgpuRenderError::NoAdapter) => {
            eprintln!("skipping GPU zero-scale prefix test (no adapter)");
        }
        Err(e) => panic!("GPU zero-scale prefix test failed: {e}"),
    }
}
