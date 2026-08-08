//! PDF/A conversion pass for [`crate::rewrite_pdf`].
//!
//! When `RewriteOptions::pdfa` is set, [`prepare`] computes the edits
//! `rewrite_pdf` applies on top of its reachability walk: new objects to
//! inject (an ICC profile stream, a `GTS_PDFA1` output intent, and a
//! `pdfaid` XMP `/Metadata` stream), per-object transforms (a catalog
//! carrying the new `/OutputIntents` + `/Metadata` and stripped of
//! PDF/A-forbidden features, page dicts with transparency groups removed
//! for A-1b, and FontDescriptors gaining a fallback `/FontFile2`), and the
//! header version to emit (`%PDF-1.4` for A-1b).
//!
//! The injected objects get object numbers continuing past the walked set,
//! and the transforms are pre-renumbered through the walk's `old → new` map
//! (existing refs) with the injected objects' final numbers (new refs), so
//! `rewrite_pdf` emits them verbatim. The output then passes
//! `zpdf_document::pdfa::validate` for the same profile.

use std::collections::HashMap;

use zpdf_core::{ObjectId, PdfDict, PdfName, PdfObject};
use zpdf_parser::PdfFile;

use crate::metadata::encode_text_string;
use crate::rewrite::{renumber, PdfaConvertConfig, PdfaProfile};
use crate::{flate_compress, invalid_data};

/// The default sRGB v2 ICC profile, embedded as the `/DestOutputProfile` for
/// the `GTS_PDFA1` output intent when the caller supplies no profile.
const SRGB_ICC: &[u8] = include_bytes!("../../zpdf-color/src/testdata/srgb.icc");

/// A new object injected by the PDF/A pass: a direct object or a stream.
pub(crate) enum ExtraObj {
    Object(PdfObject),
    Stream(PdfDict, Vec<u8>),
}

/// Edits computed by [`prepare`] that [`rewrite_pdf`] applies.
pub(crate) struct PdfaEdits {
    /// New objects to emit after the walked set, in ascending object-number
    /// order (continuing past the walk). `rewrite_pdf` pushes their offsets in
    /// this order so the xref stays ascending.
    pub extras: Vec<(u32, ExtraObj)>,
    /// Replacement bodies for existing (walked) objects, keyed by their OLD
    /// object id. Already renumbered; emitted in place of the resolved
    /// original.
    pub transforms: HashMap<ObjectId, PdfObject>,
    /// The header version line to emit (`%PDF-1.4` for A-1b, `%PDF-1.7` else).
    pub header: &'static str,
}

impl Default for PdfaEdits {
    fn default() -> Self {
        Self {
            extras: Vec::new(),
            transforms: HashMap::new(),
            header: "%PDF-1.7",
        }
    }
}

