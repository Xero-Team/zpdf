//! Glyph selection follows PDF advances and transforms, not Unicode length.

use zpdf_content::{
    interpreter::ContentInterpreter,
    search::search_spans,
    text::{TextGlyph, TextSpan},
};
use zpdf_core::{ParseLimits, Point, Rect};
use zpdf_display_list::{DisplayList, RenderCommand};
use zpdf_font::{
    cmap::{CidCMap, ToUnicodeMap},
    FontCache, LoadedFont, PdfFontType,
};

fn font() -> LoadedFont {
    let mut font = LoadedFont::new_standard("Helvetica".into()).unwrap();
    font.ascent = 800.0;
    font.descent = -200.0;
    font.cid_widths.set(b'W' as u16, 900.0);
    font.cid_widths.set(b'i' as u16, 200.0);
    font.cid_widths.set(b' ' as u16, 300.0);
    font
}

fn extract(font: LoadedFont, content: &[u8]) -> (DisplayList, Vec<TextSpan>) {
    let mut cache = FontCache::new();
    cache.insert("F1".into(), font);
    let mut spans = Vec::new();
    let list = ContentInterpreter::new(Rect::new(0.0, 0.0, 300.0, 300.0))
        .with_fonts(&mut cache)
        .with_text_sink(&mut spans)
        .interpret(content);
    (list, spans)
}

fn near(actual: f64, expected: f64) {
    assert!((actual - expected).abs() < 1e-4, "{actual} != {expected}");
}

fn point(actual: Point, x: f64, y: f64) {
    near(actual.x, x);
    near(actual.y, y);
}

fn rect(actual: Rect, expected: Rect) {
    near(actual.x0, expected.x0);
    near(actual.y0, expected.y0);
    near(actual.x1, expected.x1);
    near(actual.y1, expected.y1);
}

#[test]
fn unequal_widths_drive_selection_and_search() {
    let (_, spans) = extract(font(), b"BT /F1 10 Tf 20 50 Td (WiW) Tj ET");
    let span = &spans[0];
    assert_eq!(span.glyphs.len(), 3);
    assert_eq!(span.glyphs[1].text_range, 1..2);
    point(span.glyphs[1].origin, 29.0, 50.0);
    point(span.glyphs[1].advance, 2.0, 0.0);
    rect(span.glyphs[1].bounds(), Rect::new(29.0, 48.0, 31.0, 58.0));
    let glyph = span.glyph_at(Point::new(30.0, 53.0)).unwrap();
    assert_eq!(&span.text[glyph.text_range.clone()], "i");
    assert_eq!(
        span.glyph_at(Point::new(27.0, 53.0)).unwrap().text_range,
        0..1
    );
    rect(search_spans(&spans, "i", true)[0].bounds(), glyph.bounds());
}

#[test]
fn spacing_scaling_and_tj_offsets_keep_spaces_selectable() {
    let (_, spans) = extract(
        font(),
        b"BT /F1 10 Tf 20 50 Td 2 Tc 3 Tw 50 Tz [(W ) -200 (i)] TJ ET",
    );
    point(spans[0].glyphs[0].origin, 20.0, 50.0);
    point(spans[0].glyphs[0].advance, 4.5, 0.0);
    point(spans[0].glyphs[1].origin, 25.5, 50.0);
    point(spans[0].glyphs[1].advance, 1.5, 0.0);
    point(spans[1].glyphs[0].origin, 30.5, 50.0);
    let space = spans[0].glyph_at(Point::new(26.0, 52.0)).unwrap();
    assert_eq!(&spans[0].text[space.text_range.clone()], " ");
    let hits = search_spans(&spans, "W i", true);
    assert_eq!(hits.len(), 1);
    rect(hits[0].bounds(), Rect::new(20.0, 48.0, 31.5, 58.0));
}

#[test]
fn rotation_ctm_and_rise_transform_all_four_corners() {
    let (list, spans) = extract(
        font(),
        b"2 0 0 3 4 5 cm BT /F1 10 Tf 3 Ts 0 1 -1 0 100 20 Tm (Wi) Tj ET",
    );
    let glyph = &spans[0].glyphs[0];
    point(glyph.origin, 198.0, 65.0);
    point(glyph.advance, 0.0, 27.0);
    for (actual, expected) in
        glyph
            .quad
            .iter()
            .zip([(202.0, 65.0), (202.0, 92.0), (182.0, 92.0), (182.0, 65.0)])
    {
        point(*actual, expected.0, expected.1);
    }
    rect(
        search_spans(&spans, "i", true)[0].bounds(),
        Rect::new(182.0, 92.0, 202.0, 98.0),
    );
    let run = list
        .commands
        .iter()
        .find_map(|c| match c {
            RenderCommand::DrawGlyphRun(run) => Some(run),
            _ => None,
        })
        .unwrap();
    for (drawn, extracted) in run.glyphs.iter().zip(&spans[0].glyphs) {
        assert_eq!(
            Point::new(drawn.x as f64, drawn.y as f64).transform(&run.transform),
            extracted.origin,
        );
    }
}

