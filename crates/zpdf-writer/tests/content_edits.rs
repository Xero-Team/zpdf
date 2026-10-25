use std::io::Cursor;
use zpdf_core::{PdfName, PdfObject, Rect};
use zpdf_document::PdfDocument;
use zpdf_writer::{DocumentBuilder, IncrementalWriter, RedactOptions, StampItem};

fn save(writer: &IncrementalWriter) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    writer.write(&mut out).unwrap();
    out.into_inner()
}
fn indirect_pdf() -> Vec<u8> {
    let mut builder = DocumentBuilder::new();
    let page = builder.add_page(200.0, 200.0);
    builder
        .add_text(
            page,
            "HELLO",
            20.0,
            100.0,
            "Helvetica",
            12.0,
            (0.0, 0.0, 0.0),
        )
        .unwrap();
    builder
        .add_text(
            page,
            "PUBLIC",
            20.0,
            30.0,
            "Helvetica",
            12.0,
            (0.0, 0.0, 0.0),
        )
        .unwrap();
    let mut writer = IncrementalWriter::new(builder.build().unwrap()).unwrap();
    let page = writer.document().page(0).unwrap();
    let mut dict = writer
        .resolve_current(page.id)
        .unwrap()
        .as_dict()
        .unwrap()
        .clone();
    let refs = PdfObject::Array(page.contents.into_iter().map(PdfObject::Ref).collect());
    let (n, g) = writer.add_object(&refs);
    dict.insert(
        PdfName::new("Contents"),
        PdfObject::Ref(zpdf_core::ObjectId(n, g as u16)),
    );
    writer.overwrite_object(page.id, PdfObject::Dict(dict));
    save(&writer)
}
fn text(bytes: Vec<u8>) -> String {
    let doc = PdfDocument::open(bytes).unwrap();
    let page = doc.page(0).unwrap();
    let mut fonts = doc.load_page_fonts(&page);
    let mut spans = Vec::new();
    zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
        .with_document(doc.file(), &page.resources)
        .with_fonts(&mut fonts)
        .with_text_sink(&mut spans)
        .interpret(&doc.page_content_bytes(&page).unwrap());
    zpdf_content::text::spans_to_text(spans, 2.0)
}
fn outside(writer: &mut IncrementalWriter) {
    writer
        .redact_page(
            0,
            &[Rect::new(180.0, 180.0, 190.0, 190.0)],
            &RedactOptions { fill: None },
        )
        .unwrap();
}
fn stamp(writer: &mut IncrementalWriter) {
    writer
        .stamp_page(
            0,
            &[StampItem::Text {
                text: "STAMP".into(),
                x: 10.0,
                y: 10.0,
                font: "Helvetica".into(),
                size: 8.0,
                color: (0.0, 0.0, 0.0),
            }],
        )
        .unwrap();
}
#[test]
fn redaction_preserves_indirect_content_outside_the_region() {
    let mut writer = IncrementalWriter::new(indirect_pdf()).unwrap();
    outside(&mut writer);
    let text = text(save(&writer));
    assert!(text.contains("HELLO") && text.contains("PUBLIC"), "{text}");
}
#[test]
fn stamp_keeps_indirect_content_and_survives_a_later_redaction() {
    let mut writer = IncrementalWriter::new(indirect_pdf()).unwrap();
    stamp(&mut writer);
    let before = text(save(&writer));
    assert!(
        before.contains("HELLO") && before.contains("STAMP"),
        "{before}"
    );
    // A stamp's Form BBox covers the page; redaction intentionally drops the
    // whole Form when that BBox intersects. Keep this region outside it.
    writer
        .redact_page(
            0,
            &[Rect::new(210.0, 210.0, 220.0, 220.0)],
            &RedactOptions { fill: None },
        )
        .unwrap();
    let after = text(save(&writer));
    assert!(
        after.contains("HELLO") && after.contains("STAMP"),
        "{after}"
    );
}
#[test]
fn consecutive_redactions_read_the_latest_pending_stream() {
    let mut writer = IncrementalWriter::new(indirect_pdf()).unwrap();
    writer
        .redact_page(
            0,
            &[Rect::new(10.0, 90.0, 120.0, 120.0)],
            &RedactOptions { fill: None },
        )
        .unwrap();
    outside(&mut writer);
    let text = text(save(&writer));
    assert!(!text.contains("HELLO") && text.contains("PUBLIC"), "{text}");
}
#[test]
fn failed_decoding_leaves_the_edit_unchanged() {
    let mut writer = IncrementalWriter::new(indirect_pdf()).unwrap();
    let mut stream_dict = zpdf_core::PdfDict::new();
    stream_dict.insert(
        PdfName::new("Filter"),
        PdfObject::Name(PdfName::new("UnsupportedFilter")),
    );
    let (n, g) = writer.add_stream(&stream_dict, b"bad");
    let page_id = writer.document().page(0).unwrap().id;
    let mut page = writer
        .resolve_current(page_id)
        .unwrap()
        .as_dict()
        .unwrap()
        .clone();
    page.insert(
        PdfName::new("Contents"),
        PdfObject::Ref(zpdf_core::ObjectId(n, g as u16)),
    );
    writer.overwrite_object(page_id, PdfObject::Dict(page));
    let before = save(&writer);
    assert!(writer
        .redact_page(
            0,
            &[Rect::new(180.0, 180.0, 190.0, 190.0)],
            &RedactOptions { fill: None }
        )
        .is_err());
    assert_eq!(save(&writer), before);
}
