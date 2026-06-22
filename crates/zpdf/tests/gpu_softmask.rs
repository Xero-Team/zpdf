//! GPU<->CPU acceptance for ExtGState /SMask soft-mask fidelity, built directly
//! as `DisplayList`s (these features are awkward to provoke from hand-written PDF
//! content, and a programmatic list pins the exact `SoftMask` fields under test).
//!
//! Covers the three GPU residuals closed against the tiny-skia oracle:
//!   * `/TR` transfer function — the reduced coverage runs through the LUT;
//!   * tiling-pattern reuse `offset` — the coverage plane is sampled shifted;
//!   * a transparency group **nested inside** the mask group — composited through
//!     the layered path rather than dropping the mask.
//!
//! Gated on `gpu-render`; skips gracefully when no GPU adapter is available.
#![cfg(feature = "gpu-render")]

use std::sync::Arc;
use zpdf::display_list::{
    BlendMode, Color, DisplayList, FillRule, Paint, Path, RenderCommand, SoftMask, SoftMaskKind,
};
use zpdf::{Rect, RenderBackend};

const SCALE: f32 = 2.0;
const THRESHOLD: u8 = 16;
const MAX_DIFF_PCT: f64 = 1.0;

fn page() -> Rect {
    Rect {
        x0: 0.0,
        y0: 0.0,
        x1: 200.0,
        y1: 200.0,
    }
}

fn rect_path(x0: f64, y0: f64, x1: f64, y1: f64) -> Path {
    let mut p = Path::new();
    p.rect(Rect { x0, y0, x1, y1 });
    p
}

fn fill(x0: f64, y0: f64, x1: f64, y1: f64, c: Color, alpha: f32) -> RenderCommand {
    RenderCommand::FillPath {
        path: rect_path(x0, y0, x1, y1),
        rule: FillRule::NonZero,
        paint: Paint::Solid(c),
        alpha,
        overprint: None,
    }
}

/// A masked, isolated, Normal-blend group of one rect — the shape the interpreter
/// emits for a single object painted while an /SMask is in the graphics state.
fn masked_group(content: RenderCommand, mask: SoftMask) -> Vec<RenderCommand> {
    vec![
        RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Normal,
            isolated: true,
            knockout: false,
            bounds: page(),
            alpha: 1.0,
            mask: Some(mask),
        },
        content,
        RenderCommand::PopBlendGroup,
    ]
}

/// Build a full page: a flat background, then the masked group on top.
fn page_with(background: Color, group: Vec<RenderCommand>) -> DisplayList {
    let mut dl = DisplayList::new(page());
    dl.push(fill(0.0, 0.0, 200.0, 200.0, background, 1.0));
    for c in group {
        dl.push(c);
    }
    dl
}

/// (differing %, cpu_dims, gpu_dims).
type BackendDiff = (f64, (u32, u32), (u32, u32));

/// Render `dl` with both backends; returns the diff, or `None` when no GPU
/// adapter is present.
fn compare(dl: &DisplayList) -> Option<BackendDiff> {
    let gpu = match zpdf::gpu::WgpuRenderer::new().render_display_list(dl, SCALE) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("skipping GPU soft-mask acceptance (no adapter?): {e}");
            return None;
        }
    };
    let cpu = zpdf::cpu::CpuRenderer::new()
        .render_display_list(dl, SCALE)
        .expect("cpu render");

    if (cpu.width, cpu.height) != (gpu.width, gpu.height) {
        return Some((100.0, (cpu.width, cpu.height), (gpu.width, gpu.height)));
    }
    let total = (cpu.width * cpu.height) as u64;
    let mut diff = 0u64;
    for i in 0..total as usize {
        let b = i * 4;
        let dr = (gpu.data[b] as i32 - cpu.data[b] as i32).unsigned_abs();
        let dg = (gpu.data[b + 1] as i32 - cpu.data[b + 1] as i32).unsigned_abs();
        let db = (gpu.data[b + 2] as i32 - cpu.data[b + 2] as i32).unsigned_abs();
        if dr.max(dg).max(db) > THRESHOLD as u32 {
            diff += 1;
        }
    }
    Some((
        diff as f64 / total as f64 * 100.0,
        (cpu.width, cpu.height),
        (gpu.width, gpu.height),
    ))
}

