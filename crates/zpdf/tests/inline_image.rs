//! End-to-end test: an inline image (BI/ID/EI) in a page content stream is
//! decoded and emitted as a DrawImage command.

use zpdf::display_list::RenderCommand;
use zpdf::{ContentInterpreter, ImageCache, PdfDocument};

/// Build a single-page PDF with raw `content` bytes (so binary inline-image data
/// survives intact) and correct xref offsets.
fn build_pdf(content: &[u8]) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    let mut offsets = [0usize; 5];
    let push = |buf: &mut Vec<u8>, s: &str| buf.extend_from_slice(s.as_bytes());

    push(&mut buf, "%PDF-1.7\n");
    offsets[1] = buf.len();
    push(&mut buf, "1 0 obj\n<</Type/Catalog/Pages 2 0 R>>\nendobj\n");
    offsets[2] = buf.len();
    push(
        &mut buf,
        "2 0 obj\n<</Type/Pages/Kids[3 0 R]/Count 1>>\nendobj\n",
    );
    offsets[3] = buf.len();
    push(
        &mut buf,
        "3 0 obj\n<</Type/Page/Parent 2 0 R/MediaBox[0 0 100 100]/Resources<<>>/Contents 4 0 R>>\nendobj\n",
    );
    offsets[4] = buf.len();
    push(
        &mut buf,
        &format!("4 0 obj\n<</Length {}>>\nstream\n", content.len()),
    );
    buf.extend_from_slice(content);
    push(&mut buf, "\nendstream\nendobj\n");

    let xref_off = buf.len();
    push(&mut buf, "xref\n0 5\n0000000000 65535 f \n");
    for off in offsets.iter().skip(1) {
        push(&mut buf, &format!("{off:010} 00000 n \n"));
    }
    push(
        &mut buf,
        &format!("trailer\n<</Size 5/Root 1 0 R>>\nstartxref\n{xref_off}\n%%EOF\n"),
    );
    buf
}

#[test]
fn inline_image_emits_draw_image() {
    // 2x2 DeviceRGB 8bpc => 12 raw bytes.
    let mut content: Vec<u8> = b"q 100 0 0 100 0 0 cm BI /W 2 /H 2 /CS /RGB /BPC 8 ID ".to_vec();
    content.extend_from_slice(&[
        255, 0, 0, 0, 255, 0, // row 0: red, green
        0, 0, 255, 255, 255, 0, // row 1: blue, yellow
    ]);
    content.extend_from_slice(b" EI Q");

    let pdf = build_pdf(&content);
    let doc = PdfDocument::open(pdf).expect("open");
    let page = doc.page(0).expect("page");
    let mut fonts = doc.load_page_fonts(&page);
    let bytes = doc.page_content_bytes(&page).expect("content");
    let mut images = ImageCache::new();

    let dl = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images)
        .interpret(&bytes);

    let draw_images = dl
        .commands
        .iter()
        .filter(|c| matches!(c, RenderCommand::DrawImage(_)))
        .count();
    assert_eq!(
        draw_images, 1,
        "expected exactly one DrawImage from the inline image"
    );
}
