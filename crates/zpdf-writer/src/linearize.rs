//! Linearization ("fast web view", ISO 32000-1 Annex F).
//!
//! Produces a linearized re-serialization of a document:
//! - the **linearization parameter dictionary** (`/Linearized 1`) is the
//!   first object in the file;
//! - all objects needed to display page 1 (its dict, contents, resources —
//!   plus the catalog) come before the objects of the remaining pages;
//! - a **first-page cross-reference table** sits at the front (after the
//!   parameter dict), the main xref at the end; `/Prev`/`/P` wiring per
//!   Annex F, with the hint stream (`/H`) present but minimal (readers use
//!   hints as an optimization only and must tolerate their absence — the
//!   generic hint offsets we emit are valid but carry no per-page detail).
//!
//! The layout emitted here follows F.3 ("linearized PDF document structure"):
//!   header, lin dict, first-page xref+trailer, catalog + page-1 objects,
//!   hint stream, remaining objects, main xref, startxref, %%EOF.
//!
//! Byte-exact two-pass writing: a first pass with placeholder numbers sizes
//! every offset field, then the placeholders are patched in place (all
//! placeholder fields are fixed-width).

use std::collections::{HashMap, HashSet};

use zpdf_core::{ObjectId, PdfDict, PdfName, PdfObject, PdfStream, Result};
use zpdf_parser::PdfFile;

use crate::serialize::{write_object, write_stream};
use crate::{flate_compress, invalid_data};

