//! Table detection over extracted [`TextSpan`]s.
//!
//! PDF has no table model — a "table" is just text and (sometimes) ruled lines
//! laid out on a grid — so tabular structure has to be recovered heuristically.
//! The base detector ([`detect_tables`]) is purely **alignment-based**: it
//! groups spans into baseline rows, segments the page into vertical bands at
//! large gaps, and within each band looks for clean vertical **gutters** —
//! x-ranges that (almost) no text crosses across the band's rows. Gutters that
//! recur down a band are column separators; a band with two or more columns
//! over several rows is reported as a [`Table`].
//!
//! [`detect_tables_with_rules`] additionally consumes the page's drawn ruled
//! lines ([`RuleLine`], captured by the interpreter's rule sink): a vertical
//! rule spanning a band is a **forced column separator**, and a band whose
//! columns come from drawn rules skips the prose-fill guard — a drawn grid is
//! direct evidence of tabular intent.
//!
//! Because ordinary prose fills the line width, it *crosses* any candidate
//! gutter and so disqualifies itself — which keeps the false-positive rate low
//! without needing the page's ruled lines.
//!
//! Known limitations (all shared by purely text-based detectors; ruled-line
//! capture mitigates but does not remove them):
//! - A wrapped multi-line cell is read as separate rows (its continuation line
//!   becomes a row with the leading columns empty); cells are not re-joined.
//! - A short, left-aligned header sitting entirely to one side of a
//!   right-aligned numeric column can open a spurious gutter between the header
//!   and its data.
//! - A table that begins immediately under multi-line running prose (no blank
//!   line between) may be missed, because the prose rows share the band and
//!   cross the gutters.
//! - A dense multi-column page layout whose cells happen to be short can read as
//!   a table; a table whose cells wrap to fill the column width is rejected by
//!   the prose guard.

use std::cmp::Ordering;

use crate::text::TextSpan;

/// An axis-aligned ruled line captured from page content (a thin stroke or a
/// thin filled rectangle), in PDF user space (y-up). Produced by running the
/// interpreter with [`ContentInterpreter::with_rule_sink`]; consumed by
/// [`detect_tables_with_rules`] as drawn table-grid evidence.
///
/// [`ContentInterpreter::with_rule_sink`]: crate::interpreter::ContentInterpreter::with_rule_sink
#[derive(Debug, Clone, Copy)]
pub struct RuleLine {
    /// `true` for a vertical rule (constant x), `false` for horizontal.
    pub vertical: bool,
    /// The constant coordinate: x for a vertical rule, y for a horizontal one.
    pub pos: f64,
    /// Extent start along the rule's axis (y for vertical, x for horizontal);
    /// always ≤ `end`.
    pub start: f64,
    /// Extent end along the rule's axis.
    pub end: f64,
}

impl RuleLine {
    /// Length of the rule along its axis.
    pub fn len(&self) -> f64 {
        self.end - self.start
    }

    /// True when the rule has no extent.
    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }
}

/// A detected table: a grid of cell strings plus the column/row separator
/// positions in PDF user space (y-up).
#[derive(Debug, Clone)]
pub struct Table {
    /// Cell text in row-major order — `cells[row][col]`, rows top-to-bottom and
    /// columns left-to-right. Every row has [`Table::cols`] entries; a cell with
    /// no text is an empty string.
    pub cells: Vec<Vec<String>>,
    /// Column separator x-positions, ascending; length = `cols + 1`. `col_x[c]`
    /// and `col_x[c + 1]` bound column `c`.
    pub col_x: Vec<f64>,
    /// Row separator y-positions, descending (y-up); length = `rows + 1`.
    /// `row_y[r]` (top) and `row_y[r + 1]` (bottom) bound row `r`.
    pub row_y: Vec<f64>,
}

impl Table {
    /// Number of rows.
    pub fn rows(&self) -> usize {
        self.cells.len()
    }

    /// Number of columns.
    pub fn cols(&self) -> usize {
        self.col_x.len().saturating_sub(1)
    }

    /// The table's bounding box `(x0, y0, x1, y1)` in user space (y-up).
    pub fn bbox(&self) -> (f64, f64, f64, f64) {
        let x0 = self.col_x.first().copied().unwrap_or(0.0);
        let x1 = self.col_x.last().copied().unwrap_or(0.0);
        let y_top = self.row_y.first().copied().unwrap_or(0.0);
        let y_bot = self.row_y.last().copied().unwrap_or(0.0);
        (x0, y_bot, x1, y_top)
    }

    /// Render as delimiter-separated rows (e.g. `'\t'` for TSV). The delimiter,
    /// CR and LF are stripped from cell text so every row stays a single line.
    pub fn to_delimited(&self, delim: char) -> String {
        let mut out = String::new();
        for (r, row) in self.cells.iter().enumerate() {
            if r > 0 {
                out.push('\n');
            }
            for (c, cell) in row.iter().enumerate() {
                if c > 0 {
                    out.push(delim);
                }
                for ch in cell.chars() {
                    if ch != delim && ch != '\n' && ch != '\r' {
                        out.push(ch);
                    }
                }
            }
        }
        out
    }

    /// Render as tab-separated values.
    pub fn to_tsv(&self) -> String {
        self.to_delimited('\t')
    }

