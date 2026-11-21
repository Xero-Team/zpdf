//! Covering-rectangle clip elision acceptance tests (CPU backend).
//!
//! Verifies pixel-for-pixel equivalence against an unelided display list across
//! five subtle rasterization invariants:
//! 1. An out-of-range earlier clip skipped by `CpuRenderer::finish_within_limits`
//!    does not cause a later narrower rectangle clip to be dropped.
//! 2. Fractional page dimensions and fractional zooms preserve partial coverage
//!    at the right/bottom raster edge.
//! 3. Nested clips sharing an antialiased edge (or two edges within one pixel)
//!    preserve multiplied mask coverage.
//! 4. `sh` shadings inside a tiling pattern cell keep their original sampling
//!    grid.
//! 5. Eliding never removes the only active clip mask, preserving `tiny-skia`'s
//!    masked `SourceOver` paint rounding on opaque antialiased draws.
//! 6. Clips whose device bounds exceed `tiny-skia`'s supersampled `i32` AA
//!    scanline range are kept so scan-converter wraparound cannot diverge.
#![cfg(feature = "cpu-render")]

use zpdf::display_list::RenderCommand;
use zpdf::{ContentInterpreter, ImageCache, PdfDocument, RenderBackend};

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

fn page_pdf(media_box: &str, content: &[u8]) -> Vec<u8> {
    assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [{media_box}] /Contents 4 0 R \
             /Resources << >> >>"
        )
        .into_bytes(),
        stream_obj("", content),
    ])
}

/// Interpret and render `pdf` at `scale`, returning `(push_clip_count, rendered, images)`.
/// When `elide` is false, `clip_elision_scale` stays at its default `0.0` so
/// every clip is explicitly retained as the oracle.
fn render_case(
    pdf: &[u8],
    scale: f32,
    elide: bool,
) -> (usize, zpdf::cpu::RenderedPage, ImageCache) {
    let doc = PdfDocument::open(pdf.to_vec()).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let content = doc.page_content_bytes(&page).expect("content bytes");
    let mut images = ImageCache::new();
    let mut interp = ContentInterpreter::new(page.effective_box())
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images);
    if elide {
        let raster_scale = zpdf::cpu::CpuRenderer::new()
            .raster_scale(page.effective_box(), scale)
            .expect("valid raster scale");
        interp = interp.with_clip_elision_scale(raster_scale);
    }
    let dl = interp.interpret(&content);
    let push_clips = dl
        .commands
        .iter()
        .filter(|c| matches!(c, RenderCommand::PushClip { .. }))
        .count();
    let pop_clips = dl
        .commands
        .iter()
        .filter(|c| matches!(c, RenderCommand::PopClip))
        .count();
    assert_eq!(push_clips, pop_clips, "unbalanced PushClip/PopClip");
    let rendered = zpdf::cpu::CpuRenderer::new()
        .with_images(&images)
        .render_display_list(&dl, scale)
        .expect("cpu render");
    (push_clips, rendered, images)
}

/// RGB at PDF page-space `(x, y)` on a 100×100 pt page rendered at scale 1.
fn px(page: &zpdf::cpu::RenderedPage, x: u32, y: u32) -> [u8; 3] {
    let iy = page.height - 1 - y;
    let off = ((iy * page.width + x) * 4) as usize;
    [page.data[off], page.data[off + 1], page.data[off + 2]]
}

#[test]
fn cpu_preserves_later_rectangle_clip_when_earlier_clip_exceeds_backend_limit() {
    // Clip 1 spans x = 10..2_000_000, y = 10..20. CpuRenderer::finish_within_limits
    // rejects it at scale 1 and leaves the previous (full-page) mask active.
    // Clip 2 restricts painting to the bottom 30 pt (y = 0..30) and must be
    // kept so the full-page red fill cannot paint above y = 30.
    let pdf = page_pdf(
        "0 0 100 100",
        b"q 10 10 1999990 10 re W n 0 0 100 30 re W n 1 0 0 rg 0 0 100 100 re f Q",
    );
    let (elided_clips, elided, _) = render_case(&pdf, 1.0, true);
    let (retained_clips, retained, _) = render_case(&pdf, 1.0, false);

    assert_eq!(retained_clips, 2);
    assert_eq!(
        elided_clips, 2,
        "later clip must be preserved when earlier clip may be skipped by CPU backend"
    );
    assert_eq!(elided.data, retained.data);
    assert_eq!(px(&elided, 50, 15), [255, 0, 0]);
    assert_eq!(px(&elided, 50, 50), [255, 255, 255]);
    assert_eq!(px(&elided, 50, 80), [255, 255, 255]);
}

