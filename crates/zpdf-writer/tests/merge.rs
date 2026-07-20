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

// ---------------------------------------------------------------------------
// append_document (full merge: outlines, AcroForm, OCGs)
// ---------------------------------------------------------------------------

/// A PDF with one page, a two-item outline, one text field, and one OCG.
fn rich_pdf(field_name: &str, title_prefix: &str) -> Vec<u8> {
    let objects = [
        // 1: catalog
        "<< /Type /Catalog /Pages 2 0 R /Outlines 4 0 R /AcroForm << /Fields [7 0 R] >> \
           /OCProperties << /OCGs [8 0 R] /D << /ON [8 0 R] /OFF [] >> >> >>"
            .to_string(),
        // 2: pages root
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        // 3: page (also the widget's /P target)
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Annots [7 0 R] >>".to_string(),
        // 4: outlines root
        "<< /Type /Outlines /First 5 0 R /Last 6 0 R /Count 2 >>".to_string(),
        // 5: outline item 1
        format!("<< /Title ({title_prefix} One) /Parent 4 0 R /Next 6 0 R /Dest [3 0 R /Fit] >>"),
        // 6: outline item 2
        format!("<< /Title ({title_prefix} Two) /Parent 4 0 R /Prev 5 0 R /Dest [3 0 R /Fit] >>"),
        // 7: text field doubling as its own widget
        format!(
            "<< /FT /Tx /T ({field_name}) /V (hello) /Type /Annot /Subtype /Widget \
               /Rect [100 700 300 720] /P 3 0 R >>"
        ),
        // 8: an OCG
        "<< /Type /OCG /Name (Layer A) >>".to_string(),
    ];
    let mut pdf = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, obj) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n{}\nendobj\n", i + 1, obj).as_bytes());
    }
    let xref = pdf.len();
    pdf.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
    pdf.extend_from_slice(b"0000000000 65535 f \n");
    for ofs in offsets {
        pdf.extend_from_slice(format!("{ofs:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    pdf
}

#[test]
fn append_document_merges_outlines_fields_and_ocgs() {
    let base = rich_pdf("name", "Base");
    let other = rich_pdf("name", "Other"); // deliberately colliding field name

    let mut writer = IncrementalWriter::new(base).expect("writer");
    let src = PdfFile::parse(other).expect("parse source");
    let appended = writer.append_document(&src).expect("append_document");
    assert_eq!(appended, 1);

    let mut out = Cursor::new(Vec::new());
    writer.write(&mut out).expect("write");
    let bytes = out.into_inner();
    let doc = PdfDocument::open(bytes).expect("open merged");
    assert_eq!(doc.page_count(), 2);

    // Outline: four top-level items in order Base One, Base Two, Other One, Other Two.
    let outline = doc.outline();
    let titles: Vec<String> = outline.iter().map(|i| i.title.clone()).collect();
    assert_eq!(
        titles,
        vec!["Base One", "Base Two", "Other One", "Other Two"],
        "merged outline order"
    );

    // AcroForm: two fields, the second renamed on collision.
    let form = zpdf_document::AcroForm::parse(doc.file()).expect("acroform");
    let mut names: Vec<String> = form.fields.iter().map(|f| f.name.clone()).collect();
    names.sort();
    assert_eq!(names, vec!["name", "name_2"], "collision renaming");
}

#[test]
fn append_document_without_extras_still_works() {
    // Sources without outlines/forms/OCGs go through the same path.
    let base = pdf_with_pages(1, 600);
    let mut writer = IncrementalWriter::new(base).expect("writer");
    let src_bytes = pdf_with_pages(1, 700);
    let src = PdfFile::parse(src_bytes).expect("parse");
    let n = writer.append_document(&src).expect("append");
    assert_eq!(n, 1);
    let mut out = Cursor::new(Vec::new());
    writer.write(&mut out).expect("write");
    let doc = PdfDocument::open(out.into_inner()).expect("open");
    assert_eq!(doc.page_count(), 2);
}
