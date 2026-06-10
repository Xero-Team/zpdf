mod ccitt;
mod crypt;
pub mod filters;
mod header;
mod lexer;
mod object_parser;
mod recovery;
mod xref;

pub use header::PdfHeader;
pub use lexer::Lexer;
pub use object_parser::ObjectParser;
pub use xref::{XrefEntry, XrefTable};

use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;
use std::sync::Arc;
use zpdf_core::{ObjectId, ParseLimits, PdfDict, PdfName, PdfObject, PdfStream, Result};

/// One fully-decoded /Type /ObjStm: decoded bytes + parsed offset table, shared
/// via Arc so a cache hit is a refcount bump, not a copy of the decoded buffer.
struct DecodedObjStm {
    /// Decoded stream bytes (after the filter pipeline).
    data: Arc<[u8]>,
    /// `/First`: byte offset within `data` where object bodies begin.
    first: usize,
    /// Parsed header: (obj_num, offset_within_data) per contained object,
    /// in stream order (index == `index_in_stream`).
    entries: Vec<(u32, usize)>,
}

pub struct PdfFile {
    data: Arc<[u8]>,
    pub header: PdfHeader,
    pub xref: XrefTable,
    pub trailer: zpdf_core::PdfDict,
    limits: ParseLimits,
    /// Standard-security-handler decryptor, built once at open time from the
    /// trailer `/Encrypt` dict. `None` for unencrypted (or unsupported-handler)
    /// documents, in which case `resolve`/object-stream decoding are unchanged.
    decryptor: Option<crypt::Decryptor>,
    /// Cache of resolved top-level indirect objects, keyed by ObjectId.
    /// `RefCell` suffices: `PdfFile` is never shared across threads in this
    /// workspace (swap to `Mutex` if that ever changes).
    object_cache: RefCell<HashMap<ObjectId, PdfObject>>,
    /// Cache of decoded object streams, keyed by the ObjStm object number.
    /// Avoids re-decoding the whole stream for every compressed object it holds.
    objstm_cache: RefCell<HashMap<u32, Arc<DecodedObjStm>>>,
    /// Lazily-built repair table: populated at most once by a full-file object
    /// scan, the first time an xref offset turns out to hold the wrong object
    /// (or no parseable object at all). The inner `None` means the scan itself
    /// failed and is not retried. Open-time recovery is independent of this.
    repair_table: OnceCell<Option<XrefTable>>,
}

impl PdfFile {
    pub fn parse(data: impl Into<Arc<[u8]>>) -> Result<Self> {
        Self::parse_with_limits(data, ParseLimits::default())
    }

    pub fn parse_with_limits(data: impl Into<Arc<[u8]>>, limits: ParseLimits) -> Result<Self> {
        let data: Arc<[u8]> = data.into();
        let header = header::parse_header(&data)?;

        // Try the normal xref pipeline first. Fall back to tail-scan recovery if
        // it fails structurally OR yields a trailer whose /Root doesn't resolve.
        let normal = xref::parse_xref_and_trailer(&data, &limits);
        let (xref, trailer) = match normal {
            Ok((xref, trailer)) if root_resolves(&data, &xref, &trailer, &limits) => {
                (xref, trailer)
            }
            other => {
                match &other {
                    Err(e) => {
                        tracing::warn!("xref parse failed ({e}); attempting tail-scan recovery")
                    }
                    Ok(_) => {
                        tracing::warn!("xref /Root did not resolve; attempting tail-scan recovery")
                    }
                }
                match recovery::scan_all_objects(&data, &limits) {
                    Ok(recovered) => recovered,
                    // Recovery failed: fall back to the normal parse if it at
                    // least produced a table, else surface the recovery error.
                    Err(rec_err) => match other {
                        Ok(parsed) => parsed,
                        Err(_) => return Err(rec_err),
                    },
                }
            }
        };

        let mut file = Self {
            data,
            header,
            xref,
            trailer,
            limits,
            decryptor: None,
            object_cache: RefCell::new(HashMap::new()),
            objstm_cache: RefCell::new(HashMap::new()),
            repair_table: OnceCell::new(),
        };
        // Build the decryptor *after* construction so it can use `resolve` to
        // fetch the (never-encrypted) /Encrypt dict; `decryptor` is still `None`
        // at this point, so that resolve does not try to decrypt it.
        file.decryptor = file.build_decryptor();
        Ok(file)
    }