/// Compute the PDF/A edits for `source` under the walk's `order`/`map`.
pub(crate) fn prepare(
    source: &PdfFile,
    order: &[ObjectId],
    map: &HashMap<ObjectId, u32>,
    cfg: &PdfaConvertConfig,
) -> Result<PdfaEdits, zpdf_core::Error> {
    let mut edits = PdfaEdits {
        extras: Vec::new(),
        transforms: HashMap::new(),
        header: match cfg.profile {
            PdfaProfile::A1b => "%PDF-1.4",
            PdfaProfile::A2b => "%PDF-1.7",
        },
    };

    let root = source
        .trailer
        .get_ref("Root")
        .map_err(|_| invalid_data("trailer missing /Root"))?;
    let mut next_extra = (order.len() as u32) + 1;

    // --- ICC profile stream (the /DestOutputProfile) ---
    let icc_bytes = cfg.icc.as_deref().unwrap_or(SRGB_ICC);
    let icc_num = next_extra;
    next_extra += 1;
    let mut icc_dict = PdfDict::new();
    icc_dict.insert(PdfName::new("N"), PdfObject::Integer(3));
    icc_dict.insert(
        PdfName::new("Filter"),
        PdfObject::Name(PdfName::new("FlateDecode")),
    );
    edits.extras.push((
        icc_num,
        ExtraObj::Stream(icc_dict, flate_compress(icc_bytes)),
    ));

    // --- GTS_PDFA1 output intent ---
    let intent_num = next_extra;
    next_extra += 1;
    let mut intent = PdfDict::new();
    intent.insert(
        PdfName::new("Type"),
        PdfObject::Name(PdfName::new("OutputIntent")),
    );
    intent.insert(
        PdfName::new("S"),
        PdfObject::Name(PdfName::new("GTS_PDFA1")),
    );
    intent.insert(
        PdfName::new("OutputConditionIdentifier"),
        PdfObject::String(encode_text_string("sRGB")),
    );
    intent.insert(
        PdfName::new("Info"),
        PdfObject::String(encode_text_string("sRGB IEC61966-2.1")),
    );
    intent.insert(
        PdfName::new("DestOutputProfile"),
        PdfObject::Ref(ObjectId(icc_num, 0)),
    );
    edits
        .extras
        .push((intent_num, ExtraObj::Object(PdfObject::Dict(intent))));

    // --- pdfaid XMP /Metadata stream ---
    let xmp = build_pdfa_xmp(cfg.profile);
    let xmp_num = next_extra;
    next_extra += 1;
    let mut xmp_dict = PdfDict::new();
    xmp_dict.insert(
        PdfName::new("Type"),
        PdfObject::Name(PdfName::new("Metadata")),
    );
    xmap_sub(&mut xmp_dict);
    xmp_dict.insert(
        PdfName::new("Filter"),
        PdfObject::Name(PdfName::new("FlateDecode")),
    );
    edits
        .extras
        .push((xmp_num, ExtraObj::Stream(xmp_dict, flate_compress(&xmp))));

    // --- Catalog transform: /OutputIntents, /Metadata, strip forbidden ---
    let catalog_obj = source.resolve(root)?;
    let mut catalog_old = catalog_obj
        .as_dict()
        .map_err(|_| invalid_data("catalog is not a dict"))?
        .clone();

    // /OpenAction: strip a JavaScript/Launch action (a destination array is fine).
    if let Some(PdfObject::Dict(action)) = catalog_old
        .get("OpenAction")
        .map(|o| deref(source, o))
        .as_ref()
    {
        let s = action.get_name("S").unwrap_or("");
        if s == "JavaScript" || s == "Launch" {
            catalog_old.0.remove(&PdfName::new("OpenAction"));
        }
    }

    // /Names: if it is an indirect dict, transform that object (below); if it
    // is inline, strip the forbidden name-tree keys here.
    let names_ref_to_transform = match catalog_old.get("Names") {
        Some(PdfObject::Ref(r)) => Some(*r),
        Some(PdfObject::Dict(_)) => {
            strip_names_inline(&mut catalog_old, cfg.profile);
            None
        }
        _ => None,
    };

    // Check for a pre-existing GTS_PDFA1 intent against the *original* (old-id)
    // catalog — the renumbered refs cannot be resolved through `source`.
    let already_pdfa1 = matches!(catalog_old.get("OutputIntents"), Some(PdfObject::Array(a)) if a.iter().any(|o| {
        matches!(deref(source, o).as_dict().ok().and_then(|d| d.get_name("S").ok()), Some("GTS_PDFA1"))
    }));

    let mut cat = match renumber(&PdfObject::Dict(catalog_old), map) {
        PdfObject::Dict(d) => d,
        _ => unreachable!("renumber of a dict yields a dict"),
    };

    // /OutputIntents: append the PDFA1 intent to any existing array.
    let mut intents = match cat.get("OutputIntents") {
        Some(PdfObject::Array(a)) => a.clone(),
        _ => Vec::new(),
    };
    if !already_pdfa1 {
        intents.push(PdfObject::Ref(ObjectId(intent_num, 0)));
    }
    cat.insert(PdfName::new("OutputIntents"), PdfObject::Array(intents));

    // /Metadata: replace with the pdfaid XMP packet.
    cat.insert(
        PdfName::new("Metadata"),
        PdfObject::Ref(ObjectId(xmp_num, 0)),
    );
    edits.transforms.insert(root, PdfObject::Dict(cat));

    // Names object transform (strip JavaScript always, EmbeddedFiles for A-1b).
    if let Some(names_id) = names_ref_to_transform {
        if let Ok(PdfObject::Dict(mut names)) = source.resolve(names_id) {
            names.0.remove(&PdfName::new("JavaScript"));
            if cfg.profile == PdfaProfile::A1b {
                names.0.remove(&PdfName::new("EmbeddedFiles"));
            }
            let ren = renumber(&PdfObject::Dict(names), map);
            edits.transforms.insert(names_id, ren);
        }
    }

    // --- A-1b: strip page transparency groups ---
    if cfg.profile == PdfaProfile::A1b {
        strip_transparency_groups(source, map, root, &mut edits.transforms);
    }

    // --- Font embedding (best-effort fallback) ---
    if let Some(fallback) = &cfg.fallback_font {
        embed_fallback_fonts(source, map, root, fallback, &mut next_extra, &mut edits)?;
    }

    Ok(edits)
}

