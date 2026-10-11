use std::sync::Arc;
use zpdf_font::cmap::{CharacterCollection, CidCMap, ToUnicodeMap};
use zpdf_font::{CidWidths, LoadedFont, PdfFontType};

#[test]
fn authoritative_lengths_four_byte_codes_and_vertical_overrides() {
    let gb = CidCMap::predefined("GBK2K-H").unwrap();
    assert_eq!(
        gb.next_code(&[0x81, 0x30, 0x84, 0x36, 0x41]),
        (0x81308436, 4)
    );
    assert_eq!(gb.code_to_cid(0x81308436, 4), 22354);
    assert_eq!(gb.decode_to_unicode(0x81308436, 4), Some(0xA5));
    assert_eq!(gb.code_to_cid(0x81308436, 2), 0);
    let hk = CidCMap::predefined("HKscs-B5-H").unwrap();
    assert_eq!(hk.code_to_cid(0x8943, 2), 17718);
    assert_eq!(hk.decode_to_unicode(0x8943, 2), Some(0x650A));
    let h = CidCMap::predefined("UniJIS-UCS2-H").unwrap();
    let v = CidCMap::predefined("UniJIS-UCS2-V").unwrap();
    assert_eq!(h.code_to_cid(0x3001, 2), 634);
    assert_eq!(v.code_to_cid(0x3001, 2), 7887);
    assert_eq!(v.wmode, 1);
    for name in ["GBK-invented-H", "UniGB-unsupported-H", "HKscs-unknown-V"] {
        assert!(CidCMap::predefined(name).is_none());
    }
}

#[test]
fn inherited_resources_preserve_lengths_and_declaration_order() {
    let map = CidCMap::parse(
        b"/GBK2K-V usecmap /WMode 0 def
        2 begincidchar <41> 999 <0041> 998 endcidchar
        1 begincidrange <41> <42> 900 endcidrange
        1 begincidchar <42> 700 endcidchar",
    );
    assert_eq!(map.wmode, 0);
    assert_eq!(map.code_to_cid(0x41, 1), 900);
    assert_eq!(map.code_to_cid(0x41, 2), 998);
    assert_eq!(map.code_to_cid(0x42, 1), 700);
    assert_eq!(map.code_to_cid(0x81308436, 4), 22354);
    assert_eq!(map.next_code(&[0x81, 0x30, 0x84, 0x36]), (0x81308436, 4));
    let utf16 = CidCMap::parse(b"/UniGB-UTF16-H usecmap 1 begincidchar <D840DC87> 400 endcidchar");
    let (code, length) = utf16.next_code(&[0xD8, 0x40, 0xDC, 0x87]);
    assert_eq!((code, length), (0x20087, 4));
    assert_eq!(utf16.code_to_cid(code, length as u8), 400);
    assert_eq!(utf16.raw_code_to_cid(0xD840DC87), 400);
}

#[test]
fn unmapped_unicode_codes_keep_text_but_malformed_sequences_do_not() {
    let utf16 = CidCMap::predefined("UniGB-UTF16-H").unwrap();
    assert_eq!(utf16.code_to_cid(0x20000, 4), 0);
    assert_eq!(utf16.decode_to_unicode(0x20000, 4), Some(0x20000));
    assert!(utf16.decode_text(0xD8400041, 4).is_none());
    assert!(utf16.decode_text(0xD840, 2).is_none());
    assert!(utf16.decode_text(0x41, 1).is_none());
}

#[test]
fn lazy_collection_recovery_respects_explicit_to_unicode_misses() {
    let bytes: Arc<[u8]> = Arc::from(include_bytes!("fixtures/coverage-cmap.ttf").as_slice());
    let mut font = LoadedFont::new_substitute(
        PdfFontType::Type0CidType2,
        "lazy".into(),
        bytes,
        0,
        CidWidths::new(730.0),
    )
    .unwrap();
    let mut map = CidCMap::identity(0);
    map.set_collection("GB1");
    font.cid_cmap = Some(map);
    font.build_substitute_cid_to_gid();
    assert!(
        font.cid_to_gid.as_ref().unwrap().is_empty(),
        "canonical CIDs are resolved lazily"
    );
    assert!(font.glyph_outline(4559).is_some());
    for destination in ["00410041", "9FFF"] {
        font.to_unicode = Some(ToUnicodeMap::parse(
            format!("1 beginbfchar <11CF> <{destination}> endbfchar").as_bytes(),
        ));
        font.build_substitute_cid_to_gid();
        assert!(
            font.glyph_outline(4559).is_none(),
            "explicit Unicode semantics must win"
        );
    }
}

#[test]
fn collection_sequences_and_to_unicode_precedence_remain_complete() {
    let mut font = LoadedFont::new_placeholder("sequences".into());
    font.font_type = PdfFontType::Type0CidType2;
    let mut map = CidCMap::identity(0);
    map.set_collection("CNS1");
    font.cid_cmap = Some(map);
    assert_eq!(font.decode_to_string(&14270u16.to_be_bytes()), "芳\u{FE00}");
    assert!(CharacterCollection::Cns1
        .unicode(14270)
        .unwrap()
        .scalar()
        .is_none());
    font.to_unicode = Some(ToUnicodeMap::parse(
        b"1 beginbfchar <37BE> <00660069> endbfchar",
    ));
    assert_eq!(font.decode_to_string(&14270u16.to_be_bytes()), "fi");
}

#[test]
fn substitute_supports_raw_three_four_byte_keys_and_variation_sequences() {
    let bytes: Arc<[u8]> = Arc::from(include_bytes!("fixtures/coverage-uvs.ttf").as_slice());
    let mut font = LoadedFont::new_substitute(
        PdfFontType::Type0CidType2,
        "variation".into(),
        bytes,
        0,
        CidWidths::new(1000.0),
    )
    .unwrap();
    font.cid_cmap = Some(CidCMap::parse(
        b"2 begincodespacerange <010000> <01FFFF> <81308130> <FE39FE39> endcodespacerange
        2 begincidchar <010001> 11 <81308436> 12 endcidchar",
    ));
    font.to_unicode = Some(ToUnicodeMap::parse(
        b"2 beginbfchar <010001> <4E2D> <81308436> <4E2DDB40DD00> endbfchar",
    ));
    font.build_substitute_cid_to_gid();
    let mapping = font.cid_to_gid.as_ref().unwrap();
    assert_eq!(mapping.get(&11), Some(&2));
    assert_eq!(mapping.get(&12), Some(&3));
    font.cid_cmap = CidCMap::predefined("UniGB-UTF16-H");
    font.to_unicode = Some(ToUnicodeMap::parse(
        b"1 beginbfchar <D840DC87> <4E2DDB40DD00> endbfchar",
    ));
    assert_eq!(font.composite_unicode_gid(0x20087, 0xD840DC87, 4), Some(3));
}
