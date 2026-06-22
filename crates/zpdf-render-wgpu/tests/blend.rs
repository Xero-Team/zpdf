//! Blend-group integration tests. The content interpreter does not yet emit
//! PushBlendGroup ops, so we build DisplayLists programmatically and check the GPU
//! layered path against (a) explicit blend math and (b) the CPU oracle.

use std::sync::Arc;
use zpdf_core::Rect;
use zpdf_display_list::{
    BlendMode, Color, DisplayList, FillRule, Paint, Path, RenderCommand, SoftMask, SoftMaskKind,
};
use zpdf_render::RenderBackend;
use zpdf_render_cpu::CpuRenderer;
use zpdf_render_wgpu::WgpuRenderer;

const W: f64 = 100.0;
const H: f64 = 100.0;
const SCALE: f32 = 2.0;

fn rect_path(x0: f64, y0: f64, x1: f64, y1: f64) -> Path {
    let mut p = Path::new();
    p.rect(Rect::new(x0, y0, x1, y1));
    p
}

fn fill(c: Color, r: (f64, f64, f64, f64)) -> RenderCommand {
    RenderCommand::FillPath {
        path: rect_path(r.0, r.1, r.2, r.3),
        rule: FillRule::NonZero,
        paint: Paint::Solid(c),
        alpha: 1.0,
        overprint: None,
    }
}

/// Blue page; a transparency group (given mode) paints a red rect over the right half.
fn build(mode: BlendMode) -> DisplayList {
    let mut dl = DisplayList::new(Rect::new(0.0, 0.0, W, H));
    dl.push(fill(Color::rgb(0.0, 0.0, 1.0), (0.0, 0.0, W, H))); // base: blue
    dl.push(RenderCommand::PushBlendGroup {
        blend_mode: mode,
        isolated: false,
        knockout: false,
        bounds: Rect::new(0.0, 0.0, W, H),
        alpha: 1.0,
        mask: None,
    });
    dl.push(fill(Color::rgb(1.0, 0.0, 0.0), (W / 2.0, 0.0, W, H))); // group: red, right half
    dl.push(RenderCommand::PopBlendGroup);
    dl
}

/// Render with the GPU backend, or `None` if no GPU adapter is available (CI).
fn gpu_render(dl: &DisplayList) -> Option<(u32, u32, Vec<u8>)> {
    let mut r = WgpuRenderer::new();
    match r.render_display_list(dl, SCALE) {
        Ok(t) => Some((t.width, t.height, t.data)),
        Err(e) => {
            eprintln!("skipping GPU blend test (no adapter?): {e}");
            None
        }
    }
}

fn px(data: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * w + x) * 4) as usize;
    [data[i], data[i + 1], data[i + 2], data[i + 3]]
}

#[test]
fn multiply_group_blends_correctly() {
    // Multiply(blue backdrop, red source) = (0,0,0) over the right half; left stays blue.
    let dl = build(BlendMode::Multiply);
    let Some((w, _h, data)) = gpu_render(&dl) else {
        return;
    };
    let left = px(&data, w, w / 4, w / 4); // device px, left half -> blue
    let right = px(&data, w, w * 3 / 4, w / 4); // right half -> black (multiply)
    assert!(
        left[2] > 200 && left[0] < 40,
        "left should be blue, got {left:?}"
    );
    assert!(
        right[0] < 40 && right[1] < 40 && right[2] < 40,
        "right (multiply) should be ~black, got {right:?}"
    );
}

#[test]
fn normal_group_is_source_over() {
    // Normal: red source over blue backdrop -> red on the right, blue on the left.
    let dl = build(BlendMode::Normal);
    let Some((w, _h, data)) = gpu_render(&dl) else {
        return;
    };
    let right = px(&data, w, w * 3 / 4, w / 4);
    assert!(
        right[0] > 200 && right[2] < 40,
        "right (normal) should be red, got {right:?}"
    );
}

