//! Extracted text spans, produced by running the interpreter with a text sink.
//!
//! Each span corresponds to one show-text operation (Tj/TJ element/'/"), carrying
//! the decoded Unicode and the baseline origin in PDF user space (y-up).

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use zpdf_document::{StructElem, StructKid, StructTree};

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
    /// The marked-content id (`/MCID`) of the innermost enclosing marked-content
    /// sequence, or `None` when this run is outside any (or carries none). Binds
    /// the run to a Tagged-PDF structure element (see [`struct_ordered_text`]).
    pub mcid: Option<i32>,
}

impl TextSpan {
    /// Left/right x-bounds of the span (advance may be negative).
    pub(crate) fn x_bounds(&self) -> (f64, f64) {
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
    let out = fix_rtl_visual_order(&out);
    dehyphenate(&out)
}

/// Merge words hyphenated across line breaks: a line ending in `-` (or a soft
/// hyphen) directly after a letter is joined with the next line when that line
/// starts with a lowercase letter — the typographic signature of a broken
/// word. `co-\noperation` → `cooperation`; `UTF-\n8`, list dashes and lines
/// followed by capitalized words are left alone.
pub fn dehyphenate(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut lines = text.split('\n').peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_end();
        let joins = trimmed
            .strip_suffix(['-', '\u{00AD}'])
            .filter(|stem| stem.chars().next_back().is_some_and(|c| c.is_alphabetic()))
            .filter(|_| {
                lines
                    .peek()
                    .and_then(|next| next.trim_start().chars().next())
                    .is_some_and(|c| c.is_alphabetic() && c.is_lowercase())
            });
        match joins {
            Some(stem) => {
                out.push_str(stem);
                // The next line continues the word: no newline, no hyphen.
            }
            None => {
                out.push_str(line);
                if lines.peek().is_some() {
                    out.push('\n');
                }
            }
        }
    }
    out
}

/// True for characters of inherently right-to-left scripts (Hebrew, Arabic,
/// Syriac, Thaana, plus the Arabic/Hebrew presentation forms).
fn is_rtl_char(c: char) -> bool {
    matches!(c,
        '\u{0590}'..='\u{08FF}'      // Hebrew, Arabic, Syriac, Thaana, ext.
        | '\u{FB1D}'..='\u{FDFF}'    // presentation forms A
        | '\u{FE70}'..='\u{FEFF}'    // presentation forms B
    )
}

/// PDF content streams paint glyphs in **visual** order (left to right on the
/// page). For RTL scripts the logical (reading) order is the reverse of each
/// RTL segment. Lines with no RTL characters pass through untouched; in mixed
/// lines each maximal RTL run (including interior neutral characters like
/// spaces and punctuation between two RTL runs) is reversed in place.
fn fix_rtl_visual_order(text: &str) -> String {
    if !text.chars().any(is_rtl_char) {
        return text.to_string();
    }
    text.split('\n')
        .map(fix_rtl_line)
        .collect::<Vec<_>>()
        .join("\n")
}

