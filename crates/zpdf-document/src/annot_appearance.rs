//! Appearance generation for markup & geometric annotations whose producer
//! shipped no `/AP` stream (PDF 32000-1 §12.5.6).
//!
//! Acrobat writes an appearance stream for every annotation, but many other
//! producers leave the visual representation implicit in the annotation's
//! geometry properties (`/QuadPoints`, `/Vertices`, `/L`, `/InkList`, `/C`,
//! `/IC`, …). A viewer is expected to synthesize the appearance from those.
//! This mirrors the form-widget generator in [`crate::forms`]: it returns a
//! [`GeneratedAppearance`] (a form XObject — `/BBox`, `/Matrix`, `/Resources`
//! and a content byte stream) that the interpreter's annotation painter
//! replays exactly like a real `/AP /N`, so **both render backends draw it
//! with no backend changes**.
//!
//! The synthesized content is emitted directly in default user space (the same
//! space as the annotation's `/Rect` and geometry arrays), and the `/BBox` is
//! set equal to the `/Rect`. The painter therefore maps `/BBox` onto `/Rect`
//! as the identity and clips to the rectangle — geometry that extends past
//! `/Rect` (as it should not, for a conforming file) is clipped, never scaled.
//!
//! Supported subtypes: text markup (`Highlight` / `Underline` / `StrikeOut` /
//! `Squiggly`, from `/QuadPoints`), geometric markup (`Square` / `Circle` /
//! `Line` / `Polygon` / `PolyLine` / `Ink`), and a conservative `Link` border.
//! Everything else (an existing `/AP`, `Widget`, `Popup`, `Text`/`FreeText`/
//! `Stamp` icons, …) is left to its own appearance or to the widget generator.

use zpdf_core::{Matrix, PdfDict, PdfName, PdfObject, Rect};
use zpdf_parser::PdfFile;

use crate::forms::GeneratedAppearance;

/// Caps bounding the synthesized content size against malformed / adversarial
/// geometry arrays (consistent with the existing anti-hang budgets).
const MAX_QUADS: usize = 20_000;
const MAX_POLY_POINTS: usize = 100_000;
const MAX_INK_PATHS: usize = 10_000;
const MAX_SQUIGGLE_SEGMENTS: usize = 4_000;
/// Hard ceiling on the synthesized content-stream size. Legitimate markup is a
/// few KB; this bounds adversarial geometry (mirrors the widget generator's
/// `MAX_VALUE_CHARS`). A generated appearance over this is dropped entirely.
const MAX_APPEARANCE_BYTES: usize = 1 << 20; // 1 MiB

/// Build a generated appearance for a markup / geometric annotation that has no
/// `/AP`, or `None` when the subtype is unsupported or nothing should be drawn.
/// `dict` is the annotation dictionary and `rect` its `/Rect`.
pub fn generate_annotation_appearance(
    file: &PdfFile,
    dict: &PdfDict,
    subtype: &str,
    rect: Rect,
) -> Option<GeneratedAppearance> {
    let rect = rect.normalize();
    if !(rect.width().is_finite() && rect.height().is_finite())
        || rect.width() <= 0.0
        || rect.height() <= 0.0
    {
        return None;
    }

    // The whole-annotation constant opacity (/CA) becomes an ExtGState alpha.
    let ca = read_num(file, dict, "CA").map(|v| v.clamp(0.0, 1.0));

    let mut body = Vec::new();
    let mut multiply = false;

    let drew = match subtype {
        "Highlight" => {
            // Highlights composite onto the page with Multiply so the marked
            // text shows through (matching Acrobat's generated appearance).
            multiply = true;
            highlight(file, dict, &mut body)
        }
        "Underline" => text_markup(file, dict, &mut body, Markup::Underline),
        "StrikeOut" => text_markup(file, dict, &mut body, Markup::StrikeOut),
        "Squiggly" => text_markup(file, dict, &mut body, Markup::Squiggly),
        "Square" => square(file, dict, rect, &mut body),
        "Circle" => circle(file, dict, rect, &mut body),
        "Line" => line(file, dict, &mut body),
        "Polygon" => polyline(file, dict, &mut body, true),
        "PolyLine" => polyline(file, dict, &mut body, false),
        "Ink" => ink(file, dict, &mut body),
        "Link" => link(file, dict, rect, &mut body),
        _ => false,
    };
    if !drew || body.is_empty() || body.len() > MAX_APPEARANCE_BYTES {
        return None;
    }

    // Wrap in q/Q and apply the blend/opacity ExtGState if one is needed.
    let gs = build_gs(multiply, ca);
    let mut content = Vec::new();
    push(&mut content, "q\n");
    if gs.is_some() {
        push(&mut content, "/GS0 gs\n");
    }
    content.extend_from_slice(&body);
    push(&mut content, "Q\n");

    Some(GeneratedAppearance {
        bbox: rect,
        matrix: Matrix::identity(),
        resources: build_resources(gs),
        content,
    })
}

