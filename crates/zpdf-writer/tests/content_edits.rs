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
            &RedactOptions {
                fill: Some((0.7, 0.2, 0.1)),
            },
        )
        .unwrap();
    assert_has_fill(writer, b"180 180 10 10 re f");
}
fn assert_has_fill(writer: &IncrementalWriter, operation: &[u8]) {
    let page_id = writer.document().page(0).unwrap().id;
    let page = writer.resolve_current(page_id).unwrap();
    let id = page.as_dict().unwrap().get_ref("Contents").unwrap();
    let object = writer.resolve_current(id).unwrap();
    let stream = object.as_stream().unwrap();
    let bytes = zpdf_parser::filters::decode_stream_with_limits(
        &stream.data,
        &stream.dict,
        writer.document().file().limits(),
    )
    .unwrap();
    assert!(bytes.windows(operation.len()).any(|part| part == operation));
}
fn stamp(writer: &mut IncrementalWriter) -> zpdf_core::Result<()> {
    writer.stamp_page(
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
    stamp(&mut writer).unwrap();
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
            &RedactOptions {
                fill: Some((0.7, 0.2, 0.1)),
            },
        )
        .unwrap();
    assert_has_fill(&writer, b"210 210 10 10 re f");
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
    assert!(stamp(&mut writer).is_err());
    assert_eq!(save(&writer), before);
}

#[test]
fn encrypted_original_contents_survive_current_stream_decoding() {
    for config in [
        zpdf_writer::EncryptionConfig::rc4_128("secret", "owner"),
        zpdf_writer::EncryptionConfig::aes256("secret", "owner"),
    ] {
        let source = zpdf_parser::PdfFile::parse(indirect_pdf()).unwrap();
        let bytes = zpdf_writer::rewrite_pdf(
            &source,
            &zpdf_writer::RewriteOptions {
                encrypt: Some(config),
                ..Default::default()
            },
        )
        .unwrap();
        let mut writer = IncrementalWriter::new_with_password(bytes, b"secret").unwrap();
        stamp(&mut writer).unwrap();
        outside(&mut writer);
        let doc = PdfDocument::open_with_password(save(&writer), b"secret").unwrap();
        let page = doc.page(0).unwrap();
        let content = doc.page_content_bytes(&page).unwrap();
        assert!(content.windows(5).any(|part| part == b"HELLO"));
        assert!(content.windows(6).any(|part| part == b"PUBLIC"));
    }
}
