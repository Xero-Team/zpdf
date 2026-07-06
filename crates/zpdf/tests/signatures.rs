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
use zpdf::{CryptoStatus, DigestStatus, PdfDocument};

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

// ---- Cryptographic (public-key) verification ------------------------------
//
// These tests go beyond the byte-range digest: they build a real signer key
// pair, embed a minimal X.509 certificate carrying its public key, sign the CMS
// signed attributes with the private key, and assert the [`CryptoStatus`]
// verdict from verifying that signature against the embedded certificate.

const UTF8_STRING: u8 = 0x0c;
const NULL: u8 = 0x05;
const BIT_STRING: u8 = 0x03;
const CTX0_TAG: u8 = 0xA0;

/// Wrap DER bytes in a BIT STRING with zero unused trailing bits.
fn bit_string(body: &[u8]) -> Vec<u8> {
    let mut v = vec![0x00];
    v.extend_from_slice(body);
    der(BIT_STRING, &v)
}

/// A minimal X.509 `Certificate` whose `SubjectPublicKeyInfo` is `spki` and
/// whose subject carries the given Common Name. The signature-algorithm and
/// signature fields are placeholders — the verifier only reads the SPKI and CN.
fn build_cert(spki: &[u8], cn: &str) -> Vec<u8> {
    let cn_oid = [0x55, 0x04, 0x03]; // 2.5.4.3
    let atv = der(
        SEQ,
        &[der(OID, &cn_oid), der(UTF8_STRING, cn.as_bytes())].concat(),
    );
    let subject = der(SEQ, &der(SET, &atv));

    // TBSCertificate: serial INT, then five SEQUENCEs — the verifier filters to
    // SEQUENCEs and takes the 4th (subject) and 5th (SPKI).
    let tbs = der(
        SEQ,
        &[
            der(INT, &[0x01]),            // serialNumber
            der(SEQ, &der(OID, &[0x2a])), // signature AlgorithmIdentifier (dummy)
            der(SEQ, &[]),                // issuer (dummy)
            der(SEQ, &[]),                // validity (dummy)
            subject,                      // subject (carries CN)
            spki.to_vec(),                // subjectPublicKeyInfo
        ]
        .concat(),
    );
    der(
        SEQ,
        &[tbs, der(SEQ, &der(OID, &[0x2a])), bit_string(&[0xde, 0xad])].concat(),
    )
}

/// SubjectPublicKeyInfo for an EC P-256 public point (SEC1 uncompressed).
fn ec_p256_spki(point: &[u8]) -> Vec<u8> {
    let ec_pub_oid = [0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01]; // id-ecPublicKey
    let p256_oid = [0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07]; // prime256v1
    let alg = der(SEQ, &[der(OID, &ec_pub_oid), der(OID, &p256_oid)].concat());
    der(SEQ, &[alg, bit_string(point)].concat())
}

/// SubjectPublicKeyInfo for an RSA public key (`RSAPublicKey` PKCS#1 DER).
fn rsa_spki(pkcs1_pub_der: &[u8]) -> Vec<u8> {
    let rsa_oid = [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01]; // rsaEncryption
    let alg = der(SEQ, &[der(OID, &rsa_oid), der(NULL, &[])].concat());
    der(SEQ, &[alg, bit_string(pkcs1_pub_der)].concat())
}

