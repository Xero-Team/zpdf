//! Regression tests for GitHub issue #157: when an embedded font program
//! (`FontFile`, `FontFile2`, or `FontFile3`) fails to decode or parse, zpdf
//! must fall back to the same substitute font path used for non-embedded fonts,
//! preserve PDF placement (`/Widths`, `/W`, `/DW`), draw visible non-blank
//! glyphs on both CPU and GPU backends, and record the fallback in page
//! diagnostics without emitting it for valid embedded or non-embedded fonts.

#![cfg(feature = "cpu-render")]

use std::sync::Arc;

use zpdf::display_list::RenderCommand;
use zpdf::{FontProvider, PdfDocument, ProcessingDiagnostics, RenderBackend};

const PRIMARY_TTF: &[u8] = include_bytes!("../../zpdf-font/tests/fixtures/coverage-primary.ttf");
const BOLD_TTF: &[u8] = include_bytes!("../../zpdf-font/tests/fixtures/coverage-bold.ttf");
const TYPE1_PFA: &[u8] = include_bytes!("../../zpdf-font/tests/fixtures/coverage-type1.pfa");
const CID_OTF: &[u8] = include_bytes!("../../zpdf-font/tests/fixtures/coverage-cid.otf");
const CAD_BROKEN_TTF: &[u8] =
    include_bytes!("../../zpdf-font/tests/fixtures/cad-subset-broken.ttf");

fn assemble(objects: &[Vec<u8>]) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (index, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    let count = objects.len() + 1;
    out.extend_from_slice(format!("xref\n0 {count}\n0000000000 65535 f \n").as_bytes());
    for offset in &offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<< /Size {count} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
    );
    out
}

fn stream(dict: &str, bytes: &[u8]) -> Vec<u8> {
    let mut out = format!("<< {dict} /Length {} >>\nstream\n", bytes.len()).into_bytes();
    out.extend_from_slice(bytes);
    out.extend_from_slice(b"\nendstream");
    out
}

/// Find the byte range of an sfnt table in `ttf`.
fn sfnt_table_range(ttf: &[u8], tag: &[u8; 4]) -> std::ops::Range<usize> {
    let num_tables = u16::from_be_bytes([ttf[4], ttf[5]]) as usize;
    for i in 0..num_tables {
        let rec = 12 + i * 16;
        if &ttf[rec..rec + 4] == tag {
            let off = u32::from_be_bytes([ttf[rec + 8], ttf[rec + 9], ttf[rec + 10], ttf[rec + 11]])
                as usize;
            let len =
                u32::from_be_bytes([ttf[rec + 12], ttf[rec + 13], ttf[rec + 14], ttf[rec + 15]])
                    as usize;
            return off..off + len;
        }
    }
    panic!("table {:?} missing from fixture", std::str::from_utf8(tag));
}

/// Overwrite the `glyf` table in place with `fill`, keeping sfnt headers valid
/// so `ttf_parser::Face::parse` succeeds while glyph outlining fails.
fn corrupt_glyf_in_place(ttf: &[u8], fill: u8) -> Vec<u8> {
    let mut out = ttf.to_vec();
    let range = sfnt_table_range(&out, b"glyf");
    out[range].fill(fill);
    out
}

/// Truncate the sfnt halfway into its `glyf` table so the table directory still
/// declares `glyf`, but `ttf-parser` drops `tables.glyf` as out-of-bounds.
fn truncate_inside_glyf(ttf: &[u8]) -> Vec<u8> {
    let range = sfnt_table_range(ttf, b"glyf");
    ttf[..range.start + (range.len() / 2).max(1)].to_vec()
}

