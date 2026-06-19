//! AcroForm field-appearance generation acceptance tests: a text-field widget
//! with a value but no `/AP` must synthesize an appearance that renders the
//! value through the normal annotation-painting path.
#![cfg(feature = "cpu-render")]

use zpdf::{ContentInterpreter, ImageCache, PdfDocument, RenderBackend};

const SCALE: f32 = 2.0;

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

struct Rendered {
    page: zpdf::cpu::RenderedPage,
    glyph_runs: usize,
    has_generated_widget: bool,
}

/// Open, generate appearances, interpret, and render page 0 like the CLI does.
fn render(pdf: Vec<u8>) -> Rendered {
    let doc = PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let content = doc.page_content_bytes(&page).expect("content bytes");
    let mut images = ImageCache::new();
    let annotations = doc.page_annotations(&page);
    let has_generated_widget = annotations
        .iter()
        .any(|a| a.subtype == "Widget" && a.generated.is_some());

    let dl = {
        let interp = ContentInterpreter::new(page.media_box)
            .with_fonts(&mut fonts)
            .with_document(doc.file(), &page.resources)
            .with_images(&mut images)
            .with_annotations(&annotations);
        interp.interpret(&content)
    };
    let glyph_runs = dl
        .commands
        .iter()
        .filter(|c| matches!(c, zpdf::display_list::RenderCommand::DrawGlyphRun(_)))
        .count();

    let page = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .with_images(&images)
        .render_display_list(&dl, SCALE)
        .expect("cpu render");

    Rendered {
        page,
        glyph_runs,
        has_generated_widget,
    }
}

/// Count near-black pixels inside a page-space rectangle (page is 200pt tall).
fn dark_pixels(page: &zpdf::cpu::RenderedPage, x0: f64, y0: f64, x1: f64, y1: f64) -> usize {
    let s = SCALE as f64;
    let mut count = 0;
    let px0 = (x0 * s) as u32;
    let px1 = (x1 * s) as u32;
    // Page y grows upward; image y grows downward.
    let py0 = ((200.0 - y1) * s) as u32;
    let py1 = ((200.0 - y0) * s) as u32;
    for iy in py0..py1.min(page.height) {
        for ix in px0..px1.min(page.width) {
            let off = ((iy * page.width + ix) * 4) as usize;
            let (r, g, b) = (page.data[off], page.data[off + 1], page.data[off + 2]);
            let luma = (r as u32 * 30 + g as u32 * 59 + b as u32 * 11) / 100;
            if luma < 128 {
                count += 1;
            }
        }
    }
    count
}

const PAGE_BG: &[u8] = b"1 1 1 rg 0 0 200 200 re f"; // white background

/// A text field with a value and no /AP, using the AcroForm /DR Helvetica font:
/// the generated appearance must render the value as glyphs inside /Rect.
#[test]
fn text_field_value_renders_via_dr_font() {
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R /AcroForm 5 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << >> /Annots [6 0 R] >>"
            .to_vec(),
        stream_obj("", PAGE_BG),
        b"<< /Fields [6 0 R] /DA (/Helv 0 Tf 0 g) /DR << /Font << /Helv 7 0 R >> >> >>".to_vec(),
        b"<< /Type /Annot /Subtype /Widget /FT /Tx /T (greeting) /V (HELLO) \
          /Rect [20 80 180 120] /F 4 >>"
            .to_vec(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
            .to_vec(),
    ]);
    let r = render(pdf);

    assert!(
        r.has_generated_widget,
        "widget gained a generated appearance"
    );
    assert!(r.glyph_runs >= 1, "generated appearance emits glyph runs");
    // Text appears inside the field…
    let inside = dark_pixels(&r.page, 20.0, 80.0, 180.0, 120.0);
    assert!(
        inside > 40,
        "value renders inside the field (dark px = {inside})"
    );
    // …and the rest of the white page stays clean.
    let outside = dark_pixels(&r.page, 20.0, 140.0, 180.0, 190.0);
    assert_eq!(outside, 0, "area above the field stays white");
}

/// Same, but the AcroForm carries no /DR: the generator must fall back to a
/// synthesized standard Helvetica (exercising the inline-font load path).
#[test]
fn text_field_value_renders_with_fallback_font() {
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R /AcroForm 5 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << >> /Annots [6 0 R] >>"
            .to_vec(),
        stream_obj("", PAGE_BG),
        b"<< /Fields [6 0 R] /DA (/Helv 0 Tf 0 g) >>".to_vec(),
        b"<< /Type /Annot /Subtype /Widget /FT /Tx /T (greeting) /V (WORLD) \
          /Rect [20 80 180 120] /F 4 >>"
            .to_vec(),
    ]);
    let r = render(pdf);

    assert!(
        r.has_generated_widget,
        "widget gained a generated appearance"
    );
    assert!(
        r.glyph_runs >= 1,
        "fallback-font appearance emits glyph runs"
    );
    let inside = dark_pixels(&r.page, 20.0, 80.0, 180.0, 120.0);
    assert!(
        inside > 40,
        "value renders with fallback font (dark px = {inside})"
    );
}

/// A field with an existing /AP and no /NeedAppearances must not be regenerated
/// (the producer appearance wins): the red /AP square paints, not synthesized
/// text.
#[test]
fn existing_appearance_is_not_overridden() {
    let ap: &[u8] = b"1 0 0 rg 0 0 10 10 re f";
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R /AcroForm 5 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << >> /Annots [6 0 R] >>"
            .to_vec(),
        stream_obj("", PAGE_BG),
        b"<< /Fields [6 0 R] /DA (/Helv 0 Tf 0 g) >>".to_vec(),
        b"<< /Type /Annot /Subtype /Widget /FT /Tx /T (greeting) /V (HELLO) \
          /Rect [50 50 150 150] /F 4 /AP << /N 7 0 R >> >>"
            .to_vec(),
        stream_obj("/Type /XObject /Subtype /Form /BBox [0 0 10 10]", ap),
    ]);
    let r = render(pdf);

    assert!(
        !r.has_generated_widget,
        "existing /AP is kept (no regeneration)"
    );
    // The red /AP square fills the /Rect; sample its center.
    let s = SCALE as f64;
    let ix = (100.0 * s) as u32;
    let iy = ((200.0 - 100.0) * s) as u32;
    let off = ((iy * r.page.width + ix) * 4) as usize;
    let (red, green, blue) = (r.page.data[off], r.page.data[off + 1], r.page.data[off + 2]);
    assert!(
        red > 200 && green < 80 && blue < 80,
        "producer /AP paints (got [{red},{green},{blue}])"
    );
}
