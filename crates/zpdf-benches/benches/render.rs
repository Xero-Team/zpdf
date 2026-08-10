//! Criterion benches for the zpdf render path.
//!
//! Three groups (all parameterized over the real-PDF corpus):
//!   - `cpu_render`     — render-only, CPU backend (M1 target).
//!   - `gpu_render`     — render-only, GPU backend, one reused `WgpuRenderer`
//!     so device init is amortized across iterations (M2 target: per-page
//!     resource allocation still happens inside `render_display_list` each
//!     iter — that overhead is exactly what M2 reduces).
//!   - `full_pipeline`  — open + interpret + render, CPU — real-world wall-clock.
//!
//! `render-only` isolates the backend: parse + interpret run once in setup
//! (unmeasured), and only `render_display_list` is timed. This is the cleanest
//! before/after signal for the CPU glyph cache (M1).
//!
//! Run:
//!   cargo bench -p zpdf-benches                              # cpu_render + full_pipeline
//!   cargo bench -p zpdf-benches --features gpu-render        # + gpu_render
//!   cargo bench -p zpdf-benches -- cpu_render -- --quick     # quick mode
//!
//! Throughput is reported in pixels/sec so different DPI/page sizes compare.

use std::path::Path;

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput};

use zpdf::RenderBackend;
use zpdf_benches::{existing_corpus, load_page, PageSetup};

/// DPI for the default end-to-end matrix. 150 ≈ the CLI `render` default-ish
/// for a fidelity check; high enough to make glyph-rasterization cost visible
/// (the M1 target) without ballooning large pages past the pixel budget.
const DPI: f32 = 150.0;

/// Render-only CPU: one `CpuRenderer` per iteration (fresh caches/budget), the
/// DisplayList + caches built once in setup. Measures the backend dispatch
/// loop + rasterization — the M1 target.
fn cpu_render(c: &mut Criterion) {
    let mut group = c.benchmark_group("cpu_render");
    group.throughput(Throughput::Elements(0)); // set per-input below
    for (label, path, page) in existing_corpus() {
        let setup = load_page(&path, page, DPI);
        let pixels = page_pixels(&setup);
        group.throughput(Throughput::Elements(pixels));
        group.bench_with_input(BenchmarkId::from_parameter(label), &setup, |b, s| {
            b.iter_batched(
                // Fresh renderer each iter — mirrors `zpdf render` (no cross-page
                // reuse yet; M2 introduces that).
                || {
                    zpdf::cpu::CpuRenderer::new()
                        .with_fonts(&s.font_cache)
                        .with_images(&s.image_cache)
                },
                |mut r| r.render_display_list(&s.dl, s.scale).unwrap(),
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

/// Render-only GPU: a single `WgpuRenderer` reused across iterations so the
/// device + adapter init (expensive, one-shot) is paid in warmup, not measured.
/// Each iter is a fresh `begin_page`/`end_page`, so per-page buffer/texture/
/// bind-group allocation IS measured — that overhead is the M2 target.
#[cfg(feature = "gpu-render")]
fn gpu_render(c: &mut Criterion) {
    let mut group = c.benchmark_group("gpu_render");
    for (label, path, page) in existing_corpus() {
        let setup = load_page(&path, page, DPI);
        let pixels = page_pixels(&setup);
        group.throughput(Throughput::Elements(pixels));
        group.bench_with_input(BenchmarkId::from_parameter(label), &setup, |b, s| {
            // Build the renderer once, reuse across iters: the first call (in
            // criterion's warmup) creates the headless context; subsequent iters
            // reuse it, isolating per-page render cost.
            let mut renderer = zpdf::gpu::WgpuRenderer::new()
                .with_fonts(&s.font_cache)
                .with_images(&s.image_cache);
            b.iter(|| {
                // Reclaim + re-attach the context each iter so the renderer stays
                // reusable (end_page leaves no active page). The context itself
                // is never recreated after the first iter.
                renderer.render_display_list(&s.dl, s.scale).unwrap()
            });
        });
    }
    group.finish();
}

#[cfg(not(feature = "gpu-render"))]
fn gpu_render(_c: &mut Criterion) {}

/// Full pipeline (CPU): open + interpret + render, one page, end-to-end. The
/// real-world signal — shows how much of total wall-clock is render vs
/// parse/interpret (compare against `cpu_render` for the same page).
fn full_pipeline(c: &mut Criterion) {
    let mut group = c.benchmark_group("full_pipeline");
    for (label, path, page) in existing_corpus() {
        // Compute pixels for throughput from a one-shot setup load.
        let probe = load_page(&path, page, DPI);
        let pixels = page_pixels(&probe);
        group.throughput(Throughput::Elements(pixels));
        let scale = probe.scale;
        let path: &Path = &path;
        group.bench_with_input(
            BenchmarkId::from_parameter(label),
            &(path, page),
            |b, input| {
                let (path, page) = input;
                b.iter(|| {
                    let setup = load_page(path, *page, DPI);
                    let mut r = zpdf::cpu::CpuRenderer::new()
                        .with_fonts(&setup.font_cache)
                        .with_images(&setup.image_cache);
                    r.render_display_list(&setup.dl, scale).unwrap()
                });
            },
        );
    }
    group.finish();
}

/// Raster pixel count of a setup's page — for `Throughput::Elements`.
fn page_pixels(s: &PageSetup) -> u64 {
    let r = s.dl.page_rect;
    let w = ((r.width() * s.scale as f64).ceil().max(1.0)) as u64;
    let h = ((r.height() * s.scale as f64).ceil().max(1.0)) as u64;
    w * h
}

criterion_group!(benches, cpu_render, gpu_render, full_pipeline);
criterion_main!(benches);
