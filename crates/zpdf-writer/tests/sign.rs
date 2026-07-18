//! End-to-end signing tests: sign with zpdf-writer, verify with the existing
//! signature verifier (`PdfDocument::signatures`).

use zpdf_document::{CryptoStatus, DigestStatus, PdfDocument};
use zpdf_writer::{IncrementalWriter, SignatureOptions, SigningKey};

fn minimal_pdf() -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(b"%PDF-1.4\n");
    data.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
    data.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
    data.extend_from_slice(
        b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>\nendobj\n",
    );
    data.extend_from_slice(b"xref\n0 4\n");
    data.extend_from_slice(b"0000000000 65535 f \n");
    data.extend_from_slice(b"0000000009 00000 n \n");
    data.extend_from_slice(b"0000000058 00000 n \n");
    data.extend_from_slice(b"0000000117 00000 n \n");
    data.extend_from_slice(b"trailer\n<< /Size 4 /Root 1 0 R >>\n");
    data.extend_from_slice(b"startxref\n187\n%%EOF\n");
    data
}

// ---- Minimal DER / X.509 builders (mirrors the verifier's expectations) ----

const SEQ: u8 = 0x30;
const SET: u8 = 0x31;
const OID: u8 = 0x06;
const INT: u8 = 0x02;
const UTF8_STRING: u8 = 0x0c;
const NULL: u8 = 0x05;
const BIT_STRING: u8 = 0x03;

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

fn bit_string(body: &[u8]) -> Vec<u8> {
    let mut v = vec![0x00];
    v.extend_from_slice(body);
    der(BIT_STRING, &v)
}

fn build_cert(spki: &[u8], cn: &str) -> Vec<u8> {
    let cn_oid = [0x55, 0x04, 0x03];
    let atv = der(
        SEQ,
        &[der(OID, &cn_oid), der(UTF8_STRING, cn.as_bytes())].concat(),
    );
    let subject = der(SEQ, &der(SET, &atv));
    let tbs = der(
        SEQ,
        &[
            der(INT, &[0x01]),
            der(SEQ, &der(OID, &[0x2a])),
            der(SEQ, &[]),
            der(SEQ, &[]),
            subject,
            spki.to_vec(),
        ]
        .concat(),
    );
    der(
        SEQ,
        &[tbs, der(SEQ, &der(OID, &[0x2a])), bit_string(&[0xde, 0xad])].concat(),
    )
}

