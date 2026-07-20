//! X.509 certificate-chain verification for PDF signatures, against
//! caller-provided trust anchors.
//!
//! [`crate::signature`] answers "are the bytes intact and signed by the key in
//! the embedded certificate?". This module answers the follow-up: **"does that
//! certificate chain to a root I trust?"** — chain building (subject/issuer
//! matching over the CMS `certificates` set), per-link signature verification
//! (RSA PKCS#1 v1.5 and ECDSA P-256/P-384, SHA-1/256/384/512), validity-period
//! checks, and anchoring at a caller-supplied root set (PEM or DER).
//!
//! Deliberately out of scope: revocation (CRL/OCSP fetching needs a network),
//! name constraints, policy mapping, and RSA-PSS.

use sha2::Digest;

/// A trusted root certificate.
pub struct TrustAnchor {
    cert: CertInfo,
}

/// Verdict of a chain verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainStatus {
    /// Every link verified and the chain terminates at a provided anchor.
    /// Carries the subject CNs from leaf to anchor.
    Trusted(Vec<String>),
    /// The chain is structurally complete but fails verification — a broken
    /// signature, an expired certificate, or no path to any anchor.
    Untrusted(String),
    /// The chain could not be evaluated (no certificates, unparseable
    /// certificate, unsupported algorithm).
    Unsupported(String),
}

impl ChainStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ChainStatus::Trusted(_) => "trusted",
            ChainStatus::Untrusted(_) => "untrusted",
            ChainStatus::Unsupported(_) => "unsupported",
        }
    }
}

/// Parse trust anchors from PEM (`-----BEGIN CERTIFICATE-----` blocks) or,
/// when no PEM markers are found, a single DER certificate.
pub fn parse_trust_anchors(data: &[u8]) -> Vec<TrustAnchor> {
    let mut out = Vec::new();
    let text = String::from_utf8_lossy(data);
    let mut found_pem = false;
    let mut collecting = false;
    let mut b64 = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("-----BEGIN CERTIFICATE") {
            collecting = true;
            found_pem = true;
            b64.clear();
        } else if line.starts_with("-----END CERTIFICATE") {
            collecting = false;
            if let Some(der) = base64_decode(&b64) {
                if let Some(cert) = CertInfo::parse(&der) {
                    out.push(TrustAnchor { cert });
                }
            }
        } else if collecting {
            b64.push_str(line);
        }
    }
    if !found_pem {
        if let Some(cert) = CertInfo::parse(data) {
            out.push(TrustAnchor { cert });
        }
    }
    out
}

