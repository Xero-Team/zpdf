//! Output intents (`/OutputIntents`), document-level (catalog, ISO 32000-1
//! §14.11.5) and page-level (ISO 32000-2 / PDF 2.0).
//!
//! An output intent declares the *characterized printing condition* a document
//! was prepared for. Its `/DestOutputProfile` — an embedded ICC profile stream
//! — lets a DeviceCMYK document specify exactly how its CMYK is meant to be
//! interpreted (the PDF/X model). When that profile is 4-channel (CMYK), the
//! renderer colour-manages DeviceCMYK through it instead of the generic Adobe
//! SWOP approximation.
//!
//! This module only *parses and exposes* the metadata (including the profile
//! stream's object id and channel count); compiling the profile into a colour
//! transform and substituting it for DeviceCMYK happens in the render pipeline
//! (`zpdf-content`), which owns the `IccCache`. Keeping the compile out of the
//! document model leaves this crate free of any colour-management dependency.

use zpdf_core::{ObjectId, PdfDict, PdfObject};
use zpdf_parser::PdfFile;

use crate::forms::pdf_string_to_unicode;

/// Defensive cap on the number of entries parsed from one `/OutputIntents`
/// array — real documents carry one or two; this only bounds adversarial input.
const MAX_OUTPUT_INTENTS: usize = 64;

/// One `/OutputIntents` entry.
#[derive(Debug, Clone)]
pub struct OutputIntent {
    /// `/S` subtype name, e.g. `"GTS_PDFX"`, `"GTS_PDFA1"`, `"ISO_PDFE1"`.
    /// Empty when the entry omits `/S`.
    pub subtype: String,
    /// `/OutputConditionIdentifier` — the characterized printing condition,
    /// usually a registry key such as `"CGATS TR 001"`. Decoded as a text
    /// string (UTF-16BE with BOM, else PDFDocEncoding bytes).
    pub output_condition_identifier: Option<String>,
    /// `/OutputCondition` — human-readable condition name, if present.
    pub output_condition: Option<String>,
    /// `/Info` — additional human-readable description, if present.
    pub info: Option<String>,
    /// `/DestOutputProfile` — the embedded ICC profile stream's object id.
    /// `None` when the intent only names an external (registry) profile.
    pub dest_output_profile: Option<ObjectId>,
    /// `/N` (component count) read off the `/DestOutputProfile` stream dict
    /// *without* decoding the profile. `None` when the key is absent/unreadable.
    pub dest_profile_components: Option<i64>,
}

impl OutputIntent {
    /// Cheap heuristic — usable without decoding the profile — for whether this
    /// intent *looks like* it can colour-manage DeviceCMYK: it has an embedded
    /// profile whose `/N` is 4 or absent. It is advisory (e.g. for `zpdf info`):
    /// the render path is authoritative, accepting a profile only on its
    /// compiled channel count, so a profile with a mistyped `/N` is still
    /// honoured there even though this returns `false`.
    pub fn has_cmyk_profile(&self) -> bool {
        self.dest_output_profile.is_some() && self.dest_profile_components.is_none_or(|n| n == 4)
    }
}

/// Document-level `/OutputIntents` from the catalog (`/Root`). Empty when the
/// document declares none.
pub fn parse_output_intents(file: &PdfFile) -> Vec<OutputIntent> {
    let root = file
        .trailer
        .get_ref("Root")
        .ok()
        .and_then(|r| file.resolve(r).ok())
        .and_then(|o| o.as_dict().ok().cloned());
    match root {
        Some(dict) => parse_intents_array(file, dict.get("OutputIntents")),
        None => Vec::new(),
    }
}

/// PDF 2.0 page-level `/OutputIntents`, read off an already-resolved leaf page
/// dictionary. Empty for pre-2.0 / most documents. Page-level intents override
/// the document-level ones for that page.
pub fn parse_page_output_intents(file: &PdfFile, page_dict: &PdfDict) -> Vec<OutputIntent> {
    parse_intents_array(file, page_dict.get("OutputIntents"))
}

/// Parse an `/OutputIntents` array value (which itself may be given indirectly).
fn parse_intents_array(file: &PdfFile, obj: Option<&PdfObject>) -> Vec<OutputIntent> {
    let arr = match obj {
        Some(PdfObject::Array(a)) => a.clone(),
        Some(PdfObject::Ref(r)) => match file.resolve(*r) {
            Ok(PdfObject::Array(a)) => a,
            _ => return Vec::new(),
        },
        _ => return Vec::new(),
    };
    let mut out = Vec::new();
    for elem in arr.iter().take(MAX_OUTPUT_INTENTS) {
        let dict = match elem {
            PdfObject::Dict(d) => Some(d.clone()),
            PdfObject::Ref(r) => file
                .resolve(*r)
                .ok()
                .and_then(|o| o.as_dict().ok().cloned()),
            _ => None,
        };
        match dict {
            Some(d) => out.push(parse_one_intent(file, &d)),
            None => tracing::warn!("/OutputIntents entry is not a dictionary; skipping"),
        }
    }
    out
}

