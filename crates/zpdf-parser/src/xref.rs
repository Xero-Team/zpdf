use std::collections::HashMap;

use zpdf_core::{Error, ObjectId, ParseLimits, PdfDict, PdfObject, Result};

use crate::lexer::Lexer;

#[derive(Debug, Clone)]
pub enum XrefEntry {
    InUse {
        offset: u64,
        gen: u16,
    },
    Free {
        next: u32,
        gen: u16,
    },
    Compressed {
        stream_obj: u32,
        index_in_stream: u32,
    },
}

#[derive(Debug, Clone, Default)]
pub struct XrefTable {
    entries: HashMap<ObjectId, XrefEntry>,
}

impl XrefTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, id: ObjectId) -> Option<&XrefEntry> {
        self.entries.get(&id)
    }

    pub fn insert(&mut self, id: ObjectId, entry: XrefEntry) {
        self.entries.entry(id).or_insert(entry);
    }

    /// Insert overwriting any existing entry. Used by tail-scan recovery, where a
    /// later byte offset for the same ObjectId supersedes an earlier one
    /// (incremental-update semantics). The regular `insert` is first-wins because
    /// the /Prev chain walks newest-to-oldest.
    pub fn insert_overwrite(&mut self, id: ObjectId, entry: XrefEntry) {
        self.entries.insert(id, entry);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn object_ids(&self) -> impl Iterator<Item = ObjectId> + '_ {
        self.entries.keys().copied()
    }
}

pub fn parse_xref_and_trailer(data: &[u8], limits: &ParseLimits) -> Result<(XrefTable, PdfDict)> {
    let startxref_offset = find_startxref(data)?;
    let mut table = XrefTable::new();

    let xref_offset = startxref_offset;
    let (trailer, next_prev) = parse_xref_section(data, xref_offset, &mut table, limits)?;

    // Follow /Prev chain for incremental updates. Track visited offsets so a
    // malformed /Prev cycle (a section pointing at itself, or two pointing at
    // each other) terminates instead of looping forever.
    let mut visited = std::collections::HashSet::new();
    visited.insert(xref_offset);
    // Hybrid-reference file: parse the trailer's /XRefStm BEFORE following
    // /Prev, so first-wins insertion yields main table > XRefStm > /Prev.
    parse_hybrid_xrefstm(data, &trailer, &mut table, limits, &mut visited);
    let mut prev = next_prev;
    while let Some(prev_offset) = prev {
        if !visited.insert(prev_offset as usize) {
            break;
        }
        let (section_trailer, next) =
            parse_xref_section(data, prev_offset as usize, &mut table, limits)?;
        parse_hybrid_xrefstm(data, &section_trailer, &mut table, limits, &mut visited);
        prev = next;
    }

    Ok((table, trailer))
}

/// Hybrid-reference files (ISO 32000-1, 7.5.8.4): a traditional trailer may
/// carry `/XRefStm`, the byte offset of a cross-reference *stream* holding the
/// entries (typically the compressed-object ones) that pre-1.5 readers ignore.
/// The stream is parsed after the section that referenced it but before that
/// section's /Prev; with first-wins insertion this gives the spec precedence
/// main-table > XRefStm > /Prev chain. Never fatal: a broken /XRefStm only
/// loses the hybrid entries.
fn parse_hybrid_xrefstm(
    data: &[u8],
    trailer: &PdfDict,
    table: &mut XrefTable,
    limits: &ParseLimits,
    visited: &mut std::collections::HashSet<usize>,
) {
    let Some(PdfObject::Integer(off)) = trailer.get("XRefStm") else {
        return;
    };
    let Ok(off) = usize::try_from(*off) else {
        tracing::warn!("/XRefStm offset {off} is negative; ignoring");
        return;
    };
    // Guard against /XRefStm cycles via the same visited set as /Prev.
    if !visited.insert(off) {
        return;
    }
    if let Err(e) = parse_xref_stream(data, off, table, limits) {
        tracing::warn!("failed to parse /XRefStm at offset {off}: {e}");
    }
}

