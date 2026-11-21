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
const FALLBACK_TTF: &[u8] = include_bytes!("../../zpdf-font/tests/fixtures/coverage-fallback.ttf");
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
        assert_eq!(
            page.stats.diagnostics.decode_failures, 0,
            "{label}: recovered fallback must not report missing output"
        );
        assert_eq!(page.stats.diagnostics.first_affected_page, Some(0));
        assert_eq!(
            page.stats.diagnostics.first_failed_object,
            Some(zpdf::ObjectId(7, 0))
        );
        assert!(
            page.is_complete(),
            "{label}: complete substitute recovery keeps page complete"
        );
        page.ensure_complete().expect("recovered page is complete");
        let json = page.stats.diagnostics.to_json();
        assert!(json.contains("\"complete\":true"), "{json}");
        assert!(json.contains("\"embedded_font_fallbacks\":1"), "{json}");
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
    }
    let cases = [
        Case {
            label: "corrupt Type1 FontFile",
            subtype: "Type1",
            key: "FontFile",
            stream_dict: "",
            bytes: b"%!PS-AdobeFont-1.0: Broken\ncurrentfile eexec\n00000000".to_vec(),
        },
        Case {
            label: "corrupt raw CFF FontFile3",
            subtype: "Type1",
            key: "FontFile3",
            stream_dict: "/Subtype /Type1C",
            bytes: vec![0x01, 0x00, 0x04, 0x04, 0xFF, 0xFF, 0xFF, 0xFF],
        },
        Case {
            label: "corrupt OTF FontFile3",
            subtype: "TrueType",
            key: "FontFile3",
            stream_dict: "/Subtype /OpenType",
            bytes: b"OTTO\x00\x01truncated".to_vec(),
        },
        Case {
            label: "undecodable FontFile2 filter",
            subtype: "TrueType",
            key: "FontFile2",
            stream_dict: "/Filter /UnsupportedDecode",
            bytes: b"corrupt compressed bytes".to_vec(),
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
            page.stats.diagnostics.decode_failures, 0,
            "{}: complete substitute recovery should leave decode_failures=0",
            case.label
        );
        assert!(
            page.is_complete(),
            "{}: complete substitute recovery should pass ensure_complete",
            case.label
        );
        page.ensure_complete().expect("recovered page is complete");
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
        assert_eq!(page.stats.diagnostics.decode_failures, 0, "{label}");
        assert!(page.is_complete(), "{label}");
        page.ensure_complete()
            .expect("recovered Type0 page is complete");

        let converted = zpdf::convert_pdf(&doc, &[0], Default::default()).expect("convert");
        assert_eq!(
            converted.pages[0].text.trim(),
            "AB",
            "{label}: text extraction"
        );
        assert_eq!(converted.pages[0].diagnostics.embedded_font_fallbacks, 1);
        assert!(converted.is_complete(), "{label}");
        converted
            .ensure_complete()
            .expect("recovered conversion is complete");
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
    assert_eq!(page.stats.diagnostics.decode_failures, 0);
    assert!(page.is_complete());
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

    // Page 0: two Tj calls with the same corrupt font -> counted once and complete.
    let p0 = zpdf::interpret_page(&doc, 0, Default::default()).unwrap();
    assert_eq!(p0.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(p0.stats.diagnostics.decode_failures, 0);
    assert_eq!(p0.stats.diagnostics.first_affected_page, Some(0));
    assert!(p0.is_complete());

    // Page 1: shared font reused via Form XObject -> still draws visibly and reports fallback.
    let p1 = zpdf::interpret_page(&doc, 1, Default::default()).unwrap();
    assert_eq!(p1.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(p1.stats.diagnostics.decode_failures, 0);
    assert_eq!(p1.stats.diagnostics.first_affected_page, Some(1));
    assert!(p1.is_complete());
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
        b"not a parseable sfnt at all",
    );
    let doc = PdfDocument::open(pdf)
        .unwrap()
        .with_font_provider(Arc::new(FontProvider::memory_only()));
    let page = zpdf::interpret_page(&doc, 0, Default::default()).unwrap();
    let font = page.fonts.get_by_name("F1").unwrap().1;
    assert!(!font.has_font_data());
    assert_eq!(page.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(page.stats.diagnostics.missing_glyph_outlines, 1);
    assert_eq!(page.stats.diagnostics.decode_failures, 0);
    assert_eq!(
        page.stats.diagnostics.first_failed_object,
        Some(zpdf::ObjectId(7, 0))
    );
    assert!(!page.is_complete());
    assert!(page.ensure_complete().is_err());

    let RenderCommand::DrawGlyphRun(run) = &page.display_list.commands[0] else {
        panic!("expected glyph run");
    };
    assert!((run.glyphs[0].advance - 620.0 * 24.0 / 1000.0).abs() < 0.01);
    assert!((run.glyphs[1].advance - 780.0 * 24.0 / 1000.0).abs() < 0.01);
    assert!((run.glyphs[2].advance - 540.0 * 24.0 / 1000.0).abs() < 0.01);

    // Every character decoded cleanly through /WinAnsiEncoding, so text-only
    // extraction succeeds while retaining the fallback + missing-outline diagnostics.
    let converted = zpdf::convert_pdf(&doc, &[0], Default::default()).unwrap();
    assert_eq!(converted.pages[0].text.trim(), "ABC");
    assert_eq!(converted.pages[0].diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(converted.pages[0].diagnostics.missing_glyph_outlines, 1);
    assert_eq!(converted.pages[0].diagnostics.decode_failures, 0);
    assert!(converted.is_complete());
    converted
        .ensure_complete()
        .expect("decoded text-only conversion should succeed");
}

/// Corrupt only the `glyf` records for the specified glyph IDs in `ttf` (leaving
/// other glyphs intact) so a subset has a majority of unreadable outlines plus a
/// surviving drawable outline.
fn corrupt_selected_gids(ttf: &[u8], corrupt_gids: &[u16]) -> Vec<u8> {
    let mut out = ttf.to_vec();
    let head = sfnt_table_range(&out, b"head");
    let loca = sfnt_table_range(&out, b"loca");
    let glyf = sfnt_table_range(&out, b"glyf");
    let long = out[head.start + 50..head.start + 52] == [0, 1];
    let offset = |buf: &[u8], gid: usize| -> usize {
        if long {
            u32::from_be_bytes(
                buf[loca.start + gid * 4..loca.start + gid * 4 + 4]
                    .try_into()
                    .unwrap(),
            ) as usize
        } else {
            u16::from_be_bytes(
                buf[loca.start + gid * 2..loca.start + gid * 2 + 2]
                    .try_into()
                    .unwrap(),
            ) as usize
                * 2
        }
    };
    for &gid in corrupt_gids {
        let start = offset(&out, gid as usize);
        let end = offset(&out, gid as usize + 1);
        if start < end {
            out[glyf.start + start..glyf.start + end].fill(0xFF);
        }
    }
    out
}

#[test]
fn surviving_outlines_are_preserved_when_no_substitute_is_available() {
    // Corrupt every non-zero glyph in PRIMARY_TTF except 'C' so >50% fail
    // `has_usable_embedded_outlines()` while 'C' remains drawable.
    let probe = zpdf_font::LoadedFont::new_with_data(
        zpdf_font::PdfFontType::TrueType,
        "Probe".into(),
        PRIMARY_TTF.to_vec(),
        zpdf_font::CidWidths::new(1000.0),
    );
    let gid_a = probe.unicode_glyph('A' as u32).unwrap().0;
    let gid_b = probe.unicode_glyph('B' as u32).unwrap().0;
    let gid_c = probe.unicode_glyph('C' as u32).unwrap().0;
    let maxp = sfnt_table_range(PRIMARY_TTF, b"maxp");
    let num_glyphs = u16::from_be_bytes([PRIMARY_TTF[maxp.start + 4], PRIMARY_TTF[maxp.start + 5]]);
    let corrupt_gids: Vec<u16> = (1..num_glyphs).filter(|&g| g != gid_c).collect();
    let partial_ttf = corrupt_selected_gids(PRIMARY_TTF, &corrupt_gids);

    // 1. Simple TrueType with empty memory-only provider: 'C' still renders.
    let simple_pdf = simple_font_pdf(
        "TrueType",
        "ABCDEF+SurvivingSimple",
        "/Flags 32",
        Some("FontFile2"),
        "",
        &partial_ttf,
    );
    let doc = PdfDocument::open(simple_pdf)
        .unwrap()
        .with_font_provider(Arc::new(FontProvider::memory_only()));
    let page = zpdf::interpret_page(&doc, 0, Default::default()).unwrap();
    let font = page.fonts.get_by_name("F1").unwrap().1;
    assert!(!font.is_substitute, "original font must be kept");
    assert!(font.has_font_data(), "original program must survive");
    assert_eq!(page.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(page.stats.diagnostics.missing_glyph_outlines, 1);
    assert_eq!(page.stats.diagnostics.decode_failures, 0);
    assert!(!page.is_complete());
    let cpu = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&page.fonts)
        .render_display_list(&page.display_list, 1.0)
        .unwrap();
    let c_x0 = (15.0_f32 + (620.0 + 780.0) * 24.0 / 1000.0).floor() as usize;
    let c_x1 = (15.0_f32 + (620.0 + 780.0 + 540.0) * 24.0 / 1000.0).ceil() as usize;
    assert!(
        dark_pixels_in_rect(
            &cpu.data,
            cpu.width as usize,
            cpu.height as usize,
            c_x0,
            c_x1,
            30,
            70
        ) > 10,
        "surviving simple-font glyph 'C' must still render when no substitute exists"
    );

    // 2. Type0 CID font with /ToUnicode and 3 mapped CIDs (2 corrupt, 1 surviving)
    //    under an empty memory-only provider: the surviving CID still renders.
    let cid_to_gid_bytes: Vec<u8> = [0u16, gid_a, gid_b, gid_c]
        .into_iter()
        .flat_map(|g| g.to_be_bytes())
        .collect();
    let to_unicode = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
        1 begincodespacerange <0000> <FFFF> endcodespacerange \
        3 beginbfchar <0001> <0041> <0002> <0042> <0003> <0043> endbfchar \
        endcmap CMapName currentdict /CMap defineresource pop end end";
    let type0_pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        stream("", b"0 0 0 rg BT /F1 24 Tf 15 40 Td <000100020003> Tj ET"),
        b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+SurvivingCID /Encoding /Identity-H \
          /DescendantFonts [6 0 R] /ToUnicode 9 0 R >>"
            .to_vec(),
        b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+SurvivingCID \
          /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
          /CIDToGIDMap 10 0 R /FontDescriptor 7 0 R /DW 1000 /W [1 [620 780 540]] >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+SurvivingCID /Flags 32 /FontFile2 8 0 R >>"
            .to_vec(),
        stream("", &partial_ttf),
        stream("", to_unicode),
        stream("", &cid_to_gid_bytes),
    ]);
    let doc0 = PdfDocument::open(type0_pdf)
        .unwrap()
        .with_font_provider(Arc::new(FontProvider::memory_only()));
    let page0 = zpdf::interpret_page(&doc0, 0, Default::default()).unwrap();
    assert!(!page0.fonts.get_by_name("F1").unwrap().1.is_substitute);
    assert_eq!(page0.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(page0.stats.diagnostics.missing_glyph_outlines, 1);
    assert_eq!(page0.stats.diagnostics.decode_failures, 0);
    assert!(!page0.is_complete());
    let cpu0 = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&page0.fonts)
        .render_display_list(&page0.display_list, 1.0)
        .unwrap();
    assert!(
        dark_pixels_in_rect(
            &cpu0.data,
            cpu0.width as usize,
            cpu0.height as usize,
            c_x0,
            c_x1,
            30,
            70
        ) > 10,
        "surviving Type0 glyph 'C' must still render when no substitute exists"
    );
}

#[test]
fn simple_font_substitution_honors_missing_width_for_out_of_range_codes() {
    // /Widths covers only 'A' (65 => 620); descriptor sets /MissingWidth 333.
    // After substitution, 'B' (66) must advance by 333 (not the substitute's 1000 hmtx),
    // placing 'C' at (620 + 333) * 24 / 1000.
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        stream("", b"0 0 0 rg BT /F1 24 Tf 15 40 Td (ABC) Tj ET"),
        b"<< /Type /Font /Subtype /TrueType /BaseFont /ABCDEF+MissingWidthFace /Encoding /WinAnsiEncoding \
          /FirstChar 65 /LastChar 65 /Widths [620] /FontDescriptor 6 0 R >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+MissingWidthFace /Flags 32 /MissingWidth 333 /FontFile2 7 0 R >>"
            .to_vec(),
        stream("", b"not a valid truetype font"),
    ]);
    let doc = PdfDocument::open(pdf)
        .unwrap()
        .with_font_provider(provider_with_alias("MissingWidthFace"));
    let page = zpdf::interpret_page(&doc, 0, Default::default()).unwrap();
    assert!(page.fonts.get_by_name("F1").unwrap().1.is_substitute);
    assert!(page.is_complete());
    assert_eq!(page.stats.diagnostics.embedded_font_fallbacks, 1);

    let RenderCommand::DrawGlyphRun(run) = &page.display_list.commands[0] else {
        panic!("expected glyph run");
    };
    let expected_a = 620.0 * 24.0 / 1000.0;
    let expected_b = 333.0 * 24.0 / 1000.0;
    assert!((run.glyphs[0].advance - expected_a).abs() < 0.01);
    assert!((run.glyphs[1].advance - expected_b).abs() < 0.01);
    assert!(
        (run.glyphs[2].x - (expected_a + expected_b)).abs() < 0.01,
        "next glyph 'C' x={} expected {}",
        run.glyphs[2].x,
        expected_a + expected_b
    );
}

#[test]
fn cache_budget_exceeded_preserves_type0_mappings_and_form_diagnostics() {
    let to_unicode = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
        1 begincodespacerange <0000> <FFFF> endcodespacerange \
        2 beginbfchar <0001> <0041> <0002> <0042> endbfchar \
        endcmap CMapName currentdict /CMap defineresource pop end end";
    // Page 0 draws the corrupt Type0 font directly; Page 1 draws it inside a Form XObject.
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F1 5 0 R >> >> /Contents 10 0 R >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /XObject << /Fm 11 0 R >> >> /Contents 12 0 R >>".to_vec(),
        b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+BudgetCID /Encoding /Identity-H \
          /DescendantFonts [6 0 R] /ToUnicode 9 0 R >>"
            .to_vec(),
        b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+BudgetCID \
          /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
          /FontDescriptor 7 0 R /DW 1000 /W [1 [650 820]] /DW2 [900 -1200] >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+BudgetCID /Flags 32 /FontFile2 8 0 R >>"
            .to_vec(),
        stream("", b"corrupt cid font"),
        stream("", to_unicode),
        stream("", b"0 0 0 rg BT /F1 24 Tf 15 40 Td <00010002> Tj ET"),
        stream(
            "/Type /XObject /Subtype /Form /BBox [0 0 180 100] /Resources << /Font << /G1 5 0 R >> >>",
            b"0 0 0 rg BT /G1 24 Tf 15 40 Td <00010002> Tj ET",
        ),
        stream("", b"/Fm Do"),
    ]);

    // Budget fits the logical Type0 metrics/CMap/ToUnicode (~hundreds of bytes)
    // but cannot fit the 21 KiB PRIMARY_TTF substitute program.
    let limits = zpdf::ParseLimits {
        max_font_cache_bytes: 2048,
        ..Default::default()
    };
    let doc = PdfDocument::open_with_limits(pdf, limits)
        .unwrap()
        .with_font_provider(provider_with_alias("BudgetCID"));

    for page_idx in [0, 1] {
        let page = zpdf::interpret_page(&doc, page_idx, Default::default()).unwrap();
        assert!(
            page.stats.diagnostics.resource_limit_reached,
            "page {page_idx}: budget exceeded must set resource_limit_reached"
        );
        assert_eq!(page.stats.diagnostics.embedded_font_fallbacks, 1);
        assert_eq!(page.stats.diagnostics.decode_failures, 1);
        assert_eq!(
            page.stats.diagnostics.missing_glyph_outlines, 1,
            "page {page_idx}: load-time cache limit must still report missing glyph outlines"
        );
        assert!(!page.is_complete());
        assert!(page.ensure_complete().is_err());

        let runs: Vec<_> = page
            .display_list
            .commands
            .iter()
            .filter_map(|c| match c {
                RenderCommand::DrawGlyphRun(r) => Some(r),
                _ => None,
            })
            .collect();
        assert_eq!(runs.len(), 1);
        assert_eq!(
            runs[0].glyphs.len(),
            2,
            "page {page_idx}: Type0 2-byte segmentation must survive budget fallback"
        );
        assert!((runs[0].glyphs[0].advance - 650.0 * 24.0 / 1000.0).abs() < 0.01);
        assert!((runs[0].glyphs[1].advance - 820.0 * 24.0 / 1000.0).abs() < 0.01);
        if page_idx == 0 {
            let font = page.fonts.get_by_name("F1").unwrap().1;
            assert_eq!(font.dw2, (900.0, -1200.0));
        }

        let converted = zpdf::convert_pdf(&doc, &[page_idx], Default::default()).unwrap();
        assert_eq!(
            converted.pages[0].text.trim(),
            "AB",
            "page {page_idx}: /ToUnicode extraction must survive budget fallback"
        );
        assert!(converted.pages[0].diagnostics.resource_limit_reached);
        assert!(!converted.is_complete());
    }
}

#[test]
fn no_marks_separation_none_suppresses_fallback_diagnostic_while_clip_mode_keeps_it() {
    let sep_none_pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /ColorSpace << /CS0 [/Separation /None /DeviceGray 8 0 R] >> /Font << /F1 5 0 R >> >> /Contents 9 0 R >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F1 5 0 R >> >> /Contents 10 0 R >>".to_vec(),
        b"<< /Type /Font /Subtype /TrueType /BaseFont /ABCDEF+NoMarksFace /Encoding /WinAnsiEncoding /FirstChar 65 /LastChar 67 /Widths [620 780 540] /FontDescriptor 6 0 R >>".to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+NoMarksFace /Flags 32 /FontFile2 7 0 R >>".to_vec(),
        stream("", b"corrupt truetype bytes"),
        b"<< /FunctionType 2 /Domain [0 1] /C0 [1] /C1 [0] /N 1 >>".to_vec(),
        stream("", b"/CS0 cs 1 scn BT /F1 24 Tf 15 40 Td (ABC) Tj ET"),
        stream("", b"q BT /F1 24 Tf 7 Tr 15 40 Td (ABC) Tj ET 0 0 0 rg 0 0 180 100 re f Q"),
    ]);

    // Even with NO substitute (empty memory-only provider), mode-0 /Separation /None
    // produces no marks and no clip -> clean complete page.
    let doc = PdfDocument::open(sep_none_pdf)
        .unwrap()
        .with_font_provider(Arc::new(FontProvider::memory_only()));
    let page_none = zpdf::interpret_page(&doc, 0, Default::default()).unwrap();
    assert_eq!(
        page_none.stats.diagnostics,
        ProcessingDiagnostics::default()
    );
    assert!(page_none.is_complete());

    // Render mode 7 (clip-only) needs glyph outlines -> reports fallback and missing outlines.
    let page_clip = zpdf::interpret_page(&doc, 1, Default::default()).unwrap();
    assert_eq!(page_clip.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(page_clip.stats.diagnostics.missing_glyph_outlines, 1);
    assert!(!page_clip.is_complete());
}

#[test]
fn late_provider_registration_retries_substitution_for_retained_zeroed_glyf_font() {
    let provider = Arc::new(FontProvider::memory_only());
    let pdf = simple_font_pdf(
        "TrueType",
        "ABCDEF+LateRetryFace",
        "/Flags 32",
        Some("FontFile2"),
        "",
        &corrupt_glyf_in_place(PRIMARY_TTF, 0x00),
    );
    let doc = PdfDocument::open(pdf)
        .unwrap()
        .with_font_provider(Arc::clone(&provider));

    // First render on empty provider keeps original zeroed-glyf font -> blank & incomplete.
    let first = zpdf::interpret_page(&doc, 0, Default::default()).unwrap();
    assert!(!first.fonts.get_by_name("F1").unwrap().1.is_substitute);
    assert_eq!(first.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(first.stats.diagnostics.missing_glyph_outlines, 1);
    assert!(!first.is_complete());

    // Register substitute on the same provider and re-render on the same document.
    provider
        .register_with_aliases(PRIMARY_TTF, 0, &["LateRetryFace"])
        .unwrap();
    let second = zpdf::interpret_page(&doc, 0, Default::default()).unwrap();
    assert!(
        second.fonts.get_by_name("F1").unwrap().1.is_substitute,
        "provider generation bump must evict fallback-retained original font and retry substitution"
    );
    assert_eq!(second.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(second.stats.diagnostics.missing_glyph_outlines, 0);
    assert!(second.is_complete());
    second.ensure_complete().expect("second render completes");
    let expected_adv = [
        620.0 * 24.0 / 1000.0,
        780.0 * 24.0 / 1000.0,
        540.0 * 24.0 / 1000.0,
    ];
    let expected_x = [0.0, expected_adv[0], expected_adv[0] + expected_adv[1]];
    assert_rendered_cells_have_ink(&second, &expected_x, &expected_adv, "late provider retry");
}

#[test]
fn missing_private_use_icon_glyph_is_reported_as_unrecovered_and_unusable_tounicode_keeps_original()
{
    // 1. Corrupt embedded font mapping a private-use icon U+E001 via /ToUnicode:
    //    PRIMARY_TTF has no U+E001 glyph, so the missing icon must NOT be excused
    //    as invisible whitespace.
    let pua_tounicode = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
        1 begincodespacerange <0000> <FFFF> endcodespacerange \
        1 beginbfchar <0001> <E001> endbfchar \
        endcmap CMapName currentdict /CMap defineresource pop end end";
    let pua_pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        stream("", b"0 0 0 rg BT /F1 24 Tf 15 40 Td <0001> Tj ET"),
        b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+IconFont /Encoding /Identity-H \
          /DescendantFonts [6 0 R] /ToUnicode 9 0 R >>"
            .to_vec(),
        b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+IconFont \
          /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
          /FontDescriptor 7 0 R /DW 1000 >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+IconFont /Flags 32 /FontFile2 8 0 R >>"
            .to_vec(),
        stream("", b"corrupt icon font bytes"),
        stream("", pua_tounicode),
    ]);
    let doc_pua = PdfDocument::open(pua_pdf)
        .unwrap()
        .with_font_provider(provider_with_alias("IconFont"));
    let page_pua = zpdf::interpret_page(&doc_pua, 0, Default::default()).unwrap();
    assert_eq!(page_pua.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(page_pua.stats.diagnostics.missing_glyph_outlines, 1);
    assert!(!page_pua.is_complete());
    assert!(page_pua.ensure_complete().is_err());

    // 2. Partially readable CID font (2 unreadable + 1 surviving 'C') whose
    //    /ToUnicode is empty, undecodable, or maps only characters absent from
    //    the substitute: loader must keep the original face + /CIDToGIDMap so
    //    the surviving embedded glyph still renders.
    let probe = zpdf_font::LoadedFont::new_with_data(
        zpdf_font::PdfFontType::TrueType,
        "Probe".into(),
        PRIMARY_TTF.to_vec(),
        zpdf_font::CidWidths::new(1000.0),
    );
    let gid_a = probe.unicode_glyph('A' as u32).unwrap().0;
    let gid_b = probe.unicode_glyph('B' as u32).unwrap().0;
    let gid_c = probe.unicode_glyph('C' as u32).unwrap().0;
    let maxp = sfnt_table_range(PRIMARY_TTF, b"maxp");
    let num_glyphs = u16::from_be_bytes([PRIMARY_TTF[maxp.start + 4], PRIMARY_TTF[maxp.start + 5]]);
    let corrupt_gids: Vec<u16> = (1..num_glyphs).filter(|&g| g != gid_c).collect();
    let partial_ttf = corrupt_selected_gids(PRIMARY_TTF, &corrupt_gids);
    let cid_to_gid_bytes: Vec<u8> = [0u16, gid_a, gid_b, gid_c]
        .into_iter()
        .flat_map(|g| g.to_be_bytes())
        .collect();
    let c_x0 = (15.0_f32 + (620.0 + 780.0) * 24.0 / 1000.0).floor() as usize;
    let c_x1 = (15.0_f32 + (620.0 + 780.0 + 540.0) * 24.0 / 1000.0).ceil() as usize;

    let bad_tounicode_cases: &[(&str, &str, &[u8])] = &[
        ("empty ToUnicode stream", "", b""),
        (
            "undecodable ToUnicode stream",
            "/Filter /UnsupportedDecode",
            b"bad",
        ),
        (
            "ToUnicode mapping only unrepresented PUA",
            "",
            pua_tounicode,
        ),
    ];
    for (label, tu_dict, tu_bytes) in bad_tounicode_cases {
        let cid_pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
            stream("", b"0 0 0 rg BT /F1 24 Tf 15 40 Td <000100020003> Tj ET"),
            b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+BadToUnicodeCID /Encoding /Identity-H \
              /DescendantFonts [6 0 R] /ToUnicode 9 0 R >>"
                .to_vec(),
            b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+BadToUnicodeCID \
              /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
              /CIDToGIDMap 10 0 R /FontDescriptor 7 0 R /DW 1000 /W [1 [620 780 540]] >>"
                .to_vec(),
            b"<< /Type /FontDescriptor /FontName /ABCDEF+BadToUnicodeCID /Flags 32 /FontFile2 8 0 R >>"
                .to_vec(),
            stream("", &partial_ttf),
            stream(tu_dict, tu_bytes),
            stream("", &cid_to_gid_bytes),
        ]);
        let doc = PdfDocument::open(cid_pdf)
            .unwrap()
            .with_font_provider(provider_with_alias("BadToUnicodeCID"));
        let page = zpdf::interpret_page(&doc, 0, Default::default()).unwrap();
        assert!(
            !page.fonts.get_by_name("F1").unwrap().1.is_substitute,
            "{label}: must retain original font + /CIDToGIDMap"
        );
        let cpu = zpdf::cpu::CpuRenderer::new()
            .with_fonts(&page.fonts)
            .render_display_list(&page.display_list, 1.0)
            .unwrap();
        assert!(
            dark_pixels_in_rect(
                &cpu.data,
                cpu.width as usize,
                cpu.height as usize,
                c_x0,
                c_x1,
                30,
                70,
            ) > 10,
            "{label}: surviving CID glyph must still render"
        );
    }

    // 3. Partially readable Identity-H CID font where /ToUnicode maps ONLY CID 1
    //    to 'A' (covered by the substitute), while CID 3 is unmapped in /ToUnicode
    //    and draws solely from the surviving embedded /CIDToGIDMap: BOTH CID 1 and
    //    CID 3 must draw visibly at their exact PDF positions (CPU and GPU).
    let partial_tounicode = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
        1 begincodespacerange <0000> <FFFF> endcodespacerange \
        1 beginbfchar <0001> <0041> endbfchar \
        endcmap CMapName currentdict /CMap defineresource pop end end";
    let make_mixed_cid_pdf = |base_font: &str, content: &[u8]| {
        assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
            stream("", content),
            format!(
                "<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+{base_font} /Encoding /Identity-H \
                 /DescendantFonts [6 0 R] /ToUnicode 9 0 R >>"
            )
            .into_bytes(),
            format!(
                "<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+{base_font} \
                 /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
                 /CIDToGIDMap 10 0 R /FontDescriptor 7 0 R /DW 1000 /W [1 [620 780 540]] >>"
            )
            .into_bytes(),
            format!(
                "<< /Type /FontDescriptor /FontName /ABCDEF+{base_font} /Flags 32 /FontFile2 8 0 R >>"
            )
            .into_bytes(),
            stream("", &partial_ttf),
            stream("", partial_tounicode),
            stream("", &cid_to_gid_bytes),
        ])
    };

    // 3a. With FontProvider: <00010003> draws CID 1 from substitute + CID 3 from
    //     retained embedded companion, passing ensure_complete() on CPU & GPU.
    let mixed_complete_pdf = make_mixed_cid_pdf(
        "MixedPartialCID",
        b"0 0 0 rg BT /F1 24 Tf 15 40 Td <00010003> Tj ET",
    );
    let doc_mixed = PdfDocument::open(mixed_complete_pdf)
        .unwrap()
        .with_font_provider(provider_with_alias("MixedPartialCID"));
    let page_mixed = zpdf::interpret_page(&doc_mixed, 0, Default::default()).unwrap();
    assert!(page_mixed.fonts.get_by_name("F1").unwrap().1.is_substitute);
    assert_eq!(page_mixed.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(page_mixed.stats.diagnostics.missing_glyph_outlines, 0);
    assert!(page_mixed.is_complete());
    page_mixed
        .ensure_complete()
        .expect("mixed substitute + embedded companion recovery completes");
    let mixed_runs: Vec<_> = page_mixed
        .display_list
        .commands
        .iter()
        .filter_map(|c| match c {
            RenderCommand::DrawGlyphRun(r) => Some(r),
            _ => None,
        })
        .collect();
    assert_eq!(mixed_runs.len(), 2, "substitute + embedded companion split");
    let cid1_adv = 620.0_f32 * 24.0 / 1000.0;
    let cid3_adv = 540.0_f32 * 24.0 / 1000.0;
    assert!((mixed_runs[0].glyphs[0].x - 0.0).abs() < 0.01);
    assert!((mixed_runs[0].glyphs[0].advance - cid1_adv).abs() < 0.01);
    assert!((mixed_runs[1].glyphs[0].x - cid1_adv).abs() < 0.01);
    assert!((mixed_runs[1].glyphs[0].advance - cid3_adv).abs() < 0.01);

    let cpu_mixed = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&page_mixed.fonts)
        .render_display_list(&page_mixed.display_list, 1.0)
        .unwrap();
    for (label, gx, adv) in [
        ("CID 1 (substitute)", 0.0_f32, cid1_adv),
        ("CID 3 (surviving embedded)", cid1_adv, cid3_adv),
    ] {
        let px0 = (15.0 + gx).floor() as usize;
        let px1 = (15.0 + gx + adv).ceil() as usize;
        assert!(
            dark_pixels_in_rect(
                &cpu_mixed.data,
                cpu_mixed.width as usize,
                cpu_mixed.height as usize,
                px0,
                px1,
                30,
                70,
            ) > 10,
            "CPU {label} cell x={px0}..{px1} must have ink"
        );
    }
    #[cfg(feature = "gpu-render")]
    {
        let mut gpu = zpdf::gpu::WgpuRenderer::new();
        if gpu.adapter_identity().is_ok() {
            let gpu_mixed = gpu
                .with_fonts(&page_mixed.fonts)
                .render_display_list(&page_mixed.display_list, 1.0)
                .unwrap();
            for (label, gx, adv) in [
                ("CID 1 (substitute)", 0.0_f32, cid1_adv),
                ("CID 3 (surviving embedded)", cid1_adv, cid3_adv),
            ] {
                let px0 = (15.0 + gx).floor() as usize;
                let px1 = (15.0 + gx + adv).ceil() as usize;
                assert!(
                    dark_pixels_in_rect(
                        &gpu_mixed.data,
                        gpu_mixed.width as usize,
                        gpu_mixed.height as usize,
                        px0,
                        px1,
                        30,
                        70,
                    ) > 10,
                    "GPU {label} cell x={px0}..{px1} must have ink"
                );
            }
        }
    }

    // 3b. With default system font substitution (no FontProvider): both CID 1
    //     (from system Helvetica) and CID 3 (from surviving embedded companion)
    //     still draw visibly at their PDF positions. Only run when the matched
    //     local Helvetica face actually provides a drawable 'A' (U+0041).
    let sys_helvetica_draws_a =
        zpdf_font::system::find_system_font("Helvetica", Default::default(), None)
            .and_then(|m| {
                zpdf_font::LoadedFont::new_substitute(
                    zpdf_font::PdfFontType::TrueType,
                    "Helvetica".into(),
                    m.data,
                    m.face_index,
                    zpdf_font::CidWidths::new(1000.0),
                )
            })
            .is_some_and(|face| {
                face.unicode_glyph('A' as u32)
                    .is_some_and(|(gid, _)| face.glyph_drawn(gid))
            });
    if sys_helvetica_draws_a {
        let sys_mixed_pdf = make_mixed_cid_pdf(
            "Helvetica",
            b"0 0 0 rg BT /F1 24 Tf 15 40 Td <000100020003> Tj ET",
        );
        let doc_sys = PdfDocument::open(sys_mixed_pdf).unwrap();
        let page_sys = zpdf::interpret_page(&doc_sys, 0, Default::default()).unwrap();
        assert!(page_sys.fonts.get_by_name("F1").unwrap().1.is_substitute);
        assert_eq!(page_sys.stats.diagnostics.embedded_font_fallbacks, 1);
        assert_eq!(page_sys.stats.diagnostics.missing_glyph_outlines, 1);
        assert!(!page_sys.is_complete());
        let cpu_sys = zpdf::cpu::CpuRenderer::new()
            .with_fonts(&page_sys.fonts)
            .render_display_list(&page_sys.display_list, 1.0)
            .unwrap();
        let a_x0 = 15usize;
        let a_x1 = (15.0 + cid1_adv).ceil() as usize;
        assert!(
            dark_pixels_in_rect(
                &cpu_sys.data,
                cpu_sys.width as usize,
                cpu_sys.height as usize,
                a_x0,
                a_x1,
                30,
                70,
            ) > 10,
            "default system substitute CID 1 must render"
        );
        assert!(
            dark_pixels_in_rect(
                &cpu_sys.data,
                cpu_sys.width as usize,
                cpu_sys.height as usize,
                c_x0,
                c_x1,
                30,
                70,
            ) > 10,
            "surviving embedded CID 3 must render alongside default system substitute"
        );
    }

    // 4. Text extraction completeness is checked independently of outline
    //    recovery, honors /ActualText replacements, and checks render-mode-3 OCR.
    //    4a. Single surviving unmapped CID (<0003> Tj): outlines draw cleanly
    //        (visual render complete), but extraction emits nothing -> text
    //        conversion rejects with decode_failures=1.
    let single_unmapped_cid_pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        stream("", b"0 0 0 rg BT /F1 24 Tf 15 40 Td <0003> Tj ET"),
        b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+SingleSurvivingCID /Encoding /Identity-H /DescendantFonts [6 0 R] >>".to_vec(),
        b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+SingleSurvivingCID \
          /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
          /CIDToGIDMap 9 0 R /FontDescriptor 7 0 R /DW 1000 /W [3 [540]] >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+SingleSurvivingCID /Flags 32 /FontFile2 8 0 R >>"
            .to_vec(),
        stream("", &partial_ttf),
        stream("", &cid_to_gid_bytes),
    ]);
    let doc_single = PdfDocument::open(single_unmapped_cid_pdf)
        .unwrap()
        .with_font_provider(Arc::new(FontProvider::memory_only()));
    let page_single = zpdf::interpret_page(&doc_single, 0, Default::default()).unwrap();
    assert_eq!(page_single.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(page_single.stats.diagnostics.missing_glyph_outlines, 0);
    assert!(page_single.is_complete(), "visual outline drew cleanly");
    let conv_single = zpdf::convert_pdf(&doc_single, &[0], Default::default()).unwrap();
    assert_eq!(conv_single.pages[0].text.trim(), "");
    assert_eq!(conv_single.pages[0].diagnostics.decode_failures, 1);
    assert!(!conv_single.is_complete());
    assert!(conv_single.ensure_complete().is_err());

    //    4b. Unmapped Identity-H glyph with missing outline wrapped in
    //        /ActualText (Label): text extraction receives "Label" and succeeds,
    //        while visual rendering still rejects missing_glyph_outlines=1.
    let actual_text_pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        stream(
            "",
            b"0 0 0 rg /Span << /ActualText (Label) >> BDC BT /F1 24 Tf 15 40 Td <0001> Tj ET EMC",
        ),
        b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+ActualTextCID /Encoding /Identity-H /DescendantFonts [6 0 R] >>".to_vec(),
        b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+ActualTextCID \
          /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
          /FontDescriptor 7 0 R /DW 1000 >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+ActualTextCID /Flags 32 /FontFile2 8 0 R >>"
            .to_vec(),
        stream("", b"corrupt actual-text cid font"),
    ]);
    let doc_actual = PdfDocument::open(actual_text_pdf)
        .unwrap()
        .with_font_provider(Arc::new(FontProvider::memory_only()));
    let page_actual = zpdf::interpret_page(&doc_actual, 0, Default::default()).unwrap();
    assert_eq!(page_actual.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(page_actual.stats.diagnostics.missing_glyph_outlines, 1);
    assert!(!page_actual.is_complete());
    let conv_actual = zpdf::convert_pdf(&doc_actual, &[0], Default::default()).unwrap();
    assert_eq!(conv_actual.pages[0].text.trim(), "Label");
    assert_eq!(conv_actual.pages[0].diagnostics.decode_failures, 0);
    assert_eq!(conv_actual.pages[0].diagnostics.missing_glyph_outlines, 1);
    assert!(conv_actual.is_complete());
    conv_actual
        .ensure_complete()
        .expect("/ActualText satisfies text completeness");

    //    4c. Invisible render-mode-3 OCR layer with undecodable corrupt font:
    //        visual render stays complete (no marks), while text extraction
    //        rejects with decode_failures=1.
    let ocr_pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        stream("", b"BT /F1 24 Tf 3 Tr 15 40 Td <0001> Tj ET"),
        b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+BrokenOcrCID /Encoding /Identity-H /DescendantFonts [6 0 R] >>".to_vec(),
        b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+BrokenOcrCID \
          /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
          /FontDescriptor 7 0 R /DW 1000 >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+BrokenOcrCID /Flags 32 /FontFile2 8 0 R >>"
            .to_vec(),
        stream("", b"corrupt ocr font"),
    ]);
    let doc_ocr = PdfDocument::open(ocr_pdf)
        .unwrap()
        .with_font_provider(Arc::new(FontProvider::memory_only()));
    let page_ocr = zpdf::interpret_page(&doc_ocr, 0, Default::default()).unwrap();
    assert!(
        page_ocr.is_complete(),
        "invisible OCR layer needs no outlines"
    );
    let conv_ocr = zpdf::convert_pdf(&doc_ocr, &[0], Default::default()).unwrap();
    assert_eq!(conv_ocr.pages[0].diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(conv_ocr.pages[0].diagnostics.decode_failures, 1);
    assert!(!conv_ocr.is_complete());
    assert!(conv_ocr.ensure_complete().is_err());
}

