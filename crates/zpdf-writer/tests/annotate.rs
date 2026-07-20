use std::io::Cursor;
use zpdf_core::Rect;
use zpdf_document::PdfDocument;
use zpdf_writer::{AnnotationSpec, IncrementalWriter, MarkupKind};

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

fn add_and_reopen(spec: AnnotationSpec) -> PdfDocument {
    let mut writer = IncrementalWriter::new(minimal_pdf()).expect("writer");
    writer.add_annotation(0, &spec).expect("add annotation");
    let mut out = Cursor::new(Vec::new());
    writer.write(&mut out).expect("write");
    PdfDocument::open(out.into_inner()).expect("open")
}

/// The /Contents text of the page's first annotation, decoded from its raw
/// dict (UTF-16BE BOM or plain bytes).
fn first_annot_contents(doc: &PdfDocument) -> Option<String> {
    let page = doc.page(0).expect("page");
    let annot_ref = *page.annots.first()?;
    let dict = doc.file().resolve(annot_ref).ok()?.as_dict().ok()?.clone();
    let s = match dict.get("Contents") {
        Some(zpdf_core::PdfObject::String(s)) => s.0.clone(),
        _ => return None,
    };
    if s.starts_with(&[0xFE, 0xFF]) {
        let units: Vec<u16> = s[2..]
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16(&units).ok()
    } else {
        Some(String::from_utf8_lossy(&s).into_owned())
    }
}

#[test]
fn highlight_from_rects_roundtrip() {
    let doc = add_and_reopen(AnnotationSpec::markup_from_rects(
        MarkupKind::Highlight,
        &[Rect::new(100.0, 700.0, 300.0, 715.0)],
        (1.0, 1.0, 0.0),
        Some("important".to_string()),
    ));
    let page = doc.page(0).expect("page");
    let annots = doc.page_annotations(&page);
    assert_eq!(annots.len(), 1);
    let a = &annots[0];
    assert_eq!(a.subtype, "Highlight");
    assert_eq!(first_annot_contents(&doc).as_deref(), Some("important"));
    // The renderer synthesizes an appearance for /AP-less markup.
    assert!(
        a.appearance.is_some() || a.generated.is_some(),
        "highlight should get a synthesized appearance"
    );
}

#[test]
fn note_annotation_roundtrip() {
    let doc = add_and_reopen(AnnotationSpec::Note {
        x: 50.0,
        y: 750.0,
        contents: "多语言 note ✓".to_string(),
        color: Some((0.0, 0.5, 1.0)),
        icon: Some("Comment".to_string()),
    });
    let page = doc.page(0).expect("page");
    let annots = doc.page_annotations(&page);
    assert_eq!(annots.len(), 1);
    assert_eq!(annots[0].subtype, "Text");
    // Non-ASCII must survive the UTF-16BE round trip.
    assert_eq!(first_annot_contents(&doc).as_deref(), Some("多语言 note ✓"));
}

#[test]
fn square_circle_line_roundtrip() {
    for (spec, expected) in [
        (
            AnnotationSpec::Square {
                rect: Rect::new(10.0, 10.0, 110.0, 60.0),
                color: (1.0, 0.0, 0.0),
                interior: Some((1.0, 0.9, 0.9)),
                width: 2.0,
            },
            "Square",
        ),
        (
            AnnotationSpec::Circle {
                rect: Rect::new(10.0, 100.0, 110.0, 160.0),
                color: (0.0, 0.6, 0.0),
                interior: None,
                width: 1.5,
            },
            "Circle",
        ),
        (
            AnnotationSpec::Line {
                x1: 20.0,
                y1: 200.0,
                x2: 200.0,
                y2: 260.0,
                color: (0.0, 0.0, 1.0),
                width: 3.0,
            },
            "Line",
        ),
    ] {
        let doc = add_and_reopen(spec);
        let page = doc.page(0).expect("page");
        let annots = doc.page_annotations(&page);
        assert_eq!(annots.len(), 1);
        assert_eq!(annots[0].subtype, expected);
        assert!(
            annots[0].appearance.is_some() || annots[0].generated.is_some(),
            "{expected} should get a synthesized appearance"
        );
    }
}

#[test]
fn free_text_roundtrip() {
    let doc = add_and_reopen(AnnotationSpec::FreeText {
        rect: Rect::new(100.0, 400.0, 350.0, 460.0),
        contents: "Boxed remark".to_string(),
        size: Some(14.0),
        color: Some((0.2, 0.2, 0.2)),
    });
    let page = doc.page(0).expect("page");
    let annots = doc.page_annotations(&page);
    assert_eq!(annots.len(), 1);
    assert_eq!(annots[0].subtype, "FreeText");
    assert_eq!(first_annot_contents(&doc).as_deref(), Some("Boxed remark"));
}

