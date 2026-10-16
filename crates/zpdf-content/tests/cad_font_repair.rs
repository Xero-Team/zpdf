//! Bounded recovery for malformed CAD text fonts (PR #15): broken subsets
//! drawn per glyph from a validated page font, and fonts leaked across text
//! blocks overridden for one operator. Every repair needs evidence gathered
//! during interpretation and stops when its page budget is spent.

use std::sync::Arc;

use zpdf_content::interpreter::ContentInterpreter;
use zpdf_content::stats::InterpretStats;
use zpdf_content::text::TextSpan;
use zpdf_core::{ObjectId, Rect};
use zpdf_display_list::{DisplayList, RenderCommand};
use zpdf_document::page::ResourceDict;
use zpdf_font::cmap::{CidCMap, ToUnicodeMap};
use zpdf_font::{CidWidths, FontCache, FontId, FontProvider, LoadedFont, PdfFontType};
use zpdf_parser::PdfFile;

/// Glyph n of both fixtures: 1 = space, 2..=5 = `A`..`D`, 6 = `Ж`. The broken
/// copy keeps the glyph ids but every `loca` entry has zero length.
const CAD: &[u8] = include_bytes!("../../zpdf-font/tests/fixtures/cad-subset.ttf");
const CAD_BROKEN: &[u8] = include_bytes!("../../zpdf-font/tests/fixtures/cad-subset-broken.ttf");
const PRIMARY: &[u8] = include_bytes!("../../zpdf-font/tests/fixtures/coverage-primary.ttf");

const TO_UNICODE: &[u8] =
    b"5 beginbfchar <0001> <0020> <0002> <0041> <0003> <0042> <0004> <0043> <0005> <0044> endbfchar";

fn tiny_file() -> PdfFile {
    let mut pdf = b"%PDF-1.7\n".to_vec();
    let offset = pdf.len();
    pdf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog >>\nendobj\n");
    let xref = pdf.len();
    pdf.extend_from_slice(
        format!(
            "xref\n0 2\n0000000000 65535 f \n{offset:010} 00000 n \n\
             trailer\n<< /Size 2 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
        )
        .as_bytes(),
    );
    PdfFile::parse(pdf).expect("tiny pdf")
}

fn type0(program: &[u8], base: &str, to_unicode: &[u8]) -> LoadedFont {
    let mut font = LoadedFont::new_with_data(
        PdfFontType::Type0CidType2,
        base.into(),
        program.to_vec(),
        CidWidths::new(1000.0),
    );
    font.cid_cmap = Some(CidCMap::identity(0));
    font.to_unicode = Some(ToUnicodeMap::parse(to_unicode));
    font
}

fn simple(base: &str) -> LoadedFont {
    LoadedFont::new_with_data(
        PdfFontType::TrueType,
        base.into(),
        CAD.to_vec(),
        CidWidths::new(1000.0),
    )
}

struct Page {
    fonts: FontCache,
    ids: Vec<FontId>,
    resources: ResourceDict,
}

fn new_page(fonts: Vec<(&str, LoadedFont)>) -> Page {
    new_page_with(FontCache::new(), fonts)
}

fn new_page_with(mut cache: FontCache, fonts: Vec<(&str, LoadedFont)>) -> Page {
    let mut resources = ResourceDict::default();
    let mut ids = Vec::new();
    for (name, font) in fonts {
        resources.fonts.insert(name.to_string(), ObjectId(1, 0));
        ids.push(cache.insert(name.to_string(), font));
    }
    Page {
        fonts: cache,
        ids,
        resources,
    }
}

impl Page {
    fn run(&mut self, content: &[u8]) -> (DisplayList, InterpretStats, Vec<TextSpan>) {
        let file = tiny_file();
        let mut spans = Vec::new();
        let (list, stats) = ContentInterpreter::new(Rect::new(0.0, 0.0, 600.0, 600.0))
            .with_fonts(&mut self.fonts)
            .with_document(&file, &self.resources)
            .with_text_sink(&mut spans)
            .interpret_with_stats(content);
        (list, stats, spans)
    }
}

