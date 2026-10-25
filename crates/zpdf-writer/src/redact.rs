//! True redaction: excise content under regions from a page's content stream.
//!
//! [`redact_pdf`] is the destructive, end-to-end entry point: it performs the
//! excision below and then rewrites the whole document, so superseded object
//! revisions (including the original page content) are not present in the
//! returned bytes. [`IncrementalWriter::redact_page`] only updates the current
//! revision and leaves the original bytes in place; use it only when a later
//! [`crate::rewrite_pdf`] pass will garbage-collect the file.
//!
//! Unlike a cosmetic black box, [`IncrementalWriter::redact_page`] rewrites
//! the page's content stream(s) with the matching operators **removed**:
//! - text-show ops (`Tj`, `'`, `"`, `TJ`) whose glyph extent intersects a
//!   redaction rect are dropped;
//! - XObject draws (`Do`) and inline images (`BI…EI`) whose image footprint or
//!   transformed Form bounding box intersects a rect are dropped;
//! - path paint ops (`S`/`f`/`B`…) whose accumulated path bbox intersects a
//!   rect are downgraded to `n` (no-op paint), keeping any clip (`W`) intact.
//!
//! A filled box is (optionally) drawn over each region afterwards.
//!
//! Text that could reproduce removed content is scrubbed as well: `/ActualText`,
//! `/Alt` and `/E` are dropped from marked-content sequences (and from the
//! structure elements that own their MCIDs) that enclosed a removed operator,
//! XObjects that are no longer drawn are dropped from the page's resources,
//! and removed annotations are replaced by `null` so neither `/AcroForm` nor
//! the structure tree keeps them reachable.
//!
//! The tracker mirrors just enough interpreter state to place operators in
//! user space: `cm` (CTM with q/Q stack), `BT`/`ET`, `Tm`/`Td`/`TD`/`T*`/`TL`,
//! font size for a glyph-height estimate. Text extents are approximate: custom
//! widths, spacing and writing modes can differ from this tracker. Inspect
//! extracted text and rendered output before distributing sensitive documents.

use std::collections::HashSet;
use std::io::Cursor;

use zpdf_content::tokenizer::{ContentToken, ContentTokenizer};
use zpdf_core::{Matrix, ObjectId, PdfDict, PdfName, PdfObject, Rect, Result};
use zpdf_document::PdfDocument;

use crate::rewrite::{rewrite_pdf, RewriteOptions};
use crate::{invalid_data, IncrementalWriter};

/// Options for a redaction pass.
#[derive(Debug, Clone)]
pub struct RedactOptions {
    /// Draw an opaque box of this color over each redacted region.
    pub fill: Option<(f64, f64, f64)>,
}

impl Default for RedactOptions {
    fn default() -> Self {
        Self {
            fill: Some((0.0, 0.0, 0.0)),
        }
    }
}

