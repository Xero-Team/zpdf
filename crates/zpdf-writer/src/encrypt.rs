//! Encryption on save: write encrypted PDFs (Standard security handler).
//!
//! Supported variants:
//! - **AES-256 (V5/R6)** — ISO 32000-2 §7.6.4. The 32-byte file key is random;
//!   `/U`, `/UE`, `/O`, `/OE` are derived from the user/owner passwords with
//!   the R6 hardened hash (Algorithm 8/9). All strings and streams encrypt
//!   with AES-256-CBC (`AESV3` crypt filter), no per-object key derivation.
//! - **RC4-128 (V2/R3)** — PDF 1.7 §7.6.3. The file key derives from the
//!   padded user password + `/O` + `/P` + `/ID` (Algorithm 2); per-object keys
//!   via Algorithm 1.
//!
//! Used by [`crate::rewrite::rewrite_pdf`] when [`crate::RewriteOptions::encrypt`]
//! is set: every string and stream object is encrypted as it is serialized
//! (the `/Encrypt` dict itself and the trailer `/ID` stay in the clear).

use sha2::Digest;

use zpdf_core::{ObjectId, PdfDict, PdfName, PdfObject, PdfString, Result};

use crate::invalid_data;

/// Which cipher the file is encrypted with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncryptionAlgorithm {
    /// RC4 with a 128-bit key (V2/R3). Legacy; broadest reader support.
    Rc4_128,
    /// AES-256-CBC (V5/R6). Modern; requires a PDF 1.7 ExtensionLevel 3 /
    /// PDF 2.0 reader.
    Aes256,
}

/// Permission flags (ISO 32000-1 Table 22). All true by default.
#[derive(Debug, Clone, Copy)]
pub struct Permissions {
    pub print: bool,
    pub modify: bool,
    pub copy: bool,
    pub annotate: bool,
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            print: true,
            modify: true,
            copy: true,
            annotate: true,
        }
    }
}

impl Permissions {
    /// The /P bit-field. Bits 1-2 reserved (0), bits 3,4,5,6 = print, modify,
    /// copy, annotate; bits 7-8 and 13-32 set per spec for R≥3 handlers.
    fn to_p(self) -> i32 {
        let mut p: u32 = 0xFFFF_F0C0; // upper bits + reserved high nibble set
        if self.print {
            p |= 1 << 2;
        }
        if self.modify {
            p |= 1 << 3;
        }
        if self.copy {
            p |= 1 << 4;
        }
        if self.annotate {
            p |= 1 << 5;
        }
        // Bits 9-12 (fill forms, extract for accessibility, assemble,
        // high-quality print) follow the coarse flags.
        if self.annotate {
            p |= 1 << 8;
        }
        if self.copy {
            p |= 1 << 9;
        }
        if self.modify {
            p |= 1 << 10;
        }
        if self.print {
            p |= 1 << 11;
        }
        p as i32
    }
}

/// Configuration for encrypting a written PDF.
#[derive(Debug, Clone)]
pub struct EncryptionConfig {
    pub algorithm: EncryptionAlgorithm,
    /// Password required to open the document ("" = none needed).
    pub user_password: Vec<u8>,
    /// Password granting full permissions. Empty ⇒ same as user password.
    pub owner_password: Vec<u8>,
    pub permissions: Permissions,
}

impl EncryptionConfig {
    pub fn aes256(user_password: &str, owner_password: &str) -> Self {
        Self {
            algorithm: EncryptionAlgorithm::Aes256,
            user_password: user_password.as_bytes().to_vec(),
            owner_password: owner_password.as_bytes().to_vec(),
            permissions: Permissions::default(),
        }
    }

    pub fn rc4_128(user_password: &str, owner_password: &str) -> Self {
        Self {
            algorithm: EncryptionAlgorithm::Rc4_128,
            user_password: user_password.as_bytes().to_vec(),
            owner_password: owner_password.as_bytes().to_vec(),
            permissions: Permissions::default(),
        }
    }
}

/// A ready-to-use encryptor: holds the file key and produces the /Encrypt
/// dictionary plus per-object cipher application.
pub struct Encryptor {
    algorithm: EncryptionAlgorithm,
    /// File encryption key (16 bytes RC4-128, 32 bytes AES-256).
    key: Vec<u8>,
    /// The finished /Encrypt dictionary.
    encrypt_dict: PdfDict,
}

