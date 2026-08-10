//! Shared helpers for the zpdf criterion benches.
//!
//! The benches measure two things:
//!   1. **render-only** — open + interpret a page *once* (unmeasured setup), then
//!      time `render_display_list` repeatedly. This isolates the render backend
//!      (the M1 CPU glyph cache target) from parse/interpret noise.
//!   2. **full-pipeline** — open + interpret + render together, for real-world
//!      wall-clock signal.
//!
//! The `PageSetup` holds the owned `DisplayList` + font/image caches that a
//! backend borrows while rendering. Nothing here is measured directly.

use std::path::{Path, PathBuf};

use zpdf::display_list::DisplayList;
use zpdf::{FontCache, IccCache, ImageCache, PdfDocument};

/// Everything a render backend needs to draw one page, with the parse/interpret
/// work already done. The caches live here so the backend can borrow them.
pub struct PageSetup {
    pub dl: DisplayList,
    pub font_cache: FontCache,
    pub image_cache: ImageCache,
    /// Device pixels per page-unit (DPI / 72). Pre-computed so benches share one
    /// definition of `scale`.
    pub scale: f32,
}

/// Open `path`, interpret page `page_idx` (0-based) into a DisplayList at the
/// given DPI, and return the render-ready setup. This is the unmeasured setup
/// for `render-only` benches.
///
/// **Mirrors the CLI `render` path exactly** (`zpdf-cli/src/main.rs`): includes
/// `.with_annotations`, `.with_colors`, optional-content, and output-intent so
/// the DisplayList matches what the real CLI renders. An earlier version
/// omitted annotations and under-measured render ~20× (annotation form fields
/// and note text add many glyph runs). Don't drop these without re-checking
/// against `zpdf-cli --stats`.
pub fn load_page(path: &Path, page_idx: usize, dpi: f32) -> PageSetup {
    let data = std::fs::read(path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    let doc = PdfDocument::open(data).unwrap_or_else(|e| panic!("open {path:?}: {e}"));
    let page = doc
        .page(page_idx)
        .unwrap_or_else(|e| panic!("page {page_idx} of {path:?}: {e}"));
    let page_box = page.effective_box();

    let mut font_cache = doc.load_page_fonts(&page);
    let content_bytes = doc
        .page_content_bytes(&page)
        .unwrap_or_else(|e| panic!("content bytes of {path:?}: {e}"));

    let mut image_cache = ImageCache::new();
    let annotations = doc.page_annotations(&page);
    let oc_config = doc.oc_config();
    let mut icc_cache = IccCache::new();
    let doc_intents = doc.output_intents();
    let oi_cmyk = zpdf::output_intent_cmyk_profile(
        doc.file(),
        doc.page_output_intents(&page),
        &doc_intents,
        &mut icc_cache,
    );
    let mut interpreter = zpdf::ContentInterpreter::new(page_box)
        .with_page_rotation(page.rotate)
        .with_fonts(&mut font_cache)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut image_cache)
        .with_colors(&mut icc_cache)
        .with_annotations(&annotations)
        .with_operand_stack_limit(doc.file().limits().max_operand_stack_depth as usize);
    if let Some(oc) = &oc_config {
        interpreter = interpreter.with_optional_content(oc);
    }
    if let Some(profile) = oi_cmyk {
        interpreter = interpreter.with_output_intent_cmyk(profile);
    }
    let dl = interpreter.interpret(&content_bytes);

    if std::env::var("ZPDF_BENCH_DEBUG").as_deref() == Ok("1") {
        let (mut f, mut s, mut g, mut c, mut im) = (0u32, 0u32, 0u32, 0u32, 0u32);
        for cmd in &dl.commands {
            use zpdf::display_list::RenderCommand;
            match cmd {
                RenderCommand::FillPath { .. } => f += 1,
                RenderCommand::StrokePath { .. } => s += 1,
                RenderCommand::DrawGlyphRun(_) => g += 1,
                RenderCommand::PushClip { .. } | RenderCommand::PushClipStroke { .. } => c += 1,
                RenderCommand::DrawImage(_) => im += 1,
                _ => {}
            }
        }
        eprintln!(
            "zpdf-benches: {path:?} p{page_idx} -> {} cmds (fills={f} strokes={s} glyphs={g} clips={c} images={im}) fonts={}",
            dl.commands.len(),
            font_cache.len()
        );
    }

    // `ZPDF_BENCH_DL=1`: print the full DL command sequence (one token per cmd)
    // for designing the image-occlusion pass — image draws, clips, blend groups.
    if std::env::var("ZPDF_BENCH_DL").as_deref() == Ok("1") {
        use std::borrow::Cow;
        use zpdf::display_list::{BlendMode, RenderCommand};
        let bm = |b: BlendMode| match b {
            BlendMode::Normal => "N",
            _ => "B",
        };
        let mut seq = String::new();
        for cmd in &dl.commands {
            let tok: Cow<'_, str> = match cmd {
                RenderCommand::FillPath { .. } => "F".into(),
                RenderCommand::StrokePath { .. } => "S".into(),
                RenderCommand::DrawGlyphRun(_) => "g".into(),
                RenderCommand::DrawImage(d) => format!("I{}:a{}", d.image_id, d.alpha).into(),
                RenderCommand::PushClip { .. } | RenderCommand::PushClipStroke { .. } => "(".into(),
                RenderCommand::PopClip => ")".into(),
                RenderCommand::PushBlendGroup {
                    mask,
                    alpha,
                    blend_mode,
                    ..
                } => {
                    let m = if mask.is_some() { "+m" } else { "" };
                    format!("[{}:{}{}", bm(*blend_mode), alpha, m).into()
                }
                RenderCommand::PopBlendGroup => "]".into(),
            };
            seq.push_str(&tok);
            seq.push(' ');
        }
        eprintln!("zpdf-benches DL {path:?} p{page_idx}: {seq}");
    }

    PageSetup {
        dl,
        font_cache,
        image_cache,
        scale: dpi / 72.0,
    }
}