fn ec_p256_spki(point: &[u8]) -> Vec<u8> {
    let ec_pub_oid = [0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
    let p256_oid = [0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
    let alg = der(SEQ, &[der(OID, &ec_pub_oid), der(OID, &p256_oid)].concat());
    der(SEQ, &[alg, bit_string(point)].concat())
}

fn rsa_spki(pkcs1_pub_der: &[u8]) -> Vec<u8> {
    let rsa_oid = [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
    let alg = der(SEQ, &[der(OID, &rsa_oid), der(NULL, &[])].concat());
    der(SEQ, &[alg, bit_string(pkcs1_pub_der)].concat())
}

/// A deterministic P-256 key + matching self-styled certificate.
fn ecdsa_material() -> (Vec<u8>, SigningKey) {
    let scalar = [0x42u8; 32];
    let key = SigningKey::ecdsa_p256_from_scalar(&scalar).expect("scalar");
    let sk = p256::ecdsa::SigningKey::from_slice(&scalar).unwrap();
    let point = p256::EncodedPoint::from(sk.verifying_key())
        .to_bytes()
        .to_vec();
    let cert = build_cert(&ec_p256_spki(&point), "zpdf signer");
    (cert, key)
}

#[test]
fn ecdsa_p256_sign_then_verify_roundtrip() {
    let (cert, key) = ecdsa_material();
    let writer = IncrementalWriter::new(minimal_pdf()).expect("writer");
    let signed = writer
        .sign(
            &cert,
            &key,
            &SignatureOptions {
                name: Some("Alice".to_string()),
                reason: Some("Approval".to_string()),
                ..Default::default()
            },
        )
        .expect("sign");

    let doc = PdfDocument::open(signed).expect("open signed");
    let sigs = doc.signatures();
    assert_eq!(sigs.len(), 1, "one signature found");
    let s = &sigs[0];
    assert_eq!(s.digest, DigestStatus::Verified, "byte range digest");
    assert_eq!(s.crypto, CryptoStatus::Valid, "public-key signature");
    assert!(s.is_cryptographically_valid());
    assert_eq!(s.signature_algorithm.as_deref(), Some("ECDSA (P-256)"));
    assert_eq!(s.signer_common_name.as_deref(), Some("zpdf signer"));
    assert_eq!(s.name.as_deref(), Some("Alice"));
    assert_eq!(s.reason.as_deref(), Some("Approval"));
}

#[test]
fn rsa_sign_then_verify_roundtrip() {
    use rand_chacha::rand_core::SeedableRng;
    use rand_chacha::ChaCha20Rng;
    use rsa::pkcs1::{EncodeRsaPrivateKey, EncodeRsaPublicKey};

    let mut rng = ChaCha20Rng::from_seed([9u8; 32]);
    let priv_key = rsa::RsaPrivateKey::new(&mut rng, 2048).expect("keygen");
    let pub_der = priv_key
        .to_public_key()
        .to_pkcs1_der()
        .expect("pub der")
        .as_bytes()
        .to_vec();
    let cert = build_cert(&rsa_spki(&pub_der), "zpdf RSA signer");
    let priv_der = priv_key.to_pkcs1_der().expect("priv der");
    let key = SigningKey::rsa_from_pkcs1_der(priv_der.as_bytes()).expect("key");

    let writer = IncrementalWriter::new(minimal_pdf()).expect("writer");
    let signed = writer
        .sign(&cert, &key, &SignatureOptions::default())
        .expect("sign");

    let doc = PdfDocument::open(signed).expect("open signed");
    let s = &doc.signatures()[0];
    assert_eq!(s.digest, DigestStatus::Verified);
    assert_eq!(s.crypto, CryptoStatus::Valid);
    assert_eq!(s.signature_algorithm.as_deref(), Some("RSA"));
    assert!(s.is_cryptographically_valid());
}

#[test]
fn tampering_after_signing_is_detected() {
    let (cert, key) = ecdsa_material();
    let writer = IncrementalWriter::new(minimal_pdf()).expect("writer");
    let mut signed = writer
        .sign(&cert, &key, &SignatureOptions::default())
        .expect("sign");

    // Flip a byte inside the original document region (covered by ByteRange).
    let at = signed
        .windows(8)
        .position(|w| w == b"MediaBox")
        .expect("MediaBox");
    signed[at] ^= 0x20;

    let doc = PdfDocument::open(signed).expect("open tampered");
    let s = &doc.signatures()[0];
    assert_eq!(
        s.digest,
        DigestStatus::Mismatch,
        "tampered bytes must fail the digest"
    );
    assert!(!s.is_cryptographically_valid());
}

#[test]
fn signing_preserves_prior_edits() {
    // Annotate then sign in one revision: both must survive.
    let mut writer = IncrementalWriter::new(minimal_pdf()).expect("writer");
    writer
        .add_annotation(
            0,
            &zpdf_writer::AnnotationSpec::Note {
                x: 30.0,
                y: 700.0,
                contents: "signed note".to_string(),
                color: None,
                icon: None,
            },
        )
        .expect("annotate");

    let (cert, key) = ecdsa_material();
    let signed = writer
        .sign(&cert, &key, &SignatureOptions::default())
        .expect("sign");

    let doc = PdfDocument::open(signed).expect("open");
    let page = doc.page(0).expect("page");
    // Note + signature widget.
    assert_eq!(doc.page_annotations(&page).len(), 2);
    let s = &doc.signatures()[0];
    assert_eq!(s.digest, DigestStatus::Verified);
    assert_eq!(s.crypto, CryptoStatus::Valid);
}

#[test]
fn empty_certificate_is_rejected() {
    let (_, key) = ecdsa_material();
    let writer = IncrementalWriter::new(minimal_pdf()).expect("writer");
    assert!(writer
        .sign(&[], &key, &SignatureOptions::default())
        .is_err());
}
