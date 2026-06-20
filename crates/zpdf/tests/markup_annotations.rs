//! End-to-end render tests for generated markup & geometric annotation
//! appearances (annotations with no `/AP`). Each builds a tiny PDF, renders
//! page 0 through the CPU backend with annotations wired like the CLI, and
//! checks pixels in page space.
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

/// Build a 200×200 page whose content is `page_content` and whose single
/// annotation is `annot` (an object body). Renders page 0 with annotations.
fn render_with_annot(page_content: &[u8], annot: &[u8]) -> zpdf::cpu::RenderedPage {
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
          /Resources << >> /Annots [5 0 R] >>"
            .to_vec(),
        stream_obj("", page_content),
        annot.to_vec(),
    ]);
    render(pdf)
}

fn render(pdf: Vec<u8>) -> zpdf::cpu::RenderedPage {
    let doc = PdfDocument::open(pdf).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut fonts = doc.load_page_fonts(&page);
    let content = doc.page_content_bytes(&page).expect("content bytes");
    let mut images = ImageCache::new();
    let annotations = doc.page_annotations(&page);
    let interp = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images)
        .with_annotations(&annotations);
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
        .all(|(a, b)| (*a as i32 - *b as i32).abs() <= 16);
    assert!(ok, "{what}: got {c:?}, want ≈{want:?}");
}

/// A Highlight over black text stays black (Multiply), over white turns yellow.
#[test]
fn highlight_multiplies_over_content() {
    // White background, black rectangle over the left half.
    let content: &[u8] = b"1 1 1 rg 0 0 200 200 re f\n0 0 0 rg 0 0 100 200 re f";
    // Quad covers x[20,180] y[80,120], Acrobat point order (TL TR BL BR).
    let annot: &[u8] = b"<< /Type /Annot /Subtype /Highlight /Rect [20 80 180 120] /F 4 \
        /QuadPoints [20 120 180 120 20 80 180 80] /C [1 1 0] >>";
    let page = render_with_annot(content, annot);

    assert_near(
        px(&page, 150.0, 100.0),
        [255, 255, 0],
        "highlight over white → yellow",
    );
    assert_near(
        px(&page, 50.0, 100.0),
        [0, 0, 0],
        "highlight over black → black",
    );
    assert_near(
        px(&page, 150.0, 150.0),
        [255, 255, 255],
        "above quad untouched (white)",
    );
    assert_near(
        px(&page, 50.0, 150.0),
        [0, 0, 0],
        "above quad untouched (black)",
    );
}

/// An Underline draws a coloured line near the bottom of its quad; the middle
/// of the quad stays clear.
#[test]
fn underline_strokes_near_quad_bottom() {
    let content: &[u8] = b"1 1 1 rg 0 0 200 200 re f";
    let annot: &[u8] = b"<< /Type /Annot /Subtype /Underline /Rect [20 80 180 120] /F 4 \
        /QuadPoints [20 120 180 120 20 80 180 80] /C [1 0 0] >>";
    let page = render_with_annot(content, annot);

    // Line at y = 80 + 40*0.12 = 84.8, ~2.4pt thick.
    assert_near(
        px(&page, 100.0, 85.0),
        [255, 0, 0],
        "red underline near bottom",
    );
    assert_near(
        px(&page, 100.0, 105.0),
        [255, 255, 255],
        "quad middle is clear",
    );
}

/// A Square with an interior colour fills green and strokes red.
#[test]
fn square_fills_and_strokes() {
    let content: &[u8] = b"1 1 1 rg 0 0 200 200 re f";
    let annot: &[u8] = b"<< /Type /Annot /Subtype /Square /Rect [40 40 160 160] /F 4 \
        /IC [0 1 0] /C [1 0 0] /BS << /W 4 >> >>";
    let page = render_with_annot(content, annot);

    assert_near(px(&page, 100.0, 100.0), [0, 255, 0], "green interior");
    assert_near(px(&page, 42.0, 100.0), [255, 0, 0], "red left border");
    assert_near(px(&page, 20.0, 100.0), [255, 255, 255], "outside untouched");
}

/// A Line annotation strokes between its two endpoints.
#[test]
fn line_strokes_between_endpoints() {
    let content: &[u8] = b"1 1 1 rg 0 0 200 200 re f";
    let annot: &[u8] = b"<< /Type /Annot /Subtype /Line /Rect [40 90 160 110] /F 4 \
        /L [40 100 160 100] /C [0 0 1] /BS << /W 3 >> >>";
    let page = render_with_annot(content, annot);

    assert_near(px(&page, 100.0, 100.0), [0, 0, 255], "blue line at y=100");
    assert_near(
        px(&page, 100.0, 130.0),
        [255, 255, 255],
        "off the line is clear",
    );
}

/// A Polygon fills its interior (/IC) and strokes the default black border.
#[test]
fn polygon_fills_interior() {
    let content: &[u8] = b"1 1 1 rg 0 0 200 200 re f";
    let annot: &[u8] = b"<< /Type /Annot /Subtype /Polygon /Rect [40 40 160 160] /F 4 \
        /Vertices [50 50 150 50 100 150] /IC [0 0 1] >>";
    let page = render_with_annot(content, annot);

    assert_near(
        px(&page, 100.0, 70.0),
        [0, 0, 255],
        "blue triangle interior",
    );
    assert_near(
        px(&page, 20.0, 100.0),
        [255, 255, 255],
        "outside the triangle",
    );
}

/// A hidden (/F 2) markup annotation with no /AP generates nothing visible.
#[test]
fn hidden_markup_is_not_painted() {
    let content: &[u8] = b"1 1 1 rg 0 0 200 200 re f";
    let annot: &[u8] = b"<< /Type /Annot /Subtype /Square /Rect [40 40 160 160] /F 2 \
        /IC [0 1 0] /C [1 0 0] /BS << /W 4 >> >>";
    let page = render_with_annot(content, annot);
    assert_near(
        px(&page, 100.0, 100.0),
        [255, 255, 255],
        "hidden annot not painted",
    );
}