/// Simple font PDF drawing `"ABC"` at (15, 40) with 24pt and distinct PDF
/// `/Widths` `[620, 780, 540]` (different from the fixture's 1000-unit `hmtx`).
fn simple_font_pdf(
    subtype: &str,
    base_font: &str,
    descriptor_extras: &str,
    font_file_key: Option<&str>,
    font_stream_dict: &str,
    font_bytes: &[u8],
) -> Vec<u8> {
    let ff_entry = font_file_key
        .map(|key| format!("/{key} 7 0 R"))
        .unwrap_or_default();
    let mut objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        stream("", b"0 0 0 rg BT /F1 24 Tf 15 40 Td (ABC) Tj ET"),
        format!(
            "<< /Type /Font /Subtype /{subtype} /BaseFont /{base_font} \
             /Encoding /WinAnsiEncoding /FirstChar 65 /LastChar 67 /Widths [620 780 540] \
             /FontDescriptor 6 0 R >>"
        )
        .into_bytes(),
        format!(
            "<< /Type /FontDescriptor /FontName /{base_font} {descriptor_extras} {ff_entry} >>"
        )
        .into_bytes(),
    ];
    if font_file_key.is_some() {
        objects.push(stream(font_stream_dict, font_bytes));
    }
    assemble(&objects)
}

/// Type0 / CID font PDF drawing CIDs 1, 2 (`"AB"` via `/ToUnicode`) with `/W`
/// `[1 [650 820]]`.
fn type0_font_pdf(
    cid_subtype: &str,
    base_font: &str,
    font_file_key: Option<&str>,
    font_bytes: &[u8],
) -> Vec<u8> {
    let ff_entry = font_file_key
        .map(|key| format!("/{key} 8 0 R"))
        .unwrap_or_default();
    let to_unicode = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
        1 begincodespacerange <0000> <FFFF> endcodespacerange \
        2 beginbfchar <0001> <0041> <0002> <0042> endbfchar \
        endcmap CMapName currentdict /CMap defineresource pop end end";
    let mut objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        stream("", b"0 0 0 rg BT /F1 24 Tf 15 40 Td <00010002> Tj ET"),
        format!(
            "<< /Type /Font /Subtype /Type0 /BaseFont /{base_font} /Encoding /Identity-H \
             /DescendantFonts [6 0 R] /ToUnicode 9 0 R >>"
        )
        .into_bytes(),
        format!(
            "<< /Type /Font /Subtype /{cid_subtype} /BaseFont /{base_font} \
             /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
             /FontDescriptor 7 0 R /DW 1000 /W [1 [650 820]] >>"
        )
        .into_bytes(),
        format!("<< /Type /FontDescriptor /FontName /{base_font} /Flags 32 {ff_entry} >>")
            .into_bytes(),
    ];
    if font_file_key.is_some() {
        objects.push(stream("", font_bytes));
    } else {
        objects.push(b"<< >>".to_vec());
    }
    objects.push(stream("", to_unicode));
    assemble(&objects)
}

fn provider_with_alias(alias: &str) -> Arc<FontProvider> {
    let provider = Arc::new(FontProvider::memory_only());
    provider
        .register_with_aliases(PRIMARY_TTF, 0, &[alias])
        .unwrap();
    provider
}

fn dark_pixels_in_rect(
    rgba: &[u8],
    width: usize,
    height: usize,
    x0: usize,
    x1: usize,
    y0: usize,
    y1: usize,
) -> usize {
    let mut count = 0;
    for y in y0.min(height)..y1.min(height) {
        for x in x0.min(width)..x1.min(width) {
            let idx = (y * width + x) * 4;
            if rgba[idx] < 128 && rgba[idx + 3] > 0 {
                count += 1;
            }
        }
    }
    count
}

