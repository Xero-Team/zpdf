//! Digital signature fields (ISO 32000-1 §12.8, ISO 32000-2 + PAdES/ETSI).
//!
//! An interactive-form field of type `/Sig` carries a **signature dictionary**
//! (`/V`) describing a digital signature over the file: the handler that
//! produced it (`/Filter`), the encoding of the signed data (`/SubFilter`), the
//! human-declared signer / reason / location, and — the two entries that make
//! the signature verifiable — a `/ByteRange` naming which spans of the file are
//! signed and a `/Contents` string holding the CMS (PKCS #7) signature blob.
//!
//! This module reads that dictionary into a data model and performs two
//! independent checks:
//!
//! 1. **Byte-range integrity** ([`DigestStatus`]): it recomputes the digest of
//!    the signed byte range and compares it against the `messageDigest`
//!    attribute embedded in the CMS structure. A match proves the covered bytes
//!    are exactly what the signature committed to — the document was **not
//!    altered inside the signed range** after signing.
//!
//! 2. **Cryptographic signature** ([`CryptoStatus`]): it verifies the signer's
//!    RSA (PKCS #1 v1.5) or ECDSA (NIST P-256 / P-384) signature over the
//!    signed attributes, using the public key of the first certificate carried
//!    in the CMS blob. A [`CryptoStatus::Valid`] verdict proves the signed
//!    attributes (which bind the `messageDigest`) were produced by the holder of
//!    that certificate's private key.
//!
//! What this module deliberately does **not** do: validate the certificate
//! chain to a trust anchor, check revocation (CRL/OCSP), or honour signing-time
//! validity. Those require a trust store and network access, neither of which
//! lives in this pure-Rust, dependency-light crate. So even a fully
//! [`DigestStatus::Verified`] + [`CryptoStatus::Valid`] signature means "the
//! signed bytes are intact and were signed by the private key matching the
//! embedded certificate" — **not** "the signer is a trusted, non-revoked
//! identity." Callers presenting this to users must not overstate it. See
//! [`Signature::is_cryptographically_valid`].
//!
//! Everything here is bounded and best-effort: a malformed field tree, an
//! out-of-range `/ByteRange`, an unsupported algorithm, or a corrupt CMS blob
//! yields `None` / an [`DigestStatus::Unsupported`] / [`CryptoStatus::Unsupported`]
//! verdict, never a panic.

use std::collections::HashSet;

use sha1::Sha1;
use sha2::{Digest, Sha256, Sha384, Sha512};
use zpdf_core::{ObjectId, PdfDict, PdfObject};
use zpdf_parser::PdfFile;

use crate::forms::pdf_string_to_unicode;

/// Bounds on the field-tree walk (mirrors [`crate::forms`]).
const MAX_FIELD_DEPTH: usize = 50;
const MAX_SIG_FIELDS: usize = 4_096;
/// Cap on the CMS blob we attempt to DER-parse. Real signatures — even with a
/// full certificate chain and timestamp token — are comfortably under this;
/// the cap bounds work against an adversarial `/Contents`.
const MAX_CMS_BYTES: usize = 4 * 1024 * 1024;

/// A digital signature attached to a `/Sig` form field.
#[derive(Debug, Clone)]
pub struct Signature {
    /// The fully-qualified name of the signature field (`/T` chain).
    pub field_name: String,
    /// `/Filter` — the security handler that produced the signature
    /// (conventionally `Adobe.PPKLite`).
    pub filter: Option<String>,
    /// `/SubFilter` — the encoding of the signed data, e.g.
    /// `adbe.pkcs7.detached`, `adbe.pkcs7.sha1`, or `ETSI.CAdES.detached`
    /// (PAdES).
    pub sub_filter: Option<String>,
    /// `/Name` — the signer's name as declared in the dictionary (not
    /// cryptographically bound; see [`Signature::signer_common_name`]).
    pub name: Option<String>,
    /// `/M` — the signing time, as the raw PDF date string.
    pub signing_time: Option<String>,
    /// `/Location`.
    pub location: Option<String>,
    /// `/Reason`.
    pub reason: Option<String>,
    /// `/ContactInfo`.
    pub contact_info: Option<String>,
    /// The signed spans of the file (`/ByteRange`) and what they cover.
    pub coverage: ByteRangeCoverage,
    /// Result of comparing the recomputed digest of the covered bytes to the
    /// digest embedded in the CMS blob.
    pub digest: DigestStatus,
    /// Result of verifying the signer's public-key signature over the CMS signed
    /// attributes, using the first embedded certificate's public key.
    pub crypto: CryptoStatus,
    /// Human name of the digest algorithm named by the CMS `SignerInfo`
    /// (`SHA-1`, `SHA-256`, …), when it could be identified.
    pub digest_algorithm: Option<String>,
    /// Human name of the signature (public-key) algorithm identified from the
    /// CMS `SignerInfo` and the signer certificate (`RSA`, `ECDSA (P-256)`, …),
    /// when it could be identified.
    pub signature_algorithm: Option<String>,
    /// The Common Name (`CN`) of the first certificate in the CMS blob —
    /// typically, but not guaranteed to be, the signer's leaf certificate.
    /// Best-effort; `None` when no certificate / CN could be extracted.
    pub signer_common_name: Option<String>,
    /// The raw CMS `SignedData` blob (`/Contents`), for follow-up checks such
    /// as certificate-chain verification ([`crate::trust`]). `None` when the
    /// dictionary carried no string /Contents.
    pub cms_blob: Option<Vec<u8>>,
}

impl Signature {
    /// True only when **both** checks pass: the signed bytes are intact
    /// ([`DigestStatus::Verified`]) **and** the signer's signature over the
    /// signed attributes verifies against the embedded certificate's public key
    /// ([`CryptoStatus::Valid`]).
    ///
    /// This still does **not** establish trust: the certificate is not validated
    /// against any anchor, nor checked for revocation. A `true` here means
    /// "cryptographically sound, from the private key matching the embedded
    /// certificate" — the certificate's *trustworthiness* is a separate,
    /// out-of-scope question.
    pub fn is_cryptographically_valid(&self) -> bool {
        self.digest == DigestStatus::Verified && self.crypto == CryptoStatus::Valid
    }
}