/// `(font, glyph ids, advances)` of each glyph run.
fn runs(list: &DisplayList) -> Vec<(FontId, Vec<u16>, Vec<f32>)> {
    list.commands
        .iter()
        .filter_map(|command| match command {
            RenderCommand::DrawGlyphRun(run) => Some((
                run.font_id,
                run.glyphs.iter().map(|glyph| glyph.glyph_id).collect(),
                run.glyphs.iter().map(|glyph| glyph.x).collect(),
            )),
            _ => None,
        })
        .collect()
}

fn fonts_and_glyphs(list: &DisplayList) -> Vec<(FontId, Vec<u16>)> {
    runs(list)
        .into_iter()
        .map(|(font, glyphs, _)| (font, glyphs))
        .collect()
}

#[test]
fn broken_subset_glyphs_draw_from_a_validated_sibling_keeping_positions() {
    let mut page = new_page(vec![
        ("Broken", type0(CAD_BROKEN, "ABCDEF+Iso", TO_UNICODE)),
        ("Good", type0(CAD, "GHIJKL+Gost", TO_UNICODE)),
    ]);
    let (broken, good) = (page.ids[0], page.ids[1]);
    let content = b"BT /Broken 10 Tf <000200010003> Tj ET BT /Good 10 Tf <0004> Tj ET";
    let (list, stats, spans) = page.run(content);
    let runs = runs(&list);
    // The genuine space stays in its own font; only blank letters move.
    assert_eq!(
        runs.iter()
            .map(|(font, glyphs, _)| (*font, glyphs.clone()))
            .collect::<Vec<_>>(),
        vec![
            (good, vec![2]),
            (broken, vec![1]),
            (good, vec![3]),
            (good, vec![4])
        ]
    );
    // PDF placement is the original font's: 1000 + 1000 units at 10 pt.
    let positions: Vec<f32> = runs[..3].iter().flat_map(|(.., x)| x.clone()).collect();
    assert_eq!(positions, vec![0.0, 10.0, 20.0]);
    assert_eq!(stats.font_repairs, 2);
    assert_eq!(spans[0].text, "A B");
}

#[test]
fn siblings_with_other_text_or_widths_are_rejected_and_healthy_fonts_untouched() {
    let other_text = type0(
        CAD,
        "Other",
        b"2 beginbfchar <0002> <0042> <0003> <0041> endbfchar",
    );
    let mut other_width = type0(CAD, "Wide", TO_UNICODE);
    other_width.cid_widths.set(2, 900.0);
    other_width.cid_widths.set(3, 900.0);
    for sibling in [other_text, other_width] {
        let mut page = new_page(vec![
            ("Broken", type0(CAD_BROKEN, "Iso", TO_UNICODE)),
            ("Sibling", sibling),
        ]);
        let broken = page.ids[0];
        let (list, stats, _) = page.run(b"BT /Broken 10 Tf <00020003> Tj ET");
        assert_eq!(fonts_and_glyphs(&list), vec![(broken, vec![2, 3])]);
        assert_eq!(stats.font_repairs, 0);
    }

    let mut page = new_page(vec![
        ("Good", type0(CAD, "Gost", TO_UNICODE)),
        ("Other", type0(CAD, "Other", TO_UNICODE)),
    ]);
    let good = page.ids[0];
    let (list, stats, _) = page.run(b"BT /Good 10 Tf <0002000100030006> Tj ET");
    assert_eq!(fonts_and_glyphs(&list), vec![(good, vec![2, 1, 3, 6])]);
    assert_eq!((stats.font_repairs, stats.font_repair_probes), (0, 0));
}