fn assert_rendered_cells_have_ink(
    interpreted: &zpdf::InterpretedPage,
    expected_glyph_xs: &[f32],
    expected_advances: &[f32],
    label: &str,
) {
    let runs: Vec<_> = interpreted
        .display_list
        .commands
        .iter()
        .filter_map(|cmd| match cmd {
            RenderCommand::DrawGlyphRun(run) => Some(run),
            _ => None,
        })
        .collect();
    assert_eq!(runs.len(), 1, "{label}: expected one glyph run");
    let run = runs[0];
    assert_eq!(
        run.glyphs.len(),
        expected_glyph_xs.len(),
        "{label}: glyph count"
    );
    for (idx, ((glyph, &exp_x), &exp_adv)) in run
        .glyphs
        .iter()
        .zip(expected_glyph_xs)
        .zip(expected_advances)
        .enumerate()
    {
        assert!(
            (glyph.x - exp_x).abs() < 0.01,
            "{label}: glyph {idx} x={} expected {exp_x}",
            glyph.x
        );
        assert!(
            (glyph.advance - exp_adv).abs() < 0.01,
            "{label}: glyph {idx} advance={} expected {exp_adv}",
            glyph.advance
        );
    }

    let cpu = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&interpreted.fonts)
        .with_images(&interpreted.images)
        .render_display_list(&interpreted.display_list, 1.0)
        .expect("cpu render");
    for (idx, (&gx, &adv)) in expected_glyph_xs.iter().zip(expected_advances).enumerate() {
        // Text starts at page x=15, y=40 (100pt tall page -> pixel y in ~35..65).
        let px0 = (15.0 + gx).floor().max(0.0) as usize;
        let px1 = (15.0 + gx + adv).ceil() as usize;
        let ink = dark_pixels_in_rect(
            &cpu.data,
            cpu.width as usize,
            cpu.height as usize,
            px0,
            px1,
            30,
            70,
        );
        assert!(
            ink > 10,
            "{label}: CPU glyph {idx} cell x={px0}..{px1} rendered blank ({ink} dark px)"
        );
    }

    #[cfg(feature = "gpu-render")]
    {
        let mut gpu = zpdf::gpu::WgpuRenderer::new()
            .with_fonts(&interpreted.fonts)
            .with_images(&interpreted.images);
        match gpu.adapter_identity() {
            Ok(_) => {
                let rendered = gpu
                    .render_display_list(&interpreted.display_list, 1.0)
                    .expect("gpu render");
                for (idx, (&gx, &adv)) in
                    expected_glyph_xs.iter().zip(expected_advances).enumerate()
                {
                    let px0 = (15.0 + gx).floor().max(0.0) as usize;
                    let px1 = (15.0 + gx + adv).ceil() as usize;
                    let ink = dark_pixels_in_rect(
                        &rendered.data,
                        rendered.width as usize,
                        rendered.height as usize,
                        px0,
                        px1,
                        30,
                        70,
                    );
                    assert!(
                        ink > 10,
                        "{label}: GPU glyph {idx} cell x={px0}..{px1} rendered blank ({ink} dark px)"
                    );
                }
            }
            Err(zpdf::gpu::WgpuRenderError::NoAdapter) => {}
            Err(err) => panic!("{label}: unexpected GPU adapter error: {err}"),
        }
    }
}

#[test]
fn damaged_glyf_and_corrupt_truetype_fall_back_with_pdf_placement_and_diagnostic() {
    let cases: &[(&str, Vec<u8>)] = &[
        (
            "glyf overwritten with 0xFF",
            corrupt_glyf_in_place(PRIMARY_TTF, 0xFF),
        ),
        (
            "glyf overwritten with 0x00",
            corrupt_glyf_in_place(PRIMARY_TTF, 0x00),
        ),
        ("truncated inside glyf", truncate_inside_glyf(PRIMARY_TTF)),
        (
            "garbage FontFile2 bytes",
            b"not a valid font program".to_vec(),
        ),
        ("empty FontFile2 stream", Vec::new()),
    ];
    let expected_adv = [
        620.0 * 24.0 / 1000.0,
        780.0 * 24.0 / 1000.0,
        540.0 * 24.0 / 1000.0,
    ];
    let expected_x = [0.0, expected_adv[0], expected_adv[0] + expected_adv[1]];

    for (label, corrupt_bytes) in cases {
        let pdf = simple_font_pdf(
            "TrueType",
            "ABCDEF+BrokenTTF",
            "/Flags 32",
            Some("FontFile2"),
            "",
            corrupt_bytes,
        );
        let doc = PdfDocument::open(pdf)
            .expect("open")
            .with_font_provider(provider_with_alias("BrokenTTF"));
        let page = zpdf::interpret_page(&doc, 0, Default::default()).expect("interpret");
        let font = page.fonts.get_by_name("F1").expect("F1").1;
        assert!(font.is_substitute, "{label}: expected substitute font");
        assert!(
            font.has_font_data(),
            "{label}: substitute must have outlines"
        );

        assert_rendered_cells_have_ink(&page, &expected_x, &expected_adv, label);

        assert_eq!(
            page.stats.diagnostics.embedded_font_fallbacks, 1,
            "{label}: fallback must be recorded"
        );
        assert_eq!(page.stats.diagnostics.first_affected_page, Some(0));
        assert_eq!(
            page.stats.diagnostics.first_failed_object,
            Some(zpdf::ObjectId(7, 0))
        );
        assert!(
            !page.is_complete(),
            "{label}: fallback marks page incomplete"
        );
        assert!(page.ensure_complete().is_err());
        assert!(page
            .stats
            .diagnostics
            .to_json()
            .contains("\"embedded_font_fallbacks\":1"));
    }
}