/// Build a full CMS `SignedData` carrying `digest` as the messageDigest signed
/// attribute, the certificate `cert`, and a real `signature` over the SET-encoded
/// signed attributes produced by `sign`. `sig_alg_oid` names the signature
/// algorithm in the `SignerInfo`.
fn build_real_cms(
    digest: &[u8],
    cert: &[u8],
    sig_alg_oid: &[u8],
    sign: impl Fn(&[u8]) -> Vec<u8>,
) -> Vec<u8> {
    let sha256_oid = [0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01];
    let md_oid = [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x09, 0x04];
    let data_oid = [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x07, 0x01];
    let signed_data_oid = [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x07, 0x02];

    let digest_alg = der(SEQ, &der(OID, &sha256_oid));
    let md_attr = der(
        SEQ,
        &[der(OID, &md_oid), der(SET, &der(OCTET, digest))].concat(),
    );
    // The signature covers the SET-encoded attributes; the CMS stores the
    // [0] IMPLICIT-tagged form.
    let signature = sign(&der(SET, &md_attr));

    let signer_info = der(
        SEQ,
        &[
            der(INT, &[1]),
            der(SEQ, &[]), // sid placeholder (empty SEQUENCE, skipped by OID scan)
            digest_alg.clone(),
            der(CTX0_TAG, &md_attr), // signedAttrs [0] IMPLICIT
            der(SEQ, &der(OID, sig_alg_oid)),
            der(OCTET, &signature),
        ]
        .concat(),
    );
    let signed_data = der(
        SEQ,
        &[
            der(INT, &[1]),
            der(SET, &digest_alg),
            der(SEQ, &der(OID, &data_oid)),
            der(CTX0_TAG, cert), // certificates [0] IMPLICIT
            der(SET, &signer_info),
        ]
        .concat(),
    );
    der(
        SEQ,
        &[der(OID, &signed_data_oid), der(CTX0_TAG, &signed_data)].concat(),
    )
}

/// Assemble a signed PDF using a fixed, generously-sized `/Contents` window: the
/// real CMS hex is written at the front and the remainder is left as zero
/// padding (which the DER reader ignores). This decouples the `/Contents` size
/// from the variable-length CMS, so the `/ByteRange` is stable regardless of the
/// signature algorithm. `build_cms` receives the true digest of the signed spans.
fn assemble_signed_pdf(build_cms: impl Fn(&[u8]) -> Vec<u8>) -> Vec<u8> {
    const RESERVED_HEX: usize = 8192; // 4096 bytes of /Contents room
    let contents_placeholder = "0".repeat(RESERVED_HEX);
    let br_placeholder = "[__________ __________ __________ __________]";
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

    let contents_lt = find(&buf, b"/Contents <") + "/Contents ".len();
    let contents_start = contents_lt; // offset of `<`
    let contents_end = contents_lt + 1 + RESERVED_HEX + 1; // just past `>`
    assert_eq!(buf[contents_end - 1], b'>');

    // Patch /ByteRange, then digest the finalized signed spans.
    let br = [
        0usize,
        contents_start,
        contents_end,
        buf.len() - contents_end,
    ];
    let br_at = find(&buf, br_placeholder.as_bytes());
    for (i, &v) in br.iter().enumerate() {
        let slot = br_at + 1 + i * 11;
        buf[slot..slot + 10].copy_from_slice(format!("{v:>10}").as_bytes());
    }

    let mut hasher = Sha256::new();
    hasher.update(&buf[0..contents_start]);
    hasher.update(&buf[contents_end..]);
    let digest = hasher.finalize();

    let cms = build_cms(&digest);
    let hex = to_hex(&cms);
    assert!(
        hex.len() <= RESERVED_HEX,
        "CMS too large for reserved window"
    );
    buf[contents_lt + 1..contents_lt + 1 + hex.len()].copy_from_slice(&hex);

    buf
}

/// A deterministic ECDSA P-256 signer plus its certificate.
fn ecdsa_p256_signer() -> (Vec<u8>, impl Fn(&[u8]) -> Vec<u8>) {
    use p256::ecdsa::signature::hazmat::PrehashSigner;
    use p256::ecdsa::{Signature, SigningKey};
    use p256::EncodedPoint;

    let scalar = [0x11u8; 32];
    let sk = SigningKey::from_slice(&scalar).expect("signing key");
    let point = EncodedPoint::from(sk.verifying_key()).to_bytes().to_vec();
    let cert = build_cert(&ec_p256_spki(&point), "zpdf ECDSA Test");

    let sign = move |msg: &[u8]| {
        let h = Sha256::digest(msg);
        let sig: Signature = sk.sign_prehash(&h).expect("sign");
        sig.to_der().as_bytes().to_vec()
    };
    (cert, sign)
}

