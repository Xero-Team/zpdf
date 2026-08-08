//! `/ToUnicode` CMap serialization for the writer.
//!
//! The read side parses ToUnicode CMaps (`zpdf_font::cmap::ToUnicodeMap`); this
//! is the inverse — build the CMap stream bytes that map a font's 2-byte codes
//! (GIDs, under Identity-H) back to UTF-16BE Unicode so viewers can extract
//! text. Output is a conformant CMap with `begincodespacerange` /
//! `beginbfrange` / `beginbfchar`, packed 100 entries per block (the CMap
//! spec's per-block ceiling) and with run-encoded ranges where GID and
//! Unicode advance together (the common CJK case, where consecutive CJK code
//! points map to consecutive GIDs in a cmap subtable).

/// Build a `/ToUnicode` CMap stream body from `gid → Unicode scalar` pairs.
///
/// `mappings` is `(code, char)` where `code` is the 2-byte value emitted in
/// the content stream (the GID under Identity-H). Surrogate pairs are emitted
/// for scalars outside the BMP so the result is correct for any plane.
pub(crate) fn build_tounicode_cmap(mappings: &[(u16, char)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(
        b"/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
         /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
         /CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n",
    );
    out.extend_from_slice(b"1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n");

    // Split into runs (consecutive code, consecutive Unicode) and singletons.
    let mut sorted: Vec<(u16, char)> = mappings.to_vec();
    sorted.sort_unstable_by_key(|&(c, _)| c);
    sorted.dedup_by_key(|(c, _)| *c);

    let mut ranges: Vec<(u16, u16, char)> = Vec::new(); // (lo, hi, start_char)
    let mut singles: Vec<(u16, char)> = Vec::new();
    let mut i = 0;
    while i < sorted.len() {
        let (lo, ch) = sorted[i];
        let start_cp = ch as u32;
        let mut hi = lo;
        let mut j = i + 1;
        while j < sorted.len() {
            let (cj, chj) = sorted[j];
            if cj == hi.saturating_add(1) && (chj as u32) == start_cp + (cj - lo) as u32 {
                hi = cj;
                j += 1;
            } else {
                break;
            }
        }
        if hi > lo {
            ranges.push((lo, hi, ch));
        } else {
            singles.push((lo, ch));
        }
        i = j;
    }

    // Emit ranges in blocks of 100. A bfrange `<lo> <hi> <start>` maps lo+k →
    // start+k (the parser increments the destination); we only grouped runs
    // whose Unicode advances in lockstep with the code, so the single start
    // value suffices.
    for chunk in ranges.chunks(100) {
        out.extend_from_slice(format!("{} beginbfrange\n", chunk.len()).as_bytes());
        for &(lo, hi, ch) in chunk {
            out.extend_from_slice(format!("<{lo:04X}> <{hi:04X}> <").as_bytes());
            write_utf16_hex(&mut out, ch as u32);
            out.extend_from_slice(b">\n");
        }
        out.extend_from_slice(b"endbfrange\n");
    }

    // Emit singletons in blocks of 100.
    for chunk in singles.chunks(100) {
        out.extend_from_slice(format!("{} beginbfchar\n", chunk.len()).as_bytes());
        for &(code, ch) in chunk {
            out.extend_from_slice(format!("<{code:04X}> <").as_bytes());
            write_utf16_hex(&mut out, ch as u32);
            out.extend_from_slice(b">\n");
        }
        out.extend_from_slice(b"endbfchar\n");
    }

    out.extend_from_slice(b"endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n");
    out
}

/// Write `code_point` as UTF-16BE **hex digits** into `out` (4 hex digits for
/// BMP, 8 for a supplementary-plane surrogate pair). CMap `<bfchar>`/`<bfrange>`
/// values are hex strings, so this emits ASCII hex, not the raw bytes.
fn write_utf16_hex(out: &mut Vec<u8>, code_point: u32) {
    let mut buf = [0u16; 2];
    let s: &[u16] = match char::from_u32(code_point) {
        Some(c) => c.encode_utf16(&mut buf),
        None => &buf[..0],
    };
    for &u in s {
        out.extend_from_slice(format!("{u:04X}").as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bmp_singleton_round_trips_through_parser() {
        let cmap = build_tounicode_cmap(&[(65, 'A')]);
        let parsed = zpdf_font::cmap::ToUnicodeMap::parse(&cmap);
        assert_eq!(parsed.lookup(65), Some("A"));
    }

    #[test]
    fn cjk_range_packs_into_bfrange() {
        // Consecutive GIDs 100..103 mapping to consecutive CJK code points.
        let mappings = vec![
            (100, '\u{4E00}'),
            (101, '\u{4E01}'),
            (102, '\u{4E02}'),
            (103, '\u{4E03}'),
        ];
        let cmap = build_tounicode_cmap(&mappings);
        let body = String::from_utf8_lossy(&cmap);
        assert!(body.contains("beginbfrange"), "body: {body}");
        assert!(
            !body.contains("beginbfchar"),
            "expected no singletons: {body}"
        );
        let parsed = zpdf_font::cmap::ToUnicodeMap::parse(&cmap);
        for (gid, ch) in &mappings {
            assert_eq!(parsed.lookup(*gid as u32), Some(ch.to_string().as_str()));
        }
    }

    #[test]
    fn supplementary_plane_emits_surrogate_pair() {
        // U+1F600 (emoji) — outside the BMP, must serialize as a surrogate pair.
        let cmap = build_tounicode_cmap(&[(1, '\u{1F600}')]);
        let body = String::from_utf8_lossy(&cmap);
        assert!(body.contains("<D83DDE00>"), "body: {body}");
        let parsed = zpdf_font::cmap::ToUnicodeMap::parse(&cmap);
        assert_eq!(parsed.lookup(1), Some("\u{1F600}"));
    }
}