fn xmap_sub(dict: &mut PdfDict) {
    dict.insert(
        PdfName::new("Subtype"),
        PdfObject::Name(PdfName::new("XML")),
    );
}

/// Strip the forbidden name-tree keys from an inline /Names dict.
fn strip_names_inline(catalog: &mut PdfDict, profile: PdfaProfile) {
    if let Some(PdfObject::Dict(names)) = catalog.get("Names").cloned().as_ref() {
        let mut n = names.clone();
        n.0.remove(&PdfName::new("JavaScript"));
        if profile == PdfaProfile::A1b {
            n.0.remove(&PdfName::new("EmbeddedFiles"));
        }
        catalog.insert(PdfName::new("Names"), PdfObject::Dict(n));
    }
}

/// Walk the page tree and transform every page dict carrying a transparency
/// group (`/Group /S /Transparency`) to drop its `/Group`.
fn strip_transparency_groups(
    source: &PdfFile,
    map: &HashMap<ObjectId, u32>,
    root: ObjectId,
    transforms: &mut HashMap<ObjectId, PdfObject>,
) {
    let Ok(catalog) = source.resolve(root).and_then(|o| o.as_dict().cloned()) else {
        return;
    };
    let Ok(pages_root) = catalog.get_ref("Pages") else {
        return;
    };
    let mut stack = vec![pages_root];
    let mut visited = std::collections::HashSet::new();
    while let Some(node) = stack.pop() {
        if !visited.insert(node) {
            continue;
        }
        let Ok(dict) = source.resolve(node).and_then(|o| o.as_dict().cloned()) else {
            continue;
        };
        if let Some(PdfObject::Dict(group)) = dict.get("Group").map(|o| deref(source, o)).as_ref() {
            if group.get_name("S").ok() == Some("Transparency") {
                let mut d = dict.clone();
                d.0.remove(&PdfName::new("Group"));
                let ren = renumber(&PdfObject::Dict(d), map);
                transforms.insert(node, ren);
            }
        }
        if let Some(PdfObject::Array(kids)) = dict.get("Kids").map(|o| deref(source, o)).as_ref() {
            for kid in kids {
                if let PdfObject::Ref(r) = kid {
                    stack.push(*r);
                }
            }
        }
    }
}

