//! Two sibling form XObjects that reuse the same font resource name (`/R7`) for
//! *different* font objects must not collide in the shared FontCache. Keying by
//! name (or by form-depth+name) let the first form's font win, so the second
//! form's text rendered with the wrong subset. Keying by the font's object id
//! gives each distinct font its own cache slot.

use zpdf_content::interpreter::ContentInterpreter;
use zpdf_core::Rect;
use zpdf_document::PdfDocument;
use zpdf_font::FontCache;

/// Page invokes two forms; each form's /Resources/Font maps the *same* name
/// `/R7` to a *different* font object (7 vs 8). The page itself declares no
/// fonts. `load_form_fonts` loads every font a form references when the form is
/// invoked, so the form bodies can be empty.
fn build_pdf() -> Vec<u8> {
    let content: &[u8] = b"/Fm1 Do /Fm2 Do";
    let form_body: &[u8] = b""; // no marks needed; fonts load on invocation

    let form = |font_obj: u32| -> Vec<u8> {
        let mut v = format!(
            "<< /Type /XObject /Subtype /Form /BBox [0 0 100 100] \
             /Resources << /Font << /R7 {font_obj} 0 R >> >> /Length {} >>\nstream\n",
            form_body.len()
        )
        .into_bytes();
        v.extend_from_slice(form_body);
        v.extend_from_slice(b"\nendstream");
        v
    };

    let objs: Vec<(u32, Vec<u8>)> = vec![
        (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
        (
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] \
              /Resources << /XObject << /Fm1 5 0 R /Fm2 6 0 R >> >> /Contents 4 0 R >>"
                .to_vec(),
        ),
        (4, {
            let mut v = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
            v.extend_from_slice(content);
            v.extend_from_slice(b"\nendstream");
            v
        }),
        (5, form(7)),
        (6, form(8)),
        (7, b"<< /Type /Font /Subtype /Type1 /BaseFont /FontA >>".to_vec()),
        (8, b"<< /Type /Font /Subtype /Type1 /BaseFont /FontB >>".to_vec()),
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
fn sibling_forms_reusing_a_font_name_get_distinct_cache_slots() {
    let doc = PdfDocument::open(build_pdf()).expect("parse synthetic PDF");
    let page = doc.page(0).expect("page 0");
    let content = doc.page_content_bytes(&page).expect("content");

    // The page declares no fonts, so every FontCache entry after interpret comes
    // from the two forms. Whether the font dicts load or fall back to a
    // placeholder, each form contributes exactly one entry under its object-id
    // key — so two distinct font objects yield two entries. Keying by name would
    // give one (the second form reusing the first's slot).
    let mut cache = FontCache::new();
    let _ = ContentInterpreter::new(Rect::new(0.0, 0.0, 200.0, 200.0))
        .with_fonts(&mut cache)
        .with_document(doc.file(), &page.resources)
        .interpret(&content);

    assert_eq!(
        cache.len(),
        2,
        "two sibling forms reusing /R7 for different font objects must occupy \
         two cache slots, not collide into one"
    );
}