#[test]
fn provider_recovers_a_blank_letter_no_sibling_validates() {
    let provider = Arc::new(FontProvider::memory_only());
    provider.register(PRIMARY, 0).unwrap();
    let mut page = new_page_with(
        FontCache::new().with_provider(provider),
        vec![("Broken", type0(CAD_BROKEN, "Iso", TO_UNICODE))],
    );
    let broken = page.ids[0];
    let (list, ..) = page.run(b"BT /Broken 10 Tf <00020001> Tj ET");
    let runs = fonts_and_glyphs(&list);
    let primary = LoadedFont::new_with_data(
        PdfFontType::TrueType,
        "Primary".into(),
        PRIMARY.to_vec(),
        CidWidths::new(1000.0),
    );
    assert_eq!(runs.len(), 2);
    assert_ne!(runs[0].0, broken);
    assert_eq!(
        runs[0].1,
        vec![primary.unicode_glyph('A' as u32).unwrap().0]
    );
    assert_eq!(runs[1], (broken, vec![1]), "space keeps its blank glyph");

    // Invisible text (OCR layers) is never resolved or repaired.
    let (_, stats, _) = page.run(b"BT 3 Tr /Broken 10 Tf <00020003> Tj ET");
    assert_eq!((stats.font_repairs, stats.font_repair_probes), (0, 0));
}

#[test]
fn leaked_composite_codes_use_an_evidenced_type0_font_for_one_operator() {
    let mut page = new_page(vec![
        ("T0", type0(CAD, "Gost", TO_UNICODE)),
        ("S0", simple("Simple")),
    ]);
    let (t0, s0) = (page.ids[0], page.ids[1]);
    let content = b"BT /T0 10 Tf <0002> Tj ET \
                    BT /S0 10 Tf [<0003> -20 <0002>] TJ <0005> Tj (AB) Tj ET";
    let (list, stats, spans) = page.run(content);
    assert_eq!(
        fonts_and_glyphs(&list),
        vec![
            (t0, vec![2]),
            // The whole TJ is judged at once and shown with T0.
            (t0, vec![3]),
            (t0, vec![2]),
            // No code of this show was ever shown with T0: no evidence.
            (s0, vec![0, 0]),
            // The override never became the text state's font.
            (s0, vec![2, 3]),
        ]
    );
    assert_eq!(stats.font_repairs, 1);
    // Extraction follows the font the operator was shown with.
    let texts: Vec<&str> = spans.iter().map(|span| span.text.as_str()).collect();
    assert_eq!(&texts[..3], ["A", "B", "A"]);
}

#[test]
fn leaked_codes_need_the_candidate_cmap_to_decode_them_exactly() {
    // One- and two-byte codespaces: odd-length strings are ordinary text.
    let cmap = CidCMap::parse(
        b"2 begincodespacerange <00> <7F> <8000> <FFFF> endcodespacerange \
          2 begincidrange <41> <44> 2 <8141> <8144> 2 endcidrange",
    );
    let mut variable = type0(
        CAD,
        "ABCDEF+ZpdfCadSubset",
        b"2 beginbfchar <41> <0041> <8141> <0041> endbfchar",
    );
    variable.cid_cmap = Some(cmap);
    let mut page = new_page(vec![
        ("T0", variable),
        ("S0", simple("Simple")),
        ("TT", simple("GHIJKL+ZpdfCadSubset")),
    ]);
    let (t0, s0, tt) = (page.ids[0], page.ids[1], page.ids[2]);
    let content = b"BT /TT 10 Tf (D) Tj /T0 10 Tf <41> Tj <414243> Tj ET \
                    BT /S0 10 Tf <814141> Tj <4180> Tj ET";
    let (list, stats, _) = page.run(content);
    assert_eq!(
        fonts_and_glyphs(&list),
        vec![
            (tt, vec![5]),
            (t0, vec![2]),
            // Exactly decodable odd-length text keeps its Type0 font, even
            // though a simple font with the same base name was used.
            (t0, vec![2, 3, 4]),
            // `81 41 | 41` decodes exactly through T0's own CMap.
            (t0, vec![2, 2]),
            // `41 | 80` ends in a partial code: no repair.
            (s0, vec![2, 0]),
        ]
    );
    assert_eq!(stats.font_repairs, 1);
}

