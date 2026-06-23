//! End-to-end tests for embedded files (`/Names /EmbeddedFiles`) and PDF 2.0
//! associated files (`/AF`). Builds tiny PDFs whose attachment is a real
//! FlateDecode-compressed payload (the ZUGFeRD/Factur-X case) and checks that
//! the public [`PdfDocument`] API lists the metadata and that
//! `embedded_file_bytes` decodes the payload back through the filter pipeline.

use std::io::Write;

use flate2::write::ZlibEncoder;
use flate2::Compression;
use zpdf::{EmbeddedSource, PdfDocument};

/// Assemble numbered object bodies into a PDF with a correct xref + trailer
/// (`/Root` = object 1). Mirrors the helper used by the other facade tests.
fn assemble(objs: &[Vec<u8>]) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = Vec::with_capacity(objs.len());
    for (i, body) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref_pos = out.len();
    let n = objs.len() + 1;
    out.extend_from_slice(format!("xref\n0 {n}\n").as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for off in &offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<< /Size {n} /Root 1 0 R >>\nstartxref\n{xref_pos}\n%%EOF\n").as_bytes(),
    );
    out
}

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(data).expect("compress");
    enc.finish().expect("finish")
}

/// A FlateDecode embedded-file stream object carrying `payload`.
fn flate_embedded_file(subtype: &str, payload: &[u8]) -> Vec<u8> {
    let compressed = zlib(payload);
    let mut v = format!(
        "<< /Type /EmbeddedFile /Subtype {subtype} /Filter /FlateDecode \
         /Params << /Size {} >> /Length {} >>\nstream\n",
        payload.len(),
        compressed.len()
    )
    .into_bytes();
    v.extend_from_slice(&compressed);
    v.extend_from_slice(b"\nendstream");
    v
}

const INVOICE_XML: &[u8] =
    b"<?xml version=\"1.0\"?><CrossIndustryInvoice>factur-x payload</CrossIndustryInvoice>";

#[test]
fn name_tree_and_af_share_one_compressed_attachment() {
    // obj4 is the compressed payload; obj6 the file spec; the name tree (obj5)
    // and the catalog /AF both reference obj6 — the PDF 2.0 / ZUGFeRD shape.
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R /Names << /EmbeddedFiles 5 0 R >> /AF [6 0 R] >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>".to_vec(),
        flate_embedded_file("/application#2Fxml", INVOICE_XML),
        b"<< /Names [ (factur-x.xml) 6 0 R ] >>".to_vec(),
        b"<< /Type /Filespec /F (factur-x.xml) /UF (factur-x.xml) \
          /AFRelationship /Alternative /EF << /F 4 0 R >> >>"
            .to_vec(),
    ]);
    let doc = PdfDocument::open(pdf).expect("open");

    let embedded = doc.embedded_files();
    assert_eq!(embedded.len(), 1);
    let ef = &embedded[0];
    assert_eq!(ef.name, "factur-x.xml");
    assert_eq!(ef.subtype.as_deref(), Some("application/xml"));
    assert_eq!(ef.relationship.as_deref(), Some("Alternative"));
    assert_eq!(ef.size, Some(INVOICE_XML.len() as i64));
    assert_eq!(ef.source, EmbeddedSource::NameTree);
    assert!(ef.is_embedded());

    // Same file is reachable as a catalog-level associated file.
    let af = doc.associated_files();
    assert_eq!(af.len(), 1);
    assert_eq!(af[0].source, EmbeddedSource::AssociatedFile);
    assert_eq!(af[0].relationship.as_deref(), Some("Alternative"));
    assert_eq!(af[0].stream, ef.stream);

    // Extraction decompresses the FlateDecode payload back to the original XML.
    let bytes = doc.embedded_file_bytes(ef).expect("extract");
    assert_eq!(bytes, INVOICE_XML);
}

#[test]
fn page_level_af_is_separate_from_catalog() {
    let pdf = assemble(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /AF [5 0 R] >>".to_vec(),
        flate_embedded_file("/text#2Fplain", b"page attachment"),
        b"<< /Type /Filespec /F (note.txt) /AFRelationship /Supplement /EF << /F 4 0 R >> >>"
            .to_vec(),
    ]);
    let doc = PdfDocument::open(pdf).expect("open");
    let page = doc.page(0).expect("page");

    assert!(doc.embedded_files().is_empty());
    assert!(doc.associated_files().is_empty());
    let page_af = doc.page_associated_files(&page);
    assert_eq!(page_af.len(), 1);
    assert_eq!(page_af[0].name, "note.txt");
    assert_eq!(page_af[0].relationship.as_deref(), Some("Supplement"));
    assert_eq!(
        doc.embedded_file_bytes(&page_af[0]).expect("extract"),
        b"page attachment"
    );
}
