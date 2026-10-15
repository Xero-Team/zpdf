//! Source-anchored cases with deliberately distinct code, Unicode, CID and GID.

use crate::{font_workloads, PageSetup};
use std::sync::Arc;
use zpdf::{FontProvider, PdfDocument, RenderBackend};

pub const FONT: &[u8] = include_bytes!("../../zpdf-font/tests/fixtures/coverage-cmap.ttf");
pub const CID_CFF: &[u8] = include_bytes!("../../zpdf-font/tests/fixtures/coverage-cid.otf");

#[derive(Clone, Copy)]
pub struct Case {
    pub name: &'static str,
    pub encoding: &'static str,
    pub ordering: &'static str,
    pub code: &'static [u8],
    pub cid: u16,
    pub character: char,
    pub vertical: bool,
}

// Values are anchors in the pinned Adobe source files, not runtime lookups.
pub const CASES: &[Case] = &[
    Case {
        name: "unicode_gb",
        encoding: "UniGB-UCS2-H",
        ordering: "GB1",
        code: &[0x4E, 0x2D],
        cid: 4559,
        character: '中',
        vertical: false,
    },
    Case {
        name: "utf16_gb",
        encoding: "UniGB-UTF16-H",
        ordering: "GB1",
        code: &[0xD8, 0x40, 0xDC, 0x87],
        cid: 22048,
        character: '\u{20087}',
        vertical: false,
    },
    Case {
        name: "gbk_four_byte",
        encoding: "GBK2K-H",
        ordering: "GB1",
        code: &[0x81, 0x30, 0x84, 0x36],
        cid: 22354,
        character: '¥',
        vertical: false,
    },
    Case {
        name: "hkscs",
        encoding: "HKscs-B5-H",
        ordering: "CNS1",
        code: &[0x89, 0x43],
        cid: 17718,
        character: '攊',
        vertical: false,
    },
    Case {
        name: "vertical_jis",
        encoding: "UniJIS-UCS2-V",
        ordering: "Japan1",
        code: &[0x30, 0x01],
        cid: 7887,
        character: '、',
        vertical: true,
    },
    Case {
        name: "vertical_gbk",
        encoding: "GBK2K-V",
        ordering: "GB1",
        code: &[0xD6, 0xD0],
        cid: 4559,
        character: '中',
        vertical: true,
    },
    Case {
        name: "identity_collection",
        encoding: "Identity-H",
        ordering: "GB1",
        code: &[0x11, 0xCF],
        cid: 4559,
        character: '中',
        vertical: false,
    },
];

fn stream(bytes: &[u8]) -> Vec<u8> {
    let mut out = format!("<< /Length {} >>\nstream\n", bytes.len()).into_bytes();
    out.extend_from_slice(bytes);
    out.extend_from_slice(b"\nendstream");
    out
}

pub fn pdf(case: Case, embedded: bool, repeat: usize) -> Arc<[u8]> {
    let face = ttf_parser::Face::parse(FONT, 0).unwrap();
    let gid = face.glyph_index(case.character).unwrap().0;
    let mut mapping = vec![0; (case.cid as usize + 1) * 2];
    mapping[case.cid as usize * 2..case.cid as usize * 2 + 2].copy_from_slice(&gid.to_be_bytes());
    let encoded: String = case.code.iter().map(|byte| format!("{byte:02X}")).collect();
    let mut content = String::new();
    for row in 0..repeat {
        let (x, y) = if case.vertical {
            (30 + row * 15, 260)
        } else {
            (20, 260 - row * 15)
        };
        content.push_str(&format!(
            "BT /F1 10 Tf {x} {y} Td <{encoded}{encoded}{encoded}> Tj ET\n"
        ));
    }
    let program = if embedded { "/FontFile2 8 0 R" } else { "" };
    let objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 240 280] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        stream(content.as_bytes()),
        format!("<< /Type /Font /Subtype /Type0 /BaseFont /CMapFixture /Encoding /{} /DescendantFonts [6 0 R] >>", case.encoding).into_bytes(),
        format!("<< /Type /Font /Subtype /CIDFontType2 /BaseFont /CMapFixture /CIDSystemInfo << /Registry (Adobe) /Ordering ({}) /Supplement 7 >> /FontDescriptor 7 0 R /CIDToGIDMap 9 0 R /DW 999 /W [{} [730]] /DW2 [880 -1000] /W2 [{} [-910 210 760]] >>", case.ordering, case.cid, case.cid).into_bytes(),
        format!("<< /Type /FontDescriptor /FontName /CMapFixture /Flags 4 {program} >>").into_bytes(),
        stream(FONT),
        stream(&mapping),
    ];
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (index, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    Arc::from(out)
}

pub fn document(case: Case, embedded: bool, repeat: usize) -> PdfDocument {
    let provider = Arc::new(FontProvider::memory_only());
    if !embedded {
        provider
            .register_with_aliases(FONT, 0, &["CMapFixture"])
            .unwrap();
    }
    PdfDocument::open(pdf(case, embedded, repeat))
        .unwrap()
        .with_font_provider(provider)
}

pub fn assert_case(doc: &PdfDocument, case: Case, embedded: bool, repeat: usize) -> PageSetup {
    assert_eq!(
        font_workloads::extract(doc),
        case.character.to_string().repeat(repeat * 3)
    );
    let setup = font_workloads::setup(doc, 144.0);
    let original = zpdf_font::LoadedFont::new_with_data(
        zpdf_font::PdfFontType::TrueType,
        "reference".into(),
        FONT.to_vec(),
        zpdf_font::CidWidths::new(1000.0),
    );
    let gid = original.unicode_glyph(case.character as u32).unwrap().0;
    let expected = original.glyph_outline(gid).unwrap();
    let mut count = 0;
    for command in &setup.dl.commands {
        if let zpdf::display_list::RenderCommand::DrawGlyphRun(run) = command {
            let font = setup.font_cache.get(run.font_id).unwrap();
            assert_eq!(font.is_substitute, !embedded);
            for (index, glyph) in run.glyphs.iter().enumerate() {
                assert_eq!(
                    format!("{:?}", font.glyph_outline(glyph.glyph_id).unwrap().commands),
                    format!("{:?}", expected.commands)
                );
                let (x, y, advance) = if case.vertical {
                    (-2.1, -7.6 - index as f32 * 9.1, -9.1)
                } else {
                    (index as f32 * 7.3, 0.0, 7.3)
                };
                assert!((glyph.x - x).abs() < 0.001, "{} x", case.name);
                assert!((glyph.y - y).abs() < 0.001, "{} y", case.name);
                assert!(
                    (glyph.advance - advance).abs() < 0.001,
                    "{} width",
                    case.name
                );
                count += 1;
            }
        }
    }
    assert_eq!(count, repeat * 3);
    let pixels = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&setup.font_cache)
        .render_display_list(&setup.dl, setup.scale)
        .unwrap();
    assert!(
        pixels
            .data
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[0] < 128)
            .count()
            > repeat * 20
    );
    setup
}
