//! Regression tests for font_loader against hand-built in-memory PDFs:
//! /CIDToGIDMap stream support (with CIDFontType2 vs raw-CFF precedence) and
//! Type3 fonts whose /CharProcs //Encoding //Widths //FontMatrix are indirect.

use zpdf_core::ObjectId;
use zpdf_document::font_loader::load_single_font;
use zpdf_parser::PdfFile;

/// Assemble a complete PDF 1.7 from raw object bodies. `objects[i]` becomes
/// object `i + 1`; offsets and the xref table are computed, and object 1 is
/// expected to be the catalog (`/Root 1 0 R`).
fn build_pdf(objects: &[Vec<u8>]) -> Vec<u8> {
    let mut p = Vec::from(&b"%PDF-1.7\n"[..]);
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(p.len());
        p.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        p.extend_from_slice(body);
        p.extend_from_slice(b"\nendobj\n");
    }
    let xref_pos = p.len();
    p.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
    p.extend_from_slice(b"0000000000 65535 f \n");
    for o in &offsets {
        p.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    p.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_pos}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    p
}

fn dict_obj(s: &str) -> Vec<u8> {
    s.as_bytes().to_vec()
}

fn stream_obj(data: &[u8]) -> Vec<u8> {
    let mut v = format!("<< /Length {} >>\nstream\n", data.len()).into_bytes();
    v.extend_from_slice(data);
    v.extend_from_slice(b"\nendstream");
    v
}

/// Minimal CID-keyed CFF: Top DICT with /ROS, format-0 charset mapping
/// GID 1 → CID 5 and GID 2 → CID 9, 3-entry empty CharStrings INDEX.
/// (Mirrors the fixture in zpdf-font's unit tests.)
fn cid_keyed_cff() -> Vec<u8> {
    let dict_len: usize = 21;
    let charset_off = 19 + dict_len;
    let charstrings_off = charset_off + 5;

    let mut cff = vec![1, 0, 4, 4]; // header
    cff.extend_from_slice(&[0x00, 0x01, 0x01, 0x01, 0x02, b'T']); // Name INDEX
    cff.extend_from_slice(&[0x00, 0x01, 0x01, 0x01, dict_len as u8 + 1]); // Top DICT INDEX
    cff.extend_from_slice(&[28, 0x01, 0x87]); // SID 391
    cff.extend_from_slice(&[28, 0x01, 0x88]); // SID 392
    cff.push(139); // supplement 0
    cff.extend_from_slice(&[12, 30]); // ROS
    cff.push(29);
    cff.extend_from_slice(&(charset_off as i32).to_be_bytes());
    cff.push(15); // charset
    cff.push(29);
    cff.extend_from_slice(&(charstrings_off as i32).to_be_bytes());
    cff.push(17); // CharStrings
    cff.extend_from_slice(&[0x00, 0x00]); // String INDEX (empty)
    cff.extend_from_slice(&[0x00, 0x00]); // Global Subr INDEX (empty)
    cff.extend_from_slice(&[0, 0x00, 0x05, 0x00, 0x09]); // charset format 0
    cff.extend_from_slice(&[0x00, 0x03, 0x01, 0x01, 0x01, 0x01, 0x01]); // CharStrings
    cff
}

/// Type0/CIDFontType2 font whose /CIDToGIDMap is the given object body.
fn type0_pdf(cid_to_gid_map: &str, font_file_key: &str, font_data: &[u8], map_obj: Vec<u8>) -> Vec<u8> {
    build_pdf(&[
        dict_obj("<< /Type /Catalog >>"),
        dict_obj(
            "<< /Type /Font /Subtype /Type0 /BaseFont /TestCID /Encoding /Identity-H \
             /DescendantFonts [3 0 R] >>",
        ),
        dict_obj(&format!(
            "<< /Type /Font /Subtype /{} /BaseFont /TestCID /FontDescriptor 4 0 R \
             /CIDToGIDMap {cid_to_gid_map} /DW 1000 >>",
            if font_file_key == "FontFile3" {
                "CIDFontType0"
            } else {
                "CIDFontType2"
            }
        )),
        dict_obj(&format!(
            "<< /Type /FontDescriptor /FontName /TestCID /Flags 4 /{font_file_key} 6 0 R >>"
        )),
        map_obj,
        stream_obj(font_data),
    ])
}

