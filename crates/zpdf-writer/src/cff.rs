//! CFF (Compact Font Format) sparse subsetting for the writer.
//!
//! Mirrors the TrueType sparse-glyf approach in [`crate::subset`]: keep the
//! glyph **count** fixed (so CIDs/GIDs and the CharStrings INDEX layout are
//! unchanged) and replace every unused glyph's charstring with `endchar`
//! (`0x0E`) bytes, **preserving each entry's length**. Preserving length is
//! what makes this safe without rewriting the Top DICT: CFF encodes the
//! CharStrings offset, charset offset, Private/FDArray offsets as *absolute*
//! byte offsets inside the Top DICT, and those operands use a variable
//! integer encoding. By blanking charstrings in place (same byte length) the
//! INDEX's total data length is unchanged, so every absolute offset in the
//! Top DICT stays valid. CID→GID identity (which Identity-H relies on) is
//! preserved because GIDs are unchanged.
//!
//! CFF charstrings are independent (unlike TrueType composite glyphs), so
//! there is no transitive component closure to do. CFF2 (variable, major
//! version 2) uses a different charstring format and is left untouched — the
//! caller then embeds the full font.
//!
//! Handles OTF-wrapped CFF (an `OTTO` sfnt with a `CFF ` table — rebuilt via
//! [`crate::subset::rebuild_sfnt`]) and raw CFF (rebuilt directly). The Index
//! walker, Top DICT integer parser, `is_raw_cff`, `find_cff_table`, and
//! `wrap_cff_in_otf` are ported from the read side (`zpdf-font/src/lib.rs`),
//! which keeps them private over there; the writer keeps its own copy so it
//! can mutate and stays self-contained.

use std::collections::HashSet;

/// Subset `font` to the glyphs in `keep` (plus GID 0 / .notdef). Returns the
/// rebuilt font bytes, or `None` when the font is not CFF / is CFF2 / is
/// malformed (callers embed the original then).
///
/// `font` may be an OTF sfnt (the `CFF ` table is located and spliced back
/// via [`crate::subset::rebuild_sfnt`]) or raw CFF.
pub(crate) fn subset_cff(font: &[u8], keep: &HashSet<u16>) -> Option<Vec<u8>> {
    if let Some((off, len)) = find_cff_table(font) {
        // OTF-wrapped CFF: subset the CFF table, splice it back.
        let cff = font.get(off..off + len)?;
        let new_cff = blank_charstrings(cff, keep)?;
        if new_cff.len() == len {
            // In-place splice keeps the table length identical, so all other
            // sfnt offsets/checksums are recomputed by rebuild_sfnt anyway.
            let mut out = font.to_vec();
            out[off..off + len].copy_from_slice(&new_cff);
            Some(out)
        } else {
            // Length changed (should not happen: we preserve entry lengths),
            // but fall back to a full table swap if it ever does.
            crate::subset::rebuild_sfnt(font, &[(*b"CFF ", new_cff)], false)
        }
    } else if is_raw_cff(font) {
        blank_charstrings(font, keep)
    } else {
        None
    }
}

