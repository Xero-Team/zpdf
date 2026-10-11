//! Provider propagation through page and nested/inline Form font resources.

use std::sync::Arc;

use zpdf_content::interpreter::ContentInterpreter;
use zpdf_core::Rect;
use zpdf_display_list::RenderCommand;
use zpdf_document::PdfDocument;
use zpdf_font::FontProvider;

const PRIMARY: &[u8] = include_bytes!("../../zpdf-font/tests/fixtures/coverage-primary.ttf");
const FALLBACK: &[u8] = include_bytes!("../../zpdf-font/tests/fixtures/coverage-fallback.ttf");

#[test]
fn repeated_simple_codes_preserve_spacing_rise_and_transformed_positions() {
    use zpdf_font::{CidWidths, FontCache, LoadedFont, PdfFontType};
    for supplied in [false, true] {
        let mut widths = CidWidths::new(750.0);
        widths.set(32, 300.0);
        widths.set(65, 750.0);
        let mut font = if supplied {
            LoadedFont::new_substitute(
                PdfFontType::TrueType,
                "Repeated".into(),
                Arc::from(PRIMARY),
                0,
                widths,
            )
            .unwrap()
        } else {
            LoadedFont::new_with_data(
                PdfFontType::TrueType,
                "Repeated".into(),
                PRIMARY.to_vec(),
                widths,
            )
        };
        font.encoding = Some(zpdf_font::encoding::Encoding::win_ansi());
        let mut cache = FontCache::new();
        if supplied {
            let provider = Arc::new(FontProvider::memory_only());
            provider.register(PRIMARY, 0).unwrap();
            cache = cache.with_provider(provider);
        }
        cache.insert("F1".into(), font);
        let prefix = "BT /F1 12 Tf 3 Tc 4 Tw 80 Tz 2 Ts 1 0.2 0.1 1 20 30 Tm ";
        let mut positioned = |content: &str| {
            let mut spans = Vec::new();
            let list = ContentInterpreter::new(Rect::new(0.0, 0.0, 200.0, 200.0))
                .with_fonts(&mut cache)
                .with_text_sink(&mut spans)
                .interpret(format!("{prefix}{content} ET").as_bytes());
            let mut points = Vec::new();
            for command in list.commands {
                if let RenderCommand::DrawGlyphRun(run) = command {
                    for glyph in run.glyphs {
                        let point = zpdf_core::Point::new(glyph.x as f64, glyph.y as f64)
                            .transform(&run.transform);
                        points.push((glyph.glyph_id, glyph.advance, point.x, point.y));
                    }
                }
            }
            assert_eq!(
                spans.into_iter().map(|span| span.text).collect::<String>(),
                "AAA   AAA"
            );
            points
        };
        let repeated = positioned("(AAA   AAA) Tj");
        let separate = positioned("(A) Tj (A) Tj (A) Tj ( ) Tj ( ) Tj ( ) Tj (A) Tj (A) Tj (A) Tj");
        assert_eq!(repeated.len(), 9);
        for (actual, expected) in repeated.into_iter().zip(separate) {
            assert_eq!((actual.0, actual.1), (expected.0, expected.1));
            assert!((actual.2 - expected.2).abs() < 0.001);
            assert!((actual.3 - expected.3).abs() < 0.001);
        }
    }
}

#[test]
fn composite_descriptor_axes_survive_embedded_and_substitute_loading() {
    let variable = include_bytes!("../../zpdf-font/tests/fixtures/var.ttf").as_slice();
    for embedded in [false, true] {
        for inline in [false, true] {
            let file = if embedded { "/FontFile2 8 0 R" } else { "" };
            let descriptor = format!("<< /Type /FontDescriptor /FontName /VariableFace /Flags 64 /FontWeight 900 /FontStretch /Expanded /ItalicAngle -10 {file} >>");
            let reference = if inline { descriptor.as_str() } else { "7 0 R" };
            let bytes = pdf(vec![
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
                b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
                stream("", b"BT /F1 10 Tf <0001> Tj ET"),
                b"<< /Type /Font /Subtype /Type0 /BaseFont /VariableFace /Encoding /Identity-H /DescendantFonts [6 0 R] /ToUnicode 9 0 R >>".to_vec(),
                format!("<< /Type /Font /Subtype /CIDFontType2 /BaseFont /VariableFace /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /FontDescriptor {reference} /DW 730 >>").into_bytes(),
                descriptor.as_bytes().to_vec(),
                stream("", variable),
                stream("", b"1 beginbfchar <0001> <0041> endbfchar"),
            ]);
            let provider = Arc::new(FontProvider::memory_only());
            provider
                .register_with_aliases(variable, 0, &["VariableFace"])
                .unwrap();
            let doc = PdfDocument::open(bytes)
                .unwrap()
                .with_font_provider(provider);
            let page = doc.page(0).unwrap();
            let cache = doc.load_page_fonts(&page);
            let font = cache.get_by_name("F1").unwrap().1;
            assert_eq!(font.is_substitute, !embedded);
            assert_eq!(
                font.variations,
                vec![
                    (*b"wght", 900.0),
                    (*b"wdth", 125.0),
                    (*b"slnt", -10.0),
                    (*b"ital", 1.0)
                ]
            );
            assert_eq!(font.glyph_advance(1), 730.0);
            assert_eq!(font.decode_to_string(&[0, 1]), "A");
            assert!(font.glyph_outline(1).is_some());
        }
    }
}