#[test]
fn leaked_simple_text_uses_the_simple_font_of_the_same_base_name() {
    let mut page = new_page(vec![
        ("C2", type0(CAD, "ABCDEF+ZpdfCadSubset", TO_UNICODE)),
        ("TT", simple("GHIJKL+ZpdfCadSubset")),
        ("Other", simple("Other")),
    ]);
    let (c2, tt, other) = (page.ids[0], page.ids[1], page.ids[2]);
    let content = b"BT /Other 10 Tf (D) Tj /TT 10 Tf (D) Tj /C2 10 Tf <0002> Tj ET \
                    BT (ABC) Tj ET BT <0003> Tj ET";
    let (list, stats, spans) = page.run(content);
    assert_eq!(
        fonts_and_glyphs(&list),
        vec![
            (other, vec![5]),
            (tt, vec![5]),
            (c2, vec![2]),
            (tt, vec![2, 3, 4]),
            (c2, vec![3])
        ]
    );
    assert_eq!(stats.font_repairs, 1);
    assert_eq!(spans[3].text, "ABC");

    // The sibling must have been used on the page and share the base name.
    for content in [
        b"BT /C2 10 Tf <0002> Tj (ABC) Tj ET".as_slice(),
        b"BT /Other 10 Tf (D) Tj /C2 10 Tf (ABC) Tj ET".as_slice(),
    ] {
        let mut page = new_page(vec![
            ("C2", type0(CAD, "ABCDEF+ZpdfCadSubset", TO_UNICODE)),
            ("TT", simple("GHIJKL+ZpdfCadSubset")),
            ("Other", simple("Other")),
        ]);
        let c2 = page.ids[0];
        let (list, stats, _) = page.run(content);
        assert_eq!(fonts_and_glyphs(&list).last().unwrap().0, c2);
        assert_eq!(stats.font_repairs, 0);
    }
}

#[test]
fn operand_only_and_oversized_shows_do_no_recovery_work() {
    let fonts = || {
        vec![
            ("T0", type0(CAD, "Gost", TO_UNICODE)),
            ("S0", simple("Simple")),
            ("Broken", type0(CAD_BROKEN, "Iso", TO_UNICODE)),
        ]
    };
    // A stream of operands with no operator never reaches a show.
    let mut page = new_page(fonts());
    let content = b"<0002> ".repeat(200_000);
    let (list, stats, _) = page.run(&content);
    assert!(fonts_and_glyphs(&list).is_empty());
    assert_eq!((stats.font_repairs, stats.font_repair_probes), (0, 0));

    // A leaked operator over the size cap is shown as written.
    let mut page = new_page(fonts());
    let s0 = page.ids[1];
    let mut content = b"BT /T0 10 Tf <0002> Tj ET BT /S0 10 Tf <".to_vec();
    content.extend(b"0002".repeat(600));
    content.extend_from_slice(b"> Tj ET");
    let (list, stats, _) = page.run(&content);
    assert_eq!(fonts_and_glyphs(&list)[1].0, s0);
    assert_eq!((stats.font_repairs, stats.font_repair_probes), (0, 0));
}

#[test]
fn recovery_stops_for_the_page_once_its_budget_is_spent() {
    let mut page = new_page(vec![
        ("T0", type0(CAD, "Gost", TO_UNICODE)),
        ("S0", simple("Simple")),
    ]);
    let (t0, s0) = (page.ids[0], page.ids[1]);
    // Each leaked show has evidence (code 2) but code 7 has no glyph, so
    // every attempt is validated and rejected.
    let mut content = b"BT /T0 10 Tf <0002> Tj ET BT /S0 10 Tf ".to_vec();
    content.extend(b"<00020007> Tj ".repeat(8_000));
    content.extend_from_slice(b"<0002> Tj ET");
    let (list, stats, _) = page.run(&content);
    let runs = fonts_and_glyphs(&list);
    assert_eq!(runs[0].0, t0);
    // The last show would qualify, but recovery is already disabled.
    assert_eq!(runs.last().unwrap(), &(s0, vec![0, 0]));
    assert_eq!(stats.font_repairs, 0);
    assert!(
        (16_384..16_384 + 8).contains(&stats.font_repair_probes),
        "{}",
        stats.font_repair_probes
    );
}