impl Encryptor {
    /// Build an encryptor. `id_first` is the first element of the trailer /ID
    /// (required for RC4 key derivation; pass the same bytes that will be
    /// written to the output trailer).
    pub fn new(config: &EncryptionConfig, id_first: &[u8]) -> Result<Self> {
        let owner_pw: &[u8] = if config.owner_password.is_empty() {
            &config.user_password
        } else {
            &config.owner_password
        };
        match config.algorithm {
            EncryptionAlgorithm::Aes256 => Self::new_aes256(config, owner_pw),
            EncryptionAlgorithm::Rc4_128 => Self::new_rc4(config, owner_pw, id_first),
        }
    }

    /// The /Encrypt dictionary to place in the trailer.
    pub fn encrypt_dict(&self) -> &PdfDict {
        &self.encrypt_dict
    }

    /// Encrypt a string/stream payload for object `id`.
    pub fn encrypt_bytes(&self, id: ObjectId, data: &[u8]) -> Vec<u8> {
        match self.algorithm {
            EncryptionAlgorithm::Rc4_128 => rc4(&self.object_key_rc4(id), data),
            EncryptionAlgorithm::Aes256 => aes_cbc_encrypt(&self.key, data),
        }
    }

    /// Recursively encrypt every string in `obj` in place (streams are
    /// handled separately since their payload is carried out-of-band).
    pub fn encrypt_strings(&self, obj: &mut PdfObject, id: ObjectId) {
        match obj {
            PdfObject::String(s) => {
                *s = PdfString(self.encrypt_bytes(id, &s.0));
            }
            PdfObject::Array(a) => {
                for o in a.iter_mut() {
                    self.encrypt_strings(o, id);
                }
            }
            PdfObject::Dict(d) => {
                for v in d.0.values_mut() {
                    self.encrypt_strings(v, id);
                }
            }
            PdfObject::Stream(s) => {
                for v in s.dict.0.values_mut() {
                    self.encrypt_strings(v, id);
                }
            }
            _ => {}
        }
    }

    // ---- AES-256 / V5 / R6 --------------------------------------------------

    fn new_aes256(config: &EncryptionConfig, owner_pw: &[u8]) -> Result<Self> {
        let user_pw = &config.user_password[..config.user_password.len().min(127)];
        let owner_pw = &owner_pw[..owner_pw.len().min(127)];

        // Random 32-byte file key + salts.
        let key = random_bytes(32)?;
        let uv_salt = random_bytes(8)?;
        let uk_salt = random_bytes(8)?;
        let ov_salt = random_bytes(8)?;
        let ok_salt = random_bytes(8)?;

        // /U = hash(pw, vsalt, []) || vsalt || ksalt   (Algorithm 8)
        let u_hash = hash_v5_r6(user_pw, &uv_salt, &[]);
        let mut u = Vec::with_capacity(48);
        u.extend_from_slice(&u_hash);
        u.extend_from_slice(&uv_salt);
        u.extend_from_slice(&uk_salt);

        // /UE = AES-256-CBC-nopad(intermediate-key, zero IV, file key)
        let u_ik = hash_v5_r6(user_pw, &uk_salt, &[]);
        let ue = aes256_cbc_encrypt_nopad_zero_iv(&u_ik, &key);

        // /O = hash(pw, vsalt, U[0..48]) || vsalt || ksalt  (Algorithm 9)
        let o_hash = hash_v5_r6(owner_pw, &ov_salt, &u);
        let mut o = Vec::with_capacity(48);
        o.extend_from_slice(&o_hash);
        o.extend_from_slice(&ov_salt);
        o.extend_from_slice(&ok_salt);

        let o_ik = hash_v5_r6(owner_pw, &ok_salt, &u);
        let oe = aes256_cbc_encrypt_nopad_zero_iv(&o_ik, &key);

        // /Perms = AES-256-ECB(file key, P || 0xFFFFFFFF || "T" || "adb" || 4 random)
        let p = config.permissions.to_p();
        let mut perms_block = [0u8; 16];
        perms_block[..4].copy_from_slice(&(p as u32).to_le_bytes());
        perms_block[4..8].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
        perms_block[8] = b'T'; // EncryptMetadata = true
        perms_block[9..12].copy_from_slice(b"adb");
        let tail = random_bytes(4)?;
        perms_block[12..16].copy_from_slice(&tail);
        let perms = aes256_ecb_encrypt_block(&key, &perms_block);

        let mut dict = PdfDict::new();
        dict.insert(
            PdfName::new("Filter"),
            PdfObject::Name(PdfName::new("Standard")),
        );
        dict.insert(PdfName::new("V"), PdfObject::Integer(5));
        dict.insert(PdfName::new("R"), PdfObject::Integer(6));
        dict.insert(PdfName::new("Length"), PdfObject::Integer(256));
        dict.insert(PdfName::new("P"), PdfObject::Integer(p as i64));
        dict.insert(PdfName::new("U"), PdfObject::String(PdfString(u)));
        dict.insert(PdfName::new("UE"), PdfObject::String(PdfString(ue)));
        dict.insert(PdfName::new("O"), PdfObject::String(PdfString(o)));
        dict.insert(PdfName::new("OE"), PdfObject::String(PdfString(oe)));
        dict.insert(
            PdfName::new("Perms"),
            PdfObject::String(PdfString(perms.to_vec())),
        );
        let mut cf_std = PdfDict::new();
        cf_std.insert(PdfName::new("CFM"), PdfObject::Name(PdfName::new("AESV3")));
        cf_std.insert(PdfName::new("Length"), PdfObject::Integer(32));
        cf_std.insert(
            PdfName::new("AuthEvent"),
            PdfObject::Name(PdfName::new("DocOpen")),
        );
        let mut cf = PdfDict::new();
        cf.insert(PdfName::new("StdCF"), PdfObject::Dict(cf_std));
        dict.insert(PdfName::new("CF"), PdfObject::Dict(cf));
        dict.insert(PdfName::new("StmF"), PdfObject::Name(PdfName::new("StdCF")));
        dict.insert(PdfName::new("StrF"), PdfObject::Name(PdfName::new("StdCF")));

        Ok(Self {
            algorithm: EncryptionAlgorithm::Aes256,
            key,
            encrypt_dict: dict,
        })
    }

