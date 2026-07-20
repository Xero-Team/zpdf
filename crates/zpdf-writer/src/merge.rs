//! Document merging: append the pages of other PDFs to a base document.
//!
//! Built on [`crate::copier`]: each appended page's object subtree (contents,
//! resources, annotations) is deep-copied into the base document's update with
//! fresh object numbers, then re-parented under the base root `/Pages` node.
//!
//! Inherited page attributes (`/Resources`, `/MediaBox`, `/CropBox`,
//! `/Rotate`) are materialized onto each copied leaf first — the source's
//! interior page-tree nodes are *not* copied (`/Parent` links are dropped
//! during the copy precisely so the copier does not chase the whole source
//! tree), so anything inherited would otherwise be lost.
//!
//! Limitations: tagged-PDF structure trees and name trees of the appended
//! files are not merged. Outlines, AcroForm fields (with collision renaming)
//! and OCG configurations are — see [`IncrementalWriter::append_document`].

use std::collections::HashSet;

use zpdf_core::{ObjectId, PdfDict, PdfName, PdfObject, PdfString, Result};
use zpdf_parser::PdfFile;

use crate::copier::ObjectIdMap;
use crate::{invalid_data, unsupported, IncrementalWriter};

/// Inheritable page attributes (ISO 32000-1 Table 30), materialized before
/// the leaf is detached from its source tree.
const INHERITABLE: [&str; 4] = ["Resources", "MediaBox", "CropBox", "Rotate"];

/// A minimal one-page PDF (catalog + /Pages root + one empty placeholder
/// page) used as the base document when *extracting* pages into a fresh
/// file. A zero-page stub would be rejected by the document layer, so the
/// placeholder is appended-to and then deleted.
fn stub_pdf() -> Vec<u8> {
    let mut data = b"%PDF-1.7\n".to_vec();
    let ofs1 = data.len() as u64;
    data.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
    let ofs2 = data.len() as u64;
    data.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
    let ofs3 = data.len() as u64;
    data.extend_from_slice(
        b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>\nendobj\n",
    );
    let xref_pos = data.len();
    data.extend_from_slice(b"xref\n0 4\n0000000000 65535 f \n");
    data.extend_from_slice(format!("{ofs1:010} 00000 n \n").as_bytes());
    data.extend_from_slice(format!("{ofs2:010} 00000 n \n").as_bytes());
    data.extend_from_slice(format!("{ofs3:010} 00000 n \n").as_bytes());
    data.extend_from_slice(
        format!("trailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n{xref_pos}\n%%EOF\n").as_bytes(),
    );
    data
}

/// Extract the given 0-based pages of `source` into a fresh, self-contained
/// PDF containing only those pages (and their transitively referenced
/// objects). Pages appear in the order given.
pub fn extract_pages(source: &PdfFile, page_indices: &[usize]) -> Result<Vec<u8>> {
    if page_indices.is_empty() {
        return Err(invalid_data("no pages selected").into());
    }
    let mut writer = IncrementalWriter::new(stub_pdf())?;
    writer.append_pages(source, page_indices)?;

    // Drop the stub's placeholder page (object 3 0) from the root /Kids.
    // delete_pages can't be used here: its "don't delete every page" guard
    // works off the parse-time page count, which is 1 for the stub.
    let root = ObjectId(2, 0);
    let mut root_dict = writer.resolve_current(root)?.as_dict()?.clone();
    let kids = match root_dict.get("Kids") {
        Some(PdfObject::Array(a)) => a.clone(),
        _ => return Err(invalid_data("stub root /Kids missing").into()),
    };
    let filtered: Vec<PdfObject> = kids
        .into_iter()
        .filter(|k| !matches!(k, PdfObject::Ref(r) if *r == ObjectId(3, 0)))
        .collect();
    root_dict.insert(
        PdfName::new("Count"),
        PdfObject::Integer(filtered.len() as i64),
    );
    root_dict.insert(PdfName::new("Kids"), PdfObject::Array(filtered));
    writer.overwrite_object(root, PdfObject::Dict(root_dict));

    let mut out = std::io::Cursor::new(Vec::new());
    writer.write(&mut out)?;
    Ok(out.into_inner())
}

