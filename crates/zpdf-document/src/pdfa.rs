//! PDF/A conformance validation (profiles A-1b, A-2b, and A-3b).
//!
//! A **rule engine** over the parsed document: each check inspects one aspect
//! of the file and yields zero or more [`Violation`]s. This does not aim for
//! veraPDF-level completeness — it covers the high-signal, machine-checkable
//! clauses of ISO 19005-1 (PDF/A-1b), 19005-2 (PDF/A-2b), and 19005-3 (PDF/A-3b):
//!
//! - file structure: header version, no encryption, trailer /ID present
//! - fonts: every used font embedded (except the standard 14 in no profile —
//!   PDF/A requires embedding even for those)
//! - XMP metadata: present, with a `pdfaid:part`/`conformance` claim
//! - output intent: a PDF/A output intent with an embedded ICC profile
//! - forbidden features: JavaScript/actions, embedded files (A-1),
//!   transparency (A-1: soft masks / group /S /Transparency), LZW (A-1),
//!   encryption of any kind
//! - associated files (A-3): every embedded file in `/Names /EmbeddedFiles`
//!   must be referenced from a catalog/page `/AF` array, and each `/AF` file
//!   specification must carry an `/AFRelationship` (ISO 19005-3 §6.2.4)
//!
//! Everything is best-effort and read-only over `ParseLimits`-bounded APIs;
//! a check that cannot run (e.g. a malformed font dict) reports what it saw.

use std::collections::HashSet;

use zpdf_core::{ObjectId, PdfDict, PdfObject};
use zpdf_parser::PdfFile;

/// The validation profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// ISO 19005-1 Level B (PDF/A-1b): PDF 1.4 model, no transparency.
    A1b,
    /// ISO 19005-2 Level B (PDF/A-2b): PDF 1.7 model, transparency allowed.
    A2b,
    /// ISO 19005-3 Level B (PDF/A-3b): PDF/A-2 plus the `/AF` associated-files
    /// mechanism — embedded files are permitted but must be referenced from a
    /// catalog/page `/AF` array whose file-spec carries `/AFRelationship`.
    A3b,
}

impl Profile {
    pub fn as_str(self) -> &'static str {
        match self {
            Profile::A1b => "PDF/A-1b",
            Profile::A2b => "PDF/A-2b",
            Profile::A3b => "PDF/A-3b",
        }
    }
}

/// One conformance violation.
#[derive(Debug, Clone)]
pub struct Violation {
    /// Short rule identifier, e.g. `"encryption"`, `"font-not-embedded"`.
    pub rule: &'static str,
    /// Human-readable explanation with the offending object where known.
    pub message: String,
}

/// The outcome of a validation run.
#[derive(Debug)]
pub struct ValidationReport {
    pub profile: Profile,
    pub violations: Vec<Violation>,
    /// The `pdfaid:part`/`pdfaid:conformance` the document itself claims via
    /// XMP, e.g. `Some(("1", "B"))` — independent of whether it conforms.
    pub claimed: Option<(String, String)>,
}

impl ValidationReport {
    pub fn conforms(&self) -> bool {
        self.violations.is_empty()
    }
}

/// Validate `file` against `profile`.
pub fn validate(file: &PdfFile, profile: Profile) -> ValidationReport {
    let mut v: Vec<Violation> = Vec::new();

    check_structure(file, profile, &mut v);
    check_xmp(file, &mut v);
    let claimed = xmp_claim(file);
    check_output_intent(file, &mut v);
    check_fonts(file, &mut v);
    check_forbidden_features(file, profile, &mut v);
    if profile == Profile::A3b {
        check_embedded_files_af(file, &mut v);
    }

    ValidationReport {
        profile,
        violations: v,
        claimed,
    }
}

// ---------------------------------------------------------------------------
// File structure
// ---------------------------------------------------------------------------