fn parse_xref_section(
    data: &[u8],
    offset: usize,
    table: &mut XrefTable,
    limits: &ParseLimits,
) -> Result<(PdfDict, Option<u64>)> {
    // Guard against a garbage `startxref`/`/Prev` offset pointing past EOF.
    // Returning Err (rather than panicking on the slice below) lets the caller
    // fall back to tail-scan recovery.
    if offset >= data.len() {
        return Err(Error::InvalidXref(offset as u64));
    }
    if data[offset..].starts_with(b"xref") {
        parse_traditional_xref(data, offset, table, limits)
    } else {
        parse_xref_stream(data, offset, table, limits)
    }
}

fn parse_xref_stream(
    data: &[u8],
    offset: usize,
    table: &mut XrefTable,
    limits: &ParseLimits,
) -> Result<(PdfDict, Option<u64>)> {
    use crate::filters;
    use crate::object_parser::ObjectParser;

    let parser = ObjectParser::new(data, limits);
    let obj = parser.parse_indirect_at(offset)?;
    let stream = match obj {
        PdfObject::Stream(s) => s,
        _ => return Err(Error::InvalidXref(offset as u64)),
    };

    let dict = &stream.dict;
    if dict.get_name("Type").unwrap_or("") != "XRef" {
        return Err(Error::InvalidXref(offset as u64));
    }

    let size = dict.get_i64("Size")? as u32;

    // /W [w1 w2 w3] — field widths. Attacker-controlled: a negative width cast
    // to usize would explode entry_size, and widths above 8 cannot fit the u64
    // accumulator in read_field; reject both with a clean error.
    let w_arr = dict.get_array("W")?;
    if w_arr.len() != 3 {
        return Err(Error::InvalidXref(offset as u64));
    }
    let field_width = |obj: &PdfObject| -> Result<usize> {
        let w = obj.as_i64()?;
        if !(0..=8).contains(&w) {
            return Err(Error::InvalidXref(offset as u64));
        }
        Ok(w as usize)
    };
    let w1 = field_width(&w_arr[0])?;
    let w2 = field_width(&w_arr[1])?;
    let w3 = field_width(&w_arr[2])?;
    let entry_size = w1 + w2 + w3;
    if entry_size == 0 {
        return Err(Error::InvalidXref(offset as u64));
    }

    // Decode stream data using explicit limits for H1 security fix
    let decoded = filters::decode_stream_with_limits(&stream.data, dict, limits)?;

    // /Index [start count start count ...] — subsection ranges (optional)
    let index_ranges: Vec<(u32, u32)> = if let Ok(idx_arr) = dict.get_array("Index") {
        idx_arr
            .chunks(2)
            .filter_map(|pair| {
                if pair.len() == 2 {
                    Some((pair[0].as_i64().ok()? as u32, pair[1].as_i64().ok()? as u32))
                } else {
                    None
                }
            })
            .collect()
    } else {
        vec![(0, size)]
    };

    let mut pos = 0usize;
    for &(start, count) in &index_ranges {
        // H3 Fix: Validate range end doesn't overflow u32 before processing
        let _range_end = start
            .checked_add(count)
            .ok_or(Error::InvalidXref(offset as u64))?;

        // H3 Fix: Check total entry count against max_objects before processing range
        let new_total = table
            .len()
            .checked_add(count as usize)
            .ok_or(Error::InvalidXref(offset as u64))?;
        if new_total > limits.max_objects as usize {
            return Err(Error::InvalidXref(offset as u64));
        }

        for i in 0..count {
            if pos + entry_size > decoded.len() {
                break;
            }
            let obj_num = start + i;

            let field1 = read_field(&decoded[pos..], w1);
            let field2 = read_field(&decoded[pos + w1..], w2);
            let field3 = read_field(&decoded[pos + w1 + w2..], w3);
            pos += entry_size;

            let entry_type = if w1 == 0 { 1 } else { field1 as u8 };
            let id = ObjectId(obj_num, field3 as u16);

            match entry_type {
                0 => {
                    table.insert(
                        id,
                        XrefEntry::Free {
                            next: field2 as u32,
                            gen: field3 as u16,
                        },
                    );
                }
                1 => {
                    table.insert(
                        id,
                        XrefEntry::InUse {
                            offset: field2,
                            gen: field3 as u16,
                        },
                    );
                }
                2 => {
                    table.insert(
                        ObjectId(obj_num, 0),
                        XrefEntry::Compressed {
                            stream_obj: field2 as u32,
                            index_in_stream: field3 as u32,
                        },
                    );
                }
                _ => {}
            }
        }
    }

    // The xref stream dict itself serves as the trailer
    let trailer = dict.clone();
    let prev = trailer.get("Prev").and_then(|obj| match obj {
        PdfObject::Integer(n) => Some(*n as u64),
        _ => None,
    });

    Ok((trailer, prev))
}