#[test]
fn partial_substitute_recovery_distinguishes_missing_glyphs_from_complete_recovery() {
    // 1. Corrupt Type0 font showing 'A', space, and U+4F00 (present in FALLBACK_TTF
    //    but absent from PRIMARY_TTF).
    let to_unicode = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
        1 begincodespacerange <0000> <FFFF> endcodespacerange \
        3 beginbfchar <0001> <0041> <0002> <0020> <0003> <4F00> endbfchar \
        endcmap CMapName currentdict /CMap defineresource pop end end";
    let partial_pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        stream("", b"0 0 0 rg BT /F1 24 Tf 15 40 Td <000100020003> Tj ET"),
        b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+PartialFace /Encoding /Identity-H \
          /DescendantFonts [6 0 R] /ToUnicode 9 0 R >>"
            .to_vec(),
        b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+PartialFace \
          /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
          /FontDescriptor 7 0 R /DW 1000 /W [1 [650 350 820]] >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+PartialFace /Flags 32 /FontFile2 8 0 R >>"
            .to_vec(),
        stream("", &corrupt_glyf_in_place(PRIMARY_TTF, 0xFF)),
        stream("", to_unicode),
    ]);

    // Only PRIMARY_TTF registered: U+4F00 cannot draw -> visual output incomplete,
    // while every code decoded cleanly so text-only conversion still succeeds.
    let doc_partial = PdfDocument::open(partial_pdf.clone())
        .unwrap()
        .with_font_provider(provider_with_alias("PartialFace"));
    let page_partial = zpdf::interpret_page(&doc_partial, 0, Default::default()).unwrap();
    assert_eq!(page_partial.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(page_partial.stats.diagnostics.missing_glyph_outlines, 1);
    assert_eq!(page_partial.stats.diagnostics.decode_failures, 0);
    assert!(!page_partial.is_complete());
    assert!(page_partial.ensure_complete().is_err());
    let conv_partial = zpdf::convert_pdf(&doc_partial, &[0], Default::default()).unwrap();
    assert!(conv_partial.is_complete());
    assert!(conv_partial.pages[0].text.contains("A \u{4F00}"));

    // Add FALLBACK_TTF to the provider: coverage fallback draws U+4F00, and the
    // blank space glyph is recognized as whitespace -> complete with fallback recorded.
    let provider_complete = provider_with_alias("PartialFace");
    provider_complete
        .register_with_aliases(FALLBACK_TTF, 0, &[])
        .unwrap();
    let doc_complete = PdfDocument::open(partial_pdf)
        .unwrap()
        .with_font_provider(provider_complete);
    let page_complete = zpdf::interpret_page(&doc_complete, 0, Default::default()).unwrap();
    assert_eq!(page_complete.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(page_complete.stats.diagnostics.decode_failures, 0);
    assert!(page_complete.is_complete());
    page_complete
        .ensure_complete()
        .expect("cross-face recovery completes");
    let runs: Vec<_> = page_complete
        .display_list
        .commands
        .iter()
        .filter_map(|cmd| match cmd {
            RenderCommand::DrawGlyphRun(run) => Some(run),
            _ => None,
        })
        .collect();
    assert_eq!(
        runs.len(),
        2,
        "cross-face provider fallback should split into primary + fallback runs"
    );
    assert_eq!(runs[0].glyphs.len(), 2);
    assert_eq!(runs[1].glyphs.len(), 1);
    let expected_third_x = (650.0 + 350.0) * 24.0 / 1000.0;
    assert!((runs[1].glyphs[0].x - expected_third_x).abs() < 0.01);
    assert!((runs[1].glyphs[0].advance - 820.0 * 24.0 / 1000.0).abs() < 0.01);
    let cpu = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&page_complete.fonts)
        .with_images(&page_complete.images)
        .render_display_list(&page_complete.display_list, 1.0)
        .expect("cpu render");
    for (idx, (gx, adv)) in [
        (0.0f32, 650.0 * 24.0 / 1000.0),
        (expected_third_x, 820.0 * 24.0 / 1000.0),
    ]
    .into_iter()
    .enumerate()
    {
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
            "cross-face provider fallback glyph {idx} cell x={px0}..{px1} rendered blank ({ink} dark px)"
        );
    }

    // 2. Corrupt Type0 Identity-H without /ToUnicode or a CJK collection cannot
    //    be mapped to a substitute -> must remain incomplete.
    let unmappable_pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        stream("", b"0 0 0 rg BT /F1 24 Tf 15 40 Td <00010002> Tj ET"),
        b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+UnmappableCID /Encoding /Identity-H \
          /DescendantFonts [6 0 R] >>"
            .to_vec(),
        b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+UnmappableCID \
          /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
          /FontDescriptor 7 0 R /DW 1000 >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+UnmappableCID /Flags 32 /FontFile2 8 0 R >>"
            .to_vec(),
        stream("", &corrupt_glyf_in_place(PRIMARY_TTF, 0xFF)),
    ]);
    let doc_unmappable = PdfDocument::open(unmappable_pdf)
        .unwrap()
        .with_font_provider(provider_with_alias("UnmappableCID"));
    let page_unmappable = zpdf::interpret_page(&doc_unmappable, 0, Default::default()).unwrap();
    assert_eq!(page_unmappable.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(page_unmappable.stats.diagnostics.missing_glyph_outlines, 1);
    assert!(!page_unmappable.is_complete());
    assert!(page_unmappable.ensure_complete().is_err());
    let conv_unmappable = zpdf::convert_pdf(&doc_unmappable, &[0], Default::default()).unwrap();
    assert_eq!(conv_unmappable.pages[0].diagnostics.decode_failures, 1);
    assert!(!conv_unmappable.is_complete());
    assert!(conv_unmappable.ensure_complete().is_err());

    // 3. Embedded font stream hitting ParseLimits::max_decoded_stream_bytes stays
    //    incomplete (resource_limit_reached) even when a substitute is registered.
    let oversized_pdf = simple_font_pdf(
        "TrueType",
        "ABCDEF+LimitedFace",
        "/Flags 32",
        Some("FontFile2"),
        "",
        PRIMARY_TTF,
    );
    let limits = zpdf::ParseLimits {
        max_decoded_stream_bytes: 4096,
        ..Default::default()
    };
    let doc_limited = PdfDocument::open_with_limits(oversized_pdf, limits)
        .unwrap()
        .with_font_provider(provider_with_alias("LimitedFace"));
    let page_limited = zpdf::interpret_page(&doc_limited, 0, Default::default()).unwrap();
    assert_eq!(page_limited.stats.diagnostics.embedded_font_fallbacks, 1);
    assert!(page_limited.stats.diagnostics.resource_limit_reached);
    assert_eq!(page_limited.stats.diagnostics.decode_failures, 1);
    assert!(!page_limited.is_complete());
    assert!(page_limited.ensure_complete().is_err());
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
    assert_eq!(page.stats.diagnostics.decode_failures, 0);
    assert!(page.is_complete());
    page.ensure_complete()
        .expect("system Helvetica recovery is complete");

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
    assert_eq!(page.stats.diagnostics.decode_failures, 0);
    assert!(page.is_complete());

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
    assert_eq!(page.stats.diagnostics.decode_failures, 0);
    assert!(page.is_complete());
}

#[test]
fn embedded_companion_respects_page_font_cache_budget_even_when_unused() {
    let probe = zpdf_font::LoadedFont::new_with_data(
        zpdf_font::PdfFontType::TrueType,
        "Probe".into(),
        PRIMARY_TTF.to_vec(),
        zpdf_font::CidWidths::new(1000.0),
    );
    let gid_a = probe.unicode_glyph('A' as u32).unwrap().0;
    let gid_b = probe.unicode_glyph('B' as u32).unwrap().0;
    let gid_c = probe.unicode_glyph('C' as u32).unwrap().0;
    let maxp = sfnt_table_range(PRIMARY_TTF, b"maxp");
    let num_glyphs = u16::from_be_bytes([PRIMARY_TTF[maxp.start + 4], PRIMARY_TTF[maxp.start + 5]]);
    let corrupt_gids: Vec<u16> = (1..num_glyphs).filter(|&g| g != gid_c).collect();
    let partial_ttf = corrupt_selected_gids(PRIMARY_TTF, &corrupt_gids);
    let cid_to_gid_bytes: Vec<u8> = [0u16, gid_a, gid_b, gid_c]
        .into_iter()
        .flat_map(|g| g.to_be_bytes())
        .collect();
    let partial_tounicode = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
        1 begincodespacerange <0000> <FFFF> endcodespacerange \
        1 beginbfchar <0001> <0041> endbfchar \
        endcmap CMapName currentdict /CMap defineresource pop end end";

    // Page 0 draws only CID 1 (covered by the primary substitute; companion is unused).
    // Page 1 draws CID 1 through a Form XObject referencing the same font.
    // Page 2 has both /F1 (partially corrupt + companion) and /F2 (valid embedded BOLD_TTF).
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R] /Count 3 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F1 6 0 R >> >> /Contents 12 0 R >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /XObject << /Fm 13 0 R >> >> /Contents 14 0 R >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F1 6 0 R /F2 15 0 R >> >> /Contents 18 0 R >>".to_vec(),
        b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+BudgetCompanion /Encoding /Identity-H \
          /DescendantFonts [7 0 R] /ToUnicode 10 0 R >>"
            .to_vec(),
        b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+BudgetCompanion \
          /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
          /CIDToGIDMap 11 0 R /FontDescriptor 8 0 R /DW 1000 /W [1 [620 780 540]] >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+BudgetCompanion /Flags 32 /FontFile2 9 0 R >>"
            .to_vec(),
        stream("", &partial_ttf),
        stream("", partial_tounicode),
        stream("", &cid_to_gid_bytes),
        stream("", b"0 0 0 rg BT /F1 24 Tf 15 40 Td <0001> Tj ET"),
        stream(
            "/Type /XObject /Subtype /Form /BBox [0 0 180 100] /Resources << /Font << /G1 6 0 R >> >>",
            b"0 0 0 rg BT /G1 24 Tf 15 40 Td <0001> Tj ET",
        ),
        stream("", b"/Fm Do"),
        b"<< /Type /Font /Subtype /TrueType /BaseFont /ABCDEF+SiblingValid /Encoding /WinAnsiEncoding \
          /FirstChar 65 /LastChar 67 /Widths [620 780 540] /FontDescriptor 16 0 R >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+SiblingValid /Flags 32 /FontFile2 17 0 R >>"
            .to_vec(),
        stream("", BOLD_TTF),
        stream(
            "",
            b"0 0 0 rg BT /F1 24 Tf 15 40 Td <0001> Tj /F2 24 Tf 20 0 Td (B) Tj ET",
        ),
    ]);

    // 1. Primary fits, unused companion exceeds max_font_cache_bytes:
    //    companion is not retained and page cache stays within budget.
    let tight_budget = PRIMARY_TTF.len() as u64 + 4096;
    assert!(
        tight_budget < (PRIMARY_TTF.len() as u64) * 2,
        "tight budget must fit primary but not primary + companion"
    );
    let doc_tight = PdfDocument::open_with_limits(
        pdf.clone(),
        zpdf::ParseLimits {
            max_font_cache_bytes: tight_budget,
            ..Default::default()
        },
    )
    .unwrap()
    .with_font_provider(provider_with_alias("BudgetCompanion"));

    // Page 0 (direct page font)
    let page0 = zpdf::interpret_page(&doc_tight, 0, Default::default()).unwrap();
    let (f1_id, f1) = page0.fonts.get_by_name("F1").unwrap();
    assert!(f1.is_substitute && f1.has_font_data());
    assert!(
        !page0.fonts.has_retained_embedded_companion(f1_id),
        "unused companion exceeding page budget must not be retained"
    );
    assert_eq!(page0.fonts.len(), 1);
    assert!(
        page0.fonts.bytes_used() <= tight_budget,
        "page cache bytes {} exceeded limit {}",
        page0.fonts.bytes_used(),
        tight_budget
    );
    assert_eq!(page0.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(page0.stats.diagnostics.missing_glyph_outlines, 0);
    assert_eq!(page0.stats.diagnostics.decode_failures, 0);
    assert!(!page0.stats.diagnostics.resource_limit_reached);
    assert!(page0.is_complete());
    let cpu0 = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&page0.fonts)
        .render_display_list(&page0.display_list, 1.0)
        .unwrap();
    assert!(
        dark_pixels_in_rect(
            &cpu0.data,
            cpu0.width as usize,
            cpu0.height as usize,
            15,
            35,
            30,
            70
        ) > 10,
        "substitute CID 1 must still render when unused companion is not retained"
    );

    // Page 1 (Form XObject font)
    let page1 = zpdf::interpret_page(&doc_tight, 1, Default::default()).unwrap();
    let (g1_id, g1) = page1.fonts.get_by_name("__font_6_0").unwrap();
    assert!(g1.is_substitute && g1.has_font_data());
    assert!(!page1.fonts.has_retained_embedded_companion(g1_id));
    assert!(page1.fonts.bytes_used() <= tight_budget);
    assert!(page1.is_complete());

    // 2. Generous budget: a fitting companion may be held in budget-charged,
    //    reclaimable standby (enabling font-cache-only recovery without a
    //    `PdfFile`), while remaining unadmitted until an uncovered glyph draws
    //    from it and staying charged inside `bytes_used() <= ample_budget`.
    let ample_budget = (PRIMARY_TTF.len() as u64) * 4 + BOLD_TTF.len() as u64;
    let doc_ample = PdfDocument::open_with_limits(
        pdf.clone(),
        zpdf::ParseLimits {
            max_font_cache_bytes: ample_budget,
            ..Default::default()
        },
    )
    .unwrap()
    .with_font_provider(provider_with_alias("BudgetCompanion"));
    let page_ample = zpdf::interpret_page(&doc_ample, 0, Default::default()).unwrap();
    let (f1_ample_id, f1_ample) = page_ample.fonts.get_by_name("F1").unwrap();
    let f1_cost = f1_ample.estimated_cache_bytes();
    assert!(page_ample.fonts.has_embedded_companion(f1_ample_id));
    assert!(
        page_ample
            .fonts
            .has_retained_embedded_companion(f1_ample_id),
        "fitting companion is parked in budget-charged reclaimable standby"
    );
    assert!(
        !page_ample
            .fonts
            .has_admitted_embedded_companion(f1_ample_id),
        "unused companion must not be permanently admitted until a glyph draws from it"
    );
    assert_eq!(page_ample.fonts.len(), 1);
    assert!(
        page_ample.fonts.bytes_used() >= f1_cost + PRIMARY_TTF.len() as u64,
        "retained standby companion must be charged against page budget immediately"
    );
    assert!(page_ample.fonts.bytes_used() <= ample_budget);

    // 3. Sibling primary font: when primary /F1 + valid /F2 fit together
    //    (`f1_cost + f2_cost`) but /F1's companion does not, neither primary
    //    font is starved.
    let page2_ample = zpdf::interpret_page(&doc_ample, 2, Default::default()).unwrap();
    let f2_cost = page2_ample
        .fonts
        .get_by_name("F2")
        .unwrap()
        .1
        .estimated_cache_bytes();
    let sibling_budget = f1_cost + f2_cost;
    assert!(
        sibling_budget < f1_cost + f2_cost + PRIMARY_TTF.len() as u64,
        "sibling budget must fit F1 primary + F2 but not F1 companion as well"
    );
    let doc_sibling = PdfDocument::open_with_limits(
        pdf,
        zpdf::ParseLimits {
            max_font_cache_bytes: sibling_budget,
            ..Default::default()
        },
    )
    .unwrap()
    .with_font_provider(provider_with_alias("BudgetCompanion"));
    let page2 = zpdf::interpret_page(&doc_sibling, 2, Default::default()).unwrap();
    let (f1_id2, f1_2) = page2.fonts.get_by_name("F1").unwrap();
    let (_, f2_2) = page2.fonts.get_by_name("F2").unwrap();
    assert!(f1_2.has_font_data() && f2_2.has_font_data());
    assert!(!page2.fonts.has_retained_embedded_companion(f1_id2));
    assert!(page2.fonts.bytes_used() <= sibling_budget);
    assert!(!page2.stats.diagnostics.resource_limit_reached);
    assert!(page2.is_complete());

    // 4. Multiple distinct corrupt fonts on the same page AND inside a Form
    //    XObject (including an inline font dict) that share one substitute face
    //    while each embedded companion exceeds the remaining budget: all
    //    primaries fit, no companion is retained, and when an ample budget is
    //    used an inline form font dict loads and draws its surviving CID on demand.
    let multi_pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F1 5 0 R /F2 9 0 R /F3 13 0 R >> >> /Contents 17 0 R >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /XObject << /Fm 18 0 R >> >> /Contents 19 0 R >>".to_vec(),
        // Font 1 (objs 5..8)
        b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+BudgetCompanion /Encoding /Identity-H /DescendantFonts [6 0 R] /ToUnicode 20 0 R >>".to_vec(),
        b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+BudgetCompanion /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /CIDToGIDMap 21 0 R /FontDescriptor 7 0 R /DW 1000 /W [1 [620 780 540]] >>".to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+BudgetCompanion /Flags 32 /FontFile2 8 0 R >>".to_vec(),
        stream("", &partial_ttf),
        // Font 2 (objs 9..12)
        b"<< /Type /Font /Subtype /Type0 /BaseFont /BCDEFG+BudgetCompanion /Encoding /Identity-H /DescendantFonts [10 0 R] /ToUnicode 20 0 R >>".to_vec(),
        b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /BCDEFG+BudgetCompanion /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /CIDToGIDMap 21 0 R /FontDescriptor 11 0 R /DW 1000 /W [1 [620 780 540]] >>".to_vec(),
        b"<< /Type /FontDescriptor /FontName /BCDEFG+BudgetCompanion /Flags 32 /FontFile2 12 0 R >>".to_vec(),
        stream("", &partial_ttf),
        // Font 3 (objs 13..16)
        b"<< /Type /Font /Subtype /Type0 /BaseFont /CDEFGH+BudgetCompanion /Encoding /Identity-H /DescendantFonts [14 0 R] /ToUnicode 20 0 R >>".to_vec(),
        b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /CDEFGH+BudgetCompanion /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /CIDToGIDMap 21 0 R /FontDescriptor 15 0 R /DW 1000 /W [1 [620 780 540]] >>".to_vec(),
        b"<< /Type /FontDescriptor /FontName /CDEFGH+BudgetCompanion /Flags 32 /FontFile2 16 0 R >>".to_vec(),
        stream("", &partial_ttf),
        // Page 0 content (obj 17)
        stream(
            "",
            b"0 0 0 rg BT /F1 24 Tf 15 40 Td <0001> Tj /F2 24 Tf 20 0 Td <0001> Tj /F3 24 Tf 20 0 Td <0001> Tj ET",
        ),
        // Form XObject with both indirect and inline font dicts (obj 18)
        stream(
            "/Type /XObject /Subtype /Form /BBox [0 0 180 100] /Resources << /Font << \
              /G1 5 0 R /G2 9 0 R \
              /GInline << /Type /Font /Subtype /Type0 /BaseFont /DEFGHI+BudgetCompanion /Encoding /Identity-H /DescendantFonts [14 0 R] /ToUnicode 20 0 R >> \
            >> >>",
            b"0 0 0 rg BT /G1 24 Tf 15 40 Td <0001> Tj /GInline 24 Tf 25 0 Td <00010003> Tj ET",
        ),
        // Page 1 content (obj 19)
        stream("", b"/Fm Do"),
        // Shared ToUnicode (obj 20) & CIDToGIDMap (obj 21)
        stream("", partial_tounicode),
        stream("", &cid_to_gid_bytes),
    ]);

    let multi_tight = PRIMARY_TTF.len() as u64 + 8192;
    assert!(multi_tight < (PRIMARY_TTF.len() as u64) * 2);
    let doc_multi_tight = PdfDocument::open_with_limits(
        multi_pdf.clone(),
        zpdf::ParseLimits {
            max_font_cache_bytes: multi_tight,
            ..Default::default()
        },
    )
    .unwrap()
    .with_font_provider(provider_with_alias("BudgetCompanion"));
    let mp0 = zpdf::interpret_page(&doc_multi_tight, 0, Default::default()).unwrap();
    for name in ["F1", "F2", "F3"] {
        let (fid, f) = mp0.fonts.get_by_name(name).unwrap();
        assert!(f.is_substitute && f.has_font_data());
        assert!(!mp0.fonts.has_retained_embedded_companion(fid));
    }
    assert!(mp0.fonts.bytes_used() <= multi_tight);
    assert!(mp0.is_complete());

    // Form XObject on tight budget also stays within limit and never retains over-budget companions.
    let mp1_tight = zpdf::interpret_page(&doc_multi_tight, 1, Default::default()).unwrap();
    assert!(mp1_tight.fonts.bytes_used() <= multi_tight);
    assert!(
        !mp1_tight.is_complete(),
        "CID 3 misses outline without companion"
    );

    // On an ample budget, the inline form font dict loads its surviving
    // companion on demand and draws both CID 1 and CID 3 cleanly.
    let doc_multi_ample = PdfDocument::open(multi_pdf)
        .unwrap()
        .with_font_provider(provider_with_alias("BudgetCompanion"));
    let mp1_ample = zpdf::interpret_page(&doc_multi_ample, 1, Default::default()).unwrap();
    assert!(
        mp1_ample.is_complete(),
        "inline form font dict must load its companion on demand and draw surviving CID 3"
    );

    // 5. Order-independent companion budget reservation: two corrupt fonts on a
    //    page where budget fits both primaries + ONLY ONE companion. The font
    //    fully covered by its substitute never reserves a companion slot, so
    //    the font that actually draws an uncovered CID always gets it regardless
    //    of resource-name ordering.
    for (covered_name, needed_name) in [("F1", "F2"), ("Z1", "A2"), ("A1", "Z2")] {
        let order_pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] \
                 /Resources << /Font << /{covered_name} 5 0 R /{needed_name} 9 0 R >> >> /Contents 4 0 R >>"
            )
            .into_bytes(),
            stream(
                "",
                format!(
                    "0 0 0 rg BT /{covered_name} 24 Tf 15 40 Td <0001> Tj \
                     /{needed_name} 24 Tf 25 0 Td <00010003> Tj ET"
                )
                .as_bytes(),
            ),
            b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+BudgetCompanion /Encoding /Identity-H /DescendantFonts [6 0 R] /ToUnicode 13 0 R >>".to_vec(),
            b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+BudgetCompanion /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /CIDToGIDMap 14 0 R /FontDescriptor 7 0 R /DW 1000 /W [1 [620 780 540]] >>".to_vec(),
            b"<< /Type /FontDescriptor /FontName /ABCDEF+BudgetCompanion /Flags 32 /FontFile2 8 0 R >>".to_vec(),
            stream("", &partial_ttf),
            b"<< /Type /Font /Subtype /Type0 /BaseFont /BCDEFG+BudgetCompanion /Encoding /Identity-H /DescendantFonts [10 0 R] /ToUnicode 13 0 R >>".to_vec(),
            b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /BCDEFG+BudgetCompanion /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /CIDToGIDMap 14 0 R /FontDescriptor 11 0 R /DW 1000 /W [1 [620 780 540]] >>".to_vec(),
            b"<< /Type /FontDescriptor /FontName /BCDEFG+BudgetCompanion /Flags 32 /FontFile2 12 0 R >>".to_vec(),
            stream("", &partial_ttf),
            stream("", partial_tounicode),
            stream("", &cid_to_gid_bytes),
        ]);
        // Budget fits both primaries (sharing PRIMARY_TTF) + ONE companion, NOT two companions.
        let one_companion_budget = (PRIMARY_TTF.len() as u64) * 2 + 8192;
        assert!(one_companion_budget < (PRIMARY_TTF.len() as u64) * 3);
        let doc_order = PdfDocument::open_with_limits(
            order_pdf,
            zpdf::ParseLimits {
                max_font_cache_bytes: one_companion_budget,
                ..Default::default()
            },
        )
        .unwrap()
        .with_font_provider(provider_with_alias("BudgetCompanion"));
        let page = zpdf::interpret_page(&doc_order, 0, Default::default()).unwrap();
        let (cov_id, _) = page.fonts.get_by_name(covered_name).unwrap();
        let (need_id, _) = page.fonts.get_by_name(needed_name).unwrap();
        assert!(
            !page.fonts.has_retained_embedded_companion(cov_id),
            "{covered_name}/{needed_name}: covered font must not take a companion slot"
        );
        assert!(
            page.fonts.has_retained_embedded_companion(need_id),
            "{covered_name}/{needed_name}: font needing CID 3 must retain its companion"
        );
        assert!(page.fonts.bytes_used() <= one_companion_budget);
        assert!(
            page.is_complete(),
            "{covered_name}/{needed_name}: surviving CID 3 must draw deterministically"
        );
    }

    // 6. Unused page companion never displaces a healthy font first loaded by a Form.
    let form_healthy_pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F1 5 0 R >> /XObject << /Fm 11 0 R >> >> /Contents 4 0 R >>".to_vec(),
        stream("", b"0 0 0 rg BT /F1 24 Tf 15 40 Td <0001> Tj ET /Fm Do"),
        b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+BudgetCompanion /Encoding /Identity-H /DescendantFonts [6 0 R] /ToUnicode 9 0 R >>".to_vec(),
        b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+BudgetCompanion /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /CIDToGIDMap 10 0 R /FontDescriptor 7 0 R /DW 1000 /W [1 [620 780 540]] >>".to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+BudgetCompanion /Flags 32 /FontFile2 8 0 R >>".to_vec(),
        stream("", &partial_ttf),
        stream("", partial_tounicode),
        stream("", &cid_to_gid_bytes),
        stream(
            "/Type /XObject /Subtype /Form /BBox [0 0 180 100] /Resources << /Font << /G1 12 0 R >> >>",
            b"0 0 0 rg BT /G1 24 Tf 45 40 Td (B) Tj ET",
        ),
        b"<< /Type /Font /Subtype /TrueType /BaseFont /ABCDEF+HealthyFormFace /Encoding /WinAnsiEncoding /FirstChar 65 /LastChar 67 /Widths [620 780 540] /FontDescriptor 13 0 R >>".to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+HealthyFormFace /Flags 32 /FontFile2 14 0 R >>".to_vec(),
        stream("", BOLD_TTF),
    ]);
    let doc_form_healthy = PdfDocument::open_with_limits(
        form_healthy_pdf,
        zpdf::ParseLimits {
            max_font_cache_bytes: sibling_budget,
            ..Default::default()
        },
    )
    .unwrap()
    .with_font_provider(provider_with_alias("BudgetCompanion"));
    let pf = zpdf::interpret_page(&doc_form_healthy, 0, Default::default()).unwrap();
    let (pf1_id, _) = pf.fonts.get_by_name("F1").unwrap();
    let (_, pg1) = pf.fonts.get_by_name("__font_12_0").unwrap();
    assert!(
        !pf.fonts.has_retained_embedded_companion(pf1_id),
        "unused page companion must stay deferred"
    );
    assert!(
        pg1.has_font_data(),
        "healthy Form font must not be displaced by an unused page companion"
    );
    assert!(pf.fonts.bytes_used() <= sibling_budget);
    assert!(!pf.stats.diagnostics.resource_limit_reached);
    assert!(pf.is_complete());
}