    /// Construct the Standard-security-handler decryptor from the trailer
    /// `/Encrypt` dictionary and the first element of `/ID`. Returns `None` for
    /// unencrypted documents or unsupported handlers (AES, non-Standard).
    fn build_decryptor(&self) -> Option<crypt::Decryptor> {
        // /Encrypt is normally an indirect reference, but a direct dict is
        // legal too (a direct dict has no object id to exempt from decryption).
        // The /Encrypt dict is itself never encrypted; resolve it directly.
        let (enc_obj, encrypt_ref) = match self.trailer.get("Encrypt")? {
            PdfObject::Ref(r) => (self.resolve(*r).ok()?, Some(*r)),
            direct => (direct.clone(), None),
        };
        let enc_dict = enc_obj.as_dict().ok()?;
        let id_first = self.first_id_bytes();
        crypt::Decryptor::from_encrypt_dict(enc_dict, &id_first, encrypt_ref)
    }

    /// Raw bytes of the first element of the trailer `/ID` array (used in the
    /// encryption key derivation). `/ID` is normally a direct array but may be an
    /// indirect reference; resolve it (safe — `decryptor` is still `None` here,
    /// and `/ID` is never encrypted). Empty if absent or malformed.
    fn first_id_bytes(&self) -> Vec<u8> {
        let arr = match self.trailer.get("ID") {
            Some(PdfObject::Array(a)) => Some(std::borrow::Cow::Borrowed(a.as_slice())),
            Some(PdfObject::Ref(r)) => self.resolve(*r).ok().and_then(|o| {
                o.as_array()
                    .ok()
                    .map(|a| std::borrow::Cow::Owned(a.to_vec()))
            }),
            _ => None,
        };
        match arr.as_deref().and_then(|a| a.first()) {
            Some(PdfObject::String(s)) => s.0.clone(),
            _ => Vec::new(),
        }
    }

    pub fn resolve(&self, id: zpdf_core::ObjectId) -> Result<PdfObject> {
        self.resolve_depth(id, 0)
    }

    fn resolve_depth(&self, id: ObjectId, depth: u32) -> Result<PdfObject> {
        /// Maximum length of a ref-to-ref chain (`1 0 obj 2 0 R endobj` ...)
        /// followed before the reference is treated as null. Guards against
        /// reference cycles (`A -> B -> A`) without a per-call visited set.
        const MAX_REF_CHAIN: u32 = 32;
        if depth > MAX_REF_CHAIN {
            tracing::warn!(
                "indirect reference chain longer than {MAX_REF_CHAIN} at {id}; treating as null"
            );
            return Ok(PdfObject::Null);
        }

        // Fast path: already resolved. The borrow ends with this block.
        if let Some(obj) = self.object_cache.borrow().get(&id) {
            return Ok(obj.clone());
        }

        // ISO 32000-1, 7.3.10: a reference to an object that is missing from
        // the xref, or marked free, is a reference to the null object — not an
        // error. Cache the Null so the warning fires once per object.
        let Some(entry) = self.xref.get(id) else {
            tracing::warn!("reference to missing object {id}; treating as null");
            self.object_cache.borrow_mut().insert(id, PdfObject::Null);
            return Ok(PdfObject::Null);
        };
        let obj = match entry {
            XrefEntry::InUse { offset, .. } => self.parse_at_offset_checked(*offset, id)?,
            XrefEntry::Compressed {
                stream_obj,
                index_in_stream,
            } => self.extract_from_object_stream(*stream_obj, *index_in_stream)?,
            XrefEntry::Free { .. } => {
                tracing::warn!("reference to free object {id}; treating as null");
                PdfObject::Null
            }
        };

        // A top-level object body may itself be an indirect reference; follow
        // the chain (depth-limited) so callers always get a direct value.
        let obj = match obj {
            PdfObject::Ref(next) => self.resolve_depth(next, depth + 1)?,
            other => other,
        };

        self.object_cache.borrow_mut().insert(id, obj.clone());
        Ok(obj)
    }

