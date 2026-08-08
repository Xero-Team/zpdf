//! Production-grade signing tests: PAdES SubFilter, PEM key loading,
//! visible signature appearance, embedded extra-certs + /DSS, RFC 3161
//! timestamp (mock requester), and offline CRL revocation detection.

use zpdf_core::{ObjectId, PdfObject};
use zpdf_document::{CryptoStatus, PdfDocument, RevocationStatus};
use zpdf_writer::{
    AppearanceSpec, IncrementalWriter, SignatureOptions, SigningKey, SubFilter, TimestampRequester,
};

// ---- helpers copied from tests/sign.rs (minimal DER / X.509 builders) ----

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

const SEQ: u8 = 0x30;
const SET: u8 = 0x31;
const OID: u8 = 0x06;
const INT: u8 = 0x02;
const UTF8_STRING: u8 = 0x0c;
const NULL: u8 = 0x05;
const BIT_STRING: u8 = 0x03;
const UTC_TIME: u8 = 0x17;

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

fn ecdsa_material() -> (Vec<u8>, SigningKey) {
    let scalar = [0x42u8; 32];
    let key = SigningKey::ecdsa_p256_from_scalar(&scalar).expect("scalar");
    let sk = p256::ecdsa::SigningKey::from_slice(&scalar).unwrap();
    let point = p256::EncodedPoint::from(sk.verifying_key())
        .to_bytes()
        .to_vec();
    (build_cert(&ec_p256_spki(&point), "zpdf signer"), key)
}

/// A CRL (CertificateList) listing `serial` as revoked. Minimal but enough
/// for the verifier's `revocation` walker (which scans for the first SEQUENCE
/// after a Time and reads its children's serial INTEGERs).
fn crl_revoking(serial: &[u8]) -> Vec<u8> {
    let revoked_entry = der(
        SEQ,
        &[der(INT, serial), der(UTC_TIME, b"230101000000Z")].concat(),
    );
    let revoked = der(SEQ, &revoked_entry);
    let tbs = der(
        SEQ,
        &[
            der(SEQ, &[]),                   // signatureAlgorithm
            der(SEQ, &[]),                   // issuer
            der(UTC_TIME, b"230101000000Z"), // thisUpdate
            revoked,                         // revokedCertificates
        ]
        .concat(),
    );
    der(
        SEQ,
        &[tbs, der(SEQ, &[]), bit_string(&[0xde, 0xad])].concat(),
    )
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    let mut bits: u32 = 0;
    let mut count = 0u32;
    for &b in input {
        bits = (bits << 8) | b as u32;
        count += 8;
        while count >= 6 {
            count -= 6;
            out.push(TABLE[((bits >> count) & 0x3f) as usize] as char);
        }
    }
    if count > 0 {
        out.push(TABLE[((bits << (6 - count)) & 0x3f) as usize] as char);
    }
    while !out.len().is_multiple_of(4) {
        out.push('=');
    }
    out
}

fn one_sig(signed: &[u8]) -> zpdf_document::Signature {
    let doc = PdfDocument::open(signed.to_vec()).expect("open");
    let sigs = doc.signatures();
    assert_eq!(sigs.len(), 1, "expected exactly one signature");
    sigs.into_iter().next().unwrap()
}

#[test]
fn pades_cades_subfilter_roundtrip() {
    let (cert, key) = ecdsa_material();
    let signed = IncrementalWriter::new(minimal_pdf())
        .expect("writer")
        .sign(
            &cert,
            &key,
            &SignatureOptions {
                subfilter: SubFilter::EtsiCAdESDetached,
                name: Some("Alice".to_string()),
                ..Default::default()
            },
        )
        .expect("sign");
    let s = one_sig(&signed);
    assert_eq!(s.sub_filter.as_deref(), Some("ETSI.CAdES.detached"));
    assert_eq!(s.crypto, CryptoStatus::Valid, "PAdES CMS verifies");
}

#[test]
fn pem_key_loading_signs_and_verifies() {
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
    let pem = format!(
        "-----BEGIN RSA PRIVATE KEY-----\n{}\n-----END RSA PRIVATE KEY-----",
        base64_encode(priv_der.as_bytes())
    );
    let key = SigningKey::rsa_from_pkcs1_pem(pem.as_bytes()).expect("pem key");
    let signed = IncrementalWriter::new(minimal_pdf())
        .expect("writer")
        .sign(&cert, &key, &SignatureOptions::default())
        .expect("sign");
    let s = one_sig(&signed);
    assert_eq!(s.crypto, CryptoStatus::Valid, "PEM-loaded RSA key signs");
}

