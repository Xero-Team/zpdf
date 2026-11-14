//! Tagged-PDF authoring round-trip: build a document with tagged text/image
//! items, then read it back through `PdfDocument::struct_tree()` and confirm
//! the structure tree, roles, MCIDs, and `/Alt` survive — and that the document
//! declares itself tagged (`/MarkInfo /Marked true`).

use zpdf_document::{PdfDocument, StructKid, StructRole};
use zpdf_writer::{DocumentBuilder, ImageData, IncrementalWriter, TagSpec};

#[test]
fn tagged_pdf_round_trips_through_structure_tree() {
    let mut builder = DocumentBuilder::new();
    builder.set_lang("en");
    let page = builder.add_page(612.0, 792.0);
    builder
        .add_tagged_text(
            page,
            "Title",
            72.0,
            700.0,
            "Helvetica",
            24.0,
            (0.0, 0.0, 0.0),
            TagSpec {
                role: StructRole::H1,
                alt: None,
                actual_text: None,
            },
        )
        .unwrap();
    builder
        .add_tagged_text(
            page,
            "Body paragraph.",
            72.0,
            660.0,
            "Helvetica",
            12.0,
            (0.0, 0.0, 0.0),
            TagSpec {
                role: StructRole::P,
                alt: None,
                actual_text: None,
            },
        )
        .unwrap();
    builder
        .add_tagged_image(
            page,
            ImageData::Rgb8 {
                width: 1,
                height: 1,
                pixels: vec![0, 0, 0],
            },
            72.0,
            600.0,
            40.0,
            40.0,
            TagSpec {
                role: StructRole::Figure,
                alt: Some("A chart".to_string()),
                actual_text: None,
            },
        )
        .unwrap();

    let pdf = builder.build().expect("build");
    let body = String::from_utf8_lossy(&pdf);
    assert!(body.contains("/StructTreeRoot"), "missing StructTreeRoot");
    assert!(
        body.contains("/Marked true"),
        "missing /MarkInfo /Marked true"
    );
    assert!(body.contains("/StructParents"), "missing /StructParents");
    assert!(body.contains("/Lang"), "missing /Lang");

    let doc = PdfDocument::open(pdf.to_vec()).expect("open");
    assert!(doc.is_tagged(), "document should declare itself tagged");
    let tree = doc.struct_tree().expect("struct tree").clone();
    assert!(
        tree.marked,
        "/MarkInfo /Marked true should surface as marked"
    );

    // Three top-level elements, in the order they were tagged, each carrying
    // its MCID kid.
    assert_eq!(tree.children.len(), 3, "expected 3 top-level elements");
    let h1 = &tree.children[0];
    assert_eq!(h1.role, StructRole::H1);
    assert_eq!(h1.page, Some(0));
    assert_eq!(
        h1.kids,
        vec![StructKid::MarkedContent {
            page: Some(0),
            mcid: 0
        }]
    );
    let p = &tree.children[1];
    assert_eq!(p.role, StructRole::P);
    assert_eq!(
        p.kids,
        vec![StructKid::MarkedContent {
            page: Some(0),
            mcid: 1
        }]
    );
    let fig = &tree.children[2];
    assert_eq!(fig.role, StructRole::Figure);
    assert_eq!(fig.alt.as_deref(), Some("A chart"));
    assert_eq!(
        fig.kids,
        vec![StructKid::MarkedContent {
            page: Some(0),
            mcid: 2
        }]
    );
}

#[test]
fn untagged_pdf_has_no_struct_tree() {
    let mut builder = DocumentBuilder::new();
    let page = builder.add_page(200.0, 200.0);
    builder
        .add_text(
            page,
            "plain",
            10.0,
            10.0,
            "Helvetica",
            12.0,
            (0.0, 0.0, 0.0),
        )
        .unwrap();
    let pdf = builder.build().expect("build");
    let body = String::from_utf8_lossy(&pdf);
    assert!(
        !body.contains("/StructTreeRoot"),
        "untagged PDF must not emit a tree"
    );
    let doc = PdfDocument::open(pdf.to_vec()).expect("open");
    assert!(!doc.is_tagged());
    assert!(doc.struct_tree().is_none());
}

