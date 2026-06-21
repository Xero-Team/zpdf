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
//! `Line` / `Polygon` / `PolyLine` / `Ink`), `FreeText`, a conservative `Link`
//! border, `Text` note icons (a small vector glyph chosen by `/Name`), and
//! `Stamp` rubber-stamp badges (a bordered label decoded from `/Name`).
//! Everything else (an existing `/AP`, `Widget`, `Popup`, …) is left to its own
//! appearance or to the widget generator.

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

    // FreeText draws wrapped text (plus an optional background, border and
    // callout) and needs font resources, so it builds its own appearance
    // directly rather than going through the shared markup/geometry wrapper
    // below. It reuses the AcroForm text-layout engine in [`crate::forms`].
    if subtype == "FreeText" {
        return free_text(file, dict, rect);
    }

    // A Stamp draws a labeled badge and needs a font resource, so (like
    // FreeText) it builds its own appearance rather than going through the
    // shared markup/geometry wrapper below.
    if subtype == "Stamp" {
        return stamp(file, dict, rect);
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
        "Text" => text_icon(file, dict, rect, &mut body),
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
    // Fill the actual (possibly rotated/sheared) quadrilateral of each marked
    // run, not its axis-aligned bounding box — so rotated text highlights along
    // the baseline. All sub-paths are accumulated into one `f` so overlapping
    // quads composite once (important under the /Multiply blend, below).
    for q in &quads {
        let Some(oq) = oriented_quad(q) else {
            continue;
        };
        let c = oq.corners;
        push(out, &format!("{} {} m\n", fmt(c[0].0), fmt(c[0].1)));
        for p in &c[1..] {
            push(out, &format!("{} {} l\n", fmt(p.0), fmt(p.1)));
        }
        push(out, "h\n");
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
        // Work in the quad's own baseline frame so the mark follows rotated /
        // skewed text. `up` runs from the baseline (bottom) edge to the top
        // edge; its length is the run height.
        let Some(oq) = oriented_quad(q) else {
            continue;
        };
        let h = norm(oq.up);
        if h <= 0.0 {
            continue;
        }
        let lw = (h * 0.06).clamp(0.4, 4.0);
        push(out, &format!("{} w\n", fmt(lw)));
        // A point on the line at fraction `frac` of the height above the bottom.
        let along = |p: (f64, f64), frac: f64| (p.0 + oq.up.0 * frac, p.1 + oq.up.1 * frac);
        match kind {
            Markup::Underline => emit_seg(out, along(oq.b0, 0.12), along(oq.b1, 0.12)),
            Markup::StrikeOut => emit_seg(out, along(oq.b0, 0.45), along(oq.b1, 0.45)),
            Markup::Squiggly => {
                if squiggle_budget == 0 {
                    break;
                }
                let amp = (h * 0.08).clamp(0.6, 2.5);
                let cap = squiggle_budget.min(MAX_SQUIGGLE_SEGMENTS);
                let used = squiggle(out, &oq, amp, cap);
                squiggle_budget = squiggle_budget.saturating_sub(used);
            }
        }
        any = true;
    }
    any
}