    /// Parse the indirect object at `offset`, validating that the header's
    /// `(num, gen)` matches the id the xref claimed lives there. On mismatch or
    /// parse failure, consult the lazily-built repair table (full-file object
    /// scan, run at most once) before giving up.
    fn parse_at_offset_checked(&self, offset: u64, id: ObjectId) -> Result<PdfObject> {
        let parser = ObjectParser::new(&self.data, &self.limits);
        match parser.parse_indirect_with_id(offset as usize) {
            Ok((pid, mut obj)) if pid == id => {
                // Top-level objects parsed straight from the file are encrypted;
                // RC4-decrypt their strings and stream bytes in place (the
                // decryptor skips the /Encrypt object itself). Objects pulled
                // from an ObjStm take the Compressed arm and are already
                // plaintext (the container was decrypted in get_or_decode_objstm).
                if let Some(dec) = &self.decryptor {
                    dec.decrypt_object(&mut obj, id);
                }
                Ok(obj)
            }
            Ok((pid, _)) => {
                tracing::warn!("xref offset {offset} for {id} holds object {pid}; trying repair");
                self.repaired_object(id).ok_or_else(|| {
                    zpdf_core::Error::InvalidObject(
                        offset,
                        format!("xref entry for {id} points at object {pid}"),
                    )
                })
            }
            Err(e) => {
                tracing::warn!("failed to parse {id} at xref offset {offset} ({e}); trying repair");
                match self.repaired_object(id) {
                    Some(obj) => Ok(obj),
                    None => Err(e),
                }
            }
        }
    }

    /// Look up `id` in the repair table, building the table on first use by
    /// running tail-scan recovery over the whole file (memoized; the scan runs
    /// at most once per `PdfFile`). Returns `None` if the scan failed, the id
    /// is not in it, or the repaired entry does not actually hold `id`.
    fn repaired_object(&self, id: ObjectId) -> Option<PdfObject> {
        let table = self
            .repair_table
            .get_or_init(
                || match recovery::scan_all_objects(&self.data, &self.limits) {
                    Ok((table, _trailer)) => Some(table),
                    Err(e) => {
                        tracing::warn!("repair object scan failed: {e}");
                        None
                    }
                },
            )
            .as_ref()?;
        match table.get(id)? {
            XrefEntry::InUse { offset, .. } => {
                let parser = ObjectParser::new(&self.data, &self.limits);
                let (pid, mut obj) = parser.parse_indirect_with_id(*offset as usize).ok()?;
                if pid != id {
                    return None;
                }
                if let Some(dec) = &self.decryptor {
                    dec.decrypt_object(&mut obj, id);
                }
                Some(obj)
            }
            XrefEntry::Compressed {
                stream_obj,
                index_in_stream,
            } => self
                .extract_from_object_stream(*stream_obj, *index_in_stream)
                .ok(),
            XrefEntry::Free { .. } => None,
        }
    }

    /// Resolve a stream object and decode its data through the filter pipeline.
    /// `/Filter` and `/DecodeParms` may be indirect references (or arrays
    /// containing them); resolve those before handing the dict to the filter
    /// layer, which has no access to the file.
    pub fn resolve_stream_data(&self, id: zpdf_core::ObjectId) -> Result<Vec<u8>> {
        let obj = self.resolve(id)?;
        let stream = obj.as_stream()?;
        match self.dict_with_resolved_filters(&stream.dict) {
            Some(resolved) => filters::decode_stream(&stream.data, &resolved),
            None => filters::decode_stream(&stream.data, &stream.dict),
        }
    }

