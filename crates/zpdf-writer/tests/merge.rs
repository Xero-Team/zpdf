use std::io::Cursor;
use zpdf_document::PdfDocument;
use zpdf_parser::PdfFile;
use zpdf_writer::IncrementalWriter;

/// Build a small PDF with `n` pages, each with a distinct MediaBox width
/// (600 + page index) and a tiny content stream drawing a filled rect, so the
/// merged output's pages can be traced back to their source.
fn pdf_with_pages(n: usize, width_base: i32) -> Vec<u8> {
    let mut body: Vec<(u32, Vec<u8>)> = Vec::new();
    // 1: catalog, 2: pages root, 3..3+n: pages, 3+n..3+2n: content streams
    let mut kids = String::new();
    for i in 0..n {
        kids.push_str(&format!("{} 0 R ", 3 + i));
    }
    body.push((1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()));
    body.push((
        2,
        format!("<< /Type /Pages /Kids [{kids}] /Count {n} >>").into_bytes(),
    ));
    for i in 0..n {
        let content_num = 3 + n + i;
        body.push((
            (3 + i) as u32,
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {} 792] /Contents {content_num} 0 R >>",
                width_base + i as i32
            )
            .into_bytes(),
        ));
    }
    let content = b"0 0 1 rg 10 10 100 100 re f";
    for i in 0..n {
        let mut stream = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
        stream.extend_from_slice(content);
        stream.extend_from_slice(b"\nendstream");
        body.push(((3 + n + i) as u32, stream));
    }

    let mut data = b"%PDF-1.4\n".to_vec();
    let mut offsets = vec![0u64; body.len() + 1];
    for (num, content) in &body {
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
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
            body.len() + 1,
            xref_pos
        )
        .as_bytes(),
    );
    data
}

fn merge(base: Vec<u8>, others: &[Vec<u8>]) -> Vec<u8> {
    let mut writer = IncrementalWriter::new(base).expect("writer");
    for other in others {
        let file = PdfFile::parse(other.clone()).expect("parse source");
        writer.append_document_pages(&file).expect("append");
    }
    let mut out = Cursor::new(Vec::new());
    writer.write(&mut out).expect("write");
    out.into_inner()
}

#[test]
fn merge_two_documents_page_count_and_order() {
    let a = pdf_with_pages(2, 600);
    let b = pdf_with_pages(3, 700);
    let merged = merge(a, &[b]);

    let doc = PdfDocument::open(merged).expect("open merged");
    assert_eq!(doc.page_count(), 5);

    // Pages 0-1 from A (widths 600, 601), pages 2-4 from B (700, 701, 702).
    for (i, w) in [600.0, 601.0, 700.0, 701.0, 702.0].iter().enumerate() {
        let page = doc.page(i).expect("page");
        assert_eq!(page.width(), *w, "page {i} width");
    }
}

#[test]
fn merged_pages_keep_their_content_streams() {
    let a = pdf_with_pages(1, 600);
    let b = pdf_with_pages(1, 700);
    let merged = merge(a, &[b]);

    let doc = PdfDocument::open(merged).expect("open merged");
    for i in 0..2 {
        let page = doc.page(i).expect("page");
        let content = doc.page_content_bytes(&page).expect("content");
        assert!(
            String::from_utf8_lossy(&content).contains("re f"),
            "page {i} content stream survived the copy"
        );
    }
}

#[test]
fn merge_three_documents() {
    let a = pdf_with_pages(1, 600);
    let b = pdf_with_pages(1, 700);
    let c = pdf_with_pages(2, 800);
    let merged = merge(a, &[b, c]);

    let doc = PdfDocument::open(merged).expect("open merged");
    assert_eq!(doc.page_count(), 4);
    assert_eq!(doc.page(3).expect("page").width(), 801.0);
}

#[test]
fn append_selected_pages_in_custom_order() {
    let a = pdf_with_pages(1, 600);
    let b = pdf_with_pages(3, 700);

    let mut writer = IncrementalWriter::new(a).expect("writer");
    let file = PdfFile::parse(b).expect("parse");
    // Append page 2 then page 0 of B.
    writer.append_pages(&file, &[2, 0]).expect("append");
    let mut out = Cursor::new(Vec::new());
    writer.write(&mut out).expect("write");

    let doc = PdfDocument::open(out.into_inner()).expect("open");
    assert_eq!(doc.page_count(), 3);
    assert_eq!(doc.page(1).expect("page").width(), 702.0);
    assert_eq!(doc.page(2).expect("page").width(), 700.0);
}

#[test]
fn append_out_of_range_page_is_an_error() {
    let a = pdf_with_pages(1, 600);
    let b = pdf_with_pages(1, 700);
    let mut writer = IncrementalWriter::new(a).expect("writer");
    let file = PdfFile::parse(b).expect("parse");
    assert!(writer.append_pages(&file, &[1]).is_err());
}

#[test]
fn inherited_attributes_are_materialized() {
    // Source: MediaBox on the Pages node (inherited), not on the leaf.
    let mut data = b"%PDF-1.4\n".to_vec();
    let objs: Vec<(u32, &[u8])> = vec![
        (1, b"<< /Type /Catalog /Pages 2 0 R >>"),
        (
            2,
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 555 792] /Rotate 90 >>",
        ),
        (3, b"<< /Type /Page /Parent 2 0 R >>"),
    ];
    let mut offsets = vec![0u64; objs.len() + 1];
    for (num, content) in &objs {
        offsets[*num as usize] = data.len() as u64;
        data.extend_from_slice(format!("{num} 0 obj\n").as_bytes());
        data.extend_from_slice(content);
        data.extend_from_slice(b"\nendobj\n");
    }
    let xref_pos = data.len();
    data.extend_from_slice(format!("xref\n0 {}\n", objs.len() + 1).as_bytes());
    data.extend_from_slice(b"0000000000 65535 f \n");
    for offset in &offsets[1..] {
        data.extend_from_slice(format!("{:010} 00000 n \n", offset).as_bytes());
    }
    data.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_pos}\n%%EOF\n",
            objs.len() + 1
        )
        .as_bytes(),
    );

    let base = pdf_with_pages(1, 600);
    let merged = merge(base, &[data]);
    let doc = PdfDocument::open(merged).expect("open");
    assert_eq!(doc.page_count(), 2);
    let page = doc.page(1).expect("page");
    assert_eq!(page.width(), 555.0, "inherited MediaBox materialized");
    assert_eq!(page.rotate, 90, "inherited Rotate materialized");
}