/// How a signature's `/ByteRange` covers the file.
#[derive(Debug, Clone)]
pub struct ByteRangeCoverage {
    /// The `(offset, length)` spans of the file that are signed, in order.
    pub ranges: Vec<(usize, usize)>,
    /// True when the ranges start at byte 0 and the last range ends exactly at
    /// end-of-file (the single gap being the `/Contents` placeholder) — i.e. the
    /// signature covers the whole document.
    pub covers_whole_document: bool,
    /// Bytes present after the last signed span. Non-zero means the file was
    /// extended after this signature was applied — a later incremental update
    /// (possibly another signature, possibly a modification the signature does
    /// not cover).
    pub bytes_after_signature: usize,
}

/// Verdict of the byte-range digest check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestStatus {
    /// The recomputed digest of the signed byte range matches the
    /// `messageDigest` embedded in the CMS: the covered bytes are intact.
    Verified,
    /// The digests differ: the covered bytes were altered after signing.
    Mismatch,
    /// No comparable digest could be obtained — an unsupported `/SubFilter`,
    /// an unknown digest algorithm, an out-of-range `/ByteRange`, or a CMS blob
    /// without an extractable `messageDigest`. The other fields are still valid.
    Unsupported,
}

impl DigestStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            DigestStatus::Verified => "verified",
            DigestStatus::Mismatch => "mismatch",
            DigestStatus::Unsupported => "unsupported",
        }
    }
}

/// Verdict of the public-key signature check over the CMS signed attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CryptoStatus {
    /// The signer's signature over the signed attributes verifies against the
    /// public key of the embedded (first) certificate.
    Valid,
    /// A signature and key were present and of a supported algorithm, but the
    /// signature does **not** verify — a forged, corrupt, or wrong-key blob.
    Invalid,
    /// The signature could not be checked: an unsupported `/SubFilter`, no
    /// signed attributes, an unsupported signature/key algorithm (e.g. RSA-PSS,
    /// DSA, or a curve other than P-256/P-384), or an unparseable certificate /
    /// public key. The [`DigestStatus`] check may still be meaningful.
    Unsupported,
}

impl CryptoStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            CryptoStatus::Valid => "valid",
            CryptoStatus::Invalid => "invalid",
            CryptoStatus::Unsupported => "unsupported",
        }
    }
}

/// Parse all digital signatures in the document's AcroForm. Returns an empty
/// vector when the document has no signature fields (the common case). Read-only
/// and bounded; safe to call on adversarial input.
pub fn parse_signatures(file: &PdfFile) -> Vec<Signature> {
    let mut out = Vec::new();
    let Some(fields) = acroform_fields(file) else {
        return out;
    };

    let mut visited = HashSet::new();
    for obj in &fields {
        if let PdfObject::Ref(r) = obj {
            walk(file, *r, "", None, 0, &mut visited, &mut out);
        }
    }
    out
}

/// The `/Root /AcroForm /Fields` array, or `None`.
fn acroform_fields(file: &PdfFile) -> Option<Vec<PdfObject>> {
    let root_ref = file.trailer.get_ref("Root").ok()?;
    let root = file.resolve(root_ref).ok()?;
    let root = root.as_dict().ok()?;
    let af = deref(file, root.get("AcroForm")?);
    let af = af.as_dict().ok()?;
    match deref(file, af.get("Fields")?) {
        PdfObject::Array(a) => Some(a),
        _ => None,
    }
}

/// Walk the field tree, emitting a [`Signature`] for every terminal `/Sig` field
/// whose `/V` resolves to a signature dictionary. `/FT` is inheritable, so it is
/// threaded down from ancestors.
fn walk(
    file: &PdfFile,
    id: ObjectId,
    parent_name: &str,
    inherited_ft: Option<&str>,
    depth: usize,
    visited: &mut HashSet<ObjectId>,
    out: &mut Vec<Signature>,
) {
    if depth > MAX_FIELD_DEPTH || out.len() >= MAX_SIG_FIELDS || !visited.insert(id) {
        return;
    }
    let Ok(obj) = file.resolve(id) else { return };
    let Ok(dict) = obj.as_dict() else { return };

    let partial = dict
        .get("T")
        .and_then(|o| text_string(file, o))
        .unwrap_or_default();
    let name = if partial.is_empty() {
        parent_name.to_string()
    } else if parent_name.is_empty() {
        partial
    } else {
        format!("{parent_name}.{partial}")
    };

    let ft = dict
        .get_name("FT")
        .ok()
        .map(String::from)
        .or_else(|| inherited_ft.map(String::from));

    // Interior node: recurse into child fields (those carrying their own /T).
    let kids = match deref(file, dict.get("Kids").unwrap_or(&PdfObject::Null)) {
        PdfObject::Array(a) => a,
        _ => Vec::new(),
    };
    let mut has_child_field = false;
    for kid in &kids {
        if let PdfObject::Ref(r) = kid {
            let has_t = file
                .resolve(*r)
                .ok()
                .and_then(|o| o.as_dict().ok().map(|d| d.get("T").is_some()))
                .unwrap_or(false);
            if has_t {
                has_child_field = true;
                walk(file, *r, &name, ft.as_deref(), depth + 1, visited, out);
            }
        }
    }
    if has_child_field {
        return;
    }

    // Terminal field: emit a signature when it is a /Sig field with a /V dict.
    if ft.as_deref() != Some("Sig") {
        return;
    }
    let Some(sig_dict) = deref(file, dict.get("V").unwrap_or(&PdfObject::Null))
        .as_dict()
        .ok()
        .cloned()
    else {
        return;
    };
    out.push(build_signature(file, name, &sig_dict));
}