#[test]
fn shared_font_repeat_render_keeps_surviving_companion_glyphs_consistent() {
    let probe = zpdf_font::LoadedFont::new_with_data(
        zpdf_font::PdfFontType::TrueType,
        "Probe".into(),
        PRIMARY_TTF.to_vec(),
        zpdf_font::CidWidths::new(1000.0),
    );
    let gid_a = probe.unicode_glyph('A' as u32).unwrap().0;
    let gid_b = probe.unicode_glyph('B' as u32).unwrap().0;
    let gid_c = probe.unicode_glyph('C' as u32).unwrap().0;
    let maxp = sfnt_table_range(PRIMARY_TTF, b"maxp");
    let num_glyphs = u16::from_be_bytes([PRIMARY_TTF[maxp.start + 4], PRIMARY_TTF[maxp.start + 5]]);
    let corrupt_gids: Vec<u16> = (1..num_glyphs).filter(|&g| g != gid_c).collect();
    let partial_ttf = corrupt_selected_gids(PRIMARY_TTF, &corrupt_gids);
    let cid_to_gid_bytes: Vec<u8> = [0u16, gid_a, gid_b, gid_c]
        .into_iter()
        .flat_map(|g| g.to_be_bytes())
        .collect();
    let partial_tounicode = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
        1 begincodespacerange <0000> <FFFF> endcodespacerange \
        1 beginbfchar <0001> <0041> endbfchar \
        endcmap CMapName currentdict /CMap defineresource pop end end";

    // Page 0 loads valid embedded BOLD_TTF (partially filling SharedFonts).
    // Page 1 uses the partially readable Identity-H font drawing <00010003>
    // (CID 1 via substitute + CID 3 via surviving embedded companion).
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F0 5 0 R >> >> /Contents 8 0 R >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F1 9 0 R >> >> /Contents 15 0 R >>".to_vec(),
        b"<< /Type /Font /Subtype /TrueType /BaseFont /ABCDEF+FillerBold /Encoding /WinAnsiEncoding \
          /FirstChar 65 /LastChar 67 /Widths [620 780 540] /FontDescriptor 6 0 R >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+FillerBold /Flags 32 /FontFile2 7 0 R >>"
            .to_vec(),
        stream("", BOLD_TTF),
        stream("", b"0 0 0 rg BT /F0 24 Tf 15 40 Td (A) Tj ET"),
        b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+RepeatMixedCID /Encoding /Identity-H \
          /DescendantFonts [10 0 R] /ToUnicode 13 0 R >>"
            .to_vec(),
        b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+RepeatMixedCID \
          /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
          /CIDToGIDMap 14 0 R /FontDescriptor 11 0 R /DW 1000 /W [1 [620 780 540]] >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+RepeatMixedCID /Flags 32 /FontFile2 12 0 R >>"
            .to_vec(),
        stream("", &partial_ttf),
        stream("", partial_tounicode),
        stream("", &cid_to_gid_bytes),
        stream("", b"0 0 0 rg BT /F1 24 Tf 15 40 Td <00010003> Tj ET"),
    ]);

    // Measure exact costs first so we can pick a budget where:
    //   filler + primary <= budget < filler + primary + companion
    // AND a fresh page 1 cache fits primary + companion (`primary + companion <= budget`).
    let doc_measure = PdfDocument::open(pdf.clone())
        .unwrap()
        .with_font_provider(provider_with_alias("RepeatMixedCID"));
    let p0_measure = zpdf::interpret_page(&doc_measure, 0, Default::default()).unwrap();
    let p1_measure = zpdf::interpret_page(&doc_measure, 1, Default::default()).unwrap();
    let filler_cost = p0_measure.fonts.bytes_used();
    let primary_cost = p1_measure
        .fonts
        .get_by_name("F1")
        .unwrap()
        .1
        .estimated_cache_bytes();
    let pair_cost = p1_measure.fonts.bytes_used();
    let budget = (filler_cost + primary_cost).max(pair_cost + 1024);
    assert!(
        budget < filler_cost + pair_cost,
        "budget {budget} must fit page 1 pair ({pair_cost}) and filler+primary ({}) without fitting filler+pair ({})",
        filler_cost + primary_cost,
        filler_cost + pair_cost
    );

    let doc = PdfDocument::open_with_limits(
        pdf,
        zpdf::ParseLimits {
            max_font_cache_bytes: budget,
            ..Default::default()
        },
    )
    .unwrap()
    .with_font_provider(provider_with_alias("RepeatMixedCID"));

    // Page 0 fills part of SharedFonts.
    let p0 = zpdf::interpret_page(&doc, 0, Default::default()).unwrap();
    assert!(p0.is_complete());

    // Render Page 1 TWICE: both renders must draw CID 1 (substitute) and CID 3
    // (surviving embedded companion) identically.
    let first = zpdf::interpret_page(&doc, 1, Default::default()).unwrap();
    let second = zpdf::interpret_page(&doc, 1, Default::default()).unwrap();
    for (label, page) in [("first", &first), ("second", &second)] {
        assert!(
            page.is_complete(),
            "{label} render must recover both CIDs cleanly"
        );
        let (fid, _) = page.fonts.get_by_name("F1").unwrap();
        assert!(
            page.fonts.has_retained_embedded_companion(fid),
            "{label} render must retain companion"
        );
        let runs: Vec<_> = page
            .display_list
            .commands
            .iter()
            .filter_map(|c| match c {
                RenderCommand::DrawGlyphRun(r) => Some(r),
                _ => None,
            })
            .collect();
        assert_eq!(
            runs.len(),
            2,
            "{label} render must emit substitute + companion runs"
        );
    }

    let cpu_first = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&first.fonts)
        .render_display_list(&first.display_list, 1.0)
        .unwrap();
    let cpu_second = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&second.fonts)
        .render_display_list(&second.display_list, 1.0)
        .unwrap();
    assert_eq!(
        cpu_first.data, cpu_second.data,
        "repeat render with partially filled SharedFonts must produce identical pixels"
    );
    let cid1_adv = 620.0_f32 * 24.0 / 1000.0;
    let cid3_adv = 540.0_f32 * 24.0 / 1000.0;
    let px0 = (15.0 + cid1_adv).floor() as usize;
    let px1 = (15.0 + cid1_adv + cid3_adv).ceil() as usize;
    assert!(
        dark_pixels_in_rect(
            &cpu_second.data,
            cpu_second.width as usize,
            cpu_second.height as usize,
            px0,
            px1,
            30,
            70,
        ) > 10,
        "surviving CID 3 must remain visible on the second render"
    );
}

#[test]
fn structure_element_actual_text_satisfies_structured_text_completeness() {
    let build_tagged_pdf = |content: &[u8], extra_mcid_kid: bool| {
        let kids = if extra_mcid_kid {
            "[10 0 R 11 0 R]"
        } else {
            "[10 0 R]"
        };
        let mut objs = vec![
            b"<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 9 0 R /MarkInfo << /Marked true >> >>"
                .to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
                .to_vec(),
            stream("", content),
            b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+TaggedUnmappedCID /Encoding /Identity-H /DescendantFonts [6 0 R] >>"
                .to_vec(),
            b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+TaggedUnmappedCID \
              /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
              /FontDescriptor 7 0 R /DW 1000 >>"
                .to_vec(),
            b"<< /Type /FontDescriptor /FontName /ABCDEF+TaggedUnmappedCID /Flags 32 /FontFile2 8 0 R >>"
                .to_vec(),
            stream("", b"corrupt tagged cid font"),
            format!("<< /Type /StructTreeRoot /K {kids} >>").into_bytes(),
            b"<< /Type /StructElem /S /Span /Pg 3 0 R /ActualText (Label) /K 0 >>".to_vec(),
        ];
        if extra_mcid_kid {
            objs.push(b"<< /Type /StructElem /S /Span /Pg 3 0 R /K 1 >>".to_vec());
        }
        assemble(&objs)
    };

    // 1. Every unmapped fallback CID lives inside MCID 0, whose parent
    //    structure element provides `/ActualText (Label)`:
    //    - structured conversion succeeds with "Label" and decode_failures == 0
    //    - unstructured (geometric) conversion still rejects (decode_failures == 1)
    //    - visual interpretation still rejects missing outlines
    let covered_pdf = build_tagged_pdf(
        b"0 0 0 rg /Span << /MCID 0 >> BDC BT /F1 24 Tf 15 40 Td <0001> Tj ET EMC",
        false,
    );
    let doc_covered = PdfDocument::open(covered_pdf)
        .unwrap()
        .with_font_provider(Arc::new(FontProvider::memory_only()));

    let conv_struct = zpdf::convert_pdf(
        &doc_covered,
        &[0],
        zpdf::ConversionOptions {
            mode: zpdf::ConversionMode::TextOnly,
            use_structure: true,
        },
    )
    .unwrap();
    assert!(conv_struct.structure_order_used);
    assert_eq!(conv_struct.pages[0].text.trim(), "Label");
    assert_eq!(conv_struct.pages[0].diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(conv_struct.pages[0].diagnostics.missing_glyph_outlines, 1);
    assert_eq!(conv_struct.pages[0].diagnostics.decode_failures, 0);
    assert!(conv_struct.is_complete());
    conv_struct
        .ensure_complete()
        .expect("structure-element /ActualText satisfies structured text completeness");

    let conv_geom = zpdf::convert_pdf(
        &doc_covered,
        &[0],
        zpdf::ConversionOptions {
            mode: zpdf::ConversionMode::TextOnly,
            use_structure: false,
        },
    )
    .unwrap();
    assert_eq!(conv_geom.pages[0].diagnostics.decode_failures, 1);
    assert!(!conv_geom.is_complete());
    assert!(conv_geom.ensure_complete().is_err());

    let page_visual = zpdf::interpret_page(&doc_covered, 0, Default::default()).unwrap();
    assert_eq!(page_visual.stats.diagnostics.missing_glyph_outlines, 1);
    assert!(!page_visual.is_complete());

    // 2. Adding an uncovered MCID 1 (no `/ActualText`) with an unmapped CID
    //    must still fail structured text completeness.
    let partial_pdf = build_tagged_pdf(
        b"0 0 0 rg /Span << /MCID 0 >> BDC BT /F1 24 Tf 15 40 Td <0001> Tj ET EMC \
          /Span << /MCID 1 >> BDC BT /F1 24 Tf 60 40 Td <0002> Tj ET EMC",
        true,
    );
    let doc_partial = PdfDocument::open(partial_pdf)
        .unwrap()
        .with_font_provider(Arc::new(FontProvider::memory_only()));
    let conv_partial = zpdf::convert_pdf(
        &doc_partial,
        &[0],
        zpdf::ConversionOptions {
            mode: zpdf::ConversionMode::TextOnly,
            use_structure: true,
        },
    )
    .unwrap();
    assert_eq!(conv_partial.pages[0].diagnostics.decode_failures, 1);
    assert!(!conv_partial.is_complete());
    assert!(conv_partial.ensure_complete().is_err());

    // 3. Owning-stream scoping for matching numeric MCIDs across page and Form:
    //    Page sequence MCID 0 has `/ActualText (PageLabel)`, while Form `Fm`
    //    (obj 11 0 R) invoked outside that sequence uses its OWN MCID 0 for an
    //    undecodable fallback CID. Page `/ActualText` must NOT excuse the Form's
    //    MCID 0, so structured extraction must fail with `decode_failures == 1`.
    let page_vs_form_mcid0 = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 9 0 R /MarkInfo << /Marked true >> >>"
            .to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F1 5 0 R >> /XObject << /Fm 11 0 R >> >> /Contents 4 0 R >>".to_vec(),
        stream(
            "",
            b"0 0 0 rg /Span << /MCID 0 >> BDC BT /F1 24 Tf 15 40 Td <0001> Tj ET EMC /Fm Do",
        ),
        b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+TaggedUnmappedCID /Encoding /Identity-H /DescendantFonts [6 0 R] >>".to_vec(),
        b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+TaggedUnmappedCID \
          /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
          /FontDescriptor 7 0 R /DW 1000 >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+TaggedUnmappedCID /Flags 32 /FontFile2 8 0 R >>"
            .to_vec(),
        stream("", b"corrupt tagged cid font"),
        b"<< /Type /StructTreeRoot /K [10 0 R] >>".to_vec(),
        b"<< /Type /StructElem /S /Span /Pg 3 0 R /ActualText (PageLabel) /K 0 >>".to_vec(),
        stream(
            "/Type /XObject /Subtype /Form /BBox [0 0 180 100] /Resources << /Font << /F1 5 0 R >> >>",
            b"0 0 0 rg /Span << /MCID 0 >> BDC BT /F1 24 Tf 60 40 Td <0002> Tj ET EMC",
        ),
    ]);
    let doc_pvf = PdfDocument::open(page_vs_form_mcid0)
        .unwrap()
        .with_font_provider(Arc::new(FontProvider::memory_only()));
    let conv_pvf = zpdf::convert_pdf(
        &doc_pvf,
        &[0],
        zpdf::ConversionOptions {
            mode: zpdf::ConversionMode::TextOnly,
            use_structure: true,
        },
    )
    .unwrap();
    assert_eq!(
        conv_pvf.pages[0].diagnostics.decode_failures, 1,
        "Form MCID 0 must not inherit page MCID 0's /ActualText"
    );
    assert!(!conv_pvf.is_complete());
    assert!(conv_pvf.ensure_complete().is_err());

    // 4. Conversely, an `/MCR` referencing the Form stream `/Stm 11 0 R /MCID 0`
    //    under `/ActualText (FormLabel)` covers ONLY the Form's MCID 0 (succeeding
    //    when the page has no uncovered fallback text, and still rejecting an
    //    undecodable fallback CID in the page's own MCID 0).
    let form_stm_covered = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 9 0 R /MarkInfo << /Marked true >> >>"
            .to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /XObject << /Fm 11 0 R >> >> /Contents 4 0 R >>".to_vec(),
        stream("", b"/Fm Do"),
        b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+TaggedUnmappedCID /Encoding /Identity-H /DescendantFonts [6 0 R] >>".to_vec(),
        b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+TaggedUnmappedCID \
          /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
          /FontDescriptor 7 0 R /DW 1000 >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+TaggedUnmappedCID /Flags 32 /FontFile2 8 0 R >>"
            .to_vec(),
        stream("", b"corrupt tagged cid font"),
        b"<< /Type /StructTreeRoot /K [10 0 R] >>".to_vec(),
        b"<< /Type /StructElem /S /Span /Pg 3 0 R /ActualText (FormLabel) /K << /Type /MCR /Pg 3 0 R /Stm 11 0 R /MCID 0 >> >>".to_vec(),
        stream(
            "/Type /XObject /Subtype /Form /BBox [0 0 180 100] /Resources << /Font << /F1 5 0 R >> >>",
            b"0 0 0 rg /Span << /MCID 0 >> BDC BT /F1 24 Tf 60 40 Td <0002> Tj ET EMC",
        ),
    ]);
    let doc_fsc = PdfDocument::open(form_stm_covered)
        .unwrap()
        .with_font_provider(Arc::new(FontProvider::memory_only()));
    let conv_fsc = zpdf::convert_pdf(
        &doc_fsc,
        &[0],
        zpdf::ConversionOptions {
            mode: zpdf::ConversionMode::TextOnly,
            use_structure: true,
        },
    )
    .unwrap();
    assert_eq!(conv_fsc.pages[0].text.trim(), "FormLabel");
    assert_eq!(conv_fsc.pages[0].diagnostics.decode_failures, 0);
    assert!(conv_fsc.is_complete());

    let form_stm_vs_page_mcid0 = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 9 0 R /MarkInfo << /Marked true >> >>"
            .to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F1 5 0 R >> /XObject << /Fm 11 0 R >> >> /Contents 4 0 R >>".to_vec(),
        stream(
            "",
            b"0 0 0 rg /Span << /MCID 0 >> BDC BT /F1 24 Tf 15 40 Td <0001> Tj ET EMC /Fm Do",
        ),
        b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+TaggedUnmappedCID /Encoding /Identity-H /DescendantFonts [6 0 R] >>".to_vec(),
        b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+TaggedUnmappedCID \
          /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
          /FontDescriptor 7 0 R /DW 1000 >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+TaggedUnmappedCID /Flags 32 /FontFile2 8 0 R >>"
            .to_vec(),
        stream("", b"corrupt tagged cid font"),
        b"<< /Type /StructTreeRoot /K [10 0 R] >>".to_vec(),
        b"<< /Type /StructElem /S /Span /Pg 3 0 R /ActualText (FormLabel) /K << /Type /MCR /Pg 3 0 R /Stm 11 0 R /MCID 0 >> >>".to_vec(),
        stream(
            "/Type /XObject /Subtype /Form /BBox [0 0 180 100] /Resources << /Font << /F1 5 0 R >> >>",
            b"0 0 0 rg /Span << /MCID 0 >> BDC BT /F1 24 Tf 60 40 Td <0002> Tj ET EMC",
        ),
    ]);
    let doc_fvp = PdfDocument::open(form_stm_vs_page_mcid0)
        .unwrap()
        .with_font_provider(Arc::new(FontProvider::memory_only()));
    let conv_fvp = zpdf::convert_pdf(
        &doc_fvp,
        &[0],
        zpdf::ConversionOptions {
            mode: zpdf::ConversionMode::TextOnly,
            use_structure: true,
        },
    )
    .unwrap();
    assert_eq!(
        conv_fvp.pages[0].diagnostics.decode_failures, 1,
        "Form /MCR /Stm MCID 0 must not excuse Page MCID 0"
    );
    assert!(!conv_fvp.is_complete());

    // 5. Owning-stream scoping in structured text assembly: when a Form's
    //    `/MCR /Stm 11 0 R /MCID 0` carries `/ActualText (FormLabel)` AND the
    //    page itself has an unrelated readable span in its OWN `/MCID 0`
    //    (`PageText`) not placed in the structure tree, consuming the Form's
    //    MCID 0 must NOT drop the page's MCID 0 span — both strings must appear.
    let form_stm_preserves_page_mcid0 = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 9 0 R /MarkInfo << /Marked true >> >>"
            .to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] \
          /Resources << /Font << /F0 12 0 R /F1 5 0 R >> /XObject << /Fm 11 0 R >> >> \
          /Contents 4 0 R >>"
            .to_vec(),
        stream(
            "",
            b"0 0 0 rg /Span << /MCID 0 >> BDC BT /F0 12 Tf 15 80 Td (PageText) Tj ET EMC /Fm Do",
        ),
        b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+TaggedUnmappedCID /Encoding /Identity-H /DescendantFonts [6 0 R] >>".to_vec(),
        b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+TaggedUnmappedCID \
          /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
          /FontDescriptor 7 0 R /DW 1000 >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+TaggedUnmappedCID /Flags 32 /FontFile2 8 0 R >>"
            .to_vec(),
        stream("", b"corrupt tagged cid font"),
        b"<< /Type /StructTreeRoot /K [10 0 R] >>".to_vec(),
        b"<< /Type /StructElem /S /Span /Pg 3 0 R /ActualText (FormLabel) /K << /Type /MCR /Pg 3 0 R /Stm 11 0 R /MCID 0 >> >>".to_vec(),
        stream(
            "/Type /XObject /Subtype /Form /BBox [0 0 180 100] /Resources << /Font << /F1 5 0 R >> >>",
            b"0 0 0 rg /Span << /MCID 0 >> BDC BT /F1 24 Tf 60 40 Td <0002> Tj ET EMC",
        ),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
            .to_vec(),
    ]);
    let doc_preserve = PdfDocument::open(form_stm_preserves_page_mcid0)
        .unwrap()
        .with_font_provider(Arc::new(FontProvider::memory_only()));
    let conv_preserve = zpdf::convert_pdf(
        &doc_preserve,
        &[0],
        zpdf::ConversionOptions {
            mode: zpdf::ConversionMode::TextOnly,
            use_structure: true,
        },
    )
    .unwrap();
    assert!(
        conv_preserve.is_complete(),
        "Form /MCR /Stm replacement + readable page span must succeed cleanly"
    );
    assert_eq!(conv_preserve.pages[0].diagnostics.decode_failures, 0);
    assert!(
        conv_preserve.pages[0].text.contains("FormLabel")
            && conv_preserve.pages[0].text.contains("PageText"),
        "expected both FormLabel and PageText, got {:?}",
        conv_preserve.pages[0].text
    );

    // 6. Enclosing page `/ActualText` wrapping an untagged Form invocation:
    //    `/Span << /MCID 0 >> BDC /Fm Do EMC` covers unmapped fallback glyphs
    //    inside `Fm` (and subsumes any readable text inside `Fm` so it is not
    //    duplicated as leftover), while an invocation of `Fm` outside `/MCID 0`
    //    still rejects its unmapped glyph.
    let enclosing_form_pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 9 0 R /MarkInfo << /Marked true >> >>"
            .to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] \
          /Resources << /Font << /F0 12 0 R >> /XObject << /Fm 11 0 R >> >> /Contents 4 0 R >>"
            .to_vec(),
        stream("", b"0 0 0 rg /Span << /MCID 0 >> BDC /Fm Do EMC"),
        b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+TaggedUnmappedCID /Encoding /Identity-H /DescendantFonts [6 0 R] >>".to_vec(),
        b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+TaggedUnmappedCID \
          /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
          /FontDescriptor 7 0 R /DW 1000 >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+TaggedUnmappedCID /Flags 32 /FontFile2 8 0 R >>"
            .to_vec(),
        stream("", b"corrupt tagged cid font"),
        b"<< /Type /StructTreeRoot /K [10 0 R] >>".to_vec(),
        b"<< /Type /StructElem /S /Span /Pg 3 0 R /ActualText (Label) /K 0 >>".to_vec(),
        stream(
            "/Type /XObject /Subtype /Form /BBox [0 0 180 100] /Resources << /Font << /F0 12 0 R /F1 5 0 R >> >>",
            b"0 0 0 rg BT /F1 24 Tf 15 40 Td <0001> Tj /F0 12 Tf 40 0 Td (SubsumedForm) Tj ET",
        ),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
            .to_vec(),
    ]);
    let doc_enclosing = PdfDocument::open(enclosing_form_pdf)
        .unwrap()
        .with_font_provider(Arc::new(FontProvider::memory_only()));
    let conv_enclosing = zpdf::convert_pdf(
        &doc_enclosing,
        &[0],
        zpdf::ConversionOptions {
            mode: zpdf::ConversionMode::TextOnly,
            use_structure: true,
        },
    )
    .unwrap();
    assert_eq!(conv_enclosing.pages[0].text.trim(), "Label");
    assert_eq!(conv_enclosing.pages[0].diagnostics.decode_failures, 0);
    assert_eq!(
        conv_enclosing.pages[0].diagnostics.missing_glyph_outlines,
        1
    );
    assert!(conv_enclosing.is_complete());
    let conv_enclosing_geom = zpdf::convert_pdf(
        &doc_enclosing,
        &[0],
        zpdf::ConversionOptions {
            mode: zpdf::ConversionMode::TextOnly,
            use_structure: false,
        },
    )
    .unwrap();
    assert_eq!(conv_enclosing_geom.pages[0].diagnostics.decode_failures, 1);
    assert!(!conv_enclosing_geom.is_complete());
    let visual_enclosing = zpdf::interpret_page(&doc_enclosing, 0, Default::default()).unwrap();
    assert_eq!(visual_enclosing.stats.diagnostics.missing_glyph_outlines, 1);
    assert!(!visual_enclosing.is_complete());
}

