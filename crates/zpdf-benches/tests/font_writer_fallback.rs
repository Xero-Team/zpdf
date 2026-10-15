use std::sync::Arc;
use zpdf::{PdfDocument, RenderBackend};
use zpdf_benches::{font_pack_workloads, font_workloads};
use zpdf_font::{system::SubstituteHints, FontProvider};
use zpdf_writer::DocumentBuilder;

fn collection() -> Arc<[u8]> {
    let mut bytes = b"ttcf\0\x01\0\0\0\0\0\x02\0\0\0\0\0\0\0\0".to_vec();
    for (index, font) in [font_workloads::PRIMARY, font_workloads::FALLBACK]
        .iter()
        .enumerate()
    {
        let start = bytes.len();
        bytes[12 + index * 4..16 + index * 4].copy_from_slice(&(start as u32).to_be_bytes());
        bytes.extend_from_slice(font);
        let tables = u16::from_be_bytes([font[4], font[5]]) as usize;
        for table in 0..tables {
            let position = start + 12 + table * 16 + 8;
            let offset = u32::from_be_bytes(bytes[position..position + 4].try_into().unwrap())
                as usize
                + start;
            bytes[position..position + 4].copy_from_slice(&(offset as u32).to_be_bytes());
            if &bytes[position - 8..position - 4] == b"head" {
                bytes[offset + 8..offset + 12].fill(0);
            }
        }
    }
    Arc::from(bytes)
}

#[test]
fn multilingual_writer_embeds_all_selected_faces_and_keeps_rounded_positions() {
    for ttc in [false, true] {
        let provider = FontProvider::memory_only();
        if ttc {
            let bytes = collection();
            provider.register(Arc::clone(&bytes), 0).unwrap();
            provider.register(bytes, 1).unwrap();
        } else {
            provider.register(font_workloads::PRIMARY, 0).unwrap();
            provider.register(font_workloads::FALLBACK, 0).unwrap();
        }
        provider.register(font_pack_workloads::CJK, 0).unwrap();
        let mut builder = DocumentBuilder::new();
        let page = builder.add_page(240.0, 100.0);
        let text = "A\u{4F01}\u{20010}😀 文中A";
        builder
            .add_text_with_fonts(
                page,
                text,
                20.0,
                50.0,
                &provider,
                SubstituteHints::default(),
                10.0,
                (0.0, 0.0, 0.0),
            )
            .unwrap();
        let doc = PdfDocument::open(builder.build().unwrap())
            .unwrap()
            .with_font_provider(Arc::new(FontProvider::memory_only()));
        assert_eq!(font_workloads::extract(&doc), text);
        let setup = font_workloads::setup(&doc, 144.0);
        assert_eq!(setup.font_cache.len(), 3, "each used face is embedded once");
        let mut pen = 20.0;
        let mut chars = text.chars();
        for command in &setup.dl.commands {
            if let zpdf::display_list::RenderCommand::DrawGlyphRun(run) = command {
                let font = setup.font_cache.get(run.font_id).unwrap();
                assert!(!font.is_substitute, "written PDF must be self-contained");
                let face = ttf_parser::Face::parse(font.font_data.as_deref().unwrap(), 0).unwrap();
                for glyph in &run.glyphs {
                    let ch = chars.next().unwrap();
                    assert_eq!(glyph.glyph_id, face.glyph_index(ch).unwrap().0);
                    let x = run.transform.e + glyph.x as f64;
                    assert!((x - pen).abs() < 0.002, "run boundary drift for {ch}");
                    pen += glyph.advance as f64;
                    if ch != ' ' {
                        assert!(font.glyph_outline(glyph.glyph_id).is_some());
                    }
                }
            }
        }
        assert!(chars.next().is_none());
        let pixels = zpdf::cpu::CpuRenderer::new()
            .with_fonts(&setup.font_cache)
            .render_display_list(&setup.dl, setup.scale)
            .unwrap();
        assert!(
            pixels
                .data
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|pixel| pixel[0] < 128)
                .count()
                > 100
        );
    }
}