fn build_signature(file: &PdfFile, field_name: String, sig: &PdfDict) -> Signature {
    let sub_filter = sig.get_name("SubFilter").ok().map(String::from);
    let contents = match deref(file, sig.get("Contents").unwrap_or(&PdfObject::Null)) {
        PdfObject::String(s) => Some(s.as_bytes().to_vec()),
        _ => None,
    };

    let coverage = parse_byte_range(file, sig, file.data().len());
    let outcome = verify(file, &coverage, contents.as_deref(), sub_filter.as_deref());

    Signature {
        field_name,
        filter: sig.get_name("Filter").ok().map(String::from),
        sub_filter,
        name: sig.get("Name").and_then(|o| text_string(file, o)),
        signing_time: sig.get("M").and_then(|o| text_string(file, o)),
        location: sig.get("Location").and_then(|o| text_string(file, o)),
        reason: sig.get("Reason").and_then(|o| text_string(file, o)),
        contact_info: sig.get("ContactInfo").and_then(|o| text_string(file, o)),
        coverage,
        digest: outcome.digest,
        crypto: outcome.crypto,
        digest_algorithm: outcome.digest_algorithm,
        signature_algorithm: outcome.signature_algorithm,
        signer_common_name: outcome.signer_common_name,
        cms_blob: contents,
    }
}

/// The full result of verifying one signature's CMS blob.
struct VerifyOutcome {
    digest: DigestStatus,
    crypto: CryptoStatus,
    digest_algorithm: Option<String>,
    signature_algorithm: Option<String>,
    signer_common_name: Option<String>,
}

/// Parse `/ByteRange` into `(offset, length)` spans and classify coverage.
fn parse_byte_range(file: &PdfFile, sig: &PdfDict, file_len: usize) -> ByteRangeCoverage {
    let mut ranges = Vec::new();
    if let PdfObject::Array(arr) = deref(file, sig.get("ByteRange").unwrap_or(&PdfObject::Null)) {
        let nums: Vec<i64> = arr
            .iter()
            .filter_map(|o| match deref(file, o) {
                PdfObject::Integer(n) => Some(n),
                PdfObject::Real(r) if r.is_finite() => Some(r as i64),
                _ => None,
            })
            .collect();
        for pair in nums.chunks_exact(2) {
            if let (Ok(off), Ok(len)) = (usize::try_from(pair[0]), usize::try_from(pair[1])) {
                ranges.push((off, len));
            }
        }
    }

    // Whole-document coverage: first span at 0, last span ends at EOF.
    let covers_whole_document = ranges.first().zip(ranges.last()).is_some_and(
        |(&(first_off, _), &(last_off, last_len))| {
            first_off == 0 && last_off.saturating_add(last_len) == file_len
        },
    );
    let end = ranges
        .last()
        .map(|&(off, len)| off.saturating_add(len))
        .unwrap_or(0);
    let bytes_after_signature = file_len.saturating_sub(end);

    ByteRangeCoverage {
        ranges,
        covers_whole_document,
        bytes_after_signature,
    }
}

/// Recompute the covered-bytes digest, compare it to the CMS `messageDigest`,
/// and verify the signer's public-key signature over the signed attributes.
fn verify(
    file: &PdfFile,
    coverage: &ByteRangeCoverage,
    contents: Option<&[u8]>,
    sub_filter: Option<&str>,
) -> VerifyOutcome {
    let unsupported = VerifyOutcome {
        digest: DigestStatus::Unsupported,
        crypto: CryptoStatus::Unsupported,
        digest_algorithm: None,
        signature_algorithm: None,
        signer_common_name: None,
    };

    let Some(cms) = contents.filter(|c| !c.is_empty() && c.len() <= MAX_CMS_BYTES) else {
        return unsupported;
    };

    let Some(parsed) = cms::parse(cms) else {
        return unsupported;
    };
    let digest_algorithm = parsed.digest_alg.map(|a| a.name().to_string());
    let signature_algorithm = signature_alg_name(&parsed);
    let signer_common_name = parsed.signer_cn.clone();

    // The checks apply to the detached CMS SubFilters (PKCS#7 / CAdES), where the
    // digest is taken over the byte range and stored as the messageDigest signed
    // attribute. Other encodings (e.g. adbe.x509.rsa_sha1) are reported without a
    // verdict.
    let is_detached = matches!(
        sub_filter,
        Some("adbe.pkcs7.detached") | Some("ETSI.CAdES.detached")
    );
    if !is_detached {
        return VerifyOutcome {
            digest: DigestStatus::Unsupported,
            crypto: CryptoStatus::Unsupported,
            digest_algorithm,
            signature_algorithm,
            signer_common_name,
        };
    }

    // (1) Byte-range digest vs the messageDigest signed attribute.
    let digest = match (parsed.digest_alg, parsed.message_digest.as_deref()) {
        (Some(alg), Some(embedded)) => match gather_ranges(file.data(), &coverage.ranges) {
            Some(spans) => {
                if alg.hash(&spans) == embedded {
                    DigestStatus::Verified
                } else {
                    DigestStatus::Mismatch
                }
            }
            None => DigestStatus::Unsupported, // /ByteRange out of file bounds
        },
        _ => DigestStatus::Unsupported,
    };

    // (2) Public-key signature over the signed attributes.
    let crypto = verify_crypto(&parsed);

    VerifyOutcome {
        digest,
        crypto,
        digest_algorithm,
        signature_algorithm,
        signer_common_name,
    }
}

