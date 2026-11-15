//! Annotation parsing: `/Annots` entries resolved into renderable form —
//! `/Rect`, `/F` flags, the `/AS`-selected normal appearance stream, and the
//! optional-content membership (`/OC`). Painting itself happens in
//! zpdf-content, which replays the appearance stream as a form XObject mapped
//! onto `/Rect` (PDF 32000-1 §12.5.5).

use std::collections::HashMap;

use zpdf_core::{ObjectId, PdfObject, Rect};
use zpdf_parser::PdfFile;

use crate::destinations::{resolve_link_target, Destination};
use crate::forms::{AcroForm, GeneratedAppearance};
use crate::measure::{parse_measure, Measure};
use crate::page::PdfPage;
use crate::Catalog;

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
    /// A synthesized appearance for an interactive-form widget whose producer
    /// left no `/AP` (or set `/NeedAppearances`). Takes precedence over
    /// `appearance` when present.
    pub generated: Option<GeneratedAppearance>,
    /// /OC optional-content membership (a Ref to an OCG/OCMD, or a direct
    /// dict), evaluated against the document's OC config at paint time.
    pub oc: Option<PdfObject>,
    /// The in-document navigation target this annotation links to — a resolved
    /// [`Destination`] from a `/Dest`, a go-to action (`/A /S /GoTo`), or a
    /// remote go-to whose target page is in range. `None` for non-link
    /// annotations and for URI / external links (see [`Annotation::uri`]).
    /// Chiefly populated for `Link` annotations.
    pub dest: Option<Destination>,
    /// An external link target: a URI (`/A /S /URI`) or a remote go-to file name
    /// (`/A /S /GoToR /F`). `None` for in-document and non-link annotations.
    pub uri: Option<String>,
    /// Geospatial measure dictionary (`/Measure`) defining coordinate systems
    /// and units for map annotations (ISO 32000-1 §13.2). Primarily used with
    /// PDF 2.0 Projection annotations.
    pub measure: Option<Measure>,
}

impl Annotation {
    /// True when the annotation should be painted in a screen rendering
    /// (before optional-content evaluation).
    pub fn is_viewable(&self) -> bool {
        self.flags & (ANNOT_FLAG_HIDDEN | ANNOT_FLAG_NOVIEW) == 0
            // Popups only appear when opened interactively.
            && self.subtype != "Popup"
            && (self.appearance.is_some() || self.generated.is_some())
            && self.rect.width() > 0.0
            && self.rect.height() > 0.0
    }
}

/// Parse a page's annotations into renderable form. Unresolvable or
/// appearance-less entries are kept (callers may want link rects later) but
/// fail `is_viewable`. When an `AcroForm` is supplied, widget annotations gain
/// a generated appearance where the producer left none. Link targets (`/Dest` /
/// `/A`) are resolved to a [`Destination`] or URI via `catalog` and the
/// document-wide `named` destination map (flattened once by the caller, so the
/// name tree is never re-walked per page). The document's decoded-content
/// budget also bounds the aggregate estimated storage of generated appearances
/// for this call. Once exhausted, later appearances are not generated; records
/// and any stored `/AP` remain available to callers.
pub fn parse_annotations(
    file: &PdfFile,
    page: &PdfPage,
    catalog: &Catalog,
    named: &HashMap<Vec<u8>, PdfObject>,
    acro_form: Option<&AcroForm>,
) -> Vec<Annotation> {
    let mut remaining = file.limits().max_decoded_stream_bytes;
    let mut generate = remaining != 0;
    if !generate && !page.annots.is_empty() {
        tracing::debug!("generated annotation appearances disabled by zero decoded-content budget");
    }
    let mut annotations = Vec::new();
    for &id in &page.annots {
        let Some(mut annotation) = parse_annotation(file, id, catalog, named, acro_form, generate)
        else {
            continue;
        };
        if let Some(appearance) = &annotation.generated {
            let cost = generated_appearance_bytes(appearance);
            if let Some(left) = remaining.checked_sub(cost) {
                remaining = left;
                generate = remaining != 0;
                if !generate {
                    tracing::debug!("generated annotation appearance budget exhausted");
                }
            } else {
                // Drop this appearance before retaining the annotation. A
                // widget's selected stored AP is still a usable fallback.
                annotation.generated = None;
                generate = false;
                tracing::warn!(
                    "generated annotation appearances exceed the page decoded-content budget; \
                     skipping further generation"
                );
            }
        }
        annotations.push(annotation);
    }
    annotations
}