fn assert_match(name: &str, dl: &DisplayList) {
    match compare(dl) {
        None => eprintln!("GPU soft-mask `{name}` skipped (no adapter)."),
        Some((pct, cdim, gdim)) => {
            assert_eq!(
                cdim, gdim,
                "{name}: dimension mismatch {cdim:?} vs {gdim:?}"
            );
            println!("  softmask {name}: {pct:.3}% differing");
            assert!(
                pct < MAX_DIFF_PCT,
                "{name}: GPU vs CPU {pct:.3}% exceeds {MAX_DIFF_PCT}%"
            );
        }
    }
}

/// /TR transfer function: a luminosity mask whose group paints a 0.25-gray square
/// (luminosity 0.25); an inverting transfer LUT lifts the coverage to ~0.75. The
/// GPU must apply the same LUT, or the masked blue square would show at a quarter
/// rather than three-quarters opacity over the yellow background.
#[test]
fn gpu_matches_cpu_softmask_transfer() {
    let mask_dl = {
        let mut dl = DisplayList::new(page());
        dl.push(fill(
            40.0,
            40.0,
            160.0,
            160.0,
            Color::rgb(0.25, 0.25, 0.25),
            1.0,
        ));
        dl
    };
    // Inverting /TR (pre-sampled, as the interpreter delivers it).
    let mut lut = [0u8; 256];
    for (i, v) in lut.iter_mut().enumerate() {
        *v = 255 - i as u8;
    }
    let mask = SoftMask {
        kind: SoftMaskKind::Luminosity,
        commands: Arc::new(mask_dl),
        offset: (0.0, 0.0),
        backdrop_luma: 0.0,
        transfer: Some(Arc::new(lut)),
    };
    let group = masked_group(
        fill(40.0, 40.0, 160.0, 160.0, Color::rgb(0.0, 0.0, 1.0), 1.0),
        mask,
    );
    assert_match("transfer", &page_with(Color::rgb(1.0, 1.0, 0.0), group));
}

/// Tiling-pattern reuse offset: a luminosity mask built for a white square in the
/// lower-left is reused with a page-space `offset` of (40, 40). The coverage plane
/// must shift with it (right and up in device space); the masked blue square then
/// appears in the shifted region only. Reads outside the built mask fall to the
/// /BC backdrop (0), matching the CPU's vacated-strip fill.
#[test]
fn gpu_matches_cpu_softmask_offset() {
    let mask_dl = {
        let mut dl = DisplayList::new(page());
        dl.push(fill(20.0, 20.0, 100.0, 100.0, Color::white(), 1.0));
        dl
    };
    let mask = SoftMask {
        kind: SoftMaskKind::Luminosity,
        commands: Arc::new(mask_dl),
        offset: (40.0, 40.0),
        backdrop_luma: 0.0,
        transfer: None,
    };
    let group = masked_group(
        fill(20.0, 20.0, 180.0, 180.0, Color::rgb(0.0, 0.0, 1.0), 1.0),
        mask,
    );
    assert_match("offset", &page_with(Color::rgb(1.0, 1.0, 0.0), group));
}

/// A transparency group nested inside the mask group: the mask paints a white
/// square through an isolated, 0.6-alpha sub-group, so its composited luminosity
/// is ~0.6 inside the square and 0 outside. The GPU must composite the mask group
/// through the layered path; previously it dropped the mask on seeing the nested
/// group and drew the red square fully opaque.
#[test]
fn gpu_matches_cpu_softmask_nested_group() {
    let mask_dl = {
        let mut dl = DisplayList::new(page());
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Normal,
            isolated: true,
            knockout: false,
            bounds: page(),
            alpha: 0.6,
            mask: None,
        });
        dl.push(fill(30.0, 30.0, 170.0, 170.0, Color::white(), 1.0));
        dl.push(RenderCommand::PopBlendGroup);
        dl
    };
    let mask = SoftMask {
        kind: SoftMaskKind::Luminosity,
        commands: Arc::new(mask_dl),
        offset: (0.0, 0.0),
        backdrop_luma: 0.0,
        transfer: None,
    };
    let group = masked_group(
        fill(30.0, 30.0, 170.0, 170.0, Color::rgb(1.0, 0.0, 0.0), 1.0),
        mask,
    );
    assert_match("nested_group", &page_with(Color::rgb(1.0, 1.0, 0.0), group));
}

