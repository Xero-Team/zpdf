//! End-to-end tests for PDF/X & PDF 2.0 output intents: a document (or page)
//! that embeds a CMYK `/DestOutputProfile` colour-manages DeviceCMYK through it
//! instead of the Adobe SWOP polynomial. Each builds a tiny PDF whose page is a
//! solid CMYK black fill, renders page 0 through the CPU backend with the same
//! output-intent wiring the CLI uses, and compares the centre pixel across
//! scenarios. The CMYK and sRGB ICC fixtures live in `zpdf-color/src/testdata`;
//! tests skip gracefully when they are absent in a given checkout.
#![cfg(feature = "cpu-render")]

use zpdf::{ContentInterpreter, ImageCache, PdfDocument, RenderBackend};

const SCALE: f32 = 2.0;

fn cmyk_profile() -> Option<Vec<u8>> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../zpdf-color/src/testdata/cmyk_lut.icc"
    );
    std::fs::read(path).ok()
}

fn rgb_profile() -> Option<Vec<u8>> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../zpdf-color/src/testdata/srgb.icc"
    );
    std::fs::read(path).ok()
}

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

/// An `/OutputIntent` dict whose `/DestOutputProfile` is the next object.
fn output_intent(profile_obj: u32) -> Vec<u8> {
    format!(
        "<< /Type /OutputIntent /S /GTS_PDFX /OutputConditionIdentifier (Test) \
         /DestOutputProfile {profile_obj} 0 R >>"
    )
    .into_bytes()
}

/// An embedded ICC profile stream with a declared `/N`.
fn icc_stream(n: u8, bytes: &[u8]) -> Vec<u8> {
    stream_obj(&format!("/N {n}"), bytes)
}

/// Page content: fill the whole page with DeviceCMYK black (K colorant only).
const CMYK_BLACK_FILL: &[u8] = b"0 0 0 1 k 0 0 100 100 re f";

/// Render page 0 with the CLI's output-intent wiring and return the page.
fn render(pdf: Vec<u8>) -> zpdf::cpu::RenderedPage {
    let doc = PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let content = doc.page_content_bytes(&page).expect("content bytes");
    let mut fonts = doc.load_page_fonts(&page);
    let mut images = ImageCache::new();
    let mut colors = zpdf::IccCache::new();
    let doc_intents = doc.output_intents();
    let oi = zpdf::output_intent_cmyk_profile(
        doc.file(),
        doc.page_output_intents(&page),
        &doc_intents,
        &mut colors,
    );
    let mut interp = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images)
        .with_colors(&mut colors);
    if let Some(profile) = oi {
        interp = interp.with_output_intent_cmyk(profile);
    }
    let dl = interp.interpret(&content);
    zpdf::cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, SCALE)
        .expect("cpu render")
}

/// RGB at the centre of the 100×100 page rendered at SCALE.
fn centre(page: &zpdf::cpu::RenderedPage) -> [u8; 3] {
    let ix = (50.0 * SCALE) as u32;
    let iy = (50.0 * SCALE) as u32;
    let off = ((iy * page.width + ix) * 4) as usize;
    [page.data[off], page.data[off + 1], page.data[off + 2]]
}

fn differs(a: [u8; 3], b: [u8; 3], by: i32) -> bool {
    a.iter()
        .zip(b.iter())
        .any(|(x, y)| (*x as i32 - *y as i32).abs() > by)
}

fn no_oi_pdf() -> Vec<u8> {
    assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R /Resources << >> >>"
            .to_vec(),
        stream_obj("", CMYK_BLACK_FILL),
    ])
}

/// The baseline CMYK-black colour without any output intent (SWOP polynomial).
/// Used as the reference every scenario is compared against.
fn swop_centre() -> [u8; 3] {
    centre(&render(no_oi_pdf()))
}

#[test]
fn document_cmyk_output_intent_colour_manages() {
    let Some(cmyk) = cmyk_profile() else { return };
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R /OutputIntents [5 0 R] >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R /Resources << >> >>"
            .to_vec(),
        stream_obj("", CMYK_BLACK_FILL),
        output_intent(6),
        icc_stream(4, &cmyk),
    ]);
    let managed = centre(&render(pdf));
    assert!(
        differs(managed, swop_centre(), 8),
        "document CMYK output intent must change the rendered colour: managed {managed:?} vs SWOP {:?}",
        swop_centre()
    );
}

#[test]
fn page_level_cmyk_output_intent_colour_manages() {
    let Some(cmyk) = cmyk_profile() else { return };
    // PDF 2.0: the intent is on the page dict, not the catalog.
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R \
          /Resources << >> /OutputIntents [5 0 R] >>"
            .to_vec(),
        stream_obj("", CMYK_BLACK_FILL),
        output_intent(6),
        icc_stream(4, &cmyk),
    ]);
    let managed = centre(&render(pdf));
    assert!(
        differs(managed, swop_centre(), 8),
        "page-level CMYK output intent must change the rendered colour: {managed:?}"
    );
}

#[test]
fn rgb_output_intent_is_ignored() {
    let Some(rgb) = rgb_profile() else { return };
    // A 3-channel (RGB) DestOutputProfile does not characterize DeviceCMYK, so
    // the renderer must keep the SWOP polynomial — byte-identical to no-OI.
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R /OutputIntents [5 0 R] >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R /Resources << >> >>"
            .to_vec(),
        stream_obj("", CMYK_BLACK_FILL),
        output_intent(6),
        icc_stream(3, &rgb),
    ]);
    assert_eq!(
        centre(&render(pdf)),
        swop_centre(),
        "an RGB output intent must not colour-manage DeviceCMYK"
    );
}

#[test]
fn page_level_intent_overrides_document_level() {
    let (Some(cmyk), Some(rgb)) = (cmyk_profile(), rgb_profile()) else {
        return;
    };
    // Catalog declares an RGB intent (a no-op for CMYK); the page declares a
    // CMYK intent. The page-level intent must win, so the fill is colour-managed.
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R /OutputIntents [5 0 R] >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R \
          /Resources << >> /OutputIntents [7 0 R] >>"
            .to_vec(),
        stream_obj("", CMYK_BLACK_FILL),
        output_intent(6), // catalog intent → RGB profile (obj 6)
        icc_stream(3, &rgb),
        output_intent(8), // page intent → CMYK profile (obj 8)
        icc_stream(4, &cmyk),
    ]);
    assert!(
        differs(centre(&render(pdf)), swop_centre(), 8),
        "page-level CMYK intent must override the document-level RGB intent"
    );
}

#[test]
fn non_cmyk_page_intent_falls_back_to_document_cmyk() {
    let (Some(cmyk), Some(rgb)) = (cmyk_profile(), rgb_profile()) else {
        return;
    };
    // The page declares only an RGB intent (which does not characterize CMYK);
    // the document declares a CMYK intent. The page intent must not suppress the
    // governing document CMYK intent, so the fill is still colour-managed.
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R /OutputIntents [7 0 R] >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R \
          /Resources << >> /OutputIntents [5 0 R] >>"
            .to_vec(),
        stream_obj("", CMYK_BLACK_FILL),
        output_intent(6), // page intent → RGB profile (obj 6)
        icc_stream(3, &rgb),
        output_intent(8), // document intent → CMYK profile (obj 8)
        icc_stream(4, &cmyk),
    ]);
    assert!(
        differs(centre(&render(pdf)), swop_centre(), 8),
        "a non-CMYK page intent must fall back to the document-level CMYK intent"
    );
}