#[test]
fn corrupt_type1_cff_and_undecodable_font_streams_fall_back_and_record_diagnostics() {
    struct Case {
        label: &'static str,
        subtype: &'static str,
        key: &'static str,
        stream_dict: &'static str,
        bytes: Vec<u8>,
        expected_decode_failures: u32,
    }
    let cases = [
        Case {
            label: "corrupt Type1 FontFile",
            subtype: "Type1",
            key: "FontFile",
            stream_dict: "",
            bytes: b"%!PS-AdobeFont-1.0: Broken\ncurrentfile eexec\n00000000".to_vec(),
            expected_decode_failures: 0,
        },
        Case {
            label: "corrupt raw CFF FontFile3",
            subtype: "Type1",
            key: "FontFile3",
            stream_dict: "/Subtype /Type1C",
            bytes: vec![0x01, 0x00, 0x04, 0x04, 0xFF, 0xFF, 0xFF, 0xFF],
            expected_decode_failures: 0,
        },
        Case {
            label: "corrupt OTF FontFile3",
            subtype: "TrueType",
            key: "FontFile3",
            stream_dict: "/Subtype /OpenType",
            bytes: b"OTTO\x00\x01truncated".to_vec(),
            expected_decode_failures: 0,
        },
        Case {
            label: "undecodable FontFile2 filter",
            subtype: "TrueType",
            key: "FontFile2",
            stream_dict: "/Filter /UnsupportedDecode",
            bytes: b"corrupt compressed bytes".to_vec(),
            expected_decode_failures: 1,
        },
    ];
    let expected_adv = [
        620.0 * 24.0 / 1000.0,
        780.0 * 24.0 / 1000.0,
        540.0 * 24.0 / 1000.0,
    ];
    let expected_x = [0.0, expected_adv[0], expected_adv[0] + expected_adv[1]];

    for case in &cases {
        let pdf = simple_font_pdf(
            case.subtype,
            "ABCDEF+BrokenSimple",
            "/Flags 32",
            Some(case.key),
            case.stream_dict,
            &case.bytes,
        );
        let doc = PdfDocument::open(pdf)
            .expect("open")
            .with_font_provider(provider_with_alias("BrokenSimple"));
        let page = zpdf::interpret_page(&doc, 0, Default::default()).expect("interpret");
        assert!(
            page.fonts.get_by_name("F1").unwrap().1.is_substitute,
            "{}: expected substitute",
            case.label
        );
        assert_rendered_cells_have_ink(&page, &expected_x, &expected_adv, case.label);
        assert_eq!(
            page.stats.diagnostics.embedded_font_fallbacks, 1,
            "{}: expected embedded_font_fallbacks=1",
            case.label
        );
        assert_eq!(
            page.stats.diagnostics.decode_failures, case.expected_decode_failures,
            "{}: decode_failures",
            case.label
        );
    }
}

