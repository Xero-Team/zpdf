mod ccitt;
mod crypt;
pub use crypt::Decryptor;
pub mod filters;
mod header;
mod jbig2;
mod lexer;
mod object_parser;
mod recovery;
mod xref;

pub use header::PdfHeader;
pub use lexer::Lexer;
pub use object_parser::ObjectParser;
pub use xref::{XrefEntry, XrefTable};

use std::cell::{Cell, OnceCell, RefCell};
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
    /// trailer `/Encrypt` dict. `None` for unencrypted
    /// documents, in which case `resolve`/object-stream decoding are unchanged.
    decryptor: Option<crypt::Decryptor>,
    /// Cache of resolved top-level indirect objects, keyed by ObjectId.
    /// `RefCell` suffices: `PdfFile` is never shared across threads in this
    /// workspace (swap to `Mutex` if that ever changes).
    /// Entries are shared so [`Self::resolve_shared`] can hand them out
    /// without cloning.
    object_cache: RefCell<HashMap<ObjectId, Arc<PdfObject>>>,
    /// Estimated retained bytes in `object_cache`. Cache admission stops at the
    /// caller's limit; resolution still succeeds without retaining the object.
    object_cache_bytes: Cell<u64>,
    /// Cache of decoded object streams, keyed by the ObjStm object number.
    /// Avoids re-decoding the whole stream for every compressed object it holds.
    objstm_cache: RefCell<HashMap<u32, Arc<DecodedObjStm>>>,
    /// Retained decoded-object-stream bytes, including parsed header entries.
    objstm_cache_bytes: Cell<u64>,
    /// Object streams that failed to decode, so the members of a broken one
    /// do not each inflate it again.
    objstm_failed: RefCell<std::collections::HashSet<u32>>,
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
        Self::parse_with_password_and_limits(data, b"", limits)
    }

    /// Open with a user/owner password (for documents the empty password cannot
    /// decrypt). Returns [`zpdf_core::Error::WrongPassword`] if it authenticates
    /// as neither.
    pub fn parse_with_password(data: impl Into<Arc<[u8]>>, password: &[u8]) -> Result<Self> {
        Self::parse_with_password_and_limits(data, password, ParseLimits::default())
    }

    pub fn parse_with_password_and_limits(
        data: impl Into<Arc<[u8]>>,
        password: &[u8],
        limits: ParseLimits,
    ) -> Result<Self> {
        let data: Arc<[u8]> = data.into();
        // A missing `%PDF` marker is not fatal on its own: a sliced/headerless
        // fragment that begins directly with `N G obj` can still be opened by the
        // object-scan recovery below. Defer the NotAPdf verdict until recovery
        // has also come up empty.
        let header_res = header::parse_header(&data);

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
                    // least produced a table, else surface the most useful error.
                    // For a file that never carried a `%PDF` marker, NotAPdf is
                    // more accurate than the recovery layer's InvalidXref.
                    Err(rec_err) => match other {
                        Ok(parsed) => parsed,
                        Err(_) if header_res.is_err() => return Err(zpdf_core::Error::NotAPdf),
                        Err(_) => return Err(rec_err),
                    },
                }
            }
        };
        // Past this point the document is structurally usable; if the version
        // header was absent entirely, assume a modern default (matching
        // header::parse_header's malformed-version fallback) rather than failing.
        let header = header_res.unwrap_or(PdfHeader { major: 1, minor: 7 });

        let mut file = Self {
            data,
            header,
            xref,
            trailer,
            limits,
            decryptor: None,
            object_cache: RefCell::new(HashMap::new()),
            object_cache_bytes: Cell::new(0),
            objstm_cache: RefCell::new(HashMap::new()),
            objstm_cache_bytes: Cell::new(0),
            objstm_failed: RefCell::new(Default::default()),
            repair_table: OnceCell::new(),
        };
        // Build the decryptor *after* construction so it can use `resolve` to
        // fetch the (never-encrypted) /Encrypt dict; `decryptor` is still `None`
        // at this point, so that resolve does not try to decrypt it.
        file.decryptor = file.build_decryptor(password)?;
        Ok(file)
    }

    /// True when this successfully opened document is encrypted.
    pub fn is_encrypted(&self) -> bool {
        self.trailer.get("Encrypt").is_some()
    }

    /// The document's decryptor, when one was built at open time. Writers use
    /// it to encrypt objects added to an encrypted document with its key.
    pub fn decryptor(&self) -> Option<&crypt::Decryptor> {
        self.decryptor.as_ref()
    }

    /// Construct the Standard-security-handler decryptor from the trailer
    /// `/Encrypt` dictionary, the first element of `/ID`, and the password.
    /// `Ok(None)` only for unencrypted documents. Authentication failures,
    /// unsupported handlers and malformed dictionaries return distinct errors.
    fn build_decryptor(&self, password: &[u8]) -> Result<Option<crypt::Decryptor>> {
        // /Encrypt is normally an indirect reference, but a direct dict is
        // legal too (a direct dict has no object id to exempt from decryption).
        // The /Encrypt dict is itself never encrypted; resolve it directly.
        let Some(enc) = self.trailer.get("Encrypt") else {
            return Ok(None);
        };
        let (enc_obj, encrypt_ref) = match enc {
            PdfObject::Ref(r) => (self.resolve(*r)?, Some(*r)),
            direct => (direct.clone(), None),
        };
        let enc_dict = enc_obj.as_dict()?;
        let id_first = self.first_id_bytes();
        match crypt::Decryptor::from_encrypt_dict(enc_dict, &id_first, encrypt_ref, password) {
            crypt::BuildResult::Decryptor(d) => Ok(Some(d)),
            crypt::BuildResult::Degrade => Err(zpdf_core::Error::UnsupportedEncryption),
            crypt::BuildResult::WrongPassword => Err(zpdf_core::Error::WrongPassword),
            crypt::BuildResult::PasswordRequired => Err(zpdf_core::Error::PasswordRequired),
            crypt::BuildResult::InvalidEncryption(message) => {
                Err(zpdf_core::Error::InvalidObject(0, message.into()))
            }
        }
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

    /// Like [`Self::resolve`], but shares a cached object instead of cloning
    /// it — for callers that only read a large object, such as a page-tree
    /// node's `/Kids`, once per page.
    pub fn resolve_shared(&self, id: zpdf_core::ObjectId) -> Result<Arc<PdfObject>> {
        if let Some(obj) = self.object_cache.borrow().get(&id) {
            return Ok(Arc::clone(obj));
        }
        let obj = self.resolve_depth(id, 0)?;
        // `resolve_depth` cached it unless the cache budget is spent.
        if let Some(cached) = self.object_cache.borrow().get(&id) {
            return Ok(Arc::clone(cached));
        }
        Ok(Arc::new(obj))
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
            return Ok(PdfObject::clone(obj));
        }

        // ISO 32000-1, 7.3.10: a reference to an object that is missing from
        // the xref, or marked free, is a reference to the null object — not an
        // error. BUT a damaged xref frequently just omits (or wrongly frees)
        // objects that physically exist in the file, which would silently empty
        // the page tree. So before treating a missing/free entry as null, give
        // the lazy repair table (one memoized full-file scan) a chance to locate
        // the real object. The Null is cached either way so the warning fires
        // once per object and a genuinely-dangling ref stays cheap.
        let obj = match self.xref.get(id) {
            Some(XrefEntry::InUse { offset, .. }) => self.parse_at_offset_checked(*offset, id)?,
            Some(XrefEntry::Compressed {
                stream_obj,
                index_in_stream,
            }) => match self.extract_from_object_stream(*stream_obj, *index_in_stream, id) {
                Ok(obj) => obj,
                // A damaged container or a wrong entry: the object may still
                // exist elsewhere in the file.
                Err(e) => self.repaired_object(id).ok_or(e)?,
            },
            // Deleted by an incremental update: the stale bytes the repair
            // scan would find are exactly what the update removed.
            None if self.xref.is_superseded(id) => PdfObject::Null,
            Some(XrefEntry::Free { .. }) => match self.repaired_object(id) {
                Some(obj) => obj,
                None => {
                    tracing::warn!("reference to free object {id}; treating as null");
                    PdfObject::Null
                }
            },
            None => match self.repaired_object(id) {
                Some(obj) => obj,
                None => {
                    tracing::warn!("reference to missing object {id}; treating as null");
                    PdfObject::Null
                }
            },
        };

        // A top-level object body may itself be an indirect reference; follow
        // the chain (depth-limited) so callers always get a direct value.
        let obj = match obj {
            PdfObject::Ref(next) => self.resolve_depth(next, depth + 1)?,
            other => other,
        };

        self.cache_object(id, &obj);
        Ok(obj)
    }

    /// Admit a resolved object only while the configured retained-memory budget
    /// has room. A full cache degrades to reparsing instead of retaining an
    /// attacker-controlled number of objects for the document's lifetime.
    fn cache_object(&self, id: ObjectId, obj: &PdfObject) {
        if self.object_cache.borrow().contains_key(&id) {
            return;
        }
        let cost = estimate_cached_object_bytes(obj);
        let used = self.object_cache_bytes.get();
        if cost > self.limits.max_object_cache_bytes.saturating_sub(used) {
            return;
        }
        self.object_cache
            .borrow_mut()
            .insert(id, Arc::new(obj.clone()));
        self.object_cache_bytes.set(used.saturating_add(cost));
    }

    /// Parse the indirect object at `offset`, validating that the header's
    /// `(num, gen)` matches the id the xref claimed lives there. On mismatch or
    /// parse failure, consult the lazily-built repair table (full-file object
    /// scan, run at most once) before giving up.
    fn parse_at_offset_checked(&self, offset: u64, id: ObjectId) -> Result<PdfObject> {
        let resolve_length = |id| self.indirect_length(id);
        let parser =
            ObjectParser::new(&self.data, &self.limits).with_length_resolver(&resolve_length);
        let file_offset = usize::try_from(offset).map_err(|_| {
            zpdf_core::Error::InvalidObject(offset, "xref offset exceeds address space".into())
        })?;
        match parser.parse_indirect_with_id(file_offset) {
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

    /// The integer value of object `id`, for an indirect stream `/Length`:
    /// from the object cache, or parsed straight from its xref offset. A
    /// compressed or non-integer object yields `None` (the caller then scans
    /// for `endstream`): decoding an object stream here could recurse through
    /// that stream's own `/Length`.
    fn indirect_length(&self, id: ObjectId) -> Option<i64> {
        if let Some(obj) = self.object_cache.borrow().get(&id) {
            return obj.as_i64().ok();
        }
        let Some(XrefEntry::InUse { offset, .. }) = self.xref.get(id) else {
            return None;
        };
        let offset = usize::try_from(*offset).ok()?;
        match ObjectParser::new(&self.data, &self.limits).parse_indirect_with_id(offset) {
            Ok((pid, PdfObject::Integer(n))) if pid == id => Some(n),
            _ => None,
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
                || match recovery::scan_object_table(&self.data, &self.limits) {
                    Ok(table) => Some(table),
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
                let file_offset = usize::try_from(*offset).ok()?;
                let (pid, mut obj) = parser.parse_indirect_with_id(file_offset).ok()?;
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
                .extract_from_object_stream(*stream_obj, *index_in_stream, id)
                .ok(),
            XrefEntry::Free { .. } => None,
        }
    }

    /// Resolve a stream object and decode its data through the filter pipeline.
    /// `/Filter` and `/DecodeParms` may be indirect references (or arrays
    /// containing them); resolve those before handing the dict to the filter
    /// layer, which has no access to the file.
    pub fn resolve_stream_data(&self, id: zpdf_core::ObjectId) -> Result<Vec<u8>> {
        self.resolve_stream_data_inner(id, true)
    }

    fn resolve_stream_data_inner(
        &self,
        id: zpdf_core::ObjectId,
        inline_globals: bool,
    ) -> Result<Vec<u8>> {
        let obj = self.resolve(id)?;
        let stream = obj.as_stream()?;
        match self.dict_with_resolved_filters(&stream.dict, inline_globals) {
            Some(resolved) => {
                filters::decode_stream_with_limits(&stream.data, &resolved, &self.limits)
            }
            None => filters::decode_stream_with_limits(&stream.data, &stream.dict, &self.limits),
        }
    }

    /// If `/Filter`, `/DecodeParms`, or `/DP` is an indirect reference (or an
    /// array containing one), return a clone of `dict` with those values
    /// resolved one level. `None` when nothing needs resolving (common case —
    /// avoids cloning the dict). When `inline_globals` is set, a DecodeParms
    /// `/JBIG2Globals` stream reference is also inlined (see
    /// [`Self::inline_jbig2_globals`]).
    fn dict_with_resolved_filters(&self, dict: &PdfDict, inline_globals: bool) -> Option<PdfDict> {
        const KEYS: [&str; 3] = ["Filter", "DecodeParms", "DP"];
        // A DecodeParms dict containing a /JBIG2Globals reference needs the
        // globals stream inlined even though the dict itself is direct.
        let dict_needs_globals = |obj: &PdfObject| {
            inline_globals
                && matches!(obj, PdfObject::Dict(d)
                    if matches!(d.get("JBIG2Globals"), Some(PdfObject::Ref(_))))
        };
        let needs_resolve = |obj: &PdfObject| match obj {
            PdfObject::Ref(_) => true,
            PdfObject::Array(a) => a
                .iter()
                .any(|e| matches!(e, PdfObject::Ref(_)) || dict_needs_globals(e)),
            other => dict_needs_globals(other),
        };
        if !KEYS.iter().any(|k| dict.get(k).is_some_and(needs_resolve)) {
            return None;
        }

        let resolve_shallow = |obj: &PdfObject| match obj {
            PdfObject::Ref(r) => self.resolve(*r).unwrap_or(PdfObject::Null),
            other => other.clone(),
        };
        let inline = |obj: PdfObject| {
            if inline_globals {
                self.inline_jbig2_globals(obj)
            } else {
                obj
            }
        };
        let mut out = dict.clone();
        for key in KEYS {
            let Some(value) = dict.get(key) else { continue };
            let resolved = match resolve_shallow(value) {
                // Also resolve refs *inside* a (possibly itself indirect) array.
                PdfObject::Array(a) => {
                    PdfObject::Array(a.iter().map(resolve_shallow).map(inline).collect())
                }
                other => inline(other),
            };
            out.insert(PdfName::new(key), resolved);
        }
        Some(out)
    }

    /// If `obj` is a DecodeParms dict whose `/JBIG2Globals` is an indirect
    /// stream reference, replace the reference with an inline string holding
    /// the globals stream's *decoded* bytes — the filter layer has no file
    /// access to chase references itself. The globals stream is decoded
    /// without globals inlining of its own, so a crafted reference cycle
    /// cannot recurse. Anything else passes through unchanged.
    fn inline_jbig2_globals(&self, obj: PdfObject) -> PdfObject {
        let PdfObject::Dict(mut d) = obj else {
            return obj;
        };
        if let Some(PdfObject::Ref(r)) = d.get("JBIG2Globals") {
            let r = *r;
            let value = match self.resolve_stream_data_inner(r, false) {
                Ok(bytes) => PdfObject::String(zpdf_core::PdfString(bytes)),
                Err(e) => {
                    tracing::warn!("failed to decode /JBIG2Globals stream {r}: {e}");
                    PdfObject::Null
                }
            };
            d.insert(PdfName::new("JBIG2Globals"), value);
        }
        PdfObject::Dict(d)
    }

    /// Extract an object from a compressed object stream (/Type /ObjStm).
    /// Extract object `id`, listed at `index_in_stream` of object stream
    /// `stream_obj_num`. A member at that index with another object number
    /// means a wrong xref entry; the header is then searched for `id` instead
    /// of returning an unrelated object (the listed index still serves a
    /// header whose numbers are wrong).
    fn extract_from_object_stream(
        &self,
        stream_obj_num: u32,
        index_in_stream: u32,
        id: ObjectId,
    ) -> Result<PdfObject> {
        let objstm = self.get_or_decode_objstm(stream_obj_num)?;

        let listed = index_in_stream as usize;
        let idx = match objstm.entries.get(listed) {
            Some(&(num, _)) if num == id.0 => listed,
            _ => objstm
                .entries
                .iter()
                .position(|&(num, _)| num == id.0)
                .or((listed < objstm.entries.len()).then_some(listed))
                .ok_or_else(|| {
                    zpdf_core::Error::InvalidObject(
                        0,
                        format!(
                            "object stream {stream_obj_num} has no object {} (index {listed}, n={})",
                            id.0,
                            objstm.entries.len()
                        ),
                    )
                })?,
        };

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
        if self.objstm_failed.borrow().contains(&stream_obj_num) {
            return Err(zpdf_core::Error::StreamDecode(format!(
                "object stream {stream_obj_num} failed to decode"
            )));
        }
        let decoded = self.decode_objstm(stream_obj_num);
        if decoded.is_err() {
            self.objstm_failed.borrow_mut().insert(stream_obj_num);
        }
        decoded
    }

    fn decode_objstm(&self, stream_obj_num: u32) -> Result<Arc<DecodedObjStm>> {
        let stream_id = zpdf_core::ObjectId(stream_obj_num, 0);
        let stream_entry = self
            .xref
            .get(stream_id)
            .ok_or(zpdf_core::Error::ObjectNotFound(stream_id))?;
        let stream_obj = match stream_entry {
            XrefEntry::InUse { offset, .. } => {
                let resolve_length = |id| self.indirect_length(id);
                let parser = ObjectParser::new(&self.data, &self.limits)
                    .with_length_resolver(&resolve_length);
                let file_offset = usize::try_from(*offset).map_err(|_| {
                    zpdf_core::Error::InvalidObject(
                        *offset,
                        "object-stream offset exceeds address space".into(),
                    )
                })?;
                parser.parse_indirect_at(file_offset)?
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
        if n > self.limits.max_objects as usize {
            return Err(zpdf_core::Error::StreamDecode(format!(
                "ObjStm /N {n} exceeds object limit {}",
                self.limits.max_objects
            )));
        }

        // An encrypted document encrypts the ObjStm *container* once (keyed by
        // the container's own object id); its member objects are not separately
        // encrypted. Decrypt the raw bytes before running the filter pipeline.
        let raw: std::borrow::Cow<[u8]> = match &self.decryptor {
            Some(dec) => std::borrow::Cow::Owned(
                dec.decrypt_stream_bytes(zpdf_core::ObjectId(stream_obj_num, 0), &stream.data),
            ),
            None => std::borrow::Cow::Borrowed(&stream.data),
        };
        let decoded = filters::decode_stream_with_limits(&raw, &stream.dict, &self.limits)?;

        // Parse the header: N pairs of (obj_num, offset_within_data). Capacity is
        // bounded by the header length to avoid a huge allocation on a bogus /N.
        let header = &decoded[..first.min(decoded.len())];
        let mut header_lexer = Lexer::new(header, 0, &self.limits);
        // A malformed pair (or a /N larger than the pairs present, a common
        // writer off-by-one) ends the header; the members before it stay usable.
        let mut entries = Vec::with_capacity(n.min(header.len()));
        for _ in 0..n {
            let pair = (header_lexer.next_token(), header_lexer.next_token());
            let (Ok(obj_num_tok), Ok(offset_tok)) = pair else {
                break;
            };
            let num = obj_num_tok
                .as_i64()
                .ok()
                .and_then(|n| u32::try_from(n).ok());
            let offset = offset_tok
                .as_i64()
                .ok()
                .and_then(|o| usize::try_from(o).ok());
            let (Some(obj_num), Some(offset)) = (num, offset) else {
                break;
            };
            entries.push((obj_num, offset));
        }
        if entries.len() < n {
            tracing::warn!(
                "object stream {stream_obj_num}: header lists {} of /N {n} members",
                entries.len()
            );
        }

        let decoded_arc = Arc::new(DecodedObjStm {
            data: Arc::<[u8]>::from(decoded),
            first,
            entries,
        });
        let cost = estimate_objstm_bytes(&decoded_arc);
        let used = self.objstm_cache_bytes.get();
        if cost <= self.limits.max_objstm_cache_bytes.saturating_sub(used) {
            self.objstm_cache
                .borrow_mut()
                .insert(stream_obj_num, Arc::clone(&decoded_arc));
            self.objstm_cache_bytes.set(used.saturating_add(cost));
        }
        Ok(decoded_arc)
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Return the active parse limits (M3 security fix).
    pub fn limits(&self) -> &ParseLimits {
        &self.limits
    }

    /// Force-build (once) and return the full-file repair-scan table, or `None`
    /// if the scan found nothing. Shares the `OnceCell` the lazy per-object
    /// repair uses, so the scan runs at most once per `PdfFile`.
    pub fn force_repair_scan(&self) -> Option<&XrefTable> {
        self.repair_table
            .get_or_init(
                || match recovery::scan_object_table(&self.data, &self.limits) {
                    Ok(table) => Some(table),
                    Err(e) => {
                        tracing::warn!("repair object scan failed: {e}");
                        None
                    }
                },
            )
            .as_ref()
    }

    /// Every object id known to this file: the live xref unioned with the
    /// repair-scan table (built on demand). Deduped and sorted by `(num, gen)`.
    pub fn all_object_ids(&self) -> Vec<ObjectId> {
        let mut ids: Vec<ObjectId> = self.xref.object_ids().collect();
        if let Some(table) = self.force_repair_scan() {
            ids.extend(table.object_ids());
        }
        ids.sort_by_key(|id| (id.0, id.1));
        ids.dedup();
        ids
    }

    /// All objects whose dict `/Type` equals `ty`, in `(num, gen)` order.
    /// Resolves through [`Self::resolve`] (so /ObjStm members are decoded and,
    /// for encrypted files, decrypted) and falls back to the repair table for
    /// ids the live xref lacks. Bounded by `limits.max_objects`. The document
    /// layer uses this to rebuild a page list when the /Pages tree is
    /// unreachable.
    pub fn find_objects_by_type(&self, ty: &str) -> Vec<ObjectId> {
        let mut out = Vec::new();
        for id in self.all_object_ids() {
            if out.len() as u32 >= self.limits.max_objects {
                break;
            }
            let obj = match self.resolve(id) {
                Ok(PdfObject::Null) | Err(_) => self.repaired_object(id),
                Ok(o) => Some(o),
            };
            let is_match = obj
                .as_ref()
                .and_then(|o| o.as_dict().ok())
                .map(|d| d.get_name("Type").map(|t| t == ty).unwrap_or(false))
                .unwrap_or(false);
            if is_match {
                out.push(id);
            }
        }
        out
    }
}

/// Conservative heap-size estimate for one cached object and its hash-table
/// entry. It intentionally counts inline child enum storage as well as nested
/// payloads; overestimating only reduces cache hit rate, while underestimating
/// would defeat the retention limit.
fn estimate_cached_object_bytes(obj: &PdfObject) -> u64 {
    const ENTRY_OVERHEAD: u64 = 64;

    fn dict_bytes(dict: &PdfDict) -> u64 {
        dict.0.iter().fold(0u64, |sum, (key, value)| {
            sum.saturating_add(key.0.len() as u64)
                .saturating_add(48) // conservative BTree node/link overhead
                .saturating_add(object_bytes(value))
        })
    }

    fn object_bytes(obj: &PdfObject) -> u64 {
        let base = std::mem::size_of::<PdfObject>() as u64;
        let payload = match obj {
            PdfObject::String(s) => s.0.len() as u64,
            PdfObject::Name(n) => n.0.len() as u64,
            PdfObject::Array(items) => items.iter().fold(
                (items.len() * std::mem::size_of::<PdfObject>()) as u64,
                |sum, item| sum.saturating_add(object_bytes(item)),
            ),
            PdfObject::Dict(dict) => dict_bytes(dict),
            PdfObject::Stream(stream) => {
                (stream.data.len() as u64).saturating_add(dict_bytes(&stream.dict))
            }
            PdfObject::Null
            | PdfObject::Bool(_)
            | PdfObject::Integer(_)
            | PdfObject::Real(_)
            | PdfObject::Ref(_) => 0,
        };
        base.saturating_add(payload)
    }

    ENTRY_OVERHEAD.saturating_add(object_bytes(obj))
}

fn estimate_objstm_bytes(stream: &DecodedObjStm) -> u64 {
    const ENTRY_OVERHEAD: u64 = 64;
    ENTRY_OVERHEAD
        .saturating_add(std::mem::size_of::<DecodedObjStm>() as u64)
        .saturating_add(stream.data.len() as u64)
        .saturating_add((stream.entries.len() * std::mem::size_of::<(u32, usize)>()) as u64)
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
            let Some(file_offset) = usize::try_from(*offset).ok() else {
                return false;
            };
            matches!(
                parser
                    .parse_indirect_at(file_offset)
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
    fn an_object_freed_by_an_incremental_update_resolves_to_null() {
        // The original section has `5 0 n`; the update frees it as `5 1 f`.
        let mut d = b"%PDF-1.4\n".to_vec();
        let o1 = d.len();
        d.extend_from_slice(b"1 0 obj\n<< /Type /Catalog >>\nendobj\n");
        let o5 = d.len();
        d.extend_from_slice(b"5 0 obj\n<< /Deleted true >>\nendobj\n");
        let x1 = d.len();
        d.extend_from_slice(
            format!(
                "xref\n0 2\n0000000000 65535 f \n{o1:010} 00000 n \n5 1\n{o5:010} 00000 n \n\
                 trailer\n<< /Size 6 /Root 1 0 R >>\n"
            )
            .as_bytes(),
        );
        let x2 = d.len();
        d.extend_from_slice(
            format!(
                "xref\n0 1\n0000000005 65535 f \n5 1\n0000000000 00001 f \n\
                 trailer\n<< /Size 6 /Root 1 0 R /Prev {x1} >>\nstartxref\n{x2}\n%%EOF\n"
            )
            .as_bytes(),
        );
        let file = PdfFile::parse(d).unwrap();
        assert_eq!(file.resolve(ObjectId(5, 0)).unwrap(), PdfObject::Null);
        assert_eq!(
            file.resolve(ObjectId(1, 0))
                .unwrap()
                .as_dict()
                .unwrap()
                .get_name("Type")
                .unwrap(),
            "Catalog"
        );
    }

    #[test]
    fn object_stream_members_survive_an_overstated_n() {
        // /N 3 but two header pairs; members 6 and 7 at indices 0 and 1.
        let dec = b"6 0 7 3 42 43";
        let mut d = b"%PDF-1.5\n".to_vec();
        let o1 = d.len();
        d.extend_from_slice(b"1 0 obj\n<< /Type /Catalog >>\nendobj\n");
        let o5 = d.len();
        d.extend_from_slice(
            format!(
                "5 0 obj\n<< /Type /ObjStm /N 3 /First 8 /Length {} >>\nstream\n",
                dec.len()
            )
            .as_bytes(),
        );
        d.extend_from_slice(dec);
        d.extend_from_slice(b"\nendstream\nendobj\n");
        let mut xref = Vec::new();
        for (kind, field2, field3) in [
            (1u8, o1 as u32, 0u8),
            (1, o5 as u32, 0),
            (2, 5, 0),
            (2, 5, 1),
        ] {
            xref.push(kind);
            xref.extend_from_slice(&field2.to_be_bytes());
            xref.push(field3);
        }
        let xo = d.len();
        d.extend_from_slice(
            format!(
                "9 0 obj\n<< /Type /XRef /Size 10 /W [1 4 1] /Index [1 1 5 3] /Root 1 0 R \
                 /Length {} >>\nstream\n",
                xref.len()
            )
            .as_bytes(),
        );
        d.extend_from_slice(&xref);
        d.extend_from_slice(format!("\nendstream\nendobj\nstartxref\n{xo}\n%%EOF\n").as_bytes());
        let file = PdfFile::parse(d).unwrap();
        assert_eq!(
            file.resolve(ObjectId(6, 0)).unwrap(),
            PdfObject::Integer(42)
        );
        assert_eq!(
            file.resolve(ObjectId(7, 0)).unwrap(),
            PdfObject::Integer(43)
        );
    }

    #[test]
    fn resolve_shared_hands_out_the_cached_object() {
        let pdf = build_pdf(&[(1, "<< /Type /Catalog /Kids [1 0 R 1 0 R] >>")], 1);
        let file = PdfFile::parse(pdf).unwrap();
        let a = file.resolve_shared(ObjectId(1, 0)).unwrap();
        let b = file.resolve_shared(ObjectId(1, 0)).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(*a, file.resolve(ObjectId(1, 0)).unwrap());
    }

    #[test]
    fn indirect_stream_length_is_resolved() {
        let pdf = build_pdf(
            &[
                (1, "<< /Type /Catalog >>"),
                (2, "<< /Length 3 0 R >>\nstream\nABC\r\nendstream"),
                (3, "4"),
            ],
            1,
        );
        let file = PdfFile::parse(pdf).unwrap();
        let obj = file.resolve(ObjectId(2, 0)).unwrap();
        assert_eq!(&*obj.as_stream().unwrap().data, b"ABC\r");
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

    #[test]
    fn resolve_stream_uses_file_decode_limits() {
        let pdf = build_pdf(
            &[
                (1, "<< /Type /Catalog >>"),
                (3, "<< /Length 4 >>\nstream\nfour\nendstream"),
            ],
            1,
        );
        let limits = ParseLimits {
            max_decoded_stream_bytes: 3,
            ..ParseLimits::default()
        };
        let file = PdfFile::parse_with_limits(pdf, limits).unwrap();
        assert!(matches!(
            file.resolve_stream_data(ObjectId(3, 0)),
            Err(zpdf_core::Error::StreamSizeLimit(3))
        ));
    }

    #[test]
    fn zero_object_cache_budget_retains_nothing() {
        let pdf = build_pdf(&[(1, "<< /Type /Catalog /Marker (large) >>")], 1);
        let limits = ParseLimits {
            max_object_cache_bytes: 0,
            ..ParseLimits::default()
        };
        let file = PdfFile::parse_with_limits(pdf, limits).unwrap();
        assert_eq!(
            file.resolve(ObjectId(1, 0))
                .unwrap()
                .as_dict()
                .unwrap()
                .get_name("Type")
                .unwrap(),
            "Catalog"
        );
        assert!(file.object_cache.borrow().is_empty());
        assert_eq!(file.object_cache_bytes.get(), 0);
    }

    #[test]
    fn zero_objstm_cache_budget_decodes_without_retention() {
        let pdf = build_pdf(
            &[
                (1, "<< /Type /Catalog >>"),
                (
                    5,
                    "<< /Type /ObjStm /N 1 /First 4 /Length 6 >>\nstream\n6 0 42\nendstream",
                ),
            ],
            1,
        );
        let limits = ParseLimits {
            max_objstm_cache_bytes: 0,
            ..ParseLimits::default()
        };
        let mut file = PdfFile::parse_with_limits(pdf, limits).unwrap();
        file.xref.insert_overwrite(
            ObjectId(6, 0),
            XrefEntry::Compressed {
                stream_obj: 5,
                index_in_stream: 0,
            },
        );
        assert_eq!(
            file.resolve(ObjectId(6, 0)).unwrap(),
            PdfObject::Integer(42)
        );
        assert!(file.objstm_cache.borrow().is_empty());
        assert_eq!(file.objstm_cache_bytes.get(), 0);
    }

    /// An image stream with /Filter /JBIG2Decode whose /DecodeParms holds an
    /// indirect /JBIG2Globals stream: the globals reference must be resolved,
    /// decoded (here through its own FlateDecode), and inlined before the
    /// filter layer runs. The globals carry the page-info segment; the image
    /// stream carries an MMR generic region (two "WWWBBWWW" rows).
    #[test]
    fn jbig2_globals_stream_is_resolved_and_decoded() {
        use flate2::write::ZlibEncoder;
        use flate2::Compression;
        use std::io::Write;

        // Globals: segment 0, type 48 (page information), page 1, 8x2 page.
        let globals: Vec<u8> = [
            &[0, 0, 0, 0, 0x30, 0x00, 0x01, 0, 0, 0, 19][..], // header, length 19
            &[0, 0, 0, 8, 0, 0, 0, 2][..],                    // width 8, height 2
            &[0; 8][..],                                      // x/y resolution
            &[0x00, 0, 0][..],                                // flags, striping
        ]
        .concat();
        let mut gz = ZlibEncoder::new(Vec::new(), Compression::default());
        gz.write_all(&globals).unwrap();
        let globals_z = gz.finish().unwrap();

        // Image stream: segment 1, type 38 (immediate generic region), MMR
        // payload 0x31 0xF8 = T.6-coded WWWBBWWW twice.
        let image: Vec<u8> = [
            &[0, 0, 0, 1, 0x26, 0x00, 0x01, 0, 0, 0, 20][..], // header, length 20
            &[0, 0, 0, 8, 0, 0, 0, 2][..],                    // region 8x2 …
            &[0, 0, 0, 0, 0, 0, 0, 0, 0x00][..],              // … at (0,0), OR
            &[0x01, 0x31, 0xF8][..],                          // MMR flag + data
        ]
        .concat();

        let mut d = Vec::from(&b"%PDF-1.4\n"[..]);
        let off1 = d.len();
        d.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
        let off3 = d.len();
        d.extend_from_slice(
            format!(
                "3 0 obj\n<< /Length {} /Filter /JBIG2Decode \
                 /DecodeParms << /JBIG2Globals 4 0 R >> >>\nstream\n",
                image.len()
            )
            .as_bytes(),
        );
        d.extend_from_slice(&image);
        d.extend_from_slice(b"\nendstream\nendobj\n");
        let off4 = d.len();
        d.extend_from_slice(
            format!(
                "4 0 obj\n<< /Length {} /Filter /FlateDecode >>\nstream\n",
                globals_z.len()
            )
            .as_bytes(),
        );
        d.extend_from_slice(&globals_z);
        d.extend_from_slice(b"\nendstream\nendobj\n");
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
        // WWWBBWWW in PDF 1-bpc polarity (black = 0): 1110 0111, both rows.
        assert_eq!(data, vec![0xE7, 0xE7]);
    }
}
