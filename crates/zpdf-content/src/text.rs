//! Extracted text spans, produced by running the interpreter with a text sink.
//!
//! Each span corresponds to one show-text operation (Tj/TJ element/'/"), carrying
//! the decoded Unicode and the baseline origin in PDF user space (y-up).

use std::cmp::Ordering;

#[derive(Debug, Clone)]
pub struct TextSpan {
    /// Decoded Unicode text for this show-text operation.
    pub text: String,
    /// Baseline origin X in PDF user space (after CTM·Tm), y-up.
    pub x: f64,
    /// Baseline origin Y in PDF user space (after CTM·Tm), y-up.
    pub y: f64,
    /// Effective font size in user-space units (Tf size scaled by the transform).
    pub size: f32,
    /// Signed horizontal extent of this span in user-space units (end.x − start.x).
    pub advance: f64,
}

impl TextSpan {
    /// Left/right x-bounds of the span (advance may be negative).
    fn x_bounds(&self) -> (f64, f64) {
        (
            self.x.min(self.x + self.advance),
            self.x.max(self.x + self.advance),
        )
    }
}

/// Reconstruct plain text from extracted spans, recovering reading order.
///
/// Uses a recursive XY-cut: whole-width whitespace valleys split the page
/// horizontally (peeling off titles/headers), clear vertical valleys split it
/// into columns, and within each leaf block spans are grouped into lines by
/// baseline and joined left-to-right. This keeps multi-column layouts (e.g. a
/// two-column table of contents) in the correct order instead of interleaving.
///
/// `line_tol` is the minimum baseline tolerance for grouping spans onto one line
/// (an adaptive `0.5 · font size` is also applied).
pub fn spans_to_text(spans: Vec<TextSpan>, line_tol: f64) -> String {
    let spans: Vec<TextSpan> = spans.into_iter().filter(|s| !s.text.is_empty()).collect();
    if spans.is_empty() {
        return String::new();
    }
    let unit = median_size(&spans).max(1.0);
    let indices: Vec<usize> = (0..spans.len()).collect();

    let mut blocks: Vec<Vec<usize>> = Vec::new();
    xy_cut(&indices, &spans, unit, 0, true, &mut blocks);

    let mut out = String::new();
    for block in &blocks {
        let text = block_to_text(block, &spans, line_tol);
        if text.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&text);
    }
    out
}

/// Cluster spans into baseline rows (top-to-bottom), grouping y within ~0.6·unit.
fn group_rows(idx: &[usize], spans: &[TextSpan], unit: f64) -> Vec<Vec<usize>> {
    let mut sorted: Vec<usize> = idx.to_vec();
    sorted.sort_by(|&a, &b| {
        spans[b]
            .y
            .partial_cmp(&spans[a].y)
            .unwrap_or(Ordering::Equal)
    });
    let tol = unit * 0.6;
    let mut rows: Vec<Vec<usize>> = Vec::new();
    let mut cur_y = f64::INFINITY;
    for i in sorted {
        if rows.is_empty() || (cur_y - spans[i].y).abs() > tol {
            rows.push(Vec::new());
            cur_y = spans[i].y;
        }
        rows.last_mut().unwrap().push(i);
    }
    rows
}

fn median_size(spans: &[TextSpan]) -> f64 {
    let mut sizes: Vec<f64> = spans
        .iter()
        .map(|s| s.size as f64)
        .filter(|z| *z > 0.0)
        .collect();
    if sizes.is_empty() {
        return 12.0;
    }
    sizes.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    sizes[sizes.len() / 2]
}

/// Recursively split a set of spans into reading-ordered blocks.
///
/// `allow_vcut` caps column splitting to one level per horizontal band: once a
/// block has been produced by a vertical (column) cut its content is treated as
/// rows, so tabular field-gaps within a column (e.g. `number | title | page#`)
/// are not mistaken for further columns. Horizontal cuts preserve the flag, so a
/// full-width header above the columns is still peeled before the column split.
fn xy_cut(
    idx: &[usize],
    spans: &[TextSpan],
    unit: f64,
    depth: u32,
    allow_vcut: bool,
    out: &mut Vec<Vec<usize>>,
) {
    if idx.is_empty() {
        return;
    }
    if depth >= 24 || idx.len() <= 1 {
        out.push(idx.to_vec());
        return;
    }
    // Horizontal valley first: peels full-width bands (titles, headers, footers,
    // paragraph breaks). A global y-gap only exists where ALL columns are empty,
    // so this never splits a column's internal paragraphs while another column
    // still has content — keeping centered/full-width headers above the columns.
    if let Some((top, bottom)) = horizontal_cut(idx, spans, unit) {
        xy_cut(&top, spans, unit, depth + 1, allow_vcut, out);
        xy_cut(&bottom, spans, unit, depth + 1, allow_vcut, out);
        return;
    }
    // Then a column split: an x-position whose whitespace gap recurs across rows.
    if allow_vcut {
        if let Some((left, right)) = vertical_cut(idx, spans, unit) {
            xy_cut(&left, spans, unit, depth + 1, false, out);
            xy_cut(&right, spans, unit, depth + 1, false, out);
            return;
        }
    }
    out.push(idx.to_vec());
}

