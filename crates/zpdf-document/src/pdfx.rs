//! PDF/X conformance validation (profiles X-1a, X-3, X-4, and X-6).
//!
//! A **rule engine** over the parsed document, mirroring [`crate::pdfa`]: each
//! check inspects one aspect of the file and yields zero or more [`Violation`]s.
//! This does not aim for veraPDF-level completeness — it covers the high-signal,
//! machine-checkable clauses of ISO 15930 (the PDF/X family):
//!
//! - file structure: header version (X-1a/X-3 → PDF 1.3, X-4 → 1.4–1.7, X-6 →
//!   2.0), no encryption, trailer `/ID` present
//! - output intent: a `/GTS_PDFX` output intent with an embedded **CMYK**
//!   (`/N` = 4) ICC profile (all four profiles)
//! - fonts: every used font embedded (PDF/X, like PDF/A, exempts none — not even
//!   the standard 14)
//! - page boxes: every page has a `/TrimBox` or `/ArtBox`, and the boxes nest:
//!   `MediaBox ⊇ BleedBox ⊇ TrimBox ⊇ ArtBox`
//! - colour: PDF/X-1a forbids RGB of any kind (`DeviceRGB`/`CalRGB`/`CalGray`/
//!   `ICCBased` N=3); X-3/X-4/X-6 forbid uncalibrated `DeviceRGB`
//! - forbidden features: JavaScript/Launch actions, transparency groups
//!   (X-1a/X-3), forbidden annotation subtypes (`3D`/`Sound`/`Movie`/
//!   `FileAttachment`), and annotations that don't print (X-1a/X-3)
//! - `/Trapped`: X-1a/X-3 require `/True`/`/False`; X-4 disallows `/Unknown`
//!
//! Everything is best-effort and read-only over `ParseLimits`-bounded APIs. Two
//! known gaps, consistent with the "best-effort" framing: only *named* colour
//! spaces in page `/Resources /ColorSpace` are inspected — direct `DeviceRGB`
//! set via the `rg`/`cs` content operators, and image palettes, are not; and a
//! page whose effective `/MediaBox` fell back to the default (US Letter) still
//! has its box containment checked against that fallback.

use std::collections::HashSet;

use zpdf_core::{ObjectId, PdfDict, PdfObject, Rect};
use zpdf_parser::PdfFile;

use crate::page::{is_usable_box, resolve_rect, PdfPage};

/// The validation profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// ISO 15930-1 (PDF/X-1a): PDF 1.3, CMYK/Gray/spot only (no RGB of any
    /// kind), no transparency, no layers.
    X1a,
    /// ISO 15930-3 (PDF/X-3): PDF 1.3, CMYK plus calibrated (`CalRGB`/`CalGray`/
    /// `ICCBased`) colour, no transparency.
    X3,
    /// ISO 15930-4 (PDF/X-4): PDF 1.4–1.7, CMYK plus calibrated colour,
    /// transparency and layers allowed.
    X4,
    /// ISO 15930-7 (PDF/X-6): PDF 2.0-based PDF/X.
    X6,
}

impl Profile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::X1a => "PDF/X-1a",
            Self::X3 => "PDF/X-3",
            Self::X4 => "PDF/X-4",
            Self::X6 => "PDF/X-6",
        }
    }
}

/// One conformance violation.
#[derive(Debug, Clone)]
pub struct Violation {
    /// Short rule identifier, e.g. `"output-intent"`, `"font-not-embedded"`.
    pub rule: &'static str,
    /// Human-readable explanation with the offending object where known.
    pub message: String,
}

/// The outcome of a validation run.
#[derive(Debug)]
pub struct ValidationReport {
    pub profile: Profile,
    pub violations: Vec<Violation>,
    /// The `pdfxid:GTS_PDFXVersion` (or legacy `pdfx:GTS_PDFXVersion`) the
    /// document itself claims via XMP, e.g. `Some("X-4")` — independent of
    /// whether it conforms.
    pub claimed: Option<String>,
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
    check_page_boxes(file, &mut v);
    check_color(file, profile, &mut v);
    check_forbidden_features(file, profile, &mut v);
    check_trapped(file, profile, &mut v);

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
    if file.is_encrypted() {
        out.push(Violation {
            rule: "encryption",
            message: "document is encrypted (/Encrypt present); PDF/X forbids encryption".into(),
        });
    }

    match file.trailer.get("ID") {
        Some(PdfObject::Array(a)) if a.len() == 2 => {}
        _ => out.push(Violation {
            rule: "file-id",
            message: "trailer /ID missing or not a two-element array".into(),
        }),
    }