#[test]
fn visible_signature_appearance_emits_nonzero_rect_and_ap() {
    let (cert, key) = ecdsa_material();
    let signed = IncrementalWriter::new(minimal_pdf())
        .expect("writer")
        .sign(
            &cert,
            &key,
            &SignatureOptions {
                appearance: Some(AppearanceSpec {
                    show_text: true,
                    show_date: true,
                    ..Default::default()
                }),
                name: Some("Alice".to_string()),
                reason: Some("Approved".to_string()),
                ..Default::default()
            },
        )
        .expect("sign");
    // Find the widget annotation on page 0 and confirm it has a non-zero
    // /Rect and an /AP /N appearance XObject.
    let doc = PdfDocument::open(signed.to_vec()).expect("open");
    let file = doc.file();
    let page = doc.page(0).expect("page");
    let annots = file
        .resolve(page.id)
        .ok()
        .and_then(|o| o.as_dict().ok().cloned())
        .expect("page dict");
    let mut found_widget = false;
    if let Some(PdfObject::Array(a)) = annots.get("Annots") {
        for o in a {
            let PdfObject::Ref(r) = o else { continue };
            let Ok(PdfObject::Dict(d)) = file.resolve(*r) else {
                continue;
            };
            if d.get_name("Subtype").ok() == Some("Widget") && d.get("AP").is_some() {
                found_widget = true;
                if let Some(PdfObject::Array(rect)) = d.get("Rect") {
                    let nums: Vec<f64> = rect.iter().filter_map(|o| o.as_f64().ok()).collect();
                    assert_eq!(nums.len(), 4);
                    assert!(nums[2] > nums[0] && nums[3] > nums[1], "non-zero rect");
                } else {
                    panic!("no /Rect");
                }
                break;
            }
        }
    }
    assert!(found_widget, "visible widget with /AP not found");
}

#[test]
fn extra_certs_dss_and_crl_revocation_detected() {
    let (cert, key) = ecdsa_material();
    // A second (chain) cert and a CRL that revokes the signer's serial (01).
    let scalar = [0x99u8; 32];
    let sk2 = p256::ecdsa::SigningKey::from_slice(&scalar).unwrap();
    let point2 = p256::EncodedPoint::from(sk2.verifying_key())
        .to_bytes()
        .to_vec();
    let extra = build_cert(&ec_p256_spki(&point2), "zpdf intermediate");
    let crl = crl_revoking(&[0x01]); // signer serial is 01

    let signed = IncrementalWriter::new(minimal_pdf())
        .expect("writer")
        .sign(
            &cert,
            &key,
            &SignatureOptions {
                extra_certs: vec![extra],
                crls: vec![crl],
                ..Default::default()
            },
        )
        .expect("sign");
    let s = one_sig(&signed);
    assert_eq!(s.crypto, CryptoStatus::Valid);
    assert!(
        s.cert_count >= 2,
        "signer + extra cert embedded: {}",
        s.cert_count
    );
    assert!(s.has_dss, "/DSS written for extra_certs/crls");
    assert_eq!(
        s.revocation,
        RevocationStatus::Revoked,
        "signer serial 01 is on the embedded CRL"
    );
}

#[test]
fn crl_not_revoked_when_serial_absent() {
    let (cert, key) = ecdsa_material();
    // A CRL revoking a *different* serial — the signer (01) is not revoked.
    let crl = crl_revoking(&[0x77]);
    let signed = IncrementalWriter::new(minimal_pdf())
        .expect("writer")
        .sign(
            &cert,
            &key,
            &SignatureOptions {
                crls: vec![crl],
                ..Default::default()
            },
        )
        .expect("sign");
    let s = one_sig(&signed);
    assert_eq!(s.revocation, RevocationStatus::NotRevoked);
}

#[test]
fn no_crls_means_unknown_revocation() {
    let (cert, key) = ecdsa_material();
    let signed = IncrementalWriter::new(minimal_pdf())
        .expect("writer")
        .sign(&cert, &key, &SignatureOptions::default())
        .expect("sign");
    let s = one_sig(&signed);
    assert_eq!(s.revocation, RevocationStatus::Unknown);
}

/// A mock RFC 3161 requester returning a minimal TimeStampResp carrying a
/// ContentInfo timestamp token. The token's content is irrelevant to the
/// verifier's `has_timestamp` (which scans the signer CMS for the timestamp
/// attribute OID); it only needs to be a parseable token.
struct MockTsa;
impl TimestampRequester for MockTsa {
    fn request_timestamp(&self, _tsq: &[u8]) -> zpdf_core::Result<Vec<u8>> {
        // TimeStampResp ::= SEQUENCE { status PKIStatusInfo, timeStampToken? }
        let status = der(SEQ, &[]);
        let token = der(SEQ, &[der(OID, &[0x2a])].concat()); // a minimal ContentInfo
        Ok(der(SEQ, &[status, token].concat()))
    }
}

#[test]
fn rfc3161_timestamp_embedded_via_requester() {
    let (cert, key) = ecdsa_material();
    let signed = IncrementalWriter::new(minimal_pdf())
        .expect("writer")
        .sign(
            &cert,
            &key,
            &SignatureOptions {
                timestamp: Some(Box::new(MockTsa)),
                ..Default::default()
            },
        )
        .expect("sign");
    let s = one_sig(&signed);
    assert_eq!(s.crypto, CryptoStatus::Valid);
    assert!(
        s.has_timestamp,
        "CMS should carry a signature-time-stamp unsigned attribute"
    );
}

// Keep the ObjectId import used by the visible-appearance walk compiled.
#[allow(dead_code)]
fn _use_objectid() -> ObjectId {
    ObjectId(0, 0)
}
