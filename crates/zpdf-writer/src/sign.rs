//! Digital signature creation (ISO 32000-1 §12.8): sign a PDF with an
//! incremental update carrying a `/Sig` field + CMS `SignedData` container.
//!
//! The classic chicken-and-egg — the CMS lives inside the `/ByteRange` gap it
//! must not cover — is solved the standard way: the signature dictionary is
//! written with **fixed-width placeholders** (`/ByteRange` slots and a
//! zero-filled `/Contents` hex window), the update is serialized, the real
//! byte ranges are patched in, the covered spans are hashed, and the CMS is
//! hex-patched into the reserved window (trailing zeros are outside the DER
//! TLV and ignored by readers).
//!
//! Produced signatures are `adbe.pkcs7.detached` with SHA-256, signed
//! attributes (`contentType` + `messageDigest`, RFC 5652 §5.3) and the signer
//! certificate embedded — verifiable by zpdf's own
//! `PdfDocument::signatures()` (Verified + Valid) and standard viewers.
//! Certificate *chain trust* is out of scope (no trust store), matching the
//! verifier.
//!
//! Pure Rust via RustCrypto: `rsa` (PKCS#1 v1.5) and `p256` (ECDSA).

use sha2::{Digest, Sha256};
use zpdf_core::{ObjectId, PdfDict, PdfName, PdfObject, Result};

use crate::metadata::encode_text_string;
use crate::serialize::serialize_object_body;
use crate::{invalid_data, IncrementalWriter};

/// Reserved `/Contents` hex window (bytes of hex chars → half as many CMS
/// bytes). 16 KiB of hex fits a ~2 KiB certificate + RSA-4096 signature with
/// lots of headroom.
const RESERVED_HEX: usize = 16_384;

/// Signing key material. The certificate must carry the matching public key.
pub enum SigningKey {
    /// ECDSA over NIST P-256 (signature algorithm `ecdsa-with-SHA256`).
    EcdsaP256(Box<p256::ecdsa::SigningKey>),
    /// RSA PKCS#1 v1.5 with SHA-256 (`sha256WithRSAEncryption`).
    Rsa(Box<rsa::RsaPrivateKey>),
}

impl SigningKey {
    /// An ECDSA P-256 key from its raw 32-byte scalar.
    pub fn ecdsa_p256_from_scalar(scalar: &[u8]) -> Result<Self> {
        let key = p256::ecdsa::SigningKey::from_slice(scalar)
            .map_err(|_| invalid_data("invalid P-256 private scalar"))?;
        Ok(SigningKey::EcdsaP256(Box::new(key)))
    }

    /// An RSA private key from PKCS#1 DER (`RSAPrivateKey`).
    pub fn rsa_from_pkcs1_der(der: &[u8]) -> Result<Self> {
        use rsa::pkcs1::DecodeRsaPrivateKey;
        let key = rsa::RsaPrivateKey::from_pkcs1_der(der)
            .map_err(|_| invalid_data("invalid PKCS#1 RSA private key"))?;
        Ok(SigningKey::Rsa(Box::new(key)))
    }

    /// An RSA or EC private key from PKCS#8 DER (`PrivateKeyInfo`), tried in
    /// that order.
    pub fn from_pkcs8_der(der: &[u8]) -> Result<Self> {
        // Both crates re-export the same `pkcs8::DecodePrivateKey` trait, so a
        // single import brings it into scope for both key types.
        use p256::pkcs8::DecodePrivateKey as _;
        if let Ok(key) = rsa::RsaPrivateKey::from_pkcs8_der(der) {
            return Ok(SigningKey::Rsa(Box::new(key)));
        }
        if let Ok(key) = p256::ecdsa::SigningKey::from_pkcs8_der(der) {
            return Ok(SigningKey::EcdsaP256(Box::new(key)));
        }
        Err(invalid_data("PKCS#8 key is neither RSA nor P-256").into())
    }

    /// Sign `msg` (the DER SET of signed attributes), returning the raw
    /// signature value for the CMS `signature` OCTET STRING.
    fn sign(&self, msg: &[u8]) -> Result<Vec<u8>> {
        match self {
            SigningKey::EcdsaP256(key) => {
                use p256::ecdsa::signature::hazmat::PrehashSigner;
                let hash = Sha256::digest(msg);
                let sig: p256::ecdsa::Signature = key
                    .sign_prehash(&hash)
                    .map_err(|_| invalid_data("ECDSA signing failed"))?;
                Ok(sig.to_der().as_bytes().to_vec())
            }
            SigningKey::Rsa(key) => {
                use rsa::pkcs1v15::SigningKey as RsaSigningKey;
                use rsa::signature::{SignatureEncoding, Signer};
                let signing_key = RsaSigningKey::<Sha256>::new((**key).clone());
                Ok(signing_key.sign(msg).to_vec())
            }
        }
    }

