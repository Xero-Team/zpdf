//! End-to-end coverage of Tagged-PDF reading-order text extraction: the content
//! interpreter captures each text run's marked-content id (`/MCID`), and
//! [`zpdf::struct_ordered_text`] joins those runs through the structure tree into
//! the document's logical reading order — exercised through the public facade.

use zpdf::{
    spans_to_text, struct_ordered_text, ContentInterpreter, ImageCache, PdfDocument, TextSpan,
};

/// Concatenate 1-based objects with a classic xref table + trailer (`/Root` = 1).
fn assemble(objs: &[Vec<u8>]) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = Vec::with_capacity(objs.len());
    for (i, body) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref_pos = out.len();
    let n = objs.len() + 1;
    out.extend_from_slice(format!("xref\n0 {n}\n").as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for off in &offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<< /Size {n} /Root 1 0 R >>\nstartxref\n{xref_pos}\n%%EOF\n").as_bytes(),
    );
    out
}

fn dict(body: &str) -> Vec<u8> {
    body.as_bytes().to_vec()
}

fn stream_obj(dict_str: &str, content: &[u8]) -> Vec<u8> {
    let mut v = format!("<< {dict_str} /Length {} >>\nstream\n", content.len()).into_bytes();
    v.extend_from_slice(content);
    v.extend_from_slice(b"\nendstream");
    v
}

const HELV: &str = "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>";

/// Run the interpreter over page `pi` with a text sink and return its spans.
fn extract_spans(doc: &PdfDocument, pi: usize) -> Vec<TextSpan> {
    let page = doc.page(pi).expect("page");
    let mut fonts = doc.load_page_fonts(&page);
    let content = doc.page_content_bytes(&page).expect("content");
    let mut images = ImageCache::new();
    let mut spans = Vec::new();
    {
        let interp = ContentInterpreter::new(page.effective_box())
            .with_fonts(&mut fonts)
            .with_document(doc.file(), &page.resources)
            .with_images(&mut images)
            .with_text_sink(&mut spans);
        let _ = interp.interpret(&content);
    }
    spans
}

fn find<'a>(spans: &'a [TextSpan], needle: &str) -> &'a TextSpan {
    spans
        .iter()
        .find(|s| s.text.contains(needle))
        .unwrap_or_else(|| panic!("no span containing {needle:?} in {spans:?}"))
}

