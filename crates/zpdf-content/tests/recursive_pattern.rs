//! A form XObject that draws itself through a tiling-pattern cell recurses
//! forever. It must be refused on re-entry, not expanded tile by tile until
//! the page command budget runs out (GHOSTSCRIPT-700301-0 took 1.7 s).

use zpdf_content::interpreter::ContentInterpreter;
use zpdf_core::Rect;
use zpdf_document::PdfDocument;

fn stream(dict: &str, body: &[u8]) -> Vec<u8> {
    let mut v = format!("<< {dict} /Length {} >>\nstream\n", body.len()).into_bytes();
    v.extend_from_slice(body);
    v.extend_from_slice(b"\nendstream");
    v
}

/// Page → form 5 → pattern 6 fill → form 5 → …
fn build_pdf() -> Vec<u8> {
    let objs: Vec<(u32, Vec<u8>)> = vec![
        (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
        (
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] \
              /Resources << /XObject << /Fm0 5 0 R >> >> /Contents 4 0 R >>"
                .to_vec(),
        ),
        (4, stream("", b"/Fm0 Do")),
        (
            5,
            stream(
                "/Type /XObject /Subtype /Form /BBox [0 0 200 200] \
                 /Resources << /Pattern << /P0 6 0 R >> >>",
                b"/Pattern cs /P0 scn 0 0 200 200 re f",
            ),
        ),
        (
            6,
            stream(
                "/PatternType 1 /PaintType 1 /TilingType 1 /BBox [0 0 50 50] \
                 /XStep 50 /YStep 50 /Resources << /XObject << /Fm0 5 0 R >> >>",
                b"/Fm0 Do",
            ),
        ),
    ];

    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (num, body) in &objs {
        offsets.push(out.len());
        out.extend_from_slice(format!("{num} 0 obj\n").as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref_off = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", objs.len() + 1).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for off in &offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF",
            objs.len() + 1,
            xref_off
        )
        .as_bytes(),
    );
    out
}

#[test]
fn form_drawing_itself_through_a_pattern_is_refused() {
    let doc = PdfDocument::open(build_pdf()).expect("parse synthetic PDF");
    let page = doc.page(0).expect("page 0");
    let content = doc.page_content_bytes(&page).expect("content");

    let (list, stats) = ContentInterpreter::new(Rect::new(0.0, 0.0, 200.0, 200.0))
        .with_document(doc.file(), &page.resources)
        .interpret_with_stats(&content);

    let diagnostics = &stats.diagnostics;
    assert!(
        !diagnostics.resource_limit_reached,
        "the cycle must stop at re-entry, not at a budget"
    );
    assert!(diagnostics.malformed_content > 0);
    assert!(
        list.commands.len() < 1_000,
        "{} commands",
        list.commands.len()
    );
}
