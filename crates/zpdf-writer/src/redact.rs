//! True redaction: excise content under regions from a page's content stream.
//!
//! Unlike a cosmetic black box, [`IncrementalWriter::redact_page`] rewrites
//! the page's content stream(s) with the matching operators **removed**:
//! - text-show ops (`Tj`, `'`, `"`, `TJ`) whose glyph extent intersects a
//!   redaction rect are dropped;
//! - XObject draws (`Do`) and inline images (`BI…EI`) whose unit-square image
//!   footprint intersects a rect are dropped;
//! - path paint ops (`S`/`f`/`B`…) whose accumulated path bbox intersects a
//!   rect are downgraded to `n` (no-op paint), keeping any clip (`W`) intact.
//!
//! A filled box is (optionally) drawn over each region afterwards.
//!
//! The tracker mirrors just enough interpreter state to place operators in
//! user space: `cm` (CTM with q/Q stack), `BT`/`ET`, `Tm`/`Td`/`TD`/`T*`/`TL`,
//! font size for a conservative glyph-height estimate. Precise glyph metrics
//! are not needed — the extent estimate errs wider (more redaction), never
//! narrower.

use std::io::Cursor;

use zpdf_content::tokenizer::{ContentToken, ContentTokenizer};
use zpdf_core::{Matrix, ObjectId, PdfDict, PdfName, PdfObject, Rect, Result};

use crate::{invalid_data, IncrementalWriter};

/// Options for a redaction pass.
#[derive(Debug, Clone)]
pub struct RedactOptions {
    /// Draw an opaque box of this color over each redacted region.
    pub fill: Option<(f64, f64, f64)>,
}

impl Default for RedactOptions {
    fn default() -> Self {
        Self {
            fill: Some((0.0, 0.0, 0.0)),
        }
    }
}

