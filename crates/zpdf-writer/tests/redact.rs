//! End-to-end redaction: text under the region must vanish from extraction,
//! text outside must survive.

use std::io::Cursor;
use zpdf_core::Rect;
use zpdf_document::PdfDocument;
use zpdf_writer::redact::RedactOptions;
use zpdf_writer::{redact_pdf, DocumentBuilder, IncrementalWriter};

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

#[test]
fn destructive_redaction_removes_original_bytes() {
    let out = redact_pdf(
        two_line_pdf(),
        b"",
        0,
        &[Rect::new(60.0, 690.0, 400.0, 720.0)],
        &RedactOptions::default(),
    )
    .expect("redact");

    // The original incremental update must not be present in the output: the
    // secret may only survive if an old object revision was left behind.
    assert!(
        !out.windows(b"TOP SECRET".len()).any(|w| w == b"TOP SECRET"),
        "unredacted bytes survived the destructive rewrite"
    );

    let doc = PdfDocument::open(out).expect("open");
    let content = page_content(&doc);
    assert!(!content.contains("TOP SECRET"));
    assert!(content.contains("public information"));
}

/// Lay out a PDF from per-object body strings (object N+1 = `objects[N]`).
fn build_pdf(objects: &[String]) -> Vec<u8> {
    let mut data = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(data.len());
        data.extend_from_slice(format!("{} 0 obj\n{}\nendobj\n", i + 1, body).as_bytes());
    }
    let xref = data.len();
    data.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
    data.extend_from_slice(b"0000000000 65535 f \n");
    for off in &offsets {
        data.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    data.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    data
}

fn stream(dict: &str, data: &str) -> String {
    format!(
        "<< {dict} /Length {} >>\nstream\n{data}\nendstream",
        data.len()
    )
}

#[test]
fn inline_annotations_in_redacted_region_leave_no_reachable_contents() {
    for indirect_array in [false, true] {
        let annots = "[<< /Type /Annot /Subtype /Text /Rect [10 10 20 20] \
            /Contents (INLINE_ANNOT_SECRET) >> \
            << /Type /Annot /Subtype /Text /Rect [200 200 220 220] \
            /Contents (inline public annotation) >>]";
        let pdf = build_pdf(&[
            "<< /Type /Catalog /Pages 2 0 R >>".into(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] \
                 /Contents 4 0 R /Annots {} >>",
                if indirect_array { "5 0 R" } else { annots }
            ),
            stream("", "10 10 5 5 re f"),
            annots.into(),
        ]);
        let out = redact_pdf(
            pdf,
            b"",
            0,
            &[Rect::new(0.0, 0.0, 30.0, 30.0)],
            &RedactOptions { fill: None },
        )
        .unwrap();
        let document = PdfDocument::open(out).unwrap();
        let payload = String::from_utf8_lossy(&all_payload_bytes(&document)).into_owned();
        assert!(!payload.contains("INLINE_ANNOT_SECRET"));
        assert!(payload.contains("inline public annotation"));
    }
}

#[test]
fn form_redaction_uses_bbox_and_matrix_instead_of_the_image_unit_square() {
    let pdf = build_pdf(&[
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 400] /Contents 4 0 R \
         /Resources << /XObject << /Secret 5 0 R /Public 6 0 R >> >> >>"
            .into(),
        stream("", "q 2 0 0 2 10 20 cm /Secret Do Q /Public Do"),
        stream(
            "/Type /XObject /Subtype /Form /BBox [0 0 100 80] /Matrix [1 0 0 1 30 40]",
            "% SECRET_FORM_PAYLOAD\n0 0 100 80 re f",
        ),
        stream(
            "/Type /XObject /Subtype /Form /BBox [300 300 350 350]",
            "% PUBLIC_FORM_PAYLOAD\n300 300 50 50 re f",
        ),
    ]);
    // The form paints [70,100,270,260]; the image-unit-square guess [10,20,12,22]
    // misses this region entirely.
    let out = redact_pdf(
        pdf,
        b"",
        0,
        &[Rect::new(100.0, 120.0, 150.0, 160.0)],
        &RedactOptions { fill: None },
    )
    .unwrap();
    let doc = PdfDocument::open(out).unwrap();
    let payload = String::from_utf8_lossy(&all_payload_bytes(&doc)).into_owned();
    assert!(!payload.contains("SECRET_FORM_PAYLOAD"));
    assert!(payload.contains("PUBLIC_FORM_PAYLOAD"));
    let content = page_content(&doc);
    assert!(!content.contains("/Secret Do"));
    assert!(content.contains("/Public Do"));
}

