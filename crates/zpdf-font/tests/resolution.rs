use std::sync::Arc;
use zpdf_font::{
    cmap::ToUnicodeMap, CidWidths, DrawingGlyph, FontCache, FontProvider, LoadedFont, PdfFontType,
};

const PRIMARY: &[u8] = include_bytes!("fixtures/coverage-primary.ttf");
const FALLBACK: &[u8] = include_bytes!("fixtures/coverage-fallback.ttf");

#[test]
fn original_checks_coalesce_repeats_without_aliasing_variation_sources() {
    let provider = Arc::new(FontProvider::memory_only());
    let bytes = Arc::from(include_bytes!("fixtures/coverage-uvs.ttf").as_slice());
    let mut font = LoadedFont::new_substitute(
        PdfFontType::TrueType,
        "UVS".into(),
        bytes,
        0,
        CidWidths::new(1000.0),
    )
    .unwrap();
    font.to_unicode = Some(ToUnicodeMap::parse(
        b"2 beginbfchar <01> <4E2DFE00> <02> <4E2DDB40DD00> endbfchar",
    ));
    let mut cache = FontCache::new().with_provider(provider);
    let id = cache.insert("F".into(), font);
    assert!(cache.original_glyphs_usable(id, [(2, 1, 1, 1), (2, 1, 1, 1)].into_iter()));
    assert!(!cache.original_glyphs_usable(id, [(2, 1, 1, 1), (2, 2, 2, 1)].into_iter()));
    assert!(cache.original_glyphs_usable(id, [(2, 1, 1, 1), (3, 2, 2, 1)].into_iter()));
}

fn source(mapping: &[u8]) -> LoadedFont {
    let mut font = LoadedFont::new_with_data(
        PdfFontType::TrueType,
        "Primary".into(),
        PRIMARY.to_vec(),
        CidWidths::new(750.0),
    );
    font.to_unicode = Some(ToUnicodeMap::parse(mapping));
    font
}

#[test]
fn drawing_admission_respects_budget_and_never_changes_original_ids() {
    let provider = Arc::new(FontProvider::memory_only());
    provider.register(FALLBACK, 0).unwrap();
    let mut cache = FontCache::new().with_provider(provider);
    let id = cache.insert(
        "F".into(),
        source(b"1 beginbfchar <FF> <D83DDE00> endbfchar"),
    );
    let original = DrawingGlyph {
        font_id: id,
        glyph_id: 0,
    };
    let needed = (PRIMARY.len() + FALLBACK.len()) as u64;
    assert_eq!(
        cache.resolve_drawing_glyph(original, 255, 255, 1, needed - 1),
        original
    );
    assert_eq!(cache.len(), 1);
    let selected = cache.resolve_drawing_glyph(original, 255, 255, 1, needed);
    assert_ne!(selected.font_id, id);
    assert!(cache
        .get(selected.font_id)
        .unwrap()
        .glyph_outline(selected.glyph_id)
        .is_some());
    assert_eq!(cache.bytes_used(), needed);
    assert_eq!(cache.get_by_name("F").unwrap().0, id);
    assert_eq!(
        cache.resolve_drawing_glyph(original, 255, 255, 1, needed),
        selected
    );
    assert_eq!(cache.len(), 2);
}

#[test]
fn original_mapped_glyph_and_empty_space_keep_their_font() {
    let provider = Arc::new(FontProvider::memory_only());
    provider.register(FALLBACK, 0).unwrap();
    let mut cache = FontCache::new().with_provider(provider);
    let font = source(b"1 beginbfchar <41> <D83DDE00> endbfchar");
    let gid = font.code_to_gid(65).unwrap();
    let space = font.code_to_gid(32).unwrap();
    // A mapped glyph without contours is a valid space, not a missing glyph.
    assert!(font.glyph_outline(space).is_none());
    let id = cache.insert("F".into(), font);
    for (code, gid) in [(65, gid), (32, space)] {
        let original = DrawingGlyph {
            font_id: id,
            glyph_id: gid,
        };
        assert_eq!(
            cache.resolve_drawing_glyph(original, code, code, 1, u64::MAX),
            original
        );
    }
    assert_eq!(cache.len(), 1);
}

#[test]
fn ambiguous_ligatures_and_private_use_do_not_invent_a_replacement() {
    let provider = Arc::new(FontProvider::memory_only());
    provider.register(PRIMARY, 0).unwrap();
    for text in ["00410042", "E000"] {
        let mut cache = FontCache::new().with_provider(Arc::clone(&provider));
        let id = cache.insert(
            "F".into(),
            source(format!("1 beginbfchar <FF> <{text}> endbfchar").as_bytes()),
        );
        let original = DrawingGlyph {
            font_id: id,
            glyph_id: 0,
        };
        assert_eq!(
            cache.resolve_drawing_glyph(original, 255, 255, 1, u64::MAX),
            original
        );
    }
}