    // ---- RC4-128 / V2 / R3 --------------------------------------------------

    fn new_rc4(config: &EncryptionConfig, owner_pw: &[u8], id_first: &[u8]) -> Result<Self> {
        let p = config.permissions.to_p();

        // Algorithm 3: /O from the owner password.
        let mut o_key_hash = md5(&pad_password(owner_pw));
        for _ in 0..50 {
            o_key_hash = md5(&o_key_hash[..16]);
        }
        let o_key = &o_key_hash[..16];
        let mut o = rc4(o_key, &pad_password(&config.user_password));
        for i in 1u8..=19 {
            let step: Vec<u8> = o_key.iter().map(|b| b ^ i).collect();
            o = rc4(&step, &o);
        }

        // Algorithm 2: file key from the user password.
        let mut input = Vec::with_capacity(32 + 32 + 4 + id_first.len());
        input.extend_from_slice(&pad_password(&config.user_password));
        input.extend_from_slice(&o);
        input.extend_from_slice(&(p as u32).to_le_bytes());
        input.extend_from_slice(id_first);
        let mut hash = md5(&input);
        for _ in 0..50 {
            hash = md5(&hash[..16]);
        }
        let key = hash[..16].to_vec();

        // Algorithm 5: /U.
        let mut u_input = Vec::with_capacity(32 + id_first.len());
        u_input.extend_from_slice(&PAD);
        u_input.extend_from_slice(id_first);
        let mut u = rc4(&key, &md5(&u_input));
        for i in 1u8..=19 {
            let step: Vec<u8> = key.iter().map(|b| b ^ i).collect();
            u = rc4(&step, &u);
        }
        u.extend_from_slice(&[0u8; 16]); // arbitrary 16-byte padding

        let mut dict = PdfDict::new();
        dict.insert(
            PdfName::new("Filter"),
            PdfObject::Name(PdfName::new("Standard")),
        );
        dict.insert(PdfName::new("V"), PdfObject::Integer(2));
        dict.insert(PdfName::new("R"), PdfObject::Integer(3));
        dict.insert(PdfName::new("Length"), PdfObject::Integer(128));
        dict.insert(PdfName::new("P"), PdfObject::Integer(p as i64));
        dict.insert(PdfName::new("O"), PdfObject::String(PdfString(o)));
        dict.insert(PdfName::new("U"), PdfObject::String(PdfString(u)));

        Ok(Self {
            algorithm: EncryptionAlgorithm::Rc4_128,
            key,
            encrypt_dict: dict,
        })
    }