/// Every string, name and decoded stream payload reachable in the file.
fn all_payload_bytes(doc: &PdfDocument) -> Vec<u8> {
    fn walk(obj: &zpdf_core::PdfObject, out: &mut Vec<u8>) {
        use zpdf_core::PdfObject;
        match obj {
            PdfObject::String(s) => out.extend_from_slice(&s.0),
            PdfObject::Name(n) => out.extend_from_slice(n.as_str().as_bytes()),
            PdfObject::Array(a) => a.iter().for_each(|o| walk(o, out)),
            PdfObject::Dict(d) => d.0.values().for_each(|o| walk(o, out)),
            PdfObject::Stream(s) => s.dict.0.values().for_each(|o| walk(o, out)),
            _ => {}
        }
        out.push(b'\n');
    }
    let file = doc.file();
    let mut out = Vec::new();
    for id in file.all_object_ids() {
        if let Ok(obj) = file.resolve(id) {
            walk(&obj, &mut out);
            if matches!(obj, zpdf_core::PdfObject::Stream(_)) {
                out.extend(file.resolve_stream_data(id).unwrap_or_default());
            }
        }
    }
    out
}

#[test]
fn destructive_redaction_leaves_no_reachable_copy() {
    let content = "/P <</MCID 0 /ActualText (SECRET-ACTUALTEXT)>> BDC \
                   BT /F1 12 Tf 1 0 0 1 72 700 Tm (CLASSIFIED) Tj ET EMC\n\
                   q 100 0 0 100 72 300 cm /Im1 Do Q\n\
                   BT /F1 12 Tf 1 0 0 1 72 100 Tm (public information) Tj ET\n";
    let objects = vec![
        "<< /Type /Catalog /Pages 2 0 R /AcroForm << /Fields [6 0 R] >> \
         /StructTreeRoot 8 0 R /MarkInfo << /Marked true >> >>"
            .to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
         /Resources << /Font << /F1 4 0 R >> /XObject << /Im1 5 0 R >> >> \
         /Contents 7 0 R /Annots [6 0 R] /StructParents 0 >>"
            .to_string(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        stream(
            "/Type /XObject /Subtype /Image /Width 16 /Height 1 \
             /ColorSpace /DeviceGray /BitsPerComponent 8",
            "SECRET-IMAGE-PIX",
        ),
        "<< /Type /Annot /Subtype /Widget /FT /Tx /T (field) /V (SECRET-VALUE) \
         /Rect [60 600 300 620] /P 3 0 R /AP << /N 9 0 R >> >>"
            .to_string(),
        stream("", content),
        "<< /Type /StructTreeRoot /K [10 0 R 11 0 R] >>".to_string(),
        stream(
            "/Type /XObject /Subtype /Form /BBox [0 0 240 20]",
            "BT /F1 12 Tf (SECRET-APPEARANCE) Tj ET",
        ),
        "<< /Type /StructElem /S /P /P 8 0 R /Pg 3 0 R /K 0 \
         /ActualText (SECRET-STRUCT) /Alt (SECRET-ALT) >>"
            .to_string(),
        // The widget also participates in the structure tree, which keeps it
        // reachable unless the annotation itself is neutralized.
        "<< /Type /StructElem /S /Form /P 8 0 R /Pg 3 0 R \
         /K << /Type /OBJR /Obj 6 0 R >> /Alt (SECRET-FORM-ALT) >>"
            .to_string(),
    ];
    let out = redact_pdf(
        build_pdf(&objects),
        b"",
        0,
        &[
            Rect::new(60.0, 690.0, 400.0, 720.0),
            Rect::new(60.0, 290.0, 200.0, 410.0),
            Rect::new(50.0, 590.0, 310.0, 630.0),
        ],
        &RedactOptions::default(),
    )
    .expect("redact");

    let doc = PdfDocument::open(out).expect("open");
    let payload = all_payload_bytes(&doc);
    for secret in [
        "CLASSIFIED",
        "SECRET-ACTUALTEXT",
        "SECRET-IMAGE-PIX",
        "SECRET-VALUE",
        "SECRET-APPEARANCE",
        "SECRET-STRUCT",
        "SECRET-ALT",
        "SECRET-FORM-ALT",
    ] {
        assert!(
            !payload
                .windows(secret.len())
                .any(|w| w == secret.as_bytes()),
            "{secret} survived the destructive redaction"
        );
    }
    assert!(page_content(&doc).contains("public information"));
    let catalog = doc
        .file()
        .resolve(doc.file().trailer.get_ref("Root").unwrap());
    let form = catalog.unwrap().as_dict().unwrap().get("AcroForm").cloned();
    let fields = match form {
        Some(zpdf_core::PdfObject::Dict(f)) => f.get("Fields").cloned(),
        other => panic!("AcroForm kept: {other:?}"),
    };
    assert_eq!(
        fields,
        Some(zpdf_core::PdfObject::Array(Vec::new())),
        "the removed widget is unlinked from /Fields"
    );
}

/// Copies of redacted content that live outside the content stream and the
/// redacted annotations: the page thumbnail, a named marked-content property
/// list in `/Resources /Properties`, and the value held by a parent field
/// whose only widget was redacted.
#[test]
fn redaction_drops_thumbnail_named_properties_and_emptied_fields() {
    let content = "/Span /MC0 BDC BT /F1 12 Tf 1 0 0 1 72 700 Tm (HIDDEN) Tj ET EMC\n\
                   /Span /MC1 BDC BT /F1 12 Tf 1 0 0 1 72 100 Tm (public) Tj ET EMC\n";
    let widget = |parent: u32, y: u32| {
        format!(
            "<< /Type /Annot /Subtype /Widget /Parent {parent} 0 R \
             /Rect [60 {y} 300 {}] /P 3 0 R >>",
            y + 20
        )
    };
    let objects = vec![
        "<< /Type /Catalog /Pages 2 0 R /AcroForm << /Fields [6 0 R 9 0 R] >> >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
         /Resources << /Font << /F1 4 0 R >> /Properties << \
         /MC0 << /ActualText (SECRET-NAMED-PROPS) >> /MC1 << /ActualText (KEPT-PROPS) >> >> >> \
         /Contents 5 0 R /Annots [8 0 R 10 0 R 11 0 R] /Thumb 12 0 R >>"
            .to_string(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        stream("", content),
        // gp > p > widget 8: redacting the widget empties p, then gp.
        "<< /T (gp) /Kids [7 0 R] >>".to_string(),
        "<< /T (p) /FT /Tx /V (SECRET-PARENT-VALUE) /DV (SECRET-PARENT-DEFAULT) \
         /Parent 6 0 R /Kids [8 0 R] >>"
            .to_string(),
        widget(7, 600),
        // q keeps widget 11 outside the regions, so it keeps its value.
        "<< /T (q) /FT /Tx /V (KEPT-VALUE) /Kids [10 0 R 11 0 R] >>".to_string(),
        widget(9, 560),
        widget(9, 40),
        stream(
            "/Width 16 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8",
            "SECRET-THUMB-PIX",
        ),
    ];
    let out = redact_pdf(
        build_pdf(&objects),
        b"",
        0,
        &[
            Rect::new(60.0, 690.0, 400.0, 720.0),
            Rect::new(50.0, 590.0, 310.0, 630.0),
            Rect::new(50.0, 550.0, 310.0, 585.0),
        ],
        &RedactOptions::default(),
    )
    .expect("redact");

    let doc = PdfDocument::open(out).expect("open");
    let payload = all_payload_bytes(&doc);
    let has = |s: &str| payload.windows(s.len()).any(|w| w == s.as_bytes());
    for secret in [
        "HIDDEN",
        "SECRET-NAMED-PROPS",
        "SECRET-PARENT-VALUE",
        "SECRET-PARENT-DEFAULT",
        "SECRET-THUMB-PIX",
    ] {
        assert!(!has(secret), "{secret} survived the destructive redaction");
    }
    for kept in ["public", "KEPT-PROPS", "KEPT-VALUE"] {
        assert!(has(kept), "{kept} was removed");
    }

    let file = doc.file();
    let catalog = file.resolve(file.trailer.get_ref("Root").unwrap()).unwrap();
    let form = catalog.as_dict().unwrap().get("AcroForm").cloned();
    let Some(zpdf_core::PdfObject::Dict(form)) = form else {
        panic!("AcroForm kept: {form:?}");
    };
    let fields = form.get_array("Fields").expect("fields");
    assert_eq!(fields.len(), 1, "only q is left: {fields:?}");
    let q = file.resolve(fields[0].as_ref().unwrap()).unwrap();
    assert_eq!(
        q.as_dict().unwrap().get_array("Kids").unwrap().len(),
        1,
        "q keeps its unredacted widget"
    );
}

/// Moving a shared inherited table must remove its unused redacted entries,
/// while retaining direct and Form-mediated uses on another page.
#[test]
fn inherited_resources_are_detached_without_losing_surviving_uses() {
    for second_use in [
        "",
        "/Tile Do /Span /Caption BDC 200 200 10 10 re f EMC",
        "/Wrapper Do",
    ] {
        let objects = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 /Resources << /Font << /F1 5 0 R >> /XObject << /Tile 6 0 R /Wrapper 10 0 R >> /Properties << /Caption 7 0 R >> >> >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 8 0 R >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 9 0 R >>".to_string(),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
            stream("/Type /XObject /Subtype /Form /BBox [0 0 10 10] /Resources << >>", "% test-tile-data\n0 0 10 10 re f"),
            "<< /ActualText (test-caption-data) /Lang (en) >>".to_string(),
            stream("", "/Tile Do /Span /Caption BDC 10 10 10 10 re f EMC"),
            stream("", &format!("BT /F1 12 Tf 200 200 Td (public page) Tj ET {second_use}")),
            stream("/Type /XObject /Subtype /Form /BBox [0 0 10 10]", "/Tile Do /Span /Caption BDC 200 200 10 10 re f EMC"),
        ];
        let output = redact_pdf(
            build_pdf(&objects),
            b"",
            0,
            &[Rect::new(-1.0, -1.0, 100.0, 100.0)],
            &RedactOptions { fill: None },
        )
        .unwrap();
        let document = PdfDocument::open(output).unwrap();
        let payload = all_payload_bytes(&document);
        for marker in ["test-tile-data", "test-caption-data"] {
            assert_eq!(
                payload
                    .windows(marker.len())
                    .any(|bytes| bytes == marker.as_bytes()),
                !second_use.is_empty()
            );
        }
        let catalog = document
            .file()
            .resolve(document.file().trailer.get_ref("Root").unwrap())
            .unwrap();
        let pages = document
            .file()
            .resolve(catalog.as_dict().unwrap().get_ref("Pages").unwrap())
            .unwrap();
        assert!(pages.as_dict().unwrap().get("Resources").is_none());
        assert!(!page_content(&document).contains("/Tile Do"));
        let other_page = document.page(1).unwrap();
        assert!(
            String::from_utf8_lossy(&document.page_content_bytes(&other_page).unwrap())
                .contains("public page")
        );
    }
}

#[test]
fn supported_structure_child_shapes_lose_only_related_replacement_text() {
    let cases = [
        (
            "<< /S /P /ActualText (test-inline) /K 0 >>",
            "9 0 R",
            "<< /S /P /Alt (test-descendant) /K 0 >>",
        ),
        ("<< /MCID 0.0 >>", "<< /MCID 0.0 >>", "null"),
        ("9 0 R", "9 0 R", "<< /Type /MCR /MCID 0 >>"),
        ("9 0 R", "9 0 R", "[<< /MCID 0.0 >>]"),
        ("<< /Obj 11 0 R >>", "9 0 R", "<< /Obj 11 0 R >>"),
    ];
    for (first, second, shared) in cases {
        let objects = vec![
            "<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 6 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R /Annots [11 0 R] >>".to_string(),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
            stream("", "/P <</MCID 0>> BDC BT /F1 12 Tf 20 20 Td (selected text) Tj ET EMC /P <</MCID 1>> BDC BT /F1 12 Tf 200 200 Td (public text) Tj ET EMC"),
            "<< /Type /StructTreeRoot /Pg 3 0 R /K [7 0 R 8 0 R 10 0 R] >>".to_string(),
            format!("<< /Type /StructElem /S /P /P 6 0 R /ActualText (test-owner-one) /K {first} >>"),
            format!("<< /Type /StructElem /S /P /P 6 0 R /Alt (test-owner-two) /K {second} >>"),
            shared.to_string(),
            "<< /Type /StructElem /S /P /P 6 0 R /ActualText (public structure) /K 1 >>".to_string(),
            "<< /Type /Annot /Subtype /Text /Rect [10 10 50 50] /P 3 0 R >>".to_string(),
        ];
        let original = build_pdf(&objects);
        assert_eq!(
            PdfDocument::open(original.clone())
                .unwrap()
                .struct_tree()
                .unwrap()
                .children
                .len(),
            3
        );
        let output = redact_pdf(
            original,
            b"",
            0,
            &[Rect::new(0.0, 0.0, 180.0, 60.0)],
            &RedactOptions { fill: None },
        )
        .unwrap();
        let document = PdfDocument::open(output).unwrap();
        assert!(!page_content(&document).contains("selected text"));
        let payload = String::from_utf8_lossy(&all_payload_bytes(&document)).into_owned();
        for removed in [
            "test-owner-one",
            "test-owner-two",
            "test-inline",
            "test-descendant",
        ] {
            assert!(
                !payload.contains(removed),
                "{removed} survived for {first}, {second}, {shared}"
            );
        }
        assert!(payload.contains("public structure"));
        assert!(page_content(&document).contains("public text"));
    }
}

#[test]
fn tagging_and_redaction_compose_in_both_orders() {
    for tag_first in [false, true] {
        let mut writer = IncrementalWriter::new(two_line_pdf()).unwrap();
        if tag_first {
            writer.tag_pdf().unwrap();
        }
        writer
            .redact_page(
                0,
                &[Rect::new(60.0, 690.0, 400.0, 720.0)],
                &RedactOptions { fill: None },
            )
            .unwrap();
        if !tag_first {
            writer.tag_pdf().unwrap();
        }
        // A second tag call must see the pending MarkInfo too.
        writer.tag_pdf().unwrap();
        let mut update = Cursor::new(Vec::new());
        writer.write(&mut update).unwrap();
        let current = PdfDocument::open(update.into_inner()).unwrap();
        let output = zpdf_writer::rewrite_pdf(current.file(), &Default::default()).unwrap();
        let document = PdfDocument::open(output).unwrap();
        let payload = all_payload_bytes(&document);
        assert!(!payload
            .windows(b"TOP SECRET".len())
            .any(|bytes| bytes == b"TOP SECRET"));
        assert!(page_content(&document).contains("public information"));
        let tree = document.struct_tree().unwrap();
        assert_eq!(tree.children.len(), 1);
        if !tag_first {
            assert!(tree.children[0]
                .alt
                .as_deref()
                .unwrap()
                .contains("public information"));
        }
    }
}

/// Retained stored AP streams are independent roots for page-resource use.
/// The other page deliberately has no Contents, and an overlapping annotation
/// is removed so its appearance cannot keep the unrelated Gone resource alive.
#[test]
fn retained_annotation_appearances_preserve_inherited_resource_uses() {
    for same_page in [false, true] {
        for appearances in [
            "/N 12 0 R",
            "/N << /Off 13 0 R /On 12 0 R >>",
            "/N 13 0 R /R << /Hover 12 0 R >>",
            "/N 13 0 R /D 12 0 R",
        ] {
            let first_annots = if same_page { "10 0 R 11 0 R" } else { "11 0 R" };
            let second_annots = if same_page { "" } else { "10 0 R" };
            let objects = vec![
                "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
                "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 /Resources << /XObject << /Tile 5 0 R /Gone 6 0 R >> /Properties << /Caption 7 0 R /GoneCaption 8 0 R >> >> >>".to_string(),
                format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 9 0 R /Annots [{first_annots}] >>"),
                format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Annots [{second_annots}] >>"),
                stream("/Type /XObject /Subtype /Form /BBox [0 0 10 10] /Resources << >>", "% retained-ap-tile\n0 0 10 10 re f"),
                stream("/Type /XObject /Subtype /Form /BBox [0 0 10 10] /Resources << >>", "% removed-ap-tile\n0 0 10 10 re f"),
                "<< /ActualText (retained-ap-caption) >>".to_string(),
                "<< /ActualText (removed-ap-caption) >>".to_string(),
                stream("", "/Tile Do /Gone Do /P /Caption BDC 10 10 2 2 re f EMC /P /GoneCaption BDC 10 10 2 2 re f EMC"),
                format!("<< /Type /Annot /Subtype /Stamp /P {} 0 R /Rect [200 200 220 220] /AS /Off /AP << {appearances} >> >>", if same_page { 3 } else { 4 }),
                "<< /Type /Annot /Subtype /Stamp /P 3 0 R /Rect [10 10 20 20] /AP << /N 14 0 R >> >>".to_string(),
                stream("/Type /XObject /Subtype /Form /BBox [0 0 10 10]", "/Tile Do /P /Caption BDC 0 0 10 10 re f EMC"),
                stream("/Type /XObject /Subtype /Form /BBox [0 0 10 10]", ""),
                stream("/Type /XObject /Subtype /Form /BBox [0 0 10 10]", "/Gone Do /P /GoneCaption BDC 0 0 10 10 re f EMC"),
            ];
            let output = redact_pdf(
                build_pdf(&objects),
                b"",
                0,
                &[Rect::new(-1.0, -1.0, 100.0, 100.0)],
                &RedactOptions { fill: None },
            )
            .unwrap();
            let document = PdfDocument::open(output).unwrap();
            let annotated_page = document.page(if same_page { 0 } else { 1 }).unwrap();
            assert_eq!(annotated_page.annots.len(), 1);
            assert!(annotated_page.resources.xobjects.contains_key("Tile"));
            assert!(!annotated_page.resources.xobjects.contains_key("Gone"));
            assert!(!page_content(&document).contains("/Tile Do"));
            let payload = String::from_utf8_lossy(&all_payload_bytes(&document)).into_owned();
            assert!(payload.contains("retained-ap-tile"));
            assert!(payload.contains("retained-ap-caption"));
            assert!(!payload.contains("removed-ap-tile"));
            assert!(!payload.contains("removed-ap-caption"));
        }
    }
}

/// lookup_res falls back to the page for missing names in an own resource
/// table. Empty tables are supported; partial overrides must fail before edits
/// until a context-aware dependency walk can prove the surviving uses.
#[test]
fn empty_form_resources_preserve_fallback_and_partial_resources_fail_without_edits() {
    for annotation in [false, true] {
        for own_resources in ["<< >>", "<< /XObject << /Other 5 0 R >> >>"] {
            let second_page = if annotation {
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Annots [9 0 R] >>"
            } else {
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 8 0 R >>"
            };
            let objects = vec![
                "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
                "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 /Resources << /XObject << /Tile 5 0 R /Wrapper 6 0 R >> >> >>".to_string(),
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 7 0 R >>".to_string(),
                second_page.to_string(),
                stream("/Type /XObject /Subtype /Form /BBox [0 0 10 10] /Resources << >>", "% fallback-tile-payload\n0 0 10 10 re f"),
                stream(&format!("/Type /XObject /Subtype /Form /BBox [0 0 10 10] /Resources {own_resources}"), "/Tile Do"),
                stream("", "/Tile Do"),
                stream("", "/Wrapper Do"),
                "<< /Type /Annot /Subtype /Stamp /P 4 0 R /Rect [200 200 220 220] /AP << /N 6 0 R >> >>".to_string(),
            ];
            let mut writer = IncrementalWriter::new(build_pdf(&objects)).unwrap();
            let mut before = Cursor::new(Vec::new());
            writer.write(&mut before).unwrap();
            let result = writer.redact_page(
                0,
                &[Rect::new(-1.0, -1.0, 100.0, 100.0)],
                &RedactOptions { fill: None },
            );
            let mut after = Cursor::new(Vec::new());
            writer.write(&mut after).unwrap();
            if own_resources == "<< >>" {
                result.unwrap();
                let current = PdfDocument::open(after.into_inner()).unwrap();
                let output = zpdf_writer::rewrite_pdf(current.file(), &Default::default()).unwrap();
                let document = PdfDocument::open(output).unwrap();
                assert!(document
                    .page(1)
                    .unwrap()
                    .resources
                    .xobjects
                    .contains_key("Tile"));
                assert!(String::from_utf8_lossy(&all_payload_bytes(&document))
                    .contains("fallback-tile-payload"));
            } else {
                assert!(result.is_err());
                assert_eq!(
                    after.into_inner(),
                    before.into_inner(),
                    "unsupported ownership must not queue partial edits"
                );
            }
        }
    }
}

#[test]
fn ext_gstate_type3_selection_refuses_ambiguous_cleanup_before_edits() {
    for font_entry in ["[9 0 R 12]", "11 0 R"] {
        let objects = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 /Resources << /XObject << /Tile 5 0 R >> /ExtGState << /GS 6 0 R >> >> >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 7 0 R >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 8 0 R >>".to_string(),
            stream("/Type /XObject /Subtype /Form /BBox [0 0 10 10] /Resources << >>", "0 0 10 10 re f"),
            format!("<< /Font {font_entry} >>"),
            stream("", "/Tile Do"),
            stream("", "BT /GS gs (A) Tj ET"),
            "<< /Type /Font /Subtype /Type3 /FontBBox [0 0 10 10] /FontMatrix [1 0 0 1 0 0] /CharProcs << /A 10 0 R >> /Encoding << /Differences [65 /A] >> /FirstChar 65 /LastChar 65 /Widths [10] >>".to_string(),
            stream("", "10 0 d0 /Tile Do"),
            "[9 0 R 12]".to_string(),
        ];
        let mut writer = IncrementalWriter::new(build_pdf(&objects)).unwrap();
        let mut before = Cursor::new(Vec::new());
        writer.write(&mut before).unwrap();
        assert!(writer
            .redact_page(
                0,
                &[Rect::new(-1.0, -1.0, 100.0, 100.0)],
                &RedactOptions { fill: None }
            )
            .is_err());
        let mut after = Cursor::new(Vec::new());
        writer.write(&mut after).unwrap();
        assert_eq!(after.into_inner(), before.into_inner());
    }
}

