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

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;
use zpdf_core::{ObjectId, ParseLimits, PdfObject, PdfStream, Result};

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
    /// Cache of resolved top-level indirect objects, keyed by ObjectId.
    /// `RefCell` suffices: `PdfFile` is never shared across threads in this
    /// workspace (swap to `Mutex` if that ever changes).
    object_cache: RefCell<HashMap<ObjectId, PdfObject>>,
    /// Cache of decoded object streams, keyed by the ObjStm object number.
    /// Avoids re-decoding the whole stream for every compressed object it holds.
    objstm_cache: RefCell<HashMap<u32, Arc<DecodedObjStm>>>,
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

        Ok(Self {
            data,
            header,
            xref,
            trailer,
            limits,
            object_cache: RefCell::new(HashMap::new()),
            objstm_cache: RefCell::new(HashMap::new()),
        })
    }

    pub fn resolve(&self, id: zpdf_core::ObjectId) -> Result<PdfObject> {
        // Fast path: already resolved. The borrow ends with this block.
        if let Some(obj) = self.object_cache.borrow().get(&id) {
            return Ok(obj.clone());
        }

        let entry = self
            .xref
            .get(id)
            .ok_or(zpdf_core::Error::ObjectNotFound(id))?;
        let obj = match entry {
            XrefEntry::InUse { offset, .. } => {
                let parser = ObjectParser::new(&self.data, &self.limits);
                parser.parse_indirect_at(*offset as usize)?
            }
            XrefEntry::Compressed {
                stream_obj,
                index_in_stream,
            } => self.extract_from_object_stream(*stream_obj, *index_in_stream)?,
            XrefEntry::Free { .. } => return Err(zpdf_core::Error::ObjectNotFound(id)),
        };

        self.object_cache.borrow_mut().insert(id, obj.clone());
        Ok(obj)
    }

    /// Resolve a stream object and decode its data through the filter pipeline.
    pub fn resolve_stream_data(&self, id: zpdf_core::ObjectId) -> Result<Vec<u8>> {
        let obj = self.resolve(id)?;
        let stream = obj.as_stream()?;
        filters::decode_stream(&stream.data, &stream.dict)
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

        let decoded = filters::decode_stream(&stream.data, &stream.dict)?;

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
}
