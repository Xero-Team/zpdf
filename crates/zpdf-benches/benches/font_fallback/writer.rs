//! Authoring with the same coverage decisions as the page renderer.

use criterion::Criterion;
use std::hint::black_box;
use zpdf_benches::{font_pack_workloads, font_workloads};
use zpdf_font::{system::SubstituteHints, FontProvider};
use zpdf_writer::DocumentBuilder;

fn author(provider: &FontProvider, text: &str) -> Vec<u8> {
    let mut builder = DocumentBuilder::new();
    let page = builder.add_page(1000.0, 100.0);
    builder
        .add_text_with_fonts(
            page,
            text,
            10.0,
            50.0,
            provider,
            SubstituteHints::default(),
            10.0,
            (0.0, 0.0, 0.0),
        )
        .unwrap();
    builder.build().unwrap()
}

pub(crate) fn writer(c: &mut Criterion) {
    let provider = FontProvider::memory_only();
    provider.register(font_workloads::PRIMARY, 0).unwrap();
    provider.register(font_workloads::FALLBACK, 0).unwrap();
    provider.register(font_pack_workloads::CJK, 0).unwrap();
    let mixed = "A\u{4F01}\u{20010}😀 文中A".repeat(8);
    let doc = zpdf::PdfDocument::open(author(&provider, &mixed)).unwrap();
    assert_eq!(font_workloads::extract(&doc), mixed);
    let single = "A".repeat(64);
    let doc = zpdf::PdfDocument::open(author(&provider, &single)).unwrap();
    assert_eq!(font_workloads::extract(&doc), single);
    let mut group = c.benchmark_group("font_writer_fallback");
    group.bench_function("mixed_64", |b| {
        b.iter(|| black_box(author(&provider, &mixed)))
    });
    group.bench_function("single_face_64", |b| {
        b.iter(|| black_box(author(&provider, &single)))
    });
    group.bench_function("repeated_miss", |b| {
        b.iter(|| {
            let mut builder = DocumentBuilder::new();
            let page = builder.add_page(100.0, 100.0);
            black_box(
                builder
                    .add_text_with_fonts(
                        page,
                        "A\u{9FFF}",
                        10.0,
                        50.0,
                        &provider,
                        SubstituteHints::default(),
                        10.0,
                        (0.0, 0.0, 0.0),
                    )
                    .unwrap_err(),
            )
        })
    });
    group.finish();
}
