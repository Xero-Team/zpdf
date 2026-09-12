//! Backend-focused benches: CPU fresh-vs-reused renderer, the CPU stage
//! breakdown, the CLI as a process, and GPU readback-vs-submit-only.
//!
//! ## What "cold" does and does not mean here
//!
//! The performance notes record a 20x gap between a benchmark render (9 ms on
//! one corpus page) and the same page rendered by the CLI (179 ms). That gap is
//! **process-level** — allocator warm-up, first-touch page faults, lazy statics,
//! one-time per-page allocations — and a criterion bench cannot reproduce it,
//! because criterion runs its iterations inside one already-warm process.
//!
//! So this file separates the two honestly:
//!
//! * `backend/cpu_fresh` vs `backend/cpu_reused` — the *renderer*-level axis
//!   (a fresh `CpuRenderer` per iteration vs one reused). This isolates per-page
//!   renderer setup from steady-state drawing.
//! * `backend/cli_process` — the process-level axis, by actually spawning the
//!   CLI. This is the number quoted as 179 ms, measured the same way.
//!
//! Anything attributed to "cold rendering" without saying which of these two it
//! means is not a measurement.
//!
//! Run:
//!   cargo bench -p zpdf-benches --bench backend
//!   cargo bench -p zpdf-benches --features gpu-render --bench backend

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput};
use zpdf::RenderBackend;

use zpdf_benches::{estimate_pixels, load_page, Composition};

mod common;
use common::{apply_cost_budget, apply_cost_budget_probing, case_id_flat};

/// The DPI used by the backend groups. A single point, because this target is
/// about *which backend configuration* is faster, not about the DPI curve —
/// `stages` owns that axis.
const DPI: u32 = 150;

fn case_id(label: &str, class: Option<&str>) -> String {
    case_id_flat(label, class)
}

/// Fresh `CpuRenderer` per iteration — pays per-page renderer setup (pixmap
/// allocation, cache construction) but stays inside a warm process.
fn cpu_fresh(c: &mut Criterion) {
    let mut group = c.benchmark_group("backend/cpu_fresh");
    for page_ref in zpdf_benches::latency_corpus() {
        let setup = load_page(&page_ref.path, page_ref.page, DPI as f32).expect("load_page");
        group.throughput(Throughput::Elements(estimate_pixels(
            setup.dl.page_rect,
            setup.scale,
        )));
        let mut probe = zpdf::cpu::CpuRenderer::new()
            .with_fonts(&setup.font_cache)
            .with_images(&setup.image_cache);
        apply_cost_budget_probing(&mut group, || {
            probe
                .render_display_list(&setup.dl, setup.scale)
                .expect("render")
        });
        group.bench_with_input(
            BenchmarkId::from_parameter(case_id(&page_ref.label, page_ref.class.as_deref())),
            &setup,
            |b, s| {
                b.iter_batched(
                    || {
                        zpdf::cpu::CpuRenderer::new()
                            .with_fonts(&s.font_cache)
                            .with_images(&s.image_cache)
                    },
                    |mut r| r.render_display_list(&s.dl, s.scale).expect("render"),
                    BatchSize::SmallInput,
                )
            },
        );
    }
    group.finish();
}

/// One `CpuRenderer` reused across iterations — steady state, per-page setup
/// amortised.
fn cpu_reused(c: &mut Criterion) {
    let mut group = c.benchmark_group("backend/cpu_reused");
    for page_ref in zpdf_benches::latency_corpus() {
        let setup = load_page(&page_ref.path, page_ref.page, DPI as f32).expect("load_page");
        group.throughput(Throughput::Elements(estimate_pixels(
            setup.dl.page_rect,
            setup.scale,
        )));
        let mut probe = zpdf::cpu::CpuRenderer::new()
            .with_fonts(&setup.font_cache)
            .with_images(&setup.image_cache);
        apply_cost_budget_probing(&mut group, || {
            probe
                .render_display_list(&setup.dl, setup.scale)
                .expect("render")
        });
        group.bench_with_input(
            BenchmarkId::from_parameter(case_id(&page_ref.label, page_ref.class.as_deref())),
            &setup,
            |b, s| {
                let mut r = zpdf::cpu::CpuRenderer::new()
                    .with_fonts(&s.font_cache)
                    .with_images(&s.image_cache);
                // First call (warm-up) also constructs the pixmap.
                b.iter(|| r.render_display_list(&s.dl, s.scale).expect("render"))
            },
        );
    }
    group.finish();
}

