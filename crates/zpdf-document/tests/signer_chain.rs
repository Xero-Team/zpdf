//! Synthetic, deterministically signed X.509 chains. No external keys or tools.
use p256::ecdsa::signature::hazmat::PrehashSigner;
use p256::ecdsa::{Signature, SigningKey};
use sha2::{Digest, Sha256};
use zpdf_document::trust::{parse_trust_anchors, verify_certificate_chain, ChainStatus};

fn der(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    if body.len() < 128 {
        out.push(body.len() as u8);
    } else {
        let bytes = (body.len() as u32).to_be_bytes();
        let start = bytes.iter().position(|b| *b != 0).unwrap();
        out.push(0x80 | (4 - start) as u8);
        out.extend_from_slice(&bytes[start..]);
    }
    out.extend_from_slice(body);
    out
}
fn seq(parts: &[Vec<u8>]) -> Vec<u8> {
    der(0x30, &parts.concat())
}
fn oid(value: &[u8]) -> Vec<u8> {
    der(6, value)
}
fn name(value: &str) -> Vec<u8> {
    seq(&[der(
        0x31,
        &seq(&[oid(&[0x55, 4, 3]), der(12, value.as_bytes())]),
    )])
}
fn algorithm() -> Vec<u8> {
    seq(&[oid(&[0x2a, 0x86, 0x48, 0xce, 0x3d, 4, 3, 2])])
}
fn key(byte: u8) -> SigningKey {
    SigningKey::from_slice(&[byte; 32]).unwrap()
}
fn extension(id: u8, critical: bool, value: &[u8]) -> Vec<u8> {
    seq(&[
        oid(&[0x55, 0x1d, id]),
        der(1, &[if critical { 255 } else { 0 }]),
        der(4, value),
    ])
}
fn basic(ca: bool, limit: Option<u8>) -> Vec<u8> {
    let mut fields = vec![der(1, &[if ca { 255 } else { 0 }])];
    if let Some(limit) = limit {
        fields.push(der(2, &[limit]));
    }
    extension(19, true, &seq(&fields))
}
fn usage(bits: u8) -> Vec<u8> {
    extension(15, true, &der(3, &[bits.trailing_zeros() as u8, bits]))
}
fn cert(
    subject: &str,
    subject_key: &SigningKey,
    issuer: &str,
    signer: &SigningKey,
    extensions: &[Vec<u8>],
) -> Vec<u8> {
    let public = p256::Sec1Point::from(subject_key.verifying_key()).to_bytes();
    let spki = seq(&[
        seq(&[
            oid(&[0x2a, 0x86, 0x48, 0xce, 0x3d, 2, 1]),
            oid(&[0x2a, 0x86, 0x48, 0xce, 0x3d, 3, 1, 7]),
        ]),
        der(3, &[&[0][..], public.as_ref()].concat()),
    ]);
    let tbs = seq(&[
        der(0xa0, &der(2, &[2])),
        der(2, &[1]),
        algorithm(),
        name(issuer),
        seq(&[der(0x17, b"260101000000Z"), der(0x17, b"300101000000Z")]),
        name(subject),
        spki,
        der(0xa3, &seq(extensions)),
    ]);
    let signature: Signature = signer.sign_prehash(&Sha256::digest(&tbs)).unwrap();
    seq(&[
        tbs,
        algorithm(),
        der(3, &[&[0][..], signature.to_der().as_bytes()].concat()),
    ])
}
fn cms(certs: &[Vec<u8>], issuer: &str) -> Vec<u8> {
    let hash = seq(&[oid(&[0x60, 0x86, 0x48, 1, 0x65, 3, 4, 2, 1])]);
    // Chain validation is independent of the CMS document signature. The sid
    // identifies the signer by issuer and serial; X.509 signatures are real.
    let signer = seq(&[
        der(2, &[1]),
        seq(&[name(issuer), der(2, &[1])]),
        hash.clone(),
        algorithm(),
        der(4, &[]),
    ]);
    seq(&[
        oid(&[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 1, 7, 2]),
        der(
            0xa0,
            &seq(&[
                der(2, &[1]),
                der(0x31, &hash),
                seq(&[oid(&[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 1, 7, 1])]),
                der(0xa0, &certs.concat()),
                der(0x31, &signer),
            ]),
        ),
    ])
}
#[test]
fn chain_starts_from_sid_match_even_when_issuer_is_first() {
    let root = cert(
        "Root",
        &key(1),
        "Root",
        &key(1),
        &[basic(true, None), usage(4)],
    );
    let intermediate = cert(
        "Intermediate",
        &key(2),
        "Root",
        &key(1),
        &[basic(true, None), usage(4)],
    );
    let leaf = cert("Leaf", &key(3), "Intermediate", &key(2), &[]);
    let anchors = parse_trust_anchors(&root);
    let bag = [intermediate, leaf];
    let status = verify_certificate_chain(&cms(&bag, "Intermediate"), &anchors, None);
    assert!(
        matches!(status,ChainStatus::Trusted(names) if names == ["Leaf","Intermediate","Root"])
    );
    assert!(matches!(
        verify_certificate_chain(&cms(&bag, "Missing"), &anchors, None),
        ChainStatus::Unsupported(_)
    ));
}