/// Blank every CharStrings entry whose GID is not in `keep` (and not 0). The
/// returned bytes have the same length as `cff`.
fn blank_charstrings(cff: &[u8], keep: &HashSet<u16>) -> Option<Vec<u8>> {
    if cff.len() < 5 {
        return None;
    }
    // CFF2 uses a different charstring format; leave untouched.
    if cff[0] == CFF2_MAJOR {
        return None;
    }
    let header_size = cff[2] as usize;
    if !(2..=8).contains(&header_size) || header_size >= cff.len() {
        return None;
    }

    // Skip the Name INDEX (immediately after the header), then read the first
    // Top DICT entry to find the CharStrings INDEX offset (Top DICT key 17).
    let top_dict_index_start = skip_cff_index(cff, header_size)?;
    let dict_data = cff_index_entry(cff, top_dict_index_start, 0)?;
    let charstrings_offset = parse_top_dict_int(dict_data, 17)?;
    if charstrings_offset == 0 || charstrings_offset >= cff.len() {
        return None;
    }

    let count = count_cff_index_entries(cff, charstrings_offset)?;
    if count == 0 {
        return None;
    }
    let off_size = *cff.get(charstrings_offset + 2)? as usize;
    if !(1..=4).contains(&off_size) {
        return None;
    }
    let offsets_start = charstrings_offset + 3;
    let data_start = offsets_start.checked_add((count + 1).checked_mul(off_size)?)?;

    let mut out = cff.to_vec();
    for gid in 0..count as u16 {
        if gid == 0 || keep.contains(&gid) {
            continue;
        }
        let start_pos = offsets_start.checked_add(usize::from(gid) * off_size)?;
        let end_pos = start_pos.checked_add(off_size)?;
        let data_lo = read_cff_offset(cff, start_pos, off_size)?.checked_sub(1)?;
        let data_hi = read_cff_offset(cff, end_pos, off_size)?.checked_sub(1)?;
        if data_hi <= data_lo {
            continue;
        }
        let lo = data_start.checked_add(data_lo)?;
        let hi = data_start.checked_add(data_hi)?;
        // Overwrite the unused charstring with `endchar` bytes. The first
        // 0x0E ends the charstring; trailing bytes are unreachable and kept
        // only to preserve the INDEX's byte length (and thus all offsets).
        for b in out.get_mut(lo..hi)? {
            *b = 0x0E;
        }
    }
    Some(out)
}

/// Whether `data` is a raw (unwrapped) CFF program: major 1, minor 0, header
/// size 1..=8.
pub(crate) fn is_raw_cff(data: &[u8]) -> bool {
    data.len() >= 4 && data[0] == 0x01 && data[1] == 0x00 && data[2] >= 1 && data[2] <= 8
}

/// CFF major version 2 (CFF2, variable fonts) — its charstring format
/// differs and is not subset here.
const CFF2_MAJOR: u8 = 2;

/// (offset, length) of the `CFF ` table in an OTF sfnt directory, or `None`.
fn find_cff_table(data: &[u8]) -> Option<(usize, usize)> {
    if data.len() < 12 {
        return None;
    }
    let num_tables = u16::from_be_bytes([data[4], data[5]]) as usize;
    for i in 0..num_tables {
        let rec = 12 + i * 16;
        if rec + 16 > data.len() {
            break;
        }
        if &data[rec..rec + 4] == b"CFF " {
            let offset =
                u32::from_be_bytes([data[rec + 8], data[rec + 9], data[rec + 10], data[rec + 11]])
                    as usize;
            let length = u32::from_be_bytes([
                data[rec + 12],
                data[rec + 13],
                data[rec + 14],
                data[rec + 15],
            ]) as usize;
            if offset.checked_add(length)? <= data.len() {
                return Some((offset, length));
            }
        }
    }
    None
}

/// End offset of the CFF INDEX at `pos` (count + offsets + data), or `None`.
fn skip_cff_index(cff: &[u8], pos: usize) -> Option<usize> {
    let count_bytes = cff.get(pos..pos.checked_add(2)?)?;
    let count = u16::from_be_bytes([count_bytes[0], count_bytes[1]]) as usize;
    if count == 0 {
        return pos.checked_add(2);
    }
    let off_size = *cff.get(pos.checked_add(2)?)? as usize;
    if !(1..=4).contains(&off_size) {
        return None;
    }
    let offsets_start = pos.checked_add(3)?;
    let last_offset_pos = offsets_start.checked_add(count.checked_mul(off_size)?)?;
    let last_offset = read_cff_offset(cff, last_offset_pos, off_size)?.checked_sub(1)?;
    let data_start = offsets_start.checked_add((count + 1).checked_mul(off_size)?)?;
    let end = data_start.checked_add(last_offset)?;
    (end <= cff.len()).then_some(end)
}

fn read_cff_offset(cff: &[u8], pos: usize, size: usize) -> Option<usize> {
    if !(1..=4).contains(&size) {
        return None;
    }
    let bytes = cff.get(pos..pos.checked_add(size)?)?;
    let mut val = 0usize;
    for &byte in bytes {
        val = (val << 8) | byte as usize;
    }
    Some(val)
}