// ---------------------------------------------------------------------------
// Subtype generators (each pushes content and reports whether it drew anything)
// ---------------------------------------------------------------------------

fn highlight(file: &PdfFile, dict: &PdfDict, out: &mut Vec<u8>) -> bool {
    let Some(quads) = read_quadpoints(file, dict) else {
        return false;
    };
    // No spec default; yellow is the universal convention for a highlight.
    let Some(color) = markup_color(file, dict, "C", vec![1.0, 1.0, 0.0]) else {
        return false;
    };
    let Some(op) = color_op(&color, false) else {
        return false;
    };
    push(out, &op);
    push(out, "\n");
    let mut any = false;
    for q in &quads {
        let (x0, y0, x1, y1) = quad_bounds(q);
        if x1 <= x0 || y1 <= y0 {
            continue;
        }
        push(
            out,
            &format!(
                "{} {} {} {} re\n",
                fmt(x0),
                fmt(y0),
                fmt(x1 - x0),
                fmt(y1 - y0)
            ),
        );
        any = true;
    }
    if any {
        push(out, "f\n");
    }
    any
}

enum Markup {
    Underline,
    StrikeOut,
    Squiggly,
}

fn text_markup(file: &PdfFile, dict: &PdfDict, out: &mut Vec<u8>, kind: Markup) -> bool {
    let Some(quads) = read_quadpoints(file, dict) else {
        return false;
    };
    let Some(color) = markup_color(file, dict, "C", vec![0.0]) else {
        return false;
    };
    let Some(op) = color_op(&color, true) else {
        return false;
    };
    push(out, &op);
    push(out, "\n1 J 1 j\n"); // round caps / joins
    let mut any = false;
    // Shared squiggle-segment budget across all quads — without it, MAX_QUADS ×
    // MAX_SQUIGGLE_SEGMENTS line ops could synthesize a multi-GB stream from one
    // annotation (mirrors `ink`'s shared budget).
    let mut squiggle_budget = MAX_POLY_POINTS;
    for q in &quads {
        let (x0, y0, x1, y1) = quad_bounds(q);
        if x1 <= x0 {
            continue;
        }
        let h = (y1 - y0).max(0.0);
        let lw = (h * 0.06).clamp(0.4, 4.0);
        push(out, &format!("{} w\n", fmt(lw)));
        match kind {
            Markup::Underline => {
                let y = y0 + h * 0.12;
                push(
                    out,
                    &format!("{} {} m {} {} l S\n", fmt(x0), fmt(y), fmt(x1), fmt(y)),
                );
            }
            Markup::StrikeOut => {
                let y = y0 + h * 0.45;
                push(
                    out,
                    &format!("{} {} m {} {} l S\n", fmt(x0), fmt(y), fmt(x1), fmt(y)),
                );
            }
            Markup::Squiggly => {
                if squiggle_budget == 0 {
                    break;
                }
                let amp = (h * 0.08).clamp(0.6, 2.5);
                let cap = squiggle_budget.min(MAX_SQUIGGLE_SEGMENTS);
                let used = squiggle(out, x0, x1, y0 + amp, amp, cap);
                squiggle_budget = squiggle_budget.saturating_sub(used);
            }
        }
        any = true;
    }
    any
}

fn square(file: &PdfFile, dict: &PdfDict, rect: Rect, out: &mut Vec<u8>) -> bool {
    let bw = border_width(file, dict);
    let fill = read_color(file, dict, "IC");
    // Border colour /C; if neither colour is given, fall back to a black border
    // so the annotation is at least visible.
    let stroke = read_color(file, dict, "C").or_else(|| fill.is_none().then(|| vec![0.0]));
    let Some(dr) = drawing_rect(file, dict, rect, bw) else {
        return false;
    };
    emit_shape_setup(out, &fill, &stroke, bw);
    push(
        out,
        &format!(
            "{} {} {} {} re {}\n",
            fmt(dr.x0),
            fmt(dr.y0),
            fmt(dr.width()),
            fmt(dr.height()),
            paint_op(fill.is_some(), stroke.is_some())
        ),
    );
    true
}

