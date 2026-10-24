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