/// Emit a single stroked segment `a → b`.
fn emit_seg(out: &mut Vec<u8>, a: (f64, f64), b: (f64, f64)) {
    push(
        out,
        &format!(
            "{} {} m {} {} l S\n",
            fmt(a.0),
            fmt(a.1),
            fmt(b.0),
            fmt(b.1)
        ),
    );
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
    let (a, b) = ((l[0], l[1]), (l[2], l[3]));
    push(
        out,
        &format!(
            "{} {} m {} {} l S\n",
            fmt(a.0),
            fmt(a.1),
            fmt(b.0),
            fmt(b.1)
        ),
    );
    // Line-ending styles (/LE [start end]); a closed head is filled with the
    // interior colour /IC when present, else left hollow (stroke only).
    let (le_start, le_end) = read_line_endings(file, dict);
    let ic = read_color(file, dict, "IC");
    emit_line_ending(out, a, sub(b, a), le_start, bw, &ic);
    emit_line_ending(out, b, sub(a, b), le_end, bw, &ic);
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
    // An open PolyLine carries line-ending styles at its first and last
    // vertices (a closed Polygon has no free ends).
    if !closed && n >= 2 {
        let (le_start, le_end) = read_line_endings(file, dict);
        let pt = |i: usize| (v[2 * i], v[2 * i + 1]);
        let (p0, p1) = (pt(0), pt(1));
        let (pl, pl_prev) = (pt(n - 1), pt(n - 2));
        let ic = read_color(file, dict, "IC");
        emit_line_ending(out, p0, sub(p1, p0), le_start, bw, &ic);
        emit_line_ending(out, pl, sub(pl_prev, pl), le_end, bw, &ic);
    }
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
// FreeText (12.5.6.6) — wrapped text drawn directly on the page
// ---------------------------------------------------------------------------

/// Build a generated appearance for a `FreeText` annotation with no `/AP`:
/// `/Contents` laid out per `/DA` (font / size / colour) and `/Q`, an optional
/// `/C` background, an optional border, and an optional `/CL` callout line with
/// an `/LE` ending. Text layout reuses the AcroForm engine in [`crate::forms`].
fn free_text(file: &PdfFile, dict: &PdfDict, rect: Rect) -> Option<GeneratedAppearance> {
    let (w, h) = (rect.width(), rect.height());
    if w <= 1.0 || h <= 1.0 {
        return None;
    }

    // /RD = [left top right bottom] insets between /Rect and the text region.
    let (il, it, ir, ib) = match read_nums(file, dict, "RD") {
        Some(rd) if rd.len() == 4 && rd.iter().all(|v| v.is_finite() && *v >= 0.0) => {
            (rd[0], rd[1], rd[2], rd[3])
        }
        _ => (0.0, 0.0, 0.0, 0.0),
    };

    let background = read_color(file, dict, "C"); // FreeText /C = background colour
    let border = explicit_border_width(file, dict).filter(|w| *w > 0.0);
    // Cap pathological /Contents lengths before they reach the word-wrapper
    // (shared with the widget generator).
    let contents = read_text_string(file, dict, "Contents").map(|s| {
        s.chars()
            .take(crate::forms::MAX_APPEARANCE_TEXT_CHARS)
            .collect::<String>()
    });
    let has_text = contents
        .as_deref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let callout = read_nums(file, dict, "CL")
        .filter(|c| (c.len() == 4 || c.len() == 6) && c.iter().all(|v| v.is_finite()));

    if !has_text && background.is_none() && border.is_none() && callout.is_none() {
        return None;
    }

    // Font / DA resolution (reusing the AcroForm text engine).
    let da = crate::forms::parse_da(&read_text_string(file, dict, "DA").unwrap_or_default());
    let font_res_name = da
        .font
        .as_deref()
        .filter(|n| crate::forms::is_safe_resource_name(n))
        .unwrap_or("Helv")
        .to_string();
    let dr_fonts = read_dr_fonts(file, dict);
    let base_font = crate::forms::resolve_base_font(dr_fonts.as_ref(), &font_res_name);

    let ca = read_num(file, dict, "CA").map(|v| v.clamp(0.0, 1.0));
    let gs = build_gs(false, ca);

    let mut content = Vec::new();
    push(&mut content, "q\n");
    if gs.is_some() {
        push(&mut content, "/GS0 gs\n");
    }

    // Callout line first, so the text box paints over its tail. Drawn in user
    // space; for a conforming file /Rect encloses the callout (the painter
    // clips the form to /Rect either way).
    if let Some(cl) = &callout {
        let (le_start, _) = read_line_endings(file, dict);
        let lw = border.unwrap_or(1.0).max(0.5);
        push(&mut content, "0 G\n");
        push(&mut content, &format!("{} w 1 J 1 j\n", fmt(lw)));
        push(&mut content, &format!("{} {} m\n", fmt(cl[0]), fmt(cl[1])));
        let mut k = 2;
        while k + 1 < cl.len() {
            push(
                &mut content,
                &format!("{} {} l\n", fmt(cl[k]), fmt(cl[k + 1])),
            );
            k += 2;
        }
        push(&mut content, "S\n");
        let (p0, p1) = ((cl[0], cl[1]), (cl[2], cl[3]));
        emit_line_ending(&mut content, p0, sub(p1, p0), le_start, lw, &None);
    }

    // Background fill over the whole rect (user space).
    if let Some(bg) = &background {
        if let Some(opc) = color_op(bg, false) {
            push(&mut content, &opc);
            push(&mut content, "\n");
            push(
                &mut content,
                &format!(
                    "{} {} {} {} re f\n",
                    fmt(rect.x0),
                    fmt(rect.y0),
                    fmt(w),
                    fmt(h)
                ),
            );
        }
    }

    // Border: a black stroke inset by half its width (no dedicated colour field
    // exists for FreeText; black matches viewer convention).
    if let Some(bw) = border {
        let half = bw / 2.0;
        let (bx0, by0) = (rect.x0 + half, rect.y0 + half);
        let (bx1, by1) = (rect.x1 - half, rect.y1 - half);
        if bx1 - bx0 > 0.0 && by1 - by0 > 0.0 {
            push(&mut content, "0 G\n");
            push(&mut content, &format!("{} w\n", fmt(bw)));
            push(
                &mut content,
                &format!(
                    "{} {} {} {} re S\n",
                    fmt(bx0),
                    fmt(by0),
                    fmt(bx1 - bx0),
                    fmt(by1 - by0)
                ),
            );
        }
    }

    // Text, laid out in a local frame translated to the text region's
    // lower-left corner (so the reused layout works in box-local coordinates),
    // clipped to that region.
    if has_text {
        let text = contents.unwrap_or_default();
        let q = read_num(file, dict, "Q").map(|v| v as i64).unwrap_or(0);
        let iw = (w - il - ir).max(1.0);
        let ih = (h - it - ib).max(1.0);
        const PAD: f64 = 2.0;
        push(&mut content, "q\n");
        push(
            &mut content,
            &format!("1 0 0 1 {} {} cm\n", fmt(rect.x0 + il), fmt(rect.y0 + ib)),
        );
        push(
            &mut content,
            &format!("0 0 {} {} re W n\n", fmt(iw), fmt(ih)),
        );
        push(&mut content, "BT\n");
        crate::forms::multiline_layout(
            &mut content,
            &text,
            &da,
            &base_font,
            &font_res_name,
            iw,
            ih,
            PAD,
            q,
        );
        push(&mut content, "ET\n");
        push(&mut content, "Q\n");
    }
    push(&mut content, "Q\n");

    if content.len() > MAX_APPEARANCE_BYTES {
        return None;
    }

    // Resources: a /Font dict (when text is drawn) plus the optional GS0.
    let mut resources = if has_text {
        crate::forms::build_resources(dr_fonts.as_ref(), &font_res_name)
    } else {
        PdfDict::new()
    };
    if let Some(gs) = gs {
        let mut egs = PdfDict::new();
        egs.insert(PdfName::new("GS0"), PdfObject::Dict(gs));
        resources.insert(PdfName::new("ExtGState"), PdfObject::Dict(egs));
    }

    Some(GeneratedAppearance {
        bbox: rect,
        matrix: Matrix::identity(),
        resources,
        content,
    })
}

/// A PDF text string (`/Contents`, `/DA`), resolving one level of indirection.
fn read_text_string(file: &PdfFile, dict: &PdfDict, key: &str) -> Option<String> {
    let obj = match dict.get(key)? {
        PdfObject::Ref(r) => file.resolve(*r).ok()?,
        other => other.clone(),
    };
    match obj {
        PdfObject::String(s) => Some(crate::forms::pdf_string_to_unicode(s.as_bytes())),
        _ => None,
    }
}

/// The annotation's `/DR /Font` dictionary, if any.
fn read_dr_fonts(file: &PdfFile, dict: &PdfDict) -> Option<PdfDict> {
    let dr = read_subdict(file, dict, "DR")?;
    match dr.get("Font")? {
        PdfObject::Dict(d) => Some(d.clone()),
        PdfObject::Ref(r) => file.resolve(*r).ok()?.as_dict().ok().cloned(),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Text annotation icons (§12.5.6.4) — a small vector glyph chosen by /Name
// ---------------------------------------------------------------------------

/// Draw a `Text` (note) annotation's icon. The producer's `/Name` selects the
/// glyph (defaulting to the dog-eared note); `/C` tints it. The icon is a
/// centred square within `/Rect`, so it stays recognizable for whatever rect
/// the file gave — a conforming Text annotation's rect is already a small,
/// roughly square anchor for a fixed-size icon.
fn text_icon(file: &PdfFile, dict: &PdfDict, rect: Rect, out: &mut Vec<u8>) -> bool {
    let Some(b) = icon_box(rect) else {
        return false;
    };
    let c = read_color(file, dict, "C");
    let name = read_name(file, dict, "Name").unwrap_or_else(|| "Note".to_string());
    match name.as_str() {
        "Help" => help_icon(out, b, &c),
        "Insert" => insert_icon(out, b, &c),
        "Key" => key_icon(out, b, &c),
        "Check" | "Checkmark" => check_icon(out, b, &c),
        "Cross" => cross_icon(out, b, &c),
        // Note (the spec default), Comment, Paragraph, NewParagraph and any
        // unknown name fall back to the dog-eared note — a universally legible
        // "there is a comment here" marker.
        _ => note_icon(out, b, &c),
    }
    true
}

/// The centred square drawing area for a text icon: side = `min(width, height)`
/// of `/Rect`, inset by a 10% margin. `None` when the rect is too small to
/// carry a recognizable glyph.
fn icon_box(rect: Rect) -> Option<Rect> {
    let side = rect.width().min(rect.height());
    if side <= 3.0 {
        return None;
    }
    let (cx, cy) = ((rect.x0 + rect.x1) / 2.0, (rect.y0 + rect.y1) / 2.0);
    let r = side / 2.0 * 0.90;
    Some(Rect::new(cx - r, cy - r, cx + r, cy + r))
}

/// A stroke width for an icon of side `s` at fraction `frac`, kept visible but
/// proportionate.
fn icon_lw(s: f64, frac: f64) -> f64 {
    (s * frac).clamp(0.3, 6.0)
}

/// Map a unit-square coordinate (`u` rightward, `v` upward, both in `[0,1]`)
/// into the icon's square box.
fn at(b: Rect, u: f64, v: f64) -> (f64, f64) {
    (b.x0 + u * b.width(), b.y0 + v * b.height())
}

/// Emit a polyline through unit-square points (`m` then `l`s, optionally `h`).
fn poly(out: &mut Vec<u8>, b: Rect, pts: &[(f64, f64)], close: bool) {
    let Some((&first, rest)) = pts.split_first() else {
        return;
    };
    let (x, y) = at(b, first.0, first.1);
    push(out, &format!("{} {} m\n", fmt(x), fmt(y)));
    for &(u, v) in rest {
        let (x, y) = at(b, u, v);
        push(out, &format!("{} {} l\n", fmt(x), fmt(y)));
    }
    if close {
        push(out, "h\n");
    }
}

/// The dog-eared note: filled paper (`/C`, default note yellow) with a dark
/// outline, a folded corner, and three lines of "text".
fn note_icon(out: &mut Vec<u8>, b: Rect, c: &Option<Vec<f64>>) {
    let s = b.width();
    let lw = icon_lw(s, 0.035);
    let paper = c.clone().unwrap_or_else(|| vec![1.0, 0.93, 0.40]);
    let ink = vec![0.25];
    // Page body, the top-right corner cut away for the fold.
    emit_shape_setup(out, &Some(paper), &Some(ink.clone()), lw);
    poly(
        out,
        b,
        &[
            (0.15, 0.12),
            (0.85, 0.12),
            (0.85, 0.64),
            (0.64, 0.85),
            (0.15, 0.85),
        ],
        true,
    );
    push(out, "B\n");
    // The folded-over corner, a lighter triangle.
    emit_shape_setup(out, &Some(vec![0.80]), &Some(ink.clone()), lw);
    poly(out, b, &[(0.64, 0.85), (0.64, 0.64), (0.85, 0.64)], true);
    push(out, "B\n");
    // Three lines of "text" (the last one short).
    if let Some(op) = color_op(&ink, true) {
        push(out, &op);
        push(out, "\n");
    }
    push(out, &format!("{} w 1 J\n", fmt(lw)));
    for (y, x1) in [(0.62, 0.73), (0.50, 0.73), (0.38, 0.55)] {
        emit_seg(out, at(b, 0.27, y), at(b, x1, y));
    }
}

/// A circle enclosing a question mark.
fn help_icon(out: &mut Vec<u8>, b: Rect, c: &Option<Vec<f64>>) {
    let s = b.width();
    let ink = c.clone().unwrap_or_else(|| vec![0.0]);
    let p = |u: f64, v: f64| {
        let (x, y) = at(b, u, v);
        format!("{} {}", fmt(x), fmt(y))
    };
    if let Some(op) = color_op(&ink, true) {
        push(out, &op);
        push(out, "\n");
    }
    push(out, &format!("{} w 1 J 1 j\n", fmt(icon_lw(s, 0.06))));
    let (c0, c1) = (at(b, 0.10, 0.10), at(b, 0.90, 0.90));
    push_ellipse(out, Rect::new(c0.0, c0.1, c1.0, c1.1));
    push(out, "S\n");
    // The hook + stem of the "?", stroked.
    push(out, &format!("{} w\n", fmt(icon_lw(s, 0.075))));
    push(out, &format!("{} m\n", p(0.35, 0.58)));
    push(
        out,
        &format!("{} {} {} c\n", p(0.34, 0.78), p(0.66, 0.78), p(0.63, 0.56)),
    );
    push(
        out,
        &format!("{} {} {} c\n", p(0.61, 0.47), p(0.50, 0.50), p(0.50, 0.40)),
    );
    push(out, "S\n");
    // The dot beneath it, filled.
    if let Some(op) = color_op(&ink, false) {
        push(out, &op);
        push(out, "\n");
    }
    let (d0, d1) = (at(b, 0.455, 0.22), at(b, 0.545, 0.31));
    push_ellipse(out, Rect::new(d0.0, d0.1, d1.0, d1.1));
    push(out, "f\n");
}

/// An upward insertion caret (filled triangle).
fn insert_icon(out: &mut Vec<u8>, b: Rect, c: &Option<Vec<f64>>) {
    let ink = c.clone().unwrap_or_else(|| vec![0.0]);
    if let Some(op) = color_op(&ink, false) {
        push(out, &op);
        push(out, "\n");
    }
    poly(out, b, &[(0.5, 0.86), (0.80, 0.24), (0.20, 0.24)], true);
    push(out, "f\n");
}

/// A key: a stroked ring head, a diagonal stem and two teeth.
fn key_icon(out: &mut Vec<u8>, b: Rect, c: &Option<Vec<f64>>) {
    let s = b.width();
    let ink = c.clone().unwrap_or_else(|| vec![0.0]);
    if let Some(op) = color_op(&ink, true) {
        push(out, &op);
        push(out, "\n");
    }
    push(out, &format!("{} w 1 J 1 j\n", fmt(icon_lw(s, 0.085))));
    let (r0, r1) = (at(b, 0.14, 0.50), at(b, 0.50, 0.86));
    push_ellipse(out, Rect::new(r0.0, r0.1, r1.0, r1.1));
    push(out, "S\n");
    emit_seg(out, at(b, 0.44, 0.58), at(b, 0.84, 0.20)); // stem
    emit_seg(out, at(b, 0.74, 0.30), at(b, 0.66, 0.22)); // teeth
    emit_seg(out, at(b, 0.84, 0.20), at(b, 0.76, 0.12));
}

/// A check mark (two-segment stroke).
fn check_icon(out: &mut Vec<u8>, b: Rect, c: &Option<Vec<f64>>) {
    let s = b.width();
    let ink = c.clone().unwrap_or_else(|| vec![0.0]);
    if let Some(op) = color_op(&ink, true) {
        push(out, &op);
        push(out, "\n");
    }
    push(out, &format!("{} w 1 J 1 j\n", fmt(icon_lw(s, 0.11))));
    let a = at(b, 0.20, 0.50);
    let m = at(b, 0.42, 0.26);
    let e = at(b, 0.82, 0.74);
    push(
        out,
        &format!(
            "{} {} m {} {} l {} {} l S\n",
            fmt(a.0),
            fmt(a.1),
            fmt(m.0),
            fmt(m.1),
            fmt(e.0),
            fmt(e.1)
        ),
    );
}

/// An "X" (two crossed strokes).
fn cross_icon(out: &mut Vec<u8>, b: Rect, c: &Option<Vec<f64>>) {
    let s = b.width();
    let ink = c.clone().unwrap_or_else(|| vec![0.0]);
    if let Some(op) = color_op(&ink, true) {
        push(out, &op);
        push(out, "\n");
    }
    push(out, &format!("{} w 1 J\n", fmt(icon_lw(s, 0.11))));
    emit_seg(out, at(b, 0.24, 0.24), at(b, 0.76, 0.76));
    emit_seg(out, at(b, 0.24, 0.76), at(b, 0.76, 0.24));
}

// ---------------------------------------------------------------------------
// Stamp annotations (§12.5.6.12) — a labeled rubber-stamp badge from /Name
// ---------------------------------------------------------------------------

/// Build a generated appearance for a `Stamp` annotation with no `/AP`: a
/// rounded-rectangle border around the label that `/Name` decodes to
/// (`NotApproved` → "NOT APPROVED"; the spec default is `Draft`). The colour is
/// `/C`, else a per-name convention (green for affirmative, blue for neutral,
/// red for cautionary). The label is set in Helvetica-Bold, centred and sized
/// to fill the badge. Like a real rubber stamp, the interior is left unfilled
/// so the page shows through.
fn stamp(file: &PdfFile, dict: &PdfDict, rect: Rect) -> Option<GeneratedAppearance> {
    let (w, h) = (rect.width(), rect.height());
    if w <= 6.0 || h <= 6.0 {
        return None;
    }
    let name = read_name(file, dict, "Name").unwrap_or_else(|| "Draft".to_string());
    let label = stamp_label(&name);
    if label.is_empty() {
        return None;
    }
    let colour = read_color(file, dict, "C").unwrap_or_else(|| stamp_colour(&name));

    let inset = (w.min(h) * 0.08).clamp(2.0, 10.0);
    let badge = Rect::new(
        rect.x0 + inset,
        rect.y0 + inset,
        rect.x1 - inset,
        rect.y1 - inset,
    );
    let (bw, bh) = (badge.x1 - badge.x0, badge.y1 - badge.y0);
    if bw <= 1.0 || bh <= 1.0 {
        return None;
    }
    let border = (w.min(h) * 0.035).clamp(1.0, 4.0);
    let radius = (w.min(h) * 0.12).clamp(2.0, 12.0);

    // Size the label to fill the badge interior (after the border + padding).
    // The padding is per-axis: a wide, short stamp (e.g. a banner) must not let
    // the horizontal inset crush the available height (and vice-versa).
    let inner_w = (bw - 2.0 * (border + bw * 0.04)).max(1.0);
    let inner_h = (bh - 2.0 * (border + bh * 0.06)).max(1.0);
    let unit_w = helv_bold_width(&label, 1.0);
    let size = if unit_w > 0.0 {
        (inner_w / unit_w).min(inner_h * 0.72)
    } else {
        inner_h * 0.72
    }
    .clamp(3.0, 400.0);
    let label_w = helv_bold_width(&label, size);

    let ca = read_num(file, dict, "CA").map(|v| v.clamp(0.0, 1.0));
    let gs = build_gs(false, ca);

    let mut content = Vec::new();
    push(&mut content, "q\n");
    if gs.is_some() {
        push(&mut content, "/GS0 gs\n");
    }
    // Badge border.
    if let Some(op) = color_op(&colour, true) {
        push(&mut content, &op);
        push(&mut content, "\n");
    }
    push(&mut content, &format!("{} w 1 j\n", fmt(border)));
    push_round_rect(&mut content, badge, radius);
    push(&mut content, "S\n");

    // Centred label, drawn in a frame translated to the badge's lower-left and
    // vertically centred by cap height (all-caps text has no descender).
    if let Some(op) = color_op(&colour, false) {
        push(&mut content, &op);
        push(&mut content, "\n");
    }
    let tx = ((bw - label_w) / 2.0).max(0.0);
    let ty = (bh / 2.0 - 0.35 * size).max(0.0);
    push(&mut content, "q\n");
    push(
        &mut content,
        &format!("1 0 0 1 {} {} cm\n", fmt(badge.x0), fmt(badge.y0)),
    );
    push(&mut content, "BT\n");
    push(&mut content, &format!("/F0 {} Tf\n", fmt(size)));
    push(
        &mut content,
        &format!("1 0 0 1 {} {} Tm\n", fmt(tx), fmt(ty)),
    );
    // `label` is `[A-Z0-9 ]` only (see `stamp_label`), so it needs no PDF
    // literal-string escaping and measures with WinAnsi == ASCII metrics.
    push(&mut content, "(");
    push(&mut content, &label);
    push(&mut content, ") Tj\n");
    push(&mut content, "ET\n");
    push(&mut content, "Q\n");
    push(&mut content, "Q\n");

    if content.len() > MAX_APPEARANCE_BYTES {
        return None;
    }

    Some(GeneratedAppearance {
        bbox: rect,
        matrix: Matrix::identity(),
        resources: stamp_resources(gs),
        content,
    })
}

/// Decode a stamp `/Name` into a spaced, uppercase label, splitting camelCase
/// and digit boundaries and keeping only ASCII letters/digits — so the result
/// is always a safe PDF literal string and measures with ASCII metrics.
/// `NotApproved` → "NOT APPROVED", `TopSecret` → "TOP SECRET", `Draft` → "DRAFT".
fn stamp_label(name: &str) -> String {
    let mut out = String::new();
    let mut prev_lower_or_digit = false;
    for ch in name.chars().take(64) {
        if matches!(ch, ' ' | '_' | '-') {
            if !out.is_empty() && !out.ends_with(' ') {
                out.push(' ');
            }
            prev_lower_or_digit = false;
            continue;
        }
        if !ch.is_ascii_alphanumeric() {
            continue;
        }
        if ch.is_ascii_uppercase() && prev_lower_or_digit && !out.ends_with(' ') {
            out.push(' ');
        }
        out.push(ch.to_ascii_uppercase());
        prev_lower_or_digit = ch.is_ascii_lowercase() || ch.is_ascii_digit();
    }
    out.trim().to_string()
}

/// The conventional colour for a standard stamp `/Name`.
fn stamp_colour(name: &str) -> Vec<f64> {
    match name {
        "Approved" | "Accepted" | "Completed" | "Final" | "Reviewed" => {
            vec![0.13, 0.55, 0.20] // affirmative — green
        }
        "Experimental" | "Sold" | "ForPublicRelease" | "InformationOnly" | "PreliminaryResults"
        | "Witness" | "InitialHere" | "SignHere" | "Received" => {
            vec![0.12, 0.22, 0.55] // neutral — blue
        }
        // NotApproved, Void, Rejected, Confidential, TopSecret, Expired, AsIs,
        // NotForPublicRelease, Departmental, ForComment, Draft and any unknown
        // name → cautionary red.
        _ => vec![0.72, 0.13, 0.13],
    }
}

/// Width of an all-caps label in Helvetica-Bold text-space units at `size`
/// (standard-14 metrics; a 0.5-em estimate for any non-WinAnsi character).
fn helv_bold_width(text: &str, size: f64) -> f64 {
    let metrics = zpdf_font::standard_fonts::lookup("Helvetica-Bold");
    let mut total = 0.0;
    for ch in text.chars() {
        let w1000 = match metrics {
            Some(m) => {
                let code = u8::try_from(ch as u32).unwrap_or(b'?') as usize;
                let w = m.widths[code] as f64;
                if w == 0.0 {
                    500.0
                } else {
                    w
                }
            }
            None => 500.0,
        };
        total += w1000 / 1000.0 * size;
    }
    total
}

/// The stamp appearance `/Resources`: a `/F0` Helvetica-Bold font plus the
/// optional opacity `GS0`.
fn stamp_resources(gs: Option<PdfDict>) -> PdfDict {
    let mut fonts = PdfDict::new();
    fonts.insert(
        PdfName::new("F0"),
        PdfObject::Dict(crate::forms::standard_font_dict("Helvetica-Bold")),
    );
    let mut res = PdfDict::new();
    res.insert(PdfName::new("Font"), PdfObject::Dict(fonts));
    if let Some(gs) = gs {
        let mut egs = PdfDict::new();
        egs.insert(PdfName::new("GS0"), PdfObject::Dict(gs));
        res.insert(PdfName::new("ExtGState"), PdfObject::Dict(egs));
    }
    res
}

/// Append a rounded rectangle (four straight edges + four Bézier corner arcs),
/// no paint op. The radius is clamped to half the smaller side.
fn push_round_rect(out: &mut Vec<u8>, r: Rect, rad: f64) {
    const K: f64 = 0.552_284_75; // 4/3 * (sqrt(2) - 1)
    let rad = rad.min(r.width() / 2.0).min(r.height() / 2.0).max(0.0);
    let k = rad * K;
    let (x0, y0, x1, y1) = (r.x0, r.y0, r.x1, r.y1);
    push(out, &format!("{} {} m\n", fmt(x0 + rad), fmt(y0)));
    push(out, &format!("{} {} l\n", fmt(x1 - rad), fmt(y0)));
    push(
        out,
        &format!(
            "{} {} {} {} {} {} c\n",
            fmt(x1 - rad + k),
            fmt(y0),
            fmt(x1),
            fmt(y0 + rad - k),
            fmt(x1),
            fmt(y0 + rad)
        ),
    );
    push(out, &format!("{} {} l\n", fmt(x1), fmt(y1 - rad)));
    push(
        out,
        &format!(
            "{} {} {} {} {} {} c\n",
            fmt(x1),
            fmt(y1 - rad + k),
            fmt(x1 - rad + k),
            fmt(y1),
            fmt(x1 - rad),
            fmt(y1)
        ),
    );
    push(out, &format!("{} {} l\n", fmt(x0 + rad), fmt(y1)));
    push(
        out,
        &format!(
            "{} {} {} {} {} {} c\n",
            fmt(x0 + rad - k),
            fmt(y1),
            fmt(x0),
            fmt(y1 - rad + k),
            fmt(x0),
            fmt(y1 - rad)
        ),
    );
    push(out, &format!("{} {} l\n", fmt(x0), fmt(y0 + rad)));
    push(
        out,
        &format!(
            "{} {} {} {} {} {} c\n",
            fmt(x0),
            fmt(y0 + rad - k),
            fmt(x0 + rad - k),
            fmt(y0),
            fmt(x0 + rad),
            fmt(y0)
        ),
    );
    push(out, "h\n");
}

/// Resolve `/Name` (a possibly-indirect name) to a string.
fn read_name(file: &PdfFile, dict: &PdfDict, key: &str) -> Option<String> {
    dict.get(key).and_then(|o| name_of(file, o))
}

// ---------------------------------------------------------------------------
// Line endings (Table 176) — shared by Line, PolyLine and FreeText callouts
// ---------------------------------------------------------------------------

/// A line-ending style. Unrecognized / `None` names map to [`LineEnding::None`].
#[derive(Clone, Copy, PartialEq)]
enum LineEnding {
    None,
    OpenArrow,
    ClosedArrow,
    ROpenArrow,
    RClosedArrow,
    Butt,
    Slash,
    Square,
    Circle,
    Diamond,
}

fn parse_line_ending(name: &str) -> LineEnding {
    match name {
        "OpenArrow" => LineEnding::OpenArrow,
        "ClosedArrow" => LineEnding::ClosedArrow,
        "ROpenArrow" => LineEnding::ROpenArrow,
        "RClosedArrow" => LineEnding::RClosedArrow,
        "Butt" => LineEnding::Butt,
        "Slash" => LineEnding::Slash,
        "Square" => LineEnding::Square,
        "Circle" => LineEnding::Circle,
        "Diamond" => LineEnding::Diamond,
        _ => LineEnding::None, // "None" and any unknown name
    }
}

/// Read `/LE` as a `(start, end)` style pair. `/LE` is normally a two-name
/// array `[start end]` (Line/PolyLine) but may be a bare single name (the
/// FreeText callout ending); the second slot then defaults to `None`.
fn read_line_endings(file: &PdfFile, dict: &PdfDict) -> (LineEnding, LineEnding) {
    if let Some(arr) = read_array(file, dict, "LE") {
        let style = |o: Option<&PdfObject>| {
            o.and_then(|o| name_of(file, o))
                .map_or(LineEnding::None, |s| parse_line_ending(&s))
        };
        return (style(arr.first()), style(arr.get(1)));
    }
    if let Some(name) = dict.get("LE").and_then(|o| name_of(file, o)) {
        return (parse_line_ending(&name), LineEnding::None);
    }
    (LineEnding::None, LineEnding::None)
}

/// Resolve an object (possibly indirect) to a name string.
fn name_of(file: &PdfFile, o: &PdfObject) -> Option<String> {
    match o {
        PdfObject::Name(n) => Some(n.0.clone()),
        PdfObject::Ref(r) => match file.resolve(*r).ok()? {
            PdfObject::Name(n) => Some(n.0),
            _ => None,
        },
        _ => None,
    }
}

/// Append a line-ending decoration at point `p`. `dir` points from `p` toward
/// the line's interior; the decoration is sized from the line width `bw`. Closed
/// heads (`ClosedArrow`, `Square`, `Circle`, `Diamond`) are filled with `fill`
/// (the `/IC` interior colour) when present, else stroked hollow.
fn emit_line_ending(
    out: &mut Vec<u8>,
    p: (f64, f64),
    dir: (f64, f64),
    style: LineEnding,
    bw: f64,
    fill: &Option<Vec<f64>>,
) {
    if style == LineEnding::None {
        return;
    }
    let len = norm(dir);
    if len <= 1e-6 {
        return;
    }
    let (ux, uy) = (dir.0 / len, dir.1 / len); // unit, toward the line interior
    let (wx, wy) = (-uy, ux); // unit, perpendicular
    let lw = bw.max(0.5);
    // Sizes scale with the line width but stay within a visible range.
    let al = (lw * 3.0).clamp(6.0, 30.0); // arrowhead length
    let aw = al * 0.5; // arrowhead half-width
    let r = (lw * 1.5).clamp(3.0, 15.0); // block / circle radius
    let pt = |x: f64, y: f64| format!("{} {}", fmt(x), fmt(y));

    match style {
        LineEnding::OpenArrow | LineEnding::ROpenArrow => {
            // Reversed arrow opens away from the line (negative `s`).
            let s = if style == LineEnding::ROpenArrow {
                -1.0
            } else {
                1.0
            };
            let (bx, by) = (p.0 + s * al * ux, p.1 + s * al * uy);
            push(
                out,
                &format!(
                    "{} m {} l {} l S\n",
                    pt(bx + aw * wx, by + aw * wy),
                    pt(p.0, p.1),
                    pt(bx - aw * wx, by - aw * wy)
                ),
            );
        }
        LineEnding::ClosedArrow | LineEnding::RClosedArrow => {
            let s = if style == LineEnding::RClosedArrow {
                -1.0
            } else {
                1.0
            };
            let (bx, by) = (p.0 + s * al * ux, p.1 + s * al * uy);
            let path = format!(
                "{} m {} l {} l h\n",
                pt(p.0, p.1),
                pt(bx + aw * wx, by + aw * wy),
                pt(bx - aw * wx, by - aw * wy)
            );
            paint_closed(out, path.as_bytes(), fill);
        }
        LineEnding::Butt => emit_seg(
            out,
            (p.0 + r * wx, p.1 + r * wy),
            (p.0 - r * wx, p.1 - r * wy),
        ),
        LineEnding::Slash => {
            // A short segment at 60° to the line through `p`.
            const COS60: f64 = 0.5;
            const SIN60: f64 = 0.866_025_403_784_438_6;
            let (dx, dy) = (ux * COS60 - uy * SIN60, ux * SIN60 + uy * COS60);
            emit_seg(
                out,
                (p.0 + r * dx, p.1 + r * dy),
                (p.0 - r * dx, p.1 - r * dy),
            );
        }
        LineEnding::Square => {
            let path = format!(
                "{} m {} l {} l {} l h\n",
                pt(p.0 + r * ux + r * wx, p.1 + r * uy + r * wy),
                pt(p.0 + r * ux - r * wx, p.1 + r * uy - r * wy),
                pt(p.0 - r * ux - r * wx, p.1 - r * uy - r * wy),
                pt(p.0 - r * ux + r * wx, p.1 - r * uy + r * wy)
            );
            paint_closed(out, path.as_bytes(), fill);
        }
        LineEnding::Diamond => {
            let path = format!(
                "{} m {} l {} l {} l h\n",
                pt(p.0 + r * ux, p.1 + r * uy),
                pt(p.0 + r * wx, p.1 + r * wy),
                pt(p.0 - r * ux, p.1 - r * uy),
                pt(p.0 - r * wx, p.1 - r * wy)
            );
            paint_closed(out, path.as_bytes(), fill);
        }
        LineEnding::Circle => {
            // `push_ellipse` writes only ASCII (fmt() numbers + ` m`/` c`/`h`),
            // so the bytes go straight to `paint_closed` — no UTF-8 round-trip.
            let mut path = Vec::new();
            push_ellipse(&mut path, Rect::new(p.0 - r, p.1 - r, p.0 + r, p.1 + r));
            paint_closed(out, &path, fill);
        }
        LineEnding::None => {}
    }
}

/// Paint a constructed closed path: fill with `fill` (then stroke the outline)
/// when an interior colour is given, else stroke only. The fill colour is set
/// *before* the path so colour operators never sit between path construction
/// and painting.
fn paint_closed(out: &mut Vec<u8>, path: &[u8], fill: &Option<Vec<f64>>) {
    if let Some(c) = fill {
        if let Some(op) = color_op(c, false) {
            push(out, &op);
            push(out, "\n");
            out.extend_from_slice(path);
            push(out, "B\n"); // fill + stroke
            return;
        }
    }
    out.extend_from_slice(path);
    push(out, "S\n"); // hollow: stroke the outline only
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

/// Append a triangle-wave squiggle along the quad's baseline, oscillating
/// toward the top edge by `amp`, then stroke it. Works in the quad's own frame
/// so it follows rotated/skewed text. Emits at most `max_seg` segments and
/// returns the count emitted (the caller decrements a shared budget).
fn squiggle(out: &mut Vec<u8>, oq: &OrientedQuad, amp: f64, max_seg: usize) -> usize {
    let h = norm(oq.up);
    if h <= 0.0 {
        return 0;
    }
    // Unit vectors: `u` toward the top edge, `t` along the baseline (b0 → b1).
    let (ux, uy) = (oq.up.0 / h, oq.up.1 / h);
    let (tx, ty) = (oq.b1.0 - oq.b0.0, oq.b1.1 - oq.b0.1);
    let w = (tx * tx + ty * ty).sqrt();
    let period = (amp * 2.0).max(2.0);
    let cap = max_seg.max(1) as i64;
    let n = ((w / period).ceil() as i64).clamp(1, cap);
    // The wave rides a baseline lifted `amp` above the bottom edge, peaking a
    // further `amp` toward the top (matching the old axis-aligned amplitude).
    let base = (oq.b0.0 + ux * amp, oq.b0.1 + uy * amp);
    push(out, &format!("{} {} m\n", fmt(base.0), fmt(base.1)));
    for i in 1..=n {
        let f = i as f64 / n as f64;
        let peak = if i % 2 == 1 { amp } else { 0.0 };
        let x = base.0 + tx * f + ux * peak;
        let y = base.1 + ty * f + uy * peak;
        push(out, &format!("{} {} l\n", fmt(x), fmt(y)));
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

/// Euclidean length of a 2-vector.
fn norm(v: (f64, f64)) -> f64 {
    (v.0 * v.0 + v.1 * v.1).sqrt()
}

/// `a - b` as a 2-vector (used as a direction).
fn sub(a: (f64, f64), b: (f64, f64)) -> (f64, f64) {
    (a.0 - b.0, a.1 - b.1)
}

/// A `/QuadPoints` quadrilateral resolved into a baseline-oriented frame,
/// robust to point order and to page rotation/skew. `corners` are in convex
/// counter-clockwise order (for filling a `Highlight`); `b0`/`b1` are the
/// endpoints of the baseline (bottom) edge and `up` is the vector from the
/// bottom edge to the top edge — its length is the run height — used to place
/// underline / strike-out / squiggle marks parallel to the baseline.
#[derive(Debug)]
struct OrientedQuad {
    corners: [(f64, f64); 4],
    b0: (f64, f64),
    b1: (f64, f64),
    up: (f64, f64),
}

/// Resolve one `/QuadPoints` quad (8 numbers) into an [`OrientedQuad`], or
/// `None` when the four points are non-finite or degenerate (collinear / zero
/// area).
fn oriented_quad(q: &[f64; 8]) -> Option<OrientedQuad> {
    let pts = [(q[0], q[1]), (q[2], q[3]), (q[4], q[5]), (q[6], q[7])];
    if pts.iter().any(|p| !(p.0.is_finite() && p.1.is_finite())) {
        return None;
    }
    // Convex order: sort the four corners by angle about their centroid. This
    // turns either common /QuadPoints ordering (Acrobat's TL,TR,BL,BR or the
    // spec's counter-clockwise order) into a simple, non-self-intersecting
    // polygon — and the producer's exact ordering no longer matters.
    let cx = pts.iter().map(|p| p.0).sum::<f64>() / 4.0;
    let cy = pts.iter().map(|p| p.1).sum::<f64>() / 4.0;
    let mut c = pts;
    c.sort_by(|a, b| {
        (a.1 - cy)
            .atan2(a.0 - cx)
            .partial_cmp(&(b.1 - cy).atan2(b.0 - cx))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    // Reject a degenerate quad (shoelace area ≈ 0).
    let area = 0.5
        * (c[0].0 * c[1].1 - c[1].0 * c[0].1 + c[1].0 * c[2].1 - c[2].0 * c[1].1 + c[2].0 * c[3].1
            - c[3].0 * c[2].1
            + c[3].0 * c[0].1
            - c[0].0 * c[3].1)
            .abs();
    if area < 1e-6 {
        return None;
    }
    // The two baseline-parallel edges are the longer opposing pair: either
    // (c0-c1, c2-c3) or (c1-c2, c3-c0).
    let edge = |a: (f64, f64), b: (f64, f64)| norm((a.0 - b.0, a.1 - b.1));
    let mid = |a: (f64, f64), b: (f64, f64)| ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
    let (long0, long1) =
        if edge(c[0], c[1]) + edge(c[2], c[3]) >= edge(c[1], c[2]) + edge(c[3], c[0]) {
            ((c[0], c[1]), (c[2], c[3]))
        } else {
            ((c[1], c[2]), (c[3], c[0]))
        };
    let (m0, m1) = (mid(long0.0, long0.1), mid(long1.0, long1.1));
    // Bottom edge = the long edge with the lower midpoint (page y grows upward,
    // so "lower on the page" = smaller y). Correct for text rotated within
    // (-90°, 90°); a fully inverted run is vanishingly rare and would only shift
    // an underline to the opposite long edge (the fill stays correct).
    let (bottom, top_mid) = if m0.1 <= m1.1 {
        (long0, m1)
    } else {
        (long1, m0)
    };
    let bottom_mid = mid(bottom.0, bottom.1);
    Some(OrientedQuad {
        corners: c,
        b0: bottom.0,
        b1: bottom.1,
        up: (top_mid.0 - bottom_mid.0, top_mid.1 - bottom_mid.1),
    })
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
    fn oriented_quad_finds_baseline_regardless_of_point_order() {
        // An axis-aligned run, 100 wide × 20 tall, bottom edge at y=12.
        // Acrobat order (TL, TR, BL, BR) and the spec's CCW order must yield the
        // same baseline edge and the same upward vector.
        let approx = |a: f64, b: f64| (a - b).abs() < 1e-6;
        for q in [
            [10.0, 32.0, 110.0, 32.0, 10.0, 12.0, 110.0, 12.0], // Acrobat order
            [10.0, 12.0, 110.0, 12.0, 110.0, 32.0, 10.0, 32.0], // CCW order
        ] {
            let oq = oriented_quad(&q).expect("non-degenerate");
            // Bottom edge endpoints have y = 12 (the lower long edge).
            assert!(approx(oq.b0.1, 12.0) && approx(oq.b1.1, 12.0), "{oq:?}");
            // `up` points straight up by the run height (20).
            assert!(approx(oq.up.0, 0.0) && approx(oq.up.1, 20.0), "{oq:?}");
        }
    }

    #[test]
    fn oriented_quad_rejects_degenerate() {
        // Four collinear points (zero area).
        assert!(oriented_quad(&[0.0, 0.0, 1.0, 0.0, 2.0, 0.0, 3.0, 0.0]).is_none());
        // A non-finite coordinate.
        assert!(oriented_quad(&[0.0, 0.0, f64::NAN, 0.0, 1.0, 1.0, 0.0, 1.0]).is_none());
    }

    #[test]
    fn oriented_quad_tracks_rotation() {
        // The axis-aligned run rotated 30° CCW about the origin. The baseline
        // frame must rotate with it: the height stays 20 and the up vector is
        // the original (0, 20) rotated 30° → (-10, 17.3205…).
        let base = [10.0, 32.0, 110.0, 32.0, 10.0, 12.0, 110.0, 12.0];
        let (sin, cos) = 30.0_f64.to_radians().sin_cos();
        let mut rot = [0.0; 8];
        for i in 0..4 {
            let (x, y) = (base[2 * i], base[2 * i + 1]);
            rot[2 * i] = x * cos - y * sin;
            rot[2 * i + 1] = x * sin + y * cos;
        }
        let oq = oriented_quad(&rot).expect("non-degenerate");
        assert!(
            (norm(oq.up) - 20.0).abs() < 1e-6,
            "height preserved: {oq:?}"
        );
        assert!((oq.up.0 - (-10.0)).abs() < 1e-6, "up.x: {oq:?}");
        assert!(
            (oq.up.1 - 17.320_508_075_688_775).abs() < 1e-6,
            "up.y: {oq:?}"
        );
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
        assert!(s.contains("h\nf"), "closes and fills the quad polygon: {s}");
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
        // `/Movie` has no geometry-derived appearance (unlike Text/Stamp, which
        // now draw an icon / badge).
        let a =
            annot_of("<< /Type /Annot /Subtype /Movie /Rect [10 10 30 30] >>").expect("annotation");
        assert!(a.generated.is_none());
        assert!(!a.is_viewable(), "no appearance, not viewable");
    }

    #[test]
    fn text_note_icon_draws_filled_paper() {
        // A Text annotation with no /Name defaults to the dog-eared note, filled
        // with /C and outlined / lined in dark gray.
        let a = annot_of("<< /Type /Annot /Subtype /Text /Rect [10 10 30 30] /C [1 1 0] >>")
            .expect("annotation");
        assert!(a.is_viewable(), "generated note icon is viewable");
        let gen = a.generated.as_ref().expect("generated appearance");
        let s = String::from_utf8_lossy(&gen.content);
        assert!(
            s.contains("1.000 1.000 0.000 rg"),
            "yellow paper from /C: {s}"
        );
        assert!(s.contains("0.250 G"), "dark gray ink: {s}");
        assert!(s.contains("B\n"), "fills + strokes the page: {s}");
    }

    #[test]
    fn text_default_colour_is_note_yellow() {
        // No /C → the default note yellow, not transparent.
        let a =
            annot_of("<< /Type /Annot /Subtype /Text /Rect [10 10 30 30] >>").expect("annotation");
        let gen = a.generated.as_ref().expect("gen");
        let s = String::from_utf8_lossy(&gen.content);
        assert!(
            s.contains("1.000 0.930 0.400 rg"),
            "default note yellow: {s}"
        );
    }

    #[test]
    fn text_help_icon_draws_circle() {
        let a = annot_of("<< /Type /Annot /Subtype /Text /Name /Help /Rect [10 10 34 34] >>")
            .expect("annotation");
        let gen = a.generated.as_ref().expect("gen");
        let s = String::from_utf8_lossy(&gen.content);
        // The enclosing circle is an ellipse (Bézier arcs); the dot is filled.
        assert!(s.contains(" c\n"), "curved circle/hook: {s}");
        assert!(s.contains("f\n"), "filled dot: {s}");
    }

    #[test]
    fn text_check_icon_strokes_two_segments() {
        let a = annot_of(
            "<< /Type /Annot /Subtype /Text /Name /Check /Rect [10 10 34 34] /C [0 0.5 0] >>",
        )
        .expect("annotation");
        let gen = a.generated.as_ref().expect("gen");
        let s = String::from_utf8_lossy(&gen.content);
        assert!(s.contains("0.000 0.500 0.000 RG"), "uses /C ink: {s}");
        assert!(
            s.contains(" m ") && s.contains(" l ") && s.contains(" l S"),
            "check stroke: {s}"
        );
    }

    #[test]
    fn text_icon_too_small_generates_nothing() {
        let a =
            annot_of("<< /Type /Annot /Subtype /Text /Rect [10 10 12 12] >>").expect("annotation");
        assert!(a.generated.is_none(), "a sub-3pt rect has no icon");
    }

    #[test]
    fn stamp_label_decodes_camel_case() {
        assert_eq!(stamp_label("NotApproved"), "NOT APPROVED");
        assert_eq!(stamp_label("TopSecret"), "TOP SECRET");
        assert_eq!(stamp_label("ForPublicRelease"), "FOR PUBLIC RELEASE");
        assert_eq!(stamp_label("AsIs"), "AS IS");
        assert_eq!(stamp_label("Draft"), "DRAFT");
        // Already-spaced / punctuated input collapses to single spaces and drops
        // unsafe characters (so the result is always a safe PDF literal).
        assert_eq!(stamp_label("For (comment)"), "FOR COMMENT");
        assert_eq!(stamp_label(""), "");
    }

    #[test]
    fn stamp_draws_bordered_label() {
        let a = annot_of("<< /Type /Annot /Subtype /Stamp /Name /Approved /Rect [20 20 160 70] >>")
            .expect("annotation");
        assert!(a.is_viewable(), "generated stamp is viewable");
        let gen = a.generated.as_ref().expect("gen");
        let s = String::from_utf8_lossy(&gen.content);
        assert!(s.contains("(APPROVED) Tj"), "draws the label: {s}");
        assert!(s.contains("/F0 "), "uses the bold font resource: {s}");
        // Approved → green border + text.
        assert!(s.contains("0.130 0.550 0.200 RG"), "green border: {s}");
        assert!(s.contains("0.130 0.550 0.200 rg"), "green text: {s}");
        // A rounded-rect border (arcs) stroked, no interior fill.
        assert!(
            s.contains(" c\n") && s.contains("h\nS"),
            "rounded border stroked: {s}"
        );
        assert!(gen.resources.get("Font").is_some(), "font resource present");
    }

    #[test]
    fn stamp_default_name_is_draft() {
        let a = annot_of("<< /Type /Annot /Subtype /Stamp /Rect [20 20 160 70] >>")
            .expect("annotation");
        let gen = a.generated.as_ref().expect("gen");
        let s = String::from_utf8_lossy(&gen.content);
        assert!(s.contains("(DRAFT) Tj"), "default /Name is Draft: {s}");
        assert!(
            s.contains("0.720 0.130 0.130 RG"),
            "Draft is cautionary red: {s}"
        );
    }

    #[test]
    fn stamp_colour_override_from_c() {
        let a = annot_of(
            "<< /Type /Annot /Subtype /Stamp /Name /Approved /Rect [20 20 160 70] /C [1 0 0] >>",
        )
        .expect("annotation");
        let gen = a.generated.as_ref().expect("gen");
        let s = String::from_utf8_lossy(&gen.content);
        assert!(
            s.contains("1.000 0.000 0.000 RG"),
            "/C overrides the green: {s}"
        );
        assert!(
            !s.contains("0.130 0.550 0.200"),
            "no convention colour when /C given: {s}"
        );
    }

    #[test]
    fn stamp_opacity_uses_extgstate() {
        let a = annot_of(
            "<< /Type /Annot /Subtype /Stamp /Name /Confidential /Rect [20 20 200 80] /CA 0.5 >>",
        )
        .expect("annotation");
        let gen = a.generated.as_ref().expect("gen");
        let s = String::from_utf8_lossy(&gen.content);
        assert!(s.contains("/GS0 gs"), "applies the opacity ExtGState: {s}");
        let egs = gen
            .resources
            .get("ExtGState")
            .and_then(|o| o.as_dict().ok())
            .expect("ExtGState");
        let g0 = egs.get("GS0").and_then(|o| o.as_dict().ok()).expect("GS0");
        assert_eq!(g0.get("ca").and_then(|o| o.as_f64().ok()), Some(0.5));
    }

    #[test]
    fn text_opacity_uses_extgstate() {
        // A Text icon routes through the shared q/Q + ExtGState wrapper (a
        // different path from Stamp); confirm /CA reaches it.
        let a = annot_of("<< /Type /Annot /Subtype /Text /Rect [10 10 34 34] /CA 0.4 >>")
            .expect("annotation");
        let gen = a.generated.as_ref().expect("gen");
        let s = String::from_utf8_lossy(&gen.content);
        assert!(s.contains("/GS0 gs"), "applies the opacity ExtGState: {s}");
        let egs = gen
            .resources
            .get("ExtGState")
            .and_then(|o| o.as_dict().ok())
            .expect("ExtGState");
        let g0 = egs.get("GS0").and_then(|o| o.as_dict().ok()).expect("GS0");
        assert_eq!(g0.get("ca").and_then(|o| o.as_f64().ok()), Some(0.4));
        assert_eq!(g0.get("CA").and_then(|o| o.as_f64().ok()), Some(0.4));
    }

    #[test]
    fn text_empty_colour_still_draws_note() {
        // Unlike markup (`/C []` is transparent), a Text icon with an empty /C
        // still draws — falling back to the default note yellow.
        let a = annot_of("<< /Type /Annot /Subtype /Text /Rect [10 10 34 34] /C [] >>")
            .expect("annotation");
        let gen = a
            .generated
            .as_ref()
            .expect("empty /C still draws the note icon");
        let s = String::from_utf8_lossy(&gen.content);
        assert!(
            s.contains("1.000 0.930 0.400 rg"),
            "default note yellow: {s}"
        );
    }

    #[test]
    fn text_insert_key_cross_route_to_their_glyphs() {
        let content = |name: &str| {
            let a = annot_of(&format!(
                "<< /Type /Annot /Subtype /Text /Name /{name} /Rect [10 10 34 34] >>"
            ))
            .expect("annotation");
            String::from_utf8_lossy(&a.generated.as_ref().expect("gen").content).into_owned()
        };
        // Insert: a filled triangle — `f`, and crucially NOT the note's `B`.
        let ins = content("Insert");
        assert!(
            ins.contains("f\n") && !ins.contains("B\n"),
            "insert triangle: {ins}"
        );
        // Key: a stroked ring (Bézier `c`) plus straight segments.
        let key = content("Key");
        assert!(
            key.contains(" c\n") && key.contains(" l S\n"),
            "key ring + stem: {key}"
        );
        // Cross: two stroked segments, no fill at all.
        let cross = content("Cross");
        assert!(
            cross.matches(" l S\n").count() >= 2 && !cross.contains("f\n"),
            "cross is two strokes: {cross}"
        );
    }

    #[test]
    fn text_checkmark_alias_matches_check() {
        // `Checkmark` is an alias for `Check`; it must reach the check stroke,
        // not fall through to the note fill.
        let a = annot_of("<< /Type /Annot /Subtype /Text /Name /Checkmark /Rect [10 10 34 34] >>")
            .expect("annotation");
        let s = String::from_utf8_lossy(&a.generated.as_ref().expect("gen").content);
        assert!(
            s.contains(" l S\n") && !s.contains("B\n"),
            "Checkmark routes to the check icon: {s}"
        );
    }

    #[test]
    fn stamp_too_small_rect_generates_nothing() {
        let a = annot_of("<< /Type /Annot /Subtype /Stamp /Name /Approved /Rect [10 10 14 14] >>")
            .expect("annotation");
        assert!(a.generated.is_none(), "a <=6pt stamp rect has no badge");
    }

    #[test]
    fn stamp_non_decodable_name_generates_nothing() {
        // A /Name with no ASCII alphanumerics strips to an empty label, so no
        // badge is drawn (rather than an empty `() Tj`).
        let a = annot_of("<< /Type /Annot /Subtype /Stamp /Name /--- /Rect [20 20 160 70] >>")
            .expect("annotation");
        assert!(
            a.generated.is_none(),
            "a name that strips to empty draws nothing"
        );
    }

    #[test]
    fn round_rect_clamps_radius_on_a_flat_rect() {
        // A wide, short rect with an oversized radius: the internal clamp must
        // keep every emitted coordinate inside the rectangle (no corner-arc
        // overshoot). Numbers are emitted in x, y, x, y… order.
        let mut out = Vec::new();
        push_round_rect(&mut out, Rect::new(0.0, 0.0, 100.0, 4.0), 50.0);
        let s = String::from_utf8(out).expect("ascii");
        let nums: Vec<f64> = s
            .split_whitespace()
            .filter_map(|t| t.parse::<f64>().ok())
            .collect();
        assert!(!nums.is_empty(), "emitted coordinates: {s}");
        for (i, v) in nums.iter().enumerate() {
            if i % 2 == 0 {
                assert!((0.0..=100.0).contains(v), "x within width: {v}");
            } else {
                assert!((0.0..=4.0).contains(v), "y clamped to height: {v}");
            }
        }
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

    #[test]
    fn rotated_highlight_fills_oriented_polygon() {
        // A 45°-ish skewed quad: the fill must be a closed polygon through the
        // four corners, never an axis-aligned `re`.
        let a = annot_of(
            "<< /Type /Annot /Subtype /Highlight /Rect [10 10 120 120] \
             /QuadPoints [20 100 100 60 10 40 90 0] /C [1 1 0] >>",
        )
        .expect("annotation");
        let gen = a.generated.as_ref().expect("gen");
        let s = String::from_utf8_lossy(&gen.content);
        assert!(!s.contains(" re"), "no axis-aligned rectangle: {s}");
        assert!(
            s.contains(" m\n") && s.contains(" l\n"),
            "polygon path: {s}"
        );
        assert!(s.contains("h\nf"), "closed and filled: {s}");
    }

    #[test]
    fn line_open_arrow_strokes_a_head() {
        let a = annot_of(
            "<< /Type /Annot /Subtype /Line /Rect [0 0 200 200] /L [20 20 180 180] \
             /C [0 0 0] /LE [/OpenArrow /None] >>",
        )
        .expect("annotation");
        let gen = a.generated.as_ref().expect("gen");
        let s = String::from_utf8_lossy(&gen.content);
        assert!(s.contains("20.000 20.000 m"), "draws the line: {s}");
        // The arrowhead's tip is the first point, emitted as a line-to in the
        // open `bl m tip l br l S` path — unique to the ending.
        assert!(
            s.contains("20.000 20.000 l"),
            "open arrowhead at the start: {s}"
        );
    }

    #[test]
    fn line_closed_arrow_fills_with_interior_colour() {
        let a = annot_of(
            "<< /Type /Annot /Subtype /Line /Rect [0 0 200 200] /L [20 20 180 180] \
             /C [0 0 0] /IC [1 0 0] /LE [/None /ClosedArrow] >>",
        )
        .expect("annotation");
        let gen = a.generated.as_ref().expect("gen");
        let s = String::from_utf8_lossy(&gen.content);
        // Closed arrow at the end point, filled with /IC then stroked (`B`).
        assert!(
            s.contains("180.000 180.000 m"),
            "closed arrowhead at the end: {s}"
        );
        assert!(
            s.contains("1.000 0.000 0.000 rg"),
            "interior fill colour: {s}"
        );
        assert!(s.contains("B\n"), "fill + stroke: {s}");
    }

    #[test]
    fn polyline_carries_line_endings() {
        let a = annot_of(
            "<< /Type /Annot /Subtype /PolyLine /Rect [0 0 200 200] \
             /Vertices [20 20 100 20 100 100] /C [0 0 0] /LE [/Diamond /Butt] >>",
        )
        .expect("annotation");
        let gen = a.generated.as_ref().expect("gen");
        let s = String::from_utf8_lossy(&gen.content);
        // The polyline path itself, plus a diamond at the first vertex (closed
        // path → stroked hollow) and a butt cap at the last.
        assert!(s.contains("20.000 20.000 m"), "polyline drawn: {s}");
        assert!(
            s.contains("20.000 23.000 l"),
            "diamond at the first vertex: {s}"
        );
        assert!(
            s.contains("103.000 100.000 m 97.000 100.000 l S"),
            "butt cap at the last vertex: {s}"
        );
    }

    #[test]
    fn freetext_draws_background_and_text() {
        let a = annot_of(
            "<< /Type /Annot /Subtype /FreeText /Rect [40 40 190 140] \
             /Contents (Hello world) /DA (/Helv 12 Tf 0 0 1 rg) /C [1 1 0] >>",
        )
        .expect("annotation");
        assert!(
            a.is_viewable(),
            "FreeText with a generated appearance is viewable"
        );
        let gen = a.generated.as_ref().expect("gen");
        let s = String::from_utf8_lossy(&gen.content);
        assert!(s.contains("1.000 1.000 0.000 rg"), "yellow background: {s}");
        assert!(s.contains("re f"), "fills the rect: {s}");
        assert!(
            s.contains("1 0 0 1 40.000 40.000 cm"),
            "translates to the rect: {s}"
        );
        assert!(s.contains("/Helv 12.00 Tf"), "DA font/size: {s}");
        assert!(s.contains("0.0000 0.0000 1.0000 rg"), "DA text colour: {s}");
        assert!(s.contains("(Hello world) Tj"), "draws the text: {s}");
        assert!(gen.resources.get("Font").is_some(), "font resource present");
    }

    #[test]
    fn freetext_callout_draws_polyline_and_arrow() {
        let a = annot_of(
            "<< /Type /Annot /Subtype /FreeText /Rect [40 40 190 140] /Contents (note) \
             /DA (/Helv 10 Tf 0 g) /CL [50 60 120 120] /LE /OpenArrow >>",
        )
        .expect("annotation");
        let gen = a.generated.as_ref().expect("gen");
        let s = String::from_utf8_lossy(&gen.content);
        assert!(
            s.contains("50.000 60.000 m"),
            "callout starts at /CL[0]: {s}"
        );
        assert!(
            s.contains("120.000 120.000 l"),
            "callout reaches the box: {s}"
        );
        assert!(
            s.contains("50.000 60.000 l"),
            "open arrow tip at the callout start: {s}"
        );
        assert!(s.contains("(note) Tj"), "draws the text: {s}");
    }

    #[test]
    fn freetext_with_nothing_to_draw_generates_nothing() {
        // No /Contents, /C, border or /CL → nothing to synthesize.
        let a = annot_of("<< /Type /Annot /Subtype /FreeText /Rect [40 40 190 140] >>")
            .expect("annotation");
        assert!(a.generated.is_none(), "empty FreeText draws nothing");
        assert!(!a.is_viewable());
    }
}
