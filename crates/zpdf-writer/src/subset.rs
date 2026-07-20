//! TrueType font subsetting: strip outlines of unused glyphs.
//!
//! Keeps the sfnt structure intact (same glyph count, same cmap/hmtx), but
//! empties every `glyf` entry that is not reachable from the kept glyph set —
//! the "sparse glyf" subset. This preserves metrics and code→glyph mapping
//! (so text extraction and reflow behave identically) while dropping the
//! bulk of the outline data. Composite glyphs keep their components.
//!
//! Only TrueType-flavored fonts (with `glyf`/`loca`) are subset; CFF-flavored
//! input is returned unchanged.

use std::collections::HashSet;

/// Subset `font` to the glyphs used by `chars` (plus .notdef and composite
/// components). Returns the rebuilt font, or `None` when the font cannot be
/// subset (CFF, malformed tables) — callers should embed the original then.
pub fn subset_truetype(font: &[u8], chars: &HashSet<char>) -> Option<Vec<u8>> {
    let face = ttf_parser::Face::parse(font, 0).ok()?;
    let num_glyphs = face.number_of_glyphs();

    // Resolve the kept glyph ids from the used characters.
    let mut keep: HashSet<u16> = HashSet::new();
    keep.insert(0); // .notdef
    for &ch in chars {
        if let Some(gid) = face.glyph_index(ch) {
            keep.insert(gid.0);
        }
    }

    // Locate the raw tables.
    let glyf = raw_table(font, b"glyf")?;
    let loca = raw_table(font, b"loca")?;
    let head = raw_table(font, b"head")?;
    let long_loca = read_u16(&font[head.0..], 50)? == 1;

    let offsets = parse_loca(&font[loca.0..loca.0 + loca.1], num_glyphs, long_loca)?;

    // Transitively include composite components.
    let mut queue: Vec<u16> = keep.iter().copied().collect();
    while let Some(gid) = queue.pop() {
        let Some((start, end)) = glyph_range(&offsets, gid) else {
            continue;
        };
        if end <= start {
            continue;
        }
        let data = glyf
            .1
            .checked_sub(start)
            .and_then(|_| font.get(glyf.0 + start..glyf.0 + end.min(glyf.1)))?;
        for comp in composite_components(data) {
            if keep.insert(comp) {
                queue.push(comp);
            }
        }
    }

    // Rebuild glyf/loca: kept glyphs copy their data, others become empty.
    let mut new_glyf: Vec<u8> = Vec::with_capacity(glyf.1 / 2);
    let mut new_offsets: Vec<u32> = Vec::with_capacity(offsets.len());
    new_offsets.push(0);
    for gid in 0..num_glyphs {
        if keep.contains(&gid) {
            if let Some((start, end)) = glyph_range(&offsets, gid) {
                if end > start && end <= glyf.1 {
                    new_glyf.extend_from_slice(&font[glyf.0 + start..glyf.0 + end]);
                    // glyf entries must be 4-byte aligned for long loca safety.
                    while !new_glyf.len().is_multiple_of(4) {
                        new_glyf.push(0);
                    }
                }
            }
        }
        new_offsets.push(new_glyf.len() as u32);
    }

    // Always write long (32-bit) loca and flag it in head.
    let mut new_loca = Vec::with_capacity(new_offsets.len() * 4);
    for ofs in &new_offsets {
        new_loca.extend_from_slice(&ofs.to_be_bytes());
    }

    rebuild_sfnt(font, &[(*b"glyf", new_glyf), (*b"loca", new_loca)], true)
}

/// (offset, length) of a top-level sfnt table.
fn raw_table(font: &[u8], tag: &[u8; 4]) -> Option<(usize, usize)> {
    let num_tables = read_u16(font, 4)? as usize;
    for i in 0..num_tables {
        let rec = 12 + i * 16;
        if font.get(rec..rec + 4)? == tag {
            let offset = read_u32(font, rec + 8)? as usize;
            let length = read_u32(font, rec + 12)? as usize;
            if offset.checked_add(length)? <= font.len() {
                return Some((offset, length));
            }
        }
    }
    None
}

fn parse_loca(data: &[u8], num_glyphs: u16, long: bool) -> Option<Vec<u32>> {
    let count = num_glyphs as usize + 1;
    let mut out = Vec::with_capacity(count);
    if long {
        if data.len() < count * 4 {
            return None;
        }
        for i in 0..count {
            out.push(read_u32(data, i * 4)?);
        }
    } else {
        if data.len() < count * 2 {
            return None;
        }
        for i in 0..count {
            out.push(read_u16(data, i * 2)? as u32 * 2);
        }
    }
    Some(out)
}

