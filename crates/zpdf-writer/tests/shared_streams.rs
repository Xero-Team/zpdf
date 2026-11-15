use std::io::Cursor;
use std::sync::Arc;

use zpdf_core::{PdfDict, PdfObject};
use zpdf_document::PdfDocument;
use zpdf_parser::PdfFile;
use zpdf_writer::{copy_object_graph, DocumentBuilder, IncrementalWriter, ObjectIdMap};

fn page() -> Vec<u8> {
    let mut builder = DocumentBuilder::new();
    let p = builder.add_page(120.0, 80.0);
    builder
        .add_text(
            p,
            "shared payload",
            10.0,
            40.0,
            "Helvetica",
            12.0,
            (0.0, 0.0, 0.0),
        )
        .unwrap();
    builder.build().unwrap()
}

fn source_with_unreferenced_payload() -> Arc<[u8]> {
    let mut writer = IncrementalWriter::new(page()).unwrap();
    writer
        .try_add_stream(&PdfDict::new(), &vec![b'X'; 4 * 1024 * 1024])
        .unwrap();
    let mut out = Cursor::new(Vec::new());
    writer.write(&mut out).unwrap();
    out.into_inner().into()
}

#[test]
fn copied_stream_does_not_pin_its_source_document() {
    let bytes = source_with_unreferenced_payload();
    let weak = Arc::downgrade(&bytes);
    let source = PdfDocument::open(bytes).unwrap();
    let id = source.page(0).unwrap().contents[0];
    let expected = source.file().resolve_stream_data(id).unwrap();
    let mut writer = IncrementalWriter::new(page()).unwrap();
    let dest =
        copy_object_graph(source.file(), id, &mut writer, &mut ObjectIdMap::new(), &[]).unwrap();
    drop(source);
    assert!(
        weak.upgrade().is_none(),
        "import must release the backing PDF"
    );
    let mut out = Cursor::new(Vec::new());
    writer.write(&mut out).unwrap();
    let result = PdfFile::parse(out.into_inner()).unwrap();
    assert_eq!(result.resolve_stream_data(dest).unwrap(), expected);
}

#[test]
fn repeated_document_imports_release_each_source_before_writing() {
    let mut writer = IncrementalWriter::new(page()).unwrap();
    for _ in 0..3 {
        let bytes = source_with_unreferenced_payload();
        let weak = Arc::downgrade(&bytes);
        let source = PdfFile::parse(bytes).unwrap();
        assert_eq!(writer.append_document(&source).unwrap(), 1);
        drop(source);
        assert!(
            weak.upgrade().is_none(),
            "pending imports must not retain whole source files"
        );
    }
    let mut out = Cursor::new(Vec::new());
    writer.write(&mut out).unwrap();
    let result = PdfDocument::open(out.into_inner()).unwrap();
    assert_eq!(result.page_count(), 4);
    for i in 0..4 {
        let bytes = result.page_content_bytes(&result.page(i).unwrap()).unwrap();
        assert!(!bytes.is_empty());
    }
}

#[test]
fn same_document_edits_share_views_and_preserve_original_bytes() {
    let original = page();
    let mut writer = IncrementalWriter::new(original.clone()).unwrap();
    let id = writer.document().page(0).unwrap().contents[0];
    let object = writer.resolve_current(id).unwrap();
    let data = &object.as_stream().unwrap().data;
    let original_ptr = data.as_ptr();
    let base = writer.document().file().data();
    let offset = original_ptr as usize - base.as_ptr() as usize;
    assert_eq!(base.get(offset..offset + data.len()), Some(data.as_slice()));
    writer.overwrite_object(id, object);
    let current = writer.resolve_current(id).unwrap();
    assert_eq!(current.as_stream().unwrap().data.as_ptr(), original_ptr);
    let mut out = Cursor::new(Vec::new());
    writer.write(&mut out).unwrap();
    assert!(out.get_ref().starts_with(&original));
    let result = PdfFile::parse(out.into_inner()).unwrap();
    assert_eq!(result.resolve(id).unwrap(), current);
    assert!(matches!(current, PdfObject::Stream(_)));
}

#[test]
fn overwritten_encrypted_stream_is_reencrypted_without_mutating_source() {
    use zpdf_writer::{rewrite_pdf, EncryptionConfig, RewriteOptions};
    let encrypted = rewrite_pdf(
        &PdfFile::parse(page()).unwrap(),
        &RewriteOptions {
            encrypt: Some(EncryptionConfig::aes256("user", "owner")),
            ..Default::default()
        },
    )
    .unwrap();
    let mut writer = IncrementalWriter::new_with_password(encrypted.clone(), b"user").unwrap();
    let id = writer.document().page(0).unwrap().contents[0];
    let expected = writer.document().file().resolve_stream_data(id).unwrap();
    let object = writer.resolve_current(id).unwrap();
    writer.overwrite_object(id, object);
    let mut out = Cursor::new(Vec::new());
    writer.write(&mut out).unwrap();
    assert_eq!(writer.document().file().data(), encrypted.as_slice());
    assert!(out.get_ref().starts_with(&encrypted));
    let result = PdfFile::parse_with_password(out.into_inner(), b"user").unwrap();
    assert_eq!(result.resolve_stream_data(id).unwrap(), expected);
}
