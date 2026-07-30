//! Linearization: the cross-reference tables must cover every object.
//!
//! Why this file exists: the in-module test asserts that the output parses
//! and declares `/Linearized`, and it passed while the output was badly
//! malformed — because `zpdf-parser` repairs a broken cross-reference table
//! by scanning for objects, so zpdf reading its own output papers over
//! exactly the defect under test. Any check that routes through our own
//! parser is therefore blind here; these assertions read the raw bytes.
//!
//! The defect this pins: the main table emitted `xref 0 3` (object 0 free,
//! then the linearization dict and hint stream — objects the *first-page*
//! table already covers) and `/Size 3`, so every object after the first-page
//! section existed in the body and in no table at all. On a four-page file
//! objects 11..25 were unreachable: `qpdf --check-linearization` reported
//! `/N does not match number of pages`, MuPDF reported `object out of range
//! (11 0 R); xref size 11`, and three of the four pages dangled.

use std::collections::HashSet;

use zpdf_writer::linearize_pdf;

/// A document with enough objects that the first-page section cannot cover
/// them all — the whole point is to have a "rest" section.
fn multipage_source() -> Vec<u8> {
    let mut b = zpdf_writer::builder::DocumentBuilder::new();
    for i in 0..4 {
        let p = b.add_page(612.0, 792.0);
        b.add_text(
            p,
            &format!("page {}", i + 1),
            50.0,
            700.0,
            "Helvetica",
            12.0,
            (0.0, 0.0, 0.0),
        )
        .unwrap();
    }
    b.build().unwrap()
}

/// Object numbers that appear as `N 0 obj` in the body.
fn body_objects(bytes: &[u8]) -> HashSet<u32> {
    let text = String::from_utf8_lossy(bytes);
    let mut out = HashSet::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        if let (Some(n), Some("0"), Some("obj")) = (it.next(), it.next(), it.next()) {
            if let Ok(num) = n.parse::<u32>() {
                out.insert(num);
            }
        }
    }
    out
}

/// Object numbers covered by any `xref` subsection header in the file.
fn xref_covered(bytes: &[u8]) -> HashSet<u32> {
    let text = String::from_utf8_lossy(bytes);
    let mut out = HashSet::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        if line.trim() != "xref" {
            continue;
        }
        // Subsection headers are `start count`; entries are 20 bytes each.
        while let Some(peek) = lines.peek() {
            let parts: Vec<&str> = peek.split_whitespace().collect();
            if parts.len() == 2 {
                if let (Ok(start), Ok(count)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                    lines.next();
                    for k in 0..count {
                        out.insert(start + k);
                        lines.next(); // the entry line
                    }
                    continue;
                }
            }
            break;
        }
    }
    out
}

fn trailer_size(bytes: &[u8]) -> Option<u32> {
    let text = String::from_utf8_lossy(bytes);
    let i = text.rfind("/Size")?;
    text[i + 5..]
        .split_whitespace()
        .next()?
        .trim_end_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .ok()
}

#[test]
fn every_body_object_is_covered_by_a_cross_reference_table() {
    let out = linearize_pdf(&zpdf_parser::PdfFile::parse(multipage_source()).unwrap()).unwrap();
    let body = body_objects(&out);
    let covered = xref_covered(&out);
    assert!(!body.is_empty(), "no objects were written at all");
    let missing: Vec<u32> = {
        let mut v: Vec<u32> = body.difference(&covered).copied().collect();
        v.sort_unstable();
        v
    };
    assert!(
        missing.is_empty(),
        "objects exist in the body but in no xref table: {missing:?} \
         (body={} objects, covered={} entries)",
        body.len(),
        covered.len(),
    );
}

#[test]
fn trailer_size_matches_the_highest_object_number() {
    let out = linearize_pdf(&zpdf_parser::PdfFile::parse(multipage_source()).unwrap()).unwrap();
    let highest = *body_objects(&out).iter().max().unwrap();
    let size = trailer_size(&out).expect("no /Size in any trailer");
    assert_eq!(
        size,
        highest + 1,
        "/Size must be one more than the highest object number \
         (qpdf: \"reported number of objects is not one plus the highest object number\")"
    );
}

#[test]
fn the_main_trailer_names_the_catalog() {
    // A reader that starts from the main table rather than following /Prev
    // from the first-page one has no other way to find /Root.
    let out = linearize_pdf(&zpdf_parser::PdfFile::parse(multipage_source()).unwrap()).unwrap();
    let text = String::from_utf8_lossy(&out);
    let main_trailer = text.rfind("trailer").expect("no trailer");
    assert!(
        text[main_trailer..].contains("/Root"),
        "the last trailer must carry /Root: {}",
        &text[main_trailer..(main_trailer + 80).min(text.len())]
    );
}
