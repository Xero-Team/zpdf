//! End-to-end test: an image XObject compressed with /JBIG2Decode (including
//! an indirect /JBIG2Globals stream) flows through the full pipeline — filter
//! decode, zpdf-image raw-sample decode, content interpretation — and lands
//! in the display list with the correct pixels and polarity.

use zpdf::display_list::RenderCommand;
use zpdf::{ContentInterpreter, ImageCache, PdfDocument};

/// JBIG2 globals stream: one page-information segment (number 0) declaring an
/// 8x2 page with default pixel white.
fn jbig2_globals() -> Vec<u8> {
    [
        &[0, 0, 0, 0, 0x30, 0x00, 0x01, 0, 0, 0, 19][..], // header, length 19
        &[0, 0, 0, 8, 0, 0, 0, 2][..],                    // width 8, height 2
        &[0; 8][..],                                      // x/y resolution
        &[0x00, 0, 0][..],                                // flags, striping
    ]
    .concat()
}

/// JBIG2 page stream: one immediate generic region (MMR-coded), two rows of
/// WWWBBWWW (black at columns 3-4).
fn jbig2_image_data() -> Vec<u8> {
    [
        &[0, 0, 0, 1, 0x26, 0x00, 0x01, 0, 0, 0, 20][..], // header, length 20
        &[0, 0, 0, 8, 0, 0, 0, 2][..],                    // region 8x2 …
        &[0, 0, 0, 0, 0, 0, 0, 0, 0x00][..],              // … at (0,0), op OR
        &[0x01, 0x31, 0xF8][..],                          // MMR flag + T.6 data
    ]
    .concat()
}

/// Single-page PDF drawing an 8x2 JBIG2-encoded DeviceGray image XObject.
fn build_pdf() -> Vec<u8> {
    let image = jbig2_image_data();
    let globals = jbig2_globals();
    let content = b"q 80 0 0 20 0 0 cm /Im0 Do Q";

    let mut buf: Vec<u8> = Vec::new();
    let mut offsets = [0usize; 7];
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
        "3 0 obj\n<</Type/Page/Parent 2 0 R/MediaBox[0 0 80 20]\
         /Resources<</XObject<</Im0 5 0 R>>>>/Contents 4 0 R>>\nendobj\n",
    );
    offsets[4] = buf.len();
    push(
        &mut buf,
        &format!("4 0 obj\n<</Length {}>>\nstream\n", content.len()),
    );
    buf.extend_from_slice(content);
    push(&mut buf, "\nendstream\nendobj\n");
    offsets[5] = buf.len();
    push(
        &mut buf,
        &format!(
            "5 0 obj\n<</Type/XObject/Subtype/Image/Width 8/Height 2\
             /ColorSpace/DeviceGray/BitsPerComponent 1/Filter/JBIG2Decode\
             /DecodeParms<</JBIG2Globals 6 0 R>>/Length {}>>\nstream\n",
            image.len()
        ),
    );
    buf.extend_from_slice(&image);
    push(&mut buf, "\nendstream\nendobj\n");
    offsets[6] = buf.len();
    push(
        &mut buf,
        &format!("6 0 obj\n<</Length {}>>\nstream\n", globals.len()),
    );
    buf.extend_from_slice(&globals);
    push(&mut buf, "\nendstream\nendobj\n");

    let xref_off = buf.len();
    push(&mut buf, "xref\n0 7\n0000000000 65535 f \n");
    for off in offsets.iter().skip(1) {
        push(&mut buf, &format!("{off:010} 00000 n \n"));
    }
    push(
        &mut buf,
        &format!("trailer\n<</Size 7/Root 1 0 R>>\nstartxref\n{xref_off}\n%%EOF\n"),
    );
    buf
}

#[test]
fn jbig2_image_xobject_decodes_through_pipeline() {
    let pdf = build_pdf();
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

    let image_id = dl
        .commands
        .iter()
        .find_map(|c| match c {
            RenderCommand::DrawImage(d) => Some(d.image_id),
            _ => None,
        })
        .expect("a DrawImage command from the JBIG2 XObject");

    let img = images.get(image_id).expect("decoded image in cache");
    assert_eq!((img.width, img.height), (8, 2));

    // WWWBBWWW on both rows: black RGBA at columns 3-4, white elsewhere.
    for y in 0..2 {
        for x in 0..8 {
            let px = &img.data[(y * 8 + x) * 4..(y * 8 + x) * 4 + 4];
            let expected = if (3..=4).contains(&x) { 0 } else { 255 };
            assert_eq!(
                px,
                [expected, expected, expected, 255],
                "pixel ({x},{y})"
            );
        }
    }
}
