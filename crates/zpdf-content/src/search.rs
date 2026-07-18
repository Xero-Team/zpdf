//! Text search over extracted [`TextSpan`]s.
//!
//! Spans are grouped into baseline lines (the same clustering the reading-order
//! reconstruction uses), each line is flattened to a string with per-character
//! x-extents interpolated from the span advances, and the query is matched
//! against those line strings. Each hit carries the page-space quad(s) covering
//! the matched characters, ready for viewer highlighting.

use std::cmp::Ordering;

use zpdf_core::Rect;

use crate::text::TextSpan;

/// One search match on a page.
#[derive(Debug, Clone)]
pub struct SearchHit {
    /// The full text of the line containing the match (context for display).
    pub line: String,
    /// Char offset of the match within `line`.
    pub start: usize,
    /// Length of the match in chars.
    pub len: usize,
    /// Page-space rectangles (PDF user space, y-up) covering the matched
    /// characters. One rect per matched line for now; kept as a Vec so
    /// cross-line matches can be represented later without an API break.
    pub rects: Vec<Rect>,
}

impl SearchHit {
    /// Bounding rectangle over all of the hit's quads.
    pub fn bounds(&self) -> Rect {
        let mut r = Rect::new(
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        );
        for q in &self.rects {
            r.x0 = r.x0.min(q.x0);
            r.y0 = r.y0.min(q.y0);
            r.x1 = r.x1.max(q.x1);
            r.y1 = r.y1.max(q.y1);
        }
        r
    }
}

/// A character on a line with its interpolated horizontal extent.
struct LineChar {
    ch: char,
    x0: f64,
    x1: f64,
    /// Baseline y of the span this char came from.
    y: f64,
    /// Font size of the span this char came from (0 for synthetic gap spaces).
    size: f32,
    /// True for the space inserted at a word gap between spans (it has no
    /// glyph of its own; its extent is the gap).
    synthetic: bool,
}

/// Case-fold a char for caseless matching (first lowercase mapping; full
/// multi-char expansions like ß→ss are intentionally not applied so char
/// offsets stay 1:1 with the source line).
fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Search one page's spans for `query`. Returns hits in top-to-bottom,
/// left-to-right order. Matches never cross line boundaries; whitespace in the
/// query matches both real spaces and inter-span word gaps.
pub fn search_spans(spans: &[TextSpan], query: &str, case_sensitive: bool) -> Vec<SearchHit> {
    let query: Vec<char> = if case_sensitive {
        query.chars().collect()
    } else {
        query.chars().map(fold).collect()
    };
    if query.is_empty() {
        return Vec::new();
    }

    let mut hits = Vec::new();
    for line in build_lines(spans) {
        let folded: Vec<char> = if case_sensitive {
            line.iter().map(|c| c.ch).collect()
        } else {
            line.iter().map(|c| fold(c.ch)).collect()
        };
        if folded.len() < query.len() {
            continue;
        }
        let mut i = 0;
        while i + query.len() <= folded.len() {
            if folded[i..i + query.len()] == query[..] {
                hits.push(hit_from(&line, i, query.len()));
                i += query.len();
            } else {
                i += 1;
            }
        }
    }
    hits
}

/// Group spans into baseline lines and flatten each to positioned chars.
/// Lines are ordered top-to-bottom; chars within a line left-to-right, with a
/// synthetic space where a word gap separates adjacent spans.
fn build_lines(spans: &[TextSpan]) -> Vec<Vec<LineChar>> {
    let mut idx: Vec<usize> = (0..spans.len())
        .filter(|&i| !spans[i].text.is_empty())
        .collect();
    if idx.is_empty() {
        return Vec::new();
    }
    // Top-to-bottom (y descending).
    idx.sort_by(|&a, &b| {
        spans[b]
            .y
            .partial_cmp(&spans[a].y)
            .unwrap_or(Ordering::Equal)
    });

    // Cluster into lines: a span joins the current line when its baseline is
    // within 0.5 of the larger font size (same tolerance as reading order).
    let mut lines: Vec<Vec<usize>> = Vec::new();
    let mut cur_y = f64::INFINITY;
    let mut cur_size = 0.0f32;
    for &i in &idx {
        let s = &spans[i];
        let tol = (s.size.max(cur_size) as f64 * 0.5).max(1.0);
        if lines.is_empty() || (cur_y - s.y).abs() > tol {
            lines.push(Vec::new());
            cur_y = s.y;
            cur_size = s.size;
        }
        lines.last_mut().unwrap().push(i);
    }

    lines
        .into_iter()
        .map(|mut line| {
            // Left-to-right within the line.
            line.sort_by(|&a, &b| {
                spans[a]
                    .x_bounds()
                    .0
                    .partial_cmp(&spans[b].x_bounds().0)
                    .unwrap_or(Ordering::Equal)
            });
            let mut chars: Vec<LineChar> = Vec::new();
            for &i in &line {
                let s = &spans[i];
                let (sx0, sx1) = s.x_bounds();
                // Word gap between spans → one synthetic space covering the gap.
                if let Some(prev) = chars.last() {
                    let gap = sx0 - prev.x1;
                    if gap > s.size as f64 * 0.25
                        && !prev.ch.is_whitespace()
                        && !s.text.starts_with(char::is_whitespace)
                    {
                        chars.push(LineChar {
                            ch: ' ',
                            x0: prev.x1,
                            x1: sx0,
                            y: s.y,
                            size: 0.0,
                            synthetic: true,
                        });
                    }
                }
                // Distribute the span's extent evenly across its chars. Spans
                // carry only a total advance, so per-glyph widths are
                // approximated; adequate for highlight quads.
                let n = s.text.chars().count();
                let step = if n > 0 { (sx1 - sx0) / n as f64 } else { 0.0 };
                for (k, ch) in s.text.chars().enumerate() {
                    chars.push(LineChar {
                        ch,
                        x0: sx0 + step * k as f64,
                        x1: sx0 + step * (k + 1) as f64,
                        y: s.y,
                        size: s.size,
                        synthetic: false,
                    });
                }
            }
            chars
        })
        .collect()
}