#[test]
fn tagged_document_passes_pdfua_validator() {
    // A realistically-tagged document: a heading, a paragraph, and a figure
    // with alt text, a declared language, and every page tagged. This must
    // conform to the PDF/UA-1 validator the writer's output is designed for.
    let mut builder = DocumentBuilder::new();
    builder.set_lang("en-US");
    let page = builder.add_page(612.0, 792.0);
    builder
        .add_tagged_text(
            page,
            "Heading",
            72.0,
            700.0,
            "Helvetica",
            24.0,
            (0.0, 0.0, 0.0),
            TagSpec {
                role: StructRole::H1,
                alt: None,
                actual_text: None,
            },
        )
        .unwrap();
    builder
        .add_tagged_text(
            page,
            "A paragraph of body text.",
            72.0,
            660.0,
            "Helvetica",
            12.0,
            (0.0, 0.0, 0.0),
            TagSpec {
                role: StructRole::P,
                alt: None,
                actual_text: None,
            },
        )
        .unwrap();
    builder
        .add_tagged_image(
            page,
            ImageData::Rgb8 {
                width: 2,
                height: 2,
                pixels: vec![0, 0, 0, 255, 255, 255, 255, 255, 255, 0, 0, 0],
            },
            72.0,
            600.0,
            40.0,
            40.0,
            TagSpec {
                role: StructRole::Figure,
                alt: Some("A two-by-two checkerboard".to_string()),
                actual_text: None,
            },
        )
        .unwrap();

    let pdf = builder.build().expect("build");
    let doc = PdfDocument::open(pdf.to_vec()).expect("open");
    let report = zpdf_document::pdfua::validate(doc.file(), zpdf_document::pdfua::Profile::Ua1);
    assert!(
        report.conforms(),
        "expected PDF/UA-1 conformance, got: {:?}",
        report
            .violations
            .iter()
            .map(|v| format!("[{}] {}", v.rule, v.message))
            .collect::<Vec<_>>()
    );
}

// ---- IncrementalWriter::tag_pdf (coarse retagging of an existing PDF) -----

/// Build an untagged two-page PDF (no /StructTreeRoot, no /MarkInfo) with text
/// on each page, then retag it with `IncrementalWriter::tag_pdf`. The result
/// must declare itself tagged, carry a `/StructTreeRoot` with one `/Part`
/// element per page (each wrapping MCID 0), and the per-page `/Alt` should
/// carry the extracted page text.
#[test]
fn incremental_tag_pdf_adds_per_page_part_structure() {
    // An untagged builder PDF: two pages, plain (untagged) text on each.
    let font_bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../crates/zpdf-font/tests/fixtures/var.ttf"
    ))
    .expect("read var.ttf");
    let mut builder = DocumentBuilder::new();
    let p0 = builder.add_page(400.0, 200.0);
    let p1 = builder.add_page(400.0, 200.0);
    let font = builder.embed_font(font_bytes).expect("embed");
    builder
        .add_text_embedded(p0, "Hello", 40.0, 100.0, font, 24.0, (0.0, 0.0, 0.0))
        .unwrap();
    builder
        .add_text_embedded(p1, "World", 40.0, 100.0, font, 24.0, (0.0, 0.0, 0.0))
        .unwrap();
    let original = builder.build().expect("build");
    let body = String::from_utf8_lossy(&original);
    assert!(
        !body.contains("/StructTreeRoot"),
        "precondition: builder PDF is untagged"
    );

    // Retag via an incremental update.
    let mut writer = IncrementalWriter::new(original.clone()).expect("open for incremental");
    writer.tag_pdf().expect("tag");
    let mut out = Vec::new();
    writer
        .write(&mut std::io::Cursor::new(&mut out))
        .expect("write");
    let tagged = out;

    let out_body = String::from_utf8_lossy(&tagged);
    assert!(
        out_body.contains("/StructTreeRoot"),
        "retagged PDF has a tree"
    );
    assert!(
        out_body.contains("/Marked true"),
        "retagged PDF declares /MarkInfo /Marked true"
    );
    assert!(
        out_body.contains("/StructParents"),
        "page carries /StructParents"
    );

    let doc = PdfDocument::open(tagged.to_vec()).expect("open retagged");
    assert!(doc.is_tagged(), "is_tagged() true after retagging");
    let tree = doc
        .struct_tree()
        .expect("struct tree present after retagging")
        .clone();
    assert!(tree.marked, "/MarkInfo /Marked true surfaces as marked");
    // One /Part element per page, each wrapping MCID 0.
    assert_eq!(tree.children.len(), 2, "one /Part element per page");
    for elem in &tree.children {
        assert_eq!(elem.role, StructRole::Part, "coarse retag uses /Part");
        assert_eq!(
            elem.kids,
            vec![StructKid::MarkedContent {
                page: elem.page,
                mcid: 0
            }],
            "each /Part wraps a single MCID-0 sequence"
        );
    }
}