impl IncrementalWriter {
    /// Remove page content that intersects any of `rects` (PDF user space,
    /// y-up), then optionally paint a fill box over each region.
    ///
    /// Text inside Form XObjects is not descended into (the whole XObject is
    /// dropped if its placement intersects); annotations overlapping a region
    /// are removed from the page.
    pub fn redact_page(
        &mut self,
        page_index: usize,
        rects: &[Rect],
        options: &RedactOptions,
    ) -> Result<()> {
        if rects.is_empty() {
            return Ok(());
        }
        for r in rects {
            if ![r.x0, r.y0, r.x1, r.y1].iter().all(|v| v.is_finite()) {
                return Err(invalid_data("redaction rectangles must be finite").into());
            }
        }
        let rects: Vec<Rect> = rects.iter().map(|r| r.normalize()).collect();

        let page_id = self.page_id(page_index)?;
        let page_obj = self.resolve_current(page_id)?;
        let mut page_dict = page_obj.as_dict()?.clone();

        // Collect the decoded content bytes of every /Contents stream.
        let content_ids = self.current_content_refs(&page_dict)?;
        let content = self.current_content_bytes(&content_ids)?;

        // The page's effective resources: named marked-content property lists
        // are looked up in them, and undrawn XObjects are dropped from them.
        let resources_obj = match page_dict.get("Resources") {
            Some(obj) => Some(obj.clone()),
            None => self.find_inherited(page_id, "Resources")?,
        };
        let mut resources = match resources_obj.map(|o| self.deref_current(&o)) {
            Some(PdfObject::Dict(d)) => Some(d),
            _ => None,
        };
        let properties = resources
            .as_ref()
            .and_then(|r| r.get("Properties"))
            .map(|p| self.deref_current(p));
        let lookup_properties = |name: &str| match &properties {
            Some(PdfObject::Dict(props)) => match props.get(name).map(|p| self.deref_current(p)) {
                Some(PdfObject::Dict(d)) => Some(d),
                _ => None,
            },
            _ => None,
        };

        // Filter the combined stream.
        let xobjects = resources
            .as_ref()
            .and_then(|r| r.get("XObject"))
            .map(|x| self.deref_current(x));
        let lookup_xobject = |name: &str, ctm: &Matrix| {
            let xobjects = xobjects.as_ref()?.as_dict().ok()?;
            let obj = self.deref_current(xobjects.get(name)?);
            let dict = &obj.as_stream().ok()?.dict;
            match dict.get_name("Subtype").ok()? {
                "Image" => Some(unit_square_extent(ctm)),
                "Form" => {
                    let bounds = self.deref_current(dict.get("BBox")?);
                    let [x0, y0, x1, y1] = finite_numbers(&bounds)?;
                    let matrix = match dict.get("Matrix") {
                        Some(value) => {
                            let [a, b, c, d, e, f] = finite_numbers(&self.deref_current(value))?;
                            Matrix::new(a, b, c, d, e, f)
                        }
                        None => Matrix::identity(),
                    };
                    let transform = ctm.concat(&matrix);
                    let extent = rect_extent(Rect::new(x0, y0, x1, y1), &transform);
                    [extent.x0, extent.y0, extent.x1, extent.y1]
                        .iter()
                        .all(|v| v.is_finite())
                        .then_some(extent)
                }
                _ => None,
            }
        };
        let outcome = filter_content(&content, &rects, &lookup_properties, &lookup_xobject);
        let mut filtered = outcome.content;

        // Optional fill boxes, appended in a fresh graphics state.
        if let Some((r, g, b)) = options.fill {
            filtered.extend_from_slice(b"q\n");
            filtered.extend_from_slice(format!("{} {} {} rg\n", r, g, b).as_bytes());
            for rect in &rects {
                filtered.extend_from_slice(
                    format!(
                        "{} {} {} {} re f\n",
                        rect.x0,
                        rect.y0,
                        rect.x1 - rect.x0,
                        rect.y1 - rect.y0
                    )
                    .as_bytes(),
                );
            }
            filtered.extend_from_slice(b"Q\n");
        }

        // Replace the page's contents with a single new stream.
        let stream_ref = self.try_add_flate_stream(&PdfDict::new(), &filtered)?;
        page_dict.insert(
            PdfName::new("Contents"),
            PdfObject::Ref(ObjectId(stream_ref.0, stream_ref.1 as u16)),
        );

        // An XObject whose every draw was removed, or a named property list
        // whose every use was replaced by a scrubbed inline copy, must not stay
        // reachable through the resources, or a full rewrite would keep the
        // redacted image/form or replacement text. Give the page its own copy
        // so other pages sharing the dictionary keep theirs.
        if let Some(res) = resources.as_mut() {
            let mut pruned = false;
            for (category, dropped) in [
                ("XObject", &outcome.dropped_xobjects),
                ("Properties", &outcome.dropped_properties),
            ] {
                if dropped.is_empty() {
                    continue;
                }
                let entries = res.get(category).map(|x| self.deref_current(x));
                if let Some(PdfObject::Dict(mut entries)) = entries {
                    let before = entries.0.len();
                    entries.0.retain(|name, _| !dropped.contains(name.as_str()));
                    if entries.0.len() != before {
                        res.insert(PdfName::new(category), PdfObject::Dict(entries));
                        pruned = true;
                    }
                }
            }
            if pruned {
                page_dict.insert(PdfName::new("Resources"), PdfObject::Dict(res.clone()));
            }
        }
        // The thumbnail is a raster of the unredacted page.
        page_dict.0.remove(&PdfName::new("Thumb"));

        // Remove annotations whose /Rect intersects a redaction region (their
        // appearance may reproduce redacted content).
        let mut removed_annots = HashSet::new();
        if let Some(annots_obj) = page_dict.get("Annots").cloned() {
            let annots = match &annots_obj {
                PdfObject::Ref(r) => self
                    .resolve_current(*r)
                    .ok()
                    .and_then(|o| o.as_array().ok().map(|a| a.to_vec()))
                    .unwrap_or_default(),
                PdfObject::Array(a) => a.to_vec(),
                _ => Vec::new(),
            };
            let kept: Vec<PdfObject> = annots
                .into_iter()
                .filter(|a| {
                    let PdfObject::Ref(r) = a else { return true };
                    let Ok(obj) = self.resolve_current(*r) else {
                        return true;
                    };
                    let Ok(dict) = obj.as_dict() else { return true };
                    let keep = match annot_rect(dict) {
                        Some(ar) => !rects.iter().any(|rr| intersects(&ar, rr)),
                        None => true,
                    };
                    if !keep {
                        removed_annots.insert(*r);
                    }
                    keep
                })
                .collect();
            page_dict.insert(PdfName::new("Annots"), PdfObject::Array(kept));
        }

        self.overwrite_object(page_id, PdfObject::Dict(page_dict));

        // A removed annotation can still be referenced from /AcroForm /Fields,
        // a parent field's /Kids, a popup or the structure tree; any of those
        // would keep its appearance and value alive. Replace it with null and
        // unlink it from the form tree.
        // The page edit is already queued, so these clean-ups skip whatever
        // they cannot resolve instead of failing half-way.
        for id in &removed_annots {
            self.unlink_removed_widget(*id);
            self.overwrite_object(*id, PdfObject::Null);
        }
        self.scrub_structure(page_id, &outcome.redacted_mcids, &removed_annots);
        Ok(())
    }

    /// Drop a removed widget annotation from the form tree. A parent field
    /// left without kids holds the value the widget displayed (`/V`, `/DV`,
    /// `/RV`), so it is unlinked and replaced with null in turn, up the tree.
    /// A field that keeps other widgets keeps its value: they still show it.
    fn unlink_removed_widget(&mut self, annot: ObjectId) {
        /// Bound on field-tree levels climbed (cyclic `/Parent` chains).
        const MAX_FIELD_DEPTH: usize = 64;
        let mut node = annot;
        let mut seen = HashSet::new();
        while seen.len() < MAX_FIELD_DEPTH && seen.insert(node) {
            let parent = self.unlink_field_node(node);
            if node != annot {
                self.overwrite_object(node, PdfObject::Null);
            }
            match parent {
                Some(parent) if self.has_no_kids(parent) => node = parent,
                _ => return,
            }
        }
    }

