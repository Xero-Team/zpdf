//! Annotation /AP painting and optional-content (OCG) acceptance tests.
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

/// Render page 0 with annotations + optional content wired like the CLI does.
fn render(pdf: Vec<u8>) -> zpdf::cpu::RenderedPage {
    let doc = PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let content = doc.page_content_bytes(&page).expect("content bytes");
    let mut images = ImageCache::new();
    let annotations = doc.page_annotations(&page);
    let oc = doc.oc_config();
    let mut interp = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images)
        .with_annotations(&annotations);
    if let Some(oc) = &oc {
        interp = interp.with_optional_content(oc);
    }
    let dl = interp.interpret(&content);
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

fn assert_near(c: [u8; 3], want: [u8; 3], what: &str) {
    let ok = c
        .iter()
        .zip(want.iter())
        .all(|(a, b)| (*a as i32 - *b as i32).abs() <= 12);
    assert!(ok, "{what}: got {c:?}, want ≈{want:?}");
}

/// A Square annotation whose /AP /N draws a red square over BBox [0,10]²,
/// mapped onto /Rect [50 50 150 150] — the 12.5.5 scale-to-rect algebra.
#[test]
fn annotation_appearance_paints_into_rect() {
    let ap: &[u8] = b"1 0 0 rg 0 0 10 10 re f";
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << >> /Annots [5 0 R] >>"
            .to_vec(),
        stream_obj("", b"0 1 0 rg 0 0 200 200 re f"), // green page background
        b"<< /Type /Annot /Subtype /Square /Rect [50 50 150 150] /F 4 \
          /AP << /N 6 0 R >> >>"
            .to_vec(),
        stream_obj("/Type /XObject /Subtype /Form /BBox [0 0 10 10]", ap),
    ]);
    let page = render(pdf);

    assert_near(px(&page, 100.0, 100.0), [255, 0, 0], "AP inside /Rect");
    assert_near(px(&page, 60.0, 140.0), [255, 0, 0], "AP fills whole /Rect");
    assert_near(px(&page, 30.0, 100.0), [0, 255, 0], "outside /Rect untouched");
}

/// A hidden (/F 2) annotation paints nothing.
#[test]
fn hidden_annotation_is_skipped() {
    let ap: &[u8] = b"1 0 0 rg 0 0 10 10 re f";
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << >> /Annots [5 0 R] >>"
            .to_vec(),
        stream_obj("", b"0 1 0 rg 0 0 200 200 re f"),
        b"<< /Type /Annot /Subtype /Square /Rect [50 50 150 150] /F 2 \
          /AP << /N 6 0 R >> >>"
            .to_vec(),
        stream_obj("/Type /XObject /Subtype /Form /BBox [0 0 10 10]", ap),
    ]);
    let page = render(pdf);
    assert_near(px(&page, 100.0, 100.0), [0, 255, 0], "hidden annot not painted");
}

/// Two BDC /OC blocks: the OFF layer's red square is suppressed, the ON
/// layer's blue square paints; XObject-level /OC also suppresses.
#[test]
fn ocg_off_layers_are_hidden() {
    let content: &[u8] = b"/OC /L0 BDC 1 0 0 rg 20 20 60 60 re f EMC\n\
        /OC /L1 BDC 0 0 1 rg 120 20 60 60 re f EMC\n\
        /Fm0 Do";
    let form: &[u8] = b"0 0 0 rg 20 120 60 60 re f";
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R /OCProperties << /OCGs [5 0 R 6 0 R] \
          /D << /OFF [5 0 R] >> >> >>"
            .to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /Properties << /L0 5 0 R /L1 6 0 R >> \
          /XObject << /Fm0 7 0 R >> >> >>"
            .to_vec(),
        stream_obj("", content),
        b"<< /Type /OCG /Name (off-layer) >>".to_vec(),
        b"<< /Type /OCG /Name (on-layer) >>".to_vec(),
        stream_obj(
            "/Type /XObject /Subtype /Form /BBox [0 0 200 200] /OC 5 0 R",
            form,
        ),
    ]);
    let page = render(pdf);

    assert_near(px(&page, 50.0, 50.0), [255, 255, 255], "OFF layer hidden");
    assert_near(px(&page, 150.0, 50.0), [0, 0, 255], "ON layer painted");
    assert_near(px(&page, 50.0, 150.0), [255, 255, 255], "XObject /OC hidden");
}

/// /OCMD with /P /AllOn over one ON and one OFF group → hidden; the /VE
/// expression ["Not", off-group] → visible.
#[test]
fn ocmd_policies_and_visibility_expressions() {
    let content: &[u8] = b"/OC /M0 BDC 1 0 0 rg 20 20 60 60 re f EMC\n\
        /OC /M1 BDC 0 0 1 rg 120 20 60 60 re f EMC";
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R /OCProperties << /OCGs [5 0 R 6 0 R] \
          /D << /OFF [5 0 R] >> >> >>"
            .to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /Properties << /M0 7 0 R /M1 8 0 R >> >> >>"
            .to_vec(),
        stream_obj("", content),
        b"<< /Type /OCG /Name (off-group) >>".to_vec(),
        b"<< /Type /OCG /Name (on-group) >>".to_vec(),
        b"<< /Type /OCMD /OCGs [5 0 R 6 0 R] /P /AllOn >>".to_vec(),
        b"<< /Type /OCMD /VE [/Not 5 0 R] >>".to_vec(),
    ]);
    let page = render(pdf);

    assert_near(px(&page, 50.0, 50.0), [255, 255, 255], "AllOn with OFF member");
    assert_near(px(&page, 150.0, 50.0), [0, 0, 255], "VE Not(off) visible");
}
