//! Bounded recovery for malformed CAD text fonts (PR #15): broken subsets
//! drawn per glyph from a validated page font, and fonts leaked across text
//! blocks overridden for one operator. Every repair needs evidence gathered
//! during interpretation and stops when its page budget is spent.

use std::collections::HashMap;
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

/// A simple font drawing all printable ASCII (it has no glyph for a tab).
fn latin(base: &str) -> LoadedFont {
    LoadedFont::new_with_data(
        PdfFontType::TrueType,
        base.into(),
        PRIMARY.to_vec(),
        CidWidths::new(1000.0),
    )
}

/// A TrueType program with `count` drawn glyphs, built at test time. As a
/// full Identity-H font it gives almost every pair of Latin bytes a glyph.
fn many_glyph_font(count: u16) -> Vec<u8> {
    let (mut glyf, mut loca) = (Vec::new(), Vec::new());
    for _ in 0..count {
        loca.extend_from_slice(&(glyf.len() as u32).to_be_bytes());
        // One contour of three on-curve points: header, end point, no
        // instructions, flags, then 16-bit x and y deltas.
        for value in [1i16, 0, 0, 500, 500, 2, 0] {
            glyf.extend_from_slice(&value.to_be_bytes());
        }
        glyf.extend_from_slice(&[1, 1, 1]);
        for value in [0i16, 500, -250, 0, 0, 500] {
            glyf.extend_from_slice(&value.to_be_bytes());
        }
        glyf.resize(glyf.len().next_multiple_of(4), 0);
    }
    loca.extend_from_slice(&(glyf.len() as u32).to_be_bytes());
    let mut head = vec![0; 54];
    head[..4].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    head[12..16].copy_from_slice(&0x5F0F_3CF5u32.to_be_bytes());
    head[18..20].copy_from_slice(&1000u16.to_be_bytes());
    head[50..52].copy_from_slice(&1u16.to_be_bytes());
    let mut hhea = vec![0; 36];
    hhea[..4].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    hhea[4..6].copy_from_slice(&900i16.to_be_bytes());
    hhea[6..8].copy_from_slice(&(-200i16).to_be_bytes());
    hhea[34..36].copy_from_slice(&1u16.to_be_bytes());
    let mut maxp = 0x0000_5000u32.to_be_bytes().to_vec();
    maxp.extend_from_slice(&count.to_be_bytes());
    let mut hmtx = vec![0; 2 + 2 * count as usize];
    hmtx[..2].copy_from_slice(&1000u16.to_be_bytes());
    let tables: [(&[u8; 4], &[u8]); 6] = [
        (b"glyf", &glyf),
        (b"head", &head),
        (b"hhea", &hhea),
        (b"hmtx", &hmtx),
        (b"loca", &loca),
        (b"maxp", &maxp),
    ];
    let mut font = 0x0001_0000u32.to_be_bytes().to_vec();
    font.extend_from_slice(&(tables.len() as u16).to_be_bytes());
    font.extend_from_slice(&[0; 6]);
    let mut offset = 12 + 16 * tables.len();
    for (tag, data) in &tables {
        font.extend_from_slice(*tag);
        font.extend_from_slice(&[0; 4]);
        font.extend_from_slice(&(offset as u32).to_be_bytes());
        font.extend_from_slice(&(data.len() as u32).to_be_bytes());
        offset += data.len().next_multiple_of(4);
    }
    for (_, data) in &tables {
        font.extend_from_slice(data);
        font.resize(font.len().next_multiple_of(4), 0);
    }
    font
}

/// 32,768 glyphs: every pair of printable ASCII bytes is a valid CID.
fn large() -> &'static [u8] {
    static LARGE: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    LARGE.get_or_init(|| many_glyph_font(0x8000))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02X}")).collect()
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

