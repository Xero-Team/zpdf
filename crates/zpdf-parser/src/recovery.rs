//! Damaged-xref recovery: rebuild a synthetic xref table by linearly scanning
//! the file for `<int> <int> obj` headers, then recover or synthesize a trailer.
//!
//! Best-effort only. Limitations:
//!  * Occurrences of the literal bytes `N G obj` inside binary stream data or
//!    strings can be mis-detected. We mitigate this by requiring the `obj`
//!    keyword to sit at a token boundary preceded by two integers, and by
//!    LATER-wins semantics (the real object header, parsed last in file order
//!    for that id, usually wins). We do NOT attempt to skip over stream bodies
//!    by length because /Length itself may be unreliable in a corrupt file.
//!  * Objects that live only inside an /ObjStm are found via an optional second
//!    pass (`index_objstm_members`) once the direct objects are located.

use zpdf_core::{Error, ObjectId, ParseLimits, PdfDict, PdfName, PdfObject, Result};

use crate::lexer::Lexer;
use crate::object_parser::ObjectParser;
use crate::xref::{XrefEntry, XrefTable};

/// Entry point for recovery. Single linear pass + trailer recovery.
pub fn scan_all_objects(data: &[u8], limits: &ParseLimits) -> Result<(XrefTable, PdfDict)> {
    let headers = scan_object_headers(data, limits)?;

    let mut table = XrefTable::new();
    // LATER occurrence wins (incremental update): overwrite earlier offsets.
    for &(id, offset) in &headers {
        table.insert_overwrite(id, XrefEntry::InUse { offset, gen: id.1 });
    }

    if table.is_empty() {
        return Err(Error::InvalidXref(0));
    }

    // Optional, recommended: index members of any /ObjStm we found. Never fatal.
    index_objstm_members(data, &mut table, limits);

    // Locate a catalog among ALL header occurrences (not just the later-wins
    // survivors) and any /ObjStm members. When the catalog is a direct object
    // shadowed by a later same-id object, re-point the table entry at its true
    // offset so resolve(/Root) lands on the catalog rather than the shadow.
    let catalog = find_catalog(data, &headers, &table, limits);
    if let Some((id, Some(offset))) = catalog {
        table.insert_overwrite(id, XrefEntry::InUse { offset, gen: id.1 });
    }

    // Recovery NEVER fails once objects were found: even with no discoverable
    // catalog we return a (possibly Root-less) trailer so the file opens, and
    // the document layer then scans for /Type /Page objects to build the page
    // list. This is what lets headerless / catalog-less fragments render.
    let trailer = recover_trailer(data, &table, limits, catalog.map(|(id, _)| id));
    Ok((table, trailer))
}

/// Linear scanner. Returns `(id, offset)` for every `<int> <int> obj` header,
/// in file order, where `offset` is the byte position of the first digit of the
/// object number (exactly what `ObjectParser::parse_indirect_at` expects).
fn scan_object_headers(data: &[u8], limits: &ParseLimits) -> Result<Vec<(ObjectId, u64)>> {
    let n = data.len();
    let mut out: Vec<(ObjectId, u64)> = Vec::new();
    let mut i = 0usize;

    while i + 3 <= n {
        if &data[i..i + 3] != b"obj" {
            i += 1;
            continue;
        }
        // "obj" must sit at a token boundary on the right (ws/delim/EOF), so we
        // do not match the "obj" inside "endobj" via its trailing bytes.
        let right_ok = match data.get(i + 3).copied() {
            None => true,
            Some(b) => is_whitespace(b) || is_delimiter(b),
        };
        if !right_ok {
            i += 1;
            continue;
        }

        if let Some((obj_num, gen, start)) = parse_header_backwards(data, i) {
            if out.len() as u32 >= limits.max_objects {
                return Err(Error::InvalidXref(0));
            }
            out.push((ObjectId(obj_num, gen), start as u64));
        }
        // Advance past this keyword regardless, keeping the scan single-pass.
        i += 3;
    }
    Ok(out)
}