/// Verify the signer's RSA/ECDSA signature over the CMS signed attributes using
/// the embedded certificate's public key. Returns [`CryptoStatus::Unsupported`]
/// whenever a required piece is missing or the algorithm is not one we handle.
fn verify_crypto(p: &cms::Cms) -> CryptoStatus {
    let (Some(attrs), Some(sig), Some(key), Some(dalg), Some(salg)) = (
        p.signed_attrs_der.as_deref(),
        p.signature.as_deref(),
        p.signer_key.as_ref(),
        p.digest_alg,
        p.sig_alg,
    ) else {
        return CryptoStatus::Unsupported;
    };

    // The signature is computed over the DER encoding of the signed attributes,
    // hashed with the SignerInfo digest algorithm.
    let hashed = dalg.hash(attrs);

    let verified = match (salg, key.alg) {
        (cms::SigAlg::Rsa, cms::KeyAlg::Rsa) => pk::rsa_verify(dalg, &key.key, &hashed, sig),
        (cms::SigAlg::Ecdsa, cms::KeyAlg::EcP256) => pk::ecdsa_p256_verify(&key.key, &hashed, sig),
        (cms::SigAlg::Ecdsa, cms::KeyAlg::EcP384) => pk::ecdsa_p384_verify(&key.key, &hashed, sig),
        // RSA-PSS, DSA, mismatched sig/key algorithms, or unsupported curves.
        _ => return CryptoStatus::Unsupported,
    };

    match verified {
        Some(true) => CryptoStatus::Valid,
        Some(false) => CryptoStatus::Invalid,
        None => CryptoStatus::Unsupported, // key/signature failed to parse
    }
}

/// A display name combining the signer's public-key algorithm with the curve,
/// e.g. `RSA`, `ECDSA (P-256)`, `RSA-PSS`.
fn signature_alg_name(p: &cms::Cms) -> Option<String> {
    let salg = p.sig_alg?;
    Some(match salg {
        cms::SigAlg::Rsa => "RSA".to_string(),
        cms::SigAlg::RsaPss => "RSA-PSS".to_string(),
        cms::SigAlg::Ecdsa => match p.signer_key.as_ref().map(|k| k.alg) {
            Some(cms::KeyAlg::EcP256) => "ECDSA (P-256)".to_string(),
            Some(cms::KeyAlg::EcP384) => "ECDSA (P-384)".to_string(),
            _ => "ECDSA".to_string(),
        },
    })
}

/// Collect the covered byte spans into a single buffer, or `None` if any span
/// falls outside the file (a malformed or tampered `/ByteRange`).
fn gather_ranges(data: &[u8], ranges: &[(usize, usize)]) -> Option<Vec<u8>> {
    if ranges.is_empty() {
        return None;
    }
    let mut buf = Vec::new();
    for &(off, len) in ranges {
        let end = off.checked_add(len)?;
        let slice = data.get(off..end)?;
        buf.extend_from_slice(slice);
    }
    Some(buf)
}

// ---------------------------------------------------------------------------
// Digest algorithms
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DigestAlg {
    Sha1,
    Sha256,
    Sha384,
    Sha512,
}