#[test]
fn mirrored_and_sheared_cells_use_the_quad_for_hit_testing() {
    let (_, spans) = extract(font(), b"BT /F1 10 Tf -1 0.5 0.5 1 50 50 Tm (Wi) Tj ET");
    let glyph = &spans[0].glyphs[0];
    point(glyph.advance, -9.0, 4.5);
    assert!(glyph.contains(Point::new(47.0, 55.25)));
    // Inside the axis-aligned bounds, but outside the parallelogram.
    let bounds = glyph.bounds();
    assert!(!glyph.contains(Point::new(bounds.x0 + 0.01, bounds.y1 - 0.01)));
    rect(
        search_spans(&spans, "i", true)[0].bounds(),
        spans[0].glyphs[1].bounds(),
    );
}

#[test]
fn multibyte_unicode_and_ligatures_keep_source_glyph_ranges() {
    let mut font = font();
    font.to_unicode = Some(ToUnicodeMap::parse(
        b"3 beginbfchar <57> <00660069> <69> <4E2D> <21> <D83DDE00> endbfchar",
    ));
    let (_, spans) = extract(font, b"BT /F1 10 Tf 20 50 Td <57016921> Tj ET");
    let span = &spans[0];
    assert_eq!(span.text, "fi中😀");
    assert_eq!(span.glyphs.len(), 4);
    assert_eq!(span.glyphs[0].text_range, 0..2);
    assert_eq!(span.glyphs[1].text_range, 2..2); // unmapped control still advances
    assert_eq!(span.glyphs[2].text_range, 2..5);
    assert_eq!(span.glyphs[3].text_range, 5..9);
    for query in ["f", "i", "fi"] {
        rect(
            search_spans(&spans, query, true)[0].bounds(),
            span.glyphs[0].bounds(),
        );
    }
    rect(
        search_spans(&spans, "中", true)[0].bounds(),
        span.glyphs[2].bounds(),
    );
    rect(
        search_spans(&spans, "😀", true)[0].bounds(),
        span.glyphs[3].bounds(),
    );
}

#[test]
fn variable_length_cmap_and_cid_widths_share_the_render_segmentation() {
    let mut font = font();
    font.font_type = PdfFontType::Type0CidType2;
    font.cid_cmap = Some(CidCMap::parse(
        b"
        2 begincodespacerange <00> <7F> <8100> <81FF> endcodespacerange
        2 begincidchar <41> 10 <8140> 20 endcidchar",
    ));
    font.to_unicode = Some(ToUnicodeMap::parse(
        b"
        2 beginbfchar <41> <0041> <8140> <4E2D> endbfchar",
    ));
    font.cid_widths.set(10, 200.0);
    font.cid_widths.set(20, 900.0);
    let (_, spans) = extract(font, b"BT /F1 10 Tf 20 50 Td <41814041> Tj ET");
    assert_eq!(spans[0].text, "A中A");
    assert_eq!(spans[0].glyphs.len(), 3);
    assert_eq!(spans[0].glyphs[1].text_range, 1..4);
    rect(
        search_spans(&spans, "中", true)[0].bounds(),
        Rect::new(22.0, 48.0, 31.0, 58.0),
    );
}

#[test]
fn vertical_w2_and_dw2_place_each_glyph_independently() {
    let mut font = font();
    font.font_type = PdfFontType::Type0CidType2;
    font.cid_cmap = Some(CidCMap::identity(1));
    font.to_unicode = Some(ToUnicodeMap::parse(
        b"2 beginbfchar <0001> <4E2D> <0002> <6587> endbfchar",
    ));
    font.cid_widths.set(1, 800.0);
    font.cid_widths.set(2, 600.0);
    font.cid_widths.set_v(1, -900.0, 200.0, 700.0);
    let (_, spans) = extract(font, b"BT /F1 10 Tf 20 50 Td 1 Tc 50 Tz <00010002> Tj ET");
    let glyphs = &spans[0].glyphs;
    assert_eq!(spans[0].text, "中文");
    point(glyphs[0].origin, 18.0, 43.0);
    point(glyphs[0].advance, 0.0, -9.0);
    point(glyphs[1].origin, 17.0, 33.2);
    point(glyphs[1].advance, 0.0, -10.0);
    rect(glyphs[0].bounds(), Rect::new(18.0, 41.0, 22.0, 51.0));
    rect(
        search_spans(&spans, "文", true)[0].bounds(),
        Rect::new(17.0, 31.2, 20.0, 41.2),
    );
}

