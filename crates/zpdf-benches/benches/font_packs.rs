//! Cold decode, warm reuse and real headless CJK acquisition recovery.

use std::hint::black_box;
use std::sync::Arc;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use zpdf::{FontPack, FontPackFormat, FontPackLimits, FontProvider, PdfDocument, RenderBackend};
use zpdf_benches::{font_pack_workloads as fixtures, font_workloads};
use zpdf_font::system::SubstituteHints;

fn decoding(c: &mut Criterion) {
    let encoded = fixtures::compress(fixtures::CJK);
    let limit = fixtures::CJK.len() as u64;
    let warm = fixtures::pack(Arc::clone(&encoded), limit);
    assert_eq!(warm.decode().unwrap().as_ref(), fixtures::CJK);
    let raw = FontPack::new(
        fixtures::CJK,
        FontPackFormat::Raw,
        FontPackLimits::default(),
    )
    .unwrap();
    raw.decode().unwrap();
    let invalid: Arc<[u8]> = Arc::from(&b"invalid"[..]);
    let failed = fixtures::pack(Arc::clone(&invalid), limit);
    assert!(failed.decode().is_err());
    let mut group = c.benchmark_group("font_packs/decode");
    group.bench_function("zlib_cold", |b| {
        b.iter_batched(
            || fixtures::pack(Arc::clone(&encoded), limit),
            |pack| black_box(pack.decode().unwrap()),
            BatchSize::SmallInput,
        )
    });
    group.bench_function("zlib_warm", |b| {
        b.iter(|| black_box(warm.decode().unwrap()))
    });
    group.bench_function("raw_warm", |b| b.iter(|| black_box(raw.decode().unwrap())));
    group.bench_function("invalid_cold", |b| {
        b.iter_batched(
            || fixtures::pack(Arc::clone(&invalid), limit),
            |pack| black_box(pack.decode().unwrap_err()),
            BatchSize::SmallInput,
        )
    });
    group.bench_function("invalid_warm", |b| {
        b.iter(|| black_box(failed.decode().unwrap_err()))
    });
    group.bench_function("over_limit", |b| {
        b.iter_batched(
            || fixtures::pack(Arc::clone(&encoded), limit - 1),
            |pack| black_box(pack.decode().unwrap_err()),
            BatchSize::SmallInput,
        )
    });
    group.finish();
}

fn selection(c: &mut Criterion) {
    let encoded = fixtures::compress(fixtures::CJK);
    let provider = fixtures::provider(Arc::clone(&encoded));
    assert!(provider
        .find_font(fixtures::ALIAS, SubstituteHints::default(), None)
        .is_some());
    let irrelevant = fixtures::provider(Arc::clone(&encoded));
    let mut group = c.benchmark_group("font_packs/provider");
    group.bench_function("register_lazy", |b| {
        b.iter(|| black_box(fixtures::provider(Arc::clone(&encoded))))
    });
    group.bench_function("select_cold", |b| {
        b.iter_batched(
            || fixtures::provider(Arc::clone(&encoded)),
            |provider| {
                black_box(
                    provider
                        .find_font(fixtures::ALIAS, SubstituteHints::default(), None)
                        .unwrap(),
                )
            },
            BatchSize::SmallInput,
        )
    });
    group.bench_function("select_warm", |b| {
        b.iter(|| {
            black_box(
                provider
                    .find_font(fixtures::ALIAS, SubstituteHints::default(), None)
                    .unwrap(),
            )
        })
    });
    group.bench_function("unrelated_miss", |b| {
        b.iter(|| {
            black_box(irrelevant.find_font("UnrelatedFace", SubstituteHints::default(), None))
        })
    });
    assert_eq!(irrelevant.bytes_used(), 0);
    group.finish();
}

fn integration(c: &mut Criterion) {
    let encoded = fixtures::compress(fixtures::CJK);
    let pdf = fixtures::pdf();
    let doc = PdfDocument::open(Arc::clone(&pdf))
        .unwrap()
        .with_font_provider(fixtures::provider(Arc::clone(&encoded)));
    let setup = fixtures::assert_cjk(&doc);
    let mut group = c.benchmark_group("font_packs/document");
    group.bench_function("real_cjk_cold", |b| {
        b.iter_batched(
            || {
                PdfDocument::open(Arc::clone(&pdf))
                    .unwrap()
                    .with_font_provider(fixtures::provider(Arc::clone(&encoded)))
            },
            |doc| black_box(font_workloads::setup(&doc, 144.0)),
            BatchSize::SmallInput,
        )
    });
    group.bench_function("real_cjk_warm", |b| {
        b.iter(|| black_box(font_workloads::setup(&doc, 144.0)))
    });
    group.bench_function("real_cjk_render", |b| {
        b.iter(|| {
            black_box(
                zpdf::cpu::CpuRenderer::new()
                    .with_fonts(&setup.font_cache)
                    .render_display_list(&setup.dl, setup.scale)
                    .unwrap(),
            )
        })
    });
    group.bench_function("real_cjk_late", |b| {
        b.iter_batched(
            || {
                let provider = Arc::new(FontProvider::memory_only());
                let doc = PdfDocument::open(Arc::clone(&pdf))
                    .unwrap()
                    .with_font_provider(Arc::clone(&provider));
                black_box(font_workloads::setup(&doc, 144.0));
                (doc, provider)
            },
            |(doc, provider)| {
                provider
                    .register_pack(
                        fixtures::pack(Arc::clone(&encoded), fixtures::CJK.len() as u64),
                        0,
                        &[fixtures::ALIAS],
                    )
                    .unwrap();
                black_box(font_workloads::setup(&doc, 144.0))
            },
            BatchSize::SmallInput,
        )
    });
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(2)).sample_size(50);
    targets = decoding, selection, integration
}
criterion_main!(benches);