    /// If `/Filter`, `/DecodeParms`, or `/DP` is an indirect reference (or an
    /// array containing one), return a clone of `dict` with those values
    /// resolved one level. `None` when nothing needs resolving (common case —
    /// avoids cloning the dict).
    fn dict_with_resolved_filters(&self, dict: &PdfDict) -> Option<PdfDict> {
        const KEYS: [&str; 3] = ["Filter", "DecodeParms", "DP"];
        let needs_resolve = |obj: &PdfObject| match obj {
            PdfObject::Ref(_) => true,
            PdfObject::Array(a) => a.iter().any(|e| matches!(e, PdfObject::Ref(_))),
            _ => false,
        };
        if !KEYS.iter().any(|k| dict.get(k).is_some_and(needs_resolve)) {
            return None;
        }

        let resolve_shallow = |obj: &PdfObject| match obj {
            PdfObject::Ref(r) => self.resolve(*r).unwrap_or(PdfObject::Null),
            other => other.clone(),
        };
        let mut out = dict.clone();
        for key in KEYS {
            let Some(value) = dict.get(key) else { continue };
            let resolved = match resolve_shallow(value) {
                // Also resolve refs *inside* a (possibly itself indirect) array.
                PdfObject::Array(a) => PdfObject::Array(a.iter().map(resolve_shallow).collect()),
                other => other,
            };
            out.insert(PdfName::new(key), resolved);
        }
        Some(out)
    }

    /// Extract an object from a compressed object stream (/Type /ObjStm).
    fn extract_from_object_stream(
        &self,
        stream_obj_num: u32,
        index_in_stream: u32,
    ) -> Result<PdfObject> {
        let objstm = self.get_or_decode_objstm(stream_obj_num)?;

        let idx = index_in_stream as usize;
        if idx >= objstm.entries.len() {
            return Err(zpdf_core::Error::InvalidObject(
                0,
                format!(
                    "object stream index {idx} out of range (n={})",
                    objstm.entries.len()
                ),
            ));
        }

        let (_, obj_offset) = objstm.entries[idx];
        let oob = || {
            zpdf_core::Error::InvalidObject(0, "object stream member offset out of range".into())
        };
        let data_start = objstm.first.checked_add(obj_offset).ok_or_else(oob)?;
        let data_end = if idx + 1 < objstm.entries.len() {
            objstm
                .first
                .checked_add(objstm.entries[idx + 1].1)
                .ok_or_else(oob)?
        } else {
            objstm.data.len()
        };

        // Member offsets are attacker-controlled and need not be monotonic, so
        // guard against start > end and out-of-bounds before slicing (would
        // otherwise panic).
        let data_end = data_end.min(objstm.data.len());
        if data_start > data_end {
            return Err(zpdf_core::Error::InvalidObject(
                0,
                "object stream member offsets out of order".into(),
            ));
        }

        let obj_data = &objstm.data[data_start..data_end];
        let mut lexer = Lexer::new(obj_data, 0, &self.limits);
        lexer.next_token()
    }

