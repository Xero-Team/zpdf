//! The real pipeline, decomposed into its five measurable stages.
//!
//! The stages exist so a benchmark can attribute wall-clock time instead of
//! guessing:
//!
//! | stage | what it covers | where it is timed |
//! |---|---|---|
//! | `parse` | read the file, `PdfDocument::open` (xref, lazy repair), resolve the page | here |
//! | `interpret` | load page fonts, decode the content stream, produce the `DisplayList` | here |
//! | `render` | drive a backend over the display list | here + `StageStats` inside |
//! | `encode` | PNG-encode the rendered RGBA | here |
//! | `total` | all of the above, as a consumer actually pays it | here |
//!
//! Two stages were previously invisible in every recorded number. **Encode** was
//! never measured at all — the CLI's `image::save_buffer` on a multi-megapixel
//! page is not free, and no earlier benchmark included it. And **parse** +
//! **interpret** were only ever inferred by subtracting a render-only number from
//! a full-pipeline one; the note recorded in the performance doc that "parser is
//! light relative to rendering" was an assumption, never a measurement.
//!
//! Stage boundaries follow what the CLI does (`zpdf-cli/src/main.rs`), so an
//! `interpret` measurement here describes the same display list the CLI renders.

use std::path::{Path, PathBuf};

use zpdf::display_list::DisplayList;
use zpdf::{FontCache, IccCache, ImageCache, PdfDocument, PdfPage};
use zpdf_core::Rect;

/// Everything a render backend needs to draw one page, with the parse and
/// interpret work already done. The caches live here so the backend can borrow
/// them.
pub struct PageSetup {
    pub dl: DisplayList,
    pub font_cache: FontCache,
    pub image_cache: ImageCache,
    /// Device pixels per page-unit (DPI / 72). Pre-computed so benches share one
    /// definition of `scale`.
    pub scale: f32,
    /// Time spent *before* the interpreter starts: loading the page's fonts,
    /// fetching its content bytes, resolving annotations / optional content /
    /// output intents, and building the caches.
    ///
    /// Recorded because `stages/interpret` times this and the interpreter
    /// together, and the two are wildly different sizes depending on the page:
    /// a text page can spend nearly all of it here (re-parsing every font the
    /// page lists), while an image page spends nearly all of it in the
    /// interpreter. Without the split, "interpret" means neither.
    pub setup_ns: u64,
}

/// A parsed document plus the resolved page — the `parse` stage's output and the
/// `interpret` stage's input.
pub struct ParsedPage {
    pub doc: PdfDocument,
    pub page: PdfPage,
    pub page_box: Rect,
    /// Absolute path, kept so a stage can be re-run (and for labels).
    pub path: PathBuf,
    /// 0-based page index.
    pub page_index: usize,
    pub dpi: f32,
}

impl ParsedPage {
    /// `PageSetup` label for criterion, e.g. `test8-p0`.
    pub fn label(&self) -> String {
        format!(
            "{}-p{}",
            crate::corpus::label_for_path(&self.path),
            self.page_index
        )
    }
}

/// Stage 1 — read the file, open the document, resolve the page.
pub fn parse(path: &Path, page_index: usize, dpi: f32) -> Result<ParsedPage, String> {
    let data = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let doc = PdfDocument::open(data).map_err(|e| format!("open {}: {e}", path.display()))?;
    let page = doc
        .page(page_index)
        .map_err(|e| format!("page {page_index} of {}: {e}", path.display()))?;
    let page_box = page.effective_box();
    Ok(ParsedPage {
        doc,
        page,
        page_box,
        path: path.to_path_buf(),
        page_index,
        dpi,
    })
}

/// Consuming backend for which a display list is being interpreted.
///
/// Mirrors `zpdf-cli`: CPU rendering opts into scale-exact covering-rectangle
/// clip elision (using `CpuRenderer::raster_scale`), while GPU rendering
/// retains all clips so `WgpuRenderer` receives the production `--backend wgpu`
/// command stream and enforces its own coordinate limits (`MAX_DEVICE_COORD`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterpretTarget {
    Cpu,
    Gpu,
}

/// Stage 2 — load page fonts, decode content, interpret into a `DisplayList`.
///
/// Mirrors the CLI's CPU interpreter construction exactly (annotations, optional
/// content, output-intent CMYK, and CPU clip-elision scale). An earlier version
/// of these benches omitted `.with_annotations`, which under-measured rendering
/// by ~20x: annotation form fields and note text add many glyph runs. Don't
/// drop these without re-checking against `zpdf-cli --stats`.
pub fn interpret(parsed: &ParsedPage) -> Result<PageSetup, String> {
    interpret_page(&parsed.doc, &parsed.page, parsed.page_box, parsed.dpi).map(|(setup, _)| setup)
}

