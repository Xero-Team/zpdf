//! Overprint (PDF 8.6.7) acceptance tests on the CPU backend.
//!
//! Overprint is composited in naïve subtractive CMYK: a painting operation
//! paints only the colorants its source colour names and leaves the rest of the
//! backdrop untouched. The hallmark is that overprinting one process colour
//! onto another *mixes* them (cyan over yellow → green) instead of knocking the
//! backdrop out (cyan over yellow → cyan).
#![cfg(feature = "cpu-render")]

use zpdf::{ContentInterpreter, ImageCache, PdfDocument, RenderBackend};

const SCALE: f32 = 1.0;

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

fn px(page: &zpdf::cpu::RenderedPage, x: f64, y: f64) -> [u8; 3] {
    let ix = (x * SCALE as f64) as u32;
    let iy = ((200.0 - y) * SCALE as f64) as u32;
    let off = ((iy * page.width + ix) * 4) as usize;
    [page.data[off], page.data[off + 1], page.data[off + 2]]
}

/// A 200×200 page: `content` is the content stream, `gs0` the body of the single
/// ExtGState `/GS0`. A Separation colour space `/CS0` (alternate DeviceCMYK,
/// tint 1 → magenta) is always available for the spot-colour test.
fn page_pdf(content: &[u8], gs0: &str) -> Vec<u8> {
    assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /ExtGState << /GS0 5 0 R >> /ColorSpace << /CS0 6 0 R >> >> >>"
            .to_vec(),
        stream_obj("", content),
        format!("<< /Type /ExtGState {gs0} >>").into_bytes(),
        b"[ /Separation /MySpot /DeviceCMYK 7 0 R ]".to_vec(),
        b"<< /FunctionType 2 /Domain [0 1] /C0 [0 0 0 0] /C1 [0 1 0 0] /N 1 >>".to_vec(),
    ])
}

fn assert_near(c: [u8; 3], want: [u8; 3], tol: u8, what: &str) {
    let ok = c
        .iter()
        .zip(want.iter())
        .all(|(a, b)| (*a as i32 - *b as i32).abs() <= tol as i32);
    assert!(ok, "{what}: got {c:?}, want ≈{want:?} (±{tol})");
}

/// The hallmark: DeviceCMYK cyan (1,0,0,0) overprinting an RGB-yellow backdrop
/// paints only the cyan colorant and keeps yellow → green. A knockout fill
/// would replace the backdrop with cyan.
#[test]
fn cyan_over_yellow_is_green() {
    let content = b"1 1 0 rg 0 0 200 200 re f\n\
                    /GS0 gs 1 0 0 0 k 0 0 200 200 re f";
    let page = render(page_pdf(content, "/OP true /op true /OPM 1"));
    assert_near(
        px(&page, 100.0, 100.0),
        [0, 255, 0],
        28,
        "cyan+yellow=green",
    );
}

/// DeviceCMYK magenta (0,1,0,0) overprinting an RGB-cyan backdrop → blue.
#[test]
fn magenta_over_cyan_is_blue() {
    let content = b"0 1 1 rg 0 0 200 200 re f\n\
                    /GS0 gs 0 1 0 0 k 0 0 200 200 re f";
    let page = render(page_pdf(content, "/OP true /op true /OPM 1"));
    assert_near(
        px(&page, 100.0, 100.0),
        [0, 0, 255],
        28,
        "magenta+cyan=blue",
    );
}

/// Without overprint, the same cyan fill knocks the yellow out → SWOP cyan
/// (a high-blue colour), NOT green. Guards that overprint is off by default.
#[test]
fn no_overprint_knocks_out() {
    let content = b"1 1 0 rg 0 0 200 200 re f\n\
                    1 0 0 0 k 0 0 200 200 re f";
    let page = render(page_pdf(content, ""));
    let c = px(&page, 100.0, 100.0);
    assert!(
        c[2] > 180 && c[0] < 40,
        "no-overprint cyan should be high-blue (knockout), got {c:?}"
    );
}

/// /OPM 0 on a DeviceCMYK source is a no-op (all four process colorants are
/// painted = knockout): cyan over yellow stays cyan, not green.
#[test]
fn opm_zero_is_knockout() {
    let content = b"1 1 0 rg 0 0 200 200 re f\n\
                    /GS0 gs 1 0 0 0 k 0 0 200 200 re f";
    let page = render(page_pdf(content, "/OP true /op true /OPM 0"));
    let c = px(&page, 100.0, 100.0);
    assert!(
        c[2] > 180 && c[0] < 40,
        "OPM 0 cyan should knock out to high-blue cyan, got {c:?}"
    );
}

/// A Separation spot colour (tint 1 → magenta via DeviceCMYK alternate)
/// overprinting an RGB-cyan backdrop → blue, exercising the tint-transform
/// colorant projection. Spot colours overprint under the nonzero rule
/// regardless of /OPM (here OPM is absent → default 0).
#[test]
fn separation_spot_overprints() {
    let content = b"0 1 1 rg 0 0 200 200 re f\n\
                    /GS0 gs /CS0 cs 1 scn 0 0 200 200 re f";
    let page = render(page_pdf(content, "/OP true /op true"));
    assert_near(
        px(&page, 100.0, 100.0),
        [0, 0, 255],
        28,
        "spot magenta+cyan=blue",
    );
}

/// A real-valued `/OPM 1.0` (not just integer `1`) must select nonzero
/// overprint: cyan over yellow → green, same as `/OPM 1`.
#[test]
fn opm_real_value_selects_nonzero() {
    let content = b"1 1 0 rg 0 0 200 200 re f\n\
                    /GS0 gs 1 0 0 0 k 0 0 200 200 re f";
    let page = render(page_pdf(content, "/OP true /op true /OPM 1.0"));
    assert_near(px(&page, 100.0, 100.0), [0, 255, 0], 28, "OPM 1.0 = green");
}

/// White DeviceGray under the nonzero rule projects to no colorants (active=0),
/// so an overprinting white fill paints nothing — it must NOT knock the backdrop
/// out to white.
#[test]
fn white_gray_overprint_paints_nothing() {
    let content = b"1 1 0 rg 0 0 200 200 re f\n\
                    /GS0 gs 1 g 0 0 200 200 re f";
    let page = render(page_pdf(content, "/OP true /op true /OPM 1"));
    assert_near(
        px(&page, 100.0, 100.0),
        [255, 255, 0],
        10,
        "yellow preserved",
    );
}

/// Stroke overprint is driven by /OP: a cyan stroke over yellow mixes to green
/// along the stroked line.
#[test]
fn stroke_overprint_uses_op() {
    // Horizontal cyan line across the middle, 40pt wide, over a yellow page.
    let content = b"1 1 0 rg 0 0 200 200 re f\n\
                    /GS0 gs 1 0 0 0 K 40 w 0 100 m 200 100 l S";
    let page = render(page_pdf(content, "/OP true /OPM 1"));
    assert_near(
        px(&page, 100.0, 100.0),
        [0, 255, 0],
        30,
        "cyan stroke over yellow=green",
    );
    // Off the line (top): untouched yellow.
    assert_near(
        px(&page, 100.0, 180.0),
        [255, 255, 0],
        12,
        "yellow off-line",
    );
}
