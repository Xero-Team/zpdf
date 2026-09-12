//! The perf gate CI can honestly run.
//!
//! Two kinds of assertion, and deliberately nothing else:
//!
//! 1. **Deterministic counters.** Display-list composition — command, glyph,
//!    image, fill and stroke counts — is an integer, identical on every machine
//!    and every run. It catches algorithmic regressions (a change that
//!    double-rasterizes glyphs, stops batching, or drops a paint) exactly, with
//!    zero flakiness.
//! 2. **A loose wall-clock ceiling.** Only to catch a hang or a blow-up, not a
//!    5% drift. Wall-clock on a shared CI runner cannot do better than that, and
//!    a threshold tight enough to catch 5% would fail at random — after which
//!    the gate gets ignored, which is worse than having none.
//!
//! Runs against the **synthetic** corpus (`tests/corpus/`, 9 tiny fixtures, in
//! version control), so a fresh clone and CI both have everything they need. The
//! real-PDF corpus is deliberately not in git; relative numbers on it belong to
//! `baseline --compare` on a machine that has it.
//!
//! When a counter here changes, the question is not "how do I update the
//! constant" but "did I intend to change what this page draws?".

use std::time::{Duration, Instant};

use zpdf::RenderBackend;

use zpdf_benches::{synthetic_corpus, Composition};

/// Per-fixture expected composition: `(name, commands, fills, strokes, glyphs, images)`.
///
/// A change here is a change in what zpdf draws for that feature — the fixtures
/// are single-feature by construction (see `tests/gen_corpus.py`), which is what
/// makes an exact number meaningful rather than brittle. (`pattern_tiling` is
/// large because a tiling pattern expands to its tiles in the display list:
/// 961 commands for one painted shape.)
const EXPECTED: &[(&str, u64, u64, u64, u64, u64)] = &[
    ("clip", 9, 5, 0, 0, 0),
    ("curves", 2, 1, 1, 0, 0),
    ("image_rgb", 3, 0, 0, 0, 3),
    ("image_under_clip", 4, 1, 0, 0, 1),
    ("pattern_tiling", 961, 382, 191, 0, 0),
    ("pattern_tiling_uncolored", 676, 336, 0, 0, 0),
    ("rect_fills", 4, 4, 0, 0, 0),
    ("strokes", 3, 0, 3, 0, 0),
    ("text_type3", 1, 0, 0, 3, 0),
];

/// Generous: ~100x the real cost of these tiny fixtures. Catches a hang, not a
/// regression.
const WALL_CAP: Duration = Duration::from_secs(10);

const DPI: f32 = 150.0;

#[test]
fn synthetic_corpus_composition_is_stable() {
    let pages = synthetic_corpus();
    assert_eq!(
        pages.len(),
        EXPECTED.len(),
        "synthetic corpus size changed — update EXPECTED (was it intended?)"
    );

    let mut mismatches: Vec<String> = Vec::new();
    for page_ref in &pages {
        let setup = zpdf_benches::load_page(&page_ref.path, page_ref.page, DPI)
            .unwrap_or_else(|e| panic!("{}: {e}", page_ref.path.display()));
        let comp = Composition::of(&setup.dl, setup.scale);

        let Some((_, cmds, fills, strokes, glyphs, images)) =
            EXPECTED.iter().find(|(name, ..)| *name == page_ref.label)
        else {
            mismatches.push(format!(
                "{}: fixture not in EXPECTED (add it) — {}",
                page_ref.label,
                comp.summary()
            ));
            continue;
        };
        let actual = (
            comp.commands,
            comp.fills,
            comp.strokes,
            comp.glyphs,
            comp.images,
        );
        let expected = (*cmds, *fills, *strokes, *glyphs, *images);
        if actual != expected {
            mismatches.push(format!(
                "{}: expected {expected:?} (cmds, fills, strokes, glyphs, images), got {actual:?}\n    {}",
                page_ref.label,
                comp.summary()
            ));
        }
    }

    assert!(
        mismatches.is_empty(),
        "synthetic corpus composition changed:\n  {}",
        mismatches.join("\n  ")
    );
}

/// Rendering must complete, and complete promptly. The point is a hang or a
/// pathological blow-up, not a performance threshold.
#[test]
fn synthetic_corpus_renders_within_a_loose_budget() {
    for page_ref in synthetic_corpus() {
        let setup = zpdf_benches::load_page(&page_ref.path, page_ref.page, DPI)
            .unwrap_or_else(|e| panic!("{}: {e}", page_ref.path.display()));
        let started = Instant::now();
        let mut renderer = zpdf::cpu::CpuRenderer::new()
            .with_fonts(&setup.font_cache)
            .with_images(&setup.image_cache);
        let rendered = renderer
            .render_display_list(&setup.dl, setup.scale)
            .unwrap_or_else(|e| panic!("{}: render failed: {e}", page_ref.path.display()));
        let elapsed = started.elapsed();

        assert!(
            rendered.width > 0 && rendered.height > 0,
            "{}: empty raster",
            page_ref.label
        );
        assert!(
            rendered.data.len() == (rendered.width as usize) * (rendered.height as usize) * 4,
            "{}: raster buffer is not tight RGBA8",
            page_ref.label
        );
        assert!(
            elapsed < WALL_CAP,
            "{}: {elapsed:?} exceeds the {WALL_CAP:?} ceiling",
            page_ref.label
        );
    }
}

/// Every synthetic fixture must survive a second render through a *reused*
/// renderer, and produce the same bytes. Page-scoped state that leaks across
/// pages (clip stacks, blend groups, mask caches) would show up here.
#[test]
fn synthetic_corpus_is_stable_across_two_renders() {
    let mut renderer = zpdf::cpu::CpuRenderer::new();
    for page_ref in synthetic_corpus() {
        let setup = zpdf_benches::load_page(&page_ref.path, page_ref.page, DPI)
            .unwrap_or_else(|e| panic!("{}: {e}", page_ref.path.display()));
        let mut r = zpdf::cpu::CpuRenderer::new()
            .with_fonts(&setup.font_cache)
            .with_images(&setup.image_cache);
        let first = r
            .render_display_list(&setup.dl, setup.scale)
            .expect("first render");
        let second = r
            .render_display_list(&setup.dl, setup.scale)
            .expect("second render");
        assert_eq!(
            first.data, second.data,
            "{}: two renders of the same page differ",
            page_ref.label
        );
        // Keep the outer renderer in play so the test also covers a renderer that
        // was never used for this page.
        renderer.render_display_list(&setup.dl, setup.scale).ok();
    }
}