impl DigestAlg {
    fn name(self) -> &'static str {
        match self {
            DigestAlg::Sha1 => "SHA-1",
            DigestAlg::Sha256 => "SHA-256",
            DigestAlg::Sha384 => "SHA-384",
            DigestAlg::Sha512 => "SHA-512",
        }
    }

    fn hash(self, data: &[u8]) -> Vec<u8> {
        match self {
            DigestAlg::Sha1 => Sha1::digest(data).to_vec(),
            DigestAlg::Sha256 => Sha256::digest(data).to_vec(),
            DigestAlg::Sha384 => Sha384::digest(data).to_vec(),
            DigestAlg::Sha512 => Sha512::digest(data).to_vec(),
        }
    }

    /// Map a digest-algorithm OID (the raw content bytes of the `06` TLV).
    fn from_oid(oid: &[u8]) -> Option<DigestAlg> {
        match oid {
            [0x2b, 0x0e, 0x03, 0x02, 0x1a] => Some(DigestAlg::Sha1),
            [0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01] => Some(DigestAlg::Sha256),
            [0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02] => Some(DigestAlg::Sha384),
            [0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x03] => Some(DigestAlg::Sha512),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Minimal, bounded DER / CMS reader
// ---------------------------------------------------------------------------
//
// A hand-written TLV walker: enough of RFC 5652 (CMS SignedData) and X.509 to
// pull the digest algorithm, the messageDigest signed attribute, and the first
// certificate's subject CN. It never recurses without a depth bound, never
// indexes past the buffer, and returns `None` on any structural surprise.

mod cms {
    use super::DigestAlg;

    /// The pieces we extract from a CMS `SignedData` blob.
    pub(super) struct Cms {
        pub(super) digest_alg: Option<DigestAlg>,
        pub(super) message_digest: Option<Vec<u8>>,
        pub(super) signer_cn: Option<String>,
        /// The signed attributes, DER-encoded with the outer `[0] IMPLICIT` tag
        /// rewritten to `SET OF` (0x31) — exactly the bytes the signature is
        /// computed over (RFC 5652 §5.4). `None` when the SignerInfo carries no
        /// signed attributes.
        pub(super) signed_attrs_der: Option<Vec<u8>>,
        /// The `SignerInfo` signature value (the `signature` OCTET STRING).
        pub(super) signature: Option<Vec<u8>>,
        /// The signature (public-key) algorithm from the `SignerInfo`.
        pub(super) sig_alg: Option<SigAlg>,
        /// The public key of the first embedded certificate.
        pub(super) signer_key: Option<PublicKeyInfo>,
    }

    /// The public-key algorithm named by the `SignerInfo` `signatureAlgorithm`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum SigAlg {
        /// RSA PKCS #1 v1.5 (`rsaEncryption` or `sha*WithRSAEncryption`).
        Rsa,
        /// RSA-PSS (`id-RSASSA-PSS`) — recognised but not verified.
        RsaPss,
        /// ECDSA (`ecdsa-with-SHA*`).
        Ecdsa,
    }

    /// A signer certificate's public key: its algorithm and raw key material.
    pub(super) struct PublicKeyInfo {
        pub(super) alg: KeyAlg,
        /// For RSA: the `RSAPublicKey` DER (`SEQUENCE { modulus, exponent }`).
        /// For ECDSA: the SEC1-encoded public point.
        pub(super) key: Vec<u8>,
    }

    /// The public-key algorithm of a certificate's `SubjectPublicKeyInfo`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum KeyAlg {
        Rsa,
        EcP256,
        EcP384,
    }

    // DER tags we care about.
    const SEQUENCE: u8 = 0x30;
    const SET: u8 = 0x31;
    const OID: u8 = 0x06;
    const OCTET_STRING: u8 = 0x04;
    const BIT_STRING: u8 = 0x03;
    const CONTEXT_0: u8 = 0xA0; // [0] constructed / EXPLICIT

    // OIDs (raw content bytes).
    const OID_SIGNED_DATA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x07, 0x02];
    const OID_MESSAGE_DIGEST: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x09, 0x04];
    const OID_CN: &[u8] = &[0x55, 0x04, 0x03];

    // Public-key / signature algorithm OIDs.
    // RSA family: 1.2.840.113549.1.1.{1=rsaEncryption, 10=PSS, 4/5/11/12/13=sha*WithRSA}.
    const OID_RSA_PREFIX: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01];
    const OID_RSA_PSS: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0a];
    // rsaEncryption 1.2.840.113549.1.1.1 (SPKI key algorithm).
    const OID_RSA_PUBLIC_KEY: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
    // EC: id-ecPublicKey 1.2.840.10045.2.1; ecdsa-with-* 1.2.840.10045.4.*.
    const OID_EC_PUBLIC_KEY: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
    const OID_ECDSA_PREFIX: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04];
    // Named curves.
    const OID_CURVE_P256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
    const OID_CURVE_P384: &[u8] = &[0x2b, 0x81, 0x04, 0x00, 0x22];

    /// Read one DER TLV from the front of `buf`: returns `(tag, content, rest)`.
    /// Rejects the indefinite-length form and lengths that run past `buf`.
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
                return None; // indefinite length, or absurdly large length field
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

    /// Collect the TLVs directly contained in `content`, up to `max` items.
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

    /// Like [`children`], but each entry also carries the element's **full** raw
    /// bytes (tag + length + content) — needed to re-encode the signed
    /// attributes for hashing. Returns `(tag, content, full_tlv)`.
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

    pub(super) fn parse(blob: &[u8]) -> Option<Cms> {
        // ContentInfo ::= SEQUENCE { contentType OID, content [0] SignedData }
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
        // content [0] EXPLICIT wraps the SignedData SEQUENCE.
        let (tag, signed_data, _) = tlv(content.1)?;
        if tag != SEQUENCE {
            return None;
        }

        // SignedData ::= SEQUENCE { version, digestAlgorithms SET,
        //   encapContentInfo, certificates [0]?, crls [1]?, signerInfos SET }
        let sd = children(signed_data, 16);
        // signerInfos is the last SET; digestAlgorithms is the first SET.
        let signer_infos = sd.iter().rev().find(|(t, _)| *t == SET)?;
        let certs = sd.iter().find(|(t, _)| *t == CONTEXT_0).map(|(_, c)| *c);

        // signerInfos SET OF SignerInfo — take the first SignerInfo.
        let (tag, signer_info, _) = tlv(signer_infos.1)?;
        if tag != SEQUENCE {
            return None;
        }
        let si = children_raw(signer_info, 16);

        // SignerInfo: version INT, sid, digestAlgorithm SEQ, signedAttrs [0]?,
        // signatureAlgorithm SEQ, signature OCTET, unsignedAttrs [1]?.
        // `sid` (issuerAndSerialNumber) is *also* a SEQUENCE, so we can't pick the
        // algorithm SEQUENCEs positionally. Instead classify each SEQUENCE's OID:
        // sid's OIDs are X.509 attribute types (2.5.4.x) — never digest or
        // signature OIDs — so the first SEQUENCE yielding each is unambiguous.
        let seq_oid = |seq: &[u8]| -> Option<Vec<u8>> {
            children(seq, 2)
                .iter()
                .find(|(t, _)| *t == OID)
                .map(|(_, oid)| oid.to_vec())
        };
        let digest_alg = si
            .iter()
            .filter(|(t, _, _)| *t == SEQUENCE)
            .find_map(|(_, seq, _)| seq_oid(seq).and_then(|oid| DigestAlg::from_oid(&oid)));
        let sig_alg = si
            .iter()
            .filter(|(t, _, _)| *t == SEQUENCE)
            .find_map(|(_, seq, _)| seq_oid(seq).and_then(|oid| sig_alg_from_oid(&oid)));

        // signedAttrs is the [0] IMPLICIT tag; its content is the concatenated
        // Attribute SEQUENCEs. Find the messageDigest attribute.
        let signed_attrs = si.iter().find(|(t, _, _)| *t == CONTEXT_0);
        let message_digest = signed_attrs.and_then(|(_, attrs, _)| find_message_digest(attrs));
        // For hashing, the [0] IMPLICIT tag is replaced by SET OF (RFC 5652 §5.4).
        let signed_attrs_der = signed_attrs.map(|(_, _, full)| {
            let mut der = full.to_vec();
            der[0] = SET;
            der
        });

        // The signature value is the OCTET STRING after the two algorithm SEQs.
        let signature = si
            .iter()
            .find(|(t, _, _)| *t == OCTET_STRING)
            .map(|(_, body, _)| body.to_vec());

        let signer_cn = certs.and_then(first_cert_cn);
        let signer_key = certs.and_then(first_cert_public_key);

        Some(Cms {
            digest_alg,
            message_digest,
            signer_cn,
            signed_attrs_der,
            signature,
            sig_alg,
            signer_key,
        })
    }

    /// Classify a `SignerInfo` `signatureAlgorithm` OID into an [`SigAlg`].
    fn sig_alg_from_oid(oid: &[u8]) -> Option<SigAlg> {
        if oid == OID_RSA_PSS {
            Some(SigAlg::RsaPss)
        } else if oid.starts_with(OID_RSA_PREFIX) {
            // rsaEncryption or any sha*WithRSAEncryption → PKCS#1 v1.5.
            Some(SigAlg::Rsa)
        } else if oid.starts_with(OID_ECDSA_PREFIX) {
            Some(SigAlg::Ecdsa)
        } else {
            None
        }
    }

    /// Within a signed-attributes body (concatenated `Attribute` SEQUENCEs),
    /// find the `messageDigest` attribute's OCTET STRING value.
    fn find_message_digest(attrs: &[u8]) -> Option<Vec<u8>> {
        for (tag, attr) in children(attrs, 64) {
            if tag != SEQUENCE {
                continue;
            }
            // Attribute ::= SEQUENCE { attrType OID, attrValues SET }
            let parts = children(attr, 4);
            let is_md = parts
                .iter()
                .find(|(t, _)| *t == OID)
                .is_some_and(|(_, oid)| *oid == OID_MESSAGE_DIGEST);
            if !is_md {
                continue;
            }
            let values = parts.iter().find(|(t, _)| *t == SET)?;
            let (vtag, digest, _) = tlv(values.1)?;
            if vtag == OCTET_STRING {
                return Some(digest.to_vec());
            }
        }
        None
    }

    /// Extract the subject Common Name of the first X.509 certificate in the
    /// `certificates [0]` body. Best-effort.
    fn first_cert_cn(certs: &[u8]) -> Option<String> {
        // The first Certificate ::= SEQUENCE { tbsCertificate, sigAlg, sig }.
        let (tag, cert, _) = tlv(certs)?;
        if tag != SEQUENCE {
            return None;
        }
        let (tag, tbs, _) = tlv(cert)?;
        if tag != SEQUENCE {
            return None;
        }
        // TBSCertificate SEQUENCEs, in order: signatureAlg, issuer, validity,
        // subject, spki. The subject Name is the 4th SEQUENCE.
        let subject = children(tbs, 16)
            .into_iter()
            .filter(|(t, _)| *t == SEQUENCE)
            .nth(3)?;
        // subject Name ::= SEQUENCE OF RDN(SET) OF ATV(SEQUENCE{OID, value}).
        for (tag, rdn) in children(subject.1, 32) {
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
                    .is_some_and(|(_, oid)| *oid == OID_CN);
                if is_cn {
                    if let Some((vtag, value)) = parts.iter().rev().find(|(t, _)| *t != OID) {
                        return Some(decode_directory_string(*vtag, value));
                    }
                }
            }
        }
        None
    }

    /// Extract the [`PublicKeyInfo`] from the first X.509 certificate's
    /// `SubjectPublicKeyInfo`. Best-effort; `None` on any structural surprise or
    /// an unsupported key algorithm / curve.
    fn first_cert_public_key(certs: &[u8]) -> Option<PublicKeyInfo> {
        // Certificate ::= SEQUENCE { tbsCertificate, sigAlg, sig }.
        let (tag, cert, _) = tlv(certs)?;
        if tag != SEQUENCE {
            return None;
        }
        let (tag, tbs, _) = tlv(cert)?;
        if tag != SEQUENCE {
            return None;
        }
        // TBSCertificate SEQUENCEs, in order: signatureAlg, issuer, validity,
        // subject, subjectPublicKeyInfo. The SPKI is the 5th SEQUENCE.
        let spki = children(tbs, 16)
            .into_iter()
            .filter(|(t, _)| *t == SEQUENCE)
            .nth(4)?;

        // SubjectPublicKeyInfo ::= SEQUENCE { algorithm AlgorithmIdentifier,
        //   subjectPublicKey BIT STRING }.
        let spki_parts = children(spki.1, 2);
        let alg_id = spki_parts.iter().find(|(t, _)| *t == SEQUENCE)?.1;
        let bit_string = spki_parts.iter().find(|(t, _)| *t == BIT_STRING)?.1;
        // A BIT STRING's first content byte is the count of unused trailing bits
        // (0 for keys); the key itself follows.
        let key_bytes = bit_string
            .split_first()
            .and_then(|(unused, rest)| (*unused == 0).then(|| rest.to_vec()))?;

        // AlgorithmIdentifier ::= SEQUENCE { algorithm OID, parameters ANY? }.
        let alg_parts = children(alg_id, 2);
        let alg_oid = alg_parts.iter().find(|(t, _)| *t == OID)?.1;

        if alg_oid == OID_RSA_PUBLIC_KEY {
            Some(PublicKeyInfo {
                alg: KeyAlg::Rsa,
                key: key_bytes,
            })
        } else if alg_oid == OID_EC_PUBLIC_KEY {
            // The named curve is the *second* OID (the AlgorithmIdentifier
            // parameter) after id-ecPublicKey.
            let curve = alg_parts
                .iter()
                .filter(|(t, _)| *t == OID)
                .nth(1)
                .map(|(_, oid)| *oid)?;
            let alg = if curve == OID_CURVE_P256 {
                KeyAlg::EcP256
            } else if curve == OID_CURVE_P384 {
                KeyAlg::EcP384
            } else {
                return None;
            };
            Some(PublicKeyInfo {
                alg,
                key: key_bytes,
            })
        } else {
            None
        }
    }

    /// Decode an X.520 DirectoryString value by tag: BMPString is UTF-16BE, the
    /// rest (UTF8String / PrintableString / IA5String / …) are treated as UTF-8.
    fn decode_directory_string(tag: u8, value: &[u8]) -> String {
        const BMP_STRING: u8 = 0x1e;
        if tag == BMP_STRING {
            let units: Vec<u16> = value
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16_lossy(&units)
        } else {
            String::from_utf8_lossy(value).into_owned()
        }
    }
}

