//! Multi-page throughput and the parallel-scaling curve.
//!
//! The performance notes list "batch / multi-page throughput" as a primary goal
//! but every optimization that round considered was single-threaded — the
//! workspace contains no threading at all. This target measures the ceiling that
//! page-level parallelism could reach, which is a prerequisite for deciding
//! whether to build it.
//!
//! ## Why each task opens its own document
//!
//! `PdfFile`'s object cache is a `RefCell` (`zpdf-parser/src/lib.rs` documents it
//! as `!Sync`, with a note to swap in a `Mutex` if that ever changes), so a
//! parsed document cannot be shared across threads today. There are two ways to
//! get page-level parallelism:
//!
//! 1. **Share the bytes, not the caches** — `Arc<[u8]>` is already `Send + Sync`,
//!    and the mutable state per document is thin. A shallow-cloned document with
//!    per-thread caches would avoid both the lock and the re-parse.
//! 2. **Re-open per thread** (what this bench does) — no library change, but each
//!    task pays its own parse.
//!
//! This bench measures (2), i.e. the *lower bound* on what parallelism can buy,
//! without touching a library crate. If the ceiling is not worth having even with
//! the re-parse included, the question is settled. If it is, (1) is the
//! production shape to evaluate next.
//!
//! Measured in **pages/sec**, so the curve reads as a speedup directly.
//!
//! Run:
//!   cargo bench -p zpdf-benches --bench batch
//!   ZPDF_BENCH_PAGES=16 cargo bench -p zpdf-benches --bench batch

use std::path::Path;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use rayon::prelude::*;
use zpdf::RenderBackend;

use zpdf_benches::{batch_corpus, load_page, DocRef};

mod common;
use common::apply_time_budget;

/// DPI for the batch target — one point; `stages` owns the DPI axis.
const DPI: u32 = 150;

/// Thread counts for the scaling curve. The development machine has 20
/// physical cores, so 16 is the top of the useful range and the curve should
/// show where it stops scaling.
const THREADS: &[usize] = &[1, 2, 4, 8, 16];

/// Pages rendered per benchmark iteration, overridable with `ZPDF_BENCH_PAGES`.
fn pages_per_case() -> usize {
    std::env::var("ZPDF_BENCH_PAGES")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(8)
}

/// One page's whole pipeline: open, interpret, render. Returns a byte count so
/// the result is `Send` and cannot be optimised away.
///
/// Deliberately includes the per-task parse: that is the cost of doing this
/// without a library change, so leaving it out would flatter the ceiling.
fn render_one(path: &Path, page: usize, dpi: f32) -> usize {
    let setup = load_page(path, page, dpi).expect("load_page");
    let mut renderer = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&setup.font_cache)
        .with_images(&setup.image_cache);
    renderer
        .render_display_list(&setup.dl, setup.scale)
        .expect("render")
        .data
        .len()
}

/// Round-robin `(doc, page)` work items across the batch documents.
fn work_items(docs: &[DocRef], count: usize) -> Vec<(&Path, usize)> {
    (0..count)
        .map(|i| {
            let doc = &docs[i % docs.len()];
            // Page index within the document; keep to what the manifest says
            // exists so a short document cannot be indexed out of range.
            let page = (i / docs.len()) % doc.pages.max(1) as usize;
            (doc.path.as_path(), page)
        })
        .collect()
}

/// Same pages, one thread — the baseline the curve is read against.
fn sequential(c: &mut Criterion) {
    let docs = batch_corpus();
    if docs.is_empty() {
        eprintln!(
            "zpdf-benches: no multi-page documents in the corpus — skipping batch/sequential"
        );
        return;
    }
    let count = pages_per_case();
    let items = work_items(&docs, count);
    let mut group = c.benchmark_group("batch/sequential");
    apply_time_budget(&mut group);
    group.throughput(Throughput::Elements(count as u64));
    group.bench_function(BenchmarkId::from_parameter(format!("{count}_pages")), |b| {
        b.iter(|| {
            items
                .iter()
                .map(|(path, page)| render_one(path, *page, DPI as f32))
                .sum::<usize>()
        })
    });
    group.finish();
}

/// The scaling curve: the same work at 1/2/4/8/16 threads.
fn parallel_scaling(c: &mut Criterion) {
    let docs = batch_corpus();
    if docs.is_empty() {
        eprintln!("zpdf-benches: no multi-page documents in the corpus — skipping batch/parallel");
        return;
    }
    let count = pages_per_case();
    let items = work_items(&docs, count);

    let mut group = c.benchmark_group("batch/parallel");
    apply_time_budget(&mut group);
    group.throughput(Throughput::Elements(count as u64));
    for &threads in THREADS {
        // Build the pool outside the timed region: pool creation is setup, not
        // the thing under measurement.
        let pool = match rayon::ThreadPoolBuilder::new().num_threads(threads).build() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("zpdf-benches: cannot build a {threads}-thread pool ({e}) — skipping");
                continue;
            }
        };
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{threads}_threads_{count}_pages")),
            &threads,
            |b, _| {
                b.iter(|| {
                    pool.install(|| {
                        items
                            .par_iter()
                            .map(|(path, page)| render_one(path, *page, DPI as f32))
                            .sum::<usize>()
                    })
                })
            },
        );
    }
    group.finish();
}

criterion_group!(batch, sequential, parallel_scaling);
criterion_main!(batch);