fn read_field(data: &[u8], width: usize) -> u64 {
    let mut val = 0u64;
    for &byte in &data[..width] {
        val = (val << 8) | byte as u64;
    }
    val
}

fn parse_traditional_xref(
    data: &[u8],
    offset: usize,
    table: &mut XrefTable,
    limits: &ParseLimits,
) -> Result<(PdfDict, Option<u64>)> {
    let mut pos = offset + 4; // skip "xref"
    skip_eol(data, &mut pos);

    // Parse subsections
    loop {
        skip_whitespace(data, &mut pos);

        // Guard against a malformed/truncated table that ran the cursor off the
        // end (e.g. a subsection /count larger than the entries that fit).
        // Returning Err routes to tail-scan recovery instead of panicking.
        if pos >= data.len() {
            return Err(Error::InvalidXref(pos as u64));
        }

        if data[pos..].starts_with(b"trailer") {
            pos += 7;
            break;
        }

        // Read: <first_obj_num> <count>
        let (first_obj, count) = parse_subsection_header(data, &mut pos)?;

        // L7 Fix: Validate subsection range doesn't overflow object ID space
        let range_end = first_obj
            .checked_add(count)
            .ok_or(Error::InvalidXref(pos as u64))?;
        if range_end > limits.max_objects {
            return Err(Error::InvalidXref(pos as u64));
        }

        // L7 Fix: Check total entry count against limits before processing
        let new_total = table
            .len()
            .checked_add(count as usize)
            .ok_or(Error::InvalidXref(pos as u64))?;
        if new_total > limits.max_objects as usize {
            return Err(Error::InvalidXref(pos as u64));
        }

        for i in 0..count {
            skip_whitespace(data, &mut pos);
            // An entry is nominally 20 bytes ("nnnnnnnnnn ggggg n \r\n"), but
            // real files contain 19-byte variants (lone \n or \r terminator).
            // Parse by tokens, not a fixed stride, so short entries cannot
            // desync the rest of the table. A truncated/corrupt entry Errs out
            // to tail-scan recovery rather than under/overflowing.
            let (entry_offset, gen, in_use) = parse_xref_entry_at(data, &mut pos)?;
            let id = ObjectId(first_obj + i, gen);

            if in_use {
                table.insert(
                    id,
                    XrefEntry::InUse {
                        offset: entry_offset,
                        gen,
                    },
                );
            } else {
                table.insert(
                    id,
                    XrefEntry::Free {
                        next: entry_offset as u32,
                        gen,
                    },
                );
            }
        }
    }

    // Parse trailer dictionary
    let mut lex = Lexer::new(data, pos, limits);
    let trailer_obj = lex.next_token()?;
    let trailer = match trailer_obj {
        PdfObject::Dict(d) => d,
        _ => return Err(Error::InvalidXref(pos as u64)),
    };

    let prev = trailer.get("Prev").and_then(|obj| match obj {
        PdfObject::Integer(n) => Some(*n as u64),
        _ => None,
    });

    Ok((trailer, prev))
}