/// The unused Wrapper can keep removed roots reachable without executing a
/// single Do. Its resources may be direct, indirect, cyclic, or ref aliases.
fn unused_wrapper_pdf(
    inherited: bool,
    resources: &str,
    removed_alias: bool,
    alias_target: u32,
    second_use: &str,
) -> Vec<u8> {
    let resources_location = if inherited { " /Resources 10 0 R" } else { "" };
    let page_resources = if inherited { "" } else { " /Resources 10 0 R" };
    let kids = if inherited { "3 0 R 4 0 R" } else { "3 0 R" };
    let secret = if removed_alias && alias_target == 5 {
        13
    } else {
        5
    };
    let caption = if removed_alias && alias_target == 9 {
        13
    } else {
        9
    };
    build_pdf(&[
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        format!("<< /Type /Pages /Kids [{kids}] /Count {}{resources_location} >>", if inherited { 2 } else { 1 }),
        format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 7 0 R{page_resources} >>"),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 8 0 R >>".into(),
        stream("/Type /XObject /Subtype /Form /BBox [0 0 10 10]", "% removed-form-payload\n0 0 10 10 re f"),
        stream(&format!("/Type /XObject /Subtype /Form /BBox [200 200 210 210] /Resources {resources}"), "% retained-wrapper-payload\n"),
        stream("", "/Secret Do /Span /Caption BDC 10 10 10 10 re f EMC"),
        stream("", second_use),
        "<< /ActualText (removed-property-payload) >>".into(),
        format!("<< /XObject << /Secret {secret} 0 R /Wrapper 6 0 R >> /Properties << /Caption {caption} 0 R >> >>"),
        "<< /XObject 12 0 R >>".into(),
        "<< /Loop 6 0 R /Hidden 5 0 R >>".into(),
        format!("{alias_target} 0 R"),
        stream("/Type /XObject /Subtype /Form /BBox [200 200 210 210]", "% unrelated-resource-payload\n200 200 10 10 re f"),
    ])
}

