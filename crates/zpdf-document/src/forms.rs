//! AcroForm interactive-form support (PDF 32000-1 §12.7).
//!
//! Two responsibilities:
//!
//! 1. **Field model** ([`AcroForm`]) — walks `/Root /AcroForm /Fields`,
//!    resolving the field tree into terminal [`FormField`]s with
//!    fully-qualified names and inherited attributes (`/FT` `/V` `/DA` `/Ff`
//!    `/Q`). Each terminal field records its widget-annotation object ids, so a
//!    consumer can map a page widget back to the field that owns it.
//!
//! 2. **Appearance generation** ([`generate_widget_appearance`]) — for text and
//!    choice fields whose producer left no appearance stream (or set
//!    `/NeedAppearances`), synthesizes a form XObject that draws the field
//!    value, honoring the `/DA` font/size/color, `/Q` justification, and the
//!    multiline / comb flags. The result feeds the interpreter's annotation
//!    painter exactly like a real `/AP /N` stream.
//!
//! Buttons (checkbox/radio) keep their producer-supplied `/AP` states; only the
//! `/AS` selection is hardened (see the annotation module). Signatures are
//! modelled but never generate an appearance.

use std::collections::{HashMap, HashSet};

use zpdf_core::{Matrix, ObjectId, PdfDict, PdfName, PdfObject, Rect};
use zpdf_parser::PdfFile;

/// Hard cap on the field-tree walk depth and total field count — bounds
/// malformed or adversarial `/Kids` graphs (in concert with the visited set).
const MAX_FIELD_DEPTH: usize = 50;
const MAX_FIELDS: usize = 20_000;

// Field flags (`/Ff`, PDF Tables 226/228/230). Bit numbering is 1-based in the
// spec; the shift is `bit - 1`.
/// Common: field is read-only.
pub const FF_READONLY: i64 = 1 << 0;
/// Tx (bit 13): the text field holds multiple lines.
pub const FF_MULTILINE: i64 = 1 << 12;
/// Tx (bit 14): the value is a password — never rendered.
pub const FF_PASSWORD: i64 = 1 << 13;
/// Btn (bit 16): radio button (mutually-exclusive set).
pub const FF_RADIO: i64 = 1 << 15;
/// Btn (bit 17): push button (no persistent value).
pub const FF_PUSHBUTTON: i64 = 1 << 16;
/// Ch (bit 18): combo box (vs. list box).
pub const FF_COMBO: i64 = 1 << 17;
/// Tx (bit 25): comb formatting — `/MaxLen` equally-spaced cells.
pub const FF_COMB: i64 = 1 << 24;

/// The four AcroForm field types (`/FT`), plus an `Unknown` catch-all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Text,
    Button,
    Choice,
    Signature,
    Unknown,
}

impl FieldKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FieldKind::Text => "Tx",
            FieldKind::Button => "Btn",
            FieldKind::Choice => "Ch",
            FieldKind::Signature => "Sig",
            FieldKind::Unknown => "?",
        }
    }
}

/// A resolved field value (`/V`).
#[derive(Debug, Clone, PartialEq)]
pub enum FieldValue {
    /// Text fields, combo boxes, single-select list boxes.
    Text(String),
    /// Button on/off state (`/Yes`, `/Off`, …).
    Name(String),
    /// Multi-select list box: one entry per selected option.
    List(Vec<String>),
}

/// A terminal interactive-form field.
#[derive(Debug, Clone)]
pub struct FormField {
    /// The field dictionary's own object id — where `/V` lives (distinct from
    /// the widget ids when the field has separate widget kids).
    pub field_id: ObjectId,
    /// Fully-qualified name: the `/T` partial names of this field and its
    /// ancestors joined by `.` (PDF 12.7.3.2).
    pub name: String,
    pub kind: FieldKind,
    /// `/Ff` field flags (inherited).
    pub flags: i64,
    /// `/V` value (inherited).
    pub value: Option<FieldValue>,
    /// `/DA` default appearance string (inherited, falling back to the
    /// AcroForm-level `/DA`).
    pub default_appearance: Option<String>,
    /// `/Q` quadding: 0 left, 1 centered, 2 right (inherited).
    pub quadding: i64,
    /// `/MaxLen` (text fields) — also the comb cell count.
    pub max_len: Option<i64>,
    /// `/Opt` `(export, display)` pairs (choice fields). For plain-string
    /// options the two halves are equal.
    pub options: Vec<(String, String)>,
    /// Widget-annotation object ids that present this field on a page. When the
    /// field dict is itself the widget (the common single-widget case), this is
    /// the field's own object id.
    pub widgets: Vec<ObjectId>,
}

impl FormField {
    /// The string a renderer should draw for this field, or `None` when there
    /// is nothing to show (no value, the `Off` button state, or empty text).
    /// Choice values (which store the `/Opt` *export* value) are mapped to their
    /// human-visible display label (PDF 12.7.4.4).
    pub fn display_value(&self) -> Option<String> {
        let s = match self.value.as_ref()? {
            FieldValue::Text(s) => self.choice_label(s),
            FieldValue::Name(n) if n != "Off" => n.clone(),
            FieldValue::Name(_) => return None,
            FieldValue::List(v) => v
                .iter()
                .map(|s| self.choice_label(s))
                .collect::<Vec<_>>()
                .join("\n"),
        };
        (!s.is_empty()).then_some(s)
    }

    /// Map a choice export value to its display label, or return it unchanged
    /// (for text fields, or exports with no matching option).
    fn choice_label(&self, value: &str) -> String {
        if self.kind == FieldKind::Choice {
            if let Some((_, display)) = self.options.iter().find(|(export, _)| export == value) {
                return display.clone();
            }
        }
        value.to_string()
    }

