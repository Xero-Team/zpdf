//! ExtGState /SMask and transparency-group acceptance tests (CPU backend):
//! luminosity masks, /TR transfer inversion, and group-level constant alpha.
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

fn render(pdf: Vec<u8>) -> zpdf::cpu::RenderedPage {
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

fn assert_near(c: [u8; 3], want: [u8; 3], tol: u8, what: &str) {
    let ok = c
        .iter()
        .zip(want.iter())
        .all(|(a, b)| (*a as i32 - *b as i32).abs() <= tol as i32);
    assert!(ok, "{what}: got {c:?}, want ≈{want:?} (±{tol})");
}

/// Luminosity /SMask PDF: red page, then a masked blue page-fill. The mask
/// group paints white over the left half (luma 1 = visible); the right half
/// falls to the /BC-default black backdrop (luma 0 = hidden).
/// `tr` optionally injects a /TR entry into the SMask dict.
fn smask_pdf(tr: &str) -> Vec<u8> {
    let mask_group: &[u8] = b"1 1 1 rg 0 0 100 200 re f";
    let content: &[u8] = b"1 0 0 rg 0 0 200 200 re f\n/GS0 gs\n0 0 1 rg 0 0 200 200 re f";
    assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /ExtGState << /GS0 5 0 R >> >> >>"
            .to_vec(),
        stream_obj("", content),
        format!("<< /Type /ExtGState /SMask << /Type /Mask /S /Luminosity /G 6 0 R {tr} >> >>")
            .into_bytes(),
        stream_obj(
            "/Type /XObject /Subtype /Form /BBox [0 0 200 200] \
             /Group << /S /Transparency /CS /DeviceRGB >>",
            mask_group,
        ),
    ])
}

#[test]
fn luminosity_smask_hides_masked_area() {
    let page = render(smask_pdf(""));
    // Left half: mask luma 1 → the blue fill shows.
    assert_near(px(&page, 50.0, 100.0), [0, 0, 255], 12, "unmasked left");
    // Right half: backdrop luma 0 → blue hidden, red base shows.
    assert_near(px(&page, 150.0, 100.0), [255, 0, 0], 12, "masked right");
}

#[test]
fn smask_transfer_function_inverts() {
    // /TR maps luma v → 1-v (FunctionType 2, C0=1, C1=0), flipping the halves.
    let page = render(smask_pdf(
        "/TR << /FunctionType 2 /Domain [0 1] /C0 [1] /C1 [0] /N 1 >>",
    ));
    assert_near(px(&page, 50.0, 100.0), [255, 0, 0], 12, "left now masked");
    assert_near(
        px(&page, 150.0, 100.0),
        [0, 0, 255],
        12,
        "right now visible",
    );
}

/// Group-level constant alpha: a transparency-group form painted with
/// /ca 0.5 composites as a unit — overlapping fills inside it do not stack.
#[test]
fn transparency_group_composites_with_group_alpha() {
    let form: &[u8] = b"0 0 0 rg 20 20 100 100 re f\n60 60 100 100 re f";
    let content: &[u8] = b"/GS0 gs /Fm0 Do";
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /ExtGState << /GS0 5 0 R >> /XObject << /Fm0 6 0 R >> >> >>"
            .to_vec(),
        stream_obj("", content),
        b"<< /Type /ExtGState /ca 0.5 >>".to_vec(),
        stream_obj(
            "/Type /XObject /Subtype /Form /BBox [0 0 200 200] \
             /Group << /S /Transparency /CS /DeviceRGB /I true >>",
            form,
        ),
    ]);
    let page = render(pdf);

    // Non-overlapping part of a square: 50% black over white ≈ 127 gray.
    assert_near(px(&page, 40.0, 40.0), [127, 127, 127], 14, "single square");
    // Overlap region: composited once as a group — still ≈ 127, NOT ≈ 64.
    assert_near(px(&page, 80.0, 80.0), [127, 127, 127], 14, "overlap region");
    // Outside: untouched white.
    assert_near(px(&page, 180.0, 180.0), [255, 255, 255], 6, "background");
}

/// Knockout group (/K true): two overlapping 50%-alpha black squares. Each
/// element composites against the group's (transparent) initial backdrop, so
/// the overlap shows a single 50% black (≈127 gray), NOT the stacked 75%-black
/// (≈64) a non-knockout group would accumulate.
#[test]
fn knockout_group_elements_do_not_accumulate() {
    // Inside the group, GS1 sets fill alpha 0.5 for both squares.
    let form: &[u8] = b"/GS1 gs 0 0 0 rg 20 20 100 100 re f 60 60 100 100 re f";
    let content: &[u8] = b"/Fm0 Do";
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /XObject << /Fm0 5 0 R >> >> >>"
            .to_vec(),
        stream_obj("", content),
        stream_obj(
            "/Type /XObject /Subtype /Form /BBox [0 0 200 200] \
             /Group << /S /Transparency /CS /DeviceRGB /I true /K true >> \
             /Resources << /ExtGState << /GS1 6 0 R >> >>",
            form,
        ),
        b"<< /Type /ExtGState /ca 0.5 >>".to_vec(),
    ]);
    let page = render(pdf);

    // Square 1 only (40,40): 50% black over white ≈ 127.
    assert_near(px(&page, 40.0, 40.0), [127, 127, 127], 16, "square 1 only");
    // Square 2 only (140,140): also ≈ 127.
    assert_near(
        px(&page, 140.0, 140.0),
        [127, 127, 127],
        16,
        "square 2 only",
    );
    // Overlap (80,80): knockout → still ≈ 127, NOT ≈ 64 (stacked).
    assert_near(
        px(&page, 80.0, 80.0),
        [127, 127, 127],
        16,
        "knockout overlap",
    );
    // Background untouched.
    assert_near(px(&page, 185.0, 185.0), [255, 255, 255], 6, "background");
}

/// Non-isolated group (/I false): a Multiply fill inside the group sees the
/// page backdrop through the group, so gray × red = dark red. An isolated group
/// would multiply against transparent and show plain gray instead.
#[test]
fn non_isolated_group_blends_with_backdrop() {
    // Group form: a Multiply (GS0) 50% gray fill over the same area.
    let form: &[u8] = b"/GS0 gs 0.5 0.5 0.5 rg 40 40 120 120 re f";
    // Page: red backdrop, then the non-isolated group on top.
    let content: &[u8] = b"1 0 0 rg 40 40 120 120 re f /Fm0 Do";
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /XObject << /Fm0 5 0 R >> >> >>"
            .to_vec(),
        stream_obj("", content),
        stream_obj(
            "/Type /XObject /Subtype /Form /BBox [0 0 200 200] \
             /Group << /S /Transparency /CS /DeviceRGB /I false >> \
             /Resources << /ExtGState << /GS0 6 0 R >> >>",
            form,
        ),
        b"<< /Type /ExtGState /BM /Multiply >>".to_vec(),
    ]);
    let page = render(pdf);

    // Center: gray × red = dark red (R kept, G/B knocked to ~0).
    let c = px(&page, 100.0, 100.0);
    assert!(
        c[0] > 100 && c[1] < 60 && c[2] < 60,
        "non-isolated multiply should be dark red, got {c:?}"
    );
}
