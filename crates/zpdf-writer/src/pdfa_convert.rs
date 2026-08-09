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
//! For PDF/A-3b, [`synthesize_af_for_a3`] additionally synthesizes a catalog
//! `/AF` array (with `/AFRelationship /Unspecified`) for any existing
//! `/Names /EmbeddedFiles` entries that lack one, so the output passes the
//! PDF/A-3 associated-files rules.
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
/// the `GTS_PDFA1` output intent when the caller supplies no profile. Kept
/// inside this crate (not borrowed from `zpdf-color`'s test data) so the
/// published `zpdf-writer` package is self-contained — `cargo publish` only
/// ships files within the crate, and `include_bytes!` of a sibling crate's
/// path would break the published build.
const SRGB_ICC: &[u8] = include_bytes!("srgb.icc");

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
            PdfaProfile::A2b | PdfaProfile::A3b => "%PDF-1.7",
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

    // --- A-3b: synthesize a catalog /AF array (with /AFRelationship) for any
    // existing /Names /EmbeddedFiles entries, so the rewritten output passes
    // PDF/A-3's associated-files rules. Patches `cat` (adds /AF) and registers
    // filespec transforms (adds /AFRelationship) — must run before the catalog
    // transform is committed below. ---
    if cfg.profile == PdfaProfile::A3b {
        synthesize_af_for_a3(source, map, root, &mut cat, &mut edits.transforms);
    }

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

    // --- Annotation cleanup: drop PDF/A-forbidden annotation subtypes and
    // any annotation whose /A action is a JavaScript or Launch action. The
    // forbidden set is profile-aware (A-1b also drops FileAttachment, which
    // carries an embedded file A-1b forbids; A-2b permits it). ---
    strip_forbidden_annotations(source, map, root, cfg.profile, &mut edits.transforms);

    // --- Font embedding (best-effort fallback) ---
    if let Some(fallback) = &cfg.fallback_font {
        embed_fallback_fonts(source, map, root, fallback, &mut next_extra, &mut edits)?;
    }

    Ok(edits)
}

/// Annotation subtypes PDF/A forbids outright in both profiles (PDF/A-1
/// §6.6.3; PDF/A-2 carries the same exclusions for these interactive /
/// multimedia types). These reference non-embedded external content or carry
/// scripting — neither survives long-term archival. Widget annotations
/// (form fields) are NOT stripped: PDF/A permits AcroForm fields with
/// properly embedded appearance fonts.
const FORBIDDEN_ANNOT_SUBTYPES_BOTH: &[&str] = &["3D", "Sound", "Movie"];

/// Annotation subtypes PDF/A-1b additionally forbids. FileAttachment carries
/// an embedded file, which A-1b disallows (A-2b permits embedded files).
const FORBIDDEN_ANNOT_SUBTYPES_A1B: &[&str] = &["FileAttachment"];