fn glyph_range(offsets: &[u32], gid: u16) -> Option<(usize, usize)> {
    let start = *offsets.get(gid as usize)? as usize;
    let end = *offsets.get(gid as usize + 1)? as usize;
    Some((start, end))
}

/// Component glyph ids of a composite glyph (empty for simple glyphs).
fn composite_components(data: &[u8]) -> Vec<u16> {
    let mut out = Vec::new();
    let Some(n_contours) = read_u16(data, 0) else {
        return out;
    };
    if (n_contours as i16) >= 0 {
        return out; // simple glyph
    }
    let mut pos = 10usize;
    while let Some(flags) = read_u16(data, pos) {
        let Some(gid) = read_u16(data, pos + 2) else {
            break;
        };
        out.push(gid);
        pos += 4;
        pos += if flags & 0x0001 != 0 { 4 } else { 2 }; // args
        if flags & 0x0008 != 0 {
            pos += 2; // simple scale
        } else if flags & 0x0040 != 0 {
            pos += 4; // x&y scale
        } else if flags & 0x0080 != 0 {
            pos += 8; // 2x2 matrix
        }
        if flags & 0x0020 == 0 {
            break; // no MORE_COMPONENTS
        }
    }
    out
}

/// Reassemble the sfnt with some tables replaced. `force_long_loca` writes
/// `indexToLocFormat = 1` into the copied `head`.
fn rebuild_sfnt(
    font: &[u8],
    replacements: &[([u8; 4], Vec<u8>)],
    force_long_loca: bool,
) -> Option<Vec<u8>> {
    let num_tables = read_u16(font, 4)? as usize;

    // Collect (tag, data) for every table, applying replacements.
    let mut tables: Vec<([u8; 4], Vec<u8>)> = Vec::with_capacity(num_tables);
    for i in 0..num_tables {
        let rec = 12 + i * 16;
        let mut tag = [0u8; 4];
        tag.copy_from_slice(font.get(rec..rec + 4)?);
        let data = match replacements.iter().find(|(t, _)| *t == tag) {
            Some((_, d)) => d.clone(),
            None => {
                let offset = read_u32(font, rec + 8)? as usize;
                let length = read_u32(font, rec + 12)? as usize;
                let mut d = font.get(offset..offset + length)?.to_vec();
                if &tag == b"head" && force_long_loca && d.len() >= 52 {
                    d[50] = 0;
                    d[51] = 1; // indexToLocFormat = 1
                }
                d
            }
        };
        tables.push((tag, data));
    }

    // sfnt header + directory.
    let n = tables.len() as u16;
    let mut search_range = 1u16;
    let mut entry_selector = 0u16;
    while search_range * 2 <= n {
        search_range *= 2;
        entry_selector += 1;
    }
    search_range *= 16;
    let range_shift = n * 16 - search_range;

    let mut out = Vec::new();
    out.extend_from_slice(&font[0..4]); // sfnt version
    out.extend_from_slice(&n.to_be_bytes());
    out.extend_from_slice(&search_range.to_be_bytes());
    out.extend_from_slice(&entry_selector.to_be_bytes());
    out.extend_from_slice(&range_shift.to_be_bytes());

    let dir_len = 12 + tables.len() * 16;
    let mut offset = dir_len;
    let mut dir: Vec<u8> = Vec::with_capacity(tables.len() * 16);
    for (tag, data) in &tables {
        let padded = (data.len() + 3) & !3;
        dir.extend_from_slice(tag);
        dir.extend_from_slice(&table_checksum(data).to_be_bytes());
        dir.extend_from_slice(&(offset as u32).to_be_bytes());
        dir.extend_from_slice(&(data.len() as u32).to_be_bytes());
        offset += padded;
    }
    out.extend_from_slice(&dir);
    for (_, data) in &tables {
        out.extend_from_slice(data);
        while out.len() % 4 != 0 {
            out.push(0);
        }
    }
    Some(out)
}

fn table_checksum(data: &[u8]) -> u32 {
    let mut sum = 0u32;
    for chunk in data.chunks(4) {
        let mut word = [0u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        sum = sum.wrapping_add(u32::from_be_bytes(word));
    }
    sum
}

fn read_u16(data: &[u8], pos: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*data.get(pos)?, *data.get(pos + 1)?]))
}

fn read_u32(data: &[u8], pos: usize) -> Option<u32> {
    Some(u32::from_be_bytes([
        *data.get(pos)?,
        *data.get(pos + 1)?,
        *data.get(pos + 2)?,
        *data.get(pos + 3)?,
    ]))
}
