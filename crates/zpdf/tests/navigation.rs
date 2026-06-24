//! Integration coverage for the document navigation & metadata surface
//! (outline, named destinations, page labels, link annotations, XMP, info)
//! through the public `zpdf` facade.

use zpdf::{DestView, PdfDocument};

/// Build a PDF from numbered object bodies (object i+1), `/Root` = object 1, and
/// an optional trailer `/Info` reference. Mirrors the in-crate test helpers.
fn build(objects: &[&str], info: Option<u32>) -> Vec<u8> {
    let mut buf = Vec::from(&b"%PDF-1.7\n"[..]);
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(buf.len());
        buf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
    }
    let xref = buf.len();
    buf.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for off in &offsets {
        buf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    let info = info.map(|n| format!(" /Info {n} 0 R")).unwrap_or_default();
    buf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R{info} >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    buf
}

const PAGES2: &str = "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>";
const PAGE_A: &str = "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>";
const PAGE_B: &str = "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>";

#[test]
fn outline_named_dest_and_info_through_facade() {
    let data = build(
        &[
            // 1: catalog with outline + a named-destinations name tree
            "<< /Type /Catalog /Pages 2 0 R /Outlines 5 0 R /Names << /Dests 8 0 R >> >>",
            PAGES2,                                                              // 2
            PAGE_A,                                                              // 3
            PAGE_B,                                                              // 4
            "<< /Type /Outlines /First 6 0 R /Last 7 0 R /Count 2 >>",           // 5
            "<< /Title (Cover) /Parent 5 0 R /Next 7 0 R /Dest [3 0 R /Fit] >>", // 6
            "<< /Title (Details) /Parent 5 0 R /Prev 6 0 R /Dest (sec) >>",      // 7
            "<< /Names [ (sec) << /D [4 0 R /XYZ 0 792 0] >> ] >>",              // 8
            // 9: info dictionary
            "<< /Title (Spec) /Author (Team) /Producer (zpdf) /CreationDate (D:20240101000000Z) >>",
        ],
        Some(9),
    );
    let doc = PdfDocument::open(data).expect("open");

    // Outline: two top-level bookmarks, second resolving a named destination.
    let outline = doc.outline();
    assert_eq!(outline.len(), 2);
    assert_eq!(outline[0].title, "Cover");
    assert_eq!(outline[0].dest.as_ref().unwrap().page, Some(0));
    assert_eq!(outline[1].title, "Details");
    let d1 = outline[1].dest.as_ref().expect("named dest resolved");
    assert_eq!(d1.page, Some(1));
    assert_eq!(
        d1.view,
        DestView::Xyz {
            left: Some(0.0),
            top: Some(792.0),
            zoom: None
        }
    );

    // Named destination resolves directly through the public API too.
    let sec = doc.named_destination(b"sec").expect("named dest");
    assert_eq!(sec.page, Some(1));
    assert!(doc.named_destination(b"nope").is_none());

    // Document info.
    let info = doc.info().expect("info");
    assert_eq!(info.title.as_deref(), Some("Spec"));
    assert_eq!(info.author.as_deref(), Some("Team"));
    assert_eq!(info.producer.as_deref(), Some("zpdf"));
    assert_eq!(info.creation_date.as_deref(), Some("D:20240101000000Z"));
}

#[test]
fn resolve_destination_value_through_facade() {
    // The &PdfObject entry point (e.g. resolving a link annotation's /Dest value)
    // works end-to-end through the public facade, including PdfObject re-export.
    let data = build(
        &["<< /Type /Catalog /Pages 2 0 R >>", PAGES2, PAGE_A, PAGE_B],
        None,
    );
    let doc = PdfDocument::open(data).expect("open");

    let dest = zpdf::PdfObject::Array(vec![
        zpdf::PdfObject::Ref(zpdf::ObjectId(4, 0)),
        zpdf::PdfObject::Name(zpdf::PdfName("Fit".into())),
    ]);
    let resolved = doc.resolve_destination(&dest).expect("resolve");
    assert_eq!(resolved.page, Some(1));
    assert_eq!(resolved.view, DestView::Fit);
}