/// Conservative owned-storage estimate. Indirect resources remain object IDs;
/// direct arrays/dictionaries/strings are copied by generation and must count.
/// Shared stream payloads are conservatively charged when present inline.
fn generated_appearance_bytes(appearance: &GeneratedAppearance) -> u64 {
    fn dict_bytes(dict: &zpdf_core::PdfDict) -> u64 {
        dict.0.iter().fold(0u64, |sum, (key, value)| {
            sum.saturating_add(key.0.capacity() as u64)
                .saturating_add(48) // BTree node/link overhead.
                .saturating_add(object_bytes(value))
        })
    }
    fn object_bytes(object: &PdfObject) -> u64 {
        let payload = match object {
            PdfObject::String(s) => s.0.capacity() as u64,
            PdfObject::Name(n) => n.0.capacity() as u64,
            PdfObject::Array(items) => items.iter().fold(
                (items.capacity() as u64).saturating_mul(std::mem::size_of::<PdfObject>() as u64),
                |sum, item| sum.saturating_add(object_bytes(item)),
            ),
            PdfObject::Dict(dict) => dict_bytes(dict),
            PdfObject::Stream(stream) => {
                (stream.data.len() as u64).saturating_add(dict_bytes(&stream.dict))
            }
            _ => 0,
        };
        (std::mem::size_of::<PdfObject>() as u64).saturating_add(payload)
    }
    (std::mem::size_of::<GeneratedAppearance>() as u64)
        .saturating_add(appearance.content.capacity() as u64)
        .saturating_add(dict_bytes(&appearance.resources))
}