/// A transparency group painted at group constant alpha 0.5 must match the CPU
/// oracle (and read as ~50% red over the blue backdrop).
#[test]
fn group_alpha_matches_cpu_oracle() {
    let mut dl = DisplayList::new(Rect::new(0.0, 0.0, W, H));
    dl.push(fill(Color::rgb(0.0, 0.0, 1.0), (0.0, 0.0, W, H))); // blue base
    dl.push(RenderCommand::PushBlendGroup {
        blend_mode: BlendMode::Normal,
        isolated: true,
        knockout: false,
        bounds: Rect::new(0.0, 0.0, W, H),
        alpha: 0.5,
        mask: None,
    });
    dl.push(fill(Color::rgb(1.0, 0.0, 0.0), (0.0, 0.0, W, H))); // red fill
    dl.push(RenderCommand::PopBlendGroup);

    let Some((gw, gh, gpu)) = gpu_render(&dl) else {
        return;
    };
    let cpu = CpuRenderer::new()
        .render_display_list(&dl, SCALE)
        .expect("cpu render");
    assert_eq!((gw, gh), (cpu.width, cpu.height));

    // Center: 50% red over blue ≈ (128, 0, 128).
    let c = px(&gpu, gw, gw / 2, gh / 2);
    assert!(
        c[0] > 100 && c[0] < 160 && c[1] < 40 && c[2] > 100 && c[2] < 160,
        "group alpha 0.5: expected ~50% red over blue, got {c:?}"
    );

    let total = (gw * gh) as u64;
    let mut diff = 0u64;
    for i in 0..total as usize {
        let b = i * 4;
        let dr = (gpu[b] as i32 - cpu.data[b] as i32).unsigned_abs();
        let dg = (gpu[b + 1] as i32 - cpu.data[b + 1] as i32).unsigned_abs();
        let db = (gpu[b + 2] as i32 - cpu.data[b + 2] as i32).unsigned_abs();
        if dr.max(dg).max(db) > 16 {
            diff += 1;
        }
    }
    let pct = diff as f64 / total as f64 * 100.0;
    assert!(pct < 1.0, "group alpha: GPU vs CPU {pct:.3}% differing");
}

/// A luminosity soft mask (white left half over /BC=0) masks a red group: the
/// red shows on the left (coverage 1) and the blue backdrop shows on the right
/// (coverage 0). Must match the CPU oracle.
#[test]
fn luminosity_soft_mask_matches_cpu_oracle() {
    let mut mask_dl = DisplayList::new(Rect::new(0.0, 0.0, W, H));
    mask_dl.push(fill(Color::rgb(1.0, 1.0, 1.0), (0.0, 0.0, W / 2.0, H))); // white left half
    let mask = SoftMask {
        kind: SoftMaskKind::Luminosity,
        commands: Arc::new(mask_dl),
        offset: (0.0, 0.0),
        backdrop_luma: 0.0,
        transfer: None,
    };

    let mut dl = DisplayList::new(Rect::new(0.0, 0.0, W, H));
    dl.push(fill(Color::rgb(0.0, 0.0, 1.0), (0.0, 0.0, W, H))); // blue base
    dl.push(RenderCommand::PushBlendGroup {
        blend_mode: BlendMode::Normal,
        isolated: true,
        knockout: false,
        bounds: Rect::new(0.0, 0.0, W, H),
        alpha: 1.0,
        mask: Some(mask),
    });
    dl.push(fill(Color::rgb(1.0, 0.0, 0.0), (0.0, 0.0, W, H))); // red group
    dl.push(RenderCommand::PopBlendGroup);

    let Some((gw, gh, gpu)) = gpu_render(&dl) else {
        return;
    };
    let cpu = CpuRenderer::new()
        .render_display_list(&dl, SCALE)
        .expect("cpu render");
    assert_eq!((gw, gh), (cpu.width, cpu.height));

    // Left: masked-in red; right: masked-out → blue backdrop.
    let left = px(&gpu, gw, gw / 4, gh / 2);
    let right = px(&gpu, gw, gw * 3 / 4, gh / 2);
    assert!(
        left[0] > 200 && left[2] < 50,
        "mask left should be red, got {left:?}"
    );
    assert!(
        right[2] > 200 && right[0] < 50,
        "mask right should be blue (masked out), got {right:?}"
    );

    let total = (gw * gh) as u64;
    let mut diff = 0u64;
    for i in 0..total as usize {
        let b = i * 4;
        let dr = (gpu[b] as i32 - cpu.data[b] as i32).unsigned_abs();
        let dg = (gpu[b + 1] as i32 - cpu.data[b + 1] as i32).unsigned_abs();
        let db = (gpu[b + 2] as i32 - cpu.data[b + 2] as i32).unsigned_abs();
        if dr.max(dg).max(db) > 16 {
            diff += 1;
        }
    }
    let pct = diff as f64 / total as f64 * 100.0;
    assert!(pct < 1.0, "luminosity mask: GPU vs CPU {pct:.3}% differing");
}

