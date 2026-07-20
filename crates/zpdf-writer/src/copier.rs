//! Deep copy of object graphs between PDF documents with renumbering.
//!
//! The copier is the primitive behind document merging: it walks the object
//! graph reachable from a source object (in another parsed [`PdfFile`]),
//! assigns each visited object a fresh number in the destination writer, and
//! rewrites all indirect references through that mapping.
//!
//! Cycles (e.g. page ↔ parent) are handled by reserving the destination
//! number **before** the object body is remapped, so a back-reference simply
//! resolves to the already-reserved number. Traversal is an explicit
//! work-list, not call recursion, so deep reference chains cannot overflow
//! the stack.

use std::collections::HashMap;

use zpdf_core::{ObjectId, PdfDict, PdfObject, PdfStream, Result};
use zpdf_parser::PdfFile;

use crate::IncrementalWriter;

/// Mapping from source-document object IDs to destination object IDs,
/// built up while copying. Reusable across multiple [`copy_object_graph`]
/// calls against the same (source, destination) pair so shared resources
/// (fonts, images) are copied once.
#[derive(Debug, Default, Clone)]
pub struct ObjectIdMap {
    mapping: HashMap<ObjectId, ObjectId>,
}

impl ObjectIdMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// The destination ID a source object was copied to, if it has been.
    pub fn get(&self, source: ObjectId) -> Option<ObjectId> {
        self.mapping.get(&source).copied()
    }

    /// Number of objects copied so far.
    pub fn len(&self) -> usize {
        self.mapping.len()
    }

    pub fn is_empty(&self) -> bool {
        self.mapping.is_empty()
    }
}

/// Copy `source_id` and everything reachable from it out of `source_file`
/// into `writer`, renumbering references. Returns the destination ID of the
/// root. Objects already present in `id_map` are not copied again.
///
/// `skip_keys` names dictionary keys that are **not followed or emitted** on
/// any copied dictionary (e.g. `"Parent"` when copying page subtrees into a
/// different page tree — the caller re-parents afterwards). Pass `&[]` to
/// copy verbatim.
pub fn copy_object_graph(
    source_file: &PdfFile,
    source_id: ObjectId,
    writer: &mut IncrementalWriter,
    id_map: &mut ObjectIdMap,
    skip_keys: &[&str],
) -> Result<ObjectId> {
    if let Some(dest) = id_map.get(source_id) {
        return Ok(dest);
    }

    // Reserve the root's destination number up front, then process the
    // work-list. Every newly-discovered reference reserves its number when
    // first seen, so cyclic graphs terminate.
    let root_dest = reserve(writer, source_id, id_map)?;
    let mut work: Vec<ObjectId> = vec![source_id];
    drain_work(&mut work, source_file, writer, id_map, skip_keys)?;
    Ok(root_dest)
}

/// Remap a **caller-patched** object (e.g. a page dict with inherited
/// attributes materialized) as though it were copied from `source_file`,
/// copying every object it references. The patched object itself is *not*
/// written — the caller stores it (typically via a reserved number).
pub(crate) fn remap_via_copy(
    obj: &PdfObject,
    source_file: &PdfFile,
    writer: &mut IncrementalWriter,
    id_map: &mut ObjectIdMap,
    skip_keys: &[&str],
) -> Result<PdfObject> {
    let mut work: Vec<ObjectId> = Vec::new();
    let remapped = remap(obj, writer, id_map, skip_keys, &mut work)?;
    drain_work(&mut work, source_file, writer, id_map, skip_keys)?;
    Ok(remapped)
}

/// Copy queued source objects (and everything they newly reference) until the
/// work-list is empty.
fn drain_work(
    work: &mut Vec<ObjectId>,
    source_file: &PdfFile,
    writer: &mut IncrementalWriter,
    id_map: &mut ObjectIdMap,
    skip_keys: &[&str],
) -> Result<()> {
    while let Some(src_id) = work.pop() {
        let obj = source_file.resolve(src_id)?;
        let remapped = remap(&obj, writer, id_map, skip_keys, work)?;
        let dest = id_map
            .get(src_id)
            .expect("work-list entries are always reserved");
        writer.set_reserved_object(dest, remapped);
    }
    Ok(())
}

/// Reserve a fresh destination object number for `source_id` and record it.
pub(crate) fn reserve(
    writer: &mut IncrementalWriter,
    source_id: ObjectId,
    id_map: &mut ObjectIdMap,
) -> Result<ObjectId> {
    let dest = writer.reserve_object_number()?;
    id_map.mapping.insert(source_id, dest);
    Ok(dest)
}

/// Structurally rewrite `obj`: replace each `Ref` with its destination ref,
/// reserving numbers (and queueing the referenced object) for refs seen for
/// the first time. Streams keep their raw (still-encoded) data.
fn remap(
    obj: &PdfObject,
    writer: &mut IncrementalWriter,
    id_map: &mut ObjectIdMap,
    skip_keys: &[&str],
    work: &mut Vec<ObjectId>,
) -> Result<PdfObject> {
    Ok(match obj {
        PdfObject::Ref(r) => {
            let dest = match id_map.get(*r) {
                Some(d) => d,
                None => {
                    let d = reserve(writer, *r, id_map)?;
                    work.push(*r);
                    d
                }
            };
            PdfObject::Ref(dest)
        }
        PdfObject::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for elem in arr {
                out.push(remap(elem, writer, id_map, skip_keys, work)?);
            }
            PdfObject::Array(out)
        }
        PdfObject::Dict(dict) => {
            PdfObject::Dict(remap_dict(dict, writer, id_map, skip_keys, work)?)
        }
        PdfObject::Stream(stream) => PdfObject::Stream(PdfStream {
            dict: remap_dict(&stream.dict, writer, id_map, skip_keys, work)?,
            data: stream.data.clone(),
        }),
        // Scalars carry no references.
        other => other.clone(),
    })
}

fn remap_dict(
    dict: &PdfDict,
    writer: &mut IncrementalWriter,
    id_map: &mut ObjectIdMap,
    skip_keys: &[&str],
    work: &mut Vec<ObjectId>,
) -> Result<PdfDict> {
    let mut out = PdfDict::new();
    for (k, v) in &dict.0 {
        if skip_keys.iter().any(|s| *s == k.as_str()) {
            continue;
        }
        out.insert(k.clone(), remap(v, writer, id_map, skip_keys, work)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_id_map_basics() {
        let mut map = ObjectIdMap::new();
        assert!(map.get(ObjectId(1, 0)).is_none());
        assert!(map.is_empty());
        map.mapping.insert(ObjectId(1, 0), ObjectId(5, 0));
        assert_eq!(map.get(ObjectId(1, 0)), Some(ObjectId(5, 0)));
        assert_eq!(map.len(), 1);
    }
}