#[test]
fn multi_scalar_to_unicode_still_falls_back_by_the_cmap_character() {
    let provider = Arc::new(FontProvider::memory_only());
    provider
        .register(include_bytes!("fixtures/coverage-uvs.ttf").as_slice(), 0)
        .unwrap();
    let mut cache = FontCache::new().with_provider(provider);
    let mut font = LoadedFont::new_placeholder("MissingCid".into());
    font.font_type = PdfFontType::Type0CidType2;
    font.cid_cmap = zpdf_font::cmap::CidCMap::predefined("UniGB-UCS2-H");
    // The extraction string is a ligature, never cut to its first scalar; the
    // CMap still names U+4E2D for the code.
    font.to_unicode = Some(ToUnicodeMap::parse(
        b"1 beginbfchar <4E2D> <00660069> endbfchar",
    ));
    let id = cache.insert("F".into(), font);
    let resolved = cache.resolve_drawing_glyph(
        DrawingGlyph {
            font_id: id,
            glyph_id: 0,
        },
        0x4E2D,
        0x4E2D,
        2,
        u64::MAX,
    );
    assert_ne!(resolved.font_id, id);
    assert_eq!(resolved.glyph_id, 2);
}

#[test]
fn raw_utf16_codes_can_resolve_a_supplementary_variation_sequence() {
    let provider = Arc::new(FontProvider::memory_only());
    provider
        .register(include_bytes!("fixtures/coverage-uvs.ttf").as_slice(), 0)
        .unwrap();
    let mut cache = FontCache::new().with_provider(provider);
    let id = cache.insert(
        "F".into(),
        source(b"1 beginbfchar <D840DC00> <4E2DDB40DD00> endbfchar"),
    );
    let resolved = cache.resolve_drawing_glyph(
        DrawingGlyph {
            font_id: id,
            glyph_id: 0,
        },
        0x20000,
        0xD840DC00,
        4,
        u64::MAX,
    );
    assert_ne!(resolved.font_id, id);
    assert_eq!(resolved.glyph_id, 3);
}

#[test]
fn malformed_original_outline_can_use_a_healthy_registered_face() {
    let mut broken = PRIMARY.to_vec();
    let face = ttf_parser::Face::parse(PRIMARY, 0).unwrap();
    let gid = face.glyph_index('A').unwrap().0;
    let raw = face.raw_face();
    let loca = raw.table(ttf_parser::Tag::from_bytes(b"loca")).unwrap();
    let offset = u32::from_be_bytes(
        loca[gid as usize * 4..gid as usize * 4 + 4]
            .try_into()
            .unwrap(),
    ) as usize;
    let table = raw
        .table_records
        .into_iter()
        .find(|entry| entry.tag == ttf_parser::Tag::from_bytes(b"glyf"))
        .unwrap();
    // An impossible contour count leaves cmap and metrics valid but breaks outlining.
    broken[table.offset as usize + offset..table.offset as usize + offset + 2]
        .copy_from_slice(&32767i16.to_be_bytes());
    let mut font = source(b"1 beginbfchar <41> <0041> endbfchar");
    font.font_data = Some(Arc::from(broken));
    assert!(font.glyph_outline(gid).is_none());
    let provider = Arc::new(FontProvider::memory_only());
    provider.register(PRIMARY, 0).unwrap();
    let mut cache = FontCache::new().with_provider(provider);
    let id = cache.insert("F".into(), font);
    let result = cache.resolve_drawing_glyph(
        DrawingGlyph {
            font_id: id,
            glyph_id: gid,
        },
        65,
        65,
        1,
        u64::MAX,
    );
    assert_ne!(result.font_id, id);
    assert_eq!(result.glyph_id, gid);
    assert!(cache
        .get(result.font_id)
        .unwrap()
        .glyph_outline(gid)
        .is_some());
}

#[test]
fn explicit_encoding_miss_does_not_alias_an_ascii_glyph() {
    let mut font = source(b"");
    let mut encoding = zpdf_font::encoding::Encoding::win_ansi();
    encoding.apply_difference(b'A', "uni4F01");
    font.encoding = Some(encoding);
    assert!(
        font.code_to_gid(65).is_none(),
        "missing Chinese glyph must not become Latin A"
    );
    let provider = Arc::new(FontProvider::memory_only());
    provider.register(FALLBACK, 0).unwrap();
    let mut cache = FontCache::new().with_provider(provider);
    let id = cache.insert("F".into(), font);
    let result = cache.resolve_drawing_glyph(
        DrawingGlyph {
            font_id: id,
            glyph_id: 0,
        },
        65,
        65,
        1,
        u64::MAX,
    );
    assert_ne!(result.font_id, id);
    let face = ttf_parser::Face::parse(FALLBACK, 0).unwrap();
    assert_eq!(result.glyph_id, face.glyph_index('\u{4F01}').unwrap().0);
}
