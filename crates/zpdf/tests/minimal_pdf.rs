//! Integration tests over the hand-built minimal PDF fixture, plus a gated
//! smoke test over the local corpus. These exercise the full open path,
//! including object-resolution caching and the xref-recovery fallback.

mod common;

use common::minimal_pdf;
use zpdf::{ObjectId, PdfDocument};

#[test]
fn opens_minimal_pdf() {
    let doc = PdfDocument::open(minimal_pdf()).expect("minimal pdf should open");
    assert_eq!(doc.version(), (1, 7));
    assert_eq!(doc.page_count(), 1);

    let page = doc.page(0).expect("page 0");
    assert!(
        (page.width() - 612.0).abs() < 1e-6,
        "width {}",
        page.width()
    );
    assert!(
        (page.height() - 792.0).abs() < 1e-6,
        "height {}",
        page.height()
    );
}

#[test]
fn resolve_is_consistent_across_calls() {
    // Resolving the same object twice must yield equal results — a correctness
    // guard for the new object_cache in PdfFile.
    let doc = PdfDocument::open(minimal_pdf()).unwrap();
    let id = ObjectId(1, 0);
    let a = doc.file().resolve(id).expect("first resolve");
    let b = doc.file().resolve(id).expect("second resolve (cached)");
    assert_eq!(a, b);
    assert_eq!(a.as_dict().unwrap().get_name("Type").unwrap(), "Catalog");
}

#[test]
fn recovers_from_corrupt_startxref() {
    // Corrupt the `startxref` offset; the tail-scan recovery path must still open
    // the document and find the catalog.
    let mut bytes = minimal_pdf();
    let needle = b"startxref\n305";
    let pos = bytes
        .windows(needle.len())
        .position(|w| w == needle)
        .expect("startxref present");
    // Overwrite "305" with "999" (same length, deliberately wrong).
    let num_at = pos + b"startxref\n".len();
    bytes[num_at..num_at + 3].copy_from_slice(b"999");

    let doc = PdfDocument::open(bytes).expect("recovery should open corrupt-xref pdf");
    assert_eq!(doc.page_count(), 1);
    let root = doc.file().trailer.get_ref("Root").expect("recovered /Root");
    let cat = doc.file().resolve(root).unwrap();
    assert_eq!(cat.as_dict().unwrap().get_name("Type").unwrap(), "Catalog");
}

/// Smoke test over the local corpus. The real PDFs live under the gitignored
/// `tests/` dir and are absent in CI, so each path is existence-gated: present
/// files must open with at least one page; missing files are skipped.
#[test]
fn real_corpus_smoke() {
    let candidates = [
        "../../tests/test1/design-open-questions-proposal.pdf",
        "../../tests/test2/exam-zh-doc.pdf",
        "../../tests/test3/17.pdf",
    ];
    let mut checked = 0;
    for path in candidates {
        if !std::path::Path::new(path).exists() {
            continue;
        }
        let data = std::fs::read(path).expect("read corpus pdf");
        let doc = PdfDocument::open(data).unwrap_or_else(|e| panic!("open {path}: {e}"));
        assert!(doc.page_count() > 0, "{path} has no pages");
        // Touch every object stream / page tree by resolving each page, which
        // exercises the ObjStm decode cache on real files.
        for i in 0..doc.page_count() {
            let _ = doc.page(i);
        }
        checked += 1;
    }
    eprintln!("real_corpus_smoke: checked {checked} corpus file(s)");
}