fn circle(file: &PdfFile, dict: &PdfDict, rect: Rect, out: &mut Vec<u8>) -> bool {
    let bw = border_width(file, dict);
    let fill = read_color(file, dict, "IC");
    let stroke = read_color(file, dict, "C").or_else(|| fill.is_none().then(|| vec![0.0]));
    let Some(dr) = drawing_rect(file, dict, rect, bw) else {
        return false;
    };
    emit_shape_setup(out, &fill, &stroke, bw);
    push_ellipse(out, dr);
    push(out, paint_op(fill.is_some(), stroke.is_some()));
    push(out, "\n");
    true
}

fn line(file: &PdfFile, dict: &PdfDict, out: &mut Vec<u8>) -> bool {
    let Some(l) = read_nums(file, dict, "L") else {
        return false;
    };
    if l.len() != 4 || !l.iter().all(|v| v.is_finite()) {
        return false;
    }
    let Some(stroke) = markup_color(file, dict, "C", vec![0.0]) else {
        return false;
    };
    let Some(op) = color_op(&stroke, true) else {
        return false;
    };
    let bw = border_width(file, dict).max(0.5);
    push(out, &op);
    push(out, "\n");
    push(out, &format!("{} w 1 J\n", fmt(bw)));
    push(
        out,
        &format!(
            "{} {} m {} {} l S\n",
            fmt(l[0]),
            fmt(l[1]),
            fmt(l[2]),
            fmt(l[3])
        ),
    );
    true
}

fn polyline(file: &PdfFile, dict: &PdfDict, out: &mut Vec<u8>, closed: bool) -> bool {
    let Some(v) = read_nums(file, dict, "Vertices") else {
        return false;
    };
    if v.len() < 4 || !v.iter().all(|x| x.is_finite()) {
        return false;
    }
    let n = (v.len() / 2).min(MAX_POLY_POINTS);
    // A polygon may have an interior colour; a polyline is stroke-only.
    let fill = if closed {
        read_color(file, dict, "IC")
    } else {
        None
    };
    let stroke = read_color(file, dict, "C").or_else(|| fill.is_none().then(|| vec![0.0]));
    let bw = border_width(file, dict).max(0.5);
    emit_shape_setup(out, &fill, &stroke, bw);
    push(out, "1 J 1 j\n");
    push(out, &format!("{} {} m\n", fmt(v[0]), fmt(v[1])));
    for i in 1..n {
        push(out, &format!("{} {} l\n", fmt(v[2 * i]), fmt(v[2 * i + 1])));
    }
    if closed {
        push(out, "h\n");
    }
    push(out, paint_op(fill.is_some(), stroke.is_some()));
    push(out, "\n");
    true
}

fn ink(file: &PdfFile, dict: &PdfDict, out: &mut Vec<u8>) -> bool {
    let Some(lists) = read_array(file, dict, "InkList") else {
        return false;
    };
    let Some(stroke) = markup_color(file, dict, "C", vec![0.0]) else {
        return false;
    };
    let Some(op) = color_op(&stroke, true) else {
        return false;
    };
    let bw = border_width(file, dict).max(0.5);
    let mut paths = String::new();
    let mut any = false;
    let mut budget = MAX_POLY_POINTS;
    for path_obj in lists.iter().take(MAX_INK_PATHS) {
        let Some(pts) = nums_of(file, path_obj) else {
            continue;
        };
        let n = (pts.len() / 2).min(budget);
        if n < 2 || !pts.iter().take(2 * n).all(|v| v.is_finite()) {
            continue;
        }
        budget = budget.saturating_sub(n);
        paths.push_str(&format!("{} {} m\n", fmt(pts[0]), fmt(pts[1])));
        for i in 1..n {
            paths.push_str(&format!("{} {} l\n", fmt(pts[2 * i]), fmt(pts[2 * i + 1])));
        }
        any = true;
    }
    if !any {
        return false;
    }
    push(out, &op);
    push(out, "\n");
    push(out, &format!("{} w 1 J 1 j\n", fmt(bw)));
    out.extend_from_slice(paths.as_bytes());
    push(out, "S\n");
    true
}

