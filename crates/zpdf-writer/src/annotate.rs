//! Annotation authoring: add markup and note annotations to pages.
//!
//! Writes spec-conformant annotation dictionaries (ISO 32000-1 §12.5.6)
//! **without** `/AP` streams — zpdf's own renderer (and Acrobat, per the
//! spec's "shall generate an appearance" rule) synthesizes the appearance
//! from the geometry properties via `annot_appearance.rs`, so the authored
//! annotations render identically on both backends with zero new render
//! code. `/InkList` annotations (which carry an `/AP`) go through the
//! existing [`crate::IncrementalWriter::add_ink_annotation_to_page`].
//!
//! All coordinates are PDF user space (origin bottom-left, y-up).

use zpdf_core::{ObjectId, PdfDict, PdfName, PdfObject, PdfString, Rect, Result};

use crate::metadata::encode_text_string;
use crate::{invalid_data, IncrementalWriter};

/// An annotation to author. Colors are DeviceRGB components in `[0, 1]`.
#[derive(Debug, Clone)]
pub enum AnnotationSpec {
    /// Text-markup over one or more oriented quads. Each quad is
    /// `[x1,y1, x2,y2, x3,y3, x4,y4]` (the `/QuadPoints` order). For
    /// axis-aligned text, use [`AnnotationSpec::markup_from_rects`].
    Markup {
        kind: MarkupKind,
        quads: Vec<[f64; 8]>,
        color: (f64, f64, f64),
        /// Optional comment shown in the annotation's popup.
        contents: Option<String>,
    },
    /// A "sticky note" icon with a comment.
    Note {
        /// Icon anchor (lower-left of the icon box; standard size 20×20).
        x: f64,
        y: f64,
        contents: String,
        color: Option<(f64, f64, f64)>,
        /// Icon name: Note (default), Comment, Help, Insert, Key, Check, Cross.
        icon: Option<String>,
    },
    /// Free-floating text drawn inside a rectangle.
    FreeText {
        rect: Rect,
        contents: String,
        /// Font size for the /DA string (default 12).
        size: Option<f64>,
        color: Option<(f64, f64, f64)>,
    },
    /// Rectangle (Square annotation) with optional interior color.
    Square {
        rect: Rect,
        color: (f64, f64, f64),
        interior: Option<(f64, f64, f64)>,
        width: f64,
    },
    /// Ellipse (Circle annotation) inscribed in `rect`.
    Circle {
        rect: Rect,
        color: (f64, f64, f64),
        interior: Option<(f64, f64, f64)>,
        width: f64,
    },
    /// A straight line from `(x1,y1)` to `(x2,y2)`.
    Line {
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        color: (f64, f64, f64),
        width: f64,
    },
}

/// The four text-markup annotation subtypes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkupKind {
    Highlight,
    Underline,
    StrikeOut,
    Squiggly,
}

impl MarkupKind {
    fn subtype(self) -> &'static str {
        match self {
            MarkupKind::Highlight => "Highlight",
            MarkupKind::Underline => "Underline",
            MarkupKind::StrikeOut => "StrikeOut",
            MarkupKind::Squiggly => "Squiggly",
        }
    }
}

impl AnnotationSpec {
    /// Build a text-markup spec from axis-aligned rectangles (e.g. search-hit
    /// rects): each rect becomes one `/QuadPoints` quad.
    pub fn markup_from_rects(
        kind: MarkupKind,
        rects: &[Rect],
        color: (f64, f64, f64),
        contents: Option<String>,
    ) -> Self {
        let quads = rects
            .iter()
            .map(|r| {
                let r = r.normalize();
                // QuadPoints order: upper-left, upper-right, lower-left,
                // lower-right (the de-facto order every viewer expects).
                [r.x0, r.y1, r.x1, r.y1, r.x0, r.y0, r.x1, r.y0]
            })
            .collect();
        AnnotationSpec::Markup {
            kind,
            quads,
            color,
            contents,
        }
    }
}

impl IncrementalWriter {
    /// Append an authored annotation to a page (0-based index). Returns the
    /// new annotation's object id.
    pub fn add_annotation(&mut self, page_index: usize, spec: &AnnotationSpec) -> Result<ObjectId> {
        let dict = build_annotation_dict(spec)?;
        let page_id = self.page_id(page_index)?;
        self.ensure_object_capacity(1)?;

        let (num, gen) = self.try_add_object(&PdfObject::Dict(dict))?;
        let annot_id = ObjectId(num, gen as u16);

        // Append to the page's /Annots (same load-modify-store as ink).
        let page_obj = self.resolve_current(page_id)?;
        let mut page_dict = page_obj.as_dict()?.clone();
        let mut annots = match page_dict.get("Annots") {
            Some(PdfObject::Ref(r)) => match self.resolve_current(*r) {
                Ok(obj) => obj.as_array().ok().map(|a| a.to_vec()).unwrap_or_default(),
                Err(_) => Vec::new(),
            },
            Some(PdfObject::Array(arr)) => arr.to_vec(),
            _ => Vec::new(),
        };
        annots.push(PdfObject::Ref(annot_id));
        page_dict.insert(PdfName::new("Annots"), PdfObject::Array(annots));
        self.overwrite_object(page_id, PdfObject::Dict(page_dict));
        Ok(annot_id)
    }
}

/// The standard note icon size Acrobat uses.
const NOTE_ICON_SIZE: f64 = 20.0;