#[test]
fn font_cache_only_interpreter_recovers_mixed_cid_companion_within_budget() {
    let probe = zpdf_font::LoadedFont::new_with_data(
        zpdf_font::PdfFontType::TrueType,
        "Probe".into(),
        PRIMARY_TTF.to_vec(),
        zpdf_font::CidWidths::new(1000.0),
    );
    let gid_a = probe.unicode_glyph('A' as u32).unwrap().0;
    let gid_b = probe.unicode_glyph('B' as u32).unwrap().0;
    let gid_c = probe.unicode_glyph('C' as u32).unwrap().0;
    let maxp = sfnt_table_range(PRIMARY_TTF, b"maxp");
    let num_glyphs = u16::from_be_bytes([PRIMARY_TTF[maxp.start + 4], PRIMARY_TTF[maxp.start + 5]]);
    let corrupt_gids: Vec<u16> = (1..num_glyphs).filter(|&g| g != gid_c).collect();
    let partial_ttf = corrupt_selected_gids(PRIMARY_TTF, &corrupt_gids);
    let cid_to_gid_bytes: Vec<u8> = [0u16, gid_a, gid_b, gid_c]
        .into_iter()
        .flat_map(|g| g.to_be_bytes())
        .collect();
    let partial_tounicode = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
        1 begincodespacerange <0000> <FFFF> endcodespacerange \
        1 beginbfchar <0001> <0041> endbfchar \
        endcmap CMapName currentdict /CMap defineresource pop end end";

    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        stream("", b"0 0 0 rg BT /F1 24 Tf 15 40 Td <00010003> Tj ET"),
        b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+FontCacheOnlyMixed /Encoding /Identity-H /DescendantFonts [6 0 R] /ToUnicode 9 0 R >>".to_vec(),
        b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+FontCacheOnlyMixed /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /CIDToGIDMap 10 0 R /FontDescriptor 7 0 R /DW 1000 /W [1 [620 780 540]] >>".to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+FontCacheOnlyMixed /Flags 32 /FontFile2 8 0 R >>".to_vec(),
        stream("", &partial_ttf),
        stream("", partial_tounicode),
        stream("", &cid_to_gid_bytes),
    ]);

    // 1. Ample budget: callers passing ONLY `doc.load_page_fonts(&page)` to
    //    `ContentInterpreter::with_fonts(&mut fonts)` (no `.with_document`,
    //    even after dropping `doc`) still recover both CID 1 and surviving CID 3.
    let doc_ample = PdfDocument::open(pdf.clone())
        .unwrap()
        .with_font_provider(provider_with_alias("FontCacheOnlyMixed"));
    let page = doc_ample.page(0).unwrap();
    let page_box = page.effective_box();
    let mut fonts = doc_ample.load_page_fonts(&page);
    let content = doc_ample.page_content_bytes(&page).unwrap();
    drop(doc_ample);

    let (dl, stats) = zpdf::ContentInterpreter::new(page_box)
        .with_fonts(&mut fonts)
        .interpret_with_stats(&content);
    assert!(
        stats.diagnostics.is_complete(),
        "font-cache-only interpreter must recover both substitute CID 1 and embedded CID 3: {:?}",
        stats.diagnostics
    );
    let (fid, _) = fonts.get_by_name("F1").unwrap();
    assert!(fonts.has_admitted_embedded_companion(fid));
    let runs: Vec<_> = dl
        .commands
        .iter()
        .filter_map(|c| match c {
            RenderCommand::DrawGlyphRun(r) => Some(r),
            _ => None,
        })
        .collect();
    assert_eq!(runs.len(), 2, "must emit substitute + companion glyph runs");

    let cpu = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .render_display_list(&dl, 1.0)
        .unwrap();
    let cid1_adv = 620.0_f32 * 24.0 / 1000.0;
    let cid3_adv = 540.0_f32 * 24.0 / 1000.0;
    assert!(
        dark_pixels_in_rect(
            &cpu.data,
            cpu.width as usize,
            cpu.height as usize,
            15,
            (15.0 + cid1_adv).ceil() as usize,
            30,
            70,
        ) > 10,
        "CID 1 substitute glyph must draw"
    );
    assert!(
        dark_pixels_in_rect(
            &cpu.data,
            cpu.width as usize,
            cpu.height as usize,
            (15.0 + cid1_adv).floor() as usize,
            (15.0 + cid1_adv + cid3_adv).ceil() as usize,
            30,
            70,
        ) > 10,
        "surviving embedded CID 3 glyph must draw without PdfFile"
    );

    // 2. Tight budget: `load_page_fonts` drops the over-budget standby companion
    //    immediately so `fonts.bytes_used()` stays within `max_font_cache_bytes`.
    let tight_budget = PRIMARY_TTF.len() as u64 + 8192;
    assert!(tight_budget < (PRIMARY_TTF.len() as u64) * 2);
    let doc_tight = PdfDocument::open_with_limits(
        pdf,
        zpdf::ParseLimits {
            max_font_cache_bytes: tight_budget,
            ..Default::default()
        },
    )
    .unwrap()
    .with_font_provider(provider_with_alias("FontCacheOnlyMixed"));
    let tight_page = doc_tight.page(0).unwrap();
    let mut tight_fonts = doc_tight.load_page_fonts(&tight_page);
    let tight_content = doc_tight.page_content_bytes(&tight_page).unwrap();
    let (tight_fid, _) = tight_fonts.get_by_name("F1").unwrap();
    assert!(!tight_fonts.has_retained_embedded_companion(tight_fid));
    assert!(tight_fonts.bytes_used() <= tight_budget);
    let (_, tight_stats) = zpdf::ContentInterpreter::new(tight_page.effective_box())
        .with_fonts(&mut tight_fonts)
        .interpret_with_stats(&tight_content);
    assert!(tight_fonts.bytes_used() <= tight_budget);
    assert_eq!(tight_stats.diagnostics.missing_glyph_outlines, 1);
}

#[test]
fn partially_corrupt_embedded_subset_substitutes_only_unreadable_shown_glyphs() {
    // TrueType subset where 'B' and 'C' remain readable (so the font-wide
    // probe keeps the embedded face as primary) while 'A' has a malformed
    // nonblank `glyf` record (note: BOLD_TTF provides 'A', and gid_a=34 is
    // un-sampled by the 64-glyph load-time probe so this exercises show-time
    // unreadable detection + on-demand descriptor substitution).
    let probe = zpdf_font::LoadedFont::new_with_data(
        zpdf_font::PdfFontType::TrueType,
        "Probe".into(),
        PRIMARY_TTF.to_vec(),
        zpdf_font::CidWidths::new(1000.0),
    );
    let gid_a = probe.unicode_glyph('A' as u32).unwrap().0;
    let gid_c = probe.unicode_glyph('C' as u32).unwrap().0;
    let partial_ttf = corrupt_selected_gids(PRIMARY_TTF, &[gid_a]);

    let make_pdf_with = |ttf: &[u8], content: &[u8]| {
        assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
            stream("", content),
            b"<< /Type /Font /Subtype /TrueType /BaseFont /ABCDEF+PartialSubset \
              /Encoding /WinAnsiEncoding /FirstChar 65 /LastChar 67 /Widths [620 780 540] \
              /FontDescriptor 6 0 R >>"
                .to_vec(),
            b"<< /Type /FontDescriptor /FontName /ABCDEF+PartialSubset /Flags 32 /FontWeight 700 /FontFile2 7 0 R >>".to_vec(),
            stream("", ttf),
        ])
    };
    let make_pdf = |content: &[u8]| make_pdf_with(&partial_ttf, content);

    let provider = Arc::new(FontProvider::memory_only());
    provider
        .register_with_aliases(PRIMARY_TTF, 0, &["PartialSubset"])
        .unwrap();
    provider
        .register_with_aliases(BOLD_TTF, 0, &["PartialSubset"])
        .unwrap();

    // 1. Page showing ONLY readable glyphs 'B' and 'C': embedded primary draws
    //    both cleanly with ZERO fallback diagnostic and ZERO substitute admission.
    let healthy_only_doc =
        PdfDocument::open(make_pdf(b"0 0 0 rg BT /F1 24 Tf 15 40 Td (BC) Tj ET"))
            .unwrap()
            .with_font_provider(Arc::clone(&provider));
    let healthy_page = zpdf::interpret_page(&healthy_only_doc, 0, Default::default()).unwrap();
    let (fid_bc, f_bc) = healthy_page.fonts.get_by_name("F1").unwrap();
    assert!(!f_bc.is_substitute, "embedded face stays primary");
    assert!(!healthy_page.fonts.has_admitted_embedded_companion(fid_bc));
    assert_eq!(healthy_page.stats.diagnostics.embedded_font_fallbacks, 0);
    assert_eq!(healthy_page.stats.diagnostics.missing_glyph_outlines, 0);
    assert!(healthy_page.is_complete());

    // 2. Page showing ONLY the unreadable glyph 'A': triggers descriptor-based
    //    substitution (selecting bold BOLD_TTF via /FontWeight 700), records
    //    embedded_font_fallbacks == 1, preserves the PDF advance (620), and
    //    completes cleanly with ink.
    let a_only_pdf = make_pdf(b"0 0 0 rg BT /F1 24 Tf 15 40 Td (A) Tj ET");
    let a_doc = PdfDocument::open(a_only_pdf.clone())
        .unwrap()
        .with_font_provider(Arc::clone(&provider));
    let a_page = zpdf::interpret_page(&a_doc, 0, Default::default()).unwrap();
    let (fid_a, f_a) = a_page.fonts.get_by_name("F1").unwrap();
    assert!(!f_a.is_substitute);
    assert!(a_page.fonts.has_admitted_embedded_companion(fid_a));
    assert_eq!(a_page.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(a_page.stats.diagnostics.missing_glyph_outlines, 0);
    assert!(a_page.is_complete());
    let a_runs: Vec<_> = a_page
        .display_list
        .commands
        .iter()
        .filter_map(|cmd| match cmd {
            RenderCommand::DrawGlyphRun(run) => Some(run),
            _ => None,
        })
        .collect();
    assert_eq!(a_runs.len(), 1);
    assert_ne!(
        a_runs[0].font_id, fid_a,
        "unreadable 'A' must draw from the descriptor substitute"
    );
    assert_eq!(
        a_page
            .fonts
            .get(a_runs[0].font_id)
            .unwrap()
            .font_data
            .as_deref()
            .unwrap(),
        BOLD_TTF,
        "descriptor /FontWeight 700 must select the bold substitute face"
    );
    assert!((a_runs[0].glyphs[0].advance - 620.0 * 24.0 / 1000.0).abs() < 0.01);
    let cpu_a = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&a_page.fonts)
        .render_display_list(&a_page.display_list, 1.0)
        .unwrap();
    assert!(
        dark_pixels_in_rect(
            &cpu_a.data,
            cpu_a.width as usize,
            cpu_a.height as usize,
            15,
            35,
            30,
            70
        ) > 10
    );

    // 3. Page showing 'A', 'B', and 'C' together: 'A' draws from the descriptor
    //    substitute while 'B' and 'C' stay on the embedded face, preserving all
    //    PDF advances.
    let abc_doc = PdfDocument::open(make_pdf(b"0 0 0 rg BT /F1 24 Tf 15 40 Td (ABC) Tj ET"))
        .unwrap()
        .with_font_provider(Arc::clone(&provider));
    let abc_page = zpdf::interpret_page(&abc_doc, 0, Default::default()).unwrap();
    let (fid_abc, _) = abc_page.fonts.get_by_name("F1").unwrap();
    let abc_runs: Vec<_> = abc_page
        .display_list
        .commands
        .iter()
        .filter_map(|cmd| match cmd {
            RenderCommand::DrawGlyphRun(run) => Some(run),
            _ => None,
        })
        .collect();
    assert_eq!(abc_runs.len(), 2, "A on substitute, BC on embedded face");
    assert_ne!(abc_runs[0].font_id, fid_abc);
    assert_eq!(abc_runs[0].glyphs.len(), 1);
    assert_eq!(abc_runs[1].font_id, fid_abc);
    assert_eq!(abc_runs[1].glyphs.len(), 2);
    let expected_adv = [
        620.0 * 24.0 / 1000.0,
        780.0 * 24.0 / 1000.0,
        540.0 * 24.0 / 1000.0,
    ];
    let expected_x = [0.0, expected_adv[0], expected_adv[0] + expected_adv[1]];
    assert!((abc_runs[0].glyphs[0].x - expected_x[0]).abs() < 0.01);
    assert!((abc_runs[0].glyphs[0].advance - expected_adv[0]).abs() < 0.01);
    assert!((abc_runs[1].glyphs[0].x - expected_x[1]).abs() < 0.01);
    assert!((abc_runs[1].glyphs[0].advance - expected_adv[1]).abs() < 0.01);
    assert!((abc_runs[1].glyphs[1].x - expected_x[2]).abs() < 0.01);
    assert!((abc_runs[1].glyphs[1].advance - expected_adv[2]).abs() < 0.01);
    let cpu_abc = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&abc_page.fonts)
        .render_display_list(&abc_page.display_list, 1.0)
        .unwrap();
    for (idx, (&gx, &adv)) in expected_x.iter().zip(expected_adv.iter()).enumerate() {
        let px0 = (15.0 + gx).floor().max(0.0) as usize;
        let px1 = (15.0 + gx + adv).ceil() as usize;
        assert!(
            dark_pixels_in_rect(
                &cpu_abc.data,
                cpu_abc.width as usize,
                cpu_abc.height as usize,
                px0,
                px1,
                30,
                70,
            ) > 10,
            "mixed sub+embedded glyph {idx} cell x={px0}..{px1} must have ink"
        );
    }
    assert_eq!(abc_page.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(abc_page.stats.diagnostics.missing_glyph_outlines, 0);
    assert!(abc_page.is_complete());

    // 4. Without any substitute (memory_only provider), showing the unreadable
    //    glyph 'A' records both embedded_font_fallbacks=1 and
    //    missing_glyph_outlines=1 (never silently accepting dropped ink).
    let no_sub_doc = PdfDocument::open(a_only_pdf)
        .unwrap()
        .with_font_provider(Arc::new(FontProvider::memory_only()));
    let no_sub_page = zpdf::interpret_page(&no_sub_doc, 0, Default::default()).unwrap();
    assert_eq!(no_sub_page.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(no_sub_page.stats.diagnostics.missing_glyph_outlines, 1);
    assert!(!no_sub_page.is_complete());

    // 5a. Even when the load-time probe misses the damaged glyph (`gid_a=34` is
    //     unsampled so the embedded font probes `Healthy`), the cache-only
    //     `ContentInterpreter::with_fonts` path (with `doc` dropped and no
    //     `PdfFile`) still recovers unreadable 'A' via the retained
    //     `DescriptorSubstituteSpec` while keeping 'B' on the embedded face.
    let unsampled_doc = PdfDocument::open(make_pdf(b"0 0 0 rg BT /F1 24 Tf 15 40 Td (AB) Tj ET"))
        .unwrap()
        .with_font_provider(Arc::clone(&provider));
    let unsampled_page = unsampled_doc.page(0).unwrap();
    let unsampled_box = unsampled_page.effective_box();
    let mut unsampled_fonts = unsampled_doc.load_page_fonts(&unsampled_page);
    let unsampled_content = unsampled_doc.page_content_bytes(&unsampled_page).unwrap();
    drop(unsampled_doc);
    let (unsampled_dl, unsampled_stats) = zpdf::ContentInterpreter::new(unsampled_box)
        .with_fonts(&mut unsampled_fonts)
        .interpret_with_stats(&unsampled_content);
    let (unsampled_fid, _) = unsampled_fonts.get_by_name("F1").unwrap();
    assert!(unsampled_fonts.has_admitted_embedded_companion(unsampled_fid));
    assert_eq!(unsampled_stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(unsampled_stats.diagnostics.missing_glyph_outlines, 0);
    assert!(unsampled_stats.diagnostics.is_complete());
    let unsampled_runs: Vec<_> = unsampled_dl
        .commands
        .iter()
        .filter_map(|cmd| match cmd {
            RenderCommand::DrawGlyphRun(run) => Some(run),
            _ => None,
        })
        .collect();
    assert_eq!(unsampled_runs.len(), 2, "A on bold sub, B on embedded");
    assert_ne!(unsampled_runs[0].font_id, unsampled_fid);
    assert_eq!(
        unsampled_fonts
            .get(unsampled_runs[0].font_id)
            .unwrap()
            .font_data
            .as_deref()
            .unwrap(),
        BOLD_TTF
    );
    assert_eq!(unsampled_runs[1].font_id, unsampled_fid);

    // 5b. When the load-time probe itself observes partial corruption (gid_c=36
    //     is sampled), the cache-only `with_fonts` path (no PdfFile) likewise
    //     resolves the descriptor substitute for unreadable 'A'.
    let sampled_partial_ttf = corrupt_selected_gids(PRIMARY_TTF, &[gid_a, gid_c]);
    let sampled_doc = PdfDocument::open(make_pdf_with(
        &sampled_partial_ttf,
        b"0 0 0 rg BT /F1 24 Tf 15 40 Td (AB) Tj ET",
    ))
    .unwrap()
    .with_font_provider(provider);
    let sampled_page = sampled_doc.page(0).unwrap();
    let sampled_box = sampled_page.effective_box();
    let mut sampled_fonts = sampled_doc.load_page_fonts(&sampled_page);
    let sampled_content = sampled_doc.page_content_bytes(&sampled_page).unwrap();
    drop(sampled_doc);
    let (_, sampled_stats) = zpdf::ContentInterpreter::new(sampled_box)
        .with_fonts(&mut sampled_fonts)
        .interpret_with_stats(&sampled_content);
    let (sampled_fid, _) = sampled_fonts.get_by_name("F1").unwrap();
    assert!(sampled_fonts.has_admitted_embedded_companion(sampled_fid));
    assert_eq!(sampled_stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(sampled_stats.diagnostics.missing_glyph_outlines, 0);
    assert!(sampled_stats.diagnostics.is_complete());
}

#[test]
fn fallback_text_completeness_handles_pattern_tf_invisible_limits_and_one_byte_tounicode() {
    // 1. Patterned text whose tiling cell actually executes `/F0 10 Tf` with a
    //    healthy embedded font must still check text completeness against the
    //    outer corrupt font `/F1` (`<0022>` draws the glyph clip via substitute
    //    so `/P1` runs; `<0001>` has no `/ToUnicode` entry so decode_failures==1).
    let partial_tu = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
        1 begincodespacerange <0000> <FFFF> endcodespacerange \
        1 beginbfchar <0022> <0041> endbfchar \
        endcmap CMapName currentdict /CMap defineresource pop end end";
    let pattern_pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] \
          /Resources << /Font << /F1 5 0 R /F0 9 0 R >> /Pattern << /P1 12 0 R >> >> \
          /Contents 4 0 R >>"
            .to_vec(),
        stream(
            "",
            b"/Pattern cs /P1 scn BT /F1 24 Tf 15 40 Td <00220001> Tj ET",
        ),
        b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+PatternOuterCID /Encoding /Identity-H \
          /DescendantFonts [6 0 R] /ToUnicode 13 0 R >>"
            .to_vec(),
        b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+PatternOuterCID \
          /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
          /FontDescriptor 7 0 R /DW 1000 >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+PatternOuterCID /Flags 32 /FontFile2 8 0 R >>"
            .to_vec(),
        stream("", b"corrupt pattern outer font"),
        b"<< /Type /Font /Subtype /TrueType /BaseFont /ABCDEF+PatternTileFace \
          /Encoding /WinAnsiEncoding /FirstChar 65 /LastChar 67 /Widths [620 780 540] \
          /FontDescriptor 10 0 R >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+PatternTileFace /Flags 32 /FontFile2 11 0 R >>"
            .to_vec(),
        stream("", PRIMARY_TTF),
        stream(
            "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 \
             /BBox [0 0 20 20] /XStep 20 /YStep 20 \
             /Resources << /Font << /F0 9 0 R >> >>",
            b"0 0 0 rg BT /F0 10 Tf 2 2 Td (B) Tj ET",
        ),
        stream("", partial_tu),
    ]);
    let doc_pat = PdfDocument::open(pattern_pdf)
        .unwrap()
        .with_font_provider(provider_with_alias("PatternOuterCID"));
    let pat_page = zpdf::interpret_page(&doc_pat, 0, Default::default()).unwrap();
    let (f0_id, _) = pat_page.fonts.get_by_name("F0").unwrap();
    assert!(
        pat_page.display_list.commands.iter().any(|cmd| matches!(
            cmd,
            RenderCommand::DrawGlyphRun(run) if run.font_id == f0_id && !run.glyphs.is_empty()
        )),
        "tiling pattern cell must actually execute `/F0 10 Tf` and emit a DrawGlyphRun for /F0"
    );
    let conv_pat = zpdf::convert_pdf(&doc_pat, &[0], Default::default()).unwrap();
    assert_eq!(conv_pat.pages[0].diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(conv_pat.pages[0].diagnostics.decode_failures, 1);
    assert!(!conv_pat.is_complete());

    // 2. Render-mode-3 invisible text (and /ActualText) using an embedded font
    //    whose stream hit `max_decoded_stream_bytes` must still merge the
    //    load-time resource limit so `ensure_complete()` rejects it.
    for content in [
        b"BT /F1 24 Tf 3 Tr 15 40 Td (A) Tj ET".as_slice(),
        b"/Span << /ActualText (A) >> BDC BT /F1 24 Tf 3 Tr 15 40 Td (A) Tj ET EMC".as_slice(),
    ] {
        let limited_pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
            stream("", content),
            b"<< /Type /Font /Subtype /TrueType /BaseFont /ABCDEF+LimitedInvisible \
              /Encoding /WinAnsiEncoding /FirstChar 65 /LastChar 67 /Widths [620 780 540] \
              /FontDescriptor 6 0 R >>"
                .to_vec(),
            b"<< /Type /FontDescriptor /FontName /ABCDEF+LimitedInvisible /Flags 32 /FontFile2 7 0 R >>".to_vec(),
            stream("", PRIMARY_TTF),
        ]);
        let doc_lim = PdfDocument::open_with_limits(
            limited_pdf,
            zpdf::ParseLimits {
                max_decoded_stream_bytes: 4096,
                ..Default::default()
            },
        )
        .unwrap()
        .with_font_provider(provider_with_alias("LimitedInvisible"));
        let conv_lim = zpdf::convert_pdf(&doc_lim, &[0], Default::default()).unwrap();
        assert!(
            conv_lim.pages[0].diagnostics.resource_limit_reached,
            "load-time stream limit must be merged for invisible / ActualText shows"
        );
        assert!(!conv_lim.is_complete());
        assert!(conv_lim.ensure_complete().is_err());
    }

    // 3. Direct Type0 fallback font without `cid_cmap` and with a 1-byte
    //    /ToUnicode map: extraction segments `41 42` as two 1-byte codes ("AB"),
    //    so completeness check must agree and report decode_failures == 0.
    let one_byte_tu = zpdf_font::cmap::ToUnicodeMap::parse(
        b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
          1 begincodespacerange <00> <FF> endcodespacerange \
          2 beginbfchar <41> <0041> <42> <0042> endbfchar \
          endcmap CMapName currentdict /CMap defineresource pop end end",
    );
    let mut font = zpdf_font::LoadedFont::new_placeholder("OneByteType0".into());
    font.font_type = zpdf_font::PdfFontType::Type0CidType2;
    font.cid_cmap = None;
    font.to_unicode = Some(one_byte_tu);
    let mut fonts = zpdf::FontCache::new();
    let fid = fonts.insert("F1".into(), font);
    let mut fallback_diag = zpdf::ProcessingDiagnostics::default();
    fallback_diag.record_embedded_font_fallback(None);
    fonts.record_embedded_fallback(fid, fallback_diag);
    let mut spans = Vec::new();
    let (_, stats) = zpdf::ContentInterpreter::new(zpdf::Rect::new(0.0, 0.0, 200.0, 100.0))
        .with_fonts(&mut fonts)
        .with_text_sink(&mut spans)
        .interpret_with_stats(b"BT /F1 12 Tf 10 50 Td <4142> Tj ET");
    assert_eq!(zpdf::spans_to_text(spans, 2.0).trim(), "AB");
    assert_eq!(stats.diagnostics.decode_failures, 0);
    assert!(stats.diagnostics.is_text_complete());
}

/// Wipe only the specified glyph IDs' nonempty `glyf` records with zeros
/// (turning them into damaged zero-contour records while leaving other glyphs
/// and genuine spaces intact).
fn zero_selected_gids(ttf: &[u8], zero_gids: &[u16]) -> Vec<u8> {
    let mut out = ttf.to_vec();
    let head = sfnt_table_range(&out, b"head");
    let loca = sfnt_table_range(&out, b"loca");
    let glyf = sfnt_table_range(&out, b"glyf");
    let long = out[head.start + 50..head.start + 52] == [0, 1];
    let offset = |buf: &[u8], gid: usize| -> usize {
        if long {
            u32::from_be_bytes(
                buf[loca.start + gid * 4..loca.start + gid * 4 + 4]
                    .try_into()
                    .unwrap(),
            ) as usize
        } else {
            u16::from_be_bytes(
                buf[loca.start + gid * 2..loca.start + gid * 2 + 2]
                    .try_into()
                    .unwrap(),
            ) as usize
                * 2
        }
    };
    for &gid in zero_gids {
        let start = offset(&out, gid as usize);
        let end = offset(&out, gid as usize + 1);
        if start < end {
            out[glyf.start + start..glyf.start + end].fill(0x00);
        }
    }
    out
}

/// Build a valid sfnt variant of `PRIMARY_TTF` where genuine space (`gid_space`)
/// has a 12-byte zero-contour header (nonempty blank space record) AND `gid_a`
/// is truncated to a 12-byte zero-contour header (valid header shape, so only
/// the bounded Unicode-`cmap` check distinguishes the damaged visible letter
/// from the genuine space).
fn valid_header_zero_contour_mixed_ttf(ttf: &[u8], gid_space: u16, gid_a: u16) -> Vec<u8> {
    let head = sfnt_table_range(ttf, b"head");
    let loca = sfnt_table_range(ttf, b"loca");
    let glyf = sfnt_table_range(ttf, b"glyf");
    let maxp = sfnt_table_range(ttf, b"maxp");
    assert_eq!(
        &ttf[head.start + 50..head.start + 52],
        &[0, 1],
        "expected long loca"
    );
    let num_glyphs = u16::from_be_bytes([ttf[maxp.start + 4], ttf[maxp.start + 5]]) as usize;
    let old_off = |gid: usize| -> usize {
        u32::from_be_bytes(
            ttf[loca.start + gid * 4..loca.start + gid * 4 + 4]
                .try_into()
                .unwrap(),
        ) as usize
    };

    // Rebuild `glyf` and `loca` cleanly with 12-byte zero-contour records for
    // both `gid_space` and `gid_a`, keeping every other glyph byte-identical.
    let mut new_glyf: Vec<u8> = Vec::with_capacity(glyf.len() + 16);
    let mut new_loca: Vec<u8> = Vec::with_capacity((num_glyphs + 1) * 4);
    for g in 0..num_glyphs {
        new_loca.extend_from_slice(&(new_glyf.len() as u32).to_be_bytes());
        if g == gid_space as usize || g == gid_a as usize {
            new_glyf.extend_from_slice(&[0u8; 12]);
        } else {
            let s = old_off(g);
            let e = old_off(g + 1);
            new_glyf.extend_from_slice(&ttf[glyf.start + s..glyf.start + e]);
        }
    }
    new_loca.extend_from_slice(&(new_glyf.len() as u32).to_be_bytes());
    assert_eq!(new_loca.len(), loca.len());

    // Repack sfnt tables cleanly at 4-byte aligned offsets.
    let num_tables = u16::from_be_bytes([ttf[4], ttf[5]]) as usize;
    let mut out = Vec::with_capacity(ttf.len() + 64);
    out.extend_from_slice(&ttf[..12 + num_tables * 16]);
    for i in 0..num_tables {
        let rec = 12 + i * 16;
        let tag = &ttf[rec..rec + 4];
        let old_table_off = u32::from_be_bytes(ttf[rec + 8..rec + 12].try_into().unwrap()) as usize;
        let old_table_len =
            u32::from_be_bytes(ttf[rec + 12..rec + 16].try_into().unwrap()) as usize;
        let table_bytes: &[u8] = if tag == b"glyf" {
            &new_glyf
        } else if tag == b"loca" {
            &new_loca
        } else {
            &ttf[old_table_off..old_table_off + old_table_len]
        };
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
        let new_off = out.len() as u32;
        let new_len = table_bytes.len() as u32;
        out[rec + 8..rec + 12].copy_from_slice(&new_off.to_be_bytes());
        out[rec + 12..rec + 16].copy_from_slice(&new_len.to_be_bytes());
        out.extend_from_slice(table_bytes);
    }
    out
}