/// Detect a column boundary as the x-position whose inter-content whitespace gap
/// recurs across the most rows (a vertical "river"). This distinguishes a true
/// column separator from scattered word gaps even when dot leaders nearly bridge
/// the columns, where a single global x-projection valley would not appear.
fn vertical_cut(idx: &[usize], spans: &[TextSpan], unit: f64) -> Option<(Vec<usize>, Vec<usize>)> {
    let rows = group_rows(idx, spans, unit);
    if rows.len() < 2 {
        return None;
    }
    let min_gap = unit * 0.5;

    // Collect the internal whitespace gaps of every row.
    let mut gaps: Vec<(f64, f64)> = Vec::new();
    for row in &rows {
        let mut iv: Vec<(f64, f64)> = row.iter().map(|&i| spans[i].x_bounds()).collect();
        iv.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal));
        let mut cur = iv[0];
        for &(x0, x1) in iv.iter().skip(1) {
            if x0 - cur.1 > min_gap {
                gaps.push((cur.1, x0));
                cur = (x0, x1);
            } else {
                cur.1 = cur.1.max(x1);
            }
        }
    }
    if gaps.is_empty() {
        return None;
    }

    // Block x-extent: a real column gutter sits near the centre, whereas tabular
    // field-gaps (e.g. number | title in a TOC entry) sit off to one side. Only
    // accept a river that leaves both sides at least 20% of the block width.
    let bx0 = idx
        .iter()
        .map(|&i| spans[i].x_bounds().0)
        .fold(f64::INFINITY, f64::min);
    let bx1 = idx
        .iter()
        .map(|&i| spans[i].x_bounds().1)
        .fold(f64::NEG_INFINITY, f64::max);
    let width = bx1 - bx0;
    if width <= 0.0 {
        return None;
    }
    let margin = width * 0.2;

    // Among balanced candidates, the x covered by the most row-gaps is the column river.
    let mut points: Vec<f64> = gaps.iter().flat_map(|&(a, b)| [a, b]).collect();
    points.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    let (mut best_depth, mut best_x) = (0usize, 0.0f64);
    for w in points.windows(2) {
        if w[1] - w[0] < 1e-6 {
            continue;
        }
        let mid = (w[0] + w[1]) / 2.0;
        if mid - bx0 < margin || bx1 - mid < margin {
            continue; // too close to an edge to be a column boundary
        }
        let depth = gaps.iter().filter(|&&(a, b)| a <= mid && mid <= b).count();
        if depth > best_depth {
            best_depth = depth;
            best_x = mid;
        }
    }

    // Require the river to be supported by at least half the rows. A real column
    // gutter runs through most rows; a tabular field-gap (number | title) only
    // covers the subset of rows whose field happens to be short there.
    if best_depth < rows.len().div_ceil(2).max(2) {
        return None;
    }

    let (mut left, mut right) = (Vec::new(), Vec::new());
    for &i in idx {
        let (x0, x1) = spans[i].x_bounds();
        if (x0 + x1) / 2.0 < best_x {
            left.push(i);
        } else {
            right.push(i);
        }
    }
    if left.is_empty() || right.is_empty() {
        None
    } else {
        Some((left, right))
    }
}

/// Find the widest horizontal whitespace valley and split top/bottom at it.
fn horizontal_cut(
    idx: &[usize],
    spans: &[TextSpan],
    unit: f64,
) -> Option<(Vec<usize>, Vec<usize>)> {
    let thresh = unit * 0.8;
    // Vertical extent of each span (y-up): bottom = y − 0.2·size, top = y + 0.8·size.
    let mut iv: Vec<(f64, f64)> = idx
        .iter()
        .map(|&i| {
            let s = &spans[i];
            let sz = s.size as f64;
            (s.y - 0.2 * sz, s.y + 0.8 * sz)
        })
        .collect();
    iv.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal));

    let (mut best_gap, mut best_cut) = (0.0f64, 0.0f64);
    let mut cur_top = iv[0].1;
    for &(lo, hi) in &iv[1..] {
        if lo > cur_top {
            let gap = lo - cur_top;
            if gap > best_gap {
                best_gap = gap;
                best_cut = (cur_top + lo) / 2.0;
            }
            cur_top = hi;
        } else {
            cur_top = cur_top.max(hi);
        }
    }
    if best_gap < thresh {
        return None;
    }

    let (mut top, mut bottom) = (Vec::new(), Vec::new());
    for &i in idx {
        if spans[i].y >= best_cut {
            top.push(i); // higher y = visually higher
        } else {
            bottom.push(i);
        }
    }
    if top.is_empty() || bottom.is_empty() {
        None
    } else {
        Some((top, bottom))
    }
}