    let h = file.header;
    let ok = match profile {
        Profile::X1a | Profile::X3 => h.major == 1 && h.minor <= 3,
        Profile::X4 => h.major == 1 && (4..=7).contains(&h.minor),
        Profile::X6 => h.major == 2,
    };
    if !ok {
        let want = match profile {
            Profile::X1a | Profile::X3 => "PDF 1.3",
            Profile::X4 => "PDF 1.4–1.7",
            Profile::X6 => "PDF 2.0",
        };
        out.push(Violation {
            rule: "header-version",
            message: format!(
                "header declares PDF {}.{}; {want} is required for {}",
                h.major,
                h.minor,
                profile.as_str()
            ),
        });
    }
}

// ---------------------------------------------------------------------------
// XMP metadata
// ---------------------------------------------------------------------------

fn check_xmp(file: &PdfFile, out: &mut Vec<Violation>) {
    if crate::xmp::metadata_bytes(file).is_none() {
        out.push(Violation {
            rule: "xmp-missing",
            message: "catalog has no /Metadata XMP stream; PDF/X-4/X-6 require XMP metadata".into(),
        });
    }
    // A present-but-pdfxid-less packet is not flagged: older X-1a/X-3 files
    // identify via the output intent's /Info rather than XMP, so a missing
    // pdfxid claim is advisory, not a hard violation.
}

