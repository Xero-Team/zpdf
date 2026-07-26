//! Text grouping logic for merging nearby text runs into coherent text boxes.

use zpdf_core::Rect;

/// A text run with its bounding box and content.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct TextRun {
    pub text: String,
    pub bounds: Rect,
    pub font_size: f64,
}

/// Group nearby text runs into larger text boxes.
/// This is a placeholder for future implementation.
#[allow(dead_code)]
pub fn group_text_runs(runs: &[TextRun], _max_gap: f64) -> Vec<Vec<usize>> {
    // Simple grouping: each run is its own group for now
    // Future: merge runs that are horizontally adjacent with gap < max_gap
    (0..runs.len()).map(|i| vec![i]).collect()
}
