//! Text search over extracted [`TextSpan`]s.
//!
//! Spans are grouped into baseline lines (the same clustering the reading-order
//! reconstruction uses), each line is flattened to a string with per-character
//! geometry from the positioned glyphs, and the query is matched against those
//! line strings. Spans without glyph data retain the legacy advance-based
//! approximation. Each hit carries page-space rectangles ready for highlighting.

use std::cmp::Ordering;

use zpdf_core::Rect;

use crate::text::TextSpan;

/// One search match on a page.
#[derive(Debug, Clone)]
pub struct SearchHit {
    /// The text of the line containing the match (context for display). A
    /// line longer than [`MAX_HIT_CONTEXT_CHARS`] is cut to that many chars
    /// around the match, so a huge line with many hits stays linear.
    pub line: String,
    /// Char offset of the match within `line`.
    pub start: usize,
    /// Length of the match in chars.
    pub len: usize,
    /// Page-space rectangles (PDF user space, y-up) covering the matched
    /// characters. Empty when the match has no selectable geometry.
    pub rects: Vec<Rect>,
}

impl SearchHit {
    /// Bounding rectangle over the hit's rectangles, or a zero rectangle at
    /// the origin when the text has no highlight.
    pub fn bounds(&self) -> Rect {
        self.rects
            .iter()
            .copied()
            .reduce(union)
            .unwrap_or(Rect::new(0.0, 0.0, 0.0, 0.0))
    }
}

/// A character on a line with its glyph's page-space bounds. Several Unicode
/// characters may share the same glyph/cluster (e.g. a ToUnicode ligature).
struct LineChar {
    ch: char,
    bounds: Option<Rect>,
    /// True for the space inserted at a word gap between spans (it has no
    /// glyph of its own; its extent is the gap).
    synthetic: bool,
}

/// Longest [`SearchHit::line`] context kept, in chars.
pub const MAX_HIT_CONTEXT_CHARS: usize = 2048;

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
        let chars: Vec<char> = line.iter().map(|c| c.ch).collect();
        let whole: Option<String> =
            (chars.len() <= MAX_HIT_CONTEXT_CHARS).then(|| chars.iter().collect());
        let mut i = 0;
        while i + query.len() <= folded.len() {
            if folded[i..i + query.len()] == query[..] {
                let mut hit = hit_from(&line, i, query.len());
                match &whole {
                    Some(text) => hit.line = text.clone(),
                    None => {
                        let pad = MAX_HIT_CONTEXT_CHARS.saturating_sub(query.len()) / 2;
                        let lo = i.saturating_sub(pad);
                        let hi = (i + query.len() + pad).min(chars.len());
                        hit.line = chars[lo..hi].iter().collect();
                        hit.start = i - lo;
                    }
                }
                hits.push(hit);
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
        .filter(|&i| !spans[i].text.is_empty() && spans[i].is_finite())
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
            let mut previous_end = None;
            for &i in &line {
                let s = &spans[i];
                let (sx0, sx1) = s.x_bounds();
                // Word gap between spans → one synthetic space covering the gap.
                if let (Some(prev), Some(end)) = (chars.last(), previous_end) {
                    let gap = sx0 - end;
                    if gap > s.size as f64 * 0.25
                        && !prev.ch.is_whitespace()
                        && !s.text.starts_with(char::is_whitespace)
                    {
                        chars.push(LineChar {
                            ch: ' ',
                            bounds: Some(Rect::new(end, s.y, sx0, s.y)),
                            synthetic: true,
                        });
                    }
                }
                // Consecutive glyphs can share a range (ActualText). Union
                // that cluster once, so mapping it to many Unicode characters
                // is linear rather than glyph_count * character_count.
                let mut clusters: Vec<(std::ops::Range<usize>, Rect)> = Vec::new();
                for glyph in &s.glyphs {
                    let range = &glyph.text_range;
                    if range.is_empty() || s.text.get(range.clone()).is_none() {
                        continue;
                    }
                    let Some(rect) = glyph.selectable_bounds() else {
                        continue;
                    };
                    if let Some((_, bounds)) = clusters.last_mut().filter(|(r, _)| r == range) {
                        *bounds = union(*bounds, rect);
                    } else {
                        clusters.push((range.clone(), rect));
                    }
                }
                let mut clusters = clusters.iter().peekable();
                let step = s
                    .glyphs
                    .is_empty()
                    .then(|| (sx1 - sx0) / s.text.chars().count() as f64);
                for (k, (offset, ch)) in s.text.char_indices().enumerate() {
                    while clusters
                        .peek()
                        .is_some_and(|(range, _)| range.end <= offset)
                    {
                        clusters.next();
                    }
                    let bounds = clusters
                        .peek()
                        .filter(|(range, _)| range.contains(&offset))
                        .map(|(_, bounds)| *bounds)
                        .or_else(|| {
                            step.map(|step| {
                                Rect::new(
                                    sx0 + step * k as f64,
                                    s.y - 0.2 * s.size as f64,
                                    sx0 + step * (k + 1) as f64,
                                    s.y + 0.8 * s.size as f64,
                                )
                                .normalize()
                            })
                        });
                    chars.push(LineChar {
                        ch,
                        bounds,
                        synthetic: false,
                    });
                }
                previous_end = Some(sx1);
            }
            chars
        })
        .collect()
}

/// Build a hit from a matched char range on one line; its `line` text is
/// left for the caller to fill.
fn hit_from(line: &[LineChar], start: usize, len: usize) -> SearchHit {
    let matched = &line[start..start + len];

    // A search for just the inserted space can identify the gap itself.
    // In a mixed match only the matched character cells determine the bounds.
    let gaps_only = matched.iter().all(|c| c.synthetic);
    let bounds = matched
        .iter()
        .filter(|c| gaps_only || !c.synthetic)
        .filter_map(|c| c.bounds)
        .reduce(union);

    SearchHit {
        line: String::new(),
        start,
        len,
        rects: bounds.into_iter().collect(),
    }
}

fn union(a: Rect, b: Rect) -> Rect {
    Rect::new(
        a.x0.min(b.x0),
        a.y0.min(b.y0),
        a.x1.max(b.x1),
        a.y1.max(b.y1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hits_on_a_huge_line_carry_a_bounded_window() {
        let spans: Vec<TextSpan> = (0..2000)
            .map(|k| span("ab", k as f64 * 12.0, 700.0, 10.0, 10.0))
            .collect();
        let hits = search_spans(&spans, "b", true);
        assert_eq!(hits.len(), 2000);
        for hit in &hits {
            assert!(hit.line.chars().count() <= MAX_HIT_CONTEXT_CHARS);
            assert_eq!(hit.line.chars().nth(hit.start), Some('b'));
        }
        // A short line keeps its whole text.
        let short = search_spans(&spans[..3], "b", true);
        assert_eq!(short[0].line, "ababab");
    }

    fn span(text: &str, x: f64, y: f64, size: f32, advance: f64) -> TextSpan {
        TextSpan {
            text: text.to_string(),
            x,
            y,
            size,
            advance,
            mcid: None,
            glyphs: Vec::new(),
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
    fn legacy_interpolation_starts_at_the_left_edge_for_negative_advances() {
        let spans = [span("ab", 30.0, 50.0, 10.0, -20.0)];
        assert_eq!(
            search_spans(&spans, "a", true)[0].bounds(),
            Rect::new(10.0, 48.0, 20.0, 58.0)
        );
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