/// The `GTS_PDFXVersion` the XMP claims, when parseable. The modern
/// `pdfxid:` namespace (X-4/X-6) is tried first; the older Adobe `pdfx:`
/// namespace (X-1a/X-3) is the fallback.
fn xmp_claim(file: &PdfFile) -> Option<String> {
    let xml = crate::xmp::metadata_bytes(file)?;
    let text = String::from_utf8_lossy(&xml);
    extract_xmp_value(&text, "pdfxid:GTS_PDFXVersion")
        .or_else(|| extract_xmp_value(&text, "pdfx:GTS_PDFXVersion"))
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
    let pdfx = intents.iter().find(|i| i.subtype == "GTS_PDFX");
    match pdfx {
        None => out.push(Violation {
            rule: "output-intent",
            message: "no /GTS_PDFX output intent; PDF/X requires one for the characterized \
                      printing condition"
                .into(),
        }),
        Some(intent) => {
            if intent.dest_output_profile.is_none() {
                out.push(Violation {
                    rule: "output-intent-profile",
                    message: "PDF/X output intent has no embedded /DestOutputProfile ICC stream"
                        .into(),
                });
            } else if !intent.has_cmyk_profile() {
                out.push(Violation {
                    rule: "output-intent-cmyk",
                    message: "PDF/X output intent profile is not CMYK (/DestOutputProfile /N \
                      must be 4)"
                        .into(),
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Fonts (embedded)
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
                message: format!("font '{base}' is not embedded; PDF/X requires embedding"),
            });
        }
    }
}

/// Every font dictionary referenced from any page's /Resources /Font.
fn collect_font_dicts(file: &PdfFile) -> Vec<PdfDict> {
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
// Page boxes (TrimBox/ArtBox + containment)
// ---------------------------------------------------------------------------

fn check_page_boxes(file: &PdfFile, out: &mut Vec<Violation>) {
    let mut missing_trim: Vec<ObjectId> = Vec::new();
    let mut containment: Vec<String> = Vec::new();
    for (leaf_id, dict) in collect_leaf_pages(file) {
        // MediaBox is inheritable; resolve the effective box via the page
        // model (which walks /Parent and falls back to US Letter). TrimBox/
        // BleedBox/ArtBox are NOT inheritable — read them off the leaf dict.
        let media = match PdfPage::from_object(file, leaf_id) {
            Ok(p) => p.media_box,
            Err(_) => match resolve_rect(file, &dict, "MediaBox") {
                Some(m) => m,
                None => continue, // no MediaBox at all; cannot verify containment
            },
        };

        let trim = resolve_rect(file, &dict, "TrimBox").filter(is_usable_box);
        let bleed = resolve_rect(file, &dict, "BleedBox").filter(is_usable_box);
        let art = resolve_rect(file, &dict, "ArtBox").filter(is_usable_box);

        if trim.is_none() && art.is_none() {
            missing_trim.push(leaf_id);
        }

        // Containment: MediaBox ⊇ BleedBox ⊇ TrimBox ⊇ ArtBox.
        if let Some(b) = &bleed {
            if !contained(b, &media) {
                containment.push(format!("page {leaf_id}: /BleedBox not within /MediaBox"));
            }
        }
        if let Some(t) = &trim {
            let outer = bleed.as_ref().unwrap_or(&media);
            if !contained(t, outer) {
                containment.push(format!(
                    "page {leaf_id}: /TrimBox not within /BleedBox (or /MediaBox)"
                ));
            }
        }
        if let Some(a) = &art {
            let outer = trim.as_ref().or(bleed.as_ref()).unwrap_or(&media);
            if !contained(a, outer) {
                containment.push(format!(
                    "page {leaf_id}: /ArtBox not within /TrimBox (or /BleedBox, or /MediaBox)"
                ));
            }
        }
    }

    if !missing_trim.is_empty() {
        out.push(Violation {
            rule: "page-trimbox",
            message: summarize_pages(
                &missing_trim,
                "lack /TrimBox or /ArtBox; PDF/X requires a trim or art box on every page",
            ),
        });
    }
    if !containment.is_empty() {
        out.push(Violation {
            rule: "page-box-containment",
            message: summarize_strs(
                &containment,
                "page box not nested (MediaBox ⊇ BleedBox ⊇ TrimBox ⊇ ArtBox)",
            ),
        });
    }
}

/// Render a per-page violation as one message listing up to `MAX_LISTED` object
/// ids, then a count of the rest — keeps `validate` output bounded for
/// documents with many offending pages.
fn summarize_pages(ids: &[ObjectId], suffix: &str) -> String {
    const MAX_LISTED: usize = 8;
    let shown: Vec<String> = ids
        .iter()
        .take(MAX_LISTED)
        .map(|i| format!("{i}"))
        .collect();
    if ids.len() > MAX_LISTED {
        format!(
            "{} page(s) {} (e.g. {} … and {} more)",
            ids.len(),
            suffix,
            shown.join(", "),
            ids.len() - MAX_LISTED
        )
    } else {
        format!("page(s) {} : {}", shown.join(", "), suffix)
    }
}

/// Like [`summarize_pages`] but for pre-formatted per-page strings.
fn summarize_strs(items: &[String], summary: &str) -> String {
    const MAX_LISTED: usize = 8;
    let shown: Vec<&str> = items.iter().take(MAX_LISTED).map(|s| s.as_str()).collect();
    if items.len() > MAX_LISTED {
        format!(
            "{} page(s) {summary} (e.g. {} … and {} more)",
            items.len(),
            shown.join("; "),
            items.len() - MAX_LISTED
        )
    } else {
        format!("{} — {summary}", shown.join("; "))
    }
}

/// `inner` is contained in `outer` (both normalized) within a small tolerance.
fn contained(inner: &Rect, outer: &Rect) -> bool {
    let i = inner.normalize();
    let o = outer.normalize();
    const EPS: f64 = 1e-6;
    o.x0 <= i.x0 + EPS && o.y0 <= i.y0 + EPS && i.x1 <= o.x1 + EPS && i.y1 <= o.y1 + EPS
}

/// Every leaf page (a node with no `/Kids`) as `(object id, leaf dict)`.
/// Bounded by depth and a visited set, matching the other page-tree walks.
fn collect_leaf_pages(file: &PdfFile) -> Vec<(ObjectId, PdfDict)> {
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
        if let Some(PdfObject::Array(kids)) = dict.get("Kids").map(|o| deref(file, o)).as_ref() {
            for kid in kids {
                if let PdfObject::Ref(r) = kid {
                    stack.push((*r, depth + 1));
                }
            }
        } else {
            out.push((node, dict));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Colour spaces
// ---------------------------------------------------------------------------

fn check_color(file: &PdfFile, profile: Profile, out: &mut Vec<Violation>) {
    let mut reported: HashSet<String> = HashSet::new();
    for (name, value) in collect_color_space_entries(file) {
        let Some(tag) = classify_color_space(file, &value) else {
            continue;
        };
        let offends = match profile {
            Profile::X1a => {
                matches!(
                    tag.as_str(),
                    "DeviceRGB" | "CalRGB" | "CalGray" | "ICCBased:3"
                )
            }
            Profile::X3 | Profile::X4 | Profile::X6 => tag == "DeviceRGB",
        };
        if offends && reported.insert(tag.clone()) {
            let rule = if profile == Profile::X1a {
                "color-rgb"
            } else {
                "color-device-rgb"
            };
            out.push(Violation {
                rule,
                message: format!(
                    "colour space '{name}' ({tag}) is forbidden in {}; PDF/X-1a allows only \
                     CMYK/Gray/spot",
                    profile.as_str()
                ),
            });
        }
    }
}

/// Every `(resource name, colour-space value)` pair referenced from any page
/// node's `/Resources /ColorSpace`. Walks all page-tree nodes (Resources is
/// inheritable), dedup is by colour-space tag in the caller.
fn collect_color_space_entries(file: &PdfFile) -> Vec<(String, PdfObject)> {
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
        if let Some(PdfObject::Dict(res)) = dict.get("Resources").map(|o| deref(file, o)) {
            if let Some(PdfObject::Dict(cs)) = res.get("ColorSpace").map(|o| deref(file, o)) {
                for (k, v) in &cs.0 {
                    out.push((k.as_str().to_string(), v.clone()));
                }
            }
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

/// Classify a colour-space resource value to a canonical tag: a bare name
/// (`DeviceRGB`/`DeviceGray`/`DeviceCMYK`/`Pattern`/…), the first element of an
/// array (`CalGray`/`CalRGB`/`Lab`/`ICCBased`/`Separation`/`DeviceN`/`Indexed`/
/// …), with `ICCBased:<N>` carrying the profile's component count. `None` for
/// an unrecognisable value.
fn classify_color_space(file: &PdfFile, value: &PdfObject) -> Option<String> {
    let v = deref(file, value);
    match v {
        PdfObject::Name(n) => Some(n.as_str().to_string()),
        PdfObject::Array(a) => {
            let first = a.first()?;
            let name = match first {
                PdfObject::Name(n) => n.as_str().to_string(),
                PdfObject::Ref(r) => match file.resolve(*r).ok()? {
                    PdfObject::Name(n) => n.as_str().to_string(),
                    _ => return None,
                },
                _ => return None,
            };
            if name == "ICCBased" {
                let n = a
                    .get(1)
                    .and_then(|o| match o {
                        PdfObject::Ref(r) => file.resolve(*r).ok(),
                        _ => None,
                    })
                    .and_then(|o| match o {
                        PdfObject::Stream(s) => s.dict.get_i64("N").ok(),
                        _ => None,
                    });
                Some(match n {
                    Some(k) => format!("ICCBased:{k}"),
                    None => "ICCBased".into(),
                })
            } else {
                Some(name)
            }
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Forbidden features (actions, transparency, annotations)
// ---------------------------------------------------------------------------

fn check_forbidden_features(file: &PdfFile, profile: Profile, out: &mut Vec<Violation>) {
    let Ok(root) = file.trailer.get_ref("Root") else {
        return;
    };
    let Ok(catalog) = file.resolve(root).and_then(|o| o.as_dict().cloned()) else {
        return;
    };

    // JavaScript / launch actions (all profiles).
    if let Some(PdfObject::Dict(names)) = catalog.get("Names").map(|o| deref(file, o)).as_ref() {
        if names.get("JavaScript").is_some() {
            out.push(Violation {
                rule: "javascript",
                message: "document-level JavaScript name tree present; forbidden in PDF/X".into(),
            });
        }
    }
    if let Some(PdfObject::Dict(action)) =
        catalog.get("OpenAction").map(|o| deref(file, o)).as_ref()
    {
        let s = action.get_name("S").unwrap_or("");
        if s == "JavaScript" || s == "Launch" {
            out.push(Violation {
                rule: "open-action",
                message: format!("/OpenAction /S /{s} is forbidden in PDF/X"),
            });
        }
    }

    // X-1a/X-3: transparency groups are forbidden.
    if profile == Profile::X1a || profile == Profile::X3 {
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
                        message: "transparency group on a page; forbidden in PDF/X-1a and X-3"
                            .into(),
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

    check_forbidden_annotations(file, profile, &catalog, out);
}

/// Annotation subtypes PDF/X forbids in every profile: `3D`/`Sound`/`Movie`
/// reference non-embedded interactive/multimedia content; `FileAttachment`
/// carries an embedded file (forbidden in all four PDF/X parts, unlike PDF/A
/// where it is A-1 only).
const FORBIDDEN_ANNOT_SUBTYPES: &[&str] = &["3D", "Sound", "Movie", "FileAttachment"];

/// Walk the page tree and flag forbidden annotation subtypes, annotations
/// whose `/A` action is JavaScript/Launch, and (X-1a/X-3 only) annotations
/// without the Print flag. Best-effort: bounded by page-tree depth; an
/// unresolvable annotation is skipped, not flagged. Widget (form-field)
/// annotations are exempt from the Print-flag check — their visibility is
/// governed by the AcroForm, not the annotation /F.
fn check_forbidden_annotations(
    file: &PdfFile,
    profile: Profile,
    catalog: &PdfDict,
    out: &mut Vec<Violation>,
) {
    let Some(pages) = catalog.get_ref("Pages").ok() else {
        return;
    };
    let check_print = profile == Profile::X1a || profile == Profile::X3;
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
            if FORBIDDEN_ANNOT_SUBTYPES.contains(&subtype) {
                out.push(Violation {
                    rule: "annotation-subtype",
                    message: format!("/Annot /Subtype /{subtype} is forbidden in PDF/X"),
                });
                continue;
            }
            if let Some(PdfObject::Dict(action)) = ad.get("A").map(|o| deref(file, o)).as_ref() {
                let s = action.get_name("S").unwrap_or("");
                if s == "JavaScript" || s == "Launch" {
                    out.push(Violation {
                        rule: "annotation-action",
                        message: format!("annotation /A /S /{s} action is forbidden in PDF/X"),
                    });
                }
            }
            // X-1a/X-3 require every (non-Widget) annotation to print: the
            // Print flag is bit 3 (value 4) — bit 2 (value 2) is Hidden.
            if check_print && subtype != "Widget" {
                let prints = matches!(ad.get("F"), Some(PdfObject::Integer(n)) if (n & 4) != 0);
                if !prints {
                    out.push(Violation {
                        rule: "annotation-print-flag",
                        message: format!(
                            "/Annot /Subtype /{subtype} does not have the Print flag set; \
                             PDF/X-1a/3 require all annotations to print"
                        ),
                    });
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// /Trapped
// ---------------------------------------------------------------------------

fn check_trapped(file: &PdfFile, profile: Profile, out: &mut Vec<Violation>) {
    let trapped = crate::doc_info::parse_info(file).and_then(|i| i.trapped);
    match profile {
        Profile::X1a | Profile::X3 => match trapped.as_deref() {
            Some("True") | Some("False") => {}
            _ => out.push(Violation {
                rule: "trapped",
                message: "/Info /Trapped must be /True or /False for PDF/X-1a and X-3".into(),
            }),
        },
        Profile::X4 => {
            if trapped.as_deref() == Some("Unknown") {
                out.push(Violation {
                    rule: "trapped-unknown",
                    message: "/Info /Trapped /Unknown is not permitted in PDF/X-4 (use /True or \
                      /False, or omit the key)"
                        .into(),
                });
            }
        }
        Profile::X6 => {} // /Trapped is deprecated in PDF 2.0.
    }
}

// ---------------------------------------------------------------------------

fn deref(file: &PdfFile, obj: &PdfObject) -> PdfObject {
    match obj {
        PdfObject::Ref(r) => file.resolve(*r).unwrap_or(PdfObject::Null),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zpdf_parser::PdfFile;

    /// Build a PDF from numbered object bodies with a caller-supplied header
    /// line and trailer dict body. `build_pdf` writes a fixed trailer with no
    /// `/ID` or `/Info`; PDF/X needs both, so this variant takes them.
    fn build_with_trailer(objects: &[&str], header: &str, trailer: &str) -> Vec<u8> {
        let mut buf = Vec::from(header.as_bytes());
        let mut offsets = Vec::new();
        for (i, body) in objects.iter().enumerate() {
            offsets.push(buf.len());
            buf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
        }
        let xref = buf.len();
        buf.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for off in &offsets {
            buf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        buf.extend_from_slice(format!("trailer\n{trailer}\nstartxref\n{xref}\n%%EOF\n").as_bytes());
        buf
    }

    fn minimal_pdf() -> Vec<u8> {
        build_with_trailer(
            &[
                "<< /Type /Catalog /Pages 2 0 R >>",
                "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>",
            ],
            "%PDF-1.4\n",
            "<< /Size 4 /Root 1 0 R /ID [<01> <02>] >>",
        )
    }

    /// A PDF/X-4-compliant document: PDF 1.4, /GTS_PDFX output intent with an
    /// embedded CMYK (/N 4) ICC profile, XMP carrying `pdfxid:GTS_PDFXVersion`,
    /// a /TrimBox, /ID, and /Info /Trapped /False. Per-rule tests mutate one
    /// aspect to isolate a single violation.
    fn pdfx_compliant_pdf() -> Vec<u8> {
        let xmp = "<< /Type /Metadata /Subtype /XML >>\nstream\n\
            <?xpacket begin=\"\u{FEFF}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
            <x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n\
            <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n\
            <rdf:Description rdf:about=\"\" xmlns:pdfxid=\"http://www.npes.org/pdfx/ns/id/\">\n\
            <pdfxid:GTS_PDFXVersion>X-4</pdfxid:GTS_PDFXVersion>\n\
            </rdf:Description>\n\
            </rdf:RDF>\n\
            </x:xmpmeta>\n\
            <?xpacket end=\"w\"?>\nendstream";
        build_with_trailer(
            &[
                "<< /Type /Catalog /Pages 2 0 R /Metadata 4 0 R /OutputIntents [6 0 R] >>",
                "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /TrimBox [0 0 612 792] >>",
                xmp,
                // 5: CMYK ICC profile stream (placeholder bytes — only /N is read).
                "<< /N 4 /Length 0 >>\nstream\n\nendstream",
                // 6: GTS_PDFX output intent.
                "<< /Type /OutputIntent /S /GTS_PDFX /OutputConditionIdentifier (FOGRA39) \
                 /DestOutputProfile 5 0 R >>",
                // 7: /Info with /Trapped /False.
                "<< /Trapped /False /Producer (zpdf) >>",
            ],
            "%PDF-1.4\n",
            "<< /Size 8 /Root 1 0 R /ID [<01> <02>] /Info 7 0 R >>",
        )
    }

    fn rules(report: &ValidationReport) -> Vec<&str> {
        report.violations.iter().map(|v| v.rule).collect()
    }

    #[test]
    fn bare_pdf_fails_output_intent_and_boxes() {
        let file = PdfFile::parse(minimal_pdf()).unwrap();
        let report = validate(&file, Profile::X4);
        assert!(!report.conforms());
        let r = rules(&report);
        assert!(r.contains(&"output-intent"), "missing output intent: {r:?}");
        assert!(r.contains(&"page-trimbox"), "missing trim/art box: {r:?}");
        assert!(r.contains(&"xmp-missing"), "missing XMP: {r:?}");
        // No fonts referenced → no font violation; no colour spaces → no colour violation.
        assert!(
            !r.contains(&"font-not-embedded"),
            "no fonts, no font rule: {r:?}"
        );
    }

    #[test]
    fn compliant_x4_passes() {
        let file = PdfFile::parse(pdfx_compliant_pdf()).unwrap();
        let report = validate(&file, Profile::X4);
        if !report.conforms() {
            panic!("X-4 compliant PDF should pass: {:?}", rules(&report));
        }
        assert_eq!(report.claimed.as_deref(), Some("X-4"));
    }

    #[test]
    fn missing_cmyk_profile_flagged() {
        // GTS_PDFX intent but no /DestOutputProfile.
        let pdf = build_with_trailer(
            &[
                "<< /Type /Catalog /Pages 2 0 R /Metadata 4 0 R /OutputIntents [6 0 R] >>",
                "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /TrimBox [0 0 612 792] >>",
                "<< /Type /Metadata /Subtype /XML >>\nstream\n<rdf:Description/>\nendstream",
                "<< /N 4 /Length 0 >>\nstream\n\nendstream",
                "<< /Type /OutputIntent /S /GTS_PDFX /OutputConditionIdentifier (FOGRA39) >>",
                "<< /Trapped /False /Producer (zpdf) >>",
            ],
            "%PDF-1.4\n",
            "<< /Size 8 /Root 1 0 R /ID [<01> <02>] /Info 7 0 R >>",
        );
        let file = PdfFile::parse(pdf).unwrap();
        let report = validate(&file, Profile::X4);
        let r = rules(&report);
        assert!(
            r.contains(&"output-intent-profile"),
            "missing /DestOutputProfile must be flagged: {r:?}"
        );
    }

    #[test]
    fn non_cmyk_profile_flagged() {
        // GTS_PDFX intent with an RGB (/N 3) profile → output-intent-cmyk.
        let mut objs: Vec<String> = vec![
            "<< /Type /Catalog /Pages 2 0 R /Metadata 4 0 R /OutputIntents [6 0 R] >>".into(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /TrimBox [0 0 612 792] >>".into(),
            "<< /Type /Metadata /Subtype /XML >>\nstream\n<rdf:Description/>\nendstream".into(),
            "<< /N 3 /Length 0 >>\nstream\n\nendstream".into(),
            "<< /Type /OutputIntent /S /GTS_PDFX /DestOutputProfile 5 0 R >>".into(),
            "<< /Trapped /False /Producer (zpdf) >>".into(),
        ];
        let refs: Vec<&str> = objs.iter().map(|s| s.as_str()).collect();
        let pdf = build_with_trailer(
            &refs,
            "%PDF-1.4\n",
            "<< /Size 8 /Root 1 0 R /ID [<01> <02>] /Info 7 0 R >>",
        );
        let _ = &mut objs; // keep allocations alive
        let file = PdfFile::parse(pdf).unwrap();
        let report = validate(&file, Profile::X4);
        let r = rules(&report);
        assert!(
            r.contains(&"output-intent-cmyk"),
            "RGB ICC profile must be flagged: {r:?}"
        );
    }

    #[test]
    fn device_rgb_flagged_under_x1a_and_x4() {
        // A named /DeviceRGB colour space: flagged under X-1a (color-rgb) and
        // X-4 (color-device-rgb); a /CalRGB space flags X-1a only.
        let pdf = build_with_trailer(
            &[
                "<< /Type /Catalog /Pages 2 0 R /Metadata 4 0 R /OutputIntents [6 0 R] >>",
                "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /TrimBox [0 0 612 792] \
                 /Resources << /ColorSpace << /CS0 /DeviceRGB /CS1 [/CalRGB << >>] >> >> >>",
                "<< /Type /Metadata /Subtype /XML >>\nstream\n<rdf:Description/>\nendstream",
                "<< /N 4 /Length 0 >>\nstream\n\nendstream",
                "<< /Type /OutputIntent /S /GTS_PDFX /DestOutputProfile 5 0 R >>",
                "<< /Trapped /False /Producer (zpdf) >>",
            ],
            "%PDF-1.4\n",
            "<< /Size 8 /Root 1 0 R /ID [<01> <02>] /Info 7 0 R >>",
        );
        let file = PdfFile::parse(pdf).unwrap();
        let x1a = validate(&file, Profile::X1a);
        let x4 = validate(&file, Profile::X4);
        let x1a_r = rules(&x1a);
        let x4_r = rules(&x4);
        assert!(
            x1a_r.contains(&"color-rgb"),
            "X-1a must flag DeviceRGB/CalRGB: {x1a_r:?}"
        );
        assert!(
            x4_r.contains(&"color-device-rgb"),
            "X-4 must flag DeviceRGB: {x4_r:?}"
        );
        assert!(
            !x4_r.contains(&"color-rgb"),
            "X-4 must not use the X-1a colour rule: {x4_r:?}"
        );
    }

    #[test]
    fn header_version_gating() {
        let compliant = pdfx_compliant_pdf(); // PDF 1.4 — valid for X-4.
        let file = PdfFile::parse(compliant).unwrap();
        // 1.4 passes X-4, fails X-1a/X-3 (which want 1.3).
        let x4 = validate(&file, Profile::X4);
        let x1a = validate(&file, Profile::X1a);
        assert!(!rules(&x4).contains(&"header-version"), "1.4 ok for X-4");
        assert!(
            rules(&x1a).contains(&"header-version"),
            "1.4 too new for X-1a: {:?}",
            rules(&x1a)
        );
        // A PDF 2.0 header passes X-6, fails X-4.
        let pdf20 = build_with_trailer(
            &[
                "<< /Type /Catalog /Pages 2 0 R /Metadata 4 0 R /OutputIntents [6 0 R] >>",
                "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /TrimBox [0 0 612 792] >>",
                "<< /Type /Metadata /Subtype /XML >>\nstream\n<rdf:Description/>\nendstream",
                "<< /N 4 /Length 0 >>\nstream\n\nendstream",
                "<< /Type /OutputIntent /S /GTS_PDFX /DestOutputProfile 5 0 R >>",
                "<< /Trapped /False /Producer (zpdf) >>",
            ],
            "%PDF-2.0\n",
            "<< /Size 8 /Root 1 0 R /ID [<01> <02>] /Info 7 0 R >>",
        );
        let file20 = PdfFile::parse(pdf20).unwrap();
        let x6 = validate(&file20, Profile::X6);
        let x4_20 = validate(&file20, Profile::X4);
        assert!(!rules(&x6).contains(&"header-version"), "2.0 ok for X-6");
        assert!(
            rules(&x4_20).contains(&"header-version"),
            "2.0 too new for X-4: {:?}",
            rules(&x4_20)
        );
    }

    #[test]
    fn trimbox_containment_violation() {
        // BleedBox larger than MediaBox → page-box-containment.
        let pdf = build_with_trailer(
            &[
                "<< /Type /Catalog /Pages 2 0 R /Metadata 4 0 R /OutputIntents [6 0 R] >>",
                "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /TrimBox [0 0 100 100] \
                 /BleedBox [-10 -10 110 110] >>",
                "<< /Type /Metadata /Subtype /XML >>\nstream\n<rdf:Description/>\nendstream",
                "<< /N 4 /Length 0 >>\nstream\n\nendstream",
                "<< /Type /OutputIntent /S /GTS_PDFX /DestOutputProfile 5 0 R >>",
                "<< /Trapped /False /Producer (zpdf) >>",
            ],
            "%PDF-1.4\n",
            "<< /Size 8 /Root 1 0 R /ID [<01> <02>] /Info 7 0 R >>",
        );
        let file = PdfFile::parse(pdf).unwrap();
        let report = validate(&file, Profile::X4);
        assert!(
            rules(&report).contains(&"page-box-containment"),
            "BleedBox outside MediaBox must be flagged: {:?}",
            rules(&report)
        );
    }

    #[test]
    fn trapped_required_under_x1a() {
        let compliant = pdfx_compliant_pdf(); // has /Trapped /False.
        let file = PdfFile::parse(compliant).unwrap();
        // X-4 passes (already checked); remove /Trapped → X-1a flags, X-4 still passes.
        // Build a variant with no /Info /Trapped by dropping object 7's /Trapped.
        let xmp = "<< /Type /Metadata /Subtype /XML >>\nstream\n<rdf:Description/>\nendstream";
        let no_trapped = build_with_trailer(
            &[
                "<< /Type /Catalog /Pages 2 0 R /Metadata 4 0 R /OutputIntents [6 0 R] >>",
                "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /TrimBox [0 0 612 792] >>",
                xmp,
                "<< /N 4 /Length 0 >>\nstream\n\nendstream",
                "<< /Type /OutputIntent /S /GTS_PDFX /DestOutputProfile 5 0 R >>",
                "<< /Producer (zpdf) >>", // no /Trapped
            ],
            "%PDF-1.3\n",
            "<< /Size 8 /Root 1 0 R /ID [<01> <02>] /Info 7 0 R >>",
        );
        let f2 = PdfFile::parse(no_trapped).unwrap();
        let x1a = validate(&f2, Profile::X1a);
        assert!(
            rules(&x1a).contains(&"trapped"),
            "missing /Trapped must be flagged under X-1a: {:?}",
            rules(&x1a)
        );
        // X-4 with /Trapped /Unknown is flagged.
        let unknown_trapped = build_with_trailer(
            &[
                "<< /Type /Catalog /Pages 2 0 R /Metadata 4 0 R /OutputIntents [6 0 R] >>",
                "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /TrimBox [0 0 612 792] >>",
                xmp,
                "<< /N 4 /Length 0 >>\nstream\n\nendstream",
                "<< /Type /OutputIntent /S /GTS_PDFX /DestOutputProfile 5 0 R >>",
                "<< /Trapped /Unknown /Producer (zpdf) >>",
            ],
            "%PDF-1.4\n",
            "<< /Size 8 /Root 1 0 R /ID [<01> <02>] /Info 7 0 R >>",
        );
        let f3 = PdfFile::parse(unknown_trapped).unwrap();
        let x4 = validate(&f3, Profile::X4);
        assert!(
            rules(&x4).contains(&"trapped-unknown"),
            "/Trapped /Unknown must be flagged under X-4: {:?}",
            rules(&x4)
        );
        let _ = file;
    }

    #[test]
    fn forbidden_annotation_subtype_flagged_for_all_profiles() {
        // A FileAttachment annotation is forbidden in all four PDF/X parts
        // (unlike PDF/A, where it is A-1 only).
        let pdf = build_with_trailer(
            &[
                "<< /Type /Catalog /Pages 2 0 R /Metadata 4 0 R /OutputIntents [6 0 R] >>",
                "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /TrimBox [0 0 612 792] \
                 /Annots [8 0 R] >>",
                "<< /Type /Metadata /Subtype /XML >>\nstream\n<rdf:Description/>\nendstream",
                "<< /N 4 /Length 0 >>\nstream\n\nendstream",
                "<< /Type /OutputIntent /S /GTS_PDFX /DestOutputProfile 5 0 R >>",
                "<< /Trapped /False /Producer (zpdf) >>",
                "<< /Type /Annot /Subtype /FileAttachment /Rect [0 0 10 10] /F 4 >>",
            ],
            "%PDF-1.4\n",
            "<< /Size 9 /Root 1 0 R /ID [<01> <02>] /Info 7 0 R >>",
        );
        let file = PdfFile::parse(pdf).unwrap();
        for p in [Profile::X1a, Profile::X3, Profile::X4, Profile::X6] {
            let report = validate(&file, p);
            assert!(
                rules(&report).contains(&"annotation-subtype"),
                "{} must flag FileAttachment: {:?}",
                p.as_str(),
                rules(&report)
            );
        }
    }

    #[test]
    fn annotation_print_flag_required_under_x1a() {
        let pdf = build_with_trailer(
            &[
                "<< /Type /Catalog /Pages 2 0 R /Metadata 4 0 R /OutputIntents [6 0 R] >>",
                "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /TrimBox [0 0 612 792] \
                 /Annots [8 0 R] >>",
                "<< /Type /Metadata /Subtype /XML >>\nstream\n<rdf:Description/>\nendstream",
                "<< /N 4 /Length 0 >>\nstream\n\nendstream",
                "<< /Type /OutputIntent /S /GTS_PDFX /DestOutputProfile 5 0 R >>",
                "<< /Trapped /False /Producer (zpdf) >>",
                // Text annot with no Print flag (F absent).
                "<< /Type /Annot /Subtype /Text /Rect [0 0 10 10] >>",
            ],
            "%PDF-1.3\n",
            "<< /Size 9 /Root 1 0 R /ID [<01> <02>] /Info 7 0 R >>",
        );
        let file = PdfFile::parse(pdf).unwrap();
        let x1a = validate(&file, Profile::X1a);
        let x4 = validate(&file, Profile::X4);
        assert!(
            rules(&x1a).contains(&"annotation-print-flag"),
            "X-1a must flag non-printing annotation: {:?}",
            rules(&x1a)
        );
        assert!(
            !rules(&x4).contains(&"annotation-print-flag"),
            "X-4 must not check the print flag: {:?}",
            rules(&x4)
        );
    }

    #[test]
    fn claim_extraction_from_both_namespaces() {
        // Modern pdfxid: namespace (X-4/X-6).
        assert_eq!(
            extract_xmp_value(
                "<pdfxid:GTS_PDFXVersion>X-4</pdfxid:GTS_PDFXVersion>",
                "pdfxid:GTS_PDFXVersion"
            )
            .as_deref(),
            Some("X-4")
        );
        // Legacy Adobe pdfx: namespace (X-1a/X-3), attribute form.
        assert_eq!(
            extract_xmp_value(
                "<rdf:Description pdfx:GTS_PDFXVersion=\"X-1a\"/>",
                "pdfx:GTS_PDFXVersion"
            )
            .as_deref(),
            Some("X-1a")
        );
    }
}
