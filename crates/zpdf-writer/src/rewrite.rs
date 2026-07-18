//! Full-document rewrite: garbage-collect and re-serialize a PDF from its
//! object graph.
//!
//! Unlike the incremental writer (which appends to the original bytes), the
//! rewriter emits a **fresh file** containing only the objects reachable from
//! the trailer (`/Root`, `/Info`), renumbered densely from 1. This:
//!
//! - drops unreachable objects (orphans from incremental edits, dead page
//!   trees, superseded object generations);
//! - inlines objects out of object streams (`/ObjStm` containers are not
//!   themselves reachable and disappear);
//! - **decrypts**: `PdfFile::resolve` transparently decrypts strings and
//!   stream payloads, so rewriting an encrypted document (opened with its
//!   password) produces a plain-text equivalent — `/Encrypt` is dropped;
//! - optionally Flate-compresses streams that carry no `/Filter`.
//!
//! Stream payloads are otherwise copied verbatim (still filter-encoded), so
//! content is preserved byte-for-byte.

use std::collections::HashMap;
use std::io;

use zpdf_core::{ObjectId, PdfDict, PdfName, PdfObject, PdfStream, Result};
use zpdf_parser::PdfFile;

use crate::serialize::{write_object, write_stream};
use crate::{flate_compress, invalid_data};

/// Options for [`rewrite_pdf`].
#[derive(Debug, Clone)]
pub struct RewriteOptions {
    /// Flate-compress streams that have no `/Filter` and are at least 64
    /// bytes. Streams that already carry a filter are never touched.
    pub compress_uncompressed: bool,
}

impl Default for RewriteOptions {
    fn default() -> Self {
        Self {
            compress_uncompressed: true,
        }
    }
}

/// Rewrite `source` as a fresh, garbage-collected PDF. See module docs.
pub fn rewrite_pdf(source: &PdfFile, options: &RewriteOptions) -> Result<Vec<u8>> {
    let root = source
        .trailer
        .get_ref("Root")
        .map_err(|_| invalid_data("trailer missing /Root"))?;
    let info = source.trailer.get_ref("Info").ok();

    // Pass 1: breadth-first reachability walk building old-id → new-number
    // mapping in discovery order (root first, so the catalog is object 1).
    let mut map: HashMap<ObjectId, u32> = HashMap::new();
    let mut order: Vec<ObjectId> = Vec::new();
    let mut queue: Vec<ObjectId> = vec![root];
    if let Some(info_id) = info {
        queue.push(info_id);
    }
    for id in &queue {
        map.insert(*id, 0); // placeholder, numbered below
    }
    let mut head = 0;
    while head < queue.len() {
        let id = queue[head];
        head += 1;
        order.push(id);
        let obj = source.resolve(id)?;
        collect_refs(&obj, &mut map, &mut queue);
    }
    for (n, id) in order.iter().enumerate() {
        map.insert(*id, (n + 1) as u32);
    }

    // Pass 2: serialize in new-number order.
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n");
    let mut offsets: Vec<u64> = Vec::with_capacity(order.len());
    for (n, id) in order.iter().enumerate() {
        let new_num = (n + 1) as u32;
        offsets.push(out.len() as u64);
        let obj = renumber(&source.resolve(*id)?, &map);
        emit(&mut out, new_num, obj, options).map_err(zpdf_core::Error::Io)?;
    }

    // Xref table + trailer.
    let xref_pos = out.len();
    let size = order.len() + 1;
    out.extend_from_slice(format!("xref\n0 {size}\n").as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for offset in &offsets {
        if *offset > 9_999_999_999 {
            return Err(invalid_data("xref offset exceeds ten decimal digits").into());
        }
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    let mut trailer = PdfDict::new();
    trailer.insert(PdfName::new("Size"), PdfObject::Integer(size as i64));
    trailer.insert(
        PdfName::new("Root"),
        PdfObject::Ref(ObjectId(map[&root], 0)),
    );
    if let Some(info_id) = info {
        if let Some(&n) = map.get(&info_id) {
            trailer.insert(PdfName::new("Info"), PdfObject::Ref(ObjectId(n, 0)));
        }
    }
    // Keep the original /ID (first element identifies the document across
    // revisions); /Encrypt and /Prev are intentionally dropped.
    if let Some(id_arr @ PdfObject::Array(_)) = source.trailer.get("ID") {
        trailer.insert(PdfName::new("ID"), id_arr.clone());
    }
    out.extend_from_slice(b"trailer\n");
    crate::serialize::serialize_dict(&mut out, &trailer).map_err(zpdf_core::Error::Io)?;
    out.extend_from_slice(format!("\nstartxref\n{xref_pos}\n%%EOF\n").as_bytes());
    Ok(out)
}

/// Queue every indirect reference in `obj` that has not been seen yet.
fn collect_refs(obj: &PdfObject, map: &mut HashMap<ObjectId, u32>, queue: &mut Vec<ObjectId>) {
    match obj {
        PdfObject::Ref(r) => {
            if !map.contains_key(r) {
                map.insert(*r, 0);
                queue.push(*r);
            }
        }
        PdfObject::Array(arr) => {
            for elem in arr {
                collect_refs(elem, map, queue);
            }
        }
        PdfObject::Dict(dict) => {
            for v in dict.0.values() {
                collect_refs(v, map, queue);
            }
        }
        PdfObject::Stream(stream) => {
            for v in stream.dict.0.values() {
                collect_refs(v, map, queue);
            }
        }
        _ => {}
    }
}

/// Structurally rewrite references through the completed mapping. A ref to an
/// unmapped object (impossible after a full walk, defensive) becomes null.
fn renumber(obj: &PdfObject, map: &HashMap<ObjectId, u32>) -> PdfObject {
    match obj {
        PdfObject::Ref(r) => match map.get(r) {
            Some(&n) => PdfObject::Ref(ObjectId(n, 0)),
            None => PdfObject::Null,
        },
        PdfObject::Array(arr) => PdfObject::Array(arr.iter().map(|e| renumber(e, map)).collect()),
        PdfObject::Dict(dict) => PdfObject::Dict(renumber_dict(dict, map)),
        PdfObject::Stream(stream) => PdfObject::Stream(PdfStream {
            dict: renumber_dict(&stream.dict, map),
            data: stream.data.clone(),
        }),
        other => other.clone(),
    }
}

fn renumber_dict(dict: &PdfDict, map: &HashMap<ObjectId, u32>) -> PdfDict {
    let mut out = PdfDict::new();
    for (k, v) in &dict.0 {
        out.insert(k.clone(), renumber(v, map));
    }
    out
}

/// Serialize one object, optionally Flate-compressing bare streams.
fn emit(out: &mut Vec<u8>, num: u32, obj: PdfObject, options: &RewriteOptions) -> io::Result<()> {
    match obj {
        PdfObject::Stream(stream) => {
            let has_filter = stream.dict.get("Filter").is_some();
            if options.compress_uncompressed && !has_filter && stream.data.len() >= 64 {
                let compressed = flate_compress(&stream.data);
                // Only keep the compressed form when it actually helps.
                if compressed.len() < stream.data.len() {
                    let mut dict = stream.dict.clone();
                    dict.insert(
                        PdfName::new("Filter"),
                        PdfObject::Name(PdfName::new("FlateDecode")),
                    );
                    return write_stream(out, num, 0, &dict, &compressed);
                }
            }
            write_stream(out, num, 0, &stream.dict, &stream.data)
        }
        other => write_object(out, num, 0, &other),
    }
}