    /// Get a decoded object stream from cache, decoding+parsing it once on miss.
    /// Resolves the ObjStm container directly from the xref (it cannot itself
    /// live in another ObjStm) WITHOUT going through `self.resolve`, so it never
    /// re-enters the `object_cache` borrow.
    fn get_or_decode_objstm(&self, stream_obj_num: u32) -> Result<Arc<DecodedObjStm>> {
        if let Some(hit) = self.objstm_cache.borrow().get(&stream_obj_num) {
            return Ok(Arc::clone(hit));
        }

        let stream_id = zpdf_core::ObjectId(stream_obj_num, 0);
        let stream_entry = self
            .xref
            .get(stream_id)
            .ok_or(zpdf_core::Error::ObjectNotFound(stream_id))?;
        let stream_obj = match stream_entry {
            XrefEntry::InUse { offset, .. } => {
                let parser = ObjectParser::new(&self.data, &self.limits);
                parser.parse_indirect_at(*offset as usize)?
            }
            _ => return Err(zpdf_core::Error::ObjectNotFound(stream_id)),
        };

        let stream: &PdfStream = stream_obj.as_stream()?;
        // Reject negative /N and /First (attacker-controlled): a negative i64 cast
        // straight to usize becomes a near-usize::MAX value that overflows the
        // offset arithmetic later.
        let neg =
            |what: &str| zpdf_core::Error::InvalidObject(0, format!("ObjStm {what} is negative"));
        let n = usize::try_from(stream.dict.get_i64("N")?).map_err(|_| neg("/N"))?;
        let first = usize::try_from(stream.dict.get_i64("First")?).map_err(|_| neg("/First"))?;

        // An encrypted document encrypts the ObjStm *container* once (keyed by
        // the container's own object id); its member objects are not separately
        // encrypted. Decrypt the raw bytes before running the filter pipeline.
        let raw: std::borrow::Cow<[u8]> = match &self.decryptor {
            Some(dec) => std::borrow::Cow::Owned(
                dec.decrypt_stream_bytes(zpdf_core::ObjectId(stream_obj_num, 0), &stream.data),
            ),
            None => std::borrow::Cow::Borrowed(&stream.data),
        };
        let decoded = filters::decode_stream(&raw, &stream.dict)?;

        // Parse the header: N pairs of (obj_num, offset_within_data). Capacity is
        // bounded by the header length to avoid a huge allocation on a bogus /N.
        let header = &decoded[..first.min(decoded.len())];
        let mut header_lexer = Lexer::new(header, 0, &self.limits);
        let mut entries = Vec::with_capacity(n.min(header.len()));
        for _ in 0..n {
            let obj_num_tok = header_lexer.next_token()?;
            let offset_tok = header_lexer.next_token()?;
            let obj_num = obj_num_tok.as_i64()? as u32;
            let offset = usize::try_from(offset_tok.as_i64()?).map_err(|_| neg("member offset"))?;
            entries.push((obj_num, offset));
        }

        let decoded_arc = Arc::new(DecodedObjStm {
            data: Arc::<[u8]>::from(decoded),
            first,
            entries,
        });
        self.objstm_cache
            .borrow_mut()
            .insert(stream_obj_num, Arc::clone(&decoded_arc));
        Ok(decoded_arc)
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }
}