fn link(file: &PdfFile, dict: &PdfDict, rect: Rect, out: &mut Vec<u8>) -> bool {
    // Links are navigation, not marks; most viewers draw nothing. Only draw a
    // border when the file explicitly gives a colour AND an explicit non-zero
    // width — no width-1 default, which would box every hyperlink.
    let Some(color) = read_color(file, dict, "C") else {
        return false;
    };
    let Some(bw) = explicit_border_width(file, dict).filter(|w| *w > 0.0) else {
        return false;
    };
    let Some(dr) = drawing_rect(file, dict, rect, bw) else {
        return false;
    };
    let Some(op) = color_op(&color, true) else {
        return false;
    };
    push(out, &op);
    push(out, "\n");
    push(out, &format!("{} w\n", fmt(bw)));
    push(
        out,
        &format!(
            "{} {} {} {} re S\n",
            fmt(dr.x0),
            fmt(dr.y0),
            fmt(dr.width()),
            fmt(dr.height())
        ),
    );
    true
}

// ---------------------------------------------------------------------------
// Shared emit helpers
// ---------------------------------------------------------------------------

/// Inset `rect` by any `/RD` rectangle differences, then by half the border
/// width so a centred stroke stays inside. `None` if the result is degenerate.
fn drawing_rect(file: &PdfFile, dict: &PdfDict, rect: Rect, bw: f64) -> Option<Rect> {
    let mut r = rect;
    // /RD = [left top right bottom] differences between /Rect and the drawing.
    if let Some(rd) = read_nums(file, dict, "RD") {
        if rd.len() == 4 && rd.iter().all(|v| v.is_finite() && *v >= 0.0) {
            r = Rect::new(r.x0 + rd[0], r.y0 + rd[3], r.x1 - rd[2], r.y1 - rd[1]);
        }
    }
    let half = (bw / 2.0).max(0.0);
    let dr = Rect::new(r.x0 + half, r.y0 + half, r.x1 - half, r.y1 - half);
    // Signed extents, NOT Rect::width()/height() (which take abs() and would
    // accept an inverted rect when the border / RD inset exceeds the rect).
    (dr.x1 - dr.x0 > 0.0 && dr.y1 - dr.y0 > 0.0).then_some(dr)
}

fn emit_shape_setup(
    out: &mut Vec<u8>,
    fill: &Option<Vec<f64>>,
    stroke: &Option<Vec<f64>>,
    bw: f64,
) {
    if let Some(f) = fill {
        if let Some(op) = color_op(f, false) {
            push(out, &op);
            push(out, "\n");
        }
    }
    if let Some(s) = stroke {
        if let Some(op) = color_op(s, true) {
            push(out, &op);
            push(out, "\n");
        }
    }
    push(out, &format!("{} w\n", fmt(bw.max(0.0))));
}

/// The paint operator for the combination of fill and stroke present.
fn paint_op(fill: bool, stroke: bool) -> &'static str {
    match (fill, stroke) {
        (true, true) => "B",
        (true, false) => "f",
        (false, true) => "S",
        (false, false) => "n",
    }
}

/// Append an ellipse inscribed in `r` as four cubic Bézier arcs (no paint op).
fn push_ellipse(out: &mut Vec<u8>, r: Rect) {
    const K: f64 = 0.552_284_75; // 4/3 * (sqrt(2) - 1)
    let (cx, cy) = ((r.x0 + r.x1) / 2.0, (r.y0 + r.y1) / 2.0);
    let (rx, ry) = (r.width() / 2.0, r.height() / 2.0);
    let (ox, oy) = (rx * K, ry * K);
    push(out, &format!("{} {} m\n", fmt(cx + rx), fmt(cy)));
    let c = |o: &mut Vec<u8>, a, b, c2, d, e, f2| {
        push(
            o,
            &format!(
                "{} {} {} {} {} {} c\n",
                fmt(a),
                fmt(b),
                fmt(c2),
                fmt(d),
                fmt(e),
                fmt(f2)
            ),
        );
    };
    c(out, cx + rx, cy + oy, cx + ox, cy + ry, cx, cy + ry);
    c(out, cx - ox, cy + ry, cx - rx, cy + oy, cx - rx, cy);
    c(out, cx - rx, cy - oy, cx - ox, cy - ry, cx, cy - ry);
    c(out, cx + ox, cy - ry, cx + rx, cy - oy, cx + rx, cy);
    push(out, "h\n");
}

