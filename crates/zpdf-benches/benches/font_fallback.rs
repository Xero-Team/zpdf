//! Mixed-face recovery, placement and rendering. Original font/provider targets
//! remain unchanged regression controls, including registered simple-font pages.

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use std::hint::black_box;
use std::sync::Arc;
use std::time::Duration;
use zpdf::{ContentInterpreter, PdfDocument, RenderBackend};
use zpdf_benches::{font_fallback_workloads as fixtures, font_workloads};

fn fallback(c: &mut Criterion) {
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
    group.bench_function("vertical", |b| {
        b.iter(|| black_box(font_workloads::setup(&vertical, 144.0)))
    });
    let clipped = fixtures::document(false, 7, false, 16);
    group.bench_function("text_clip", |b| {
        b.iter(|| black_box(font_workloads::setup(&clipped, 144.0)))
    });
    let form = fixtures::document(false, 0, true, 16);
    group.bench_function("form", |b| {
        b.iter(|| black_box(font_workloads::setup(&form, 144.0)))
    });
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(2)).sample_size(50);
    targets = fallback
}
criterion_main!(benches);
