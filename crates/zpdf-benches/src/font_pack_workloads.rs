//! Real CJK outlines supplied lazily to an otherwise fontless PDF.

use std::io::Write;
use std::sync::Arc;

use flate2::{write::ZlibEncoder, Compression};
use zpdf::{FontPack, FontPackFormat, FontPackLimits, FontProvider, PdfDocument, RenderBackend};

use crate::{font_workloads, PageSetup};

pub const CJK: &[u8] = include_bytes!("../../zpdf-font/tests/fixtures/noto-sans-sc-subset.ttf");
pub const TEXT: &str = "中文字体";
pub const ALIAS: &str = "ApplicationCjk";

pub fn compress(bytes: &[u8]) -> Arc<[u8]> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes).expect("compress font fixture");
    Arc::from(encoder.finish().expect("finish font fixture"))
}

pub fn pack(bytes: Arc<[u8]>, decoded_limit: u64) -> Arc<FontPack> {
    Arc::new(
        FontPack::new(
            bytes,
            FontPackFormat::Zlib,
            FontPackLimits {
                max_decoded_bytes: decoded_limit,
                ..Default::default()
            },
        )
        .expect("bounded font fixture"),
    )
}

pub fn provider(bytes: Arc<[u8]>) -> Arc<FontProvider> {
    let provider = Arc::new(FontProvider::memory_only());
    provider
        .register_pack(pack(bytes, CJK.len() as u64), 0, &[ALIAS])
        .unwrap();
    provider
}

/// CIDs deliberately differ from Unicode and font GIDs. PDF widths deliberately
/// differ from the fallback's advances, so incorrect substitutions are visible.
pub fn pdf() -> Arc<[u8]> {
    let content = "BT /F1 20 Tf 10 60 Td <002A002B002C002D> Tj ET";
    let cmap = "1 begincodespacerange <0000> <FFFF> endcodespacerange 4 beginbfchar <002A> <4E2D> <002B> <6587> <002C> <5B57> <002D> <4F53> endbfchar";
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".into(),
        format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
        format!("<< /Type /Font /Subtype /Type0 /BaseFont /{ALIAS} /Encoding /Identity-H /DescendantFonts [6 0 R] /ToUnicode 8 0 R >>"),
        format!("<< /Type /Font /Subtype /CIDFontType0 /BaseFont /{ALIAS} /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /FontDescriptor 7 0 R /DW 1000 /W [42 [750 900 600 1100]] >>"),
        format!("<< /Type /FontDescriptor /FontName /{ALIAS} /Flags 32 >>"),
        format!("<< /Length {} >>\nstream\n{cmap}\nendstream", cmap.len()),
    ];
    let mut bytes = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (index, body) in objects.iter().enumerate() {
        offsets.push(bytes.len());
        bytes.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", index + 1).as_bytes());
    }
    let start = bytes.len();
    bytes.extend_from_slice(b"xref\n0 9\n0000000000 65535 f \n");
    for offset in offsets {
        bytes.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    bytes.extend_from_slice(
        format!("trailer\n<< /Size 9 /Root 1 0 R >>\nstartxref\n{start}\n%%EOF\n").as_bytes(),
    );
    Arc::from(bytes)
}

pub fn assert_cjk(doc: &PdfDocument) -> PageSetup {
    assert_eq!(font_workloads::extract(doc), TEXT);
    let setup = font_workloads::setup(doc, 144.0);
    let face = ttf_parser::Face::parse(CJK, 0).unwrap();
    let expected: Vec<_> = TEXT.chars().collect();
    let advances = [15.0, 18.0, 12.0, 22.0];
    let positions = [0.0, 15.0, 33.0, 45.0];
    let mut count = 0;
    for command in &setup.dl.commands {
        if let zpdf::display_list::RenderCommand::DrawGlyphRun(run) = command {
            let font = setup.font_cache.get(run.font_id).unwrap();
            assert!(font.is_substitute);
            assert_eq!(font.font_data.as_deref().unwrap(), CJK);
            for glyph in &run.glyphs {
                let gid = face.glyph_index(expected[count]).unwrap().0;
                assert_ne!(gid, 0);
                assert_eq!(
                    font.cid_to_gid.as_ref().unwrap().get(&glyph.glyph_id),
                    Some(&gid)
                );
                let outline = font.glyph_outline(glyph.glyph_id).unwrap();
                assert!(outline.commands.len() > 10, "real CJK outline missing");
                assert!((glyph.advance - advances[count]).abs() < 0.001);
                assert!((glyph.x - positions[count]).abs() < 0.001);
                count += 1;
            }
        }
    }
    assert_eq!(count, 4);
    let image = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&setup.font_cache)
        .render_display_list(&setup.dl, setup.scale)
        .unwrap();
    for index in 0..4 {
        let x0 = ((10.0 + positions[index]) * setup.scale) as usize;
        let x1 = ((10.0 + positions[index] + advances[index]) * setup.scale) as usize;
        let ink = (76..126)
            .flat_map(|y| (x0..x1).map(move |x| (y * image.width as usize + x) * 4))
            .filter(|&offset| image.data[offset] < 128)
            .count();
        assert!(ink > 20, "CJK cell {index} has no readable ink");
    }
    setup
}