#[test]
fn cpu_preserves_partial_coverage_at_fractional_page_edge_and_zoom() {
    // 100.5 pt wide page at scale 1 -> 101 columns. Even under an active
    // full-raster outer clip [0, 0, 101, 100], a nested page-sized clip
    // [0, 0, 100.5, 100] partially covers column 100 and must NOT be elided.
    let frac_page_pdf = page_pdf(
        "0 0 100.5 100",
        b"q 0 0 101 100 re W n 0 0 100.5 100 re W n 1 0 0 rg 0 0 120 100 re f Q",
    );
    let (elided_clips, elided, _) = render_case(&frac_page_pdf, 1.0, true);
    let (retained_clips, retained, _) = render_case(&frac_page_pdf, 1.0, false);
    assert_eq!(retained_clips, 2);
    assert_eq!(elided_clips, 2, "fractional page-edge clip must be kept");
    assert_eq!(elided.width, 101);
    assert_eq!(elided.data, retained.data);
    // Column 100 should have partial red over white (not full [255, 0, 0]).
    let edge = px(&elided, 100, 50);
    assert!(
        edge[1] > 100 && edge[1] < 160,
        "expected ~50% coverage on last column, got {edge:?}"
    );

    // A nested clip [0, 0, 101, 100] DOES cover all 101 raster columns of the
    // active outer clip and is elided with bit-identical pixels.
    let covering_pdf = page_pdf(
        "0 0 100.5 100",
        b"q 0 0 101 100 re W n 0 0 101 100 re W n 1 0 0 rg 0 0 120 100 re f Q",
    );
    let (cov_elided_clips, cov_elided, _) = render_case(&covering_pdf, 1.0, true);
    let (cov_retained_clips, cov_retained, _) = render_case(&covering_pdf, 1.0, false);
    assert_eq!(cov_retained_clips, 2);
    assert_eq!(
        cov_elided_clips, 1,
        "nested clip covering full 101px raster elides"
    );
    assert_eq!(cov_elided.data, cov_retained.data);

    // Fractional zoom on an integer 100×100 pt page: at 150 DPI (scale 150/72),
    // 100 * 150/72 = 208.333 -> 209 columns. Under an active full-raster clip,
    // a nested page-sized clip [0, 0, 100, 100] gives partial coverage on
    // column 208 and row 208 and must be kept.
    let zoom = 150.0 / 72.0;
    let zoom_pdf = page_pdf(
        "0 0 100 100",
        b"q -2 -2 104 104 re W n 0 0 100 100 re W n 1 0 0 rg -10 -10 120 120 re f Q",
    );
    let (zoom_elided_clips, zoom_elided, _) = render_case(&zoom_pdf, zoom, true);
    let (zoom_retained_clips, zoom_retained, _) = render_case(&zoom_pdf, zoom, false);
    assert_eq!(zoom_retained_clips, 2);
    assert_eq!(
        zoom_elided_clips, 2,
        "page-sized clip at fractional zoom must be kept"
    );
    assert_eq!(zoom_elided.width, 209);
    assert_eq!(zoom_elided.data, zoom_retained.data);

    // Nested oversized clip covering all 209×209 device pixels at 150 DPI elides.
    let zoom_cover_pdf = page_pdf(
        "0 0 100 100",
        b"q -2 -2 104 104 re W n -2 -2 104 104 re W n 1 0 0 rg -10 -10 120 120 re f Q",
    );
    let (zc_elided_clips, zc_elided, _) = render_case(&zoom_cover_pdf, zoom, true);
    let (zc_retained_clips, zc_retained, _) = render_case(&zoom_cover_pdf, zoom, false);
    assert_eq!(zc_retained_clips, 2);
    assert_eq!(zc_elided_clips, 1);
    assert_eq!(zc_elided.data, zc_retained.data);
}

