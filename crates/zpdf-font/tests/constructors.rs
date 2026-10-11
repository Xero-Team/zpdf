use zpdf_font::{CidWidths, LoadedFont, OutlineCommand, PdfFontType};

#[test]
fn absent_zero_and_vertical_metrics_remain_distinct() {
    let mut widths = CidWidths::new(f64::NAN);
    assert_eq!(widths.get(65), 1000.0);
    assert_eq!(widths.get_opt(65), None);
    widths.set(65, f64::INFINITY);
    assert!(widths.is_empty());
    widths.set_v(65, -1000.0, 500.0, 880.0);
    assert!(widths.is_empty(), "vertical data is not a horizontal width");
    widths.set(65, 0.0);
    assert_eq!(widths.get_opt(65), Some(0.0));
    assert!(!widths.is_empty());
    let mut clone = widths.clone();
    clone.set(65, 750.0);
    assert_eq!(widths.get(65), 0.0);
    assert_eq!(clone.get(65), 750.0);
    assert_eq!(clone.get_v(65), Some((-1000.0, 500.0, 880.0)));
    assert_eq!(clone.get_v(66), None);
}

#[test]
fn encrypted_type1_program_retains_glyphs_and_metrics() {
    let bytes = include_bytes!("fixtures/coverage-type1.pfa");
    let font = LoadedFont::new_with_data(
        PdfFontType::Type1,
        "ZpdfCoverageType1".into(),
        bytes.to_vec(),
        CidWidths::new(1000.0),
    );
    assert!(font.has_font_data());
    assert!(font.type1.is_some());
    assert_eq!(font.units_per_em, 1000.0);
    let gid = font.code_to_gid(b'A' as u16).unwrap();
    assert_eq!(gid, 1);
    let outline = font.glyph_outline(gid).unwrap();
    assert_eq!(outline.advance_width, 1000.0);
    assert!(outline.commands.iter().any(|command| {
        matches!(command, OutlineCommand::LineTo(x, y) if *x == 263.0 && *y == 449.0)
    }));
}

#[test]
fn type1_differences_without_character_meaning_keep_the_builtin_glyph() {
    let bytes = include_bytes!("fixtures/coverage-type1.pfa");
    let mut font = LoadedFont::new_with_data(
        PdfFontType::Type1,
        "ZpdfCoverageType1".into(),
        bytes.to_vec(),
        CidWidths::new(1000.0),
    );
    let builtin = font.code_to_gid(b'A' as u16).unwrap();
    // A producer-private name the program lacks carries no character, so the
    // program's own encoding still identifies the glyph.
    let mut encoding = zpdf_font::encoding::Encoding::standard();
    encoding.apply_difference(b'A', "g3");
    font.encoding = Some(encoding.clone());
    assert_eq!(font.code_to_gid(b'A' as u16), Some(builtin));
    // A named character the program lacks must not become its Latin A.
    encoding.apply_difference(b'A', "uni4F01");
    font.encoding = Some(encoding);
    assert_eq!(font.code_to_gid(b'A' as u16), None);
}

#[test]
fn malformed_program_preserves_pdf_widths_without_inventing_glyphs() {
    let mut widths = CidWidths::new(600.0);
    widths.set(65, 750.0);
    let font = LoadedFont::new_with_data(
        PdfFontType::TrueType,
        "Malformed".into(),
        b"not a font program".to_vec(),
        widths,
    );
    assert!(!font.has_font_data());
    assert!(font.glyph_outline(65).is_none());
    assert_eq!(font.simple_glyph_advance(65, 65), 750.0);
    assert_eq!(font.simple_glyph_advance(66, 66), 600.0);
}
