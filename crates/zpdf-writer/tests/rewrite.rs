use std::io::Cursor;
use zpdf_document::PdfDocument;
use zpdf_parser::PdfFile;
use zpdf_writer::{rewrite_pdf, IncrementalWriter, RewriteOptions};

/// Minimal one-page PDF with an extra *unreferenced* object (junk) to be
/// garbage-collected, plus an uncompressed content stream.
fn pdf_with_junk() -> Vec<u8> {
    let content = b"0 0 1 rg 10 10 100 100 re f ".repeat(20);
    let mut body: Vec<(u32, Vec<u8>)> = vec![
        (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
        (
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R >>".to_vec(),
        ),
        (4, {
            let mut s = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
            s.extend_from_slice(&content);
            s.extend_from_slice(b"\nendstream");
            s
        }),
        // Object 5 is referenced by nothing.
        (5, b"<< /Junk (unreferenced ballast object) >>".to_vec()),
    ];
    let mut data = b"%PDF-1.4\n".to_vec();
    let mut offsets = vec![0u64; body.len() + 1];
    for (num, content) in &mut body {
        offsets[*num as usize] = data.len() as u64;
        data.extend_from_slice(format!("{num} 0 obj\n").as_bytes());
        data.extend_from_slice(content);
        data.extend_from_slice(b"\nendobj\n");
    }
    let xref_pos = data.len();
    data.extend_from_slice(format!("xref\n0 {}\n", body.len() + 1).as_bytes());
    data.extend_from_slice(b"0000000000 65535 f \n");
    for offset in &offsets[1..] {
        data.extend_from_slice(format!("{:010} 00000 n \n", offset).as_bytes());
    }
    data.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_pos}\n%%EOF\n",
            body.len() + 1
        )
        .as_bytes(),
    );
    data
}

#[test]
fn rewrite_drops_unreachable_objects() {
    let original = pdf_with_junk();
    let file = PdfFile::parse(original).expect("parse");
    let rewritten = rewrite_pdf(&file, &RewriteOptions::default()).expect("rewrite");

    let s = String::from_utf8_lossy(&rewritten);
    assert!(
        !s.contains("unreferenced ballast object"),
        "junk object must be garbage-collected"
    );

    let doc = PdfDocument::open(rewritten).expect("open rewritten");
    assert_eq!(doc.page_count(), 1);
}

#[test]
fn rewrite_compresses_bare_streams() {
    let original = pdf_with_junk();
    let orig_len = original.len();
    let file = PdfFile::parse(original).expect("parse");
    let rewritten = rewrite_pdf(&file, &RewriteOptions::default()).expect("rewrite");
    // The 560-byte repetitive content stream compresses well.
    assert!(
        rewritten.len() < orig_len,
        "rewritten ({}) should be smaller than original ({orig_len})",
        rewritten.len()
    );

    // Content still decodes to the original bytes.
    let doc = PdfDocument::open(rewritten).expect("open");
    let page = doc.page(0).expect("page");
    let content = doc.page_content_bytes(&page).expect("content");
    assert!(String::from_utf8_lossy(&content).contains("re f"));
}

#[test]
fn rewrite_without_compression_keeps_stream_raw() {
    let original = pdf_with_junk();
    let file = PdfFile::parse(original).expect("parse");
    let rewritten = rewrite_pdf(
        &file,
        &RewriteOptions {
            compress_uncompressed: false,
        },
    )
    .expect("rewrite");
    let s = String::from_utf8_lossy(&rewritten);
    assert!(s.contains("re f"), "raw content stream stays readable");
}

#[test]
fn rewrite_collapses_incremental_updates() {
    // Base file + an incremental update that overwrites the page (rotation):
    // the rewritten file must contain a single version of the page.
    let original = pdf_with_junk();
    let mut writer = IncrementalWriter::new(original).expect("writer");
    writer.rotate_page(0, 90).expect("rotate");
    let mut out = Cursor::new(Vec::new());
    writer.write(&mut out).expect("write");
    let updated = out.into_inner();

    let file = PdfFile::parse(updated).expect("parse");
    let rewritten = rewrite_pdf(&file, &RewriteOptions::default()).expect("rewrite");

    let doc = PdfDocument::open(rewritten.clone()).expect("open");
    assert_eq!(doc.page(0).expect("page").rotate, 90, "edit survives");

    // Only one /Type /Page object body in the file.
    let occurrences = String::from_utf8_lossy(&rewritten)
        .matches("/Type /Page ")
        .count();
    assert_eq!(occurrences, 1, "superseded page version dropped");
}

#[test]
fn rewrite_preserves_info_and_id() {
    // Give the base file an /Info and /ID via incremental metadata edit.
    let original = pdf_with_junk();
    let mut writer = IncrementalWriter::new(original).expect("writer");
    writer
        .set_info(&zpdf_writer::InfoUpdate {
            title: Some(Some("Rewrite Test".to_string())),
            ..Default::default()
        })
        .expect("set_info");
    let mut out = Cursor::new(Vec::new());
    writer.write(&mut out).expect("write");

    let file = PdfFile::parse(out.into_inner()).expect("parse");
    let rewritten = rewrite_pdf(&file, &RewriteOptions::default()).expect("rewrite");
    let doc = PdfDocument::open(rewritten).expect("open");
    let info = doc.info().expect("info dict survives rewrite");
    assert_eq!(info.title.as_deref(), Some("Rewrite Test"));
}
