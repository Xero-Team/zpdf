//! End-to-end text-extraction tests over hand-built minimal PDFs.
//!
//! These double as a tiny synthetic corpus: each test assembles a syntactically
//! valid PDF (with correct xref offsets) entirely in memory, then runs the full
//! open → load fonts → interpret → extract pipeline.

use zpdf::{ContentInterpreter, ImageCache, PdfDocument, TextSpan};

/// Build a single-page PDF whose only content is `content`, using `font_obj` as
/// object 4 (referenced as /F1 in the page resources).
fn build_pdf(font_obj: &str, content: &str) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    let mut offsets = [0usize; 6];

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
        "3 0 obj\n<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]\
/Resources<</Font<</F1 4 0 R>>>>/Contents 5 0 R>>\nendobj\n",
    );

    offsets[4] = buf.len();
    push(&mut buf, &format!("4 0 obj\n{font_obj}\nendobj\n"));

    offsets[5] = buf.len();
    push(
        &mut buf,
        &format!("5 0 obj\n<</Length {}>>\nstream\n", content.len()),
    );
    push(&mut buf, content);
    push(&mut buf, "\nendstream\nendobj\n");

    let xref_off = buf.len();
    push(&mut buf, "xref\n0 6\n0000000000 65535 f \n");
    for off in offsets.iter().skip(1) {
        push(&mut buf, &format!("{off:010} 00000 n \n"));
    }
    push(
        &mut buf,
        &format!("trailer\n<</Size 6/Root 1 0 R>>\nstartxref\n{xref_off}\n%%EOF\n"),
    );

    buf
}

fn extract_first_page(pdf: &[u8]) -> Vec<TextSpan> {
    let doc = PdfDocument::open(pdf.to_vec()).expect("open pdf");
    let page = doc.page(0).expect("page 0");
    let mut font_cache = doc.load_page_fonts(&page);
    let content = doc.page_content_bytes(&page).expect("content bytes");
    let mut image_cache = ImageCache::new();
    let mut spans: Vec<TextSpan> = Vec::new();
    {
        let interpreter = ContentInterpreter::new(page.media_box)
            .with_fonts(&mut font_cache)
            .with_document(doc.file(), &page.resources)
            .with_images(&mut image_cache)
            .with_text_sink(&mut spans);
        let _ = interpreter.interpret(&content);
    }
    spans
}

#[test]
fn extracts_winansi_standard_font_text() {
    let pdf = build_pdf(
        "<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>",
        "BT /F1 24 Tf 20 100 Td (Hello, World!) Tj ET",
    );
    let spans = extract_first_page(&pdf);
    let text: String = spans.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(text, "Hello, World!");
    // Baseline origin should reflect the Td position.
    assert!((spans[0].x - 20.0).abs() < 1.0, "x was {}", spans[0].x);
    assert!((spans[0].y - 100.0).abs() < 1.0, "y was {}", spans[0].y);
}

#[test]
fn applies_encoding_differences() {
    // Map code 65 ('A') to the glyph "bullet" (U+2022) and 66 ('B') to "emdash".
    let pdf = build_pdf(
        "<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding\
<</Type/Encoding/BaseEncoding/WinAnsiEncoding/Differences[65/bullet 66/emdash]>>>>",
        "BT /F1 24 Tf 20 100 Td (AB C) Tj ET",
    );
    let spans = extract_first_page(&pdf);
    let text: String = spans.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(text, "\u{2022}\u{2014} C");
}

#[test]
fn symbol_font_extraction() {
    // The Symbol standard font carries a built-in encoding even with no /Encoding:
    // codes a/b/g map to glyphs alpha/beta/gamma -> U+03B1/03B2/03B3.
    let pdf = build_pdf(
        "<</Type/Font/Subtype/Type1/BaseFont/Symbol>>",
        "BT /F1 12 Tf 10 100 Td (abg) Tj ET",
    );
    let spans = extract_first_page(&pdf);
    let text: String = spans.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(text, "\u{03B1}\u{03B2}\u{03B3}");
}

#[test]
fn winansi_high_bytes_decode() {
    // 0x93/0x94 are the curly double quotes in WinAnsi; \205 (0x85) is ellipsis.
    let pdf = build_pdf(
        "<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>",
        "BT /F1 12 Tf 10 100 Td (\\223hi\\224\\205) Tj ET",
    );
    let spans = extract_first_page(&pdf);
    let text: String = spans.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(text, "\u{201C}hi\u{201D}\u{2026}");
}