/// Both safe outcomes are allowed: complete graph cleanup, or an error before
/// queuing any edits. A successful rewrite retaining either marker is a leak.
fn assert_no_removed_resource_after_redaction(bytes: Vec<u8>) {
    let mut writer = IncrementalWriter::new(bytes).unwrap();
    let mut before = Cursor::new(Vec::new());
    writer.write(&mut before).unwrap();
    let result = writer.redact_page(
        0,
        &[Rect::new(-1.0, -1.0, 100.0, 100.0)],
        &RedactOptions { fill: None },
    );
    let mut after = Cursor::new(Vec::new());
    writer.write(&mut after).unwrap();
    if result.is_err() {
        assert_eq!(
            before.into_inner(),
            after.into_inner(),
            "a refused graph cleanup must not queue edits"
        );
        return;
    }
    let current = PdfDocument::open(after.into_inner()).unwrap();
    let output = zpdf_writer::rewrite_pdf(current.file(), &Default::default()).unwrap();
    let document = PdfDocument::open(output).unwrap();
    let payload = String::from_utf8_lossy(&all_payload_bytes(&document)).into_owned();
    for marker in ["removed-form-payload", "removed-property-payload"] {
        assert!(
            !payload.contains(marker),
            "unused resource graph retained {marker}"
        );
    }
}

