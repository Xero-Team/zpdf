//! GPU↔CPU acceptance for overprint (PDF 8.6.7), built directly as
//! `DisplayList`s so the exact `Overprint` colorant/mask fields are pinned.
//!
//! An overprinting element is composited against the backdrop in naïve
//! subtractive CMYK on both backends; this guards that the wgpu offscreen-layer
//! + overprint composite shader matches the tiny-skia oracle.
//!
//! Gated on `gpu-render`; skips gracefully when no GPU adapter is available.
#![cfg(feature = "gpu-render")]

use zpdf::display_list::{
    Color, DisplayList, FillRule, Overprint, Paint, Path, RenderCommand, StrokeStyle,
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

fn fill(x0: f64, y0: f64, x1: f64, y1: f64, c: Color) -> RenderCommand {
    RenderCommand::FillPath {
        path: rect_path(x0, y0, x1, y1),
        rule: FillRule::NonZero,
        paint: Paint::Solid(c),
        alpha: 1.0,
        overprint: None,
    }
}

/// An overprinting fill: `cmyk`/`active` drive the composite; the RGB paint is
/// unused by the overprint path (only its coverage·alpha matters).
fn op_fill(
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    cmyk: [f32; 4],
    active: u8,
    alpha: f32,
) -> RenderCommand {
    RenderCommand::FillPath {
        path: rect_path(x0, y0, x1, y1),
        rule: FillRule::NonZero,
        paint: Paint::Solid(Color::gray(0.5)),
        alpha,
        overprint: Some(Overprint { cmyk, active }),
    }
}

fn op_stroke(path: Path, width: f32, cmyk: [f32; 4], active: u8) -> RenderCommand {
    RenderCommand::StrokePath {
        path,
        style: StrokeStyle {
            width,
            ..StrokeStyle::default()
        },
        paint: Paint::Solid(Color::gray(0.5)),
        alpha: 1.0,
        overprint: Some(Overprint { cmyk, active }),
    }
}

/// Page with a solid background, then `ops` on top.
fn page_with(bg: Color, ops: Vec<RenderCommand>) -> DisplayList {
    let mut dl = DisplayList::new(page());
    dl.push(fill(0.0, 0.0, 200.0, 200.0, bg));
    for c in ops {
        dl.push(c);
    }
    dl
}

/// (differing %, cpu_dims, gpu_dims).
type BackendDiff = (f64, (u32, u32), (u32, u32));

fn compare(dl: &DisplayList) -> Option<BackendDiff> {
    let gpu = match zpdf::gpu::WgpuRenderer::new().render_display_list(dl, SCALE) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("skipping GPU overprint acceptance (no adapter?): {e}");
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
        None => eprintln!("GPU overprint `{name}` skipped (no adapter)."),
        Some((pct, cdim, gdim)) => {
            assert_eq!(
                cdim, gdim,
                "{name}: dimension mismatch {cdim:?} vs {gdim:?}"
            );
            println!("  overprint {name}: {pct:.3}% differing");
            assert!(
                pct < MAX_DIFF_PCT,
                "{name}: GPU vs CPU {pct:.3}% exceeds {MAX_DIFF_PCT}%"
            );
        }
    }
}

/// Cyan colorant overprinting a yellow page → green where it covers.
#[test]
fn gpu_matches_cpu_cyan_over_yellow() {
    let dl = page_with(
        Color::rgb(1.0, 1.0, 0.0),
        vec![op_fill(
            40.0,
            40.0,
            160.0,
            160.0,
            [1.0, 0.0, 0.0, 0.0],
            Overprint::C,
            1.0,
        )],
    );
    assert_match("cyan_over_yellow", &dl);
}

/// Magenta colorant overprinting a cyan page → blue.
#[test]
fn gpu_matches_cpu_magenta_over_cyan() {
    let dl = page_with(
        Color::rgb(0.0, 1.0, 1.0),
        vec![op_fill(
            0.0,
            0.0,
            200.0,
            200.0,
            [0.0, 1.0, 0.0, 0.0],
            Overprint::M,
            1.0,
        )],
    );
    assert_match("magenta_over_cyan", &dl);
}

/// K-only black overprinting a red page (active = K): keeps red's colorants,
/// adds black → darkened. Exercises the retain-vs-paint channel split.
#[test]
fn gpu_matches_cpu_black_over_red() {
    let dl = page_with(
        Color::rgb(1.0, 0.0, 0.0),
        vec![op_fill(
            20.0,
            20.0,
            180.0,
            180.0,
            [0.0, 0.0, 0.0, 0.5],
            Overprint::K,
            1.0,
        )],
    );
    assert_match("black_over_red", &dl);
}

/// Overprint with a constant alpha < 1: the coverage·opacity path must agree.
#[test]
fn gpu_matches_cpu_partial_alpha() {
    let dl = page_with(
        Color::rgb(1.0, 1.0, 0.0),
        vec![op_fill(
            40.0,
            40.0,
            160.0,
            160.0,
            [1.0, 0.0, 0.0, 0.0],
            Overprint::C,
            0.5,
        )],
    );
    assert_match("partial_alpha", &dl);
}

/// A multi-colorant DeviceN-style overprint (C+Y active) over a magenta page.
#[test]
fn gpu_matches_cpu_multi_colorant() {
    let dl = page_with(
        Color::rgb(1.0, 0.0, 1.0),
        vec![op_fill(
            30.0,
            30.0,
            170.0,
            170.0,
            [0.8, 0.0, 0.6, 0.0],
            Overprint::C | Overprint::Y,
            1.0,
        )],
    );
    assert_match("multi_colorant", &dl);
}

/// Overprinting cyan stroke over a yellow page → green along the line.
#[test]
fn gpu_matches_cpu_stroke_overprint() {
    let mut line = Path::new();
    line.move_to(zpdf::Point::new(0.0, 100.0));
    line.line_to(zpdf::Point::new(200.0, 100.0));
    let dl = page_with(
        Color::rgb(1.0, 1.0, 0.0),
        vec![op_stroke(line, 30.0, [1.0, 0.0, 0.0, 0.0], Overprint::C)],
    );
    assert_match("stroke_overprint", &dl);
}