#[test]
fn actual_text_retains_all_replaced_geometry_as_one_cluster() {
    let (_, spans) = extract(
        font(),
        b"BT /F1 10 Tf 20 50 Td
        /Span << /MCID 7 /ActualText (replacement) >> BDC
        (W) Tj /Span << /ActualText (inner) >> BDC (i) Tj EMC (W) Tj EMC ET",
    );
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].text, "replacement");
    assert_eq!(spans[0].mcid, Some(7));
    assert_eq!(spans[0].glyphs.len(), 3);
    assert!(spans[0].glyphs.iter().all(|g| g.text_range == (0..11)));
    rect(
        search_spans(&spans, "place", true)[0].bounds(),
        Rect::new(20.0, 48.0, 40.0, 58.0),
    );
    assert_eq!(
        spans[0]
            .glyph_at(Point::new(30.0, 52.0))
            .unwrap()
            .text_range,
        0..11
    );
}

#[test]
fn invisible_text_still_has_geometry_and_sink_does_not_change_rendering() {
    let content = b"BT /F1 10 Tf 20 50 Td (W) Tj 3 Tr (i) Tj ET";
    let (with, spans) = extract(font(), content);
    assert_eq!(spans.len(), 2);
    rect(
        search_spans(&spans, "i", true)[0].bounds(),
        Rect::new(29.0, 48.0, 31.0, 58.0),
    );
    let mut cache = FontCache::new();
    cache.insert("F1".into(), font());
    let without = ContentInterpreter::new(Rect::new(0.0, 0.0, 300.0, 300.0))
        .with_fonts(&mut cache)
        .interpret(content);
    assert_eq!(
        format!("{:?}", with.commands),
        format!("{:?}", without.commands)
    );
}

#[test]
fn missing_font_keeps_filtered_codes_aligned_and_degenerate_cells_do_not_hit() {
    let mut spans = Vec::new();
    ContentInterpreter::new(Rect::new(0.0, 0.0, 100.0, 100.0))
        .with_text_sink(&mut spans)
        .interpret(b"BT /Missing 10 Tf <014169> Tj 0 Tz (W) Tj ET");
    assert_eq!(spans[0].text, "Ai");
    assert_eq!(spans[0].glyphs[0].text_range, 0..0);
    point(spans[0].glyphs[1].origin, 6.0, 0.0);
    assert_eq!(spans[1].glyph_at(spans[1].glyphs[0].origin), None);
    assert!(!spans[0].glyphs[1].contains(Point::new(f64::NAN, 0.0)));
}

#[test]
fn search_keeps_text_but_only_highlights_selectable_cells() {
    let (_, spans) = extract(font(), b"BT /F1 10 Tf 1 1 1 1 20 50 Tm (W) Tj ET");
    let hit = &search_spans(&spans, "W", true)[0];
    assert_eq!((hit.start, hit.len), (0, 1));
    assert!(hit.rects.is_empty());
    assert_eq!(hit.bounds(), Rect::new(0.0, 0.0, 0.0, 0.0));

    let (_, mut spans) = extract(font(), b"BT /F1 10 Tf (Wi) Tj ET");
    spans[0].glyphs[0].quad[2].x = f64::NAN;
    assert!(search_spans(&spans, "W", true)[0].rects.is_empty());
    rect(
        search_spans(&spans, "Wi", true)[0].bounds(),
        spans[0].glyphs[1].bounds(),
    );
}

#[test]
fn type0_to_unicode_fallback_maps_all_text_to_the_drawn_cells() {
    let mut font = font();
    font.font_type = PdfFontType::Type0CidType2;
    font.to_unicode = Some(ToUnicodeMap::parse(
        b"
        1 begincodespacerange <00> <FF> endcodespacerange
        2 beginbfchar <57> <0057> <69> <4E2D> endbfchar",
    ));
    let expected = font.decode_to_string(b"WiW");
    let (list, spans) = extract(font, b"BT /F1 10 Tf (WiW) Tj ET");
    assert_eq!(expected, "W中W");
    assert_eq!(spans[0].text, expected);
    let glyphs = &spans[0].glyphs;
    assert_eq!(glyphs.len(), 2);
    assert_eq!(glyphs[0].text_range, 0..4);
    assert_eq!(glyphs[1].text_range, 4..5);
    rect(
        search_spans(&spans, "中", true)[0].bounds(),
        glyphs[0].bounds(),
    );
    assert!(list
        .commands
        .iter()
        .any(|cmd| matches!(cmd, RenderCommand::DrawGlyphRun(run)
        if run.glyphs.iter().map(|g| g.code).eq([0x5769, 0x57]))));
}