    /// Remove `node` from its parent field's `/Kids`, or from `/AcroForm
    /// /Fields` when it is a top-level field. Returns the parent field.
    fn unlink_field_node(&mut self, node: ObjectId) -> Option<ObjectId> {
        let parent = match self.resolve_current(node) {
            Ok(PdfObject::Dict(d)) => d.get("Parent").cloned(),
            _ => return None,
        };
        let (owner, key) = match parent {
            Some(PdfObject::Ref(p)) => (p, "Kids"),
            _ => {
                let Ok(PdfObject::Dict(mut catalog)) = self.resolve_current(self.catalog_ref())
                else {
                    return None;
                };
                match catalog.get("AcroForm").cloned() {
                    Some(PdfObject::Ref(f)) => (f, "Fields"),
                    Some(PdfObject::Dict(mut form)) => {
                        if remove_ref_from(&mut form, "Fields", node, self) {
                            catalog.insert(PdfName::new("AcroForm"), PdfObject::Dict(form));
                            self.overwrite_object(self.catalog_ref(), PdfObject::Dict(catalog));
                        }
                        return None;
                    }
                    _ => return None,
                }
            }
        };
        let Ok(PdfObject::Dict(mut dict)) = self.resolve_current(owner) else {
            return None;
        };
        if remove_ref_from(&mut dict, key, node, self) {
            self.overwrite_object(owner, PdfObject::Dict(dict));
        }
        (key == "Kids").then_some(owner)
    }

    /// Whether field `id` has no `/Kids` entries left (or none at all).
    fn has_no_kids(&self, id: ObjectId) -> bool {
        let Ok(PdfObject::Dict(dict)) = self.resolve_current(id) else {
            return false;
        };
        match dict.get("Kids").map(|k| self.deref_current(k)) {
            None => true,
            Some(PdfObject::Array(kids)) => kids.is_empty(),
            Some(_) => false,
        }
    }

