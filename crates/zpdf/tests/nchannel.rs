//! Separation / DeviceN / NChannel colorant-semantics acceptance tests on the
//! CPU backend (ISO 32000-1 §8.6.6.4/§8.6.6.5, PDF 2.0 NChannel).
//!
//! Covers the special colorant names `None` (produces no marks) and `All`
//! (Separation → every colorant, an overprint knockout), and the PDF 2.0
//! NChannel per-colorant overprint mask: a `/None` component contributes no
//! ink, and `/Colorants` spot colorants project through their own Separation
//! space.
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

/// A 200×200 page whose `/Resources` dict body is `resources`, with `content` as
/// its stream. `extra` objects are appended after the four base objects, so they
/// are numbered from `5 0 R` upward and can be referenced from `resources`.
fn build(content: &[u8], resources: &str, extra: &[Vec<u8>]) -> Vec<u8> {
    let mut objs = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
             /Resources << {resources} >> >>"
        )
        .into_bytes(),
        stream_obj("", content),
    ];
    objs.extend_from_slice(extra);
    assemble(&objs)
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

fn assert_near(c: [u8; 3], want: [u8; 3], tol: u8, what: &str) {
    let ok = c
        .iter()
        .zip(want.iter())
        .all(|(a, b)| (*a as i32 - *b as i32).abs() <= tol as i32);
    assert!(ok, "{what}: got {c:?}, want ≈{want:?} (±{tol})");
}

/// A `/None` Separation produces no marks: a fill in it must leave the red
/// backdrop untouched, even though its tint transform would map to black.
#[test]
fn none_separation_fill_paints_nothing() {
    let resources = "/ColorSpace << /NoneCS [/Separation /None /DeviceCMYK \
        << /FunctionType 2 /Domain [0 1] /C0 [0 0 0 0] /C1 [0 0 0 1] /N 1 >>] >>";
    let content = b"1 0 0 rg 0 0 200 200 re f\n\
                    /NoneCS cs 1 scn 0 0 200 200 re f";
    let page = render(build(content, resources, &[]));
    assert_near(
        px(&page, 100.0, 100.0),
        [255, 0, 0],
        4,
        "None fill is invisible",
    );
}

/// A `/None` Separation stroke is likewise invisible — a thick line drawn in it
/// must not appear over the red backdrop.
#[test]
fn none_separation_stroke_paints_nothing() {
    let resources = "/ColorSpace << /NoneCS [/Separation /None /DeviceGray \
        << /FunctionType 2 /Domain [0 1] /C0 [1] /C1 [0] /N 1 >>] >>";
    let content = b"1 0 0 rg 0 0 200 200 re f\n\
                    /NoneCS CS 20 w 1 SCN 0 100 m 200 100 l S";
    let page = render(build(content, resources, &[]));
    assert_near(
        px(&page, 100.0, 100.0),
        [255, 0, 0],
        4,
        "None stroke is invisible",
    );
}

/// A DeviceN whose colorants are all `/None` produces no marks (here a two-
/// component all-None space with a real two-input tint transform).
#[test]
fn all_none_devicen_paints_nothing() {
    // `{ pop pop 0 0 0 1 }`: drop both inputs, emit CMYK black.
    let func = stream_obj(
        "/FunctionType 4 /Domain [0 1 0 1] /Range [0 1 0 1 0 1 0 1]",
        b"{ pop pop 0 0 0 1 }",
    );
    let resources = "/ColorSpace << /NN [/DeviceN [/None /None] /DeviceCMYK 5 0 R] >>";
    let content = b"1 0 0 rg 0 0 200 200 re f\n\
                    /NN cs 0.5 0.5 scn 0 0 200 200 re f";
    let page = render(build(content, resources, &[func]));
    assert_near(
        px(&page, 100.0, 100.0),
        [255, 0, 0],
        4,
        "all-None DeviceN is invisible",
    );
}

/// PDF 2.0 NChannel per-colorant overprint: `[/Magenta /None]` overprinting onto
/// white. The `/None` component must contribute no ink even though the tint
/// transform maps it to Yellow — so only the Magenta colorant is painted and the
/// result is magenta (high blue), not the orange a leaked Yellow would give.
#[test]
fn nchannel_none_component_adds_no_ink() {
    // `{ 0 3 1 roll 0 }`: inputs (m, n) → CMYK (0, m, n, 0) — the None component
    // would map to Yellow if it were (incorrectly) honoured.
    let func = stream_obj(
        "/FunctionType 4 /Domain [0 1 0 1] /Range [0 1 0 1 0 1 0 1]",
        b"{ 0 3 1 roll 0 }",
    );
    let gs = b"<< /Type /ExtGState /OP true /op true /OPM 1 >>".to_vec();
    let resources = "/ColorSpace << /NC [/DeviceN [/Magenta /None] /DeviceCMYK 5 0 R] >> \
        /ExtGState << /GS0 6 0 R >>";
    let content = b"1 1 1 rg 0 0 200 200 re f\n\
                    /GS0 gs /NC cs 0.8 0.6 scn 0 0 200 200 re f";
    let page = render(build(content, resources, &[func, gs]));
    let c = px(&page, 100.0, 100.0);
    // Magenta over white in naïve CMYK ≈ (255, 51, 255). A leaked Yellow would
    // drop blue to ≈102, so the discriminating check is "blue stayed high".
    assert!(
        c[2] > 180 && c[0] > 180 && c[1] < 120,
        "magenta-only overprint should keep blue high (got {c:?})"
    );
}