/// An alpha soft mask (opaque left half) masks a red group by the mask group's
/// alpha: red shows on the left, the backdrop on the right. Matches CPU.
#[test]
fn alpha_soft_mask_matches_cpu_oracle() {
    let mut mask_dl = DisplayList::new(Rect::new(0.0, 0.0, W, H));
    mask_dl.push(fill(Color::rgb(0.0, 1.0, 0.0), (0.0, 0.0, W / 2.0, H))); // opaque left half
    let mask = SoftMask {
        kind: SoftMaskKind::Alpha,
        commands: Arc::new(mask_dl),
        offset: (0.0, 0.0),
        backdrop_luma: 0.0,
        transfer: None,
    };

    let mut dl = DisplayList::new(Rect::new(0.0, 0.0, W, H));
    dl.push(fill(Color::rgb(0.0, 0.0, 1.0), (0.0, 0.0, W, H))); // blue base
    dl.push(RenderCommand::PushBlendGroup {
        blend_mode: BlendMode::Normal,
        isolated: true,
        knockout: false,
        bounds: Rect::new(0.0, 0.0, W, H),
        alpha: 1.0,
        mask: Some(mask),
    });
    dl.push(fill(Color::rgb(1.0, 0.0, 0.0), (0.0, 0.0, W, H))); // red group
    dl.push(RenderCommand::PopBlendGroup);

    let Some((gw, gh, gpu)) = gpu_render(&dl) else {
        return;
    };
    let cpu = CpuRenderer::new()
        .render_display_list(&dl, SCALE)
        .expect("cpu render");

    let left = px(&gpu, gw, gw / 4, gh / 2);
    let right = px(&gpu, gw, gw * 3 / 4, gh / 2);
    assert!(
        left[0] > 200 && left[2] < 50,
        "alpha mask left red: {left:?}"
    );
    assert!(
        right[2] > 200 && right[0] < 50,
        "alpha mask right blue: {right:?}"
    );

    let total = (gw * gh) as u64;
    let mut diff = 0u64;
    for i in 0..total as usize {
        let b = i * 4;
        let d = (gpu[b] as i32 - cpu.data[b] as i32)
            .unsigned_abs()
            .max((gpu[b + 1] as i32 - cpu.data[b + 1] as i32).unsigned_abs())
            .max((gpu[b + 2] as i32 - cpu.data[b + 2] as i32).unsigned_abs());
        if d > 16 {
            diff += 1;
        }
    }
    let pct = diff as f64 / total as f64 * 100.0;
    assert!(pct < 1.0, "alpha mask: GPU vs CPU {pct:.3}% differing");
}

#[test]
fn blend_groups_match_cpu_oracle() {
    for mode in [
        BlendMode::Normal,
        BlendMode::Multiply,
        BlendMode::Screen,
        BlendMode::Darken,
        BlendMode::Lighten,
        BlendMode::Difference,
    ] {
        let dl = build(mode);
        let Some((gw, gh, gpu)) = gpu_render(&dl) else {
            return;
        };
        let cpu = CpuRenderer::new()
            .render_display_list(&dl, SCALE)
            .expect("cpu render");
        assert_eq!(
            (gw, gh),
            (cpu.width, cpu.height),
            "dims differ for {mode:?}"
        );

        let total = (gw * gh) as u64;
        let mut diff = 0u64;
        for i in 0..total as usize {
            let b = i * 4;
            let dr = (gpu[b] as i32 - cpu.data[b] as i32).unsigned_abs();
            let dg = (gpu[b + 1] as i32 - cpu.data[b + 1] as i32).unsigned_abs();
            let db = (gpu[b + 2] as i32 - cpu.data[b + 2] as i32).unsigned_abs();
            if dr.max(dg).max(db) > 16 {
                diff += 1;
            }
        }
        let pct = diff as f64 / total as f64 * 100.0;
        assert!(pct < 1.0, "mode {mode:?}: GPU vs CPU {pct:.3}% differing");
    }
}