#[test]
fn reading_order_follows_structure_not_geometry() {
    // "Alpha" is painted higher on the page (y=700) and first in the content
    // stream; "Beta" lower (y=680). The structure tree lists Beta's element
    // before Alpha's, so the reading order reverses the geometric one.
    let content = b"BT /F0 12 Tf \
        /Span <</MCID 0>> BDC 1 0 0 1 100 700 Tm (Alpha) Tj EMC \
        /Span <</MCID 1>> BDC 1 0 0 1 100 680 Tm (Beta) Tj EMC ET";
    let pdf = assemble(&[
        dict(
            "<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 5 0 R /MarkInfo << /Marked true >> >>",
        ),
        dict("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
        dict(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R \
              /Resources << /Font << /F0 6 0 R >> >> >>",
        ),
        stream_obj("", content),
        dict("<< /Type /StructTreeRoot /K 7 0 R >>"),
        dict(HELV),
        dict("<< /Type /StructElem /S /Document /P 5 0 R /K [8 0 R 9 0 R] >>"),
        // Beta's element (MCID 1) comes first in the structure.
        dict("<< /Type /StructElem /S /Span /P 7 0 R /Pg 3 0 R /K 1 >>"),
        dict("<< /Type /StructElem /S /Span /P 7 0 R /Pg 3 0 R /K 0 >>"),
    ]);
    let doc = PdfDocument::open(pdf).expect("open");
    let spans = extract_spans(&doc, 0);

    // The interpreter captured each run's MCID.
    assert_eq!(find(&spans, "Alpha").mcid, Some(0));
    assert_eq!(find(&spans, "Beta").mcid, Some(1));

    let tree = doc.struct_tree().expect("structure tree");
    // Structure order: Beta then Alpha (on separate lines).
    assert_eq!(struct_ordered_text(&spans, 0, &tree), "Beta\nAlpha");
    // Geometric order is the opposite (top-to-bottom).
    assert_eq!(spans_to_text(spans, 2.0), "Alpha\nBeta");
}

#[test]
fn nested_sequence_inherits_enclosing_mcid() {
    // An inner marked-content sequence with no /MCID inherits the enclosing one.
    let content = b"BT /F0 12 Tf 1 0 0 1 100 700 Tm \
        /Span <</MCID 5>> BDC (Outer) Tj \
        /Span <</Lang (en)>> BDC (Inner) Tj EMC EMC ET";
    let pdf = assemble(&[
        dict(
            "<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 5 0 R /MarkInfo << /Marked true >> >>",
        ),
        dict("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
        dict(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R \
              /Resources << /Font << /F0 6 0 R >> >> >>",
        ),
        stream_obj("", content),
        dict("<< /Type /StructTreeRoot /K 7 0 R >>"),
        dict(HELV),
        dict("<< /Type /StructElem /S /P /P 5 0 R /Pg 3 0 R /K 5 >>"),
    ]);
    let doc = PdfDocument::open(pdf).expect("open");
    let spans = extract_spans(&doc, 0);
    assert!(!spans.is_empty());
    assert!(
        spans.iter().all(|s| s.mcid == Some(5)),
        "inner run must inherit the enclosing MCID 5: {:?}",
        spans.iter().map(|s| (&s.text, s.mcid)).collect::<Vec<_>>()
    );
    // Both runs bind to the single MCID-5 element.
    let tree = doc.struct_tree().expect("tree");
    let text = struct_ordered_text(&spans, 0, &tree);
    assert!(text.contains("Outer") && text.contains("Inner"), "{text:?}");
}

#[test]
fn malformed_mcid_is_not_captured() {
    // A negative /MCID is rejected; the run carries no marked-content id.
    let content = b"BT /F0 12 Tf 1 0 0 1 100 700 Tm \
        /Span <</MCID -3>> BDC (Bad) Tj EMC ET";
    let pdf = assemble(&[
        dict("<< /Type /Catalog /Pages 2 0 R >>"),
        dict("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
        dict(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R \
              /Resources << /Font << /F0 5 0 R >> >> >>",
        ),
        stream_obj("", content),
        dict(HELV),
    ]);
    let doc = PdfDocument::open(pdf).expect("open");
    let spans = extract_spans(&doc, 0);
    assert_eq!(find(&spans, "Bad").mcid, None);
}

#[test]
fn form_xobject_marked_content_does_not_leak() {
    // The page opens /MCID 0 around a form `Do`; the form leaves an unbalanced
    // BDC open. After the form, the page's MCID must be restored (no leak), and
    // the form's own content is a separate marked-content scope.
    let page_content = b"BT /F0 12 Tf 1 0 0 1 100 700 Tm \
        /Span <</MCID 0>> BDC (Before) Tj ET \
        /Fm0 Do \
        BT 1 0 0 1 100 640 Tm (After) Tj EMC ET";
    let form_content = b"BT /F0 12 Tf 1 0 0 1 100 680 Tm (Inside) Tj \
        /Junk <</MCID 9>> BDC ET";
    let pdf = assemble(&[
        dict("<< /Type /Catalog /Pages 2 0 R >>"),
        dict("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
        dict(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R \
              /Resources << /Font << /F0 5 0 R >> /XObject << /Fm0 6 0 R >> >> >>",
        ),
        stream_obj("", page_content),
        dict(HELV),
        stream_obj(
            "/Type /XObject /Subtype /Form /BBox [0 0 612 792] \
             /Resources << /Font << /F0 5 0 R >> >>",
            form_content,
        ),
    ]);
    let doc = PdfDocument::open(pdf).expect("open");
    let spans = extract_spans(&doc, 0);

    assert_eq!(find(&spans, "Before").mcid, Some(0));
    // The form's unbalanced BDC must not change the page's marked-content state.
    assert_eq!(
        find(&spans, "After").mcid,
        Some(0),
        "page MCID leaked across the form boundary"
    );
    // The form content is a fresh marked-content scope (its run precedes the
    // form's own BDC), so it inherits nothing from the page.
    assert_eq!(find(&spans, "Inside").mcid, None);
}

#[test]
fn text_sink_does_not_change_render_commands() {
    // The MCID rides only on TextSpan; the DisplayList the backends consume must
    // be byte-for-byte identical with and without a text sink installed — the
    // structural guarantee that extraction cannot perturb CPU/GPU rendering.
    let content = b"BT /F0 12 Tf 1 0 0 1 100 700 Tm \
        /Span <</MCID 0>> BDC (Alpha) Tj EMC \
        /Span <</MCID 1>> BDC 1 0 0 1 100 680 Tm (Beta) Tj EMC ET";
    let pdf = assemble(&[
        dict("<< /Type /Catalog /Pages 2 0 R >>"),
        dict("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
        dict(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R \
              /Resources << /Font << /F0 5 0 R >> >> >>",
        ),
        stream_obj("", content),
        dict(HELV),
    ]);
    let doc = PdfDocument::open(pdf).expect("open");
    let page = doc.page(0).expect("page");
    let content_bytes = doc.page_content_bytes(&page).expect("content");

    let render = |sink: Option<&mut Vec<TextSpan>>| {
        let mut fonts = doc.load_page_fonts(&page);
        let mut images = ImageCache::new();
        let mut interp = ContentInterpreter::new(page.effective_box())
            .with_fonts(&mut fonts)
            .with_document(doc.file(), &page.resources)
            .with_images(&mut images);
        if let Some(s) = sink {
            interp = interp.with_text_sink(s);
        }
        format!("{:?}", interp.interpret(&content_bytes).commands)
    };

    let without = render(None);
    let mut spans = Vec::new();
    let with = render(Some(&mut spans));
    assert_eq!(without, with, "text sink must not change render commands");
    assert!(!spans.is_empty(), "spans were still extracted");
}