/// Build a hit from a matched char range on one line.
fn hit_from(line: &[LineChar], start: usize, len: usize) -> SearchHit {
    let text: String = line.iter().map(|c| c.ch).collect();
    let matched = &line[start..start + len];

    // Vertical extent from the matched glyphs' baselines and sizes, using the
    // same descender/ascender fractions as the layout heuristics (0.2/0.8).
    // Synthetic gap spaces carry no size and are ignored for the y-extent.
    let (mut x0, mut x1) = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut y0, mut y1) = (f64::INFINITY, f64::NEG_INFINITY);
    for c in matched {
        x0 = x0.min(c.x0);
        x1 = x1.max(c.x1);
        if !c.synthetic {
            let sz = c.size as f64;
            y0 = y0.min(c.y - 0.2 * sz);
            y1 = y1.max(c.y + 0.8 * sz);
        }
    }
    if !y0.is_finite() {
        // Match consisted solely of synthetic spaces; fall back to baseline.
        let y = matched.first().map(|c| c.y).unwrap_or(0.0);
        y0 = y;
        y1 = y;
    }

    SearchHit {
        line: text,
        start,
        len,
        rects: vec![Rect::new(x0, y0, x1, y1)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(text: &str, x: f64, y: f64, size: f32, advance: f64) -> TextSpan {
        TextSpan {
            text: text.to_string(),
            x,
            y,
            size,
            advance,
            mcid: None,
        }
    }

    #[test]
    fn finds_match_within_single_span() {
        let spans = vec![span("Hello World", 10.0, 700.0, 12.0, 66.0)];
        let hits = search_spans(&spans, "World", true);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line, "Hello World");
        assert_eq!(hits[0].start, 6);
        assert_eq!(hits[0].len, 5);
        let r = hits[0].bounds();
        // "World" is chars 6..11 of 11; each char is 6pt wide.
        assert!((r.x0 - 46.0).abs() < 1e-6, "x0={}", r.x0);
        assert!((r.x1 - 76.0).abs() < 1e-6, "x1={}", r.x1);
        assert!((r.y0 - (700.0 - 2.4)).abs() < 1e-6);
        assert!((r.y1 - (700.0 + 9.6)).abs() < 1e-6);
    }

    #[test]
    fn case_insensitive_by_default_flag() {
        let spans = vec![span("Hello World", 10.0, 700.0, 12.0, 66.0)];
        assert_eq!(search_spans(&spans, "world", false).len(), 1);
        assert_eq!(search_spans(&spans, "world", true).len(), 0);
    }

    #[test]
    fn match_crosses_spans_via_word_gap() {
        // Two spans on one baseline with a word gap: "Hello" then "World".
        let spans = vec![
            span("Hello", 10.0, 700.0, 12.0, 30.0),
            span("World", 50.0, 700.0, 12.0, 30.0),
        ];
        let hits = search_spans(&spans, "Hello World", true);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line, "Hello World");
    }

    #[test]
    fn no_match_across_lines() {
        let spans = vec![
            span("Hello", 10.0, 700.0, 12.0, 30.0),
            span("World", 10.0, 680.0, 12.0, 30.0),
        ];
        assert!(search_spans(&spans, "Hello World", true).is_empty());
        // But each line matches individually.
        assert_eq!(search_spans(&spans, "Hello", true).len(), 1);
        assert_eq!(search_spans(&spans, "World", true).len(), 1);
    }

    #[test]
    fn multiple_hits_ordered_top_to_bottom() {
        let spans = vec![
            span("abc abc", 10.0, 600.0, 10.0, 40.0),
            span("abc", 10.0, 700.0, 10.0, 18.0),
        ];
        let hits = search_spans(&spans, "abc", true);
        assert_eq!(hits.len(), 3);
        // First hit from the higher line (y=700).
        assert!(hits[0].bounds().y0 > hits[1].bounds().y0 - 1e-9);
        // Non-overlapping matches within one line.
        assert!(hits[1].start != hits[2].start);
    }

    #[test]
    fn empty_query_returns_nothing() {
        let spans = vec![span("Hello", 10.0, 700.0, 12.0, 30.0)];
        assert!(search_spans(&spans, "", true).is_empty());
    }
}