    pub fn is_multiline(&self) -> bool {
        self.kind == FieldKind::Text && self.flags & FF_MULTILINE != 0
    }

    pub fn is_password(&self) -> bool {
        self.kind == FieldKind::Text && self.flags & FF_PASSWORD != 0
    }

    pub fn is_comb(&self) -> bool {
        self.kind == FieldKind::Text
            // Comb (bit 25) is meaningful only when Multiline/Password are clear.
            && self.flags & (FF_COMB | FF_MULTILINE | FF_PASSWORD) == FF_COMB
            && self.max_len.unwrap_or(0) > 0
    }
}

/// The document's interactive form.
pub struct AcroForm {
    /// Terminal fields, in document order.
    pub fields: Vec<FormField>,
    /// `/NeedAppearances`: the producer relies on the viewer to (re)generate
    /// appearance streams.
    pub need_appearances: bool,
    /// `/DR /Font`: default font resources referenced by `/DA` font names.
    pub dr_fonts: Option<PdfDict>,
    /// Widget object id → index into `fields`.
    widget_owner: HashMap<ObjectId, usize>,
}

impl AcroForm {
    /// Parse the document's `/AcroForm`, or `None` when the document has no
    /// interactive form.
    pub fn parse(file: &PdfFile) -> Option<AcroForm> {
        let root_ref = file.trailer.get_ref("Root").ok()?;
        let root = file.resolve(root_ref).ok()?;
        let root = root.as_dict().ok()?;
        let af = deref(file, root.get("AcroForm")?);
        let af = af.as_dict().ok()?;

        let need_appearances = matches!(af.get("NeedAppearances"), Some(PdfObject::Bool(true)));
        let dr_fonts = deref_opt(file, af.get("DR"))
            .and_then(|dr| dr.as_dict().ok().cloned())
            .and_then(|dr| match dr.get("Font") {
                Some(obj) => deref(file, obj).as_dict().ok().cloned(),
                None => None,
            });

        let root_inherited = Inherited {
            ft: None,
            flags: 0,
            value: None,
            da: af.get("DA").and_then(|o| text_string(file, o)),
            quadding: int_value(file, af.get("Q")).unwrap_or(0),
        };

        let mut state = WalkState {
            file,
            fields: Vec::new(),
            widget_owner: HashMap::new(),
            visited: HashSet::new(),
        };
        if let Some(arr) = deref_array(file, af.get("Fields")) {
            for obj in &arr {
                if let PdfObject::Ref(r) = obj {
                    walk_field(&mut state, *r, "", &root_inherited, 0);
                }
            }
        }

        Some(AcroForm {
            fields: state.fields,
            need_appearances,
            dr_fonts,
            widget_owner: state.widget_owner,
        })
    }

    /// The terminal field presented by the given widget-annotation id.
    pub fn field_for_widget(&self, id: ObjectId) -> Option<&FormField> {
        self.widget_owner.get(&id).and_then(|&i| self.fields.get(i))
    }
}

/// Attributes inherited down the field tree (PDF 12.7.3.2).
#[derive(Clone)]
struct Inherited {
    ft: Option<String>,
    flags: i64,
    value: Option<FieldValue>,
    da: Option<String>,
    quadding: i64,
}

struct WalkState<'a> {
    file: &'a PdfFile,
    fields: Vec<FormField>,
    widget_owner: HashMap<ObjectId, usize>,
    visited: HashSet<ObjectId>,
}

fn walk_field(
    state: &mut WalkState,
    id: ObjectId,
    parent_name: &str,
    inherited: &Inherited,
    depth: usize,
) {
    if depth > MAX_FIELD_DEPTH || state.fields.len() >= MAX_FIELDS {
        return;
    }
    if !state.visited.insert(id) {
        return; // cycle
    }
    let file = state.file;
    let obj = match file.resolve(id) {
        Ok(o) => o,
        Err(_) => return,
    };
    let Ok(dict) = obj.as_dict() else { return };

    // Fully-qualified name: append this node's partial name `/T` (if any),
    // resolving one level of indirection like the other inherited attributes.
    let partial = dict.get("T").and_then(|o| text_string(file, o));
    let name = match &partial {
        Some(t) if parent_name.is_empty() => t.clone(),
        Some(t) => format!("{parent_name}.{t}"),
        None => parent_name.to_string(),
    };

    // Merge inheritable attributes (this node's own values win).
    let merged = Inherited {
        ft: dict
            .get_name("FT")
            .ok()
            .map(String::from)
            .or_else(|| inherited.ft.clone()),
        flags: int_value(file, dict.get("Ff")).unwrap_or(inherited.flags),
        value: field_value(file, dict.get("V")).or_else(|| inherited.value.clone()),
        da: dict
            .get("DA")
            .and_then(|o| text_string(file, o))
            .or_else(|| inherited.da.clone()),
        quadding: int_value(file, dict.get("Q")).unwrap_or(inherited.quadding),
    };

    // Classify the kids: those with a `/T` are child *fields* (recurse); those
    // without are this terminal field's widget annotations.
    let kids = deref_array(file, dict.get("Kids")).unwrap_or_default();
    let mut child_fields = Vec::new();
    let mut widget_kids = Vec::new();
    for kid in &kids {
        if let PdfObject::Ref(r) = kid {
            let kid_obj = file.resolve(*r).ok();
            let has_t = kid_obj
                .as_ref()
                .and_then(|o| o.as_dict().ok())
                .map(|d| d.get("T").is_some())
                .unwrap_or(false);
            if has_t {
                child_fields.push(*r);
            } else {
                widget_kids.push(*r);
            }
        }
    }

    // Descend into child fields (interior node behavior).
    let has_child_fields = !child_fields.is_empty();
    for r in child_fields {
        walk_field(state, r, &name, &merged, depth + 1);
    }

    // Emit a terminal field for this node's own widgets:
    //  - its widget-only kids, or
    //  - the node dict itself when it has no kids at all (merged field+widget).
    // A pure interior node (only field kids) owns no widgets and emits nothing;
    // a *mixed* node (both field and widget kids) still maps its own widgets so
    // their value can be rendered.
    let widgets = if !widget_kids.is_empty() {
        widget_kids
    } else if has_child_fields {
        Vec::new()
    } else {
        vec![id] // the field dict is itself the widget
    };
    if widgets.is_empty() {
        return;
    }

    let kind = field_kind(merged.ft.as_deref());
    let options = if kind == FieldKind::Choice {
        parse_options(file, dict)
    } else {
        Vec::new()
    };
    let max_len = int_value(file, dict.get("MaxLen"));

    let index = state.fields.len();
    for &w in &widgets {
        state.widget_owner.entry(w).or_insert(index);
    }
    state.fields.push(FormField {
        field_id: id,
        name,
        kind,
        flags: merged.flags,
        value: merged.value,
        default_appearance: merged.da,
        quadding: merged.quadding,
        max_len,
        options,
        widgets,
    });
}