#[test]
fn type0_three_byte_unicode_codes_share_intersecting_drawn_cells() {
    let mut font = font();
    font.font_type = PdfFontType::Type0CidType2;
    font.to_unicode = Some(ToUnicodeMap::parse(
        b"
        1 begincodespacerange <000000> <FFFFFF> endcodespacerange
        3 beginbfchar <010203> <0041> <040506> <0042> <0708> <0043> endbfchar",
    ));
    let (_, spans) = extract(font, b"BT /F1 10 Tf <0102030405060708> Tj ET");
    assert_eq!(spans[0].text, "ABC");
    assert_eq!(
        spans[0]
            .glyphs
            .iter()
            .map(|g| g.text_range.clone())
            .collect::<Vec<_>>(),
        [0..2, 0..2, 0..2, 2..3]
    );
}

#[test]
fn actual_text_keeps_unmapped_shows_and_cells_inside_empty_replacements() {
    let (_, spans) = extract(
        font(),
        b"BT /F1 10 Tf 20 50 Td
        /Span << /ActualText (A) >> BDC <01> Tj
          /Span << /ActualText () >> BDC (i) Tj EMC
        EMC <01> Tj /Span << /ActualText () >> BDC (W) Tj EMC ET",
    );
    assert_eq!(spans.len(), 1);
    let span = &spans[0];
    assert_eq!(span.text, "A");
    assert_eq!(span.glyphs.len(), 2);
    for glyph in &span.glyphs {
        let b = glyph.bounds();
        assert_eq!(
            span.glyph_at(Point::new((b.x0 + b.x1) / 2.0, (b.y0 + b.y1) / 2.0))
                .unwrap()
                .text_range,
            0..1
        );
    }
    let highlight = search_spans(&spans, "A", true)[0].bounds();
    near(highlight.x0, span.glyphs[0].bounds().x0);
    near(highlight.x1, span.glyphs[1].bounds().x1);
}

#[test]
fn page_geometry_budget_keeps_text_and_never_publishes_a_partial_cluster() {
    let limits = ParseLimits {
        max_decoded_stream_bytes: 2 * size_of::<TextGlyph>() as u64,
        ..Default::default()
    };
    let mut cache = FontCache::new();
    cache.insert("F1".into(), font());
    let mut spans = Vec::new();
    let (_, stats) = ContentInterpreter::new(Rect::new(0.0, 0.0, 300.0, 300.0))
        .with_limits(&limits)
        .with_fonts(&mut cache)
        .with_text_sink(&mut spans)
        .interpret_with_stats(
            b"BT /F1 10 Tf (W) Tj /Span << /ActualText (A) >> BDC
            (i) Tj (WW) Tj EMC (i) Tj ET",
        );
    assert!(stats.diagnostics.resource_limit_reached);
    assert_eq!(
        spans.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(),
        ["W", "A", "i"]
    );
    assert_eq!(
        spans.iter().map(|s| s.glyphs.len()).collect::<Vec<_>>(),
        [1, 0, 0]
    );
    near(search_spans(&spans, "A", true)[0].bounds().width(), 20.0);
}

#[test]
fn expired_text_deadline_reports_timeout_without_publishing_a_partial_show() {
    let mut cache = FontCache::new();
    cache.insert("F1".into(), font());
    for content in [
        &b"BT /F1 10 Tf (Wi) Tj ET"[..],
        b"BT /F1 10 Tf /Span << /ActualText (A) >> BDC [(Wi) (W)] TJ EMC ET",
    ] {
        let mut spans = Vec::new();
        let (_, stats) = ContentInterpreter::new(Rect::new(0.0, 0.0, 300.0, 300.0))
            .with_fonts(&mut cache)
            .with_text_sink(&mut spans)
            .with_deadline(zpdf_core::time::Instant::now())
            .interpret_with_stats(content);
        assert!(stats.diagnostics.time_limit_reached);
        assert!(spans.is_empty());
    }
}

#[test]
fn unicode_expansion_and_replacements_share_the_page_text_byte_limit() {
    let mut font = font();
    font.to_unicode = Some(ToUnicodeMap::parse(
        b"1 beginbfchar <57> <0057006F00720064> endbfchar",
    ));
    let mut cache = FontCache::new();
    cache.insert("F1".into(), font);
    let limits = ParseLimits {
        max_decoded_stream_bytes: 7,
        ..Default::default()
    };
    let mut spans = Vec::new();
    let (_, stats) = ContentInterpreter::new(Rect::new(0.0, 0.0, 300.0, 300.0))
        .with_limits(&limits)
        .with_fonts(&mut cache)
        .with_text_sink(&mut spans)
        .interpret_with_stats(
            b"BT /F1 10 Tf (W) Tj /Span << /ActualText (X) >> BDC
            (W) Tj EMC [(iii) (i)] TJ ET",
        );
    assert!(stats.diagnostics.resource_limit_reached);
    assert_eq!(
        spans.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(),
        ["Word", "X"]
    );
}