#[test]
fn corrupt_type0_cid_fonts_fall_back_preserving_widths_text_and_rendering() {
    let cases: &[(&str, &str, &str, Vec<u8>)] = &[
        (
            "Type0 CIDFontType2 damaged glyf",
            "CIDFontType2",
            "FontFile2",
            corrupt_glyf_in_place(PRIMARY_TTF, 0xFF),
        ),
        (
            "Type0 CIDFontType2 garbage bytes",
            "CIDFontType2",
            "FontFile2",
            b"corrupt cid truetype".to_vec(),
        ),
        (
            "Type0 CIDFontType0 corrupt raw CFF",
            "CIDFontType0",
            "FontFile3",
            vec![0x01, 0x00, 0x04, 0x04, 0xFF, 0x00, 0x00, 0x00],
        ),
    ];
    let expected_adv = [650.0 * 24.0 / 1000.0, 820.0 * 24.0 / 1000.0];
    let expected_x = [0.0, expected_adv[0]];

    for (label, cid_subtype, key, bytes) in cases {
        let pdf = type0_font_pdf(cid_subtype, "ABCDEF+BrokenCID", Some(key), bytes);
        let doc = PdfDocument::open(pdf)
            .expect("open")
            .with_font_provider(provider_with_alias("BrokenCID"));
        let page = zpdf::interpret_page(&doc, 0, Default::default()).expect("interpret");
        let font = page.fonts.get_by_name("F1").expect("F1").1;
        assert!(font.is_substitute, "{label}: expected substitute");
        assert_rendered_cells_have_ink(&page, &expected_x, &expected_adv, label);
        assert_eq!(page.stats.diagnostics.embedded_font_fallbacks, 1, "{label}");
        assert!(!page.is_complete(), "{label}");

        let converted = zpdf::convert_pdf(&doc, &[0], Default::default()).expect("convert");
        assert_eq!(
            converted.pages[0].text.trim(),
            "AB",
            "{label}: text extraction"
        );
        assert_eq!(converted.pages[0].diagnostics.embedded_font_fallbacks, 1);
    }
}

#[test]
fn valid_embedded_and_non_embedded_fonts_record_no_fallback_diagnostic() {
    // Valid embedded TrueType
    let valid_ttf_pdf = simple_font_pdf(
        "TrueType",
        "ABCDEF+ValidTTF",
        "/Flags 32",
        Some("FontFile2"),
        "",
        PRIMARY_TTF,
    );
    let doc = PdfDocument::open(valid_ttf_pdf)
        .unwrap()
        .with_font_provider(provider_with_alias("ValidTTF"));
    let page = zpdf::interpret_page(&doc, 0, Default::default()).unwrap();
    assert!(!page.fonts.get_by_name("F1").unwrap().1.is_substitute);
    assert!(page.is_complete());
    assert_eq!(page.stats.diagnostics, ProcessingDiagnostics::default());

    // Valid embedded Type 1
    let valid_t1_pdf = simple_font_pdf(
        "Type1",
        "ABCDEF+ValidT1",
        "/Flags 32",
        Some("FontFile"),
        "",
        TYPE1_PFA,
    );
    let doc = PdfDocument::open(valid_t1_pdf)
        .unwrap()
        .with_font_provider(provider_with_alias("ValidT1"));
    let page = zpdf::interpret_page(&doc, 0, Default::default()).unwrap();
    assert!(!page.fonts.get_by_name("F1").unwrap().1.is_substitute);
    assert!(page.fonts.get_by_name("F1").unwrap().1.type1.is_some());
    assert!(page.is_complete());
    assert_eq!(page.stats.diagnostics, ProcessingDiagnostics::default());

    // Valid embedded CID OpenType/CFF
    let valid_cid_pdf = type0_font_pdf(
        "CIDFontType0",
        "ABCDEF+ValidCID",
        Some("FontFile3"),
        CID_OTF,
    );
    let doc = PdfDocument::open(valid_cid_pdf)
        .unwrap()
        .with_font_provider(provider_with_alias("ValidCID"));
    let page = zpdf::interpret_page(&doc, 0, Default::default()).unwrap();
    assert!(!page.fonts.get_by_name("F1").unwrap().1.is_substitute);
    assert!(page.is_complete());
    assert_eq!(page.stats.diagnostics, ProcessingDiagnostics::default());

    // Normal non-embedded font (no FontFile* entry) must NOT trigger embedded fallback diagnostic
    let non_embedded_pdf =
        simple_font_pdf("TrueType", "NonEmbeddedFace", "/Flags 32", None, "", &[]);
    let doc = PdfDocument::open(non_embedded_pdf)
        .unwrap()
        .with_font_provider(provider_with_alias("NonEmbeddedFace"));
    let page = zpdf::interpret_page(&doc, 0, Default::default()).unwrap();
    assert!(page.fonts.get_by_name("F1").unwrap().1.is_substitute);
    assert!(page.is_complete());
    assert_eq!(page.stats.diagnostics.embedded_font_fallbacks, 0);

    // Zero-length `loca` CAD broken subset must keep its embedded program for CAD sibling repair
    let cad_pdf = type0_font_pdf(
        "CIDFontType2",
        "ABCDEF+CadBroken",
        Some("FontFile2"),
        CAD_BROKEN_TTF,
    );
    let doc = PdfDocument::open(cad_pdf)
        .unwrap()
        .with_font_provider(provider_with_alias("CadBroken"));
    let page = zpdf::interpret_page(&doc, 0, Default::default()).unwrap();
    assert!(!page.fonts.get_by_name("F1").unwrap().1.is_substitute);
    assert!(page.is_complete());
    assert_eq!(page.stats.diagnostics.embedded_font_fallbacks, 0);
}