    /// The `SignerInfo` signatureAlgorithm OID for this key type.
    fn sig_alg_oid(&self) -> &'static [u8] {
        match self {
            // ecdsa-with-SHA256: 1.2.840.10045.4.3.2
            SigningKey::EcdsaP256(_) => &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02],
            // sha256WithRSAEncryption: 1.2.840.113549.1.1.11
            SigningKey::Rsa(_) => &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b],
        }
    }

    /// An RSA or EC private key from a PEM-encoded PKCS#8 (`-----BEGIN PRIVATE
    /// KEY-----`) or PKCS#1 (`-----BEGIN RSA PRIVATE KEY-----`) block.
    pub fn from_pem(pem: &[u8]) -> Result<Self> {
        if let Ok(der) = pem_decode(pem, "PRIVATE KEY") {
            return Self::from_pkcs8_der(&der);
        }
        if let Ok(der) = pem_decode(pem, "RSA PRIVATE KEY") {
            return Self::rsa_from_pkcs1_der(&der);
        }
        Err(invalid_data("PEM key is neither PKCS#8 PRIVATE KEY nor RSA PRIVATE KEY").into())
    }

    /// An RSA private key from a PEM-encoded PKCS#1 block.
    pub fn rsa_from_pkcs1_pem(pem: &[u8]) -> Result<Self> {
        let der = pem_decode(pem, "RSA PRIVATE KEY")?;
        Self::rsa_from_pkcs1_der(&der)
    }

    /// An RSA or EC private key from a PEM-encoded PKCS#8 block.
    pub fn from_pkcs8_pem(pem: &[u8]) -> Result<Self> {
        let der = pem_decode(pem, "PRIVATE KEY")?;
        Self::from_pkcs8_der(&der)
    }
}

/// Decode a PEM block, returning the DER bytes of the body. `expected_tag` is
/// the label inside the `-----BEGIN <tag>-----`/`-----END <tag>-----` envelope
/// (e.g. `"PRIVATE KEY"`, `"CERTIFICATE"`). Hand-rolled base64 keeps this
/// C-dependency-free.
pub fn pem_decode(pem: &[u8], expected_tag: &str) -> Result<Vec<u8>> {
    let text = std::str::from_utf8(pem).map_err(|_| invalid_data("PEM is not valid UTF-8"))?;
    let begin = format!("-----BEGIN {expected_tag}-----");
    let end = format!("-----END {expected_tag}-----");
    let start = text
        .find(&begin)
        .ok_or_else(|| invalid_data(&format!("PEM missing BEGIN {expected_tag}")))?;
    let body_start = start + begin.len();
    let end_pos = text[body_start..]
        .find(&end)
        .ok_or_else(|| invalid_data(&format!("PEM missing END {expected_tag}")))?
        + body_start;
    let b64: String = text[body_start..end_pos]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    base64_decode(b64.as_bytes())
}

/// Decode a base64 string to bytes (standard alphabet with padding).
fn base64_decode(input: &[u8]) -> Result<Vec<u8>> {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut bits: u32 = 0;
    let mut count = 0u32;
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    for &b in input {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/' => {
                let val = TABLE.iter().position(|&t| t == b).unwrap() as u32;
                bits = (bits << 6) | val;
                count += 6;
                if count >= 8 {
                    count -= 8;
                    out.push((bits >> count) as u8);
                }
            }
            b'=' => break,
            _ => return Err(invalid_data("invalid base64 character").into()),
        }
    }
    Ok(out)
}

/// The signature dictionary `/SubFilter` — the CMS flavor.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SubFilter {
    /// `adbe.pkcs7.detached` — the classic Adobe detached PKCS#7 (default).
    #[default]
    AdbePkcs7Detached,
    /// `ETSI.CAdES.detached` — PAdES B-B level (RFC 5126), recognized by zpdf's
    /// verifier and standard PAdES viewers. Same CMS content as detached; the
    /// SubFilter name is what makes it PAdES.
    EtsiCAdESDetached,
}

impl SubFilter {
    pub fn as_str(self) -> &'static str {
        match self {
            SubFilter::AdbePkcs7Detached => "adbe.pkcs7.detached",
            SubFilter::EtsiCAdESDetached => "ETSI.CAdES.detached",
        }
    }
}

/// A visible signature appearance: a non-zero `/Rect` widget with a rendered
/// `/AP /N` Form XObject (signer name, date, reason; optional background and
/// border). When `None`, the signature is invisible (the original behavior).
#[derive(Debug, Clone)]
pub struct AppearanceSpec {
    /// The widget rectangle in page coordinates (PDF user space, origin bottom-left).
    pub rect: zpdf_core::Rect,
    /// Render the signer's `/Name` and `/Reason` text inside the appearance.
    pub show_text: bool,
    /// Render the signing `/M` date inside the appearance.
    pub show_date: bool,
    /// Fill color (RGB, 0–1); `None` for no fill.
    pub bg: Option<(f64, f64, f64)>,
    /// Border color (RGB, 0–1); `None` for no border.
    pub border: Option<(f64, f64, f64)>,
}

