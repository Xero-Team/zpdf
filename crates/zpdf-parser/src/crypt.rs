//! PDF **Standard Security Handler** decryption.
//!
//! Implements the empty-user-password decryption path for the Standard
//! security handler: key derivation (PDF 1.7 §7.6.3, Algorithms 2 & 1) and the
//! RC4 cipher (V1/V2, R2–R4). AES (V4 `AESV2` / V5 `AESV3`) is detected and
//! reported as unsupported rather than producing garbage.
//!
//! Pure Rust, zero external dependencies — MD5 and RC4 are implemented inline.
//!
//! ## How it plugs in
//! [`Decryptor`] is built once at file-open time from the `/Encrypt` dictionary
//! and the first element of the trailer `/ID`. Every top-level object parsed
//! straight from the file (xref `InUse` entries) is then walked with
//! [`Decryptor::decrypt_object`], which RC4-decrypts every string and stream in
//! place using a per-object key. Objects pulled out of a `/Type /ObjStm`
//! compressed stream are **not** decrypted individually (the container stream
//! is), and the `/Encrypt` dictionary itself is never decrypted.

use std::sync::Arc;
use zpdf_core::{ObjectId, PdfDict, PdfObject, PdfString};

/// The 32-byte password-padding string from PDF 1.7 §7.6.3.3 (Algorithm 2).
const PAD: [u8; 32] = [
    0x28, 0xBF, 0x4E, 0x5E, 0x4E, 0x75, 0x8A, 0x41, 0x64, 0x00, 0x4E, 0x56, 0xFF, 0xFA, 0x01, 0x08,
    0x2E, 0x2E, 0x00, 0xB6, 0xD0, 0x68, 0x3E, 0x80, 0x2F, 0x0C, 0xA9, 0xFE, 0x64, 0x53, 0x69, 0x7A,
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Algo {
    /// RC4 (V1/V2).
    Rc4,
    /// AES-128 CBC (V4, crypt filter `AESV2`) — not yet implemented.
    AesV2,
    /// AES-256 CBC (V5/R6, crypt filter `AESV3`) — not yet implemented.
    AesV3,
}

/// A built Standard-security-handler decryptor for one file.
pub struct Decryptor {
    /// File-level encryption key (Algorithm 2 output), `n` bytes.
    key: Vec<u8>,
    algo: Algo,
    /// Revision (`/R`) — affects per-object key length & key derivation.
    revision: i64,
    /// The `/Encrypt` dictionary's own object id, which must never be
    /// decrypted (its strings are stored in the clear).
    encrypt_id: Option<ObjectId>,
}

impl Decryptor {
    /// Build a decryptor from the `/Encrypt` dictionary and the first element of
    /// the trailer `/ID`. Returns `Ok(None)` (with a warning) for security
    /// handlers we don't support yet (non-`Standard` filter, AES) so the caller
    /// degrades gracefully instead of emitting garbage.
    pub fn from_encrypt_dict(
        dict: &PdfDict,
        id_first: &[u8],
        encrypt_id: Option<ObjectId>,
    ) -> Option<Self> {
        let filter = dict.get_name("Filter").unwrap_or("");
        if filter != "Standard" {
            tracing::warn!("unsupported security handler /Filter {filter}; document will not decrypt");
            return None;
        }

        let v = dict.get_i64("V").unwrap_or(0);
        let r = dict.get_i64("R").unwrap_or(0);

        let o = string_bytes(dict, "O");
        let u = string_bytes(dict, "U");
        // /P is an integer bit-field, but some producers write it as a Real.
        let p = match dict.get("P") {
            Some(PdfObject::Integer(n)) => *n as i32,
            Some(PdfObject::Real(n)) => *n as i32,
            _ => 0,
        };
        // Default true (R<4 has no such key and always encrypts metadata).
        let encrypt_metadata = match dict.get("EncryptMetadata") {
            Some(PdfObject::Bool(b)) => *b,
            _ => true,
        };

        let algo = classify_algo(dict, v);
        if matches!(algo, Algo::AesV2 | Algo::AesV3) {
            tracing::warn!(
                "AES-encrypted PDF (V={v} R={r}) is not yet supported; document will not decrypt"
            );
            return None;
        }
        if v >= 5 {
            tracing::warn!("encryption V={v} (AES-256) is not yet supported");
            return None;
        }

        let length_bits = key_length_bits(dict, v);
        let key = compute_key_rc4(&o, p, id_first, r, length_bits, encrypt_metadata);

        // Diagnostic: a correct empty-user-password key reproduces /U
        // (Algorithm 4/6). A mismatch means a wrong key was derived (e.g. the
        // document needs a password, or /ID//P were malformed) — surface it
        // instead of silently rendering garbage. We still proceed, so a quirk in
        // this check can never break a document that would otherwise decrypt.
        if !validate_user_password(&key, &u, id_first, r) {
            tracing::warn!(
                "encryption key did not validate against /U (V={v} R={r}); the PDF may require a \
                 password — decrypted content may be garbage"
            );
        }

        Some(Self {
            key,
            algo,
            revision: r,
            encrypt_id,
        })
    }

    /// Recursively decrypt every string and stream contained in `obj`, in place,
    /// using the per-object key derived from `id`. No-op for the `/Encrypt`
    /// object itself.
    pub fn decrypt_object(&self, obj: &mut PdfObject, id: ObjectId) {
        if Some(id) == self.encrypt_id {
            return;
        }
        self.walk(obj, id);
    }

    /// Decrypt a raw stream byte buffer with the per-object key for `id`. Used
    /// for the `/Type /ObjStm` container, which is decrypted directly (not via
    /// [`decrypt_object`](Self::decrypt_object)) before its filter pipeline.
    pub fn decrypt_stream_bytes(&self, id: ObjectId, data: &[u8]) -> Vec<u8> {
        self.decrypt(id, data)
    }

    fn walk(&self, obj: &mut PdfObject, id: ObjectId) {
        match obj {
            PdfObject::String(s) => {
                *s = PdfString(self.decrypt(id, &s.0));
            }
            PdfObject::Array(a) => {
                for o in a.iter_mut() {
                    self.walk(o, id);
                }
            }
            PdfObject::Dict(d) => {
                for v in d.0.values_mut() {
                    self.walk(v, id);
                }
            }
            PdfObject::Stream(s) => {
                // Cross-reference streams are never encrypted (PDF 1.7 §7.6.1).
                let is_xref = s.dict.get_name("Type").map(|t| t == "XRef").unwrap_or(false);
                if !is_xref {
                    let dec = self.decrypt(id, &s.data);
                    s.data = Arc::from(dec);
                }
                for v in s.dict.0.values_mut() {
                    self.walk(v, id);
                }
            }
            // Refs are followed (and decrypted) when resolved; scalars are plain.
            _ => {}
        }
    }

    /// Per-object key derivation (Algorithm 1) + cipher application.
    fn decrypt(&self, id: ObjectId, data: &[u8]) -> Vec<u8> {
        let obj_key = self.object_key(id);
        match self.algo {
            Algo::Rc4 => rc4(&obj_key, data),
            // Unreachable: AES handlers return None from `from_encrypt_dict`.
            Algo::AesV2 | Algo::AesV3 => data.to_vec(),
        }
    }

    /// Algorithm 1: object key = MD5(file_key || obj_num[3 LE] || gen[2 LE]
    /// [|| "sAlT" for AES]), truncated to min(n+5, 16) bytes.
    fn object_key(&self, id: ObjectId) -> Vec<u8> {
        // V5 (R6) uses the file key directly with no per-object derivation.
        if self.revision >= 6 {
            return self.key.clone();
        }
        let mut input = Vec::with_capacity(self.key.len() + 9);
        input.extend_from_slice(&self.key);
        let num = id.0.to_le_bytes();
        input.extend_from_slice(&num[..3]);
        let gen = id.1.to_le_bytes();
        input.extend_from_slice(&gen[..2]);
        if matches!(self.algo, Algo::AesV2 | Algo::AesV3) {
            input.extend_from_slice(b"sAlT");
        }
        let hash = md5(&input);
        let n = (self.key.len() + 5).min(16);
        hash[..n].to_vec()
    }
}

/// Effective file-key length, in bits. For V≥4 the key size of an RC4 crypt
/// filter lives in `/CF/<StmF>/Length` and is expressed in **bytes** (ISO 32000
/// §7.6.5), distinct from the document-level `/Encrypt /Length` (in **bits**,
/// §7.6.3). Prefer the crypt-filter length; fall back to the document length,
/// then to 40.
fn key_length_bits(dict: &PdfDict, v: i64) -> i64 {
    if v >= 4 {
        let stmf = dict.get_name("StmF").unwrap_or("Identity");
        if stmf != "Identity" {
            if let Some(len) = dict
                .get_dict("CF")
                .ok()
                .and_then(|cf| cf.get_dict(stmf).ok())
                .and_then(|f| f.get_i64("Length").ok())
            {
                // Spec says bytes (5..=16); a value clearly too large for bytes is
                // a non-conforming producer that wrote bits — accept either.
                return if len <= 32 { len * 8 } else { len };
            }
        }
    }
    dict.get_i64("Length").unwrap_or(40)
}

/// Validate the derived file key against `/U` for the empty user password.
/// R2: `/U` == RC4(key, PAD) (Algorithm 4). R≥3: the first 16 bytes of `/U`
/// match the Algorithm 5 computation (Algorithm 6). Returns `true` when `/U` is
/// absent (nothing to check against).
fn validate_user_password(key: &[u8], u: &[u8], id_first: &[u8], r: i64) -> bool {
    if u.is_empty() {
        return true;
    }
    if r == 2 {
        return rc4(key, &PAD) == u;
    }
    // R≥3 (Algorithm 5): MD5(PAD || ID), RC4 with the key, then 19 more RC4
    // passes whose key is the file key XORed with the 1-based iteration index.
    let mut input = Vec::with_capacity(PAD.len() + id_first.len());
    input.extend_from_slice(&PAD);
    input.extend_from_slice(id_first);
    let mut x = rc4(key, &md5(&input));
    for i in 1u8..=19 {
        let step_key: Vec<u8> = key.iter().map(|b| b ^ i).collect();
        x = rc4(&step_key, &x);
    }
    // Only the first 16 bytes of /U are deterministic; the rest is padding.
    u.len() >= 16 && x.len() >= 16 && x[..16] == u[..16]
}

/// Decide the cipher. For V<4 it's always RC4. For V≥4 the `/StmF` crypt filter
/// (in `/CF`) names `V2` (RC4), `AESV2`, or `AESV3`.
fn classify_algo(dict: &PdfDict, v: i64) -> Algo {
    if v < 4 {
        return Algo::Rc4;
    }
    // V4/V5: look up the stream crypt filter's /CFM.
    let stmf = dict.get_name("StmF").unwrap_or("Identity");
    if stmf == "Identity" {
        return Algo::Rc4; // no stream encryption; harmless default
    }
    let cfm = dict
        .get_dict("CF")
        .ok()
        .and_then(|cf| cf.get_dict(stmf).ok())
        .and_then(|f| f.get_name("CFM").ok())
        .unwrap_or("V2");
    match cfm {
        "AESV2" => Algo::AesV2,
        "AESV3" => Algo::AesV3,
        _ => Algo::Rc4,
    }
}

/// Algorithm 2 (RC4/AES key, revisions 2–4): derive the file encryption key
/// from the empty user password.
fn compute_key_rc4(
    o: &[u8],
    p: i32,
    id_first: &[u8],
    r: i64,
    length_bits: i64,
    encrypt_metadata: bool,
) -> Vec<u8> {
    let n = if r == 2 {
        5
    } else {
        ((length_bits / 8).clamp(5, 16)) as usize
    };

    let mut input = Vec::with_capacity(32 + 32 + 4 + id_first.len() + 4);
    // Step (a): padded empty password is exactly the 32-byte pad.
    input.extend_from_slice(&PAD);
    // Step (b): the /O entry, padded/truncated to 32 bytes.
    let mut o32 = [0u8; 32];
    let take = o.len().min(32);
    o32[..take].copy_from_slice(&o[..take]);
    input.extend_from_slice(&o32);
    // Step (c): /P as 4 bytes, low-order byte first.
    input.extend_from_slice(&(p as u32).to_le_bytes());
    // Step (d): the first file identifier.
    input.extend_from_slice(id_first);
    // Step (e): R≥4 with EncryptMetadata=false appends 0xFFFFFFFF.
    if r >= 4 && !encrypt_metadata {
        input.extend_from_slice(&[0xff, 0xff, 0xff, 0xff]);
    }

    let mut hash = md5(&input);
    // Step (f), R≥3: 50 extra MD5 passes over the first n bytes.
    if r >= 3 {
        for _ in 0..50 {
            hash = md5(&hash[..n]);
        }
    }
    hash[..n].to_vec()
}

/// Read a PDF string entry's raw bytes from a dict (empty if absent/non-string).
fn string_bytes(dict: &PdfDict, key: &str) -> Vec<u8> {
    match dict.get(key) {
        Some(PdfObject::String(s)) => s.0.clone(),
        _ => Vec::new(),
    }
}

// ----------------------------------------------------------------------------
// RC4 stream cipher
// ----------------------------------------------------------------------------

/// RC4 encrypt/decrypt (symmetric). Returns the input unchanged if `key` is
/// empty (degenerate, should not happen for a valid handler).
fn rc4(key: &[u8], data: &[u8]) -> Vec<u8> {
    if key.is_empty() {
        return data.to_vec();
    }
    let mut s: [u8; 256] = [0; 256];
    for (i, b) in s.iter_mut().enumerate() {
        *b = i as u8;
    }
    let mut j: u8 = 0;
    for i in 0..256 {
        j = j
            .wrapping_add(s[i])
            .wrapping_add(key[i % key.len()]);
        s.swap(i, j as usize);
    }

    let mut out = Vec::with_capacity(data.len());
    let mut i: u8 = 0;
    let mut j: u8 = 0;
    for &byte in data {
        i = i.wrapping_add(1);
        j = j.wrapping_add(s[i as usize]);
        s.swap(i as usize, j as usize);
        let k = s[(s[i as usize].wrapping_add(s[j as usize])) as usize];
        out.push(byte ^ k);
    }
    out
}

// ----------------------------------------------------------------------------
// MD5 (RFC 1321) — one-shot
// ----------------------------------------------------------------------------

/// Per-round left-rotation amounts.
const MD5_S: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

/// Precomputed `floor(2^32 * abs(sin(i+1)))` constants.
const MD5_K: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

/// Compute the MD5 digest of `data`.
pub fn md5(data: &[u8]) -> [u8; 16] {
    let mut a0: u32 = 0x67452301;
    let mut b0: u32 = 0xefcdab89;
    let mut c0: u32 = 0x98badcfe;
    let mut d0: u32 = 0x10325476;

    // Pad: append 0x80, then zeros, then the 64-bit little-endian bit length.
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut m = [0u32; 16];
        for (i, word) in m.iter_mut().enumerate() {
            *word = u32::from_le_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }

        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((b & c) | (!b & d), i),
                16..=31 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f
                .wrapping_add(a)
                .wrapping_add(MD5_K[i])
                .wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(MD5_S[i]));
        }

        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }

    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&a0.to_le_bytes());
    out[4..8].copy_from_slice(&b0.to_le_bytes());
    out[8..12].copy_from_slice(&c0.to_le_bytes());
    out[12..16].copy_from_slice(&d0.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// Validates the full RC4-40 key-derivation pipeline against reference
    /// values computed independently from tests/test4/1.pdf (/V 1 /R 2
    /// /Length 40, empty user password). Self-contained: no file needed.
    #[test]
    fn test4_rc4_key_derivation_oracle() {
        let o = unhex("c5e5cd078ac4b56637f8a5d03a1ecd261ecf59fdcd8b50944ba1bb0e9e95ebfb");
        let u = unhex("ffe4a8e86d2951800946f19d21089e1a71ca3d813608e586339bab72aa28206a");
        let id0 = unhex("1a6dd6c3b3c1957a915bb98dbf691ce0");
        let p: i32 = -64;

        // Algorithm 2 → file encryption key.
        let key = compute_key_rc4(&o, p, &id0, 2, 40, true);
        assert_eq!(hex(&key), "b374aaeaf4", "file key (Algorithm 2)");

        // Algorithm 4 (R2): RC4(file_key, PAD) must equal stored /U.
        assert_eq!(rc4(&key, &PAD), u, "user-password validation (Algorithm 4)");
        assert!(
            validate_user_password(&key, &u, &id0, 2),
            "validate_user_password should accept the correct R2 key"
        );
        // A wrong key (empty /ID) must NOT validate.
        let wrong = compute_key_rc4(&o, p, &[], 2, 40, true);
        assert!(
            !validate_user_password(&wrong, &u, &id0, 2),
            "validate_user_password should reject a wrong key"
        );

        // Algorithm 1 → per-object key for the page-1 content stream (1652, 0).
        let dec = Decryptor {
            key,
            algo: Algo::Rc4,
            revision: 2,
            encrypt_id: None,
        };
        let objkey = dec.object_key(ObjectId(1652, 0));
        assert_eq!(hex(&objkey), "30dadd6463d5f9765abc", "per-object key (Algorithm 1)");
    }

    #[test]
    fn md5_known_answers() {
        // RFC 1321 test suite.
        assert_eq!(hex(&md5(b"")), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex(&md5(b"a")), "0cc175b9c0f1b6a831c399e269772661");
        assert_eq!(hex(&md5(b"abc")), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            hex(&md5(b"message digest")),
            "f96b697d7cb7938d525a2f31aaf161d0"
        );
        assert_eq!(
            hex(&md5(b"abcdefghijklmnopqrstuvwxyz")),
            "c3fcd3d76192e4007dfb496cca67e13b"
        );
        // Exercises the multi-block (>56 byte) padding path.
        assert_eq!(
            hex(&md5(
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"
            )),
            "d174ab98d277d9f5a5611c2c9f419d9f"
        );
    }

    #[test]
    fn v4_rc4_key_length_from_crypt_filter() {
        use zpdf_core::{PdfDict, PdfName, PdfObject};
        // A /V 4 /R 4 RC4-128 handler that records its key size only in the
        // crypt filter (/CF/StdCF/Length 16 bytes), with no top-level /Length.
        let mut stdcf = PdfDict::new();
        stdcf.insert(PdfName::new("CFM"), PdfObject::Name(PdfName::new("V2")));
        stdcf.insert(PdfName::new("Length"), PdfObject::Integer(16)); // bytes
        let mut cf = PdfDict::new();
        cf.insert(PdfName::new("StdCF"), PdfObject::Dict(stdcf));
        let mut dict = PdfDict::new();
        dict.insert(PdfName::new("CF"), PdfObject::Dict(cf));
        dict.insert(PdfName::new("StmF"), PdfObject::Name(PdfName::new("StdCF")));
        dict.insert(PdfName::new("V"), PdfObject::Integer(4));

        // Must read 16 bytes → 128 bits from the crypt filter, not default to 40.
        assert_eq!(key_length_bits(&dict, 4), 128);
        // And the cipher must classify as RC4 (CFM == V2).
        assert_eq!(classify_algo(&dict, 4), Algo::Rc4);
    }

    #[test]
    fn rc4_known_answers() {
        // Classic RC4 test vectors (key "Key", plaintext "Plaintext").
        let ct = rc4(b"Key", b"Plaintext");
        assert_eq!(hex(&ct), "bbf316e8d940af0ad3");
        // Symmetry: decrypting the ciphertext recovers the plaintext.
        assert_eq!(rc4(b"Key", &ct), b"Plaintext");

        let ct = rc4(b"Wiki", b"pedia");
        assert_eq!(hex(&ct), "1021bf0420");

        let ct = rc4(b"Secret", b"Attack at dawn");
        assert_eq!(hex(&ct), "45a01f645fc35b383552544b9bf5");
    }
}