impl IncrementalWriter {
    /// Append every page of `source` (an already-parsed PDF) after the base
    /// document's existing pages. Returns the number of pages appended.
    ///
    /// May be called repeatedly (with different sources) before `write`.
    pub fn append_document_pages(&mut self, source: &PdfFile) -> Result<usize> {
        let count = source_page_ids(source)?.len();
        self.append_pages(source, &(0..count).collect::<Vec<_>>())?;
        Ok(count)
    }

    /// Append every page of `source` **and** merge its document-level
    /// structures: outline (bookmarks), AcroForm fields (renamed on name
    /// collision), and optional-content groups/configuration. Returns the
    /// number of pages appended.
    pub fn append_document(&mut self, source: &PdfFile) -> Result<usize> {
        let count = source_page_ids(source)?.len();
        let id_map = self.append_pages_mapped(source, &(0..count).collect::<Vec<_>>())?;
        self.merge_outlines(source, &id_map)?;
        self.merge_acroform(source, &id_map)?;
        self.merge_ocproperties(source, &id_map)?;
        Ok(count)
    }

    /// Append the given 0-based pages of `source`, in the given order.
    pub fn append_pages(&mut self, source: &PdfFile, page_indices: &[usize]) -> Result<()> {
        self.append_pages_mapped(source, page_indices)?;
        Ok(())
    }

    /// [`Self::append_pages`], returning the source→dest object-id map so
    /// document-level merges can reuse already-copied objects (fields whose
    /// widgets came along with the pages, OCGs referenced from content).
    fn append_pages_mapped(
        &mut self,
        source: &PdfFile,
        page_indices: &[usize],
    ) -> Result<ObjectIdMap> {
        let mut id_map = ObjectIdMap::new();
        if page_indices.is_empty() {
            return Ok(id_map);
        }
        let src_pages = source_page_ids(source)?;
        for &idx in page_indices {
            if idx >= src_pages.len() {
                return Err(invalid_data(&format!(
                    "source page index {idx} out of range ({} pages)",
                    src_pages.len()
                ))
                .into());
            }
        }

        let root = self.root_pages_id_pub()?;
        // One id-map per source document: pages sharing fonts/images copy the
        // shared objects once.
        let mut new_kids: Vec<ObjectId> = Vec::with_capacity(page_indices.len());

        for &idx in page_indices {
            let src_page = src_pages[idx];

            // Materialize inherited attributes into a patched leaf dict before
            // copying, since the copy severs /Parent.
            let mut leaf = source.resolve(src_page)?.as_dict()?.clone();
            for attr in INHERITABLE {
                if leaf.get(attr).is_none() {
                    if let Some(v) = find_inherited_in_source(source, src_page, attr)? {
                        leaf.insert(PdfName::new(attr), v);
                    }
                }
            }

            // Copy the page graph. /Parent is dropped on every copied dict:
            // for the page leaf that's what severs it from the source tree;
            // annotation /Parent back-references (widget → field) go with it,
            // which is acceptable while AcroForm dicts are not merged.
            let dest_page = copy_patched_page(source, src_page, &leaf, self, &mut id_map)?;
            new_kids.push(dest_page);
        }

        // Re-parent the copied leaves and append them to the base root /Kids.
        for &kid in &new_kids {
            let mut dict = self.resolve_current(kid)?.as_dict()?.clone();
            dict.insert(PdfName::new("Parent"), PdfObject::Ref(root));
            self.overwrite_object(kid, PdfObject::Dict(dict));
        }
        let mut root_dict = self.resolve_current(root)?.as_dict()?.clone();
        let mut kids = match root_dict.get("Kids") {
            Some(PdfObject::Array(a)) => a.clone(),
            Some(PdfObject::Ref(r)) => match self.resolve_current(*r)? {
                PdfObject::Array(a) => a,
                _ => return Err(invalid_data("/Kids reference is not an array").into()),
            },
            _ => return Err(invalid_data("root /Pages node has no /Kids array").into()),
        };
        kids.extend(new_kids.iter().map(|&id| PdfObject::Ref(id)));
        let old_count = match root_dict.get("Count") {
            Some(PdfObject::Integer(n)) => *n,
            _ => 0,
        };
        root_dict.insert(PdfName::new("Kids"), PdfObject::Array(kids));
        root_dict.insert(
            PdfName::new("Count"),
            PdfObject::Integer(old_count + new_kids.len() as i64),
        );
        self.overwrite_object(root, PdfObject::Dict(root_dict));
        Ok(id_map)
    }

