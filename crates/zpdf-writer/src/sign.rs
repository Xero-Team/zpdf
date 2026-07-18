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
}

/// Optional signature metadata written into the signature dictionary.
#[derive(Debug, Clone, Default)]
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
            b"<< /Type /Sig /Filter /Adobe.PPKLite /SubFilter /adbe.pkcs7.detached",
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
        // Invisible: zero rect + Hidden=0, Print flag set (bit 3, value 4).
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
        field.insert(PdfName::new("P"), PdfObject::Ref(page_id));
        let (field_num, _) = self.try_add_object(&PdfObject::Dict(field))?;
        let field_ref = ObjectId(field_num, 0);

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
            if v > 9_999_999_999 {
                return Err(invalid_data("file too large for ByteRange slots").into());
            }
            let slot = br_open + 1 + i * 11;
            buf[slot..slot + 10].copy_from_slice(format!("{v:010}").as_bytes());
        }

        let mut hasher = Sha256::new();
        hasher.update(&buf[..contents_start]);
        hasher.update(&buf[contents_end..]);
        let digest = hasher.finalize();

        let cms = build_cms(&digest, certificate_der, key)?;
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

/// Build the detached CMS `SignedData` over `digest` (the SHA-256 of the
/// signed byte ranges), embedding `cert` and signing with `key`.
fn build_cms(digest: &[u8], cert: &[u8], key: &SigningKey) -> Result<Vec<u8>> {
    let digest_alg = der(SEQ, &der(OID, OID_SHA256));

    // Signed attributes: contentType(data) then messageDigest — this is also
    // their DER SET-of lexicographic order (identical prefixes; 0x03 < 0x04).
    let ct_attr = der(
        SEQ,
        &[der(OID, OID_CONTENT_TYPE), der(SET, &der(OID, OID_DATA))].concat(),
    );
    let md_attr = der(
        SEQ,
        &[der(OID, OID_MESSAGE_DIGEST), der(SET, &der(OCTET, digest))].concat(),
    );
    let attrs = [ct_attr, md_attr].concat();

    // The signature covers the SET-encoded attributes (RFC 5652 §5.4); the
    // CMS itself stores the [0] IMPLICIT form.
    let signature = key.sign(&der(SET, &attrs))?;

    let signer_info = der(
        SEQ,
        &[
            der(INT, &[1]),
            der(SEQ, &[]), // sid: not read by verifiers that scan by OID
            digest_alg.clone(),
            der(CTX0, &attrs), // signedAttrs [0] IMPLICIT
            der(SEQ, &der(OID, key.sig_alg_oid())),
            der(OCTET, &signature),
        ]
        .concat(),
    );
    let signed_data = der(
        SEQ,
        &[
            der(INT, &[1]),
            der(SET, &digest_alg),
            der(SEQ, &der(OID, OID_DATA)),
            der(CTX0, cert), // certificates [0] IMPLICIT
            der(SET, &signer_info),
        ]
        .concat(),
    );
    Ok(der(
        SEQ,
        &[der(OID, OID_SIGNED_DATA), der(CTX0, &signed_data)].concat(),
    ))
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