    /// Render as RFC-4180 CSV (cells with `,`/`"`/newline are quoted, inner
    /// quotes doubled).
    pub fn to_csv(&self) -> String {
        let mut out = String::new();
        for (r, row) in self.cells.iter().enumerate() {
            if r > 0 {
                out.push_str("\r\n");
            }
            for (c, cell) in row.iter().enumerate() {
                if c > 0 {
                    out.push(',');
                }
                if cell.contains([',', '"', '\n', '\r']) {
                    out.push('"');
                    for ch in cell.chars() {
                        if ch == '"' {
                            out.push('"');
                        }
                        out.push(ch);
                    }
                    out.push('"');
                } else {
                    out.push_str(cell);
                }
            }
        }
        out
    }
}

// Bounds against adversarial / pathological span sets (mirrors the anti-hang
// budgets used elsewhere in the crate).
const MAX_SPANS: usize = 50_000;
const MAX_TABLES: usize = 1_000;
/// A table needs at least this many rows and columns.
const MIN_ROWS: usize = 3;
const MIN_COLS: usize = 2;
/// Rows separated by more than this many `unit`s start a new vertical band.
const BAND_GAP: f64 = 2.2;
/// A baseline-row cluster spans this many `unit`s of y.
const ROW_TOL: f64 = 0.6;
/// The minimum width of a column gutter, in `unit`s (a real gutter is at least
/// a wide space; this rejects inter-word gaps).
const MIN_GUTTER: f64 = 0.4;
/// Fraction of a band's rows a gutter must stay clear of for it to count as a
/// column separator (the rest may be crossed by an over-wide cell).
const GUTTER_SUPPORT: f64 = 0.85;
/// If the median non-empty cell fills more than this fraction of its column,
/// the band is prose columns, not a table.
const PROSE_FILL: f64 = 0.80;
/// A row whose text occupies a single run this wide a fraction of the band is a
/// full-width spanning row (a title, caption or subtotal drawn as one string);
/// it abstains from the column-gutter vote so it cannot erase a separator.
const SPAN_FULL: f64 = 0.9;
/// Ruled-line inputs beyond this are ignored (anti-adversarial bound).
const MAX_RULES: usize = 20_000;
/// A vertical rule must cover at least this fraction of a band's height to be
/// trusted as a drawn column separator for that band.
const RULE_COVER: f64 = 0.5;

/// Detect tables among a page's extracted text spans (in PDF user space, y-up).
/// Returns one [`Table`] per detected grid, in top-to-bottom page order.
pub fn detect_tables(spans: &[TextSpan]) -> Vec<Table> {
    detect_tables_with_rules(spans, &[])
}

/// [`detect_tables`], additionally informed by drawn ruled lines (captured via
/// [`ContentInterpreter::with_rule_sink`]). Vertical rules that span a band act
/// as **forced column separators** — they establish columns even where the text
/// alone leaves no clean whitespace gutter — and a band whose columns come from
/// drawn rules skips the prose-fill guard (a drawn grid is stronger evidence
/// than cell-width statistics). With no rules this is exactly [`detect_tables`].
///
/// [`ContentInterpreter::with_rule_sink`]: crate::interpreter::ContentInterpreter::with_rule_sink
pub fn detect_tables_with_rules(spans: &[TextSpan], rules: &[RuleLine]) -> Vec<Table> {
    // Drop empty and non-finite spans up front: a NaN x/y/advance would make the
    // downstream `partial_cmp` sort comparators violate a total order (a panic on
    // Rust ≥ 1.81) and could poison the gutter sweepline — neither is allowed
    // under the no-panic / no-hang corpus contract.
    let items: Vec<&TextSpan> = spans
        .iter()
        .filter(|s| {
            !s.text.trim().is_empty()
                && s.x.is_finite()
                && s.y.is_finite()
                && s.advance.is_finite()
                && s.size.is_finite()
        })
        .take(MAX_SPANS)
        .collect();
    if items.len() < MIN_ROWS * MIN_COLS {
        return Vec::new();
    }
    let unit = median_size(&items).max(1.0);

    // Sanitize rules once: finite, non-empty, bounded count.
    let rules: Vec<&RuleLine> = rules
        .iter()
        .filter(|r| r.pos.is_finite() && r.start.is_finite() && r.end.is_finite() && !r.is_empty())
        .take(MAX_RULES)
        .collect();

    let rows = group_rows(&items, unit);
    if rows.len() < MIN_ROWS {
        return Vec::new();
    }

    let mut tables = Vec::new();
    for band in segment_bands(&rows, &items, unit) {
        if band.len() < MIN_ROWS {
            continue;
        }
        if let Some(t) = detect_in_band(&band, &items, unit, &rules) {
            tables.push(t);
            if tables.len() >= MAX_TABLES {
                break;
            }
        }
    }
    tables
}