#[test]
fn unused_wrapper_resource_graph_cannot_keep_redacted_payloads() {
    for inherited in [false, true] {
        for resources in [
            "<< /XObject << /Hidden 5 0 R >> >>",
            "<< /Properties << /Hidden 9 0 R >> >>",
            "<< /XObject 12 0 R >>",
            "11 0 R",
        ] {
            assert_no_removed_resource_after_redaction(unused_wrapper_pdf(
                inherited, resources, false, 5, "",
            ));
        }
    }
}

#[test]
fn unused_wrapper_resource_graph_detects_both_ref_alias_directions() {
    for alias_target in [5, 9] {
        let category = if alias_target == 5 {
            "XObject"
        } else {
            "Properties"
        };
        for removed_alias in [false, true] {
            let hidden = if removed_alias { alias_target } else { 13 };
            let resources = format!("<< /{category} << /Hidden {hidden} 0 R >> >>");
            assert_no_removed_resource_after_redaction(unused_wrapper_pdf(
                true,
                &resources,
                removed_alias,
                alias_target,
                "",
            ));
        }
    }
}

#[test]
fn unrelated_cyclic_resource_graph_keeps_surviving_sibling_uses() {
    for second_use in ["", "/Secret Do /Span /Caption BDC 200 200 10 10 re f EMC"] {
        let bytes = unused_wrapper_pdf(
            true,
            "<< /XObject << /Loop 6 0 R /Public 14 0 R >> >>",
            false,
            5,
            second_use,
        );
        let output = redact_pdf(
            bytes,
            b"",
            0,
            &[Rect::new(-1.0, -1.0, 100.0, 100.0)],
            &RedactOptions { fill: None },
        )
        .unwrap();
        let document = PdfDocument::open(output).unwrap();
        let payload = String::from_utf8_lossy(&all_payload_bytes(&document)).into_owned();
        assert!(payload.contains("retained-wrapper-payload"));
        assert!(payload.contains("unrelated-resource-payload"));
        for marker in ["removed-form-payload", "removed-property-payload"] {
            assert_eq!(
                payload.contains(marker),
                !second_use.is_empty(),
                "sibling use of {marker} changed"
            );
        }
        assert!(!page_content(&document).contains("/Secret Do"));
    }
}

