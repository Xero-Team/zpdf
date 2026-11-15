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
fn retained_image_with_inactive_dp_allows_redaction() {
    let original = build_pdf(&[
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /Resources << /XObject << /Secret 5 0 R /Public 6 0 R >> >> >>".into(),
        stream("", "/Secret Do q 1 0 0 1 200 200 cm /Public Do Q"),
        stream("/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8", "removed-image-bytes"),
        stream("/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8 /DecodeParms << >> /DP 7 0 R", "public-image-bytes"),
        "(ignored malformed DP)".into(),
    ]);
    let output = redact_pdf(
        original,
        b"",
        0,
        &[Rect::new(-1.0, -1.0, 100.0, 100.0)],
        &RedactOptions { fill: None },
    )
    .unwrap();
    let document = PdfDocument::open(output).unwrap();
    let payload = String::from_utf8_lossy(&all_payload_bytes(&document)).into_owned();
    assert!(!payload.contains("removed-image-bytes"));
    assert!(payload.contains("public-image-bytes"));
    assert!(page_content(&document).contains("/Public Do"));
}

#[test]
fn inactive_dp_cannot_prove_removed_descendant_is_used() {
    let original = build_pdf(&[
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /Resources << /XObject << /Secret 5 0 R /Public 6 0 R >> >> >>".into(),
        stream("", "/Secret Do q 1 0 0 1 200 200 cm /Public Do Q"),
        stream("/Type /XObject /Subtype /Form /BBox [0 0 10 10] /Resources << /XObject << /Inside 7 0 R >> >>", "/Inside Do"),
        stream("/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8 /DecodeParms << >> /DP << /JBIG2Globals 7 0 R >>", "public-image-bytes"),
        stream("/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8", "removed-form-payload"),
    ]);
    assert_no_removed_resource_after_redaction(original);
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

const ORIGINAL_CONTENT_SECRET: &str = "original-content-secret";
const ORIGINAL_CONTENT_TEXT: &str = "BT /F1 12 Tf 20 20 Td (original-content-secret) Tj ET \
    BT /F1 12 Tf 200 200 Td (public text) Tj ET";

/// Objects 1..5 are the catalog, page tree, page, original content and font.
fn original_content_alias_pdf(
    contents: &str,
    resources: &str,
    annots: &str,
    extras: &[String],
) -> Vec<u8> {
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] \
             /Contents {contents} /Resources << /Font << /F1 5 0 R >> {resources} >> \
             {annots} >>"
        ),
        stream("/Subtype /Form /BBox [0 0 300 300]", ORIGINAL_CONTENT_TEXT),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
    ];
    objects.extend_from_slice(extras);
    build_pdf(&objects)
}

/// Retaining an original content stream must fail atomically. Record the
/// decoded leak too, so a regression explains the unsafe successful result.
fn assert_original_content_alias_refused(mut writer: IncrementalWriter) {
    use zpdf_core::{ObjectId, PdfName, PdfObject, PdfString};

    let mut page = writer
        .resolve_current(ObjectId(3, 0))
        .unwrap()
        .as_dict()
        .unwrap()
        .clone();
    page.insert(
        PdfName::new("PriorEdit"),
        PdfObject::String(PdfString(b"keep prior edit".to_vec())),
    );
    writer.overwrite_object(ObjectId(3, 0), PdfObject::Dict(page));
    let mut before = Cursor::new(Vec::new());
    writer.write(&mut before).unwrap();
    let result = writer.redact_page(
        0,
        &[Rect::new(0.0, 0.0, 190.0, 60.0)],
        &RedactOptions { fill: None },
    );
    let mut after = Cursor::new(Vec::new());
    writer.write(&mut after).unwrap();
    if result.is_ok() {
        let current = PdfDocument::open(after.into_inner()).unwrap();
        let rewritten = zpdf_writer::rewrite_pdf(current.file(), &Default::default()).unwrap();
        let rewritten = PdfDocument::open(rewritten).unwrap();
        let payload = all_payload_bytes(&rewritten);
        let leaked = payload
            .windows(ORIGINAL_CONTENT_SECRET.len())
            .any(|value| value == ORIGINAL_CONTENT_SECRET.as_bytes());
        panic!(
            "retained original content must be refused; decoded secret survived rewrite: {leaked}"
        );
    }
    assert_eq!(
        before.into_inner(),
        after.into_inner(),
        "refusal changed pending edits or allocation state"
    );
}

#[test]
fn text_redaction_refuses_original_content_in_unused_resources() {
    for (resources, extras) in [
        ("/XObject << /Backup 4 0 R >>", vec![]),
        (
            "/XObject << /Wrapper 6 0 R >>",
            vec![
                stream(
                    "/Subtype /Form /BBox [200 200 220 220] /Resources 7 0 R",
                    "",
                ),
                "<< /XObject << /Backup 8 0 R >> /Loop 6 0 R >>".into(),
                "9 0 R".into(),
                "4 0 R".into(),
            ],
        ),
    ] {
        let pdf = original_content_alias_pdf("4 0 R", resources, "", &extras);
        assert_original_content_alias_refused(IncrementalWriter::new(pdf).unwrap());
    }
}

#[test]
fn text_redaction_refuses_original_content_in_retained_annotations() {
    for alias in ["/AP << /N 4 0 R >>", "/Backup 4 0 R"] {
        let pdf = original_content_alias_pdf(
            "4 0 R",
            "",
            "/Annots [6 0 R]",
            &[format!(
                "<< /Type /Annot /Subtype /Text /P 3 0 R /Rect [220 220 240 240] {alias} >>"
            )],
        );
        assert_original_content_alias_refused(IncrementalWriter::new(pdf).unwrap());
    }
}

#[test]
fn text_redaction_tracks_all_original_content_streams_and_aliases() {
    for contents in ["[6 0 R 4 0 R]", "7 0 R", "8 0 R"] {
        let pdf = original_content_alias_pdf(
            contents,
            "/XObject << /Backup 4 0 R >>",
            "",
            &[
                stream("", "BT /F1 12 Tf 200 240 Td (another public line) Tj ET"),
                "[6 0 R 4 0 R]".into(),
                "9 0 R".into(),
                "4 0 R".into(),
            ],
        );
        assert_original_content_alias_refused(IncrementalWriter::new(pdf).unwrap());
    }
}

#[test]
fn text_redaction_refuses_aliases_to_pending_content() {
    use zpdf_core::{ObjectId, PdfDict, PdfName, PdfObject};

    for resources in [
        "/XObject << /Backup 7 0 R >>",
        "/XObject << /Backup 6 0 R >>",
    ] {
        // The source alias initially dangles. It starts retaining content only
        // after the writer allocates the pending stream numbered 7.
        let pdf = original_content_alias_pdf("4 0 R", resources, "", &["7 0 R".into()]);
        let mut writer = IncrementalWriter::new(pdf).unwrap();
        let pending = writer.add_stream(&PdfDict::new(), ORIGINAL_CONTENT_TEXT.as_bytes());
        assert_eq!(pending, (7, 0));
        let mut page = writer
            .resolve_current(ObjectId(3, 0))
            .unwrap()
            .as_dict()
            .unwrap()
            .clone();
        page.insert(
            PdfName::new("Contents"),
            PdfObject::Ref(ObjectId(pending.0, pending.1 as u16)),
        );
        writer.overwrite_object(ObjectId(3, 0), PdfObject::Dict(page));
        assert_original_content_alias_refused(writer);
    }
}

