//! Correctness gates for every portable font benchmark fixture.

use zpdf::{PdfDocument, RenderBackend};
use zpdf_benches::font_workloads as fixtures;
use zpdf_font::{CidWidths, LoadedFont, PdfFontType};

#[test]
fn every_benchmarked_glyph_has_ink_text_and_pdf_placement() {
    for (_, text, composite) in fixtures::text_cases() {
        let doc = PdfDocument::open(fixtures::make_pdf(&text, composite, 1)).unwrap();
        for dpi in [96.0, 150.0, 300.0] {
            fixtures::assert_text_case(&doc, &text, dpi);
        }
    }
}

#[test]
fn coverage_fixtures_distinguish_missing_glyphs_from_empty_space() {
    let primary = fixtures::primary_font();
    let fallback = LoadedFont::new_with_data(
        PdfFontType::TrueType,
        "ZpdfCoverageFallback".into(),
        fixtures::FALLBACK.to_vec(),
        CidWidths::new(1000.0),
    );
    for cp in [65, 0x4E2D, 0x20000] {
        let gid = primary.unicode_glyph(cp).unwrap().0;
        assert!(primary.glyph_outline(gid).is_some());
        assert!(fallback.unicode_glyph(cp).is_none());
    }
    for cp in [0x4F60, 0x20010, 0x1F600, 0x1D400] {
        assert!(primary.unicode_glyph(cp).is_none());
        let gid = fallback.unicode_glyph(cp).unwrap().0;
        assert!(fallback.glyph_outline(gid).is_some());
    }
    let space = primary.unicode_glyph(32).unwrap();
    assert_eq!(space.1, 350.0);
    assert!(primary.glyph_outline(space.0).is_none());
    assert!(primary.unicode_glyph(0x10FFFD).is_none());
}

#[test]
fn cff_fixture_resolves_known_glyph_identity() {
    let font = fixtures::quartz_font();
    for (code, expected) in [(b'U', 26), (b'n', 42), (b'q', 45)] {
        let gid = font.code_to_gid(code as u16).unwrap();
        assert_eq!(gid, expected);
        assert!(!font.glyph_outline(gid).unwrap().commands.is_empty());
    }
    for dpi in [96.0, 150.0, 300.0] {
        let setup = fixtures::cff_setup(dpi);
        let mut renderer = zpdf::cpu::CpuRenderer::new().with_fonts(&setup.font_cache);
        let image = renderer
            .render_display_list(&setup.dl, setup.scale)
            .unwrap();
        assert!(
            image
                .data
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|pixel| pixel[0] < 128)
                .count()
                > 1000
        );
    }
}