/// Median effective font size — the layout `unit`.
fn median_size(items: &[&TextSpan]) -> f64 {
    let mut sizes: Vec<f64> = items
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

/// Cluster span indices into baseline rows, top-to-bottom. Returns rows as
/// index lists into `items`; each row's spans share a baseline within `ROW_TOL`.
fn group_rows(items: &[&TextSpan], unit: f64) -> Vec<Vec<usize>> {
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by(|&a, &b| {
        items[b]
            .y
            .partial_cmp(&items[a].y)
            .unwrap_or(Ordering::Equal)
    });
    let tol = unit * ROW_TOL;
    let mut rows: Vec<Vec<usize>> = Vec::new();
    let mut cur_y = f64::INFINITY;
    for i in order {
        if rows.is_empty() || (cur_y - items[i].y).abs() > tol {
            rows.push(Vec::new());
            cur_y = items[i].y;
        }
        rows.last_mut().unwrap().push(i);
    }
    rows
}

/// A row's representative baseline (its topmost span's y).
fn row_baseline(row: &[usize], items: &[&TextSpan]) -> f64 {
    row.iter()
        .map(|&i| items[i].y)
        .fold(f64::NEG_INFINITY, f64::max)
}

/// Split top-to-bottom rows into bands at baseline gaps wider than `BAND_GAP`.
fn segment_bands(rows: &[Vec<usize>], items: &[&TextSpan], unit: f64) -> Vec<Vec<Vec<usize>>> {
    let mut bands: Vec<Vec<Vec<usize>>> = Vec::new();
    let mut cur: Vec<Vec<usize>> = Vec::new();
    let mut prev_y = f64::NAN;
    for row in rows {
        let y = row_baseline(row, items);
        if !cur.is_empty() && (prev_y - y) > BAND_GAP * unit {
            bands.push(std::mem::take(&mut cur));
        }
        cur.push(row.clone());
        prev_y = y;
    }
    if !cur.is_empty() {
        bands.push(cur);
    }
    bands
}

/// Try to read one band of rows as a table.
fn detect_in_band(
    band: &[Vec<usize>],
    items: &[&TextSpan],
    unit: f64,
    rules: &[&RuleLine],
) -> Option<Table> {
    // Band content x-extent.
    let mut x0 = f64::INFINITY;
    let mut x1 = f64::NEG_INFINITY;
    for row in band {
        for &i in row {
            let (a, b) = items[i].x_bounds();
            x0 = x0.min(a);
            x1 = x1.max(b);
        }
    }
    if !(x0.is_finite() && x1.is_finite()) || x1 - x0 <= 0.0 {
        return None;
    }

    // Drawn vertical rules that span this band are forced column separators —
    // stronger evidence than whitespace, and available even where cells sit too
    // close for a clean gutter.
    let ruled_x = band_rule_columns(band, items, unit, x0, x1, rules);
    let gutters = find_gutters(band, items, unit, x0, x1);
    if gutters.is_empty() && ruled_x.is_empty() {
        return None; // single column → not a table
    }

    // Column boundaries: band edges plus each gutter's midpoint plus each
    // spanning rule's x, deduplicated (a rule usually sits inside the gutter
    // the text leaves for it — keep one separator, preferring the rule).
    let mut col_x = Vec::with_capacity(gutters.len() + ruled_x.len() + 2);
    col_x.push(x0);
    col_x.extend(ruled_x.iter().copied());
    for &(g0, g1) in &gutters {
        // Skip a whitespace gutter that a drawn rule already separates.
        if !ruled_x.iter().any(|&rx| rx >= g0 && rx <= g1) {
            col_x.push((g0 + g1) / 2.0);
        }
    }
    col_x.push(x1);
    col_x.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    // Merge separators closer than a hair (duplicate rules / rule-on-edge).
    col_x.dedup_by(|b, a| (*b - *a).abs() < unit * 0.2);
    let ncols = col_x.len() - 1;
    if ncols < MIN_COLS {
        return None;
    }

    // Trim leading/trailing rows that occupy a single column — captions and
    // titles above or below the grid — keeping interior single-column rows
    // (sub-headers) in place.
    let col_of = |x: f64| -> usize {
        // Column whose [col_x[c], col_x[c+1]) contains x (last column inclusive).
        for c in 0..ncols {
            if x < col_x[c + 1] || c + 1 == ncols {
                return c;
            }
        }
        ncols - 1
    };
    let row_cols = |row: &[usize]| -> usize {
        let mut seen = vec![false; ncols];
        for &i in row {
            let (a, b) = items[i].x_bounds();
            seen[col_of((a + b) / 2.0)] = true;
        }
        seen.iter().filter(|v| **v).count()
    };
    let mut lo = 0;
    let mut hi = band.len();
    while lo < hi && row_cols(&band[lo]) < 2 {
        lo += 1;
    }
    while hi > lo && row_cols(&band[hi - 1]) < 2 {
        hi -= 1;
    }
    let body = &band[lo..hi];
    if body.len() < MIN_ROWS {
        return None;
    }

    // A table needs at least half its rows (and no fewer than two) to actually
    // span multiple columns. A minimal 3-row table may have a single spanning
    // interior row (a sub-header / subtotal) between two multi-column rows.
    let multi = body.iter().filter(|r| row_cols(r) >= 2).count();
    if multi < body.len().div_ceil(2).max(MIN_COLS) {
        return None;
    }

    // Assemble cells.
    let mut cells: Vec<Vec<String>> = Vec::with_capacity(body.len());
    for row in body {
        let mut buckets: Vec<Vec<usize>> = vec![Vec::new(); ncols];
        for &i in row {
            let (a, b) = items[i].x_bounds();
            buckets[col_of((a + b) / 2.0)].push(i);
        }
        let cell_row = buckets
            .iter()
            .map(|b| join_cell(b, items))
            .collect::<Vec<_>>();
        cells.push(cell_row);
    }

    // Require at least two columns that carry content in two or more rows
    // (rejects a single column of text split by a lone coincidental gutter).
    let filled_cols = (0..ncols)
        .filter(|&c| cells.iter().filter(|r| !r[c].is_empty()).count() >= 2)
        .count();
    if filled_cols < MIN_COLS {
        return None;
    }

    // Prose guard: real table cells leave whitespace; prose columns fill width.
    // A band whose columns are established by drawn rules is exempt — a drawn
    // grid is direct evidence of tabular intent, and its cells may legitimately
    // fill their columns (e.g. wrapped text inside ruled cells).
    if ruled_x.is_empty() && ncols >= 2 && median_fill(&cells, items, body, &col_x) > PROSE_FILL {
        return None;
    }

    Some(Table {
        cells,
        col_x,
        row_y: row_separators(body, items),
    })
}

/// Interior x-positions of drawn vertical rules that cover enough of this
/// band's y-extent to be trusted as column separators, ascending and deduped.
fn band_rule_columns(
    band: &[Vec<usize>],
    items: &[&TextSpan],
    unit: f64,
    x0: f64,
    x1: f64,
    rules: &[&RuleLine],
) -> Vec<f64> {
    // Band y-extent from row baselines, padded by a line so a rule that stops
    // at the text's cap height still counts.
    let mut y_top = f64::NEG_INFINITY;
    let mut y_bot = f64::INFINITY;
    for row in band {
        let y = row_baseline(row, items);
        y_top = y_top.max(y);
        y_bot = y_bot.min(y);
    }
    if !(y_top.is_finite() && y_bot.is_finite()) {
        return Vec::new();
    }
    let band_h = (y_top - y_bot).max(unit);

    let mut xs: Vec<f64> = rules
        .iter()
        .filter(|r| r.vertical)
        // Interior to the band's text extent (a hair of tolerance so a rule
        // exactly on the text edge is not counted as a phantom outer column).
        .filter(|r| r.pos > x0 + unit * 0.2 && r.pos < x1 - unit * 0.2)
        // Must overlap the band's y-range substantially.
        .filter(|r| {
            let overlap = r.end.min(y_top + unit) - r.start.max(y_bot - unit);
            overlap >= band_h * RULE_COVER
        })
        .map(|r| r.pos)
        .collect();
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    xs.dedup_by(|b, a| (*b - *a).abs() < unit * 0.2);
    xs
}

/// Find column gutters: maximal interior x-ranges that stay clear of text in
/// all but a few of the band's rows. Returned ascending, each `(start, end)`.
fn find_gutters(
    band: &[Vec<usize>],
    items: &[&TextSpan],
    unit: f64,
    x0: f64,
    x1: f64,
) -> Vec<(f64, f64)> {
    // Sweepline of per-row merged occupied intervals: a +1/-1 event pair per
    // merged interval, so the running sum at any x is the number of rows whose
    // text covers x. (Within a row the intervals are merged/disjoint, so each
    // row contributes at most +1 at a given x.)
    let span_full = (x1 - x0) * SPAN_FULL;
    let mut events: Vec<(f64, i32)> = Vec::new();
    for row in band {
        let mut iv: Vec<(f64, f64)> = row.iter().map(|&i| items[i].x_bounds()).collect();
        iv.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal));
        let mut merged: Vec<(f64, f64)> = Vec::new();
        for (a, b) in iv {
            if let Some(last) = merged.last_mut() {
                if a <= last.1 {
                    last.1 = last.1.max(b);
                    continue;
                }
            }
            merged.push((a, b));
        }
        // A full-width single run is a spanning row (title / caption / subtotal):
        // it abstains so it cannot veto an interior gutter that the data rows
        // leave clear.
        if merged.len() == 1 && merged[0].1 - merged[0].0 >= span_full {
            continue;
        }
        for (a, b) in merged {
            events.push((a, 1));
            events.push((b, -1));
        }
    }
    if events.is_empty() {
        return Vec::new();
    }
    // Ends before starts at equal x, so two touching cells leave no spurious
    // zero-coverage sliver between them.
    events.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(Ordering::Equal)
            .then(a.1.cmp(&b.1))
    });

    // Round (not floor) and tolerate at least one crossing once a band has four
    // or more rows, so a single over-wide cell (or a spanning row that did not
    // abstain above) cannot delete an otherwise-clean column separator. A 3-row
    // band stays strict — one bad row out of three is too many.
    let max_cross = (((1.0 - GUTTER_SUPPORT) * band.len() as f64).round() as i32)
        .max(if band.len() >= 4 { 1 } else { 0 });
    let min_w = unit * MIN_GUTTER;

    // Walk the coverage segments, collecting maximal low-coverage runs.
    let mut gutters: Vec<(f64, f64)> = Vec::new();
    let mut running = 0i32;
    let mut prev_x = x0;
    let mut open: Option<f64> = None; // start of the current gutter run
    let mut k = 0;
    while k < events.len() {
        let x = events[k].0;
        if x > prev_x {
            // Coverage of the open segment (prev_x, x) is `running`.
            if running <= max_cross {
                open.get_or_insert(prev_x);
            } else if let Some(g0) = open.take() {
                push_gutter(&mut gutters, g0, prev_x, x0, x1, min_w);
            }
        }
        while k < events.len() && events[k].0 == x {
            running += events[k].1;
            k += 1;
        }
        prev_x = x;
    }
    if let Some(g0) = open.take() {
        push_gutter(&mut gutters, g0, prev_x, x0, x1, min_w);
    }
    gutters
}

