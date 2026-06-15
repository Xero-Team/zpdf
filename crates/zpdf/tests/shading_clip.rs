//! `sh` shading acceptance (CPU backend): a shading painted under a small clip
//! must appear inside the clip and nowhere else. Guards the interpreter
//! optimization that rasterizes `sh` over the active clip bounds rather than the
//! whole page (a map with hundreds of small `sh` markers otherwise rasterizes a
//! full-page gradient per call — ~9s of interpret time on a large media box).
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

/// Radial red→blue shading, painted only inside a 40×40 clip centered at (70,70)
/// on a white 200×200 page.
fn clipped_shading_pdf() -> Vec<u8> {
    let content = b"1 1 1 rg 0 0 200 200 re f\n\
                    q 50 50 40 40 re W n\n/Sh1 sh\nQ";
    assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << /Shading << /Sh1 5 0 R >> >> >>"
            .to_vec(),
        stream_obj("", content),
        b"<< /ShadingType 3 /ColorSpace /DeviceRGB /Coords [70 70 0 70 70 30] \
          /Function 6 0 R /Extend [true true] >>"
            .to_vec(),
        b"<< /FunctionType 2 /Domain [0 1] /C0 [1 0 0] /C1 [0 0 1] /N 1 >>".to_vec(),
    ])
}

#[test]
fn sh_shading_paints_only_inside_clip() {
    let page = render(clipped_shading_pdf());

    // Center of the clip + shading: near the radial start color (red).
    let center = px(&page, 70.0, 70.0);
    assert!(
        center[0] > 120 && center[2] < 160 && center[0] > center[2],
        "clip center should be reddish shading, got {center:?}"
    );

    // Far corner, outside the clip: the shading must NOT bleed there — stays the
    // white background. (Before the clip-bounds fix the gradient was rasterized
    // over the whole page; the clip still cropped it, but this locks correctness.)
    let outside = px(&page, 20.0, 20.0);
    assert_eq!(outside, [255, 255, 255], "outside the clip must stay white");

    // Just outside the clip rect but inside the shading's radius if unclipped.
    let near_outside = px(&page, 95.0, 95.0);
    assert_eq!(
        near_outside,
        [255, 255, 255],
        "shading must be clipped to the 40×40 rect, got {near_outside:?}"
    );
}