impl Default for AppearanceSpec {
    fn default() -> Self {
        Self {
            rect: zpdf_core::Rect::new(20.0, 20.0, 220.0, 60.0),
            show_text: true,
            show_date: true,
            bg: Some((0.96, 0.97, 0.99)),
            border: Some((0.4, 0.5, 0.7)),
        }
    }
}

/// A pluggable RFC 3161 timestamp requester. The signer builds a
/// `TimeStampReq` (SHA-256 over the signature value), the requester fetches a
/// `TimeStampResp` from a TSA, and the signer embeds the timestamp token as a
/// CMS `unsignedAttr`. The library has no network dependency by default; opt
/// into the built-in ureq implementation with the `timestamp` feature.
pub trait TimestampRequester {
    /// POST a `TimeStampReq` (DER) and return the `TimeStampResp` (DER).
    fn request_timestamp(&self, time_stamp_req_der: &[u8]) -> Result<Vec<u8>>;
}

/// A `TimestampRequester` over HTTP(S) using the pure-Rust `ureq` client
/// (behind the `timestamp` feature). POSTs the request to the TSA URL with
/// `Content-Type: application/timestamp-query`.
#[cfg(feature = "timestamp")]
pub struct UreqTimestampRequester {
    tsa_url: String,
}

#[cfg(feature = "timestamp")]
impl UreqTimestampRequester {
    /// Create a requester for a TSA endpoint (e.g.
    /// `http://timestamp.digicert.com`).
    pub fn new(tsa_url: String) -> Self {
        Self { tsa_url }
    }
}

#[cfg(feature = "timestamp")]
impl TimestampRequester for UreqTimestampRequester {
    fn request_timestamp(&self, time_stamp_req_der: &[u8]) -> Result<Vec<u8>> {
        use std::io::Read;
        let resp = ureq::post(&self.tsa_url)
            .set("Content-Type", "application/timestamp-query")
            .send_bytes(time_stamp_req_der)
            .map_err(|e| invalid_data(&format!("TSA request failed: {e}")))?;
        let mut bytes = Vec::new();
        resp.into_reader()
            .read_to_end(&mut bytes)
            .map_err(|e| invalid_data(&format!("TSA response read failed: {e}")))?;
        Ok(bytes)
    }
}

/// Optional signature metadata written into the signature dictionary.
#[derive(Default)]
pub struct SignatureOptions {
    /// `/Name` — the signer's name.
    pub name: Option<String>,
    /// `/Reason` — why the document was signed.
    pub reason: Option<String>,
    /// `/Location` — where it was signed.
    pub location: Option<String>,
    /// `/ContactInfo`.
    pub contact: Option<String>,
    /// Field name (`/T`); default `Signature1`.
    pub field_name: Option<String>,
    /// The `/SubFilter` (CMS flavor). Defaults to `adbe.pkcs7.detached`.
    pub subfilter: SubFilter,
    /// A visible signature appearance; `None` for an invisible signature.
    pub appearance: Option<AppearanceSpec>,
    /// Additional certificates to embed in the CMS `certificates` set (e.g.
    /// the chain up to the root), enabling trust verification and DSS/LTV.
    pub extra_certs: Vec<Vec<u8>>,
    /// CRLs to embed in the CMS `crls` set for offline revocation checking.
    pub crls: Vec<Vec<u8>>,
    /// An RFC 3161 timestamp requester; when present, a timestamp token is
    /// fetched over the signature and embedded as a CMS `unsignedAttr`.
    pub timestamp: Option<Box<dyn TimestampRequester>>,
}

impl Clone for SignatureOptions {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            reason: self.reason.clone(),
            location: self.location.clone(),
            contact: self.contact.clone(),
            field_name: self.field_name.clone(),
            subfilter: self.subfilter,
            appearance: self.appearance.clone(),
            extra_certs: self.extra_certs.clone(),
            crls: self.crls.clone(),
            timestamp: None, // the requester is not Cloneable; clone without it
        }
    }
}

