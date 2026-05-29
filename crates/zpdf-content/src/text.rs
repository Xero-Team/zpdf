//! Extracted text spans, produced by running the interpreter with a text sink.
//!
//! Each span corresponds to one show-text operation (Tj/TJ element/'/"), carrying
//! the decoded Unicode and the baseline origin in PDF user space (y-up).

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
    /// Total horizontal advance of this span in user-space units.
    pub advance: f64,
}

/// Group spans into visual lines and join them into a plain-text string.
///
/// Spans are sorted top-to-bottom (descending PDF Y) then left-to-right (ascending X).
/// Two spans are on the same line when their baselines differ by less than an
/// adaptive threshold — `max(line_tol, 0.5 · font size)` — so the grouping scales
/// with text size rather than assuming a fixed leading.
pub fn spans_to_text(mut spans: Vec<TextSpan>, line_tol: f64) -> String {
    if spans.is_empty() {
        return String::new();
    }
    // Sort primarily by line (Y descending), then by X ascending.
    spans.sort_by(|a, b| {
        b.y.partial_cmp(&a.y)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal))
    });

    let mut out = String::new();
    let mut current_y = spans[0].y;
    let mut prev_end_x: Option<f64> = None;

    for span in &spans {
        let thresh = line_tol.max(span.size as f64 * 0.5);
        if (current_y - span.y).abs() > thresh {
            out.push('\n');
            current_y = span.y;
            // (prev_end_x is unconditionally refreshed below, so no reset needed —
            // the newline branch never consults it.)
        } else if let Some(prev_x) = prev_end_x {
            // Insert a space if there is a visible gap and the text doesn't already
            // start/end with whitespace.
            let gap = span.x - prev_x;
            let needs_space = gap > span.size as f64 * 0.25
                && !out.ends_with(char::is_whitespace)
                && !span.text.starts_with(char::is_whitespace);
            if needs_space {
                out.push(' ');
            }
        }
        out.push_str(&span.text);
        prev_end_x = Some(span.x + span.advance);
    }

    out
}