/// Given the index of an `obj` keyword, parse the preceding `<int> <int>`.
/// Returns (obj_num, gen, start_offset_of_first_int) or None.
fn parse_header_backwards(data: &[u8], obj_kw: usize) -> Option<(u32, u16, usize)> {
    // require whitespace between gen and "obj"
    let p = skip_ws_back(data, obj_kw)?;
    let (gen_end, gen_start) = take_int_back(data, p)?;
    let gen: u16 = parse_uint(&data[gen_start..gen_end])?.try_into().ok()?;

    let q = skip_ws_back(data, gen_start)?;
    let (num_end, num_start) = take_int_back(data, q)?;
    let obj_num: u32 = parse_uint(&data[num_start..num_end])?;

    // Left boundary: start-of-file or a whitespace/delimiter before the number.
    if num_start > 0 {
        let prev = data[num_start - 1];
        if !(is_whitespace(prev) || is_delimiter(prev)) {
            return None;
        }
    }
    Some((obj_num, gen, num_start))
}

/// Skip a run of whitespace ending at `end` (exclusive). Returns the index of
/// the first whitespace byte of the run, or None if there was no whitespace.
fn skip_ws_back(data: &[u8], end: usize) -> Option<usize> {
    let mut p = end;
    let mut saw = false;
    while p > 0 && is_whitespace(data[p - 1]) {
        p -= 1;
        saw = true;
    }
    if saw {
        Some(p)
    } else {
        None
    }
}

/// Given an index `end` one-past a digit run, return (end, start) covering the
/// contiguous ascii-digit run ending at `end`. None if no digit precedes `end`.
fn take_int_back(data: &[u8], end: usize) -> Option<(usize, usize)> {
    let mut p = end;
    while p > 0 && data[p - 1].is_ascii_digit() {
        p -= 1;
    }
    if p == end {
        None
    } else {
        Some((end, p))
    }
}