impl IncrementalWriter {
    /// Sign the document: adds an invisible signature field to the first
    /// page, wires it into the AcroForm, and returns the **finalized signed
    /// bytes** (the writer is consumed — a signature covers the whole file,
    /// so no further edits are possible in this revision).
    ///
    /// `certificate_der` is the signer's X.509 certificate (DER). Its public
    /// key must match `key`.
    pub fn sign(
        mut self,
        certificate_der: &[u8],
        key: &SigningKey,
        options: &SignatureOptions,
    ) -> Result<Vec<u8>> {
        if certificate_der.is_empty() {
            return Err(invalid_data("certificate must not be empty").into());
        }

        // --- 1. The signature dictionary, as a raw body with placeholders.
        let field_name = options.field_name.as_deref().unwrap_or("Signature1");
        let mut sig_body = Vec::new();
        sig_body.extend_from_slice(
            format!(
                "<< /Type /Sig /Filter /Adobe.PPKLite /SubFilter /{}",
                options.subfilter.as_str()
            )
            .as_bytes(),
        );
        for (k, v) in [
            ("Name", &options.name),
            ("Reason", &options.reason),
            ("Location", &options.location),
            ("ContactInfo", &options.contact),
        ] {
            if let Some(text) = v {
                sig_body.extend_from_slice(format!(" /{k} ").as_bytes());
                serialize_object_body(&mut sig_body, &PdfObject::String(encode_text_string(text)))?;
            }
        }
        sig_body.extend_from_slice(format!(" /M (D:{}Z)", pdf_timestamp()).as_bytes());
        sig_body.extend_from_slice(b" /ByteRange [0000000000 0000000000 0000000000 0000000000]");
        sig_body.extend_from_slice(b" /Contents <");
        sig_body.extend_from_slice(&vec![b'0'; RESERVED_HEX]);
        sig_body.extend_from_slice(b"> >>");

        self.ensure_object_capacity(2)?;
        let (sig_num, _) = self.try_add_raw_object(&sig_body)?;
        let sig_ref = ObjectId(sig_num, 0);

        // --- 2. The signature form field / widget annotation.
        let page_id = self.page_id(0)?;
        let mut field = PdfDict::new();
        field.insert(PdfName::new("FT"), PdfObject::Name(PdfName::new("Sig")));
        field.insert(
            PdfName::new("T"),
            PdfObject::String(encode_text_string(field_name)),
        );
        field.insert(PdfName::new("V"), PdfObject::Ref(sig_ref));
        field.insert(PdfName::new("Type"), PdfObject::Name(PdfName::new("Annot")));
        field.insert(
            PdfName::new("Subtype"),
            PdfObject::Name(PdfName::new("Widget")),
        );
        field.insert(PdfName::new("P"), PdfObject::Ref(page_id));

        if let Some(appearance) = &options.appearance {
            // Visible signature: a non-zero /Rect widget with a rendered
            // /AP /N Form XObject (signer name, date, reason, background,
            // border) plus /MK appearance characteristics and a /DA.
            let r = &appearance.rect;
            field.insert(
                PdfName::new("Rect"),
                PdfObject::Array(vec![
                    PdfObject::Real(r.x0),
                    PdfObject::Real(r.y0),
                    PdfObject::Real(r.x1),
                    PdfObject::Real(r.y1),
                ]),
            );
            field.insert(PdfName::new("F"), PdfObject::Integer(4)); // Print
            let (content, w, h) = build_appearance_stream(
                appearance,
                options.name.as_deref().unwrap_or(""),
                options.reason.as_deref().unwrap_or(""),
            );
            let mut ap_dict = PdfDict::new();
            ap_dict.insert(
                PdfName::new("Type"),
                PdfObject::Name(PdfName::new("XObject")),
            );
            ap_dict.insert(
                PdfName::new("Subtype"),
                PdfObject::Name(PdfName::new("Form")),
            );
            ap_dict.insert(
                PdfName::new("BBox"),
                PdfObject::Array(vec![
                    PdfObject::Integer(0),
                    PdfObject::Integer(0),
                    PdfObject::Real(w),
                    PdfObject::Real(h),
                ]),
            );
            // The appearance uses a standard-14 Helvetica for its text.
            let mut res = PdfDict::new();
            let mut fonts = PdfDict::new();
            let mut helv = PdfDict::new();
            helv.insert(PdfName::new("Type"), PdfObject::Name(PdfName::new("Font")));
            helv.insert(
                PdfName::new("Subtype"),
                PdfObject::Name(PdfName::new("Type1")),
            );
            helv.insert(
                PdfName::new("BaseFont"),
                PdfObject::Name(PdfName::new("Helvetica")),
            );
            fonts.insert(PdfName::new("Helv"), PdfObject::Dict(helv));
            res.insert(PdfName::new("Font"), PdfObject::Dict(fonts));
            ap_dict.insert(PdfName::new("Resources"), PdfObject::Dict(res));
            let (ap_num, _) = self.try_add_stream(&ap_dict, &content)?;
            let mut ap = PdfDict::new();
            ap.insert(PdfName::new("N"), PdfObject::Ref(ObjectId(ap_num, 0)));
            field.insert(PdfName::new("AP"), PdfObject::Dict(ap));
            // /MK appearance characteristics: background, border, caption.
            let mut mk = PdfDict::new();
            if let Some((br, bg, bb)) = appearance.border {
                mk.insert(
                    PdfName::new("BC"),
                    PdfObject::Array(vec![
                        PdfObject::Real(br),
                        PdfObject::Real(bg),
                        PdfObject::Real(bb),
                    ]),
                );
            }
            if let Some((r2, g2, b2)) = appearance.bg {
                mk.insert(
                    PdfName::new("BG"),
                    PdfObject::Array(vec![
                        PdfObject::Real(r2),
                        PdfObject::Real(g2),
                        PdfObject::Real(b2),
                    ]),
                );
            }
            mk.insert(
                PdfName::new("CA"),
                PdfObject::String(encode_text_string("Signature")),
            );
            field.insert(PdfName::new("MK"), PdfObject::Dict(mk));
            field.insert(
                PdfName::new("DA"),
                PdfObject::String(encode_text_string("0 0 0 rg /Helv 9 Tf")),
            );
        } else {
            // Invisible: zero rect + Print flag set (bit 3, value 4).
            field.insert(
                PdfName::new("Rect"),
                PdfObject::Array(vec![
                    PdfObject::Integer(0),
                    PdfObject::Integer(0),
                    PdfObject::Integer(0),
                    PdfObject::Integer(0),
                ]),
            );
            field.insert(PdfName::new("F"), PdfObject::Integer(4));
        }
        let (field_num, _) = self.try_add_object(&PdfObject::Dict(field))?;
        let field_ref = ObjectId(field_num, 0);

        // --- 2b. DSS (Document Security Store): embed extra certs + CRLs for
        // LTV / offline validation. /VRI is omitted (optional); /Certs and
        // /CRLs carry the caller-supplied validation material.
        let dss_ref = if !options.extra_certs.is_empty() || !options.crls.is_empty() {
            let mut cert_refs = Vec::new();
            for cert in &options.extra_certs {
                let (n, _) = self.try_add_stream(&PdfDict::new(), cert)?;
                cert_refs.push(PdfObject::Ref(ObjectId(n, 0)));
            }
            let mut crl_refs = Vec::new();
            for crl in &options.crls {
                let (n, _) = self.try_add_stream(&PdfDict::new(), crl)?;
                crl_refs.push(PdfObject::Ref(ObjectId(n, 0)));
            }
            let mut dss = PdfDict::new();
            if !cert_refs.is_empty() {
                dss.insert(PdfName::new("Certs"), PdfObject::Array(cert_refs));
            }
            if !crl_refs.is_empty() {
                dss.insert(PdfName::new("CRLs"), PdfObject::Array(crl_refs));
            }
            let (dss_num, _) = self.try_add_object(&PdfObject::Dict(dss))?;
            Some(ObjectId(dss_num, 0))
        } else {
            None
        };

        // --- 3. Wire into the page /Annots and the AcroForm /Fields.
        let page_obj = self.resolve_current(page_id)?;
        let mut page_dict = page_obj.as_dict()?.clone();
        let mut annots = match page_dict.get("Annots") {
            Some(PdfObject::Ref(r)) => match self.resolve_current(*r) {
                Ok(obj) => obj.as_array().ok().map(|a| a.to_vec()).unwrap_or_default(),
                Err(_) => Vec::new(),
            },
            Some(PdfObject::Array(arr)) => arr.to_vec(),
            _ => Vec::new(),
        };
        annots.push(PdfObject::Ref(field_ref));
        page_dict.insert(PdfName::new("Annots"), PdfObject::Array(annots));
        self.overwrite_object(page_id, PdfObject::Dict(page_dict));

        let catalog_id = self.catalog_ref();
        let catalog = self.resolve_current(catalog_id)?;
        let mut catalog_dict = catalog.as_dict()?.clone();
        // Load (or create) the AcroForm dict, following one indirection.
        let (acro_id, mut acro_dict) = match catalog_dict.get("AcroForm") {
            Some(PdfObject::Ref(r)) => {
                let d = self.resolve_current(*r)?.as_dict()?.clone();
                (Some(*r), d)
            }
            Some(PdfObject::Dict(d)) => (None, d.clone()),
            _ => (None, PdfDict::new()),
        };
        let mut fields = match acro_dict.get("Fields") {
            Some(PdfObject::Array(a)) => a.clone(),
            Some(PdfObject::Ref(r)) => match self.resolve_current(*r) {
                Ok(PdfObject::Array(a)) => a,
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        fields.push(PdfObject::Ref(field_ref));
        acro_dict.insert(PdfName::new("Fields"), PdfObject::Array(fields));
        // SigFlags 3 = SignaturesExist | AppendOnly.
        acro_dict.insert(PdfName::new("SigFlags"), PdfObject::Integer(3));
        match acro_id {
            Some(id) => self.overwrite_object(id, PdfObject::Dict(acro_dict)),
            None => {
                catalog_dict.insert(PdfName::new("AcroForm"), PdfObject::Dict(acro_dict));
                self.overwrite_object(catalog_id, PdfObject::Dict(catalog_dict));
            }
        }
        // Attach the /DSS to the catalog (re-resolve so the AcroForm overwrite
        // above, if any, is preserved).
        if let Some(dss) = dss_ref {
            let catalog = self.resolve_current(catalog_id)?;
            let mut catalog_dict = catalog.as_dict()?.clone();
            catalog_dict.insert(PdfName::new("DSS"), PdfObject::Ref(dss));
            self.overwrite_object(catalog_id, PdfObject::Dict(catalog_dict));
        }

        // --- 4. Serialize, patch /ByteRange, hash, patch /Contents.
        let mut cursor = std::io::Cursor::new(Vec::new());
        self.write(&mut cursor).map_err(zpdf_core::Error::Io)?;
        let mut buf = cursor.into_inner();

        // Locate the placeholders (search only the appended update region).
        let tail_start = self.document().file().data().len();
        let br_marker = b"/ByteRange [0000000000";
        let br_at = find_from(&buf, br_marker, tail_start)
            .ok_or_else(|| invalid_data("ByteRange placeholder not found"))?;
        let contents_marker = b"/Contents <";
        let contents_at = find_from(&buf, contents_marker, br_at)
            .ok_or_else(|| invalid_data("Contents placeholder not found"))?;
        let contents_start = contents_at + contents_marker.len() - 1; // offset of `<`
        let contents_end = contents_start + 1 + RESERVED_HEX + 1; // past `>`
        if buf.get(contents_end - 1) != Some(&b'>') {
            return Err(invalid_data("Contents window corrupt").into());
        }

        let ranges = [
            0usize,
            contents_start,
            contents_end,
            buf.len() - contents_end,
        ];
        let br_open = br_at + b"/ByteRange ".len(); // offset of `[`
        for (i, &v) in ranges.iter().enumerate() {
            // Compare in u64: usize is 32-bit on wasm32, where the ten-digit
            // literal would not even fit the type.
            if v as u64 > 9_999_999_999 {
                return Err(invalid_data("file too large for ByteRange slots").into());
            }
            let slot = br_open + 1 + i * 11;
            buf[slot..slot + 10].copy_from_slice(format!("{v:010}").as_bytes());
        }

        let mut hasher = Sha256::new();
        hasher.update(&buf[..contents_start]);
        hasher.update(&buf[contents_end..]);
        let digest = hasher.finalize();

        // Two-phase CMS: compute the signature over the signed attributes
        // first, then (optionally) fetch an RFC 3161 timestamp over the
        // signature value and embed it as an unsignedAttr, before assembling
        // the final SignedData.
        let signed_attrs = build_signed_attrs(&digest);
        let signature = key.sign(&der(SET, &signed_attrs))?;

        let unsigned_attrs: Option<Vec<u8>> = if let Some(requester) = &options.timestamp {
            // Hash the signature value for the TimeStampReq messageImprint.
            let sig_hash = Sha256::digest(&signature);
            let tsq = build_time_stamp_req(&sig_hash)?;
            let tsr = requester.request_timestamp(&tsq)?;
            let tst = extract_time_stamp_token(&tsr).ok_or_else(|| {
                invalid_data("TSA returned no timestamp token (status error or missing token)")
            })?;
            Some(build_unsigned_attrs_timestamp(&tst))
        } else {
            None
        };

        let cms = build_cms(
            certificate_der,
            &options.extra_certs,
            &options.crls,
            &signed_attrs,
            &signature,
            key.sig_alg_oid(),
            unsigned_attrs.as_deref(),
        );
        let hex = to_hex(&cms);
        if hex.len() > RESERVED_HEX {
            return Err(invalid_data("CMS exceeds the reserved /Contents window").into());
        }
        buf[contents_start + 1..contents_start + 1 + hex.len()].copy_from_slice(&hex);
        Ok(buf)
    }
}

// ---- CMS SignedData builder (RFC 5652) -------------------------------------

const SEQ: u8 = 0x30;
const SET: u8 = 0x31;
const OID: u8 = 0x06;
const OCTET: u8 = 0x04;
const INT: u8 = 0x02;
const CTX0: u8 = 0xA0;
const CTX1: u8 = 0xA1;

const OID_SHA256: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01];
const OID_CONTENT_TYPE: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x09, 0x03];
const OID_MESSAGE_DIGEST: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x09, 0x04];
const OID_DATA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x07, 0x01];
const OID_SIGNED_DATA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x07, 0x02];