#[test]
fn damaged_zero_contour_glyph_in_mixed_subset_recovers_while_genuine_space_stays_embedded() {
    let probe = zpdf_font::LoadedFont::new_with_data(
        zpdf_font::PdfFontType::TrueType,
        "Probe".into(),
        PRIMARY_TTF.to_vec(),
        zpdf_font::CidWidths::new(1000.0),
    );
    let gid_space = probe.unicode_glyph(' ' as u32).unwrap().0;
    let gid_a = probe.unicode_glyph('A' as u32).unwrap().0;

    // Exercise BOTH damaged zero-contour shapes in an otherwise readable mixed
    // subset containing genuine space (' '), 'B', and 'C':
    // 1. Wiped 36-byte zero-contour 'A' + zero-length genuine space
    // 2. 12-byte valid-header zero-contour 'A' + 12-byte valid-header genuine space
    let wiped_ttf = zero_selected_gids(PRIMARY_TTF, &[gid_a]);
    let header_ttf = valid_header_zero_contour_mixed_ttf(PRIMARY_TTF, gid_space, gid_a);

    let provider = Arc::new(FontProvider::memory_only());
    provider
        .register_with_aliases(PRIMARY_TTF, 0, &["ZeroContourMix"])
        .unwrap();
    provider
        .register_with_aliases(BOLD_TTF, 0, &["ZeroContourMix"])
        .unwrap();

    // WinAnsi widths for codes 32 (' ')..=67 ('C'): space=300, A=620, B=780, C=540.
    let mut widths = [500u16; 36];
    widths[0] = 300; // code 32 (' ')
    widths[65 - 32] = 620; // code 65 ('A')
    widths[66 - 32] = 780; // code 66 ('B')
    widths[67 - 32] = 540; // code 67 ('C')
    let widths_str: String = widths
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(" ");

    let make_pdf = |ttf: &[u8], content: &[u8]| {
        assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
            stream("", content),
            format!(
                "<< /Type /Font /Subtype /TrueType /BaseFont /ABCDEF+ZeroContourMix \
                 /Encoding /WinAnsiEncoding /FirstChar 32 /LastChar 67 /Widths [{widths_str}] \
                 /FontDescriptor 6 0 R >>"
            )
            .into_bytes(),
            b"<< /Type /FontDescriptor /FontName /ABCDEF+ZeroContourMix /Flags 32 /FontWeight 700 /FontFile2 7 0 R >>".to_vec(),
            stream("", ttf),
        ])
    };

    for (label, ttf) in [
        ("wiped-36B-zero-contour", &wiped_ttf),
        ("valid-12B-header-zero-contour", &header_ttf),
    ] {
        // 1. Showing healthy letters + genuine space ("B C"): genuine space must
        //    NOT trigger fallback or substitute admission; embedded face draws cleanly.
        let space_doc =
            PdfDocument::open(make_pdf(ttf, b"0 0 0 rg BT /F1 24 Tf 15 40 Td (B C) Tj ET"))
                .unwrap()
                .with_font_provider(Arc::new(FontProvider::memory_only()));
        let space_page = zpdf::interpret_page(&space_doc, 0, Default::default()).unwrap();
        let (fid_sp, f_sp) = space_page.fonts.get_by_name("F1").unwrap();
        assert!(!f_sp.is_substitute, "{label}: embedded face stays primary");
        assert!(!space_page.fonts.has_admitted_embedded_companion(fid_sp));
        assert_eq!(
            space_page.stats.diagnostics.embedded_font_fallbacks, 0,
            "{label}: genuine space must not trigger fallback"
        );
        assert_eq!(space_page.stats.diagnostics.missing_glyph_outlines, 0);
        assert!(space_page.is_complete(), "{label}: B C must stay complete");

        // 2. Showing damaged zero-contour 'A' + genuine space + healthy 'B' ("A B")
        //    with NO provider and NO sibling font: 'A' must NOT silently disappear —
        //    must report embedded_font_fallbacks=1 and missing_glyph_outlines=1.
        let ab_pdf = make_pdf(ttf, b"0 0 0 rg BT /F1 24 Tf 15 40 Td (A B) Tj ET");
        let no_sub_doc = PdfDocument::open(ab_pdf.clone())
            .unwrap()
            .with_font_provider(Arc::new(FontProvider::memory_only()));
        let no_sub_page = zpdf::interpret_page(&no_sub_doc, 0, Default::default()).unwrap();
        assert_eq!(
            no_sub_page.stats.diagnostics.embedded_font_fallbacks, 1,
            "{label}: damaged zero-contour 'A' must report fallback with no provider"
        );
        assert_eq!(
            no_sub_page.stats.diagnostics.missing_glyph_outlines, 1,
            "{label}: unrecovered damaged zero-contour 'A' must report missing outline"
        );
        assert!(
            !no_sub_page.is_complete(),
            "{label}: must reject silent drop"
        );
        // Text-only extraction still succeeds because WinAnsi decodes "A B".
        let conv_text = zpdf::convert_pdf(&no_sub_doc, &[0], Default::default()).unwrap();
        assert_eq!(conv_text.pages[0].text.trim(), "A B");
        assert_eq!(conv_text.pages[0].diagnostics.embedded_font_fallbacks, 1);
        assert_eq!(conv_text.pages[0].diagnostics.decode_failures, 0);
        assert!(conv_text.is_complete());

        // 3. Showing "A B" WITH descriptor substitute available: only 'A' moves
        //    to the bold descriptor substitute while genuine space ' ' and 'B'
        //    stay on the embedded face, preserving all PDF advances and ink.
        let sub_doc = PdfDocument::open(ab_pdf)
            .unwrap()
            .with_font_provider(Arc::clone(&provider));
        let sub_page = zpdf::interpret_page(&sub_doc, 0, Default::default()).unwrap();
        let (fid_ab, _) = sub_page.fonts.get_by_name("F1").unwrap();
        assert!(sub_page.fonts.has_admitted_embedded_companion(fid_ab));
        assert_eq!(sub_page.stats.diagnostics.embedded_font_fallbacks, 1);
        assert_eq!(sub_page.stats.diagnostics.missing_glyph_outlines, 0);
        assert!(sub_page.is_complete(), "{label}: recovered A B is complete");

        let runs: Vec<_> = sub_page
            .display_list
            .commands
            .iter()
            .filter_map(|c| match c {
                RenderCommand::DrawGlyphRun(r) => Some(r),
                _ => None,
            })
            .collect();
        assert_eq!(runs.len(), 2, "{label}: A on substitute, ' B' on embedded");
        assert_ne!(runs[0].font_id, fid_ab);
        assert_eq!(runs[0].glyphs.len(), 1);
        assert_eq!(
            sub_page
                .fonts
                .get(runs[0].font_id)
                .unwrap()
                .font_data
                .as_deref()
                .unwrap(),
            BOLD_TTF,
            "{label}: descriptor /FontWeight 700 must select bold substitute"
        );
        assert_eq!(runs[1].font_id, fid_ab);
        assert_eq!(runs[1].glyphs.len(), 2);

        let a_adv = 620.0_f32 * 24.0 / 1000.0;
        let sp_adv = 300.0_f32 * 24.0 / 1000.0;
        let b_adv = 780.0_f32 * 24.0 / 1000.0;
        assert!((runs[0].glyphs[0].advance - a_adv).abs() < 0.01);
        assert!((runs[1].glyphs[0].x - a_adv).abs() < 0.01);
        assert!((runs[1].glyphs[0].advance - sp_adv).abs() < 0.01);
        assert!((runs[1].glyphs[1].x - (a_adv + sp_adv)).abs() < 0.01);
        assert!((runs[1].glyphs[1].advance - b_adv).abs() < 0.01);

        let cpu = zpdf::cpu::CpuRenderer::new()
            .with_fonts(&sub_page.fonts)
            .render_display_list(&sub_page.display_list, 1.0)
            .unwrap();
        assert!(
            dark_pixels_in_rect(
                &cpu.data,
                cpu.width as usize,
                cpu.height as usize,
                15,
                (15.0 + a_adv).ceil() as usize,
                30,
                70,
            ) > 10,
            "{label}: substituted 'A' must draw ink"
        );
        assert!(
            dark_pixels_in_rect(
                &cpu.data,
                cpu.width as usize,
                cpu.height as usize,
                (15.0 + a_adv + sp_adv).floor() as usize,
                (15.0 + a_adv + sp_adv + b_adv).ceil() as usize,
                30,
                70,
            ) > 10,
            "{label}: embedded 'B' must draw ink"
        );
    }
}

#[test]
fn hidden_text_in_disabled_ocg_or_none_colorant_does_not_consume_companion_budget() {
    let probe = zpdf_font::LoadedFont::new_with_data(
        zpdf_font::PdfFontType::TrueType,
        "Probe".into(),
        PRIMARY_TTF.to_vec(),
        zpdf_font::CidWidths::new(1000.0),
    );
    let gid_a = probe.unicode_glyph('A' as u32).unwrap().0;
    let partial_ttf = corrupt_selected_gids(PRIMARY_TTF, &[gid_a]);

    let provider = Arc::new(FontProvider::memory_only());
    provider
        .register_with_aliases(PRIMARY_TTF, 0, &["HiddenBudgetOne"])
        .unwrap();
    provider
        .register_with_aliases(BOLD_TTF, 0, &["VisibleBudgetTwo"])
        .unwrap();

    let make_pdf = |content: &[u8]| {
        assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R /OCProperties << /OCGs [12 0 R] /D << /OFF [12 0 R] >> >> >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] \
              /Resources << /ColorSpace << /CS0 [/Separation /None /DeviceGray 11 0 R] >> \
              /Properties << /Hidden 12 0 R >> \
              /Font << /F1 5 0 R /F2 8 0 R >> >> /Contents 4 0 R >>"
                .to_vec(),
            stream("", content),
            b"<< /Type /Font /Subtype /TrueType /BaseFont /ABCDEF+HiddenBudgetOne \
              /Encoding /WinAnsiEncoding /FirstChar 65 /LastChar 67 /Widths [620 780 540] \
              /FontDescriptor 6 0 R >>"
                .to_vec(),
            b"<< /Type /FontDescriptor /FontName /ABCDEF+HiddenBudgetOne /Flags 32 /FontFile2 7 0 R >>"
                .to_vec(),
            stream("", &partial_ttf),
            b"<< /Type /Font /Subtype /TrueType /BaseFont /GHIJKL+VisibleBudgetTwo \
              /Encoding /WinAnsiEncoding /FirstChar 65 /LastChar 67 /Widths [620 780 540] \
              /FontDescriptor 9 0 R >>"
                .to_vec(),
            b"<< /Type /FontDescriptor /FontName /GHIJKL+VisibleBudgetTwo /Flags 32 /FontFile2 10 0 R >>"
                .to_vec(),
            stream("", &partial_ttf),
            b"<< /FunctionType 2 /Domain [0 1] /C0 [1] /C1 [0] /N 1 >>".to_vec(),
            b"<< /Type /OCG /Name (HiddenLayer) >>".to_vec(),
        ])
    };

    // Measure exact cache budget that fits both primary faces + ONE companion
    // (BOLD_TTF) but NOT both distinct companions (PRIMARY_TTF + BOLD_TTF).
    let probe_doc = PdfDocument::open(make_pdf(
        b"0 0 0 rg BT /F1 24 Tf 15 40 Td (A) Tj /F2 24 Tf (A) Tj ET",
    ))
    .unwrap()
    .with_font_provider(Arc::clone(&provider));
    let probe_page = zpdf::interpret_page(&probe_doc, 0, Default::default()).unwrap();
    let (_, f1) = probe_page.fonts.get_by_name("F1").unwrap();
    let (_, f2) = probe_page.fonts.get_by_name("F2").unwrap();
    let primaries_bytes = f1.estimated_cache_bytes() + f2.estimated_cache_bytes();
    let one_companion_budget = primaries_bytes + BOLD_TTF.len() as u64 + 8192;
    assert!(
        one_companion_budget < primaries_bytes + PRIMARY_TTF.len() as u64 + BOLD_TTF.len() as u64,
        "budget must have room for only one companion face"
    );

    let limits = zpdf::ParseLimits {
        max_font_cache_bytes: one_companion_budget,
        ..Default::default()
    };

    // Case A: disabled OCG show on /F1 followed by visible show on /F2.
    let ocg_pdf =
        make_pdf(b"0 0 0 rg BT /OC /Hidden BDC /F1 24 Tf 15 40 Td (A) Tj EMC /F2 24 Tf (A) Tj ET");
    let ocg_doc = PdfDocument::open_with_limits(ocg_pdf, limits.clone())
        .unwrap()
        .with_font_provider(Arc::clone(&provider));
    let ocg_page = zpdf::interpret_page(&ocg_doc, 0, Default::default()).unwrap();
    let (fid1, _) = ocg_page.fonts.get_by_name("F1").unwrap();
    let (fid2, _) = ocg_page.fonts.get_by_name("F2").unwrap();
    assert!(
        !ocg_page.fonts.has_admitted_embedded_companion(fid1),
        "disabled OCG show must not admit /F1's companion"
    );
    assert!(
        ocg_page.fonts.has_admitted_embedded_companion(fid2),
        "visible /F2 must still fit its companion"
    );
    assert_eq!(ocg_page.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(ocg_page.stats.diagnostics.missing_glyph_outlines, 0);
    assert!(ocg_page.is_complete());
    let a_adv = 620.0_f32 * 24.0 / 1000.0;
    let ocg_cpu = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&ocg_page.fonts)
        .render_display_list(&ocg_page.display_list, 1.0)
        .unwrap();
    assert_eq!(
        dark_pixels_in_rect(
            &ocg_cpu.data,
            ocg_cpu.width as usize,
            ocg_cpu.height as usize,
            15,
            (15.0 + a_adv).floor() as usize,
            30,
            70,
        ),
        0,
        "disabled OCG 'A' must not paint"
    );
    assert!(
        dark_pixels_in_rect(
            &ocg_cpu.data,
            ocg_cpu.width as usize,
            ocg_cpu.height as usize,
            (15.0 + a_adv).floor() as usize,
            (15.0 + 2.0 * a_adv).ceil() as usize,
            30,
            70,
        ) > 10,
        "visible /F2 'A' must paint after disabled OCG text advancement"
    );

    // Case B: fill-only /Separation /None show on /F1 followed by visible /F2.
    let none_pdf =
        make_pdf(b"BT /CS0 cs 1 scn /F1 24 Tf 15 40 Td (A) Tj 0 0 0 rg /F2 24 Tf (A) Tj ET");
    let none_doc = PdfDocument::open_with_limits(none_pdf, limits)
        .unwrap()
        .with_font_provider(provider);
    let none_page = zpdf::interpret_page(&none_doc, 0, Default::default()).unwrap();
    let (nfid1, _) = none_page.fonts.get_by_name("F1").unwrap();
    let (nfid2, _) = none_page.fonts.get_by_name("F2").unwrap();
    assert!(
        !none_page.fonts.has_admitted_embedded_companion(nfid1),
        "/None colorant show must not admit /F1's companion"
    );
    assert!(
        none_page.fonts.has_admitted_embedded_companion(nfid2),
        "visible /F2 must still fit its companion"
    );
    assert_eq!(none_page.stats.diagnostics.missing_glyph_outlines, 0);
    assert!(none_page.is_complete());
    let none_cpu = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&none_page.fonts)
        .render_display_list(&none_page.display_list, 1.0)
        .unwrap();
    assert_eq!(
        dark_pixels_in_rect(
            &none_cpu.data,
            none_cpu.width as usize,
            none_cpu.height as usize,
            15,
            (15.0 + a_adv).floor() as usize,
            30,
            70,
        ),
        0,
        "/None colorant 'A' must not paint"
    );
    assert!(
        dark_pixels_in_rect(
            &none_cpu.data,
            none_cpu.width as usize,
            none_cpu.height as usize,
            (15.0 + a_adv).floor() as usize,
            (15.0 + 2.0 * a_adv).ceil() as usize,
            30,
            70,
        ) > 10,
        "visible /F2 'A' must paint at the advanced x position"
    );
    // Text extraction for /None fill-only text must still be preserved.
    let conv_none = zpdf::convert_pdf(&none_doc, &[0], Default::default()).unwrap();
    assert_eq!(conv_none.pages[0].text.trim(), "AA");
    assert!(conv_none.is_complete());
}

#[test]
fn deferred_descriptor_substitutes_deduplicate_shared_program_bytes_against_budget() {
    let probe = zpdf_font::LoadedFont::new_with_data(
        zpdf_font::PdfFontType::TrueType,
        "Probe".into(),
        PRIMARY_TTF.to_vec(),
        zpdf_font::CidWidths::new(1000.0),
    );
    let gid_a = probe.unicode_glyph('A' as u32).unwrap().0;
    let partial_ttf = corrupt_selected_gids(PRIMARY_TTF, &[gid_a]);

    // Both /F1 and /F2 resolve their deferred descriptor substitute to the SAME
    // registered `PRIMARY_TTF` (`Arc<[u8]>`, ~18 KB).
    let provider = Arc::new(FontProvider::memory_only());
    provider
        .register_with_aliases(PRIMARY_TTF, 0, &["SharedSubOne", "SharedSubTwo"])
        .unwrap();

    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] \
          /Resources << /Font << /F1 5 0 R /F2 8 0 R >> >> /Contents 4 0 R >>"
            .to_vec(),
        stream(
            "",
            b"0 0 0 rg BT /F1 24 Tf 15 40 Td (A) Tj /F2 24 Tf 40 0 Td (A) Tj ET",
        ),
        b"<< /Type /Font /Subtype /TrueType /BaseFont /ABCDEF+SharedSubOne \
          /Encoding /WinAnsiEncoding /FirstChar 65 /LastChar 67 /Widths [620 780 540] \
          /FontDescriptor 6 0 R >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+SharedSubOne /Flags 32 /FontFile2 7 0 R >>"
            .to_vec(),
        stream("", &partial_ttf),
        b"<< /Type /Font /Subtype /TrueType /BaseFont /GHIJKL+SharedSubTwo \
          /Encoding /WinAnsiEncoding /FirstChar 65 /LastChar 67 /Widths [620 780 540] \
          /FontDescriptor 9 0 R >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /GHIJKL+SharedSubTwo /Flags 32 /FontFile2 10 0 R >>"
            .to_vec(),
        stream("", &partial_ttf),
    ]);

    let probe_doc = PdfDocument::open(pdf.clone())
        .unwrap()
        .with_font_provider(Arc::clone(&provider));
    let probe_page = zpdf::interpret_page(&probe_doc, 0, Default::default()).unwrap();
    let (_, f1) = probe_page.fonts.get_by_name("F1").unwrap();
    let (_, f2) = probe_page.fonts.get_by_name("F2").unwrap();
    let probe_runs: Vec<_> = probe_page
        .display_list
        .commands
        .iter()
        .filter_map(|cmd| match cmd {
            RenderCommand::DrawGlyphRun(run) => Some(run),
            _ => None,
        })
        .collect();
    assert_eq!(probe_runs.len(), 2);
    let sub1_bytes = probe_page
        .fonts
        .get(probe_runs[0].font_id)
        .unwrap()
        .estimated_cache_bytes();
    let sub2_bytes = probe_page
        .fonts
        .get(probe_runs[1].font_id)
        .unwrap()
        .estimated_cache_bytes();
    let primaries_bytes = f1.estimated_cache_bytes() + f2.estimated_cache_bytes();
    let shared_program_bytes = PRIMARY_TTF.len() as u64;

    // Exact deduplicated budget: both primaries + sub1 (including PRIMARY_TTF)
    // + sub2's owned metadata only (excluding the shared PRIMARY_TTF Arc<[u8]>).
    // Strictly below paying sub2.estimated_cache_bytes() again!
    let tight_budget = primaries_bytes + sub1_bytes + (sub2_bytes - shared_program_bytes);
    assert!(
        tight_budget < primaries_bytes + sub1_bytes + sub2_bytes - 4096,
        "budget must rely on shared-program deduplication for the second font"
    );

    let doc = PdfDocument::open_with_limits(
        pdf,
        zpdf::ParseLimits {
            max_font_cache_bytes: tight_budget,
            ..Default::default()
        },
    )
    .unwrap()
    .with_font_provider(provider);
    let page = zpdf::interpret_page(&doc, 0, Default::default()).unwrap();
    let (fid1, _) = page.fonts.get_by_name("F1").unwrap();
    let (fid2, _) = page.fonts.get_by_name("F2").unwrap();
    assert!(page.fonts.has_admitted_embedded_companion(fid1));
    assert!(page.fonts.has_admitted_embedded_companion(fid2));
    assert_eq!(page.stats.diagnostics.embedded_font_fallbacks, 2);
    assert_eq!(page.stats.diagnostics.missing_glyph_outlines, 0);
    assert!(page.is_complete());

    let cpu = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&page.fonts)
        .render_display_list(&page.display_list, 1.0)
        .unwrap();
    assert!(
        dark_pixels_in_rect(
            &cpu.data,
            cpu.width as usize,
            cpu.height as usize,
            15,
            35,
            30,
            70
        ) > 10
    );
    assert!(
        dark_pixels_in_rect(
            &cpu.data,
            cpu.width as usize,
            cpu.height as usize,
            55,
            75,
            30,
            70
        ) > 10
    );
}

#[test]
fn cjk_partially_corrupt_subset_with_duplicate_bad_cids_keeps_embedded_primary() {
    let probe = zpdf_font::LoadedFont::new_with_data(
        zpdf_font::PdfFontType::TrueType,
        "Probe".into(),
        PRIMARY_TTF.to_vec(),
        zpdf_font::CidWidths::new(1000.0),
    );
    let gid_a = probe.unicode_glyph('A' as u32).unwrap().0;
    let gid_b = probe.unicode_glyph('B' as u32).unwrap().0;
    // Corrupt only `gid_a`. Because the explicit `/CIDToGIDMap` maps only two
    // distinct nonzero GIDs (`gid_b` drawable and `gid_a` unreadable), the
    // load-time probe inspects those two GIDs and classifies the font as
    // `PartiallyCorrupt` (1 of 2), even though 3 of the 5 CIDs point to `gid_a`
    // (`embedded_outline_failure_rate() == 3/5 == 0.6 > 0.5`).
    let partial_ttf = corrupt_selected_gids(PRIMARY_TTF, &[gid_a]);

    // Map 5 nonzero CIDs: 2 point to healthy `gid_b`, and 3 duplicates point to
    // corrupt `gid_a`.
    let mut cid_to_gid = Vec::new();
    for gid in [0u16, gid_b, gid_b, gid_a, gid_a, gid_a] {
        cid_to_gid.extend_from_slice(&gid.to_be_bytes());
    }
    let to_unicode = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
        1 begincodespacerange <0000> <FFFF> endcodespacerange \
        5 beginbfchar <0001> <0042> <0002> <0042> <0003> <0041> <0004> <0041> <0005> <0041> endbfchar \
        endcmap CMapName currentdict /CMap defineresource pop end end";

    let provider = Arc::new(FontProvider::memory_only());
    provider
        .register_with_aliases(BOLD_TTF, 0, &["PartialCjkFace"])
        .unwrap();

    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] \
          /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
            .to_vec(),
        stream("", b"0 0 0 rg BT /F1 24 Tf 15 40 Td <00010003> Tj ET"),
        b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+PartialCjkFace /Encoding /Identity-H \
          /DescendantFonts [6 0 R] /ToUnicode 9 0 R >>"
            .to_vec(),
        b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+PartialCjkFace \
          /CIDSystemInfo << /Registry (Adobe) /Ordering (GB1) /Supplement 0 >> \
          /CIDToGIDMap 10 0 R /FontDescriptor 7 0 R /DW 700 >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+PartialCjkFace /Flags 32 /FontFile2 8 0 R >>"
            .to_vec(),
        stream("", &partial_ttf),
        stream("", to_unicode),
        stream("", &cid_to_gid),
    ]);

    let doc = PdfDocument::open(pdf).unwrap().with_font_provider(provider);
    let page = zpdf::interpret_page(&doc, 0, Default::default()).unwrap();
    let (fid, font) = page.fonts.get_by_name("F1").unwrap();
    assert!(
        !font.is_substitute,
        "PartiallyCorrupt CJK embedded font must stay primary despite duplicate bad CIDs"
    );
    let runs: Vec<_> = page
        .display_list
        .commands
        .iter()
        .filter_map(|cmd| match cmd {
            RenderCommand::DrawGlyphRun(run) => Some(run),
            _ => None,
        })
        .collect();
    assert_eq!(
        runs.len(),
        2,
        "healthy CID on embedded, corrupt CID on substitute"
    );
    assert_eq!(runs[0].font_id, fid);
    assert_ne!(runs[1].font_id, fid);
    assert_eq!(
        page.fonts
            .get(runs[1].font_id)
            .unwrap()
            .font_data
            .as_deref()
            .unwrap(),
        BOLD_TTF
    );
    assert_eq!(page.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(page.stats.diagnostics.missing_glyph_outlines, 0);
    assert!(page.is_complete());
}

#[test]
fn explicit_out_of_range_cid_to_gid_triggers_substitute_while_absent_cid_does_not() {
    let probe = zpdf_font::LoadedFont::new_with_data(
        zpdf_font::PdfFontType::TrueType,
        "Probe".into(),
        PRIMARY_TTF.to_vec(),
        zpdf_font::CidWidths::new(1000.0),
    );
    let gid_b = probe.unicode_glyph('B' as u32).unwrap().0;
    let gid_c = probe.unicode_glyph('C' as u32).unwrap().0;

    // CID 1 -> gid_b, CID 2 -> gid_c, CID 3 -> explicit out-of-range GID 999
    // (PRIMARY_TTF has 368 glyphs). CID 9 is absent from the CIDToGIDMap stream.
    let mut cid_to_gid = Vec::new();
    for gid in [0u16, gid_b, gid_c, 999u16] {
        cid_to_gid.extend_from_slice(&gid.to_be_bytes());
    }
    let to_unicode = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
        1 begincodespacerange <0000> <FFFF> endcodespacerange \
        4 beginbfchar <0001> <0042> <0002> <0043> <0003> <0041> <0009> <0041> endbfchar \
        endcmap CMapName currentdict /CMap defineresource pop end end";

    let provider = Arc::new(FontProvider::memory_only());
    provider
        .register_with_aliases(BOLD_TTF, 0, &["OutOfRangeGidFace"])
        .unwrap();

    let make_pdf = |content: &[u8]| {
        assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] \
              /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
                .to_vec(),
            stream("", content),
            b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+OutOfRangeGidFace /Encoding /Identity-H \
              /DescendantFonts [6 0 R] /ToUnicode 9 0 R >>"
                .to_vec(),
            b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+OutOfRangeGidFace \
              /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
              /CIDToGIDMap 10 0 R /FontDescriptor 7 0 R /DW 700 >>"
                .to_vec(),
            b"<< /Type /FontDescriptor /FontName /ABCDEF+OutOfRangeGidFace /Flags 32 /FontFile2 8 0 R >>"
                .to_vec(),
            stream("", PRIMARY_TTF),
            stream("", to_unicode),
            stream("", &cid_to_gid),
        ])
    };

    // 1. Explicit out-of-range nonzero GID (CID 3 -> 999) triggers substitute.
    let oor_doc = PdfDocument::open(make_pdf(b"0 0 0 rg BT /F1 24 Tf 15 40 Td <00010003> Tj ET"))
        .unwrap()
        .with_font_provider(Arc::clone(&provider));
    let oor_page = zpdf::interpret_page(&oor_doc, 0, Default::default()).unwrap();
    let (fid, font) = oor_page.fonts.get_by_name("F1").unwrap();
    assert!(!font.is_substitute);
    assert!(oor_page.fonts.has_admitted_embedded_companion(fid));
    assert_eq!(oor_page.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(oor_page.stats.diagnostics.missing_glyph_outlines, 0);
    assert!(oor_page.is_complete());
    let runs: Vec<_> = oor_page
        .display_list
        .commands
        .iter()
        .filter_map(|cmd| match cmd {
            RenderCommand::DrawGlyphRun(run) => Some(run),
            _ => None,
        })
        .collect();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].font_id, fid);
    assert_ne!(runs[1].font_id, fid);
    assert_eq!(
        oor_page
            .fonts
            .get(runs[1].font_id)
            .unwrap()
            .font_data
            .as_deref()
            .unwrap(),
        BOLD_TTF
    );

    // 2. Absent CID mapping (CID 9 not in CIDToGIDMap -> None/GID 0) stays distinct
    //    and does NOT trigger embedded-font fallback.
    let absent_doc =
        PdfDocument::open(make_pdf(b"0 0 0 rg BT /F1 24 Tf 15 40 Td <00010009> Tj ET"))
            .unwrap()
            .with_font_provider(provider);
    let absent_page = zpdf::interpret_page(&absent_doc, 0, Default::default()).unwrap();
    let (afid, _) = absent_page.fonts.get_by_name("F1").unwrap();
    assert!(!absent_page.fonts.has_admitted_embedded_companion(afid));
    assert_eq!(absent_page.stats.diagnostics.embedded_font_fallbacks, 0);
}