#[test]
fn text_redaction_reads_pending_targets_inside_source_content_aliases() {
    use zpdf_core::{ObjectId, PdfDict, PdfObject};

    for indirect_array in [false, true] {
        let pdf = original_content_alias_pdf("6 0 R", "", "", &["7 0 R".into(), "4 0 R".into()]);
        let mut writer = IncrementalWriter::new(pdf).unwrap();
        let pending = writer.add_stream(&PdfDict::new(), b"BT /F1 12 Tf 20 20 Td (pending selected) Tj ET BT /F1 12 Tf 200 200 Td (pending public) Tj ET");
        assert_eq!(pending, (8, 0));
        let target = PdfObject::Ref(ObjectId(pending.0, pending.1 as u16));
        writer.overwrite_object(
            ObjectId(7, 0),
            if indirect_array {
                PdfObject::Array(vec![target])
            } else {
                target
            },
        );
        writer
            .redact_page(
                0,
                &[Rect::new(0.0, 0.0, 190.0, 60.0)],
                &RedactOptions { fill: None },
            )
            .unwrap();
        let mut updated = Cursor::new(Vec::new());
        writer.write(&mut updated).unwrap();
        let current = PdfDocument::open(updated.into_inner()).unwrap();
        let output = zpdf_writer::rewrite_pdf(current.file(), &Default::default()).unwrap();
        let document = PdfDocument::open(output).unwrap();
        let payload = String::from_utf8_lossy(&all_payload_bytes(&document)).into_owned();
        assert!(!payload.contains("pending selected"));
        assert!(!payload.contains(ORIGINAL_CONTENT_SECRET));
        assert!(page_content(&document).contains("pending public"));
        assert!(!page_content(&document).contains("public text"));
    }
}

#[test]
fn text_redaction_tracks_pending_contents_through_a_page_alias() {
    use zpdf_core::{ObjectId, PdfDict, PdfName, PdfObject};

    let pdf = build_pdf(&[
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Kids [6 0 R] /Count 1 >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> /Annots [7 0 R] >>".into(),
        stream("", "BT /F1 12 Tf 200 200 Td (source public) Tj ET"),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
        "3 0 R".into(),
        "<< /Type /Annot /Subtype /Text /P 3 0 R /Rect [220 220 240 240] /Contents (public annotation) >>".into(),
    ]);
    let mut writer = IncrementalWriter::new(pdf).unwrap();
    let pending = writer.add_stream(&PdfDict::new(), ORIGINAL_CONTENT_TEXT.as_bytes());
    assert_eq!(pending, (8, 0));
    let mut page = writer
        .resolve_current(ObjectId(3, 0))
        .unwrap()
        .as_dict()
        .unwrap()
        .clone();
    page.insert(
        PdfName::new("Contents"),
        PdfObject::Ref(ObjectId(pending.0, pending.1 as u16)),
    );
    writer.overwrite_object(ObjectId(3, 0), PdfObject::Dict(page));
    assert_original_content_alias_refused(writer);
}

#[test]
fn text_redaction_refuses_original_content_shared_with_a_sibling() {
    let pdf = build_pdf(&[
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Kids [3 0 R 6 0 R] /Count 2 /Resources << /Font << /F1 5 0 R >> >> >>"
            .into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R >>".into(),
        stream("", ORIGINAL_CONTENT_TEXT),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R >>".into(),
    ]);
    assert_original_content_alias_refused(IncrementalWriter::new(pdf).unwrap());
}

#[test]
fn text_redaction_accepts_removed_aliases_and_retained_page_backrefs() {
    let pdf = original_content_alias_pdf(
        "4 0 R", "", "/Annots [6 0 R 7 0 R]",
        &[
            "<< /Type /Annot /Subtype /Text /P 3 0 R /Rect [10 10 30 30] /AP << /N 4 0 R >> >>".into(),
            "<< /Type /Annot /Subtype /Text /P 8 0 R /Rect [220 220 240 240] /Contents (public annotation) >>".into(),
            "3 0 R".into(),
        ],
    );
    let output = redact_pdf(
        pdf,
        b"",
        0,
        &[Rect::new(0.0, 0.0, 190.0, 60.0)],
        &RedactOptions { fill: None },
    )
    .unwrap();
    let document = PdfDocument::open(output).unwrap();
    let payload = String::from_utf8_lossy(&all_payload_bytes(&document)).into_owned();
    assert!(!payload.contains(ORIGINAL_CONTENT_SECRET));
    assert!(payload.contains("public annotation"));
    assert!(page_content(&document).contains("public text"));
}