fn der(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let len = content.len();
    if len < 0x80 {
        out.push(len as u8);
    } else if len < 0x100 {
        out.extend_from_slice(&[0x81, len as u8]);
    } else if len < 0x10000 {
        out.extend_from_slice(&[0x82, (len >> 8) as u8, (len & 0xff) as u8]);
    } else {
        out.extend_from_slice(&[
            0x83,
            (len >> 16) as u8,
            ((len >> 8) & 0xff) as u8,
            (len & 0xff) as u8,
        ]);
    }
    out.extend_from_slice(content);
    out
}

/// Build the signed-attributes SET body (contentType(data) then
/// messageDigest), in their DER SET-of lexicographic order. The returned
/// bytes are the concatenation of the two attribute SEQUENCEs — used both as
/// the IMPLICIT `[0]` signedAttrs content and (wrapped in a SET) as the data
/// the signature covers.
fn build_signed_attrs(digest: &[u8]) -> Vec<u8> {
    let ct_attr = der(
        SEQ,
        &[der(OID, OID_CONTENT_TYPE), der(SET, &der(OID, OID_DATA))].concat(),
    );
    let md_attr = der(
        SEQ,
        &[der(OID, OID_MESSAGE_DIGEST), der(SET, &der(OCTET, digest))].concat(),
    );
    [ct_attr, md_attr].concat()
}

