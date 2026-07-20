//! Full-document rewrite: garbage-collect and re-serialize a PDF from its
//! object graph.
//!
//! Unlike the incremental writer (which appends to the original bytes), the
//! rewriter emits a **fresh file** containing only the objects reachable from
//! the trailer (`/Root`, `/Info`), renumbered densely from 1. This:
//!
//! - drops unreachable objects (orphans from incremental edits, dead page
//!   trees, superseded object generations);
//! - inlines objects out of object streams (`/ObjStm` containers are not
//!   themselves reachable and disappear);
//! - **decrypts**: `PdfFile::resolve` transparently decrypts strings and
//!   stream payloads, so rewriting an encrypted document (opened with its
//!   password) produces a plain-text equivalent — `/Encrypt` is dropped;
//! - optionally Flate-compresses streams that carry no `/Filter`.
//!
//! Stream payloads are otherwise copied verbatim (still filter-encoded), so
//! content is preserved byte-for-byte.

use std::collections::HashMap;
use std::io;

use zpdf_core::{ObjectId, PdfDict, PdfName, PdfObject, PdfStream, Result};
use zpdf_parser::PdfFile;

use crate::encrypt::{EncryptionConfig, Encryptor};
use crate::serialize::{write_object, write_stream};
use crate::{flate_compress, invalid_data};

/// Options for [`rewrite_pdf`].
#[derive(Debug, Clone)]
pub struct RewriteOptions {
    /// Flate-compress streams that have no `/Filter` and are at least 64
    /// bytes. Streams that already carry a filter are never touched.
    pub compress_uncompressed: bool,
    /// Encrypt the output (Standard security handler). The source is written
    /// decrypted-then-re-encrypted, so this also re-keys an encrypted input.
    pub encrypt: Option<EncryptionConfig>,
    /// Downsample FlateDecode RGB/Gray images whose longer side exceeds this
    /// many pixels (box filter, halved until within bounds). `None` keeps all
    /// images at original resolution. DCT/JPX/CCITT images are never resampled
    /// (they would have to be re-encoded).
    pub max_image_dimension: Option<u32>,
}

impl Default for RewriteOptions {
    fn default() -> Self {
        Self {
            compress_uncompressed: true,
            encrypt: None,
            max_image_dimension: None,
        }
    }
}

