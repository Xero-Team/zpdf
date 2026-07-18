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
//! Limitations (v1): document-level structures of the appended files are not
//! merged — outlines, AcroForm field dictionaries (widgets still render via
//! their `/AP`), name trees, optional content configs, tagged-PDF structure.

use zpdf_core::{ObjectId, PdfName, PdfObject, Result};
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

    /// Append the given 0-based pages of `source`, in the given order.
    pub fn append_pages(&mut self, source: &PdfFile, page_indices: &[usize]) -> Result<()> {
        if page_indices.is_empty() {
            return Ok(());
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
        let mut id_map = ObjectIdMap::new();
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
