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

use std::collections::{BTreeMap, HashSet};
use std::io::{self, Cursor, Write};

use zpdf_content::tokenizer::{ContentToken, ContentTokenizer};
use zpdf_core::{Matrix, ObjectId, ParseLimits, PdfDict, PdfName, PdfObject, Rect, Result};

use crate::bounded::{BoundedBytes, ByteBudget};
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
        let deadline = zpdf_core::time::Instant::now() + std::time::Duration::from_secs(8);
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
        let inherited_resource_owner = self
            .current_resource_owner(page_id)?
            .filter(|owner| *owner != page_id);
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
        let mut outcome = filter_content_with_deadline(
            &content,
            &rects,
            &lookup_properties,
            &lookup_xobject,
            self.document().file().limits(),
            deadline,
        )?;
        let mut filtered = outcome.content;
        // Establish surviving annotations before resource pruning. Overlapping
        // annotations are removed; their appearances must not keep resources
        // reachable after the redaction.
        let removed_annots = self.prune_annotations(&mut page_dict, &rects, deadline)?;

        // Kept content and annotation Forms can still use a name whose
        // top-level draw was removed. Retain those legitimate shared uses.
        if let Some(resources) = resources.as_ref().filter(|_| {
            !outcome.dropped_xobjects.is_empty() || !outcome.dropped_properties.is_empty()
        }) {
            let (xobjects, properties) = self.inherited_resource_uses(
                &filtered,
                resources,
                &page_dict,
                &removed_annots,
                deadline,
            )?;
            outcome
                .dropped_xobjects
                .retain(|name| !xobjects.contains(name));
            outcome
                .dropped_properties
                .retain(|name| !properties.contains(name));
        }

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

        if filtered.len() as u64 > self.document().file().limits().max_decoded_stream_bytes {
            return Err(zpdf_core::Error::StreamSizeLimit(
                self.document().file().limits().max_decoded_stream_bytes,
            ));
        }

        // An XObject whose every draw was removed, or a named property list
        // whose every use was replaced by a scrubbed inline copy, must not stay
        // reachable through the resources, or a full rewrite would keep the
        // redacted image/form or replacement text. Give the page its own copy
        // so other pages sharing the dictionary keep theirs.
        let mut resource_edits = BTreeMap::new();
        if let Some(res) = resources.as_mut() {
            if let Some(owner) = inherited_resource_owner {
                if !outcome.dropped_xobjects.is_empty() || !outcome.dropped_properties.is_empty() {
                    resource_edits = self.prepare_inherited_resource_cleanup(
                        page_id,
                        owner,
                        res,
                        (&outcome.dropped_xobjects, &outcome.dropped_properties),
                        &removed_annots,
                        deadline,
                    )?;
                }
            }
            let pruned = self.prune_resource_candidates(
                res,
                &outcome.dropped_xobjects,
                &outcome.dropped_properties,
                deadline,
            )?;
            if pruned || !resource_edits.is_empty() {
                page_dict.insert(PdfName::new("Resources"), PdfObject::Dict(res.clone()));
            }
        }
        // The thumbnail is a raster of the unredacted page.
        page_dict.0.remove(&PdfName::new("Thumb"));

        let structure_edits =
            self.prepare_structure_scrub(page_id, &outcome.redacted_mcids, &removed_annots)?;

        // Replace the page's contents with a single new stream.
        let stream_ref = self.try_add_flate_stream(&PdfDict::new(), &filtered)?;
        page_dict.insert(
            PdfName::new("Contents"),
            PdfObject::Ref(ObjectId(stream_ref.0, stream_ref.1 as u16)),
        );

        self.overwrite_object(page_id, PdfObject::Dict(page_dict));
        for (id, object) in resource_edits {
            self.overwrite_object(id, object);
        }
        for (id, object) in structure_edits {
            // An unusual structure dictionary can alias a catalog/page/tree
            // object. Merge only fields touched by structure cleanup, so it
            // cannot restore the pre-redaction page or inherited resources.
            let object = match (object, self.resolve_current(id)?) {
                (PdfObject::Dict(edited), PdfObject::Dict(mut current)) => {
                    for key in SENSITIVE_KEYS {
                        if edited.get(key).is_none() {
                            current.0.remove(&PdfName::new(key));
                        }
                    }
                    for key in ["K", "StructTreeRoot"] {
                        if let Some(value) = edited.get(key) {
                            current.insert(PdfName::new(key), value.clone());
                        }
                    }
                    PdfObject::Dict(current)
                }
                (object, _) => object,
            };
            self.overwrite_object(id, object);
        }

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
        Ok(())
    }

    /// Prepare the retained annotation array before queuing any writer edits.
    fn prune_annotations(
        &self,
        page_dict: &mut PdfDict,
        rects: &[Rect],
        deadline: zpdf_core::time::Instant,
    ) -> Result<HashSet<ObjectId>> {
        let check_deadline = || -> Result<()> {
            if zpdf_core::time::Instant::now() >= deadline {
                return Err(invalid_data("redaction exceeds annotation time limit").into());
            }
            Ok(())
        };
        check_deadline()?;
        let mut removed_annots = HashSet::new();
        if let Some(annots_obj) = page_dict.get("Annots").cloned() {
            let annots = match &annots_obj {
                PdfObject::Ref(r) => self.resolve_current(*r)?.as_array()?.to_vec(),
                PdfObject::Array(a) => a.to_vec(),
                PdfObject::Null => Vec::new(),
                _ => {
                    return Err(
                        invalid_data("redaction cannot resolve annotation ownership").into(),
                    )
                }
            };
            let mut kept = Vec::new();
            for annotation in annots {
                check_deadline()?;
                let resolved;
                let object = match &annotation {
                    PdfObject::Ref(id) => {
                        resolved = self.resolve_current(*id)?;
                        &resolved
                    }
                    object => object,
                };
                let mut remove = false;
                if !matches!(object, PdfObject::Null) {
                    let dict = object.as_dict()?;
                    if let Some(bounds) = annot_rect(dict) {
                        for rect in rects {
                            check_deadline()?;
                            if intersects(&bounds, rect) {
                                remove = true;
                                break;
                            }
                        }
                    }
                }
                if remove {
                    if let PdfObject::Ref(id) = annotation {
                        removed_annots.insert(id);
                    }
                } else {
                    kept.push(annotation);
                }
            }
            check_deadline()?;
            page_dict.insert(PdfName::new("Annots"), PdfObject::Array(kept));
        }

        Ok(removed_annots)
    }

    /// Locate the effective Resources owner without changing the page tree.
    fn current_resource_owner(&self, page: ObjectId) -> Result<Option<ObjectId>> {
        let mut seen = HashSet::new();
        let mut id = page;
        for _ in 0..64 {
            if !seen.insert(id) {
                return Err(invalid_data("page resources contain a /Parent cycle").into());
            }
            let object = self.resolve_current(id)?;
            let dict = object.as_dict()?;
            if dict.get("Resources").is_some() {
                return Ok(Some(id));
            }
            match dict.get("Parent") {
                Some(PdfObject::Ref(parent)) => id = *parent,
                _ => return Ok(None),
            }
        }
        Err(invalid_data("page resources exceed inheritance depth").into())
    }

    /// Move an affected inherited table onto its consumers before disconnecting
    /// the ancestor copy. Only removed candidate entries are pruned; all other
    /// resources, including candidates used by another page, are preserved.
    fn prepare_inherited_resource_cleanup(
        &self,
        page: ObjectId,
        owner: ObjectId,
        resources: &PdfDict,
        (dropped_xobjects, dropped_properties): (&HashSet<String>, &HashSet<String>),
        removed_annots: &HashSet<ObjectId>,
        deadline: zpdf_core::time::Instant,
    ) -> Result<BTreeMap<ObjectId, PdfObject>> {
        let mut edits = BTreeMap::new();
        for index in self.page_indices() {
            if zpdf_core::time::Instant::now() >= deadline {
                return Err(invalid_data("resource cleanup exceeds time limit").into());
            }
            let id = self.page_id(*index)?;
            if id == page || self.current_resource_owner(id)? != Some(owner) {
                continue;
            }
            let mut leaf = self.resolve_current(id)?.as_dict()?.clone();
            let content = self.current_content_bytes(&self.current_content_refs(&leaf)?)?;
            let (used_xobjects, used_properties) =
                self.inherited_resource_uses(&content, resources, &leaf, removed_annots, deadline)?;
            let mut own = resources.clone();
            let dropped_xobjects = dropped_xobjects
                .difference(&used_xobjects)
                .cloned()
                .collect();
            let dropped_properties = dropped_properties
                .difference(&used_properties)
                .cloned()
                .collect();
            self.prune_resource_candidates(
                &mut own,
                &dropped_xobjects,
                &dropped_properties,
                deadline,
            )?;
            leaf.insert(PdfName::new("Resources"), PdfObject::Dict(own));
            edits.insert(id, PdfObject::Dict(leaf));
        }
        let mut ancestor = self.resolve_current(owner)?.as_dict()?.clone();
        ancestor.0.remove(&PdfName::new("Resources"));
        edits.insert(owner, PdfObject::Dict(ancestor));
        Ok(edits)
    }

    /// Pruning a name is not sufficient if another retained resource points
    /// back to its payload. Inspect the entire retained resource graph,
    /// including unused Forms, before allowing any edit to be queued. This
    /// intentionally refuses ambiguous aliases rather than deleting resources
    /// whose use could not be proved. A sibling checks only its own removals.
    fn prune_resource_candidates(
        &self,
        resources: &mut PdfDict,
        dropped_xobjects: &HashSet<String>,
        dropped_properties: &HashSet<String>,
        deadline: zpdf_core::time::Instant,
    ) -> Result<bool> {
        let mut graph = ResourceGraphCheck::new(self, deadline);
        let mut pruned = false;
        for (category, dropped) in [
            ("XObject", dropped_xobjects),
            ("Properties", dropped_properties),
        ] {
            if dropped.is_empty() {
                continue;
            }
            let Some(value) = resources.get(category) else {
                continue;
            };
            let object = match value {
                PdfObject::Ref(id) => self.resolve_current(*id)?,
                value => value.clone(),
            };
            let PdfObject::Dict(mut entries) = object else {
                return Err(invalid_data("resource cleanup cannot resolve resource table").into());
            };
            for name in dropped {
                if let Some(value) = entries.0.remove(name.as_str()) {
                    graph.remember_removed(&value, 0)?;
                    pruned = true;
                }
            }
            resources.insert(PdfName::new(category), PdfObject::Dict(entries));
        }
        if pruned {
            for value in resources.0.values() {
                graph.check_retained(value, 0)?;
            }
        }
        Ok(pruned)
    }

    /// The renderer falls back per name, even when /Resources exists. Only
    /// an empty/missing effective table can be scanned wholly in the page
    /// context; partial overrides require a context-aware dependency walk.
    fn require_page_resource_context(&self, dict: &PdfDict) -> Result<()> {
        if let Some(PdfObject::Dict(resources)) = dict
            .get("Resources")
            .map(|object| self.deref_current(object))
        {
            if !resources.0.is_empty() {
                return Err(invalid_data(
                    "resource cleanup cannot prove Form resource ownership with nonempty Resources",
                )
                .into());
            }
        }
        Ok(())
    }

    /// Stored appearance roots of annotations that remain in the update.
    /// Keep all AP states, not just /AS, and reject unsupported shapes before
    /// any page/resource edits are committed.
    fn page_appearance_refs(
        &self,
        page: &PdfDict,
        removed: &HashSet<ObjectId>,
        deadline: zpdf_core::time::Instant,
    ) -> Result<HashSet<ObjectId>> {
        let resolve = |object: &PdfObject| -> Result<PdfObject> {
            match object {
                PdfObject::Ref(id) => self.resolve_current(*id),
                object => Ok(object.clone()),
            }
        };
        let mut refs = HashSet::new();
        let Some(annots) = page.get("Annots") else {
            return Ok(refs);
        };
        let annots = resolve(annots)?;
        if matches!(annots, PdfObject::Null) {
            return Ok(refs);
        }
        let annots = annots.as_array()?;
        if annots.len() > 65_536 {
            return Err(invalid_data("resource cleanup exceeds annotation limit").into());
        }
        let mut remaining = self.document().file().limits().max_objects;
        for annotation in annots {
            if zpdf_core::time::Instant::now() >= deadline {
                return Err(invalid_data("resource cleanup exceeds time limit").into());
            }
            remaining = remaining
                .checked_sub(1)
                .ok_or_else(|| invalid_data("resource cleanup exceeds appearance entry limit"))?;
            if matches!(annotation, PdfObject::Ref(id) if removed.contains(id)) {
                continue;
            }
            let annotation = resolve(annotation)?;
            if matches!(annotation, PdfObject::Null) {
                continue;
            }
            let Some(appearance) = annotation.as_dict()?.get("AP") else {
                continue;
            };
            let appearance = resolve(appearance)?;
            if matches!(appearance, PdfObject::Null) {
                continue;
            }
            let appearance = appearance.as_dict()?;
            for key in ["N", "R", "D"] {
                if let Some(value) = appearance.get(key) {
                    self.collect_appearance_refs(value, true, &mut remaining, &mut refs, deadline)?;
                }
            }
        }
        Ok(refs)
    }

    fn collect_appearance_refs(
        &self,
        value: &PdfObject,
        allow_states: bool,
        remaining: &mut u32,
        refs: &mut HashSet<ObjectId>,
        deadline: zpdf_core::time::Instant,
    ) -> Result<()> {
        if zpdf_core::time::Instant::now() >= deadline {
            return Err(invalid_data("resource cleanup exceeds time limit").into());
        }
        *remaining = (*remaining)
            .checked_sub(1)
            .ok_or_else(|| invalid_data("resource cleanup exceeds appearance entry limit"))?;
        let (id, value) = match value {
            PdfObject::Ref(id) => (Some(*id), self.resolve_current(*id)?),
            value => (None, value.clone()),
        };
        match value {
            PdfObject::Null => {}
            PdfObject::Stream(_) => {
                let id = id.ok_or_else(|| {
                    invalid_data("resource cleanup requires indirect appearance streams")
                })?;
                refs.insert(id);
            }
            PdfObject::Dict(states) if allow_states => {
                for state in states.0.values() {
                    self.collect_appearance_refs(state, false, remaining, refs, deadline)?;
                }
            }
            _ => {
                return Err(
                    invalid_data("resource cleanup cannot resolve appearance ownership").into(),
                )
            }
        }
        Ok(())
    }

    /// Resource names used directly, or by Forms falling back to this resource
    /// table. Refuse ambiguous inherited resource dependencies instead of
    /// deleting unrelated content or silently claiming complete cleanup.
    fn inherited_resource_uses(
        &self,
        content: &[u8],
        resources: &PdfDict,
        page: &PdfDict,
        removed_annots: &HashSet<ObjectId>,
        deadline: zpdf_core::time::Instant,
    ) -> Result<(HashSet<String>, HashSet<String>)> {
        let limits = self.document().file().limits();
        let mut xobjects = HashSet::new();
        let mut properties = HashSet::new();
        let mut visited = HashSet::new();
        let mut pending = vec![content.to_vec()];
        let mut bytes_left = limits
            .max_decoded_stream_bytes
            .checked_sub(content.len() as u64)
            .ok_or_else(|| invalid_data("resource cleanup exceeds decoded byte limit"))?;
        let mut ops_left = limits.max_page_operators;
        let category = |key: &str| resources.get(key).map(|object| self.deref_current(object));
        let fonts = category("Font");
        let states = category("ExtGState");
        let patterns = category("Pattern");
        let lookup = |category: &str, name: &str| -> Option<PdfObject> {
            let entries = match category {
                "Font" => &fonts,
                "ExtGState" => &states,
                "Pattern" => &patterns,
                _ => return None,
            };
            entries
                .as_ref()?
                .as_dict()
                .ok()?
                .get(name)
                .map(|object| self.deref_current(object))
        };
        // Appearance replay has the same page-resource fallback as a Do.
        // Include every retained normal/rollover/down appearance state, even
        // an inactive state that a viewer can select later.
        for id in self.page_appearance_refs(page, removed_annots, deadline)? {
            if zpdf_core::time::Instant::now() >= deadline {
                return Err(invalid_data("resource cleanup exceeds time limit").into());
            }
            if visited.insert(id) {
                let object = self.resolve_current(id)?;
                self.require_page_resource_context(&object.as_stream()?.dict)?;
                let bytes = self.current_content_bytes(&[id])?;
                bytes_left = bytes_left
                    .checked_sub(bytes.len() as u64)
                    .ok_or_else(|| invalid_data("resource cleanup exceeds decoded byte limit"))?;
                pending.push(bytes);
            }
        }
        let xobject_entries = category("XObject");
        while let Some(content) = pending.pop() {
            let mut tokenizer = ContentTokenizer::new(&content);
            let mut operands = Vec::new();
            while let Some(token) = tokenizer.next_token() {
                if zpdf_core::time::Instant::now() >= deadline {
                    return Err(invalid_data("resource cleanup exceeds time limit").into());
                }
                match token {
                    ContentToken::Operand(object) => {
                        if operands.len() >= limits.max_operand_stack_depth as usize {
                            return Err(
                                invalid_data("resource cleanup exceeds operand limit").into()
                            );
                        }
                        operands.push(object);
                        continue;
                    }
                    ContentToken::Operator(op) => {
                        ops_left = ops_left.checked_sub(1).ok_or_else(|| {
                            invalid_data("resource cleanup exceeds operator limit")
                        })?;
                        match op.as_str() {
                            "BDC" | "DP" => {
                                if let Some(PdfObject::Name(name)) = operands.get(1) {
                                    properties.insert(name.as_str().to_string());
                                }
                            }
                            "Do" => {
                                if let Some(PdfObject::Name(name)) = operands.first() {
                                    xobjects.insert(name.as_str().to_string());
                                    let value = xobject_entries
                                        .as_ref()
                                        .and_then(|object| object.as_dict().ok())
                                        .and_then(|dict| dict.get(name.as_str()));
                                    if let Some(PdfObject::Ref(id)) = value {
                                        if visited.insert(*id) {
                                            let object = self.resolve_current(*id)?;
                                            let stream = object.as_stream()?;
                                            if stream.dict.get_name("Subtype").ok() == Some("Form")
                                            {
                                                self.require_page_resource_context(&stream.dict)?;
                                                let bytes = self.current_content_bytes(&[*id])?;
                                                bytes_left = bytes_left.checked_sub(bytes.len() as u64)
                                                    .ok_or_else(|| invalid_data("resource cleanup exceeds decoded byte limit"))?;
                                                pending.push(bytes);
                                            }
                                        }
                                    }
                                }
                            }
                            "Tf" => {
                                if let Some(PdfObject::Name(name)) = operands.first() {
                                    if let Some(PdfObject::Dict(font)) =
                                        lookup("Font", name.as_str())
                                    {
                                        if font.get_name("Subtype").ok() == Some("Type3") {
                                            return Err(invalid_data("resource cleanup cannot prove Type3 resource ownership").into());
                                        }
                                    }
                                }
                            }
                            "gs" => {
                                if let Some(PdfObject::Name(name)) = operands.first() {
                                    if let Some(PdfObject::Dict(state)) =
                                        lookup("ExtGState", name.as_str())
                                    {
                                        // ExtGState can select a font without a Tf
                                        // operator, including inside an appearance.
                                        if let Some(PdfObject::Array(font)) =
                                            state.get("Font").map(|value| self.deref_current(value))
                                        {
                                            if let Some(PdfObject::Dict(font)) =
                                                font.first().map(|value| self.deref_current(value))
                                            {
                                                if font.get_name("Subtype").ok() == Some("Type3") {
                                                    return Err(invalid_data("resource cleanup cannot prove Type3 resource ownership").into());
                                                }
                                            }
                                        }
                                        if let Some(mask) = state.get("SMask") {
                                            if !matches!(mask, PdfObject::Name(name) if name.as_str() == "None")
                                            {
                                                return Err(invalid_data("resource cleanup cannot prove soft-mask resource ownership").into());
                                            }
                                        }
                                    }
                                }
                            }
                            "scn" | "SCN" => {
                                if let Some(PdfObject::Name(name)) = operands.last() {
                                    if let Some(object) = lookup("Pattern", name.as_str()) {
                                        if matches!(object, PdfObject::Stream(_)) {
                                            return Err(invalid_data("resource cleanup cannot prove pattern resource ownership").into());
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    ContentToken::InlineImage { .. } => {
                        ops_left = ops_left.checked_sub(1).ok_or_else(|| {
                            invalid_data("resource cleanup exceeds operator limit")
                        })?;
                    }
                }
                operands.clear();
            }
            let diagnostics = tokenizer.diagnostics();
            if !diagnostics.is_complete() {
                return Err(zpdf_core::Error::IncompleteOutput(diagnostics));
            }
        }
        Ok((xobjects, properties))
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
    fn prepare_structure_scrub(
        &self,
        page: ObjectId,
        mcids: &HashSet<i64>,
        objects: &HashSet<ObjectId>,
    ) -> Result<BTreeMap<ObjectId, PdfObject>> {
        let mut walk = StructureScrub {
            writer: self,
            page,
            mcids,
            objects,
            pages: self.page_slots.iter().flatten().copied().collect(),
            active: HashSet::new(),
            edits: BTreeMap::new(),
            remaining: 1 << 20,
        };
        if mcids.is_empty() && objects.is_empty() {
            return Ok(walk.edits);
        }
        let mut catalog = self.resolve_current(self.catalog_ref())?.as_dict()?.clone();
        let Some(root_value) = catalog.get("StructTreeRoot") else {
            return Ok(walk.edits);
        };
        let root_id = match root_value {
            PdfObject::Ref(id) => Some(*id),
            _ => None,
        };
        let mut root = self.deref_current(root_value).as_dict()?.clone();
        let pg = walk.page_of(&root, None);
        if let Some(mut kids) = root.get("K").cloned() {
            let (_, changed) = walk.kid(&mut kids, pg, true, 0)?;
            if changed {
                root.insert(PdfName::new("K"), kids);
                if let Some(id) = root_id {
                    walk.edits.insert(id, PdfObject::Dict(root));
                } else {
                    catalog.insert(PdfName::new("StructTreeRoot"), PdfObject::Dict(root));
                    walk.edits
                        .insert(self.catalog_ref(), PdfObject::Dict(catalog));
                }
            }
        }
        Ok(walk.edits)
    }
}

/// Bounded copy-on-write walk matching the structure reader's /K shapes.
/// References are guarded on the current path, not globally: a shared MCR or
/// K array still associates removed content with every distinct owner.
struct StructureScrub<'a> {
    writer: &'a IncrementalWriter,
    page: ObjectId,
    mcids: &'a HashSet<i64>,
    objects: &'a HashSet<ObjectId>,
    pages: HashSet<ObjectId>,
    active: HashSet<ObjectId>,
    edits: BTreeMap<ObjectId, PdfObject>,
    remaining: usize,
}

impl StructureScrub<'_> {
    fn page_of(&self, dict: &PdfDict, inherited: Option<ObjectId>) -> Option<ObjectId> {
        dict.get_ref("Pg")
            .ok()
            .filter(|id| self.pages.contains(id))
            .or(inherited)
    }

    fn kid(
        &mut self,
        value: &mut PdfObject,
        pg: Option<ObjectId>,
        normalize_array: bool,
        depth: u32,
    ) -> Result<(bool, bool)> {
        if depth > 256 || self.remaining == 0 {
            return Err(invalid_data("structure redaction exceeds traversal limit").into());
        }
        self.remaining -= 1;
        match value {
            PdfObject::Ref(id) => {
                let id = *id;
                if !self.active.insert(id) {
                    return Err(invalid_data("structure redaction found a /K cycle").into());
                }
                let mut object = match self.edits.get(&id) {
                    Some(object) => object.clone(),
                    None => self.writer.resolve_current(id)?,
                };
                // A referenced integer is not a reader-accepted MCID child.
                let result = if matches!(object, PdfObject::Dict(_))
                    || (normalize_array && matches!(object, PdfObject::Array(_)))
                {
                    self.kid(&mut object, pg, normalize_array, depth + 1)?
                } else {
                    (false, false)
                };
                self.active.remove(&id);
                if result.1 {
                    self.edits.insert(id, object);
                }
                Ok((result.0, false))
            }
            PdfObject::Array(values) if normalize_array => {
                let (mut hit, mut changed) = (false, false);
                for value in values {
                    let result = self.kid(value, pg, false, depth + 1)?;
                    hit |= result.0;
                    changed |= result.1;
                }
                Ok((hit, changed))
            }
            PdfObject::Integer(mcid) => {
                Ok((pg == Some(self.page) && self.mcids.contains(mcid), false))
            }
            PdfObject::Dict(dict) => {
                let pg = self.page_of(dict, pg);
                let kind = match dict.get_name("Type") {
                    Ok("MCR") => "MCR",
                    Ok("OBJR") => "OBJR",
                    Ok("StructElem") => "StructElem",
                    _ if dict.get("S").is_none() && dict.get("MCID").is_some() => "MCR",
                    _ if dict.get("S").is_none() && dict.get("Obj").is_some() => "OBJR",
                    _ => "StructElem",
                };
                match kind {
                    "MCR" => {
                        let mcid = match dict.get("MCID") {
                            Some(PdfObject::Integer(n)) => Some(*n),
                            Some(PdfObject::Real(f)) if f.is_finite() && f.fract() == 0.0 => {
                                Some(*f as i64)
                            }
                            _ => None,
                        };
                        Ok((
                            pg == Some(self.page) && mcid.is_some_and(|m| self.mcids.contains(&m)),
                            false,
                        ))
                    }
                    "OBJR" => Ok((
                        dict.get_ref("Obj")
                            .ok()
                            .is_some_and(|id| self.objects.contains(&id)),
                        false,
                    )),
                    _ => {
                        let (hit, mut changed) = match dict.0.get_mut(&PdfName::new("K")) {
                            Some(kids) => self.kid(kids, pg, true, depth + 1)?,
                            None => (false, false),
                        };
                        if hit {
                            changed |= strip_sensitive_keys(dict);
                        }
                        Ok((hit, changed))
                    }
                }
            }
            _ => Ok((false, false)),
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

/// Source-object graph validation only: stream bytes are hashed as stored,
/// never decoded. The canonical fingerprints also catch ref-to-ref aliases,
/// which the parser resolves transparently. Removed roots and their referenced
/// descendants are tracked; an identical retained object at a reference
/// boundary is conservatively treated as ambiguous ownership under another ID.
struct ResourceGraphCheck<'a> {
    writer: &'a IncrementalWriter,
    removed_ids: HashSet<ObjectId>,
    removed_fingerprints: HashSet<[u8; 32]>,
    visited: HashSet<ObjectId>,
    nodes_left: u32,
    bytes_left: u64,
    depth_limit: u32,
    deadline: zpdf_core::time::Instant,
}

impl<'a> ResourceGraphCheck<'a> {
    fn new(writer: &'a IncrementalWriter, deadline: zpdf_core::time::Instant) -> Self {
        let limits = writer.document().file().limits();
        Self {
            writer,
            removed_ids: HashSet::new(),
            removed_fingerprints: HashSet::new(),
            visited: HashSet::new(),
            nodes_left: limits.max_objects,
            bytes_left: limits.max_decoded_stream_bytes,
            depth_limit: limits.max_object_depth.min(128),
            deadline,
        }
    }

    fn charge_node(&mut self, depth: u32) -> Result<()> {
        if zpdf_core::time::Instant::now() >= self.deadline {
            return Err(invalid_data("resource cleanup exceeds time limit").into());
        }
        if depth > self.depth_limit {
            return Err(invalid_data("resource cleanup exceeds graph depth limit").into());
        }
        self.nodes_left = self
            .nodes_left
            .checked_sub(1)
            .ok_or_else(|| invalid_data("resource cleanup exceeds graph work limit"))?;
        Ok(())
    }

    fn remember_removed(&mut self, object: &PdfObject, depth: u32) -> Result<()> {
        self.remember_removed_object(object, depth, true)
    }

    fn remember_removed_object(
        &mut self,
        object: &PdfObject,
        depth: u32,
        reference_boundary: bool,
    ) -> Result<()> {
        self.charge_node(depth)?;
        if let PdfObject::Ref(id) = object {
            if self.removed_ids.insert(*id) {
                self.remember_removed_object(&self.writer.resolve_current(*id)?, depth + 1, true)?;
            }
            return Ok(());
        }
        // Root objects and referenced descendants carry the owned payload.
        // Do not add every inline scalar/dictionary: common Rect numbers,
        // names and empty Resources are not evidence of shared secret data.
        if reference_boundary && Self::has_payload(object) {
            let fingerprint = self.fingerprint(object)?;
            self.removed_fingerprints.insert(fingerprint);
        }
        match object {
            PdfObject::Array(values) => {
                for value in values {
                    self.remember_removed_object(value, depth + 1, false)?;
                }
            }
            PdfObject::Dict(dict) => {
                for value in dict.0.values() {
                    self.remember_removed_object(value, depth + 1, false)?;
                }
            }
            PdfObject::Stream(stream) => {
                for value in stream.dict.0.values() {
                    self.remember_removed_object(value, depth + 1, false)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn has_payload(object: &PdfObject) -> bool {
        match object {
            PdfObject::String(value) => !value.0.is_empty(),
            PdfObject::Array(values) => !values.is_empty(),
            PdfObject::Dict(dict) => !dict.0.is_empty(),
            PdfObject::Stream(stream) => !stream.dict.0.is_empty() || !stream.data.is_empty(),
            _ => false,
        }
    }

    fn check_retained(&mut self, object: &PdfObject, depth: u32) -> Result<()> {
        self.charge_node(depth)?;
        if let PdfObject::Ref(id) = object {
            if self.removed_ids.contains(id) {
                return Err(invalid_data("resource cleanup retains a removed payload").into());
            }
            if self.visited.insert(*id) {
                let resolved = self.writer.resolve_current(*id)?;
                // Compare at object boundaries, including indirect strings,
                // so transparent aliases to owned descendants cannot bypass
                // the ID check. Inline scalar coincidences are ignored.
                if Self::has_payload(&resolved) {
                    let fingerprint = self.fingerprint(&resolved)?;
                    if self.removed_fingerprints.contains(&fingerprint) {
                        return Err(
                            invalid_data("resource cleanup retains a removed payload").into()
                        );
                    }
                }
                self.check_retained(&resolved, depth + 1)?;
            }
            return Ok(());
        }
        match object {
            PdfObject::Array(values) => {
                for value in values {
                    self.check_retained(value, depth + 1)?;
                }
            }
            PdfObject::Dict(dict) => {
                for value in dict.0.values() {
                    self.check_retained(value, depth + 1)?;
                }
            }
            PdfObject::Stream(stream) => {
                for value in stream.dict.0.values() {
                    self.check_retained(value, depth + 1)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn fingerprint(&mut self, object: &PdfObject) -> Result<[u8; 32]> {
        use sha2::Digest;
        let mut hash = sha2::Sha256::new();
        self.hash_object(object, &mut hash, 0)?;
        Ok(hash.finalize().into())
    }

    fn hash_bytes(&mut self, hash: &mut sha2::Sha256, bytes: &[u8]) -> Result<()> {
        use sha2::Digest;
        if zpdf_core::time::Instant::now() >= self.deadline {
            return Err(invalid_data("resource cleanup exceeds time limit").into());
        }
        self.bytes_left = self
            .bytes_left
            .checked_sub(bytes.len() as u64)
            .ok_or_else(|| invalid_data("resource cleanup exceeds graph byte limit"))?;
        hash.update(bytes);
        Ok(())
    }

    fn hash_dict(&mut self, dict: &PdfDict, hash: &mut sha2::Sha256, depth: u32) -> Result<()> {
        self.hash_bytes(hash, &(dict.0.len() as u64).to_le_bytes())?;
        for (key, value) in &dict.0 {
            self.hash_bytes(hash, &(key.as_str().len() as u64).to_le_bytes())?;
            self.hash_bytes(hash, key.as_str().as_bytes())?;
            self.hash_object(value, hash, depth + 1)?;
        }
        Ok(())
    }

    /// Type tags, explicit lengths and fixed-width scalars make the hash
    /// unambiguous without a second payload allocation or PDF serialization.
    fn hash_object(
        &mut self,
        object: &PdfObject,
        hash: &mut sha2::Sha256,
        depth: u32,
    ) -> Result<()> {
        self.charge_node(depth)?;
        match object {
            PdfObject::Null => self.hash_bytes(hash, &[0]),
            PdfObject::Bool(value) => self.hash_bytes(hash, &[1, u8::from(*value)]),
            PdfObject::Integer(value) => {
                self.hash_bytes(hash, &[2])?;
                self.hash_bytes(hash, &value.to_le_bytes())
            }
            PdfObject::Real(value) => {
                self.hash_bytes(hash, &[3])?;
                self.hash_bytes(hash, &value.to_bits().to_le_bytes())
            }
            PdfObject::Name(value) => {
                self.hash_bytes(hash, &[4])?;
                self.hash_bytes(hash, &(value.as_str().len() as u64).to_le_bytes())?;
                self.hash_bytes(hash, value.as_str().as_bytes())
            }
            PdfObject::String(value) => {
                self.hash_bytes(hash, &[5])?;
                self.hash_bytes(hash, &(value.0.len() as u64).to_le_bytes())?;
                self.hash_bytes(hash, &value.0)
            }
            PdfObject::Array(values) => {
                self.hash_bytes(hash, &[6])?;
                self.hash_bytes(hash, &(values.len() as u64).to_le_bytes())?;
                for value in values {
                    self.hash_object(value, hash, depth + 1)?;
                }
                Ok(())
            }
            PdfObject::Dict(dict) => {
                self.hash_bytes(hash, &[7])?;
                self.hash_dict(dict, hash, depth)
            }
            PdfObject::Ref(id) => {
                self.hash_bytes(hash, &[8])?;
                self.hash_bytes(hash, &id.0.to_le_bytes())?;
                self.hash_bytes(hash, &id.1.to_le_bytes())
            }
            PdfObject::Stream(stream) => {
                self.hash_bytes(hash, &[9])?;
                self.hash_dict(&stream.dict, hash, depth)?;
                self.hash_bytes(hash, &(stream.data.len() as u64).to_le_bytes())?;
                self.hash_bytes(hash, &stream.data)
            }
        }
    }
}

/// An open BDC, retained only until its first removed operation or EMC.
struct MarkedSequence {
    range: std::ops::Range<usize>,
    operands: Vec<PdfObject>,
}

/// Walk and filter within the same content limits as the document interpreter.
/// A budget failure aborts before the writer queues the page edit.
fn filter_content_with_deadline(
    content: &[u8],
    rects: &[Rect],
    properties: &dyn Fn(&str) -> Option<PdfDict>,
    xobject_extent: &dyn Fn(&str, &Matrix) -> Option<Rect>,
    limits: &ParseLimits,
    deadline: zpdf_core::time::Instant,
) -> Result<FilterOutcome> {
    let mut out = BoundedBytes::new(limits.max_decoded_stream_bytes);
    let mut tokenizer = ContentTokenizer::new(content);
    let mut operands: Vec<PdfObject> = Vec::new();
    let mut state = TrackState::default();
    let mut stack: Vec<TrackState> = Vec::new();
    let mut path_bbox: Option<Rect> = None;
    let mut cur_point: Option<(f64, f64)> = None;

    // BMCs and already-scrubbed BDCs need only a nesting slot. Each live BDC
    // enters `unmarked` once and is removed once; repeated paint removals do
    // not scan already-marked scopes (including stacks made entirely of BMCs).
    let mut open: Vec<Option<MarkedSequence>> = Vec::new();
    let mut unmarked: Vec<usize> = Vec::new();
    let mut replacements: Vec<(std::ops::Range<usize>, Vec<u8>)> = Vec::new();
    let mut property_work = ByteBudget(limits.max_decoded_stream_bytes);
    let mut replacement_bytes = ByteBudget(limits.max_decoded_stream_bytes);
    let mut redacted_mcids = HashSet::new();
    let mut dropped_properties = HashSet::new();
    let mut kept_properties = HashSet::new();
    let mut dropped_xobjects = HashSet::new();
    let mut kept_xobjects = HashSet::new();
    let mut operators = 0u64;

    while let Some(token) = tokenizer.next_token() {
        if zpdf_core::time::Instant::now() >= deadline {
            return Err(invalid_data("redaction exceeds content time limit").into());
        }
        if let ContentToken::Operand(obj) = token {
            if operands.len() >= limits.max_operand_stack_depth as usize {
                return Err(invalid_data("redaction exceeds operand stack limit").into());
            }
            operands.push(obj);
            continue;
        }
        if operators >= limits.max_page_operators {
            return Err(invalid_data("redaction exceeds page operator limit").into());
        }
        operators += 1;
        let removed = match token {
            ContentToken::InlineImage { dict, data } => {
                let extent = unit_square_extent(&state.ctm);
                let removed = rects.iter().any(|r| intersects(&extent, r));
                if !removed {
                    write_inline_image(&mut out, &dict, &data)?;
                }
                operands.clear();
                removed
            }
            ContentToken::Operator(op) => {
                if op == "q" && stack.len() >= limits.max_graphics_state_depth as usize {
                    return Err(invalid_data("redaction exceeds graphics state limit").into());
                }
                if matches!(op.as_str(), "BDC" | "BMC")
                    && open.len() >= limits.max_marked_content_depth as usize
                {
                    return Err(invalid_data("redaction exceeds marked content limit").into());
                }
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
                    Keep::Yes => write_op(&mut out, &operands, &op)?,
                    Keep::No => {}
                    Keep::Replace(rep) => {
                        out.write_all(rep.as_bytes())?;
                        out.write_all(b"\n")?;
                    }
                }
                let removed = !matches!(keep, Keep::Yes);
                match op.as_str() {
                    "BDC" => {
                        unmarked.push(open.len());
                        open.push(Some(MarkedSequence {
                            range: start..out.len(),
                            operands: std::mem::take(&mut operands),
                        }));
                    }
                    "BMC" => open.push(None),
                    "EMC" => {
                        if let Some(sequence) = open.pop() {
                            if unmarked.last() == Some(&open.len()) {
                                unmarked.pop();
                            }
                            if let Some(sequence) = sequence {
                                if let Some(PdfObject::Name(name)) = sequence.operands.get(1) {
                                    kept_properties.insert(name.as_str().to_string());
                                }
                            }
                        }
                    }
                    "DP" => {
                        if let Some(PdfObject::Name(name)) = operands.get(1) {
                            kept_properties.insert(name.as_str().to_string());
                        }
                    }
                    "Do" => {
                        if let Some(PdfObject::Name(name)) = operands.first() {
                            let set = if removed {
                                &mut dropped_xobjects
                            } else {
                                &mut kept_xobjects
                            };
                            set.insert(name.as_str().to_string());
                        }
                    }
                    _ => {}
                }
                operands.clear();
                removed
            }
            ContentToken::Operand(_) => unreachable!(),
        };
        if removed {
            for index in unmarked.drain(..) {
                let Some(mut sequence) = open[index].take() else {
                    continue;
                };
                let optional_content = matches!(sequence.operands.first(), Some(PdfObject::Name(tag)) if tag.as_str() == "OC");
                let property = if sequence.operands.len() > 1 {
                    sequence.operands.remove(1)
                } else {
                    PdfObject::Null
                };
                let (properties, name) = match property {
                    PdfObject::Dict(dict) => (Some(dict), None),
                    PdfObject::Name(name) => (properties(name.as_str()), Some(name.0)),
                    _ => (None, None),
                };
                let mut inlined = false;
                if let Some(mut properties) = properties {
                    // Account for repeated owned named-property resolution as
                    // cumulative work, then release the copy immediately. No
                    // closed scope retains property dictionaries until EOF.
                    crate::serialize::serialize_dict(&mut property_work, &properties)?;
                    if let Some(PdfObject::Integer(mcid)) = properties.get("MCID") {
                        redacted_mcids.insert(*mcid);
                    }
                    if !optional_content && strip_sensitive_keys(&mut properties) {
                        properties
                            .0
                            .retain(|_, value| !matches!(value, PdfObject::Ref(_)));
                        let mut replacement = BoundedBytes::new(replacement_bytes.0);
                        let tag = sequence
                            .operands
                            .into_iter()
                            .next()
                            .unwrap_or(PdfObject::Null);
                        write_op(&mut replacement, &[tag, PdfObject::Dict(properties)], "BDC")?;
                        replacement_bytes.write_all(replacement.as_slice())?;
                        replacements.push((sequence.range, replacement.into_inner()));
                        inlined = true;
                    }
                }
                if let Some(name) = name {
                    if inlined {
                        dropped_properties.insert(name);
                    } else {
                        kept_properties.insert(name);
                    }
                }
            }
        }
    }
    let diagnostics = tokenizer.diagnostics();
    if !diagnostics.is_complete() {
        return Err(zpdf_core::Error::IncompleteOutput(diagnostics));
    }
    for sequence in open.into_iter().flatten() {
        if let Some(PdfObject::Name(name)) = sequence.operands.get(1) {
            kept_properties.insert(name.as_str().to_string());
        }
    }

    // One forward reconstruction replaces repeated suffix-moving splices.
    // The ordinary output and replacement bytes have independent hard caps;
    // the final content must also fit the document's aggregate decoded limit.
    replacements.sort_unstable_by_key(|(range, _)| range.start);
    let content = if replacements.is_empty() {
        out.into_inner()
    } else {
        let mut result = BoundedBytes::new(limits.max_decoded_stream_bytes);
        let mut at = 0;
        for (range, replacement) in replacements {
            result.write_all(&out.as_slice()[at..range.start])?;
            result.write_all(&replacement)?;
            at = range.end;
        }
        result.write_all(&out.as_slice()[at..])?;
        result.into_inner()
    };
    dropped_xobjects.retain(|name| !kept_xobjects.contains(name));
    dropped_properties.retain(|name| !kept_properties.contains(name));
    Ok(FilterOutcome {
        content,
        redacted_mcids,
        dropped_xobjects,
        dropped_properties,
    })
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
fn write_op<W: Write>(out: &mut W, operands: &[PdfObject], op: &str) -> io::Result<()> {
    for object in operands {
        crate::serialize::serialize_object_body(out, object)?;
        out.write_all(b" ")?;
    }
    out.write_all(op.as_bytes())?;
    out.write_all(b"\n")
}

/// Emit an inline image section with fallible, bounded writes.
fn write_inline_image<W: Write>(out: &mut W, dict: &PdfDict, data: &[u8]) -> io::Result<()> {
    out.write_all(b"BI ")?;
    for (key, value) in &dict.0 {
        crate::serialize::serialize_object_body(out, &PdfObject::Name(key.clone()))?;
        out.write_all(b" ")?;
        crate::serialize::serialize_object_body(out, value)?;
        out.write_all(b" ")?;
    }
    out.write_all(b"ID\n")?;
    out.write_all(data)?;
    out.write_all(b"\nEI\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter_content(
        content: &[u8],
        rects: &[Rect],
        properties: &dyn Fn(&str) -> Option<PdfDict>,
        xobject_extent: &dyn Fn(&str, &Matrix) -> Option<Rect>,
        limits: &ParseLimits,
    ) -> Result<FilterOutcome> {
        filter_content_with_deadline(
            content,
            rects,
            properties,
            xobject_extent,
            limits,
            zpdf_core::time::Instant::now() + std::time::Duration::from_secs(8),
        )
    }

    #[test]
    fn expired_annotation_deadline_aborts_without_changing_page() {
        let mut builder = crate::DocumentBuilder::new();
        builder.add_page(100.0, 100.0);
        let writer = IncrementalWriter::new(builder.build().unwrap()).unwrap();
        let mut page = PdfDict::new();
        page.insert(
            PdfName::new("Annots"),
            PdfObject::Array(vec![PdfObject::Null]),
        );
        let before = page.clone();
        let result = writer.prune_annotations(
            &mut page,
            &[Rect::new(0.0, 0.0, 10.0, 10.0)],
            zpdf_core::time::Instant::now(),
        );
        assert!(result.is_err(), "expired annotation work must be rejected");
        assert_eq!(
            page, before,
            "failure must not replace the annotation array"
        );
    }

    #[test]
    fn annotation_deadline_does_not_add_an_entry_limit() {
        let mut builder = crate::DocumentBuilder::new();
        builder.add_page(100.0, 100.0);
        let writer = IncrementalWriter::new(builder.build().unwrap()).unwrap();
        let mut page = PdfDict::new();
        page.insert(
            PdfName::new("Annots"),
            PdfObject::Array(vec![PdfObject::Null; 65_537]),
        );
        let before = page.clone();
        writer
            .prune_annotations(
                &mut page,
                &[Rect::new(0.0, 0.0, 10.0, 10.0)],
                zpdf_core::time::Instant::now() + std::time::Duration::from_secs(8),
            )
            .unwrap();
        assert_eq!(page, before);
    }

    fn default_filter_content(
        content: &[u8],
        rects: &[Rect],
        properties: &dyn Fn(&str) -> Option<PdfDict>,
        xobject_extent: &dyn Fn(&str, &Matrix) -> Option<Rect>,
    ) -> FilterOutcome {
        filter_content(
            content,
            rects,
            properties,
            xobject_extent,
            &ParseLimits::default(),
        )
        .unwrap()
    }

    fn filter(content: &[u8], rects: &[Rect]) -> Vec<u8> {
        default_filter_content(content, rects, &|_| None, &|_, ctm| {
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
        let outcome = default_filter_content(content, &rects, &|_| None, &|_, ctm| {
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
        let outcome = default_filter_content(content, &rects, &lookup, &|_, ctm| {
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
        let outcome = default_filter_content(content, &rects, &lookup, &|_, ctm| {
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
        let outcome = default_filter_content(content, &rects, &|_| None, &|_, ctm| {
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
    #[test]
    fn redaction_obeys_small_document_content_limits() {
        let no_properties = |_: &str| None;
        let extent = |_: &str, matrix: &Matrix| Some(unit_square_extent(matrix));
        let check = |content: &[u8], limits: &ParseLimits| {
            filter_content(content, &[], &no_properties, &extent, limits).is_ok()
        };
        let limits = ParseLimits {
            max_page_operators: 1,
            ..ParseLimits::default()
        };
        assert!(check(b"q", &limits));
        assert!(!check(b"q Q", &limits));
        let limits = ParseLimits {
            max_operand_stack_depth: 2,
            ..ParseLimits::default()
        };
        assert!(check(b"1 2 m", &limits));
        assert!(!check(b"1 2 3 m", &limits));
        let limits = ParseLimits {
            max_graphics_state_depth: 1,
            ..ParseLimits::default()
        };
        assert!(check(b"q Q q Q", &limits));
        assert!(!check(b"q q Q Q", &limits));
        let limits = ParseLimits {
            max_marked_content_depth: 1,
            ..ParseLimits::default()
        };
        assert!(check(b"/P BMC EMC /P BMC EMC", &limits));
        assert!(!check(b"/P BMC /P BMC EMC EMC", &limits));
    }

    #[test]
    fn each_marked_scope_resolves_properties_only_on_its_first_removal() {
        let lookups = std::cell::Cell::new(0);
        let properties = |_: &str| {
            lookups.set(lookups.get() + 1);
            let mut dict = PdfDict::new();
            dict.insert(
                PdfName::new("ActualText"),
                PdfObject::String(zpdf_core::PdfString(b"test".to_vec())),
            );
            Some(dict)
        };
        let content =
            b"/P /Caption BDC 1 1 2 2 re f 2 2 2 2 re f EMC /P /Caption BDC 1 1 2 2 re f EMC";
        let result = filter_content(
            content,
            &[Rect::new(0.0, 0.0, 10.0, 10.0)],
            &properties,
            &|_, _| None,
            &ParseLimits::default(),
        )
        .unwrap();
        assert_eq!(lookups.get(), 2);
        assert!(!String::from_utf8_lossy(&result.content).contains("test"));
        lookups.set(0);
        filter_content(
            b"/P /Caption BDC EMC",
            &[],
            &properties,
            &|_, _| None,
            &ParseLimits::default(),
        )
        .unwrap();
        assert_eq!(
            lookups.get(),
            0,
            "an untouched sequence needs no owned property copy"
        );
    }

    #[test]
    fn derived_content_and_property_work_have_byte_limits() {
        let mut limits = ParseLimits {
            max_decoded_stream_bytes: 2,
            ..ParseLimits::default()
        };
        assert!(filter_content(b"q", &[], &|_| None, &|_, _| None, &limits).is_ok());
        limits.max_decoded_stream_bytes = 1;
        assert!(filter_content(b"q", &[], &|_| None, &|_, _| None, &limits).is_err());
        let mut budget = ByteBudget(3);
        budget.write_all(b"ab").unwrap();
        budget.write_all(b"c").unwrap();
        assert!(budget.write_all(b"d").is_err());
    }
}
