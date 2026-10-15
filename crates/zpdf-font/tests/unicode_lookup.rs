use std::sync::Arc;
use zpdf_font::{CidWidths, LoadedFont, PdfFontType};

const PRIMARY: &[u8] = include_bytes!("fixtures/coverage-primary.ttf");
const FALLBACK: &[u8] = include_bytes!("fixtures/coverage-fallback.ttf");

fn check(bytes: &[u8], index: u32, configured: bool) {
    let mut font = LoadedFont::new_substitute(
        PdfFontType::TrueType,
        "Lookup".into(),
        Arc::from(bytes),
        index,
        CidWidths::new(1000.0),
    )
    .unwrap();
    let mut face = ttf_parser::Face::parse(bytes, index).unwrap();
    if configured {
        font.set_variations(Some(900.0), Some(125.0), Some(-12.0), false);
        for &(tag, value) in &font.variations {
            face.set_variation(ttf_parser::Tag::from_bytes(&tag), value);
        }
    }
    let mut codes = vec![
        0,
        32,
        65,
        0x4E2D,
        0x20000,
        0x10FFFD,
        0xD800,
        0x110000,
        u32::MAX,
    ];
    if let Some(cmap) = face.tables().cmap {
        for table in cmap.subtables {
            if table.is_unicode() {
                table.codepoints(|code| codes.push(code));
            }
        }
    }
    for code in codes {
        let expected = char::from_u32(code)
            .and_then(|ch| face.glyph_index(ch))
            .map(|gid| {
                (
                    gid.0,
                    face.glyph_hor_advance(gid)
                        .map_or(500.0, |value| value as f64 * 1000.0 / font.units_per_em),
                )
            });
        assert_eq!(
            font.unicode_glyph(code),
            expected,
            "code {code:X}, face {index}, configured {configured}"
        );
    }
    font.to_unicode = Some(zpdf_font::cmap::ToUnicodeMap::parse(
        b"1 beginbfchar <007F> <0041> endbfchar",
    ));
    assert_eq!(
        font.composite_unicode_gid(0x7F, 0x7F, 2),
        face.glyph_index('A')
            .map(|gid| gid.0)
            .filter(|&gid| gid != 0)
    );
}

#[test]
fn scalar_lookup_matches_face_for_all_fixture_codepoints_and_variations() {
    for bytes in [
        PRIMARY,
        FALLBACK,
        include_bytes!("fixtures/noto-sans-sc-subset.ttf").as_slice(),
        include_bytes!("fixtures/coverage-cid.otf").as_slice(),
        include_bytes!("fixtures/coverage-uvs.ttf").as_slice(),
        include_bytes!("fixtures/var.ttf").as_slice(),
    ] {
        for configured in [false, true] {
            check(bytes, 0, configured);
        }
    }
}

#[test]
fn collection_face_offsets_are_preserved() {
    let mut collection = b"ttcf\0\x01\0\0\0\0\0\x02\0\0\0\0\0\0\0\0".to_vec();
    for (index, font) in [PRIMARY, FALLBACK].iter().enumerate() {
        let start = collection.len();
        collection[12 + index * 4..16 + index * 4].copy_from_slice(&(start as u32).to_be_bytes());
        collection.extend_from_slice(font);
        let count = u16::from_be_bytes(font[4..6].try_into().unwrap()) as usize;
        for table in 0..count {
            let pos = start + 12 + table * 16 + 8;
            let offset =
                u32::from_be_bytes(collection[pos..pos + 4].try_into().unwrap()) + start as u32;
            collection[pos..pos + 4].copy_from_slice(&offset.to_be_bytes());
        }
    }
    check(&collection, 0, false);
    check(&collection, 1, false);
}

#[test]
fn unsorted_directories_and_missing_optional_metrics_match_face() {
    let count = u16::from_be_bytes(PRIMARY[4..6].try_into().unwrap()) as usize;
    let mut unsorted = PRIMARY.to_vec();
    let records: Vec<_> = PRIMARY[12..12 + count * 16]
        .as_chunks::<16>()
        .0
        .iter()
        .rev()
        .collect();
    for (index, &record) in records.iter().enumerate() {
        unsorted[12 + index * 16..28 + index * 16].copy_from_slice(record);
    }
    check(&unsorted, 0, false);
    let mut missing = PRIMARY.to_vec();
    for index in 0..count {
        let record = 12 + index * 16;
        if &missing[record..record + 4] == b"hmtx" {
            missing[record + 12..record + 16].copy_from_slice(&2u32.to_be_bytes());
        }
    }
    check(&missing, 0, false);
}

#[test]
fn malformed_required_tables_do_not_bypass_face_validation() {
    let count = u16::from_be_bytes(PRIMARY[4..6].try_into().unwrap()) as usize;
    for tag in [b"head", b"hhea", b"maxp"] {
        let mut bytes = PRIMARY.to_vec();
        for index in 0..count {
            let record = 12 + index * 16;
            if &bytes[record..record + 4] == tag {
                bytes[record + 12..record + 16].copy_from_slice(&1u32.to_be_bytes());
            }
        }
        assert!(ttf_parser::Face::parse(&bytes, 0).is_err());
        let mut font = LoadedFont::new_placeholder("Malformed".into());
        font.font_data = Some(Arc::from(bytes));
        assert!(font.unicode_glyph(65).is_none());
    }
}

#[test]
fn invalid_later_directory_record_clears_the_previous_optional_table() {
    let mut bytes = PRIMARY.to_vec();
    let count = u16::from_be_bytes(bytes[4..6].try_into().unwrap()) as usize;
    let record = 12 + (count - 1) * 16;
    assert_eq!(&bytes[record..record + 4], b"post");
    bytes[record..record + 4].copy_from_slice(b"cmap");
    bytes[record + 8..record + 12].copy_from_slice(&u32::MAX.to_be_bytes());
    bytes[record + 12..record + 16].copy_from_slice(&4u32.to_be_bytes());
    assert!(ttf_parser::Face::parse(&bytes, 0)
        .unwrap()
        .glyph_index('A')
        .is_none());
    check(&bytes, 0, false);
}
