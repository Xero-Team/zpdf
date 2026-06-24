//! Integration coverage for the document navigation & metadata surface
//! (outline, named destinations, info) through the public `zpdf` facade.

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
fn absent_navigation_surfaces_are_empty() {
    let data = build(
        &["<< /Type /Catalog /Pages 2 0 R >>", PAGES2, PAGE_A, PAGE_B],
        None,
    );
    let doc = PdfDocument::open(data).expect("open");
    assert!(doc.outline().is_empty());
    assert!(doc.info().is_none());
    assert!(doc.named_destination(b"anything").is_none());
}