fn parse_annotation(
    file: &PdfFile,
    id: ObjectId,
    catalog: &Catalog,
    named: &HashMap<Vec<u8>, PdfObject>,
    acro_form: Option<&AcroForm>,
    generate: bool,
) -> Option<Annotation> {
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
    // Resolve the navigation target (cheap (None, None) when the annotation
    // carries neither /Dest nor /A — the common case for non-link annotations).
    let (dest, uri) = resolve_link_target(file, catalog, dict, Some(named));

    // Parse geospatial measure dictionary if present.
    let measure = parse_measure(file, dict);

    // Generate an appearance when the producer left none. Form widgets defer to
    // the AcroForm generator (which also honours /NeedAppearances and keeps
    // button /AP states); markup & geometric annotations synthesize their
    // appearance from geometry properties (/QuadPoints, /Vertices, /L, …).
    let generated = if !generate {
        None
    } else if subtype == "Widget" {
        acro_form
            .and_then(|af| af.field_for_widget(id).map(|field| (af, field)))
            .filter(|(af, _)| af.need_appearances || appearance.is_none())
            .and_then(|(af, field)| {
                crate::forms::generate_widget_appearance(field, rect, af.dr_fonts.as_ref())
            })
    } else if appearance.is_none() {
        crate::annot_appearance::generate_annotation_appearance(file, dict, &subtype, rect)
    } else {
        None
    };

    Some(Annotation {
        subtype,
        rect,
        flags,
        appearance,
        generated,
        oc,
        dest,
        uri,
        measure,
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
    // Prefer /AS; for a checkbox/radio whose /AS is absent, the on/off state is
    // named by /V (present on the merged field+widget dict).
    let state = annot.get_name("AS").ok().or_else(|| match annot.get("V") {
        Some(PdfObject::Name(n)) => Some(n.as_str()),
        _ => None,
    });
    if let Some(state) = state {
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

#[cfg(test)]
mod tests {
    use crate::test_util::build_pdf;
    use crate::PdfDocument;

    /// Two-page document; page 0 (object 3) carries the `/Annots` under test,
    /// page 1 (object 4) is a destination target. Annotation objects start at 5.
    fn doc_with_annots(annot_refs: &str, annots: &[&str]) -> PdfDocument {
        doc_with_annots_and_limits(annot_refs, annots, zpdf_core::ParseLimits::default())
    }

    fn doc_with_annots_and_limits(
        annot_refs: &str,
        annots: &[&str],
        limits: zpdf_core::ParseLimits,
    ) -> PdfDocument {
        let mut objs: Vec<String> = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".into(),
            "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".into(),
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Annots [{annot_refs}] >>"
            ),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>".into(),
        ];
        objs.extend(annots.iter().map(|a| (*a).to_string()));
        let refs: Vec<&str> = objs.iter().map(|s| s.as_str()).collect();
        PdfDocument::open_with_limits(build_pdf(&refs), limits).expect("open")
    }

    #[test]
    fn generated_appearances_share_a_page_storage_budget() {
        let refs = "5 0 R 5 0 R 5 0 R";
        let objects = ["<< /Type /Annot /Subtype /Square /Rect [0 0 20 20] /C [1 0 0] >>"];
        let doc = doc_with_annots(refs, &objects);
        let annotations = doc.page_annotations(&doc.page(0).unwrap());
        let cost = super::generated_appearance_bytes(annotations[0].generated.as_ref().unwrap());
        let limits = zpdf_core::ParseLimits {
            max_decoded_stream_bytes: cost,
            ..Default::default()
        };
        let doc = doc_with_annots_and_limits(refs, &objects, limits);
        let annotations = doc.page_annotations(&doc.page(0).unwrap());
        assert_eq!(annotations.len(), 3, "annotation records remain available");
        assert_eq!(
            annotations.iter().filter(|a| a.generated.is_some()).count(),
            1
        );
        let mut appearance = annotations[0].generated.clone().unwrap();
        let before_resource = super::generated_appearance_bytes(&appearance);
        appearance.resources.insert(
            zpdf_core::PdfName::new("Direct"),
            zpdf_core::PdfObject::String(zpdf_core::PdfString(vec![0; 8])),
        );
        assert!(
            super::generated_appearance_bytes(&appearance) > before_resource,
            "direct resource storage must count as well as content"
        );
    }

    #[test]
    fn generated_widget_budget_keeps_stored_appearance_fallback() {
        let pdf = build_pdf(&[
            "<< /Type /Catalog /Pages 2 0 R /AcroForm << /Fields [4 0 R] /NeedAppearances true /DA (/Helv 10 Tf 0 g) >> >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Annots [4 0 R] >>",
            "<< /Type /Annot /Subtype /Widget /FT /Tx /T (field) /V (hello) /Rect [0 0 80 20] /AP << /N 5 0 R >> >>",
            "<< /Length 0 >>\nstream\n\nendstream",
        ]);
        let doc = PdfDocument::open(pdf.clone()).unwrap();
        assert!(doc.page_annotations(&doc.page(0).unwrap())[0]
            .generated
            .is_some());
        let limits = zpdf_core::ParseLimits {
            max_decoded_stream_bytes: 1,
            ..Default::default()
        };
        let doc = PdfDocument::open_with_limits(pdf, limits).unwrap();
        let annotations = doc.page_annotations(&doc.page(0).unwrap());
        assert!(annotations[0].generated.is_none());
        assert_eq!(annotations[0].appearance, Some(zpdf_core::ObjectId(5, 0)));
        assert!(annotations[0].is_viewable());
    }

    #[test]
    fn link_explicit_dest_resolves() {
        let doc = doc_with_annots(
            "5 0 R",
            &["<< /Type /Annot /Subtype /Link /Rect [10 10 100 30] /Dest [4 0 R /Fit] >>"],
        );
        let page = doc.page(0).unwrap();
        let annots = doc.page_annotations(&page);
        assert_eq!(annots.len(), 1);
        let d = annots[0].dest.as_ref().expect("dest");
        assert_eq!(d.page, Some(1));
        assert!(annots[0].uri.is_none());
    }

    #[test]
    fn link_uri_action_captured() {
        let doc = doc_with_annots(
            "5 0 R",
            &["<< /Type /Annot /Subtype /Link /Rect [0 0 100 20] \
               /A << /S /URI /URI (https://example.com) >> >>"],
        );
        let page = doc.page(0).unwrap();
        let a = &doc.page_annotations(&page)[0];
        assert_eq!(a.uri.as_deref(), Some("https://example.com"));
        assert!(a.dest.is_none());
    }

    #[test]
    fn link_goto_action_dest_resolves() {
        let doc = doc_with_annots(
            "5 0 R",
            &["<< /Type /Annot /Subtype /Link /Rect [0 0 50 50] \
               /A << /S /GoTo /D [3 0 R /XYZ null 700 null] >> >>"],
        );
        let page = doc.page(0).unwrap();
        let d = doc.page_annotations(&page)[0].dest.clone().expect("dest");
        assert_eq!(d.page, Some(0));
    }

    #[test]
    fn link_gotor_remote_file_name() {
        let doc = doc_with_annots(
            "5 0 R",
            &["<< /Type /Annot /Subtype /Link /Rect [0 0 50 50] \
               /A << /S /GoToR /F (other.pdf) >> >>"],
        );
        let page = doc.page(0).unwrap();
        let a = &doc.page_annotations(&page)[0];
        assert_eq!(a.uri.as_deref(), Some("other.pdf"));
        assert!(a.dest.is_none());
    }

    #[test]
    fn non_link_annotation_has_no_target() {
        let doc = doc_with_annots(
            "5 0 R",
            &["<< /Type /Annot /Subtype /Text /Rect [0 0 20 20] /Contents (note) >>"],
        );
        let page = doc.page(0).unwrap();
        let a = &doc.page_annotations(&page)[0];
        assert!(a.dest.is_none() && a.uri.is_none());
    }

    #[test]
    fn link_named_dest_via_collected_map() {
        // A link naming a destination registered in the /Names /Dests name tree,
        // resolved through the once-per-page collected map.
        let doc = PdfDocument::open(build_pdf(&[
            "<< /Type /Catalog /Pages 2 0 R /Names << /Dests 6 0 R >> >>",
            "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Annots [5 0 R] >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>",
            "<< /Type /Annot /Subtype /Link /Rect [0 0 50 50] /Dest (chap2) >>",
            "<< /Names [ (chap2) [4 0 R /Fit] ] >>",
        ]))
        .expect("open");
        let page = doc.page(0).unwrap();
        let d = doc.page_annotations(&page)[0]
            .dest
            .clone()
            .expect("named dest");
        assert_eq!(d.page, Some(1));
    }
}
