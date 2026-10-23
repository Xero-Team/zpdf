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
    build_pdf_with_fonts(&["<< /R7 7 0 R >>", "<< /R7 8 0 R >>"], Vec::new())
}

/// The two-form page with each form's `/Font` entry given by `fonts`, plus
/// `extra` objects.
fn build_pdf_with_fonts(fonts: &[&str; 2], extra: Vec<(u32, Vec<u8>)>) -> Vec<u8> {
    let content: &[u8] = b"/Fm1 Do /Fm2 Do";
    let form_body: &[u8] = b""; // no marks needed; fonts load on invocation

    let form = |fonts: &str| -> Vec<u8> {
        let mut v = format!(
            "<< /Type /XObject /Subtype /Form /BBox [0 0 100 100] \
             /Resources << /Font {fonts} >> /Length {} >>\nstream\n",
            form_body.len()
        )
        .into_bytes();
        v.extend_from_slice(form_body);
        v.extend_from_slice(b"\nendstream");
        v
    };

    let mut objs: Vec<(u32, Vec<u8>)> = vec![
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
        (5, form(fonts[0])),
        (6, form(fonts[1])),
        (
            7,
            b"<< /Type /Font /Subtype /Type1 /BaseFont /FontA >>".to_vec(),
        ),
        (
            8,
            b"<< /Type /Font /Subtype /Type1 /BaseFont /FontB >>".to_vec(),
        ),
    ];
    objs.extend(extra);
    layout(&objs)
}

/// Serialize objects numbered 1..=N (in order) with a classic xref table.
fn layout(objs: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (num, body) in objs {
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

#[test]
fn an_indirect_form_font_dict_is_loaded() {
    // Fm2's /Font is a reference to the font dict rather than the dict itself.
    let pdf = build_pdf_with_fonts(
        &["<< /R7 7 0 R >>", "9 0 R"],
        vec![(9, b"<< /R7 8 0 R >>".to_vec())],
    );
    let doc = PdfDocument::open(pdf).expect("parse synthetic PDF");
    let page = doc.page(0).expect("page 0");
    let content = doc.page_content_bytes(&page).expect("content");
    let mut cache = FontCache::new();
    let _ = ContentInterpreter::new(Rect::new(0.0, 0.0, 200.0, 200.0))
        .with_fonts(&mut cache)
        .with_document(doc.file(), &page.resources)
        .interpret(&content);
    assert_eq!(
        cache.len(),
        2,
        "the indirect /Font dict's font is loaded too"
    );
}

#[test]
fn form_fonts_load_once_per_document_with_shared_fonts() {
    let doc = PdfDocument::open(build_pdf()).expect("parse synthetic PDF");
    let page = doc.page(0).expect("page 0");
    let content = doc.page_content_bytes(&page).expect("content");

    // Interpret the page twice, each time with a fresh page cache as a
    // multi-page render would: the second pass reuses the first's parse.
    let interpret = || {
        let mut cache = doc.load_page_fonts(&page);
        let _ = ContentInterpreter::new(Rect::new(0.0, 0.0, 200.0, 200.0))
            .with_fonts(&mut cache)
            .with_document(doc.file(), &page.resources)
            .with_shared_fonts(&doc)
            .interpret(&content);
        cache
    };
    let (first, second) = (interpret(), interpret());
    for key in ["__font_7_0", "__font_8_0"] {
        let (_, a) = first.get_by_name(key).expect("form font in the first pass");
        let (_, b) = second
            .get_by_name(key)
            .expect("form font in the second pass");
        assert!(std::ptr::eq(a, b), "{key} is parsed once and shared");
    }
}

#[test]
fn sibling_forms_with_inline_fonts_get_distinct_cache_slots() {
    // Same resource name /R7, but each form carries its own inline font dict.
    // There is no object id to key on, so the cache slot must still be unique
    // per form instead of falling back to the bare resource name.
    let pdf = build_pdf_with_fonts(
        &[
            "<< /R7 << /Type /Font /Subtype /Type1 /BaseFont /FontA >> >>",
            "<< /R7 << /Type /Font /Subtype /Type1 /BaseFont /FontB >> >>",
        ],
        Vec::new(),
    );
    let doc = PdfDocument::open(pdf).expect("parse synthetic PDF");
    let page = doc.page(0).expect("page 0");
    let content = doc.page_content_bytes(&page).expect("content");
    let mut cache = FontCache::new();
    let _ = ContentInterpreter::new(Rect::new(0.0, 0.0, 200.0, 200.0))
        .with_fonts(&mut cache)
        .with_document(doc.file(), &page.resources)
        .interpret(&content);
    assert_eq!(
        cache.len(),
        2,
        "sibling inline fonts reusing /R7 must not share one cache slot"
    );
}

/// One page painting a tiling pattern (whose resources carry an inline font)
/// `paints` times.
fn pattern_pdf(base_font: &str, paints: usize) -> Vec<u8> {
    let content = "/Pattern cs /P1 scn 0 0 50 50 re f\n".repeat(paints);
    let stream = |dict: &str, body: &str| {
        format!(
            "<< {dict} /Length {} >>\nstream\n{body}\nendstream",
            body.len()
        )
        .into_bytes()
    };
    layout(&[
        (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
        (
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] \
              /Resources << /Pattern << /P1 5 0 R >> >> /Contents 4 0 R >>"
                .to_vec(),
        ),
        (4, stream("", &content)),
        (
            5,
            stream(
                &format!(
                    "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
                     /BBox [0 0 10 10] /XStep 10 /YStep 10 /Resources << /Font << \
                     /R7 << /Type /Font /Subtype /Type1 /BaseFont /{base_font} >> >> >>"
                ),
                "",
            ),
        ),
    ])
}

fn interpret_into(pdf: Vec<u8>, cache: &mut FontCache) {
    let doc = PdfDocument::open(pdf).expect("parse synthetic PDF");
    let page = doc.page(0).expect("page 0");
    let content = doc.page_content_bytes(&page).expect("content");
    let _ = ContentInterpreter::new(Rect::new(0.0, 0.0, 200.0, 200.0))
        .with_fonts(cache)
        .with_document(doc.file(), &page.resources)
        .interpret(&content);
}

#[test]
fn repeated_pattern_paints_reuse_one_inline_font_slot() {
    // Every paint replays the pattern's resources; the inline font must map to
    // the same cache slot each time instead of filling the cache.
    let mut cache = FontCache::new();
    interpret_into(pattern_pdf("FontA", 5), &mut cache);
    assert_eq!(cache.len(), 1);
}

#[test]
fn interpreters_sharing_a_cache_keep_different_inline_fonts_apart() {
    // Two pages (here: two documents) reuse /R7 for different inline fonts
    // and share one FontCache. Each must get its own slot, not the slot the
    // first interpreter happened to create under the same key.
    let mut cache = FontCache::new();
    interpret_into(pattern_pdf("FontA", 1), &mut cache);
    interpret_into(pattern_pdf("FontB", 1), &mut cache);
    assert_eq!(cache.len(), 2);
    // …while the same inline font is still shared.
    interpret_into(pattern_pdf("FontA", 1), &mut cache);
    assert_eq!(cache.len(), 2);
}