fn pdf(objects: Vec<Vec<u8>>) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (index, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let start = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{start}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

fn stream(dict: &str, bytes: &[u8]) -> Vec<u8> {
    let mut out = format!("<< {dict} /Length {} >>\nstream\n", bytes.len()).into_bytes();
    out.extend_from_slice(bytes);
    out.extend_from_slice(b"\nendstream");
    out
}

fn font_dict() -> &'static str {
    "<< /Type /Font /Subtype /TrueType /BaseFont /ApplicationFace /Encoding /WinAnsiEncoding /FirstChar 65 /LastChar 65 /Widths [750] >>"
}

fn page_pdf() -> Vec<u8> {
    pdf(vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        stream("", b"BT /F1 10 Tf 10 50 Td (A) Tj ET"),
        font_dict().as_bytes().to_vec(),
    ])
}

#[test]
fn late_registration_updates_new_page_caches_without_invalidating_old_ones() {
    let provider = Arc::new(FontProvider::memory_only());
    let doc = PdfDocument::open(page_pdf())
        .unwrap()
        .with_font_provider(Arc::clone(&provider));
    let page = doc.page(0).unwrap();
    let old = doc.load_page_fonts(&page);
    assert!(!old.get_by_name("F1").unwrap().1.has_font_data());
    let bytes: Arc<[u8]> = Arc::from(PRIMARY);
    provider
        .register_with_aliases(Arc::clone(&bytes), 0, &["ApplicationFace"])
        .unwrap();
    let mut new = doc.load_page_fonts(&page);
    let font = new.get_by_name("F1").unwrap().1;
    assert!(font.is_substitute);
    assert!(Arc::ptr_eq(font.font_data.as_ref().unwrap(), &bytes));
    assert!(!old.get_by_name("F1").unwrap().1.has_font_data());
    let dl = ContentInterpreter::new(page.effective_box())
        .with_fonts(&mut new)
        .interpret(&doc.page_content_bytes(&page).unwrap());
    let RenderCommand::DrawGlyphRun(run) = &dl.commands[0] else {
        panic!("missing glyph run")
    };
    assert_eq!(
        run.glyphs[0].advance, 7.5,
        "PDF width overrides substitute hmtx"
    );
}

#[test]
fn nested_forms_resolve_indirect_and_inline_fonts_with_the_document_provider() {
    for inline in [false, true] {
        let font = if inline { font_dict() } else { "7 0 R" };
        let data = pdf(vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /XObject << /Outer 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
            stream("", b"/Outer Do"),
            stream("/Type /XObject /Subtype /Form /BBox [0 0 100 100] /Resources << /XObject << /Inner 6 0 R >> >>", b"/Inner Do"),
            stream(&format!("/Type /XObject /Subtype /Form /BBox [0 0 100 100] /Resources << /Font << /F1 {font} >> >>"), b"BT /F1 10 Tf 10 50 Td (A) Tj ET"),
            font_dict().as_bytes().to_vec(),
        ]);
        let provider = Arc::new(FontProvider::memory_only());
        provider
            .register_with_aliases(PRIMARY, 0, &["ApplicationFace"])
            .unwrap();
        let doc = PdfDocument::open(data)
            .unwrap()
            .with_font_provider(provider);
        let page = doc.page(0).unwrap();
        let mut fonts = doc.load_page_fonts(&page);
        let dl = ContentInterpreter::new(Rect::new(0.0, 0.0, 100.0, 100.0))
            .with_fonts(&mut fonts)
            .with_document(doc.file(), &page.resources)
            .interpret(&doc.page_content_bytes(&page).unwrap());
        let runs: Vec<_> = dl
            .commands
            .iter()
            .filter_map(|command| match command {
                RenderCommand::DrawGlyphRun(run) => Some(run),
                _ => None,
            })
            .collect();
        assert_eq!(runs.len(), 1);
        let font = fonts.get(runs[0].font_id).unwrap();
        assert!(font.is_substitute, "nested form ignored the provider");
        assert_eq!(font.font_data.as_deref().unwrap(), PRIMARY);
        assert_eq!(runs[0].glyphs[0].advance, 7.5);
        assert!(font.glyph_outline(runs[0].glyphs[0].glyph_id).is_some());
    }
}

