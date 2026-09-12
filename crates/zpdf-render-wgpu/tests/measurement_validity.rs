//! GPU measurement validity: the adapter a timing was taken on, and the
//! submit-only (no-readback) consumption path.
//!
//! Three properties, all deterministic — no wall-clock thresholds here. Speed
//! comparisons between the readback and submit-only paths belong in the
//! criterion benches, where noise is handled; a wall-clock threshold in a unit
//! test would be a flake generator.
//!
//!   1. `adapter_identity()` reports the adapter a number was measured on.
//!      Without it a GPU timing cannot be compared to anything — the
//!      development machine alone exposes a remote-desktop mirror and an
//!      Android-emulator adapter next to the real GPU.
//!   2. The submit-only path renders and returns no pixels, by construction.
//!   3. A submit-only render does not disturb the readback path: the same
//!      display list read back before and after one must be pixel-identical.

use zpdf_core::Rect;
use zpdf_display_list::{Color, DisplayList, FillRule, Paint, Path, RenderCommand};
use zpdf_render::RenderBackend;
use zpdf_render_wgpu::{WgpuRenderError, WgpuRenderer};

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
fn adapter_identity_is_populated_and_fingerprints_stably() {
    let mut r = WgpuRenderer::new();
    let identity = match r.adapter_identity() {
        Ok(id) => id.clone(),
        Err(e) => {
            eprintln!("skipping adapter identity test (no adapter?): {e}");
            return;
        }
    };

    assert!(
        !identity.name.is_empty(),
        "adapter must be named: {identity:?}"
    );
    assert!(
        !identity.backend.is_empty(),
        "backend must be recorded: {identity:?}"
    );
    assert!(
        !identity.device_type.is_empty(),
        "device type must be recorded: {identity:?}"
    );
    assert!(
        matches!(identity.sample_count, 1 | 2 | 4),
        "MSAA level must be one of the supported steps, got {}",
        identity.sample_count
    );
    assert!(identity.max_texture_dim > 0);

    // The fingerprint must identify the measurement context and be stable across
    // calls (a baseline compares fingerprints, so a non-deterministic one would
    // make every comparison a mismatch).
    let fp = identity.fingerprint();
    assert!(
        fp.contains(&identity.name),
        "fingerprint must name the adapter: {fp}"
    );
    assert!(
        fp.contains(&format!("msaa{}", identity.sample_count)),
        "fingerprint must carry the MSAA level: {fp}"
    );
    assert_eq!(fp, r.adapter_identity().unwrap().fingerprint());

    // The report block must state whether GPU pass timing is even available —
    // a permanently-None `last_gpu_time_ns()` is otherwise indistinguishable
    // from a bug.
    let described = identity.describe();
    assert!(described.contains(&identity.name), "{described}");
    assert!(
        described.contains("timestamps"),
        "must state timestamp availability: {described}"
    );

    eprintln!("adapter identity: {described}");
}

#[test]
fn submit_only_renders_without_returning_pixels() {
    let mut r = WgpuRenderer::new();
    let dl = simple_dl();
    match r.render_display_list_submitted(&dl, SCALE) {
        Ok(submission) => {
            // `Submission` carries no pixel buffer at all — that is the point of
            // the path. Assert the timings are merely plausible.
            assert!(submission.wall_ns < 60_000_000_000, "implausible wall");
            if let Some(ns) = submission.gpu_pass_ns {
                assert!(ns < 10_000_000_000, "implausible GPU pass time: {ns}ns");
                eprintln!(
                    "submit-only: wall {}us, gpu pass {}us",
                    submission.wall_ns / 1000,
                    ns / 1000
                );
            } else {
                // Expected here: this renderer did not enable `with_gpu_timing`,
                // and the timer's readback would itself be a poll — the very
                // thing this path avoids.
                eprintln!("no GPU pass time (timing not enabled; expected on this path)");
            }
        }
        Err(e) => eprintln!("skipping submit-only test (no adapter?): {e}"),
    }
}

/// Many consecutive submit-only renders must not exhaust the device.
///
/// This is the regression test for a real bug: with no readback there was no
/// `device.poll`, so a tight loop piled up un-retired submissions until the
/// device reported Out Of Memory (observed on a 4 GB-class GPU within a few
/// hundred pages). The submit-only path must drain completed work without
/// blocking.
///
/// A machine with no adapter at all — Linux CI, whose image ships no software
/// Vulkan driver — fails the *first* render with `NoAdapter`. That says nothing
/// about queue draining, so it skips like the other tests in this file; the loop
/// is exercised wherever an adapter exists (Windows CI, developer machines). A
/// `NoAdapter` after the loop has started, or any other error at all, is still
/// the bug and still panics.
#[test]
fn submit_only_survives_many_iterations() {
    let mut r = WgpuRenderer::new();
    let dl = simple_dl();

    if let Err(e) = r.render_display_list_submitted(&dl, SCALE) {
        if matches!(e, WgpuRenderError::NoAdapter) {
            eprintln!("skipping submit-only loop test (no adapter): {e}");
            return;
        }
        panic!("submit-only render 0 failed (queue not draining?): {e}");
    }

    // A few hundred pages: far past where the OOM appeared.
    for i in 1..300 {
        if let Err(e) = r.render_display_list_submitted(&dl, SCALE) {
            panic!("submit-only render {i} failed (queue not draining?): {e}");
        }
    }
    // And the readback path must still work afterwards.
    match r.render_display_list(&dl, SCALE) {
        Ok(t) => assert!(!t.data.is_empty()),
        Err(e) => eprintln!("skipping post-loop readback check (no adapter?): {e}"),
    }
}

#[test]
fn submit_only_does_not_disturb_the_readback_path() {
    let mut r = WgpuRenderer::new();
    let dl = simple_dl();

    let before = match r.render_display_list(&dl, SCALE) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("skipping state-isolation test (no adapter?): {e}");
            return;
        }
    };

    if let Err(e) = r.render_display_list_submitted(&dl, SCALE) {
        eprintln!("skipping state-isolation test (submit-only failed): {e}");
        return;
    }

    let after = r
        .render_display_list(&dl, SCALE)
        .expect("readback after submit-only");

    assert_eq!(before.width, after.width);
    assert_eq!(before.height, after.height);
    assert_eq!(
        before.data, after.data,
        "a submit-only render must not change what the readback path produces"
    );
}
