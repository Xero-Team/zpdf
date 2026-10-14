//! Missing-glyph fallback benchmarks: mixed-face pages, writer authoring with
//! registered coverage, and lazy font packs. See `docs/performance/fonts.md`.

use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion};

mod packs;
mod page;
mod writer;

criterion_group! {
    name = benches;
    config = Criterion::default().warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(2)).sample_size(50);
    targets = page::fallback, writer::writer, packs::decoding, packs::selection,
        packs::integration
}
criterion_main!(benches);
