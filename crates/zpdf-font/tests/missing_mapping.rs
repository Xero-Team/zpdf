use std::sync::Arc;
use zpdf_font::{cmap::ToUnicodeMap, CidWidths, LoadedFont, PdfFontType};

const PRIMARY: &[u8] = include_bytes!("fixtures/coverage-primary.ttf");

fn substitute() -> LoadedFont {
    LoadedFont::new_substitute(
        PdfFontType::Type0CidType2,
        "MissingFace".into(),
        Arc::from(PRIMARY),
        0,
        CidWidths::new(750.0),
    )
    .unwrap()
}

#[test]
fn absent_empty_and_multi_scalar_mappings_never_alias_substitute_gids() {
    for mapping in [None, Some("<002A> <9FFF>"), Some("<002A> <00410042>")] {
        let mut font = substitute();
        assert!(
            font.glyph_outline(42).is_some(),
            "fixture must expose the accidental GID"
        );
        font.to_unicode = mapping.map(|mapping| {
            ToUnicodeMap::parse(format!("1 beginbfchar {mapping} endbfchar").as_bytes())
        });
        font.build_substitute_cid_to_gid();
        assert!(font.cid_to_gid.as_ref().unwrap().is_empty());
        assert!(font.glyph_outline(42).is_none());
        assert_eq!(font.glyph_advance(42), 750.0);
        if mapping == Some("<002A> <00410042>") {
            assert_eq!(font.decode_to_string(&[0, 42]), "AB");
        }
    }
}

#[test]
fn valid_empty_space_keeps_its_nonzero_mapping_and_advance() {
    let mut font = substitute();
    font.to_unicode = Some(ToUnicodeMap::parse(
        b"1 beginbfchar <002A> <0020> endbfchar",
    ));
    font.build_substitute_cid_to_gid();
    let gid = *font.cid_to_gid.as_ref().unwrap().get(&42).unwrap();
    assert_ne!(gid, 0);
    assert!(
        font.glyph_outline(42).is_none(),
        "space is intentionally empty"
    );
    assert_eq!(font.glyph_advance(42), 750.0);
    assert_eq!(font.decode_to_string(&[0, 42]), " ");
}

#[test]
fn missing_program_does_not_erase_a_unicode_encoding_cmap() {
    let mut font = LoadedFont::new_placeholder("MissingFace".into());
    font.font_type = PdfFontType::Type0CidType2;
    font.cid_cmap = zpdf_font::cmap::CidCMap::predefined("UniGB-UTF16-H");
    font.validate_cid_cmap();
    assert!(font.cid_cmap.as_ref().unwrap().codes_are_unicode);
    assert_eq!(
        font.decode_to_string(&[0xd8, 0x40, 0xdc, 0x00]),
        "\u{20000}"
    );
}