fn parse_uint(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() || bytes.len() > 10 {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse::<u32>().ok()
}

/// Recover a trailer. Preference order: an explicit `trailer << ... >>` whose
/// `/Root` actually resolves to a catalog; otherwise the `recovered_catalog`
/// found by [`find_catalog`]; otherwise the explicit trailer's (unvalidated)
/// `/Root` as a last resort; otherwise a Root-less dict so the file still opens
/// (the document layer scans for `/Type /Page` objects to build the page list).
fn recover_trailer(
    data: &[u8],
    table: &XrefTable,
    limits: &ParseLimits,
    recovered_catalog: Option<ObjectId>,
) -> PdfDict {
    if let Some(mut d) = find_trailer_dict(data, limits) {
        // Trust an explicit /Root only if it genuinely points at a catalog —
        // fuzzed files routinely carry a /Root aimed at a free/wrong object.
        if let Ok(root) = d.get_ref("Root") {
            if root_points_at_catalog(data, table, root, limits) {
                return d;
            }
        }
        if let Some(root) = recovered_catalog {
            d.insert(PdfName::new("Root"), PdfObject::Ref(root));
            return d;
        }
        if d.get_ref("Root").is_ok() {
            return d; // best effort; document layer falls back to a page scan
        }
    }

    let mut trailer = PdfDict::new();
    if let Some(root) = recovered_catalog {
        trailer.insert(PdfName::new("Root"), PdfObject::Ref(root));
    }
    trailer
}

/// True if `root` resolves (via the recovered table, direct or compressed) to a
/// catalog-shaped dict. Used to vet an explicit `trailer /Root` before trusting it.
fn root_points_at_catalog(
    data: &[u8],
    table: &XrefTable,
    root: ObjectId,
    limits: &ParseLimits,
) -> bool {
    let obj = match table.get(root) {
        Some(XrefEntry::InUse { offset, .. }) => {
            ObjectParser::new(data, limits).parse_indirect_at(*offset as usize).ok()
        }
        Some(XrefEntry::Compressed {
            stream_obj,
            index_in_stream,
        }) => decode_objstm_member(data, table, *stream_obj, *index_in_stream, limits),
        _ => None,
    };
    obj.as_ref().map(object_dict).map(dict_is_catalog).unwrap_or(false)
}

/// Find the LAST `trailer` keyword in the buffer and lex the following dict.
fn find_trailer_dict(data: &[u8], limits: &ParseLimits) -> Option<PdfDict> {
    let kw = b"trailer";
    let pos = data.windows(kw.len()).rposition(|w| w == kw)?;
    let mut lex = Lexer::new(data, pos + kw.len(), limits);
    match lex.next_token().ok()? {
        PdfObject::Dict(d) => Some(d),
        _ => None,
    }
}

/// Locate a document catalog among the recovered objects. Scans EVERY object
/// header occurrence (in file order; later wins, matching incremental updates)
/// plus any /ObjStm members, so a catalog shadowed by a later same-id object —
/// or one living only inside an object stream — is still found. Returns the
/// catalog's id and, for a direct object, its true byte offset (so the caller
/// can re-point the xref entry); a `None` offset means a compressed member that
/// is already addressable via its existing `Compressed` table entry.
fn find_catalog(
    data: &[u8],
    headers: &[(ObjectId, u64)],
    table: &XrefTable,
    limits: &ParseLimits,
) -> Option<(ObjectId, Option<u64>)> {
    let parser = ObjectParser::new(data, limits);
    let mut best: Option<(ObjectId, Option<u64>)> = None;
    // Direct headers in file order: a later occurrence supersedes an earlier one.
    for &(id, offset) in headers {
        if let Ok((pid, obj)) = parser.parse_indirect_with_id(offset as usize) {
            if pid == id && dict_is_catalog(object_dict(&obj)) {
                best = Some((id, Some(offset)));
            }
        }
    }
    if best.is_some() {
        return best;
    }
    // None direct: look inside object streams (catalog compressed in an /ObjStm).
    for id in table.object_ids() {
        if let Some(XrefEntry::Compressed {
            stream_obj,
            index_in_stream,
        }) = table.get(id)
        {
            if let Some(obj) = decode_objstm_member(data, table, *stream_obj, *index_in_stream, limits)
            {
                if dict_is_catalog(object_dict(&obj)) {
                    best = Some((id, None));
                }
            }
        }
    }
    best
}

/// The dictionary backing an object, whether a bare dict or a stream's dict.
fn object_dict(obj: &PdfObject) -> Option<&PdfDict> {
    match obj {
        PdfObject::Dict(d) => Some(d),
        PdfObject::Stream(s) => Some(&s.dict),
        _ => None,
    }
}

/// Catalog detection tolerant of byte-flipped/absent `/Type`: a dict is a
/// catalog if `/Type` is `/Catalog`, or — when `/Type` is wrong/absent — it is
/// "catalog-shaped": carries `/Pages` and is neither a page-tree node (`/Kids`)
/// nor a page leaf (`/Parent`/`/Contents`/`/MediaBox`).
fn dict_is_catalog(dict: Option<&PdfDict>) -> bool {
    let Some(d) = dict else { return false };
    if d.get_name("Type").ok() == Some("Catalog") {
        return true;
    }
    d.get("Pages").is_some()
        && d.get("Kids").is_none()
        && d.get("Parent").is_none()
        && d.get("Contents").is_none()
        && d.get("MediaBox").is_none()
}

/// Decode and extract one member of an `/ObjStm` container during recovery
/// (pre-decryptor; encrypted object streams are handled later by the
/// document-level page scan instead). Returns `None` on any malformation rather
/// than erroring — recovery is best-effort. Mirrors the bounds/ordering guards
/// of `PdfFile::extract_from_object_stream`.
fn decode_objstm_member(
    data: &[u8],
    table: &XrefTable,
    stream_obj: u32,
    index_in_stream: u32,
    limits: &ParseLimits,
) -> Option<PdfObject> {
    use crate::filters;
    let off = match table.get(ObjectId(stream_obj, 0))? {
        XrefEntry::InUse { offset, .. } => *offset,
        _ => return None,
    };
    let obj = ObjectParser::new(data, limits)
        .parse_indirect_at(off as usize)
        .ok()?;
    let PdfObject::Stream(stream) = obj else {
        return None;
    };
    if stream.dict.get_name("Type").unwrap_or("") != "ObjStm" {
        return None;
    }
    let n = usize::try_from(stream.dict.get_i64("N").ok()?).ok()?;
    let first = usize::try_from(stream.dict.get_i64("First").ok()?).ok()?;
    let decoded = filters::decode_stream(&stream.data, &stream.dict).ok()?;

    let header = &decoded[..first.min(decoded.len())];
    let mut hlex = Lexer::new(header, 0, limits);
    let mut offsets = Vec::with_capacity(n.min(header.len()));
    for _ in 0..n {
        let _num = hlex.next_token().ok()?.as_i64().ok()?;
        let m_off = usize::try_from(hlex.next_token().ok()?.as_i64().ok()?).ok()?;
        offsets.push(m_off);
    }
    let idx = index_in_stream as usize;
    let start = first.checked_add(*offsets.get(idx)?)?;
    let end = match offsets.get(idx + 1) {
        Some(next) => first.checked_add(*next)?,
        None => decoded.len(),
    }
    .min(decoded.len());
    if start > end {
        return None;
    }
    Lexer::new(&decoded[start..end], 0, limits).next_token().ok()
}

/// For each recovered /Type /ObjStm object, decode it and add Compressed entries
/// for members not already present as direct objects. Never fatal.
fn index_objstm_members(data: &[u8], table: &mut XrefTable, limits: &ParseLimits) {
    use crate::filters;
    let parser = ObjectParser::new(data, limits);
    // Snapshot direct stream ids first (avoid mutating while iterating).
    let stream_ids: Vec<(ObjectId, u64)> = table
        .object_ids()
        .filter_map(|id| match table.get(id) {
            Some(XrefEntry::InUse { offset, .. }) => Some((id, *offset)),
            _ => None,
        })
        .collect();

    for (sid, offset) in stream_ids {
        let Ok(obj) = parser.parse_indirect_at(offset as usize) else {
            continue;
        };
        let PdfObject::Stream(stream) = obj else {
            continue;
        };
        if stream.dict.get_name("Type").unwrap_or("") != "ObjStm" {
            continue;
        }
        let (Ok(n), Ok(first)) = (stream.dict.get_i64("N"), stream.dict.get_i64("First")) else {
            continue;
        };
        let Ok(decoded) = filters::decode_stream(&stream.data, &stream.dict) else {
            tracing::debug!("recovery: failed to decode ObjStm {sid}");
            continue;
        };
        let header = &decoded[..(first as usize).min(decoded.len())];
        let mut hlex = Lexer::new(header, 0, limits);
        for idx in 0..n as u32 {
            let Ok(num_tok) = hlex.next_token() else {
                break;
            };
            let Ok(_off_tok) = hlex.next_token() else {
                break;
            };
            let Ok(member_num) = num_tok.as_i64() else {
                break;
            };
            let member_id = ObjectId(member_num as u32, 0);
            // Direct objects already found by tail-scan take precedence.
            if table.get(member_id).is_none() {
                table.insert_overwrite(
                    member_id,
                    XrefEntry::Compressed {
                        stream_obj: sid.0,
                        index_in_stream: idx,
                    },
                );
            }
        }
    }
}

fn is_whitespace(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n' | 0x00 | 0x0c)
}

fn is_delimiter(b: u8) -> bool {
    matches!(
        b,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PdfFile;

    /// Build a minimal 3-object PDF (Catalog, Pages, Page) whose `startxref`
    /// points at a garbage offset and whose xref table is absent.
    fn broken_pdf() -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(b"%PDF-1.4\n");
        d.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
        d.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
        d.extend_from_slice(
            b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>\nendobj\n",
        );
        d.extend_from_slice(b"startxref\n99999\n%%EOF\n");
        d
    }

    #[test]
    fn recovers_catalog_from_garbage_startxref() {
        let data = broken_pdf();
        let file = PdfFile::parse(data).expect("recovery should open the file");
        let root = file.trailer.get_ref("Root").expect("recovered /Root");
        assert_eq!(root, ObjectId(1, 0));
        let cat = file.resolve(root).unwrap();
        assert_eq!(cat.as_dict().unwrap().get_name("Type").unwrap(), "Catalog");
    }

    #[test]
    fn scan_finds_all_three_objects() {
        let data = broken_pdf();
        let limits = ParseLimits::default();
        let (table, _trailer) = scan_all_objects(&data, &limits).unwrap();
        assert!(table.get(ObjectId(1, 0)).is_some());
        assert!(table.get(ObjectId(2, 0)).is_some());
        assert!(table.get(ObjectId(3, 0)).is_some());
    }

    #[test]
    fn later_occurrence_wins() {
        let mut d = Vec::new();
        d.extend_from_slice(b"%PDF-1.4\n");
        d.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R /Old true >>\nendobj\n");
        d.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [] /Count 0 >>\nendobj\n");
        let second_off = d.len();
        d.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R /New true >>\nendobj\n");
        d.extend_from_slice(b"startxref\n0\n%%EOF\n");

        let limits = ParseLimits::default();
        let (table, _t) = scan_all_objects(&d, &limits).unwrap();
        match table.get(ObjectId(1, 0)).unwrap() {
            XrefEntry::InUse { offset, .. } => assert_eq!(*offset as usize, second_off),
            _ => panic!("expected InUse"),
        }
    }

    #[test]
    fn explicit_trailer_dict_is_preferred() {
        let mut d = Vec::new();
        d.extend_from_slice(b"%PDF-1.4\n");
        d.extend_from_slice(b"7 0 obj\n<< /Type /Catalog /Pages 8 0 R >>\nendobj\n");
        d.extend_from_slice(b"8 0 obj\n<< /Type /Pages /Kids [] /Count 0 >>\nendobj\n");
        d.extend_from_slice(b"trailer\n<< /Root 7 0 R >>\nstartxref\n88888\n%%EOF\n");
        let limits = ParseLimits::default();
        let (_table, trailer) = scan_all_objects(&d, &limits).unwrap();
        assert_eq!(trailer.get_ref("Root").unwrap(), ObjectId(7, 0));
    }

    #[test]
    fn empty_buffer_errors() {
        let data = b"%PDF-1.4\nnothing useful here\n".to_vec();
        let limits = ParseLimits::default();
        assert!(scan_all_objects(&data, &limits).is_err());
    }

    #[test]
    fn respects_max_objects_limit() {
        let mut d = Vec::from(&b"%PDF-1.4\n"[..]);
        for i in 1..=10u32 {
            d.extend_from_slice(format!("{i} 0 obj\n<< >>\nendobj\n").as_bytes());
        }
        let limits = ParseLimits {
            max_objects: 3,
            ..Default::default()
        };
        assert!(scan_all_objects(&d, &limits).is_err());
    }

    #[test]
    fn endobj_is_not_matched_as_header() {
        // "endobj" contains "obj" but must not be parsed as an object header.
        let data = b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog >>\nendobj\n";
        let limits = ParseLimits::default();
        let headers = scan_object_headers(data, &limits).unwrap();
        assert_eq!(headers.len(), 1, "only the real header, not endobj");
        assert_eq!(headers[0].0, ObjectId(1, 0));
    }

    // --- Robustness regressions (adversarial xref/ObjStm inputs must not panic/hang) ---

    #[test]
    fn prev_cycle_terminates() {
        // A trailer whose /Prev points back at its own xref section must not loop
        // forever. Build a valid traditional xref with a self-referencing /Prev.
        let mut d = Vec::new();
        d.extend_from_slice(b"%PDF-1.4\n");
        let off1 = d.len();
        d.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
        let off2 = d.len();
        d.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [] /Count 0 >>\nendobj\n");
        let xref_off = d.len();
        d.extend_from_slice(b"xref\n0 3\n0000000000 65535 f \n");
        d.extend_from_slice(format!("{off1:010} 00000 n \n{off2:010} 00000 n \n").as_bytes());
        d.extend_from_slice(
            format!(
                "trailer\n<< /Size 3 /Root 1 0 R /Prev {xref_off} >>\nstartxref\n{xref_off}\n%%EOF\n"
            )
            .as_bytes(),
        );
        // Must return (not hang). Cycle is broken by the visited-set.
        let file = PdfFile::parse(d).expect("self-/Prev cycle should still open");
        let cat = file.resolve(ObjectId(1, 0)).unwrap();
        assert_eq!(cat.as_dict().unwrap().get_name("Type").unwrap(), "Catalog");
    }

    #[test]
    fn truncated_xref_subsection_does_not_panic() {
        // xref placed at EOF whose subsection claims 500 entries but only one
        // 18-byte entry fits. Must Err out of the normal path (not panic) and
        // recover via tail-scan.
        let mut head = Vec::new();
        head.extend_from_slice(b"%PDF-1.4\n");
        let off1 = head.len();
        head.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
        head.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [] /Count 0 >>\nendobj\n");
        // Fixed-point: the xref begins right after the "startxref\n<off>\n" line.
        let mut off = head.len();
        loop {
            let new_off = head.len() + format!("startxref\n{off}\n").len();
            if new_off == off {
                break;
            }
            off = new_off;
        }
        let mut d = head;
        d.extend_from_slice(format!("startxref\n{off}\n").as_bytes());
        assert_eq!(d.len(), off);
        d.extend_from_slice(b"xref\n0 500\n");
        // One 18-byte entry, no trailing whitespace, ending exactly at EOF.
        d.extend_from_slice(format!("{off1:010} 00000 n").as_bytes());

        let file = PdfFile::parse(d).expect("recovery should open truncated-xref file");
        let cat = file.resolve(ObjectId(1, 0)).unwrap();
        assert_eq!(cat.as_dict().unwrap().get_name("Type").unwrap(), "Catalog");
    }

    #[test]
    fn objstm_descending_offsets_errors_not_panics() {
        // ObjStm with descending member offsets would make data_end < data_start.
        // resolve() must return Err, not panic on the slice.
        let mut d = Vec::new();
        d.extend_from_slice(b"%PDF-1.4\n");
        d.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
        d.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [] /Count 0 >>\nendobj\n");
        // N=2, First=8, header "6 5 7 0 " => member 6 @ offset 5, member 7 @ 0
        // (descending). Body fills out the 16-byte buffer.
        let body: &[u8] = b"6 5 7 0 ABCDEFGH";
        d.extend_from_slice(
            format!(
                "5 0 obj\n<< /Type /ObjStm /N 2 /First 8 /Length {} >>\nstream\n",
                body.len()
            )
            .as_bytes(),
        );
        d.extend_from_slice(body);
        d.extend_from_slice(b"\nendstream\nendobj\n");
        d.extend_from_slice(b"startxref\n99999\n%%EOF\n"); // garbage -> recovery

        let file = PdfFile::parse(d).expect("recovery opens it");
        // Member 6 lives in ObjStm 5 at a descending offset -> clean Err.
        let r = file.resolve(ObjectId(6, 0));
        assert!(r.is_err(), "expected clean Err, got {r:?}");
    }
}