/// Append a triangle-wave squiggle from `x0` to `x1` oscillating above `y`,
/// then stroke it. Emits at most `max_seg` segments; returns the count emitted
/// (the caller decrements a shared budget).
fn squiggle(out: &mut Vec<u8>, x0: f64, x1: f64, y: f64, amp: f64, max_seg: usize) -> usize {
    let w = x1 - x0;
    let period = (amp * 2.0).max(2.0);
    let cap = max_seg.max(1) as i64;
    let n = ((w / period).ceil() as i64).clamp(1, cap);
    push(out, &format!("{} {} m\n", fmt(x0), fmt(y)));
    for i in 1..=n {
        let x = x0 + (i as f64) * w / (n as f64);
        let yy = if i % 2 == 1 { y + amp } else { y };
        push(out, &format!("{} {} l\n", fmt(x), fmt(yy)));
    }
    push(out, "S\n");
    n as usize
}

/// Build the `GS0` ExtGState dict carrying the blend mode and/or opacity, or
/// `None` when neither is needed.
fn build_gs(multiply: bool, ca: Option<f64>) -> Option<PdfDict> {
    let need_ca = ca.map(|a| a < 1.0).unwrap_or(false);
    if !multiply && !need_ca {
        return None;
    }
    let mut d = PdfDict::new();
    if multiply {
        d.insert(
            PdfName::new("BM"),
            PdfObject::Name(PdfName::new("Multiply")),
        );
    }
    if let Some(a) = ca.filter(|a| *a < 1.0) {
        d.insert(PdfName::new("ca"), PdfObject::Real(a));
        d.insert(PdfName::new("CA"), PdfObject::Real(a));
    }
    Some(d)
}

fn build_resources(gs: Option<PdfDict>) -> PdfDict {
    let mut res = PdfDict::new();
    if let Some(gs) = gs {
        let mut egs = PdfDict::new();
        egs.insert(PdfName::new("GS0"), PdfObject::Dict(gs));
        res.insert(PdfName::new("ExtGState"), PdfObject::Dict(egs));
    }
    res
}

// ---------------------------------------------------------------------------
// Property readers
// ---------------------------------------------------------------------------

/// Resolve a possibly-indirect object to a finite number.
fn as_num(file: &PdfFile, o: &PdfObject) -> Option<f64> {
    let v = match o {
        PdfObject::Ref(r) => file.resolve(*r).ok()?.as_f64().ok()?,
        other => other.as_f64().ok()?,
    };
    v.is_finite().then_some(v)
}

fn read_num(file: &PdfFile, dict: &PdfDict, key: &str) -> Option<f64> {
    dict.get(key).and_then(|o| as_num(file, o))
}

fn read_array(file: &PdfFile, dict: &PdfDict, key: &str) -> Option<Vec<PdfObject>> {
    match dict.get(key)? {
        PdfObject::Array(a) => Some(a.clone()),
        PdfObject::Ref(r) => match file.resolve(*r).ok()? {
            PdfObject::Array(a) => Some(a),
            _ => None,
        },
        _ => None,
    }
}

/// A flat numeric array (`/QuadPoints`, `/Vertices`, `/L`, `/RD`, an `/InkList`
/// sub-path). All elements must be numbers, else `None` (a misaligned array
/// would otherwise silently shift coordinates).
fn read_nums(file: &PdfFile, dict: &PdfDict, key: &str) -> Option<Vec<f64>> {
    nums_of(file, dict.get(key)?)
}

fn nums_of(file: &PdfFile, obj: &PdfObject) -> Option<Vec<f64>> {
    let arr = match obj {
        PdfObject::Array(a) => a.clone(),
        PdfObject::Ref(r) => match file.resolve(*r).ok()? {
            PdfObject::Array(a) => a,
            _ => return None,
        },
        _ => return None,
    };
    arr.iter().map(|o| as_num(file, o)).collect()
}

/// Read a 1/3/4-component colour array (`/C`, `/IC`), clamped to `[0,1]`. An
/// empty array (transparent) or wrong arity yields `None`.
fn read_color(file: &PdfFile, dict: &PdfDict, key: &str) -> Option<Vec<f64>> {
    let nums = read_nums(file, dict, key)?;
    match nums.len() {
        1 | 3 | 4 => Some(nums.iter().map(|v| v.clamp(0.0, 1.0)).collect()),
        _ => None,
    }
}

