//! Coverage selection costs before integrating resolved drawing runs.

use criterion::{BatchSize, Criterion};
use std::hint::black_box;
use std::sync::Arc;
use zpdf_benches::{font_pack_workloads as packs, font_workloads as fixtures};
use zpdf_font::system::SubstituteHints;
use zpdf_font::{FontCharacter, FontProvider};

fn request(text: &str) -> FontCharacter {
    FontCharacter::from_text(text).unwrap()
}

fn provider() -> FontProvider {
    let provider = FontProvider::memory_only();
    provider.register(fixtures::PRIMARY, 0).unwrap();
    provider.register(fixtures::FALLBACK, 0).unwrap();
    provider
}

pub(crate) fn coverage(c: &mut Criterion) {
    let hints = SubstituteHints::default();
    let hit = request("😀");
    let miss = request("\u{9FFF}");
    let warm = provider();
    let found = warm.resolve_character(hit, hints).unwrap();
    assert_eq!(found.font.data.as_ref(), fixtures::FALLBACK);
    assert_ne!(found.glyph_id, 0);
    assert!(warm.resolve_character(miss, hints).is_none());
    let variation = request("中\u{E0100}");
    warm.register(
        include_bytes!("../../../zpdf-font/tests/fixtures/coverage-uvs.ttf").as_slice(),
        0,
    )
    .unwrap();
    assert_eq!(
        warm.resolve_character(variation, hints).unwrap().glyph_id,
        3
    );
    warm.resolve_character(hit, hints).unwrap();
    assert!(warm.resolve_character(miss, hints).is_none());
    let mixed: Vec<_> = [
        "A",
        "中",
        "\u{20000}",
        "\u{20010}",
        "😀",
        "\u{4F01}",
        " ",
        "\u{9FFF}",
    ]
    .into_iter()
    .cycle()
    .take(256)
    .map(request)
    .collect();
    assert_eq!(
        mixed
            .iter()
            .filter(|&&ch| warm.resolve_character(ch, hints).is_some())
            .count(),
        224
    );
    let mut group = c.benchmark_group("font_coverage/resolve");
    group.bench_function("cold_hit", |b| {
        b.iter_batched(
            provider,
            |provider| black_box(provider.resolve_character(hit, hints).unwrap()),
            BatchSize::SmallInput,
        )
    });
    group.bench_function("warm_hit", |b| {
        b.iter(|| black_box(warm.resolve_character(hit, hints).unwrap()))
    });
    group.bench_function("cold_miss", |b| {
        b.iter_batched(
            provider,
            |provider| black_box(provider.resolve_character(miss, hints)),
            BatchSize::SmallInput,
        )
    });
    group.bench_function("warm_miss", |b| {
        b.iter(|| black_box(warm.resolve_character(miss, hints)))
    });
    group.bench_function("mixed_256", |b| {
        b.iter(|| {
            for &ch in &mixed {
                black_box(warm.resolve_character(ch, hints));
            }
        })
    });
    group.bench_function("variation_warm", |b| {
        b.iter(|| black_box(warm.resolve_character(variation, hints).unwrap()))
    });
    group.bench_function("late_registration", |b| {
        b.iter_batched(
            || {
                let provider = FontProvider::memory_only();
                provider.register(fixtures::PRIMARY, 0).unwrap();
                assert!(provider.resolve_character(hit, hints).is_none());
                provider
            },
            |provider| {
                provider.register(fixtures::FALLBACK, 0).unwrap();
                black_box(provider.resolve_character(hit, hints).unwrap())
            },
            BatchSize::SmallInput,
        )
    });
    let encoded = packs::compress(fixtures::FALLBACK);
    group.bench_function("lazy_pack_cold", |b| {
        b.iter_batched(
            || {
                let provider = FontProvider::memory_only();
                provider
                    .register_pack(
                        packs::pack(Arc::clone(&encoded), fixtures::FALLBACK.len() as u64),
                        0,
                        &["Symbols"],
                    )
                    .unwrap();
                provider
            },
            |provider| black_box(provider.resolve_character(hit, hints).unwrap()),
            BatchSize::SmallInput,
        )
    });
    group.finish();
}