#[test]
fn cpu_preserves_antialiased_shared_edges_in_nested_clips() {
    // Same fractional left edge (x = 10.5) applied twice at scale 1: multiplying
    // two ~50% masks leaves column 10 at ~25% coverage. Both clips must stay.
    let same_edge_pdf = page_pdf(
        "0 0 100 100",
        b"q 10.5 10.5 50 50 re W n 10.5 10.5 50 50 re W n 0 0 0 rg 0 0 100 100 re f Q",
    );
    let (elided_clips, elided, _) = render_case(&same_edge_pdf, 1.0, true);
    let (retained_clips, retained, _) = render_case(&same_edge_pdf, 1.0, false);
    assert_eq!(retained_clips, 2);
    assert_eq!(elided_clips, 2, "shared fractional edge clip must be kept");
    assert_eq!(elided.data, retained.data);
    // ~25% black over white => gray ~191 (50% would be ~128).
    let col10 = px(&elided, 10, 35);
    assert!(
        col10[0] > 175 && col10[0] < 210,
        "expected ~25% multiplied coverage on column 10, got {col10:?}"
    );

    // Two distinct edges within pixel column 10 (10.5 and 10.2): both must stay.
    let distinct_same_px_pdf = page_pdf(
        "0 0 100 100",
        b"q 10.5 10.5 50 50 re W n 10.2 10.0 51.0 51.0 re W n 0 0 0 rg 0 0 100 100 re f Q",
    );
    let (d_elided_clips, d_elided, _) = render_case(&distinct_same_px_pdf, 1.0, true);
    let (d_retained_clips, d_retained, _) = render_case(&distinct_same_px_pdf, 1.0, false);
    assert_eq!(d_retained_clips, 2);
    assert_eq!(
        d_elided_clips, 2,
        "distinct edge inside affected pixel column must be kept"
    );
    assert_eq!(d_elided.data, d_retained.data);

    // Second rectangle [10.0, 10.0, 61.0, 61.0] fully covers every affected
    // device pixel [10..61) × [39..90) and elides with byte-identical output.
    let full_px_cover_pdf = page_pdf(
        "0 0 100 100",
        b"q 10.5 10.5 50 50 re W n 10.0 10.0 51.0 51.0 re W n 0 0 0 rg 0 0 100 100 re f Q",
    );
    let (c_elided_clips, c_elided, _) = render_case(&full_px_cover_pdf, 1.0, true);
    let (c_retained_clips, c_retained, _) = render_case(&full_px_cover_pdf, 1.0, false);
    assert_eq!(c_retained_clips, 2);
    assert_eq!(c_elided_clips, 1, "full affected-pixel cover should elide");
    assert_eq!(c_elided.data, c_retained.data);
}

#[test]
fn cpu_preserves_shading_sampling_grid_inside_tiling_cell() {
    // 600×600 pt unclipped page painting an interior 100×100 pt tiling pattern
    // cell whose content draws a sharp axial gradient via `/Sh0 sh`.
    // Clip elision must NOT tighten `current.clip_bounds` (which drives
    // `shading_region()`), so the shading keeps its original 768×768 sampling
    // grid and pixel output matches the unelided display list byte-for-byte.
    let cell = b"/Sh0 sh";
    let content = b"/Pattern cs /P0 scn 100 100 100 100 re f";
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 600] /Contents 4 0 R \
          /Resources << /Pattern << /P0 5 0 R >> >> >>"
            .to_vec(),
        stream_obj("", content),
        stream_obj(
            "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
             /BBox [0 0 100 100] /XStep 100 /YStep 100 \
             /Resources << /Shading << /Sh0 6 0 R >> >>",
            cell,
        ),
        // Sharp transition across x = 49.5..50.5 inside the cell so any
        // sampling-grid change shifts boundary pixels immediately.
        b"<< /ShadingType 2 /ColorSpace /DeviceRGB /Coords [49.5 0 50.5 0] \
          /Function 7 0 R /Extend [true true] >>"
            .to_vec(),
        b"<< /FunctionType 2 /Domain [0 1] /C0 [1 0 0] /C1 [0 0 1] /N 1 >>".to_vec(),
    ]);

    let (_elided_clips, elided, elided_images) = render_case(&pdf, 1.0, true);
    let (_retained_clips, retained, retained_images) = render_case(&pdf, 1.0, false);

    assert_eq!(elided.data, retained.data);
    let elided_sh = elided_images.get(0).expect("shading image");
    let retained_sh = retained_images.get(0).expect("retained shading image");
    assert_eq!(
        (elided_sh.width, elided_sh.height),
        (retained_sh.width, retained_sh.height),
        "shading inside tiling cell must preserve its original sample grid"
    );
    assert_eq!((elided_sh.width, elided_sh.height), (768, 768));
}

