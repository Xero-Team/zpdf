//! CJK-capable authoring round-trip: embed a font as a Type0/Identity-H
//! composite font, write text, and read it back through the document/font
//! pipeline.
//!
//! `var.ttf` (the committed Latin variable-font fixture) is tiny and lacks
//! most glyphs, so it is used only for the always-on structural and
//! classification assertions. The load-bearing round-trip uses a real embedded
//! CJK font program pulled from the committed local corpus (`tests/test1/1.pdf`)
//! — skipped if the corpus is absent.

use zpdf_core::PdfObject;
use zpdf_document::PdfDocument;
use zpdf_writer::{classify_font_program, DocumentBuilder, FontProgram};

const VAR_TTF: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../crates/zpdf-font/tests/fixtures/var.ttf"
);
const CJK_CORPUS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/test1/1.pdf");

/// Extract text from page 0 by running the content interpreter with a text
/// sink (the same path the `zpdf text` CLI uses), then collapsing the spans.
fn page0_text(pdf: &[u8]) -> String {
    let doc = PdfDocument::open(pdf.to_vec()).expect("open PDF");
    let page = doc.page(0).expect("page 0");
    let mut font_cache = doc.load_page_fonts(&page);
    let content = doc.page_content_bytes(&page).expect("content bytes");
    let mut spans: Vec<zpdf_content::text::TextSpan> = Vec::new();
    {
        let interp = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_fonts(&mut font_cache)
            .with_document(doc.file(), &page.resources)
            .with_text_sink(&mut spans);
        let _ = interp.interpret(&content);
    }
    zpdf_content::text::spans_to_text(spans, 2.0)
}

#[test]
fn classify_var_ttf_as_truetype() {
    let font_bytes = std::fs::read(VAR_TTF).expect("read var.ttf");
    // var.ttf is a TrueType-flavored sfnt (has a glyf table, no CFF table).
    assert_eq!(
        classify_font_program(&font_bytes),
        Some(FontProgram::TrueType),
    );
}

#[test]
fn composite_font_emits_type0_structure() {
    // var.ttf lacks most glyphs (the single char below resolves to .notdef),
    // but the *emission* path is what matters here: a Type0/Identity-H font
    // with a CIDFontType2 descendant, /W, /ToUnicode, and an embedded
    // FontFile2 must be produced and parse back cleanly.
    let font_bytes = std::fs::read(VAR_TTF).expect("read var.ttf");
    let mut builder = DocumentBuilder::new();
    let page = builder.add_page(400.0, 200.0);
    let font = builder
        .embed_composite_font(font_bytes)
        .expect("embed composite");
    builder
        .add_text_embedded(page, "A", 40.0, 120.0, font, 24.0, (0.0, 0.0, 0.0))
        .expect("add text");
    let pdf = builder.build().expect("build");

    let body = String::from_utf8_lossy(&pdf);
    assert!(body.contains("/Subtype /Type0"), "missing Type0: {body}");
    assert!(body.contains("/Identity-H"), "missing Identity-H: {body}");
    assert!(
        body.contains("/CIDFontType2"),
        "missing CIDFontType2: {body}"
    );
    assert!(
        body.contains("/CIDToGIDMap /Identity"),
        "missing CIDToGIDMap"
    );
    assert!(body.contains("/ToUnicode"), "missing ToUnicode");
    assert!(body.contains("/FontFile2"), "missing FontFile2");
    // The produced PDF parses back through the document pipeline.
    let doc = PdfDocument::open(pdf.to_vec()).expect("re-open built PDF");
    assert_eq!(doc.page_count(), 1);
}