/// The stage split inside one page render, plus the page's composition.
///
/// Reported as a printed diagnostic, not as a criterion number: criterion
/// measures time, and the value of these stats is *attribution* — which bucket
/// holds the time. The counters (`glyphs`, `glyph_outlines_parsed`, `fills`, …)
/// are deterministic integers and therefore the CI-gateable half.
///
/// Note this group times a **timing-enabled** render, which is slightly slower
/// than production (the clock reads are inside the primitive loops). It exists to
/// explain `backend/cpu_reused`, not to replace it.
fn cpu_stats(c: &mut Criterion) {
    let mut group = c.benchmark_group("backend/cpu_stats");
    for page_ref in zpdf_benches::latency_corpus() {
        let setup = load_page(&page_ref.path, page_ref.page, DPI as f32).expect("load_page");
        let pixels = estimate_pixels(setup.dl.page_rect, setup.scale);
        group.throughput(Throughput::Elements(pixels));

        // One measuring render, reported to stderr for the report; repeated
        // `--verbose` iterations would only multiply the same explanation. Its
        // duration also sets this case's sample count.
        let mut probe = zpdf::cpu::CpuRenderer::new()
            .with_fonts(&setup.font_cache)
            .with_images(&setup.image_cache)
            .with_stage_timing(true);
        let started = std::time::Instant::now();
        probe
            .render_display_list(&setup.dl, setup.scale)
            .expect("render");
        apply_cost_budget(&mut group, started.elapsed());
        if let Some(st) = probe.stage_stats() {
            let comp = Composition::of(&setup.dl, setup.scale);
            eprintln!(
                "zpdf-benches stage-split {} [{}] total={:.2}ms | \
                 outline-parse={:.2}ms glyph-raster={:.2}ms fill={:.2}ms stroke={:.2}ms \
                 image={:.2}ms clip={:.2}ms soft-mask={:.2}ms (render={:.2} reduce={:.2} fold={:.2} composite={:.2}) | \
                 shares: glyph={:.0}% image={:.0}% fill={:.0}% clip={:.0}% mask={:.0}% | \
                 counters: glyphs={} outlines={} fills={} strokes={} images={} clips={} masks={} \
                 composite_px={} region_px={} leak_px={} reduce_px={} reduce_leak_px={} \
                 of page×groups={} (scoped {:.0}%, content {:.0}%)\n  {}",
                case_id(&page_ref.label, page_ref.class.as_deref()),
                comp.class().as_str(),
                st.total_ns as f64 / 1e6,
                st.outline_parse_ns as f64 / 1e6,
                st.glyph_raster_ns as f64 / 1e6,
                st.fill_ns as f64 / 1e6,
                st.stroke_ns as f64 / 1e6,
                st.image_ns as f64 / 1e6,
                st.clip_ns as f64 / 1e6,
                st.soft_mask_ns as f64 / 1e6,
                st.mask_render_ns as f64 / 1e6,
                st.mask_reduce_ns as f64 / 1e6,
                st.mask_fold_ns as f64 / 1e6,
                st.mask_composite_ns as f64 / 1e6,
                st.share(st.glyph_ns()) * 100.0,
                st.share(st.image_ns) * 100.0,
                st.share(st.fill_ns) * 100.0,
                st.share(st.clip_ns) * 100.0,
                st.share(st.soft_mask_ns) * 100.0,
                st.glyphs,
                st.glyph_outlines_parsed,
                st.fills,
                st.strokes,
                st.images,
                st.clips_pushed,
                st.soft_mask_planes,
                st.mask_composite_px,
                st.mask_composite_region_px,
                st.mask_composite_leak_px,
                st.mask_reduce_px,
                st.mask_reduce_leak_px,
                pixels.saturating_mul(comp.blend_groups.max(1)),
                100.0 * st.mask_composite_region_px as f64
                    / (pixels.saturating_mul(comp.blend_groups.max(1))).max(1) as f64,
                100.0 * st.mask_composite_px as f64
                    / (pixels.saturating_mul(comp.blend_groups.max(1))).max(1) as f64,
                comp.summary(),
            );
        }

        group.bench_with_input(
            BenchmarkId::from_parameter(case_id(&page_ref.label, page_ref.class.as_deref())),
            &setup,
            |b, s| {
                let mut r = zpdf::cpu::CpuRenderer::new()
                    .with_fonts(&s.font_cache)
                    .with_images(&s.image_cache)
                    .with_stage_timing(true);
                b.iter(|| r.render_display_list(&s.dl, s.scale).expect("render"))
            },
        );
    }
    group.finish();
}