/// Re-serialize `source` as a linearized PDF.
pub fn linearize_pdf(source: &PdfFile) -> Result<Vec<u8>> {
    let root = source
        .trailer
        .get_ref("Root")
        .map_err(|_| invalid_data("trailer missing /Root"))?;
    let info = source.trailer.get_ref("Info").ok();

    // ---- classify objects: first-page set vs the rest -----------------------
    let catalog = source.resolve(root)?.as_dict()?.clone();
    let pages_root = catalog
        .get_ref("Pages")
        .map_err(|_| invalid_data("catalog missing /Pages"))?;
    let page_ids = collect_page_ids(source, pages_root)?;
    if page_ids.is_empty() {
        return Err(invalid_data("document has no pages").into());
    }
    let first_page = page_ids[0];

    // Objects reachable from page 1 (its whole graph, /Parent links skipped).
    let mut first_set: HashSet<ObjectId> = HashSet::new();
    reach(source, first_page, &mut first_set, &["Parent"])?;
    // The catalog and pages root belong to the first-page section too.
    first_set.insert(root);
    first_set.insert(pages_root);

    // Everything reachable from the trailer, in BFS order.
    let mut all_order: Vec<ObjectId> = Vec::new();
    let mut seen: HashSet<ObjectId> = HashSet::new();
    let mut queue: Vec<ObjectId> = vec![root];
    if let Some(i) = info {
        queue.push(i);
    }
    for id in &queue {
        seen.insert(*id);
    }
    let mut head = 0;
    while head < queue.len() {
        let id = queue[head];
        head += 1;
        all_order.push(id);
        let obj = source.resolve(id)?;
        collect_refs(&obj, &mut seen, &mut queue);
    }

    // Partition preserving BFS order: first-page objects, then the rest.
    let first_objs: Vec<ObjectId> = all_order
        .iter()
        .copied()
        .filter(|id| first_set.contains(id))
        .collect();
    let rest_objs: Vec<ObjectId> = all_order
        .iter()
        .copied()
        .filter(|id| !first_set.contains(id))
        .collect();

    // ---- assign object numbers ----------------------------------------------
    // Annex F layout puts the first-page objects right after the linearization
    // dict. Numbering: lin dict gets the highest-ish first number by
    // convention, but any assignment is legal as long as xref matches. We use:
    //   1 = linearization dict, 2 = hint stream,
    //   3.. = first-page objects, then the rest.
    let mut number: HashMap<ObjectId, u32> = HashMap::new();
    let mut next = 3u32;
    for id in first_objs.iter().chain(rest_objs.iter()) {
        number.insert(*id, next);
        next += 1;
    }
    let total_objects = next; // includes 0 (free), 1, 2

    // ---- pass 1: serialize with placeholder numeric fields ------------------
    // Fixed-width decimal placeholders let us patch without resizing.
    const W: usize = 10; // width of every patched number

    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n");

    // (1) Linearization parameter dict — /L, /O, /E, /T, /H patched later.
    let lin_ofs = out.len();
    let lin_dict_text = format!(
        "1 0 obj\n<< /Linearized 1 /L {:0w$} /H [{:0w$} {:0w$}] /O {} /E {:0w$} /N {} /T {:0w$} >>\nendobj\n",
        0,
        0,
        0,
        number[&first_page],
        0,
        page_ids.len(),
        0,
        w = W
    );
    out.extend_from_slice(lin_dict_text.as_bytes());

    // (2) First-page xref table (covers objects 1, 2 and the first-page set)
    // plus trailer with /Prev pointing at the main xref (patched later).
    let first_xref_ofs = out.len();
    let first_page_count = 2 + first_objs.len(); // objects 1, 2, then the set
    let mut first_xref = String::new();
    first_xref.push_str(&format!("xref\n1 {}\n", first_page_count));
    for _ in 0..first_page_count {
        first_xref.push_str(&format!("{:010} 00000 n \n", 0));
    }
    first_xref.push_str("trailer\n");
    out.extend_from_slice(first_xref.as_bytes());
    let first_trailer_ofs = out.len();
    let first_trailer = format!(
        "<< /Size {} /Root {} 0 R{} /Prev {:0w$} >>\nstartxref\n0\n%%EOF\n",
        total_objects,
        number[&root],
        match info.and_then(|i| number.get(&i)) {
            Some(n) => format!(" /Info {n} 0 R"),
            None => String::new(),
        },
        0,
        w = W
    );
    out.extend_from_slice(first_trailer.as_bytes());

    // (3) First-page objects.
    let mut offsets: HashMap<u32, u64> = HashMap::new();
    for id in &first_objs {
        let num = number[id];
        offsets.insert(num, out.len() as u64);
        let obj = renumber(&source.resolve(*id)?, &number);
        emit(&mut out, num, obj)?;
    }
    let end_first_page = out.len() as u64; // /E

    // (4) Hint stream (object 2). Minimal but structurally valid: readers
    // treat hints as advisory. Content: a compressed empty page-offset table.
    let hint_ofs = out.len() as u64;
    {
        let mut dict = PdfDict::new();
        dict.insert(
            PdfName::new("Filter"),
            PdfObject::Name(PdfName::new("FlateDecode")),
        );
        dict.insert(PdfName::new("S"), PdfObject::Integer(0));
        let payload = flate_compress(&[0u8; 4]);
        write_stream(&mut out, 2, 0, &dict, &payload).map_err(zpdf_core::Error::Io)?;
    }
    let hint_len = out.len() as u64 - hint_ofs;

    // (5) Remaining objects.
    for id in &rest_objs {
        let num = number[id];
        offsets.insert(num, out.len() as u64);
        let obj = renumber(&source.resolve(*id)?, &number);
        emit(&mut out, num, obj)?;
    }

    // (6) Main xref: object 0 free + objects 1 (lin dict) and 2 (hint stream).
    let main_xref_ofs = out.len() as u64;
    let mut main_xref = String::new();
    main_xref.push_str("xref\n0 3\n");
    main_xref.push_str("0000000000 65535 f \n");
    main_xref.push_str(&format!("{lin_ofs:010} 00000 n \n"));
    main_xref.push_str(&format!("{hint_ofs:010} 00000 n \n"));
    main_xref.push_str(&format!(
        "trailer\n<< /Size 3 >>\nstartxref\n{first_xref_ofs}\n%%EOF\n"
    ));
    out.extend_from_slice(main_xref.as_bytes());
    let total_len = out.len() as u64; // /L

    // ---- pass 2: patch the placeholders --------------------------------------
    // Linearization dict: /L, /H [offset length], /E, /T.
    patch_number(&mut out, lin_ofs, &lin_dict_text, "/L ", total_len, W)?;
    patch_number(&mut out, lin_ofs, &lin_dict_text, "/H [", hint_ofs, W)?;
    patch_second_h(&mut out, lin_ofs, &lin_dict_text, hint_len, W)?;
    patch_number(&mut out, lin_ofs, &lin_dict_text, "/E ", end_first_page, W)?;
    patch_number(&mut out, lin_ofs, &lin_dict_text, "/T ", main_xref_ofs, W)?;

    // First-page xref entries: object 1, 2, then the first-page set.
    {
        let base = first_xref_ofs + format!("xref\n1 {}\n", first_page_count).len();
        let entry = |i: usize| base + i * 20;
        let mut write_entry = |i: usize, ofs: u64| {
            let s = format!("{ofs:010}");
            out[entry(i)..entry(i) + 10].copy_from_slice(s.as_bytes());
        };
        write_entry(0, lin_ofs as u64);
        write_entry(1, hint_ofs);
        for (i, id) in first_objs.iter().enumerate() {
            write_entry(2 + i, offsets[&number[id]]);
        }
    }

    // First trailer /Prev → main xref offset; its startxref stays 0 (per
    // Annex F the first startxref points at 0 or the main xref; readers use
    // the last startxref — which is ours at EOF pointing at the first xref).
    patch_number(
        &mut out,
        first_trailer_ofs,
        &first_trailer,
        "/Prev ",
        main_xref_ofs,
        W,
    )?;

    Ok(out)
}