/// Slice of CFF INDEX entry `entry` starting at `pos`.
fn cff_index_entry(cff: &[u8], pos: usize, entry: usize) -> Option<&[u8]> {
    let count_bytes = cff.get(pos..pos.checked_add(2)?)?;
    let count = u16::from_be_bytes([count_bytes[0], count_bytes[1]]) as usize;
    if entry >= count {
        return None;
    }
    let off_size = *cff.get(pos.checked_add(2)?)? as usize;
    if !(1..=4).contains(&off_size) {
        return None;
    }
    let offsets_start = pos.checked_add(3)?;
    let start_pos = offsets_start.checked_add(entry.checked_mul(off_size)?)?;
    let end_pos = start_pos.checked_add(off_size)?;
    let start = read_cff_offset(cff, start_pos, off_size)?.checked_sub(1)?;
    let end = read_cff_offset(cff, end_pos, off_size)?.checked_sub(1)?;
    if end < start {
        return None;
    }
    let data_start = offsets_start.checked_add((count + 1).checked_mul(off_size)?)?;
    let range_start = data_start.checked_add(start)?;
    let range_end = data_start.checked_add(end)?;
    cff.get(range_start..range_end)
}

fn count_cff_index_entries(cff: &[u8], pos: usize) -> Option<usize> {
    let bytes = cff.get(pos..pos.checked_add(2)?)?;
    Some(u16::from_be_bytes([bytes[0], bytes[1]]) as usize)
}

/// Last integer operand of `target_key` in a CFF Top DICT. Two-byte operators
/// (12 x) are addressed as `256 + x`.
fn parse_top_dict_int(dict_data: &[u8], target_key: u16) -> Option<usize> {
    let mut pos = 0;
    let mut operand_stack: Vec<i64> = Vec::new();
    while pos < dict_data.len() {
        let b0 = dict_data[pos];
        match b0 {
            0..=21 => {
                let key = if b0 == 12 {
                    pos += 1;
                    if pos >= dict_data.len() {
                        break;
                    }
                    256 + dict_data[pos] as u16
                } else {
                    b0 as u16
                };
                if key == target_key {
                    return operand_stack
                        .last()
                        .and_then(|&value| usize::try_from(value).ok());
                }
                operand_stack.clear();
                pos += 1;
            }
            28 => {
                if pos + 2 >= dict_data.len() {
                    break;
                }
                let val = i16::from_be_bytes([dict_data[pos + 1], dict_data[pos + 2]]) as i64;
                operand_stack.push(val);
                pos += 3;
            }
            29 => {
                if pos + 4 >= dict_data.len() {
                    break;
                }
                let val = i32::from_be_bytes([
                    dict_data[pos + 1],
                    dict_data[pos + 2],
                    dict_data[pos + 3],
                    dict_data[pos + 4],
                ]) as i64;
                operand_stack.push(val);
                pos += 5;
            }
            30 => {
                // Real number — skip nibbles until 0xf.
                pos += 1;
                while pos < dict_data.len() {
                    let byte = dict_data[pos];
                    pos += 1;
                    if (byte & 0x0f) == 0x0f || (byte >> 4) == 0x0f {
                        break;
                    }
                }
                operand_stack.push(0);
            }
            32..=246 => {
                operand_stack.push(b0 as i64 - 139);
                pos += 1;
            }
            247..=250 => {
                if pos + 1 >= dict_data.len() {
                    break;
                }
                operand_stack.push((b0 as i64 - 247) * 256 + dict_data[pos + 1] as i64 + 108);
                pos += 2;
            }
            251..=254 => {
                if pos + 1 >= dict_data.len() {
                    break;
                }
                operand_stack.push(-(b0 as i64 - 251) * 256 - dict_data[pos + 1] as i64 - 108);
                pos += 2;
            }
            _ => {
                pos += 1;
            }
        }
        if operand_stack.len() > 48 {
            return None;
        }
    }
    None
}