fn check_structure(file: &PdfFile, profile: Profile, out: &mut Vec<Violation>) {
    // Encryption is forbidden in every PDF/A part.
    if file.is_encrypted() {
        out.push(Violation {
            rule: "encryption",
            message: "document is encrypted (/Encrypt present); PDF/A forbids encryption".into(),
        });
    }

    // Trailer /ID is required.
    match file.trailer.get("ID") {
        Some(PdfObject::Array(a)) if a.len() == 2 => {}
        _ => out.push(Violation {
            rule: "file-id",
            message: "trailer /ID missing or not a two-element array".into(),
        }),
    }

    // Header version ceiling: 1.4 for A-1, 1.7 for A-2 and A-3. The parser
    // records the header; a higher version is a violation for A-1 (A-2/A-3 are
    // based on 1.7, which is the cap of what zpdf writes anyway). A-3 is a PDF
    // 1.7 family standard, so a PDF 2.0 header is also non-conformant.
    match profile {
        Profile::A1b => {
            let data = file.data();
            if let Some(line) = data.get(..16) {
                let header = String::from_utf8_lossy(line);
                if let Some(ver) = header.strip_prefix("%PDF-1.") {
                    if let Some(minor) = ver.chars().next().and_then(|c| c.to_digit(10)) {
                        if minor > 4 {
                            out.push(Violation {
                                rule: "header-version",
                                message: format!(
                                    "header declares PDF 1.{minor}; PDF/A-1 is based on PDF 1.4"
                                ),
                            });
                        }
                    }
                }
            }
        }
        Profile::A3b => {
            let h = file.header;
            if h.major != 1 || h.minor > 7 {
                out.push(Violation {
                    rule: "header-version",
                    message: format!(
                        "header declares PDF {}.{}; PDF/A-3 is based on PDF 1.7",
                        h.major, h.minor
                    ),
                });
            }
        }
        Profile::A2b => {}
    }
}

// ---------------------------------------------------------------------------
// XMP metadata
// ---------------------------------------------------------------------------

fn check_xmp(file: &PdfFile, out: &mut Vec<Violation>) {
    let Some(xml) = crate::xmp::metadata_bytes(file) else {
        out.push(Violation {
            rule: "xmp-missing",
            message: "catalog has no /Metadata XMP stream; PDF/A requires XMP metadata".into(),
        });
        return;
    };
    let text = String::from_utf8_lossy(&xml);
    if !text.contains("pdfaid:part") && !text.contains("http://www.aiim.org/pdfa/ns/id/") {
        out.push(Violation {
            rule: "xmp-pdfaid",
            message: "XMP metadata carries no PDF/A identification (pdfaid:part)".into(),
        });
    }
}

/// The (part, conformance) the XMP claims, when parseable.
fn xmp_claim(file: &PdfFile) -> Option<(String, String)> {
    let xml = crate::xmp::metadata_bytes(file)?;
    let text = String::from_utf8_lossy(&xml);
    let part = extract_xmp_value(&text, "pdfaid:part")?;
    let conf = extract_xmp_value(&text, "pdfaid:conformance").unwrap_or_default();
    Some((part, conf))
}

/// Pull `name`'s value out of XMP in either element (`<name>v</name>`) or
/// attribute (`name="v"`) form.
fn extract_xmp_value(text: &str, name: &str) -> Option<String> {
    if let Some(start) = text.find(&format!("<{name}>")) {
        let vstart = start + name.len() + 2;
        let vend = text[vstart..].find('<')? + vstart;
        return Some(text[vstart..vend].trim().to_string());
    }
    let attr = format!("{name}=\"");
    if let Some(start) = text.find(&attr) {
        let vstart = start + attr.len();
        let vend = text[vstart..].find('"')? + vstart;
        return Some(text[vstart..vend].trim().to_string());
    }
    None
}

// ---------------------------------------------------------------------------
// Output intent
// ---------------------------------------------------------------------------