fn find_startxref(data: &[u8]) -> Result<usize> {
    // Search the WHOLE buffer for the LAST `startxref` rather than only the
    // final 1 KiB. Real files frequently carry substantial trailing bytes after
    // the last %%EOF (truncated incremental appends, fuzzer junk, appended
    // objects); a tail-only window misses the real startxref and forfeits an
    // otherwise-valid xref. This runs once per open, so the full rposition is
    // affordable, and a wrong hit still falls through to tail-scan recovery.
    let marker = b"startxref";
    let marker_pos = data
        .windows(marker.len())
        .rposition(|w| w == marker)
        .ok_or(Error::InvalidXref(0))?;

    let after_marker = marker_pos + marker.len();
    let num_start = data[after_marker..]
        .iter()
        .position(|b| b.is_ascii_digit())
        .ok_or(Error::InvalidXref(0))?;

    let num_bytes = &data[after_marker + num_start..];
    let num_end = num_bytes
        .iter()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(num_bytes.len());

    let offset_str =
        std::str::from_utf8(&num_bytes[..num_end]).map_err(|_| Error::InvalidXref(0))?;
    let offset: usize = offset_str.parse().map_err(|_| Error::InvalidXref(0))?;

    Ok(offset)
}

fn parse_subsection_header(data: &[u8], pos: &mut usize) -> Result<(u32, u32)> {
    let start = *pos;
    while *pos < data.len() && data[*pos].is_ascii_digit() {
        *pos += 1;
    }
    let first: u32 = std::str::from_utf8(&data[start..*pos])
        .map_err(|_| Error::InvalidXref(start as u64))?
        .parse()
        .map_err(|_| Error::InvalidXref(start as u64))?;

    skip_whitespace(data, pos);

    let count_start = *pos;
    while *pos < data.len() && data[*pos].is_ascii_digit() {
        *pos += 1;
    }
    let count: u32 = std::str::from_utf8(&data[count_start..*pos])
        .map_err(|_| Error::InvalidXref(count_start as u64))?
        .parse()
        .map_err(|_| Error::InvalidXref(count_start as u64))?;

    skip_eol(data, pos);
    Ok((first, count))
}

/// Parse a single traditional xref entry ("nnnnnnnnnn ggggg n") at `*pos`,
/// advancing the cursor just past the type letter. Field widths are not
/// assumed: digit runs of any length and any amount of inter-field whitespace
/// are accepted, which tolerates the 19-byte entries some writers emit.
fn parse_xref_entry_at(data: &[u8], pos: &mut usize) -> Result<(u64, u16, bool)> {
    let start = *pos as u64;
    let offset = read_decimal(data, pos).ok_or(Error::InvalidXref(start))?;
    skip_whitespace(data, pos);
    let gen = read_decimal(data, pos)
        .and_then(|g| u16::try_from(g).ok())
        .ok_or(Error::InvalidXref(start))?;
    skip_whitespace(data, pos);
    let in_use = match data.get(*pos) {
        Some(b'n') => true,
        Some(b'f') => false,
        _ => return Err(Error::InvalidXref(start)),
    };
    *pos += 1;
    Ok((offset, gen, in_use))
}

/// Read a run of ASCII digits at `*pos` as a u64, advancing past it.
/// `None` if there is no digit at `*pos` or the value overflows.
fn read_decimal(data: &[u8], pos: &mut usize) -> Option<u64> {
    let start = *pos;
    while *pos < data.len() && data[*pos].is_ascii_digit() {
        *pos += 1;
    }
    if *pos == start {
        return None;
    }
    std::str::from_utf8(&data[start..*pos]).ok()?.parse().ok()
}

fn skip_whitespace(data: &[u8], pos: &mut usize) {
    while *pos < data.len() && matches!(data[*pos], b' ' | b'\t' | b'\r' | b'\n') {
        *pos += 1;
    }
}

