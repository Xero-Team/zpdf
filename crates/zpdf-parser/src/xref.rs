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
    let mut prev = next_prev;
    while let Some(prev_offset) = prev {
        if !visited.insert(prev_offset as usize) {
            break;
        }
        let (_, next) = parse_xref_section(data, prev_offset as usize, &mut table, limits)?;
        prev = next;
    }

    Ok((table, trailer))
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

    // /W [w1 w2 w3] — field widths
    let w_arr = dict.get_array("W")?;
    if w_arr.len() != 3 {
        return Err(Error::InvalidXref(offset as u64));
    }
    let w1 = w_arr[0].as_i64()? as usize;
    let w2 = w_arr[1].as_i64()? as usize;
    let w3 = w_arr[2].as_i64()? as usize;
    let entry_size = w1 + w2 + w3;

    // Decode stream data
    let decoded = filters::decode_stream(&stream.data, dict)?;

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

        for i in 0..count {
            skip_whitespace(data, &mut pos);
            // A standard entry is 20 bytes ("nnnnnnnnnn ggggg n \r\n"); 18 are
            // the minimum parse_xref_entry needs. If fewer remain, the table is
            // truncated/corrupt — bail to recovery rather than under/overflowing.
            let avail = data.len().saturating_sub(pos);
            if avail < 18 {
                return Err(Error::InvalidXref(pos as u64));
            }
            let entry_data = &data[pos..pos + avail.min(20)];

            let (entry_offset, gen, in_use) = parse_xref_entry(entry_data)?;
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

            // Advance past the 20-byte entry
            pos += 20;
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
    let search_size = 1024.min(data.len());
    let tail = &data[data.len() - search_size..];

    let marker = b"startxref";
    let marker_pos = tail
        .windows(marker.len())
        .rposition(|w| w == marker)
        .ok_or(Error::InvalidXref(0))?;

    let after_marker = marker_pos + marker.len();
    let num_start = tail[after_marker..]
        .iter()
        .position(|b| b.is_ascii_digit())
        .ok_or(Error::InvalidXref(0))?;

    let num_bytes = &tail[after_marker + num_start..];
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

fn parse_xref_entry(data: &[u8]) -> Result<(u64, u16, bool)> {
    // Format: "0000000000 65535 f \n" (20 bytes)
    if data.len() < 18 {
        return Err(Error::InvalidXref(0));
    }

    let offset: u64 = std::str::from_utf8(&data[..10])
        .map_err(|_| Error::InvalidXref(0))?
        .trim()
        .parse()
        .map_err(|_| Error::InvalidXref(0))?;

    let gen: u16 = std::str::from_utf8(&data[11..16])
        .map_err(|_| Error::InvalidXref(0))?
        .trim()
        .parse()
        .map_err(|_| Error::InvalidXref(0))?;

    let in_use = data[17] == b'n';

    Ok((offset, gen, in_use))
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
        let entry = b"0000000010 00000 n \r\n";
        let (offset, gen, in_use) = parse_xref_entry(entry).unwrap();
        assert_eq!(offset, 10);
        assert_eq!(gen, 0);
        assert!(in_use);
    }

    #[test]
    fn parse_xref_entry_free() {
        let entry = b"0000000000 65535 f \r\n";
        let (offset, gen, in_use) = parse_xref_entry(entry).unwrap();
        assert_eq!(offset, 0);
        assert_eq!(gen, 65535);
        assert!(!in_use);
    }
}
