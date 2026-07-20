//! PDF **Standard Security Handler** decryption.
//!
//! Authenticates a user or owner password (empty by default) and decrypts the
//! Standard security handler:
//! - RC4 (V1/V2, R2–R4) with MD5 key derivation (PDF 1.7 §7.6.3, Algorithms 2 & 1)
//! - AES-128-CBC (V4/R4, crypt filter `AESV2`) — per-object key = MD5(file key
//!   ‖ objnum ‖ gen ‖ `sAlT`); the first 16 bytes of each payload are the IV
//! - AES-256-CBC (V5, R5/R6, crypt filter `AESV3`) — file key recovered from
//!   `/UE` (or `/OE`) per ISO 32000-2 Algorithm 2.A, with the R6 hardened hash
//!   (Algorithm 2.B); no per-object key derivation
//!
//! MD5 and RC4 are implemented inline; AES-CBC and SHA-2 come from the
//! pure-Rust RustCrypto crates (`aes`, `cbc`, `sha2`). Zero C/C++ deps.
//!
//! ## How it plugs in
//! [`Decryptor`] is built once at file-open time from the `/Encrypt` dictionary
//! and the first element of the trailer `/ID`. Every top-level object parsed
//! straight from the file (xref `InUse` entries) is then walked with
//! [`Decryptor::decrypt_object`], which decrypts every string and stream in
//! place (streams with the `/StmF` cipher, strings with the `/StrF` cipher —
//! either may be `Identity`). Objects pulled out of a `/Type /ObjStm`
//! compressed stream are **not** decrypted individually (the container stream
//! is), and the `/Encrypt` dictionary itself is never decrypted.