#[test]
fn multiple_annotations_accumulate() {
    let mut writer = IncrementalWriter::new(minimal_pdf()).expect("writer");
    for i in 0..3 {
        writer
            .add_annotation(
                0,
                &AnnotationSpec::Note {
                    x: 50.0 + 30.0 * i as f64,
                    y: 700.0,
                    contents: format!("note {i}"),
                    color: None,
                    icon: None,
                },
            )
            .expect("add");
    }
    let mut out = Cursor::new(Vec::new());
    writer.write(&mut out).expect("write");
    let doc = PdfDocument::open(out.into_inner()).expect("open");
    let page = doc.page(0).expect("page");
    assert_eq!(doc.page_annotations(&page).len(), 3);
}

#[test]
fn empty_quads_rejected() {
    let mut writer = IncrementalWriter::new(minimal_pdf()).expect("writer");
    let err = writer.add_annotation(
        0,
        &AnnotationSpec::Markup {
            kind: MarkupKind::Underline,
            quads: vec![],
            color: (0.0, 0.0, 0.0),
            contents: None,
        },
    );
    assert!(err.is_err());
}

#[test]
fn non_finite_quads_rejected() {
    let mut writer = IncrementalWriter::new(minimal_pdf()).expect("writer");
    let err = writer.add_annotation(
        0,
        &AnnotationSpec::Markup {
            kind: MarkupKind::Highlight,
            quads: vec![[0.0, 0.0, f64::NAN, 0.0, 0.0, 0.0, 0.0, 0.0]],
            color: (0.0, 0.0, 0.0),
            contents: None,
        },
    );
    assert!(err.is_err());
}

/// Authored annotations must carry a baked /AP /N appearance stream so they
/// render in viewers that never synthesize appearances.
#[test]
fn authored_annotations_carry_appearance_streams() {
    use zpdf_core::PdfObject;

    let specs = vec![
        AnnotationSpec::markup_from_rects(
            MarkupKind::Highlight,
            &[Rect::new(50.0, 700.0, 200.0, 715.0)],
            (1.0, 1.0, 0.0),
            None,
        ),
        AnnotationSpec::Square {
            rect: Rect::new(100.0, 500.0, 250.0, 600.0),
            color: (1.0, 0.0, 0.0),
            interior: Some((1.0, 0.9, 0.9)),
            width: 2.0,
        },
        AnnotationSpec::Line {
            x1: 50.0,
            y1: 400.0,
            x2: 300.0,
            y2: 450.0,
            color: (0.0, 0.0, 1.0),
            width: 1.5,
        },
        AnnotationSpec::Note {
            x: 400.0,
            y: 700.0,
            contents: "sticky".into(),
            color: None,
            icon: None,
        },
        AnnotationSpec::FreeText {
            rect: Rect::new(50.0, 300.0, 300.0, 350.0),
            contents: "free text body".into(),
            size: Some(12.0),
            color: Some((0.0, 0.0, 0.0)),
        },
    ];

    for spec in specs {
        let doc = add_and_reopen(spec.clone());
        let page = doc.page(0).expect("page");
        let annot_ref = *page.annots.first().expect("annot present");
        let dict = doc
            .file()
            .resolve(annot_ref)
            .expect("resolve annot")
            .as_dict()
            .expect("dict")
            .clone();
        let subtype = match dict.get("Subtype") {
            Some(PdfObject::Name(n)) => n.as_str().to_string(),
            _ => String::new(),
        };
        let ap = dict
            .get("AP")
            .unwrap_or_else(|| panic!("{subtype}: /AP missing"));
        let ap_dict = match ap {
            PdfObject::Dict(d) => d.clone(),
            PdfObject::Ref(r) => doc
                .file()
                .resolve(*r)
                .expect("resolve AP")
                .as_dict()
                .expect("AP dict")
                .clone(),
            other => panic!("{subtype}: /AP has wrong type: {other:?}"),
        };
        let n_ref = match ap_dict.get("N") {
            Some(PdfObject::Ref(r)) => *r,
            other => panic!("{subtype}: /AP /N must be a stream ref, got {other:?}"),
        };
        let content = doc
            .file()
            .resolve_stream_data(n_ref)
            .unwrap_or_else(|e| panic!("{subtype}: /N stream unreadable: {e}"));
        assert!(
            !content.is_empty(),
            "{subtype}: appearance stream must not be empty"
        );
    }
}
