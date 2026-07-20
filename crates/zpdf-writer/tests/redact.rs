//! End-to-end redaction: text under the region must vanish from extraction,
//! text outside must survive.

use std::io::Cursor;
use zpdf_core::Rect;
use zpdf_document::PdfDocument;
use zpdf_writer::redact::RedactOptions;
use zpdf_writer::{DocumentBuilder, IncrementalWriter};

fn two_line_pdf() -> Vec<u8> {
    let mut b = DocumentBuilder::new();
    let p = b.add_page(612.0, 792.0);
    b.add_text(
        p,
        "TOP SECRET LINE",
        72.0,
        700.0,
        "Helvetica",
        14.0,
        (0.0, 0.0, 0.0),
    )
    .unwrap();
    b.add_text(
        p,
        "public information",
        72.0,
        100.0,
        "Helvetica",
        14.0,
        (0.0, 0.0, 0.0),
    )
    .unwrap();
    b.build().unwrap()
}

/// Decoded content of every page content stream.
fn page_content(doc: &PdfDocument) -> String {
    let page = doc.page(0).expect("page");
    let mut all = String::new();
    for id in &page.contents {
        let bytes = doc.file().resolve_stream_data(*id).unwrap_or_default();
        all.push_str(&String::from_utf8_lossy(&bytes));
    }
    all
}

#[test]
fn redacted_text_is_gone_from_content_stream() {
    let mut writer = IncrementalWriter::new(two_line_pdf()).expect("writer");
    writer
        .redact_page(
            0,
            &[Rect::new(60.0, 690.0, 400.0, 720.0)],
            &RedactOptions::default(),
        )
        .expect("redact");
    let mut out = Cursor::new(Vec::new());
    writer.write(&mut out).expect("write");
    let doc = PdfDocument::open(out.into_inner()).expect("open");

    let content = page_content(&doc);
    assert!(
        !content.contains("TOP SECRET"),
        "redacted string must not survive in the content stream: {content}"
    );
    assert!(
        content.contains("public information"),
        "unrelated text must survive: {content}"
    );
    // The default black box must be present.
    assert!(content.contains("re f"), "fill box drawn: {content}");
}

#[test]
fn redaction_without_fill_leaves_no_box() {
    let mut writer = IncrementalWriter::new(two_line_pdf()).expect("writer");
    writer
        .redact_page(
            0,
            &[Rect::new(60.0, 690.0, 400.0, 720.0)],
            &RedactOptions { fill: None },
        )
        .expect("redact");
    let mut out = Cursor::new(Vec::new());
    writer.write(&mut out).expect("write");
    let doc = PdfDocument::open(out.into_inner()).expect("open");
    let content = page_content(&doc);
    assert!(!content.contains("TOP SECRET"));
    assert!(!content.contains("re f"), "no fill box: {content}");
}

#[test]
fn annotations_in_region_are_removed() {
    use zpdf_writer::{AnnotationSpec, MarkupKind};

    // First add an annotation over the to-be-redacted area…
    let mut writer = IncrementalWriter::new(two_line_pdf()).expect("writer");
    writer
        .add_annotation(
            0,
            &AnnotationSpec::markup_from_rects(
                MarkupKind::Highlight,
                &[Rect::new(70.0, 695.0, 200.0, 715.0)],
                (1.0, 1.0, 0.0),
                Some("covers the secret".into()),
            ),
        )
        .expect("annotate");
    let mut out = Cursor::new(Vec::new());
    writer.write(&mut out).expect("write");

    // …then redact the region in a second revision.
    let mut writer = IncrementalWriter::new(out.into_inner()).expect("reopen");
    writer
        .redact_page(
            0,
            &[Rect::new(60.0, 690.0, 400.0, 720.0)],
            &RedactOptions::default(),
        )
        .expect("redact");
    let mut out = Cursor::new(Vec::new());
    writer.write(&mut out).expect("write");
    let doc = PdfDocument::open(out.into_inner()).expect("open");
    let page = doc.page(0).expect("page");
    assert!(
        page.annots.is_empty(),
        "annotation overlapping the redaction must be removed"
    );
}

#[test]
fn non_finite_rect_is_rejected() {
    let mut writer = IncrementalWriter::new(two_line_pdf()).expect("writer");
    let result = writer.redact_page(
        0,
        &[Rect::new(f64::NAN, 0.0, 10.0, 10.0)],
        &RedactOptions::default(),
    );
    assert!(result.is_err());
}