/// Build a minimal valid Type 1 PFA with drawable `/B` and `/C`, a genuine
/// width-only `/space`, and a damaged width-only (empty) visible `/A`.
fn build_empty_a_type1_pfa() -> Vec<u8> {
    let mut private = Vec::new();
    private.extend_from_slice(
        b"dup /Private 4 dict dup begin /lenIV -1 def /CharStrings 5 dict dup begin\n",
    );
    let entries: &[(&str, &[u8])] = &[
        (".notdef", &[139, 248, 136, 13, 14]),
        ("space", &[139, 247, 192, 13, 14]),
        ("A", &[139, 249, 0, 13, 14]),
        (
            "B",
            &[
                139, 249, 160, 13, 189, 139, 21, 139, 249, 80, 5, 248, 136, 139, 5, 139, 253, 80,
                5, 9, 14,
            ],
        ),
        (
            "C",
            &[
                139, 248, 176, 13, 189, 139, 21, 139, 249, 80, 5, 248, 36, 139, 5, 139, 253, 80, 5,
                9, 14,
            ],
        ),
    ];
    for (name, cs) in entries {
        private.extend_from_slice(format!("/{name} {} RD ", cs.len()).as_bytes());
        private.extend_from_slice(cs);
        private.extend_from_slice(b" ND\n");
    }
    private.extend_from_slice(b"end end readonly def\n");

    let mut r: u16 = 55665;
    let mut hex = String::with_capacity((private.len() + 4) * 2);
    for &p in [0u8, 0, 0, 0].iter().chain(private.iter()) {
        let c = p ^ (r >> 8) as u8;
        r = (c as u16)
            .wrapping_add(r)
            .wrapping_mul(52845)
            .wrapping_add(22719);
        use std::fmt::Write;
        let _ = write!(&mut hex, "{c:02X}");
    }

    let mut pfa = Vec::new();
    pfa.extend_from_slice(
        b"%!PS-AdobeFont-1.0: EmptyAType1 1.0\n\
          8 dict begin\n\
          /FontName /EmptyAType1 def /FontType 1 def /PaintType 0 def\n\
          /FontMatrix [0.001 0 0 0.001 0 0] readonly def\n\
          /FontBBox [0 0 1000 900] readonly def /Encoding StandardEncoding def\n\
          currentdict end currentfile eexec\n",
    );
    pfa.extend_from_slice(hex.as_bytes());
    pfa.push(b'\n');
    pfa
}

#[test]
fn empty_visible_type1_charstring_recovers_via_substitute_while_space_stays_embedded() {
    let pfa = build_empty_a_type1_pfa();
    let provider = Arc::new(FontProvider::memory_only());
    provider
        .register_with_aliases(BOLD_TTF, 0, &["EmptyAType1"])
        .unwrap();

    let mut widths = [500u16; 36];
    widths[0] = 300;
    widths[65 - 32] = 620;
    widths[66 - 32] = 780;
    widths[67 - 32] = 540;
    let widths_str: String = widths
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(" ");

    let make_pdf = |content: &[u8]| {
        assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] \
              /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
                .to_vec(),
            stream("", content),
            format!(
                "<< /Type /Font /Subtype /Type1 /BaseFont /ABCDEF+EmptyAType1 \
                 /Encoding /WinAnsiEncoding /FirstChar 32 /LastChar 67 /Widths [{widths_str}] \
                 /FontDescriptor 6 0 R >>"
            )
            .into_bytes(),
            b"<< /Type /FontDescriptor /FontName /ABCDEF+EmptyAType1 /Flags 32 /FontFile 7 0 R >>"
                .to_vec(),
            stream("", &pfa),
        ])
    };

    // 1. Healthy letters + genuine space ("B C"): embedded Type 1 stays primary
    //    with zero fallbacks.
    let bc_doc = PdfDocument::open(make_pdf(b"0 0 0 rg BT /F1 24 Tf 15 40 Td (B C) Tj ET"))
        .unwrap()
        .with_font_provider(Arc::new(FontProvider::memory_only()));
    let bc_page = zpdf::interpret_page(&bc_doc, 0, Default::default()).unwrap();
    let (fid_bc, f_bc) = bc_page.fonts.get_by_name("F1").unwrap();
    assert!(!f_bc.is_substitute);
    assert!(!bc_page.fonts.has_admitted_embedded_companion(fid_bc));
    assert_eq!(bc_page.stats.diagnostics.embedded_font_fallbacks, 0);
    assert_eq!(bc_page.stats.diagnostics.missing_glyph_outlines, 0);
    assert!(bc_page.is_complete());

    // 2. Empty visible /A + genuine space + healthy /B ("A B") with substitute:
    //    only 'A' moves to BOLD_TTF while ' ' and 'B' remain on embedded Type 1.
    let ab_pdf = make_pdf(b"0 0 0 rg BT /F1 24 Tf 15 40 Td (A B) Tj ET");
    let sub_doc = PdfDocument::open(ab_pdf.clone())
        .unwrap()
        .with_font_provider(provider);
    let sub_page = zpdf::interpret_page(&sub_doc, 0, Default::default()).unwrap();
    let (fid_ab, f_ab) = sub_page.fonts.get_by_name("F1").unwrap();
    assert!(!f_ab.is_substitute);
    assert!(sub_page.fonts.has_admitted_embedded_companion(fid_ab));
    assert_eq!(sub_page.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(sub_page.stats.diagnostics.missing_glyph_outlines, 0);
    assert!(sub_page.is_complete());
    let runs: Vec<_> = sub_page
        .display_list
        .commands
        .iter()
        .filter_map(|cmd| match cmd {
            RenderCommand::DrawGlyphRun(run) => Some(run),
            _ => None,
        })
        .collect();
    assert_eq!(
        runs.len(),
        2,
        "A on BOLD_TTF substitute, ' B' on embedded Type 1"
    );
    assert_ne!(runs[0].font_id, fid_ab);
    assert_eq!(runs[1].font_id, fid_ab);
    assert_eq!(runs[1].glyphs.len(), 2);

    // 3. Without any substitute, empty visible /A reports missing outline.
    let no_sub_doc = PdfDocument::open(ab_pdf)
        .unwrap()
        .with_font_provider(Arc::new(FontProvider::memory_only()));
    let no_sub_page = zpdf::interpret_page(&no_sub_doc, 0, Default::default()).unwrap();
    assert_eq!(no_sub_page.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(no_sub_page.stats.diagnostics.missing_glyph_outlines, 1);
    assert!(!no_sub_page.is_complete());
}

#[test]
fn render_mode_3_unreadable_unmapped_embedded_glyph_flags_text_decode_failure_unless_actual_text() {
    let probe = zpdf_font::LoadedFont::new_with_data(
        zpdf_font::PdfFontType::TrueType,
        "Probe".into(),
        PRIMARY_TTF.to_vec(),
        zpdf_font::CidWidths::new(1000.0),
    );
    let gid_a = probe.unicode_glyph('A' as u32).unwrap().0;
    assert_eq!(gid_a, 0x0022);
    // Only unsampled gid_a is corrupted -> embedded Identity-H font stays primary.
    let partial_ttf = corrupt_selected_gids(PRIMARY_TTF, &[gid_a]);

    let make_pdf = |content: &[u8]| {
        assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] \
              /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
                .to_vec(),
            stream("", content),
            b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+Mode3UnmappedCID /Encoding /Identity-H \
              /DescendantFonts [6 0 R] >>"
                .to_vec(),
            b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+Mode3UnmappedCID \
              /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
              /CIDToGIDMap /Identity /FontDescriptor 7 0 R /DW 700 >>"
                .to_vec(),
            b"<< /Type /FontDescriptor /FontName /ABCDEF+Mode3UnmappedCID /Flags 32 /FontFile2 8 0 R >>"
                .to_vec(),
            stream("", &partial_ttf),
        ])
    };

    // 1. Render-mode-3 invisible text showing unreadable unmapped <0022>:
    //    visual rendering stays complete (no visual outline omission), while
    //    text extraction flags embedded_font_fallbacks=1 and decode_failures=1.
    let doc = PdfDocument::open(make_pdf(b"BT /F1 24 Tf 3 Tr 15 40 Td <0022> Tj ET"))
        .unwrap()
        .with_font_provider(Arc::new(FontProvider::memory_only()));
    let page = zpdf::interpret_page(&doc, 0, Default::default()).unwrap();
    assert_eq!(page.stats.diagnostics.missing_glyph_outlines, 0);
    assert!(page.is_complete());

    let conv = zpdf::convert_pdf(&doc, &[0], Default::default()).unwrap();
    assert_eq!(conv.pages[0].diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(conv.pages[0].diagnostics.decode_failures, 1);
    assert_eq!(conv.pages[0].diagnostics.missing_glyph_outlines, 0);
    assert!(!conv.is_complete());
    assert!(conv.ensure_complete().is_err());

    // 2. Wrapping the same invisible show in /ActualText exempts text decoding.
    let doc_actual = PdfDocument::open(make_pdf(
        b"/Span << /ActualText (OK) >> BDC BT /F1 24 Tf 3 Tr 15 40 Td <0022> Tj ET EMC",
    ))
    .unwrap()
    .with_font_provider(Arc::new(FontProvider::memory_only()));
    let conv_actual = zpdf::convert_pdf(&doc_actual, &[0], Default::default()).unwrap();
    assert_eq!(conv_actual.pages[0].text.trim(), "OK");
    assert_eq!(conv_actual.pages[0].diagnostics.decode_failures, 0);
    assert_eq!(conv_actual.pages[0].diagnostics.missing_glyph_outlines, 0);
    assert!(conv_actual.is_complete());
}

/// Build a symbolic Type 1 PFA whose built-in `/Encoding` remaps byte 65 ('A')
/// to `/B` (damaged empty visible charstring) and byte 66 ('B') to `/C`. When
/// `all_empty` is false, `/C` and `/D` are drawable so the embedded Type 1 font
/// stays primary (`PartiallyCorrupt`); when `all_empty` is true, every visible
/// charstring is empty so the font falls back at load time (`Unusable`).
fn build_remapped_symbolic_type1_pfa(all_empty: bool) -> Vec<u8> {
    let rect_c: &[u8] = if all_empty {
        &[139, 248, 176, 13, 14]
    } else {
        &[
            139, 248, 176, 13, 189, 139, 21, 139, 249, 80, 5, 248, 36, 139, 5, 139, 253, 80, 5, 9,
            14,
        ]
    };
    let rect_d: &[u8] = if all_empty {
        &[139, 248, 176, 13, 14]
    } else {
        &[
            139, 248, 176, 13, 189, 139, 21, 139, 249, 80, 5, 248, 136, 139, 5, 139, 253, 80, 5, 9,
            14,
        ]
    };
    let mut private = Vec::new();
    private.extend_from_slice(
        b"dup /Private 4 dict dup begin /lenIV -1 def /CharStrings 4 dict dup begin\n",
    );
    let entries: &[(&str, &[u8])] = &[
        (".notdef", &[139, 248, 136, 13, 14]),
        ("B", &[139, 249, 160, 13, 14]), // damaged empty visible /B at byte 65
        ("C", rect_c),
        ("D", rect_d),
    ];
    for (name, cs) in entries {
        private.extend_from_slice(format!("/{name} {} RD ", cs.len()).as_bytes());
        private.extend_from_slice(cs);
        private.extend_from_slice(b" ND\n");
    }
    private.extend_from_slice(b"end end readonly def\n");

    let mut r: u16 = 55665;
    let mut hex = String::with_capacity((private.len() + 4) * 2);
    for &p in [0u8, 0, 0, 0].iter().chain(private.iter()) {
        let c = p ^ (r >> 8) as u8;
        r = (c as u16)
            .wrapping_add(r)
            .wrapping_mul(52845)
            .wrapping_add(22719);
        use std::fmt::Write;
        let _ = write!(&mut hex, "{c:02X}");
    }

    let mut pfa = Vec::new();
    pfa.extend_from_slice(
        b"%!PS-AdobeFont-1.0: RemappedSymT1 1.0\n\
          8 dict begin\n\
          /FontName /RemappedSymT1 def /FontType 1 def /PaintType 0 def\n\
          /FontMatrix [0.001 0 0 0.001 0 0] readonly def\n\
          /FontBBox [0 0 1000 900] readonly def\n\
          /Encoding 256 array dup 65 /B put dup 66 /C put dup 67 /D put readonly def\n\
          currentdict end currentfile eexec\n",
    );
    pfa.extend_from_slice(hex.as_bytes());
    pfa.push(b'\n');
    pfa
}

#[test]
fn symbolic_type1_remapped_builtin_encoding_recovers_mapped_glyph_not_raw_byte() {
    let probe = zpdf_font::LoadedFont::new_with_data(
        zpdf_font::PdfFontType::TrueType,
        "Probe".into(),
        PRIMARY_TTF.to_vec(),
        zpdf_font::CidWidths::new(1000.0),
    );
    let gid_a = probe.unicode_glyph('A' as u32).unwrap().0;
    let gid_b = probe.unicode_glyph('B' as u32).unwrap().0;
    assert_ne!(gid_a, gid_b);

    let make_pdf = |pfa: &[u8], content: &[u8]| {
        assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] \
              /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
                .to_vec(),
            stream("", content),
            // Symbolic Type 1 with NO PDF /Encoding and NO /ToUnicode:
            // byte 65 ('A') maps to /B and byte 66 ('B') maps to /C via the
            // embedded Type 1 program's built-in /Encoding.
            b"<< /Type /Font /Subtype /Type1 /BaseFont /ABCDEF+RemappedSymT1 \
              /FirstChar 65 /LastChar 67 /Widths [780 540 540] \
              /FontDescriptor 6 0 R >>"
                .to_vec(),
            b"<< /Type /FontDescriptor /FontName /ABCDEF+RemappedSymT1 /Flags 4 /FontFile 7 0 R >>"
                .to_vec(),
            stream("", pfa),
        ])
    };

    let primary_provider = Arc::new(FontProvider::memory_only());
    primary_provider
        .register_with_aliases(PRIMARY_TTF, 0, &["RemappedSymT1"])
        .unwrap();
    // BOLD_TTF contains 'A' ONLY — it does NOT contain 'B'.
    let bold_only_a_provider = Arc::new(FontProvider::memory_only());
    bold_only_a_provider
        .register_with_aliases(BOLD_TTF, 0, &["RemappedSymT1"])
        .unwrap();

    // 1. Partially corrupt symbolic Type 1 (embedded stays primary; byte 65 =
    //    damaged /B, byte 66 = healthy /C) with PRIMARY_TTF (has both 'A' and 'B'):
    //    byte 65 must recover to PRIMARY_TTF's GID for 'B', NOT 'A'.
    let partial_pfa = build_remapped_symbolic_type1_pfa(false);
    let partial_pdf = make_pdf(&partial_pfa, b"0 0 0 rg BT /F1 24 Tf 15 40 Td (AB) Tj ET");
    let doc_ok = PdfDocument::open(partial_pdf.clone())
        .unwrap()
        .with_font_provider(Arc::clone(&primary_provider));
    let page_ok = zpdf::interpret_page(&doc_ok, 0, Default::default()).unwrap();
    let (fid_ok, f_ok) = page_ok.fonts.get_by_name("F1").unwrap();
    assert!(!f_ok.is_substitute, "embedded Type 1 stays primary");
    assert!(f_ok.encoding.is_none() && f_ok.symbolic);
    assert!(page_ok.fonts.has_admitted_embedded_companion(fid_ok));
    assert_eq!(page_ok.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(page_ok.stats.diagnostics.missing_glyph_outlines, 0);
    assert!(page_ok.is_complete());
    let runs_ok: Vec<_> = page_ok
        .display_list
        .commands
        .iter()
        .filter_map(|cmd| match cmd {
            RenderCommand::DrawGlyphRun(run) => Some(run),
            _ => None,
        })
        .collect();
    assert_eq!(
        runs_ok.len(),
        2,
        "byte 65 (/B) on substitute, byte 66 (/C) on embedded"
    );
    assert_ne!(runs_ok[0].font_id, fid_ok);
    assert_eq!(runs_ok[0].glyphs.len(), 1);
    assert_eq!(
        runs_ok[0].glyphs[0].glyph_id, gid_b,
        "remapped byte 65 -> /B must select 'B' GID ({gid_b}), not raw-byte 'A' GID ({gid_a})"
    );
    assert_eq!(runs_ok[1].font_id, fid_ok);

    // Text extraction & completeness on the same partially corrupt symbolic
    // Type 1 must decode bytes 65, 66 through its built-in encoding to "BC"
    // with zero decode failures.
    let conv_ok = zpdf::convert_pdf(&doc_ok, &[0], Default::default()).unwrap();
    assert_eq!(conv_ok.pages[0].text.trim(), "BC");
    assert_eq!(conv_ok.pages[0].diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(conv_ok.pages[0].diagnostics.decode_failures, 0);
    assert!(conv_ok.is_complete());
    assert!(conv_ok.ensure_complete().is_ok());

    // 2. Same partially corrupt symbolic Type 1 with BOLD_TTF (which has 'A'
    //    but lacks 'B'): must NOT substitute raw byte 65 with 'A' or report
    //    successful recovery.
    let doc_miss = PdfDocument::open(partial_pdf)
        .unwrap()
        .with_font_provider(Arc::clone(&bold_only_a_provider));
    let page_miss = zpdf::interpret_page(&doc_miss, 0, Default::default()).unwrap();
    let (fid_miss, _) = page_miss.fonts.get_by_name("F1").unwrap();
    assert!(
        !page_miss.fonts.has_admitted_embedded_companion(fid_miss),
        "substitute lacking /B must not be admitted via raw byte 65 -> 'A'"
    );
    assert_eq!(page_miss.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(page_miss.stats.diagnostics.missing_glyph_outlines, 1);
    assert!(!page_miss.is_complete());
    let conv_miss = zpdf::convert_pdf(&doc_miss, &[0], Default::default()).unwrap();
    assert_eq!(conv_miss.pages[0].text.trim(), "BC");
    assert_eq!(conv_miss.pages[0].diagnostics.decode_failures, 0);
    assert!(conv_miss.is_complete());

    // 3. Load-time Unusable symbolic Type 1 (all charstrings empty, built-in
    //    encoding still intact): primary substitute likewise inherits `65 -> /B`,
    //    drawing 'B' from PRIMARY_TTF and rejecting missing 'B' in BOLD_TTF.
    let unusable_pfa = build_remapped_symbolic_type1_pfa(true);
    let unusable_pdf = make_pdf(&unusable_pfa, b"0 0 0 rg BT /F1 24 Tf 15 40 Td (A) Tj ET");
    let doc_unusable_ok = PdfDocument::open(unusable_pdf.clone())
        .unwrap()
        .with_font_provider(primary_provider);
    let page_unusable_ok = zpdf::interpret_page(&doc_unusable_ok, 0, Default::default()).unwrap();
    let (_, f_uok) = page_unusable_ok.fonts.get_by_name("F1").unwrap();
    assert!(f_uok.is_substitute);
    assert_eq!(
        page_unusable_ok.stats.diagnostics.embedded_font_fallbacks,
        1
    );
    assert_eq!(page_unusable_ok.stats.diagnostics.missing_glyph_outlines, 0);
    assert!(page_unusable_ok.is_complete());
    let uok_runs: Vec<_> = page_unusable_ok
        .display_list
        .commands
        .iter()
        .filter_map(|cmd| match cmd {
            RenderCommand::DrawGlyphRun(run) => Some(run),
            _ => None,
        })
        .collect();
    assert_eq!(uok_runs.len(), 1);
    assert_eq!(uok_runs[0].glyphs[0].glyph_id, gid_b);
    let conv_unusable_ok = zpdf::convert_pdf(&doc_unusable_ok, &[0], Default::default()).unwrap();
    assert_eq!(conv_unusable_ok.pages[0].text.trim(), "B");
    assert_eq!(conv_unusable_ok.pages[0].diagnostics.decode_failures, 0);
    assert!(conv_unusable_ok.is_complete());

    let doc_unusable_miss = PdfDocument::open(unusable_pdf)
        .unwrap()
        .with_font_provider(bold_only_a_provider);
    let page_unusable_miss =
        zpdf::interpret_page(&doc_unusable_miss, 0, Default::default()).unwrap();
    assert_eq!(
        page_unusable_miss.stats.diagnostics.embedded_font_fallbacks,
        1
    );
    assert_eq!(
        page_unusable_miss.stats.diagnostics.missing_glyph_outlines,
        1
    );
    assert!(!page_unusable_miss.is_complete());
}

#[test]
fn descriptor_metadata_ignores_unrecognized_ordering_and_accounts_shared_lookup_name_across_aliases(
) {
    // Build a healthy embedded Type 0 font with:
    // - an arbitrary 100 KB `/CIDSystemInfo /Ordering` string (must NOT be retained),
    // - an omitted `/BaseFont` and a 16 KB `/FontDescriptor /FontName` override,
    // - 32 resource aliases (`/F0`..`/F31`) on one page referencing the same font object.
    let huge_ordering = "X".repeat(100_000);
    let large_lookup_name = "N".repeat(16_384);
    let mut font_res = String::from("<< ");
    for i in 0..32 {
        use std::fmt::Write;
        let _ = write!(&mut font_res, "/F{i} 5 0 R ");
    }
    font_res.push_str(">>");

    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] \
              /Resources << /Font {font_res} >> /Contents 4 0 R >>"
        )
        .into_bytes(),
        stream("", b"0 0 0 rg BT /F0 12 Tf 10 50 Td <0022> Tj ET"),
        b"<< /Type /Font /Subtype /Type0 /Encoding /Identity-H /DescendantFonts [6 0 R] >>"
            .to_vec(),
        format!(
            "<< /Type /Font /Subtype /CIDFontType2 \
              /CIDSystemInfo << /Registry (Adobe) /Ordering ({huge_ordering}) /Supplement 0 >> \
              /CIDToGIDMap /Identity /FontDescriptor 7 0 R /DW 1000 >>"
        )
        .into_bytes(),
        format!(
            "<< /Type /FontDescriptor /FontName /{large_lookup_name} /Flags 32 /FontFile2 8 0 R >>"
        )
        .into_bytes(),
        stream("", PRIMARY_TTF),
    ]);

    let base_font_bytes = PRIMARY_TTF.len() as u64;
    let lookup_bytes = large_lookup_name.len() as u64;

    // 1. With budget fitting the embedded font + ONE shared copy of the 16 KB
    //    override name (strictly < font + 2 * 16 KB and strictly < 100 KB),
    //    all 32 aliases share the Arc spec and charge `lookup_bytes` once,
    //    never retaining the 100 KB unrecognized `/Ordering` string.
    let shared_budget = base_font_bytes + lookup_bytes + 512;
    assert!(shared_budget < base_font_bytes + 2 * lookup_bytes);
    assert!(shared_budget < huge_ordering.len() as u64);
    let doc_shared = PdfDocument::open_with_limits(
        pdf.clone(),
        zpdf::ParseLimits {
            max_font_cache_bytes: shared_budget,
            ..Default::default()
        },
    )
    .unwrap();
    let page = doc_shared.page(0).unwrap();
    let fonts = doc_shared.load_page_fonts(&page);
    assert!(
        fonts.bytes_used() <= shared_budget,
        "page cache bytes {} must stay within budget {shared_budget}",
        fonts.bytes_used()
    );
    assert_eq!(
        fonts.bytes_used(),
        base_font_bytes + lookup_bytes,
        "shared override name must be charged once across all 32 aliases (ignoring 100 KB /Ordering)"
    );

    // 2. When budget fits only the embedded font (not the 16 KB override name),
    //    the oversized override is dropped so `bytes_used()` stays exact and
    //    within `max_font_cache_bytes`.
    let doc_tight = PdfDocument::open_with_limits(
        pdf,
        zpdf::ParseLimits {
            max_font_cache_bytes: base_font_bytes,
            ..Default::default()
        },
    )
    .unwrap();
    let page_tight = doc_tight.page(0).unwrap();
    let fonts_tight = doc_tight.load_page_fonts(&page_tight);
    assert_eq!(fonts_tight.bytes_used(), base_font_bytes);
}

#[test]
fn zero_incremental_cost_descriptor_substitute_admits_in_exactly_full_cache() {
    let probe = zpdf_font::LoadedFont::new_with_data(
        zpdf_font::PdfFontType::TrueType,
        "Probe".into(),
        PRIMARY_TTF.to_vec(),
        zpdf_font::CidWidths::new(1000.0),
    );
    let gid_a = probe.unicode_glyph('A' as u32).unwrap().0;
    // Adobe-GB1 CID 34 (0x0022) is U+0041 ('A'), matching `gid_a == 34`.
    assert_eq!(gid_a, 34);
    let partial_ttf = corrupt_selected_gids(PRIMARY_TTF, &[gid_a]);

    // Both /F1 (corrupt load-time fallback) and /F2 (partially corrupt
    // Identity-H with Adobe-GB1 collection, default /DW 1000, no /W, and no
    // /ToUnicode) select the SAME registered PRIMARY_TTF (`Arc<[u8]>`).
    let provider = Arc::new(FontProvider::memory_only());
    provider
        .register_with_aliases(PRIMARY_TTF, 0, &["FullCacheSharedFace"])
        .unwrap();

    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] \
          /Resources << /Font << /F1 5 0 R /F2 8 0 R >> >> /Contents 4 0 R >>"
            .to_vec(),
        stream(
            "",
            b"0 0 0 rg BT /F1 24 Tf 15 40 Td (A) Tj /F2 24 Tf 35 0 Td <0022> Tj ET",
        ),
        b"<< /Type /Font /Subtype /TrueType /BaseFont /ABCDEF+FullCacheSharedFace \
          /Encoding /WinAnsiEncoding /FirstChar 65 /LastChar 67 /Widths [620 780 540] \
          /FontDescriptor 6 0 R >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+FullCacheSharedFace /Flags 32 /FontFile2 7 0 R >>"
            .to_vec(),
        stream("", b"corrupt simple font stream"),
        b"<< /Type /Font /Subtype /Type0 /BaseFont /GHIJKL+FullCacheSharedFace /Encoding /Identity-H \
          /DescendantFonts [9 0 R] >>"
            .to_vec(),
        b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /GHIJKL+FullCacheSharedFace \
          /CIDSystemInfo << /Registry (Adobe) /Ordering (GB1) /Supplement 0 >> \
          /CIDToGIDMap /Identity /FontDescriptor 10 0 R /DW 1000 >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /GHIJKL+FullCacheSharedFace /Flags 32 /FontFile2 11 0 R >>"
            .to_vec(),
        stream("", &partial_ttf),
    ]);

    // Measure the exact bytes used after loading /F1 and /F2 so the page cache
    // starts 100% full (`remaining == 0`) before any text show runs.
    let probe_doc = PdfDocument::open(pdf.clone())
        .unwrap()
        .with_font_provider(Arc::clone(&provider));
    let probe_page = probe_doc.page(0).unwrap();
    let exact_full_budget = probe_doc.load_page_fonts(&probe_page).bytes_used();

    let doc = PdfDocument::open_with_limits(
        pdf,
        zpdf::ParseLimits {
            max_font_cache_bytes: exact_full_budget,
            ..Default::default()
        },
    )
    .unwrap()
    .with_font_provider(provider);
    let page = zpdf::interpret_page(&doc, 0, Default::default()).unwrap();
    let (fid2, f2) = page.fonts.get_by_name("F2").unwrap();
    assert!(!f2.is_substitute, "/F2 embedded face stays primary");
    assert_eq!(
        page.fonts.bytes_used(),
        exact_full_budget,
        "zero-incremental-cost substitute must admit without exceeding an exactly full cache"
    );
    assert!(
        page.fonts.has_admitted_embedded_companion(fid2),
        "/F2 must admit its zero-incremental-cost descriptor substitute at remaining == 0"
    );
    assert_eq!(page.stats.diagnostics.embedded_font_fallbacks, 2);
    assert_eq!(page.stats.diagnostics.missing_glyph_outlines, 0);
    assert!(page.is_complete());

    let cpu = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&page.fonts)
        .render_display_list(&page.display_list, 1.0)
        .unwrap();
    assert!(
        dark_pixels_in_rect(
            &cpu.data,
            cpu.width as usize,
            cpu.height as usize,
            50,
            75,
            30,
            70,
        ) > 10,
        "/F2's recovered CID 34 ('A') must draw ink even when the cache started exactly full"
    );
}