/// The retained wrapper refers to a descendant rather than the removed root.
/// alias_side: 0 = direct, 1 = removed edge aliases, 2 = retained edge aliases.
fn unused_wrapper_descendant_pdf(property: bool, alias_side: u8) -> Vec<u8> {
    let removed = if alias_side == 1 { 9 } else { 8 };
    let retained = if alias_side == 2 { 9 } else { 8 };
    let (content, secret, wrapper_resources, caption, descendant) = if property {
        (
            "/Span /Caption BDC 10 10 10 10 re f EMC".to_string(),
            stream("/Type /XObject /Subtype /Form /BBox [0 0 10 10]", ""),
            "<< /Properties << /Unrelated 10 0 R >> >>".to_string(),
            format!("<< /ActualText {removed} 0 R >>"),
            "(removed-property-payload)".to_string(),
        )
    } else {
        (
            "/Secret Do".to_string(),
            stream(
                &format!("/Type /XObject /Subtype /Form /BBox [0 0 10 10] /Resources << /XObject << /Inside {removed} 0 R >> >>"),
                "/Inside Do",
            ),
            format!("<< /XObject << /Unrelated {retained} 0 R >> >>"),
            "<< >>".to_string(),
            stream(
                "/Type /XObject /Subtype /Image /Width 20 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8",
                "removed-form-payload",
            ),
        )
    };
    build_pdf(&[
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 /Resources << /XObject << /Secret 5 0 R /Wrapper 6 0 R >> /Properties << /Caption 7 0 R >> >> >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R >>".into(),
        stream("", &content),
        secret,
        stream(&format!("/Type /XObject /Subtype /Form /BBox [200 200 210 210] /Resources {wrapper_resources}"), "% retained-wrapper-payload\n"),
        caption,
        descendant,
        "8 0 R".into(),
        // Different owning dictionaries, so comparing only root fingerprints
        // cannot discover the shared string in this descendant object.
        format!("<< /ActualText {retained} 0 R /Lang (en) >>"),
    ])
}

#[test]
fn unused_wrapper_resource_graph_cannot_keep_descendant_images() {
    for alias_side in [0, 1, 2] {
        assert_no_removed_resource_after_redaction(unused_wrapper_descendant_pdf(
            false, alias_side,
        ));
    }
}

#[test]
fn unused_wrapper_resource_graph_cannot_keep_descendant_property_strings() {
    for alias_side in [0, 1, 2] {
        assert_no_removed_resource_after_redaction(unused_wrapper_descendant_pdf(true, alias_side));
    }
}

#[test]
fn resource_graph_allows_ordinary_inline_values_and_empty_resources() {
    let output = redact_pdf(
        unused_wrapper_pdf(true, "<< >>", false, 5, ""),
        b"",
        0,
        &[Rect::new(-1.0, -1.0, 100.0, 100.0)],
        &RedactOptions { fill: None },
    )
    .unwrap();
    let document = PdfDocument::open(output).unwrap();
    let payload = String::from_utf8_lossy(&all_payload_bytes(&document)).into_owned();
    assert!(!payload.contains("removed-form-payload"));
    assert!(!payload.contains("removed-property-payload"));
    assert!(payload.contains("retained-wrapper-payload"));
}