    /// Merge the source's `/Outlines` tree: its top-level items are appended
    /// as top-level items of the destination outline (created if absent).
    /// Destinations referencing copied pages remap through `id_map`; items
    /// pointing at pages that were not copied keep their actions dangling
    /// (viewers ignore unresolvable destinations).
    fn merge_outlines(&mut self, source: &PdfFile, id_map: &ObjectIdMap) -> Result<()> {
        let src_root = source
            .trailer
            .get_ref("Root")
            .map_err(|_| invalid_data("source trailer missing /Root"))?;
        let src_catalog = source.resolve(src_root)?.as_dict()?.clone();
        let Ok(src_outlines_id) = src_catalog.get_ref("Outlines") else {
            return Ok(()); // nothing to merge
        };
        let src_outlines = source.resolve(src_outlines_id)?.as_dict()?.clone();
        let (Some(PdfObject::Ref(src_first)), Some(PdfObject::Ref(src_last))) =
            (src_outlines.get("First"), src_outlines.get("Last"))
        else {
            return Ok(()); // empty outline
        };
        let (src_first, src_last) = (*src_first, *src_last);

        // Deep-copy the outline item graph (items reference pages via /Dest
        // arrays or /A /D — those page refs remap through the shared id_map,
        // which is pre-seeded with every copied page). /Parent is dropped and
        // re-linked below.
        let mut id_map = id_map.clone();
        let dest_first =
            crate::copier::copy_object_graph(source, src_first, self, &mut id_map, &["Parent"])?;
        let dest_last = id_map.get(src_last).unwrap_or(dest_first);

        // Ensure the destination catalog has an /Outlines root.
        let catalog_id = self.catalog_ref();
        let mut catalog = self.resolve_current(catalog_id)?.as_dict()?.clone();
        let outlines_id = match catalog.get("Outlines") {
            Some(PdfObject::Ref(r)) => *r,
            _ => {
                let mut dict = PdfDict::new();
                dict.insert(
                    PdfName::new("Type"),
                    PdfObject::Name(PdfName::new("Outlines")),
                );
                let (num, gen) = self.try_add_object(&PdfObject::Dict(dict))?;
                let id = ObjectId(num, gen as u16);
                catalog.insert(PdfName::new("Outlines"), PdfObject::Ref(id));
                self.overwrite_object(catalog_id, PdfObject::Dict(catalog.clone()));
                id
            }
        };
        let mut outlines = self.resolve_current(outlines_id)?.as_dict()?.clone();

        // Splice: previous last item ↔ first copied item.
        let copied_count = count_chain(self, dest_first, dest_last);
        match outlines.get("Last") {
            Some(PdfObject::Ref(old_last)) => {
                let old_last = *old_last;
                let mut last_dict = self.resolve_current(old_last)?.as_dict()?.clone();
                last_dict.insert(PdfName::new("Next"), PdfObject::Ref(dest_first));
                self.overwrite_object(old_last, PdfObject::Dict(last_dict));
                let mut first_dict = self.resolve_current(dest_first)?.as_dict()?.clone();
                first_dict.insert(PdfName::new("Prev"), PdfObject::Ref(old_last));
                first_dict.insert(PdfName::new("Parent"), PdfObject::Ref(outlines_id));
                self.overwrite_object(dest_first, PdfObject::Dict(first_dict));
            }
            _ => {
                outlines.insert(PdfName::new("First"), PdfObject::Ref(dest_first));
                let mut first_dict = self.resolve_current(dest_first)?.as_dict()?.clone();
                first_dict.insert(PdfName::new("Parent"), PdfObject::Ref(outlines_id));
                self.overwrite_object(dest_first, PdfObject::Dict(first_dict));
            }
        }
        outlines.insert(PdfName::new("Last"), PdfObject::Ref(dest_last));
        let old_count = match outlines.get("Count") {
            Some(PdfObject::Integer(n)) if *n > 0 => *n,
            _ => 0,
        };
        outlines.insert(
            PdfName::new("Count"),
            PdfObject::Integer(old_count + copied_count),
        );
        // Re-parent every copied top-level item.
        let mut cur = dest_first;
        for _ in 0..MAX_OUTLINE_ITEMS {
            let mut dict = self.resolve_current(cur)?.as_dict()?.clone();
            dict.insert(PdfName::new("Parent"), PdfObject::Ref(outlines_id));
            let next = match dict.get("Next") {
                Some(PdfObject::Ref(r)) => Some(*r),
                _ => None,
            };
            self.overwrite_object(cur, PdfObject::Dict(dict));
            match next {
                Some(n) if cur != dest_last => cur = n,
                _ => break,
            }
        }
        self.overwrite_object(outlines_id, PdfObject::Dict(outlines));
        Ok(())
    }

