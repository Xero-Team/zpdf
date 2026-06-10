//! Dash-pattern flattening shared by render backends.
//!
//! tiny-skia has native dashing (the CPU backend uses `tiny_skia::StrokeDash`),
//! but lyon does not — the wgpu backend flattens dashed paths into solid
//! sub-polylines with this helper before stroke tessellation. Inputs are
//! expected in a single consistent space (the wgpu backend passes device
//! pixels: dash array and phase pre-multiplied by the page scale).

/// True when a dash array cannot produce a meaningful on/off pattern and the
/// stroke should be drawn solid instead: empty array, any negative or
/// non-finite entry (invalid per PDF 8.4.3.6), or all entries zero.
pub fn is_degenerate(array: &[f32]) -> bool {
    array.is_empty()
        || array.iter().any(|v| !v.is_finite() || *v < 0.0)
        || array.iter().all(|&v| v == 0.0)
}

/// Split one polyline into the "on" runs of a dash pattern.
///
/// The pattern restarts at the beginning of each sub-path (PDF 8.4.3.6), so
/// callers invoke this once per polyline. An odd-length array repeats, which
/// is equivalent to the array concatenated with itself (`[3]` == `[3, 3]`).
/// Degenerate patterns (see [`is_degenerate`]) return the input unchanged as a
/// single solid run. Zero-length "on" intervals yield two-point runs with
/// identical endpoints (dots under round caps); callers that cannot render
/// those may skip zero-length runs.
pub fn dash_polyline(points: &[[f32; 2]], array: &[f32], phase: f32) -> Vec<Vec<[f32; 2]>> {
    if points.len() < 2 || is_degenerate(array) {
        return vec![points.to_vec()];
    }

    let mut pattern: Vec<f32> = array.to_vec();
    if pattern.len() % 2 == 1 {
        pattern.extend_from_slice(array);
    }
    let period: f32 = pattern.iter().sum();

    // Advance to the pattern position selected by the phase. `is_degenerate`
    // guarantees `period > 0`, so both loops terminate. The `pos > 0` guard keeps
    // a zero-length leading interval (a dot at the path start) from being skipped.
    let mut pos = phase.rem_euclid(period);
    let mut idx = 0usize;
    while pos > 0.0 && pos >= pattern[idx] {
        pos -= pattern[idx];
        idx = (idx + 1) % pattern.len();
    }
    // Even intervals are "on" (drawn), odd are "off" (gaps).
    let mut on = idx.is_multiple_of(2);
    let mut remaining = pattern[idx] - pos;

    let mut runs: Vec<Vec<[f32; 2]>> = Vec::new();
    let mut current: Vec<[f32; 2]> = Vec::new();
    if on {
        current.push(points[0]);
    }

    for w in points.windows(2) {
        let (p0, p1) = (w[0], w[1]);
        let dx = p1[0] - p0[0];
        let dy = p1[1] - p0[1];
        let len = (dx * dx + dy * dy).sqrt();
        if len <= 0.0 {
            continue;
        }
        let mut consumed = 0.0f32;
        while len - consumed > remaining {
            consumed += remaining;
            let k = consumed / len;
            let split = [p0[0] + dx * k, p0[1] + dy * k];
            if on {
                current.push(split);
                runs.push(std::mem::take(&mut current));
            } else {
                current.push(split);
            }
            on = !on;
            idx = (idx + 1) % pattern.len();
            remaining = pattern[idx];
        }
        remaining -= len - consumed;
        if on {
            current.push(p1);
        }
    }
    if on && current.len() >= 2 {
        runs.push(current);
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(x0: f32, x1: f32) -> Vec<[f32; 2]> {
        vec![[x0, 0.0], [x1, 0.0]]
    }

    #[test]
    fn degenerate_arrays_detected() {
        assert!(is_degenerate(&[]));
        assert!(is_degenerate(&[0.0, 0.0]));
        assert!(is_degenerate(&[3.0, -1.0]));
        assert!(is_degenerate(&[f32::NAN]));
        assert!(!is_degenerate(&[3.0]));
        assert!(!is_degenerate(&[0.0, 2.0]));
    }

    #[test]
    fn even_pattern_splits_line() {
        let runs = dash_polyline(&line(0.0, 20.0), &[4.0, 4.0], 0.0);
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0], vec![[0.0, 0.0], [4.0, 0.0]]);
        assert_eq!(runs[1], vec![[8.0, 0.0], [12.0, 0.0]]);
        assert_eq!(runs[2], vec![[16.0, 0.0], [20.0, 0.0]]);
    }

    #[test]
    fn odd_array_behaves_doubled() {
        // [4] is equivalent to [4, 4] per PDF dash semantics.
        let a = dash_polyline(&line(0.0, 20.0), &[4.0], 0.0);
        let b = dash_polyline(&line(0.0, 20.0), &[4.0, 4.0], 0.0);
        assert_eq!(a, b);
    }

    #[test]
    fn phase_shifts_pattern_start() {
        // Phase 2 starts 2 units into the first (on) interval.
        let runs = dash_polyline(&line(0.0, 12.0), &[4.0, 4.0], 2.0);
        assert_eq!(runs[0], vec![[0.0, 0.0], [2.0, 0.0]]);
        assert_eq!(runs[1], vec![[6.0, 0.0], [10.0, 0.0]]);
    }

    #[test]
    fn pattern_continues_across_vertices() {
        // L-shaped polyline of total length 20; on-run of 15 wraps the corner.
        let pts = vec![[0.0, 0.0], [10.0, 0.0], [10.0, 10.0]];
        let runs = dash_polyline(&pts, &[15.0, 5.0], 0.0);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0], vec![[0.0, 0.0], [10.0, 0.0], [10.0, 5.0]]);
    }

    #[test]
    fn degenerate_pattern_returns_solid_run() {
        let pts = line(0.0, 20.0);
        let runs = dash_polyline(&pts, &[0.0, 0.0], 0.0);
        assert_eq!(runs, vec![pts]);
    }

    #[test]
    fn zero_on_interval_terminates_with_dot_runs() {
        // [0, 4]: zero-length "on" dots every 4 units — must not loop forever.
        // A dot landing exactly on the path end (at 12) is dropped.
        let runs = dash_polyline(&line(0.0, 12.0), &[0.0, 4.0], 0.0);
        for run in &runs {
            assert_eq!(run[0], run[run.len() - 1], "dot runs are zero-length");
        }
        assert_eq!(runs.len(), 3); // dots at 0, 4, 8
        assert_eq!(runs[0][0], [0.0, 0.0]);
        assert_eq!(runs[1][0], [4.0, 0.0]);
        assert_eq!(runs[2][0], [8.0, 0.0]);
    }
}