/// Build a TrueType variant of `PRIMARY_TTF` whose `cmap` contains ONLY a
/// Windows `(3, 1)` Unicode format-4 subtable mapping `U+F041` (PUA Alpha) to
/// `target_gid` (with NO `U+0391` mapping and a format-3.0 `post` table with
/// NO glyph names).
fn build_windows_unicode_pua_only_ttf(ttf: &[u8], target_gid: u16) -> Vec<u8> {
    // Format-4 subtable with 2 segments: [0xF041..=0xF041 -> target_gid] and
    // sentinel [0xFFFF..=0xFFFF -> 0].
    let seg_count: u16 = 2;
    let id_delta: u16 = target_gid.wrapping_sub(0xF041);
    let mut subtable = Vec::new();
    subtable.extend_from_slice(&4u16.to_be_bytes()); // format
    subtable.extend_from_slice(&32u16.to_be_bytes()); // length
    subtable.extend_from_slice(&0u16.to_be_bytes()); // language
    subtable.extend_from_slice(&(seg_count * 2).to_be_bytes()); // segCountX2
    subtable.extend_from_slice(&4u16.to_be_bytes()); // searchRange
    subtable.extend_from_slice(&1u16.to_be_bytes()); // entrySelector
    subtable.extend_from_slice(&0u16.to_be_bytes()); // rangeShift
    subtable.extend_from_slice(&0xF041u16.to_be_bytes()); // endCode[0]
    subtable.extend_from_slice(&0xFFFFu16.to_be_bytes()); // endCode[1]
    subtable.extend_from_slice(&0u16.to_be_bytes()); // reservedPad
    subtable.extend_from_slice(&0xF041u16.to_be_bytes()); // startCode[0]
    subtable.extend_from_slice(&0xFFFFu16.to_be_bytes()); // startCode[1]
    subtable.extend_from_slice(&id_delta.to_be_bytes()); // idDelta[0]
    subtable.extend_from_slice(&1u16.to_be_bytes()); // idDelta[1]
    subtable.extend_from_slice(&0u16.to_be_bytes()); // idRangeOffset[0]
    subtable.extend_from_slice(&0u16.to_be_bytes()); // idRangeOffset[1]
    assert_eq!(subtable.len(), 32);

    let mut new_cmap = Vec::new();
    new_cmap.extend_from_slice(&0u16.to_be_bytes()); // version
    new_cmap.extend_from_slice(&1u16.to_be_bytes()); // numTables
    new_cmap.extend_from_slice(&3u16.to_be_bytes()); // platformID = Windows
    new_cmap.extend_from_slice(&1u16.to_be_bytes()); // encodingID = Unicode BMP
    new_cmap.extend_from_slice(&12u32.to_be_bytes()); // offset
    new_cmap.extend_from_slice(&subtable);

    // post format 3.0 (32 bytes): explicitly carries NO glyph names.
    let mut new_post = [0u8; 32];
    new_post[..4].copy_from_slice(&0x0003_0000u32.to_be_bytes());

    let num_tables = u16::from_be_bytes([ttf[4], ttf[5]]) as usize;
    let mut out = Vec::with_capacity(ttf.len());
    out.extend_from_slice(&ttf[..12 + num_tables * 16]);
    for i in 0..num_tables {
        let rec = 12 + i * 16;
        let tag = &ttf[rec..rec + 4];
        let old_off = u32::from_be_bytes(ttf[rec + 8..rec + 12].try_into().unwrap()) as usize;
        let old_len = u32::from_be_bytes(ttf[rec + 12..rec + 16].try_into().unwrap()) as usize;
        let table_bytes: &[u8] = if tag == b"cmap" {
            &new_cmap
        } else if tag == b"post" {
            &new_post
        } else {
            &ttf[old_off..old_off + old_len]
        };
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
        let new_off = out.len() as u32;
        let new_len = table_bytes.len() as u32;
        out[rec + 8..rec + 12].copy_from_slice(&new_off.to_be_bytes());
        out[rec + 12..rec + 16].copy_from_slice(&new_len.to_be_bytes());
        out.extend_from_slice(table_bytes);
    }
    out
}

#[test]
fn canonical_symbol_substitute_uses_windows_unicode_pua_while_remapped_type1_does_not() {
    let probe = zpdf_font::LoadedFont::new_with_data(
        zpdf_font::PdfFontType::TrueType,
        "Probe".into(),
        PRIMARY_TTF.to_vec(),
        zpdf_font::CidWidths::new(1000.0),
    );
    let gid_a = probe.unicode_glyph('A' as u32).unwrap().0;
    let pua_ttf = build_windows_unicode_pua_only_ttf(PRIMARY_TTF, gid_a);

    // 1. Symbolic `/Symbol` font (receives canonical `SYMBOL_ENCODING` where
    //    byte 65 = `/Alpha`) substituting onto a face that maps Alpha ONLY at
    //    Windows `(3,1)` `U+F041` (no `U+0391` and no `post` glyph names):
    //    byte 65 must still resolve to `gid_a` and render cleanly.
    let sym_provider = Arc::new(FontProvider::memory_only());
    sym_provider
        .register_with_aliases(pua_ttf.clone(), 0, &["Symbol", "RemappedSymT1"])
        .unwrap();
    let sym_pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] \
          /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
            .to_vec(),
        stream("", b"0 0 0 rg BT /F1 24 Tf 15 40 Td (A) Tj ET"),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Symbol /FontDescriptor 6 0 R >>".to_vec(),
        b"<< /Type /FontDescriptor /FontName /Symbol /Flags 4 /FontFile 7 0 R >>".to_vec(),
        stream("", b"corrupt symbol font"),
    ]);
    let sym_doc = PdfDocument::open(sym_pdf)
        .unwrap()
        .with_font_provider(Arc::clone(&sym_provider));
    let sym_page = zpdf::interpret_page(&sym_doc, 0, Default::default()).unwrap();
    let (_, sym_font) = sym_page.fonts.get_by_name("F1").unwrap();
    assert!(sym_font.is_substitute && sym_font.symbolic && sym_font.encoding.is_some());
    assert_eq!(sym_font.code_to_gid(65), Some(gid_a));
    assert_eq!(sym_page.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(sym_page.stats.diagnostics.missing_glyph_outlines, 0);
    assert!(sym_page.is_complete());

    // 2. Remapped symbolic Type 1 (`65 -> /B`, not canonical Symbol `/Alpha`)
    //    substituting onto the same PUA-only face must NOT match `U+F041`!
    let remapped_pfa = build_remapped_symbolic_type1_pfa(false);
    let remapped_pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] \
          /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
            .to_vec(),
        stream("", b"0 0 0 rg BT /F1 24 Tf 15 40 Td (A) Tj ET"),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /ABCDEF+RemappedSymT1 \
          /FirstChar 65 /LastChar 67 /Widths [780 540 540] /FontDescriptor 6 0 R >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+RemappedSymT1 /Flags 4 /FontFile 7 0 R >>"
            .to_vec(),
        stream("", &remapped_pfa),
    ]);
    let remapped_doc = PdfDocument::open(remapped_pdf)
        .unwrap()
        .with_font_provider(sym_provider);
    let remapped_page = zpdf::interpret_page(&remapped_doc, 0, Default::default()).unwrap();
    let (rfid, _) = remapped_page.fonts.get_by_name("F1").unwrap();
    assert!(!remapped_page.fonts.has_admitted_embedded_companion(rfid));
    assert_eq!(remapped_page.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(remapped_page.stats.diagnostics.missing_glyph_outlines, 1);
    assert!(!remapped_page.is_complete());
}

#[test]
fn shared_cache_shortfall_preserves_descriptor_lookup_name_for_later_pages() {
    let probe = zpdf_font::LoadedFont::new_with_data(
        zpdf_font::PdfFontType::TrueType,
        "Probe".into(),
        PRIMARY_TTF.to_vec(),
        zpdf_font::CidWidths::new(1000.0),
    );
    let gid_a = probe.unicode_glyph('A' as u32).unwrap().0;
    let partial_ttf = corrupt_selected_gids(PRIMARY_TTF, &[gid_a]);

    // Font `/F2` omits `/BaseFont` and carries a `/FontDescriptor /FontName`
    // that selects registered `BOLD_TTF` (while `"Unknown"` matches nothing).
    let desc_lookup_name = "DescOnlyFaceName";
    let provider = Arc::new(FontProvider::memory_only());
    provider
        .register_with_aliases(BOLD_TTF, 0, &[desc_lookup_name])
        .unwrap();

    // Page 0 references `/Filler` (fills part of `SharedFonts`) AND `/F2` (shows
    // only healthy 'B'). Page 1 references ONLY `/F2` and shows damaged 'A'.
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] \
          /Resources << /Font << /Filler 7 0 R /F2 10 0 R >> >> /Contents 5 0 R >>"
            .to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] \
          /Resources << /Font << /F2 10 0 R >> >> /Contents 6 0 R >>"
            .to_vec(),
        stream(
            "",
            b"0 0 0 rg BT /Filler 24 Tf 15 40 Td (B) Tj /F2 24 Tf 35 0 Td (B) Tj ET",
        ),
        stream("", b"0 0 0 rg BT /F2 24 Tf 15 40 Td (A) Tj ET"),
        b"<< /Type /Font /Subtype /TrueType /BaseFont /ABCDEF+FillerFace \
          /Encoding /WinAnsiEncoding /FirstChar 65 /LastChar 67 /Widths [620 780 540] \
          /FontDescriptor 8 0 R >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+FillerFace /Flags 32 /FontFile2 9 0 R >>"
            .to_vec(),
        stream("", PRIMARY_TTF),
        b"<< /Type /Font /Subtype /TrueType \
          /Encoding /WinAnsiEncoding /FirstChar 65 /LastChar 67 /Widths [620 780 540] \
          /FontDescriptor 11 0 R >>"
            .to_vec(),
        format!(
            "<< /Type /FontDescriptor /FontName /{desc_lookup_name} /Flags 32 /FontFile2 12 0 R >>"
        )
        .into_bytes(),
        stream("", &partial_ttf),
    ]);

    // Pick a budget that fits BOTH primaries together in `SharedFonts` alone
    // (leaving less than `desc_lookup_name.len()` bytes free on Page 0), while
    // Page 1 alone has plenty of room for F2 + `desc_lookup_name` + BOLD_TTF.
    let probe_doc = PdfDocument::open(pdf.clone()).unwrap();
    let p0_probe = probe_doc.page(0).unwrap();
    let p0_fonts = probe_doc.load_page_fonts(&p0_probe);
    let (_, filler_f) = p0_fonts.get_by_name("Filler").unwrap();
    let (_, f2_f) = p0_fonts.get_by_name("F2").unwrap();
    let two_primaries = filler_f.estimated_cache_bytes() + f2_f.estimated_cache_bytes();
    let budget = two_primaries + 4;
    assert!(
        budget < two_primaries + desc_lookup_name.len() as u64,
        "Page 0 shared cache must fit both primaries alone, not both + descriptor lookup name"
    );
    assert!(
        budget
            > f2_f.estimated_cache_bytes()
                + desc_lookup_name.len() as u64
                + BOLD_TTF.len() as u64
                + 8192,
        "Page 1 alone must have plenty of room for F2 + descriptor lookup name + BOLD_TTF"
    );

    let doc = PdfDocument::open_with_limits(
        pdf,
        zpdf::ParseLimits {
            max_font_cache_bytes: budget,
            ..Default::default()
        },
    )
    .unwrap()
    .with_font_provider(provider);
    let page0 = zpdf::interpret_page(&doc, 0, Default::default()).unwrap();
    assert!(page0.is_complete());

    // Page 1 must NOT hit a degraded SharedFonts entry that stripped `/FontName`;
    // it must preserve the full `/FontName` and recover 'A' via BOLD_TTF!
    let page1 = zpdf::interpret_page(&doc, 1, Default::default()).unwrap();
    let (fid2, _) = page1.fonts.get_by_name("F2").unwrap();
    assert!(
        page1.fonts.has_admitted_embedded_companion(fid2),
        "Page 1 must retain full descriptor /FontName and admit BOLD_TTF"
    );
    assert_eq!(page1.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(page1.stats.diagnostics.missing_glyph_outlines, 0);
    assert!(page1.is_complete());
}

#[test]
fn primary_font_programs_take_priority_over_optional_descriptor_lookup_names_in_either_order() {
    // Two distinct embedded primary fonts on one page:
    // - `unused` has omitted `/BaseFont` and a 16 KB `/FontDescriptor /FontName`
    // - `visible` is a normal healthy embedded primary font actually drawn on the page.
    // Under a budget that fits BOTH primary programs together, but NOT both
    // primaries + the 16 KB optional lookup name, BOTH admission orders
    // (`unused` first, then `visible`; AND `visible` first, then `unused`) must
    // keep `visible` fully drawable (never downgraded to metrics-only).
    let long_name = format!("UnusedOverride{}", "Q".repeat(16_384));
    let unused_primary = Arc::new(zpdf_font::LoadedFont::new_with_data(
        zpdf_font::PdfFontType::TrueType,
        "Unknown".into(),
        PRIMARY_TTF.to_vec(),
        zpdf_font::CidWidths::new(1000.0),
    ));
    let visible_primary = Arc::new(zpdf_font::LoadedFont::new_with_data(
        zpdf_font::PdfFontType::TrueType,
        "VisiblePrimary".into(),
        PRIMARY_TTF.to_vec(),
        zpdf_font::CidWidths::new(1000.0),
    ));
    let spec = Arc::new(zpdf_font::DescriptorSubstituteSpec {
        lookup_name: Some(Arc::<str>::from(long_name.as_str())),
        hints: zpdf_font::system::SubstituteHints::default(),
        ordering: None,
        font_file_id: None,
    });

    let two_primaries =
        unused_primary.estimated_cache_bytes() + visible_primary.estimated_cache_bytes();
    let budget = two_primaries + 1024;
    assert!(budget < two_primaries + long_name.len() as u64);

    for unused_first in [true, false] {
        let mut cache = zpdf::FontCache::new();
        if unused_first {
            let ufid = cache
                .admit_with_diagnostics(
                    "FUnused".into(),
                    Arc::clone(&unused_primary),
                    Default::default(),
                    None,
                    budget,
                )
                .unwrap();
            cache.record_descriptor_substitute(ufid, Arc::clone(&spec), budget);
            let _ = cache
                .admit_with_diagnostics(
                    "FVisible".into(),
                    Arc::clone(&visible_primary),
                    Default::default(),
                    None,
                    budget,
                )
                .unwrap();
        } else {
            let _ = cache
                .admit_with_diagnostics(
                    "FVisible".into(),
                    Arc::clone(&visible_primary),
                    Default::default(),
                    None,
                    budget,
                )
                .unwrap();
            let ufid = cache
                .admit_with_diagnostics(
                    "FUnused".into(),
                    Arc::clone(&unused_primary),
                    Default::default(),
                    None,
                    budget,
                )
                .unwrap();
            cache.record_descriptor_substitute(ufid, Arc::clone(&spec), budget);
        }

        let (_, vis) = cache.get_by_name("FVisible").unwrap();
        let (_, un) = cache.get_by_name("FUnused").unwrap();
        assert!(
            vis.has_font_data(),
            "unused_first={unused_first}: visible primary must never be downgraded to metrics-only"
        );
        assert!(
            un.has_font_data(),
            "unused_first={unused_first}: unused primary must also stay intact"
        );
        assert!(
            cache.bytes_used() <= budget,
            "unused_first={unused_first}: cache bytes {} must stay within budget {budget}",
            cache.bytes_used()
        );
        assert!(
            cache.diagnostics().is_empty(),
            "unused_first={unused_first}: no resource-limit fallback diagnostic expected"
        );
    }
}

/// Build a TrueType variant of `PRIMARY_TTF` with 12-byte valid-header
/// zero-contour records for `gid_space` and `gid_a`, and a large 5,000-group
/// format-12 Unicode `cmap` (larger than `CMAP_PROBE_BUDGET = 4096`) where
/// `' '` (`U+0020 -> gid_space`) and `'A'` (`U+0041 -> gid_a`) sit near the
/// start of the subtable.
fn build_large_format12_zero_contour_ttf(
    ttf: &[u8],
    gid_space: u16,
    gid_a: u16,
    gid_b: u16,
) -> Vec<u8> {
    let base = valid_header_zero_contour_mixed_ttf(ttf, gid_space, gid_a);
    let mut groups: Vec<(u32, u32, u32)> = Vec::with_capacity(5004);
    groups.push((0x0020, 0x0020, gid_space as u32));
    groups.push((0x0041, 0x0041, gid_a as u32));
    groups.push((0x0042, 0x0042, gid_b as u32));
    // Add 5,000 extra distinct non-contiguous groups mapping to gid_b so a full
    // reverse scan for `gid_space` exceeds `CMAP_PROBE_BUDGET` (4096).
    for i in 0..5000u32 {
        let cp = 0x10000 + i * 2;
        groups.push((cp, cp, gid_b as u32));
    }
    let sub_len = (16 + groups.len() * 12) as u32;
    let mut subtable = Vec::with_capacity(sub_len as usize);
    subtable.extend_from_slice(&12u16.to_be_bytes()); // format 12
    subtable.extend_from_slice(&0u16.to_be_bytes()); // reserved
    subtable.extend_from_slice(&sub_len.to_be_bytes()); // length
    subtable.extend_from_slice(&0u32.to_be_bytes()); // language
    subtable.extend_from_slice(&(groups.len() as u32).to_be_bytes());
    for (start_cp, end_cp, start_gid) in groups {
        subtable.extend_from_slice(&start_cp.to_be_bytes());
        subtable.extend_from_slice(&end_cp.to_be_bytes());
        subtable.extend_from_slice(&start_gid.to_be_bytes());
    }

    let mut new_cmap = Vec::with_capacity(12 + subtable.len());
    new_cmap.extend_from_slice(&0u16.to_be_bytes()); // version
    new_cmap.extend_from_slice(&1u16.to_be_bytes()); // numTables
    new_cmap.extend_from_slice(&3u16.to_be_bytes()); // platformID = Windows
    new_cmap.extend_from_slice(&10u16.to_be_bytes()); // encodingID = Unicode full repertoire
    new_cmap.extend_from_slice(&12u32.to_be_bytes()); // offset
    new_cmap.extend_from_slice(&subtable);

    let num_tables = u16::from_be_bytes([base[4], base[5]]) as usize;
    let mut out = Vec::with_capacity(base.len() + new_cmap.len());
    out.extend_from_slice(&base[..12 + num_tables * 16]);
    for i in 0..num_tables {
        let rec = 12 + i * 16;
        let tag = &base[rec..rec + 4];
        let old_off = u32::from_be_bytes(base[rec + 8..rec + 12].try_into().unwrap()) as usize;
        let old_len = u32::from_be_bytes(base[rec + 12..rec + 16].try_into().unwrap()) as usize;
        let table_bytes: &[u8] = if tag == b"cmap" {
            &new_cmap
        } else {
            &base[old_off..old_off + old_len]
        };
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
        let new_off = out.len() as u32;
        let new_len = table_bytes.len() as u32;
        out[rec + 8..rec + 12].copy_from_slice(&new_off.to_be_bytes());
        out[rec + 12..rec + 16].copy_from_slice(&new_len.to_be_bytes());
        out.extend_from_slice(table_bytes);
    }
    out
}

#[test]
fn zero_contour_space_in_large_format12_cmap_does_not_exhaust_recovery_for_later_letter() {
    let probe = zpdf_font::LoadedFont::new_with_data(
        zpdf_font::PdfFontType::TrueType,
        "Probe".into(),
        PRIMARY_TTF.to_vec(),
        zpdf_font::CidWidths::new(1000.0),
    );
    let gid_space = probe.unicode_glyph(' ' as u32).unwrap().0;
    let gid_a = probe.unicode_glyph('A' as u32).unwrap().0;
    let gid_b = probe.unicode_glyph('B' as u32).unwrap().0;
    let large_cmap_ttf =
        build_large_format12_zero_contour_ttf(PRIMARY_TTF, gid_space, gid_a, gid_b);

    let provider = Arc::new(FontProvider::memory_only());
    provider
        .register_with_aliases(BOLD_TTF, 0, &["LargeFmt12Face"])
        .unwrap();

    // 1. Simple WinAnsi font: showing space FIRST in a 5,000-group format-12
    //    cmap must use direct evidence (spending zero reverse-cmap budget) and
    //    never prevent later 'A' from recovering via BOLD_TTF.
    let mut widths = [500u16; 36];
    widths[0] = 300;
    widths[65 - 32] = 620;
    widths[66 - 32] = 780;
    let widths_str: String = widths
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    let winansi_pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] \
          /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
            .to_vec(),
        stream(
            "",
            b"0 0 0 rg BT /F1 24 Tf 15 40 Td ( ) Tj (A) Tj (B) Tj ET",
        ),
        format!(
            "<< /Type /Font /Subtype /TrueType /BaseFont /ABCDEF+LargeFmt12Face \
             /Encoding /WinAnsiEncoding /FirstChar 32 /LastChar 67 /Widths [{widths_str}] \
             /FontDescriptor 6 0 R >>"
        )
        .into_bytes(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+LargeFmt12Face /Flags 32 /FontFile2 7 0 R >>"
            .to_vec(),
        stream("", &large_cmap_ttf),
    ]);
    let win_doc = PdfDocument::open(winansi_pdf)
        .unwrap()
        .with_font_provider(Arc::clone(&provider));
    let win_page = zpdf::interpret_page(&win_doc, 0, Default::default()).unwrap();
    let (wfid, wf) = win_page.fonts.get_by_name("F1").unwrap();
    assert!(!wf.is_substitute);
    assert!(win_page.fonts.has_admitted_embedded_companion(wfid));
    assert_eq!(win_page.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(win_page.stats.diagnostics.missing_glyph_outlines, 0);
    assert!(win_page.is_complete());

    // 2. Identity-H Type 0 WITHOUT `/ToUnicode`: showing unmapped space
    //    `<0001>` FIRST must NOT exhaust `cmap_budget` for later damaged
    //    zero-contour `<0022>` ('A', near the start of the 5,000-group cmap)
    //    and must report fallback + missing outline instead of silently blanking 'A'.
    let cid_pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] \
          /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
            .to_vec(),
        stream(
            "",
            format!(
                "0 0 0 rg BT /F1 24 Tf 15 40 Td <{gid_space:04X}> Tj <{gid_a:04X}> Tj <{gid_b:04X}> Tj ET"
            )
            .as_bytes(),
        ),
        b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+LargeFmt12Face /Encoding /Identity-H \
          /DescendantFonts [6 0 R] >>"
            .to_vec(),
        b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+LargeFmt12Face \
          /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
          /CIDToGIDMap /Identity /FontDescriptor 7 0 R /DW 700 >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+LargeFmt12Face /Flags 32 /FontFile2 8 0 R >>"
            .to_vec(),
        stream("", &large_cmap_ttf),
    ]);
    let cid_doc = PdfDocument::open(cid_pdf)
        .unwrap()
        .with_font_provider(Arc::new(FontProvider::memory_only()));
    let cid_page = zpdf::interpret_page(&cid_doc, 0, Default::default()).unwrap();
    assert_eq!(
        cid_page.stats.diagnostics.embedded_font_fallbacks, 1,
        "showing unmapped space first must not suppress fallback detection for later 'A'"
    );
    assert_eq!(cid_page.stats.diagnostics.missing_glyph_outlines, 1);
    assert!(!cid_page.is_complete());
}

/// Build a Type 1 PFA with drawable `/B` and `/C`, genuine `/space`, and an
/// empty charstring under custom glyph name `/g3`.
fn build_type1_empty_custom_g3_pfa() -> Vec<u8> {
    let mut private = Vec::new();
    private.extend_from_slice(
        b"dup /Private 4 dict dup begin /lenIV -1 def /CharStrings 5 dict dup begin\n",
    );
    let entries: &[(&str, &[u8])] = &[
        (".notdef", &[139, 248, 136, 13, 14]),
        ("space", &[139, 247, 192, 13, 14]),
        ("g3", &[139, 249, 0, 13, 14]), // empty custom glyph name
        (
            "B",
            &[
                139, 249, 160, 13, 189, 139, 21, 139, 249, 80, 5, 248, 136, 139, 5, 139, 253, 80,
                5, 9, 14,
            ],
        ),
        (
            "C",
            &[
                139, 248, 176, 13, 189, 139, 21, 139, 249, 80, 5, 248, 36, 139, 5, 139, 253, 80, 5,
                9, 14,
            ],
        ),
    ];
    for (name, cs) in entries {
        private.extend_from_slice(format!("/{name} {} RD ", cs.len()).as_bytes());
        private.extend_from_slice(cs);
        private.extend_from_slice(b" ND\n");
    }
    private.extend_from_slice(b"end end readonly def\n");

    let mut r: u16 = 55665;
    let mut hex = String::with_capacity((private.len() + 4) * 2);
    for &p in [0u8, 0, 0, 0].iter().chain(private.iter()) {
        let c = p ^ (r >> 8) as u8;
        r = (c as u16)
            .wrapping_add(r)
            .wrapping_mul(52845)
            .wrapping_add(22719);
        use std::fmt::Write;
        let _ = write!(&mut hex, "{c:02X}");
    }

    let mut pfa = Vec::new();
    pfa.extend_from_slice(
        b"%!PS-AdobeFont-1.0: CustomG3Type1 1.0\n\
          8 dict begin\n\
          /FontName /CustomG3Type1 def /FontType 1 def /PaintType 0 def\n\
          /FontMatrix [0.001 0 0 0.001 0 0] readonly def\n\
          /FontBBox [0 0 1000 900] readonly def /Encoding StandardEncoding def\n\
          currentdict end currentfile eexec\n",
    );
    pfa.extend_from_slice(hex.as_bytes());
    pfa.push(b'\n');
    pfa
}