/// Overwrite the fixed-width decimal following `marker` inside the region that
/// starts at `region_ofs` (whose pass-1 text was `region_text`).
fn patch_number(
    out: &mut [u8],
    region_ofs: usize,
    region_text: &str,
    marker: &str,
    value: u64,
    width: usize,
) -> Result<()> {
    let rel = region_text
        .find(marker)
        .ok_or_else(|| invalid_data("linearization patch marker missing"))?;
    let pos = region_ofs + rel + marker.len();
    let s = format!("{value:0width$}");
    if s.len() != width {
        return Err(invalid_data("linearized offset exceeds placeholder width").into());
    }
    out[pos..pos + width].copy_from_slice(s.as_bytes());
    Ok(())
}

/// The second number of `/H [a b]` — after the first placeholder + a space.
fn patch_second_h(
    out: &mut [u8],
    region_ofs: usize,
    region_text: &str,
    value: u64,
    width: usize,
) -> Result<()> {
    let rel = region_text
        .find("/H [")
        .ok_or_else(|| invalid_data("linearization /H marker missing"))?;
    let pos = region_ofs + rel + "/H [".len() + width + 1;
    let s = format!("{value:0width$}");
    out[pos..pos + width].copy_from_slice(s.as_bytes());
    Ok(())
}

fn emit(out: &mut Vec<u8>, num: u32, obj: PdfObject) -> Result<()> {
    match obj {
        PdfObject::Stream(stream) => {
            write_stream(out, num, 0, &stream.dict, &stream.data).map_err(zpdf_core::Error::Io)
        }
        other => write_object(out, num, 0, &other).map_err(zpdf_core::Error::Io),
    }
}

/// BFS over the reference graph from `start`, skipping `skip_keys` dict keys.
fn reach(
    source: &PdfFile,
    start: ObjectId,
    seen: &mut HashSet<ObjectId>,
    skip_keys: &[&str],
) -> Result<()> {
    let mut queue = vec![start];
    seen.insert(start);
    while let Some(id) = queue.pop() {
        let obj = source.resolve(id)?;
        collect_refs_filtered(&obj, seen, &mut queue, skip_keys);
    }
    Ok(())
}

fn collect_refs(obj: &PdfObject, seen: &mut HashSet<ObjectId>, queue: &mut Vec<ObjectId>) {
    collect_refs_filtered(obj, seen, queue, &[]);
}