/// Build the detached CMS `SignedData` over `digest` (the SHA-256 of the
/// signed byte ranges), embedding the signer cert plus any `extra_certs`
/// (chain) and `crls`, carrying pre-computed `signed_attrs` and `signature`,
/// and an optional `unsigned_attrs` (RFC 3161 timestamp token).
fn build_cms(
    signer_cert: &[u8],
    extra_certs: &[Vec<u8>],
    crls: &[Vec<u8>],
    signed_attrs: &[u8],
    signature: &[u8],
    sig_alg_oid: &[u8],
    unsigned_attrs: Option<&[u8]>,
) -> Vec<u8> {
    let digest_alg = der(SEQ, &der(OID, OID_SHA256));

    // certificates [0] IMPLICIT: signer cert first, then the chain.
    let mut certs_blob = signer_cert.to_vec();
    for c in extra_certs {
        certs_blob.extend_from_slice(c);
    }
    let certificates = der(CTX0, &certs_blob);

    // crls [1] IMPLICIT (optional).
    let mut crls_blob = Vec::new();
    for c in crls {
        crls_blob.extend_from_slice(c);
    }
    let crls_field = if crls_blob.is_empty() {
        Vec::new()
    } else {
        der(CTX1, &crls_blob)
    };

    let mut signer_info_fields = vec![
        der(INT, &[1]),
        der(SEQ, &[]), // sid: not read by verifiers that scan by OID
        digest_alg.clone(),
        der(CTX0, signed_attrs), // signedAttrs [0] IMPLICIT
        der(SEQ, &der(OID, sig_alg_oid)),
        der(OCTET, signature),
    ];
    if let Some(ua) = unsigned_attrs {
        signer_info_fields.push(der(CTX1, ua)); // unsignedAttrs [1] IMPLICIT
    }
    let signer_info = der(SEQ, &signer_info_fields.concat());

    let mut sd_fields = vec![
        der(INT, &[1]),
        der(SET, &digest_alg),
        der(SEQ, &der(OID, OID_DATA)),
        certificates,
    ];
    if !crls_field.is_empty() {
        sd_fields.push(crls_field);
    }
    sd_fields.push(der(SET, &signer_info));
    let signed_data = der(SEQ, &sd_fields.concat());

    der(
        SEQ,
        &[der(OID, OID_SIGNED_DATA), der(CTX0, &signed_data)].concat(),
    )
}

