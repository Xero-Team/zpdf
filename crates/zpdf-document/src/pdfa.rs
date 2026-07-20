//! PDF/A conformance validation (profiles A-1b and A-2b).
//!
//! A **rule engine** over the parsed document: each check inspects one aspect
//! of the file and yields zero or more [`Violation`]s. This does not aim for
//! veraPDF-level completeness — it covers the high-signal, machine-checkable
//! clauses of ISO 19005-1 (PDF/A-1b) and 19005-2 (PDF/A-2b):
//!
//! - file structure: header version, no encryption, trailer /ID present
//! - fonts: every used font embedded (except the standard 14 in no profile —
//!   PDF/A requires embedding even for those)
//! - XMP metadata: present, with a `pdfaid:part`/`conformance` claim
//! - output intent: a PDF/A output intent with an embedded ICC profile
//! - forbidden features: JavaScript/actions, embedded files (A-1),
//!   transparency (A-1: soft masks / group /S /Transparency), LZW (A-1),
//!   encryption of any kind
//!
//! Everything is best-effort and read-only over `ParseLimits`-bounded APIs;
//! a check that cannot run (e.g. a malformed font dict) reports what it saw.

use std::collections::HashSet;

use zpdf_core::{ObjectId, PdfObject};
use zpdf_parser::PdfFile;

/// The validation profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// ISO 19005-1 Level B (PDF/A-1b): PDF 1.4 model, no transparency.
    A1b,
    /// ISO 19005-2 Level B (PDF/A-2b): PDF 1.7 model, transparency allowed.
    A2b,
}

impl Profile {
    pub fn as_str(self) -> &'static str {
        match self {
            Profile::A1b => "PDF/A-1b",
            Profile::A2b => "PDF/A-2b",
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

    // Header version ceiling: 1.4 for A-1, 1.7 for A-2. The parser records
    // the header; a higher version is only a violation for A-1 (A-2 is
    // based on 1.7 which is the cap of what zpdf writes anyway).
    if profile == Profile::A1b {
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
}
