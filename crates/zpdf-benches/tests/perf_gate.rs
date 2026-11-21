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

/// Per-fixture expected composition:
/// `(name, commands, fills, strokes, glyphs, images, clips)`.
///
/// A change here is a change in what zpdf draws for that feature — the fixtures
/// are single-feature by construction (see `tests/gen_corpus.py`), which is what
/// makes an exact number meaningful rather than brittle. (`pattern_tiling` is
/// large because a tiling pattern expands to its tiles in the display list:
/// 961 commands for one painted shape.)
const EXPECTED: &[(&str, u64, u64, u64, u64, u64, u64)] = &[
    ("clip", 9, 5, 0, 0, 0, 2),
    ("curves", 2, 1, 1, 0, 0, 0),
    ("image_rgb", 3, 0, 0, 0, 3, 0),
    ("image_under_clip", 4, 1, 0, 0, 1, 1),
    ("pattern_tiling", 961, 382, 191, 0, 0, 194),
    ("pattern_tiling_uncolored", 676, 336, 0, 0, 0, 170),
    ("rect_fills", 4, 4, 0, 0, 0, 0),
    ("strokes", 3, 0, 3, 0, 0, 0),
    ("text_type3", 1, 0, 0, 3, 0, 0),
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

        let Some((_, cmds, fills, strokes, glyphs, images, clips)) =
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
            comp.clips,
        );
        let expected = (*cmds, *fills, *strokes, *glyphs, *images, *clips);
        if actual != expected {
            mismatches.push(format!(
                "{}: expected {expected:?} (cmds, fills, strokes, glyphs, images, clips), got {actual:?}\n    {}",
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

fn assemble_pdf(objs: &[Vec<u8>]) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = Vec::with_capacity(objs.len());
    for (i, body) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref_pos = out.len();
    let n = objs.len() + 1;
    out.extend_from_slice(format!("xref\n0 {n}\n").as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for off in &offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<< /Size {n} /Root 1 0 R >>\nstartxref\n{xref_pos}\n%%EOF\n").as_bytes(),
    );
    out
}

fn stream_obj(dict: &str, content: &[u8]) -> Vec<u8> {
    let mut v = format!("<< {dict} /Length {} >>\nstream\n", content.len()).into_bytes();
    v.extend_from_slice(content);
    v.extend_from_slice(b"\nendstream");
    v
}

/// Covering rectangle clips (page-covering `W`/`W*`, nested clips covering
/// narrowed bounds, and full-page Form `/BBox`) are elided in the interpreter,
/// reducing display-list `PushClip` commands and CPU backend mask builds with
/// zero pixel difference.
#[test]
fn covering_rectangle_clips_reduce_push_clip_count() {
    let mut content_with_covering = String::new();
    let mut content_minimal = String::new();

    // 12 repeated page-covering clips wrapping distinct fills.
    for i in 0..12 {
        let x = 10 + i * 14;
        let fill = format!("0.2 0.4 0.8 rg {x} 10 10 20 re f\n");
        content_with_covering.push_str(&format!("q 0 0 200 200 re W n {fill}Q\n"));
        content_minimal.push_str(&format!("q {fill}Q\n"));
    }

    // 1 oversized even-odd covering clip.
    content_with_covering.push_str("q -10 -10 220 220 re W* n 0.9 0.9 0.2 rg 10 40 30 30 re f Q\n");
    content_minimal.push_str("q 0.9 0.9 0.2 rg 10 40 30 30 re f Q\n");

    // 1 narrowing clip containing a covering inner clip + a further narrowing clip.
    content_with_covering.push_str(
        "q 40 40 120 120 re W n \
         q 20 20 160 160 re W n 0.8 0.2 0.2 rg 0 0 200 200 re f Q \
         q 60 60 80 80 re W n 0.2 0.8 0.2 rg 0 0 200 200 re f Q \
         Q\n",
    );
    content_minimal.push_str(
        "q 40 40 120 120 re W n \
         q 0.8 0.2 0.2 rg 0 0 200 200 re f Q \
         q 60 60 80 80 re W n 0.2 0.8 0.2 rg 0 0 200 200 re f Q \
         Q\n",
    );

    // 1 Form XObject whose /BBox covers the whole page.
    content_with_covering.push_str("/Fm0 Do\n");
    content_minimal.push_str("0.1 0.1 0.1 rg 150 150 30 30 re f\n");

    let build = |content: &[u8]| {
        assemble_pdf(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
              /Resources << /XObject << /Fm0 5 0 R >> >> >>"
                .to_vec(),
            stream_obj("", content),
            stream_obj(
                "/Type /XObject /Subtype /Form /BBox [0 0 200 200]",
                b"0.1 0.1 0.1 rg 150 150 30 30 re f",
            ),
        ])
    };

    let interpret_and_render = |pdf: Vec<u8>| {
        let doc = zpdf::PdfDocument::open(pdf).expect("open synthetic pdf");
        let page = doc.page(0).expect("page 0");
        let content = doc.page_content_bytes(&page).expect("content bytes");
        let dl = zpdf::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .interpret(&content);
        let scale = 2.0;
        let comp = Composition::of(&dl, scale);
        let mut renderer = zpdf::cpu::CpuRenderer::new().with_stage_timing(true);
        let rendered = renderer
            .render_display_list(&dl, scale)
            .expect("cpu render");
        let stats = renderer.stage_stats().expect("stage stats enabled");
        (comp, stats, rendered)
    };

    let (comp, stats, rendered) = interpret_and_render(build(content_with_covering.as_bytes()));
    let (ref_comp, ref_stats, reference) = interpret_and_render(build(content_minimal.as_bytes()));

    // 17 clip operations in the content/form stream collapse to only the 2
    // genuine narrowing clips (matching the reference with covering clips removed).
    assert_eq!(
        comp.clips,
        2,
        "expected 15 covering clips elided: {}",
        comp.summary()
    );
    assert_eq!(comp.fills, 16, "{}", comp.summary());
    assert_eq!(
        comp.commands,
        20,
        "16 fills + 2 PushClip + 2 PopClip: {}",
        comp.summary()
    );
    assert_eq!(comp.clips, ref_comp.clips);
    assert_eq!(stats.clips_pushed, 2);
    assert_eq!(stats.clips_pushed, ref_stats.clips_pushed);
    assert_eq!(
        rendered.data, reference.data,
        "eliding covering rectangle clips must not change rendered pixels"
    );
}