/// What the CLI actually costs, as a process: spawn it and wait.
///
/// This is the only place the process-level cold cost is measurable, and it is
/// the honest counterpart to `backend/cpu_reused`. Includes the PNG file write,
/// because the CLI does it and a user pays it.
///
/// Skipped with a loud note when no CLI binary has been built (a benchmark that
/// silently measures nothing is the failure mode this whole harness exists to
/// avoid).
fn cli_process(c: &mut Criterion) {
    let Some(bin) = find_cli_binary() else {
        eprintln!(
            "zpdf-benches: no CLI binary found — skipping backend/cli_process.\n  \
             Build one first: cargo build -p zpdf-cli --release"
        );
        return;
    };
    let out_dir = std::env::temp_dir().join("zpdf-bench-cli");
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!(
            "zpdf-benches: cannot create {}: {e} — skipping cli_process",
            out_dir.display()
        );
        return;
    }

    let mut group = c.benchmark_group("backend/cli_process");
    // One operation here costs hundreds of milliseconds (a subprocess spawn, or a
    // GPU submit + readback), so a fixed cost estimate replaces a probe.
    apply_cost_budget(&mut group, std::time::Duration::from_millis(300));
    for page_ref in zpdf_benches::latency_corpus() {
        let probe = load_page(&page_ref.path, page_ref.page, DPI as f32).expect("load_page");
        group.throughput(Throughput::Elements(estimate_pixels(
            probe.dl.page_rect,
            probe.scale,
        )));
        let out = out_dir.join(format!("{}.png", page_ref.label));
        let path = page_ref.path.clone();
        let page = page_ref.page;
        group.bench_with_input(
            BenchmarkId::from_parameter(case_id(&page_ref.label, page_ref.class.as_deref())),
            &(path, page, out),
            |b, (path, page, out)| {
                b.iter(|| {
                    let status = std::process::Command::new(&bin)
                        .arg("render")
                        .arg(path)
                        .arg("-p")
                        .arg((page + 1).to_string()) // CLI pages are 1-based
                        .arg("-o")
                        .arg(out)
                        .arg("--dpi")
                        .arg(DPI.to_string())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status()
                        .expect("spawn zpdf CLI");
                    assert!(status.success(), "CLI render failed for {}", path.display());
                })
            },
        );
    }
    group.finish();
}