#[test]
fn unsupported_writer_text_does_not_add_partial_content() {
    let provider = FontProvider::memory_only();
    provider.register(font_workloads::PRIMARY, 0).unwrap();
    let mut builder = DocumentBuilder::new();
    let page = builder.add_page(100.0, 100.0);
    builder
        .add_text_with_fonts(
            page,
            "A",
            10.0,
            50.0,
            &provider,
            SubstituteHints::default(),
            10.0,
            (0.0, 0.0, 0.0),
        )
        .unwrap();
    for text in ["AA\u{9FFF}", "AA\u{FE00}", "A\u{0301}"] {
        assert!(builder
            .add_text_with_fonts(
                page,
                text,
                10.0,
                50.0,
                &provider,
                SubstituteHints::default(),
                10.0,
                (0.0, 0.0, 0.0)
            )
            .is_err());
    }
    assert!(builder
        .embed_composite_font_face(&collection(), 99)
        .is_err());
    let doc = PdfDocument::open(builder.build().unwrap()).unwrap();
    assert_eq!(font_workloads::extract(&doc), "A");
}

#[test]
fn layout_overflow_rolls_back_new_faces_and_preserves_existing_content() {
    let provider = FontProvider::memory_only();
    provider.register(font_workloads::PRIMARY, 0).unwrap();
    provider.register(font_workloads::FALLBACK, 0).unwrap();
    let mut builder = DocumentBuilder::new();
    let page = builder.add_page(100.0, 100.0);
    builder
        .add_text_with_fonts(
            page,
            "A",
            10.0,
            50.0,
            &provider,
            SubstituteHints::default(),
            10.0,
            (0.0, 0.0, 0.0),
        )
        .unwrap();
    // The first glyph introduces a new face before the second advance overflows.
    assert!(builder
        .add_text_with_fonts(
            page,
            "\u{20010}A",
            0.0,
            50.0,
            &provider,
            SubstituteHints::default(),
            f64::MAX,
            (0.0, 0.0, 0.0),
        )
        .is_err());
    builder
        .add_text_with_fonts(
            page,
            "\u{20010}",
            20.0,
            50.0,
            &provider,
            SubstituteHints::default(),
            10.0,
            (0.0, 0.0, 0.0),
        )
        .unwrap();
    let doc = PdfDocument::open(builder.build().unwrap()).unwrap();
    assert_eq!(font_workloads::extract(&doc), "A\u{20010}");
    let setup = font_workloads::setup(&doc, 96.0);
    assert_eq!(setup.font_cache.len(), 2);
    let positions: Vec<_> = setup
        .dl
        .commands
        .iter()
        .filter_map(|command| match command {
            zpdf::display_list::RenderCommand::DrawGlyphRun(run) => Some(run.transform.e),
            _ => None,
        })
        .collect();
    assert_eq!(positions, [10.0, 20.0]);
}

#[test]
fn overlapping_ttc_records_cannot_amplify_the_extracted_face() {
    let mut bytes = collection().to_vec();
    let start = u32::from_be_bytes(bytes[12..16].try_into().unwrap()) as usize;
    let count = u16::from_be_bytes(bytes[start + 4..start + 6].try_into().unwrap()) as usize;
    let size = bytes.len() as u32;
    for index in 0..count {
        let pos = start + 12 + index * 16;
        if &bytes[pos..pos + 4] == b"post" {
            bytes[pos..pos + 4].copy_from_slice(b"zzzz");
            bytes[pos + 8..pos + 12].copy_from_slice(&0u32.to_be_bytes());
            bytes[pos + 12..pos + 16].copy_from_slice(&size.to_be_bytes());
        }
    }
    // Required font tables remain parseable; the extraction budget rejects
    // the extra overlapping payload before allocating a copy of it.
    assert!(ttf_parser::Face::parse(&bytes, 0).is_ok());
    let mut builder = DocumentBuilder::new();
    assert!(builder.embed_composite_font_face(&bytes, 0).is_err());
}
