//! Small shared object-graph helpers for the document-level *readers*
//! (outline, destinations, document info). These mirror the private helpers
//! proven in [`crate::embedded_files`] — reference-following accessors that
//! resolve a value given directly or as an indirect reference — collected here
//! so the navigation/metadata readers share one bounded accessor set. The
//! embedded-files reader keeps its own copies untouched.

use zpdf_core::{PdfDict, PdfObject};
use zpdf_parser::PdfFile;

use crate::forms::pdf_string_to_unicode;

/// The catalog dictionary (`/Root`), or `None` if unreachable.
pub(crate) fn catalog_dict(file: &PdfFile) -> Option<PdfDict> {
    let root_ref = file.trailer.get_ref("Root").ok()?;
    file.resolve(root_ref).ok()?.as_dict().ok().cloned()
}

/// Resolve a dictionary value that may be given directly or indirectly. A stream
/// also satisfies a dictionary lookup (its dict is exposed) for lax producers.
pub(crate) fn resolve_dict(file: &PdfFile, obj: Option<&PdfObject>) -> Option<PdfDict> {
    match obj? {
        PdfObject::Dict(d) => Some(d.clone()),
        PdfObject::Stream(s) => Some(s.dict.clone()),
        PdfObject::Ref(r) => match file.resolve(*r).ok()? {
            PdfObject::Dict(d) => Some(d),
            PdfObject::Stream(s) => Some(s.dict),
            _ => None,
        },
        _ => None,
    }
}

/// Resolve an array value that may be given directly or indirectly.
pub(crate) fn resolve_array(file: &PdfFile, obj: Option<&PdfObject>) -> Option<Vec<PdfObject>> {
    match obj? {
        PdfObject::Array(a) => Some(a.clone()),
        PdfObject::Ref(r) => match file.resolve(*r).ok()? {
            PdfObject::Array(a) => Some(a),
            _ => None,
        },
        _ => None,
    }
}

/// Decode a text-string dict entry, following one indirect reference. UTF-16BE
/// when the value carries the `FE FF` BOM, otherwise the bytes are taken as
/// PDFDocEncoding *approximated by Latin-1* (exact for the common range; a few
/// PDFDoc-specific code points in `0x18`–`0x1F` / `0x80`–`0xA0` differ) — see
/// [`pdf_string_to_unicode`].
pub(crate) fn text(file: &PdfFile, dict: &PdfDict, key: &str) -> Option<String> {
    let value = match dict.get(key)? {
        PdfObject::String(s) => return Some(pdf_string_to_unicode(s.as_bytes())),
        PdfObject::Ref(r) => file.resolve(*r).ok()?,
        _ => return None,
    };
    match value {
        PdfObject::String(s) => Some(pdf_string_to_unicode(s.as_bytes())),
        _ => None,
    }
}

/// Read a Name-valued dict entry, following one indirect reference.
pub(crate) fn name_value(file: &PdfFile, dict: &PdfDict, key: &str) -> Option<String> {
    resolve_name(file, dict.get(key))
}

/// Read a Name-valued object, following one indirect reference.
pub(crate) fn resolve_name(file: &PdfFile, obj: Option<&PdfObject>) -> Option<String> {
    match obj? {
        PdfObject::Name(n) => Some(n.as_str().to_string()),
        PdfObject::Ref(r) => match file.resolve(*r).ok()? {
            PdfObject::Name(n) => Some(n.as_str().to_string()),
            _ => None,
        },
        _ => None,
    }
}

/// Read a numeric object (Integer or Real) as `f32`, following one indirect
/// reference. `null` (used in a destination array for "retain current value")
/// and non-numeric values yield `None`.
pub(crate) fn resolve_number(file: &PdfFile, obj: Option<&PdfObject>) -> Option<f32> {
    let value = match obj? {
        PdfObject::Integer(n) => return Some(*n as f32),
        PdfObject::Real(r) if r.is_finite() => return Some(*r as f32),
        PdfObject::Ref(r) => file.resolve(*r).ok()?,
        _ => return None,
    };
    match value {
        PdfObject::Integer(n) => Some(n as f32),
        PdfObject::Real(r) if r.is_finite() => Some(r as f32),
        _ => None,
    }
}
