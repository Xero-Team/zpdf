//! Mixed embedded/application font recovery with authoritative PDF placement.

use crate::{font_pack_workloads, font_workloads, PageSetup};
use std::sync::Arc;
use zpdf::{FontProvider, PdfDocument, RenderBackend};

pub const TEXT: &str = "A\u{4F01}\u{20010}😀 中文\u{9FFF}";
pub const WIDTHS: [f32; 8] = [700.0, 800.0, 900.0, 600.0, 300.0, 1100.0, 1000.0, 500.0];

fn stream(extra: &str, data: &[u8]) -> Vec<u8> {
    let mut output = format!("<< {extra} /Length {} >>\nstream\n", data.len()).into_bytes();
    output.extend_from_slice(data);
    output.extend_from_slice(b"\nendstream");
    output
}

pub fn provider() -> Arc<FontProvider> {
    let provider = Arc::new(FontProvider::memory_only());
    provider.register(font_workloads::FALLBACK, 0).unwrap();
    provider.register(font_pack_workloads::CJK, 0).unwrap();
    provider
}

pub fn pdf(vertical: bool, render_mode: u8, form: bool, repeat: usize) -> Arc<[u8]> {
    let height = repeat.max(16) * 15 + 40;
    let face = ttf_parser::Face::parse(font_workloads::PRIMARY, 0).unwrap();
    let mut mapping = vec![0u8; 18];
    let mut unicode =
        String::from("1 begincodespacerange <0000> <FFFF> endcodespacerange 8 beginbfchar\n");
    for (index, ch) in TEXT.chars().enumerate() {
        let cid = index + 1;
        let gid = face.glyph_index(ch).map_or(0, |gid| gid.0);
        mapping[cid * 2..cid * 2 + 2].copy_from_slice(&gid.to_be_bytes());
        let mut buf = [0u16; 2];
        let encoded: String = ch
            .encode_utf16(&mut buf)
            .iter()
            .map(|unit| format!("{unit:04X}"))
            .collect();
        unicode.push_str(&format!("<{cid:04X}> <{encoded}>\n"));
    }
    unicode.push_str("endbfchar");
    let mut content = String::from("q\n");
    for row in 0..repeat {
        let position = if vertical {
            format!("{} 250", 20 + row * 15)
        } else {
            format!("20 {}", height - 20 - row * 15)
        };
        content.push_str(&format!("BT /F1 10 Tf {render_mode} Tr {position} Td <00010002000300040005000600070008> Tj ET\n"));
    }
    if render_mode == 7 {
        content.push_str(&format!("0 0 240 {height} re f\n"));
    }
    content.push_str("Q\n");
    let encoding = if vertical { "Identity-V" } else { "Identity-H" };
    let font_resources = "/Resources << /Font << /F1 5 0 R >> >>";
    let resources = if form {
        "/Resources << /XObject << /Form 11 0 R >> >>"
    } else {
        font_resources
    };
    let mut objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 240 {height}] {resources} /Contents 4 0 R >>").into_bytes(),
        stream("", if form { b"/Form Do" } else { content.as_bytes() }),
        format!("<< /Type /Font /Subtype /Type0 /BaseFont /EmbeddedPrimary /Encoding /{encoding} /DescendantFonts [6 0 R] /ToUnicode 10 0 R >>").into_bytes(),
        b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /EmbeddedPrimary /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /FontDescriptor 7 0 R /CIDToGIDMap 9 0 R /DW 1000 /W [1 [700 800 900 600 300 1100 1000 500]] /DW2 [880 -1000] /W2 [1 [-900 200 700 -1100 400 800 -800 300 850 -1000 200 750 -300 150 880 -1200 550 880 -900 500 800 -700 250 850]] >>".to_vec(),
        b"<< /Type /FontDescriptor /FontName /EmbeddedPrimary /Flags 32 /FontFile2 8 0 R >>".to_vec(),
        stream("", font_workloads::PRIMARY),
        stream("", &mapping),
        stream("", unicode.as_bytes()),
    ];
    if form {
        objects.push(stream(
            &format!("/Type /XObject /Subtype /Form /BBox [0 0 240 {height}] {font_resources}"),
            content.as_bytes(),
        ));
    }
    let mut bytes = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (index, body) in objects.iter().enumerate() {
        offsets.push(bytes.len());
        bytes.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
        bytes.extend_from_slice(body);
        bytes.extend_from_slice(b"\nendobj\n");
    }
    let xref = bytes.len();
    bytes.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        bytes.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    bytes.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    Arc::from(bytes)
}

pub fn document(vertical: bool, render_mode: u8, form: bool, repeat: usize) -> PdfDocument {
    PdfDocument::open(pdf(vertical, render_mode, form, repeat))
        .unwrap()
        .with_font_provider(provider())
}

pub fn assert_mixed(doc: &PdfDocument, repeat: usize) -> PageSetup {
    assert_eq!(font_workloads::extract(doc), TEXT.repeat(repeat));
    let setup = font_workloads::setup(doc, 144.0);
    let expected: Vec<_> = TEXT.chars().collect();
    let mut count = 0;
    let mut x = 0.0;
    for command in &setup.dl.commands {
        if let zpdf::display_list::RenderCommand::DrawGlyphRun(run) = command {
            let font = setup.font_cache.get(run.font_id).unwrap();
            for glyph in &run.glyphs {
                let index = count % 8;
                if index == 0 {
                    x = 0.0;
                }
                assert!((glyph.x - x).abs() < 0.001);
                assert!((glyph.advance - WIDTHS[index] / 100.0).abs() < 0.001);
                x += WIDTHS[index] / 100.0;
                let bytes = match index {
                    0 | 4 | 5 | 7 => font_workloads::PRIMARY,
                    1..=3 => font_workloads::FALLBACK,
                    6 => font_pack_workloads::CJK,
                    _ => unreachable!(),
                };
                assert_eq!(font.font_data.as_deref().unwrap(), bytes);
                if index == 7 {
                    assert!(font.glyph_outline(glyph.glyph_id).is_none());
                } else if matches!(index, 0 | 4 | 5) {
                    assert_eq!(glyph.glyph_id, index as u16 + 1, "embedded CID retained");
                } else {
                    let face = ttf_parser::Face::parse(bytes, 0).unwrap();
                    assert_eq!(glyph.glyph_id, face.glyph_index(expected[index]).unwrap().0);
                    assert!(font.glyph_outline(glyph.glyph_id).is_some());
                }
                count += 1;
            }
        }
    }
    assert_eq!(count, repeat * 8);
    let image = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&setup.font_cache)
        .render_display_list(&setup.dl, setup.scale)
        .unwrap();
    for row in 0..repeat {
        let mut x = 20.0f32;
        for (index, width) in WIDTHS.into_iter().enumerate() {
            if !matches!(index, 4 | 7) {
                let x0 = (x * setup.scale) as usize;
                let x1 = ((x + width / 100.0) * setup.scale) as usize;
                let y0 = ((20 + row * 15 - 10) as f32 * setup.scale) as usize;
                let y1 = ((20 + row * 15 + 2) as f32 * setup.scale) as usize;
                let ink = (y0..y1)
                    .flat_map(|y| (x0..x1).map(move |x| (y * image.width as usize + x) * 4))
                    .filter(|&offset| image.data[offset] < 128)
                    .count();
                assert!(ink > 2, "missing ink for glyph {index} on row {row}");
            }
            x += width / 100.0;
        }
    }
    setup
}