/// Keep a gutter run if it is interior (not touching a band edge) and wide
/// enough to be a real column gap rather than an inter-word space.
fn push_gutter(out: &mut Vec<(f64, f64)>, g0: f64, g1: f64, x0: f64, x1: f64, min_w: f64) {
    if g0 > x0 && g1 < x1 && g1 - g0 >= min_w {
        out.push((g0, g1));
    }
}

/// Join a cell's spans left-to-right, inserting a space across wide gaps.
fn join_cell(bucket: &[usize], items: &[&TextSpan]) -> String {
    if bucket.is_empty() {
        return String::new();
    }
    let mut ord = bucket.to_vec();
    ord.sort_by(|&a, &b| {
        items[a]
            .x
            .partial_cmp(&items[b].x)
            .unwrap_or(Ordering::Equal)
    });
    let mut out = String::new();
    let mut prev_end: Option<f64> = None;
    for &i in &ord {
        let s = items[i];
        if let Some(px) = prev_end {
            let gap = s.x_bounds().0 - px;
            if gap > s.size as f64 * 0.25
                && !out.ends_with(char::is_whitespace)
                && !s.text.starts_with(char::is_whitespace)
            {
                out.push(' ');
            }
        }
        out.push_str(s.text.trim_end_matches(['\n', '\r']));
        prev_end = Some(s.x_bounds().1);
    }
    out.trim().to_string()
}

