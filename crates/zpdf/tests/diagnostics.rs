use zpdf::{ContentInterpreter, Error, PdfDocument, ProcessingDiagnostics, Rect};
use zpdf_core::{PdfDict, PdfName, PdfObject};
use zpdf_writer::{DocumentBuilder, IncrementalWriter};

fn damaged_content() -> Vec<u8> {
    let mut b = DocumentBuilder::new();
    let p = b.add_page(100.0, 100.0);
    b.add_text(
        p,
        "SURVIVES",
        10.0,
        50.0,
        "Helvetica",
        12.0,
        (0.0, 0.0, 0.0),
    )
    .unwrap();
    let mut w = IncrementalWriter::new(b.build().unwrap()).unwrap();
    let page = w.document().page(0).unwrap();
    let mut dict = w
        .document()
        .file()
        .resolve(page.id)
        .unwrap()
        .as_dict()
        .unwrap()
        .clone();
    let mut bad = PdfDict::new();
    bad.insert(
        PdfName::new("Filter"),
        PdfObject::Name(PdfName::new("UnsupportedDecode")),
    );
    let (n, g) = w.try_add_stream(&bad, b"bad").unwrap();
    dict.insert(
        PdfName::new("Contents"),
        PdfObject::Array(vec![
            PdfObject::Ref(page.contents[0]),
            PdfObject::Ref(zpdf::ObjectId(n, g as u16)),
        ]),
    );
    w.overwrite_object(page.id, PdfObject::Dict(dict));
    let mut out = std::io::Cursor::new(Vec::new());
    w.write(&mut out).unwrap();
    out.into_inner()
}

#[test]
fn partial_page_content_is_explicit_in_both_high_level_apis() {
    let doc = PdfDocument::open(damaged_content()).unwrap();
    assert!(matches!(
        doc.page_content_bytes(&doc.page(0).unwrap()),
        Err(Error::IncompleteOutput(_))
    ));
    let (content, diagnostics) = doc
        .page_content_with_diagnostics(&doc.page(0).unwrap())
        .unwrap();
    assert_eq!(diagnostics.decode_failures, 1);
    assert!(String::from_utf8_lossy(&content).contains("SURVIVES"));
    let page = zpdf::interpret_page(&doc, 0, Default::default()).unwrap();
    assert!(!page.is_complete());
    assert_eq!(page.stats.diagnostics.decode_failures, 1);
    let converted = zpdf::convert_pdf(&doc, &[0], Default::default()).unwrap();
    assert!(converted.pages[0].text.contains("SURVIVES"));
    assert_eq!(converted.pages[0].diagnostics.decode_failures, 1);
    assert_eq!(converted.pages[0].diagnostics.first_affected_page, Some(0));
    assert!(converted.pages[0].diagnostics.first_failed_object.is_some());
    assert!(converted
        .diagnostics()
        .to_json()
        .contains("\"complete\":false"));
    assert!(converted.ensure_complete().is_err());
}

#[test]
fn limits_malformed_tokens_and_unknown_operators_are_reported() {
    let rect = Rect::new(0.0, 0.0, 100.0, 100.0);
    let (_, stats) = ContentInterpreter::new(rect)
        .with_command_limit(1)
        .interpret_with_stats(b"0 0 1 1 re f 2 2 1 1 re f");
    assert!(stats.diagnostics.resource_limit_reached);
    let (_, stats) = ContentInterpreter::new(rect)
        .with_operand_stack_limit(2)
        .interpret_with_stats(b"0 0 1 1 re f");
    assert!(stats.diagnostics.resource_limit_reached);
    let (_, stats) = ContentInterpreter::new(rect)
        .with_deadline(zpdf_core::time::Instant::now())
        .interpret_with_stats(&b"q Q ".repeat(32));
    assert!(stats.diagnostics.time_limit_reached);
    let (_, stats) = ContentInterpreter::new(rect).interpret_with_stats(b"(unterminated");
    assert!(stats.diagnostics.malformed_content > 0);
    let (_, stats) = ContentInterpreter::new(rect).interpret_with_stats(b"vendor_operator");
    assert_eq!(stats.diagnostics.unsupported_features, 1);
    let (_, stats) =
        ContentInterpreter::new(rect).interpret_with_stats(b"BX vendor_operator EX 0 0 1 1 re f");
    assert_eq!(stats.diagnostics, ProcessingDiagnostics::default());
}

#[test]
fn malformed_xobjects_are_reported_but_hidden_draws_are_not_failures() {
    let mut builder = DocumentBuilder::new();
    builder.add_page(100.0, 100.0);
    let mut writer = IncrementalWriter::new(builder.build().unwrap()).unwrap();
    let page = writer.document().page(0).unwrap();
    let (n, g) = writer.try_add_object(&PdfObject::Integer(42)).unwrap();
    let bad = zpdf::ObjectId(n, g as u16);
    let mut objects = PdfDict::new();
    objects.insert(PdfName::new("Bad"), PdfObject::Ref(bad));
    let mut resources = PdfDict::new();
    resources.insert(PdfName::new("XObject"), PdfObject::Dict(objects));
    let mut dict = writer
        .resolve_current(page.id)
        .unwrap()
        .as_dict()
        .unwrap()
        .clone();
    dict.insert(PdfName::new("Resources"), PdfObject::Dict(resources));
    writer.overwrite_object(page.id, PdfObject::Dict(dict));
    let mut bytes = std::io::Cursor::new(Vec::new());
    writer.write(&mut bytes).unwrap();
    let doc = PdfDocument::open(bytes.into_inner()).unwrap();
    let page = doc.page(0).unwrap();
    let (_, stats) = ContentInterpreter::new(page.effective_box())
        .with_document(doc.file(), &page.resources)
        .interpret_with_stats(b"/Bad Do");
    assert!(!stats.diagnostics.is_complete());
    assert_eq!(stats.diagnostics.first_failed_object, Some(bad));
    let oc = zpdf::OcConfig::default();
    let (_, stats) = ContentInterpreter::new(page.effective_box())
        .with_document(doc.file(), &page.resources)
        .with_optional_content(&oc)
        .interpret_with_stats(
            b"/OC << /Type /OCMD /VE [/Not << /Type /OCG >>] >> BDC /Bad Do /Missing Do EMC",
        );
    assert!(stats.diagnostics.is_complete(), "{:?}", stats.diagnostics);
}