#[test]
fn cid_to_gid_map_stream_applied_for_cid_font_type2() {
    // CID 0 → GID 1, CID 1 → GID 5, CID 2 → GID 0 (omitted), CID 3 → GID 7.
    let map_data = [0u8, 1, 0, 5, 0, 0, 0, 7];
    // Unparseable font data: the map must attach even without outlines.
    let pdf = type0_pdf("5 0 R", "FontFile2", b"JUNK", stream_obj(&map_data));

    let file = PdfFile::parse(pdf).expect("parse synthetic pdf");
    let font = load_single_font(&file, ObjectId(2, 0)).expect("load font");

    let map = font.cid_to_gid.expect("CIDToGIDMap stream applied");
    assert_eq!(map.get(&0), Some(&1));
    assert_eq!(map.get(&1), Some(&5));
    assert_eq!(map.get(&2), None, "GID 0 entries are omitted");
    assert_eq!(map.get(&3), Some(&7));
}

#[test]
fn cid_to_gid_map_identity_keeps_identity() {
    let pdf = type0_pdf(
        "/Identity",
        "FontFile2",
        b"JUNK",
        dict_obj("<< /Unused true >>"), // object 5 is a placeholder
    );

    let file = PdfFile::parse(pdf).expect("parse synthetic pdf");
    let font = load_single_font(&file, ObjectId(2, 0)).expect("load font");
    assert!(font.cid_to_gid.is_none());
}

#[test]
fn cid_to_gid_stream_does_not_clobber_cff_charset_map() {
    // Raw-CFF CIDFontType0 descendant: the charset-derived map (CID 5 → GID 1)
    // must win over a (spec-illegal) /CIDToGIDMap stream claiming CID 5 → 42.
    let mut map_data = vec![0u8; 12];
    map_data[10] = 0;
    map_data[11] = 42; // CID 5 → GID 42
    let pdf = type0_pdf("5 0 R", "FontFile3", &cid_keyed_cff(), stream_obj(&map_data));

    let file = PdfFile::parse(pdf).expect("parse synthetic pdf");
    let font = load_single_font(&file, ObjectId(2, 0)).expect("load font");

    let map = font.cid_to_gid.expect("charset-derived map");
    assert_eq!(map.get(&5), Some(&1), "charset map kept, stream ignored");
    assert_eq!(map.get(&9), Some(&2));
}

#[test]
fn type3_indirect_refs_resolve() {
    // /FontMatrix, /CharProcs, /Encoding and /Widths are all indirect — a
    // direct-only read would silently drop every glyph.
    let glyph_a = b"0 0 100 100 re f";
    let glyph_b = b"0 0 50 50 re f";
    let pdf = build_pdf(&[
        dict_obj("<< /Type /Catalog >>"),
        dict_obj(
            "<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
             /FontMatrix 3 0 R /CharProcs 4 0 R /Encoding 5 0 R \
             /FirstChar 65 /LastChar 66 /Widths 6 0 R >>",
        ),
        dict_obj("[0.01 0 0 0.01 0 0]"),
        dict_obj("<< /glyphA 7 0 R /glyphB 8 0 R >>"),
        dict_obj("<< /Type /Encoding /Differences [65 /glyphA /glyphB] >>"),
        dict_obj("[500 600]"),
        stream_obj(glyph_a),
        stream_obj(glyph_b),
    ]);

    let file = PdfFile::parse(pdf).expect("parse synthetic pdf");
    let font = load_single_font(&file, ObjectId(2, 0)).expect("load font");
    assert!(font.is_type3());

    let (stream, matrix) = font.type3_glyph_stream(65).expect("glyph A proc");
    assert_eq!(stream, glyph_a);
    assert_eq!(matrix, [0.01, 0.0, 0.0, 0.01, 0.0, 0.0]);

    let (stream, _) = font.type3_glyph_stream(66).expect("glyph B proc");
    assert_eq!(stream, glyph_b);

    assert_eq!(font.type3_glyph_width(65), 500.0);
    assert_eq!(font.type3_glyph_width(66), 600.0);
}