/// Interpret one page of an already-parsed document for the CPU backend.
///
/// Separate from [`interpret`] so a whole-document run can parse once and then
/// interpret every page (see `benches/longform.rs`) instead of re-parsing the
/// file per page.
pub fn interpret_page(
    doc: &PdfDocument,
    page: &PdfPage,
    page_box: Rect,
    dpi: f32,
) -> Result<(PageSetup, zpdf::InterpretStats), String> {
    interpret_page_for(doc, page, page_box, dpi, InterpretTarget::Cpu)
}

/// Interpret one page of an already-parsed document using the clip-elision
/// policy for `target` (CPU vs GPU, matching `zpdf-cli`).
pub fn interpret_page_for(
    doc: &PdfDocument,
    page: &PdfPage,
    page_box: Rect,
    dpi: f32,
    target: InterpretTarget,
) -> Result<(PageSetup, zpdf::InterpretStats), String> {
    interpret_page_impl(doc, page, page_box, dpi, false, target)
}

/// Interpret one page with the per-category work buckets enabled — the
/// diagnostic path behind `ZPDF_BENCH_DEBUG=1`.
///
/// Kept separate from [`interpret_page`] rather than driven by a parameter of
/// its own, so it is obvious at the call site that the *timed* benchmarks never
/// measure the instrumented path: those clocks sit on every attributed operator.
pub fn interpret_instrumented(
    parsed: &ParsedPage,
) -> Result<(PageSetup, zpdf::InterpretStats), String> {
    interpret_page_impl(
        &parsed.doc,
        &parsed.page,
        parsed.page_box,
        parsed.dpi,
        true,
        InterpretTarget::Cpu,
    )
}

fn interpret_page_impl(
    doc: &PdfDocument,
    page: &PdfPage,
    page_box: Rect,
    dpi: f32,
    work_timing: bool,
    target: InterpretTarget,
) -> Result<(PageSetup, zpdf::InterpretStats), String> {
    let setup_started = std::time::Instant::now();
    let mut font_cache = doc.load_page_fonts(page);
    let content_bytes = doc
        .page_content_bytes(page)
        .map_err(|e| format!("content bytes: {e}"))?;

    let mut image_cache = ImageCache::new();
    let annotations = doc.page_annotations(page);
    let oc_config = doc.oc_config();
    let mut icc_cache = IccCache::new();
    let doc_intents = doc.output_intents();
    let oi_cmyk = zpdf::output_intent_cmyk_profile(
        doc.file(),
        doc.page_output_intents(page),
        &doc_intents,
        &mut icc_cache,
    );

    let scale = dpi / 72.0;
    let clip_elision_scale = match target {
        InterpretTarget::Cpu => {
            #[cfg(feature = "cpu-render")]
            {
                zpdf::cpu::CpuRenderer::new()
                    .with_limits(doc.file().limits())
                    .raster_scale(page_box, scale)
            }
            #[cfg(not(feature = "cpu-render"))]
            {
                None
            }
        }
        InterpretTarget::Gpu => None,
    };

    let mut interpreter = zpdf::ContentInterpreter::new(page_box)
        .with_page_rotation(page.rotate)
        .with_fonts(&mut font_cache)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut image_cache)
        .with_colors(&mut icc_cache)
        .with_annotations(&annotations)
        .with_work_timing(work_timing)
        .with_operand_stack_limit(doc.file().limits().max_operand_stack_depth as usize);
    if let Some(clip_scale) = clip_elision_scale {
        interpreter = interpreter.with_clip_elision_scale(clip_scale);
    }
    if let Some(oc) = &oc_config {
        interpreter = interpreter.with_optional_content(oc);
    }
    if let Some(profile) = oi_cmyk {
        interpreter = interpreter.with_output_intent_cmyk(profile);
    }
    // Everything above is page setup; `InterpretStats::total_ns` starts here.
    let setup_ns = setup_started.elapsed().as_nanos() as u64;
    let (dl, stats) = interpreter.interpret_with_stats(&content_bytes);

    Ok((
        PageSetup {
            dl,
            font_cache,
            image_cache,
            scale: dpi / 72.0,
            setup_ns,
        },
        stats,
    ))
}

/// Parse + interpret for the CPU backend, as one call — what CPU render-only
/// benches need in setup.
pub fn load_page(path: &Path, page_index: usize, dpi: f32) -> Result<PageSetup, String> {
    load_page_for(path, page_index, dpi, InterpretTarget::Cpu)
}

/// Parse + interpret for `target`, using that backend's CLI clip-elision policy.
pub fn load_page_for(
    path: &Path,
    page_index: usize,
    dpi: f32,
    target: InterpretTarget,
) -> Result<PageSetup, String> {
    let parsed = parse(path, page_index, dpi)?;
    interpret_page_for(
        &parsed.doc,
        &parsed.page,
        parsed.page_box,
        parsed.dpi,
        target,
    )
    .map(|(setup, _)| setup)
}