/// The load-bearing CJK round-trip: pull a real embedded Type0 font program
/// from the committed corpus, re-embed it as a composite font, write a CJK
/// glyph the font's cmap supports, and confirm it survives the round-trip
/// (2-byte GID codes + /ToUnicode + subsetting). Skipped when the corpus is
/// absent or has no extractable Type0 font.
#[test]
fn cjk_corpus_font_round_trips() {
    let Ok(corpus) = std::fs::read(CJK_CORPUS) else {
        eprintln!("(skipping CJK corpus test: {CJK_CORPUS} not present)");
        return;
    };
    let doc = PdfDocument::open(corpus).expect("open corpus");
    let Some((font_bytes, program)) = first_embedded_type0_program(&doc) else {
        eprintln!("(skipping CJK corpus test: no embedded Type0 font found)");
        return;
    };

    // Pick a CJK character the font's cmap actually maps, so the 2-byte code
    // is a real GID (not .notdef) and the ToUnicode round-trips.
    let face = match ttf_parser::Face::parse(&font_bytes, 0) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("(skipping CJK corpus test: cannot parse extracted font: {e})");
            return;
        }
    };
    let Some(cjk_char) = (0x4E00u32..=0x9FFF)
        .find_map(|cp| char::from_u32(cp).filter(|ch| face.glyph_index(*ch).is_some()))
    else {
        eprintln!("(skipping CJK corpus test: font has no CJK BMP glyph");
        return;
    };

    let mut builder = DocumentBuilder::new();
    let page = builder.add_page(400.0, 200.0);
    let font = builder
        .embed_composite_font(font_bytes)
        .expect("embed composite");
    let s: String = (0..3).map(|_| cjk_char).collect();
    builder
        .add_text_embedded(page, &s, 40.0, 120.0, font, 24.0, (0.0, 0.0, 0.0))
        .expect("add text");
    let pdf = builder.build().expect("build");

    let body = String::from_utf8_lossy(&pdf);
    assert!(body.contains("/Subtype /Type0"), "missing Type0");
    assert!(body.contains("/Identity-H"), "missing Identity-H");
    let expected_descendant = match program {
        FontProgram::TrueType => "/CIDFontType2",
        FontProgram::OpenTypeCff => "/CIDFontType0",
    };
    assert!(
        body.contains(expected_descendant),
        "missing {expected_descendant}"
    );

    let text = page0_text(&pdf);
    assert!(
        text.contains(&s),
        "expected {s:?} in extracted CJK text, got: {text:?}"
    );
}

/// Walk the corpus object graph for the first `/Subtype /Type0` font and
/// return its embedded program bytes (FontFile2 or FontFile3) plus the
/// detected program flavor.
fn first_embedded_type0_program(doc: &PdfDocument) -> Option<(Vec<u8>, FontProgram)> {
    let file = doc.file();
    for id in file.all_object_ids() {
        let Ok(obj) = file.resolve(id) else { continue };
        let Ok(dict) = obj.as_dict() else { continue };
        if dict.get_name("Subtype").ok() != Some("Type0") {
            continue;
        }
        let desc = dict.get("DescendantFonts").and_then(|o| deref(file, o))?;
        let PdfObject::Array(a) = desc else { continue };
        let first = deref(file, a.first()?)?;
        let PdfObject::Dict(cid) = first else {
            continue;
        };
        let fd = cid.get("FontDescriptor").and_then(|o| deref(file, o))?;
        let PdfObject::Dict(fd) = fd else { continue };
        // FontFile2 (TrueType) or FontFile3 (OpenType/CFF).
        if let Ok(ref_id) = fd.get_ref("FontFile2") {
            if let Ok(data) = file.resolve_stream_data(ref_id) {
                return Some((data, FontProgram::TrueType));
            }
        }
        if let Ok(ref_id) = fd.get_ref("FontFile3") {
            if let Ok(data) = file.resolve_stream_data(ref_id) {
                // The /Subtype on the FontFile3 stream decides CFF vs OpenType,
                // but for re-embedding both go through the OTF/CFF path here.
                return Some((data, FontProgram::OpenTypeCff));
            }
        }
    }
    None
}

fn deref(file: &zpdf_parser::PdfFile, obj: &PdfObject) -> Option<PdfObject> {
    match obj {
        PdfObject::Ref(r) => file.resolve(*r).ok(),
        other => Some(other.clone()),
    }
}