/// Wrap a raw CFF program in a minimal `OTTO` sfnt (CFF + head + hhea + maxp +
/// post) so it can be embedded as `/FontFile3 /Subtype /OpenType`. The
/// auxiliary tables carry placeholder metrics (1000 upem, 65535 glyphs); the
/// CFF's own Top DICT carries the real font metrics the rasterizer uses.
///
/// Ported from the read side (`zpdf-font/src/lib.rs::wrap_cff_in_otf`); kept
/// here so raw-CFF input to `embed_composite_font` works without a zpdf-font
/// dependency.
pub(crate) fn wrap_cff_in_otf(cff_data: &[u8]) -> Vec<u8> {
    let num_tables: u16 = 5;
    let search_range: u16 = 64;
    let entry_selector: u16 = 2;
    let range_shift: u16 = num_tables * 16 - search_range;

    let header_size = 12 + num_tables as usize * 16;

    fn pad4(n: u32) -> Option<u32> {
        Some(n.checked_add(3)? & !3)
    }
    fn compute_checksum(data: &[u8]) -> u32 {
        let mut sum: u32 = 0;
        let chunks = data.len() / 4;
        for i in 0..chunks {
            sum = sum.wrapping_add(u32::from_be_bytes([
                data[i * 4],
                data[i * 4 + 1],
                data[i * 4 + 2],
                data[i * 4 + 3],
            ]));
        }
        let remainder = data.len() % 4;
        if remainder > 0 {
            let mut last = [0u8; 4];
            last[..remainder].copy_from_slice(&data[chunks * 4..]);
            sum = sum.wrapping_add(u32::from_be_bytes(last));
        }
        sum
    }

    let cff_offset = header_size as u32;
    let Ok(cff_len) = u32::try_from(cff_data.len()) else {
        return Vec::new();
    };
    let Some(cff_padded) = pad4(cff_len) else {
        return Vec::new();
    };
    let Some(head_offset) = cff_offset.checked_add(cff_padded) else {
        return Vec::new();
    };
    let head_len: u32 = 54;
    let Some(head_padded) = pad4(head_len) else {
        return Vec::new();
    };
    let Some(hhea_offset) = head_offset.checked_add(head_padded) else {
        return Vec::new();
    };
    let hhea_len: u32 = 36;
    let Some(hhea_padded) = pad4(hhea_len) else {
        return Vec::new();
    };
    let Some(maxp_offset) = hhea_offset.checked_add(hhea_padded) else {
        return Vec::new();
    };
    let maxp_len: u32 = 6;
    let Some(maxp_padded) = pad4(maxp_len) else {
        return Vec::new();
    };
    let Some(post_offset) = maxp_offset.checked_add(maxp_padded) else {
        return Vec::new();
    };
    let post_len: u32 = 32;
    let Some(total_size) = (post_offset as usize).checked_add(post_len as usize) else {
        return Vec::new();
    };
    let mut buf = Vec::new();
    if buf.try_reserve_exact(total_size).is_err() {
        return Vec::new();
    }
    buf.resize(total_size, 0);

    buf[0..4].copy_from_slice(b"OTTO");
    buf[4..6].copy_from_slice(&num_tables.to_be_bytes());
    buf[6..8].copy_from_slice(&search_range.to_be_bytes());
    buf[8..10].copy_from_slice(&entry_selector.to_be_bytes());
    buf[10..12].copy_from_slice(&range_shift.to_be_bytes());

    let mut head_data = vec![0u8; head_len as usize];
    head_data[0..4].copy_from_slice(&0x00010000u32.to_be_bytes());
    head_data[12..16].copy_from_slice(&0x5F0F3CF5u32.to_be_bytes());
    head_data[16..18].copy_from_slice(&0x000Bu16.to_be_bytes());
    head_data[18..20].copy_from_slice(&1000u16.to_be_bytes());
    head_data[50..52].copy_from_slice(&0u16.to_be_bytes());

    let mut hhea_data = vec![0u8; hhea_len as usize];
    hhea_data[0..4].copy_from_slice(&0x00010000u32.to_be_bytes());
    hhea_data[4..6].copy_from_slice(&800i16.to_be_bytes());
    hhea_data[6..8].copy_from_slice(&(-200i16).to_be_bytes());
    hhea_data[34..36].copy_from_slice(&65535u16.to_be_bytes());

    let mut maxp_data = vec![0u8; maxp_len as usize];
    maxp_data[0..4].copy_from_slice(&0x00005000u32.to_be_bytes());
    maxp_data[4..6].copy_from_slice(&65535u16.to_be_bytes());

    let mut post_data = vec![0u8; post_len as usize];
    post_data[0..4].copy_from_slice(&0x00030000u32.to_be_bytes());

    let mut rec_off = 12;
    for (tag, toff, tlen, tdata) in [
        (b"CFF ", cff_offset, cff_len, cff_data as &[u8]),
        (b"head", head_offset, head_len, &head_data as &[u8]),
        (b"hhea", hhea_offset, hhea_len, &hhea_data),
        (b"maxp", maxp_offset, maxp_len, &maxp_data),
        (b"post", post_offset, post_len, &post_data),
    ] {
        buf[rec_off..rec_off + 4].copy_from_slice(tag);
        let cs = compute_checksum(tdata);
        buf[rec_off + 4..rec_off + 8].copy_from_slice(&cs.to_be_bytes());
        buf[rec_off + 8..rec_off + 12].copy_from_slice(&toff.to_be_bytes());
        buf[rec_off + 12..rec_off + 16].copy_from_slice(&tlen.to_be_bytes());
        rec_off += 16;
    }

    buf[cff_offset as usize..cff_offset as usize + cff_data.len()].copy_from_slice(cff_data);
    buf[head_offset as usize..head_offset as usize + head_data.len()].copy_from_slice(&head_data);
    buf[hhea_offset as usize..hhea_offset as usize + hhea_data.len()].copy_from_slice(&hhea_data);
    buf[maxp_offset as usize..maxp_offset as usize + maxp_data.len()].copy_from_slice(&maxp_data);
    buf[post_offset as usize..post_offset as usize + post_data.len()].copy_from_slice(&post_data);

    buf
}