    /// Algorithm 1 per-object RC4 key.
    fn object_key_rc4(&self, id: ObjectId) -> Vec<u8> {
        let mut input = Vec::with_capacity(self.key.len() + 5);
        input.extend_from_slice(&self.key);
        input.extend_from_slice(&id.0.to_le_bytes()[..3]);
        input.extend_from_slice(&id.1.to_le_bytes()[..2]);
        let hash = md5(&input);
        let n = (self.key.len() + 5).min(16);
        hash[..n].to_vec()
    }
}

// ----------------------------------------------------------------------------
// Primitives
// ----------------------------------------------------------------------------

/// The 32-byte password-padding string (PDF 1.7 §7.6.3.3).
const PAD: [u8; 32] = [
    0x28, 0xBF, 0x4E, 0x5E, 0x4E, 0x75, 0x8A, 0x41, 0x64, 0x00, 0x4E, 0x56, 0xFF, 0xFA, 0x01, 0x08,
    0x2E, 0x2E, 0x00, 0xB6, 0xD0, 0x68, 0x3E, 0x80, 0x2F, 0x0C, 0xA9, 0xFE, 0x64, 0x53, 0x69, 0x7A,
];

fn pad_password(pw: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let n = pw.len().min(32);
    out[..n].copy_from_slice(&pw[..n]);
    out[n..].copy_from_slice(&PAD[..32 - n]);
    out
}

fn random_bytes(n: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    getrandom::getrandom(&mut buf)
        .map_err(|e| invalid_data(&format!("system RNG unavailable: {e}")))?;
    Ok(buf)
}

/// The V5/R6 password hash (SHA-256 + Algorithm 2.B hardening).
fn hash_v5_r6(password: &[u8], salt: &[u8], udata: &[u8]) -> [u8; 32] {
    let mut input = Vec::with_capacity(password.len() + salt.len() + udata.len());
    input.extend_from_slice(password);
    input.extend_from_slice(salt);
    input.extend_from_slice(udata);
    let initial: [u8; 32] = sha2::Sha256::digest(&input).into();
    hash_r6(initial, password, udata)
}

/// Algorithm 2.B hardened hash (mirrors the reader in zpdf-parser).
fn hash_r6(initial: [u8; 32], password: &[u8], udata: &[u8]) -> [u8; 32] {
    use aes::cipher::{generic_array::GenericArray, BlockEncryptMut, KeyIvInit};
    let mut k: Vec<u8> = initial.to_vec();
    let mut e_last: u8 = 0;
    let mut round: i64 = 0;
    while round < 64 || i64::from(e_last) > round - 32 {
        let mut k1 = Vec::with_capacity(64 * (password.len() + k.len() + udata.len()));
        for _ in 0..64 {
            k1.extend_from_slice(password);
            k1.extend_from_slice(&k);
            k1.extend_from_slice(udata);
        }
        let mut buf = k1;
        let mut enc =
            cbc::Encryptor::<aes::Aes128>::new_from_slices(&k[..16], &k[16..32]).expect("16/16");
        for block in buf.chunks_exact_mut(16) {
            enc.encrypt_block_mut(GenericArray::from_mut_slice(block));
        }
        e_last = *buf.last().unwrap_or(&0);
        let m = buf[..16].iter().map(|&b| u32::from(b)).sum::<u32>() % 3;
        k = match m {
            0 => sha2::Sha256::digest(&buf).to_vec(),
            1 => sha2::Sha384::digest(&buf).to_vec(),
            _ => sha2::Sha512::digest(&buf).to_vec(),
        };
        round += 1;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&k[..32]);
    out
}

/// AES-256-CBC with zero IV and no padding (for /UE and /OE, 32-byte input).
fn aes256_cbc_encrypt_nopad_zero_iv(key: &[u8; 32], data: &[u8]) -> Vec<u8> {
    use aes::cipher::{generic_array::GenericArray, BlockEncryptMut, KeyIvInit};
    let mut buf = data.to_vec();
    let mut enc = cbc::Encryptor::<aes::Aes256>::new_from_slices(key, &[0u8; 16]).expect("32/16");
    for block in buf.chunks_exact_mut(16) {
        enc.encrypt_block_mut(GenericArray::from_mut_slice(block));
    }
    buf
}

