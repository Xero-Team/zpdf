//! The five pipeline stages, one criterion group each.
//!
//! Groups: `stages/parse`, `stages/interpret`, `stages/render_cpu`,
//! `stages/encode`, `stages/total` — over the corpus × DPI{96,150,300} matrix.
//!
//! Each group measures **one** stage with its prerequisites in (unmeasured)
//! setup, so the groups are independently readable: the time reported for
//! `render_cpu` does not include parse or interpret, and `total` is the sum a
//! consumer actually pays (minus the PNG file write, which is I/O — see
//! `pipeline::encode_png`).
//!
//! Two of these stages were previously invisible. **Encode** appeared in no
//! recorded number anywhere, despite the CLI paying it on every `render`. And
//! **parse/interpret** were only ever inferred by subtracting one measured
//! number from another; the note that "the parser is light relative to
//! rendering" was an assumption that survived a whole investigation without
//! being measured.
//!
//! Sample counts are chosen from **measured per-case cost** (`common::-
//! apply_cost_budget`): the corpus spans ~10 ms text pages and ~400 ms
//! image-bound ones, and criterion extends the measurement window when samples
//! are slow, so a single group-wide count silently turns a 3 s budget into
//! minutes.
//!
//! Run:
//!   cargo bench -p zpdf-benches --bench stages
//!   cargo bench -p zpdf-benches --bench stages -- stages/encode
//!   ZPDF_BENCH_PER_CLASS=2 cargo bench -p zpdf-benches --bench stages   # quicker

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput};
use zpdf::RenderBackend;

use zpdf_benches::{
    encode_png, estimate_pixels, interpret, latency_corpus, load_page, parse, Composition, Rgba,
};

mod common;
use common::{apply_cost_budget, apply_cost_budget_probing, case_id, DPI_MATRIX};

/// Stage 1: read the file, open the document (xref + lazy repair), resolve page.
fn parse_stage(c: &mut Criterion) {
    let mut group = c.benchmark_group("stages/parse");
    // Parse is a per-file cost, not a per-pixel one, so throughput is not
    // reported in pixels here — the numbers to compare are absolute.
    for page_ref in latency_corpus() {
        for &dpi in DPI_MATRIX {
            apply_cost_budget_probing(&mut group, || {
                parse(&page_ref.path, page_ref.page, dpi as f32).expect("parse");
            });
            group.bench_with_input(
                BenchmarkId::from_parameter(case_id(
                    &page_ref.label,
                    page_ref.class.as_deref(),
                    dpi,
                )),
                &page_ref,
                |b, p| b.iter(|| parse(&p.path, p.page, dpi as f32).expect("parse")),
            );
        }
    }
    group.finish();
}

/// Stage 2: load page fonts, decode content, interpret into a `DisplayList`.
fn interpret_stage(c: &mut Criterion) {
    let mut group = c.benchmark_group("stages/interpret");
    for page_ref in latency_corpus() {
        for &dpi in DPI_MATRIX {
            let parsed = parse(&page_ref.path, page_ref.page, dpi as f32).expect("parse");
            let scale = dpi as f32 / 72.0;
            group.throughput(Throughput::Elements(estimate_pixels(
                parsed.page_box,
                scale,
            )));

            // Setup: one interpret, both for the cost estimate and for the
            // composition report.
            let (_, cost) = time_once_with(|| interpret(&parsed).expect("interpret"));
            apply_cost_budget(&mut group, cost);
            let comp = Composition::of(&interpret(&parsed).expect("interpret").dl, scale);
            zpdf_benches::classify::maybe_report(&parsed.label(), &comp);

            // One *instrumented* interpret for the per-category work split,
            // printed beside the composition when `ZPDF_BENCH_DEBUG=1`. Separate
            // from the timed run above on purpose: those clocks sit on every
            // attributed operator, so the criterion numbers must come from an
            // uninstrumented pass.
            if let Ok((setup, work)) = zpdf_benches::interpret_instrumented(&parsed) {
                zpdf_benches::classify::maybe_report_interpret_work(
                    &parsed.label(),
                    dpi,
                    setup.setup_ns,
                    &work,
                );
            }

            group.bench_with_input(
                BenchmarkId::from_parameter(case_id(
                    &page_ref.label,
                    page_ref.class.as_deref(),
                    dpi,
                )),
                &parsed,
                |b, p| b.iter(|| interpret(p).expect("interpret")),
            );
        }
    }
    group.finish();
}