#[test]
fn cpu_preserves_masked_paint_rounding_when_only_clip_is_page_sized() {
    // In tiny-skia, RasterPipelineBlitter strength-reduces opaque SourceOver to
    // Source when mask.is_none() (rounding the lerp once -> 127), whereas with
    // even a full-coverage mask it stays on SourceOver (rounding source and
    // destination separately -> 128). Keeping the only mask preserves exact
    // byte-for-byte output against the retained clip.
    let single_clip_pdf = page_pdf(
        "0 0 100 100",
        b"0.5 g 0 0 100 100 re f q 0 0 100 100 re W n 10.5 10 20 20 re f Q",
    );
    let (elided_clips, elided, _) = render_case(&single_clip_pdf, 1.0, true);
    let (retained_clips, retained, _) = render_case(&single_clip_pdf, 1.0, false);

    assert_eq!(retained_clips, 1);
    assert_eq!(
        elided_clips, 1,
        "only mask must be retained to preserve masked SourceOver rounding"
    );
    assert_eq!(elided.data, retained.data);
    assert_eq!(px(&elided, 10, 20), [128, 128, 128]);

    // Contrast: without any clip, tiny-skia uses Source lerp and produces 127.
    let unclipped_pdf = page_pdf(
        "0 0 100 100",
        b"0.5 g 0 0 100 100 re f q 10.5 10 20 20 re f Q",
    );
    let (unclipped_clips, unclipped, _) = render_case(&unclipped_pdf, 1.0, false);
    assert_eq!(unclipped_clips, 0);
    assert_eq!(px(&unclipped, 10, 20), [127, 127, 127]);

    // Once a clip mask is already installed, a nested full-coverage rectangle
    // clip IS elided while keeping the exact same masked SourceOver output.
    let nested_clip_pdf = page_pdf(
        "0 0 100 100",
        b"0.5 g 0 0 100 100 re f q 0 0 100 100 re W n 0 0 100 100 re W n 10.5 10 20 20 re f Q",
    );
    let (nested_elided_clips, nested_elided, _) = render_case(&nested_clip_pdf, 1.0, true);
    let (nested_retained_clips, nested_retained, _) = render_case(&nested_clip_pdf, 1.0, false);

    assert_eq!(nested_retained_clips, 2);
    assert_eq!(
        nested_elided_clips, 1,
        "nested covering rectangle under an active mask must elide"
    );
    assert_eq!(nested_elided.data, nested_retained.data);
    assert_eq!(px(&nested_elided, 10, 20), [128, 128, 128]);
}

#[test]
fn cpu_keeps_clip_exceeding_tiny_skia_supersampled_scanline_range() {
    // On an 8,388,608 × 1 raster at scale 1, CpuRenderer::finish_within_limits
    // allows coordinates up to 536,870,912 (1 << 29). A clip with device bounds
    // [0, 0, 2, 536_870_912] overflows tiny-skia 0.12's `stop_y <<= 2` in i32
    // before tile clamping, leaving the clip mask empty. Eliding it would make
    // the red fill paint pixels (0, 0) and (1, 0) instead of keeping them white.
    let overflow_pdf = page_pdf(
        "0 0 8388608 1",
        b"q 0 0 2 1 re W n 0 -536870912 2 536870913 re W n 1 0 0 rg 0 0 2 1 re f Q",
    );
    let (elided_clips, elided, _) = render_case(&overflow_pdf, 1.0, true);
    let (retained_clips, retained, _) = render_case(&overflow_pdf, 1.0, false);

    assert_eq!(retained_clips, 2);
    assert_eq!(
        elided_clips, 2,
        "clip overflowing tiny-skia supersampled scanline range must be kept"
    );
    assert_eq!((elided.width, elided.height), (8_388_608, 1));
    assert!(
        elided.data == retained.data,
        "elided render must match retained clip byte-for-byte"
    );
    // tiny-skia's wrapped scanline bound leaves the mask empty -> pixels stay white.
    assert_eq!(px(&elided, 0, 0), [255, 255, 255]);
    assert_eq!(px(&elided, 1, 0), [255, 255, 255]);

    // Contrast: within tiny-skia's safe ±2^28 AA range on the same wide raster,
    // the covering clip IS elided and both pixels match the retained red fill.
    let safe_pdf = page_pdf(
        "0 0 8388608 1",
        b"q 0 0 2 1 re W n 0 -268435000 2 268435001 re W n 1 0 0 rg 0 0 2 1 re f Q",
    );
    let (safe_elided_clips, safe_elided, _) = render_case(&safe_pdf, 1.0, true);
    let (safe_retained_clips, safe_retained, _) = render_case(&safe_pdf, 1.0, false);

    assert_eq!(safe_retained_clips, 2);
    assert_eq!(
        safe_elided_clips, 1,
        "covering clip inside safe supersampled AA range should elide"
    );
    assert!(
        safe_elided.data == safe_retained.data,
        "elided safe-range render must match retained clip byte-for-byte"
    );
    assert_eq!(px(&safe_elided, 0, 0), [255, 0, 0]);
    assert_eq!(px(&safe_elided, 1, 0), [255, 0, 0]);
}