/// OID for the CMS `signature-time-stamp` signed/unsigned attribute
/// (1.2.840.113549.1.9.16.2.14).
const OID_SIGNATURE_TIME_STAMP: &[u8] = &[
    0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x09, 0x10, 0x02, 0x0e,
];

/// Build the `unsignedAttrs` SET body carrying a `signature-time-stamp`
/// attribute whose value is the timestamp token (a ContentInfo). `tst` is the
/// full ContentInfo TLV extracted from the TSA's TimeStampResp.
fn build_unsigned_attrs_timestamp(tst: &[u8]) -> Vec<u8> {
    der(
        SEQ,
        &[der(OID, OID_SIGNATURE_TIME_STAMP), der(SET, tst)].concat(),
    )
}

/// Build an RFC 3161 `TimeStampReq` over the SHA-256 of the signature value,
/// with a fresh nonce and `certReq = true` (ask the TSA to include its cert).
fn build_time_stamp_req(signature_hash: &[u8]) -> Result<Vec<u8>> {
    let mut nonce = [0u8; 8];
    getrandom::getrandom(&mut nonce)
        .map_err(|_| invalid_data("getrandom failed for timestamp nonce"))?;
    let message_imprint = der(
        SEQ,
        &[der(SEQ, &der(OID, OID_SHA256)), der(OCTET, signature_hash)].concat(),
    );
    let req = der(
        SEQ,
        &[
            der(INT, &[1]),
            message_imprint,
            der(INT, &nonce),
            der(0x01, &[0xff]), // certReq BOOLEAN TRUE
        ]
        .concat(),
    );
    Ok(req)
}