#[test]
fn descriptor_weight_selects_bold_substitute_when_embedded_program_fails() {
    let provider = Arc::new(FontProvider::memory_only());
    provider
        .register_with_aliases(PRIMARY_TTF, 0, &["WeightTest"])
        .unwrap();
    provider
        .register_with_aliases(BOLD_TTF, 0, &["WeightTest"])
        .unwrap();

    // Name carries no "Bold" token; only /FontDescriptor /FontWeight 700 asks for bold.
    let pdf = simple_font_pdf(
        "TrueType",
        "ABCDEF+WeightTest",
        "/Flags 32 /FontWeight 700",
        Some("FontFile2"),
        "",
        b"corrupt font bytes",
    );
    let doc = PdfDocument::open(pdf).unwrap().with_font_provider(provider);
    let page = zpdf::interpret_page(&doc, 0, Default::default()).unwrap();
    let font = page.fonts.get_by_name("F1").unwrap().1;
    assert!(font.is_substitute);
    assert_eq!(
        font.font_data.as_deref().unwrap(),
        BOLD_TTF,
        "/FontWeight 700 should select the bold substitute face"
    );
    assert_eq!(page.stats.diagnostics.embedded_font_fallbacks, 1);
}

#[test]
fn form_xobjects_multipage_shared_fonts_and_hidden_layers_report_diagnostics_accurately() {
    let corrupt = corrupt_glyf_in_place(PRIMARY_TTF, 0xFF);
    // Two pages sharing corrupt font 6 0 R: page 0 draws it twice directly,
    // page 1 draws it through Form XObject 8 0 R, and page 2 leaves it unused
    // (draws only valid font 9 0 R) + hidden under OC.
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R] /Count 3 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F1 6 0 R >> >> /Contents 11 0 R >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /XObject << /Fm 8 0 R >> >> /Contents 12 0 R >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F1 6 0 R /F2 9 0 R >> >> /Contents 13 0 R >>".to_vec(),
        b"<< /Type /Font /Subtype /TrueType /BaseFont /ABCDEF+SharedCorrupt /Encoding /WinAnsiEncoding /FirstChar 65 /LastChar 67 /Widths [620 780 540] /FontDescriptor 7 0 R >>".to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+SharedCorrupt /Flags 32 /FontFile2 14 0 R >>".to_vec(),
        stream(
            "/Type /XObject /Subtype /Form /BBox [0 0 180 100] /Resources << /Font << /G1 6 0 R >> >>",
            b"0 0 0 rg BT /G1 24 Tf 15 40 Td (AB) Tj ET",
        ),
        b"<< /Type /Font /Subtype /TrueType /BaseFont /ABCDEF+ValidFace /Encoding /WinAnsiEncoding /FontDescriptor 10 0 R >>".to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+ValidFace /Flags 32 /FontFile2 15 0 R >>".to_vec(),
        stream("", b"0 0 0 rg BT /F1 24 Tf 15 40 Td (A) Tj (B) Tj ET"),
        stream("", b"/Fm Do"),
        stream(
            "",
            b"0 0 0 rg BT /F2 24 Tf 15 40 Td (A) Tj ET /OC << /Type /OCMD /VE [/Not << /Type /OCG >>] >> BDC BT /F1 24 Tf (B) Tj ET EMC",
        ),
        stream("", &corrupt),
        stream("", PRIMARY_TTF),
    ]);

    let doc = PdfDocument::open(pdf)
        .unwrap()
        .with_font_provider(provider_with_alias("SharedCorrupt"));

    // Page 0: two Tj calls with the same corrupt font -> counted once.
    let p0 = zpdf::interpret_page(&doc, 0, Default::default()).unwrap();
    assert_eq!(p0.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(p0.stats.diagnostics.first_affected_page, Some(0));

    // Page 1: shared font reused via Form XObject -> still draws visibly and reports fallback.
    let p1 = zpdf::interpret_page(&doc, 1, Default::default()).unwrap();
    assert_eq!(p1.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(p1.stats.diagnostics.first_affected_page, Some(1));
    let cpu1 = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&p1.fonts)
        .render_display_list(&p1.display_list, 1.0)
        .unwrap();
    assert!(
        dark_pixels_in_rect(
            &cpu1.data,
            cpu1.width as usize,
            cpu1.height as usize,
            10,
            80,
            30,
            70
        ) > 20,
        "form XObject fallback text should render visibly"
    );

    // Page 2: corrupt font is only under a hidden OC layer -> complete and clean.
    let page2 = doc.page(2).unwrap();
    let mut fonts2 = doc.load_page_fonts(&page2);
    let oc = zpdf::OcConfig::default();
    let (_, stats2) = zpdf::ContentInterpreter::new(page2.effective_box())
        .with_fonts(&mut fonts2)
        .with_document(doc.file(), &page2.resources)
        .with_optional_content(&oc)
        .interpret_with_stats(&doc.page_content_bytes(&page2).unwrap());
    assert!(stats2.diagnostics.is_complete(), "{:?}", stats2.diagnostics);
}