/// Row separator y-positions (descending), one more than the number of rows.
fn row_separators(body: &[Vec<usize>], items: &[&TextSpan]) -> Vec<f64> {
    let baseline = |row: &[usize]| row_baseline(row, items);
    let size_of = |row: &[usize]| {
        row.iter()
            .map(|&i| items[i].size as f64)
            .fold(0.0_f64, f64::max)
            .max(1.0)
    };
    let mut sep = Vec::with_capacity(body.len() + 1);
    // Top edge: a little above the first row's baseline.
    sep.push(baseline(&body[0]) + 0.8 * size_of(&body[0]));
    for w in body.windows(2) {
        sep.push((baseline(&w[0]) + baseline(&w[1])) / 2.0);
    }
    // Bottom edge: a little below the last row's baseline.
    let last = &body[body.len() - 1];
    sep.push(baseline(last) - 0.2 * size_of(last));
    sep
}

/// Median fraction of column width that non-empty cells fill (the prose guard).
fn median_fill(
    cells: &[Vec<String>],
    items: &[&TextSpan],
    body: &[Vec<usize>],
    col_x: &[f64],
) -> f64 {
    let ncols = col_x.len() - 1;
    let col_of = |x: f64| -> usize {
        for c in 0..ncols {
            if x < col_x[c + 1] || c + 1 == ncols {
                return c;
            }
        }
        ncols - 1
    };
    let mut fills: Vec<f64> = Vec::new();
    for (r, row) in body.iter().enumerate() {
        // Per-column text extent for this row.
        let mut ext: Vec<(f64, f64)> = vec![(f64::INFINITY, f64::NEG_INFINITY); ncols];
        for &i in row {
            let (a, b) = items[i].x_bounds();
            let c = col_of((a + b) / 2.0);
            ext[c].0 = ext[c].0.min(a);
            ext[c].1 = ext[c].1.max(b);
        }
        for c in 0..ncols {
            if cells[r][c].is_empty() || ext[c].1 < ext[c].0 {
                continue;
            }
            let width = col_x[c + 1] - col_x[c];
            if width > 0.0 {
                fills.push(((ext[c].1 - ext[c].0) / width).clamp(0.0, 1.0));
            }
        }
    }
    if fills.is_empty() {
        return 0.0;
    }
    fills.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    fills[fills.len() / 2]
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

    /// Build a grid: `rows` of cells at the given column x-origins, one baseline
    /// per row (`y` decreasing by `lead`).
    fn grid(cells: &[&[&str]], col_x: &[f64], y0: f64, lead: f64, size: f32) -> Vec<TextSpan> {
        let mut out = Vec::new();
        for (r, row) in cells.iter().enumerate() {
            let y = y0 - r as f64 * lead;
            for (c, &t) in row.iter().enumerate() {
                if !t.is_empty() {
                    out.push(span(t, col_x[c], y, size));
                }
            }
        }
        out
    }

    #[test]
    fn clean_three_column_table() {
        let spans = grid(
            &[
                &["Name", "Qty", "Price"],
                &["Apple", "3", "1.20"],
                &["Pear", "12", "0.80"],
                &["Plum", "5", "2.50"],
            ],
            &[50.0, 200.0, 320.0],
            700.0,
            14.0,
            10.0,
        );
        let tables = detect_tables(&spans);
        assert_eq!(tables.len(), 1, "one table");
        let t = &tables[0];
        assert_eq!(t.rows(), 4);
        assert_eq!(t.cols(), 3);
        assert_eq!(t.cells[0], vec!["Name", "Qty", "Price"]);
        assert_eq!(t.cells[2], vec!["Pear", "12", "0.80"]);
    }

    #[test]
    fn caption_row_is_trimmed() {
        // A single-column title sits above a 2-column grid; it must be trimmed.
        let mut spans = vec![span("Table 1: results", 50.0, 740.0, 12.0)];
        spans.extend(grid(
            &[&["alpha", "100"], &["beta", "200"], &["gamma", "300"]],
            &[50.0, 260.0],
            700.0,
            14.0,
            10.0,
        ));
        let tables = detect_tables(&spans);
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].rows(), 3, "caption trimmed");
        assert_eq!(tables[0].cells[0], vec!["alpha", "100"]);
    }

    #[test]
    fn single_column_prose_is_not_a_table() {
        let spans = vec![
            span("This is a paragraph of running text", 50.0, 700.0, 12.0),
            span("that continues onto a second line and", 50.0, 686.0, 12.0),
            span("a third line with no column structure", 50.0, 672.0, 12.0),
            span("and finally a fourth closing line here", 50.0, 658.0, 12.0),
        ];
        assert!(detect_tables(&spans).is_empty(), "prose is not a table");
    }

    #[test]
    fn two_column_prose_is_rejected_by_fill_guard() {
        // Two columns whose cells fill nearly the whole column width (a typical
        // two-column article), not a table.
        let long_l = "left column text filling the whole width here";
        let long_r = "right column text also filling its column";
        let mut spans = Vec::new();
        for r in 0..5 {
            let y = 700.0 - r as f64 * 14.0;
            // Each line spans ~x[50..250] and ~x[270..470] with a clean gutter.
            spans.push(TextSpan {
                text: long_l.into(),
                x: 50.0,
                y,
                size: 12.0,
                advance: 195.0,
                mcid: None,
            });
            spans.push(TextSpan {
                text: long_r.into(),
                x: 270.0,
                y,
                size: 12.0,
                advance: 195.0,
                mcid: None,
            });
        }
        assert!(
            detect_tables(&spans).is_empty(),
            "two-column prose rejected by the fill guard"
        );
    }

    #[test]
    fn two_column_key_value_table_detected() {
        // Short cells with a wide gutter — a key/value table, not prose.
        let spans = grid(
            &[
                &["Name", "Ada"],
                &["Born", "1815"],
                &["Field", "Math"],
                &["City", "London"],
            ],
            &[50.0, 260.0],
            700.0,
            14.0,
            10.0,
        );
        let tables = detect_tables(&spans);
        assert_eq!(tables.len(), 1, "key/value table detected");
        assert_eq!(tables[0].cols(), 2);
        assert_eq!(tables[0].cells[1], vec!["Born", "1815"]);
    }

    #[test]
    fn two_separate_tables_split_by_gap() {
        let mut spans = grid(
            &[&["a", "1"], &["b", "2"], &["c", "3"]],
            &[50.0, 260.0],
            700.0,
            14.0,
            10.0,
        );
        // A second table far below (a band gap well over BAND_GAP * unit).
        spans.extend(grid(
            &[&["x", "9"], &["y", "8"], &["z", "7"]],
            &[50.0, 260.0],
            500.0,
            14.0,
            10.0,
        ));
        let tables = detect_tables(&spans);
        assert_eq!(tables.len(), 2, "two bands → two tables");
        assert_eq!(tables[0].cells[0], vec!["a", "1"]);
        assert_eq!(tables[1].cells[0], vec!["x", "9"]);
    }

    #[test]
    fn sparse_cell_left_empty() {
        // The middle row's second column is missing; that cell stays empty.
        let spans = grid(
            &[&["k1", "v1"], &["k2", ""], &["k3", "v3"], &["k4", "v4"]],
            &[50.0, 260.0],
            700.0,
            14.0,
            10.0,
        );
        let tables = detect_tables(&spans);
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].cells[1], vec!["k2", ""]);
    }

    #[test]
    fn empty_input_yields_no_tables() {
        assert!(detect_tables(&[]).is_empty());
        assert!(detect_tables(&[span("lonely", 0.0, 0.0, 12.0)]).is_empty());
    }

    #[test]
    fn csv_quotes_special_characters() {
        let t = Table {
            cells: vec![
                vec!["a,b".into(), "plain".into()],
                vec!["he said \"hi\"".into(), "x".into()],
            ],
            col_x: vec![0.0, 10.0, 20.0],
            row_y: vec![20.0, 10.0, 0.0],
        };
        assert_eq!(t.to_csv(), "\"a,b\",plain\r\n\"he said \"\"hi\"\"\",x");
        assert_eq!(t.to_tsv(), "a,b\tplain\nhe said \"hi\"\tx");
    }

    fn sp(text: &str, x: f64, y: f64, size: f32, advance: f64) -> TextSpan {
        TextSpan {
            text: text.into(),
            x,
            y,
            size,
            advance,
            mcid: None,
        }
    }

    #[test]
    fn non_finite_spans_do_not_panic() {
        // A degenerate text matrix can yield NaN/inf span coordinates; these must
        // be dropped, never reaching the sort comparators (a total-order panic on
        // Rust >= 1.81) or the gutter sweepline (a hang).
        let mut spans = grid(
            &[&["a", "1"], &["b", "2"], &["c", "3"]],
            &[50.0, 260.0],
            700.0,
            14.0,
            10.0,
        );
        spans.push(sp("nanx", f64::NAN, 690.0, 10.0, 30.0));
        spans.push(sp("infy", 100.0, f64::INFINITY, 10.0, 30.0));
        spans.push(sp("nansz", 120.0, 680.0, f32::NAN, 30.0));
        spans.push(sp("nanadv", 140.0, 670.0, 10.0, f64::NAN));
        let tables = detect_tables(&spans); // must not panic / hang
        for t in &tables {
            assert!(t.col_x.iter().all(|v| v.is_finite()), "finite col_x");
            assert!(t.row_y.iter().all(|v| v.is_finite()), "finite row_y");
        }
    }

    #[test]
    fn spanning_header_keeps_all_columns() {
        // A group header ("Revenue") crossing the gutter between two number
        // columns must not collapse them: the over-wide header row is tolerated.
        let mut spans = vec![
            sp("Year", 50.0, 716.0, 10.0, 20.0),
            sp("Revenue", 205.0, 716.0, 10.0, 130.0), // spans cols 2 & 3
        ];
        spans.extend(grid(
            &[
                &["2020", "100", "200"],
                &["2021", "150", "250"],
                &["2022", "180", "300"],
            ],
            &[50.0, 210.0, 320.0],
            700.0,
            14.0,
            10.0,
        ));
        let tables = detect_tables(&spans);
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].cols(), 3, "spanning header did not merge columns");
        assert_eq!(tables[0].cells.last().unwrap(), &vec!["2022", "180", "300"]);
    }

    #[test]
    fn interior_spanning_subtotal_kept() {
        // A full-width subtotal row in the middle abstains from the gutter vote,
        // so the 2-column structure survives.
        let mut spans = grid(
            &[&["Apple", "1.20"], &["Pear", "0.80"]],
            &[50.0, 260.0],
            700.0,
            14.0,
            10.0,
        );
        spans.push(sp("Subtotal so far this page", 50.0, 672.0, 10.0, 300.0));
        spans.extend(grid(
            &[&["Plum", "2.50"], &["Fig", "3.10"]],
            &[50.0, 260.0],
            658.0,
            14.0,
            10.0,
        ));
        let tables = detect_tables(&spans);
        assert_eq!(tables.len(), 1, "subtotal row did not split the table");
        assert_eq!(tables[0].cols(), 2);
    }

    #[test]
    fn three_column_prose_is_rejected() {
        // Three columns of justified prose, each cell filling its column — a
        // page layout, not a table. The prose guard must reject it for ncols > 2.
        let mut spans = Vec::new();
        for r in 0..6 {
            let y = 700.0 - r as f64 * 14.0;
            for &x in &[50.0, 230.0, 410.0] {
                spans.push(sp("column text filling width", x, y, 11.0, 165.0));
            }
        }
        assert!(
            detect_tables(&spans).is_empty(),
            "three-column prose is not a table"
        );
    }

    #[test]
    fn three_row_table_with_spanning_interior_row() {
        // header + a single-column interior sub-header + data — only 3 rows, the
        // middle one occupying one column. The multi-row threshold (#8 fix) must
        // still accept it (old code required ALL 3 rows to be multi-column).
        let mut spans = vec![
            sp("Item", 50.0, 700.0, 10.0, 25.0),
            sp("Qty", 200.0, 700.0, 10.0, 18.0),
            sp("Price", 320.0, 700.0, 10.0, 28.0),
        ];
        spans.push(sp("Sec", 50.0, 686.0, 10.0, 18.0)); // interior single-column
        spans.extend([
            sp("Apple", 50.0, 672.0, 10.0, 30.0),
            sp("3", 200.0, 672.0, 10.0, 6.0),
            sp("1.20", 320.0, 672.0, 10.0, 22.0),
        ]);
        let tables = detect_tables(&spans);
        assert_eq!(
            tables.len(),
            1,
            "3-row table with a spanning interior accepted"
        );
        assert_eq!(tables[0].cols(), 3);
    }

    #[test]
    fn trailing_caption_is_trimmed() {
        let mut spans = grid(
            &[&["a", "1"], &["b", "2"], &["c", "3"]],
            &[50.0, 260.0],
            700.0,
            14.0,
            10.0,
        );
        spans.push(sp(
            "Source: somewhere",
            50.0,
            700.0 - 3.0 * 14.0,
            10.0,
            120.0,
        ));
        let tables = detect_tables(&spans);
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].rows(), 3, "trailing caption trimmed off");
    }

    #[test]
    fn four_column_table() {
        let spans = grid(
            &[
                &["A", "1", "x", "9"],
                &["B", "2", "y", "8"],
                &["C", "3", "z", "7"],
            ],
            &[50.0, 160.0, 270.0, 380.0],
            700.0,
            14.0,
            10.0,
        );
        let tables = detect_tables(&spans);
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].cols(), 4);
        assert_eq!(tables[0].cells[1], vec!["B", "2", "y", "8"]);
    }

    // ---- Ruled-line-aware detection ----

    fn vrule(x: f64, y0: f64, y1: f64) -> RuleLine {
        RuleLine {
            vertical: true,
            pos: x,
            start: y0,
            end: y1,
        }
    }

    #[test]
    fn no_rules_is_identical_to_plain_detection() {
        let spans = grid(
            &[
                &["Name", "Qty", "Price"],
                &["Apple", "3", "1.20"],
                &["Pear", "12", "0.80"],
            ],
            &[50.0, 200.0, 320.0],
            700.0,
            14.0,
            10.0,
        );
        let plain = detect_tables(&spans);
        let with = detect_tables_with_rules(&spans, &[]);
        assert_eq!(plain.len(), with.len());
        assert_eq!(plain[0].cells, with[0].cells);
    }

    #[test]
    fn vertical_rule_forces_column_where_gutter_is_too_narrow() {
        // Cells sit close enough that the whitespace gutter (< MIN_GUTTER·unit
        // = 4pt at size 10) is rejected — text alone finds no columns. A drawn
        // vertical rule between them must establish the separator.
        let cells: &[&[&str]] = &[
            &["alpha", "100"],
            &["beta", "200"],
            &["gamma", "300"],
            &["delta", "400"],
        ];
        // Left cells ~x[50..50+5*5=75]; right cells start at 78 → 3pt gutter.
        let spans = grid(cells, &[50.0, 78.0], 700.0, 14.0, 10.0);
        assert!(
            detect_tables(&spans).is_empty(),
            "narrow gutter alone must not form a table"
        );
        let rules = [vrule(76.5, 640.0, 710.0)];
        let tables = detect_tables_with_rules(&spans, &rules);
        assert_eq!(tables.len(), 1, "rule forces the column split");
        assert_eq!(tables[0].cols(), 2);
        assert_eq!(tables[0].cells[1], vec!["beta", "200"]);
    }

    #[test]
    fn ruled_band_skips_prose_fill_guard() {
        // Two columns of width-filling text: rejected as prose without rules,
        // accepted as a table when a spanning vertical rule divides them.
        let long_l = "left column text filling the whole width here";
        let long_r = "right column text also filling its column";
        let mut spans = Vec::new();
        for r in 0..5 {
            let y = 700.0 - r as f64 * 14.0;
            spans.push(TextSpan {
                text: long_l.into(),
                x: 50.0,
                y,
                size: 12.0,
                advance: 195.0,
                mcid: None,
            });
            spans.push(TextSpan {
                text: long_r.into(),
                x: 270.0,
                y,
                size: 12.0,
                advance: 195.0,
                mcid: None,
            });
        }
        assert!(detect_tables(&spans).is_empty(), "prose without rules");
        let rules = [vrule(258.0, 630.0, 710.0)];
        let tables = detect_tables_with_rules(&spans, &rules);
        assert_eq!(tables.len(), 1, "drawn rule overrides the prose guard");
        assert_eq!(tables[0].cols(), 2);
    }

    #[test]
    fn short_rule_does_not_force_a_column() {
        // A vertical tick covering well under RULE_COVER of the band height
        // must not split a prose paragraph into a fake table.
        let spans = vec![
            sp(
                "This is a paragraph of running text",
                50.0,
                700.0,
                12.0,
                220.0,
            ),
            sp(
                "that continues onto a second line an",
                50.0,
                686.0,
                12.0,
                220.0,
            ),
            sp(
                "a third line with no column structur",
                50.0,
                672.0,
                12.0,
                220.0,
            ),
            sp(
                "and finally a fourth closing line he",
                50.0,
                658.0,
                12.0,
                220.0,
            ),
        ];
        let rules = [vrule(150.0, 695.0, 705.0)]; // 10pt of a ~42pt band
        assert!(
            detect_tables_with_rules(&spans, &rules).is_empty(),
            "short tick must not create a table"
        );
    }

    #[test]
    fn rule_inside_gutter_is_deduped_with_it() {
        // A rule drawn inside the whitespace gutter must yield ONE separator,
        // not a rule-column plus a gutter-column (which would make 3 columns).
        let spans = grid(
            &[&["a", "1"], &["b", "2"], &["c", "3"]],
            &[50.0, 260.0],
            700.0,
            14.0,
            10.0,
        );
        let rules = [vrule(150.0, 640.0, 710.0)];
        let tables = detect_tables_with_rules(&spans, &rules);
        assert_eq!(tables.len(), 1);
        assert_eq!(
            tables[0].cols(),
            2,
            "rule and gutter merge to one separator"
        );
    }

    #[test]
    fn adversarial_rules_are_bounded_and_finite_filtered() {
        // NaN/empty/duplicate rules must not panic or distort detection.
        let spans = grid(
            &[&["a", "1"], &["b", "2"], &["c", "3"]],
            &[50.0, 260.0],
            700.0,
            14.0,
            10.0,
        );
        let mut rules = vec![
            RuleLine {
                vertical: true,
                pos: f64::NAN,
                start: 0.0,
                end: 100.0,
            },
            RuleLine {
                vertical: true,
                pos: 150.0,
                start: 700.0,
                end: 700.0, // empty
            },
        ];
        for _ in 0..100 {
            rules.push(vrule(150.0, 640.0, 710.0)); // duplicates dedupe
        }
        let tables = detect_tables_with_rules(&spans, &rules);
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].cols(), 2);
    }
}