/// A rendered page, backend-agnostic: tight RGBA8, top-left origin,
/// `data.len() == width * height * 4`.
///
/// Both backends' targets convert into this, so the encode stage and the
/// fidelity comparisons do not care which backend produced the pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rgba {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl Rgba {
    /// Pixel count — the throughput unit the benches report in, so pages of
    /// different sizes and DPIs compare.
    pub fn pixels(&self) -> u64 {
        self.width as u64 * self.height as u64
    }
}

/// Raster pixel count for a page rect at a device scale — the throughput unit,
/// usable before any render has happened (ceil-rounded, matching the backends).
pub fn estimate_pixels(rect: Rect, scale: f32) -> u64 {
    let w = ((rect.width() * scale as f64).ceil().max(1.0)) as u64;
    let h = ((rect.height() * scale as f64).ceil().max(1.0)) as u64;
    w * h
}

impl From<zpdf::cpu::RenderedPage> for Rgba {
    fn from(p: zpdf::cpu::RenderedPage) -> Self {
        Self {
            width: p.width,
            height: p.height,
            data: p.data,
        }
    }
}

#[cfg(feature = "gpu-render")]
impl From<zpdf::gpu::GpuTexture> for Rgba {
    fn from(t: zpdf::gpu::GpuTexture) -> Self {
        Self {
            width: t.width,
            height: t.height,
            data: t.data,
        }
    }
}

/// Stage 4 — PNG-encode, using the **same** encoder the CLI's
/// `image::save_buffer` path uses (the `png` codec with default settings).
///
/// Encoded to memory, not to a file: the file write is I/O, and mixing it into a
/// CPU-stage number would make the result depend on the filesystem. `--stats`
/// and this stage together account for a CLI run's CPU cost.
pub fn encode_png(rgba: &Rgba) -> Result<Vec<u8>, String> {
    use image::ImageEncoder;
    let mut out = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut out);
    encoder
        .write_image(
            &rgba.data,
            rgba.width,
            rgba.height,
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|e| format!("png encode: {e}"))?;
    Ok(out)
}

/// Interpret a page *with* its stage timings — the only way to attribute shading
/// cost, which happens here and is invisible to a render backend.
pub fn interpret_with_stats(
    parsed: &ParsedPage,
) -> Result<(PageSetup, zpdf::InterpretStats), String> {
    interpret_page(&parsed.doc, &parsed.page, parsed.page_box, parsed.dpi)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zpdf_display_list::RenderCommand;

    fn single_page_pdf(media_box: &str, content: &[u8]) -> Vec<u8> {
        let objs = [
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [{media_box}] \
                 /Contents 4 0 R /Resources << >> >>"
            )
            .into_bytes(),
            {
                let mut v = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
                v.extend_from_slice(content);
                v.extend_from_slice(b"\nendstream");
                v
            },
        ];
        let mut out = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
        let mut offsets = Vec::with_capacity(objs.len());
        for (i, body) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            out.extend_from_slice(body);
            out.extend_from_slice(b"\nendobj\n");
        }
        let xref = out.len();
        let n = objs.len() + 1;
        out.extend_from_slice(format!("xref\n0 {n}\n0000000000 65535 f \n").as_bytes());
        for off in offsets {
            out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!("trailer\n<< /Size {n} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
        );
        out
    }

    #[test]
    fn gpu_target_retains_clips_that_cpu_target_elides() {
        // On a 16,000 × 100 px raster at 72 DPI (scale 1), CPU's coordinate
        // limit is 16,000 * 64 = 1,024,000 so a covering rect reaching
        // x = 1,010,000 elides for CPU, while wgpu's MAX_DEVICE_COORD is
        // 1,000,000 and the shipping `--backend wgpu` CLI retains the clip.
        let pdf = single_page_pdf(
            "0 0 16000 100",
            b"q 0 0 16000 100 re W n 0 0 1010000 100 re W n 1 0 0 rg 0 0 16000 100 re f Q",
        );
        let doc = PdfDocument::open(pdf).expect("open pdf");
        let page = doc.page(0).expect("page 0");
        let page_box = page.effective_box();

        let (gpu_setup, _) =
            interpret_page_for(&doc, &page, page_box, 72.0, InterpretTarget::Gpu).expect("gpu");
        let gpu_clips = gpu_setup
            .dl
            .commands
            .iter()
            .filter(|c| matches!(c, RenderCommand::PushClip { .. }))
            .count();
        assert_eq!(
            gpu_clips, 2,
            "GPU bench display list must match --backend wgpu and retain all clips"
        );

        #[cfg(feature = "cpu-render")]
        {
            let (cpu_setup, _) =
                interpret_page_for(&doc, &page, page_box, 72.0, InterpretTarget::Cpu).expect("cpu");
            let cpu_clips = cpu_setup
                .dl
                .commands
                .iter()
                .filter(|c| matches!(c, RenderCommand::PushClip { .. }))
                .count();
            assert_eq!(
                cpu_clips, 1,
                "CPU bench display list elides covering nested rectangle"
            );
        }
    }
}