    /// Remove `/ActualText`, `/Alt` and `/E` from every structure element that
    /// owns a redacted MCID on `page` or references a removed annotation, and
    /// from its ancestors (whose replacement text spans the same content).
    fn scrub_structure(
        &mut self,
        page: ObjectId,
        mcids: &HashSet<i64>,
        objects: &HashSet<ObjectId>,
    ) {
        /// Bound on structure-tree entries visited (adversarial trees).
        const MAX_STRUCT_ENTRIES: usize = 1 << 20;
        if mcids.is_empty() && objects.is_empty() {
            return;
        }
        let Ok(PdfObject::Dict(catalog)) = self.resolve_current(self.catalog_ref()) else {
            return;
        };
        let root = match catalog.get("StructTreeRoot").map(|r| self.deref_current(r)) {
            Some(PdfObject::Dict(d)) => d,
            _ => return,
        };

        // Indirect structure elements seen so far, each with its parent's index.
        let mut elems: Vec<(ObjectId, Option<usize>)> = Vec::new();
        let mut hits: Vec<usize> = Vec::new();
        let mut visited = HashSet::new();
        // (kid entry, effective /Pg, index of the owning element)
        let mut stack: Vec<(PdfObject, Option<ObjectId>, Option<usize>)> = root
            .get("K")
            .map(|k| vec![(k.clone(), None, None)])
            .unwrap_or_default();
        let mut budget = MAX_STRUCT_ENTRIES;
        while let Some((kid, pg, owner)) = stack.pop() {
            if budget == 0 {
                break;
            }
            budget -= 1;
            let (id, obj) = match kid {
                PdfObject::Ref(r) => {
                    if !visited.insert(r) {
                        continue;
                    }
                    (Some(r), self.resolve_current(r).unwrap_or(PdfObject::Null))
                }
                other => (None, other),
            };
            match obj {
                PdfObject::Integer(mcid) => {
                    if pg == Some(page) && mcids.contains(&mcid) {
                        hits.extend(owner);
                    }
                }
                PdfObject::Array(items) => {
                    stack.extend(items.into_iter().map(|k| (k, pg, owner)));
                }
                PdfObject::Dict(d) => {
                    let own_pg = match d.get("Pg") {
                        Some(PdfObject::Ref(p)) => Some(*p),
                        _ => pg,
                    };
                    match d.get_name("Type") {
                        Ok("MCR") => {
                            let hit = matches!(d.get("MCID"), Some(PdfObject::Integer(m)) if mcids.contains(m));
                            if hit && own_pg == Some(page) {
                                hits.extend(owner);
                            }
                        }
                        Ok("OBJR") => {
                            if matches!(d.get("Obj"), Some(PdfObject::Ref(o)) if objects.contains(o))
                            {
                                hits.extend(owner);
                            }
                        }
                        // A structure element. Direct (inline) elements cannot be
                        // rewritten on their own; their kids are still walked.
                        _ => {
                            let index = match id {
                                Some(id) => {
                                    elems.push((id, owner));
                                    Some(elems.len() - 1)
                                }
                                None => owner,
                            };
                            if let Some(k) = d.get("K") {
                                stack.push((k.clone(), own_pg, index));
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        let mut scrub = HashSet::new();
        for hit in hits {
            let mut at = Some(hit);
            while let Some(i) = at {
                if !scrub.insert(elems[i].0) {
                    break; // this ancestor chain is already queued
                }
                at = elems[i].1;
            }
        }
        for id in scrub {
            let Ok(PdfObject::Dict(mut dict)) = self.resolve_current(id) else {
                continue;
            };
            if strip_sensitive_keys(&mut dict) {
                self.overwrite_object(id, PdfObject::Dict(dict));
            }
        }
    }
}

/// Marked-content / structure keys whose text stands in for the enclosed
/// content (ISO 32000-1 §14.9.3–14.9.5): replacement text, alternate
/// description and abbreviation expansion.
const SENSITIVE_KEYS: [&str; 3] = ["ActualText", "Alt", "E"];

fn strip_sensitive_keys(dict: &mut PdfDict) -> bool {
    let mut changed = false;
    for key in SENSITIVE_KEYS {
        changed |= dict.0.remove(&PdfName::new(key)).is_some();
    }
    changed
}

/// Remove `target` from the (possibly indirect) array under `key`. An
/// indirect array is rewritten in place. Returns whether `dict` itself changed.
fn remove_ref_from(
    dict: &mut PdfDict,
    key: &str,
    target: ObjectId,
    writer: &mut IncrementalWriter,
) -> bool {
    let not_target = |o: &PdfObject| !matches!(o, PdfObject::Ref(r) if *r == target);
    match dict.get(key).cloned() {
        Some(PdfObject::Array(mut items)) => {
            let before = items.len();
            items.retain(not_target);
            if items.len() == before {
                return false;
            }
            dict.insert(PdfName::new(key), PdfObject::Array(items));
            true
        }
        Some(PdfObject::Ref(r)) => {
            if let Ok(PdfObject::Array(mut items)) = writer.resolve_current(r) {
                let before = items.len();
                items.retain(not_target);
                if items.len() != before {
                    writer.overwrite_object(r, PdfObject::Array(items));
                }
            }
            false
        }
        _ => false,
    }
}

/// Destructively redact a page and return a fresh, garbage-collected PDF.
///
/// This is the safe end-to-end API: it runs [`IncrementalWriter::redact_page`],
/// then rewrites the whole document so superseded object revisions — in
/// particular the original, unredacted content stream — are not present in the
/// returned bytes. Pass an empty `password` for unencrypted inputs.
///
/// The output is **not encrypted**: the rewrite drops `/Encrypt`, and the
/// original owner password and permissions cannot be reconstructed from the
/// one password supplied here. Re-encrypt the result with
/// [`crate::rewrite_pdf`] and [`RewriteOptions::encrypt`] if required.
///
/// Only the page's region content is removed. Document-level text that is not
/// tied to a region — `/Info` and XMP metadata, bookmarks, attachments, other
/// pages — is kept as is.
pub fn redact_pdf(
    input: Vec<u8>,
    password: &[u8],
    page_index: usize,
    rects: &[Rect],
    options: &RedactOptions,
) -> Result<Vec<u8>> {
    let mut writer = IncrementalWriter::new_with_password(input, password)?;
    writer.redact_page(page_index, rects, options)?;
    let mut updated = Cursor::new(Vec::new());
    writer.write(&mut updated)?;

    // Re-open the update and serialize only the reachable object graph. The
    // original file prefix (and every superseded revision in it) is dropped.
    let doc = PdfDocument::open_with_password(updated.into_inner(), password)?;
    rewrite_pdf(doc.file(), &RewriteOptions::default())
}

fn annot_rect(dict: &PdfDict) -> Option<Rect> {
    match dict.get("Rect") {
        Some(PdfObject::Array(a)) if a.len() == 4 => {
            let mut v = [0.0f64; 4];
            for (i, o) in a.iter().enumerate() {
                v[i] = match o {
                    PdfObject::Integer(n) => *n as f64,
                    PdfObject::Real(f) => *f,
                    _ => return None,
                };
            }
            Some(Rect::new(v[0], v[1], v[2], v[3]).normalize())
        }
        _ => None,
    }
}

fn intersects(a: &Rect, b: &Rect) -> bool {
    a.x0 < b.x1 && b.x0 < a.x1 && a.y0 < b.y1 && b.y0 < a.y1
}

/// Minimal graphics/text state for placing operators in user space.
#[derive(Clone)]
struct TrackState {
    ctm: Matrix,
    text_matrix: Matrix,
    line_matrix: Matrix,
    font_size: f64,
    leading: f64,
}

impl Default for TrackState {
    fn default() -> Self {
        Self {
            ctm: Matrix::identity(),
            text_matrix: Matrix::identity(),
            line_matrix: Matrix::identity(),
            font_size: 12.0,
            leading: 0.0,
        }
    }
}

/// The filtered content plus what the follow-up cleanup needs to know.
struct FilterOutcome {
    content: Vec<u8>,
    /// MCIDs of marked-content sequences that enclosed removed content.
    redacted_mcids: HashSet<i64>,
    /// XObject names that were drawn only by removed `Do` operators.
    dropped_xobjects: HashSet<String>,
    /// `/Properties` names whose every use was replaced by a scrubbed inline
    /// copy of the property list.
    dropped_properties: HashSet<String>,
}

/// A `BDC` operator already written to the output, kept so its property list
/// can be scrubbed once the sequence turns out to enclose removed content.
struct MarkedSequence {
    /// Byte range of the serialized operator in the output.
    range: std::ops::Range<usize>,
    operands: Vec<PdfObject>,
    /// The property list, resolved from `/Properties` when given by name.
    properties: Option<PdfDict>,
    /// The `/Properties` name of a named property list.
    name: Option<String>,
    redacted: bool,
}

/// Walk `content` and re-serialize it with operators that paint into any of
/// `rects` removed. `properties` resolves a named marked-content property
/// list (`/Tag /Name BDC`) from the page's `/Properties` resources.
fn filter_content(
    content: &[u8],
    rects: &[Rect],
    properties: &dyn Fn(&str) -> Option<PdfDict>,
    xobject_extent: &dyn Fn(&str, &Matrix) -> Option<Rect>,
) -> FilterOutcome {
    let mut out = Vec::with_capacity(content.len());
    let mut tokenizer = ContentTokenizer::new(content);
    let mut operands: Vec<PdfObject> = Vec::new();

    let mut state = TrackState::default();
    let mut stack: Vec<TrackState> = Vec::new();

    // Current path bbox (for path-paint suppression).
    let mut path_bbox: Option<Rect> = None;
    let mut cur_point: Option<(f64, f64)> = None;

    // Marked-content nesting: `Some(i)` indexes `sequences` for a BDC, `None`
    // stands for a BMC (no property list to scrub).
    let mut sequences: Vec<MarkedSequence> = Vec::new();
    let mut open: Vec<Option<usize>> = Vec::new();
    let mut dropped_xobjects = HashSet::new();
    let mut kept_xobjects = HashSet::new();
    let mut kept_properties = HashSet::new();

    while let Some(token) = tokenizer.next_token() {
        let removed = match token {
            ContentToken::Operand(obj) => {
                operands.push(obj);
                continue;
            }
            ContentToken::InlineImage { dict, data } => {
                // The inline image paints the CTM's unit square.
                let extent = unit_square_extent(&state.ctm);
                let removed = rects.iter().any(|r| intersects(&extent, r));
                if !removed {
                    // Keep: re-serialize the BI/ID/EI section verbatim.
                    write_inline_image(&mut out, &dict, &data);
                }
                operands.clear();
                removed
            }
            ContentToken::Operator(op) => {
                let keep = apply_operator(
                    &op,
                    &operands,
                    &mut state,
                    &mut stack,
                    &mut path_bbox,
                    &mut cur_point,
                    rects,
                    xobject_extent,
                );
                let start = out.len();
                match keep {
                    Keep::Yes => write_op(&mut out, &operands, &op),
                    Keep::No => {}
                    Keep::Replace(rep) => {
                        out.extend_from_slice(rep.as_bytes());
                        out.push(b'\n');
                    }
                }
                let removed = !matches!(keep, Keep::Yes);
                match op.as_str() {
                    "BDC" => {
                        let (props, name) = match operands.get(1) {
                            Some(PdfObject::Dict(d)) => (Some(d.clone()), None),
                            Some(PdfObject::Name(n)) => {
                                (properties(n.as_str()), Some(n.as_str().to_string()))
                            }
                            _ => (None, None),
                        };
                        sequences.push(MarkedSequence {
                            range: start..out.len(),
                            operands: std::mem::take(&mut operands),
                            properties: props,
                            name,
                            redacted: false,
                        });
                        open.push(Some(sequences.len() - 1));
                    }
                    "BMC" => open.push(None),
                    "EMC" => {
                        // A sequence can only be marked while open: once
                        // closed untouched, its operands are no longer needed
                        // and its named property list stays in use.
                        if let Some(Some(i)) = open.pop() {
                            let seq = &mut sequences[i];
                            if !seq.redacted {
                                seq.operands = Vec::new();
                                seq.properties = None;
                                kept_properties.extend(seq.name.take());
                            }
                        }
                    }
                    "Do" => {
                        if let Some(PdfObject::Name(n)) = operands.first() {
                            let set = if removed {
                                &mut dropped_xobjects
                            } else {
                                &mut kept_xobjects
                            };
                            set.insert(n.as_str().to_string());
                        }
                    }
                    _ => {}
                }
                operands.clear();
                removed
            }
        };
        if removed {
            for i in open.iter().flatten() {
                sequences[*i].redacted = true;
            }
        }
    }

    // Scrub the property lists of sequences that lost content. Splice back to
    // front so earlier byte ranges stay valid.
    let mut redacted_mcids = HashSet::new();
    let mut dropped_properties = HashSet::new();
    for seq in sequences.iter_mut().rev().filter(|s| s.redacted) {
        let name = seq.name.take();
        let inlined = seq.properties.take().is_some_and(|mut props| {
            if let Some(PdfObject::Integer(mcid)) = props.get("MCID") {
                redacted_mcids.insert(*mcid);
            }
            // Optional-content names must keep pointing at their OCG/OCMD,
            // and carry no replacement text anyway.
            let optional_content =
                matches!(seq.operands.first(), Some(PdfObject::Name(t)) if t.as_str() == "OC");
            if optional_content || !strip_sensitive_keys(&mut props) {
                return false;
            }
            // An inline property list holds direct objects only: inline the
            // scrubbed copy of a named one without its indirect values.
            props.0.retain(|_, v| !matches!(v, PdfObject::Ref(_)));
            let mut operator = Vec::new();
            let tag = seq.operands.first().cloned().unwrap_or(PdfObject::Null);
            write_op(&mut operator, &[tag, PdfObject::Dict(props)], "BDC");
            out.splice(seq.range.clone(), operator);
            true
        });
        if let Some(name) = name {
            if inlined {
                dropped_properties.insert(name);
            } else {
                kept_properties.insert(name);
            }
        }
    }
    // Sequences left open at the end still name their property list.
    kept_properties.extend(sequences.iter_mut().filter_map(|s| s.name.take()));

    dropped_xobjects.retain(|name| !kept_xobjects.contains(name));
    dropped_properties.retain(|name| !kept_properties.contains(name));
    FilterOutcome {
        content: out,
        redacted_mcids,
        dropped_xobjects,
        dropped_properties,
    }
}

enum Keep {
    Yes,
    No,
    /// Emit this replacement text instead (e.g. `n` for a suppressed paint).
    Replace(&'static str),
}

#[allow(clippy::too_many_arguments)]
fn apply_operator(
    op: &str,
    operands: &[PdfObject],
    state: &mut TrackState,
    stack: &mut Vec<TrackState>,
    path_bbox: &mut Option<Rect>,
    cur_point: &mut Option<(f64, f64)>,
    rects: &[Rect],
    xobject_extent: &dyn Fn(&str, &Matrix) -> Option<Rect>,
) -> Keep {
    let num = |i: usize| -> f64 {
        match operands.get(i) {
            Some(PdfObject::Integer(n)) => *n as f64,
            Some(PdfObject::Real(f)) => *f,
            _ => 0.0,
        }
    };

    match op {
        "q" => {
            stack.push(state.clone());
            Keep::Yes
        }
        "Q" => {
            if let Some(s) = stack.pop() {
                *state = s;
            }
            Keep::Yes
        }
        "cm" => {
            if operands.len() >= 6 {
                let m = Matrix::new(num(0), num(1), num(2), num(3), num(4), num(5));
                state.ctm = state.ctm.concat(&m);
            }
            Keep::Yes
        }

        // ---- text state ----
        "BT" => {
            state.text_matrix = Matrix::identity();
            state.line_matrix = Matrix::identity();
            Keep::Yes
        }
        "ET" => Keep::Yes,
        "Tf" => {
            state.font_size = num(1);
            Keep::Yes
        }
        "TL" => {
            state.leading = num(0);
            Keep::Yes
        }
        "Tm" => {
            if operands.len() >= 6 {
                let m = Matrix::new(num(0), num(1), num(2), num(3), num(4), num(5));
                state.text_matrix = m;
                state.line_matrix = m;
            }
            Keep::Yes
        }
        "Td" => {
            let m = Matrix::new(1.0, 0.0, 0.0, 1.0, num(0), num(1));
            state.line_matrix = state.line_matrix.concat(&m);
            state.text_matrix = state.line_matrix;
            Keep::Yes
        }
        "TD" => {
            state.leading = -num(1);
            let m = Matrix::new(1.0, 0.0, 0.0, 1.0, num(0), num(1));
            state.line_matrix = state.line_matrix.concat(&m);
            state.text_matrix = state.line_matrix;
            Keep::Yes
        }
        "T*" => {
            let m = Matrix::new(1.0, 0.0, 0.0, 1.0, 0.0, -state.leading);
            state.line_matrix = state.line_matrix.concat(&m);
            state.text_matrix = state.line_matrix;
            Keep::Yes
        }

        // ---- text showing ----
        "Tj" | "TJ" | "'" | "\"" => {
            if op == "'" || op == "\"" {
                let m = Matrix::new(1.0, 0.0, 0.0, 1.0, 0.0, -state.leading);
                state.line_matrix = state.line_matrix.concat(&m);
                state.text_matrix = state.line_matrix;
            }
            let extent = text_extent(operands, state);
            let redact = rects.iter().any(|r| intersects(&extent, r));
            // Advance the text matrix by the estimated width either way, so
            // subsequent shows on the same line stay placed.
            let width = estimate_text_width(operands, state.font_size);
            let adv = Matrix::new(1.0, 0.0, 0.0, 1.0, width, 0.0);
            state.text_matrix = state.text_matrix.concat(&adv);
            if redact {
                Keep::No
            } else {
                Keep::Yes
            }
        }

        // ---- XObjects ----
        "Do" => {
            let extent = operands
                .first()
                .and_then(|o| o.as_name().ok())
                .and_then(|name| xobject_extent(name, &state.ctm));
            // Unknown or malformed placements cannot safely be kept.
            if extent.is_none_or(|extent| rects.iter().any(|r| intersects(&extent, r))) {
                Keep::No
            } else {
                Keep::Yes
            }
        }

        // ---- paths ----
        "m" | "l" => {
            let (x, y) = user_point(num(0), num(1), &state.ctm);
            extend_bbox(path_bbox, x, y);
            *cur_point = Some((x, y));
            Keep::Yes
        }
        "c" => {
            for i in (0..6).step_by(2) {
                let (x, y) = user_point(num(i), num(i + 1), &state.ctm);
                extend_bbox(path_bbox, x, y);
            }
            Keep::Yes
        }
        "v" | "y" => {
            for i in (0..4).step_by(2) {
                let (x, y) = user_point(num(i), num(i + 1), &state.ctm);
                extend_bbox(path_bbox, x, y);
            }
            Keep::Yes
        }
        "re" => {
            let (x, y, w, h) = (num(0), num(1), num(2), num(3));
            for (px, py) in [(x, y), (x + w, y), (x, y + h), (x + w, y + h)] {
                let (ux, uy) = user_point(px, py, &state.ctm);
                extend_bbox(path_bbox, ux, uy);
            }
            Keep::Yes
        }
        "h" => Keep::Yes,

        // Path-painting: suppress (replace with `n`) when the path intersects.
        "S" | "s" | "f" | "F" | "f*" | "B" | "B*" | "b" | "b*" => {
            let hit = path_bbox
                .as_ref()
                .is_some_and(|bb| rects.iter().any(|r| intersects(bb, r)));
            *path_bbox = None;
            *cur_point = None;
            if hit {
                Keep::Replace("n")
            } else {
                Keep::Yes
            }
        }
        "n" => {
            *path_bbox = None;
            *cur_point = None;
            Keep::Yes
        }

        // Everything else passes through with state untouched.
        _ => Keep::Yes,
    }
}

fn user_point(x: f64, y: f64, ctm: &Matrix) -> (f64, f64) {
    (ctm.a * x + ctm.c * y + ctm.e, ctm.b * x + ctm.d * y + ctm.f)
}

fn extend_bbox(bbox: &mut Option<Rect>, x: f64, y: f64) {
    match bbox {
        Some(b) => {
            b.x0 = b.x0.min(x);
            b.y0 = b.y0.min(y);
            b.x1 = b.x1.max(x);
            b.y1 = b.y1.max(y);
        }
        None => *bbox = Some(Rect::new(x, y, x, y)),
    }
}

/// The user-space extent of the CTM-mapped unit square (image placement).
fn unit_square_extent(ctm: &Matrix) -> Rect {
    rect_extent(Rect::new(0.0, 0.0, 1.0, 1.0), ctm)
}

fn finite_numbers<const N: usize>(obj: &PdfObject) -> Option<[f64; N]> {
    let array = obj.as_array().ok()?;
    if array.len() != N {
        return None;
    }
    let mut numbers = [0.0; N];
    for (number, value) in numbers.iter_mut().zip(array) {
        *number = value.as_f64().ok()?;
        if !number.is_finite() {
            return None;
        }
    }
    Some(numbers)
}

fn rect_extent(rect: Rect, ctm: &Matrix) -> Rect {
    let corners = [
        user_point(rect.x0, rect.y0, ctm),
        user_point(rect.x1, rect.y0, ctm),
        user_point(rect.x0, rect.y1, ctm),
        user_point(rect.x1, rect.y1, ctm),
    ];
    let mut r = Rect::new(corners[0].0, corners[0].1, corners[0].0, corners[0].1);
    for (x, y) in corners.iter().skip(1) {
        r.x0 = r.x0.min(*x);
        r.y0 = r.y0.min(*y);
        r.x1 = r.x1.max(*x);
        r.y1 = r.y1.max(*y);
    }
    r
}

/// Approximate width of a shown string: 0.55 em per byte. This is not a
/// conservative bound for arbitrary PDF fonts or text-state settings.
fn estimate_text_width(operands: &[PdfObject], font_size: f64) -> f64 {
    let mut units = 0.0f64;
    for obj in operands {
        match obj {
            PdfObject::String(s) => units += s.0.len() as f64 * 0.55,
            PdfObject::Array(arr) => {
                for el in arr {
                    match el {
                        PdfObject::String(s) => units += s.0.len() as f64 * 0.55,
                        PdfObject::Integer(n) => units -= *n as f64 / 1000.0,
                        PdfObject::Real(f) => units -= f / 1000.0,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    units * font_size
}

/// User-space extent of a text-show op: from the current text-space origin,
/// the estimated width, and the font size for height (descender to ascender,
/// padded ±20%).
fn text_extent(operands: &[PdfObject], state: &TrackState) -> Rect {
    let trm = state.ctm.concat(&state.text_matrix);
    let width = estimate_text_width(operands, state.font_size);
    let h = state.font_size;
    let corners = [
        (0.0, -0.3 * h),
        (width, -0.3 * h),
        (0.0, 1.1 * h),
        (width, 1.1 * h),
    ];
    let mut pts = corners
        .iter()
        .map(|&(x, y)| (trm.a * x + trm.c * y + trm.e, trm.b * x + trm.d * y + trm.f));
    let first = pts.next().unwrap();
    let mut r = Rect::new(first.0, first.1, first.0, first.1);
    for (x, y) in pts {
        r.x0 = r.x0.min(x);
        r.y0 = r.y0.min(y);
        r.x1 = r.x1.max(x);
        r.y1 = r.y1.max(y);
    }
    r
}

/// Re-serialize one operator with its operands.
fn write_op(out: &mut Vec<u8>, operands: &[PdfObject], op: &str) {
    for obj in operands {
        write_operand(out, obj);
        out.push(b' ');
    }
    out.extend_from_slice(op.as_bytes());
    out.push(b'\n');
}

fn write_operand(out: &mut Vec<u8>, obj: &PdfObject) {
    // Content-stream operands are always direct objects; reuse the PDF
    // serializer via a cursor (it never fails on a Vec).
    let mut cursor = Cursor::new(Vec::new());
    let _ = crate::serialize::serialize_object_body(&mut cursor, obj);
    out.extend_from_slice(&cursor.into_inner());
}

/// Emit an inline image section (`BI <dict entries> ID <data> EI`).
fn write_inline_image(out: &mut Vec<u8>, dict: &PdfDict, data: &[u8]) {
    out.extend_from_slice(b"BI ");
    for (k, v) in &dict.0 {
        out.push(b'/');
        out.extend_from_slice(k.as_str().as_bytes());
        out.push(b' ');
        write_operand(out, v);
        out.push(b' ');
    }
    out.extend_from_slice(b"ID\n");
    out.extend_from_slice(data);
    out.extend_from_slice(b"\nEI\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(content: &[u8], rects: &[Rect]) -> Vec<u8> {
        filter_content(content, rects, &|_| None, &|_, ctm| {
            Some(unit_square_extent(ctm))
        })
        .content
    }

    #[test]
    fn filter_drops_text_inside_rect() {
        let content = b"BT /F1 12 Tf 1 0 0 1 50 700 Tm (secret) Tj ET\n\
                        BT /F1 12 Tf 1 0 0 1 50 100 Tm (public) Tj ET\n";
        let rects = [Rect::new(40.0, 690.0, 200.0, 720.0)];
        let filtered = filter(content, &rects);
        let text = String::from_utf8_lossy(&filtered);
        assert!(!text.contains("(secret)"), "redacted text removed: {text}");
        assert!(text.contains("(public)"), "other text kept: {text}");
    }

    #[test]
    fn filter_drops_image_inside_rect() {
        let content = b"q 100 0 0 100 50 500 cm /Im1 Do Q\n\
                        q 100 0 0 100 400 500 cm /Im2 Do Q\n";
        let rects = [Rect::new(40.0, 490.0, 160.0, 610.0)];
        let filtered = filter(content, &rects);
        let text = String::from_utf8_lossy(&filtered);
        assert!(!text.contains("/Im1 Do"), "hit image dropped: {text}");
        assert!(text.contains("/Im2 Do"), "clear image kept: {text}");
    }

    #[test]
    fn filter_neutralizes_path_paint_inside_rect() {
        let content = b"10 10 50 50 re f\n300 300 20 20 re f\n";
        let rects = [Rect::new(0.0, 0.0, 100.0, 100.0)];
        let filtered = filter(content, &rects);
        let text = String::from_utf8_lossy(&filtered);
        // First rect's paint is downgraded to `n`; second one keeps `f`.
        assert!(text.contains("10 10 50 50 re\nn"), "suppressed: {text}");
        assert!(text.contains("300 300 20 20 re\nf"), "kept: {text}");
    }

    #[test]
    fn q_stack_restores_ctm() {
        // Image inside q/Q with a translation must be measured with it, and
        // content after Q measured without it.
        let content = b"q 1 0 0 1 500 500 cm 50 0 0 50 0 0 cm /Im1 Do Q\n\
                        q 50 0 0 50 10 10 cm /Im2 Do Q\n";
        let rects = [Rect::new(490.0, 490.0, 600.0, 600.0)];
        let filtered = filter(content, &rects);
        let text = String::from_utf8_lossy(&filtered);
        assert!(!text.contains("/Im1"), "translated image dropped: {text}");
        assert!(text.contains("/Im2"), "origin image kept: {text}");
    }

    #[test]
    fn actual_text_of_redacted_sequence_is_scrubbed() {
        let content = b"/Span <</ActualText (TOP SECRET) /MCID 3>> BDC \
                        BT /F1 12 Tf 1 0 0 1 50 700 Tm (XXX) Tj ET EMC\n\
                        /Span <</ActualText (public) /MCID 4>> BDC \
                        BT /F1 12 Tf 1 0 0 1 50 100 Tm (public) Tj ET EMC\n";
        let rects = [Rect::new(40.0, 690.0, 200.0, 720.0)];
        let outcome = filter_content(content, &rects, &|_| None, &|_, ctm| {
            Some(unit_square_extent(ctm))
        });
        let text = String::from_utf8_lossy(&outcome.content);
        assert!(!text.contains("TOP SECRET"), "ActualText scrubbed: {text}");
        assert!(
            text.contains("/MCID 3"),
            "MCID kept for the structure: {text}"
        );
        assert!(text.contains("/ActualText (public)"), "untouched: {text}");
        assert_eq!(outcome.redacted_mcids, HashSet::from([3]));
    }

    #[test]
    fn enclosing_sequences_are_scrubbed_too() {
        let content = b"/Figure <</Alt (SECRET CHART)>> BDC /P <</E (SECRET)>> BDC \
                        q 100 0 0 100 50 500 cm /Im1 Do Q EMC EMC\n";
        let rects = [Rect::new(40.0, 490.0, 160.0, 610.0)];
        let text = String::from_utf8_lossy(&filter(content, &rects)).into_owned();
        assert!(!text.contains("SECRET"), "{text}");
        assert_eq!(text.matches("BDC").count(), 2, "{text}");
        assert_eq!(text.matches("EMC").count(), 2, "{text}");
    }

    #[test]
    fn named_property_list_is_inlined_without_replacement_text() {
        let content = b"/Span /MC0 BDC BT /F1 12 Tf 1 0 0 1 50 700 Tm (XXX) Tj ET EMC\n\
                        /OC /OC1 BDC BT /F1 12 Tf 1 0 0 1 50 700 Tm (YYY) Tj ET EMC\n";
        let lookup = |name: &str| {
            (name == "MC0").then(|| {
                let mut d = PdfDict::new();
                d.insert(
                    PdfName::new("ActualText"),
                    PdfObject::String(zpdf_core::PdfString(b"TOP SECRET".to_vec())),
                );
                d.insert(PdfName::new("MCID"), PdfObject::Integer(7));
                d.insert(PdfName::new("Extra"), PdfObject::Ref(ObjectId(9, 0)));
                d
            })
        };
        let rects = [Rect::new(40.0, 690.0, 200.0, 720.0)];
        let outcome = filter_content(content, &rects, &lookup, &|_, ctm| {
            Some(unit_square_extent(ctm))
        });
        let text = String::from_utf8_lossy(&outcome.content);
        assert!(!text.contains("/MC0"), "named list replaced: {text}");
        assert!(!text.contains("TOP SECRET"), "{text}");
        assert!(!text.contains("9 0 R"), "no indirect value inline: {text}");
        assert!(text.contains("/MCID 7"), "{text}");
        assert!(
            text.contains("/OC /OC1 BDC"),
            "optional content untouched: {text}"
        );
        assert_eq!(outcome.redacted_mcids, HashSet::from([7]));
        assert_eq!(
            outcome.dropped_properties,
            HashSet::from(["MC0".to_string()])
        );
    }

    /// A property list that a kept sequence still names stays in the
    /// resources, even when a redacted sequence used an inline copy of it.
    #[test]
    fn property_list_still_named_elsewhere_is_kept() {
        let content = b"/Span /MC0 BDC BT /F1 12 Tf 1 0 0 1 50 700 Tm (XXX) Tj ET EMC\n\
                        /Span /MC0 BDC BT /F1 12 Tf 1 0 0 1 50 100 Tm (ZZZ) Tj ET EMC\n\
                        /Span /MC1 BDC BT /F1 12 Tf 1 0 0 1 50 700 Tm (XXX) Tj ET\n";
        let lookup = |_: &str| {
            let mut d = PdfDict::new();
            d.insert(
                PdfName::new("ActualText"),
                PdfObject::String(zpdf_core::PdfString(b"TOP SECRET".to_vec())),
            );
            Some(d)
        };
        let rects = [Rect::new(40.0, 690.0, 200.0, 720.0)];
        let outcome = filter_content(content, &rects, &lookup, &|_, ctm| {
            Some(unit_square_extent(ctm))
        });
        let text = String::from_utf8_lossy(&outcome.content);
        assert!(text.contains("/Span /MC0 BDC"), "kept use: {text}");
        // MC1 is never closed: its scrubbed copy is inlined all the same.
        assert!(!text.contains("/MC1"), "{text}");
        assert_eq!(
            outcome.dropped_properties,
            HashSet::from(["MC1".to_string()])
        );
    }

    #[test]
    fn xobjects_only_drawn_inside_rects_are_reported() {
        let content = b"q 100 0 0 100 50 500 cm /Im1 Do Q\n\
                        q 100 0 0 100 50 500 cm /Im2 Do Q\n\
                        q 100 0 0 100 400 500 cm /Im2 Do Q\n";
        let rects = [Rect::new(40.0, 490.0, 160.0, 610.0)];
        let outcome = filter_content(content, &rects, &|_| None, &|_, ctm| {
            Some(unit_square_extent(ctm))
        });
        assert_eq!(outcome.dropped_xobjects, HashSet::from(["Im1".to_string()]));
    }

    #[test]
    fn td_advance_places_later_lines() {
        // Line 2 moves down via Td and must be evaluated at its new position.
        let content = b"BT /F1 12 Tf 1 0 0 1 50 700 Tm (top) Tj 0 -600 Td (bottom) Tj ET\n";
        let rects = [Rect::new(0.0, 0.0, 600.0, 200.0)]; // bottom strip
        let filtered = filter(content, &rects);
        let text = String::from_utf8_lossy(&filtered);
        assert!(text.contains("(top)"), "top text kept: {text}");
        assert!(!text.contains("(bottom)"), "bottom text dropped: {text}");
    }
}