fn skip_eol(data: &[u8], pos: &mut usize) {
    while *pos < data.len() && matches!(data[*pos], b' ' | b'\t' | b'\r' | b'\n') {
        *pos += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_startxref_offset() {
        let data = b"%PDF-1.4\n...lots of content...\nstartxref\n1234\n%%EOF";
        let offset = find_startxref(data).unwrap();
        assert_eq!(offset, 1234);
    }

    #[test]
    fn parse_xref_entry_in_use() {
        let mut pos = 0usize;
        let (offset, gen, in_use) =
            parse_xref_entry_at(b"0000000010 00000 n \r\n", &mut pos).unwrap();
        assert_eq!(offset, 10);
        assert_eq!(gen, 0);
        assert!(in_use);
        assert_eq!(pos, 18, "cursor stops just past the type letter");
    }

    #[test]
    fn parse_xref_entry_free() {
        let mut pos = 0usize;
        let (offset, gen, in_use) =
            parse_xref_entry_at(b"0000000000 65535 f \r\n", &mut pos).unwrap();
        assert_eq!(offset, 0);
        assert_eq!(gen, 65535);
        assert!(!in_use);
    }

    #[test]
    fn parse_xref_entry_truncated_errors() {
        let mut pos = 0usize;
        assert!(parse_xref_entry_at(b"0000000010 000", &mut pos).is_err());
    }

    #[test]
    fn traditional_xref_with_19_byte_entries() {
        // Entries terminated by a lone \n (19 bytes) must not desync the table.
        let mut d = Vec::new();
        d.extend_from_slice(b"%PDF-1.4\n");
        let off1 = d.len();
        d.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
        let off2 = d.len();
        d.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [] /Count 0 >>\nendobj\n");
        let xref_off = d.len();
        d.extend_from_slice(b"xref\n0 3\n");
        d.extend_from_slice(b"0000000000 65535 f\n"); // 19 bytes
        d.extend_from_slice(format!("{off1:010} 00000 n\n").as_bytes()); // 19 bytes
        d.extend_from_slice(format!("{off2:010} 00000 n\n").as_bytes()); // 19 bytes
        d.extend_from_slice(
            format!("trailer\n<< /Size 3 /Root 1 0 R >>\nstartxref\n{xref_off}\n%%EOF\n")
                .as_bytes(),
        );

        let (table, trailer) = parse_xref_and_trailer(&d, &ParseLimits::default()).unwrap();
        assert_eq!(trailer.get_ref("Root").unwrap(), ObjectId(1, 0));
        match table.get(ObjectId(1, 0)).unwrap() {
            XrefEntry::InUse { offset, .. } => assert_eq!(*offset as usize, off1),
            other => panic!("expected InUse, got {other:?}"),
        }
        match table.get(ObjectId(2, 0)).unwrap() {
            XrefEntry::InUse { offset, .. } => assert_eq!(*offset as usize, off2),
            other => panic!("expected InUse, got {other:?}"),
        }
    }

    /// Build a minimal xref-stream object at offset 0 with the given /W array
    /// and raw (unfiltered) entry data.
    fn xref_stream_bytes(w: &str, size: u32, index: &str, body: &[u8]) -> Vec<u8> {
        let mut d = format!(
            "9 0 obj\n<< /Type /XRef /Size {size} /W {w} {index} /Length {} >>\nstream\n",
            body.len()
        )
        .into_bytes();
        d.extend_from_slice(body);
        d.extend_from_slice(b"\nendstream\nendobj\n");
        d
    }

    #[test]
    fn xref_stream_rejects_negative_w_width() {
        let d = xref_stream_bytes("[1 -2 2]", 1, "", &[]);
        let mut table = XrefTable::new();
        assert!(parse_xref_stream(&d, 0, &mut table, &ParseLimits::default()).is_err());
    }

    #[test]
    fn xref_stream_rejects_oversized_w_width() {
        let d = xref_stream_bytes("[9 4 2]", 1, "", &[]);
        let mut table = XrefTable::new();
        assert!(parse_xref_stream(&d, 0, &mut table, &ParseLimits::default()).is_err());
    }

    #[test]
    fn xref_stream_rejects_zero_entry_size() {
        let d = xref_stream_bytes("[0 0 0]", 1, "", &[]);
        let mut table = XrefTable::new();
        assert!(parse_xref_stream(&d, 0, &mut table, &ParseLimits::default()).is_err());
    }

    #[test]
    fn hybrid_xrefstm_is_parsed_with_correct_precedence() {
        // Hybrid-reference layout: the traditional table covers objects 0,1,4;
        // the trailer's /XRefStm points at an xref stream that covers 4 and 5.
        // Object 5 must come from the stream; object 4 must keep the
        // main-table offset (main table wins over /XRefStm).
        let mut d = Vec::new();
        d.extend_from_slice(b"%PDF-1.4\n");
        let off1 = d.len();
        d.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
        let off4_table = d.len();
        d.extend_from_slice(b"4 0 obj\n<< /Marker /FromTable >>\nendobj\n");
        let off4_stm = d.len();
        d.extend_from_slice(b"4 0 obj\n<< /Marker /FromStm >>\nendobj\n");
        let off5 = d.len();
        d.extend_from_slice(b"5 0 obj\n<< /Marker /StmOnly >>\nendobj\n");

        // Xref stream (object 6): /W [1 4 2], /Index [4 2], raw (no filter).
        let mut body = Vec::new();
        for (off, gen) in [(off4_stm as u32, 0u16), (off5 as u32, 0)] {
            body.push(1u8); // type 1: in use
            body.extend_from_slice(&off.to_be_bytes());
            body.extend_from_slice(&gen.to_be_bytes());
        }
        let off6 = d.len();
        d.extend_from_slice(
            format!(
                "6 0 obj\n<< /Type /XRef /Size 7 /W [1 4 2] /Index [4 2] /Length {} >>\nstream\n",
                body.len()
            )
            .as_bytes(),
        );
        d.extend_from_slice(&body);
        d.extend_from_slice(b"\nendstream\nendobj\n");

        let xref_off = d.len();
        d.extend_from_slice(b"xref\n0 2\n0000000000 65535 f \n");
        d.extend_from_slice(format!("{off1:010} 00000 n \n").as_bytes());
        d.extend_from_slice(b"4 1\n");
        d.extend_from_slice(format!("{off4_table:010} 00000 n \n").as_bytes());
        d.extend_from_slice(
            format!(
                "trailer\n<< /Size 7 /Root 1 0 R /XRefStm {off6} >>\nstartxref\n{xref_off}\n%%EOF\n"
            )
            .as_bytes(),
        );

        let (table, trailer) = parse_xref_and_trailer(&d, &ParseLimits::default()).unwrap();
        assert_eq!(trailer.get_ref("Root").unwrap(), ObjectId(1, 0));
        // Object 5 exists only via the /XRefStm.
        match table.get(ObjectId(5, 0)).unwrap() {
            XrefEntry::InUse { offset, .. } => assert_eq!(*offset as usize, off5),
            other => panic!("expected InUse from XRefStm, got {other:?}"),
        }
        // Object 4: the traditional table's offset wins over the stream's.
        match table.get(ObjectId(4, 0)).unwrap() {
            XrefEntry::InUse { offset, .. } => assert_eq!(*offset as usize, off4_table),
            other => panic!("expected InUse, got {other:?}"),
        }

        // End-to-end: the document opens and the stream-only object resolves.
        let file = crate::PdfFile::parse(d).unwrap();
        let o5 = file.resolve(ObjectId(5, 0)).unwrap();
        assert_eq!(o5.as_dict().unwrap().get_name("Marker").unwrap(), "StmOnly");
        let o4 = file.resolve(ObjectId(4, 0)).unwrap();
        assert_eq!(
            o4.as_dict().unwrap().get_name("Marker").unwrap(),
            "FromTable"
        );
    }
}