#[test]
fn latin_text_beside_a_large_identity_font_is_never_redirected() {
    // Every pair of printable ASCII bytes is a drawable CID of T0, and the
    // page has shown each of them with T0: only the shape of leaked bytes
    // (lead bytes the simple font cannot draw) separates a leak from text.
    let mut page = new_page(vec![
        (
            "T0",
            type0(large(), "Large", b"1 beginbfchar <4142> <4E2D> endbfchar"),
        ),
        ("S0", latin("ABCDEF+Latin")),
    ]);
    let (t0, s0) = (page.ids[0], page.ids[1]);
    let pairs: Vec<u8> = (0x20..0x7F)
        .flat_map(|lead| (0x20..0x7F).flat_map(move |trail| [lead, trail]))
        .collect();
    let mut content = format!("BT /T0 10 Tf <{}> Tj ET BT /S0 10 Tf ", hex(&pairs)).into_bytes();
    let lines: [&[u8]; 8] = [
        b"AB\tC",
        b"\tCzz",
        b"Total\t1,234,567.89 USD",
        b"Item\tQuantity\tUnit price\tAmount",
        b"\t\t",
        b"\r\n\t ",
        b"x\ty",
        b"See also: Appendix B, pages 12-15\tfor details",
    ];
    for line in lines {
        content.extend_from_slice(format!("<{}> Tj ", hex(line)).as_bytes());
    }
    content.extend_from_slice(b"ET");
    let (list, stats, spans) = page.run(&content);
    let runs = fonts_and_glyphs(&list);
    assert_eq!(runs.len(), 1 + lines.len());
    assert_eq!(runs[0].0, t0);
    assert!(runs[1..].iter().all(|(font, _)| *font == s0), "{runs:?}");
    assert_eq!(stats.font_repairs, 0);
    // Extraction keeps the Latin text (the tab is dropped, as before).
    assert_eq!(spans[1].text, "ABC");
}

#[test]
fn shows_under_an_override_are_not_evidence() {
    let mut page = new_page(vec![
        ("T0", type0(CAD, "Gost", TO_UNICODE)),
        ("S0", simple("Simple")),
    ]);
    let (t0, s0) = (page.ids[0], page.ids[1]);
    // The first leaked show has half of its codes in T0's evidence and is
    // repaired. Its own codes must not become evidence for the second.
    let content = b"BT /T0 10 Tf <0002> Tj ET BT /S0 10 Tf <00020003> Tj <00030004> Tj ET";
    let (list, stats, _) = page.run(content);
    assert_eq!(
        fonts_and_glyphs(&list),
        vec![(t0, vec![2]), (t0, vec![2, 3]), (s0, vec![0, 0, 0, 0])]
    );
    assert_eq!(stats.font_repairs, 1);
}

#[test]
fn leak_candidates_must_draw_the_codes_they_mean() {
    // A blank glyph is acceptable only where the candidate's own meaning for
    // the code is whitespace; visible or unknown meanings need an outline.
    for to_unicode in [TO_UNICODE, b"".as_slice()] {
        let mut page = new_page(vec![
            ("T0", type0(CAD_BROKEN, "Iso", to_unicode)),
            ("S0", simple("Simple")),
        ]);
        let s0 = page.ids[1];
        let (list, stats, _) = page.run(b"BT /T0 10 Tf <0002> Tj ET BT /S0 10 Tf <00020002> Tj ET");
        assert_eq!(fonts_and_glyphs(&list)[1].0, s0);
        assert_eq!(stats.font_repairs, 0);
    }
    let mut page = new_page(vec![
        ("T0", type0(CAD, "Gost", TO_UNICODE)),
        ("S0", simple("Simple")),
    ]);
    let t0 = page.ids[0];
    let (list, stats, _) = page.run(b"BT /T0 10 Tf <0002> Tj ET BT /S0 10 Tf <00020001> Tj ET");
    assert_eq!(fonts_and_glyphs(&list)[1], (t0, vec![2, 1]), "blank space");
    assert_eq!(stats.font_repairs, 1);
}

#[test]
fn type0_codes_outside_a_narrow_codespace_stay_composite_text() {
    // The CMap maps codes beyond its declared codespace; lenient decoding
    // draws them, as before, even with a same-name simple font on the page.
    let mut composite = type0(
        large(),
        "ABCDEF+Large",
        b"1 beginbfchar <4142> <4E2D> endbfchar",
    );
    composite.cid_cmap = Some(CidCMap::parse(
        b"1 begincodespacerange <0000> <3FFF> endcodespacerange \
          1 begincidrange <0000> <FFFF> 0 endcidrange",
    ));
    let mut page = new_page(vec![("T0", composite), ("S0", latin("GHIJKL+Large"))]);
    let (t0, s0) = (page.ids[0], page.ids[1]);
    let gid = |byte: u8| {
        page.fonts
            .get(s0)
            .unwrap()
            .code_to_gid(byte as u16)
            .unwrap()
    };
    let expected_simple = [gid(b'x'), gid(b'A'), gid(b'B'), gid(b'C')];
    let (list, stats, spans) =
        page.run(b"BT /S0 10 Tf (x) Tj /T0 10 Tf <4142> Tj (AB) Tj (ABC) Tj ET");
    assert_eq!(
        fonts_and_glyphs(&list),
        vec![
            (s0, expected_simple[..1].to_vec()),
            (t0, vec![0x4142]),
            (t0, vec![0x4142]),
            // A trailing fragment shorter than any code is leaked text.
            (s0, expected_simple[1..].to_vec()),
        ]
    );
    assert_eq!(stats.font_repairs, 1);
    assert_eq!(spans[1].text, "中");
}

