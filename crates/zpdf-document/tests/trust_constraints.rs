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
    // identifies the first certificate, whose X.509 signatures are all real.
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
fn verify(
    intermediate_extensions: &[Vec<u8>],
    root_limit: Option<u8>,
    leaf_extensions: &[Vec<u8>],
) -> ChainStatus {
    let root_key = key(1);
    let inter_key = key(2);
    let leaf_key = key(3);
    let root = cert(
        "Root",
        &root_key,
        "Root",
        &root_key,
        &[basic(true, root_limit), usage(4)],
    );
    let intermediate = cert(
        "Intermediate",
        &inter_key,
        "Root",
        &root_key,
        intermediate_extensions,
    );
    let leaf = cert(
        "Leaf",
        &leaf_key,
        "Intermediate",
        &inter_key,
        leaf_extensions,
    );
    verify_certificate_chain(
        &cms(&[leaf, intermediate], "Intermediate"),
        &parse_trust_anchors(&root),
        Some(1_800_000_000),
    )
}
#[test]
fn valid_ca_chain_is_trusted() {
    assert!(matches!(
        verify(&[basic(true, Some(0)), usage(4)], Some(1), &[]),
        ChainStatus::Trusted(_)
    ));
}
#[test]
fn non_ca_issuer_and_missing_ca_assertion_are_rejected() {
    for extensions in [vec![basic(false, None), usage(0x80)], vec![usage(4)]] {
        assert!(matches!(
            verify(&extensions, None, &[]),
            ChainStatus::Untrusted(_)
        ));
    }
}
#[test]
fn issuer_key_usage_must_allow_signing_certificates() {
    assert!(matches!(
        verify(&[basic(true, None), usage(0x80)], None, &[]),
        ChainStatus::Untrusted(_)
    ));
}
#[test]
fn root_path_length_bounds_intermediate_authorities() {
    assert!(matches!(
        verify(&[basic(true, None), usage(4)], Some(0), &[]),
        ChainStatus::Untrusted(_)
    ));
}
#[test]
fn unknown_critical_extensions_fail_closed() {
    let critical = extension(99, true, &der(5, &[]));
    assert!(matches!(
        verify(&[basic(true, None)], None, &[critical]),
        ChainStatus::Unsupported(_)
    ));
    let optional = extension(99, false, &der(5, &[]));
    assert!(matches!(
        verify(&[basic(true, None)], None, &[optional]),
        ChainStatus::Trusted(_)
    ));
}
#[test]
fn malformed_and_duplicate_constraints_are_rejected() {
    for extensions in [
        vec![basic(true, None), basic(true, None)],
        vec![extension(19, true, &[0x30, 2, 1])],
        vec![basic(false, Some(1))],
        vec![basic(true, None), extension(15, true, &der(3, &[8, 4]))],
        vec![basic(true, None), extension(15, true, &der(3, &[7, 4]))],
        vec![extension(19, true, &seq(&[der(1, &[255]), der(2, &[255])]))],
    ] {
        assert!(matches!(
            verify(&extensions, None, &[]),
            ChainStatus::Untrusted(_)
        ));
    }
}
#[test]
fn explicitly_trusted_signer_need_not_be_a_ca() {
    let k = key(3);
    let leaf = cert("Leaf", &k, "Leaf", &k, &[basic(false, None), usage(0x80)]);
    let anchors = parse_trust_anchors(&leaf);
    assert!(matches!(
        verify_certificate_chain(&cms(&[leaf], "Leaf"), &anchors, Some(1_800_000_000)),
        ChainStatus::Trusted(_)
    ));
}

#[test]
fn malformed_certificate_set_tail_fails_closed() {
    let root_key = key(1);
    let leaf_key = key(2);
    let root = cert(
        "Root",
        &root_key,
        "Root",
        &root_key,
        &[basic(true, None), usage(4)],
    );
    let leaf = cert("Leaf", &leaf_key, "Root", &root_key, &[]);
    let anchors = parse_trust_anchors(&root);
    // Truncated DER and unsupported CertificateChoices must both fail closed.
    for tail in [vec![0x30, 0x80], der(0xa2, &seq(&[]))] {
        let blob = cms(&[leaf.clone(), tail], "Root");
        assert!(matches!(
            verify_certificate_chain(&blob, &anchors, Some(1_800_000_000)),
            ChainStatus::Unsupported(_)
        ));
    }
}

#[test]
fn certificate_count_limit_does_not_accept_a_truncated_set() {
    let root_key = key(1);
    let inter_key = key(2);
    let leaf_key = key(3);
    let root = cert(
        "Root",
        &root_key,
        "Root",
        &root_key,
        &[basic(true, None), usage(4)],
    );
    let intermediate = cert(
        "Intermediate",
        &inter_key,
        "Root",
        &root_key,
        &[basic(true, None), usage(4)],
    );
    let leaf = cert("Leaf", &leaf_key, "Intermediate", &inter_key, &[]);
    let mut certs = vec![leaf, intermediate];
    certs.extend(std::iter::repeat_n(root.clone(), 30));
    let anchors = parse_trust_anchors(&root);
    assert!(matches!(
        verify_certificate_chain(&cms(&certs, "Intermediate"), &anchors, Some(1_800_000_000)),
        ChainStatus::Trusted(_)
    ));
    certs.push(root);
    assert!(matches!(
        verify_certificate_chain(&cms(&certs, "Intermediate"), &anchors, Some(1_800_000_000)),
        ChainStatus::Unsupported(_)
    ));
}

#[test]
fn intermediate_path_length_excludes_the_end_entity() {
    let root_key = key(1);
    let upper_key = key(2);
    let lower_key = key(3);
    let leaf_key = key(4);
    let root = cert(
        "Root",
        &root_key,
        "Root",
        &root_key,
        &[basic(true, Some(2)), usage(4)],
    );
    let lower = cert(
        "Lower",
        &lower_key,
        "Upper",
        &upper_key,
        &[basic(true, Some(0)), usage(4)],
    );
    let leaf = cert("Leaf", &leaf_key, "Lower", &lower_key, &[]);
    let anchors = parse_trust_anchors(&root);
    for limit in [0, 1] {
        let upper = cert(
            "Upper",
            &upper_key,
            "Root",
            &root_key,
            &[basic(true, Some(limit)), usage(4)],
        );
        let status = verify_certificate_chain(
            &cms(&[leaf.clone(), lower.clone(), upper], "Lower"),
            &anchors,
            Some(1_800_000_000),
        );
        if limit == 0 {
            assert!(
                matches!(status, ChainStatus::Untrusted(ref reason) if reason.contains("path length"))
            );
        } else {
            assert!(matches!(status, ChainStatus::Trusted(_)));
        }
    }
}