// ---------------------------------------------------------------------------
// Public-key signature verification (RustCrypto)
// ---------------------------------------------------------------------------
//
// Each verifier takes the already-computed digest of the signed attributes and
// the raw signature/key bytes, and returns `Some(true)` on a valid signature,
// `Some(false)` on a well-formed-but-failing one, or `None` when the key or
// signature could not be parsed at all.

mod pk {
    use super::DigestAlg;
    use rsa::pkcs1::DecodeRsaPublicKey;
    use rsa::{Pkcs1v15Sign, RsaPublicKey};
    use sha1::Sha1;
    use sha2::{Sha256, Sha384, Sha512};

    /// Verify an RSA PKCS #1 v1.5 signature. `key_der` is the `RSAPublicKey`
    /// DER (`SEQUENCE { modulus, publicExponent }`); `hashed` is the digest of
    /// the signed attributes under `alg`.
    pub(super) fn rsa_verify(
        alg: DigestAlg,
        key_der: &[u8],
        hashed: &[u8],
        sig: &[u8],
    ) -> Option<bool> {
        let key = RsaPublicKey::from_pkcs1_der(key_der).ok()?;
        let scheme = match alg {
            DigestAlg::Sha1 => Pkcs1v15Sign::new::<Sha1>(),
            DigestAlg::Sha256 => Pkcs1v15Sign::new::<Sha256>(),
            DigestAlg::Sha384 => Pkcs1v15Sign::new::<Sha384>(),
            DigestAlg::Sha512 => Pkcs1v15Sign::new::<Sha512>(),
        };
        Some(key.verify(scheme, hashed, sig).is_ok())
    }

