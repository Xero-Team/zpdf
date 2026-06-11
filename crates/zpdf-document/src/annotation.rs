//! Annotation parsing: `/Annots` entries resolved into renderable form —
//! `/Rect`, `/F` flags, the `/AS`-selected normal appearance stream, and the
//! optional-content membership (`/OC`). Painting itself happens in
//! zpdf-content, which replays the appearance stream as a form XObject mapped
//! onto `/Rect` (PDF 32000-1 §12.5.5).

use zpdf_core::{ObjectId, PdfObject, Rect};
use zpdf_parser::PdfFile;

use crate::page::PdfPage;

/// Annotation flag bits (PDF 32000-1 Table 165).
pub const ANNOT_FLAG_HIDDEN: i64 = 1 << 1;
pub const ANNOT_FLAG_NOVIEW: i64 = 1 << 5;

#[derive(Debug, Clone)]
pub struct Annotation {
    pub subtype: String,
    /// Target rectangle in default page user space.
    pub rect: Rect,
    /// /F flags (Hidden / NoView suppress screen rendering).
    pub flags: i64,
    /// The selected normal appearance stream: `/AP /N`, indexed by `/AS`
    /// when /N is a state dictionary.
    pub appearance: Option<ObjectId>,
    /// /OC optional-content membership (a Ref to an OCG/OCMD, or a direct
    /// dict), evaluated against the document's OC config at paint time.
    pub oc: Option<PdfObject>,
}

impl Annotation {
    /// True when the annotation should be painted in a screen rendering
    /// (before optional-content evaluation).
    pub fn is_viewable(&self) -> bool {
        self.flags & (ANNOT_FLAG_HIDDEN | ANNOT_FLAG_NOVIEW) == 0
            // Popups only appear when opened interactively.
            && self.subtype != "Popup"
            && self.appearance.is_some()
            && self.rect.width() > 0.0
            && self.rect.height() > 0.0
    }
}

/// Parse a page's annotations into renderable form. Unresolvable or
/// appearance-less entries are kept (callers may want link rects later) but
/// fail `is_viewable`.
pub fn parse_annotations(file: &PdfFile, page: &PdfPage) -> Vec<Annotation> {
    page.annots
        .iter()
        .filter_map(|&id| parse_annotation(file, id))
        .collect()
}

fn parse_annotation(file: &PdfFile, id: ObjectId) -> Option<Annotation> {
    let obj = file.resolve(id).ok()?;
    let dict = obj.as_dict().ok()?;

    let subtype = dict.get_name("Subtype").unwrap_or("").to_string();
    let rect = crate::page::resolve_rect(file, dict, "Rect")?;
    let flags = match dict.get("F") {
        Some(PdfObject::Integer(n)) => *n,
        Some(PdfObject::Ref(r)) => file
            .resolve(*r)
            .ok()
            .and_then(|o| o.as_i64().ok())
            .unwrap_or(0),
        _ => 0,
    };

    let appearance = select_appearance(file, dict);
    let oc = dict.get("OC").cloned();

    Some(Annotation {
        subtype,
        rect,
        flags,
        appearance,
        oc,
    })
}

/// Resolve `/AP /N` to a concrete stream id, indexing state dictionaries by
/// `/AS` (with the common single-entry leniency when /AS is absent).
fn select_appearance(file: &PdfFile, annot: &zpdf_core::PdfDict) -> Option<ObjectId> {
    let ap = match annot.get("AP")? {
        PdfObject::Dict(d) => d.clone(),
        PdfObject::Ref(r) => file.resolve(*r).ok()?.as_dict().ok()?.clone(),
        _ => return None,
    };
    let n = ap.get("N")?;

    // /N as a direct stream ref.
    if let PdfObject::Ref(r) = n {
        match file.resolve(*r).ok()? {
            PdfObject::Stream(_) => return Some(*r),
            PdfObject::Dict(states) => return select_state(file, &states, annot),
            _ => return None,
        }
    }
    // /N as a direct state dictionary.
    if let PdfObject::Dict(states) = n {
        return select_state(file, states, annot);
    }
    None
}

fn select_state(
    file: &PdfFile,
    states: &zpdf_core::PdfDict,
    annot: &zpdf_core::PdfDict,
) -> Option<ObjectId> {
    let as_name = annot.get_name("AS").ok();
    if let Some(state) = as_name {
        if let Some(PdfObject::Ref(r)) = states.get(state) {
            return Some(*r);
        }
    }
    // Lenient fallback: a one-entry state dict needs no /AS.
    if states.0.len() == 1 {
        if let Some(PdfObject::Ref(r)) = states.0.values().next() {
            return Some(*r);
        }
    }
    let _ = file;
    None
}
