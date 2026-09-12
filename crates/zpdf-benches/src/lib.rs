//! Shared helpers for the zpdf criterion benches.
//!
//! Four concerns, one module each:
//!
//! * [`corpus`] — which pages to measure, and the manifest that proves the
//!   local-only fixtures are the ones the baseline was taken on.
//! * [`manifest`] — integrity for corpus files that are deliberately not in
//!   version control.
//! * [`pipeline`] — the real pipeline, decomposed into five measurable stages.
//! * [`classify`] — what a page is made of, **measured** from its display list
//!   rather than guessed from a filename (see the module docs for why).
//!
//! Nothing in this crate is measured by the benches except through these; the
//! bench targets in `benches/` own the measurement.

use std::sync::OnceLock;

pub mod classify;
pub mod corpus;
pub mod manifest;
pub mod pipeline;

pub use classify::{Composition, LoadClass};
pub use corpus::{Corpus, DocRef, PageRef};
pub use pipeline::{
    encode_png, estimate_pixels, interpret, interpret_instrumented, interpret_with_stats,
    load_page, parse, PageSetup, ParsedPage, Rgba,
};

/// The verified corpus, cached for the process.
///
/// Verification hashes the whole real corpus (331.7 MiB) and every bench group
/// needs it, so without caching one `cargo bench` run would re-hash it once per
/// group — five times over. Panics with the verification error, which is the
/// intended hard failure (see [`Corpus`]).
fn verified_corpus() -> &'static Corpus {
    static CORPUS: OnceLock<Corpus> = OnceLock::new();
    CORPUS.get_or_init(|| {
        let c = Corpus::load().unwrap_or_else(|e| panic!("\n{e}\n"));
        eprintln!("zpdf-benches: corpus — {}", c.summary());
        c
    })
}

/// The single-page latency set, verified against the checked-in corpus
/// manifest.
///
/// **Fails hard** when the real corpus is missing or altered: a benchmark that
/// silently measures nothing is worse than one that refuses to run. The
/// previous version of this function filtered missing files out with a stderr
/// note, which is exactly how the last round's numbers went stale.
///
/// `ZPDF_BENCH_SYNTHETIC_ONLY=1` is the only degraded mode, and it announces
/// itself loudly.
pub fn latency_corpus() -> Vec<PageRef> {
    let c = verified_corpus();
    let pages = if c.synthetic_only {
        c.synthetic_pages()
    } else {
        c.latency_pages()
    };
    if pages.is_empty() {
        panic!(
            "bench corpus selection is empty — no usable pages in the manifest. \
             Regenerate it with:\n  cargo run -p zpdf-benches --bin corpus-manifest -- --update"
        );
    }
    pages
}

/// The synthetic corpus (in version control, so always present). Used by the CI
/// smoke target, where a flake-free deterministic counter is the signal.
///
/// Needs no manifest and no real PDFs — see [`Corpus::synthetic_only`].
pub fn synthetic_corpus() -> Vec<PageRef> {
    Corpus::synthetic_only().synthetic_pages()
}

/// Multi-page documents for the throughput / parallel-scaling benches.
pub fn batch_corpus() -> Vec<DocRef> {
    verified_corpus().batch_docs()
}