/// Walk the page tree and, for each page's `/Annots`, drop annotations whose
/// `/Subtype` is forbidden (profile-aware) or whose `/A` action is a
/// JavaScript/Launch action. Edits the page dict (renumbered) only when at
/// least one annotation is removed, leaving unfiltered pages untouched.
fn strip_forbidden_annotations(
    source: &PdfFile,
    map: &HashMap<ObjectId, u32>,
    root: ObjectId,
    profile: PdfaProfile,
    transforms: &mut HashMap<ObjectId, PdfObject>,
) {
    let Ok(catalog) = source.resolve(root).and_then(|o| o.as_dict().cloned()) else {
        return;
    };
    let Ok(pages_root) = catalog.get_ref("Pages") else {
        return;
    };
    let a1b_extra = profile == PdfaProfile::A1b;
    let mut stack = vec![pages_root];
    let mut visited = std::collections::HashSet::new();
    while let Some(node) = stack.pop() {
        if !visited.insert(node) {
            continue;
        }
        let Ok(dict) = source.resolve(node).and_then(|o| o.as_dict().cloned()) else {
            continue;
        };
        if let Some(PdfObject::Array(kids)) = dict.get("Kids").map(|o| deref(source, o)).as_ref() {
            for kid in kids {
                if let PdfObject::Ref(r) = kid {
                    stack.push(*r);
                }
            }
        }
        let annots_obj = dict.get("Annots").map(|o| deref(source, o));
        let Some(PdfObject::Array(annots)) = annots_obj.as_ref() else {
            continue;
        };
        let mut kept: Vec<PdfObject> = Vec::with_capacity(annots.len());
        let mut removed = false;
        for a in annots {
            let resolved = deref(source, a);
            let PdfObject::Dict(ad) = &resolved else {
                kept.push(a.clone());
                continue;
            };
            let subtype = ad.get_name("Subtype").unwrap_or("");
            let forbidden = FORBIDDEN_ANNOT_SUBTYPES_BOTH.contains(&subtype)
                || (a1b_extra && FORBIDDEN_ANNOT_SUBTYPES_A1B.contains(&subtype));
            if forbidden {
                removed = true;
                continue;
            }
            // An annotation /A action that is JavaScript/Launch is forbidden.
            if let Some(PdfObject::Dict(action)) = ad.get("A").map(|o| deref(source, o)).as_ref() {
                let s = action.get_name("S").unwrap_or("");
                if s == "JavaScript" || s == "Launch" {
                    removed = true;
                    continue;
                }
            }
            kept.push(a.clone());
        }
        if removed {
            let mut d = dict.clone();
            if kept.is_empty() {
                d.0.remove(&PdfName::new("Annots"));
            } else {
                d.insert(PdfName::new("Annots"), PdfObject::Array(kept));
            }
            let ren = renumber(&PdfObject::Dict(d), map);
            transforms.insert(node, ren);
        }
    }
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
/// but satisfies PDF/A's embedding requirement). Type3 fonts are skipped
/// (exempt). Type0/CID fonts get the fallback embedded on their descendant
/// CIDFont's `/FontDescriptor` as `/FontFile2`, with `/CIDToGIDMap /Identity`
/// and a default `/W` — the same best-effort semantics (embedding satisfied,
/// glyphs may not match). Now that composite-font authoring exists, this
/// closes the last unembedded-font gap for PDF/A.
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
                    if subtype == "Type3" {
                        continue;
                    }
                    if subtype == "Type0" {
                        // Descendant CIDFont: its /FontDescriptor is where the
                        // program is embedded. Best-effort — embed the fallback
                        // and set /CIDToGIDMap /Identity so the CID→GID path is
                        // valid even if glyphs don't match the original.
                        embed_type0_fallback(
                            source,
                            map,
                            &font,
                            fallback,
                            &compressed,
                            next_extra,
                            &mut seen,
                            edits,
                        );
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

/// Embed the fallback font program on a non-embedded Type0 font's descendant
/// CIDFont, satisfying PDF/A's embedding requirement. Best-effort: the
/// fallback's glyphs need not match the original. Allocates a `/FontFile2`
/// stream for the program and patches the descendant CIDFont dict with
/// `/FontFile2` on its `/FontDescriptor` and `/CIDToGIDMap /Identity` (plus a
/// default `/W`/`/DW` when absent) so the CID→GID path is valid. Skips fonts
/// that already embed a program or have no resolvable descendant/descriptor.
#[allow(clippy::too_many_arguments)]
fn embed_type0_fallback(
    source: &PdfFile,
    map: &HashMap<ObjectId, u32>,
    font: &PdfDict,
    fallback: &[u8],
    compressed: &[u8],
    next_extra: &mut u32,
    seen: &mut std::collections::HashSet<ObjectId>,
    edits: &mut PdfaEdits,
) {
    // /DescendantFonts [cidfont_ref]
    let descs = font.get("DescendantFonts").map(|o| deref(source, o));
    let Some(PdfObject::Array(descs)) = descs.as_ref() else {
        return;
    };
    let Some(PdfObject::Ref(cid_ref)) = descs.first() else {
        return;
    };
    let Ok(cid) = source.resolve(*cid_ref).and_then(|o| o.as_dict().cloned()) else {
        return;
    };
    let Some(PdfObject::Ref(desc_ref)) = cid.get("FontDescriptor") else {
        return;
    };
    if !seen.insert(*desc_ref) {
        return;
    }
    let Ok(desc) = source.resolve(*desc_ref).and_then(|o| o.as_dict().cloned()) else {
        return;
    };
    let already_embedded = desc.get("FontFile").is_some()
        || desc.get("FontFile2").is_some()
        || desc.get("FontFile3").is_some();
    if already_embedded {
        return;
    }

    // Allocate the fallback program stream.
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
        .push((file_num, ExtraObj::Stream(file_dict, compressed.to_vec())));

    // Patch the FontDescriptor with /FontFile2.
    let mut new_desc = desc.clone();
    new_desc.insert(
        PdfName::new("FontFile2"),
        PdfObject::Ref(ObjectId(file_num, 0)),
    );
    let ren = renumber(&PdfObject::Dict(new_desc), map);
    edits.transforms.insert(*desc_ref, ren);

    // Patch the descendant CIDFont: /CIDToGIDMap /Identity + a default /DW
    // when absent (so the read side resolves glyph metrics without the
    // original, possibly-absent /W). Preserve any existing /W.
    let mut new_cid = cid.clone();
    new_cid.insert(
        PdfName::new("CIDToGIDMap"),
        PdfObject::Name(PdfName::new("Identity")),
    );
    if new_cid.get("DW").is_none() && new_cid.get("W").is_none() {
        new_cid.insert(PdfName::new("DW"), PdfObject::Integer(1000));
    }
    let ren = renumber(&PdfObject::Dict(new_cid), map);
    edits.transforms.insert(*cid_ref, ren);
}

/// PDF/A-3 associated-files synthesis. PDF/A-3 (ISO 19005-3 §6.2.4) requires
/// every embedded file in `/Names /EmbeddedFiles` to be referenced from a
/// catalog/page `/AF` array, and every `/AF` file specification to carry an
/// `/AFRelationship`. When converting an existing document that has embedded
/// files but no `/AF`, this synthesizes one so the rewritten output passes
/// `zpdf_document::pdfa::validate --profile pdfa-3b`:
///
/// - Walks the OLD catalog's `/Names /EmbeddedFiles` name tree (bounded by
///   depth and a visited set) collecting filespec **references**.
/// - Builds a renumbered `/AF` array pointing at those filespecs' new object
///   numbers (assigned by the reachability walk, which already followed
///   `/Names`).
/// - For each filespec whose resolved dict lacks `/AFRelationship`, registers a
///   transform adding `/AFRelationship /Unspecified` (renumbered).
/// - Inserts `/AF` into the (already-renumbered) catalog `cat`.
///
/// Best-effort: inline (non-ref) filespecs are skipped (their new numbers are
/// not known); a catalog that already carries `/AF` is left untouched (a
/// producer's relationships are not clobbered).
fn synthesize_af_for_a3(
    source: &PdfFile,
    map: &HashMap<ObjectId, u32>,
    root: ObjectId,
    cat: &mut PdfDict,
    transforms: &mut HashMap<ObjectId, PdfObject>,
) {
    // Resolve the OLD catalog fresh (the caller has already moved catalog_old
    // into the renumbered `cat`).
    let Ok(catalog) = source.resolve(root).and_then(|o| o.as_dict().cloned()) else {
        return;
    };
    // Don't clobber an existing /AF.
    if catalog.get("AF").is_some() {
        return;
    }
    let names_obj = catalog.get("Names").map(|o| deref(source, o));
    let Some(PdfObject::Dict(names)) = names_obj.as_ref() else {
        return;
    };
    let tree_root_ref = match names.get("EmbeddedFiles") {
        Some(PdfObject::Ref(r)) => Some(*r),
        // An inline name-tree root: walk it directly without a visited-set seed.
        Some(PdfObject::Dict(_)) => None,
        _ => return,
    };
    let names_embedded = names.get("EmbeddedFiles").cloned();
    let Some(tree_root) = resolve_dict(source, names_embedded.as_ref()) else {
        return;
    };

    // Walk the name tree collecting filespec references (the value slot of
    // each /Names pair). Bounded by depth + a visited set (mirrors
    // zpdf_document::embedded_files::walk_name_tree).
    let mut filespec_refs: Vec<ObjectId> = Vec::new();
    let mut visited = std::collections::HashSet::new();
    if let Some(seed) = tree_root_ref {
        visited.insert(seed);
    }
    walk_name_tree_for_filespecs(source, &tree_root, &mut filespec_refs, &mut visited, 0);

    if filespec_refs.is_empty() {
        return;
    }

    // Build the renumbered /AF array from the filespecs the walk reached.
    let mut af: Vec<PdfObject> = Vec::with_capacity(filespec_refs.len());
    for fs_ref in &filespec_refs {
        match map.get(fs_ref) {
            Some(&n) => af.push(PdfObject::Ref(ObjectId(n, 0))),
            // An unreached filespec (not in the walk's map) cannot be
            // referenced; skip it rather than emit a dangling ref.
            None => continue,
        }
    }
    if af.is_empty() {
        return;
    }

    // For each filespec lacking /AFRelationship, add a transform patching it in.
    for fs_ref in &filespec_refs {
        let Ok(fs_dict) = source.resolve(*fs_ref).and_then(|o| o.as_dict().cloned()) else {
            continue;
        };
        if fs_dict.get("AFRelationship").is_some() {
            continue;
        }
        let mut patched = fs_dict;
        patched.insert(
            PdfName::new("AFRelationship"),
            PdfObject::Name(PdfName::new("Unspecified")),
        );
        let ren = renumber(&PdfObject::Dict(patched), map);
        transforms.insert(*fs_ref, ren);
    }

    cat.insert(PdfName::new("AF"), PdfObject::Array(af));
}

/// Walk a `/Names /EmbeddedFiles` name-tree node, appending the filespec
/// **references** (the value slot of each `/Names` pair) to `out`. Leaf nodes
/// carry `/Names [key0 val0 …]`; interior nodes carry `/Kids [refs]`. Bounded
/// by depth and a per-reference visited set so a cyclic tree terminates.
fn walk_name_tree_for_filespecs(
    source: &PdfFile,
    node: &PdfDict,
    out: &mut Vec<ObjectId>,
    visited: &mut std::collections::HashSet<ObjectId>,
    depth: usize,
) {
    if depth > 64 || out.len() >= 16_384 {
        return;
    }
    // Leaf entries: alternating (name-string, filespec) pairs — collect the
    // value when it is an indirect reference.
    if let Some(names) = resolve_array(source, node.get("Names")) {
        let mut i = 1; // start at the first value (index 1)
        while i < names.len() {
            if let PdfObject::Ref(r) = &names[i] {
                out.push(*r);
            }
            i += 2;
        }
        if out.len() >= 16_384 {
            return;
        }
    }
    // Interior children.
    if let Some(kids) = resolve_array(source, node.get("Kids")) {
        for kid in &kids {
            let kid_dict = match kid {
                PdfObject::Ref(r) => {
                    if !visited.insert(*r) {
                        continue;
                    }
                    resolve_dict(source, Some(kid))
                }
                PdfObject::Dict(_) => resolve_dict(source, Some(kid)),
                _ => None,
            };
            if let Some(d) = kid_dict {
                walk_name_tree_for_filespecs(source, &d, out, visited, depth + 1);
            }
        }
    }
}

/// Resolve a dictionary value that may be direct or indirect (a helper local
/// to the AF synthesis, mirroring zpdf_document::embedded_files::resolve_dict).
fn resolve_dict(file: &PdfFile, obj: Option<&PdfObject>) -> Option<PdfDict> {
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

/// Resolve an array value that may be direct or indirect.
fn resolve_array(file: &PdfFile, obj: Option<&PdfObject>) -> Option<Vec<PdfObject>> {
    match obj? {
        PdfObject::Array(a) => Some(a.clone()),
        PdfObject::Ref(r) => match file.resolve(*r).ok()? {
            PdfObject::Array(a) => Some(a),
            _ => None,
        },
        _ => None,
    }
}

/// Build a minimal `pdfaid` XMP packet declaring the given PDF/A part.
fn build_pdfa_xmp(profile: PdfaProfile) -> Vec<u8> {
    let (part, conf) = match profile {
        PdfaProfile::A1b => ("1", "B"),
        PdfaProfile::A2b => ("2", "B"),
        PdfaProfile::A3b => ("3", "B"),
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
        let xmp3 = build_pdfa_xmp(PdfaProfile::A3b);
        assert!(String::from_utf8(xmp3)
            .unwrap()
            .contains("<pdfaid:part>3</pdfaid:part>"));
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