/// A markup colour (`/C`) with `default` applied only when the key is ABSENT.
/// A present-but-empty `/C []` is spec-transparent (PDF 32000-1 §12.5.6.2) — it
/// yields `None`, so the caller draws nothing rather than the default colour.
fn markup_color(file: &PdfFile, dict: &PdfDict, key: &str, default: Vec<f64>) -> Option<Vec<f64>> {
    match dict.get(key) {
        None => Some(default),
        Some(_) => read_color(file, dict, key),
    }
}

/// `/QuadPoints` as a list of 8-number quads (capped). Truncates a trailing
/// partial quad; `None` if there is not even one whole quad.
fn read_quadpoints(file: &PdfFile, dict: &PdfDict) -> Option<Vec<[f64; 8]>> {
    let nums = read_nums(file, dict, "QuadPoints")?;
    let count = (nums.len() / 8).min(MAX_QUADS);
    if count == 0 {
        return None;
    }
    let mut quads = Vec::with_capacity(count);
    for i in 0..count {
        let mut q = [0.0; 8];
        q.copy_from_slice(&nums[i * 8..i * 8 + 8]);
        quads.push(q);
    }
    Some(quads)
}

/// The border width: `/BS /W` wins over `/Border[2]`; default 1.
fn border_width(file: &PdfFile, dict: &PdfDict) -> f64 {
    explicit_border_width(file, dict).unwrap_or(1.0)
}

/// Like [`border_width`] but `None` when neither `/BS /W` nor `/Border[2]` is
/// present (no width-1 default). Used by Link, where a default border would
/// draw a box around every hyperlink — which no mainstream viewer does.
fn explicit_border_width(file: &PdfFile, dict: &PdfDict) -> Option<f64> {
    if let Some(bs) = read_subdict(file, dict, "BS") {
        if let Some(w) = read_num(file, &bs, "W") {
            return Some(w.max(0.0));
        }
    }
    if let Some(PdfObject::Array(b)) = dict.get("Border") {
        if let Some(w) = b.get(2).and_then(|o| as_num(file, o)) {
            return Some(w.max(0.0));
        }
    }
    None
}