/// Group a leaf block's spans into lines (by baseline) and join them.
fn block_to_text(block: &[usize], spans: &[TextSpan], line_tol: f64) -> String {
    let mut items: Vec<&TextSpan> = block.iter().map(|&i| &spans[i]).collect();
    // Top-to-bottom (y descending), then left-to-right (x ascending).
    items.sort_by(|a, b| {
        b.y.partial_cmp(&a.y)
            .unwrap_or(Ordering::Equal)
            .then(a.x.partial_cmp(&b.x).unwrap_or(Ordering::Equal))
    });

    let mut out = String::new();
    let mut current_y = items[0].y;
    let mut prev_end_x: Option<f64> = None;

    for s in &items {
        let thresh = line_tol.max(s.size as f64 * 0.5);
        if (current_y - s.y).abs() > thresh {
            out.push('\n');
            current_y = s.y;
        } else if let Some(prev_x) = prev_end_x {
            let gap = s.x_bounds().0 - prev_x;
            let needs_space = gap > s.size as f64 * 0.25
                && !out.ends_with(char::is_whitespace)
                && !s.text.starts_with(char::is_whitespace);
            if needs_space {
                out.push(' ');
            }
        }
        out.push_str(&s.text);
        prev_end_x = Some(s.x_bounds().1);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(text: &str, x: f64, y: f64, size: f32) -> TextSpan {
        TextSpan {
            text: text.to_string(),
            x,
            y,
            size,
            advance: text.chars().count() as f64 * size as f64 * 0.5,
        }
    }

    #[test]
    fn single_column_top_to_bottom() {
        let spans = vec![
            span("first line", 50.0, 700.0, 12.0),
            span("second line", 50.0, 686.0, 12.0),
        ];
        assert_eq!(spans_to_text(spans, 2.0), "first line\nsecond line");
    }

    #[test]
    fn two_columns_read_left_then_right() {
        // Left column at x≈50, right column at x≈300; both span the same rows.
        let spans = vec![
            span("L1", 50.0, 700.0, 12.0),
            span("R1", 300.0, 700.0, 12.0),
            span("L2", 50.0, 686.0, 12.0),
            span("R2", 300.0, 686.0, 12.0),
        ];
        // Without column awareness this would be "L1 R1\nL2 R2"; XY-cut keeps
        // each column together.
        let out = spans_to_text(spans, 2.0);
        assert_eq!(out, "L1\nL2\nR1\nR2");
    }

    #[test]
    fn full_width_header_above_columns() {
        let spans = vec![
            span("TITLE SPANS WHOLE WIDTH HERE", 50.0, 760.0, 16.0),
            span("left a", 50.0, 700.0, 12.0),
            span("right a", 320.0, 700.0, 12.0),
            span("left b", 50.0, 686.0, 12.0),
            span("right b", 320.0, 686.0, 12.0),
        ];
        let out = spans_to_text(spans, 2.0);
        assert_eq!(
            out,
            "TITLE SPANS WHOLE WIDTH HERE\nleft a\nleft b\nright a\nright b"
        );
    }

    #[test]
    fn word_gap_inserts_space() {
        let spans = vec![
            span("Hello", 50.0, 700.0, 12.0),
            // start well to the right of where "Hello" ends.
            TextSpan {
                text: "World".into(),
                x: 130.0,
                y: 700.0,
                size: 12.0,
                advance: 30.0,
            },
        ];
        assert_eq!(spans_to_text(spans, 2.0), "Hello World");
    }

    #[test]
    fn new_line_has_no_leading_gap_space() {
        // Line 1 ends far to the right; line 2 starts at the left. The line break
        // must not produce a leading space on line 2 — this guards the removal of
        // the dead `prev_end_x = None` store in the new-line branch.
        let spans = vec![
            span("AAAAAAAAAA", 50.0, 700.0, 12.0),
            span("B", 50.0, 686.0, 12.0),
        ];
        assert_eq!(spans_to_text(spans, 2.0), "AAAAAAAAAA\nB");
    }
}