/// An alpha mask with a non-trivial /TR: coverage is the mask group's alpha
/// (0.5 here via a half-transparent fill), then transferred. Guards the alpha
/// branch + transfer interaction on both backends.
#[test]
fn gpu_matches_cpu_softmask_alpha_transfer() {
    let mask_dl = {
        let mut dl = DisplayList::new(page());
        dl.push(fill(50.0, 50.0, 150.0, 150.0, Color::white(), 0.5));
        dl
    };
    // Gamma-ish transfer: square the coverage (0.5 -> ~0.25).
    let mut lut = [0u8; 256];
    for (i, v) in lut.iter_mut().enumerate() {
        let x = i as f32 / 255.0;
        *v = (x * x * 255.0).round() as u8;
    }
    let mask = SoftMask {
        kind: SoftMaskKind::Alpha,
        commands: Arc::new(mask_dl),
        offset: (0.0, 0.0),
        backdrop_luma: 0.0,
        transfer: Some(Arc::new(lut)),
    };
    let group = masked_group(
        fill(50.0, 50.0, 150.0, 150.0, Color::rgb(0.0, 0.0, 1.0), 1.0),
        mask,
    );
    assert_match(
        "alpha_transfer",
        &page_with(Color::rgb(1.0, 1.0, 0.0), group),
    );
}

/// Colored luminosity mask + a steep /TR — a regression guard for the luminosity
/// reduction weights. The mask group is *pure blue* (luma 0.114 → byte 29 under
/// Rec.601), and the /TR is a step at 29, so the masked square is fully shown iff
/// the coverage byte is ≥ 29. If the GPU used the (0.30,0.59,0.11) blend-mode
/// weights instead of the oracle's Rec.601, pure blue would reduce to byte 28 →
/// the step would hide the whole square → ~100% divergence. Gray/white masks
/// (the other tests) can't catch this because both weight sets sum to 1.0.
#[test]
fn gpu_matches_cpu_softmask_colored_luminosity() {
    let mask_dl = {
        let mut dl = DisplayList::new(page());
        dl.push(fill(
            40.0,
            40.0,
            160.0,
            160.0,
            Color::rgb(0.0, 0.0, 1.0),
            1.0,
        ));
        dl
    };
    let mut lut = [0u8; 256];
    for (i, v) in lut.iter_mut().enumerate() {
        *v = if i >= 29 { 255 } else { 0 };
    }
    let mask = SoftMask {
        kind: SoftMaskKind::Luminosity,
        commands: Arc::new(mask_dl),
        offset: (0.0, 0.0),
        backdrop_luma: 0.0,
        transfer: Some(Arc::new(lut)),
    };
    let group = masked_group(
        fill(40.0, 40.0, 160.0, 160.0, Color::rgb(1.0, 0.0, 0.0), 1.0),
        mask,
    );
    assert_match(
        "colored_luminosity",
        &page_with(Color::rgb(1.0, 1.0, 0.0), group),
    );
}

/// A mask nested inside a mask: the outer mask's group is itself a group carrying
/// its own sub-mask. Drives the recursion `composite_into → apply_soft_mask →
/// composite_into → apply_soft_mask` and the layer-pool recycling at depth, plus
/// the recursive image/op walk. The outer red square is visible only where the
/// inner sub-mask (a white square in the lower-left) lets the outer mask paint.
#[test]
fn gpu_matches_cpu_softmask_mask_in_mask() {
    // Inner sub-mask: white square over the lower-left quadrant.
    let inner_dl = {
        let mut dl = DisplayList::new(page());
        dl.push(fill(30.0, 30.0, 100.0, 100.0, Color::white(), 1.0));
        dl
    };
    let inner = SoftMask {
        kind: SoftMaskKind::Luminosity,
        commands: Arc::new(inner_dl),
        offset: (0.0, 0.0),
        backdrop_luma: 0.0,
        transfer: None,
    };
    // Outer mask group: a group (carrying the inner sub-mask) painting white.
    let outer_dl = {
        let mut dl = DisplayList::new(page());
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Normal,
            isolated: true,
            knockout: false,
            bounds: page(),
            alpha: 1.0,
            mask: Some(inner),
        });
        dl.push(fill(30.0, 30.0, 170.0, 170.0, Color::white(), 1.0));
        dl.push(RenderCommand::PopBlendGroup);
        dl
    };
    let outer = SoftMask {
        kind: SoftMaskKind::Luminosity,
        commands: Arc::new(outer_dl),
        offset: (0.0, 0.0),
        backdrop_luma: 0.0,
        transfer: None,
    };
    let group = masked_group(
        fill(30.0, 30.0, 170.0, 170.0, Color::rgb(1.0, 0.0, 0.0), 1.0),
        outer,
    );
    assert_match("mask_in_mask", &page_with(Color::rgb(1.0, 1.0, 0.0), group));
}