/// Best-effort check that the trailer's /Root points at a usable Catalog. Runs
/// once at open time (before `PdfFile` exists), so it is a free function that
/// parses the Root directly rather than going through `PdfFile::resolve`.
///
/// Lenient by design: a Root that is present but compressed/free is trusted
/// (the normal pipeline handles it); only a direct InUse Root is strictly
/// checked for `/Type /Catalog`. A missing Root triggers recovery.
fn root_resolves(
    data: &[u8],
    xref: &XrefTable,
    trailer: &zpdf_core::PdfDict,
    limits: &ParseLimits,
) -> bool {
    let Ok(root_ref) = trailer.get_ref("Root") else {
        return false;
    };
    match xref.get(root_ref) {
        Some(XrefEntry::InUse { offset, .. }) => {
            let parser = ObjectParser::new(data, limits);
            matches!(
                parser
                    .parse_indirect_at(*offset as usize)
                    .ok()
                    .and_then(|o| o
                        .as_dict()
                        .ok()
                        .map(|d| d.get_name("Type").unwrap_or("").to_string())),
                Some(t) if t == "Catalog"
            )
        }
        Some(_) => true, // compressed/free-but-present: trust the normal pipeline
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Validates the object-stream header parse + body-slicing arithmetic that
    /// `get_or_decode_objstm`/`extract_from_object_stream` rely on, without
    /// needing a full xref-stream fixture.
    #[test]
    fn objstm_header_and_slicing_math() {
        let limits = ParseLimits::default();
        let o10 = b"<< /Type /Catalog /Pages 2 0 R >>";
        let o11 = b"42";
        let header = format!("10 0 11 {} ", o10.len() + 1);
        let first = header.len();
        let mut decoded = header.into_bytes();
        decoded.extend_from_slice(o10);
        decoded.push(b' ');
        decoded.extend_from_slice(o11);

        // Mirror the header parse.
        let mut hx = Lexer::new(&decoded[..first], 0, &limits);
        let mut entries = Vec::new();
        for _ in 0..2 {
            let num = hx.next_token().unwrap().as_i64().unwrap() as u32;
            let off = hx.next_token().unwrap().as_i64().unwrap() as usize;
            entries.push((num, off));
        }
        assert_eq!(entries, vec![(10, 0), (11, o10.len() + 1)]);

        // Slice + lex object index 0 (obj 10).
        let (start0, end0) = (first + entries[0].1, first + entries[1].1);
        let obj = Lexer::new(&decoded[start0..end0], 0, &limits)
            .next_token()
            .unwrap();
        assert!(obj.as_dict().is_ok(), "obj 10 should lex as a dict");

        // Slice + lex object index 1 (obj 11) — runs to end of decoded.
        let start1 = first + entries[1].1;
        let n = Lexer::new(&decoded[start1..], 0, &limits)
            .next_token()
            .unwrap();
        assert_eq!(n.as_i64().unwrap(), 42);
    }

    /// Assemble a minimal PDF: the given `(num, body)` objects at gen 0, a
    /// traditional xref covering each (one single-entry subsection apiece),
    /// and a trailer pointing /Root at `root`.
    fn build_pdf(objects: &[(u32, &str)], root: u32) -> Vec<u8> {
        let mut d = Vec::from(&b"%PDF-1.4\n"[..]);
        let mut offsets = Vec::new();
        for (num, body) in objects {
            offsets.push((*num, d.len()));
            d.extend_from_slice(format!("{num} 0 obj\n{body}\nendobj\n").as_bytes());
        }
        let xref_off = d.len();
        d.extend_from_slice(b"xref\n0 1\n0000000000 65535 f \n");
        for (num, off) in &offsets {
            d.extend_from_slice(format!("{num} 1\n{off:010} 00000 n \n").as_bytes());
        }
        let size = objects.iter().map(|(n, _)| n + 1).max().unwrap_or(1);
        d.extend_from_slice(
            format!("trailer\n<< /Size {size} /Root {root} 0 R >>\nstartxref\n{xref_off}\n%%EOF\n")
                .as_bytes(),
        );
        d
    }

    #[test]
    fn dangling_ref_resolves_to_null() {
        // Object 9 is referenced but absent from the xref entirely: per
        // ISO 32000 7.3.10 it resolves to null, not an error.
        let pdf = build_pdf(&[(1, "<< /Type /Catalog /Pages 9 0 R >>")], 1);
        let file = PdfFile::parse(pdf).unwrap();
        assert_eq!(file.resolve(ObjectId(9, 0)).unwrap(), PdfObject::Null);
        // Second resolve hits the cache (warn fires once).
        assert_eq!(file.resolve(ObjectId(9, 0)).unwrap(), PdfObject::Null);
    }

    #[test]
    fn free_entry_resolves_to_null() {
        let mut d = Vec::from(&b"%PDF-1.4\n"[..]);
        let off1 = d.len();
        d.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
        let xref_off = d.len();
        d.extend_from_slice(b"xref\n0 1\n0000000000 65535 f \n1 1\n");
        d.extend_from_slice(format!("{off1:010} 00000 n \n").as_bytes());
        d.extend_from_slice(b"2 1\n0000000000 00000 f \n");
        d.extend_from_slice(
            format!("trailer\n<< /Size 3 /Root 1 0 R >>\nstartxref\n{xref_off}\n%%EOF\n")
                .as_bytes(),
        );

        let file = PdfFile::parse(d).unwrap();
        assert!(matches!(
            file.xref.get(ObjectId(2, 0)),
            Some(XrefEntry::Free { .. })
        ));
        assert_eq!(file.resolve(ObjectId(2, 0)).unwrap(), PdfObject::Null);
    }

    #[test]
    fn header_mismatch_triggers_lazy_repair() {
        // The xref entry for object 3 points at object 2's offset; the real
        // object 3 lives elsewhere. resolve(3) must repair via the lazy scan.
        let mut d = Vec::from(&b"%PDF-1.4\n"[..]);
        let off1 = d.len();
        d.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
        let off2 = d.len();
        d.extend_from_slice(b"2 0 obj\n<< /Marker /Wrong >>\nendobj\n");
        // Real object 3 — its offset is deliberately NOT in the xref.
        d.extend_from_slice(b"3 0 obj\n<< /Marker /Real >>\nendobj\n");
        let xref_off = d.len();
        d.extend_from_slice(b"xref\n0 1\n0000000000 65535 f \n");
        d.extend_from_slice(format!("1 1\n{off1:010} 00000 n \n").as_bytes());
        d.extend_from_slice(format!("2 1\n{off2:010} 00000 n \n").as_bytes());
        d.extend_from_slice(format!("3 1\n{off2:010} 00000 n \n").as_bytes()); // wrong!
        d.extend_from_slice(
            format!("trailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n{xref_off}\n%%EOF\n")
                .as_bytes(),
        );

        let file = PdfFile::parse(d).unwrap();
        let obj = file.resolve(ObjectId(3, 0)).unwrap();
        assert_eq!(obj.as_dict().unwrap().get_name("Marker").unwrap(), "Real");
        // Object 2 still resolves normally (its entry was correct).
        let obj2 = file.resolve(ObjectId(2, 0)).unwrap();
        assert_eq!(obj2.as_dict().unwrap().get_name("Marker").unwrap(), "Wrong");
    }

    #[test]
    fn ref_to_ref_chain_resolves() {
        let pdf = build_pdf(
            &[
                (1, "<< /Type /Catalog /Pages 2 0 R >>"),
                (4, "5 0 R"),
                (5, "42"),
            ],
            1,
        );
        let file = PdfFile::parse(pdf).unwrap();
        assert_eq!(
            file.resolve(ObjectId(4, 0)).unwrap(),
            PdfObject::Integer(42)
        );
    }

    #[test]
    fn ref_cycle_resolves_to_null() {
        // 4 -> 5 -> 4: the chain guard must terminate (no hang/stack overflow)
        // and degrade the value to null.
        let pdf = build_pdf(
            &[
                (1, "<< /Type /Catalog /Pages 2 0 R >>"),
                (4, "5 0 R"),
                (5, "4 0 R"),
            ],
            1,
        );
        let file = PdfFile::parse(pdf).unwrap();
        assert_eq!(file.resolve(ObjectId(4, 0)).unwrap(), PdfObject::Null);
    }

    #[test]
    fn indirect_filter_is_resolved() {
        use flate2::write::ZlibEncoder;
        use flate2::Compression;
        use std::io::Write;

        let payload = b"indirect filter payload";
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(payload).unwrap();
        let compressed = enc.finish().unwrap();

        let mut d = Vec::from(&b"%PDF-1.4\n"[..]);
        let off1 = d.len();
        d.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
        let off3 = d.len();
        d.extend_from_slice(
            format!(
                "3 0 obj\n<< /Length {} /Filter 4 0 R >>\nstream\n",
                compressed.len()
            )
            .as_bytes(),
        );
        d.extend_from_slice(&compressed);
        d.extend_from_slice(b"\nendstream\nendobj\n");
        let off4 = d.len();
        d.extend_from_slice(b"4 0 obj\n/FlateDecode\nendobj\n");
        let xref_off = d.len();
        d.extend_from_slice(b"xref\n0 1\n0000000000 65535 f \n");
        d.extend_from_slice(format!("1 1\n{off1:010} 00000 n \n").as_bytes());
        d.extend_from_slice(format!("3 1\n{off3:010} 00000 n \n").as_bytes());
        d.extend_from_slice(format!("4 1\n{off4:010} 00000 n \n").as_bytes());
        d.extend_from_slice(
            format!("trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{xref_off}\n%%EOF\n")
                .as_bytes(),
        );

        let file = PdfFile::parse(d).unwrap();
        let data = file.resolve_stream_data(ObjectId(3, 0)).unwrap();
        assert_eq!(data, payload);
    }
}