/// Extract the `timeStampToken` (a ContentInfo TLV) from a `TimeStampResp`.
/// Returns the full ContentInfo bytes (tag + length + value) to embed as the
/// unsigned attribute value. `None` when the response carries no token (TSA
/// error or missing token).
fn extract_time_stamp_token(tsr: &[u8]) -> Option<Vec<u8>> {
    // TimeStampResp ::= SEQUENCE { status PKIStatusInfo, timeStampToken? ContentInfo }
    let (outer_tag, outer_content) = tlv_split(tsr)?;
    if outer_tag != SEQ {
        return None;
    }
    let mut rest = outer_content;
    let mut idx = 0;
    while !rest.is_empty() {
        let total = tlv_total_len(rest);
        if idx == 1 {
            // Second child is the timeStampToken ContentInfo.
            return Some(rest[..total].to_vec());
        }
        idx += 1;
        rest = &rest[total..];
    }
    None
}

/// (tag, content) of the DER TLV at the start of `bytes`.
fn tlv_split(bytes: &[u8]) -> Option<(u8, &[u8])> {
    let &tag = bytes.first()?;
    let len_byte = *bytes.get(1)?;
    let (len, header) = if len_byte < 0x80 {
        (len_byte as usize, 2)
    } else {
        let n = (len_byte & 0x7f) as usize;
        if !(1..=4).contains(&n) || bytes.len() < 2 + n {
            return None;
        }
        let mut l = 0usize;
        for i in 0..n {
            l = (l << 8) | bytes[2 + i] as usize;
        }
        (l, 2 + n)
    };
    let content = bytes.get(header..header + len)?;
    Some((tag, content))
}

/// Total byte length of the TLV at the start of `bytes` (header + content).
fn tlv_total_len(bytes: &[u8]) -> usize {
    let Some((_, content)) = tlv_split(bytes) else {
        return bytes.len();
    };
    let len_byte = match bytes.get(1) {
        Some(&b) => b,
        None => return bytes.len(),
    };
    let header = if len_byte < 0x80 {
        2
    } else {
        2 + (len_byte & 0x7f) as usize
    };
    header + content.len()
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

fn find_from(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    let start = from.min(haystack.len());
    haystack[start..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + start)
}

/// `YYYYMMDDHHMMSS` (UTC) for the signature `/M` date.
fn pdf_timestamp() -> String {
    crate::metadata::pdf_date_now_raw()
}

/// Build a visible-signature appearance Form XObject content stream in the
/// XObject's own `[0 0 w h]` coordinate space: an optional filled background,
/// an optional stroked border, and up to three lines of text (signer name,
/// reason, signing date) set in standard-14 Helvetica 9pt. Returns the
/// content bytes plus the (width, height) the caller should use as the
/// XObject `/BBox`.
fn build_appearance_stream(spec: &AppearanceSpec, name: &str, reason: &str) -> (Vec<u8>, f64, f64) {
    let w = (spec.rect.x1 - spec.rect.x0).max(1.0);
    let h = (spec.rect.y1 - spec.rect.y0).max(1.0);
    let mut ops = Vec::new();
    ops.extend_from_slice(b"q\n");
    if let Some((r, g, b)) = spec.bg {
        ops.extend_from_slice(format!("{r} {g} {b} rg 0 0 {w} {h} re f\n").as_bytes());
    }
    if let Some((r, g, b)) = spec.border {
        ops.extend_from_slice(format!("{r} {g} {b} RG 1 w 0 0 {w} {h} re S\n").as_bytes());
    }
    if spec.show_text || spec.show_date {
        ops.extend_from_slice(b"BT\n/Helv 9 Tf 0 0 0 rg\n");
        let mut y = h - 14.0;
        if spec.show_text && !name.is_empty() {
            ops.extend_from_slice(format!("1 0 0 1 8 {y} Tm (").as_bytes());
            escape_appearance_text(name, &mut ops);
            ops.extend_from_slice(b") Tj\n");
            y -= 12.0;
        }
        if spec.show_text && !reason.is_empty() {
            ops.extend_from_slice(format!("1 0 0 1 8 {y} Tm (").as_bytes());
            escape_appearance_text(reason, &mut ops);
            ops.extend_from_slice(b") Tj\n");
            y -= 12.0;
        }
        if spec.show_date {
            let date = pdf_timestamp();
            ops.extend_from_slice(format!("1 0 0 1 8 {y} Tm (").as_bytes());
            escape_appearance_text(&date, &mut ops);
            ops.extend_from_slice(b") Tj\n");
        }
        ops.extend_from_slice(b"ET\n");
    }
    ops.extend_from_slice(b"Q\n");
    (ops, w, h)
}

/// Escape a string into a PDF literal-string body, WinAnsi-ish-encoding each
/// char (code points > 0xFF fall back to `?`).
fn escape_appearance_text(s: &str, out: &mut Vec<u8>) {
    for ch in s.chars() {
        let b = if (ch as u32) <= 0xFF { ch as u8 } else { b'?' };
        match b {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'(' => out.extend_from_slice(b"\\("),
            b')' => out.extend_from_slice(b"\\)"),
            b'\r' => out.extend_from_slice(b"\\r"),
            _ => out.push(b),
        }
    }
}
