//! Encryption round-trips: rewrite_pdf with encryption must produce files
//! that zpdf's own reader opens (with the right password) and refuses (with
//! a wrong one). Content checks read the decrypted content stream bytes.

use zpdf_document::PdfDocument;
use zpdf_parser::PdfFile;
use zpdf_writer::rewrite::{rewrite_pdf, RewriteOptions};
use zpdf_writer::{DocumentBuilder, EncryptionConfig};

/// A small source document with text content to encrypt.
fn source_pdf() -> Vec<u8> {
    let mut b = DocumentBuilder::new();
    let p = b.add_page(300.0, 300.0);
    b.add_text(
        p,
        "secret payload",
        20.0,
        200.0,
        "Helvetica",
        14.0,
        (0.0, 0.0, 0.0),
    )
    .unwrap();
    b.build().unwrap()
}

/// Decrypted+decoded first-page content stream of a document.
fn page_content(doc: &PdfDocument) -> String {
    let page = doc.page(0).expect("page");
    let bytes = doc
        .file()
        .resolve_stream_data(page.contents[0])
        .expect("content stream");
    String::from_utf8_lossy(&bytes).into_owned()
}

fn roundtrip(config: EncryptionConfig, password: &str) {
    let source = source_pdf();
    let file = PdfFile::parse(source).expect("parse source");
    let encrypted = rewrite_pdf(
        &file,
        &RewriteOptions {
            compress_uncompressed: true,
            encrypt: Some(config),
            ..Default::default()
        },
    )
    .expect("rewrite+encrypt");

    // The trailer must reference /Encrypt, and the plaintext must not appear
    // anywhere in the file.
    let raw = String::from_utf8_lossy(&encrypted);
    assert!(raw.contains("/Encrypt"), "trailer must carry /Encrypt");
    assert!(
        !raw.contains("secret payload"),
        "plaintext must not leak into the encrypted file"
    );

    // Reopen with the correct password: content must decrypt.
    let doc = PdfDocument::open_with_password(encrypted.clone(), password.as_bytes())
        .expect("open with password");
    assert!(doc.is_encrypted());
    assert_eq!(doc.page_count(), 1);
    let content = page_content(&doc);
    assert!(
        content.contains("secret payload"),
        "content must survive the encryption round-trip; got: {content:?}"
    );
}

#[test]
fn aes256_roundtrip() {
    roundtrip(EncryptionConfig::aes256("user-pw", "owner-pw"), "user-pw");
}

#[test]
fn aes256_owner_password_also_opens() {
    roundtrip(EncryptionConfig::aes256("user-pw", "owner-pw"), "owner-pw");
}

#[test]
fn aes256_empty_user_password_opens_by_default() {
    let source = source_pdf();
    let file = PdfFile::parse(source).expect("parse");
    let encrypted = rewrite_pdf(
        &file,
        &RewriteOptions {
            compress_uncompressed: true,
            encrypt: Some(EncryptionConfig::aes256("", "owner-only")),
            ..Default::default()
        },
    )
    .expect("encrypt");
    let doc = PdfDocument::open(encrypted).expect("open with empty password");
    assert!(page_content(&doc).contains("secret payload"));
}

#[test]
fn rc4_roundtrip() {
    roundtrip(EncryptionConfig::rc4_128("legacy-pw", ""), "legacy-pw");
}

#[test]
fn wrong_password_is_rejected() {
    let source = source_pdf();
    let file = PdfFile::parse(source).expect("parse");
    let encrypted = rewrite_pdf(
        &file,
        &RewriteOptions {
            compress_uncompressed: true,
            encrypt: Some(EncryptionConfig::aes256("correct", "correct-owner")),
            ..Default::default()
        },
    )
    .expect("encrypt");
    let result = PdfDocument::open_with_password(encrypted, b"wrong");
    assert!(result.is_err(), "wrong password must not open the document");
}

