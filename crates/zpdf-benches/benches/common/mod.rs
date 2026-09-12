//! Criterion helpers shared by the bench targets.
//!
//! These live here rather than in `src/lib.rs` because `criterion` is a
//! *dev*-dependency: benches compile with dev-dependencies, the library target
//! does not, so a helper that names a criterion type cannot live in the lib.
//!
//! Every bench target declares `mod common;`, which compiles this file into each
//! one — hence the blanket `dead_code` allow, since a given target uses only some
//! of these.
#![allow(dead_code)]

use criterion::{BenchmarkGroup, BenchmarkId, Criterion};
use std::time::Duration;

/// The DPI values the default matrix covers.
///
/// A DPI *curve* rather than one point, because it is the only axis that
/// separates fill-rate-bound work (raster cost grows with DPI²) from
/// geometry-bound work (glyph outline extraction and charstring interpretation
/// cost the same at every DPI). The previous round's conclusions about what was
/// "glyph-bound" all came from a single 150 DPI point, where the two are
/// indistinguishable.
pub const DPI_MATRIX: &[u32] = &[96, 150, 300];

/// At and above this DPI a case is sampled down to [`HIGH_DPI_SAMPLES`]: raster
/// area grows with the square of DPI, so a 300 DPI page is 4x the work of 150
/// DPI and criterion's default 100 samples would spend ~80s on the slowest page
/// alone.
pub const HIGH_DPI: u32 = 300;

/// Criterion sample count for [`HIGH_DPI`] cases.
pub const HIGH_DPI_SAMPLES: usize = 10;

/// Pick criterion's sample count from the measured cost of one operation.
///
/// The matrix deliberately spans a wide cost range — a text page renders in
/// ~10 ms, the image-bound page in ~400 ms — and one group-wide sample count
/// cannot serve both. Worse, criterion *extends* the measurement window when
/// samples are slow, so a "3 s" budget silently becomes 40 s on the expensive
/// cases (measured: 100 samples of one page took 39 s). Selecting from measured
/// cost keeps the default run inside its budget without under-sampling the cheap
/// cases.
pub fn samples_for(cost: Duration) -> usize {
    let ms = cost.as_secs_f64() * 1000.0;
    if ms >= 200.0 {
        10
    } else if ms >= 50.0 {
        30
    } else {
        100
    }
}

/// Time one call, to feed [`samples_for`].
pub fn time_once(mut op: impl FnMut()) -> Duration {
    let started = std::time::Instant::now();
    op();
    started.elapsed()
}

/// Apply the time budget and a cost-appropriate sample count to a group.
///
/// Call once per case, after `throughput` and before `bench_with_input`, passing
/// the cost of one operation measured in setup.
pub fn apply_cost_budget(
    group: &mut BenchmarkGroup<'_, criterion::measurement::WallTime>,
    cost: Duration,
) {
    apply_time_budget(group);
    group.sample_size(samples_for(cost));
}

/// [`apply_cost_budget`], measuring the cost from one probe call.
pub fn apply_cost_budget_probing<T>(
    group: &mut BenchmarkGroup<'_, criterion::measurement::WallTime>,
    mut probe: impl FnMut() -> T,
) {
    let started = std::time::Instant::now();
    std::hint::black_box(probe());
    apply_cost_budget(group, started.elapsed());
}

/// Apply the default per-case time budget to a group.
///
/// Criterion's defaults (3 s warm-up + 5 s measurement per case) would put the
/// default `cargo bench` well past the ~10-minute budget: the `stages` matrix
/// alone is pages × 3 DPIs × 5 groups. Trading samples for coverage is the right
/// call, because the question a stage split answers ("which stage dominates this
/// page?") needs many cases rather than extreme precision on a few. Raise it with
/// `-- --measurement-time 10` for a publication-quality run.
pub fn apply_time_budget(group: &mut BenchmarkGroup<'_, criterion::measurement::WallTime>) {
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));
}

/// `test8-p0-text@150` — label, measured class, and DPI, so a criterion report is
/// self-describing and a load class is never guessed from a filename again.
pub fn case_id(label: &str, class: Option<&str>, dpi: u32) -> String {
    match class {
        Some(c) => format!("{label}-{c}@{dpi}"),
        None => format!("{label}@{dpi}"),
    }
}

/// Same as [`case_id`] without the DPI, for single-DPI targets.
pub fn case_id_flat(label: &str, class: Option<&str>) -> String {
    match class {
        Some(c) => format!("{label}-{c}"),
        None => label.to_string(),
    }
}

/// Convenience for the common `BenchmarkId::from_parameter(case_id(..))` shape.
pub fn id(label: &str, class: Option<&str>, dpi: u32) -> BenchmarkId {
    BenchmarkId::from_parameter(case_id(label, class, dpi))
}

/// Silences an unused-import warning in targets that only use some helpers.
pub fn _unused(_: &Criterion) {}