/// PDF 2.0 NChannel `/Colorants`: a spot colorant projects through its own
/// Separation space for overprint. `[/Cyan /MySpot]` where `/MySpot` maps to
/// Yellow → cyan + yellow overprinting onto white mix to green.
#[test]
fn nchannel_colorants_spot_overprints() {
    // Display transform (2-in → CMYK); irrelevant under overprint (only coverage
    // is consumed), so it just emits black.
    let func = stream_obj(
        "/FunctionType 4 /Domain [0 1 0 1] /Range [0 1 0 1 0 1 0 1]",
        b"{ pop pop 0 0 0 1 }",
    );
    let gs = b"<< /Type /ExtGState /OP true /op true /OPM 1 >>".to_vec();
    // /MySpot's own Separation maps tint 1 → DeviceCMYK yellow (0,0,1,0).
    let resources = "/ColorSpace << /NC [/DeviceN [/Cyan /MySpot] /DeviceCMYK 5 0 R \
        << /Subtype /NChannel /Colorants << /MySpot [/Separation /MySpot /DeviceCMYK \
        << /FunctionType 2 /Domain [0 1] /C0 [0 0 0 0] /C1 [0 0 1 0] /N 1 >>] >> >>] >> \
        /ExtGState << /GS0 6 0 R >>";
    let content = b"1 1 1 rg 0 0 200 200 re f\n\
                    /GS0 gs /NC cs 1 1 scn 0 0 200 200 re f";
    let page = render(build(content, resources, &[func, gs]));
    // Cyan (C=1) + spot Yellow (Y=1) over white → green (0,255,0).
    assert_near(
        px(&page, 100.0, 100.0),
        [0, 255, 0],
        28,
        "cyan + spot-yellow = green",
    );
}

/// A `/None` dropped from the overprint mask even when a *sibling* spot is
/// unclassifiable (no `/Colorants`): `[/Spot1 /None]` with a transform that maps
/// the `/None` component to Yellow. Spot1 isolates to Magenta and the `/None`
/// contributes nothing, so blue stays high (a leaked Yellow would drop it).
#[test]
fn nchannel_none_dropped_beside_unclassifiable_spot() {
    // `{ 0 3 1 roll 0 }`: inputs (s, n) → CMYK (0, s, n, 0).
    let func = stream_obj(
        "/FunctionType 4 /Domain [0 1 0 1] /Range [0 1 0 1 0 1 0 1]",
        b"{ 0 3 1 roll 0 }",
    );
    let gs = b"<< /Type /ExtGState /OP true /op true /OPM 1 >>".to_vec();
    let resources = "/ColorSpace << /NC [/DeviceN [/Spot1 /None] /DeviceCMYK 5 0 R] >> \
        /ExtGState << /GS0 6 0 R >>";
    let content = b"1 1 1 rg 0 0 200 200 re f\n\
                    /GS0 gs /NC cs 0.8 0.6 scn 0 0 200 200 re f";
    let page = render(build(content, resources, &[func, gs]));
    let c = px(&page, 100.0, 100.0);
    assert!(
        c[2] > 180 && c[0] > 180 && c[1] < 120,
        "isolated Spot1 should keep blue high (None dropped), got {c:?}"
    );
}

/// A `/ImageMask` stencil paints in the current fill colour, so a `/None` fill
/// colour space must suppress it: an all-paint stencil over a red backdrop must
/// leave the backdrop untouched.
#[test]
fn none_fill_suppresses_image_mask_stencil() {
    // 1×1 stencil, byte 0x00 → sample 0 → paints (default /Decode [0 1]).
    let img = stream_obj(
        "/Subtype /Image /Width 1 /Height 1 /ImageMask true /BitsPerComponent 1",
        &[0x00],
    );
    let resources = "/ColorSpace << /NoneCS [/Separation /None /DeviceCMYK \
        << /FunctionType 2 /Domain [0 1] /C0 [0 0 0 0] /C1 [0 0 0 1] /N 1 >>] >> \
        /XObject << /Im 5 0 R >>";
    let content = b"1 0 0 rg 0 0 200 200 re f\n\
                    /NoneCS cs 1 scn q 200 0 0 200 0 0 cm /Im Do Q";
    let page = render(build(content, resources, &[img]));
    assert_near(
        px(&page, 100.0, 100.0),
        [255, 0, 0],
        4,
        "None fill suppresses stencil mask",
    );
}

/// A normal spot Separation still marks (a control for the `/None` cases): tint
/// 1 → magenta via the DeviceCMYK alternate, painted opaquely over white.
#[test]
fn spot_separation_still_paints() {
    let resources = "/ColorSpace << /Spot [/Separation /MySpot /DeviceCMYK \
        << /FunctionType 2 /Domain [0 1] /C0 [0 0 0 0] /C1 [0 1 0 0] /N 1 >>] >>";
    let content = b"1 1 1 rg 0 0 200 200 re f\n\
                    /Spot cs 1 scn 0 0 200 200 re f";
    let page = render(build(content, resources, &[]));
    let c = px(&page, 100.0, 100.0);
    // Magenta is not white: it has a clearly depressed green channel.
    assert!(c[1] < 120, "spot magenta should paint (got {c:?})");
}