fn fix_rtl_line(line: &str) -> String {
    if !line.chars().any(is_rtl_char) {
        return line.to_string();
    }
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < chars.len() {
        if is_rtl_char(chars[i]) {
            // Extend the run to the last RTL character, absorbing neutral
            // characters (spaces, digits, punctuation) that sit *between* RTL
            // characters — they belong to the RTL segment.
            let mut end = i;
            let mut j = i + 1;
            while j < chars.len() {
                if is_rtl_char(chars[j]) {
                    end = j;
                } else if chars[j].is_alphabetic() {
                    break; // a strong LTR character terminates the segment
                }
                j += 1;
            }
            for &c in chars[i..=end].iter().rev() {
                out.push(c);
            }
            i = end + 1;
        } else {
            out.push(chars[i]);
            i += 1;
        }
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

/// Defensive cap on structure-tree recursion while serializing reading-order
/// text. The tree from the structure reader is already depth-bounded; this
/// guards a caller-constructed tree.
const MAX_STRUCT_TEXT_DEPTH: usize = 256;

/// Extract a page's text in the document's **logical reading order**, driven by
/// the Tagged-PDF structure tree (the order the producer intends, rather than the
/// geometric XY-cut of [`spans_to_text`]).
///
/// Each structure element contributes, in tree order, either its `/ActualText`
/// (the exact replacement for the element and its children), or the text of the
/// page content its marked-content (`/MCID`) kids reference, or — for a
/// content-less element such as a figure — its `/Alt` description. `spans` are
/// this page's extracted spans (each carrying its [`TextSpan::mcid`], produced by
/// running the interpreter with a text sink); `page_index` is the 0-based page
/// they came from. Block-level elements ([`zpdf_document::StructRole::is_block_level`])
/// are separated by newlines; within a block, runs are joined with the same
/// word-gap / line-break heuristic as [`spans_to_text`].
///
/// Any run the structure tree does not place — a run with no `/MCID` (e.g.
/// `/Artifact` content: running headers, footers, page numbers) or an `/MCID` no
/// structure element references — is **appended** in geometric reading order
/// rather than dropped, so `--struct` never silently loses a page's text. An
/// entirely untagged page therefore degrades to the geometric [`spans_to_text`].
pub fn struct_ordered_text(spans: &[TextSpan], page_index: usize, tree: &StructTree) -> String {
    // Index this page's spans by MCID; a span with no MCID is not reachable
    // through the structure tree. Insertion order is content (reading) order.
    let mut by_mcid: HashMap<i64, Vec<&TextSpan>> = HashMap::new();
    for s in spans {
        if let Some(m) = s.mcid {
            by_mcid.entry(m as i64).or_default().push(s);
        }
    }

    let mut b = ReadingOrder::default();
    // MCIDs actually placed by the tree (including those subsumed by an
    // `/ActualText`), so the leftover pass doesn't re-emit them.
    let mut consumed: HashSet<i64> = HashSet::new();
    for elem in &tree.children {
        emit_struct_elem(elem, page_index, &by_mcid, &mut consumed, 0, &mut b);
    }
    let mut result = b.out.trim().to_string();

    // Append any of this page's text the tree left unplaced (no MCID, or an MCID
    // no element referenced) in geometric order. This also covers a fully
    // untagged page (every span is leftover → the geometric reading order).
    let leftover: Vec<TextSpan> = spans
        .iter()
        .filter(|s| match s.mcid {
            Some(m) => !consumed.contains(&(m as i64)),
            None => true,
        })
        .cloned()
        .collect();
    if !leftover.is_empty() {
        let extra = spans_to_text(leftover, 2.0);
        if !extra.is_empty() {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(&extra);
        }
    }
    result
}

/// Accumulates reading-order text, inserting line breaks and word-gap spaces
/// between successive spans the way [`block_to_text`] does.
#[derive(Default)]
struct ReadingOrder {
    out: String,
    /// `(end_x, baseline_y, size)` of the last appended span, for gap/line logic.
    last: Option<(f64, f64, f32)>,
}

impl ReadingOrder {
    fn push_span(&mut self, s: &TextSpan) {
        if let Some((prev_end, prev_y, prev_size)) = self.last {
            let line_tol = (s.size.max(prev_size)) as f64 * 0.5;
            if (prev_y - s.y).abs() > line_tol {
                self.out.push('\n');
            } else {
                let gap = s.x_bounds().0 - prev_end;
                if gap > s.size as f64 * 0.25
                    && !self.out.ends_with(char::is_whitespace)
                    && !s.text.starts_with(char::is_whitespace)
                {
                    self.out.push(' ');
                }
            }
        }
        self.out.push_str(&s.text);
        self.last = Some((s.x_bounds().1, s.y, s.size));
    }

    /// Append known replacement text (`/ActualText` or `/Alt`), whose geometry is
    /// unknown — separate it from any preceding run with a single space.
    fn push_replacement(&mut self, t: &str) {
        if self.last.is_some()
            && !self.out.ends_with(char::is_whitespace)
            && !t.starts_with(char::is_whitespace)
        {
            self.out.push(' ');
        }
        self.out.push_str(t);
        self.last = None;
    }

    /// Start a new block (paragraph/heading/cell) on its own line, collapsing
    /// consecutive breaks.
    fn block_break(&mut self) {
        if !self.out.is_empty() && !self.out.ends_with('\n') {
            self.out.push('\n');
        }
        self.last = None;
    }
}

fn emit_struct_elem(
    elem: &StructElem,
    page: usize,
    by_mcid: &HashMap<i64, Vec<&TextSpan>>,
    consumed: &mut HashSet<i64>,
    depth: usize,
    b: &mut ReadingOrder,
) {
    if depth > MAX_STRUCT_TEXT_DEPTH {
        return;
    }
    // An element's own text (`/ActualText`, `/Alt`) lives on its effective page;
    // when that page is known and differs from the one being extracted, the
    // element contributes none of it here (this is what keeps a figure's `/Alt`
    // from repeating on every page). A `None` page is unresolved, not excluded.
    // Child elements are still recursed (a child may carry its own `/Pg`), and a
    // marked-content kid is page-filtered independently below.
    let on_this_page = elem.page.is_none_or(|p| p == page);

    let block = elem.role.is_block_level();
    if block {
        b.block_break();
    }
    // /ActualText is the exact replacement for the element and its children.
    if let Some(actual) = &elem.actual_text {
        if on_this_page {
            b.push_replacement(actual);
            // The replacement subsumes the subtree's marked content; mark it
            // consumed so the leftover pass doesn't re-emit the replaced glyphs.
            mark_consumed(elem, page, 0, consumed);
        }
        if block {
            b.block_break();
        }
        return;
    }

    let before = b.out.len();
    for kid in &elem.kids {
        match kid {
            StructKid::Element(child) => {
                emit_struct_elem(child, page, by_mcid, consumed, depth + 1, b)
            }
            StructKid::MarkedContent {
                page: kid_page,
                mcid,
            } if *kid_page == Some(page) => {
                if let Some(list) = by_mcid.get(mcid) {
                    consumed.insert(*mcid);
                    for s in list {
                        b.push_span(s);
                    }
                }
            }
            _ => {}
        }
    }
    // A content-less element (a figure/formula with no extractable glyphs) on
    // this page falls back to its alternate description.
    if on_this_page && b.out[before..].trim().is_empty() {
        if let Some(alt) = &elem.alt {
            b.push_replacement(alt);
        }
    }

    if block {
        b.block_break();
    }
}

/// Mark every marked-content id in `elem`'s subtree (on `page`) as consumed,
/// without emitting — used when an `/ActualText` replaces the subtree, so the
/// leftover pass does not re-emit the glyphs it stood in for.
fn mark_consumed(elem: &StructElem, page: usize, depth: usize, consumed: &mut HashSet<i64>) {
    if depth > MAX_STRUCT_TEXT_DEPTH {
        return;
    }
    for kid in &elem.kids {
        match kid {
            StructKid::Element(child) => mark_consumed(child, page, depth + 1, consumed),
            StructKid::MarkedContent {
                page: kid_page,
                mcid,
            } if *kid_page == Some(page) => {
                consumed.insert(*mcid);
            }
            _ => {}
        }
    }
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
            mcid: None,
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
                mcid: None,
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

    // ---- struct_ordered_text (Tagged-PDF reading order) ----

    use zpdf_document::StructRole;

    fn span_m(text: &str, x: f64, y: f64, size: f32, mcid: i32) -> TextSpan {
        let mut s = span(text, x, y, size);
        s.mcid = Some(mcid);
        s
    }

    fn selem(role: StructRole, page: Option<usize>, kids: Vec<StructKid>) -> StructElem {
        StructElem {
            role,
            raw_type: String::new(),
            title: None,
            lang: None,
            alt: None,
            actual_text: None,
            expansion: None,
            page,
            kids,
        }
    }

    /// A marked-content kid binding `mcid` on `page`.
    fn mc(page: usize, mcid: i64) -> StructKid {
        StructKid::MarkedContent {
            page: Some(page),
            mcid,
        }
    }

    #[test]
    fn struct_order_differs_from_geometry() {
        // "A" sits higher on the page (y=700), "B" lower (y=686). The structure
        // tree lists B before A, so the reading order is "B\nA" — the reverse of
        // the geometric top-to-bottom order.
        let spans = vec![
            span_m("A", 50.0, 700.0, 12.0, 0),
            span_m("B", 50.0, 686.0, 12.0, 1),
        ];
        let tree = StructTree {
            marked: true,
            children: vec![selem(StructRole::P, Some(0), vec![mc(0, 1), mc(0, 0)])],
        };
        assert_eq!(struct_ordered_text(&spans, 0, &tree), "B\nA");
        // Geometric reading order is the opposite.
        assert_eq!(spans_to_text(spans, 2.0), "A\nB");
    }

    #[test]
    fn actual_text_replaces_marked_content() {
        // An element's /ActualText is the exact replacement for its content.
        let spans = vec![span_m("\u{FB01}", 50.0, 700.0, 12.0, 0)]; // "ﬁ" ligature
        let mut span_el = selem(StructRole::Span, Some(0), vec![mc(0, 0)]);
        span_el.actual_text = Some("fi".to_string());
        let tree = StructTree {
            marked: true,
            children: vec![span_el],
        };
        assert_eq!(struct_ordered_text(&spans, 0, &tree), "fi");
    }

    #[test]
    fn alt_text_for_content_less_figure() {
        // A figure that references no extractable text falls back to /Alt.
        let mut fig = selem(StructRole::Figure, Some(0), vec![]);
        fig.alt = Some("A bar chart".to_string());
        let tree = StructTree {
            marked: true,
            children: vec![fig],
        };
        assert_eq!(struct_ordered_text(&[], 0, &tree), "A bar chart");
    }

    #[test]
    fn empty_tree_falls_back_to_geometry() {
        let spans = vec![
            span("Hello", 50.0, 700.0, 12.0),
            span("World", 50.0, 686.0, 12.0),
        ];
        let tree = StructTree {
            marked: false,
            children: vec![],
        };
        let geo = spans_to_text(spans.clone(), 2.0);
        assert_eq!(struct_ordered_text(&spans, 0, &tree), geo);
    }

    #[test]
    fn marked_content_is_page_scoped() {
        // MCID values are per content stream. Each page is extracted with its own
        // spans; a structure element on another page contributes nothing.
        let tree = StructTree {
            marked: true,
            children: vec![
                selem(StructRole::P, Some(0), vec![mc(0, 0)]),
                selem(StructRole::P, Some(1), vec![mc(1, 1)]),
            ],
        };
        let page0 = vec![span_m("zero", 50.0, 700.0, 12.0, 0)];
        let page1 = vec![span_m("one", 50.0, 700.0, 12.0, 1)];
        assert_eq!(struct_ordered_text(&page0, 0, &tree), "zero");
        assert_eq!(struct_ordered_text(&page1, 1, &tree), "one");
    }

    #[test]
    fn unreferenced_and_artifact_runs_are_appended() {
        // A tagged run (mcid 0), an artifact run with no MCID, and a run whose
        // MCID (9) no element references. The tree places the first; the other two
        // are appended in geometric order rather than silently dropped.
        let spans = vec![
            span_m("Tagged", 50.0, 700.0, 12.0, 0),
            span("Footer", 50.0, 50.0, 10.0), // mcid None (artifact)
            span_m("Orphan", 50.0, 600.0, 12.0, 9), // referenced by no element
        ];
        let tree = StructTree {
            marked: true,
            children: vec![selem(StructRole::P, Some(0), vec![mc(0, 0)])],
        };
        let out = struct_ordered_text(&spans, 0, &tree);
        assert!(out.starts_with("Tagged"), "structure text first: {out:?}");
        assert!(out.contains("Footer"), "artifact run not dropped: {out:?}");
        assert!(
            out.contains("Orphan"),
            "unreferenced run not dropped: {out:?}"
        );
    }

    #[test]
    fn alt_text_does_not_leak_across_pages() {
        // A figure on page 1 with /Alt: extracting page 0 must not surface it,
        // page 1 must — so the description isn't repeated on every page.
        let mut fig = selem(StructRole::Figure, Some(1), vec![]);
        fig.alt = Some("Chart on page two".to_string());
        let tree = StructTree {
            marked: true,
            children: vec![fig],
        };
        assert_eq!(struct_ordered_text(&[], 0, &tree), "");
        assert_eq!(struct_ordered_text(&[], 1, &tree), "Chart on page two");
    }

    #[test]
    fn list_item_label_and_body_read_inline() {
        // LI { Lbl "1.", LBody "Item text" } reads on one line, not split in two.
        let spans = vec![
            span_m("1.", 50.0, 700.0, 12.0, 0),
            span_m("Item text", 80.0, 700.0, 12.0, 1),
        ];
        let li = selem(
            StructRole::Li,
            Some(0),
            vec![
                StructKid::Element(selem(StructRole::Lbl, Some(0), vec![mc(0, 0)])),
                StructKid::Element(selem(StructRole::LBody, Some(0), vec![mc(0, 1)])),
            ],
        );
        let tree = StructTree {
            marked: true,
            children: vec![li],
        };
        assert_eq!(struct_ordered_text(&spans, 0, &tree), "1. Item text");
    }

    #[test]
    fn table_row_cells_read_across_the_row() {
        // TR { TD "a", TD "b" } reads as one row line, not one cell per line.
        let spans = vec![
            span_m("a", 50.0, 700.0, 12.0, 0),
            span_m("b", 120.0, 700.0, 12.0, 1),
        ];
        let tr = selem(
            StructRole::Tr,
            Some(0),
            vec![
                StructKid::Element(selem(StructRole::Td, Some(0), vec![mc(0, 0)])),
                StructKid::Element(selem(StructRole::Td, Some(0), vec![mc(0, 1)])),
            ],
        );
        let tree = StructTree {
            marked: true,
            children: vec![tr],
        };
        assert_eq!(struct_ordered_text(&spans, 0, &tree), "a b");
    }

    #[test]
    fn block_elements_are_newline_separated() {
        let spans = vec![
            span_m("Title", 50.0, 700.0, 16.0, 0),
            span_m("Body", 50.0, 670.0, 12.0, 1),
        ];
        let tree = StructTree {
            marked: true,
            children: vec![
                selem(StructRole::H1, Some(0), vec![mc(0, 0)]),
                selem(StructRole::P, Some(0), vec![mc(0, 1)]),
            ],
        };
        assert_eq!(struct_ordered_text(&spans, 0, &tree), "Title\nBody");
    }

    #[test]
    fn inline_runs_join_with_word_gap() {
        // Two runs in one paragraph with a clear horizontal gap get one space.
        let mut world = span_m("World", 130.0, 700.0, 12.0, 1);
        world.advance = 30.0;
        let spans = vec![span_m("Hello", 50.0, 700.0, 12.0, 0), world];
        let tree = StructTree {
            marked: true,
            children: vec![selem(StructRole::P, Some(0), vec![mc(0, 0), mc(0, 1)])],
        };
        assert_eq!(struct_ordered_text(&spans, 0, &tree), "Hello World");
    }
}

#[cfg(test)]
mod dehyphen_tests {
    use super::{dehyphenate, fix_rtl_visual_order};

    #[test]
    fn joins_hyphenated_word_across_lines() {
        assert_eq!(dehyphenate("coopera-\ntion works"), "cooperation works");
    }

    #[test]
    fn soft_hyphen_also_joins() {
        assert_eq!(dehyphenate("con\u{00AD}\ntinued"), "continued");
    }

    #[test]
    fn capitalized_next_line_is_not_joined() {
        // Likely a compound name or new sentence, not a broken word.
        assert_eq!(dehyphenate("Smith-\nJones"), "Smith-\nJones");
    }

    #[test]
    fn digit_before_hyphen_is_not_joined() {
        assert_eq!(dehyphenate("UTF-\n8 encoding"), "UTF-\n8 encoding");
    }

    #[test]
    fn hyphen_mid_line_untouched() {
        assert_eq!(dehyphenate("well-known fact"), "well-known fact");
    }

    #[test]
    fn no_hyphens_passthrough() {
        let text = "plain\nlines\nhere";
        assert_eq!(dehyphenate(text), text);
    }

    #[test]
    fn rtl_line_is_reversed_to_logical_order() {
        // Visual order (as painted): "םולש" — logical: "שלום".
        let visual = "\u{05DD}\u{05D5}\u{05DC}\u{05E9}";
        let logical = "\u{05E9}\u{05DC}\u{05D5}\u{05DD}";
        assert_eq!(fix_rtl_visual_order(visual), logical);
    }

    #[test]
    fn mixed_line_reverses_only_rtl_segment() {
        // "abc " + visual-RTL + " xyz"
        let visual = format!("abc {} xyz", "\u{05D2}\u{05D1}\u{05D0}");
        let expected = format!("abc {} xyz", "\u{05D0}\u{05D1}\u{05D2}");
        assert_eq!(fix_rtl_visual_order(&visual), expected);
    }

    #[test]
    fn ltr_text_passthrough() {
        assert_eq!(fix_rtl_visual_order("hello world"), "hello world");
    }

    #[test]
    fn rtl_segment_with_interior_space() {
        // Two RTL words separated by a space: whole segment reverses.
        let visual = "\u{05D1} \u{05D0}"; // visual "ב א"
        let logical = "\u{05D0} \u{05D1}"; // logical "א ב"
        assert_eq!(fix_rtl_visual_order(visual), logical);
    }
}
