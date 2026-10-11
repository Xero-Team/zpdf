use std::sync::Arc;
use zpdf::{FontProvider, PdfDocument};
use zpdf_benches::{font_cmap_workloads as fixtures, font_workloads};
use zpdf_writer::{DocumentBuilder, PredefinedOrdering};

#[test]
fn embedded_and_substitute_cmaps_preserve_cid_metrics_without_to_unicode() {
    for &case in fixtures::CASES {
        for embedded in [false, true] {
            let doc = fixtures::document(case, embedded, 2);
            fixtures::assert_case(&doc, case, embedded, 2);
        }
    }
}

#[test]
fn predefined_writer_uses_collection_cids_for_widths_and_glyphs() {
    for vertical in [false, true] {
        let mut builder = DocumentBuilder::new();
        let font = builder
            .embed_composite_font_predefined(
                fixtures::FONT.to_vec(),
                PredefinedOrdering::Gb,
                vertical,
            )
            .unwrap();
        let page = builder.add_page(100.0, 100.0);
        builder
            .add_text_embedded(page, "A中", 20.0, 50.0, font, 10.0, (0.0, 0.0, 0.0))
            .unwrap();
        let doc = PdfDocument::open(builder.build().unwrap())
            .unwrap()
            .with_font_provider(Arc::new(FontProvider::memory_only()));
        assert_eq!(font_workloads::extract(&doc), "A中");
        let setup = font_workloads::setup(&doc, 144.0);
        let face = ttf_parser::Face::parse(fixtures::FONT, 0).unwrap();
        let mut cids = Vec::new();
        for command in &setup.dl.commands {
            if let zpdf::display_list::RenderCommand::DrawGlyphRun(run) = command {
                let font = setup.font_cache.get(run.font_id).unwrap();
                for (index, glyph) in run.glyphs.iter().enumerate() {
                    cids.push(glyph.glyph_id);
                    let gid = face.glyph_index(['A', '中'][index]).unwrap();
                    let width = face.glyph_hor_advance(gid).unwrap() as f32;
                    let advance = if vertical { 10.0 } else { width / 100.0 };
                    assert!((glyph.advance.abs() - advance).abs() < 0.001);
                    assert_eq!(font.glyph_advance(glyph.glyph_id), width as f64);
                    assert_eq!(
                        (
                            run.transform.a,
                            run.transform.b,
                            run.transform.c,
                            run.transform.d
                        ),
                        (1.0, 0.0, 0.0, 1.0)
                    );
                    assert!(font.glyph_outline(glyph.glyph_id).is_some());
                }
                let mapping = font.cid_to_gid.as_ref().unwrap();
                assert_eq!(mapping.get(&34), Some(&face.glyph_index('A').unwrap().0));
                assert_eq!(mapping.get(&4559), Some(&face.glyph_index('中').unwrap().0));
            }
        }
        assert_eq!(cids, [34, 4559]);
    }
}

#[test]
fn cid_keyed_cff_writer_keeps_nonidentity_charset() {
    let face = ttf_parser::Face::parse(fixtures::CID_CFF, 0).unwrap();
    assert_eq!(
        face.tables().cff.unwrap().glyph_cid(ttf_parser::GlyphId(1)),
        Some(7)
    );
    let mut builder = DocumentBuilder::new();
    let font = builder
        .embed_composite_font(fixtures::CID_CFF.to_vec())
        .unwrap();
    let page = builder.add_page(100.0, 100.0);
    builder
        .add_text_embedded(
            page,
            "A中\u{20000}",
            20.0,
            50.0,
            font,
            10.0,
            (0.0, 0.0, 0.0),
        )
        .unwrap();
    let bytes = builder.build().unwrap();
    assert!(!bytes.windows(12).any(|part| part == b"/CIDToGIDMap"));
    let doc = PdfDocument::open(bytes)
        .unwrap()
        .with_font_provider(Arc::new(FontProvider::memory_only()));
    assert_eq!(font_workloads::extract(&doc), "A中\u{20000}");
    let setup = font_workloads::setup(&doc, 144.0);
    let original = zpdf_font::LoadedFont::new_with_data(
        zpdf_font::PdfFontType::Type0CidType2,
        "reference".into(),
        fixtures::CID_CFF.to_vec(),
        zpdf_font::CidWidths::new(1000.0),
    );
    let mut seen = Vec::new();
    for command in &setup.dl.commands {
        if let zpdf::display_list::RenderCommand::DrawGlyphRun(run) = command {
            let font = setup.font_cache.get(run.font_id).unwrap();
            assert!(!font.is_substitute);
            for glyph in &run.glyphs {
                seen.push(glyph.glyph_id);
                assert_eq!(
                    format!("{:?}", font.glyph_outline(glyph.glyph_id).unwrap().commands),
                    format!(
                        "{:?}",
                        original.glyph_outline(glyph.glyph_id).unwrap().commands
                    )
                );
                assert_eq!(glyph.advance, 10.0);
            }
        }
    }
    assert_eq!(seen, [7, 400, 655]);
}

#[test]
fn vertical_writer_normalizes_vmtx_and_preserves_origin_overrides() {
    for (bytes, origins) in [
        (
            include_bytes!("../../zpdf-font/tests/fixtures/coverage-vertical.ttf").as_slice(),
            [290.0, 319.0],
        ),
        (
            include_bytes!("../../zpdf-font/tests/fixtures/coverage-vertical-vorg.ttf").as_slice(),
            [800.0, 850.0],
        ),
    ] {
        let face = ttf_parser::Face::parse(bytes, 0).unwrap();
        assert_eq!(face.units_per_em(), 2000);
        let mut builder = DocumentBuilder::new();
        let font = builder
            .embed_composite_font_predefined(bytes.to_vec(), PredefinedOrdering::Gb, true)
            .unwrap();
        let page = builder.add_page(100.0, 100.0);
        builder
            .add_text_embedded(page, "A中", 20.0, 50.0, font, 10.0, (0.0, 0.0, 0.0))
            .unwrap();
        let doc = PdfDocument::open(builder.build().unwrap())
            .unwrap()
            .with_font_provider(Arc::new(FontProvider::memory_only()));
        let setup = font_workloads::setup(&doc, 144.0);
        let mut count = 0;
        for command in &setup.dl.commands {
            if let zpdf::display_list::RenderCommand::DrawGlyphRun(run) = command {
                let font = setup.font_cache.get(run.font_id).unwrap();
                assert_eq!(font.dw2, (750.0, -1000.0));
                for (index, glyph) in run.glyphs.iter().enumerate() {
                    let advance = [-1000.0, -1100.0][index];
                    assert_eq!(
                        font.cid_v_metric(glyph.glyph_id),
                        Some((advance, 250.0, origins[index]))
                    );
                    assert!((glyph.advance as f64 - advance / 100.0).abs() < 0.001);
                    assert!(
                        (glyph.y as f64 + index as f64 * 10.0 + origins[index] / 100.0).abs()
                            < 0.001
                    );
                    count += 1;
                }
            }
        }
        assert_eq!(count, 2);
    }
}