#[test]
fn corrupt_embedded_font_without_any_substitute_never_panics_and_preserves_placement() {
    let pdf = simple_font_pdf(
        "TrueType",
        "ABCDEF+NoSubstituteAvailable",
        "/Flags 32",
        Some("FontFile2"),
        "",
        &corrupt_glyf_in_place(PRIMARY_TTF, 0xFF),
    );
    let doc = PdfDocument::open(pdf)
        .unwrap()
        .with_font_provider(Arc::new(FontProvider::memory_only()));
    let page = zpdf::interpret_page(&doc, 0, Default::default()).unwrap();
    let font = page.fonts.get_by_name("F1").unwrap().1;
    assert!(!font.has_font_data());
    assert_eq!(page.stats.diagnostics.embedded_font_fallbacks, 1);

    let RenderCommand::DrawGlyphRun(run) = &page.display_list.commands[0] else {
        panic!("expected glyph run");
    };
    assert!((run.glyphs[0].advance - 620.0 * 24.0 / 1000.0).abs() < 0.01);
    assert!((run.glyphs[1].advance - 780.0 * 24.0 / 1000.0).abs() < 0.01);
    assert!((run.glyphs[2].advance - 540.0 * 24.0 / 1000.0).abs() < 0.01);

    let converted = zpdf::convert_pdf(&doc, &[0], Default::default()).unwrap();
    assert_eq!(converted.pages[0].text.trim(), "ABC");
    assert_eq!(converted.pages[0].diagnostics.embedded_font_fallbacks, 1);
}

#[test]
fn default_system_font_substitution_renders_corrupt_embedded_font() {
    if zpdf_font::system::find_system_font("Helvetica", Default::default(), None).is_none() {
        eprintln!("skipping: no system substitute for Helvetica installed");
        return;
    }
    let pdf = simple_font_pdf(
        "TrueType",
        "ABCDEF+Helvetica",
        "/Flags 32",
        Some("FontFile2"),
        "",
        &corrupt_glyf_in_place(PRIMARY_TTF, 0xFF),
    );
    let doc = PdfDocument::open(pdf).expect("open");
    let page = zpdf::interpret_page(&doc, 0, Default::default()).expect("interpret");
    let font = page.fonts.get_by_name("F1").unwrap().1;
    assert!(font.is_substitute);
    assert!(font.has_font_data());
    assert_eq!(page.stats.diagnostics.embedded_font_fallbacks, 1);

    let expected_adv = [
        620.0 * 24.0 / 1000.0,
        780.0 * 24.0 / 1000.0,
        540.0 * 24.0 / 1000.0,
    ];
    let expected_x = [0.0, expected_adv[0], expected_adv[0] + expected_adv[1]];
    assert_rendered_cells_have_ink(
        &page,
        &expected_x,
        &expected_adv,
        "system Helvetica fallback",
    );
}