/// `tag_pdf` is a no-op on an already-tagged document (does not re-wrap or
/// duplicate the structure tree).
#[test]
fn tag_pdf_is_noop_on_already_tagged() {
    let mut builder = DocumentBuilder::new();
    builder.set_lang("en");
    let page = builder.add_page(200.0, 200.0);
    builder
        .add_tagged_text(
            page,
            "Title",
            10.0,
            100.0,
            "Helvetica",
            12.0,
            (0.0, 0.0, 0.0),
            TagSpec {
                role: StructRole::H1,
                alt: None,
                actual_text: None,
            },
        )
        .unwrap();
    let tagged = builder.build().expect("build");
    let original_len = tagged.len();

    let mut writer = IncrementalWriter::new(tagged.clone()).expect("open");
    writer.tag_pdf().expect("tag (no-op)");
    let mut out = Vec::new();
    writer
        .write(&mut std::io::Cursor::new(&mut out))
        .expect("write");

    // The update appended nothing beyond the minimal incremental scaffolding:
    // no new /StructTreeRoot should appear in the added region. The simplest
    // stable check is that the document still parses, is tagged, and has the
    // original single H1 element (not duplicated into a /Part).
    let doc = PdfDocument::open(out.to_vec()).expect("open");
    assert!(doc.is_tagged());
    let tree = doc.struct_tree().expect("tree").clone();
    assert_eq!(tree.children.len(), 1, "no duplicate /Part added");
    assert_eq!(tree.children[0].role, StructRole::H1);
    // The output is the original plus an incremental update (so strictly larger),
    // but no tagging objects were added — sanity-check it didn't shrink.
    assert!(out.len() >= original_len);
}

#[test]
fn pending_tagging_snapshot_preserves_export_encryption() {
    use zpdf_writer::{rewrite_pdf, EncryptionConfig, RewriteOptions};
    let mut builder = DocumentBuilder::new();
    let page = builder.add_page(200.0, 200.0);
    builder
        .add_text(
            page,
            "private test text",
            10.0,
            100.0,
            "Helvetica",
            12.0,
            (0.0, 0.0, 0.0),
        )
        .unwrap();
    let original = PdfDocument::open(builder.build().unwrap()).unwrap();
    let encrypted = rewrite_pdf(
        original.file(),
        &RewriteOptions {
            encrypt: Some(EncryptionConfig::rc4_128("test-user", "test-owner")),
            ..Default::default()
        },
    )
    .unwrap();
    let mut writer = IncrementalWriter::new_with_password(encrypted, b"test-user").unwrap();
    writer.tag_pdf().unwrap();
    let mut output = std::io::Cursor::new(Vec::new());
    writer.write(&mut output).unwrap();
    let output = output.into_inner();
    assert!(PdfDocument::open(output.clone()).is_err());
    assert!(!output
        .windows(b"private test text".len())
        .any(|bytes| bytes == b"private test text"));
    let document = PdfDocument::open_with_password(output, b"test-user").unwrap();
    assert!(document.is_encrypted());
    assert!(document.struct_tree().unwrap().children[0]
        .alt
        .as_deref()
        .unwrap()
        .contains("private test text"));
}