#[test]
fn decrypt_on_rewrite_still_works() {
    // Encrypt, then rewrite without encryption: output is decrypted again.
    let source = source_pdf();
    let file = PdfFile::parse(source).expect("parse");
    let encrypted = rewrite_pdf(
        &file,
        &RewriteOptions {
            compress_uncompressed: true,
            encrypt: Some(EncryptionConfig::aes256("pw", "pw")),
            ..Default::default()
        },
    )
    .expect("encrypt");

    let file2 = PdfFile::parse_with_password(encrypted, b"pw").expect("reopen");
    let decrypted = rewrite_pdf(&file2, &RewriteOptions::default()).expect("decrypt rewrite");
    let text = String::from_utf8_lossy(&decrypted);
    assert!(
        !text.contains("/Encrypt"),
        "decrypted output must drop /Encrypt"
    );
    let doc = PdfDocument::open(decrypted).expect("open decrypted");
    assert!(page_content(&doc).contains("secret payload"));
}

#[test]
fn incremental_update_of_encrypted_document() {
    use std::io::Cursor;
    use zpdf_writer::{AnnotationSpec, IncrementalWriter, MarkupKind};

    // Build an AES-256-encrypted document.
    let source = source_pdf();
    let file = PdfFile::parse(source).expect("parse");
    let encrypted = rewrite_pdf(
        &file,
        &RewriteOptions {
            compress_uncompressed: true,
            encrypt: Some(EncryptionConfig::aes256("pw", "pw")),
            ..Default::default()
        },
    )
    .expect("encrypt");

    // Incrementally add an annotation (with /Contents string + /AP stream —
    // both must be encrypted with the document key).
    let mut writer =
        IncrementalWriter::new_with_password(encrypted, b"pw").expect("open for update");
    writer
        .add_annotation(
            0,
            &AnnotationSpec::Markup {
                kind: MarkupKind::Highlight,
                quads: vec![[20.0, 214.0, 140.0, 214.0, 20.0, 198.0, 140.0, 198.0]],
                color: (1.0, 1.0, 0.0),
                contents: Some("encrypted comment".into()),
            },
        )
        .expect("annotate");
    let mut out = Cursor::new(Vec::new());
    writer.write(&mut out).expect("write update");
    let updated = out.into_inner();

    // The updated file still opens with the password and carries the
    // annotation; the comment text must not appear in the clear.
    let raw = String::from_utf8_lossy(&updated);
    assert!(
        !raw.contains("encrypted comment"),
        "annotation text must be encrypted in the update"
    );
    let doc = PdfDocument::open_with_password(updated, b"pw").expect("reopen");
    let page = doc.page(0).expect("page");
    assert_eq!(page.annots.len(), 1, "annotation present after update");
    let annot = doc
        .file()
        .resolve(page.annots[0])
        .expect("annot")
        .as_dict()
        .expect("dict")
        .clone();
    match annot.get("Contents") {
        Some(zpdf_core::PdfObject::String(s)) => {
            let text = String::from_utf8_lossy(&s.0);
            assert!(
                text.contains("encrypted comment"),
                "decrypted /Contents must round-trip; got {text:?}"
            );
        }
        other => panic!("/Contents missing or wrong type: {other:?}"),
    }

    // Plain new() must refuse without the password.
    // (Empty-password AES-256 docs with a user password set do not open.)
}

#[test]
fn incremental_update_of_encrypted_document_requires_password() {
    use zpdf_writer::IncrementalWriter;

    let source = source_pdf();
    let file = PdfFile::parse(source).expect("parse");
    let encrypted = rewrite_pdf(
        &file,
        &RewriteOptions {
            compress_uncompressed: true,
            encrypt: Some(EncryptionConfig::aes256("pw", "pw")),
            ..Default::default()
        },
    )
    .expect("encrypt");

    assert!(
        IncrementalWriter::new(encrypted).is_err(),
        "encrypted document must not open for update without its password"
    );
}
