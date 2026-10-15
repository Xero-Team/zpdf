//! Authoritative predefined CMaps keep the PDF's code → CID semantics:
//! embedded programs draw CIDs with /W advances, and substituted legacy
//! vertical fonts keep their per-CID /W2 metrics.

use std::sync::Arc;

use zpdf_content::interpreter::ContentInterpreter;
use zpdf_core::Rect;
use zpdf_display_list::{PositionedGlyph, RenderCommand};
use zpdf_font::cmap::CidCMap;
use zpdf_font::{CidWidths, FontCache, LoadedFont, PdfFontType};

const PRIMARY: &[u8] = include_bytes!("../../zpdf-font/tests/fixtures/coverage-primary.ttf");

fn shown_glyphs(font: LoadedFont, content: &[u8]) -> Vec<PositionedGlyph> {
    let mut cache = FontCache::new();
    cache.insert("F1".into(), font);
    let list = ContentInterpreter::new(Rect::new(0.0, 0.0, 100.0, 100.0))
        .with_fonts(&mut cache)
        .interpret(content);
    list.commands
        .into_iter()
        .find_map(|command| match command {
            RenderCommand::DrawGlyphRun(run) => Some(run.glyphs),
            _ => None,
        })
        .expect("glyph run")
}

#[test]
fn embedded_unicode_cmap_draws_the_cid_with_its_pdf_width() {
    let cmap = CidCMap::predefined("UniJIS-UCS2-H").unwrap();
    let cid = cmap.code_to_cid(0x41, 2) as u16;
    assert_ne!(cid, 0);
    let mut widths = CidWidths::new(1000.0);
    widths.set(cid, 777.0);
    let mut font = LoadedFont::new_with_data(
        PdfFontType::Type0CidType2,
        "Embedded".into(),
        PRIMARY.to_vec(),
        widths,
    );
    assert!(cmap.codes_are_unicode && cmap.is_authoritative());
    assert!(!font.is_substitute);
    font.cid_cmap = Some(cmap);
    let glyphs = shown_glyphs(font, b"BT /F1 10 Tf <0041> Tj ET");
    // The embedded program's own Unicode cmap and hmtx must not replace the
    // CID (resolved through /CIDToGIDMap at draw time) or the /W advance.
    assert_eq!(glyphs[0].glyph_id, cid);
    assert!((glyphs[0].advance - 7.77).abs() < 0.001);
}

#[test]
fn substituted_legacy_vertical_cmap_keeps_per_cid_w2_metrics() {
    let cmap = CidCMap::predefined("90ms-RKSJ-V").unwrap();
    let cid = cmap.code_to_cid(0x8141, 2) as u16;
    assert_ne!(cid, 0);
    let mut widths = CidWidths::new(1000.0);
    widths.set_v(cid, -900.0, 200.0, 700.0);
    let mut font = LoadedFont::new_substitute(
        PdfFontType::Type0CidType2,
        "Substitute".into(),
        Arc::from(PRIMARY),
        0,
        widths,
    )
    .unwrap();
    font.cid_cmap = Some(cmap);
    assert!(font.legacy_substitute());
    let glyphs = shown_glyphs(font, b"BT /F1 10 Tf <8141> Tj ET");
    // /W2 [cid [-900 200 700]] at 10pt, not the /DW2 default (880, -1000).
    assert!((glyphs[0].x + 2.0).abs() < 0.001);
    assert!((glyphs[0].y + 7.0).abs() < 0.001);
    assert!((glyphs[0].advance + 9.0).abs() < 0.001);
}