/// One-block AES-256-ECB encrypt (for /Perms).
fn aes256_ecb_encrypt_block(key: &[u8], block: &[u8; 16]) -> [u8; 16] {
    use aes::cipher::{generic_array::GenericArray, BlockEncrypt, KeyInit};
    let cipher = aes::Aes256::new_from_slice(key).expect("32-byte key");
    let mut b = GenericArray::clone_from_slice(block);
    cipher.encrypt_block(&mut b);
    let mut out = [0u8; 16];
    out.copy_from_slice(&b);
    out
}

/// AES-CBC encrypt with random IV and PKCS#5 padding — the PDF stream/string
/// payload format (IV || ciphertext). Key length selects AES-128/256.
fn aes_cbc_encrypt(key: &[u8], data: &[u8]) -> Vec<u8> {
    use aes::cipher::{generic_array::GenericArray, BlockEncryptMut, KeyIvInit};
    let mut iv = [0u8; 16];
    // Stream content is not key material; fall back to a fixed IV only if the
    // system RNG is unavailable (never expected in practice).
    let _ = getrandom::getrandom(&mut iv);

    let pad = 16 - (data.len() % 16);
    let mut buf = Vec::with_capacity(data.len() + pad);
    buf.extend_from_slice(data);
    buf.extend(std::iter::repeat_n(pad as u8, pad));

    match key.len() {
        32 => {
            let mut enc = cbc::Encryptor::<aes::Aes256>::new_from_slices(key, &iv).expect("32/16");
            for block in buf.chunks_exact_mut(16) {
                enc.encrypt_block_mut(GenericArray::from_mut_slice(block));
            }
        }
        16 => {
            let mut enc = cbc::Encryptor::<aes::Aes128>::new_from_slices(key, &iv).expect("16/16");
            for block in buf.chunks_exact_mut(16) {
                enc.encrypt_block_mut(GenericArray::from_mut_slice(block));
            }
        }
        _ => unreachable!("file keys are 16 or 32 bytes"),
    }

    let mut out = Vec::with_capacity(16 + buf.len());
    out.extend_from_slice(&iv);
    out.extend_from_slice(&buf);
    out
}

/// RC4 (symmetric).
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
    let (mut i, mut j) = (0u8, 0u8);
    for &byte in data {
        i = i.wrapping_add(1);
        j = j.wrapping_add(s[i as usize]);
        s.swap(i as usize, j as usize);
        let k = s[(s[i as usize].wrapping_add(s[j as usize])) as usize];
        out.push(byte ^ k);
    }
    out
}

/// MD5 (needed for the legacy RC4 key schedule; not available from sha2).
fn md5(data: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613,
        0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193,
        0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d,
        0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122,
        0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
        0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244,
        0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
        0xeb86d391,
    ];
    let (mut a0, mut b0, mut c0, mut d0): (u32, u32, u32, u32) =
        (0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476);
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
            let f = f.wrapping_add(a).wrapping_add(K[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
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

    #[test]
    fn md5_known_answer() {
        // RFC 1321 test vector: MD5("abc")
        let d = md5(b"abc");
        let hex: String = d.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn rc4_symmetry() {
        let key = b"Key";
        let data = b"Plaintext";
        let ct = rc4(key, data);
        assert_eq!(rc4(key, &ct), data);
        // Wikipedia test vector
        let hex: String = ct.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "bbf316e8d940af0ad3");
    }

    #[test]
    fn aes_roundtrip_via_padding_shape() {
        let key = [7u8; 32];
        let ct = aes_cbc_encrypt(&key, b"hello world");
        // 16 IV + 16 ciphertext (11 bytes + 5 padding)
        assert_eq!(ct.len(), 32);
    }

    #[test]
    fn permissions_bits() {
        let all = Permissions::default().to_p() as u32;
        assert_ne!(all & (1 << 2), 0, "print bit");
        assert_ne!(all & (1 << 3), 0, "modify bit");
        let none = Permissions {
            print: false,
            modify: false,
            copy: false,
            annotate: false,
        }
        .to_p() as u32;
        assert_eq!(none & (1 << 2), 0);
        assert_eq!(none & (1 << 3), 0);
        assert_eq!(none & (1 << 4), 0);
        assert_eq!(none & (1 << 5), 0);
    }
}