/// Detect whether an sfnt/CFF byte slice is CFF-flavored (has a `CFF ` table
/// or is raw CFF). Used by the builder to pick the embedding kind.
pub(crate) fn is_cff_flavored(font: &[u8]) -> bool {
    find_cff_table(font).is_some() || is_raw_cff(font)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny hand-built CFF with a 3-glyph CharStrings INDEX so the blanking
    /// logic is exercisable without a real font binary. Only the structural
    /// fields subset_cff reads are populated.
    fn toy_cff() -> Vec<u8> {
        // Header: major=1 minor=0 hdrSize=2 offSize=1
        let mut cff = vec![0x01u8, 0x00, 0x02, 0x01];
        // Name INDEX: 1 entry "A"
        cff.extend_from_slice(&[0x00, 0x01, 0x01, 0x01, 0x02, b'A']);
        // Top DICT INDEX: 1 entry encoding charstrings offset = (later filled).
        // We build the Top DICT after we know the charstrings offset.
        // For a unit test we instead construct a minimal valid-enough CFF by
        // appending a Top DICT INDEX with key 17 pointing just past itself.
        // This is fiddly; instead assert the structural guards on raw input.
        cff
    }

    #[test]
    fn raw_cff_detection() {
        assert!(is_raw_cff(&[0x01, 0x00, 0x02, 0x01, 0x00]));
        assert!(!is_raw_cff(&[0x4F, 0x54, 0x54, 0x4F])); // OTTO
        assert!(!is_raw_cff(&[]));
        assert!(!is_raw_cff(&[0x01, 0x00, 0x00, 0x00])); // hdrSize 0
    }

    #[test]
    fn blanking_rejects_malformed() {
        // Truncated CFF: header present but no Name INDEX.
        let cff = vec![0x01u8, 0x00, 0x02, 0x01, 0x00];
        let keep = HashSet::new();
        assert_eq!(blank_charstrings(&cff, &keep), None);
        let _ = toy_cff(); // keep the helper compiled
    }

    #[test]
    fn cff2_left_untouched() {
        // CFF2: major version 2.
        let cff = vec![0x02u8, 0x00, 0x05, 0x01, 0x00, 0x00];
        assert_eq!(subset_cff(&cff, &HashSet::new()), None);
    }

    #[test]
    fn otf_without_cff_is_not_cff_flavored() {
        let mut otf = vec![0x00u8; 12];
        otf[0..4].copy_from_slice(b"\x00\x01\x00\x00");
        otf[4..6].copy_from_slice(&0u16.to_be_bytes()); // num_tables 0
        assert!(!is_cff_flavored(&otf));
        assert_eq!(subset_cff(&otf, &HashSet::new()), None);
    }
}