#[test]
fn thousands_of_aliasing_font_names_bound_sibling_work() {
    let good = Arc::new(type0(CAD, "Gost", TO_UNICODE));
    let broken = Arc::new(type0(CAD_BROKEN, "Iso", TO_UNICODE));
    let mut fonts = FontCache::new();
    let mut resources = ResourceDict::default();
    let mut alias = |name: String, font: &Arc<LoadedFont>| {
        resources.fonts.insert(name.clone(), ObjectId(1, 0));
        fonts.insert_shared(name, Arc::clone(font));
    };
    alias("A".into(), &good);
    for index in 0..64 {
        alias(format!("B{index:04}"), &broken);
    }
    for index in 0..4000 {
        alias(format!("Z{index:06}"), &good);
    }
    let mut page = Page {
        fonts,
        ids: Vec::new(),
        resources,
    };
    let mut content = Vec::new();
    for index in 0..64 {
        content.extend_from_slice(
            format!("BT /B{index:04} 10 Tf <00020003000400050001> Tj ET ").as_bytes(),
        );
    }
    let (_, stats, _) = page.run(&content);
    // One candidate list for the page (distinct programs only): 1 + 4065/16
    // probes, then one probe per decision for 64 fonts × 4 letters.
    assert_eq!(stats.font_repairs, 256);
    assert_eq!(stats.font_repair_probes, 1 + 4065 / 16 + 256);
}

#[test]
fn broken_simple_subsets_are_repaired_per_glyph_too() {
    let to_unicode = b"3 beginbfchar <20> <0020> <41> <0041> <42> <0042> endbfchar";
    let mut fonts = Vec::new();
    for (name, program) in [("Broken", CAD_BROKEN), ("Good", CAD)] {
        let mut font = LoadedFont::new_with_data(
            PdfFontType::TrueType,
            name.into(),
            program.to_vec(),
            CidWidths::new(1000.0),
        );
        font.to_unicode = Some(ToUnicodeMap::parse(to_unicode));
        fonts.push((name, font));
    }
    let mut page = new_page(fonts);
    let (broken, good) = (page.ids[0], page.ids[1]);
    let (list, stats, spans) = page.run(b"BT /Broken 10 Tf (A B) Tj ET");
    assert_eq!(
        fonts_and_glyphs(&list),
        vec![(good, vec![2]), (broken, vec![1]), (good, vec![3])]
    );
    assert_eq!(stats.font_repairs, 2);
    assert_eq!(spans[0].text, "A B");
}

#[test]
fn one_code_outside_the_collection_keeps_the_type0_operator() {
    // UniJIS-UCS2-H codes are Unicode scalars; U+4E66 (书) has no
    // Adobe-Japan1 CID while the rest of the line does. Every byte of these
    // codes is printable ASCII, so the same-name simple font could draw the
    // whole operator.
    let cmap = CidCMap::predefined("UniJIS-UCS2-H").unwrap();
    let codes: Vec<u16> = "中字本年大山水书".encode_utf16().collect();
    assert_eq!(cmap.code_to_cid(0x4E66, 2), 0);
    assert!(codes[..7]
        .iter()
        .all(|&code| cmap.code_to_cid(code as u32, 2) != 0));
    assert!(codes
        .iter()
        .flat_map(|code| code.to_be_bytes())
        .all(|byte| (0x20..0x7F).contains(&byte)));
    let mut composite = type0(large(), "ABCDEF+Large", b"");
    composite.cid_cmap = Some(cmap);
    let mut page = new_page(vec![("T0", composite), ("S0", latin("GHIJKL+Large"))]);
    let t0 = page.ids[0];
    let bytes: Vec<u8> = codes.iter().flat_map(|code| code.to_be_bytes()).collect();
    let content = format!("BT /S0 10 Tf (x) Tj /T0 10 Tf <{}> Tj ET", hex(&bytes));
    let (list, stats, _) = page.run(content.as_bytes());
    let runs = fonts_and_glyphs(&list);
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[1].0, t0, "one missing character is not leaked text");
    assert_eq!(runs[1].1.len(), 8);
    assert_eq!(runs[1].1[7], 0, "the missing character keeps .notdef");
    assert_eq!(stats.font_repairs, 0);
}