/// Rewrite `source` as a fresh, garbage-collected PDF. See module docs.
pub fn rewrite_pdf(source: &PdfFile, options: &RewriteOptions) -> Result<Vec<u8>> {
    let root = source
        .trailer
        .get_ref("Root")
        .map_err(|_| invalid_data("trailer missing /Root"))?;
    let info = source.trailer.get_ref("Info").ok();

    // Pass 1: breadth-first reachability walk building old-id → new-number
    // mapping in discovery order (root first, so the catalog is object 1).
    let mut map: HashMap<ObjectId, u32> = HashMap::new();
    let mut order: Vec<ObjectId> = Vec::new();
    let mut queue: Vec<ObjectId> = vec![root];
    if let Some(info_id) = info {
        queue.push(info_id);
    }
    for id in &queue {
        map.insert(*id, 0); // placeholder, numbered below
    }
    let mut head = 0;
    while head < queue.len() {
        let id = queue[head];
        head += 1;
        order.push(id);
        let obj = source.resolve(id)?;
        collect_refs(&obj, &mut map, &mut queue);
    }
    for (n, id) in order.iter().enumerate() {
        map.insert(*id, (n + 1) as u32);
    }

    // Encryptor, when requested. The /ID first element doubles as RC4 key
    // input, so fix it before any object is emitted: keep the original ID or
    // derive a fresh one from the content.
    let id_first: Vec<u8> = match source.trailer.get("ID") {
        Some(PdfObject::Array(a)) => match a.first() {
            Some(PdfObject::String(s)) if !s.0.is_empty() => s.0.clone(),
            _ => derive_file_id(source),
        },
        _ => derive_file_id(source),
    };
    let encryptor = match &options.encrypt {
        Some(config) => Some(Encryptor::new(config, &id_first)?),
        None => None,
    };

    // Pass 2: serialize in new-number order.
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n");
    let mut offsets: Vec<u64> = Vec::with_capacity(order.len());
    for (n, id) in order.iter().enumerate() {
        let new_num = (n + 1) as u32;
        offsets.push(out.len() as u64);
        let mut obj = renumber(&source.resolve(*id)?, &map);
        if let Some(max_dim) = options.max_image_dimension {
            if let PdfObject::Stream(stream) = &mut obj {
                if let Some(smaller) = downsample_image_stream(stream, max_dim) {
                    *stream = smaller;
                }
            }
        }
        if let Some(enc) = &encryptor {
            let new_id = ObjectId(new_num, 0);
            enc.encrypt_strings(&mut obj, new_id);
            if let PdfObject::Stream(stream) = &mut obj {
                // Compress-then-encrypt: run the optional compression first so
                // ciphertext is not (incompressibly) recompressed.
                let (dict, data) = prepared_stream_parts(stream, options.compress_uncompressed);
                let encrypted = enc.encrypt_bytes(new_id, &data);
                *stream = PdfStream {
                    dict,
                    data: encrypted.into(),
                };
                write_stream(&mut out, new_num, 0, &stream.dict, &stream.data)
                    .map_err(zpdf_core::Error::Io)?;
                continue;
            }
            write_object(&mut out, new_num, 0, &obj).map_err(zpdf_core::Error::Io)?;
            continue;
        }
        emit(&mut out, new_num, obj, options).map_err(zpdf_core::Error::Io)?;
    }

    // The /Encrypt dictionary itself is stored in the clear, after all
    // encrypted objects.
    let encrypt_ref = match &encryptor {
        Some(enc) => {
            let num = (order.len() + 1) as u32;
            offsets.push(out.len() as u64);
            write_object(
                &mut out,
                num,
                0,
                &PdfObject::Dict(enc.encrypt_dict().clone()),
            )
            .map_err(zpdf_core::Error::Io)?;
            Some(ObjectId(num, 0))
        }
        None => None,
    };

    // Xref table + trailer.
    let xref_pos = out.len();
    let size = offsets.len() + 1;
    out.extend_from_slice(format!("xref\n0 {size}\n").as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for offset in &offsets {
        if *offset > 9_999_999_999 {
            return Err(invalid_data("xref offset exceeds ten decimal digits").into());
        }
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    let mut trailer = PdfDict::new();
    trailer.insert(PdfName::new("Size"), PdfObject::Integer(size as i64));
    trailer.insert(
        PdfName::new("Root"),
        PdfObject::Ref(ObjectId(map[&root], 0)),
    );
    if let Some(info_id) = info {
        if let Some(&n) = map.get(&info_id) {
            trailer.insert(PdfName::new("Info"), PdfObject::Ref(ObjectId(n, 0)));
        }
    }
    if let Some(enc_ref) = encrypt_ref {
        trailer.insert(PdfName::new("Encrypt"), PdfObject::Ref(enc_ref));
        // An encrypted file MUST carry /ID; write the exact bytes the key was
        // derived from.
        trailer.insert(
            PdfName::new("ID"),
            PdfObject::Array(vec![
                PdfObject::String(zpdf_core::PdfString(id_first.clone())),
                PdfObject::String(zpdf_core::PdfString(id_first)),
            ]),
        );
    } else if let Some(id_arr @ PdfObject::Array(_)) = source.trailer.get("ID") {
        // Keep the original /ID (first element identifies the document across
        // revisions); /Prev is intentionally dropped.
        trailer.insert(PdfName::new("ID"), id_arr.clone());
    }
    out.extend_from_slice(b"trailer\n");
    crate::serialize::serialize_dict(&mut out, &trailer).map_err(zpdf_core::Error::Io)?;
    out.extend_from_slice(format!("\nstartxref\n{xref_pos}\n%%EOF\n").as_bytes());
    Ok(out)
}

/// A stable /ID for files that lack one: MD5-free digest of length + head
/// bytes (uniqueness matters, cryptographic strength does not).
fn derive_file_id(source: &PdfFile) -> Vec<u8> {
    use sha2::Digest;
    let data = source.data();
    let mut h = sha2::Sha256::new();
    h.update((data.len() as u64).to_le_bytes());
    h.update(&data[..data.len().min(1024)]);
    h.finalize()[..16].to_vec()
}

/// The (dict, data) a stream would serialize as under the compression option,
/// without writing it. Mirrors [`emit`]'s stream arm.
fn prepared_stream_parts(stream: &PdfStream, compress: bool) -> (PdfDict, Vec<u8>) {
    let has_filter = stream.dict.get("Filter").is_some();
    if compress && !has_filter && stream.data.len() >= 64 {
        let compressed = flate_compress(&stream.data);
        if compressed.len() < stream.data.len() {
            let mut dict = stream.dict.clone();
            dict.insert(
                PdfName::new("Filter"),
                PdfObject::Name(PdfName::new("FlateDecode")),
            );
            return (dict, compressed);
        }
    }
    (stream.dict.clone(), stream.data.to_vec())
}

/// Box-filter downsample an 8-bit Flate (or unfiltered) DeviceRGB/DeviceGray
/// image XObject whose longer side exceeds `max_dim`. Returns the replacement
/// stream, or `None` when the image is not eligible (other filters, palettes,
/// masks, unusual bit depths) or already small enough.
fn downsample_image_stream(stream: &PdfStream, max_dim: u32) -> Option<PdfStream> {
    let dict = &stream.dict;
    if dict.get_name("Subtype").ok() != Some("Image") {
        return None;
    }
    // Only self-contained 8-bit gray/RGB images: no palette, no masking that
    // would change geometry-sensitive semantics.
    match dict.get("Filter") {
        None => {}
        Some(PdfObject::Name(n)) if n.as_str() == "FlateDecode" => {}
        _ => return None,
    }
    if dict.get("Mask").is_some() || dict.get("Decode").is_some() {
        return None;
    }
    // A predictor would make raw zlib output non-sample data; skip rather
    // than corrupt.
    if dict.get("DecodeParms").is_some() || dict.get("DP").is_some() {
        return None;
    }
    if dict.get_i64("BitsPerComponent").ok() != Some(8) {
        return None;
    }
    let channels: u32 = match dict.get_name("ColorSpace").ok() {
        Some("DeviceRGB") => 3,
        Some("DeviceGray") => 1,
        _ => return None,
    };
    let width = u32::try_from(dict.get_i64("Width").ok()?).ok()?;
    let height = u32::try_from(dict.get_i64("Height").ok()?).ok()?;
    if width.max(height) <= max_dim || width == 0 || height == 0 {
        return None;
    }

    // Decode the sample data.
    let raw: Vec<u8> = if dict.get("Filter").is_some() {
        use flate2::read::ZlibDecoder;
        use std::io::Read;
        let mut decoder = ZlibDecoder::new(stream.data.as_ref());
        let mut buf = Vec::new();
        decoder.read_to_end(&mut buf).ok()?;
        buf
    } else {
        stream.data.to_vec()
    };
    let row = (width as usize).checked_mul(channels as usize)?;
    if raw.len() < row.checked_mul(height as usize)? {
        return None;
    }

    // Halve repeatedly until within bounds (cheap, avoids resample kernels).
    let mut cur = raw;
    let (mut w, mut h) = (width, height);
    while w.max(h) > max_dim && w >= 2 && h >= 2 {
        let (nw, nh) = (w / 2, h / 2);
        let mut next = vec![0u8; nw as usize * nh as usize * channels as usize];
        for y in 0..nh as usize {
            for x in 0..nw as usize {
                for c in 0..channels as usize {
                    let idx = |xx: usize, yy: usize| (yy * w as usize + xx) * channels as usize + c;
                    let sum = cur[idx(2 * x, 2 * y)] as u32
                        + cur[idx(2 * x + 1, 2 * y)] as u32
                        + cur[idx(2 * x, 2 * y + 1)] as u32
                        + cur[idx(2 * x + 1, 2 * y + 1)] as u32;
                    next[(y * nw as usize + x) * channels as usize + c] = (sum / 4) as u8;
                }
            }
        }
        cur = next;
        w = nw;
        h = nh;
    }
    if (w, h) == (width, height) {
        return None;
    }

    let mut new_dict = dict.clone();
    new_dict.insert(PdfName::new("Width"), PdfObject::Integer(w as i64));
    new_dict.insert(PdfName::new("Height"), PdfObject::Integer(h as i64));
    new_dict.insert(
        PdfName::new("Filter"),
        PdfObject::Name(PdfName::new("FlateDecode")),
    );
    // /Length is recomputed at serialization; a stale /DecodeParms could
    // corrupt decoding, so drop it (we write plain zlib).
    new_dict.0.remove(&PdfName::new("DecodeParms"));
    Some(PdfStream {
        dict: new_dict,
        data: flate_compress(&cur).into(),
    })
}

/// Queue every indirect reference in `obj` that has not been seen yet.
fn collect_refs(obj: &PdfObject, map: &mut HashMap<ObjectId, u32>, queue: &mut Vec<ObjectId>) {
    match obj {
        PdfObject::Ref(r) => {
            if !map.contains_key(r) {
                map.insert(*r, 0);
                queue.push(*r);
            }
        }
        PdfObject::Array(arr) => {
            for elem in arr {
                collect_refs(elem, map, queue);
            }
        }
        PdfObject::Dict(dict) => {
            for v in dict.0.values() {
                collect_refs(v, map, queue);
            }
        }
        PdfObject::Stream(stream) => {
            for v in stream.dict.0.values() {
                collect_refs(v, map, queue);
            }
        }
        _ => {}
    }
}

/// Structurally rewrite references through the completed mapping. A ref to an
/// unmapped object (impossible after a full walk, defensive) becomes null.
fn renumber(obj: &PdfObject, map: &HashMap<ObjectId, u32>) -> PdfObject {
    match obj {
        PdfObject::Ref(r) => match map.get(r) {
            Some(&n) => PdfObject::Ref(ObjectId(n, 0)),
            None => PdfObject::Null,
        },
        PdfObject::Array(arr) => PdfObject::Array(arr.iter().map(|e| renumber(e, map)).collect()),
        PdfObject::Dict(dict) => PdfObject::Dict(renumber_dict(dict, map)),
        PdfObject::Stream(stream) => PdfObject::Stream(PdfStream {
            dict: renumber_dict(&stream.dict, map),
            data: stream.data.clone(),
        }),
        other => other.clone(),
    }
}

fn renumber_dict(dict: &PdfDict, map: &HashMap<ObjectId, u32>) -> PdfDict {
    let mut out = PdfDict::new();
    for (k, v) in &dict.0 {
        out.insert(k.clone(), renumber(v, map));
    }
    out
}

/// Serialize one object, optionally Flate-compressing bare streams.
fn emit(out: &mut Vec<u8>, num: u32, obj: PdfObject, options: &RewriteOptions) -> io::Result<()> {
    match obj {
        PdfObject::Stream(stream) => {
            let has_filter = stream.dict.get("Filter").is_some();
            if options.compress_uncompressed && !has_filter && stream.data.len() >= 64 {
                let compressed = flate_compress(&stream.data);
                // Only keep the compressed form when it actually helps.
                if compressed.len() < stream.data.len() {
                    let mut dict = stream.dict.clone();
                    dict.insert(
                        PdfName::new("Filter"),
                        PdfObject::Name(PdfName::new("FlateDecode")),
                    );
                    return write_stream(out, num, 0, &dict, &compressed);
                }
            }
            write_stream(out, num, 0, &stream.dict, &stream.data)
        }
        other => write_object(out, num, 0, &other),
    }
}