/// Locate a **release** CLI binary.
///
/// Release only, deliberately. Measuring a debug build would report a number
/// several times the real cost (measured: a debug CLI takes ~15 s on a page the
/// release build does in ~0.2 s) and it would sit in the same table as optimized
/// numbers, looking comparable. A skipped group with instructions is honest; a
/// misleading row is not.
fn find_cli_binary() -> Option<std::path::PathBuf> {
    let root = zpdf_benches::corpus::root();
    for rel in ["target/release/zpdf.exe", "target/release/zpdf"] {
        let p = root.join(rel);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

// -- GPU --------------------------------------------------------------------

/// GPU dual numbers: `wall` (with readback) and the no-readback submit-only
/// path, with the adapter identity printed once.
///
/// Read this with the identity block: a GPU timing without the adapter it was
/// taken on is not comparable to anything.
#[cfg(feature = "gpu-render")]
fn gpu(c: &mut Criterion) {
    use zpdf::gpu::WgpuRenderer;

    let mut probe = WgpuRenderer::new();
    let identity = match probe.adapter_identity() {
        Ok(id) => id.clone(),
        Err(e) => {
            eprintln!("zpdf-benches: no usable GPU adapter ({e}) — skipping GPU groups");
            return;
        }
    };
    eprintln!(
        "zpdf-benches GPU adapter (record this with any GPU number):\n  {}",
        identity.describe().replace('\n', "\n  ")
    );
    if identity.is_software() {
        eprintln!(
            "zpdf-benches: WARNING — software adapter; GPU timings describe the fallback path"
        );
    }

    let mut group = c.benchmark_group("backend/gpu_readback");
    // One operation here costs hundreds of milliseconds (a GPU submit +
    // readback), so a fixed cost estimate replaces a probe.
    apply_cost_budget(&mut group, std::time::Duration::from_millis(300));
    // One renderer for the whole group: device + adapter init is one-shot and
    // must not be charged to a page.
    let mut renderer = WgpuRenderer::new();
    for page_ref in zpdf_benches::latency_corpus() {
        let setup = load_page(&page_ref.path, page_ref.page, DPI as f32).expect("load_page");
        // Probe once. An adapter that cannot render this page at all (OOM,
        // unsupported size) must skip the case with a note, not panic the whole
        // target — a run that dies at case 40 of 54 wastes everything before it.
        if let Err(e) = renderer.render_display_list(&setup.dl, setup.scale) {
            eprintln!(
                "zpdf-benches: skipping backend/gpu_readback/{} — {e}",
                page_ref.label
            );
            continue;
        }
        group.throughput(Throughput::Elements(estimate_pixels(
            setup.dl.page_rect,
            setup.scale,
        )));
        group.bench_with_input(
            BenchmarkId::from_parameter(case_id(&page_ref.label, page_ref.class.as_deref())),
            &setup,
            |b, s| {
                b.iter(|| {
                    renderer
                        .render_display_list(&s.dl, s.scale)
                        .expect("gpu render")
                })
            },
        );
    }
    group.finish();

    // Submit-only: same GPU work, no readback — the viewer consumption mode.
    let mut group = c.benchmark_group("backend/gpu_submit_only");
    apply_cost_budget(&mut group, std::time::Duration::from_millis(300));
    let mut renderer = WgpuRenderer::new();
    for page_ref in zpdf_benches::latency_corpus() {
        let setup = load_page(&page_ref.path, page_ref.page, DPI as f32).expect("load_page");
        // Probe once — see the readback group above.
        if let Err(e) = renderer.render_display_list_submitted(&setup.dl, setup.scale) {
            eprintln!(
                "zpdf-benches: skipping backend/gpu_submit_only/{} — {e}",
                page_ref.label
            );
            continue;
        }
        group.throughput(Throughput::Elements(estimate_pixels(
            setup.dl.page_rect,
            setup.scale,
        )));
        group.bench_with_input(
            BenchmarkId::from_parameter(case_id(&page_ref.label, page_ref.class.as_deref())),
            &setup,
            |b, s| {
                b.iter(|| {
                    renderer
                        .render_display_list_submitted(&s.dl, s.scale)
                        .expect("gpu submit")
                })
            },
        );
    }
    group.finish();
}

#[cfg(not(feature = "gpu-render"))]
fn gpu(_c: &mut Criterion) {
    // Enabled with: cargo bench -p zpdf-benches --features gpu-render --bench backend
}

criterion_group!(backend, cpu_fresh, cpu_reused, cpu_stats, cli_process, gpu);
criterion_main!(backend);