use aes::cipher::{generic_array::GenericArray, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use sha2::Digest;
use std::sync::Arc;
use zpdf_core::{ObjectId, PdfDict, PdfObject, PdfString};

/// The 32-byte password-padding string from PDF 1.7 §7.6.3.3 (Algorithm 2).
const PAD: [u8; 32] = [
    0x28, 0xBF, 0x4E, 0x5E, 0x4E, 0x75, 0x8A, 0x41, 0x64, 0x00, 0x4E, 0x56, 0xFF, 0xFA, 0x01, 0x08,
    0x2E, 0x2E, 0x00, 0xB6, 0xD0, 0x68, 0x3E, 0x80, 0x2F, 0x0C, 0xA9, 0xFE, 0x64, 0x53, 0x69, 0x7A,
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Algo {
    /// No encryption for this class (`/StmF` or `/StrF` is `Identity`, or the
    /// crypt filter's `/CFM` is `None`).
    Identity,
    /// RC4 (V1/V2, or a V4 crypt filter with `/CFM /V2`).
    Rc4,
    /// AES-128 CBC (V4, crypt filter `AESV2`).
    AesV2,
    /// AES-256 CBC (V5 R5/R6, crypt filter `AESV3`).
    AesV3,
}

/// Outcome of attempting to build a [`Decryptor`] for a document.
pub enum BuildResult {
    /// A usable decryptor (the password authenticated, or the empty-password
    /// best-effort path was taken for an unvalidated RC4 document).
    Decryptor(Decryptor),
    /// No decryption: an unsupported security handler, or a V5 document whose
    /// empty password did not validate. The document opens undecrypted.
    Degrade,
    /// A non-empty password was supplied but authenticated as neither the user
    /// nor the owner password.
    WrongPassword,
}

/// A built Standard-security-handler decryptor for one file.
pub struct Decryptor {
    /// File-level encryption key (Algorithm 2 or 2.A output), `n` bytes.
    key: Vec<u8>,
    /// Cipher for stream payloads (`/StmF` crypt filter).
    stm_algo: Algo,
    /// Cipher for strings (`/StrF` crypt filter).
    str_algo: Algo,
    /// The `/Encrypt` dictionary's own object id, which must never be
    /// decrypted (its strings are stored in the clear). `None` when the
    /// trailer carries a direct (non-reference) `/Encrypt` dict.
    encrypt_id: Option<ObjectId>,
    /// `/EncryptMetadata` (default true). When false, the document-level
    /// `/Type /Metadata` stream payload is stored in the clear and must not be
    /// "decrypted" (which would corrupt it).
    encrypt_metadata: bool,
}

impl Decryptor {
    /// Build a decryptor from the `/Encrypt` dictionary, the first element of the
    /// trailer `/ID`, and a user/owner password (empty for the default open).
    ///
    /// The password is authenticated against `/U` (user) and `/O` (owner). A
    /// non-empty password that matches neither yields [`BuildResult::WrongPassword`].
    /// The empty-password default preserves the lenient behavior: an RC4 document
    /// whose `/U` does not validate still opens (best-effort, with a warning),
    /// since malformed-but-empty-password files are common.
    pub fn from_encrypt_dict(
        dict: &PdfDict,
        id_first: &[u8],
        encrypt_id: Option<ObjectId>,
        password: &[u8],
    ) -> BuildResult {
        let filter = dict.get_name("Filter").unwrap_or("");
        if filter != "Standard" {
            tracing::warn!(
                "unsupported security handler /Filter {filter}; document will not decrypt"
            );
            return BuildResult::Degrade;
        }

        let v = dict.get_i64("V").unwrap_or(0);
        let r = dict.get_i64("R").unwrap_or(0);

        // Per-class ciphers. V<4: one document-wide RC4 cipher for everything.
        // V4/V5: `/CF` crypt filters selected by `/StmF` (streams) and `/StrF`
        // (strings); per spec the default for each is `Identity` (no
        // encryption for that class).
        let (stm_algo, str_algo) = if v >= 4 {
            (
                algo_for_filter(dict, dict.get_name("StmF").unwrap_or("Identity")),
                algo_for_filter(dict, dict.get_name("StrF").unwrap_or("Identity")),
            )
        } else {
            (Algo::Rc4, Algo::Rc4)
        };

        // Default true (R<4 has no such key and always encrypts metadata).
        let encrypt_metadata = match dict.get("EncryptMetadata") {
            Some(PdfObject::Bool(b)) => *b,
            _ => true,
        };

        let key = if v >= 5 {
            // V5 (AESV3): ISO 32000-2 Algorithm 2.A. Validates the password and
            // recovers the 32-byte file key from /UE or /OE. No best-effort path —
            // the key can only come from a correct password.
            match compute_key_v5(dict, r, password) {
                Some(k) => k,
                None if password.is_empty() => return BuildResult::Degrade,
                None => return BuildResult::WrongPassword,
            }
        } else {
            let o = string_bytes(dict, "O");
            let u = string_bytes(dict, "U");
            // /P is an integer bit-field, but some producers write it as a Real.
            let p = match dict.get("P") {
                Some(PdfObject::Integer(n)) => *n as i32,
                Some(PdfObject::Real(n)) => *n as i32,
                _ => 0,
            };
            // AESV2 always uses a 128-bit key regardless of what /Length says.
            let length_bits = if stm_algo == Algo::AesV2 || str_algo == Algo::AesV2 {
                128
            } else {
                key_length_bits(dict, v)
            };

            match authenticate_rc4(
                password,
                &o,
                &u,
                p,
                id_first,
                r,
                length_bits,
                encrypt_metadata,
            ) {
                Ok(key) => key,
                // Authentication failed. Refuse only when a non-empty password
                // was supplied AND there was a /U to check it against — that is
                // an unambiguously wrong password. Otherwise open best-effort
                // with the derived key (the lenient empty-password default, or a
                // malformed document with no /U), preserving prior behavior.
                Err(_) if !password.is_empty() && !u.is_empty() => {
                    return BuildResult::WrongPassword;
                }
                Err(best_effort_key) => {
                    if u.is_empty() && !password.is_empty() {
                        tracing::warn!(
                            "encrypted document has no /U to authenticate the password against \
                             (V={v} R={r}); proceeding with the supplied password unverified"
                        );
                    } else {
                        tracing::warn!(
                            "encryption key did not validate against /U (V={v} R={r}); the PDF \
                             may require a password — decrypted content may be garbage"
                        );
                    }
                    best_effort_key
                }
            }
        };

        BuildResult::Decryptor(Self {
            key,
            stm_algo,
            str_algo,
            encrypt_id,
            encrypt_metadata,
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
        self.decrypt(id, data, self.stm_algo)
    }

    /// **Encrypt** a stream payload for object `id` (the write-side inverse of
    /// [`Self::decrypt_stream_bytes`]). Used by the incremental writer to add
    /// objects to an already-encrypted document with its existing key.
    pub fn encrypt_stream_bytes(&self, id: ObjectId, data: &[u8]) -> Vec<u8> {
        self.encrypt(id, data, self.stm_algo)
    }

    /// **Encrypt** a string payload for object `id`.
    pub fn encrypt_string_bytes(&self, id: ObjectId, data: &[u8]) -> Vec<u8> {
        self.encrypt(id, data, self.str_algo)
    }

    /// Recursively encrypt every string in `obj` in place with the per-object
    /// key for `id`. Stream payloads are NOT touched (they are encrypted
    /// separately via [`Self::encrypt_stream_bytes`], since writers carry the
    /// payload out-of-band).
    pub fn encrypt_object_strings(&self, obj: &mut PdfObject, id: ObjectId) {
        match obj {
            PdfObject::String(s) if self.str_algo != Algo::Identity => {
                *s = PdfString(self.encrypt(id, &s.0, self.str_algo));
            }
            PdfObject::String(_) => {}
            PdfObject::Array(a) => {
                for o in a.iter_mut() {
                    self.encrypt_object_strings(o, id);
                }
            }
            PdfObject::Dict(d) => {
                for v in d.0.values_mut() {
                    self.encrypt_object_strings(v, id);
                }
            }
            PdfObject::Stream(s) => {
                for v in s.dict.0.values_mut() {
                    self.encrypt_object_strings(v, id);
                }
            }
            _ => {}
        }
    }

    /// Per-object key derivation + cipher application, encrypt direction.
    fn encrypt(&self, id: ObjectId, data: &[u8], algo: Algo) -> Vec<u8> {
        match algo {
            Algo::Identity => data.to_vec(),
            // RC4 is symmetric.
            Algo::Rc4 => rc4(&self.object_key(id, algo), data),
            Algo::AesV2 | Algo::AesV3 => aes_cbc_encrypt(&self.object_key(id, algo), data),
        }
    }

    fn walk(&self, obj: &mut PdfObject, id: ObjectId) {
        match obj {
            PdfObject::String(s) if self.str_algo != Algo::Identity => {
                *s = PdfString(self.decrypt(id, &s.0, self.str_algo));
            }
            PdfObject::String(_) => {}
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
                let typ = s.dict.get_name("Type").unwrap_or("");
                // Cross-reference streams are never encrypted (PDF 1.7 §7.6.1).
                let is_xref = typ == "XRef";
                // /EncryptMetadata false: metadata stream payloads are stored
                // in the clear; "decrypting" them would corrupt the XMP.
                // Limitation: only streams self-identifying as /Type /Metadata
                // are detectable here (we don't know if this object is the
                // catalog's /Metadata target) — that covers conforming files.
                let plain_meta = !self.encrypt_metadata && typ == "Metadata";
                if !is_xref && !plain_meta && self.stm_algo != Algo::Identity {
                    let dec = self.decrypt(id, &s.data, self.stm_algo);
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
    fn decrypt(&self, id: ObjectId, data: &[u8], algo: Algo) -> Vec<u8> {
        match algo {
            Algo::Identity => data.to_vec(),
            Algo::Rc4 => rc4(&self.object_key(id, algo), data),
            Algo::AesV2 | Algo::AesV3 => aes_cbc_decrypt(&self.object_key(id, algo), data),
        }
    }

    /// Algorithm 1: object key = MD5(file_key || obj_num[3 LE] || gen[2 LE]
    /// [|| "sAlT" for AESV2]), truncated to min(n+5, 16) bytes. AESV3 (V5) has
    /// no per-object derivation — the 32-byte file key is used directly.
    fn object_key(&self, id: ObjectId, algo: Algo) -> Vec<u8> {
        if algo == Algo::AesV3 {
            return self.key.clone();
        }
        let mut input = Vec::with_capacity(self.key.len() + 9);
        input.extend_from_slice(&self.key);
        let num = id.0.to_le_bytes();
        input.extend_from_slice(&num[..3]);
        let gen = id.1.to_le_bytes();
        input.extend_from_slice(&gen[..2]);
        if algo == Algo::AesV2 {
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

/// Validate the derived file key against `/U` (Algorithm 6).
/// R2: `/U` == RC4(key, PAD) (Algorithm 4). R≥3: the first 16 bytes of `/U`
/// match the Algorithm 5 computation. Returns `false` when `/U` is absent —
/// there is nothing to authenticate against, which the caller handles as a
/// best-effort open rather than a confirmed match.
fn validate_user_password(key: &[u8], u: &[u8], id_first: &[u8], r: i64) -> bool {
    if u.is_empty() {
        return false;
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

/// Map a `/StmF`-or-`/StrF` crypt-filter name to a cipher. `Identity` (also the
/// spec default) means no encryption for that class. Otherwise the named filter
/// in `/CF` declares its method via `/CFM`: `V2` (RC4), `AESV2`, `AESV3`, or
/// `None`.
fn algo_for_filter(dict: &PdfDict, filter_name: &str) -> Algo {
    if filter_name == "Identity" {
        return Algo::Identity;
    }
    let cfm = dict
        .get_dict("CF")
        .ok()
        .and_then(|cf| cf.get_dict(filter_name).ok())
        .and_then(|f| f.get_name("CFM").ok())
        .unwrap_or("V2");
    match cfm {
        "AESV2" => Algo::AesV2,
        "AESV3" => Algo::AesV3,
        "None" => Algo::Identity,
        _ => Algo::Rc4,
    }
}

/// Pad a password to 32 bytes per Algorithm 2 step (a): the first ≤32 password
/// bytes followed by the standard 32-byte PAD, truncated to 32.
fn pad_password(password: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let take = password.len().min(32);
    out[..take].copy_from_slice(&password[..take]);
    out[take..].copy_from_slice(&PAD[..32 - take]);
    out
}

/// The RC4/AES-128 key-derivation byte length `n` for the given revision.
fn rc4_key_len(r: i64, length_bits: i64) -> usize {
    if r == 2 {
        5
    } else {
        (length_bits / 8).clamp(5, 16) as usize
    }
}

/// Authenticate `password` for an RC4/AES-128 document: try it as the user
/// password (Algorithm 6), then as the owner password (Algorithm 7, which
/// recovers the user password from `/O`). `Ok(key)` is a validated key; `Err(key)`
/// carries the user-password-derived key as a best-effort fallback (for the
/// lenient empty-password open, or a malformed document with no `/U` to check).
#[allow(clippy::too_many_arguments)]
fn authenticate_rc4(
    password: &[u8],
    o: &[u8],
    u: &[u8],
    p: i32,
    id_first: &[u8],
    r: i64,
    length_bits: i64,
    encrypt_metadata: bool,
) -> std::result::Result<Vec<u8>, Vec<u8>> {
    // User password (Algorithm 6).
    let key = compute_key_rc4(password, o, p, id_first, r, length_bits, encrypt_metadata);
    if validate_user_password(&key, u, id_first, r) {
        return Ok(key);
    }
    // Owner password (Algorithm 7): recover the user password from /O, then
    // run Algorithm 2 with it.
    let recovered = recover_user_password_rc4(password, o, r, length_bits);
    let owner_key = compute_key_rc4(&recovered, o, p, id_first, r, length_bits, encrypt_metadata);
    if validate_user_password(&owner_key, u, id_first, r) {
        return Ok(owner_key);
    }
    Err(key)
}

/// Algorithm 7: recover the (padded) user password from `/O` using the supplied
/// owner password. The owner key is derived as in Algorithm 3, then `/O` is
/// RC4-decrypted (a single pass for R2, 20 reverse-counter passes for R≥3).
fn recover_user_password_rc4(owner_password: &[u8], o: &[u8], r: i64, length_bits: i64) -> Vec<u8> {
    let n = rc4_key_len(r, length_bits);
    let mut hash = md5(&pad_password(owner_password));
    if r >= 3 {
        for _ in 0..50 {
            hash = md5(&hash[..n]);
        }
    }
    let owner_key = &hash[..n];

    let mut user = o.to_vec();
    if r == 2 {
        user = rc4(owner_key, &user);
    } else {
        for i in (0..=19u8).rev() {
            let step_key: Vec<u8> = owner_key.iter().map(|b| b ^ i).collect();
            user = rc4(&step_key, &user);
        }
    }
    user
}

/// Algorithm 2 (RC4/AES-128 key, revisions 2–4): derive the file encryption key
/// from the (padded) user password.
fn compute_key_rc4(
    password: &[u8],
    o: &[u8],
    p: i32,
    id_first: &[u8],
    r: i64,
    length_bits: i64,
    encrypt_metadata: bool,
) -> Vec<u8> {
    let n = rc4_key_len(r, length_bits);

    let mut input = Vec::with_capacity(32 + 32 + 4 + id_first.len() + 4);
    // Step (a): the padded user password.
    input.extend_from_slice(&pad_password(password));
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

// ----------------------------------------------------------------------------
// V5 (AES-256) key derivation — ISO 32000-2 §7.6.4.3.3/4, Algorithms 2.A & 2.B
// ----------------------------------------------------------------------------

/// Algorithm 2.A: validate `password` as the user password against `/U` and
/// recover the 32-byte file key from `/UE`; fall back to the owner password
/// (`/O` with the first 48 bytes of `/U` appended to the hash input) and `/OE`.
/// Returns `None` (with a warning) when neither validates — a wrong/missing
/// password.
fn compute_key_v5(dict: &PdfDict, r: i64, password: &[u8]) -> Option<Vec<u8>> {
    let o = string_bytes(dict, "O");
    let u = string_bytes(dict, "U");
    let oe = string_bytes(dict, "OE");
    let ue = string_bytes(dict, "UE");
    // ISO 32000-2 §7.6.4.3.3: the V5 password is UTF-8, SASLprep-normalized, and
    // truncated to at most 127 bytes before hashing. We apply the byte cap (the
    // common interop case); SASLprep normalization of non-ASCII passwords is not
    // performed (it would need a stringprep table — out of scope for now).
    let password = &password[..password.len().min(127)];

    // Algorithm 11 (user): /U = hash[32] || validation-salt[8] || key-salt[8].
    // On a validation hit but a broken /UE, fall through to the owner path.
    if u.len() >= 48 {
        let (vsalt, ksalt) = (&u[32..40], &u[40..48]);
        if hash_v5(r, password, vsalt, &[])[..] == u[..32] {
            let ik = hash_v5(r, password, ksalt, &[]);
            if let Some(key) = decrypt_file_key(&ik, &ue, "UE") {
                return Some(key);
            }
        }
    }
    // Algorithm 12 (owner): same layout, with U[0..48] appended to the input.
    if o.len() >= 48 && u.len() >= 48 {
        let u48 = &u[..48];
        let (vsalt, ksalt) = (&o[32..40], &o[40..48]);
        if hash_v5(r, password, vsalt, u48)[..] == o[..32] {
            let ik = hash_v5(r, password, ksalt, u48);
            if let Some(key) = decrypt_file_key(&ik, &oe, "OE") {
                return Some(key);
            }
        }
    }
    tracing::warn!(
        "V5/R{r} password validation failed (the PDF likely requires a password); \
         document will not decrypt"
    );
    None
}

/// Decrypt the 32-byte file key from `/UE` or `/OE`: AES-256-CBC with the
/// intermediate key, a zero IV, and no padding.
fn decrypt_file_key(intermediate: &[u8; 32], encrypted: &[u8], which: &str) -> Option<Vec<u8>> {
    if encrypted.len() != 32 {
        tracing::warn!(
            "/{which} must be 32 bytes, got {}; document will not decrypt",
            encrypted.len()
        );
        return None;
    }
    let mut buf = encrypted.to_vec();
    if !cbc_decrypt_in_place(intermediate, &[0u8; 16], &mut buf) {
        return None;
    }
    Some(buf)
}

/// The V5 password hash: SHA-256(password ‖ salt ‖ udata), hardened with
/// Algorithm 2.B for R6.
fn hash_v5(r: i64, password: &[u8], salt: &[u8], udata: &[u8]) -> [u8; 32] {
    let mut input = Vec::with_capacity(password.len() + salt.len() + udata.len());
    input.extend_from_slice(password);
    input.extend_from_slice(salt);
    input.extend_from_slice(udata);
    let initial: [u8; 32] = sha2::Sha256::digest(&input).into();
    if r >= 6 {
        hash_r6(initial, password, udata)
    } else {
        initial
    }
}

/// Algorithm 2.B (R6 hardened hash): iterate AES-128-CBC over 64 repetitions of
/// (password ‖ K ‖ udata), re-hashing K with SHA-256/384/512 chosen by the
/// first 16 bytes of the ciphertext mod 3. At least 64 rounds; stop once the
/// last ciphertext byte is ≤ (round − 32).
fn hash_r6(initial: [u8; 32], password: &[u8], udata: &[u8]) -> [u8; 32] {
    let mut k: Vec<u8> = initial.to_vec();
    let mut e_last: u8 = 0;
    let mut round: i64 = 0;
    while round < 64 || i64::from(e_last) > round - 32 {
        // K1 = 64 repetitions of (password || K || udata). Its length is always
        // a multiple of 16 (any unit length × 64 is a multiple of 64).
        let mut k1 = Vec::with_capacity(64 * (password.len() + k.len() + udata.len()));
        for _ in 0..64 {
            k1.extend_from_slice(password);
            k1.extend_from_slice(&k);
            k1.extend_from_slice(udata);
        }
        let e = aes128_cbc_encrypt_nopad(&k[..16], &k[16..32], &k1);
        e_last = *e.last().unwrap_or(&0);
        let m = e[..16].iter().map(|&b| u32::from(b)).sum::<u32>() % 3;
        k = match m {
            0 => sha2::Sha256::digest(&e).to_vec(),
            1 => sha2::Sha384::digest(&e).to_vec(),
            _ => sha2::Sha512::digest(&e).to_vec(),
        };
        round += 1;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&k[..32]);
    out
}

/// Read a PDF string entry's raw bytes from a dict (empty if absent/non-string).
fn string_bytes(dict: &PdfDict, key: &str) -> Vec<u8> {
    match dict.get(key) {
        Some(PdfObject::String(s)) => s.0.clone(),
        _ => Vec::new(),
    }
}

// ----------------------------------------------------------------------------
// AES-CBC (pure-Rust RustCrypto `aes` + `cbc`)
// ----------------------------------------------------------------------------

/// Decrypt an AES-CBC payload as stored in a PDF: the first 16 bytes are the
/// IV, the rest is ciphertext with PKCS#5 padding. The padding is stripped
/// defensively — on invalid padding the unpadded plaintext is kept (with a
/// warning) rather than truncated arbitrarily. A structurally impossible
/// payload (length not 16+16k) or a bad key length returns the input unchanged.
fn aes_cbc_decrypt(key: &[u8], data: &[u8]) -> Vec<u8> {
    if data.is_empty() {
        return Vec::new();
    }
    if data.len() < 16 || !(data.len() - 16).is_multiple_of(16) {
        tracing::warn!(
            "AES-CBC payload length {} is not 16+16k; leaving data unmodified",
            data.len()
        );
        return data.to_vec();
    }
    let (iv, ct) = data.split_at(16);
    let mut buf = ct.to_vec();
    if !cbc_decrypt_in_place(key, iv, &mut buf) {
        tracing::warn!(
            "invalid AES key length {}; leaving data unmodified",
            key.len()
        );
        return data.to_vec();
    }
    strip_pkcs5_padding(buf)
}

/// Strip PKCS#5/7 padding in place; on malformed padding keep the data and warn.
fn strip_pkcs5_padding(mut buf: Vec<u8>) -> Vec<u8> {
    let Some(&last) = buf.last() else { return buf };
    let pad = last as usize;
    if (1..=16).contains(&pad)
        && pad <= buf.len()
        && buf[buf.len() - pad..].iter().all(|&b| b == last)
    {
        buf.truncate(buf.len() - pad);
    } else {
        tracing::warn!("invalid PKCS#5 padding byte {last}; keeping unpadded data");
    }
    buf
}

/// AES-CBC decrypt `buf` in place with no padding handling. Key length selects
/// AES-128 vs AES-256. Returns `false` for an unsupported key or IV length.
fn cbc_decrypt_in_place(key: &[u8], iv: &[u8], buf: &mut [u8]) -> bool {
    debug_assert_eq!(buf.len() % 16, 0);
    match key.len() {
        16 => {
            let Ok(mut dec) = cbc::Decryptor::<aes::Aes128>::new_from_slices(key, iv) else {
                return false;
            };
            for block in buf.chunks_exact_mut(16) {
                dec.decrypt_block_mut(GenericArray::from_mut_slice(block));
            }
            true
        }
        32 => {
            let Ok(mut dec) = cbc::Decryptor::<aes::Aes256>::new_from_slices(key, iv) else {
                return false;
            };
            for block in buf.chunks_exact_mut(16) {
                dec.decrypt_block_mut(GenericArray::from_mut_slice(block));
            }
            true
        }
        _ => false,
    }
}

/// AES-128-CBC **encrypt** with no padding (input length must be a multiple of
/// 16). Used only by the R6 hardened hash (Algorithm 2.B).
fn aes128_cbc_encrypt_nopad(key: &[u8], iv: &[u8], data: &[u8]) -> Vec<u8> {
    debug_assert_eq!(data.len() % 16, 0);
    let mut buf = data.to_vec();
    let Ok(mut enc) = cbc::Encryptor::<aes::Aes128>::new_from_slices(key, iv) else {
        return buf; // unreachable: callers always pass 16-byte key/iv slices
    };
    for block in buf.chunks_exact_mut(16) {
        enc.encrypt_block_mut(GenericArray::from_mut_slice(block));
    }
    buf
}

/// AES-CBC **encrypt** in the PDF payload format: random IV || ciphertext with
/// PKCS#5 padding. Key length selects AES-128 (AESV2) vs AES-256 (AESV3). The
/// write-side inverse of [`aes_cbc_decrypt`]; used when adding objects to an
/// encrypted document.
fn aes_cbc_encrypt(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut iv = [0u8; 16];
    // The IV must be unpredictable but need not be secret. getrandom is not a
    // parser dependency, so derive it from entropy we have: MD5 over the data
    // plus a process-unique counter. (Writers with stronger requirements
    // encrypt via zpdf-writer's Encryptor, which uses the system RNG.)
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nonce = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut seed = Vec::with_capacity(data.len().min(256) + 16);
    seed.extend_from_slice(&nonce.to_le_bytes());
    seed.extend_from_slice(&(data.len() as u64).to_le_bytes());
    seed.extend_from_slice(&data[..data.len().min(240)]);
    iv.copy_from_slice(&md5(&seed));

    let pad = 16 - (data.len() % 16);
    let mut buf = Vec::with_capacity(data.len() + pad);
    buf.extend_from_slice(data);
    buf.extend(std::iter::repeat_n(pad as u8, pad));

    let ok = match key.len() {
        16 => {
            if let Ok(mut enc) = cbc::Encryptor::<aes::Aes128>::new_from_slices(key, &iv) {
                for block in buf.chunks_exact_mut(16) {
                    enc.encrypt_block_mut(GenericArray::from_mut_slice(block));
                }
                true
            } else {
                false
            }
        }
        32 => {
            if let Ok(mut enc) = cbc::Encryptor::<aes::Aes256>::new_from_slices(key, &iv) {
                for block in buf.chunks_exact_mut(16) {
                    enc.encrypt_block_mut(GenericArray::from_mut_slice(block));
                }
                true
            } else {
                false
            }
        }
        _ => false,
    };
    if !ok {
        tracing::warn!(
            "invalid AES key length {}; data left unencrypted",
            key.len()
        );
        return data.to_vec();
    }

    let mut out = Vec::with_capacity(16 + buf.len());
    out.extend_from_slice(&iv);
    out.extend_from_slice(&buf);
    out
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
        j = j.wrapping_add(s[i]).wrapping_add(key[i % key.len()]);
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
            let f = f.wrapping_add(a).wrapping_add(MD5_K[i]).wrapping_add(m[g]);
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
    use zpdf_core::{PdfName, PdfStream};

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// AES-256-CBC encrypt with no padding — test-only helper for building V5
    /// fixtures (the production code only ever decrypts with AES-256).
    fn aes256_cbc_encrypt_nopad(key: &[u8], iv: &[u8], data: &[u8]) -> Vec<u8> {
        let mut buf = data.to_vec();
        let mut enc = cbc::Encryptor::<aes::Aes256>::new_from_slices(key, iv).unwrap();
        for block in buf.chunks_exact_mut(16) {
            enc.encrypt_block_mut(GenericArray::from_mut_slice(block));
        }
        buf
    }

    /// Navigate Root → Pages → Kids[0] → Contents and decode the stream.
    fn content_stream_bytes(file: &crate::PdfFile) -> Vec<u8> {
        let root = file.trailer.get_ref("Root").expect("trailer /Root");
        let cat = file.resolve(root).expect("resolve catalog");
        let pages_ref = cat.as_dict().unwrap().get_ref("Pages").unwrap();
        let pages = file.resolve(pages_ref).expect("resolve pages");
        let kids = pages.as_dict().unwrap().get_array("Kids").unwrap().to_vec();
        let PdfObject::Ref(page_ref) = kids[0] else {
            panic!("Kids[0] is not a reference")
        };
        let page = file.resolve(page_ref).expect("resolve page");
        let contents_ref = page.as_dict().unwrap().get_ref("Contents").unwrap();
        file.resolve_stream_data(contents_ref)
            .expect("decode content stream")
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
        let key = compute_key_rc4(b"", &o, p, &id0, 2, 40, true);
        assert_eq!(hex(&key), "b374aaeaf4", "file key (Algorithm 2)");

        // Algorithm 4 (R2): RC4(file_key, PAD) must equal stored /U.
        assert_eq!(rc4(&key, &PAD), u, "user-password validation (Algorithm 4)");
        assert!(
            validate_user_password(&key, &u, &id0, 2),
            "validate_user_password should accept the correct R2 key"
        );
        // A wrong key (empty /ID) must NOT validate.
        let wrong = compute_key_rc4(b"", &o, p, &[], 2, 40, true);
        assert!(
            !validate_user_password(&wrong, &u, &id0, 2),
            "validate_user_password should reject a wrong key"
        );

        // Algorithm 1 → per-object key for the page-1 content stream (1652, 0).
        let dec = Decryptor {
            key,
            stm_algo: Algo::Rc4,
            str_algo: Algo::Rc4,
            encrypt_id: None,
            encrypt_metadata: true,
        };
        let objkey = dec.object_key(ObjectId(1652, 0), Algo::Rc4);
        assert_eq!(
            hex(&objkey),
            "30dadd6463d5f9765abc",
            "per-object key (Algorithm 1)"
        );
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
        assert_eq!(algo_for_filter(&dict, "StdCF"), Algo::Rc4);
        // /Identity and /CFM /None mean "no encryption for this class".
        assert_eq!(algo_for_filter(&dict, "Identity"), Algo::Identity);
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

    #[test]
    fn aes_cbc_known_answers() {
        // NIST SP 800-38A F.2.1 (CBC-AES128.Encrypt), first block.
        let key = unhex("2b7e151628aed2a6abf7158809cf4f3c");
        let iv = unhex("000102030405060708090a0b0c0d0e0f");
        let pt = unhex("6bc1bee22e409f96e93d7e117393172a");
        let ct = aes128_cbc_encrypt_nopad(&key, &iv, &pt);
        assert_eq!(hex(&ct), "7649abac8119b246cee98e9b12e9197d");
        // Decrypt round-trips.
        let mut buf = ct.clone();
        assert!(cbc_decrypt_in_place(&key, &iv, &mut buf));
        assert_eq!(buf, pt);

        // NIST SP 800-38A F.2.5 (CBC-AES256.Encrypt), first block.
        let key = unhex("603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4");
        let ct256 = aes256_cbc_encrypt_nopad(&key, &iv, &pt);
        assert_eq!(hex(&ct256), "f58c4c04d6e5f1ba779eabfb5f7bfbd6");
        let mut buf = ct256.clone();
        assert!(cbc_decrypt_in_place(&key, &iv, &mut buf));
        assert_eq!(buf, pt);
    }

    #[test]
    fn aes_iv_prefix_and_padding() {
        let key = unhex("000102030405060708090a0b0c0d0e0f");
        let iv = [0x42u8; 16];
        let plaintext = b"attack at dawn".to_vec(); // 14 bytes → 2 bytes padding

        // Build a PDF-style payload: IV || AES-CBC(plaintext + PKCS#5 pad).
        let mut padded = plaintext.clone();
        padded.extend_from_slice(&[2, 2]);
        let mut payload = iv.to_vec();
        payload.extend_from_slice(&aes128_cbc_encrypt_nopad(&key, &iv, &padded));
        assert_eq!(aes_cbc_decrypt(&key, &payload), plaintext);

        // Invalid padding (last byte 0 / out of range): keep the data, warn.
        let mut bad = plaintext.clone();
        bad.extend_from_slice(&[2, 0]);
        let mut payload = iv.to_vec();
        payload.extend_from_slice(&aes128_cbc_encrypt_nopad(&key, &iv, &bad));
        assert_eq!(aes_cbc_decrypt(&key, &payload), bad);

        // Structurally impossible lengths are returned unmodified.
        assert_eq!(aes_cbc_decrypt(&key, &[1, 2, 3]), vec![1, 2, 3]);
        assert_eq!(
            aes_cbc_decrypt(&key, &payload[..17]),
            payload[..17].to_vec()
        );
        assert_eq!(aes_cbc_decrypt(&key, b""), Vec::<u8>::new());
        // An empty ciphertext (IV only) decodes to an empty plaintext.
        assert_eq!(aes_cbc_decrypt(&key, &iv), Vec::<u8>::new());
    }

    /// Round-trip ISO 32000-2 Algorithm 2.A against a synthetic /Encrypt dict
    /// built with the writer-side algorithms (8 & 9): both the user (/U + /UE)
    /// and owner (/O + /OE) paths must recover the same file key.
    #[test]
    fn v5_key_derivation_roundtrip() {
        for r in [5i64, 6] {
            let file_key = [0xA5u8; 32];
            let (uvsalt, uksalt) = ([0x11u8; 8], [0x22u8; 8]);

            // Algorithm 8: /U and /UE for the empty user password.
            let mut u = hash_v5(r, b"", &uvsalt, &[]).to_vec();
            u.extend_from_slice(&uvsalt);
            u.extend_from_slice(&uksalt);
            let ik = hash_v5(r, b"", &uksalt, &[]);
            let ue = aes256_cbc_encrypt_nopad(&ik, &[0u8; 16], &file_key);

            // Algorithm 9: /O and /OE for the empty owner password (over U).
            let (ovsalt, oksalt) = ([0x33u8; 8], [0x44u8; 8]);
            let mut o = hash_v5(r, b"", &ovsalt, &u[..48]).to_vec();
            o.extend_from_slice(&ovsalt);
            o.extend_from_slice(&oksalt);
            let oik = hash_v5(r, b"", &oksalt, &u[..48]);
            let oe = aes256_cbc_encrypt_nopad(&oik, &[0u8; 16], &file_key);

            let mut dict = PdfDict::new();
            dict.insert(PdfName::new("U"), PdfObject::String(PdfString(u.clone())));
            dict.insert(PdfName::new("UE"), PdfObject::String(PdfString(ue)));
            dict.insert(PdfName::new("O"), PdfObject::String(PdfString(o.clone())));
            dict.insert(PdfName::new("OE"), PdfObject::String(PdfString(oe.clone())));
            assert_eq!(
                compute_key_v5(&dict, r, b"").as_deref(),
                Some(&file_key[..]),
                "user-password path, R{r}"
            );

            // Omit /UE: the user hash still validates but the file key cannot
            // be recovered from it, so the owner (/O + /OE) path must take
            // over. (The owner hashes bind to the original /U, so /U itself
            // must stay intact.)
            let mut dict2 = PdfDict::new();
            dict2.insert(PdfName::new("U"), PdfObject::String(PdfString(u)));
            dict2.insert(PdfName::new("O"), PdfObject::String(PdfString(o)));
            dict2.insert(PdfName::new("OE"), PdfObject::String(PdfString(oe)));
            assert_eq!(
                compute_key_v5(&dict2, r, b"").as_deref(),
                Some(&file_key[..]),
                "owner-password fallback, R{r}"
            );
        }
    }

    /// /StmF Identity + /StrF StdCF(RC4): strings decrypt, streams pass through.
    #[test]
    fn v4_identity_stream_filter_leaves_streams_alone() {
        let mut stdcf = PdfDict::new();
        stdcf.insert(PdfName::new("CFM"), PdfObject::Name(PdfName::new("V2")));
        stdcf.insert(PdfName::new("Length"), PdfObject::Integer(16));
        let mut cf = PdfDict::new();
        cf.insert(PdfName::new("StdCF"), PdfObject::Dict(stdcf));
        let mut dict = PdfDict::new();
        dict.insert(
            PdfName::new("Filter"),
            PdfObject::Name(PdfName::new("Standard")),
        );
        dict.insert(PdfName::new("V"), PdfObject::Integer(4));
        dict.insert(PdfName::new("R"), PdfObject::Integer(4));
        dict.insert(PdfName::new("CF"), PdfObject::Dict(cf));
        dict.insert(
            PdfName::new("StmF"),
            PdfObject::Name(PdfName::new("Identity")),
        );
        dict.insert(PdfName::new("StrF"), PdfObject::Name(PdfName::new("StdCF")));

        let dec = match Decryptor::from_encrypt_dict(&dict, &[], None, b"") {
            BuildResult::Decryptor(d) => d,
            _ => panic!("decryptor"),
        };
        assert_eq!(dec.stm_algo, Algo::Identity);
        assert_eq!(dec.str_algo, Algo::Rc4);

        let stream_data = b"stream payload".to_vec();
        let string_data = b"string payload".to_vec();
        let mut arr = PdfObject::Array(vec![
            PdfObject::Stream(PdfStream::new(PdfDict::new(), stream_data.clone())),
            PdfObject::String(PdfString(string_data.clone())),
        ]);
        dec.decrypt_object(&mut arr, ObjectId(9, 0));
        let PdfObject::Array(items) = &arr else {
            unreachable!()
        };
        let PdfObject::Stream(s) = &items[0] else {
            unreachable!()
        };
        assert_eq!(
            &s.data[..],
            &stream_data[..],
            "Identity /StmF must not touch streams"
        );
        let PdfObject::String(st) = &items[1] else {
            unreachable!()
        };
        assert_ne!(st.0, string_data, "/StrF StdCF must decrypt strings");
        // ObjStm container bytes go through the stream filter → untouched too.
        assert_eq!(
            dec.decrypt_stream_bytes(ObjectId(9, 0), b"objstm"),
            b"objstm"
        );
    }

    /// /EncryptMetadata false leaves /Type /Metadata stream payloads alone.
    #[test]
    fn encrypt_metadata_false_skips_metadata_stream() {
        let dec = Decryptor {
            key: vec![1, 2, 3, 4, 5],
            stm_algo: Algo::Rc4,
            str_algo: Algo::Rc4,
            encrypt_id: None,
            encrypt_metadata: false,
        };
        let xmp = b"<x:xmpmeta/>".to_vec();
        let mut meta_dict = PdfDict::new();
        meta_dict.insert(
            PdfName::new("Type"),
            PdfObject::Name(PdfName::new("Metadata")),
        );
        let mut meta = PdfObject::Stream(PdfStream::new(meta_dict, xmp.clone()));
        dec.decrypt_object(&mut meta, ObjectId(7, 0));
        let PdfObject::Stream(s) = &meta else {
            unreachable!()
        };
        assert_eq!(
            &s.data[..],
            &xmp[..],
            "plaintext metadata must not be corrupted"
        );

        // A regular stream under the same decryptor IS decrypted.
        let mut other = PdfObject::Stream(PdfStream::new(PdfDict::new(), xmp.clone()));
        dec.decrypt_object(&mut other, ObjectId(7, 0));
        let PdfObject::Stream(s) = &other else {
            unreachable!()
        };
        assert_ne!(&s.data[..], &xmp[..]);
    }

    // ------------------------------------------------------------------
    // End-to-end fixtures (generated by target/crypto_fixtures/make_fixtures.py
    // via pypdf, empty user password). See tests/fixtures/.
    // ------------------------------------------------------------------

    const AES_MARKER: &[u8] = b"(Hello AES zpdf fixture) Tj";

    fn assert_fixture_decrypts(bytes: &[u8]) {
        let file = crate::PdfFile::parse(bytes.to_vec()).expect("parse encrypted fixture");
        let content = content_stream_bytes(&file);
        assert!(
            content.windows(AES_MARKER.len()).any(|w| w == AES_MARKER),
            "decrypted content stream should contain the known marker, got: {:?}",
            String::from_utf8_lossy(&content)
        );
    }

    /// V4/R4 crypt filter AESV2 (AES-128-CBC), empty user password.
    #[test]
    fn aesv2_r4_decrypts_end_to_end() {
        assert_fixture_decrypts(include_bytes!("../tests/fixtures/aesv2_r4.pdf"));
    }

    /// V5/R5 crypt filter AESV3 (AES-256-CBC, plain SHA-256 hash).
    #[test]
    fn aesv3_r5_decrypts_end_to_end() {
        assert_fixture_decrypts(include_bytes!("../tests/fixtures/aesv3_r5.pdf"));
    }

    /// V5/R6 crypt filter AESV3 (AES-256-CBC, Algorithm 2.B hardened hash).
    #[test]
    fn aesv3_r6_decrypts_end_to_end() {
        assert_fixture_decrypts(include_bytes!("../tests/fixtures/aesv3_r6.pdf"));
    }

    // ------------------------------------------------------------------
    // Direct (non-reference) /Encrypt dict in the trailer + RC4 regression
    // ------------------------------------------------------------------

    fn hexstr(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// Hand-build a tiny V1/R2 RC4-40 PDF whose trailer carries the /Encrypt
    /// dictionary **directly** (not as an indirect reference).
    fn build_rc4_direct_encrypt_pdf(content_plain: &[u8]) -> Vec<u8> {
        // Algorithm 3 (R2): /O = RC4(MD5(padded owner pwd)[..5], padded user pwd),
        // both passwords empty → both pads.
        let okey = md5(&PAD);
        let o = rc4(&okey[..5], &PAD);
        let id0: Vec<u8> = (0u8..16).collect();
        let p: i32 = -1;
        let key = compute_key_rc4(b"", &o, p, &id0, 2, 40, true);
        let u = rc4(&key, &PAD); // Algorithm 4

        // RC4 is symmetric: "decrypting" the plaintext produces the ciphertext.
        let enc = Decryptor {
            key,
            stm_algo: Algo::Rc4,
            str_algo: Algo::Rc4,
            encrypt_id: None,
            encrypt_metadata: true,
        };
        let content_enc = enc.decrypt_stream_bytes(ObjectId(5, 0), content_plain);

        let mut stream_obj = format!("<< /Length {} >>\nstream\n", content_enc.len()).into_bytes();
        stream_obj.extend_from_slice(&content_enc);
        stream_obj.extend_from_slice(b"\nendstream");
        let bodies: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 5 0 R >>".to_vec(),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
            stream_obj,
        ];

        let mut out: Vec<u8> = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in bodies.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            out.extend_from_slice(body);
            out.extend_from_slice(b"\nendobj\n");
        }
        let xref_pos = out.len();
        out.extend_from_slice(b"xref\n0 6\n0000000000 65535 f \n");
        for off in &offsets {
            out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size 6 /Root 1 0 R /ID [<{id}> <{id}>] /Encrypt << /Filter \
                 /Standard /V 1 /R 2 /Length 40 /O <{o}> /U <{u}> /P {p} >> >>\nstartxref\n\
                 {xref_pos}\n%%EOF\n",
                id = hexstr(&id0),
                o = hexstr(&o),
                u = hexstr(&u),
            )
            .as_bytes(),
        );
        out
    }

    /// A direct /Encrypt dict must still enable decryption (regression: it used
    /// to silently disable it), and the plain RC4 path must keep working.
    #[test]
    fn rc4_direct_encrypt_dict_in_trailer() {
        let plain = b"BT /F1 12 Tf (direct encrypt dict) Tj ET";
        let pdf = build_rc4_direct_encrypt_pdf(plain);
        let file = crate::PdfFile::parse(pdf).expect("parse hand-built encrypted PDF");
        assert_eq!(content_stream_bytes(&file), plain);
    }

    // ------------------------------------------------------------------
    // Non-empty-password (RC4 V2/R3, 128-bit) authentication
    // ------------------------------------------------------------------

    /// Hand-build a V2/R3 RC4-128 PDF encrypted with distinct user and owner
    /// passwords (Algorithms 2/3/5 on the encrypt side). `omit_u` drops `/U` to
    /// model a malformed document with nothing to authenticate against.
    fn build_rc4_password_pdf(
        user_pw: &[u8],
        owner_pw: &[u8],
        content_plain: &[u8],
        omit_u: bool,
    ) -> Vec<u8> {
        let (r, bits, n) = (3i64, 128i64, 16usize);
        let id0: Vec<u8> = (0u8..16).collect();
        let p: i32 = -44;

        // Algorithm 3: /O = encrypt(padded user pwd) under the owner key.
        let mut okey = md5(&pad_password(owner_pw));
        for _ in 0..50 {
            okey = md5(&okey[..n]);
        }
        let owner_key = &okey[..n];
        let mut o = pad_password(user_pw).to_vec();
        for i in 0..=19u8 {
            let step_key: Vec<u8> = owner_key.iter().map(|b| b ^ i).collect();
            o = rc4(&step_key, &o);
        }

        // Algorithm 2: file key from the user password + /O.
        let key = compute_key_rc4(user_pw, &o, p, &id0, r, bits, true);

        // Algorithm 5 (R≥3): /U = first 16 bytes of the iterated RC4 of MD5(PAD‖ID),
        // padded out to 32 bytes.
        let mut u_input = Vec::new();
        u_input.extend_from_slice(&PAD);
        u_input.extend_from_slice(&id0);
        let mut x = rc4(&key, &md5(&u_input));
        for i in 1..=19u8 {
            let step_key: Vec<u8> = key.iter().map(|b| b ^ i).collect();
            x = rc4(&step_key, &x);
        }
        let mut u = x;
        u.extend_from_slice(&[0u8; 16]);

        let enc = Decryptor {
            key,
            stm_algo: Algo::Rc4,
            str_algo: Algo::Rc4,
            encrypt_id: None,
            encrypt_metadata: true,
        };
        let content_enc = enc.decrypt_stream_bytes(ObjectId(5, 0), content_plain);

        let mut stream_obj = format!("<< /Length {} >>\nstream\n", content_enc.len()).into_bytes();
        stream_obj.extend_from_slice(&content_enc);
        stream_obj.extend_from_slice(b"\nendstream");
        let bodies: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 5 0 R >>".to_vec(),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
            stream_obj,
        ];

        let mut out: Vec<u8> = b"%PDF-1.6\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in bodies.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            out.extend_from_slice(body);
            out.extend_from_slice(b"\nendobj\n");
        }
        let xref_pos = out.len();
        out.extend_from_slice(b"xref\n0 6\n0000000000 65535 f \n");
        for off in &offsets {
            out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        let u_entry = if omit_u {
            String::new()
        } else {
            format!("/U <{}> ", hexstr(&u))
        };
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size 6 /Root 1 0 R /ID [<{id}> <{id}>] /Encrypt << /Filter \
                 /Standard /V 2 /R 3 /Length 128 /O <{o}> {u_entry}/P {p} >> >>\nstartxref\n\
                 {xref_pos}\n%%EOF\n",
                id = hexstr(&id0),
                o = hexstr(&o),
            )
            .as_bytes(),
        );
        out
    }

    #[test]
    fn user_password_decrypts() {
        let plain = b"BT (user password works) Tj ET";
        let pdf = build_rc4_password_pdf(b"secret", b"master", plain, false);
        let file =
            crate::PdfFile::parse_with_password(pdf, b"secret").expect("user password opens");
        assert_eq!(content_stream_bytes(&file), plain);
    }

    #[test]
    fn owner_password_decrypts_via_recovery() {
        // The owner password authenticates by recovering the user password from
        // /O (Algorithm 7), then deriving the same file key.
        let plain = b"BT (owner password works) Tj ET";
        let pdf = build_rc4_password_pdf(b"secret", b"master", plain, false);
        let file =
            crate::PdfFile::parse_with_password(pdf, b"master").expect("owner password opens");
        assert_eq!(content_stream_bytes(&file), plain);
    }

    #[test]
    fn wrong_password_is_rejected() {
        let pdf = build_rc4_password_pdf(b"secret", b"master", b"BT (x) Tj ET", false);
        match crate::PdfFile::parse_with_password(pdf, b"nope") {
            Err(zpdf_core::Error::WrongPassword) => {}
            Err(e) => panic!("expected WrongPassword, got error {e:?}"),
            Ok(_) => panic!("expected WrongPassword, but the document opened"),
        }
    }

    #[test]
    fn empty_password_open_degrades_without_erroring() {
        // The default open (empty password) must NOT error on a password-needing
        // document — it opens best-effort, but the content does not decrypt to
        // the plaintext.
        let plain = b"BT (needs a password) Tj ET";
        let pdf = build_rc4_password_pdf(b"secret", b"master", plain, false);
        let file = crate::PdfFile::parse(pdf).expect("default open still succeeds");
        assert!(file.is_encrypted());
        assert_ne!(content_stream_bytes(&file), plain);
    }

    #[test]
    fn missing_u_opens_best_effort_not_wrong_password() {
        // A malformed document with no /U cannot be authenticated, so a supplied
        // password is used unverified (best-effort) rather than reported wrong.
        // The correct password still yields the right key and decrypts.
        let plain = b"BT (no /U to check) Tj ET";
        let pdf = build_rc4_password_pdf(b"secret", b"master", plain, true);
        let file =
            crate::PdfFile::parse_with_password(pdf, b"secret").expect("correct password opens");
        assert_eq!(content_stream_bytes(&file), plain);

        // A wrong password also opens (garbage), but never WrongPassword.
        let pdf = build_rc4_password_pdf(b"secret", b"master", plain, true);
        let file = crate::PdfFile::parse_with_password(pdf, b"nope")
            .expect("wrong password still opens best-effort (no /U to reject against)");
        assert_ne!(content_stream_bytes(&file), plain);
    }
}