impl IncrementalWriter {
    /// Remove page content that intersects any of `rects` (PDF user space,
    /// y-up), then optionally paint a fill box over each region.
    ///
    /// Text inside Form XObjects is not descended into (the whole XObject is
    /// dropped if its placement intersects); annotations overlapping a region
    /// are removed from the page.
    pub fn redact_page(
        &mut self,
        page_index: usize,
        rects: &[Rect],
        options: &RedactOptions,
    ) -> Result<()> {
        if rects.is_empty() {
            return Ok(());
        }
        for r in rects {
            if ![r.x0, r.y0, r.x1, r.y1].iter().all(|v| v.is_finite()) {
                return Err(invalid_data("redaction rectangles must be finite").into());
            }
        }
        let rects: Vec<Rect> = rects.iter().map(|r| r.normalize()).collect();

        let page_id = self.page_id(page_index)?;
        let page_obj = self.resolve_current(page_id)?;
        let mut page_dict = page_obj.as_dict()?.clone();

        // Collect the decoded content bytes of every /Contents stream.
        let content_ids: Vec<ObjectId> = match page_dict.get("Contents") {
            Some(PdfObject::Ref(r)) => vec![*r],
            Some(PdfObject::Array(arr)) => arr
                .iter()
                .filter_map(|o| match o {
                    PdfObject::Ref(r) => Some(*r),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        let mut content = Vec::new();
        for id in &content_ids {
            let bytes = self
                .document()
                .file()
                .resolve_stream_data(*id)
                .unwrap_or_default();
            content.extend_from_slice(&bytes);
            content.push(b'\n');
        }

        // Filter the combined stream.
        let mut filtered = filter_content(&content, &rects);

        // Optional fill boxes, appended in a fresh graphics state.
        if let Some((r, g, b)) = options.fill {
            filtered.extend_from_slice(b"q\n");
            filtered.extend_from_slice(format!("{} {} {} rg\n", r, g, b).as_bytes());
            for rect in &rects {
                filtered.extend_from_slice(
                    format!(
                        "{} {} {} {} re f\n",
                        rect.x0,
                        rect.y0,
                        rect.x1 - rect.x0,
                        rect.y1 - rect.y0
                    )
                    .as_bytes(),
                );
            }
            filtered.extend_from_slice(b"Q\n");
        }

        // Replace the page's contents with a single new stream.
        let stream_ref = self.try_add_flate_stream(&PdfDict::new(), &filtered)?;
        page_dict.insert(
            PdfName::new("Contents"),
            PdfObject::Ref(ObjectId(stream_ref.0, stream_ref.1 as u16)),
        );

        // Remove annotations whose /Rect intersects a redaction region (their
        // appearance may reproduce redacted content).
        if let Some(annots_obj) = page_dict.get("Annots").cloned() {
            let annots = match &annots_obj {
                PdfObject::Ref(r) => self
                    .resolve_current(*r)
                    .ok()
                    .and_then(|o| o.as_array().ok().map(|a| a.to_vec()))
                    .unwrap_or_default(),
                PdfObject::Array(a) => a.to_vec(),
                _ => Vec::new(),
            };
            let kept: Vec<PdfObject> = annots
                .into_iter()
                .filter(|a| {
                    let PdfObject::Ref(r) = a else { return true };
                    let Ok(obj) = self.resolve_current(*r) else {
                        return true;
                    };
                    let Ok(dict) = obj.as_dict() else { return true };
                    match annot_rect(dict) {
                        Some(ar) => !rects.iter().any(|rr| intersects(&ar, rr)),
                        None => true,
                    }
                })
                .collect();
            page_dict.insert(PdfName::new("Annots"), PdfObject::Array(kept));
        }

        self.overwrite_object(page_id, PdfObject::Dict(page_dict));
        Ok(())
    }
}

fn annot_rect(dict: &PdfDict) -> Option<Rect> {
    match dict.get("Rect") {
        Some(PdfObject::Array(a)) if a.len() == 4 => {
            let mut v = [0.0f64; 4];
            for (i, o) in a.iter().enumerate() {
                v[i] = match o {
                    PdfObject::Integer(n) => *n as f64,
                    PdfObject::Real(f) => *f,
                    _ => return None,
                };
            }
            Some(Rect::new(v[0], v[1], v[2], v[3]).normalize())
        }
        _ => None,
    }
}

fn intersects(a: &Rect, b: &Rect) -> bool {
    a.x0 < b.x1 && b.x0 < a.x1 && a.y0 < b.y1 && b.y0 < a.y1
}

/// Minimal graphics/text state for placing operators in user space.
#[derive(Clone)]
struct TrackState {
    ctm: Matrix,
    text_matrix: Matrix,
    line_matrix: Matrix,
    font_size: f64,
    leading: f64,
}

impl Default for TrackState {
    fn default() -> Self {
        Self {
            ctm: Matrix::identity(),
            text_matrix: Matrix::identity(),
            line_matrix: Matrix::identity(),
            font_size: 12.0,
            leading: 0.0,
        }
    }
}

/// Walk `content` and re-serialize it with operators that paint into any of
/// `rects` removed.
fn filter_content(content: &[u8], rects: &[Rect]) -> Vec<u8> {
    let mut out = Vec::with_capacity(content.len());
    let mut tokenizer = ContentTokenizer::new(content);
    let mut operands: Vec<PdfObject> = Vec::new();

    let mut state = TrackState::default();
    let mut stack: Vec<TrackState> = Vec::new();

    // Current path bbox (for path-paint suppression).
    let mut path_bbox: Option<Rect> = None;
    let mut cur_point: Option<(f64, f64)> = None;

    while let Some(token) = tokenizer.next_token() {
        match token {
            ContentToken::Operand(obj) => operands.push(obj),
            ContentToken::InlineImage { dict, data } => {
                // The inline image paints the CTM's unit square.
                let extent = unit_square_extent(&state.ctm);
                if !rects.iter().any(|r| intersects(&extent, r)) {
                    // Keep: re-serialize the BI/ID/EI section verbatim.
                    write_inline_image(&mut out, &dict, &data);
                }
                operands.clear();
            }
            ContentToken::Operator(op) => {
                let keep = apply_operator(
                    &op,
                    &operands,
                    &mut state,
                    &mut stack,
                    &mut path_bbox,
                    &mut cur_point,
                    rects,
                );
                match keep {
                    Keep::Yes => write_op(&mut out, &operands, &op),
                    Keep::No => {}
                    Keep::Replace(rep) => {
                        out.extend_from_slice(rep.as_bytes());
                        out.push(b'\n');
                    }
                }
                operands.clear();
            }
        }
    }
    out
}

enum Keep {
    Yes,
    No,
    /// Emit this replacement text instead (e.g. `n` for a suppressed paint).
    Replace(&'static str),
}

#[allow(clippy::too_many_arguments)]
fn apply_operator(
    op: &str,
    operands: &[PdfObject],
    state: &mut TrackState,
    stack: &mut Vec<TrackState>,
    path_bbox: &mut Option<Rect>,
    cur_point: &mut Option<(f64, f64)>,
    rects: &[Rect],
) -> Keep {
    let num = |i: usize| -> f64 {
        match operands.get(i) {
            Some(PdfObject::Integer(n)) => *n as f64,
            Some(PdfObject::Real(f)) => *f,
            _ => 0.0,
        }
    };

    match op {
        "q" => {
            stack.push(state.clone());
            Keep::Yes
        }
        "Q" => {
            if let Some(s) = stack.pop() {
                *state = s;
            }
            Keep::Yes
        }
        "cm" => {
            if operands.len() >= 6 {
                let m = Matrix::new(num(0), num(1), num(2), num(3), num(4), num(5));
                state.ctm = state.ctm.concat(&m);
            }
            Keep::Yes
        }

        // ---- text state ----
        "BT" => {
            state.text_matrix = Matrix::identity();
            state.line_matrix = Matrix::identity();
            Keep::Yes
        }
        "ET" => Keep::Yes,
        "Tf" => {
            state.font_size = num(1);
            Keep::Yes
        }
        "TL" => {
            state.leading = num(0);
            Keep::Yes
        }
        "Tm" => {
            if operands.len() >= 6 {
                let m = Matrix::new(num(0), num(1), num(2), num(3), num(4), num(5));
                state.text_matrix = m;
                state.line_matrix = m;
            }
            Keep::Yes
        }
        "Td" => {
            let m = Matrix::new(1.0, 0.0, 0.0, 1.0, num(0), num(1));
            state.line_matrix = state.line_matrix.concat(&m);
            state.text_matrix = state.line_matrix;
            Keep::Yes
        }
        "TD" => {
            state.leading = -num(1);
            let m = Matrix::new(1.0, 0.0, 0.0, 1.0, num(0), num(1));
            state.line_matrix = state.line_matrix.concat(&m);
            state.text_matrix = state.line_matrix;
            Keep::Yes
        }
        "T*" => {
            let m = Matrix::new(1.0, 0.0, 0.0, 1.0, 0.0, -state.leading);
            state.line_matrix = state.line_matrix.concat(&m);
            state.text_matrix = state.line_matrix;
            Keep::Yes
        }

        // ---- text showing ----
        "Tj" | "TJ" | "'" | "\"" => {
            if op == "'" || op == "\"" {
                let m = Matrix::new(1.0, 0.0, 0.0, 1.0, 0.0, -state.leading);
                state.line_matrix = state.line_matrix.concat(&m);
                state.text_matrix = state.line_matrix;
            }
            let extent = text_extent(operands, state);
            let redact = rects.iter().any(|r| intersects(&extent, r));
            // Advance the text matrix by the estimated width either way, so
            // subsequent shows on the same line stay placed.
            let width = estimate_text_width(operands, state.font_size);
            let adv = Matrix::new(1.0, 0.0, 0.0, 1.0, width, 0.0);
            state.text_matrix = state.text_matrix.concat(&adv);
            if redact {
                Keep::No
            } else {
                Keep::Yes
            }
        }

        // ---- XObjects ----
        "Do" => {
            let extent = unit_square_extent(&state.ctm);
            if rects.iter().any(|r| intersects(&extent, r)) {
                Keep::No
            } else {
                Keep::Yes
            }
        }

        // ---- paths ----
        "m" | "l" => {
            let (x, y) = user_point(num(0), num(1), &state.ctm);
            extend_bbox(path_bbox, x, y);
            *cur_point = Some((x, y));
            Keep::Yes
        }
        "c" => {
            for i in (0..6).step_by(2) {
                let (x, y) = user_point(num(i), num(i + 1), &state.ctm);
                extend_bbox(path_bbox, x, y);
            }
            Keep::Yes
        }
        "v" | "y" => {
            for i in (0..4).step_by(2) {
                let (x, y) = user_point(num(i), num(i + 1), &state.ctm);
                extend_bbox(path_bbox, x, y);
            }
            Keep::Yes
        }
        "re" => {
            let (x, y, w, h) = (num(0), num(1), num(2), num(3));
            for (px, py) in [(x, y), (x + w, y), (x, y + h), (x + w, y + h)] {
                let (ux, uy) = user_point(px, py, &state.ctm);
                extend_bbox(path_bbox, ux, uy);
            }
            Keep::Yes
        }
        "h" => Keep::Yes,

        // Path-painting: suppress (replace with `n`) when the path intersects.
        "S" | "s" | "f" | "F" | "f*" | "B" | "B*" | "b" | "b*" => {
            let hit = path_bbox
                .as_ref()
                .is_some_and(|bb| rects.iter().any(|r| intersects(bb, r)));
            *path_bbox = None;
            *cur_point = None;
            if hit {
                Keep::Replace("n")
            } else {
                Keep::Yes
            }
        }
        "n" => {
            *path_bbox = None;
            *cur_point = None;
            Keep::Yes
        }

        // Everything else passes through with state untouched.
        _ => Keep::Yes,
    }
}

fn user_point(x: f64, y: f64, ctm: &Matrix) -> (f64, f64) {
    (ctm.a * x + ctm.c * y + ctm.e, ctm.b * x + ctm.d * y + ctm.f)
}

fn extend_bbox(bbox: &mut Option<Rect>, x: f64, y: f64) {
    match bbox {
        Some(b) => {
            b.x0 = b.x0.min(x);
            b.y0 = b.y0.min(y);
            b.x1 = b.x1.max(x);
            b.y1 = b.y1.max(y);
        }
        None => *bbox = Some(Rect::new(x, y, x, y)),
    }
}

/// The user-space extent of the CTM-mapped unit square (image placement).
fn unit_square_extent(ctm: &Matrix) -> Rect {
    let corners = [
        user_point(0.0, 0.0, ctm),
        user_point(1.0, 0.0, ctm),
        user_point(0.0, 1.0, ctm),
        user_point(1.0, 1.0, ctm),
    ];
    let mut r = Rect::new(corners[0].0, corners[0].1, corners[0].0, corners[0].1);
    for (x, y) in corners.iter().skip(1) {
        r.x0 = r.x0.min(*x);
        r.y0 = r.y0.min(*y);
        r.x1 = r.x1.max(*x);
        r.y1 = r.y1.max(*y);
    }
    r
}

/// Conservative width estimate for a shown string: 0.55 em per byte (wider
/// than most body fonts average, so extents err on the redacting side).
fn estimate_text_width(operands: &[PdfObject], font_size: f64) -> f64 {
    let mut units = 0.0f64;
    for obj in operands {
        match obj {
            PdfObject::String(s) => units += s.0.len() as f64 * 0.55,
            PdfObject::Array(arr) => {
                for el in arr {
                    match el {
                        PdfObject::String(s) => units += s.0.len() as f64 * 0.55,
                        PdfObject::Integer(n) => units -= *n as f64 / 1000.0,
                        PdfObject::Real(f) => units -= f / 1000.0,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    units * font_size
}

/// User-space extent of a text-show op: from the current text-space origin,
/// the estimated width, and the font size for height (descender to ascender,
/// padded ±20%).
fn text_extent(operands: &[PdfObject], state: &TrackState) -> Rect {
    let trm = state.ctm.concat(&state.text_matrix);
    let width = estimate_text_width(operands, state.font_size);
    let h = state.font_size;
    let corners = [
        (0.0, -0.3 * h),
        (width, -0.3 * h),
        (0.0, 1.1 * h),
        (width, 1.1 * h),
    ];
    let mut pts = corners
        .iter()
        .map(|&(x, y)| (trm.a * x + trm.c * y + trm.e, trm.b * x + trm.d * y + trm.f));
    let first = pts.next().unwrap();
    let mut r = Rect::new(first.0, first.1, first.0, first.1);
    for (x, y) in pts {
        r.x0 = r.x0.min(x);
        r.y0 = r.y0.min(y);
        r.x1 = r.x1.max(x);
        r.y1 = r.y1.max(y);
    }
    r
}

/// Re-serialize one operator with its operands.
fn write_op(out: &mut Vec<u8>, operands: &[PdfObject], op: &str) {
    for obj in operands {
        write_operand(out, obj);
        out.push(b' ');
    }
    out.extend_from_slice(op.as_bytes());
    out.push(b'\n');
}

fn write_operand(out: &mut Vec<u8>, obj: &PdfObject) {
    // Content-stream operands are always direct objects; reuse the PDF
    // serializer via a cursor (it never fails on a Vec).
    let mut cursor = Cursor::new(Vec::new());
    let _ = crate::serialize::serialize_object_body(&mut cursor, obj);
    out.extend_from_slice(&cursor.into_inner());
}

/// Emit an inline image section (`BI <dict entries> ID <data> EI`).
fn write_inline_image(out: &mut Vec<u8>, dict: &PdfDict, data: &[u8]) {
    out.extend_from_slice(b"BI ");
    for (k, v) in &dict.0 {
        out.push(b'/');
        out.extend_from_slice(k.as_str().as_bytes());
        out.push(b' ');
        write_operand(out, v);
        out.push(b' ');
    }
    out.extend_from_slice(b"ID\n");
    out.extend_from_slice(data);
    out.extend_from_slice(b"\nEI\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_drops_text_inside_rect() {
        let content = b"BT /F1 12 Tf 1 0 0 1 50 700 Tm (secret) Tj ET\n\
                        BT /F1 12 Tf 1 0 0 1 50 100 Tm (public) Tj ET\n";
        let rects = [Rect::new(40.0, 690.0, 200.0, 720.0)];
        let filtered = filter_content(content, &rects);
        let text = String::from_utf8_lossy(&filtered);
        assert!(!text.contains("(secret)"), "redacted text removed: {text}");
        assert!(text.contains("(public)"), "other text kept: {text}");
    }

    #[test]
    fn filter_drops_image_inside_rect() {
        let content = b"q 100 0 0 100 50 500 cm /Im1 Do Q\n\
                        q 100 0 0 100 400 500 cm /Im2 Do Q\n";
        let rects = [Rect::new(40.0, 490.0, 160.0, 610.0)];
        let filtered = filter_content(content, &rects);
        let text = String::from_utf8_lossy(&filtered);
        assert!(!text.contains("/Im1 Do"), "hit image dropped: {text}");
        assert!(text.contains("/Im2 Do"), "clear image kept: {text}");
    }

    #[test]
    fn filter_neutralizes_path_paint_inside_rect() {
        let content = b"10 10 50 50 re f\n300 300 20 20 re f\n";
        let rects = [Rect::new(0.0, 0.0, 100.0, 100.0)];
        let filtered = filter_content(content, &rects);
        let text = String::from_utf8_lossy(&filtered);
        // First rect's paint is downgraded to `n`; second one keeps `f`.
        assert!(text.contains("10 10 50 50 re\nn"), "suppressed: {text}");
        assert!(text.contains("300 300 20 20 re\nf"), "kept: {text}");
    }

    #[test]
    fn q_stack_restores_ctm() {
        // Image inside q/Q with a translation must be measured with it, and
        // content after Q measured without it.
        let content = b"q 1 0 0 1 500 500 cm 50 0 0 50 0 0 cm /Im1 Do Q\n\
                        q 50 0 0 50 10 10 cm /Im2 Do Q\n";
        let rects = [Rect::new(490.0, 490.0, 600.0, 600.0)];
        let filtered = filter_content(content, &rects);
        let text = String::from_utf8_lossy(&filtered);
        assert!(!text.contains("/Im1"), "translated image dropped: {text}");
        assert!(text.contains("/Im2"), "origin image kept: {text}");
    }

    #[test]
    fn td_advance_places_later_lines() {
        // Line 2 moves down via Td and must be evaluated at its new position.
        let content = b"BT /F1 12 Tf 1 0 0 1 50 700 Tm (top) Tj 0 -600 Td (bottom) Tj ET\n";
        let rects = [Rect::new(0.0, 0.0, 600.0, 200.0)]; // bottom strip
        let filtered = filter_content(content, &rects);
        let text = String::from_utf8_lossy(&filtered);
        assert!(text.contains("(top)"), "top text kept: {text}");
        assert!(!text.contains("(bottom)"), "bottom text dropped: {text}");
    }
}
