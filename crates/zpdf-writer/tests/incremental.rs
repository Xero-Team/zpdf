use std::io::Cursor;
use zpdf_core::PdfObject;
use zpdf_document::{InkAnnotationBuilder, PdfDocument};
use zpdf_writer::IncrementalWriter;

/// Create a minimal single-page PDF in memory for testing.
fn minimal_pdf() -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(b"%PDF-1.4\n");
    data.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
    data.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
    data.extend_from_slice(
        b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>\nendobj\n",
    );
    data.extend_from_slice(b"xref\n0 4\n");
    data.extend_from_slice(b"0000000000 65535 f \n");
    data.extend_from_slice(b"0000000009 00000 n \n");
    data.extend_from_slice(b"0000000058 00000 n \n");
    data.extend_from_slice(b"0000000117 00000 n \n");
    data.extend_from_slice(b"trailer\n<< /Size 4 /Root 1 0 R >>\n");
    data.extend_from_slice(b"startxref\n187\n%%EOF\n");
    data
}

#[test]
fn round_trip_ink_annotation() {
    // 1. Create a minimal PDF.
    let original = minimal_pdf();

    // 2. Build an ink annotation.
    let mut builder = InkAnnotationBuilder::new();
    builder.set_color(1.0, 0.0, 0.0); // red
    builder.set_width(2.0);
    builder.add_stroke(vec![(100.0, 200.0), (150.0, 250.0), (200.0, 200.0)]);
    let (annot_dict, appearance) = builder.build().expect("build");

    // 3. Write the annotation to the PDF.
    let mut writer = IncrementalWriter::new(original.clone()).expect("new writer");
    writer
        .add_ink_annotation_to_page(0, &annot_dict, &appearance)
        .expect("add annotation");

    let mut output = Cursor::new(Vec::new());
    writer.write(&mut output).expect("write");
    let modified = output.into_inner();

    // 4. Parse the modified PDF and verify the annotation exists.
    let doc = PdfDocument::open(modified.as_slice()).expect("parse modified");
    let page = doc.page(0).expect("page 0");

    // The page should now have annotation references.
    assert_eq!(page.annots.len(), 1, "page should have 1 annotation");

    // We could verify the annotation contents here, but that would require
    // resolving the annotation dict and checking its fields. For now, just
    // confirm the annotation was added.
}

#[test]
fn modified_pdf_preserves_original_objects() {
    let original = minimal_pdf();
    let mut builder = InkAnnotationBuilder::new();
    builder.add_stroke(vec![(50.0, 50.0), (100.0, 100.0)]);
    let (annot_dict, appearance) = builder.build().expect("build");

    let mut writer = IncrementalWriter::new(original.clone()).expect("writer");
    writer
        .add_ink_annotation_to_page(0, &annot_dict, &appearance)
        .expect("add");

    let mut output = Cursor::new(Vec::new());
    writer.write(&mut output).expect("write");
    let modified = output.into_inner();

    // The modified PDF should start with the original bytes.
    assert!(modified.starts_with(&original[..50]));

    // It should contain a new xref section and trailer.
    let s = String::from_utf8_lossy(&modified);
    assert!(s.contains("xref"), "should have new xref");
    assert!(
        s.contains("/Prev"),
        "new trailer should reference original xref"
    );
}

#[test]
fn writer_assigns_sequential_object_numbers() {
    let original = minimal_pdf();
    let mut writer = IncrementalWriter::new(original).expect("writer");

    // The original has objects 1, 2, 3 and /Size 4, so next should be 4.
    let (num1, gen1) = writer.add_object(&PdfObject::Integer(42));
    assert_eq!(num1, 4);
    assert_eq!(gen1, 0);

    let (num2, gen2) = writer.add_object(&PdfObject::Integer(99));
    assert_eq!(num2, 5);
    assert_eq!(gen2, 0);
}