#[test]
fn corrupt_simple_font_with_custom_encoding_recovers_via_tounicode_and_cid_collection_fallback() {
    // 1. Simple TrueType subset with custom /Differences (/g1, /g2) + /ToUnicode
    //    and a damaged embedded FontFile2: substitute resolves glyphs via /ToUnicode
    //    while preserving PDF /Widths.
    let to_unicode = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
        1 begincodespacerange <00> <FF> endcodespacerange \
        2 beginbfchar <01> <0041> <02> <0042> endbfchar \
        endcmap CMapName currentdict /CMap defineresource pop end end";
    let custom_simple_pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        stream("", b"0 0 0 rg BT /F1 24 Tf 15 40 Td <0102> Tj ET"),
        b"<< /Type /Font /Subtype /TrueType /BaseFont /ABCDEF+SubsetCustom \
          /Encoding << /Type /Encoding /Differences [1 /g1 /g2] >> \
          /FirstChar 1 /LastChar 2 /Widths [650 820] \
          /FontDescriptor 6 0 R /ToUnicode 8 0 R >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+SubsetCustom /Flags 4 /FontFile2 7 0 R >>"
            .to_vec(),
        stream("", &corrupt_glyf_in_place(PRIMARY_TTF, 0xFF)),
        stream("", to_unicode),
    ]);
    let doc = PdfDocument::open(custom_simple_pdf)
        .unwrap()
        .with_font_provider(provider_with_alias("SubsetCustom"));
    let page = zpdf::interpret_page(&doc, 0, Default::default()).unwrap();
    let expected_adv = [650.0 * 24.0 / 1000.0, 820.0 * 24.0 / 1000.0];
    let expected_x = [0.0, expected_adv[0]];
    assert_rendered_cells_have_ink(
        &page,
        &expected_x,
        &expected_adv,
        "custom-encoding simple ToUnicode fallback",
    );
    assert_eq!(page.stats.diagnostics.embedded_font_fallbacks, 1);

    // 2. Type0 Adobe-GB1 font without /ToUnicode whose embedded FontFile2 is
    //    corrupt: ordering/collection selects the substitute and resolves CID 34
    //    (Adobe-GB1 'A') via the canonical Adobe-GB1 collection.
    let provider = Arc::new(FontProvider::memory_only());
    provider
        .register_with_aliases(PRIMARY_TTF, 0, &["SimSun"])
        .unwrap();
    let gb1_pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        stream("", b"0 0 0 rg BT /F1 24 Tf 15 40 Td <00220023> Tj ET"),
        b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+UnknownGB1Font \
          /Encoding /Identity-H /DescendantFonts [6 0 R] >>"
            .to_vec(),
        b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+UnknownGB1Font \
          /CIDSystemInfo << /Registry (Adobe) /Ordering (GB1) /Supplement 4 >> \
          /FontDescriptor 7 0 R /DW 1000 /W [34 [650 820]] >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+UnknownGB1Font /Flags 32 /FontFile2 8 0 R >>"
            .to_vec(),
        stream("", b"corrupt gb1 embedded font"),
    ]);
    let doc = PdfDocument::open(gb1_pdf)
        .unwrap()
        .with_font_provider(provider);
    let page = zpdf::interpret_page(&doc, 0, Default::default()).unwrap();
    assert!(page.fonts.get_by_name("F1").unwrap().1.is_substitute);
    assert_rendered_cells_have_ink(
        &page,
        &expected_x,
        &expected_adv,
        "Adobe-GB1 CID collection fallback",
    );
    assert_eq!(page.stats.diagnostics.embedded_font_fallbacks, 1);
}
