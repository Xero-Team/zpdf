//! End-to-end digital-signature acceptance tests (ISO 32000-1 §12.8).
//!
//! Each test assembles a real signed PDF — a `/Sig` form field whose signature
//! dictionary carries an exact `/ByteRange` and a hand-built CMS `/Contents`
//! blob whose `messageDigest` attribute is the true SHA-256 of the signed spans
//! — then opens it through the public API and asserts the byte-range integrity
//! verdict. This exercises the whole path: field-tree walk, `/ByteRange` parse,
//! DER/CMS descent, and digest comparison.
//!
//! To sidestep the signing chicken-and-egg (the CMS lives inside the excluded
//! `/Contents` gap), the `/ByteRange` numbers and `/Contents` hex are written as
//! fixed-width placeholders, patched once offsets are known, and only then is
//! the digest taken over the finalized signed bytes.

use sha2::{Digest, Sha256};
use zpdf::{DigestStatus, PdfDocument};

// ---- Minimal DER builder --------------------------------------------------

const SEQ: u8 = 0x30;
const SET: u8 = 0x31;
const OID: u8 = 0x06;
const OCTET: u8 = 0x04;
const INT: u8 = 0x02;
const CTX0: u8 = 0xA0;

fn der(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let len = content.len();
    if len < 0x80 {
        out.push(len as u8);
    } else if len < 0x100 {
        out.extend_from_slice(&[0x81, len as u8]);
    } else {
        out.extend_from_slice(&[0x82, (len >> 8) as u8, (len & 0xff) as u8]);
    }
    out.extend_from_slice(content);
    out
}

/// A detached CMS `SignedData` blob carrying `digest` as the SHA-256
/// `messageDigest` signed attribute. Its byte length is independent of the
/// digest *value*, so it is stable across the placeholder→final patch.
fn build_cms(digest: &[u8]) -> Vec<u8> {
    let sha256_oid = [0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01];
    let md_oid = [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x09, 0x04];
    let data_oid = [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x07, 0x01];
    let signed_data_oid = [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x07, 0x02];

    let digest_alg = der(SEQ, &der(OID, &sha256_oid));
    let md_attr = der(
        SEQ,
        &[der(OID, &md_oid), der(SET, &der(OCTET, digest))].concat(),
    );
    let signed_attrs = der(CTX0, &md_attr);
    let signer_info = der(
        SEQ,
        &[
            der(INT, &[1]),
            der(SEQ, &[]), // sid placeholder
            digest_alg.clone(),
            signed_attrs,
            der(SEQ, &der(OID, &[0x2a])), // signatureAlgorithm placeholder
            der(OCTET, &[0xde, 0xad, 0xbe]), // signature placeholder
        ]
        .concat(),
    );
    let signer_infos = der(SET, &signer_info);
    let signed_data = der(
        SEQ,
        &[
            der(INT, &[1]),
            der(SET, &digest_alg),
            der(SEQ, &der(OID, &data_oid)),
            signer_infos,
        ]
        .concat(),
    );
    der(
        SEQ,
        &[der(OID, &signed_data_oid), der(CTX0, &signed_data)].concat(),
    )
}

fn to_hex(bytes: &[u8]) -> Vec<u8> {
    const HEX: &[u8] = b"0123456789abcdef";
    let mut out = Vec::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize]);
        out.push(HEX[(b & 0xf) as usize]);
    }
    out
}

// ---- Signed-PDF assembler -------------------------------------------------