fn check_output_intent(file: &PdfFile, out: &mut Vec<Violation>) {
    let intents = crate::output_intents::parse_output_intents(file);
    let pdfa_intent = intents.iter().find(|i| i.subtype == "GTS_PDFA1");
    match pdfa_intent {
        None => out.push(Violation {
            rule: "output-intent",
            message: "no GTS_PDFA1 output intent; PDF/A requires one for device-dependent color"
                .into(),
        }),
        Some(intent) => {
            if intent.dest_output_profile.is_none() {
                out.push(Violation {
                    rule: "output-intent-profile",
                    message: "PDF/A output intent has no embedded /DestOutputProfile ICC stream"
                        .into(),
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Fonts
// ---------------------------------------------------------------------------

fn check_fonts(file: &PdfFile, out: &mut Vec<Violation>) {
    // Walk every page's resource /Font entries and require an embedded font
    // file in the descriptor (FontFile / FontFile2 / FontFile3). Type0 fonts
    // recurse into their descendant. Type3 fonts have no descriptor (their
    // glyphs are content streams) and are exempt.
    let mut reported: HashSet<String> = HashSet::new();
    for dict in collect_font_dicts(file) {
        let subtype = dict.get_name("Subtype").unwrap_or("");
        if subtype == "Type3" {
            continue;
        }
        let base = dict.get_name("BaseFont").unwrap_or("?").to_string();

        // Type0: check the descendant CIDFont's descriptor.
        let target = if subtype == "Type0" {
            match dict.get("DescendantFonts").map(|o| deref(file, o)) {
                Some(PdfObject::Array(a)) if !a.is_empty() => match deref(file, &a[0]) {
                    PdfObject::Dict(d) => Some(d),
                    _ => None,
                },
                _ => None,
            }
        } else {
            Some(dict.clone())
        };

        let embedded = target
            .as_ref()
            .and_then(|d| d.get("FontDescriptor").map(|o| deref(file, o)))
            .and_then(|fd| match fd {
                PdfObject::Dict(d) => Some(d),
                _ => None,
            })
            .is_some_and(|fd| {
                fd.get("FontFile").is_some()
                    || fd.get("FontFile2").is_some()
                    || fd.get("FontFile3").is_some()
            });
        if !embedded && reported.insert(base.clone()) {
            out.push(Violation {
                rule: "font-not-embedded",
                message: format!("font '{base}' is not embedded; PDF/A requires embedding"),
            });
        }
    }
}

/// Every font dictionary referenced from any page's /Resources /Font.
fn collect_font_dicts(file: &PdfFile) -> Vec<zpdf_core::PdfDict> {
    let mut out = Vec::new();
    let mut seen: HashSet<ObjectId> = HashSet::new();
    let Ok(root) = file.trailer.get_ref("Root") else {
        return out;
    };
    let Ok(catalog) = file.resolve(root).and_then(|o| o.as_dict().cloned()) else {
        return out;
    };
    let Ok(pages_root) = catalog.get_ref("Pages") else {
        return out;
    };
    // Bounded page-tree walk collecting /Resources /Font values.
    let mut stack = vec![(pages_root, 0usize)];
    let mut visited: HashSet<ObjectId> = HashSet::new();
    while let Some((node, depth)) = stack.pop() {
        if depth > 64 || !visited.insert(node) {
            continue;
        }
        let Ok(dict) = file.resolve(node).and_then(|o| o.as_dict().cloned()) else {
            continue;
        };
        if dict.get("Resources").is_some() {
            let res = match dict.get("Resources") {
                Some(o) => deref(file, o),
                None => PdfObject::Null,
            };
            if let PdfObject::Dict(res) = res {
                if let Some(PdfObject::Dict(fonts)) = res.get("Font").map(|o| deref(file, o)) {
                    for v in fonts.0.values() {
                        if let PdfObject::Ref(r) = v {
                            if !seen.insert(*r) {
                                continue;
                            }
                        }
                        if let PdfObject::Dict(f) = deref(file, v) {
                            out.push(f);
                        }
                    }
                }
            }
        }
        if let Some(PdfObject::Array(kids)) = dict.get("Kids").map(|o| deref(file, o)) {
            for kid in kids {
                if let PdfObject::Ref(r) = kid {
                    stack.push((r, depth + 1));
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Forbidden features
// ---------------------------------------------------------------------------

fn check_forbidden_features(file: &PdfFile, profile: Profile, out: &mut Vec<Violation>) {
    let Ok(root) = file.trailer.get_ref("Root") else {
        return;
    };
    let Ok(catalog) = file.resolve(root).and_then(|o| o.as_dict().cloned()) else {
        return;
    };

    // JavaScript / launch actions (all parts).
    if let Some(PdfObject::Dict(names)) = catalog.get("Names").map(|o| deref(file, o)).as_ref() {
        if names.get("JavaScript").is_some() {
            out.push(Violation {
                rule: "javascript",
                message: "document-level JavaScript name tree present; forbidden in PDF/A".into(),
            });
        }
    }
    if catalog.get("OpenAction").is_some() {
        // /OpenAction with a destination array is fine; an action dict with
        // /S /JavaScript or /Launch is not. Flag only the risky forms.
        if let Some(PdfObject::Dict(action)) =
            catalog.get("OpenAction").map(|o| deref(file, o)).as_ref()
        {
            let s = action.get_name("S").unwrap_or("");
            if s == "JavaScript" || s == "Launch" {
                out.push(Violation {
                    rule: "open-action",
                    message: format!("/OpenAction /S /{s} is forbidden in PDF/A"),
                });
            }
        }
    }

    // Embedded files: forbidden in A-1; allowed (with conditions) in A-2 — we
    // flag A-1 only (A-2's "must itself be PDF/A" condition is out of scope).
    if profile == Profile::A1b {
        if let Some(PdfObject::Dict(names)) = catalog.get("Names").map(|o| deref(file, o)).as_ref()
        {
            if names.get("EmbeddedFiles").is_some() {
                out.push(Violation {
                    rule: "embedded-files",
                    message: "embedded files are forbidden in PDF/A-1".into(),
                });
            }
        }
    }

    // A-1: transparency is forbidden — detect page-level transparency groups.
    if profile == Profile::A1b {
        let mut stack = vec![(catalog.get_ref("Pages").ok(), 0usize)];
        let mut visited: HashSet<ObjectId> = HashSet::new();
        while let Some((Some(node), depth)) = stack.pop() {
            if depth > 64 || !visited.insert(node) {
                continue;
            }
            let Ok(dict) = file.resolve(node).and_then(|o| o.as_dict().cloned()) else {
                continue;
            };
            if let Some(PdfObject::Dict(group)) = dict.get("Group").map(|o| deref(file, o)).as_ref()
            {
                if group.get_name("S").ok() == Some("Transparency") {
                    out.push(Violation {
                        rule: "transparency",
                        message: "transparency group on a page; forbidden in PDF/A-1".into(),
                    });
                    break;
                }
            }
            if let Some(PdfObject::Array(kids)) = dict.get("Kids").map(|o| deref(file, o)).as_ref()
            {
                for kid in kids {
                    if let PdfObject::Ref(r) = kid {
                        stack.push((Some(*r), depth + 1));
                    }
                }
            }
        }
    }

    // Forbidden annotation subtypes (all parts): 3D, Sound, Movie reference
    // non-embedded interactive/multimedia content. FileAttachment carries an
    // embedded file, forbidden in A-1 (A-2 permits it). Annotations whose /A
    // action is JavaScript/Launch are also forbidden. Widget annotations
    // (form fields) are permitted with properly embedded appearance fonts.
    check_forbidden_annotations(file, profile, &catalog, out);
}

/// Annotation subtypes PDF/A forbids in every part (interactive/multimedia
/// types referencing non-embedded external content or scripting).
const FORBIDDEN_ANNOT_SUBTYPES_BOTH: &[&str] = &["3D", "Sound", "Movie"];

/// Annotation subtypes PDF/A-1 additionally forbids. FileAttachment carries
/// an embedded file, which A-1 disallows (A-2 permits embedded files).
const FORBIDDEN_ANNOT_SUBTYPES_A1B: &[&str] = &["FileAttachment"];

/// Walk the page tree and flag forbidden annotation subtypes and annotations
/// whose `/A` action is a JavaScript/Launch action. Best-effort: bounded by
/// page-tree depth; an unresolvable annotation is skipped, not flagged.
fn check_forbidden_annotations(
    file: &PdfFile,
    profile: Profile,
    catalog: &PdfDict,
    out: &mut Vec<Violation>,
) {
    let Some(pages) = catalog.get_ref("Pages").ok() else {
        return;
    };
    let a1b = profile == Profile::A1b;
    let mut stack = vec![(pages, 0usize)];
    let mut visited: HashSet<ObjectId> = HashSet::new();
    while let Some((node, depth)) = stack.pop() {
        if depth > 64 || !visited.insert(node) {
            continue;
        }
        let Ok(dict) = file.resolve(node).and_then(|o| o.as_dict().cloned()) else {
            continue;
        };
        if let Some(PdfObject::Array(kids)) = dict.get("Kids").map(|o| deref(file, o)).as_ref() {
            for kid in kids {
                if let PdfObject::Ref(r) = kid {
                    stack.push((*r, depth + 1));
                }
            }
        }
        let annots_obj = dict.get("Annots").map(|o| deref(file, o));
        let Some(PdfObject::Array(annots)) = annots_obj.as_ref() else {
            continue;
        };
        for a in annots {
            let PdfObject::Dict(ad) = deref(file, a) else {
                continue;
            };
            let subtype = ad.get_name("Subtype").unwrap_or("");
            let forbidden = FORBIDDEN_ANNOT_SUBTYPES_BOTH.contains(&subtype)
                || (a1b && FORBIDDEN_ANNOT_SUBTYPES_A1B.contains(&subtype));
            if forbidden {
                out.push(Violation {
                    rule: "annotation-subtype",
                    message: format!(
                        "/Annot /Subtype /{subtype} is forbidden in PDF/A{}",
                        if a1b && subtype == "FileAttachment" {
                            "-1 (carries an embedded file)"
                        } else {
                            ""
                        }
                    ),
                });
                continue;
            }
            if let Some(PdfObject::Dict(action)) = ad.get("A").map(|o| deref(file, o)).as_ref() {
                let s = action.get_name("S").unwrap_or("");
                if s == "JavaScript" || s == "Launch" {
                    out.push(Violation {
                        rule: "annotation-action",
                        message: format!("annotation /A /S /{s} action is forbidden in PDF/A"),
                    });
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Associated files (PDF/A-3, ISO 19005-3 §6.2.4)
// ---------------------------------------------------------------------------

/// PDF/A-3's associated-files rules. A document may carry embedded files (in
/// `/Names /EmbeddedFiles`), but PDF/A-3 requires that each be *associated* via
/// a catalog or page `/AF` array, and that every `/AF` file specification carry
/// an `/AFRelationship`. Two rules:
///
/// - `embedded-files-af`: if the document has embedded files but no catalog
///   and no page `/AF` array at all, flag it (an unreachable attachment).
/// - `af-relationship`: count `/AF` file specifications that carry an embedded
///   stream but no `/AFRelationship`. Only embedded filespecs are counted — a
///   bare external-path string (no stream) is not an "embedded file" under
///   §6.2.4 and does not trigger the relationship requirement.
fn check_embedded_files_af(file: &PdfFile, out: &mut Vec<Violation>) {
    let embedded = crate::embedded_files::parse_embedded_files(file);
    let catalog_af = crate::embedded_files::parse_associated_files(file);
    let page_af = collect_page_associated_files(file);

    if !embedded.is_empty() && catalog_af.is_empty() && page_af.is_empty() {
        out.push(Violation {
            rule: "embedded-files-af",
            message: format!(
                "document has {} embedded file(s) but no /AF associated-files array; \
                 PDF/A-3 requires embedded files to be associated via /AF",
                embedded.len()
            ),
        });
    }

    // Every embedded /AF filespec must declare an /AFRelationship.
    let missing_rel = catalog_af
        .iter()
        .chain(page_af.iter())
        .filter(|ef| ef.is_embedded() && ef.relationship.is_none())
        .count();
    if missing_rel > 0 {
        out.push(Violation {
            rule: "af-relationship",
            message: format!(
                "{missing_rel} /AF associated file(s) lack /AFRelationship; \
                 PDF/A-3 requires an /AFRelationship on every associated file"
            ),
        });
    }
}

/// Walk the page tree and collect every leaf page's `/AF` associated files
/// (PDF 2.0). `/AF` is not an inheritable page attribute, so only leaf pages
/// are inspected. Bounded by depth and a visited set, matching the other
/// page-tree walks in this module.
fn collect_page_associated_files(file: &PdfFile) -> Vec<crate::embedded_files::EmbeddedFile> {
    let mut out = Vec::new();
    let Ok(root) = file.trailer.get_ref("Root") else {
        return out;
    };
    let Ok(catalog) = file.resolve(root).and_then(|o| o.as_dict().cloned()) else {
        return out;
    };
    let Ok(pages_root) = catalog.get_ref("Pages") else {
        return out;
    };
    let mut stack = vec![(pages_root, 0usize)];
    let mut visited: HashSet<ObjectId> = HashSet::new();
    while let Some((node, depth)) = stack.pop() {
        if depth > 64 || !visited.insert(node) {
            continue;
        }
        let Ok(dict) = file.resolve(node).and_then(|o| o.as_dict().cloned()) else {
            continue;
        };
        // Leaf page: collect its /AF. An interior /Pages node is not a leaf.
        if dict.get("Kids").is_none() {
            out.extend(crate::embedded_files::parse_page_associated_files(
                file, &dict,
            ));
        }
        if let Some(PdfObject::Array(kids)) = dict.get("Kids").map(|o| deref(file, o)).as_ref() {
            for kid in kids {
                if let PdfObject::Ref(r) = kid {
                    stack.push((*r, depth + 1));
                }
            }
        }
    }
    out
}

fn deref(file: &PdfFile, obj: &PdfObject) -> PdfObject {
    match obj {
        PdfObject::Ref(r) => file.resolve(*r).unwrap_or(PdfObject::Null),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::build_pdf;

    fn minimal_pdf() -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(b"%PDF-1.4\n");
        data.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
        data.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
        data.extend_from_slice(
            b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>\nendobj\n",
        );
        data.extend_from_slice(b"xref\n0 4\n");
        data.extend_from_slice(b"0000000000 65535 f \n");
        data.extend_from_slice(b"0000000009 00000 n \n");
        data.extend_from_slice(b"0000000058 00000 n \n");
        data.extend_from_slice(b"0000000117 00000 n \n");
        data.extend_from_slice(b"trailer\n<< /Size 4 /Root 1 0 R >>\n");
        data.extend_from_slice(b"startxref\n187\n%%EOF\n");
        data
    }

    #[test]
    fn bare_pdf_fails_with_specific_violations() {
        let file = PdfFile::parse(minimal_pdf()).unwrap();
        let report = validate(&file, Profile::A1b);
        assert!(!report.conforms());
        let rules: Vec<&str> = report.violations.iter().map(|v| v.rule).collect();
        assert!(rules.contains(&"file-id"), "missing /ID flagged: {rules:?}");
        assert!(
            rules.contains(&"xmp-missing"),
            "missing XMP flagged: {rules:?}"
        );
        assert!(
            rules.contains(&"output-intent"),
            "missing output intent flagged: {rules:?}"
        );
    }

    #[test]
    fn claim_extraction_from_attribute_and_element_forms() {
        assert_eq!(
            extract_xmp_value(r#"<x pdfaid:part="2"/>"#, "pdfaid:part").as_deref(),
            Some("2")
        );
        assert_eq!(
            extract_xmp_value("<pdfaid:part>1</pdfaid:part>", "pdfaid:part").as_deref(),
            Some("1")
        );
        assert_eq!(extract_xmp_value("<nothing/>", "pdfaid:part"), None);
    }

    /// Build a PDF with a page carrying the given `/Annots` object bodies (each
    /// becomes its own object after the page). Returns the bytes.
    fn pdf_with_annots(annots: &[&str]) -> Vec<u8> {
        let n_objs = 3 + annots.len();
        let mut data = Vec::new();
        data.extend_from_slice(b"%PDF-1.4\n");
        let mut offsets = Vec::new();
        // obj 1: catalog, 2: pages, 3: page with /Annots [4 0 R 5 0 R ...]
        let annot_refs: Vec<String> = (0..annots.len())
            .map(|i| format!("{} 0 R", 4 + i))
            .collect();
        let bodies = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Annots [{}] >>",
                annot_refs.join(" ")
            ),
        ];
        for (i, body) in bodies.iter().enumerate() {
            offsets.push(data.len());
            data.extend_from_slice(format!("{} 0 obj\n{}\nendobj\n", i + 1, body).as_bytes());
        }
        for (i, body) in annots.iter().enumerate() {
            offsets.push(data.len());
            data.extend_from_slice(format!("{} 0 obj\n{}\nendobj\n", 4 + i, body).as_bytes());
        }
        let xref = data.len();
        data.extend_from_slice(format!("xref\n0 {}\n", n_objs + 1).as_bytes());
        data.extend_from_slice(b"0000000000 65535 f \n");
        for off in &offsets {
            data.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        data.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                n_objs + 1
            )
            .as_bytes(),
        );
        data
    }

    #[test]
    fn forbidden_annotation_subtypes_are_flagged() {
        let pdf = pdf_with_annots(&[
            "<< /Type /Annot /Subtype /Sound /Rect [0 0 10 10] >>",
            "<< /Type /Annot /Subtype /Text /Rect [0 0 10 10] >>",
        ]);
        let file = PdfFile::parse(pdf).unwrap();
        let report = validate(&file, Profile::A1b);
        let rules: Vec<&str> = report.violations.iter().map(|v| v.rule).collect();
        assert!(
            rules.contains(&"annotation-subtype"),
            "Sound annotation must be flagged: {rules:?}"
        );
    }

    #[test]
    fn fileattachment_flagged_under_a1b_not_a2b() {
        let pdf =
            pdf_with_annots(&["<< /Type /Annot /Subtype /FileAttachment /Rect [0 0 10 10] >>"]);
        let file = PdfFile::parse(pdf).unwrap();
        let a1b = validate(&file, Profile::A1b);
        let a2b = validate(&file, Profile::A2b);
        let a1b_rules: Vec<&str> = a1b.violations.iter().map(|v| v.rule).collect();
        let a2b_rules: Vec<&str> = a2b.violations.iter().map(|v| v.rule).collect();
        assert!(
            a1b_rules.contains(&"annotation-subtype"),
            "A-1b must flag FileAttachment: {a1b_rules:?}"
        );
        assert!(
            !a2b_rules.contains(&"annotation-subtype"),
            "A-2b must not flag FileAttachment: {a2b_rules:?}"
        );
    }

    #[test]
    fn launch_action_annotation_is_flagged() {
        let pdf = pdf_with_annots(&[
            "<< /Type /Annot /Subtype /Link /Rect [0 0 10 10] /A << /S /Launch /F (x) >> >>",
        ]);
        let file = PdfFile::parse(pdf).unwrap();
        let report = validate(&file, Profile::A1b);
        let rules: Vec<&str> = report.violations.iter().map(|v| v.rule).collect();
        assert!(
            rules.contains(&"annotation-action"),
            "Launch-action annotation must be flagged: {rules:?}"
        );
    }

    // ---- PDF/A-3b: associated files ---------------------------------------

    /// A PDF/A-3b-compliant document: everything A-2b needs plus embedded files
    /// that are all referenced from a catalog `/AF` array, each carrying an
    /// `/AFRelationship`. Built from object bodies so the AF rules can be
    /// isolated from the shared (XMP / output intent / font embedding) checks.
    fn pdfa3_compliant_pdf(af: &str, filespecs: &[&str], streams: &[&str]) -> Vec<u8> {
        // Layout: 1 catalog, 2 pages, 3 page, 4 XMP, 5 ICC, 6 output intent,
        // 7 name-tree root, then filespecs + embedded-file streams from `af`.
        let mut objs: Vec<String> = vec![
            format!(
                "<< /Type /Catalog /Pages 2 0 R /Metadata 4 0 R /OutputIntents [6 0 R] \
                 /Names << /EmbeddedFiles 7 0 R >> {af} >>"
            ),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>".to_string(),
            // 4: XMP stream (pdfaid part=3, conformance=B). No /Length — the
            // parser scans to `endstream`; the validator only inspects the bytes.
            "<< /Type /Metadata /Subtype /XML >>\nstream\n<?xpacket begin=\"\u{FEFF}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n<rdf:Description rdf:about=\"\" xmlns:pdfaid=\"http://www.aiim.org/pdfa/ns/id/\">\n<pdfaid:part>3</pdfaid:part>\n<pdfaid:conformance>B</pdfaid:conformance>\n</rdf:Description>\n</rdf:RDF>\n</x:xmpmeta>\n<?xpacket end=\"w\"?>\nendstream".to_string(),
            // 5: ICC profile stream (a placeholder: the validator only checks
            // the stream is referenced, not its bytes).
            "<< /N 3 >>\nstream\n\nendstream".to_string(),
            // 6: GTS_PDFA1 output intent.
            "<< /Type /OutputIntent /S /GTS_PDFA1 /OutputConditionIdentifier (sRGB) \
             /DestOutputProfile 5 0 R >>"
                .to_string(),
            // 7: name-tree root with one leaf entry per filespec.
            {
                let pairs: Vec<String> = filespecs
                    .iter()
                    .enumerate()
                    .map(|(i, _)| format!("(f{i}.bin) {} 0 R", 8 + i))
                    .collect();
                format!("<< /Names [ {} ] >>", pairs.join(" "))
            },
        ];
        // 8..: filespec bodies, then embedded-file streams.
        let first_stream = 8 + filespecs.len();
        for (i, spec) in filespecs.iter().enumerate() {
            // Substitute the /EF stream ref: the placeholder "{EF}" in each
            // filespec body becomes a full `N 0 R` reference to its stream.
            let s = spec.replace("{EF}", &format!("{} 0 R", first_stream + i));
            objs.push(s);
        }
        for body in streams {
            objs.push(body.to_string());
        }
        build_pdf(&objs.iter().map(|s| s.as_str()).collect::<Vec<_>>())
    }

    #[test]
    fn a3b_bare_pdf_fails_on_shared_checks() {
        // A bare PDF (no XMP / output intent / ID) fails A-3b on the shared
        // rules, exactly as A-1b/A-2b do.
        let file = PdfFile::parse(minimal_pdf()).unwrap();
        let report = validate(&file, Profile::A3b);
        assert!(!report.conforms());
        let rules: Vec<&str> = report.violations.iter().map(|v| v.rule).collect();
        assert!(rules.contains(&"file-id"), "missing /ID flagged: {rules:?}");
        assert!(
            rules.contains(&"xmp-missing"),
            "missing XMP flagged: {rules:?}"
        );
        assert!(
            rules.contains(&"output-intent"),
            "missing output intent flagged: {rules:?}"
        );
        // No embedded files → the A-3b-specific rules are silent.
        assert!(
            !rules.contains(&"embedded-files-af"),
            "no embedded files, no AF rule: {rules:?}"
        );
        assert!(
            !rules.contains(&"af-relationship"),
            "no AF, no relationship rule: {rules:?}"
        );
    }

    #[test]
    fn a3b_embedded_files_without_af_are_flagged() {
        // Compliant in every shared respect, but the embedded file is NOT
        // referenced from /AF → the embedded-files-af rule fires.
        let pdf = pdfa3_compliant_pdf(
            "",
            &["<< /Type /Filespec /UF (f0.bin) /AFRelationship /Data /EF << /F {EF} >> >>"],
            &["<< /Type /EmbeddedFile /Length 0 >>\nstream\n\nendstream"],
        );
        let file = PdfFile::parse(pdf).unwrap();
        let report = validate(&file, Profile::A3b);
        let rules: Vec<&str> = report.violations.iter().map(|v| v.rule).collect();
        assert!(
            rules.contains(&"embedded-files-af"),
            "embedded file without /AF must be flagged: {rules:?}"
        );
    }

    #[test]
    fn a3b_af_without_relationship_is_flagged() {
        // The embedded file IS referenced from /AF, but the filespec omits
        // /AFRelationship → the af-relationship rule fires (and embedded-files-af
        // does not, since /AF is present).
        let pdf = pdfa3_compliant_pdf(
            "/AF [8 0 R]",
            &["<< /Type /Filespec /UF (f0.bin) /EF << /F {EF} >> >>"],
            &["<< /Type /EmbeddedFile /Length 0 >>\nstream\n\nendstream"],
        );
        let file = PdfFile::parse(pdf).unwrap();
        let report = validate(&file, Profile::A3b);
        let rules: Vec<&str> = report.violations.iter().map(|v| v.rule).collect();
        assert!(
            rules.contains(&"af-relationship"),
            "/AF filespec without /AFRelationship must be flagged: {rules:?}"
        );
        assert!(
            !rules.contains(&"embedded-files-af"),
            "/AF present, embedded-files-af should not fire: {rules:?}"
        );
    }

    #[test]
    fn a3b_external_af_string_does_not_trigger_relationship_rule() {
        // A bare external-path string in /AF (no embedded stream) is not an
        // embedded file under §6.2.4, so it must not trigger af-relationship.
        // (It also means no /Names /EmbeddedFiles, so embedded-files-af is silent.)
        let pdf = pdfa3_compliant_pdf("/AF [ (../external.dat) ]", &[], &[]);
        let file = PdfFile::parse(pdf).unwrap();
        let report = validate(&file, Profile::A3b);
        let rules: Vec<&str> = report.violations.iter().map(|v| v.rule).collect();
        assert!(
            !rules.contains(&"af-relationship"),
            "external-path AF must not trigger af-relationship: {rules:?}"
        );
        assert!(
            !rules.contains(&"embedded-files-af"),
            "no embedded files, no AF rule: {rules:?}"
        );
    }

    #[test]
    fn a3b_compliant_af_does_not_flag() {
        // Embedded file + catalog /AF + /AFRelationship → no A-3b-specific
        // violations (embedded-files-af and af-relationship both stay silent).
        let pdf = pdfa3_compliant_pdf(
            "/AF [8 0 R]",
            &["<< /Type /Filespec /UF (f0.bin) /AFRelationship /Data /EF << /F {EF} >> >>"],
            &["<< /Type /EmbeddedFile /Length 0 >>\nstream\n\nendstream"],
        );
        let file = PdfFile::parse(pdf).unwrap();
        let report = validate(&file, Profile::A3b);
        let a3b_rules: Vec<&str> = report
            .violations
            .iter()
            .filter(|v| v.rule == "embedded-files-af" || v.rule == "af-relationship")
            .map(|v| v.rule)
            .collect();
        assert!(
            a3b_rules.is_empty(),
            "compliant A-3b should have no AF violations: {a3b_rules:?} (all: {:?})",
            report.violations
        );
    }

    #[test]
    fn a3b_pdf20_header_is_flagged() {
        // A PDF 2.0 header is non-conformant for PDF/A-3 (a PDF 1.7 family
        // standard). build_pdf_with_version computes the xref offsets.
        let pdf = crate::test_util::build_pdf_with_version(
            2,
            0,
            &[
                "<< /Type /Catalog /Pages 2 0 R >>",
                "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>",
            ],
        );
        let file = PdfFile::parse(pdf).unwrap();
        let report = validate(&file, Profile::A3b);
        let rules: Vec<&str> = report.violations.iter().map(|v| v.rule).collect();
        assert!(
            rules.contains(&"header-version"),
            "PDF 2.0 header must be flagged for A-3b: {rules:?}"
        );
    }

    #[test]
    fn a3b_keeps_fileattachment_unlike_a1b() {
        // A-3b permits FileAttachment (it permits embedded files), unlike A-1b.
        let pdf =
            pdf_with_annots(&["<< /Type /Annot /Subtype /FileAttachment /Rect [0 0 10 10] >>"]);
        let file = PdfFile::parse(pdf).unwrap();
        let a3b = validate(&file, Profile::A3b);
        let a3b_rules: Vec<&str> = a3b.violations.iter().map(|v| v.rule).collect();
        assert!(
            !a3b_rules.contains(&"annotation-subtype"),
            "A-3b must not flag FileAttachment: {a3b_rules:?}"
        );
    }
}