    /// Verify an ECDSA signature over the NIST P-256 curve. `point` is the
    /// SEC1-encoded public point; `sig` is the DER-encoded `(r, s)`.
    pub(super) fn ecdsa_p256_verify(point: &[u8], hashed: &[u8], sig: &[u8]) -> Option<bool> {
        use p256::ecdsa::signature::hazmat::PrehashVerifier;
        use p256::ecdsa::{Signature, VerifyingKey};
        let key = VerifyingKey::from_sec1_bytes(point).ok()?;
        let sig = Signature::from_der(sig).ok()?;
        Some(key.verify_prehash(hashed, &sig).is_ok())
    }

    /// Verify an ECDSA signature over the NIST P-384 curve.
    pub(super) fn ecdsa_p384_verify(point: &[u8], hashed: &[u8], sig: &[u8]) -> Option<bool> {
        use p384::ecdsa::signature::hazmat::PrehashVerifier;
        use p384::ecdsa::{Signature, VerifyingKey};
        let key = VerifyingKey::from_sec1_bytes(point).ok()?;
        let sig = Signature::from_der(sig).ok()?;
        Some(key.verify_prehash(hashed, &sig).is_ok())
    }
}

// ---------------------------------------------------------------------------
// Small object-graph helpers (local copies, mirroring crate::forms)
// ---------------------------------------------------------------------------

fn deref(file: &PdfFile, obj: &PdfObject) -> PdfObject {
    match obj {
        PdfObject::Ref(r) => file.resolve(*r).unwrap_or(PdfObject::Null),
        other => other.clone(),
    }
}

