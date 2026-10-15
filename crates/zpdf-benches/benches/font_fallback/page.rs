//! Mixed-face recovery, placement and rendering. The `fonts` target's pipeline
//! cases remain the controls for pages that need no fallback.

use criterion::{BatchSize, Criterion};
use std::hint::black_box;
use std::sync::Arc;
use zpdf::display_list::RenderCommand;
use zpdf::{ContentInterpreter, PdfDocument, RenderBackend};
use zpdf_benches::{font_fallback_workloads as fixtures, font_workloads};

/// Extracted text including Form XObjects, which resolve only with the
/// document's resources.
fn extract(doc: &PdfDocument) -> String {
    let page = doc.page(0).unwrap();
    let mut fonts = doc.load_page_fonts(&page);
    let content = doc.page_content_bytes(&page).unwrap();
    let mut spans = Vec::new();
    ContentInterpreter::new(page.effective_box())
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_text_sink(&mut spans)
        .interpret(&content);
    spans.into_iter().map(|span| span.text).collect()
}

/// Layout-independent check for the vertical and Form variants: extraction is
/// intact and every glyph draws an outline except the space (index 4) and the
/// unrecoverable code (index 7).
fn assert_recovered(doc: &PdfDocument, repeat: usize) {
    assert_eq!(extract(doc), fixtures::TEXT.repeat(repeat));
    let setup = font_workloads::setup(doc, 144.0);
    let mut count = 0;
    for command in &setup.dl.commands {
        if let RenderCommand::DrawGlyphRun(run) = command {
            let font = setup.font_cache.get(run.font_id).unwrap();
            for glyph in &run.glyphs {
                assert_eq!(
                    font.glyph_outline(glyph.glyph_id).is_some(),
                    !matches!(count % 8, 4 | 7),
                    "glyph {count}"
                );
                count += 1;
            }
        }
    }
    assert_eq!(count, repeat * 8);
}

/// Clip-only text draws nothing but must install the faces' union as a clip.
fn assert_clipped(doc: &PdfDocument, repeat: usize) {
    assert_eq!(extract(doc), fixtures::TEXT.repeat(repeat));
    let commands = font_workloads::setup(doc, 144.0).dl.commands;
    assert!(!commands
        .iter()
        .any(|command| matches!(command, RenderCommand::DrawGlyphRun(_))));
    assert!(commands.iter().any(
        |command| matches!(command, RenderCommand::PushClip { path, .. } if !path.is_empty())
    ));
}

pub(crate) fn fallback(c: &mut Criterion) {
    let bytes = fixtures::pdf(false, 0, false, 32);
    let doc = PdfDocument::open(Arc::clone(&bytes))
        .unwrap()
        .with_font_provider(fixtures::provider());
    let setup = fixtures::assert_mixed(&doc, 32);
    let page = doc.page(0).unwrap();
    let content = doc.page_content_bytes(&page).unwrap();
    let mut warm_fonts = doc.load_page_fonts(&page);
    ContentInterpreter::new(page.effective_box())
        .with_fonts(&mut warm_fonts)
        .interpret(&content);
    let mut group = c.benchmark_group("font_fallback/pipeline");
    group.bench_function("document_cold", |b| {
        b.iter_batched(
            || {
                PdfDocument::open(Arc::clone(&bytes))
                    .unwrap()
                    .with_font_provider(fixtures::provider())
            },
            |doc| black_box(font_workloads::setup(&doc, 144.0)),
            BatchSize::SmallInput,
        )
    });
    group.bench_function("document_warm", |b| {
        b.iter(|| black_box(font_workloads::setup(&doc, 144.0)))
    });
    group.bench_function("page_cache_warm", |b| {
        b.iter(|| {
            black_box(
                ContentInterpreter::new(page.effective_box())
                    .with_fonts(&mut warm_fonts)
                    .interpret(&content),
            )
        })
    });
    group.bench_function("extract", |b| {
        b.iter(|| black_box(font_workloads::extract(&doc)))
    });
    group.bench_function("render_cpu", |b| {
        b.iter(|| {
            black_box(
                zpdf::cpu::CpuRenderer::new()
                    .with_fonts(&setup.font_cache)
                    .render_display_list(&setup.dl, setup.scale)
                    .unwrap(),
            )
        })
    });
    group.bench_function("export_svg", |b| {
        b.iter(|| {
            black_box(zpdf_svg_export::display_list_to_svg(
                &setup.dl,
                &setup.font_cache,
                &setup.image_cache,
                &Default::default(),
            ))
        })
    });
    let vertical = fixtures::document(true, 0, false, 8);
    assert_recovered(&vertical, 8);
    group.bench_function("vertical", |b| {
        b.iter(|| black_box(font_workloads::setup(&vertical, 144.0)))
    });
    let clipped = fixtures::document(false, 7, false, 16);
    assert_clipped(&clipped, 16);
    group.bench_function("text_clip", |b| {
        b.iter(|| black_box(font_workloads::setup(&clipped, 144.0)))
    });
    let form = fixtures::document(false, 0, true, 16);
    assert_recovered(&form, 16);
    group.bench_function("form", |b| {
        b.iter(|| black_box(font_workloads::setup(&form, 144.0)))
    });
    group.finish();
}
