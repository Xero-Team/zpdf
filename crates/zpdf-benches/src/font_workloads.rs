//! Portable font workloads shared by correctness gates and Criterion.
//!
//! Generated geometric fonts isolate coverage and placement. The existing
//! Quartz CFF and variable TrueType fixtures cover real charstrings and axis
//! changes. These controls do not replace the optional full-document corpus.

use std::sync::Arc;

use zpdf::{ContentInterpreter, PdfDocument, RenderBackend};
use zpdf_font::{CidWidths, LoadedFont, PdfFontType};
use zpdf_writer::DocumentBuilder;

use crate::pipeline::{interpret_page, PageSetup};

pub const PRIMARY: &[u8] = include_bytes!("../../zpdf-font/tests/fixtures/coverage-primary.ttf");
pub const FALLBACK: &[u8] = include_bytes!("../../zpdf-font/tests/fixtures/coverage-fallback.ttf");
pub const VARIABLE: &[u8] = include_bytes!("../../zpdf-font/tests/fixtures/var.ttf");
pub const QUARTZ: &[u8] = include_bytes!("../../zpdf/tests/fixtures/quartz_cff_subset.pdf");

pub fn primary_font() -> LoadedFont {
    LoadedFont::new_with_data(
        PdfFontType::TrueType,
        "ZpdfCoveragePrimary".into(),
        PRIMARY.to_vec(),
        CidWidths::new(1000.0),
    )
}

pub fn quartz_font() -> LoadedFont {
    let doc = PdfDocument::open(QUARTZ).expect("Quartz PDF");
    zpdf_document::font_loader::load_single_font(doc.file(), zpdf::ObjectId(4, 0))
        .expect("Quartz CFF font")
}

/// Fixed, visible workloads; each contains 256 non-space glyphs.
pub fn text_cases() -> Vec<(&'static str, String, bool)> {
    vec![
        ("latin_repeated", "A".repeat(256), false),
        (
            "cjk_unique",
            (0x4E00..0x4F00)
                .map(|u| char::from_u32(u).unwrap())
                .collect(),
            true,
        ),
        (
            "mixed_supplementary",
            "A中\u{20000}\u{2000f}".repeat(64),
            true,
        ),
    ]
}

/// Build outside timed read/render stages; writer timing calls this explicitly.
pub fn make_pdf(text: &str, composite: bool, pages: usize) -> Arc<[u8]> {
    let mut builder = DocumentBuilder::new();
    let handle = if composite {
        builder.embed_composite_font(PRIMARY.to_vec())
    } else {
        builder.embed_font(PRIMARY.to_vec())
    }
    .expect("embed fixture");
    let chars: Vec<char> = text.chars().collect();
    for _ in 0..pages {
        let page = builder.add_page(240.0, 280.0);
        for (line, chunk) in chars.chunks(16).enumerate() {
            builder
                .add_text_embedded(
                    page,
                    &chunk.iter().collect::<String>(),
                    20.0,
                    260.0 - line as f64 * 15.0,
                    handle,
                    10.0,
                    (0.0, 0.0, 0.0),
                )
                .expect("add fixture text");
        }
    }
    Arc::from(builder.build().expect("build fixture PDF"))
}

pub fn setup(doc: &PdfDocument, dpi: f32) -> PageSetup {
    let page = doc.page(0).expect("fixture page");
    interpret_page(doc, &page, page.effective_box(), dpi)
        .expect("interpret fixture")
        .0
}

pub fn extract(doc: &PdfDocument) -> String {
    let page = doc.page(0).expect("fixture page");
    let mut fonts = doc.load_page_fonts(&page);
    let content = doc.page_content_bytes(&page).expect("fixture content");
    let mut spans = Vec::new();
    ContentInterpreter::new(page.effective_box())
        .with_fonts(&mut fonts)
        .with_text_sink(&mut spans)
        .interpret(&content);
    spans.into_iter().map(|span| span.text).collect()
}

/// Validate every glyph, its authoritative advance, extraction and ink before
/// timing. This must remain independent of whether a timed case becomes faster.
pub fn assert_text_case(doc: &PdfDocument, expected: &str, dpi: f32) {
    assert_eq!(extract(doc), expected, "fixture extraction changed");
    let setup = setup(doc, dpi);
    let original = primary_font();
    let mut expected_chars = expected.chars();
    let mut count = 0;
    for command in &setup.dl.commands {
        if let zpdf::display_list::RenderCommand::DrawGlyphRun(run) = command {
            let font = setup.font_cache.get(run.font_id).expect("run font");
            for (index, glyph) in run.glyphs.iter().enumerate() {
                let ch = expected_chars.next().expect("unexpected extra glyph");
                assert_eq!(
                    glyph.glyph_id,
                    original.unicode_glyph(ch as u32).unwrap().0,
                    "glyph identity changed for {ch:?}"
                );
                let outline = font.glyph_outline(glyph.glyph_id).expect("visible glyph");
                assert!(!outline.commands.is_empty(), "empty visible glyph");
                assert!((glyph.advance - 10.0).abs() < 0.001, "PDF advance changed");
                assert!(
                    (glyph.x - index as f32 * 10.0).abs() < 0.001,
                    "PDF position changed"
                );
                count += 1;
            }
        }
    }
    assert_eq!(count, expected.chars().count(), "glyphs were dropped");
    let mut renderer = zpdf::cpu::CpuRenderer::new().with_fonts(&setup.font_cache);
    let rendered = renderer
        .render_display_list(&setup.dl, setup.scale)
        .expect("render fixture");
    let scale = setup.scale;
    // Each synthetic glyph contains this interior point. Check every cell,
    // rather than allowing one correctly painted glyph to pass a whole page.
    for index in 0..count {
        let x = ((20.0 + (index % 16) as f32 * 10.0 + 1.5) * scale) as usize;
        let y = ((20.0 + (index / 16) as f32 * 15.0 - 2.0) * scale) as usize;
        let offset = (y * rendered.width as usize + x) * 4;
        assert!(rendered.data[offset] < 128, "missing ink in cell {index}");
    }
}

/// Exercise actual CFF outlines in a dense page without modifying the source
/// PDF fixture, which intentionally contains only resource dictionaries.
pub fn cff_setup(dpi: f32) -> PageSetup {
    let rect = zpdf::Rect::new(0.0, 0.0, 240.0, 280.0);
    let mut font_cache = zpdf::FontCache::new();
    font_cache.insert("F1".into(), quartz_font());
    let mut content = String::new();
    for row in 0..16 {
        content.push_str(&format!(
            "BT /F1 10 Tf 20 {} Td (UnqdUnqdUnqdUnqd) Tj ET\n",
            260 - row * 15
        ));
    }
    let mut spans = Vec::new();
    let dl = ContentInterpreter::new(rect)
        .with_fonts(&mut font_cache)
        .with_text_sink(&mut spans)
        .interpret(content.as_bytes());
    assert_eq!(
        spans.into_iter().map(|span| span.text).collect::<String>(),
        "Unqd".repeat(64)
    );
    let mut count = 0;
    for cmd in &dl.commands {
        if let zpdf::display_list::RenderCommand::DrawGlyphRun(run) = cmd {
            for (index, glyph) in run.glyphs.iter().enumerate() {
                assert_eq!(glyph.glyph_id, [26, 42, 45, 33][index % 4]);
                count += 1;
            }
        }
    }
    assert_eq!(count, 256);
    PageSetup {
        dl,
        font_cache,
        image_cache: zpdf::ImageCache::new(),
        scale: dpi / 72.0,
        setup_ns: 0,
    }
}
