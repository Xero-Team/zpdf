//! Whole-document throughput — the "convert this 500-page PDF" number.
//!
//! **Not part of a default `cargo bench`.** It is gated behind the `bench-long`
//! feature, because a single 526-page document at 150 DPI takes tens of seconds
//! per run: criterion would spend minutes on one case, and nobody would run it.
//! The gating is the point — a benchmark nobody runs is how the previous
//! harness's numbers went stale.
//!
//! This target is a plain `main`, not a criterion group: the interesting output
//! is one absolute per-document wall time (and pages/sec), not a distribution
//! over many samples.
//!
//! Sequential only, deliberately. Page-level parallelism is measured in
//! `benches/batch.rs`, where each task re-opens its own document (the parser's
//! object cache is `!Sync`). Repeating that per-page parse here would bury the
//! signal in parse cost rather than measure the document honestly.
//!
//! Run:
//!   cargo bench -p zpdf-benches --features bench-long --bench longform
//!   ZPDF_BENCH_MAX_PAGES=50 cargo bench -p zpdf-benches --features bench-long --bench longform

use std::time::Instant;

use zpdf::{PdfDocument, RenderBackend};

use zpdf_benches::{batch_corpus, pipeline};

/// Documents are measured at one DPI: this target is about document size, not
/// about the DPI curve (`stages` owns that axis).
const DPI: u32 = 150;

fn main() {
    let docs = batch_corpus();
    if docs.is_empty() {
        eprintln!(
            "zpdf-benches: no multi-page documents in the verified corpus.\n  \
             longform needs the real corpus (not in version control); see \
             crates/zpdf-benches/corpus-manifest.tsv"
        );
        std::process::exit(1);
    }

    // 0 (default) = every page.
    let max_pages: usize = std::env::var("ZPDF_BENCH_MAX_PAGES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    println!(
        "longform: whole-document throughput at {DPI} DPI (sequential, one reused renderer per page)\n"
    );
    println!(
        "{:<16} {:>6} {:>11} {:>10} {:>11}",
        "document", "pages", "total", "ms/page", "pages/sec"
    );

    let mut grand_pages = 0usize;
    let mut grand_time = std::time::Duration::ZERO;

    for doc in &docs {
        let data = match std::fs::read(&doc.path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("zpdf-benches: read {}: {e} — skipping", doc.path.display());
                continue;
            }
        };
        let pdf = match PdfDocument::open(data) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("zpdf-benches: open {}: {e} — skipping", doc.path.display());
                continue;
            }
        };
        let total_pages = pdf.page_count();
        let pages = if max_pages == 0 {
            total_pages
        } else {
            total_pages.min(max_pages)
        };

        let started = Instant::now();
        let mut rendered_bytes = 0usize;
        let mut failed = 0usize;
        for index in 0..pages {
            let page = match pdf.page(index) {
                Ok(p) => p,
                Err(e) => {
                    if failed == 0 {
                        eprintln!("  page {index}: {e}");
                    }
                    failed += 1;
                    continue;
                }
            };
            let page_box = page.effective_box();
            let setup = match pipeline::interpret_page(&pdf, &page, page_box, DPI as f32) {
                Ok((s, _)) => s,
                Err(e) => {
                    if failed == 0 {
                        eprintln!("  page {index} interpret: {e}");
                    }
                    failed += 1;
                    continue;
                }
            };
            let mut renderer = zpdf::cpu::CpuRenderer::new()
                .with_fonts(&setup.font_cache)
                .with_images(&setup.image_cache);
            match renderer.render_display_list(&setup.dl, setup.scale) {
                Ok(rendered) => rendered_bytes += rendered.data.len(),
                Err(e) => {
                    if failed == 0 {
                        eprintln!("  page {index} render: {e}");
                    }
                    failed += 1;
                }
            }
        }
        let elapsed = started.elapsed();

        let secs = elapsed.as_secs_f64();
        println!(
            "{:<16} {:>6} {:>10.2}s {:>10.2} {:>11.1}{}",
            doc.label,
            pages,
            secs,
            secs * 1000.0 / pages.max(1) as f64,
            pages as f64 / secs.max(f64::MIN_POSITIVE),
            if failed > 0 {
                format!("   ({failed} page(s) failed)")
            } else {
                String::new()
            }
        );
        std::hint::black_box(rendered_bytes);

        grand_pages += pages - failed;
        grand_time += elapsed;
    }

    let secs = grand_time.as_secs_f64();
    println!(
        "\n{:<16} {:>6} {:>10.2}s {:>10.2} {:>11.1}",
        "TOTAL",
        grand_pages,
        secs,
        secs * 1000.0 / grand_pages.max(1) as f64,
        grand_pages as f64 / secs.max(f64::MIN_POSITIVE),
    );
}