/// A deterministic RSA-2048 signer plus its certificate.
fn rsa_signer() -> (Vec<u8>, impl Fn(&[u8]) -> Vec<u8>) {
    use rand_chacha::rand_core::SeedableRng;
    use rand_chacha::ChaCha20Rng;
    use rsa::pkcs1::EncodeRsaPublicKey;
    use rsa::pkcs1v15::SigningKey;
    use rsa::signature::{SignatureEncoding, Signer};
    use rsa::RsaPrivateKey;

    let mut rng = ChaCha20Rng::from_seed([7u8; 32]);
    let priv_key = RsaPrivateKey::new(&mut rng, 2048).expect("rsa keygen");
    let pub_der = priv_key
        .to_public_key()
        .to_pkcs1_der()
        .expect("pkcs1 der")
        .as_bytes()
        .to_vec();
    let cert = build_cert(&rsa_spki(&pub_der), "zpdf RSA Test");

    let signing_key = SigningKey::<Sha256>::new(priv_key);
    let sign = move |msg: &[u8]| signing_key.sign(msg).to_vec();
    (cert, sign)
}

#[test]
fn ecdsa_p256_signature_verifies() {
    let (cert, sign) = ecdsa_p256_signer();
    // ecdsa-with-SHA256: 1.2.840.10045.4.3.2
    let sig_oid = [0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02];
    let pdf = assemble_signed_pdf(|d| build_real_cms(d, &cert, &sig_oid, &sign));

    let doc = PdfDocument::open(pdf).expect("open");
    let s = &doc.signatures()[0];
    assert_eq!(s.digest, DigestStatus::Verified);
    assert_eq!(s.crypto, CryptoStatus::Valid, "ECDSA signature must verify");
    assert_eq!(s.signature_algorithm.as_deref(), Some("ECDSA (P-256)"));
    assert_eq!(s.signer_common_name.as_deref(), Some("zpdf ECDSA Test"));
    assert!(s.is_cryptographically_valid());
}

#[test]
fn rsa_signature_verifies() {
    let (cert, sign) = rsa_signer();
    // sha256WithRSAEncryption: 1.2.840.113549.1.1.11
    let sig_oid = [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b];
    let pdf = assemble_signed_pdf(|d| build_real_cms(d, &cert, &sig_oid, &sign));

    let doc = PdfDocument::open(pdf).expect("open");
    let s = &doc.signatures()[0];
    assert_eq!(s.digest, DigestStatus::Verified);
    assert_eq!(s.crypto, CryptoStatus::Valid, "RSA signature must verify");
    assert_eq!(s.signature_algorithm.as_deref(), Some("RSA"));
    assert_eq!(s.signer_common_name.as_deref(), Some("zpdf RSA Test"));
    assert!(s.is_cryptographically_valid());
}

#[test]
fn corrupted_signature_is_detected_as_invalid() {
    let (cert, sign) = ecdsa_p256_signer();
    let sig_oid = [0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02];
    // Flip the last byte of the CMS — inside the signature value, which lives in
    // the (unsigned) /Contents gap, so the byte-range digest stays intact but the
    // public-key signature no longer verifies.
    let pdf = assemble_signed_pdf(|d| {
        let mut cms = build_real_cms(d, &cert, &sig_oid, &sign);
        let last = cms.len() - 1;
        cms[last] ^= 0xff;
        cms
    });

    let doc = PdfDocument::open(pdf).expect("open");
    let s = &doc.signatures()[0];
    assert_eq!(s.digest, DigestStatus::Verified, "signed bytes untouched");
    assert_eq!(
        s.crypto,
        CryptoStatus::Invalid,
        "a corrupted signature must not verify"
    );
    assert!(!s.is_cryptographically_valid());
}

#[test]
fn tampered_body_fails_both_checks() {
    let (cert, sign) = ecdsa_p256_signer();
    let sig_oid = [0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02];
    let mut pdf = assemble_signed_pdf(|d| build_real_cms(d, &cert, &sig_oid, &sign));
    // Alter a signed byte: the digest breaks (Mismatch); the crypto signature
    // over the attributes is still internally consistent, but the overall
    // signature is not valid because the document no longer matches.
    let at = find(&pdf, b"/MediaBox [0 0 200 200]") + "/MediaBox [0 0 ".len();
    pdf[at] = b'9';

    let doc = PdfDocument::open(pdf).expect("open");
    let s = &doc.signatures()[0];
    assert_eq!(s.digest, DigestStatus::Mismatch);
    assert!(
        !s.is_cryptographically_valid(),
        "a tampered document is never cryptographically valid"
    );
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
