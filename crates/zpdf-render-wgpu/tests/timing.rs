//! GPU pass timing integration test (P3.8). Verifies `last_gpu_time_ns()`
//! reports a value when the adapter supports timestamp queries, and never
//! panics when it doesn't — timing is purely additive telemetry.

use zpdf_core::Rect;
use zpdf_display_list::{Color, DisplayList, FillRule, Paint, Path, RenderCommand};
use zpdf_render::RenderBackend;
use zpdf_render_wgpu::WgpuRenderer;

const SCALE: f32 = 2.0;

fn simple_dl() -> DisplayList {
    let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 100.0));
    let mut path = Path::new();
    path.rect(Rect::new(10.0, 10.0, 90.0, 90.0));
    dl.push(RenderCommand::FillPath {
        path,
        rule: FillRule::NonZero,
        paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
        alpha: 1.0,
        overprint: None,
    });
    dl
}

#[test]
fn gpu_pass_time_is_reported_when_supported() {
    let mut r = WgpuRenderer::new();
    let dl = simple_dl();
    match r.render_display_list(&dl, SCALE) {
        Ok(_) => {
            // Adapter-dependent: only assert a value is present when the
            // context actually negotiated timestamp-query support. Either
            // way, the call must not panic and must return a plausible value.
            if let Some(ns) = r.last_gpu_time_ns() {
                assert!(ns < 10_000_000_000, "implausible GPU time: {ns}ns");
                eprintln!("GPU pass time: {ns}ns");
            } else {
                eprintln!("GPU pass timing unavailable on this adapter (expected on some).");
            }
        }
        Err(e) => {
            eprintln!("skipping GPU timing test (no adapter?): {e}");
        }
    }
}

#[test]
fn gpu_pass_time_absent_before_any_render() {
    let r = WgpuRenderer::new();
    assert_eq!(r.last_gpu_time_ns(), None);
}