fn parse_one_intent(file: &PdfFile, dict: &PdfDict) -> OutputIntent {
    let text = |key: &str| match dict.get(key) {
        Some(PdfObject::String(s)) => Some(pdf_string_to_unicode(s.as_bytes())),
        _ => None,
    };
    let dest_output_profile = dict.get_ref("DestOutputProfile").ok();
    // Read the profile stream's /N without decoding the (potentially large)
    // ICC payload — enough to tell a CMYK characterization from an RGB one.
    let dest_profile_components = dest_output_profile.and_then(|id| {
        file.resolve(id)
            .ok()
            .and_then(|o| o.as_stream().ok().and_then(|s| s.dict.get_i64("N").ok()))
    });
    OutputIntent {
        subtype: dict.get_name("S").unwrap_or("").to_string(),
        output_condition_identifier: text("OutputConditionIdentifier"),
        output_condition: text("OutputCondition"),
        info: text("Info"),
        dest_output_profile,
        dest_profile_components,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::build_pdf;
    use zpdf_parser::PdfFile;

    fn parse(objects: &[&str]) -> Vec<OutputIntent> {
        let file = PdfFile::parse(build_pdf(objects)).expect("parse pdf");
        parse_output_intents(&file)
    }

    #[test]
    fn document_output_intent_with_cmyk_profile() {
        // obj1 catalog, obj2 pages, obj3 page, obj4 intent, obj5 ICC stream /N 4.
        let ois = parse(&[
            "<< /Type /Catalog /Pages 2 0 R /OutputIntents [4 0 R] >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>",
            "<< /Type /OutputIntent /S /GTS_PDFX \
             /OutputConditionIdentifier (CGATS TR 001) \
             /OutputCondition (SWOP) /Info (U.S. Web Coated) /DestOutputProfile 5 0 R >>",
            "<< /N 4 /Length 0 >>\nstream\n\nendstream",
        ]);
        assert_eq!(ois.len(), 1);
        let oi = &ois[0];
        assert_eq!(oi.subtype, "GTS_PDFX");
        assert_eq!(
            oi.output_condition_identifier.as_deref(),
            Some("CGATS TR 001")
        );
        assert_eq!(oi.output_condition.as_deref(), Some("SWOP"));
        assert_eq!(oi.info.as_deref(), Some("U.S. Web Coated"));
        assert_eq!(oi.dest_output_profile, Some(ObjectId(5, 0)));
        assert_eq!(oi.dest_profile_components, Some(4));
        assert!(oi.has_cmyk_profile());
    }

    #[test]
    fn absent_output_intents_is_empty() {
        let ois = parse(&[
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>",
        ]);
        assert!(ois.is_empty());
    }

    #[test]
    fn external_profile_intent_has_no_object_id() {
        // An intent with a condition identifier but no embedded /DestOutputProfile
        // is still parsed (reportable), and is not a CMYK-management candidate.
        let ois = parse(&[
            "<< /Type /Catalog /Pages 2 0 R /OutputIntents [4 0 R] >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>",
            "<< /Type /OutputIntent /S /GTS_PDFX /OutputConditionIdentifier (FOGRA39) >>",
        ]);
        assert_eq!(ois.len(), 1);
        assert_eq!(ois[0].dest_output_profile, None);
        assert_eq!(ois[0].dest_profile_components, None);
        assert!(!ois[0].has_cmyk_profile());
    }

    #[test]
    fn rgb_profile_is_not_a_cmyk_candidate() {
        let ois = parse(&[
            "<< /Type /Catalog /Pages 2 0 R /OutputIntents [4 0 R] >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>",
            "<< /Type /OutputIntent /S /GTS_PDFA1 /DestOutputProfile 5 0 R >>",
            "<< /N 3 /Length 0 >>\nstream\n\nendstream",
        ]);
        assert_eq!(ois[0].dest_profile_components, Some(3));
        assert!(!ois[0].has_cmyk_profile());
    }

    #[test]
    fn utf16be_condition_identifier_decodes() {
        // <FEFF 0053 0057 004F 0050> = "SWOP" in UTF-16BE with a BOM.
        let ois = parse(&[
            "<< /Type /Catalog /Pages 2 0 R /OutputIntents [4 0 R] >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>",
            "<< /Type /OutputIntent /S /GTS_PDFX \
             /OutputConditionIdentifier <FEFF00530057004F0050> >>",
        ]);
        assert_eq!(ois[0].output_condition_identifier.as_deref(), Some("SWOP"));
    }

    #[test]
    fn non_dict_entries_are_skipped_without_panic() {
        let ois = parse(&[
            "<< /Type /Catalog /Pages 2 0 R /OutputIntents [4 0 R 99 0 R] >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>",
            "<< /Type /OutputIntent /S /GTS_PDFX >>",
            // obj 5 exists but is not referenced; 99 0 R above is dangling.
            "null",
        ]);
        // The dangling 99 0 R entry is dropped; the valid one survives.
        assert_eq!(ois.len(), 1);
        assert_eq!(ois[0].subtype, "GTS_PDFX");
    }
}