/// A corpus entry: `(label, path relative to workspace root, 0-based page)`.
/// Picked from the real PDFs already in `tests/` — small/medium/large, text /
/// vector / image-heavy, Chinese + English. Paths are resolved against the
/// workspace root (computed from `CARGO_MANIFEST_DIR`) so `cargo bench` works
/// from the crate dir.
pub fn corpus() -> Vec<(&'static str, PathBuf, usize)> {
    let root = workspace_root();
    // (label, relative path, page). `None`-friendly: entries whose file is
    // missing are filtered out by `existing_corpus` at bench time, so a missing
    // PDF never aborts the whole bench run.
    let raw: &[(&str, &str, usize)] = &[
        // text-heavy (M1 targets)
        (
            "text-testpdf-ai",
            "tests/testpdf/科幻电影中的AI伦理冲突.pdf",
            0,
        ),
        ("text-test8", "tests/test8/1.pdf", 0),
        ("text-zzztest2", "tests/zzztest/2.pdf", 0),
        // small quick mix
        ("small-test6", "tests/test6/1.pdf", 0),
        ("small-test3", "tests/test3/17.pdf", 0),
        // image-heavy (M2/M3 targets)
        ("image-test10", "tests/test10/1.pdf", 0),
    ];
    raw.iter()
        .map(|(label, rel, page)| (*label, root.join(rel), *page))
        .collect()
}

/// Filter the corpus to entries whose file actually exists. Called at the top
/// of each bench group so a missing file is skipped (with a stderr note) rather
/// than panicking — lets the bench run on a checkout that lacks some test PDFs.
pub fn existing_corpus() -> Vec<(&'static str, PathBuf, usize)> {
    corpus()
        .into_iter()
        .filter(|(_, path, _)| {
            if path.exists() {
                true
            } else {
                eprintln!("zpdf-benches: skipping missing {}", path.display());
                false
            }
        })
        .collect()
}

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = crates/zpdf-benches; go up two for the workspace root.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .unwrap_or(manifest)
}