fn collect_refs_filtered(
    obj: &PdfObject,
    seen: &mut HashSet<ObjectId>,
    queue: &mut Vec<ObjectId>,
    skip_keys: &[&str],
) {
    match obj {
        PdfObject::Ref(r) => {
            if seen.insert(*r) {
                queue.push(*r);
            }
        }
        PdfObject::Array(arr) => {
            for e in arr {
                collect_refs_filtered(e, seen, queue, skip_keys);
            }
        }
        PdfObject::Dict(d) => {
            for (k, v) in &d.0 {
                if !skip_keys.contains(&k.as_str()) {
                    collect_refs_filtered(v, seen, queue, skip_keys);
                }
            }
        }
        PdfObject::Stream(s) => {
            for (k, v) in &s.dict.0 {
                if !skip_keys.contains(&k.as_str()) {
                    collect_refs_filtered(v, seen, queue, skip_keys);
                }
            }
        }
        _ => {}
    }
}

fn renumber(obj: &PdfObject, map: &HashMap<ObjectId, u32>) -> PdfObject {
    match obj {
        PdfObject::Ref(r) => match map.get(r) {
            Some(&n) => PdfObject::Ref(ObjectId(n, 0)),
            None => PdfObject::Null,
        },
        PdfObject::Array(arr) => PdfObject::Array(arr.iter().map(|e| renumber(e, map)).collect()),
        PdfObject::Dict(d) => {
            let mut out = PdfDict::new();
            for (k, v) in &d.0 {
                out.insert(k.clone(), renumber(v, map));
            }
            PdfObject::Dict(out)
        }
        PdfObject::Stream(s) => {
            let mut dict = PdfDict::new();
            for (k, v) in &s.dict.0 {
                dict.insert(k.clone(), renumber(v, map));
            }
            PdfObject::Stream(PdfStream {
                dict,
                data: s.data.clone(),
            })
        }
        other => other.clone(),
    }
}

/// Page leaf ids in document order (bounded walk).
fn collect_page_ids(source: &PdfFile, pages_root: ObjectId) -> Result<Vec<ObjectId>> {
    const MAX_DEPTH: usize = 64;
    const MAX_PAGES: usize = 1_000_000;
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    fn walk(
        source: &PdfFile,
        node: ObjectId,
        depth: usize,
        seen: &mut HashSet<ObjectId>,
        out: &mut Vec<ObjectId>,
    ) -> Result<()> {
        if depth > MAX_DEPTH || out.len() >= MAX_PAGES || !seen.insert(node) {
            return Ok(());
        }
        let dict = source.resolve(node)?.as_dict()?.clone();
        if dict.get_name("Type").ok() == Some("Pages") || dict.get("Kids").is_some() {
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
                    walk(source, r, depth + 1, seen, out)?;
                }
            }
        } else {
            out.push(node);
        }
        Ok(())
    }
    walk(source, pages_root, 0, &mut seen, &mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linearized_output_parses_and_declares_linearization() {
        let mut b = crate::builder::DocumentBuilder::new();
        let p1 = b.add_page(612.0, 792.0);
        b.add_text(
            p1,
            "page one",
            50.0,
            700.0,
            "Helvetica",
            12.0,
            (0.0, 0.0, 0.0),
        )
        .unwrap();
        let p2 = b.add_page(612.0, 792.0);
        b.add_text(
            p2,
            "page two",
            50.0,
            700.0,
            "Times-Roman",
            12.0,
            (0.0, 0.0, 0.0),
        )
        .unwrap();
        let source_bytes = b.build().unwrap();
        let source = zpdf_parser::PdfFile::parse(source_bytes).unwrap();

        let lin = linearize_pdf(&source).unwrap();
        let text = String::from_utf8_lossy(&lin);
        assert!(text.contains("/Linearized 1"), "lin dict present");
        // The lin dict must be the first object after the header.
        let first_obj = text.find(" obj").unwrap();
        assert!(text[..first_obj].contains("1 0"), "lin dict is object 1");

        // The output must reparse with both pages intact.
        let reopened = zpdf_parser::PdfFile::parse(lin).unwrap();
        let root = reopened.trailer.get_ref("Root").unwrap();
        assert!(reopened.resolve(root).is_ok());
        let doc = zpdf_document::PdfDocument::open(reopened.data().to_vec()).unwrap();
        assert_eq!(doc.page_count(), 2);
    }
}