fn text_string(file: &PdfFile, obj: &PdfObject) -> Option<String> {
    match deref(file, obj) {
        PdfObject::String(s) => Some(pdf_string_to_unicode(s.as_bytes())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- DER / CMS unit tests -------------------------------------------

    /// Build a DER TLV with short/long length as appropriate.
    fn der(tag: u8, content: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        let len = content.len();
        if len < 0x80 {
            out.push(len as u8);
        } else if len < 0x100 {
            out.push(0x81);
            out.push(len as u8);
        } else {
            out.push(0x82);
            out.push((len >> 8) as u8);
            out.push((len & 0xff) as u8);
        }
        out.extend_from_slice(content);
        out
    }

    const SEQ: u8 = 0x30;
    const SET: u8 = 0x31;
    const OID: u8 = 0x06;
    const OCTET: u8 = 0x04;
    const INT: u8 = 0x02;
    const CTX0: u8 = 0xA0;

    /// Hand-assemble a minimal detached-CMS blob carrying `digest` as the
    /// messageDigest attribute, signed with SHA-256.
    fn synth_cms(digest: &[u8]) -> Vec<u8> {
        let sha256_oid = [0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01];
        let md_oid = [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x09, 0x04];

        // digestAlgorithm SEQUENCE { OID sha256 }
        let digest_alg = der(SEQ, &der(OID, &sha256_oid));

        // messageDigest Attribute SEQUENCE { OID, SET { OCTET digest } }
        let md_attr = der(
            SEQ,
            &[der(OID, &md_oid), der(SET, &der(OCTET, digest))].concat(),
        );
        // signedAttrs [0] IMPLICIT holding the one attribute.
        let signed_attrs = der(CTX0, &md_attr);

        // SignerInfo SEQUENCE { version, sid(SEQ), digestAlg(SEQ), signedAttrs[0],
        //   sigAlg(SEQ), signature(OCTET) }
        let signer_info = der(
            SEQ,
            &[
                der(INT, &[1]),
                der(SEQ, &[]), // sid placeholder
                digest_alg.clone(),
                signed_attrs,
                der(SEQ, &der(OID, &[0x2a])), // sigAlg placeholder
                der(OCTET, &[0xde, 0xad]),    // signature placeholder
            ]
            .concat(),
        );
        let signer_infos = der(SET, &signer_info);

        // SignedData SEQUENCE { version, digestAlgorithms SET, encap SEQ, signerInfos SET }
        let signed_data = der(
            SEQ,
            &[
                der(INT, &[1]),
                der(SET, &digest_alg),
                der(
                    SEQ,
                    &der(OID, &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x07, 0x01]),
                ),
                signer_infos,
            ]
            .concat(),
        );

        // ContentInfo SEQUENCE { OID signedData, [0] SignedData }
        let signed_data_oid = [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x07, 0x02];
        der(
            SEQ,
            &[der(OID, &signed_data_oid), der(CTX0, &signed_data)].concat(),
        )
    }

    #[test]
    fn cms_extracts_digest_and_algorithm() {
        let digest: Vec<u8> = (0u8..32).collect();
        let blob = synth_cms(&digest);
        let parsed = cms::parse(&blob).expect("cms");
        assert_eq!(parsed.digest_alg, Some(DigestAlg::Sha256));
        assert_eq!(parsed.message_digest.as_deref(), Some(digest.as_slice()));
    }

    #[test]
    fn cms_rejects_truncated_blob() {
        let blob = synth_cms(&[0u8; 32]);
        // Any prefix shorter than the whole must not panic; parse returns None
        // or a partial-but-safe result.
        for cut in 1..blob.len() {
            let _ = cms::parse(&blob[..cut]);
        }
    }

    #[test]
    fn cms_rejects_indefinite_length() {
        // Tag SEQUENCE, indefinite length byte 0x80 — DER forbids it.
        assert!(cms::parse(&[0x30, 0x80, 0x00, 0x00]).is_none());
    }

    #[test]
    fn digest_alg_oid_mapping() {
        assert_eq!(
            DigestAlg::from_oid(&[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01]),
            Some(DigestAlg::Sha256)
        );
        assert_eq!(
            DigestAlg::from_oid(&[0x2b, 0x0e, 0x03, 0x02, 0x1a]),
            Some(DigestAlg::Sha1)
        );
        assert_eq!(DigestAlg::from_oid(&[0x00]), None);
    }

    #[test]
    fn gather_ranges_bounds_checked() {
        let data = b"0123456789";
        assert_eq!(
            gather_ranges(data, &[(0, 3), (7, 3)]).as_deref(),
            Some(&b"012789"[..])
        );
        // Out-of-range span is rejected.
        assert!(gather_ranges(data, &[(0, 3), (7, 99)]).is_none());
        assert!(gather_ranges(data, &[]).is_none());
    }

    #[test]
    fn sha256_matches_reference() {
        // "abc" SHA-256, a well-known test vector.
        let d = DigestAlg::Sha256.hash(b"abc");
        assert_eq!(
            d,
            hex(b"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
    }

    fn hex(h: &[u8]) -> Vec<u8> {
        h.chunks_exact(2)
            .map(|c| {
                let s = std::str::from_utf8(c).unwrap();
                u8::from_str_radix(s, 16).unwrap()
            })
            .collect()
    }

    // ---- Robustness / adversarial inputs ---------------------------------

    use crate::test_util::build_pdf;
    use zpdf_parser::PdfFile;

    /// A signature field with an out-of-range /ByteRange (spans past EOF) must
    /// not panic, and must report Unsupported (cannot verify what doesn't exist).
    #[test]
    fn out_of_range_byte_range_reports_unsupported() {
        let pdf = build_pdf(&[
            "<< /Type /Catalog /Pages 2 0 R /AcroForm 4 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>",
            "<< /Fields [5 0 R] >>",
            "<< /FT /Sig /T (S1) /V << /ByteRange [0 100 200 999999] /Contents <aabbcc> >> >>",
        ]);
        let file = PdfFile::parse(pdf.as_slice()).expect("parse");
        let sigs = parse_signatures(&file);
        assert_eq!(sigs.len(), 1);
        // An out-of-range span cannot be hashed; verdict is Unsupported.
        assert_eq!(sigs[0].digest, DigestStatus::Unsupported);
    }

    /// A corrupt or oversized /Contents blob must not hang or panic. The parser
    /// caps CMS size at 4 MiB and the DER walker rejects truncated/indefinite TLVs.
    #[test]
    fn malformed_cms_contents_do_not_hang() {
        let pdf = build_pdf(&[
            "<< /Type /Catalog /Pages 2 0 R /AcroForm 4 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>",
            "<< /Fields [5 0 R 6 0 R 7 0 R] >>",
            // Corrupt: truncated SEQUENCE (length claims 0x30 bytes, content is 4).
            "<< /FT /Sig /T (Truncated) /V << /ByteRange [0 10 20 30] /Contents <30304142> >> >>",
            // Empty /Contents.
            "<< /FT /Sig /T (Empty) /V << /ByteRange [0 10 20 30] /Contents <> >> >>",
            // Indefinite-length form (DER forbids): tag 0x30, length 0x80.
            "<< /FT /Sig /T (Indefinite) /V << /ByteRange [0 10 20 30] /Contents <308000> >> >>",
        ]);
        let file = PdfFile::parse(pdf.as_slice()).expect("parse");
        let sigs = parse_signatures(&file);
        // All three parse, but none can extract a digest (Unsupported).
        assert_eq!(sigs.len(), 3);
        for s in &sigs {
            assert_eq!(s.digest, DigestStatus::Unsupported);
        }
    }

    /// A pathological field tree (deep nesting, many fields) must terminate cleanly.
    #[test]
    fn deep_field_tree_terminates() {
        // 60 fields in a flat tree (exceeds MAX_SIG_FIELDS = 4096 is impractical
        // in a hand-rolled PDF; test depth instead). A chain of 100 nested /Kids
        // exceeds MAX_FIELD_DEPTH = 50 and is pruned.
        let mut objs = vec![
            "<< /Type /Catalog /Pages 2 0 R /AcroForm 4 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>".to_string(),
            "<< /Fields [5 0 R] >>".to_string(),
        ];
        // Build a chain: obj 5 → obj 6 → obj 7 … → obj 104 (100 links).
        for i in 0..100 {
            let next = if i < 99 {
                format!("{} 0 R", 5 + i + 1)
            } else {
                "null".to_string()
            };
            objs.push(format!("<< /T (Field{i}) /FT /Sig /Kids [{}] >>", next));
        }
        let pdf = build_pdf(&objs.iter().map(|s| s.as_str()).collect::<Vec<_>>());
        let file = PdfFile::parse(pdf.as_slice()).expect("parse");
        let sigs = parse_signatures(&file);
        // The walk terminates at depth 50; no signatures are extracted (none had /V).
        assert!(sigs.len() < 100);
    }
}
