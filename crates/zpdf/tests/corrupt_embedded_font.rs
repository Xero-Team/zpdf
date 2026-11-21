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
    //     still draw visibly at their PDF positions.
    if zpdf_font::system::find_system_font("Helvetica", Default::default(), None).is_some() {
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
        !page0.fonts.has_embedded_companion(f1_id),
        "unused companion exceeding page budget must not be retained"
    );
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
        "substitute CID 1 must still render when unused companion is dropped for budget"
    );

    // Page 1 (Form XObject font)
    let page1 = zpdf::interpret_page(&doc_tight, 1, Default::default()).unwrap();
    let (g1_id, g1) = page1.fonts.get_by_name("__font_6_0").unwrap();
    assert!(g1.is_substitute && g1.has_font_data());
    assert!(!page1.fonts.has_embedded_companion(g1_id));
    assert!(page1.fonts.bytes_used() <= tight_budget);
    assert!(page1.is_complete());

    // 2. Generous budget: companion IS retained and charged immediately in
    //    `bytes_used()` even though page 0 draws only from the substitute.
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
    assert!(page_ample.fonts.has_embedded_companion(f1_ample_id));
    assert!(
        page_ample.fonts.bytes_used() >= (PRIMARY_TTF.len() as u64) * 2,
        "retained companion must be charged against bytes_used() even when unused"
    );
    assert!(page_ample.fonts.bytes_used() <= ample_budget);

    // 3. Two-pass admission: measure exact primary costs for /F1 and /F2; when
    //    both primaries fit together (`f1_cost + f2_cost`) but /F1's companion
    //    does not, neither primary font is starved regardless of map order.
    let page2_ample = zpdf::interpret_page(&doc_ample, 2, Default::default()).unwrap();
    let f1_cost = f1_ample.estimated_cache_bytes();
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
    assert!(!page2.fonts.has_embedded_companion(f1_id2));
    assert!(page2.fonts.bytes_used() <= sibling_budget);
    assert!(!page2.stats.diagnostics.resource_limit_reached);
    assert!(page2.is_complete());
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
    let budget = (filler_cost + primary_cost).max(pair_cost);
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
            page.fonts.has_embedded_companion(fid),
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
}