/// Verify the certificate chain embedded in a CMS `SignedData` blob (a PDF
/// signature's `/Contents`) against `anchors`.
///
/// `at_seconds_since_epoch` is the validation time (e.g. `SystemTime::now()`);
/// pass `None` to skip validity-period checks.
pub fn verify_certificate_chain(
    cms_blob: &[u8],
    anchors: &[TrustAnchor],
    at_seconds_since_epoch: Option<u64>,
) -> ChainStatus {
    if anchors.is_empty() {
        return ChainStatus::Unsupported("no trust anchors provided".into());
    }
    let certs = match extract_cms_certificates(cms_blob) {
        Some(c) if !c.is_empty() => c,
        _ => return ChainStatus::Unsupported("no certificates in signature".into()),
    };
    let parsed: Vec<CertInfo> = certs.iter().filter_map(|d| CertInfo::parse(d)).collect();
    if parsed.is_empty() {
        return ChainStatus::Unsupported("certificates could not be parsed".into());
    }

    // The leaf is the first certificate (the same convention the signature
    // verifier uses for the signer key).
    let mut chain: Vec<&CertInfo> = vec![&parsed[0]];
    let mut names = vec![parsed[0].subject_cn.clone().unwrap_or_default()];

    const MAX_CHAIN: usize = 16;
    loop {
        if chain.len() > MAX_CHAIN {
            return ChainStatus::Untrusted("chain too long".into());
        }
        let current = *chain.last().expect("nonempty");

        // Validity window.
        if let Some(now) = at_seconds_since_epoch {
            if let Some((nb, na)) = current.validity {
                if now < nb {
                    return ChainStatus::Untrusted(format!(
                        "certificate '{}' not yet valid",
                        current.subject_cn.as_deref().unwrap_or("?")
                    ));
                }
                if now > na {
                    return ChainStatus::Untrusted(format!(
                        "certificate '{}' expired",
                        current.subject_cn.as_deref().unwrap_or("?")
                    ));
                }
            }
        }

        // Anchored? (issuer matches an anchor's subject and the anchor key
        // verifies this cert — or the cert IS an anchor byte-for-byte.)
        for anchor in anchors {
            if anchor.cert.subject_raw == current.subject_raw
                && anchor.cert.spki_raw == current.spki_raw
            {
                return ChainStatus::Trusted(names);
            }
            if anchor.cert.subject_raw == current.issuer_raw {
                match verify_cert_signature(current, &anchor.cert) {
                    Some(true) => {
                        names.push(anchor.cert.subject_cn.clone().unwrap_or_default());
                        return ChainStatus::Trusted(names);
                    }
                    Some(false) => {
                        return ChainStatus::Untrusted(format!(
                            "signature of '{}' does not verify against anchor",
                            current.subject_cn.as_deref().unwrap_or("?")
                        ));
                    }
                    None => {} // unsupported algorithm; try other paths
                }
            }
        }

        // Otherwise find the issuer among the embedded certificates.
        let next = parsed.iter().find(|c| {
            c.subject_raw == current.issuer_raw
                && !std::ptr::eq(*c, current)
                && !chain.iter().any(|seen| std::ptr::eq(*seen, *c))
        });
        match next {
            Some(issuer) => match verify_cert_signature(current, issuer) {
                Some(true) => {
                    names.push(issuer.subject_cn.clone().unwrap_or_default());
                    chain.push(issuer);
                }
                Some(false) => {
                    return ChainStatus::Untrusted(format!(
                        "signature of '{}' does not verify against its issuer",
                        current.subject_cn.as_deref().unwrap_or("?")
                    ));
                }
                None => {
                    return ChainStatus::Unsupported(format!(
                        "unsupported signature algorithm in chain at '{}'",
                        current.subject_cn.as_deref().unwrap_or("?")
                    ));
                }
            },
            None => {
                return ChainStatus::Untrusted(format!(
                    "no path to a trust anchor from '{}'",
                    names.last().map(String::as_str).unwrap_or("?")
                ));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// X.509 parsing (minimal, hand-written DER — mirrors signature.rs's approach)
// ---------------------------------------------------------------------------

const SEQUENCE: u8 = 0x30;
const SET: u8 = 0x31;
const OID: u8 = 0x06;
const BIT_STRING: u8 = 0x03;
const UTC_TIME: u8 = 0x17;
const GENERALIZED_TIME: u8 = 0x18;
const CONTEXT_0: u8 = 0xA0;

const OID_CN: &[u8] = &[0x55, 0x04, 0x03];
const OID_SIGNED_DATA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x07, 0x02];
const OID_RSA_PREFIX: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01];
const OID_ECDSA_PREFIX: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04];
const OID_EC_PUBLIC_KEY: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
const OID_RSA_PUBLIC_KEY: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
const OID_CURVE_P256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
const OID_CURVE_P384: &[u8] = &[0x2b, 0x81, 0x04, 0x00, 0x22];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HashAlg {
    Sha1,
    Sha256,
    Sha384,
    Sha512,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChainKeyAlg {
    Rsa,
    EcP256,
    EcP384,
}

/// The parts of one parsed certificate needed for chain verification.
struct CertInfo {
    /// Full DER of the `tbsCertificate` element (tag+len+content) — the bytes
    /// the issuer signed.
    tbs_raw: Vec<u8>,
    /// Raw content bytes of the subject / issuer `Name` (compared for equality).
    subject_raw: Vec<u8>,
    issuer_raw: Vec<u8>,
    subject_cn: Option<String>,
    /// (notBefore, notAfter) as seconds since the Unix epoch.
    validity: Option<(u64, u64)>,
    /// Raw SPKI element (for anchor identity comparison).
    spki_raw: Vec<u8>,
    key_alg: Option<ChainKeyAlg>,
    /// RSA: PKCS#1 RSAPublicKey DER. EC: SEC1 point.
    key_bytes: Vec<u8>,
    /// The certificate's signatureAlgorithm → hash, and the signature bits.
    sig_hash: Option<HashAlg>,
    sig_is_ecdsa: bool,
    signature: Vec<u8>,
}

impl CertInfo {
    fn parse(der: &[u8]) -> Option<CertInfo> {
        // Certificate ::= SEQUENCE { tbsCertificate, signatureAlgorithm, signature }
        let (tag, cert, _) = tlv(der)?;
        if tag != SEQUENCE {
            return None;
        }
        let parts = children_raw(cert, 4);
        if parts.len() < 3 {
            return None;
        }
        let (tbs_tag, tbs, tbs_raw) = parts[0];
        if tbs_tag != SEQUENCE {
            return None;
        }
        let (alg_tag, alg, _) = parts[1];
        if alg_tag != SEQUENCE {
            return None;
        }
        let (sig_tag, sig_bits, _) = parts[2];
        if sig_tag != BIT_STRING {
            return None;
        }
        let signature = sig_bits
            .split_first()
            .and_then(|(unused, rest)| (*unused == 0).then(|| rest.to_vec()))?;

        // signatureAlgorithm → hash + family.
        let alg_oid = children(alg, 2)
            .into_iter()
            .find(|(t, _)| *t == OID)?
            .1
            .to_vec();
        let (sig_hash, sig_is_ecdsa) = classify_sig_alg(&alg_oid);

        // TBSCertificate ::= SEQUENCE { version [0]?, serialNumber, signature,
        //   issuer Name, validity, subject Name, subjectPublicKeyInfo, ... }
        // The SEQUENCEs in order (skipping [0] version and INTEGER serial):
        //   0: signature AlgorithmIdentifier
        //   1: issuer Name
        //   2: validity
        //   3: subject Name
        //   4: subjectPublicKeyInfo
        let tbs_children = children_raw(tbs, 16);
        let seqs: Vec<(u8, &[u8], &[u8])> = tbs_children
            .iter()
            .filter(|(t, _, _)| *t == SEQUENCE)
            .copied()
            .collect();
        if seqs.len() < 5 {
            return None;
        }
        let issuer_raw = seqs[1].1.to_vec();
        let validity_body = seqs[2].1;
        let subject_raw = seqs[3].1.to_vec();
        let subject_cn = name_cn(seqs[3].1);
        let spki = seqs[4];

        // validity ::= SEQUENCE { notBefore Time, notAfter Time }
        let times = children(validity_body, 2);
        let validity = match (times.first(), times.get(1)) {
            (Some(&(t1, v1)), Some(&(t2, v2))) => match (parse_time(t1, v1), parse_time(t2, v2)) {
                (Some(nb), Some(na)) => Some((nb, na)),
                _ => None,
            },
            _ => None,
        };

        // SubjectPublicKeyInfo.
        let spki_parts = children(spki.1, 2);
        let key_alg_body = spki_parts.iter().find(|(t, _)| *t == SEQUENCE)?.1;
        let key_bits = spki_parts.iter().find(|(t, _)| *t == BIT_STRING)?.1;
        let key_bytes = key_bits
            .split_first()
            .and_then(|(unused, rest)| (*unused == 0).then(|| rest.to_vec()))?;
        let alg_children = children(key_alg_body, 2);
        let key_oid = alg_children.iter().find(|(t, _)| *t == OID)?.1;
        let key_alg = if key_oid == OID_RSA_PUBLIC_KEY {
            Some(ChainKeyAlg::Rsa)
        } else if key_oid == OID_EC_PUBLIC_KEY {
            // Curve is the second OID.
            match alg_children.iter().filter(|(t, _)| *t == OID).nth(1) {
                Some((_, curve)) if *curve == OID_CURVE_P256 => Some(ChainKeyAlg::EcP256),
                Some((_, curve)) if *curve == OID_CURVE_P384 => Some(ChainKeyAlg::EcP384),
                _ => None,
            }
        } else {
            None
        };

        Some(CertInfo {
            tbs_raw: tbs_raw.to_vec(),
            subject_raw,
            issuer_raw,
            subject_cn,
            validity,
            spki_raw: spki.2.to_vec(),
            key_alg,
            key_bytes,
            sig_hash,
            sig_is_ecdsa,
            signature,
        })
    }
}

/// Signature algorithm OID → (hash, is-ecdsa).
fn classify_sig_alg(oid: &[u8]) -> (Option<HashAlg>, bool) {
    if oid.starts_with(OID_RSA_PREFIX) && oid.len() == 9 {
        let hash = match oid[8] {
            0x05 => Some(HashAlg::Sha1),   // sha1WithRSA
            0x0b => Some(HashAlg::Sha256), // sha256WithRSA
            0x0c => Some(HashAlg::Sha384),
            0x0d => Some(HashAlg::Sha512),
            _ => None,
        };
        (hash, false)
    } else if oid.starts_with(OID_ECDSA_PREFIX) {
        // ecdsa-with-SHA1 = ...04 01; with-SHA2xx = ...04 03 0{2,3,4}
        let hash = match (oid.get(6), oid.get(7)) {
            (Some(0x01), None) => Some(HashAlg::Sha1),
            (Some(0x03), Some(0x02)) => Some(HashAlg::Sha256),
            (Some(0x03), Some(0x03)) => Some(HashAlg::Sha384),
            (Some(0x03), Some(0x04)) => Some(HashAlg::Sha512),
            _ => None,
        };
        (hash, true)
    } else {
        (None, false)
    }
}

/// Verify `child`'s signature with `issuer`'s public key.
fn verify_cert_signature(child: &CertInfo, issuer: &CertInfo) -> Option<bool> {
    let hash = child.sig_hash?;
    let digest: Vec<u8> = match hash {
        HashAlg::Sha1 => sha1_digest(&child.tbs_raw),
        HashAlg::Sha256 => sha2::Sha256::digest(&child.tbs_raw).to_vec(),
        HashAlg::Sha384 => sha2::Sha384::digest(&child.tbs_raw).to_vec(),
        HashAlg::Sha512 => sha2::Sha512::digest(&child.tbs_raw).to_vec(),
    };
    match (issuer.key_alg?, child.sig_is_ecdsa) {
        (ChainKeyAlg::Rsa, false) => rsa_verify(hash, &issuer.key_bytes, &digest, &child.signature),
        (ChainKeyAlg::EcP256, true) => {
            use p256::ecdsa::signature::hazmat::PrehashVerifier;
            use p256::ecdsa::{Signature, VerifyingKey};
            let key = VerifyingKey::from_sec1_bytes(&issuer.key_bytes).ok()?;
            let sig = Signature::from_der(&child.signature).ok()?;
            Some(key.verify_prehash(&digest, &sig).is_ok())
        }
        (ChainKeyAlg::EcP384, true) => {
            use p384::ecdsa::signature::hazmat::PrehashVerifier;
            use p384::ecdsa::{Signature, VerifyingKey};
            let key = VerifyingKey::from_sec1_bytes(&issuer.key_bytes).ok()?;
            let sig = Signature::from_der(&child.signature).ok()?;
            Some(key.verify_prehash(&digest, &sig).is_ok())
        }
        _ => None, // algorithm family mismatch
    }
}

fn rsa_verify(alg: HashAlg, key_der: &[u8], hashed: &[u8], sig: &[u8]) -> Option<bool> {
    use rsa::pkcs1::DecodeRsaPublicKey;
    use rsa::{Pkcs1v15Sign, RsaPublicKey};
    let key = RsaPublicKey::from_pkcs1_der(key_der).ok()?;
    let scheme = match alg {
        HashAlg::Sha1 => Pkcs1v15Sign::new::<sha1::Sha1>(),
        HashAlg::Sha256 => Pkcs1v15Sign::new::<sha2::Sha256>(),
        HashAlg::Sha384 => Pkcs1v15Sign::new::<sha2::Sha384>(),
        HashAlg::Sha512 => Pkcs1v15Sign::new::<sha2::Sha512>(),
    };
    Some(key.verify(scheme, hashed, sig).is_ok())
}

fn sha1_digest(data: &[u8]) -> Vec<u8> {
    use sha1::{Digest as _, Sha1};
    Sha1::digest(data).to_vec()
}

/// Every certificate DER in a CMS blob's `certificates [0]` field.
fn extract_cms_certificates(blob: &[u8]) -> Option<Vec<Vec<u8>>> {
    let (tag, ci, _) = tlv(blob)?;
    if tag != SEQUENCE {
        return None;
    }
    let ci = children(ci, 4);
    let ctype = ci.iter().find(|(t, _)| *t == OID)?;
    if ctype.1 != OID_SIGNED_DATA {
        return None;
    }
    let content = ci.iter().find(|(t, _)| *t == CONTEXT_0)?;
    let (tag, signed_data, _) = tlv(content.1)?;
    if tag != SEQUENCE {
        return None;
    }
    let sd = children(signed_data, 16);
    let certs_body = sd.iter().find(|(t, _)| *t == CONTEXT_0)?.1;

    let mut out = Vec::new();
    let mut rest = certs_body;
    while !rest.is_empty() && out.len() < 32 {
        let before = rest;
        let Some((tag, _, next)) = tlv(rest) else {
            break;
        };
        let consumed = before.len() - next.len();
        if tag == SEQUENCE {
            out.push(before[..consumed].to_vec());
        }
        rest = next;
    }
    Some(out)
}

/// The CN of an X.501 `Name` body.
fn name_cn(name_body: &[u8]) -> Option<String> {
    for (tag, rdn) in children(name_body, 32) {
        if tag != SET {
            continue;
        }
        for (tag, atv) in children(rdn, 8) {
            if tag != SEQUENCE {
                continue;
            }
            let parts = children(atv, 2);
            let is_cn = parts
                .iter()
                .find(|(t, _)| *t == OID)
                .is_some_and(|(_, o)| *o == OID_CN);
            if is_cn {
                if let Some((_, value)) = parts.iter().rev().find(|(t, _)| *t != OID) {
                    return Some(String::from_utf8_lossy(value).into_owned());
                }
            }
        }
    }
    None
}

/// UTCTime (`YYMMDDHHMMSSZ`) or GeneralizedTime (`YYYYMMDDHHMMSSZ`) → Unix
/// seconds. Fractional seconds and offsets are not handled (CAs emit Z).
fn parse_time(tag: u8, body: &[u8]) -> Option<u64> {
    let s = std::str::from_utf8(body).ok()?;
    let s = s.strip_suffix('Z').unwrap_or(s);
    let (year, rest): (i64, &str) = match tag {
        UTC_TIME => {
            let yy: i64 = s.get(0..2)?.parse().ok()?;
            // RFC 5280: 00-49 ⇒ 20xx, 50-99 ⇒ 19xx.
            (if yy < 50 { 2000 + yy } else { 1900 + yy }, s.get(2..)?)
        }
        GENERALIZED_TIME => (s.get(0..4)?.parse().ok()?, s.get(4..)?),
        _ => return None,
    };
    let month: u32 = rest.get(0..2)?.parse().ok()?;
    let day: u64 = rest.get(2..4)?.parse().ok()?;
    let hour: u64 = rest.get(4..6)?.parse().ok()?;
    let minute: u64 = rest.get(6..8)?.parse().ok()?;
    let second: u64 = rest.get(8..10).and_then(|t| t.parse().ok()).unwrap_or(0);
    if !(1..=12).contains(&month) || day == 0 || day > 31 {
        return None;
    }

    // Days since epoch (civil-from-days inverse, Howard Hinnant's algorithm).
    let y = year - i64::from(month <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = ((month + 9) % 12) as u64;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe as i64 - 719_468;
    if days < 0 {
        return None;
    }
    Some(days as u64 * 86_400 + hour * 3_600 + minute * 60 + second)
}

// -- DER primitives (same shapes as signature.rs's private cms module) -------

fn tlv(buf: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    if buf.len() < 2 {
        return None;
    }
    let tag = buf[0];
    let first = buf[1];
    let (len, header) = if first < 0x80 {
        (first as usize, 2)
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 4 || buf.len() < 2 + n {
            return None;
        }
        let mut len = 0usize;
        for &b in &buf[2..2 + n] {
            len = (len << 8) | b as usize;
        }
        (len, 2 + n)
    };
    let end = header.checked_add(len)?;
    if end > buf.len() {
        return None;
    }
    Some((tag, &buf[header..end], &buf[end..]))
}

fn children(content: &[u8], max: usize) -> Vec<(u8, &[u8])> {
    let mut out = Vec::new();
    let mut rest = content;
    while !rest.is_empty() && out.len() < max {
        let Some((tag, body, next)) = tlv(rest) else {
            break;
        };
        out.push((tag, body));
        rest = next;
    }
    out
}

#[allow(clippy::type_complexity)]
fn children_raw(content: &[u8], max: usize) -> Vec<(u8, &[u8], &[u8])> {
    let mut out = Vec::new();
    let mut rest = content;
    while !rest.is_empty() && out.len() < max {
        let before = rest;
        let Some((tag, body, next)) = tlv(rest) else {
            break;
        };
        let consumed = before.len() - next.len();
        out.push((tag, body, &before[..consumed]));
        rest = next;
    }
    out
}

/// Minimal base64 decoder (standard alphabet, ignores whitespace).
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for &c in s.as_bytes() {
        if c.is_ascii_whitespace() || c == b'=' {
            continue;
        }
        acc = (acc << 6) | u32::from(val(c)?);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrip() {
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64_decode("aGVs\nbG8=").unwrap(), b"hello");
        assert!(base64_decode("!!!").is_none());
    }

    #[test]
    fn utc_time_parses() {
        // 2026-01-02 03:04:05 UTC
        let t = parse_time(UTC_TIME, b"260102030405Z").unwrap();
        assert_eq!(t, 1_767_323_045);
        // Generalized form of the same instant.
        let g = parse_time(GENERALIZED_TIME, b"20260102030405Z").unwrap();
        assert_eq!(g, t);
    }

    #[test]
    fn empty_anchor_set_is_unsupported() {
        let status = verify_certificate_chain(b"junk", &[], None);
        assert_eq!(status.as_str(), "unsupported");
    }

    #[test]
    fn garbage_cms_is_unsupported() {
        let anchors = parse_trust_anchors(b"not a cert");
        // Garbage anchor input yields no anchors → unsupported either way.
        let status = verify_certificate_chain(b"junk", &anchors, None);
        assert_eq!(status.as_str(), "unsupported");
    }
}