/// Stage 3: drive the CPU backend over the display list. Parse + interpret are in
/// setup; a fresh renderer per iteration (no cross-page reuse).
fn render_stage(c: &mut Criterion) {
    let mut group = c.benchmark_group("stages/render_cpu");
    for page_ref in latency_corpus() {
        for &dpi in DPI_MATRIX {
            let setup = load_page(&page_ref.path, page_ref.page, dpi as f32).expect("load_page");
            group.throughput(Throughput::Elements(estimate_pixels(
                setup.dl.page_rect,
                setup.scale,
            )));

            let mut probe = zpdf::cpu::CpuRenderer::new()
                .with_fonts(&setup.font_cache)
                .with_images(&setup.image_cache);
            let (_, cost) = time_once_with(|| {
                probe
                    .render_display_list(&setup.dl, setup.scale)
                    .expect("render")
            });
            apply_cost_budget(&mut group, cost);

            group.bench_with_input(
                BenchmarkId::from_parameter(case_id(
                    &page_ref.label,
                    page_ref.class.as_deref(),
                    dpi,
                )),
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
    }
    group.finish();
}

/// Stage 4: PNG-encode the rendered RGBA, through the same codec the CLI uses.
///
/// Encoded to memory: the file write is I/O and would make the number depend on
/// the filesystem rather than on the encoder.
fn encode_stage(c: &mut Criterion) {
    let mut group = c.benchmark_group("stages/encode");
    for page_ref in latency_corpus() {
        for &dpi in DPI_MATRIX {
            let setup = load_page(&page_ref.path, page_ref.page, dpi as f32).expect("load_page");
            let mut renderer = zpdf::cpu::CpuRenderer::new()
                .with_fonts(&setup.font_cache)
                .with_images(&setup.image_cache);
            let rgba = Rgba::from(
                renderer
                    .render_display_list(&setup.dl, setup.scale)
                    .expect("render"),
            );
            group.throughput(Throughput::Elements(rgba.pixels()));
            let (_, cost) = time_once_with(|| encode_png(&rgba).expect("encode"));
            apply_cost_budget(&mut group, cost);
            group.bench_with_input(
                BenchmarkId::from_parameter(case_id(
                    &page_ref.label,
                    page_ref.class.as_deref(),
                    dpi,
                )),
                &rgba,
                |b, rgba| b.iter(|| encode_png(rgba).expect("encode")),
            );
        }
    }
    group.finish();
}

/// All five stages in sequence — what a consumer pays, end to end.
fn total_stage(c: &mut Criterion) {
    let mut group = c.benchmark_group("stages/total");
    for page_ref in latency_corpus() {
        for &dpi in DPI_MATRIX {
            let probe = load_page(&page_ref.path, page_ref.page, dpi as f32).expect("load_page");
            group.throughput(Throughput::Elements(estimate_pixels(
                probe.dl.page_rect,
                probe.scale,
            )));
            let (_, cost) = time_once_with(|| {
                let parsed = parse(&page_ref.path, page_ref.page, dpi as f32).expect("parse");
                let setup = interpret(&parsed).expect("interpret");
                let mut renderer = zpdf::cpu::CpuRenderer::new()
                    .with_fonts(&setup.font_cache)
                    .with_images(&setup.image_cache);
                let rendered = renderer
                    .render_display_list(&setup.dl, setup.scale)
                    .expect("render");
                encode_png(&Rgba::from(rendered)).expect("encode")
            });
            apply_cost_budget(&mut group, cost);

            let path = page_ref.path.clone();
            let page = page_ref.page;
            group.bench_with_input(
                BenchmarkId::from_parameter(case_id(
                    &page_ref.label,
                    page_ref.class.as_deref(),
                    dpi,
                )),
                &(path, page),
                |b, (path, page)| {
                    b.iter(|| {
                        let parsed = parse(path, *page, dpi as f32).expect("parse");
                        let setup = interpret(&parsed).expect("interpret");
                        let mut renderer = zpdf::cpu::CpuRenderer::new()
                            .with_fonts(&setup.font_cache)
                            .with_images(&setup.image_cache);
                        let rendered = renderer
                            .render_display_list(&setup.dl, setup.scale)
                            .expect("render");
                        encode_png(&Rgba::from(rendered)).expect("encode")
                    })
                },
            );
        }
    }
    group.finish();
}

/// [`time_once`], but returning the closure's value too.
fn time_once_with<T>(mut op: impl FnMut() -> T) -> (T, std::time::Duration) {
    let started = std::time::Instant::now();
    let value = op();
    (value, started.elapsed())
}

criterion_group!(
    stages,
    parse_stage,
    interpret_stage,
    render_stage,
    encode_stage,
    total_stage
);
criterion_main!(stages);
