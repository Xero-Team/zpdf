//! Page tree operations: rotate, delete, reorder.
//!
//! All operations edit the page tree through the incremental update:
//! - **rotate** touches only the leaf page dict (`/Rotate`);
//! - **delete** filters the affected `/Kids` arrays and walks each `/Parent`
//!   chain decrementing `/Count`, preserving attribute inheritance (deleted
//!   page objects are orphaned, not freed);
//! - **reorder** flattens the tree to the root `/Pages` node, materializing
//!   inherited attributes (`/Resources`, `/MediaBox`, `/CropBox`, `/Rotate`)
//!   onto each leaf so nothing is lost when leaves are re-parented.
//!
//! Operations take **original** 0-based page indices: the writer's view of the
//! page list is fixed at [`IncrementalWriter::new`] time.

use std::collections::{HashMap, HashSet};

use zpdf_core::{ObjectId, PdfName, PdfObject, Result};

use crate::{invalid_data, unsupported, IncrementalWriter};

/// Matches the document crate's page-tree depth guard.
const MAX_TREE_DEPTH: usize = 64;

/// The inheritable page attributes (ISO 32000-1 Table 30).
const INHERITABLE: [&str; 4] = ["Resources", "MediaBox", "CropBox", "Rotate"];

impl IncrementalWriter {
    /// Set a page's `/Rotate` to an absolute value. `degrees` must be a
    /// multiple of 90 (negative values are normalized into `0..360`).
    pub fn rotate_page(&mut self, page_index: usize, degrees: i32) -> Result<()> {
        if degrees % 90 != 0 {
            return Err(invalid_data("rotation must be a multiple of 90 degrees").into());
        }
        let normalized = degrees.rem_euclid(360);
        let page_id = self.page_id(page_index)?;
        let mut dict = self.resolve_current(page_id)?.as_dict()?.clone();
        dict.insert(
            PdfName::new("Rotate"),
            PdfObject::Integer(normalized as i64),
        );
        self.overwrite_object(page_id, PdfObject::Dict(dict));
        Ok(())
    }

    /// Delete the given pages (0-based indices, duplicates tolerated).
    /// Deleting every page is an error. The deleted page objects and their
    /// annotations are orphaned, not freed.
    pub fn delete_pages(&mut self, indices: &[usize]) -> Result<()> {
        let total = self.document().page_count();
        let unique: HashSet<usize> = indices.iter().copied().collect();
        if unique.is_empty() {
            return Ok(());
        }
        if unique.iter().any(|&index| index >= total) {
            return Err(invalid_data("page index out of range").into());
        }
        if unique.len() == total {
            return Err(invalid_data("cannot delete every page of the document").into());
        }

        // Map indices to leaf ids and group by direct parent; also count how
        // many deleted leaves sit beneath every ancestor node.
        let mut deleted: HashSet<ObjectId> = HashSet::new();
        let mut direct_parent: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
        let mut count_dec: HashMap<ObjectId, i64> = HashMap::new();
        for &idx in &unique {
            let leaf = self.page_id(idx)?;
            deleted.insert(leaf);
            let parent = self.parent_of(leaf)?;
            direct_parent.entry(parent).or_default().push(leaf);
            for node in self.ancestor_chain(parent)? {
                *count_dec.entry(node).or_insert(0) += 1;
            }
        }

        // Filter each direct parent's /Kids and apply the /Count decrements to
        // every affected ancestor.
        for &node in count_dec.keys() {
            let mut dict = self.resolve_current(node)?.as_dict()?.clone();
            if direct_parent.contains_key(&node) {
                let kids = self.kids_of(&dict)?;
                let filtered: Vec<PdfObject> = kids
                    .into_iter()
                    .filter(|k| !matches!(k, PdfObject::Ref(r) if deleted.contains(r)))
                    .collect();
                dict.insert(PdfName::new("Kids"), PdfObject::Array(filtered));
            }
            let old_count = match self.deref_current(dict.get("Count").unwrap_or(&PdfObject::Null))
            {
                PdfObject::Integer(n) => n,
                _ => 0,
            };
            let new_count = old_count.saturating_sub(count_dec[&node]).max(0);
            dict.insert(PdfName::new("Count"), PdfObject::Integer(new_count));
            self.overwrite_object(node, PdfObject::Dict(dict));
        }

        // Cascade: prune interior nodes whose /Kids became empty (never the
        // root — deleting every page was rejected above).
        loop {
            let mut pruned = false;
            let nodes: Vec<ObjectId> = count_dec.keys().copied().collect();
            for node in nodes {
                let dict = self.resolve_current(node)?.as_dict()?.clone();
                let empty = matches!(dict.get("Kids"), Some(PdfObject::Array(a)) if a.is_empty());
                if !empty || dict.get("Parent").is_none() {
                    continue;
                }
                let Ok(parent) = self.parent_of(node) else {
                    continue;
                };
                let mut pdict = self.resolve_current(parent)?.as_dict()?.clone();
                let kids = self.kids_of(&pdict)?;
                let before = kids.len();
                let filtered: Vec<PdfObject> = kids
                    .into_iter()
                    .filter(|k| !matches!(k, PdfObject::Ref(r) if *r == node))
                    .collect();
                if filtered.len() != before {
                    pdict.insert(PdfName::new("Kids"), PdfObject::Array(filtered));
                    self.overwrite_object(parent, PdfObject::Dict(pdict));
                    pruned = true;
                }
            }
            if !pruned {
                break;
            }
        }
        Ok(())
    }