    /// Merge the source's AcroForm: fields are deep-copied (their widgets are
    /// usually already in `id_map` from the page copy, so field→widget links
    /// stay intact) and appended to the destination `/AcroForm /Fields`.
    /// A source field whose fully-qualified name collides with an existing
    /// destination field is renamed by appending `_2`, `_3`, ….
    fn merge_acroform(&mut self, source: &PdfFile, id_map: &ObjectIdMap) -> Result<()> {
        let src_root = source
            .trailer
            .get_ref("Root")
            .map_err(|_| invalid_data("source trailer missing /Root"))?;
        let src_catalog = source.resolve(src_root)?.as_dict()?.clone();
        let src_acro = match src_catalog.get("AcroForm") {
            Some(PdfObject::Ref(r)) => source.resolve(*r)?.as_dict()?.clone(),
            Some(PdfObject::Dict(d)) => d.clone(),
            _ => return Ok(()),
        };
        let src_fields = match src_acro.get("Fields") {
            Some(PdfObject::Array(a)) => a.clone(),
            Some(PdfObject::Ref(r)) => match source.resolve(*r)? {
                PdfObject::Array(a) => a,
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        if src_fields.is_empty() {
            return Ok(());
        }

        // Destination /AcroForm (created if absent).
        let catalog_id = self.catalog_ref();
        let mut catalog = self.resolve_current(catalog_id)?.as_dict()?.clone();
        let (mut dest_acro, acro_ref) = match catalog.get("AcroForm") {
            Some(PdfObject::Ref(r)) => (self.resolve_current(*r)?.as_dict()?.clone(), Some(*r)),
            Some(PdfObject::Dict(d)) => (d.clone(), None),
            _ => (PdfDict::new(), None),
        };
        let mut dest_fields = match dest_acro.get("Fields") {
            Some(PdfObject::Array(a)) => a.clone(),
            Some(PdfObject::Ref(r)) => match self.resolve_current(*r)? {
                PdfObject::Array(a) => a,
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };

        // Existing top-level field names, for collision renaming.
        let mut names: HashSet<String> = HashSet::new();
        for f in &dest_fields {
            if let PdfObject::Ref(r) = f {
                if let Ok(obj) = self.resolve_current(*r) {
                    if let Ok(d) = obj.as_dict() {
                        if let Some(PdfObject::String(s)) = d.get("T") {
                            names.insert(String::from_utf8_lossy(&s.0).into_owned());
                        }
                    }
                }
            }
        }

        // Copy each root field graph; widgets already copied with the pages
        // resolve through the shared map instead of duplicating.
        let mut id_map = id_map.clone();
        for field in src_fields {
            let PdfObject::Ref(src_field) = field else {
                continue;
            };
            let dest_field = crate::copier::copy_object_graph(
                source,
                src_field,
                self,
                &mut id_map,
                // /Parent of a root field would climb into nothing; widget
                // /P (page back-refs) remap normally via the map.
                &[],
            )?;

            // Collision rename on the copied field's /T.
            let mut dict = self.resolve_current(dest_field)?.as_dict()?.clone();
            if let Some(PdfObject::String(s)) = dict.get("T") {
                let base = String::from_utf8_lossy(&s.0).into_owned();
                if names.contains(&base) {
                    let mut n = 2usize;
                    let renamed = loop {
                        let candidate = format!("{base}_{n}");
                        if !names.contains(&candidate) {
                            break candidate;
                        }
                        n += 1;
                    };
                    dict.insert(
                        PdfName::new("T"),
                        PdfObject::String(PdfString(renamed.clone().into_bytes())),
                    );
                    self.overwrite_object(dest_field, PdfObject::Dict(dict));
                    names.insert(renamed);
                } else {
                    names.insert(base);
                }
            }
            dest_fields.push(PdfObject::Ref(dest_field));
        }

        dest_acro.insert(PdfName::new("Fields"), PdfObject::Array(dest_fields));
        // Carry /DR and /DA from the source when the destination has none
        // (appearance regeneration for the copied fields needs the fonts).
        for key in ["DR", "DA", "NeedAppearances"] {
            if dest_acro.get(key).is_none() {
                if let Some(v) = src_acro.get(key) {
                    let remapped =
                        crate::copier::remap_via_copy(v, source, self, &mut id_map, &[])?;
                    dest_acro.insert(PdfName::new(key), remapped);
                }
            }
        }

        match acro_ref {
            Some(r) => self.overwrite_object(r, PdfObject::Dict(dest_acro)),
            None => {
                catalog.insert(PdfName::new("AcroForm"), PdfObject::Dict(dest_acro));
                self.overwrite_object(catalog_id, PdfObject::Dict(catalog));
            }
        }
        Ok(())
    }

    /// Merge `/OCProperties`: source OCGs (already copied where content
    /// references them) are appended to the destination `/OCGs` array and to
    /// the default configuration's `/ON`/`/OFF` lists per their source state.
    fn merge_ocproperties(&mut self, source: &PdfFile, id_map: &ObjectIdMap) -> Result<()> {
        let src_root = source
            .trailer
            .get_ref("Root")
            .map_err(|_| invalid_data("source trailer missing /Root"))?;
        let src_catalog = source.resolve(src_root)?.as_dict()?.clone();
        let src_ocp = match src_catalog.get("OCProperties") {
            Some(PdfObject::Ref(r)) => source.resolve(*r)?.as_dict()?.clone(),
            Some(PdfObject::Dict(d)) => d.clone(),
            _ => return Ok(()),
        };
        let src_ocgs = match src_ocp.get("OCGs") {
            Some(PdfObject::Array(a)) => a.clone(),
            _ => return Ok(()),
        };
        if src_ocgs.is_empty() {
            return Ok(());
        }

        // Copy every source OCG (deduped through the shared map).
        let mut id_map = id_map.clone();
        let mut copied: Vec<(ObjectId, ObjectId)> = Vec::new(); // (src, dest)
        for ocg in &src_ocgs {
            if let PdfObject::Ref(r) = ocg {
                let dest = crate::copier::copy_object_graph(source, *r, self, &mut id_map, &[])?;
                copied.push((*r, dest));
            }
        }

        // Source default-config OFF set (everything else defaults ON).
        let src_off: HashSet<ObjectId> = match src_ocp.get("D") {
            Some(PdfObject::Dict(d)) => match d.get("OFF") {
                Some(PdfObject::Array(a)) => a
                    .iter()
                    .filter_map(|o| match o {
                        PdfObject::Ref(r) => Some(*r),
                        _ => None,
                    })
                    .collect(),
                _ => HashSet::new(),
            },
            _ => HashSet::new(),
        };

        // Destination /OCProperties (created if absent).
        let catalog_id = self.catalog_ref();
        let mut catalog = self.resolve_current(catalog_id)?.as_dict()?.clone();
        let (mut dest_ocp, ocp_ref) = match catalog.get("OCProperties") {
            Some(PdfObject::Ref(r)) => (self.resolve_current(*r)?.as_dict()?.clone(), Some(*r)),
            Some(PdfObject::Dict(d)) => (d.clone(), None),
            _ => (PdfDict::new(), None),
        };
        let mut ocgs = match dest_ocp.get("OCGs") {
            Some(PdfObject::Array(a)) => a.clone(),
            _ => Vec::new(),
        };
        let mut d_cfg = match dest_ocp.get("D") {
            Some(PdfObject::Dict(d)) => d.clone(),
            _ => PdfDict::new(),
        };
        let mut on = match d_cfg.get("ON") {
            Some(PdfObject::Array(a)) => a.clone(),
            _ => Vec::new(),
        };
        let mut off = match d_cfg.get("OFF") {
            Some(PdfObject::Array(a)) => a.clone(),
            _ => Vec::new(),
        };

        for (src, dest) in copied {
            ocgs.push(PdfObject::Ref(dest));
            if src_off.contains(&src) {
                off.push(PdfObject::Ref(dest));
            } else {
                on.push(PdfObject::Ref(dest));
            }
        }
        d_cfg.insert(PdfName::new("ON"), PdfObject::Array(on));
        d_cfg.insert(PdfName::new("OFF"), PdfObject::Array(off));
        dest_ocp.insert(PdfName::new("OCGs"), PdfObject::Array(ocgs));
        dest_ocp.insert(PdfName::new("D"), PdfObject::Dict(d_cfg));

        match ocp_ref {
            Some(r) => self.overwrite_object(r, PdfObject::Dict(dest_ocp)),
            None => {
                catalog.insert(PdfName::new("OCProperties"), PdfObject::Dict(dest_ocp));
                self.overwrite_object(catalog_id, PdfObject::Dict(catalog));
            }
        }
        Ok(())
    }

    /// The root `/Pages` node id (public counterpart of the private helper in
    /// `pages.rs`, kept separate to avoid changing that module's API).
    fn root_pages_id_pub(&self) -> Result<ObjectId> {
        let catalog = self.resolve_current(self.catalog_ref())?;
        catalog
            .as_dict()?
            .get_ref("Pages")
            .map_err(|_| unsupported("catalog has no /Pages reference; cannot append pages").into())
    }
}

/// Defensive cap when walking sibling chains of copied outline items.
const MAX_OUTLINE_ITEMS: usize = 65_536;

/// Number of items in the `First → Next → … → Last` sibling chain.
fn count_chain(writer: &IncrementalWriter, first: ObjectId, last: ObjectId) -> i64 {
    let mut count = 1i64;
    let mut cur = first;
    for _ in 0..MAX_OUTLINE_ITEMS {
        if cur == last {
            break;
        }
        let Ok(obj) = writer.resolve_current(cur) else {
            break;
        };
        let Ok(dict) = obj.as_dict() else { break };
        match dict.get("Next") {
            Some(PdfObject::Ref(r)) => {
                cur = *r;
                count += 1;
            }
            _ => break,
        }
    }
    count
}

/// Copy the (already attribute-materialized) page dict `leaf` and its graph.
/// The leaf itself is written manually so the patched dict is used; its
/// referenced objects flow through the normal copier.
fn copy_patched_page(
    source: &PdfFile,
    src_page: ObjectId,
    leaf: &zpdf_core::PdfDict,
    writer: &mut IncrementalWriter,
    id_map: &mut ObjectIdMap,
) -> Result<ObjectId> {
    // Wrap the patched dict in a temporary object and remap it through the
    // copier by copying each referenced value. Reserve the page's own number
    // first so self-references resolve.
    if let Some(dest) = id_map.get(src_page) {
        return Ok(dest);
    }
    let dest = crate::copier::reserve(writer, src_page, id_map)?;
    let remapped = crate::copier::remap_via_copy(
        &PdfObject::Dict(leaf.clone()),
        source,
        writer,
        id_map,
        &["Parent"],
    )?;
    writer.set_reserved_object(dest, remapped);
    Ok(dest)
}

/// Walk the source page tree upward for an inheritable attribute.
fn find_inherited_in_source(
    source: &PdfFile,
    leaf: ObjectId,
    attr: &str,
) -> Result<Option<PdfObject>> {
    const MAX_DEPTH: usize = 64;
    let mut node = match source.resolve(leaf)?.as_dict()?.get("Parent") {
        Some(PdfObject::Ref(r)) => *r,
        _ => return Ok(None),
    };
    let mut seen = std::collections::HashSet::new();
    seen.insert(leaf);
    for _ in 0..MAX_DEPTH {
        if !seen.insert(node) {
            return Ok(None); // cycle: give up on inheritance, not the copy
        }
        let dict = source.resolve(node)?.as_dict()?.clone();
        if let Some(v) = dict.get(attr) {
            return Ok(Some(v.clone()));
        }
        match dict.get("Parent") {
            Some(PdfObject::Ref(r)) => node = *r,
            _ => return Ok(None),
        }
    }
    Ok(None)
}

/// Flatten the source's page tree to leaf object ids in page order.
fn source_page_ids(source: &PdfFile) -> Result<Vec<ObjectId>> {
    const MAX_DEPTH: usize = 64;
    const MAX_PAGES: usize = 1_000_000;
    let root = source
        .trailer
        .get_ref("Root")
        .map_err(|_| invalid_data("source trailer missing /Root"))?;
    let catalog = source.resolve(root)?.as_dict()?.clone();
    let pages_root = catalog
        .get_ref("Pages")
        .map_err(|_| invalid_data("source catalog missing /Pages"))?;

    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    walk_pages(
        source, pages_root, 0, MAX_DEPTH, MAX_PAGES, &mut seen, &mut out,
    )?;
    Ok(out)
}

fn walk_pages(
    source: &PdfFile,
    node: ObjectId,
    depth: usize,
    max_depth: usize,
    max_pages: usize,
    seen: &mut std::collections::HashSet<ObjectId>,
    out: &mut Vec<ObjectId>,
) -> Result<()> {
    if depth > max_depth || out.len() >= max_pages || !seen.insert(node) {
        return Ok(());
    }
    let dict = source.resolve(node)?.as_dict()?.clone();
    match dict.get_name("Type") {
        Ok("Pages") => {
            let kids = match dict.get("Kids") {
                Some(PdfObject::Array(a)) => a.clone(),
                Some(PdfObject::Ref(r)) => match source.resolve(*r)? {
                    PdfObject::Array(a) => a,
                    _ => Vec::new(),
                },
                _ => Vec::new(),
            };
            for kid in kids {
                if let PdfObject::Ref(r) = kid {
                    walk_pages(source, r, depth + 1, max_depth, max_pages, seen, out)?;
                }
            }
        }
        // Leaf page — or a node with no /Type but also no /Kids (lenient).
        _ => {
            if dict.get("Kids").is_none() {
                out.push(node);
            }
        }
    }
    Ok(())
}
