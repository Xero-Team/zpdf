use std::io::Write;
use std::sync::Arc;

use flate2::{write::ZlibEncoder, Compression};
use zpdf_font::system::SubstituteHints;
use zpdf_font::{FontCharacter, FontPack, FontPackFormat, FontPackLimits, FontProvider};

const PRIMARY: &[u8] = include_bytes!("fixtures/coverage-primary.ttf");
const FALLBACK: &[u8] = include_bytes!("fixtures/coverage-fallback.ttf");
const UVS: &[u8] = include_bytes!("fixtures/coverage-uvs.ttf");

fn character(text: &str) -> FontCharacter {
    FontCharacter::from_text(text).unwrap()
}

#[test]
fn selection_uses_actual_coverage_and_preserves_empty_space() {
    let provider = FontProvider::memory_only();
    let primary = provider.register(PRIMARY, 0).unwrap();
    let fallback = provider.register(FALLBACK, 0).unwrap();
    for (text, expected) in [
        ("A", primary),
        ("中", primary),
        ("\u{20000}", primary),
        ("\u{4F01}", fallback),
        ("😀", fallback),
        (" ", primary),
    ] {
        let result = provider
            .resolve_character(character(text), SubstituteHints::default())
            .unwrap();
        assert_eq!(result.font_id, expected);
        let face = ttf_parser::Face::parse(&result.font.data, result.font.face_index).unwrap();
        assert_eq!(
            result.glyph_id,
            face.glyph_index(text.chars().next().unwrap()).unwrap().0
        );
        assert_ne!(result.glyph_id, 0);
        let again = provider
            .resolve_character(character(text), SubstituteHints::default())
            .unwrap();
        assert!(Arc::ptr_eq(&result.font.data, &again.font.data));
    }
}

#[test]
fn cached_misses_are_invalidated_by_registration() {
    let provider = FontProvider::memory_only();
    provider.register(PRIMARY, 0).unwrap();
    for _ in 0..3 {
        assert!(provider
            .resolve_character(character("😀"), SubstituteHints::default())
            .is_none());
    }
    let id = provider.register(FALLBACK, 0).unwrap();
    assert_eq!(
        provider
            .resolve_character(character("😀"), SubstituteHints::default())
            .unwrap()
            .font_id,
        id
    );
}

#[test]
fn variation_sequences_are_resolved_without_losing_the_selector() {
    let provider = FontProvider::memory_only();
    let primary = provider.register(PRIMARY, 0).unwrap();
    let variation = provider.register(UVS, 0).unwrap();
    let plain = provider
        .resolve_character(character("中"), SubstituteHints::default())
        .unwrap();
    assert_eq!(plain.font_id, primary);
    let default = provider
        .resolve_character(character("中\u{FE00}"), SubstituteHints::default())
        .unwrap();
    let alternate = provider
        .resolve_character(character("中\u{E0100}"), SubstituteHints::default())
        .unwrap();
    assert_eq!((default.font_id, default.glyph_id), (variation, 2));
    assert_eq!((alternate.font_id, alternate.glyph_id), (variation, 3));
    assert!(provider
        .resolve_character(character("中\u{FE01}"), SubstituteHints::default())
        .is_none());
    for invalid in ["", "AB", "\u{FE00}", "中\u{FE00}A", "A\u{0301}"] {
        assert!(FontCharacter::from_text(invalid).is_none());
    }
}

#[test]
fn metadata_style_selection_has_distinct_cached_results() {
    let provider = FontProvider::memory_only();
    let regular = provider.register(PRIMARY, 0).unwrap();
    let bold = provider
        .register(include_bytes!("fixtures/coverage-bold.ttf").as_slice(), 0)
        .unwrap();
    let hints = SubstituteHints {
        bold: true,
        ..Default::default()
    };
    for _ in 0..3 {
        assert_eq!(
            provider
                .resolve_character(character("A"), hints)
                .unwrap()
                .font_id,
            bold
        );
        assert_eq!(
            provider
                .resolve_character(character("A"), SubstituteHints::default())
                .unwrap()
                .font_id,
            regular
        );
    }
}

#[test]
fn lazy_coverage_hints_avoid_unrelated_decode_and_still_verify_cmap() {
    let provider = FontProvider::memory_only();
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(FALLBACK).unwrap();
    let pack = Arc::new(
        FontPack::new(
            encoder.finish().unwrap(),
            FontPackFormat::Zlib,
            FontPackLimits {
                max_decoded_bytes: FALLBACK.len() as u64,
                ..Default::default()
            },
        )
        .unwrap()
        .with_coverage(vec![(0x1F600, 0x1F601)])
        .unwrap(),
    );
    provider
        .register_pack(Arc::clone(&pack), 0, &["Symbols"])
        .unwrap();
    assert!(provider
        .resolve_character(character("A"), SubstituteHints::default())
        .is_none());
    assert!(!pack.is_decoded());
    assert!(provider
        .resolve_character(character("😁"), SubstituteHints::default())
        .is_none());
    assert!(pack.is_decoded());
    assert!(provider
        .resolve_character(character("😀"), SubstituteHints::default())
        .is_some());
}

#[test]
fn concurrent_coverage_selection_shares_programs() {
    let provider = Arc::new(FontProvider::memory_only());
    let id = provider.register(PRIMARY, 0).unwrap();
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let provider = Arc::clone(&provider);
            std::thread::spawn(move || {
                provider
                    .resolve_character(character("中"), SubstituteHints::default())
                    .unwrap()
            })
        })
        .collect();
    let glyphs: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    assert!(glyphs
        .iter()
        .all(|glyph| glyph.font_id == id && Arc::ptr_eq(&glyph.font.data, &glyphs[0].font.data)));
}
