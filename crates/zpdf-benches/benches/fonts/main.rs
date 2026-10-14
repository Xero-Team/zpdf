//! Font benchmarks: loading, mapping and outlines, the application font
//! provider, coverage selection and predefined CMaps. See
//! `docs/performance/fonts.md` for the cases and how to compare runs.

use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion};

mod base;
mod cmaps;
mod coverage;
mod provider;

criterion_group! {
    name = benches;
    config = Criterion::default().warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(2)).sample_size(50);
    targets = base::loading, base::mapping, base::outlines, base::pipeline,
        base::type1_variable, provider::registration, provider::integration,
        coverage::coverage, cmaps::cmaps
}
criterion_main!(benches);