#[test]
fn one_cid_missing_from_the_cid_map_keeps_the_type0_operator() {
    let mut composite = type0(
        large(),
        "ABCDEF+Large",
        b"3 beginbfchar <4142> <4E2D> <4344> <6587> <4546> <5B57> endbfchar",
    );
    composite.cid_to_gid = Some(HashMap::from([(0x4142, 0x4142), (0x4344, 0x4344)]));
    let mut page = new_page(vec![("T0", composite), ("S0", latin("GHIJKL+Large"))]);
    let t0 = page.ids[0];
    let (list, stats, spans) = page.run(b"BT /S0 10 Tf (x) Tj /T0 10 Tf <414243444546> Tj ET");
    assert_eq!(
        fonts_and_glyphs(&list)[1],
        (t0, vec![0x4142, 0x4344, 0x4546])
    );
    assert_eq!(stats.font_repairs, 0);
    assert_eq!(spans[1].text, "中文字");
}

#[test]
fn a_type0_show_with_no_drawable_code_is_still_leaked_simple_text() {
    let mut composite = type0(CAD, "ABCDEF+ZpdfCadSubset", TO_UNICODE);
    composite.cid_to_gid = Some(HashMap::from([(2, 2), (3, 3)]));
    let mut page = new_page(vec![
        ("C2", composite),
        ("TT", simple("GHIJKL+ZpdfCadSubset")),
    ]);
    let (c2, tt) = (page.ids[0], page.ids[1]);
    // Even-length `AB` decodes to one code, 0x4142, which has no glyph.
    let (list, stats, spans) = page.run(b"BT /TT 10 Tf (D) Tj /C2 10 Tf <0002> Tj (AB) Tj ET");
    assert_eq!(
        fonts_and_glyphs(&list),
        vec![(tt, vec![5]), (c2, vec![2]), (tt, vec![2, 3])]
    );
    assert_eq!(stats.font_repairs, 1);
    assert_eq!(spans[2].text, "AB");
}

/// A realistic page with a system CJK font: Chinese text in a full
/// Identity-H font and English lines with tabs in a simple font. Opt-in,
/// since it needs a font from macOS.
#[test]
#[ignore]
fn system_font_probe_latin_lines_on_a_chinese_page() {
    let Ok(arial) = std::fs::read("/System/Library/Fonts/Supplemental/Arial Unicode.ttf") else {
        return;
    };
    let gbk = CidCMap::predefined("GBK-EUC-H").unwrap();
    let full = type0(&arial, "ArialUnicodeMS", b"");
    let mut cids = Vec::new();
    for lead in 0xB0u32..=0xD7 {
        for trail in 0xA1u32..=0xFE {
            if let Some(gid) = gbk
                .decode_to_unicode((lead << 8) | trail, 2)
                .and_then(|unicode| full.unicode_glyph(unicode))
            {
                cids.extend_from_slice(&gid.0.to_be_bytes());
            }
        }
    }
    let mut page = new_page(vec![
        ("T0", type0(&arial, "ArialUnicodeMS", b"")),
        (
            "S0",
            LoadedFont::new_with_data(
                PdfFontType::TrueType,
                "ABCDEF+Latin".into(),
                arial.clone(),
                CidWidths::new(1000.0),
            ),
        ),
    ]);
    let s0 = page.ids[1];
    let mut content = format!("BT /T0 10 Tf <{}> Tj ET BT /S0 10 Tf ", hex(&cids)).into_bytes();
    for line in [
        "Annual report 2024\tsummary of operations",
        "Revenue grew by 12 percent over the year.\t",
        "Contact: sales@example.com\tPhone 555-0100",
        "Table 4: Results of the 2023 survey\t(n = 1,024)",
    ] {
        content.extend_from_slice(format!("<{}> Tj ", hex(line.as_bytes())).as_bytes());
    }
    content.extend_from_slice(b"ET");
    let (list, stats, _) = page.run(&content);
    assert!(fonts_and_glyphs(&list)[1..]
        .iter()
        .all(|(font, _)| *font == s0));
    assert_eq!(stats.font_repairs, 0);
}