/// For every non-embedded simple font, embed `fallback` as its descriptor's
/// `/FontFile2` (best-effort: the program may not match the original glyphs,
/// but satisfies PDF/A's embedding requirement). Type0/CID and Type3 fonts
/// are skipped (Type3 is exempt; Type0 fallback needs composite-font authoring).
fn embed_fallback_fonts(
    source: &PdfFile,
    map: &HashMap<ObjectId, u32>,
    root: ObjectId,
    fallback: &[u8],
    next_extra: &mut u32,
    edits: &mut PdfaEdits,
) -> Result<(), zpdf_core::Error> {
    let Ok(catalog) = source.resolve(root).and_then(|o| o.as_dict().cloned()) else {
        return Ok(());
    };
    let Ok(pages_root) = catalog.get_ref("Pages") else {
        return Ok(());
    };
    let compressed = flate_compress(fallback);
    let mut seen: std::collections::HashSet<ObjectId> = std::collections::HashSet::new();
    let mut stack = vec![pages_root];
    let mut visited = std::collections::HashSet::new();
    while let Some(node) = stack.pop() {
        if !visited.insert(node) {
            continue;
        }
        let Ok(dict) = source.resolve(node).and_then(|o| o.as_dict().cloned()) else {
            continue;
        };
        // Collect this node's /Resources /Font (if any) — pages carry resources
        // directly or inherit them, so check every node that has /Resources.
        if let Some(PdfObject::Dict(res)) = dict.get("Resources").map(|o| deref(source, o)).as_ref()
        {
            if let Some(PdfObject::Dict(fonts)) = res.get("Font").map(|o| deref(source, o)).as_ref()
            {
                for v in fonts.0.values() {
                    let PdfObject::Ref(font_ref) = v else {
                        continue;
                    };
                    let Ok(font) = source.resolve(*font_ref).and_then(|o| o.as_dict().cloned())
                    else {
                        continue;
                    };
                    let subtype = font.get_name("Subtype").unwrap_or("");
                    if subtype == "Type3" || subtype == "Type0" {
                        continue;
                    }
                    // A simple font: check its descriptor for an embedded file.
                    let Some(PdfObject::Ref(desc_ref)) = font.get("FontDescriptor") else {
                        continue;
                    };
                    if !seen.insert(*desc_ref) {
                        continue;
                    }
                    let Ok(desc) = source.resolve(*desc_ref).and_then(|o| o.as_dict().cloned())
                    else {
                        continue;
                    };
                    let already_embedded = desc.get("FontFile").is_some()
                        || desc.get("FontFile2").is_some()
                        || desc.get("FontFile3").is_some();
                    if already_embedded {
                        continue;
                    }
                    // Embed the fallback as /FontFile2 on this descriptor.
                    let file_num = *next_extra;
                    *next_extra += 1;
                    let mut file_dict = PdfDict::new();
                    file_dict.insert(
                        PdfName::new("Filter"),
                        PdfObject::Name(PdfName::new("FlateDecode")),
                    );
                    file_dict.insert(
                        PdfName::new("Length1"),
                        PdfObject::Integer(fallback.len() as i64),
                    );
                    edits
                        .extras
                        .push((file_num, ExtraObj::Stream(file_dict, compressed.clone())));
                    let mut new_desc = desc.clone();
                    new_desc.insert(
                        PdfName::new("FontFile2"),
                        PdfObject::Ref(ObjectId(file_num, 0)),
                    );
                    let ren = renumber(&PdfObject::Dict(new_desc), map);
                    edits.transforms.insert(*desc_ref, ren);
                }
            }
        }
        if let Some(PdfObject::Array(kids)) = dict.get("Kids").map(|o| deref(source, o)).as_ref() {
            for kid in kids {
                if let PdfObject::Ref(r) = kid {
                    stack.push(*r);
                }
            }
        }
    }
    Ok(())
}

fn deref(file: &PdfFile, obj: &PdfObject) -> PdfObject {
    match obj {
        PdfObject::Ref(r) => file.resolve(*r).unwrap_or(PdfObject::Null),
        other => other.clone(),
    }
}

/// Build a minimal `pdfaid` XMP packet declaring the given PDF/A part.
fn build_pdfa_xmp(profile: PdfaProfile) -> Vec<u8> {
    let (part, conf) = match profile {
        PdfaProfile::A1b => ("1", "B"),
        PdfaProfile::A2b => ("2", "B"),
    };
    let xmp = format!(
        "<?xpacket begin=\"\u{FEFF}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
         <x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n\
         <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n\
         <rdf:Description rdf:about=\"\"\n\
         xmlns:pdfaid=\"http://www.aiim.org/pdfa/ns/id/\">\n\
         <pdfaid:part>{part}</pdfaid:part>\n\
         <pdfaid:conformance>{conf}</pdfaid:conformance>\n\
         </rdf:Description>\n\
         </rdf:RDF>\n\
         </x:xmpmeta>\n\
         <?xpacket end=\"w\"?>\n"
    );
    xmp.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xmp_declares_part_and_conformance() {
        let xmp = build_pdfa_xmp(PdfaProfile::A1b);
        let s = String::from_utf8(xmp).unwrap();
        assert!(s.contains("<pdfaid:part>1</pdfaid:part>"));
        assert!(s.contains("<pdfaid:conformance>B</pdfaid:conformance>"));
        assert!(s.contains("http://www.aiim.org/pdfa/ns/id/"));
        let xmp2 = build_pdfa_xmp(PdfaProfile::A2b);
        assert!(String::from_utf8(xmp2)
            .unwrap()
            .contains("<pdfaid:part>2</pdfaid:part>"));
    }

    #[test]
    fn srgb_icc_fixture_is_present() {
        // A valid ICC profile carries the 'acsp' signature at offset 36.
        assert!(
            SRGB_ICC.len() > 100,
            "embedded sRGB ICC fixture looks empty"
        );
        assert_eq!(&SRGB_ICC[36..40], b"acsp", "ICC signature missing");
    }
}
