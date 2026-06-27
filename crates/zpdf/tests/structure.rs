//! Integration coverage for the logical structure tree / Tagged PDF surface
//! (`struct_tree`, `is_tagged`, roles, role-map resolution, marked-content and
//! object kids) through the public `zpdf` facade.

use zpdf::{PdfDocument, StructKid, StructRole};

/// Build a PDF from numbered object bodies (object i+1), `/Root` = object 1.
fn build(objects: &[&str]) -> Vec<u8> {
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
    buf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    buf
}

const PAGES: &str = "<< /Type /Pages /Kids [3 0 R] /Count 1 >>";
const PAGE: &str = "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>";

#[test]
fn tagged_structure_tree_through_facade() {
    // A small tagged document:
    //   StructTreeRoot(4) /RoleMap { Heading1 -> H1 }
    //     Document(5)
    //       Heading1(6)  -> H1, /Pg page 0, MCID 0
    //       P(7)         -> /Pg page 0, MCID 1
    //       Figure(8)    -> /Alt, OBJR -> annotation(9)
    let data = build(&[
        "<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 4 0 R \
         /MarkInfo << /Marked true >> >>", // 1
        PAGES,                                                                  // 2
        PAGE,                                                                   // 3
        "<< /Type /StructTreeRoot /K 5 0 R /RoleMap << /Heading1 /H1 >> >>",    // 4
        "<< /Type /StructElem /S /Document /P 4 0 R /K [6 0 R 7 0 R 8 0 R] >>", // 5
        "<< /Type /StructElem /S /Heading1 /P 5 0 R /Pg 3 0 R /K 0 >>",         // 6
        "<< /Type /StructElem /S /P /P 5 0 R /Pg 3 0 R /K [1] >>",              // 7
        "<< /Type /StructElem /S /Figure /P 5 0 R /Pg 3 0 R /Alt (A diagram) \
         /K << /Type /OBJR /Obj 9 0 R /Pg 3 0 R >> >>", // 8
        "<< /Type /Annot /Subtype /Link >>",                                    // 9
    ]);
    let doc = PdfDocument::open(data).expect("open");

    assert!(doc.is_tagged());
    let tree = doc.struct_tree().expect("structure tree");
    assert!(tree.marked);
    assert_eq!(tree.children.len(), 1);
    assert_eq!(tree.element_count(), 4); // Document + 3 children

    let document = &tree.children[0];
    assert_eq!(document.role, StructRole::Document);
    let kids: Vec<&zpdf::StructElem> = document.child_elements().collect();
    assert_eq!(kids.len(), 3);

    // Heading1 resolves through /RoleMap to the standard H1 role; the raw type is
    // preserved, and the bare-integer kid is a marked-content sequence on page 0.
    let heading = kids[0];
    assert_eq!(heading.role, StructRole::H1);
    assert!(heading.role.is_heading());
    assert_eq!(heading.raw_type, "Heading1");
    assert_eq!(heading.page, Some(0));
    assert_eq!(
        heading.kids,
        vec![StructKid::MarkedContent {
            page: Some(0),
            mcid: 0
        }]
    );

    // The paragraph.
    assert_eq!(kids[1].role, StructRole::P);

    // The figure carries alternate text and an OBJR kid to the annotation.
    let figure = kids[2];
    assert_eq!(figure.role, StructRole::Figure);
    assert_eq!(figure.accessible_text(), Some("A diagram"));
    match &figure.kids[0] {
        StructKid::Object { page, obj } => {
            assert_eq!(*page, Some(0));
            assert_eq!(obj.0, 9);
        }
        other => panic!("expected OBJR kid, got {other:?}"),
    }
}

#[test]
fn untagged_document_has_no_structure_tree() {
    let data = build(&["<< /Type /Catalog /Pages 2 0 R >>", PAGES, PAGE]);
    let doc = PdfDocument::open(data).expect("open");
    assert!(!doc.is_tagged());
    assert!(doc.struct_tree().is_none());
}