/// A pending append preserves the source's /Contents representation. Tagging
/// must flatten an indirect array before adding its prefix/suffix streams.
#[test]
fn appended_page_tagging_preserves_all_contents_representations() {
    use std::io::Cursor;
    use zpdf_core::PdfObject;
    use zpdf_parser::PdfFile;

    let first = b"BT /F1 12 Tf 10 100 Td (First fragment) Tj ET\n";
    let second = b"BT /F1 12 Tf 10 80 Td (Second fragment) Tj ET\n";
    let stream = |bytes: &[u8]| {
        let mut object = format!("<< /Length {} >>\nstream\n", bytes.len()).into_bytes();
        object.extend_from_slice(bytes);
        object.extend_from_slice(b"\nendstream");
        object
    };
    for (contents, expected_streams) in [("4 0 R", 1), ("[4 0 R 5 0 R]", 2), ("6 0 R", 2)] {
        let objects = [
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] \
                 /Resources << /Font << /F1 7 0 R >> >> /Contents {contents} >>"
            )
            .into_bytes(),
            stream(first),
            stream(second),
            b"[4 0 R 5 0 R]".to_vec(),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
        ];
        let mut source = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(source.len());
            source.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
            source.extend_from_slice(object);
            source.extend_from_slice(b"\nendobj\n");
        }
        let xref = source.len();
        source.extend_from_slice(b"xref\n0 8\n0000000000 65535 f \n");
        for offset in offsets {
            source.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        source.extend_from_slice(
            format!("trailer\n<< /Size 8 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
        );
        let original = PdfDocument::open(source.clone()).unwrap();
        let original_content = original
            .page_content_bytes(&original.page(0).unwrap())
            .unwrap();

        let mut base = DocumentBuilder::new();
        base.add_page(200.0, 200.0);
        let mut writer = IncrementalWriter::new(base.build().unwrap()).unwrap();
        writer
            .append_document_pages(&PdfFile::parse(source).unwrap())
            .unwrap();
        writer.tag_pdf().unwrap();
        let mut output = Cursor::new(Vec::new());
        writer.write(&mut output).unwrap();
        let document = PdfDocument::open(output.into_inner()).unwrap();
        assert_eq!(document.page_count(), 2);
        assert!(document.is_tagged());
        let page = document.page(1).unwrap();
        let content = document
            .page_content_bytes(&page)
            .unwrap_or_else(|error| panic!("{contents}: tagged content failed: {error}"));
        assert!(content.starts_with(b"q /Part <</MCID 0>> BDC\n"));
        assert!(content.ends_with(b"\nEMC Q\n"));
        assert!(content
            .windows(original_content.len())
            .any(|part| part == original_content));
        assert_eq!(page.contents.len(), expected_streams + 2);
        for id in &page.contents {
            assert!(matches!(
                document.file().resolve(*id).unwrap(),
                PdfObject::Stream(_)
            ));
        }
        let tree = document.struct_tree().unwrap();
        assert_eq!(tree.children.len(), 2);
        let alt = tree.children[1].alt.as_deref().unwrap();
        assert!(alt.contains("First fragment"));
        assert_eq!(alt.contains("Second fragment"), expected_streams == 2);
    }
}