#[test]
fn embedded_program_wins_over_a_registered_name_match() {
    let data = pdf(vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        stream("", b"BT /F1 10 Tf (A) Tj ET"),
        b"<< /Type /Font /Subtype /TrueType /BaseFont /ApplicationFace /Encoding /WinAnsiEncoding /FontDescriptor 6 0 R >>".to_vec(),
        b"<< /Type /FontDescriptor /FontName /ApplicationFace /Flags 32 /FontFile2 7 0 R >>".to_vec(),
        stream("", PRIMARY),
    ]);
    let provider = Arc::new(FontProvider::memory_only());
    provider
        .register_with_aliases(FALLBACK, 0, &["ApplicationFace"])
        .unwrap();
    let doc = PdfDocument::open(data)
        .unwrap()
        .with_font_provider(provider);
    let fonts = doc.load_page_fonts(&doc.page(0).unwrap());
    let font = fonts.get_by_name("F1").unwrap().1;
    assert!(!font.is_substitute);
    assert_eq!(font.font_data.as_deref().unwrap(), PRIMARY);
    assert!(font.code_to_gid(b'A' as u16).is_some());
}

#[test]
fn composite_substitution_routes_original_cids_through_tounicode() {
    let data = pdf(vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        stream("", b"BT /F1 10 Tf <002A> Tj ET"),
        b"<< /Type /Font /Subtype /Type0 /BaseFont /ApplicationFace /Encoding /Identity-H /DescendantFonts [6 0 R] /ToUnicode 8 0 R >>".to_vec(),
        b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ApplicationFace /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /FontDescriptor 7 0 R /DW 1000 /W [42 [750]] >>".to_vec(),
        b"<< /Type /FontDescriptor /FontName /ApplicationFace /Flags 32 >>".to_vec(),
        stream("", b"1 begincodespacerange <0000> <FFFF> endcodespacerange 1 beginbfchar <002A> <4E2D> endbfchar"),
    ]);
    let provider = Arc::new(FontProvider::memory_only());
    provider
        .register_with_aliases(PRIMARY, 0, &["ApplicationFace"])
        .unwrap();
    let doc = PdfDocument::open(data)
        .unwrap()
        .with_font_provider(provider);
    let page = doc.page(0).unwrap();
    let mut fonts = doc.load_page_fonts(&page);
    let font = fonts.get_by_name("F1").unwrap().1;
    assert_eq!(font.cid_to_gid.as_ref().unwrap().get(&42), Some(&141));
    assert!(font.glyph_outline(42).is_some());
    let mut spans = Vec::new();
    let dl = ContentInterpreter::new(page.effective_box())
        .with_fonts(&mut fonts)
        .with_text_sink(&mut spans)
        .interpret(&doc.page_content_bytes(&page).unwrap());
    assert_eq!(spans[0].text, "中");
    let RenderCommand::DrawGlyphRun(run) = &dl.commands[0] else {
        panic!("missing CID glyph")
    };
    assert_eq!(run.glyphs[0].advance, 7.5);
}

#[test]
fn missing_composite_program_keeps_code_segmentation_and_pdf_metrics() {
    for vertical in [false, true] {
        let encoding = if vertical { "Identity-V" } else { "Identity-H" };
        let data = pdf(vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
            stream("", b"BT /F1 10 Tf <002A002B> Tj ET"),
            format!("<< /Type /Font /Subtype /Type0 /BaseFont /MissingFace /Encoding /{encoding} /DescendantFonts [6 0 R] /ToUnicode 7 0 R >>").into_bytes(),
            b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /MissingFace /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /DW 1000 /W [42 [750 0]] /DW2 [900 -1200] /W2 [42 [-900 200 700 0 100 600]] >>".to_vec(),
            stream("", b"2 beginbfchar <002A> <4E2D> <002B> <6587> endbfchar"),
        ]);
        let doc = PdfDocument::open(data)
            .unwrap()
            .with_font_provider(Arc::new(FontProvider::memory_only()));
        let page = doc.page(0).unwrap();
        let mut fonts = doc.load_page_fonts(&page);
        let font = fonts.get_by_name("F1").unwrap().1;
        assert!(!font.has_font_data());
        assert!(matches!(
            font.font_type,
            zpdf_font::PdfFontType::Type0CidType2
        ));
        assert_eq!(font.dw2, (900.0, -1200.0));
        let mut spans = Vec::new();
        let dl = ContentInterpreter::new(page.effective_box())
            .with_fonts(&mut fonts)
            .with_text_sink(&mut spans)
            .interpret(&doc.page_content_bytes(&page).unwrap());
        assert_eq!(spans[0].text, "中文");
        let RenderCommand::DrawGlyphRun(run) = &dl.commands[0] else {
            panic!("missing run")
        };
        assert_eq!(run.glyphs.len(), 2);
        assert_eq!(
            run.glyphs.iter().map(|g| g.glyph_id).collect::<Vec<_>>(),
            [42, 43]
        );
        if vertical {
            for (glyph, (x, y, advance)) in run
                .glyphs
                .iter()
                .zip([(-2.0, -7.0, -9.0), (-1.0, -15.0, 0.0)])
            {
                assert!((glyph.x - x).abs() < 0.001);
                assert!((glyph.y - y).abs() < 0.001);
                assert!((glyph.advance - advance).abs() < 0.001);
            }
        } else {
            assert_eq!(run.glyphs[0].advance, 7.5);
            assert_eq!(run.glyphs[1].advance, 0.0);
            assert_eq!(run.glyphs[1].x, 7.5);
        }
    }
}