fn build_annotation_dict(spec: &AnnotationSpec) -> Result<PdfDict> {
    let mut dict = PdfDict::new();
    dict.insert(PdfName::new("Type"), PdfObject::Name(PdfName::new("Annot")));

    match spec {
        AnnotationSpec::Markup {
            kind,
            quads,
            color,
            contents,
        } => {
            if quads.is_empty() {
                return Err(invalid_data("markup annotation needs at least one quad").into());
            }
            for q in quads {
                if q.iter().any(|v| !v.is_finite()) {
                    return Err(invalid_data("quad coordinates must be finite").into());
                }
            }
            dict.insert(
                PdfName::new("Subtype"),
                PdfObject::Name(PdfName::new(kind.subtype())),
            );
            // /Rect = bounding box of all quads.
            let (mut x0, mut y0) = (f64::INFINITY, f64::INFINITY);
            let (mut x1, mut y1) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
            let mut qp = Vec::with_capacity(quads.len() * 8);
            for q in quads {
                for (i, &v) in q.iter().enumerate() {
                    if i % 2 == 0 {
                        x0 = x0.min(v);
                        x1 = x1.max(v);
                    } else {
                        y0 = y0.min(v);
                        y1 = y1.max(v);
                    }
                    qp.push(PdfObject::Real(v));
                }
            }
            set_rect(&mut dict, Rect::new(x0, y0, x1, y1));
            dict.insert(PdfName::new("QuadPoints"), PdfObject::Array(qp));
            set_color(&mut dict, "C", *color);
            if let Some(text) = contents {
                set_contents(&mut dict, text);
            }
        }
        AnnotationSpec::Note {
            x,
            y,
            contents,
            color,
            icon,
        } => {
            dict.insert(
                PdfName::new("Subtype"),
                PdfObject::Name(PdfName::new("Text")),
            );
            set_rect(
                &mut dict,
                Rect::new(*x, *y, x + NOTE_ICON_SIZE, y + NOTE_ICON_SIZE),
            );
            set_contents(&mut dict, contents);
            if let Some(c) = color {
                set_color(&mut dict, "C", *c);
            }
            if let Some(name) = icon {
                dict.insert(PdfName::new("Name"), PdfObject::Name(PdfName::new(name)));
            }
        }
        AnnotationSpec::FreeText {
            rect,
            contents,
            size,
            color,
        } => {
            dict.insert(
                PdfName::new("Subtype"),
                PdfObject::Name(PdfName::new("FreeText")),
            );
            set_rect(&mut dict, *rect);
            set_contents(&mut dict, contents);
            // /DA: font + size + fill color (the appearance synthesizer's
            // FreeText path reads it like a form field default appearance).
            let (r, g, b) = color.unwrap_or((0.0, 0.0, 0.0));
            let da = format!("/Helv {} Tf {r:.3} {g:.3} {b:.3} rg", size.unwrap_or(12.0));
            dict.insert(
                PdfName::new("DA"),
                PdfObject::String(PdfString(da.into_bytes())),
            );
        }
        AnnotationSpec::Square {
            rect,
            color,
            interior,
            width,
        }
        | AnnotationSpec::Circle {
            rect,
            color,
            interior,
            width,
        } => {
            let subtype = if matches!(spec, AnnotationSpec::Square { .. }) {
                "Square"
            } else {
                "Circle"
            };
            dict.insert(
                PdfName::new("Subtype"),
                PdfObject::Name(PdfName::new(subtype)),
            );
            set_rect(&mut dict, *rect);
            set_color(&mut dict, "C", *color);
            if let Some(ic) = interior {
                set_color(&mut dict, "IC", *ic);
            }
            set_border_width(&mut dict, *width);
        }
        AnnotationSpec::Line {
            x1,
            y1,
            x2,
            y2,
            color,
            width,
        } => {
            dict.insert(
                PdfName::new("Subtype"),
                PdfObject::Name(PdfName::new("Line")),
            );
            let pad = width.max(1.0);
            set_rect(
                &mut dict,
                Rect::new(
                    x1.min(*x2) - pad,
                    y1.min(*y2) - pad,
                    x1.max(*x2) + pad,
                    y1.max(*y2) + pad,
                ),
            );
            dict.insert(
                PdfName::new("L"),
                PdfObject::Array(vec![
                    PdfObject::Real(*x1),
                    PdfObject::Real(*y1),
                    PdfObject::Real(*x2),
                    PdfObject::Real(*y2),
                ]),
            );
            set_color(&mut dict, "C", *color);
            set_border_width(&mut dict, *width);
        }
    }
    Ok(dict)
}

fn set_rect(dict: &mut PdfDict, rect: Rect) {
    let r = rect.normalize();
    dict.insert(
        PdfName::new("Rect"),
        PdfObject::Array(vec![
            PdfObject::Real(r.x0),
            PdfObject::Real(r.y0),
            PdfObject::Real(r.x1),
            PdfObject::Real(r.y1),
        ]),
    );
}

fn set_color(dict: &mut PdfDict, key: &str, (r, g, b): (f64, f64, f64)) {
    dict.insert(
        PdfName::new(key),
        PdfObject::Array(vec![
            PdfObject::Real(r.clamp(0.0, 1.0)),
            PdfObject::Real(g.clamp(0.0, 1.0)),
            PdfObject::Real(b.clamp(0.0, 1.0)),
        ]),
    );
}

fn set_contents(dict: &mut PdfDict, text: &str) {
    dict.insert(
        PdfName::new("Contents"),
        PdfObject::String(encode_text_string(text)),
    );
}

fn set_border_width(dict: &mut PdfDict, width: f64) {
    let mut bs = PdfDict::new();
    bs.insert(PdfName::new("W"), PdfObject::Real(width.max(0.0)));
    dict.insert(PdfName::new("BS"), PdfObject::Dict(bs));
}