/// Build a complete signed PDF. Returns the finalized bytes. The signature
/// covers the whole document via a correct `/ByteRange` and a CMS whose
/// `messageDigest` is the real SHA-256 over the signed spans.
fn build_signed_pdf() -> Vec<u8> {
    // Fixed CMS size (digest value doesn't change the length): use a dummy
    // 32-byte digest to size the /Contents field.
    let cms_len = build_cms(&[0u8; 32]).len();
    let hex_len = cms_len * 2;

    // Object bodies. The sig dict (obj 6) uses fixed-width placeholders:
    //  - each /ByteRange number is a 10-char field (space-padded on patch),
    //  - /Contents holds `hex_len` '0' chars between < >.
    let br_placeholder = "[__________ __________ __________ __________]";
    let contents_placeholder = "0".repeat(hex_len);
    let sig_dict = format!(
        "<< /Type /Sig /Filter /Adobe.PPKLite /SubFilter /adbe.pkcs7.detached \
         /ByteRange {br_placeholder} /Contents <{contents_placeholder}> >>"
    );

    let objects: Vec<String> = vec![
        "<< /Type /Catalog /Pages 2 0 R /AcroForm 4 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] >>".to_string(),
        "<< /Fields [5 0 R] /SigFlags 3 >>".to_string(),
        "<< /FT /Sig /T (Signature1) /V 6 0 R /Subtype /Widget /Rect [0 0 0 0] /P 3 0 R >>"
            .to_string(),
        sig_dict,
    ];

    let mut buf = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (i, body) in objects.iter().enumerate() {
        offsets.push(buf.len());
        buf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
    }
    let xref_pos = buf.len();
    let n = objects.len() + 1;
    buf.extend_from_slice(format!("xref\n0 {n}\n0000000000 65535 f \n").as_bytes());
    for off in &offsets {
        buf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    buf.extend_from_slice(
        format!("trailer\n<< /Size {n} /Root 1 0 R >>\nstartxref\n{xref_pos}\n%%EOF\n").as_bytes(),
    );

    // Locate the /Contents literal: `<` then `hex_len` chars then `>`.
    let contents_lt = find(&buf, b"/Contents <") + "/Contents ".len();
    assert_eq!(buf[contents_lt], b'<');
    let contents_start = contents_lt; // offset of `<`
    let contents_end = contents_lt + 1 + hex_len + 1; // just past `>`
    assert_eq!(buf[contents_end - 1], b'>');

    // Patch /ByteRange placeholders (before digesting — they lie in span 1).
    let br = [
        0usize,
        contents_start,
        contents_end,
        buf.len() - contents_end,
    ];
    let br_at = find(&buf, br_placeholder.as_bytes());
    // Four 10-char slots separated by single spaces after the leading '['.
    for (i, &v) in br.iter().enumerate() {
        let slot = br_at + 1 + i * 11; // '[' + i*(10 + ' ')
        let s = format!("{v:>10}");
        buf[slot..slot + 10].copy_from_slice(s.as_bytes());
    }

    // Digest the finalized signed spans, build the real CMS, splice its hex in.
    let mut hasher = Sha256::new();
    hasher.update(&buf[0..contents_start]);
    hasher.update(&buf[contents_end..]);
    let digest = hasher.finalize();

    let cms = build_cms(&digest);
    assert_eq!(cms.len(), cms_len, "CMS length must be digest-value stable");
    let hex = to_hex(&cms);
    assert_eq!(hex.len(), hex_len);
    buf[contents_lt + 1..contents_lt + 1 + hex_len].copy_from_slice(&hex);

    buf
}

fn find(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
        .expect("needle present")
}

// ---- Tests ----------------------------------------------------------------

#[test]
fn signed_document_verifies_intact_byte_range() {
    let pdf = build_signed_pdf();
    let doc = PdfDocument::open(pdf).expect("open");
    let sigs = doc.signatures();
    assert_eq!(sigs.len(), 1);

    let s = &sigs[0];
    assert_eq!(s.field_name, "Signature1");
    assert_eq!(s.filter.as_deref(), Some("Adobe.PPKLite"));
    assert_eq!(s.sub_filter.as_deref(), Some("adbe.pkcs7.detached"));
    assert_eq!(s.digest_algorithm.as_deref(), Some("SHA-256"));

    assert_eq!(s.coverage.ranges.len(), 2);
    assert!(s.coverage.covers_whole_document);
    assert_eq!(s.coverage.bytes_after_signature, 0);

    assert_eq!(
        s.digest,
        DigestStatus::Verified,
        "digest of the signed byte range must match the CMS messageDigest"
    );
}

#[test]
fn tampered_signed_region_is_detected() {
    let mut pdf = build_signed_pdf();
    // Flip a byte inside the first signed span (the MediaBox digit region is
    // well before /Contents). Any covered-byte change must break the digest.
    let at = find(&pdf, b"/MediaBox [0 0 200 200]") + "/MediaBox [0 0 ".len();
    pdf[at] = b'9'; // 200 -> 900

    let doc = PdfDocument::open(pdf).expect("open");
    let sigs = doc.signatures();
    assert_eq!(sigs.len(), 1);
    assert_eq!(
        sigs[0].digest,
        DigestStatus::Mismatch,
        "altering a signed byte must be detected"
    );
}

#[test]
fn bytes_appended_after_signing_are_reported() {
    let mut pdf = build_signed_pdf();
    let original_len = pdf.len();
    // Simulate an incremental update: append content after the signed range.
    // The signature's /ByteRange still ends at the original EOF.
    pdf.extend_from_slice(b"\n% appended incremental update bytes\n");

    let doc = PdfDocument::open(pdf).expect("open");
    let s = &doc.signatures()[0];
    // The signed byte range no longer reaches EOF.
    assert!(!s.coverage.covers_whole_document);
    assert_eq!(
        s.coverage.bytes_after_signature,
        (pdf_len_after(original_len))
    );
    // The originally-signed bytes are unchanged, so integrity still holds.
    assert_eq!(s.digest, DigestStatus::Verified);
}

/// Number of bytes appended in the incremental-update test.
fn pdf_len_after(_original_len: usize) -> usize {
    b"\n% appended incremental update bytes\n".len()
}

#[test]
fn document_without_signatures_is_empty() {
    // A plain document (no AcroForm) yields no signatures and does not panic.
    let pdf = b"%PDF-1.7\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
                2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n\
                3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>\nendobj\n\
                xref\n0 4\n0000000000 65535 f \n\
                trailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n0\n%%EOF"
        .to_vec();
    let doc = PdfDocument::open(pdf).expect("open");
    assert!(doc.signatures().is_empty());
}