    /// Reorder pages to the given permutation of `0..page_count` (`order[i]`
    /// is the original index of the page that becomes new page `i`).
    ///
    /// The tree is flattened: every leaf is re-parented onto the root `/Pages`
    /// node with its inherited attributes materialized, so intermediate nodes
    /// become orphaned. Outline/link destinations keep working — they point at
    /// page *objects*, which survive.
    pub fn reorder_pages(&mut self, order: &[usize]) -> Result<()> {
        let total = self.document().page_count();
        if order.len() != total || {
            let s: HashSet<usize> = order.iter().copied().collect();
            s.len() != total || order.iter().any(|&i| i >= total)
        } {
            return Err(invalid_data(&format!("order must be a permutation of 0..{total}")).into());
        }

        let root = self.root_pages_id()?;
        let mut kid_refs = Vec::with_capacity(total);
        for &idx in order {
            let leaf = self.page_id(idx)?;
            let mut dict = self.resolve_current(leaf)?.as_dict()?.clone();

            // Materialize inherited attributes before re-parenting.
            for attr in INHERITABLE {
                if dict.get(attr).is_some() {
                    continue;
                }
                if let Some(value) = self.find_inherited(leaf, attr)? {
                    dict.insert(PdfName::new(attr), value);
                }
            }
            dict.insert(PdfName::new("Parent"), PdfObject::Ref(root));
            self.overwrite_object(leaf, PdfObject::Dict(dict));
            kid_refs.push(PdfObject::Ref(leaf));
        }

        let mut root_dict = self.resolve_current(root)?.as_dict()?.clone();
        root_dict.insert(PdfName::new("Kids"), PdfObject::Array(kid_refs));
        root_dict.insert(PdfName::new("Count"), PdfObject::Integer(total as i64));
        self.overwrite_object(root, PdfObject::Dict(root_dict));
        Ok(())
    }

    /// The root `/Pages` node id (trailer `/Root` → `/Pages`).
    fn root_pages_id(&self) -> Result<ObjectId> {
        let catalog = self.resolve_current(self.catalog_ref)?;
        catalog.as_dict()?.get_ref("Pages").map_err(|_| {
            unsupported("catalog has no /Pages reference; cannot edit page tree").into()
        })
    }

    /// A node's `/Parent` reference (must be an indirect reference).
    fn parent_of(&self, id: ObjectId) -> Result<ObjectId> {
        let dict = self.resolve_current(id)?.as_dict()?.clone();
        match dict.get("Parent") {
            Some(PdfObject::Ref(r)) => Ok(*r),
            _ => Err(unsupported(&format!(
                "page-tree node {} has no /Parent reference; cannot edit page tree",
                id.0
            ))
            .into()),
        }
    }

    /// The `/Parent` chain from `start` (inclusive) to the root, guarded
    /// against cycles and over-deep trees.
    fn ancestor_chain(&self, start: ObjectId) -> Result<Vec<ObjectId>> {
        let mut chain = Vec::new();
        let mut visited = HashSet::new();
        let mut node = start;
        loop {
            if chain.len() >= MAX_TREE_DEPTH {
                return Err(invalid_data("page tree too deep").into());
            }
            if !visited.insert(node) {
                return Err(invalid_data("page tree contains a /Parent cycle").into());
            }
            chain.push(node);
            let dict = self.resolve_current(node)?.as_dict()?.clone();
            match dict.get("Parent") {
                Some(PdfObject::Ref(r)) => node = *r,
                _ => return Ok(chain),
            }
        }
    }

    /// A node's `/Kids` as an owned array, following one level of indirection.
    fn kids_of(&self, dict: &zpdf_core::PdfDict) -> Result<Vec<PdfObject>> {
        match dict.get("Kids") {
            Some(PdfObject::Array(a)) => Ok(a.clone()),
            Some(PdfObject::Ref(r)) => match self.resolve_current(*r)? {
                PdfObject::Array(a) => Ok(a),
                _ => Err(invalid_data("/Kids reference is not an array").into()),
            },
            _ => Err(invalid_data("page-tree node has no /Kids array").into()),
        }
    }

    /// Find an inheritable attribute by walking the raw `/Parent` chain above
    /// `leaf` (the leaf itself is not consulted). Values are copied verbatim —
    /// indirect references stay references, which remain valid after the move.
    pub(crate) fn find_inherited(&self, leaf: ObjectId, attr: &str) -> Result<Option<PdfObject>> {
        let mut visited = HashSet::new();
        visited.insert(leaf);
        let mut node = match self.resolve_current(leaf)?.as_dict()?.get("Parent") {
            Some(PdfObject::Ref(r)) => *r,
            _ => return Ok(None),
        };
        for _ in 0..MAX_TREE_DEPTH {
            if !visited.insert(node) {
                return Err(invalid_data("page tree contains a /Parent cycle").into());
            }
            let dict = self.resolve_current(node)?.as_dict()?.clone();
            if let Some(v) = dict.get(attr) {
                return Ok(Some(v.clone()));
            }
            match dict.get("Parent") {
                Some(PdfObject::Ref(r)) => node = *r,
                _ => return Ok(None),
            }
        }
        Err(invalid_data("page tree too deep").into())
    }
}