#[test]
fn page_labels_through_facade() {
    // Roman front matter (i, ii) then a prefixed decimal body (A-1, A-2) over a
    // two-page document, exercised end-to-end through the public facade.
    let data = build(
        &[
            "<< /Type /Catalog /Pages 2 0 R /PageLabels \
             << /Nums [0 << /S /r >> 1 << /S /D /P (A-) >>] >> >>",
            PAGES2,
            PAGE_A,
            PAGE_B,
        ],
        None,
    );
    let doc = PdfDocument::open(data).expect("open");
    let labels = doc.page_labels().expect("page labels");
    assert_eq!(labels.label(0).as_deref(), Some("i"));
    assert_eq!(labels.label(1).as_deref(), Some("A-1"));
}

#[test]
fn link_annotation_targets_through_facade() {
    // Page 0 carries two Link annotations: one to an in-document page, one to a
    // URI. Both resolve through the public `page_annotations` surface.
    let data = build(
        &[
            "<< /Type /Catalog /Pages 2 0 R >>",
            PAGES2,
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Annots [5 0 R 6 0 R] >>",
            PAGE_B,
            "<< /Type /Annot /Subtype /Link /Rect [0 0 50 20] /Dest [4 0 R /Fit] >>",
            "<< /Type /Annot /Subtype /Link /Rect [0 30 50 50] \
             /A << /S /URI /URI (https://example.com) >> >>",
        ],
        None,
    );
    let doc = PdfDocument::open(data).expect("open");
    let page = doc.page(0).expect("page");
    let annots = doc.page_annotations(&page);
    let dest_pages: Vec<_> = annots
        .iter()
        .filter_map(|a| a.dest.as_ref().and_then(|d| d.page))
        .collect();
    assert_eq!(dest_pages, vec![1]);
    let uris: Vec<_> = annots.iter().filter_map(|a| a.uri.clone()).collect();
    assert_eq!(uris, vec!["https://example.com".to_string()]);
}

#[test]
fn xmp_metadata_through_facade() {
    // A catalog /Metadata stream carrying an XMP packet, parsed end-to-end.
    let xml = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
<rdf:Description rdf:about="" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:pdf="http://ns.adobe.com/pdf/1.3/" pdf:Producer="zpdf">
<dc:title><rdf:Alt><rdf:li xml:lang="x-default">Hello XMP</rdf:li></rdf:Alt></dc:title>
</rdf:Description>
</rdf:RDF>
</x:xmpmeta>"#;
    let meta_obj = format!(
        "<< /Type /Metadata /Subtype /XML /Length {} >>\nstream\n{xml}\nendstream",
        xml.len()
    );
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R /Metadata 4 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>".to_string(),
        meta_obj,
    ];
    let refs: Vec<&str> = objects.iter().map(|s| s.as_str()).collect();
    let doc = PdfDocument::open(build(&refs, None)).expect("open");

    let xmp = doc.xmp_metadata().expect("xmp metadata");
    assert_eq!(xmp.title.as_deref(), Some("Hello XMP"));
    assert_eq!(xmp.producer.as_deref(), Some("zpdf"));

    let raw = doc.metadata_bytes().expect("raw metadata bytes");
    assert!(std::str::from_utf8(&raw).unwrap().contains("Hello XMP"));
}

#[test]
fn absent_navigation_surfaces_are_empty() {
    let data = build(
        &["<< /Type /Catalog /Pages 2 0 R >>", PAGES2, PAGE_A, PAGE_B],
        None,
    );
    let doc = PdfDocument::open(data).expect("open");
    assert!(doc.outline().is_empty());
    assert!(doc.info().is_none());
    assert!(doc.named_destination(b"anything").is_none());
    assert!(doc.page_labels().is_none());
    assert!(doc.xmp_metadata().is_none());
    let page = doc.page(0).expect("page");
    assert!(doc
        .page_annotations(&page)
        .iter()
        .all(|a| a.dest.is_none() && a.uri.is_none()));
}