fn read_subdict(file: &PdfFile, dict: &PdfDict, key: &str) -> Option<PdfDict> {
    match dict.get(key)? {
        PdfObject::Dict(d) => Some(d.clone()),
        PdfObject::Ref(r) => match file.resolve(*r).ok()? {
            PdfObject::Dict(d) => Some(d),
            _ => None,
        },
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// The axis-aligned bounds of a quad's four corners (robust to point ordering).
fn quad_bounds(q: &[f64; 8]) -> (f64, f64, f64, f64) {
    let xs = [q[0], q[2], q[4], q[6]];
    let ys = [q[1], q[3], q[5], q[7]];
    let min = |a: &[f64; 4]| a.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = |a: &[f64; 4]| a.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    (min(&xs), min(&ys), max(&xs), max(&ys))
}

/// A colour-setting operator: `g`/`rg`/`k` for fill, `G`/`RG`/`K` for stroke.
fn color_op(c: &[f64], stroke: bool) -> Option<String> {
    let nums = |c: &[f64]| c.iter().map(|v| fmt(*v)).collect::<Vec<_>>().join(" ");
    match c.len() {
        1 => Some(format!("{} {}", nums(c), if stroke { "G" } else { "g" })),
        3 => Some(format!("{} {}", nums(c), if stroke { "RG" } else { "rg" })),
        4 => Some(format!("{} {}", nums(c), if stroke { "K" } else { "k" })),
        _ => None,
    }
}

/// Format a coordinate for the content stream, guarding non-finite values and
/// clamping absurd magnitudes. Real page coordinates are far inside ±1e7;
/// anything beyond is clipped to `/Rect` by the form BBox anyway, and the clamp
/// keeps a single token short (a near-`f64::MAX` value would otherwise format to
/// a ~313-character fixed-point string).
fn fmt(v: f64) -> String {
    if v.is_finite() {
        format!("{:.3}", v.clamp(-1.0e7, 1.0e7))
    } else {
        "0".to_string()
    }
}

fn push(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(s.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::build_pdf;
    use crate::PdfDocument;

    #[test]
    fn quad_bounds_ignores_point_order() {
        // Acrobat order (TL, TR, BL, BR) and a rotated order give the same box.
        let acrobat = [10.0, 20.0, 30.0, 20.0, 10.0, 12.0, 30.0, 12.0];
        assert_eq!(quad_bounds(&acrobat), (10.0, 12.0, 30.0, 20.0));
        let ccw = [10.0, 12.0, 30.0, 12.0, 30.0, 20.0, 10.0, 20.0];
        assert_eq!(quad_bounds(&ccw), (10.0, 12.0, 30.0, 20.0));
    }

    #[test]
    fn color_op_arities() {
        assert_eq!(color_op(&[0.0], false).unwrap(), "0.000 g");
        assert_eq!(
            color_op(&[1.0, 0.0, 0.0], true).unwrap(),
            "1.000 0.000 0.000 RG"
        );
        assert_eq!(
            color_op(&[0.1, 0.2, 0.3, 0.4], false).unwrap(),
            "0.100 0.200 0.300 0.400 k"
        );
        assert!(color_op(&[0.0, 1.0], false).is_none());
    }

    #[test]
    fn paint_op_selection() {
        assert_eq!(paint_op(true, true), "B");
        assert_eq!(paint_op(true, false), "f");
        assert_eq!(paint_op(false, true), "S");
        assert_eq!(paint_op(false, false), "n");
    }

    /// Open a one-page doc whose single annotation is object 4, and return its
    /// parsed (generated) appearance via the public annotation path.
    fn annot_of(annot_body: &str) -> Option<crate::Annotation> {
        let doc = PdfDocument::open(build_pdf(&[
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Annots [4 0 R] >>",
            annot_body,
        ]))
        .expect("open");
        let page = doc.page(0).expect("page");
        doc.page_annotations(&page).into_iter().next()
    }

    #[test]
    fn highlight_generates_multiply_appearance() {
        let a = annot_of(
            "<< /Type /Annot /Subtype /Highlight /Rect [10 10 110 30] \
             /QuadPoints [10 30 110 30 10 10 110 10] /C [1 1 0] >>",
        )
        .expect("annotation");
        assert!(a.is_viewable(), "a generated appearance is viewable");
        let gen = a.generated.as_ref().expect("generated appearance");
        let s = String::from_utf8_lossy(&gen.content);
        assert!(s.contains("/GS0 gs"), "uses the blend ExtGState: {s}");
        assert!(s.contains("1.000 1.000 0.000 rg"), "yellow fill: {s}");
        assert!(s.contains(" re"), "fills a rectangle: {s}");
        // The ExtGState carries Multiply.
        let egs = gen
            .resources
            .get("ExtGState")
            .and_then(|o| o.as_dict().ok())
            .unwrap();
        let g0 = egs.get("GS0").and_then(|o| o.as_dict().ok()).unwrap();
        assert_eq!(g0.get_name("BM").unwrap(), "Multiply");
    }

    #[test]
    fn underline_defaults_to_black_stroke() {
        let a = annot_of(
            "<< /Type /Annot /Subtype /Underline /Rect [10 10 110 30] \
             /QuadPoints [10 30 110 30 10 10 110 10] >>",
        )
        .expect("annotation");
        let gen = a.generated.as_ref().expect("gen");
        let s = String::from_utf8_lossy(&gen.content);
        assert!(s.contains("0.000 G"), "black stroke colour: {s}");
        assert!(
            s.contains(" m ") && s.contains(" l S"),
            "strokes a line: {s}"
        );
    }

    #[test]
    fn existing_ap_is_not_overridden() {
        // A Square WITH a valid /AP keeps its stored appearance (no generation).
        let doc = PdfDocument::open(build_pdf(&[
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Annots [4 0 R] >>",
            "<< /Type /Annot /Subtype /Square /Rect [10 10 110 110] /AP << /N 5 0 R >> >>",
            "<< /Type /XObject /Subtype /Form /BBox [0 0 10 10] /Length 23 >>\nstream\n\
             1 0 0 rg 0 0 10 10 re f\nendstream",
        ]))
        .expect("open");
        let page = doc.page(0).expect("page");
        let a = doc
            .page_annotations(&page)
            .into_iter()
            .next()
            .expect("annotation");
        assert!(a.appearance.is_some(), "valid /AP resolved");
        assert!(
            a.generated.is_none(),
            "generation suppressed when /AP present"
        );
    }

    #[test]
    fn unsupported_subtype_generates_nothing() {
        let a =
            annot_of("<< /Type /Annot /Subtype /Text /Rect [10 10 30 30] >>").expect("annotation");
        assert!(a.generated.is_none());
        assert!(!a.is_viewable(), "no appearance, not viewable");
    }

    #[test]
    fn square_with_interior_colour_fills_and_strokes() {
        let a = annot_of(
            "<< /Type /Annot /Subtype /Square /Rect [10 10 110 110] \
             /IC [0 0 1] /C [1 0 0] /BS << /W 2 >> >>",
        )
        .expect("annotation");
        let gen = a.generated.as_ref().expect("gen");
        let s = String::from_utf8_lossy(&gen.content);
        assert!(s.contains("0.000 0.000 1.000 rg"), "blue interior: {s}");
        assert!(s.contains("1.000 0.000 0.000 RG"), "red border: {s}");
        assert!(s.contains("2.000 w"), "border width: {s}");
        assert!(
            s.trim_end().ends_with('B') || s.contains(" B\n"),
            "fill+stroke op: {s}"
        );
    }

    #[test]
    fn link_without_colour_is_invisible() {
        let a = annot_of("<< /Type /Annot /Subtype /Link /Rect [10 10 110 30] /Border [0 0 1] >>")
            .expect("annotation");
        assert!(a.generated.is_none(), "no /C → no visible border");
    }

    #[test]
    fn degenerate_rect_is_rejected() {
        let a = annot_of(
            "<< /Type /Annot /Subtype /Highlight /Rect [10 10 10 30] \
             /QuadPoints [10 30 10 30 10 10 10 10] /C [1 1 0] >>",
        )
        .expect("annotation");
        assert!(a.generated.is_none(), "zero-width rect generates nothing");
    }

    #[test]
    fn inverted_inset_rect_draws_nothing() {
        // A 20×20 Square with a 100pt border: the half-border inset inverts the
        // drawing rect. The signed-extent guard must reject it (not paint a box).
        let a = annot_of(
            "<< /Type /Annot /Subtype /Square /Rect [10 10 30 30] \
             /IC [0 1 0] /BS << /W 100 >> >>",
        )
        .expect("annotation");
        assert!(
            a.generated.is_none(),
            "border wider than rect → nothing drawn"
        );
    }

    #[test]
    fn empty_color_array_is_transparent() {
        // /C [] is spec-transparent: no opaque-yellow fallback.
        let a = annot_of(
            "<< /Type /Annot /Subtype /Highlight /Rect [10 10 110 30] \
             /QuadPoints [10 30 110 30 10 10 110 10] /C [] >>",
        )
        .expect("annotation");
        assert!(a.generated.is_none(), "empty /C draws nothing");
    }

    #[test]
    fn link_needs_explicit_border() {
        let none = annot_of("<< /Type /Annot /Subtype /Link /Rect [10 10 110 30] /C [0 0 1] >>")
            .expect("annotation");
        assert!(
            none.generated.is_none(),
            "/C but no explicit border → invisible"
        );

        let drawn = annot_of(
            "<< /Type /Annot /Subtype /Link /Rect [10 10 110 30] /C [0 0 1] /Border [0 0 2] >>",
        )
        .expect("annotation");
        assert!(
            drawn.generated.is_some(),
            "/C + explicit non-zero border → drawn"
        );
    }

    #[test]
    fn squiggly_with_many_quads_is_bounded() {
        // Without a shared segment budget this would synthesize a multi-GB
        // stream. It must complete and stay within the byte ceiling.
        let mut quads = String::new();
        for i in 0..6000 {
            let x = (i % 50) as f64 * 10.0;
            // A wide quad so each squiggle wants its full per-quad segment cap.
            quads.push_str(&format!("{x} 20 {} 20 {x} 10 {} 10 ", x + 500.0, x + 500.0));
        }
        let body = format!(
            "<< /Type /Annot /Subtype /Squiggly /Rect [0 0 600 30] \
             /QuadPoints [{quads}] /C [0 0 0] >>"
        );
        let a = annot_of(&body).expect("annotation");
        if let Some(gen) = &a.generated {
            assert!(
                gen.content.len() <= super::MAX_APPEARANCE_BYTES,
                "bounded content: {} bytes",
                gen.content.len()
            );
        }
    }
}