#[test]
fn type1_empty_custom_glyph_name_uses_tounicode_without_caching_by_gid_alone() {
    let pfa = build_type1_empty_custom_g3_pfa();
    // Both code 1 and code 2 map via `/Differences` to the SAME empty custom
    // glyph `/g3`. `/ToUnicode` maps code 1 -> 'A' (visible!) and code 2 -> ' '
    // (genuine space!).
    let to_unicode = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
        1 begincodespacerange <00> <FF> endcodespacerange \
        4 beginbfchar <01> <0041> <02> <0020> <42> <0042> <43> <0043> endbfchar \
        endcmap CMapName currentdict /CMap defineresource pop end end";
    let make_pdf = |content: &[u8]| {
        assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] \
              /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
                .to_vec(),
            stream("", content),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /ABCDEF+CustomG3Type1 \
              /Encoding << /Type /Encoding /BaseEncoding /WinAnsiEncoding /Differences [1 /g3 /g3] >> \
              /ToUnicode 8 0 R /FontDescriptor 6 0 R >>"
                .to_vec(),
            b"<< /Type /FontDescriptor /FontName /ABCDEF+CustomG3Type1 /Flags 32 /FontFile 7 0 R >>"
                .to_vec(),
            stream("", &pfa),
            stream("", to_unicode),
        ])
    };

    // 1. Showing ONLY code 2 (/g3 -> ' ') and 'B' keeps the embedded Type 1
    //    primary with zero fallbacks.
    let space_only_doc =
        PdfDocument::open(make_pdf(b"0 0 0 rg BT /F1 24 Tf 15 40 Td <0242> Tj ET"))
            .unwrap()
            .with_font_provider(Arc::new(FontProvider::memory_only()));
    let space_page = zpdf::interpret_page(&space_only_doc, 0, Default::default()).unwrap();
    let (sfid, sf) = space_page.fonts.get_by_name("F1").unwrap();
    assert!(!sf.is_substitute);
    assert!(!space_page.fonts.has_admitted_embedded_companion(sfid));
    assert_eq!(space_page.stats.diagnostics.embedded_font_fallbacks, 0);
    assert_eq!(space_page.stats.diagnostics.missing_glyph_outlines, 0);
    assert!(space_page.is_complete());

    // 2. Showing code 2 (/g3 -> ' ') FIRST followed by code 1 (/g3 -> 'A') on
    //    the SAME page must NOT cache /g3 as readable by GID alone: code 1 must
    //    recover via descriptor substitute (BOLD_TTF via /ToUnicode 'A') while
    //    code 2 and 'B' stay on the embedded Type 1 face.
    let mixed_pdf = make_pdf(b"0 0 0 rg BT /F1 24 Tf 15 40 Td <02> Tj <01> Tj (B) Tj ET");
    let provider = Arc::new(FontProvider::memory_only());
    provider
        .register_with_aliases(BOLD_TTF, 0, &["CustomG3Type1"])
        .unwrap();
    let sub_doc = PdfDocument::open(mixed_pdf.clone())
        .unwrap()
        .with_font_provider(provider);
    let sub_page = zpdf::interpret_page(&sub_doc, 0, Default::default()).unwrap();
    let (fid, f) = sub_page.fonts.get_by_name("F1").unwrap();
    assert!(!f.is_substitute);
    assert!(sub_page.fonts.has_admitted_embedded_companion(fid));
    assert_eq!(sub_page.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(sub_page.stats.diagnostics.missing_glyph_outlines, 0);
    assert!(sub_page.is_complete());

    // 3. Without any substitute (memory_only), showing code 2 then code 1 must
    //    still flag embedded_font_fallbacks=1 and missing_glyph_outlines=1.
    let no_sub_doc = PdfDocument::open(mixed_pdf)
        .unwrap()
        .with_font_provider(Arc::new(FontProvider::memory_only()));
    let no_sub_page = zpdf::interpret_page(&no_sub_doc, 0, Default::default()).unwrap();
    assert_eq!(no_sub_page.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(no_sub_page.stats.diagnostics.missing_glyph_outlines, 1);
    assert!(!no_sub_page.is_complete());
}

#[test]
fn shared_cid_nonempty_zero_contour_glyph_respects_per_code_tounicode_in_both_orders() {
    let probe = zpdf_font::LoadedFont::new_with_data(
        zpdf_font::PdfFontType::TrueType,
        "Probe".into(),
        PRIMARY_TTF.to_vec(),
        zpdf_font::CidWidths::new(1000.0),
    );
    let gid_space = probe.unicode_glyph(' ' as u32).unwrap().0;
    let gid_a = probe.unicode_glyph('A' as u32).unwrap().0;
    let gid_b = probe.unicode_glyph('B' as u32).unwrap().0;
    // `gid_space` has a 12-byte valid-header zero-contour record in a compact
    // 3-group format-12 cmap (well under `PER_GLYPH_CMAP_BUDGET`), mapping only
    // U+0020 (space) to `gid_space`, so its reverse cmap probe deterministically
    // returns `Some(false)` (`CodeDependentFallbackReadable`) rather than
    // exhausting budget.
    let base_ttf = valid_header_zero_contour_mixed_ttf(PRIMARY_TTF, gid_space, gid_a);
    let groups: [(u32, u32, u32); 3] = [
        (0x0020, 0x0020, gid_space as u32),
        (0x0041, 0x0041, gid_a as u32),
        (0x0042, 0x0042, gid_b as u32),
    ];
    let sub_len = (16 + groups.len() * 12) as u32;
    let mut subtable = Vec::with_capacity(sub_len as usize);
    subtable.extend_from_slice(&12u16.to_be_bytes());
    subtable.extend_from_slice(&0u16.to_be_bytes());
    subtable.extend_from_slice(&sub_len.to_be_bytes());
    subtable.extend_from_slice(&0u32.to_be_bytes());
    subtable.extend_from_slice(&(groups.len() as u32).to_be_bytes());
    for (start_cp, end_cp, start_gid) in groups {
        subtable.extend_from_slice(&start_cp.to_be_bytes());
        subtable.extend_from_slice(&end_cp.to_be_bytes());
        subtable.extend_from_slice(&start_gid.to_be_bytes());
    }
    let mut new_cmap = Vec::with_capacity(12 + subtable.len());
    new_cmap.extend_from_slice(&0u16.to_be_bytes());
    new_cmap.extend_from_slice(&1u16.to_be_bytes());
    new_cmap.extend_from_slice(&3u16.to_be_bytes());
    new_cmap.extend_from_slice(&10u16.to_be_bytes());
    new_cmap.extend_from_slice(&12u32.to_be_bytes());
    new_cmap.extend_from_slice(&subtable);
    let num_tables = u16::from_be_bytes([base_ttf[4], base_ttf[5]]) as usize;
    let mut ttf = Vec::with_capacity(base_ttf.len());
    ttf.extend_from_slice(&base_ttf[..12 + num_tables * 16]);
    for i in 0..num_tables {
        let rec = 12 + i * 16;
        let tag = &base_ttf[rec..rec + 4];
        let old_off = u32::from_be_bytes(base_ttf[rec + 8..rec + 12].try_into().unwrap()) as usize;
        let old_len = u32::from_be_bytes(base_ttf[rec + 12..rec + 16].try_into().unwrap()) as usize;
        let table_bytes: &[u8] = if tag == b"cmap" {
            &new_cmap
        } else {
            &base_ttf[old_off..old_off + old_len]
        };
        while !ttf.len().is_multiple_of(4) {
            ttf.push(0);
        }
        let new_off = ttf.len() as u32;
        let new_len = table_bytes.len() as u32;
        ttf[rec + 8..rec + 12].copy_from_slice(&new_off.to_be_bytes());
        ttf[rec + 12..rec + 16].copy_from_slice(&new_len.to_be_bytes());
        ttf.extend_from_slice(table_bytes);
    }

    // Custom CMap maps TWO distinct source codes `<0001>` and `<0002>` to the
    // SAME CID (`gid_space`), and `<0003>` to healthy `gid_b`.
    let cmap = format!(
        "/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
         /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> def \
         /CMapName /TwoCodesOneCid def /CMapType 1 def \
         1 begincodespacerange <0000> <FFFF> endcodespacerange \
         3 begincidchar <0001> {gid_space} <0002> {gid_space} <0003> {gid_b} endcidchar \
         endcmap CMapName currentdict /CMap defineresource pop end end"
    );
    // `/ToUnicode` maps ONLY `<0002>` -> 'A' and `<0003>` -> 'B', leaving
    // `<0001>` without direct `/ToUnicode` evidence.
    let to_unicode = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
        1 begincodespacerange <0000> <FFFF> endcodespacerange \
        2 beginbfchar <0002> <0041> <0003> <0042> endbfchar \
        endcmap CMapName currentdict /CMap defineresource pop end end";

    let provider = Arc::new(FontProvider::memory_only());
    provider
        .register_with_aliases(BOLD_TTF, 0, &["SharedCidZeroContour"])
        .unwrap();

    let make_pdf = |content: &[u8]| {
        assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] \
              /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
                .to_vec(),
            stream("", content),
            b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+SharedCidZeroContour \
              /Encoding 9 0 R /DescendantFonts [6 0 R] /ToUnicode 10 0 R >>"
                .to_vec(),
            b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+SharedCidZeroContour \
              /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
              /CIDToGIDMap /Identity /FontDescriptor 7 0 R /DW 700 >>"
                .to_vec(),
            b"<< /Type /FontDescriptor /FontName /ABCDEF+SharedCidZeroContour /Flags 32 /FontFile2 8 0 R >>"
                .to_vec(),
            stream("", &ttf),
            stream("", cmap.as_bytes()),
            stream("", to_unicode),
        ])
    };

    // Showing only unmapped `<0001>` and healthy `<0003>` stays clean on the
    // embedded face (reverse cmap confirms `gid_space` has no visible mapping).
    let clean_doc = PdfDocument::open(make_pdf(
        b"0 0 0 rg BT /F1 24 Tf 15 40 Td <0001> Tj <0003> Tj ET",
    ))
    .unwrap()
    .with_font_provider(Arc::clone(&provider));
    let clean_page = zpdf::interpret_page(&clean_doc, 0, Default::default()).unwrap();
    let (cfid, cf) = clean_page.fonts.get_by_name("F1").unwrap();
    let mut probe_budget = zpdf_font::CMAP_PROBE_BUDGET;
    let mut unmapped_outcome = None;
    cf.classify_embedded_glyphs_with_codes(
        &[(gid_space, 0x0001, 0x0001, 2)],
        &mut probe_budget,
        |_, out| unmapped_outcome = Some(out),
    );
    assert_eq!(
        unmapped_outcome,
        Some(zpdf_font::EmbeddedGlyphHealthOutcome::CodeDependentFallbackReadable),
        "reverse cmap probe on unmapped <0001> must return Some(false) as CodeDependentFallbackReadable"
    );
    assert!(!cf.is_substitute);
    assert!(!clean_page.fonts.has_admitted_embedded_companion(cfid));
    assert_eq!(clean_page.stats.diagnostics.embedded_font_fallbacks, 0);
    assert!(clean_page.is_complete());

    // Cover BOTH show orders across separate `Tj` shows and within a single `Tj`:
    // 1. Unmapped `<0001>` FIRST, then `<0002>` ('A') SECOND (reverse cmap on
    //    `<0001>` must cache as a fallback only, never hiding `<0002>`'s 'A').
    // 2. `<0002>` ('A') FIRST, then unmapped `<0001>`, then `<0002>` again.
    // 3. Both `<0001><0002><0003>` in a single `Tj` show.
    for (label, content, expected_sub_runs) in [
        (
            "unmapped-first-separate-tj",
            b"0 0 0 rg BT /F1 24 Tf 15 40 Td <0001> Tj <0002> Tj <0003> Tj ET".as_slice(),
            1usize,
        ),
        (
            "tounicode-first-separate-tj",
            b"0 0 0 rg BT /F1 24 Tf 15 40 Td <0002> Tj <0001> Tj <0002> Tj <0003> Tj ET".as_slice(),
            2usize,
        ),
        (
            "mixed-single-tj",
            b"0 0 0 rg BT /F1 24 Tf 15 40 Td <000100020003> Tj ET".as_slice(),
            1usize,
        ),
    ] {
        let pdf = make_pdf(content);
        let doc = PdfDocument::open(pdf.clone())
            .unwrap()
            .with_font_provider(Arc::clone(&provider));
        let page = zpdf::interpret_page(&doc, 0, Default::default()).unwrap();
        let (fid, f) = page.fonts.get_by_name("F1").unwrap();
        assert!(!f.is_substitute, "{label}: embedded face stays primary");
        assert!(
            page.fonts.has_admitted_embedded_companion(fid),
            "{label}: <0002> ('A') must admit BOLD_TTF substitute"
        );
        assert_eq!(page.stats.diagnostics.embedded_font_fallbacks, 1, "{label}");
        assert_eq!(page.stats.diagnostics.missing_glyph_outlines, 0, "{label}");
        assert!(page.is_complete(), "{label}");
        let sub_runs = page
            .display_list
            .commands
            .iter()
            .filter(|cmd| match cmd {
                RenderCommand::DrawGlyphRun(run) => {
                    run.font_id != fid
                        && page
                            .fonts
                            .get(run.font_id)
                            .and_then(|f| f.font_data.as_deref())
                            == Some(BOLD_TTF)
                }
                _ => false,
            })
            .count();
        assert_eq!(
            sub_runs, expected_sub_runs,
            "{label}: expected {expected_sub_runs} BOLD_TTF substitute run(s) for <0002>"
        );

        // Without any substitute, `<0002>` must report missing outline rather
        // than silently blanking 'A'.
        let no_sub_doc = PdfDocument::open(pdf)
            .unwrap()
            .with_font_provider(Arc::new(FontProvider::memory_only()));
        let no_sub_page = zpdf::interpret_page(&no_sub_doc, 0, Default::default()).unwrap();
        assert_eq!(
            no_sub_page.stats.diagnostics.embedded_font_fallbacks, 1,
            "{label}"
        );
        assert_eq!(
            no_sub_page.stats.diagnostics.missing_glyph_outlines, 1,
            "{label}"
        );
        assert!(!no_sub_page.is_complete(), "{label}");
    }
}

/// Build a TrueType variant of `ttf` with a single Windows `(3, encoding_id)`
/// format-4 cmap subtable mapping ONLY `pua_code` (e.g. `0xF021`) to
/// `target_gid` and a format-3.0 `post` table carrying NO glyph names.
fn build_windows_pua_only_ttf(
    ttf: &[u8],
    encoding_id: u16,
    pua_code: u16,
    target_gid: u16,
) -> Vec<u8> {
    let seg_count: u16 = 2;
    let id_delta: u16 = target_gid.wrapping_sub(pua_code);
    let mut subtable = Vec::new();
    subtable.extend_from_slice(&4u16.to_be_bytes());
    subtable.extend_from_slice(&32u16.to_be_bytes());
    subtable.extend_from_slice(&0u16.to_be_bytes());
    subtable.extend_from_slice(&(seg_count * 2).to_be_bytes());
    subtable.extend_from_slice(&4u16.to_be_bytes());
    subtable.extend_from_slice(&1u16.to_be_bytes());
    subtable.extend_from_slice(&0u16.to_be_bytes());
    subtable.extend_from_slice(&pua_code.to_be_bytes());
    subtable.extend_from_slice(&0xFFFFu16.to_be_bytes());
    subtable.extend_from_slice(&0u16.to_be_bytes());
    subtable.extend_from_slice(&pua_code.to_be_bytes());
    subtable.extend_from_slice(&0xFFFFu16.to_be_bytes());
    subtable.extend_from_slice(&id_delta.to_be_bytes());
    subtable.extend_from_slice(&1u16.to_be_bytes());
    subtable.extend_from_slice(&0u16.to_be_bytes());
    subtable.extend_from_slice(&0u16.to_be_bytes());
    assert_eq!(subtable.len(), 32);

    let mut new_cmap = Vec::new();
    new_cmap.extend_from_slice(&0u16.to_be_bytes());
    new_cmap.extend_from_slice(&1u16.to_be_bytes());
    new_cmap.extend_from_slice(&3u16.to_be_bytes()); // platformID = Windows
    new_cmap.extend_from_slice(&encoding_id.to_be_bytes()); // 0 = Symbol, 1 = Unicode BMP
    new_cmap.extend_from_slice(&12u32.to_be_bytes());
    new_cmap.extend_from_slice(&subtable);

    let mut new_post = [0u8; 32];
    new_post[..4].copy_from_slice(&0x0003_0000u32.to_be_bytes());

    let num_tables = u16::from_be_bytes([ttf[4], ttf[5]]) as usize;
    let mut out = Vec::with_capacity(ttf.len());
    out.extend_from_slice(&ttf[..12 + num_tables * 16]);
    for i in 0..num_tables {
        let rec = 12 + i * 16;
        let tag = &ttf[rec..rec + 4];
        let old_off = u32::from_be_bytes(ttf[rec + 8..rec + 12].try_into().unwrap()) as usize;
        let old_len = u32::from_be_bytes(ttf[rec + 12..rec + 16].try_into().unwrap()) as usize;
        let table_bytes: &[u8] = if tag == b"cmap" {
            &new_cmap
        } else if tag == b"post" {
            &new_post
        } else {
            &ttf[old_off..old_off + old_len]
        };
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
        let new_off = out.len() as u32;
        let new_len = table_bytes.len() as u32;
        out[rec + 8..rec + 12].copy_from_slice(&new_off.to_be_bytes());
        out[rec + 12..rec + 16].copy_from_slice(&new_len.to_be_bytes());
        out.extend_from_slice(table_bytes);
    }
    out
}

#[test]
fn zapf_dingbats_substitute_uses_windows_symbol_pua_for_canonical_slot() {
    let probe = zpdf_font::LoadedFont::new_with_data(
        zpdf_font::PdfFontType::TrueType,
        "Probe".into(),
        PRIMARY_TTF.to_vec(),
        zpdf_font::CidWidths::new(1000.0),
    );
    let gid_a = probe.unicode_glyph('A' as u32).unwrap().0;

    // Test both Windows (3,0) Symbol and (3,1) Unicode BMP PUA-only substitutes
    // mapping ONLY U+F021 -> gid_a (with no U+2701 and no post glyph names).
    for (label, enc_id) in [("win-symbol-3-0", 0u16), ("win-unicode-3-1", 1u16)] {
        let pua_ttf = build_windows_pua_only_ttf(PRIMARY_TTF, enc_id, 0xF021, gid_a);
        let provider = Arc::new(FontProvider::memory_only());
        provider
            .register_with_aliases(pua_ttf, 0, &["ZapfDingbats"])
            .unwrap();

        // 1. Canonical `/BaseFont /ZapfDingbats` (builtin ZapfDingbats encoding,
        //    byte 0x21 = `/a1`): must resolve via U+F021 to `gid_a`.
        let zd_pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] \
              /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
                .to_vec(),
            stream("", b"0 0 0 rg BT /F1 24 Tf 15 40 Td <21> Tj ET"),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /ZapfDingbats /FontDescriptor 6 0 R >>"
                .to_vec(),
            b"<< /Type /FontDescriptor /FontName /ZapfDingbats /Flags 4 /FontFile 7 0 R >>"
                .to_vec(),
            stream("", b"corrupt zapf dingbats font"),
        ]);
        let doc = PdfDocument::open(zd_pdf)
            .unwrap()
            .with_font_provider(Arc::clone(&provider));
        let page = zpdf::interpret_page(&doc, 0, Default::default()).unwrap();
        let (_, font) = page.fonts.get_by_name("F1").unwrap();
        assert!(
            font.is_substitute && font.symbolic && font.encoding.is_some(),
            "{label}"
        );
        assert_eq!(font.code_to_gid(0x21), Some(gid_a), "{label}");
        assert_eq!(page.stats.diagnostics.embedded_font_fallbacks, 1, "{label}");
        assert_eq!(page.stats.diagnostics.missing_glyph_outlines, 0, "{label}");
        assert!(page.is_complete(), "{label}");

        // 2. Remapped ZapfDingbats where byte 0x21 is overridden via /Differences
        //    to `/a2` (NOT its canonical `/a1` slot): must NOT hit U+F021!
        let remapped_pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] \
              /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
                .to_vec(),
            stream("", b"0 0 0 rg BT /F1 24 Tf 15 40 Td <21> Tj ET"),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /ZapfDingbats \
              /Encoding << /Type /Encoding /BaseEncoding /ZapfDingbats /Differences [33 /a2] >> \
              /FontDescriptor 6 0 R >>"
                .to_vec(),
            b"<< /Type /FontDescriptor /FontName /ZapfDingbats /Flags 4 /FontFile 7 0 R >>"
                .to_vec(),
            stream("", b"corrupt zapf dingbats font"),
        ]);
        let remapped_doc = PdfDocument::open(remapped_pdf)
            .unwrap()
            .with_font_provider(provider);
        let remapped_page = zpdf::interpret_page(&remapped_doc, 0, Default::default()).unwrap();
        let (_, rfont) = remapped_page.fonts.get_by_name("F1").unwrap();
        assert_eq!(
            rfont.code_to_gid(0x21),
            None,
            "{label}: remapped 0x21 -> /a2 must not wrongly hit canonical U+F021"
        );
        assert_eq!(
            remapped_page.stats.diagnostics.missing_glyph_outlines, 1,
            "{label}"
        );
        assert!(!remapped_page.is_complete(), "{label}");
    }
}

#[test]
fn type1_custom_g3_recovers_after_256_other_embedded_fonts_on_same_page() {
    let pfa = build_type1_empty_custom_g3_pfa();
    let to_unicode = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
        1 begincodespacerange <00> <FF> endcodespacerange \
        3 beginbfchar <01> <0041> <02> <0020> <42> <0042> endbfchar \
        endcmap CMapName currentdict /CMap defineresource pop end end";

    // Create 256 distinct resource names `/F0`..`/F255` (occupying all 256
    // `EmbeddedGlyphHealth` cache slots) plus a 257th embedded Type 1 font
    // `/FG3` whose empty custom `/g3` is mapped to 'A' by `/ToUnicode`.
    let mut font_res = String::from("<< ");
    let mut content = String::from("0 0 0 rg BT ");
    for i in 0..256 {
        use std::fmt::Write;
        let _ = write!(&mut font_res, "/F{i} 5 0 R ");
        let _ = write!(&mut content, "/F{i} 10 Tf 10 80 Td (B) Tj ");
    }
    font_res.push_str("/FG3 8 0 R >>");
    content.push_str("/FG3 24 Tf 15 40 Td <01> Tj (B) Tj ET");

    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] \
              /Resources << /Font {font_res} >> /Contents 4 0 R >>"
        )
        .into_bytes(),
        stream("", content.as_bytes()),
        // Object 5: healthy embedded filler font referenced under 256 distinct FontIds.
        b"<< /Type /Font /Subtype /Type1 /BaseFont /ABCDEF+FillerType1 \
          /Encoding /WinAnsiEncoding /FontDescriptor 6 0 R >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+FillerType1 /Flags 32 /FontFile 7 0 R >>"
            .to_vec(),
        stream("", &pfa),
        // Object 8: 257th embedded font (no EmbeddedGlyphHealth cache slot).
        b"<< /Type /Font /Subtype /Type1 /BaseFont /GHIJKL+CustomG3Type1 \
          /Encoding << /Type /Encoding /BaseEncoding /WinAnsiEncoding /Differences [1 /g3 /g3] >> \
          /ToUnicode 10 0 R /FontDescriptor 9 0 R >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /GHIJKL+CustomG3Type1 /Flags 32 /FontFile 7 0 R >>"
            .to_vec(),
        stream("", to_unicode),
    ]);

    let provider = Arc::new(FontProvider::memory_only());
    provider
        .register_with_aliases(BOLD_TTF, 0, &["CustomG3Type1"])
        .unwrap();
    let doc = PdfDocument::open(pdf.clone())
        .unwrap()
        .with_font_provider(provider);
    let page = zpdf::interpret_page(&doc, 0, Default::default()).unwrap();
    let (fid_g3, fg3) = page.fonts.get_by_name("FG3").unwrap();
    assert!(
        page.fonts.len() >= 257,
        "page must have >256 embedded fonts so FG3 misses EmbeddedGlyphHealth cache"
    );
    assert!(!fg3.is_substitute, "embedded Type 1 stays primary");
    assert!(
        page.fonts.has_admitted_embedded_companion(fid_g3),
        "257th font's blank /g3 -> 'A' must still admit BOLD_TTF on cache miss"
    );
    assert_eq!(page.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(page.stats.diagnostics.missing_glyph_outlines, 0);
    assert!(page.is_complete());

    // Without a substitute, the 257th font's blank `/g3` -> 'A' must still
    // report embedded_font_fallbacks=1 and missing_glyph_outlines=1.
    let no_sub_doc = PdfDocument::open(pdf)
        .unwrap()
        .with_font_provider(Arc::new(FontProvider::memory_only()));
    let no_sub_page = zpdf::interpret_page(&no_sub_doc, 0, Default::default()).unwrap();
    assert_eq!(no_sub_page.stats.diagnostics.embedded_font_fallbacks, 1);
    assert_eq!(no_sub_page.stats.diagnostics.missing_glyph_outlines, 1);
    assert!(!no_sub_page.is_complete());
}

#[test]
fn promoting_higher_id_companion_releases_own_lookup_name_before_reclaiming_lower_id() {
    let probe = zpdf_font::LoadedFont::new_with_data(
        zpdf_font::PdfFontType::TrueType,
        "Probe".into(),
        PRIMARY_TTF.to_vec(),
        zpdf_font::CidWidths::new(1000.0),
    );
    let gid_a = probe.unicode_glyph('A' as u32).unwrap().0;
    let partial_ttf = corrupt_selected_gids(PRIMARY_TTF, &[gid_a]);

    let name_f1 = "LowerIdFaceNameA";
    let name_f2 = "HigherIdFaceNameB";
    let provider = Arc::new(FontProvider::memory_only());
    provider
        .register_with_aliases(PRIMARY_TTF, 0, &[name_f1])
        .unwrap();
    provider
        .register_with_aliases(BOLD_TTF, 0, &[name_f2])
        .unwrap();

    let make_primary = || {
        let mut font = zpdf_font::LoadedFont::new_with_data(
            zpdf_font::PdfFontType::TrueType,
            "Unknown".into(),
            partial_ttf.clone(),
            zpdf_font::CidWidths::new(1000.0),
        );
        font.encoding = Some(zpdf_font::encoding::Encoding::win_ansi());
        Arc::new(font)
    };
    let make_spec = |name: &str| {
        Arc::new(zpdf_font::DescriptorSubstituteSpec {
            lookup_name: Some(Arc::<str>::from(name)),
            hints: zpdf_font::system::SubstituteHints::default(),
            ordering: None,
            font_file_id: None,
        })
    };

    // Measure the exact incremental cost of admitting F2's descriptor substitute.
    let sub_f2_cost = {
        let mut measure = zpdf::FontCache::new().with_provider(Arc::clone(&provider));
        let fid = measure.insert_shared("M".into(), make_primary());
        measure.record_descriptor_substitute(fid, make_spec(name_f2), u64::MAX);
        let before = measure.bytes_used() - name_f2.len() as u64;
        let resolved = measure
            .resolve_embedded_companion_glyph(
                zpdf_font::DrawingGlyph {
                    font_id: fid,
                    glyph_id: gid_a,
                },
                65,
                65,
                1,
                u64::MAX,
            )
            .expect("measurement substitute must resolve");
        assert_eq!(
            measure
                .get(resolved.font_id)
                .and_then(|f| f.font_data.as_deref()),
            Some(BOLD_TTF)
        );
        measure.bytes_used() - before
    };

    // Build cache with lower-ID F1 (`name_f1`) and higher-ID F2 (`name_f2`).
    // Budget fits BOTH primaries + F2's substitute + F1's `name_f1`, but NOT
    // F2's own `name_f2` on top of F2's substitute.
    let f1 = make_primary();
    let f2 = make_primary();
    let base = f1.estimated_cache_bytes() + f2.estimated_cache_bytes();
    let budget = base + sub_f2_cost + name_f1.len() as u64;
    assert!(
        budget >= base + name_f1.len() as u64 + name_f2.len() as u64,
        "initial cache must retain both lookup names"
    );

    let mut cache = zpdf::FontCache::new().with_provider(Arc::clone(&provider));
    let fid1 = cache
        .admit_with_diagnostics("F1".into(), f1, Default::default(), None, budget)
        .unwrap();
    cache.record_descriptor_substitute(fid1, make_spec(name_f1), budget);
    let fid2 = cache
        .admit_with_diagnostics("F2".into(), f2, Default::default(), None, budget)
        .unwrap();
    cache.record_descriptor_substitute(fid2, make_spec(name_f2), budget);
    assert!(fid1 < fid2);
    assert_eq!(
        cache.bytes_used(),
        base + name_f1.len() as u64 + name_f2.len() as u64
    );

    // 1. Failed admission under a tight budget that cannot fit BOLD_TTF must
    //    restore F2's lookup name intact.
    let tight = base + name_f1.len() as u64 + name_f2.len() as u64;
    assert!(tight < base + sub_f2_cost);
    assert!(cache
        .resolve_embedded_companion_glyph(
            zpdf_font::DrawingGlyph {
                font_id: fid2,
                glyph_id: gid_a,
            },
            65,
            65,
            1,
            tight,
        )
        .is_none());
    assert_eq!(
        cache.bytes_used(),
        base + name_f1.len() as u64 + name_f2.len() as u64,
        "failed admission must restore F2's own descriptor_spec intact"
    );

    // 2. Promoting higher-ID F2 under `budget` releases F2's own `name_f2`
    //    first, so lower-ID F1 keeps `name_f1` intact (`bytes_used == budget`).
    let f2_resolved = cache
        .resolve_embedded_companion_glyph(
            zpdf_font::DrawingGlyph {
                font_id: fid2,
                glyph_id: gid_a,
            },
            65,
            65,
            1,
            budget,
        )
        .expect("F2 must promote BOLD_TTF under budget");
    assert_eq!(
        cache
            .get(f2_resolved.font_id)
            .and_then(|f| f.font_data.as_deref()),
        Some(BOLD_TTF)
    );
    assert_eq!(
        cache.bytes_used(),
        budget,
        "F1's lookup name must remain charged in standby rather than stripped"
    );

    // 3. Verify lower-ID F1 still resolves `name_f1` -> PRIMARY_TTF (would fail
    //    to match `"Unknown"` if F1's lookup name had been stripped).
    let f1_resolved = cache
        .resolve_embedded_companion_glyph(
            zpdf_font::DrawingGlyph {
                font_id: fid1,
                glyph_id: gid_a,
            },
            65,
            65,
            1,
            u64::MAX,
        )
        .expect("F1 must retain its descriptor /FontName override and match PRIMARY_TTF");
    assert_eq!(
        cache
            .get(f1_resolved.font_id)
            .and_then(|f| f.font_data.as_deref()),
        Some(PRIMARY_TTF)
    );
}

#[test]
fn many_shared_aliases_and_distinct_specs_account_and_reclaim_exact_bytes() {
    let shared_font = Arc::new(zpdf_font::LoadedFont::new_with_data(
        zpdf_font::PdfFontType::TrueType,
        "Unknown".into(),
        PRIMARY_TTF.to_vec(),
        zpdf_font::CidWidths::new(1000.0),
    ));
    let shared_name = "SharedAliasOverride".to_owned() + &"S".repeat(4096);
    let shared_spec = Arc::new(zpdf_font::DescriptorSubstituteSpec {
        lookup_name: Some(Arc::<str>::from(shared_name.as_str())),
        hints: zpdf_font::system::SubstituteHints::default(),
        ordering: None,
        font_file_id: None,
    });
    let base_bytes = shared_font.estimated_cache_bytes();
    let spec_bytes = shared_name.len() as u64;
    let budget = base_bytes + spec_bytes + 512;

    let mut cache = zpdf::FontCache::new();
    for i in 0..512 {
        let fid = cache
            .admit_with_diagnostics(
                format!("Alias{i}"),
                Arc::clone(&shared_font),
                Default::default(),
                None,
                budget,
            )
            .unwrap();
        cache.record_descriptor_substitute(fid, Arc::clone(&shared_spec), budget);
    }
    assert_eq!(cache.len(), 512);
    assert_eq!(
        cache.bytes_used(),
        base_bytes + spec_bytes,
        "512 shared aliases must charge the shared Arc spec exactly once"
    );

    // Admitting a second primary program that needs the shared spec's space
    // must reclaim all 512 aliases' shared spec in one pass and keep the new
    // primary fully drawable.
    let second_primary = Arc::new(zpdf_font::LoadedFont::new_with_data(
        zpdf_font::PdfFontType::TrueType,
        "SecondPrimary".into(),
        BOLD_TTF.to_vec(),
        zpdf_font::CidWidths::new(1000.0),
    ));
    let tight_budget = base_bytes + second_primary.estimated_cache_bytes();
    let fid_second = cache
        .admit_with_diagnostics(
            "FSecond".into(),
            second_primary,
            Default::default(),
            None,
            tight_budget,
        )
        .unwrap();
    assert!(cache.get(fid_second).unwrap().has_font_data());
    assert_eq!(cache.bytes_used(), tight_budget);
}