fn field_kind(ft: Option<&str>) -> FieldKind {
    match ft {
        Some("Tx") => FieldKind::Text,
        Some("Btn") => FieldKind::Button,
        Some("Ch") => FieldKind::Choice,
        Some("Sig") => FieldKind::Signature,
        _ => FieldKind::Unknown,
    }
}

/// `/Opt`: each entry is a display string, or an `[export, display]` pair. The
/// returned `(export, display)` keeps both; plain strings export == display.
fn parse_options(file: &PdfFile, dict: &PdfDict) -> Vec<(String, String)> {
    let as_text = |o: &PdfObject| match o {
        PdfObject::String(s) => Some(pdf_string_to_unicode(s.as_bytes())),
        _ => None,
    };
    deref_array(file, dict.get("Opt"))
        .map(|arr| {
            arr.iter()
                .map(|o| match deref(file, o) {
                    PdfObject::String(s) => {
                        let t = pdf_string_to_unicode(s.as_bytes());
                        (t.clone(), t)
                    }
                    PdfObject::Array(a) => {
                        let export = a.first().and_then(as_text).unwrap_or_default();
                        let display = a.get(1).and_then(as_text).unwrap_or_else(|| export.clone());
                        (export, display)
                    }
                    _ => (String::new(), String::new()),
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Appearance generation
// ---------------------------------------------------------------------------

/// Cap on the number of characters laid out for any synthesized text
/// appearance — no real field value or `FreeText` note is longer, and it bounds
/// the word-wrap / measurement work against adversarial input. Shared by the
/// widget generator here and the `FreeText` generator in
/// [`crate::annot_appearance`].
pub(crate) const MAX_APPEARANCE_TEXT_CHARS: usize = 50_000;

/// A synthesized appearance stream for a widget the producer left without one
/// (or that `/NeedAppearances` asks the viewer to regenerate). Mirrors a form
/// XObject: a `/BBox`, `/Matrix`, `/Resources` and a content byte stream.
#[derive(Debug, Clone)]
pub struct GeneratedAppearance {
    pub bbox: Rect,
    pub matrix: Matrix,
    pub resources: PdfDict,
    pub content: Vec<u8>,
}

/// Build a generated appearance for a widget, or `None` when nothing should be
/// drawn (button/signature fields, empty/absent values, password fields, or a
/// degenerate rectangle). `dr_fonts` is the AcroForm `/DR /Font` dictionary,
/// used to resolve the `/DA` font name to a concrete font object.
pub fn generate_widget_appearance(
    field: &FormField,
    rect: Rect,
    dr_fonts: Option<&PdfDict>,
) -> Option<GeneratedAppearance> {
    if !matches!(field.kind, FieldKind::Text | FieldKind::Choice) || field.is_password() {
        return None;
    }
    // Cap pathological value lengths — no real field shows this much, and it
    // bounds the synthesized content size / measurement work.
    let text: String = field
        .display_value()?
        .chars()
        .take(MAX_APPEARANCE_TEXT_CHARS)
        .collect();
    let rect = rect.normalize();
    let (w, h) = (rect.width(), rect.height());
    if w <= 1.0 || h <= 1.0 {
        return None;
    }

    let da = field
        .default_appearance
        .as_deref()
        .unwrap_or("/Helv 0 Tf 0 g");
    let da = parse_da(da);
    // The font name becomes both a content-stream token and a resource key, so
    // sanitize it to a safe charset (fall back to the standard Helvetica key).
    let font_res_name = da
        .font
        .as_deref()
        .filter(|n| is_safe_resource_name(n))
        .unwrap_or("Helv")
        .to_string();
    let base_font = resolve_base_font(dr_fonts, &font_res_name);

    const PAD: f64 = 2.0;
    let comb = field.is_comb();
    let mut body: Vec<u8> = Vec::new();
    push_str(&mut body, "BT\n");

    // List boxes (a non-combo choice) stack their selected lines like a
    // multiline text field; combo boxes and plain text fields are single-line.
    let stacked =
        field.is_multiline() || (field.kind == FieldKind::Choice && field.flags & FF_COMBO == 0);

    if comb {
        comb_layout(
            &mut body,
            &one_line(&text),
            &da,
            &base_font,
            &font_res_name,
            w,
            h,
            field,
        );
    } else if stacked {
        multiline_layout(
            &mut body,
            &text,
            &da,
            &base_font,
            &font_res_name,
            w,
            h,
            PAD,
            field.quadding,
        );
    } else {
        single_line_layout(
            &mut body,
            &one_line(&text),
            &da,
            &base_font,
            &font_res_name,
            w,
            h,
            PAD,
            field.quadding,
        );
    }
    push_str(&mut body, "ET\n");

    // Wrap in a marked-content `/Tx` block, clipped to the field. Text/multiline
    // use a 2pt inset; comb cells span the full width, so they clip to the BBox.
    let inset = if comb { 0.0 } else { PAD };
    let clip_w = (w - 2.0 * inset).max(0.0);
    let clip_h = (h - 2.0 * inset).max(0.0);
    let mut content: Vec<u8> = Vec::new();
    push_str(&mut content, "/Tx BMC\nq\n");
    push_str(&mut content, &fmt_num(inset));
    push_str(&mut content, " ");
    push_str(&mut content, &fmt_num(inset));
    push_str(&mut content, " ");
    push_str(&mut content, &fmt_num(clip_w));
    push_str(&mut content, " ");
    push_str(&mut content, &fmt_num(clip_h));
    push_str(&mut content, " re W n\n");
    content.extend_from_slice(&body);
    push_str(&mut content, "Q\nEMC\n");

    Some(GeneratedAppearance {
        bbox: Rect::new(0.0, 0.0, w, h),
        matrix: Matrix::identity(),
        resources: build_resources(dr_fonts, &font_res_name),
        content,
    })
}

#[allow(clippy::too_many_arguments)]
fn single_line_layout(
    body: &mut Vec<u8>,
    text: &str,
    da: &DaInfo,
    base_font: &str,
    font_res_name: &str,
    w: f64,
    h: f64,
    pad: f64,
    quadding: i64,
) {
    let usable = (w - 2.0 * pad).max(1.0);
    let mut size = if da.size > 0.0 {
        da.size
    } else {
        // Auto: fit the field height (capped), then shrink to fit the width.
        let mut s = (h * 0.7).clamp(4.0, 12.0);
        let tw = measure(text, base_font, s);
        if tw > usable {
            s *= usable / tw;
        }
        s.max(2.0)
    };
    if size <= 0.0 {
        size = 12.0;
    }

    let tw = measure(text, base_font, size);
    let x = match quadding {
        1 => (w - tw) / 2.0, // centered
        2 => w - pad - tw,   // right
        _ => pad,            // left (default)
    };
    let y = vertical_baseline(h, size);

    emit_font(body, da, font_res_name, size);
    emit_line(body, x, y, text);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn multiline_layout(
    body: &mut Vec<u8>,
    text: &str,
    da: &DaInfo,
    base_font: &str,
    font_res_name: &str,
    w: f64,
    h: f64,
    pad: f64,
    quadding: i64,
) {
    let usable = (w - 2.0 * pad).max(1.0);
    let usable_h = (h - 2.0 * pad).max(1.0);

    // Auto (DA size 0): shrink so the wrapped lines fit the box height, capped
    // at 12pt; otherwise honor the explicit size.
    let size = if da.size > 0.0 {
        da.size
    } else {
        let mut s = 12.0_f64;
        while s > 4.0 {
            let lines = wrap_lines(text, base_font, s, usable);
            if lines.len() as f64 * s * 1.15 <= usable_h {
                break;
            }
            s -= 1.0;
        }
        s
    };
    let leading = size * 1.15;
    let lines = wrap_lines(text, base_font, size, usable);

    emit_font(body, da, font_res_name, size);
    // Top line baseline sits one ascent below the top inset.
    let mut y = h - pad - size * 0.72;
    for line in &lines {
        if y < -size {
            break; // fully below the box
        }
        let lw = measure(line, base_font, size);
        let x = match quadding {
            1 => (w - lw) / 2.0, // centered
            2 => w - pad - lw,   // right
            _ => pad,            // left (default)
        };
        emit_line(body, x, y, line);
        y -= leading;
    }
}

#[allow(clippy::too_many_arguments)]
fn comb_layout(
    body: &mut Vec<u8>,
    text: &str,
    da: &DaInfo,
    base_font: &str,
    font_res_name: &str,
    w: f64,
    h: f64,
    field: &FormField,
) {
    let n = field.max_len.unwrap_or(1).max(1) as f64;
    let cell = w / n;
    let size = if da.size > 0.0 {
        da.size
    } else {
        ((h - 4.0).min(cell)).clamp(2.0, 12.0)
    };
    let y = vertical_baseline(h, size);

    emit_font(body, da, font_res_name, size);
    for (i, ch) in text.chars().take(n as usize).enumerate() {
        let s = ch.to_string();
        let cw = measure(&s, base_font, size);
        let x = cell * i as f64 + (cell - cw) / 2.0;
        emit_line(body, x, y, &s);
    }
}

/// Baseline y that vertically centers a line of the given font size in a box of
/// height `h`. Uses nominal Helvetica ascent/descent ratios.
fn vertical_baseline(h: f64, size: f64) -> f64 {
    // Glyph box spans [baseline - 0.21·size, baseline + 0.72·size]; centering
    // its midpoint at h/2 gives baseline = h/2 - 0.255·size.
    (h / 2.0 - 0.255 * size).max(0.0)
}

/// Emit the font/color setup: the DA color (or black) then `/Font size Tf`.
fn emit_font(body: &mut Vec<u8>, da: &DaInfo, font_res_name: &str, size: f64) {
    push_str(body, &format!("{}\n", da.color_ops));
    push_str(body, &format!("/{font_res_name} {} Tf\n", fmt_num(size)));
}

/// Emit one absolutely-positioned line: `1 0 0 1 x y Tm (text) Tj`.
fn emit_line(body: &mut Vec<u8>, x: f64, y: f64, text: &str) {
    push_str(body, &format!("1 0 0 1 {} {} Tm\n", fmt_num(x), fmt_num(y)));
    body.push(b'(');
    escape_text(text, body);
    push_str(body, ") Tj\n");
}

/// Format a coordinate/size for the content stream, mapping any non-finite
/// value (an overflowed measurement from an adversarial DA size) to `0` so the
/// emitted stream never contains `inf`/`-inf`/`NaN` tokens.
fn fmt_num(v: f64) -> String {
    if v.is_finite() {
        format!("{v:.2}")
    } else {
        "0".to_string()
    }
}

/// A font resource name safe to emit as a content-stream `/Name` token and use
/// as a resource-dict key (no delimiters, whitespace, or `(`/`)`).
pub(crate) fn is_safe_resource_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '.'))
}

/// Greedy word-wrap, also breaking on explicit newlines.
fn wrap_lines(text: &str, base_font: &str, size: f64, usable: f64) -> Vec<String> {
    // Anti-runaway ceiling, checked at the top so a newline-heavy value cannot
    // bypass it through the empty-paragraph fast path.
    const MAX_LINES: usize = 1000;
    let mut out = Vec::new();
    for paragraph in text.split('\n') {
        if out.len() > MAX_LINES {
            break;
        }
        if paragraph.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut line = String::new();
        for word in paragraph.split(' ') {
            let candidate = if line.is_empty() {
                word.to_string()
            } else {
                format!("{line} {word}")
            };
            if measure(&candidate, base_font, size) <= usable || line.is_empty() {
                line = candidate;
            } else {
                out.push(std::mem::take(&mut line));
                line = word.to_string();
            }
        }
        out.push(line);
    }
    out
}

/// Text width in text-space units at `size`, from the standard-14 metrics of
/// `base_font` (or a 0.5-em estimate for non-standard faces).
fn measure(text: &str, base_font: &str, size: f64) -> f64 {
    let metrics = zpdf_font::standard_fonts::lookup(base_font);
    let mut total = 0.0;
    for ch in text.chars() {
        let w1000 = match metrics {
            Some(m) => {
                let code = unicode_to_winansi(ch).unwrap_or(b'?') as usize;
                m.widths[code] as f64
            }
            None => 500.0,
        };
        let w1000 = if w1000 == 0.0 { 500.0 } else { w1000 };
        total += w1000 / 1000.0 * size;
    }
    total
}

/// Parsed `/DA` default-appearance pieces we care about. Shared with the
/// markup-annotation appearance generator ([`crate::annot_appearance`]), which
/// reuses this whole text-layout engine for `FreeText` annotations.
pub(crate) struct DaInfo {
    pub(crate) font: Option<String>,
    pub(crate) size: f64,
    /// A color-setting fragment (`0 g`, `1 0 0 rg`, …) ready to emit verbatim.
    pub(crate) color_ops: String,
}

/// Extract the font resource name, size, and color operators from a `/DA`
/// content fragment (e.g. `0 0 1 rg /Helv 12 Tf`).
pub(crate) fn parse_da(da: &str) -> DaInfo {
    let mut font = None;
    let mut size: f64 = 0.0;
    let mut color = String::new();
    let mut operands: Vec<&str> = Vec::new();

    for tok in da.split_whitespace() {
        match tok {
            "Tf" => {
                if operands.len() >= 2 {
                    if let Some(name) = operands[operands.len() - 2].strip_prefix('/') {
                        font = Some(name.to_string());
                    }
                    size = operands[operands.len() - 1].parse().unwrap_or(0.0);
                }
                operands.clear();
            }
            "g" if !operands.is_empty() => {
                if let Some(c) = da_color(&operands, 1, "g") {
                    color = c;
                }
                operands.clear();
            }
            "rg" if operands.len() >= 3 => {
                if let Some(c) = da_color(&operands, 3, "rg") {
                    color = c;
                }
                operands.clear();
            }
            "k" if operands.len() >= 4 => {
                if let Some(c) = da_color(&operands, 4, "k") {
                    color = c;
                }
                operands.clear();
            }
            other => operands.push(other),
        }
    }

    // Clamp the font size to a sane ceiling so an adversarial DA (`/Helv 1e308
    // Tf`) cannot overflow downstream width math to infinity.
    const MAX_FONT_SIZE: f64 = 1000.0;
    DaInfo {
        font,
        size: if size.is_finite() && size >= 0.0 {
            size.min(MAX_FONT_SIZE)
        } else {
            0.0
        },
        color_ops: if color.is_empty() {
            "0 g".to_string()
        } else {
            color
        },
    }
}

/// Build a validated color-setting operator from the last `n` DA operands,
/// accepting only finite numbers (clamped to `[0,1]`). Returns `None` when any
/// operand is not a number — so adversarial tokens never reach the content
/// stream verbatim.
fn da_color(operands: &[&str], n: usize, op: &str) -> Option<String> {
    let vals: Option<Vec<f64>> = operands[operands.len() - n..]
        .iter()
        .map(|t| {
            t.parse::<f64>()
                .ok()
                .filter(|v| v.is_finite())
                .map(|v| v.clamp(0.0, 1.0))
        })
        .collect();
    let parts: Vec<String> = vals?.iter().map(|v| format!("{v:.4}")).collect();
    Some(format!("{} {op}", parts.join(" ")))
}

/// Resolve a `/DA` font resource name to a base-font name for metrics: prefer
/// the `/DR` font's `/BaseFont`, else map the conventional Acrobat resource
/// name (`Helv`, `Cour`, …), else Helvetica.
pub(crate) fn resolve_base_font(dr_fonts: Option<&PdfDict>, res_name: &str) -> String {
    if let Some(dr) = dr_fonts {
        if let Some(PdfObject::Dict(fd)) = dr.get(res_name) {
            if let Ok(bf) = fd.get_name("BaseFont") {
                return strip_subset_prefix(bf).to_string();
            }
        }
    }
    acrobat_standard_name(res_name).to_string()
}

/// The conventional AcroForm `/DR` resource names for the standard-14 fonts.
fn acrobat_standard_name(res_name: &str) -> &str {
    match res_name {
        "Helv" => "Helvetica",
        "HeBO" | "HeBo" => "Helvetica-Bold",
        "HeOb" => "Helvetica-Oblique",
        "Cour" => "Courier",
        "CoBO" | "CoBo" => "Courier-Bold",
        "TiRo" => "Times-Roman",
        "TiBo" => "Times-Bold",
        "TiIt" => "Times-Italic",
        "Symb" => "Symbol",
        "ZaDb" => "ZapfDingbats",
        other => other,
    }
}

fn strip_subset_prefix(name: &str) -> &str {
    // "ABCDEF+Helvetica" → "Helvetica"
    name.rsplit('+').next().unwrap_or(name)
}

/// Build the appearance `/Resources`: a `/Font` dict mapping the DA font name to
/// the `/DR` font object (if any) or a synthesized standard Helvetica.
pub fn build_resources(dr_fonts: Option<&PdfDict>, font_res_name: &str) -> PdfDict {
    let font_entry = dr_fonts
        .and_then(|dr| dr.get(font_res_name).cloned())
        .unwrap_or_else(|| PdfObject::Dict(standard_font_dict("Helvetica")));

    let mut fonts = PdfDict::new();
    fonts.insert(PdfName::new(font_res_name), font_entry);
    let mut res = PdfDict::new();
    res.insert(PdfName::new("Font"), PdfObject::Dict(fonts));
    res
}

/// A synthesized standard-14 Type1 font dict (`/BaseFont base`, WinAnsi). Shared
/// by the widget generator and the markup/annotation appearance generator so the
/// font-dict shape lives in one place.
pub fn standard_font_dict(base: &str) -> PdfDict {
    let mut d = PdfDict::new();
    d.insert(PdfName::new("Type"), PdfObject::Name(PdfName::new("Font")));
    d.insert(
        PdfName::new("Subtype"),
        PdfObject::Name(PdfName::new("Type1")),
    );
    d.insert(
        PdfName::new("BaseFont"),
        PdfObject::Name(PdfName::new(base)),
    );
    d.insert(
        PdfName::new("Encoding"),
        PdfObject::Name(PdfName::new("WinAnsiEncoding")),
    );
    d
}

/// Escape a string into a PDF literal-string body (`(`/`)`/`\` and CR), encoding
/// each character as its WinAnsiEncoding byte (the declared appearance-font
/// encoding); characters with no WinAnsi byte fall back to `?`.
pub fn escape_text(s: &str, out: &mut Vec<u8>) {
    for ch in s.chars() {
        let b = unicode_to_winansi(ch).unwrap_or(b'?');
        match b {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'(' => out.extend_from_slice(b"\\("),
            b')' => out.extend_from_slice(b"\\)"),
            b'\r' => out.extend_from_slice(b"\\r"),
            _ => out.push(b),
        }
    }
}

/// Map a Unicode scalar to its WinAnsiEncoding byte. ASCII (0x20–0x7E) and
/// Latin-1 (0xA0–0xFF) are identity; the WinAnsi C1 block (0x80–0x9F) holds
/// typographic punctuation / currency whose Unicode code points are ≥ 0x100.
/// Returns `None` for code points with no WinAnsi representation.
pub fn unicode_to_winansi(ch: char) -> Option<u8> {
    let cp = ch as u32;
    match cp {
        0x20..=0x7E | 0xA0..=0xFF => Some(cp as u8),
        0x20AC => Some(0x80),
        0x201A => Some(0x82),
        0x0192 => Some(0x83),
        0x201E => Some(0x84),
        0x2026 => Some(0x85),
        0x2020 => Some(0x86),
        0x2021 => Some(0x87),
        0x02C6 => Some(0x88),
        0x2030 => Some(0x89),
        0x0160 => Some(0x8A),
        0x2039 => Some(0x8B),
        0x0152 => Some(0x8C),
        0x017D => Some(0x8E),
        0x2018 => Some(0x91),
        0x2019 => Some(0x92),
        0x201C => Some(0x93),
        0x201D => Some(0x94),
        0x2022 => Some(0x95),
        0x2013 => Some(0x96),
        0x2014 => Some(0x97),
        0x02DC => Some(0x98),
        0x2122 => Some(0x99),
        0x0161 => Some(0x9A),
        0x203A => Some(0x9B),
        0x0153 => Some(0x9C),
        0x017E => Some(0x9E),
        0x0178 => Some(0x9F),
        _ => None,
    }
}

fn push_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(s.as_bytes());
}

/// Collapse line breaks and tabs to spaces for single-line / comb rendering.
fn one_line(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c == '\n' || c == '\r' || c == '\t' {
                ' '
            } else {
                c
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Small resolution helpers
// ---------------------------------------------------------------------------

/// Resolve one level of indirection, returning `Null` on failure.
fn deref(file: &PdfFile, obj: &PdfObject) -> PdfObject {
    match obj {
        PdfObject::Ref(r) => file.resolve(*r).unwrap_or(PdfObject::Null),
        other => other.clone(),
    }
}

fn deref_opt(file: &PdfFile, obj: Option<&PdfObject>) -> Option<PdfObject> {
    obj.map(|o| deref(file, o))
}

fn deref_array(file: &PdfFile, obj: Option<&PdfObject>) -> Option<Vec<PdfObject>> {
    match deref(file, obj?) {
        PdfObject::Array(a) => Some(a),
        _ => None,
    }
}

/// A string's text, resolving one level of indirection.
fn text_string(file: &PdfFile, obj: &PdfObject) -> Option<String> {
    match deref(file, obj) {
        PdfObject::String(s) => Some(pdf_string_to_unicode(s.as_bytes())),
        _ => None,
    }
}

fn field_value(file: &PdfFile, obj: Option<&PdfObject>) -> Option<FieldValue> {
    match deref(file, obj?) {
        PdfObject::String(s) => Some(FieldValue::Text(pdf_string_to_unicode(s.as_bytes()))),
        PdfObject::Name(n) => Some(FieldValue::Name(n.0)),
        PdfObject::Array(a) => {
            let items: Vec<String> = a
                .iter()
                .filter_map(|o| match o {
                    PdfObject::String(s) => Some(pdf_string_to_unicode(s.as_bytes())),
                    _ => None,
                })
                .collect();
            (!items.is_empty()).then_some(FieldValue::List(items))
        }
        _ => None,
    }
}

fn int_value(file: &PdfFile, obj: Option<&PdfObject>) -> Option<i64> {
    match deref(file, obj?) {
        PdfObject::Integer(n) => Some(n),
        PdfObject::Real(r) => Some(r as i64),
        _ => None,
    }
}

/// Decode a PDF text string: UTF-16BE when it carries the `FE FF` BOM, else the
/// bytes as PDFDocEncoding (approximated by Latin-1 for the common range).
pub(crate) fn pdf_string_to_unicode(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        bytes.iter().map(|&b| b as char).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::build_pdf;
    use crate::PdfDocument;

    #[test]
    fn field_tree_names_inheritance_and_widgets() {
        let doc = PdfDocument::open(build_pdf(&[
            "<< /Type /Catalog /Pages 2 0 R /AcroForm 4 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] >>",
            "<< /Fields [5 0 R] /DA (/Helv 0 Tf 0 g) /DR << /Font << /Helv 8 0 R >> >> >>",
            // Parent field carries /FT and is the inheritance source.
            "<< /T (address) /FT /Tx /Kids [6 0 R 7 0 R] >>",
            "<< /T (street) /V (Main St) >>",
            "<< /T (city) /V (Springfield) /Q 1 >>",
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
        ]))
        .expect("open");

        let form = doc.acro_form().expect("acroform");
        assert!(!form.need_appearances);
        assert!(form.dr_fonts.is_some());
        assert_eq!(form.fields.len(), 2);

        let street = &form.fields[0];
        assert_eq!(street.name, "address.street");
        assert_eq!(street.kind, FieldKind::Text); // inherited /FT
        assert_eq!(street.value, Some(FieldValue::Text("Main St".into())));
        assert_eq!(street.default_appearance.as_deref(), Some("/Helv 0 Tf 0 g")); // inherited /DA
        assert_eq!(street.quadding, 0);
        // The terminal field with no widget kids is itself the widget.
        assert_eq!(street.widgets, vec![ObjectId(6, 0)]);
        assert_eq!(
            form.field_for_widget(ObjectId(6, 0))
                .map(|f| f.name.as_str()),
            Some("address.street")
        );

        let city = &form.fields[1];
        assert_eq!(city.name, "address.city");
        assert_eq!(city.quadding, 1); // own /Q overrides
    }

    #[test]
    fn single_widget_field_and_button_value() {
        let doc = PdfDocument::open(build_pdf(&[
            "<< /Type /Catalog /Pages 2 0 R /AcroForm 4 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Annots [5 0 R] >>",
            "<< /Fields [5 0 R] /NeedAppearances true >>",
            // A checkbox that is its own widget (merged field+annotation).
            "<< /T (agree) /FT /Btn /V /Yes /AS /Yes /Subtype /Widget /Rect [10 10 30 30] >>",
        ]))
        .expect("open");

        let form = doc.acro_form().expect("acroform");
        assert!(form.need_appearances);
        assert_eq!(form.fields.len(), 1);
        let f = &form.fields[0];
        assert_eq!(f.name, "agree");
        assert_eq!(f.kind, FieldKind::Button);
        assert_eq!(f.value, Some(FieldValue::Name("Yes".into())));
        // A button never generates an appearance.
        assert!(generate_widget_appearance(f, Rect::new(10.0, 10.0, 30.0, 30.0), None).is_none());
    }

    #[test]
    fn no_acroform_returns_none() {
        let doc = PdfDocument::open(build_pdf(&[
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] >>",
        ]))
        .expect("open");
        assert!(doc.acro_form().is_none());
    }

    #[test]
    fn da_parsing_extracts_font_size_color() {
        // Color operands are validated and re-emitted with fixed precision.
        let da = parse_da("0 0 1 rg /Helv 12 Tf");
        assert_eq!(da.font.as_deref(), Some("Helv"));
        assert_eq!(da.size, 12.0);
        assert_eq!(da.color_ops, "0.0000 0.0000 1.0000 rg");

        let da = parse_da("/Cour 0 Tf 0.2 g");
        assert_eq!(da.font.as_deref(), Some("Cour"));
        assert_eq!(da.size, 0.0);
        assert_eq!(da.color_ops, "0.2000 g");

        // Missing color defaults to black.
        let da = parse_da("/Helv 10 Tf");
        assert_eq!(da.color_ops, "0 g");

        // Adversarial size is clamped; injected non-numeric color is dropped.
        let da = parse_da("/Helv 1e308 Tf");
        assert_eq!(da.size, 1000.0);
        let da = parse_da("1)Tj/Evil 0 0 rg /Helv 10 Tf");
        assert_eq!(da.color_ops, "0 g"); // bad operand → color rejected → default
    }

    #[test]
    fn winansi_punctuation_round_trips() {
        // Smart quote / em dash / euro map to their WinAnsi bytes, not '?'.
        assert_eq!(unicode_to_winansi('\u{2019}'), Some(0x92));
        assert_eq!(unicode_to_winansi('\u{2014}'), Some(0x97));
        assert_eq!(unicode_to_winansi('\u{20AC}'), Some(0x80));
        assert_eq!(unicode_to_winansi('A'), Some(0x41));
        assert_eq!(unicode_to_winansi('\u{00E9}'), Some(0xE9)); // é (Latin-1)
        assert_eq!(unicode_to_winansi('\u{4E2D}'), None); // CJK → fallback
    }

    #[test]
    fn non_finite_numbers_never_reach_output() {
        assert_eq!(fmt_num(f64::INFINITY), "0");
        assert_eq!(fmt_num(f64::NAN), "0");
        assert_eq!(fmt_num(-1.5), "-1.50");
    }

    #[test]
    fn utf16be_value_is_decoded() {
        // BOM + "Hi" in UTF-16BE.
        let bytes = [0xFE, 0xFF, 0x00, b'H', 0x00, b'i'];
        assert_eq!(pdf_string_to_unicode(&bytes), "Hi");
    }

    #[test]
    fn escape_handles_parens_and_backslash() {
        let mut out = Vec::new();
        escape_text("a(b)\\c", &mut out);
        assert_eq!(out, b"a\\(b\\)\\\\c");
    }

    #[test]
    fn standard_name_mapping() {
        assert_eq!(acrobat_standard_name("Helv"), "Helvetica");
        assert_eq!(acrobat_standard_name("ZaDb"), "ZapfDingbats");
        assert_eq!(acrobat_standard_name("F1"), "F1");
    }

    #[test]
    fn choice_value_maps_export_to_display_label() {
        let f = FormField {
            field_id: ObjectId(0, 0),
            name: "month".into(),
            kind: FieldKind::Choice,
            flags: 0,
            value: Some(FieldValue::Text("01".into())),
            default_appearance: None,
            quadding: 0,
            max_len: None,
            options: vec![
                ("01".into(), "January".into()),
                ("02".into(), "February".into()),
            ],
            widgets: vec![],
        };
        // /V holds the export value "01"; the rendered label is "January".
        assert_eq!(f.display_value().as_deref(), Some("January"));
        // An export with no matching option falls back to the raw value.
        let f2 = FormField {
            value: Some(FieldValue::Text("99".into())),
            ..f
        };
        assert_eq!(f2.display_value().as_deref(), Some("99"));
    }

    #[test]
    fn comb_is_suppressed_when_multiline() {
        let base = FormField {
            field_id: ObjectId(0, 0),
            name: "x".into(),
            kind: FieldKind::Text,
            flags: FF_COMB | FF_MULTILINE,
            value: Some(FieldValue::Text("AB".into())),
            default_appearance: None,
            quadding: 0,
            max_len: Some(4),
            options: vec![],
            widgets: vec![],
        };
        // Comb (bit 25) is meaningless with Multiline set.
        assert!(!base.is_comb());
        assert!(base.is_multiline());
    }

    #[test]
    fn comb_field_detection() {
        let f = FormField {
            field_id: ObjectId(0, 0),
            name: "x".into(),
            kind: FieldKind::Text,
            flags: FF_COMB,
            value: Some(FieldValue::Text("AB".into())),
            default_appearance: None,
            quadding: 0,
            max_len: Some(4),
            options: vec![],
            widgets: vec![],
        };
        assert!(f.is_comb());
        // Comb without MaxLen is not comb.
        let f2 = FormField {
            max_len: None,
            ..f.clone()
        };
        assert!(!f2.is_comb());
    }

    #[test]
    fn generated_appearance_draws_value() {
        let f = FormField {
            field_id: ObjectId(0, 0),
            name: "name".into(),
            kind: FieldKind::Text,
            flags: 0,
            value: Some(FieldValue::Text("Test".into())),
            default_appearance: Some("/Helv 12 Tf 0 g".into()),
            quadding: 0,
            max_len: None,
            options: vec![],
            widgets: vec![],
        };
        let ap = generate_widget_appearance(&f, Rect::new(0.0, 0.0, 200.0, 40.0), None)
            .expect("appearance");
        assert_eq!(ap.bbox, Rect::new(0.0, 0.0, 200.0, 40.0));
        let s = String::from_utf8_lossy(&ap.content);
        assert!(s.contains("/Tx BMC"));
        assert!(s.contains("Tf"));
        assert!(s.contains("(Test) Tj"));
        // Resources define the DA font name.
        assert!(ap.resources.get("Font").is_some());
    }

    #[test]
    fn empty_and_button_values_generate_nothing() {
        let base = FormField {
            field_id: ObjectId(0, 0),
            name: "x".into(),
            kind: FieldKind::Text,
            flags: 0,
            value: Some(FieldValue::Text(String::new())),
            default_appearance: None,
            quadding: 0,
            max_len: None,
            options: vec![],
            widgets: vec![],
        };
        assert!(
            generate_widget_appearance(&base, Rect::new(0.0, 0.0, 100.0, 20.0), None).is_none()
        );

        let button = FormField {
            kind: FieldKind::Button,
            value: Some(FieldValue::Name("Yes".into())),
            ..base.clone()
        };
        assert!(
            generate_widget_appearance(&button, Rect::new(0.0, 0.0, 100.0, 20.0), None).is_none()
        );

        let password = FormField {
            flags: FF_PASSWORD,
            value: Some(FieldValue::Text("secret".into())),
            ..base
        };
        assert!(
            generate_widget_appearance(&password, Rect::new(0.0, 0.0, 100.0, 20.0), None).is_none()
        );
    }
}