#[test]
fn text_redaction_accepts_type3_pattern_and_soft_mask_resources() {
    let pdf = build_pdf(&[
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Kids [3 0 R 12 0 R] /Count 2 /Resources << /Font << /F1 5 0 R /F3 6 0 R >> /Pattern << /P1 8 0 R >> /ExtGState << /GS 9 0 R >> >> >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R >>".into(),
        stream("", &format!("{ORIGINAL_CONTENT_TEXT} BT /F3 12 Tf 220 250 Td (A) Tj ET q /GS gs /Pattern cs /P1 scn 220 220 10 10 re f Q")),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
        "<< /Type /Font /Subtype /Type3 /FontBBox [0 0 10 10] /FontMatrix [1 0 0 1 0 0] /CharProcs << /A 7 0 R >> /Encoding << /Differences [65 /A] >> /FirstChar 65 /LastChar 65 /Widths [10] >>".into(),
        stream("", "10 0 d0 0 0 5 5 re f"),
        stream("/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 /BBox [0 0 10 10] /XStep 10 /YStep 10 /Resources << >>", "0 0 5 5 re f"),
        "<< /Type /ExtGState /SMask << /S /Luminosity /G 10 0 R >> >>".into(),
        stream("/Subtype /Form /BBox [0 0 10 10] /Group << /S /Transparency /CS /DeviceGray >> /Resources << >>", "1 g 0 0 10 10 re f"),
        stream("", "BT /F1 12 Tf 200 200 Td (public sibling) Tj ET"),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 11 0 R >>".into(),
    ]);
    let output = redact_pdf(
        pdf,
        b"",
        0,
        &[Rect::new(0.0, 0.0, 190.0, 60.0)],
        &RedactOptions { fill: None },
    )
    .unwrap();
    let document = PdfDocument::open(output).unwrap();
    let payload = String::from_utf8_lossy(&all_payload_bytes(&document)).into_owned();
    assert!(!payload.contains(ORIGINAL_CONTENT_SECRET));
    assert!(page_content(&document).contains("public text"));
    assert!(payload.contains("public sibling"));
    let sibling = document.page(1).unwrap();
    assert!(
        String::from_utf8_lossy(&document.page_content_bytes(&sibling).unwrap())
            .contains("public sibling")
    );
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

fn annotation_rect_reference_pdf(array_ref: bool, element_refs: bool) -> Vec<u8> {
    let inside = if element_refs {
        "[11 0 R 12 0 R 13 0 R 14 0 R]"
    } else {
        "[10 10 20 20]"
    };
    let outside = if element_refs {
        "[15 0 R 16 0 R 17 0 R 18 0 R]"
    } else {
        "[200 200 220 220]"
    };
    build_pdf(&[
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /Annots [5 0 R 6 0 R 7 0 R 8 0 R] >>".into(),
        stream("", "10 10 5 5 re f"),
        format!("<< /Type /Annot /Subtype /Text /Rect {} /Contents (first annotation payload) >>", if array_ref { "9 0 R" } else { inside }),
        format!("<< /Type /Annot /Subtype /Text /Rect {} /Contents (second annotation payload) >>", if array_ref { "10 0 R" } else { outside }),
        "<< /Type /Annot /Subtype /Text /Rect [10 10 /Bad 20] /Contents (malformed rectangle retained) >>".into(),
        "<< /Type /Annot /Subtype /Text /Contents (missing rectangle retained) >>".into(),
        inside.into(),
        outside.into(),
        "10".into(), "10.0".into(), "20".into(), "20.0".into(),
        "200".into(), "200.0".into(), "220".into(), "220.0".into(),
    ])
}

fn check_annotation_rect_references(array_ref: bool, element_refs: bool) {
    let output = redact_pdf(
        annotation_rect_reference_pdf(array_ref, element_refs),
        b"",
        0,
        &[Rect::new(0.0, 0.0, 30.0, 30.0)],
        &RedactOptions { fill: None },
    )
    .unwrap();
    let document = PdfDocument::open(output).unwrap();
    let payload = String::from_utf8_lossy(&all_payload_bytes(&document)).into_owned();
    assert!(
        !payload.contains("first annotation payload"),
        "overlapping annotation remains: {payload}"
    );
    assert!(payload.contains("second annotation payload"));
    assert!(payload.contains("malformed rectangle retained"));
    assert!(payload.contains("missing rectangle retained"));
    assert_eq!(document.page(0).unwrap().annots.len(), 3);
}

#[test]
fn annotation_rect_array_reference_is_resolved_before_overlap() {
    check_annotation_rect_references(true, false);
}

#[test]
fn annotation_rect_element_references_are_resolved_before_overlap() {
    check_annotation_rect_references(false, true);
}

#[test]
fn annotation_rect_combined_references_are_resolved_before_overlap() {
    check_annotation_rect_references(true, true);
}

#[test]
fn annotation_rect_references_follow_pending_coordinate_edits() {
    use zpdf_core::{ObjectId, PdfObject};

    let mut writer = IncrementalWriter::new(annotation_rect_reference_pdf(true, true)).unwrap();
    // Move the first annotation outside and the second inside using pending
    // indirect-number replacements; the original reader sees the opposite.
    for (id, value) in [
        (11, 200),
        (12, 200),
        (13, 220),
        (14, 220),
        (15, 10),
        (16, 10),
        (17, 20),
        (18, 20),
    ] {
        writer.overwrite_object(ObjectId(id, 0), PdfObject::Integer(value));
    }
    writer
        .redact_page(
            0,
            &[Rect::new(0.0, 0.0, 30.0, 30.0)],
            &RedactOptions { fill: None },
        )
        .unwrap();
    let mut output = Cursor::new(Vec::new());
    writer.write(&mut output).unwrap();
    let current = PdfDocument::open(output.into_inner()).unwrap();
    let rewritten = zpdf_writer::rewrite_pdf(current.file(), &Default::default()).unwrap();
    let document = PdfDocument::open(rewritten).unwrap();
    let payload = String::from_utf8_lossy(&all_payload_bytes(&document)).into_owned();
    assert!(payload.contains("first annotation payload"));
    assert!(!payload.contains("second annotation payload"));
    assert!(payload.contains("malformed rectangle retained"));
    assert!(payload.contains("missing rectangle retained"));
    assert_eq!(document.page(0).unwrap().annots.len(), 3);
}

#[test]
fn annotation_rect_alias_chains_follow_pending_targets() {
    use zpdf_core::{ObjectId, PdfObject};

    for pending_alias in [false, true] {
        let pdf = build_pdf(&[
            "<< /Type /Catalog /Pages 2 0 R >>".into(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /Annots [5 0 R] >>".into(),
            stream("", "10 10 5 5 re f"),
            "<< /Type /Annot /Subtype /Text /Rect 6 0 R /Contents (aliased annotation payload) >>".into(),
            "7 0 R".into(),
            "[8 0 R 9 0 R 10 0 R 11 0 R]".into(),
            "12 0 R".into(), "13 0 R".into(), "14 0 R".into(), "15 0 R".into(),
            "200".into(), "200".into(), "220".into(), "220".into(),
        ]);
        let mut writer = IncrementalWriter::new(pdf).unwrap();
        for (id, value) in [(12, 10), (13, 10), (14, 20), (15, 20)] {
            writer.overwrite_object(ObjectId(id, 0), PdfObject::Integer(value));
        }
        if pending_alias {
            writer.overwrite_object(ObjectId(6, 0), PdfObject::Ref(ObjectId(7, 0)));
            writer.overwrite_object(ObjectId(8, 0), PdfObject::Ref(ObjectId(12, 0)));
        }
        writer
            .redact_page(
                0,
                &[Rect::new(0.0, 0.0, 30.0, 30.0)],
                &RedactOptions { fill: None },
            )
            .unwrap();
        let mut output = Cursor::new(Vec::new());
        writer.write(&mut output).unwrap();
        let current = PdfDocument::open(output.into_inner()).unwrap();
        let rewritten = zpdf_writer::rewrite_pdf(current.file(), &Default::default()).unwrap();
        let document = PdfDocument::open(rewritten).unwrap();
        assert!(document.page(0).unwrap().annots.is_empty());
        assert!(!String::from_utf8_lossy(&all_payload_bytes(&document))
            .contains("aliased annotation payload"));
    }
}

#[test]
fn annotation_alias_entries_follow_pending_array_and_dictionary_edits() {
    use zpdf_core::{ObjectId, PdfDict, PdfName, PdfObject, PdfString};
    for indirect_array in [false, true] {
        let pdf = build_pdf(&[
            "<< /Type /Catalog /Pages 2 0 R >>".into(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
            format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /Annots {} >>", if indirect_array { "6 0 R" } else { "[8 0 R 9 0 R]" }),
            stream("", "10 10 5 5 re f"),
            "<< /Type /Annot /Subtype /Text /Rect [200 200 220 220] /Contents (source annotation) >>".into(),
            "7 0 R".into(), "[8 0 R]".into(), "5 0 R".into(),
            "<< /Type /Annot /Subtype /Text /Rect [200 200 220 220] /Contents (public annotation) >>".into(),
        ]);
        let mut writer = IncrementalWriter::new(pdf).unwrap();
        let mut annotation = PdfDict::new();
        annotation.insert(PdfName::new("Type"), PdfObject::Name(PdfName::new("Annot")));
        annotation.insert(
            PdfName::new("Subtype"),
            PdfObject::Name(PdfName::new("Text")),
        );
        annotation.insert(
            PdfName::new("Rect"),
            PdfObject::Array(
                [10, 10, 20, 20]
                    .into_iter()
                    .map(PdfObject::Integer)
                    .collect(),
            ),
        );
        annotation.insert(
            PdfName::new("Contents"),
            PdfObject::String(PdfString(b"pending annotation secret".to_vec())),
        );
        writer.overwrite_object(ObjectId(5, 0), PdfObject::Dict(annotation));
        writer.overwrite_object(
            ObjectId(7, 0),
            PdfObject::Array(vec![
                PdfObject::Ref(ObjectId(8, 0)),
                PdfObject::Ref(ObjectId(9, 0)),
            ]),
        );
        writer
            .redact_page(
                0,
                &[Rect::new(0.0, 0.0, 30.0, 30.0)],
                &RedactOptions { fill: None },
            )
            .unwrap();
        let mut output = Cursor::new(Vec::new());
        writer.write(&mut output).unwrap();
        let current = PdfDocument::open(output.into_inner()).unwrap();
        let rewritten = zpdf_writer::rewrite_pdf(current.file(), &Default::default()).unwrap();
        let document = PdfDocument::open(rewritten).unwrap();
        let payload = String::from_utf8_lossy(&all_payload_bytes(&document)).into_owned();
        assert!(!payload.contains("pending annotation secret"));
        assert!(payload.contains("public annotation"));
        assert_eq!(document.page(0).unwrap().annots.len(), 1);
    }
}

fn widget_annotation_alias_pdf(multihop: bool) -> Vec<u8> {
    let fields = if multihop {
        "5 0 R 8 0 R 9 0 R 10 0 R 11 0 R"
    } else {
        "5 0 R 10 0 R"
    };
    let structure = if multihop {
        "/StructTreeRoot 13 0 R"
    } else {
        ""
    };
    build_pdf(&[
        format!("<< /Type /Catalog /Pages 2 0 R /AcroForm << /Fields [{fields}] >> {structure} >>"),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /Annots [8 0 R 11 0 R] >>".into(),
        stream("", "10 10 5 5 re f 200 200 5 5 re f"),
        "<< /Type /Annot /Subtype /Widget /FT /Tx /T (selected) /V (ALIAS-PRIVATE-VALUE) /Contents (ALIAS-PRIVATE-CONTENTS) /Rect [10 10 20 20] /P 3 0 R /AP << /N 6 0 R >> >>".into(),
        stream("/Type /XObject /Subtype /Form /BBox [0 0 10 10]", "% ALIAS-PRIVATE-AP\n0 0 10 10 re f"),
        "5 0 R".into(),
        if multihop { "9 0 R".into() } else { "5 0 R".into() },
        "5 0 R".into(),
        "<< /Type /Annot /Subtype /Widget /FT /Tx /T (retained) /V (ALIAS-PUBLIC-VALUE) /Contents (ALIAS-PUBLIC-CONTENTS) /Rect [200 200 220 220] /P 3 0 R /AP << /N 12 0 R >> >>".into(),
        "10 0 R".into(),
        stream("/Type /XObject /Subtype /Form /BBox [0 0 20 20]", "% ALIAS-PUBLIC-AP\n0 0 20 20 re f"),
        "<< /Type /StructTreeRoot /K [14 0 R 15 0 R 16 0 R 17 0 R 18 0 R] >>".into(),
        "<< /Type /StructElem /S /Form /Pg 3 0 R /K << /Type /OBJR /Obj 5 0 R >> /ActualText (ALIAS-PRIVATE-STRUCT-TARGET) >>".into(),
        "<< /Type /StructElem /S /Form /Pg 3 0 R /K << /Type /OBJR /Obj 9 0 R >> /Alt (ALIAS-PRIVATE-STRUCT-MIDDLE) >>".into(),
        "<< /Type /StructElem /S /Form /Pg 3 0 R /K << /Type /OBJR /Obj 8 0 R >> /E (ALIAS-PRIVATE-STRUCT-ENTRY) >>".into(),
        "<< /Type /StructElem /S /Form /Pg 3 0 R /K << /Type /OBJR /Obj 11 0 R >> /ActualText (ALIAS-PUBLIC-STRUCT) >>".into(),
        "<< /Type /StructElem /S /Form /Pg 3 0 R /K << /Type /OBJR /Obj 7 0 R >> /ActualText (ALIAS-PRIVATE-STRUCT-OTHER) >>".into(),
    ])
}

fn assert_widget_annotation_alias_cleanup(document: &PdfDocument, multihop: bool) {
    let payload = String::from_utf8_lossy(&all_payload_bytes(document)).into_owned();
    for secret in [
        "ALIAS-PRIVATE-VALUE",
        "ALIAS-PRIVATE-CONTENTS",
        "ALIAS-PRIVATE-AP",
        "ALIAS-PRIVATE-STRUCT-TARGET",
        "ALIAS-PRIVATE-STRUCT-MIDDLE",
        "ALIAS-PRIVATE-STRUCT-ENTRY",
        "ALIAS-PRIVATE-PENDING",
        "ALIAS-PRIVATE-STRUCT-OTHER",
    ] {
        assert!(
            !payload.contains(secret),
            "{secret} survived through an annotation alias"
        );
    }
    for public in [
        "ALIAS-PUBLIC-VALUE",
        "ALIAS-PUBLIC-CONTENTS",
        "ALIAS-PUBLIC-AP",
    ] {
        assert!(payload.contains(public), "outside annotation lost {public}");
    }
    if multihop {
        assert!(payload.contains("ALIAS-PUBLIC-STRUCT"));
    }
    assert_eq!(document.page(0).unwrap().annots.len(), 1);
    let file = document.file();
    let catalog = file.resolve(file.trailer.get_ref("Root").unwrap()).unwrap();
    let fields = catalog
        .as_dict()
        .unwrap()
        .get("AcroForm")
        .unwrap()
        .as_dict()
        .unwrap()
        .get_array("Fields")
        .unwrap();
    assert_eq!(
        fields.len(),
        if multihop { 2 } else { 1 },
        "removed aliases must all be unlinked"
    );
    for field in fields {
        let value = file.resolve(field.as_ref().unwrap()).unwrap();
        let expected =
            zpdf_core::PdfObject::String(zpdf_core::PdfString(b"ALIAS-PUBLIC-VALUE".to_vec()));
        assert_eq!(value.as_dict().unwrap().get("V"), Some(&expected));
    }
}

#[test]
fn annotation_widget_alias_removes_direct_acroform_target() {
    let output = redact_pdf(
        widget_annotation_alias_pdf(false),
        b"",
        0,
        &[Rect::new(0.0, 0.0, 30.0, 30.0)],
        &RedactOptions { fill: None },
    )
    .unwrap();
    let document = PdfDocument::open(output).unwrap();
    assert_widget_annotation_alias_cleanup(&document, false);
}

#[test]
fn annotation_widget_alias_multihop_scrubs_all_field_and_objr_targets() {
    let output = redact_pdf(
        widget_annotation_alias_pdf(true),
        b"",
        0,
        &[Rect::new(0.0, 0.0, 30.0, 30.0)],
        &RedactOptions { fill: None },
    )
    .unwrap();
    let document = PdfDocument::open(output).unwrap();
    assert_widget_annotation_alias_cleanup(&document, true);
}

#[test]
fn annotation_widget_alias_cleanup_follows_pending_middle_and_target() {
    use zpdf_core::{ObjectId, PdfName, PdfObject, PdfString};
    let mut writer = IncrementalWriter::new(widget_annotation_alias_pdf(true)).unwrap();
    // Make the middle edge and target both pending; ordinary resolution of a
    // pending top-level reference must not hide the target's field ownership.
    writer.overwrite_object(ObjectId(9, 0), PdfObject::Ref(ObjectId(5, 0)));
    let mut widget = writer
        .resolve_current(ObjectId(5, 0))
        .unwrap()
        .as_dict()
        .unwrap()
        .clone();
    widget.insert(
        PdfName::new("V"),
        PdfObject::String(PdfString(b"ALIAS-PRIVATE-PENDING".to_vec())),
    );
    writer.overwrite_object(ObjectId(5, 0), PdfObject::Dict(widget));
    writer
        .redact_page(
            0,
            &[Rect::new(0.0, 0.0, 30.0, 30.0)],
            &RedactOptions { fill: None },
        )
        .unwrap();
    let mut output = Cursor::new(Vec::new());
    writer.write(&mut output).unwrap();
    let current = PdfDocument::open(output.into_inner()).unwrap();
    let output = zpdf_writer::rewrite_pdf(current.file(), &Default::default()).unwrap();
    let document = PdfDocument::open(output).unwrap();
    assert_widget_annotation_alias_cleanup(&document, true);
}

#[test]
fn annotation_widget_alias_budget_failure_preserves_pending_edits() {
    use zpdf_core::{ObjectId, PdfName, PdfObject};
    let mut writer = IncrementalWriter::new(widget_annotation_alias_pdf(false)).unwrap();
    let aliases: Vec<_> = (0..33)
        .map(|_| {
            let (number, generation) = writer.try_add_object(&PdfObject::Null).unwrap();
            ObjectId(number, generation as u16)
        })
        .collect();
    for (index, alias) in aliases.iter().enumerate() {
        let next = aliases.get(index + 1).copied().unwrap_or(ObjectId(5, 0));
        writer.overwrite_object(*alias, PdfObject::Ref(next));
    }
    let mut page = writer
        .resolve_current(ObjectId(3, 0))
        .unwrap()
        .as_dict()
        .unwrap()
        .clone();
    page.insert(
        PdfName::new("Annots"),
        PdfObject::Array(vec![
            PdfObject::Ref(ObjectId(8, 0)),
            PdfObject::Ref(aliases[0]),
        ]),
    );
    writer.overwrite_object(ObjectId(3, 0), PdfObject::Dict(page));
    let mut before = Cursor::new(Vec::new());
    writer.write(&mut before).unwrap();
    let result = writer.redact_page(
        0,
        &[Rect::new(0.0, 0.0, 30.0, 30.0)],
        &RedactOptions { fill: None },
    );
    assert!(
        matches!(result, Err(zpdf_core::Error::RecursionLimit(32))),
        "an over-budget entry chain must fail before committing the earlier removal"
    );
    let mut after = Cursor::new(Vec::new());
    writer.write(&mut after).unwrap();
    assert_eq!(before.into_inner(), after.into_inner());
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

/// Prune the inherited table once, preserving inheritance and direct or
/// Form-mediated uses on another page.
#[test]
fn inherited_resources_are_pruned_once_without_losing_surviving_uses() {
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
        assert!(pages.as_dict().unwrap().get("Resources").is_some());
        for index in 0..2 {
            let leaf = document
                .file()
                .resolve(document.page(index).unwrap().id)
                .unwrap();
            assert!(leaf.as_dict().unwrap().get("Resources").is_none());
        }
        assert!(!page_content(&document).contains("/Tile Do"));
        let other_page = document.page(1).unwrap();
        assert!(
            String::from_utf8_lossy(&document.page_content_bytes(&other_page).unwrap())
                .contains("public page")
        );
    }
}

#[test]
fn structure_alias_to_page_preserves_prepared_redaction() {
    let original = build_pdf(&[
        "<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 6 0 R >>".into(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R /Thumb 7 0 R /S /P /Pg 3 0 R /K 0 /ActualText (selected replacement) >>".into(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
        stream("", "/P <</MCID 0>> BDC BT /F1 12 Tf 20 20 Td (selected text) Tj ET EMC BT /F1 12 Tf 200 200 Td (public text) Tj ET"),
        "<< /Type /StructTreeRoot /K 3 0 R >>".into(),
        stream("/Subtype /Image", "selected thumbnail"),
    ]);
    let output = redact_pdf(
        original,
        b"",
        0,
        &[Rect::new(0.0, 0.0, 180.0, 60.0)],
        &RedactOptions { fill: None },
    )
    .unwrap();
    let document = PdfDocument::open(output).unwrap();
    let payload = String::from_utf8_lossy(&all_payload_bytes(&document)).into_owned();
    assert!(!payload.contains("selected"), "{payload}");
    assert!(page_content(&document).contains("public text"));
}

#[test]
fn malformed_structure_leaves_existing_writer_edits_unchanged() {
    use zpdf_core::{ObjectId, PdfName, PdfObject, PdfString};

    let original = build_pdf(&[
        "<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 6 0 R >>".into(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>".into(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
        stream("", "/P <</MCID 0>> BDC BT /F1 12 Tf 20 20 Td (selected text) Tj ET EMC"),
        "<< /Type /StructTreeRoot /Pg 3 0 R /K 7 0 R >>".into(),
        "<< /S /P /K 7 0 R /ActualText (selected replacement) >>".into(),
    ]);
    let mut writer = IncrementalWriter::new(original).unwrap();
    let mut page = writer
        .resolve_current(ObjectId(3, 0))
        .unwrap()
        .as_dict()
        .unwrap()
        .clone();
    page.insert(
        PdfName::new("PriorEdit"),
        PdfObject::String(PdfString(b"keep prior edit".to_vec())),
    );
    writer.overwrite_object(ObjectId(3, 0), PdfObject::Dict(page));
    let mut before = Cursor::new(Vec::new());
    writer.write(&mut before).unwrap();
    assert!(writer
        .redact_page(
            0,
            &[Rect::new(0.0, 0.0, 180.0, 60.0)],
            &RedactOptions { fill: None },
        )
        .is_err());
    let mut after = Cursor::new(Vec::new());
    writer.write(&mut after).unwrap();
    assert_eq!(before.into_inner(), after.into_inner());
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
/// table. Empty and partial tables must both preserve the fallback use.
#[test]
fn empty_and_partial_form_resources_preserve_per_name_fallback() {
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
            let result = writer.redact_page(
                0,
                &[Rect::new(-1.0, -1.0, 100.0, 100.0)],
                &RedactOptions { fill: None },
            );
            let mut after = Cursor::new(Vec::new());
            writer.write(&mut after).unwrap();
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
            assert!(!page_content(&document).contains("/Tile Do"));
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

/// These normal-PDF controls require success; a fail-closed error is a regression.
#[test]
fn removed_image_preserves_shared_and_identical_color_space_resources() {
    for separate in [false, true] {
        for indexed in [false, true] {
            let kept_space = if separate { 9 } else { 7 };
            let color_space = |profile: u32| {
                if indexed {
                    format!("[/Indexed /DeviceRGB 1 {profile} 0 R]")
                } else {
                    format!("[/ICCBased {profile} 0 R]")
                }
            };
            let profile = stream(if indexed { "" } else { "/N 1" }, "shared-color-profile");
            let objects = vec![
                "<< /Type /Catalog /Pages 2 0 R >>".into(),
                "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /Resources << /XObject << /Secret 5 0 R /Public 6 0 R >> >> >>".into(),
                stream("", "q 10 0 0 10 10 10 cm /Secret Do Q q 10 0 0 10 200 200 cm /Public Do Q"),
                stream("/Type /XObject /Subtype /Image /Width 20 /Height 1 /ColorSpace 7 0 R /BitsPerComponent 8", "removed-image-bytes"),
                stream(&format!("/Type /XObject /Subtype /Image /Width 20 /Height 1 /ColorSpace {kept_space} 0 R /BitsPerComponent 8"), "public-image-bytes"),
                color_space(8),
                profile.clone(),
                color_space(10),
                profile,
            ];
            let output = redact_pdf(
                build_pdf(&objects),
                b"",
                0,
                &[Rect::new(0.0, 0.0, 100.0, 100.0)],
                &RedactOptions { fill: None },
            )
            .unwrap();
            let document = PdfDocument::open(output).unwrap();
            let payload = String::from_utf8_lossy(&all_payload_bytes(&document)).into_owned();
            assert!(!payload.contains("removed-image-bytes"));
            assert!(payload.contains("public-image-bytes"));
            assert!(payload.contains("shared-color-profile"));
            let page = document.page(0).unwrap();
            assert!(!page.resources.xobjects.contains_key("Secret"));
            assert!(page.resources.xobjects.contains_key("Public"));
            assert!(page_content(&document).contains("/Public Do"));
        }
    }
}

#[test]
fn removed_form_preserves_page_font_used_by_surviving_text() {
    let objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /Resources << /Font << /F1 6 0 R >> /XObject << /Secret 5 0 R >> >> >>".into(),
        stream("", "/Secret Do BT /F1 12 Tf 200 200 Td (public text) Tj ET"),
        stream("/Type /XObject /Subtype /Form /BBox [0 0 10 10] /Resources << /Font << /F1 6 0 R >> >>", "% removed-form-bytes\nBT /F1 8 Tf (private) Tj ET"),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /TestMarker (shared-font-payload) >>".into(),
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
    let payload = String::from_utf8_lossy(&all_payload_bytes(&document)).into_owned();
    assert!(!payload.contains("removed-form-bytes"));
    assert!(!payload.contains("private"));
    assert!(payload.contains("shared-font-payload"));
    assert!(page_content(&document).contains("public text"));
    assert!(document.page(0).unwrap().resources.fonts.contains_key("F1"));
}

#[test]
fn kept_form_and_freetext_appearance_with_own_font_resources_allow_redaction() {
    for annotation in [false, true] {
        let annots = if annotation { "/Annots [6 0 R]" } else { "" };
        let kept_draw = if annotation { "" } else { "/Public Do" };
        let objects = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".into(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
            format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /Resources << /XObject << /Secret 5 0 R /Public 7 0 R >> >> {annots} >>"),
            stream("", &format!("/Secret Do {kept_draw}")),
            stream("/Type /XObject /Subtype /Image /Width 20 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8", "removed-image-bytes"),
            "<< /Type /Annot /Subtype /FreeText /Rect [200 200 230 230] /Contents (public annotation) /AP << /N 7 0 R >> >>".into(),
            stream("/Type /XObject /Subtype /Form /BBox [200 200 230 230] /Resources << /Font << /Helv 8 0 R >> >>", "BT /Helv 12 Tf 200 200 Td (public appearance) Tj ET"),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /TestMarker (appearance-font-payload) >>".into(),
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
        let payload = String::from_utf8_lossy(&all_payload_bytes(&document)).into_owned();
        assert!(!payload.contains("removed-image-bytes"));
        assert!(payload.contains("public appearance"));
        assert!(payload.contains("appearance-font-payload"));
        assert_eq!(
            document.page(0).unwrap().annots.len(),
            usize::from(annotation)
        );
        if annotation {
            assert!(payload.contains("public annotation"));
        } else {
            assert!(page_content(&document).contains("/Public Do"));
        }
    }
}

#[test]
fn inherited_resource_cleanup_keeps_one_table_for_many_siblings() {
    const PAGES: usize = 24;
    let content_id = PAGES + 3;
    let image_id = PAGES + 5;
    let kids = (3..PAGES + 3)
        .map(|id| format!("{id} 0 R"))
        .collect::<Vec<_>>()
        .join(" ");
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        format!("<< /Type /Pages /Kids [{kids}] /Count {PAGES} /Resources << /XObject << /Secret {image_id} 0 R >> >> >>"),
    ];
    for index in 0..PAGES {
        let contents = content_id + usize::from(index != 0);
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents {contents} 0 R >>"
        ));
    }
    objects.push(stream("", "/Secret Do"));
    objects.push(stream("", "200 200 10 10 re f"));
    objects.push(stream("/Type /XObject /Subtype /Image /Width 20 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8", "removed-image-bytes"));
    let output = redact_pdf(
        build_pdf(&objects),
        b"",
        0,
        &[Rect::new(-1.0, -1.0, 100.0, 100.0)],
        &RedactOptions { fill: None },
    )
    .unwrap();
    let document = PdfDocument::open(output).unwrap();
    let catalog = document
        .file()
        .resolve(document.file().trailer.get_ref("Root").unwrap())
        .unwrap();
    let pages = document
        .file()
        .resolve(catalog.as_dict().unwrap().get_ref("Pages").unwrap())
        .unwrap();
    assert!(pages.as_dict().unwrap().get("Resources").is_some());
    for index in 0..PAGES {
        let page = document.page(index).unwrap();
        let leaf = document.file().resolve(page.id).unwrap();
        assert!(
            leaf.as_dict().unwrap().get("Resources").is_none(),
            "page {index} duplicated Resources"
        );
        assert!(!page.resources.xobjects.contains_key("Secret"));
    }
    assert!(!String::from_utf8_lossy(&all_payload_bytes(&document)).contains("removed-image-bytes"));
}

#[test]
fn nested_forms_preserve_context_fallback_and_do_not_promote_shadowed_names() {
    for include_second_context in [false, true] {
        let second = if include_second_context {
            "/OuterB Do"
        } else {
            ""
        };
        let objects = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".into(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 /Resources << /XObject << /Secret 5 0 R /Tile 6 0 R /OuterA 7 0 R /OuterB 8 0 R >> >> >>".into(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R >>".into(),
            stream("", &format!("/Secret Do /Tile Do /OuterA Do {second}")),
            stream("/Type /XObject /Subtype /Image /Width 20 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8", "removed-image-bytes"),
            stream("/Type /XObject /Subtype /Image /Width 20 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8", "page-fallback-image"),
            stream("/Type /XObject /Subtype /Form /BBox [200 200 210 210] /Resources << /XObject << /Inner 10 0 R /Tile 9 0 R >> >>", "/Inner Do"),
            stream("/Type /XObject /Subtype /Form /BBox [220 220 230 230] /Resources << /XObject << /Inner 10 0 R >> >>", "/Inner Do"),
            stream("/Type /XObject /Subtype /Image /Width 20 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8", "local-shadow-image"),
            stream("/Type /XObject /Subtype /Form /BBox [0 0 10 10] /Resources << /Font << /F1 11 0 R >> >>", "/Tile Do BT /F1 12 Tf (nested public text) Tj ET"),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
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
        let payload = String::from_utf8_lossy(&all_payload_bytes(&document)).into_owned();
        assert!(!payload.contains("removed-image-bytes"));
        assert!(payload.contains("local-shadow-image"));
        assert!(payload.contains("nested public text"));
        assert_eq!(
            payload.contains("page-fallback-image"),
            include_second_context
        );
        assert_eq!(
            document
                .page(0)
                .unwrap()
                .resources
                .xobjects
                .contains_key("Tile"),
            include_second_context
        );
        assert!(!page_content(&document).contains("/Tile Do"));
    }
}

#[test]
fn extgstate_font_selection_preserves_shared_font_descendants() {
    let objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /Resources << /XObject << /Secret 5 0 R >> /ExtGState << /GS << /Font [6 0 R 12] >> >> >> >>".into(),
        stream("", "/Secret Do BT /GS gs 200 200 Td (public text) Tj ET"),
        stream("/Type /XObject /Subtype /Form /BBox [0 0 10 10] /Resources << /Font << /F1 6 0 R >> >>", "% removed-form-bytes\nBT /F1 12 Tf (private text) Tj ET"),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /FontDescriptor 7 0 R >>".into(),
        "<< /Type /FontDescriptor /FontName /Helvetica /FontFamily (shared-font-family) /Flags 32 /FontBBox [0 0 1000 1000] >>".into(),
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
    let payload = String::from_utf8_lossy(&all_payload_bytes(&document)).into_owned();
    assert!(!payload.contains("removed-form-bytes"));
    assert!(payload.contains("shared-font-family"));
    assert!(page_content(&document).contains("public text"));
}

#[test]
fn used_forms_and_appearances_do_not_promote_unused_resource_descendants() {
    for annotation in [false, true] {
        let annots = if annotation { "/Annots [8 0 R]" } else { "" };
        let draw = if annotation { "" } else { "/Wrapper Do" };
        let objects = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".into(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 /Resources << /XObject << /Secret 5 0 R /Wrapper 6 0 R >> >> >>".into(),
            format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R {annots} >>"),
            stream("", &format!("/Secret Do {draw}")),
            stream("/Type /XObject /Subtype /Form /BBox [0 0 10 10] /Resources << /XObject << /Inside 7 0 R >> >>", "/Inside Do"),
            stream("/Type /XObject /Subtype /Form /BBox [200 200 210 210] /Resources << /XObject << /Unused 7 0 R >> >>", "% public-wrapper\n200 200 10 10 re f"),
            stream("/Type /XObject /Subtype /Image /Width 20 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8", "removed-form-payload"),
            "<< /Type /Annot /Subtype /Stamp /Rect [200 200 210 210] /AP << /N 6 0 R >> >>".into(),
        ];
        assert_no_removed_resource_after_redaction(build_pdf(&objects));
    }
}

#[test]
fn oversized_resource_tables_refuse_cleanup_before_edits() {
    // The renderer exhausts its shared 65,536-entry cap on fonts before it
    // reaches XObject. Cleanup must not infer additional rendered consumers.
    let fonts = (0..65_536)
        .map(|index| format!("/F{index} 6 0 R "))
        .collect::<String>();
    let objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /Resources << /Font << {fonts} >> /XObject << /Secret 5 0 R >> >> >>"),
        stream("", "/Secret Do"),
        stream("/Type /XObject /Subtype /Image /Width 20 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8", "removed-image-bytes"),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
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
    assert_eq!(before.into_inner(), after.into_inner());
}

#[test]
fn removed_and_kept_forms_can_share_indirect_resource_containers() {
    let objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /Resources << /XObject << /Secret 5 0 R /Public 6 0 R >> >> >>".into(),
        stream("", "/Secret Do /Public Do"),
        stream("/Type /XObject /Subtype /Form /BBox [0 0 10 10] /Resources 7 0 R", "% removed-form-bytes\nBT /F1 12 Tf (private text) Tj ET"),
        stream("/Type /XObject /Subtype /Form /BBox [200 200 230 230] /Resources 7 0 R", "BT /F1 12 Tf 200 200 Td (public text) Tj ET"),
        "<< /Font 8 0 R >>".into(),
        "<< /F1 9 0 R >>".into(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /FontDescriptor 10 0 R >>".into(),
        "<< /Type /FontDescriptor /FontName /Helvetica /FontFamily (shared-font-family) /Flags 32 /FontBBox [0 0 1000 1000] >>".into(),
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
    let payload = String::from_utf8_lossy(&all_payload_bytes(&document)).into_owned();
    assert!(!payload.contains("removed-form-bytes"));
    assert!(!payload.contains("private text"));
    assert!(payload.contains("public text"));
    assert!(payload.contains("shared-font-family"));
    assert!(document
        .page(0)
        .unwrap()
        .resources
        .xobjects
        .contains_key("Public"));
}

fn document_resource_alias_pdf(owner: &str, backup: &str, descendant: bool) -> Vec<u8> {
    let custom = |where_: &str| {
        if owner == where_ {
            format!(" /Backup {backup}")
        } else {
            String::new()
        }
    };
    let secret = if descendant {
        stream("/Type /XObject /Subtype /Form /BBox [0 0 10 10] /Resources << /XObject << /Inside 11 0 R >> >>", "/Inside Do")
    } else {
        stream("/Type /XObject /Subtype /Image /Width 20 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8", "removed-form-payload")
    };
    let pdf = build_pdf(&[
        format!("<< /Type /Catalog /Pages 2 0 R{} >>", custom("Catalog")),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /Resources << /XObject << /Secret 5 0 R /Public 6 0 R >> /Properties << /Caption 9 0 R >> >>{} >>", custom("Page")),
        stream("", "q 10 0 0 10 10 10 cm /Secret Do Q /P /Caption BDC 10 10 10 10 re f EMC q 10 0 0 10 200 200 cm /Public Do Q"),
        secret,
        stream("/Type /XObject /Subtype /Image /Width 20 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8", "public-image-payload"),
        "5 0 R".into(),
        format!("<< /Title (public metadata){} >>", custom("Info")),
        "<< /ActualText 10 0 R >>".into(),
        "(removed-property-payload)".into(),
        stream("/Type /XObject /Subtype /Image /Width 20 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8", "removed-form-payload"),
    ]);
    String::from_utf8(pdf)
        .unwrap()
        .replace("/Root 1 0 R >>", "/Root 1 0 R /Info 8 0 R >>")
        .into_bytes()
}

fn assert_document_resource_alias_refused(mut writer: IncrementalWriter) {
    let mut before = Cursor::new(Vec::new());
    writer.write(&mut before).unwrap();
    let result = writer.redact_page(
        0,
        &[Rect::new(-1.0, -1.0, 100.0, 100.0)],
        &RedactOptions { fill: None },
    );
    let mut after = Cursor::new(Vec::new());
    writer.write(&mut after).unwrap();
    if result.is_ok() {
        let current = PdfDocument::open(after.into_inner()).unwrap();
        let output = zpdf_writer::rewrite_pdf(current.file(), &Default::default()).unwrap();
        let document = PdfDocument::open(output).unwrap();
        let payload = String::from_utf8_lossy(&all_payload_bytes(&document)).into_owned();
        panic!(
            "removed resource alias survived preflight; decoded leak: {}",
            payload.contains("removed-form-payload")
                || payload.contains("removed-property-payload")
        );
    }
    assert_eq!(
        before.into_inner(),
        after.into_inner(),
        "refusal changed pending writer state"
    );
}

#[test]
fn document_resource_aliases_cannot_keep_removed_roots() {
    for owner in ["Page", "Catalog", "Info"] {
        for backup in ["5 0 R", "7 0 R"] {
            let pdf = document_resource_alias_pdf(owner, backup, false);
            assert_document_resource_alias_refused(IncrementalWriter::new(pdf).unwrap());
        }
    }
}

#[test]
fn document_resource_aliases_cannot_keep_descendants_or_inline_copies() {
    for (owner, backup) in [
        ("Page", "11 0 R"),
        ("Catalog", "10 0 R"),
        ("Info", "(removed-property-payload)"),
        ("Page", "<< /ActualText 10 0 R >>"),
        ("Catalog", "[<< /Copy (removed-property-payload) >>]"),
    ] {
        let pdf = document_resource_alias_pdf(owner, backup, true);
        assert_document_resource_alias_refused(IncrementalWriter::new(pdf).unwrap());
    }
}

#[test]
fn document_resource_aliases_follow_pending_edges() {
    use zpdf_core::{ObjectId, PdfObject};
    let mut writer =
        IncrementalWriter::new(document_resource_alias_pdf("Page", "7 0 R", false)).unwrap();
    writer.overwrite_object(ObjectId(7, 0), PdfObject::Ref(ObjectId(10, 0)));
    assert_document_resource_alias_refused(writer);
}

#[test]
fn document_resource_alias_walk_uses_prepared_pages_and_ancestors() {
    for inherited in [false, true] {
        let resources = "/Resources << /XObject << /Secret 5 0 R /Public 6 0 R >> >>";
        let objects = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".into(),
            format!("<< /Type /Pages /Kids [3 0 R] /Count 1 {} >>", if inherited { resources } else { "" }),
            format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R {} >>", if inherited { "" } else { resources }),
            stream("", "q 10 0 0 10 10 10 cm /Secret Do Q q 10 0 0 10 200 200 cm /Public Do Q"),
            stream("/Type /XObject /Subtype /Image /Width 20 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8", "removed-form-payload"),
            stream("/Type /XObject /Subtype /Image /Width 20 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8 /Owner 3 0 R /Parent 2 0 R", "public-image-payload"),
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
        let payload = String::from_utf8_lossy(&all_payload_bytes(&document)).into_owned();
        assert!(!payload.contains("removed-form-payload"));
        assert!(payload.contains("public-image-payload"));
        assert!(page_content(&document).contains("/Public Do"));
    }
}

#[test]
fn document_resource_aliases_allow_proven_shared_color_payloads() {
    use zpdf_core::{ObjectId, PdfName, PdfObject};
    let objects = vec![
        "<< /Type /Catalog /Pages 2 0 R /Backup 7 0 R >>".into(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /Resources << /XObject << /Secret 5 0 R /Public 6 0 R >> >> >>".into(),
        stream("", "q 10 0 0 10 10 10 cm /Secret Do Q q 10 0 0 10 200 200 cm /Public Do Q"),
        stream("/Type /XObject /Subtype /Image /Width 20 /Height 1 /ColorSpace [/ICCBased 7 0 R] /BitsPerComponent 8", "removed-form-payload"),
        stream("/Type /XObject /Subtype /Image /Width 20 /Height 1 /ColorSpace [/ICCBased 7 0 R] /BitsPerComponent 8", "public-image-payload"),
        stream("/N 1", "shared-color-profile"),
    ];
    let mut writer = IncrementalWriter::new(build_pdf(&objects)).unwrap();
    let mut page = writer
        .resolve_current(ObjectId(3, 0))
        .unwrap()
        .as_dict()
        .unwrap()
        .clone();
    page.insert(PdfName::new("Backup"), PdfObject::Ref(ObjectId(7, 0)));
    writer.overwrite_object(ObjectId(3, 0), PdfObject::Dict(page));
    writer
        .redact_page(
            0,
            &[Rect::new(-1.0, -1.0, 100.0, 100.0)],
            &RedactOptions { fill: None },
        )
        .unwrap();
    let mut updated = Cursor::new(Vec::new());
    writer.write(&mut updated).unwrap();
    let current = PdfDocument::open(updated.into_inner()).unwrap();
    let output = zpdf_writer::rewrite_pdf(current.file(), &Default::default()).unwrap();
    let document = PdfDocument::open(output).unwrap();
    let payload = String::from_utf8_lossy(&all_payload_bytes(&document)).into_owned();
    assert!(!payload.contains("removed-form-payload"));
    assert!(payload.contains("shared-color-profile"));
    assert!(payload.contains("public-image-payload"));
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

/// An actually drawn Form's private keys are not rendering dependencies. They
/// must not turn a removed Form's replacement-text descendants into allowed
/// shared resources just because the other Form has a surviving Do operator.
#[test]
fn drawn_form_private_key_cannot_whitelist_removed_property_text() {
    let original = build_pdf(&[
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /Resources << /XObject << /Secret 5 0 R /Public 6 0 R >> >> >>".into(),
        stream("", "/Secret Do /Public Do"),
        stream("/Type /XObject /Subtype /Form /BBox [0 0 10 10] /Resources << /Properties << /Caption 7 0 R >> >>", "% removed-form-payload\n/Span /Caption BDC 0 0 10 10 re f EMC"),
        stream("/Type /XObject /Subtype /Form /BBox [200 200 210 210] /Ignored 8 0 R", "% public-form-payload\n200 200 10 10 re f"),
        "<< /ActualText 8 0 R >>".into(),
        "(removed-property-payload)".into(),
    ]);
    assert_no_removed_resource_after_redaction(original);
}

/// The interpreter checks inline Properties across the resource stack before
/// checking references. Resource cleanup must preserve the same selected name.
#[test]
fn retained_form_preserves_page_inline_properties_before_local_reference() {
    let original = build_pdf(&[
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /Resources << /XObject << /Public 5 0 R >> /Properties << /Caption << /ActualText (public-inline-caption) >> >> >> >>".into(),
        stream("", "/Span /Caption BDC 10 10 10 10 re f EMC /Public Do"),
        stream("/Type /XObject /Subtype /Form /BBox [200 200 210 210] /Resources << /Properties << /Caption 6 0 R >> >>", "/Span /Caption BDC 200 200 10 10 re f EMC"),
        "<< /ActualText (local-reference-caption) >>".into(),
    ]);
    let output = redact_pdf(
        original,
        b"",
        0,
        &[Rect::new(-1.0, -1.0, 100.0, 100.0)],
        &RedactOptions { fill: None },
    )
    .unwrap();
    let document = PdfDocument::open(output).unwrap();
    assert!(
        document
            .page(0)
            .unwrap()
            .resources
            .properties_inline
            .contains_key("Caption"),
        "the kept Form still selects the page's inline Caption before its local ref"
    );
    assert!(
        String::from_utf8_lossy(&all_payload_bytes(&document)).contains("public-inline-caption")
    );
    assert!(!page_content(&document).contains("/Caption BDC"));
}

fn removed_image_via_misclassified_resource(
    extra_resources: &str,
    retained_content: &str,
    extra_object: &str,
) -> Vec<u8> {
    build_pdf(&[
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /Resources << /XObject << /Secret 5 0 R >> {extra_resources} >> >>"),
        stream("", &format!("/Secret Do {retained_content}")),
        stream("/Type /XObject /Subtype /Form /BBox [0 0 10 10] /Resources << /XObject << /Image 7 0 R >> >>", "% removed-form-payload\n/Image Do"),
        extra_object.into(),
        stream("/Type /XObject /Subtype /Image /Width 24 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8", "removed-property-payload"),
    ])
}

#[test]
fn image_without_shading_type_cannot_whitelist_removed_descendant() {
    assert_no_removed_resource_after_redaction(removed_image_via_misclassified_resource(
        "/Shading << /Fake 7 0 R >>",
        "/Fake sh",
        "null",
    ));
}

#[test]
fn image_without_function_type_cannot_whitelist_removed_descendant() {
    assert_no_removed_resource_after_redaction(removed_image_via_misclassified_resource(
        "/ExtGState << /Fake 6 0 R >>",
        "/Fake gs",
        "<< /TR 7 0 R >>",
    ));
}

#[test]
fn property_dict_without_pattern_type_cannot_whitelist_removed_descendant() {
    let original = build_pdf(&[
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /Resources << /XObject << /Secret 5 0 R >> /Pattern << /Fake 6 0 R >> >> >>".into(),
        stream("", "/Secret Do /Pattern cs /Fake scn"),
        stream("/Type /XObject /Subtype /Form /BBox [0 0 10 10] /Resources << /Properties << /Caption 6 0 R >> >>", "% removed-form-payload\n/Span /Caption BDC 0 0 10 10 re f EMC"),
        "<< /ActualText (removed-property-payload) >>".into(),
    ]);
    assert_no_removed_resource_after_redaction(original);
}

/// Selecting the dictionary for its line width does not make its unrelated
/// ActualText a retained replacement-text use. Sharing the object across two
/// semantic roles must not retain the removed Form's readable caption.
#[test]
fn shared_extgstate_does_not_whitelist_unused_replacement_text() {
    let original = build_pdf(&[
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /Resources << /XObject << /Secret 5 0 R >> /ExtGState << /Public 6 0 R >> >> >>".into(),
        stream("", "/Secret Do /Public gs 200 200 10 10 re S"),
        stream("/Type /XObject /Subtype /Form /BBox [0 0 10 10] /Resources << /Properties << /Caption 6 0 R >> >>", "% removed-form-payload\n/Span /Caption BDC 0 0 10 10 re f EMC"),
        "<< /Type /ExtGState /LW 1 /ActualText (removed-property-payload) >>".into(),
    ]);
    assert_no_removed_resource_after_redaction(original);
}

/// A non-text selection may precede a genuine replacement-text selection of
/// the same shared object; only the retained semantic use justifies the text.
#[test]
fn shared_extgstate_then_properties_preserves_retained_replacement_text() {
    let original = build_pdf(&[
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /Resources << /XObject << /Secret 5 0 R >> /ExtGState << /Public 6 0 R >> /Properties << /Caption 6 0 R >> >> >>".into(),
        stream("", "/Secret Do /Public gs /Span /Caption BDC 200 200 10 10 re f EMC"),
        stream("/Type /XObject /Subtype /Form /BBox [0 0 10 10] /Resources << /Properties << /Caption 6 0 R >> >>", "% removed-form-payload\n/Span /Caption BDC 0 0 10 10 re f EMC"),
        "<< /Type /ExtGState /LW 1 /ActualText (public-shared-caption) >>".into(),
    ]);
    let output = redact_pdf(
        original,
        b"",
        0,
        &[Rect::new(-1.0, -1.0, 100.0, 100.0)],
        &RedactOptions { fill: None },
    )
    .unwrap();
    let document = PdfDocument::open(output).unwrap();
    let payload = String::from_utf8_lossy(&all_payload_bytes(&document)).into_owned();
    assert!(!payload.contains("removed-form-payload"));
    assert!(payload.contains("public-shared-caption"));
    assert!(page_content(&document).contains("/Caption BDC"));
    let page = document.page(0).unwrap();
    assert!(page.resources.ext_g_state.contains_key("Public"));
    assert!(page.resources.properties.contains_key("Caption"));
}
