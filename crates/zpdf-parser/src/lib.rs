pub mod filters;
mod header;
mod lexer;
mod object_parser;
mod xref;

pub use header::PdfHeader;
pub use lexer::Lexer;
pub use object_parser::ObjectParser;
pub use xref::{XrefEntry, XrefTable};

use std::sync::Arc;
use zpdf_core::{ParseLimits, PdfObject, PdfStream, Result};

pub struct PdfFile {
    data: Arc<[u8]>,
    pub header: PdfHeader,
    pub xref: XrefTable,
    pub trailer: zpdf_core::PdfDict,
    limits: ParseLimits,
}

impl PdfFile {
    pub fn parse(data: impl Into<Arc<[u8]>>) -> Result<Self> {
        Self::parse_with_limits(data, ParseLimits::default())
    }

    pub fn parse_with_limits(data: impl Into<Arc<[u8]>>, limits: ParseLimits) -> Result<Self> {
        let data: Arc<[u8]> = data.into();
        let header = header::parse_header(&data)?;
        let (xref, trailer) = xref::parse_xref_and_trailer(&data, &limits)?;
        Ok(Self {
            data,
            header,
            xref,
            trailer,
            limits,
        })
    }

    pub fn resolve(&self, id: zpdf_core::ObjectId) -> Result<PdfObject> {
        let entry = self
            .xref
            .get(id)
            .ok_or(zpdf_core::Error::ObjectNotFound(id))?;
        match entry {
            XrefEntry::InUse { offset, .. } => {
                let parser = ObjectParser::new(&self.data, &self.limits);
                parser.parse_indirect_at(*offset as usize)
            }
            XrefEntry::Compressed {
                stream_obj,
                index_in_stream,
            } => self.extract_from_object_stream(*stream_obj, *index_in_stream),
            XrefEntry::Free { .. } => Err(zpdf_core::Error::ObjectNotFound(id)),
        }
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
        // Resolve the object stream itself (must be a direct/InUse object)
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

        let stream = stream_obj.as_stream()?;
        let n = stream.dict.get_i64("N")? as usize;
        let first = stream.dict.get_i64("First")? as usize;

        // Decode the stream data
        let decoded = filters::decode_stream(&stream.data, &stream.dict)?;

        // Parse the header: N pairs of (obj_num, offset_within_data)
        let header = &decoded[..first.min(decoded.len())];
        let mut header_lexer = Lexer::new(header, 0);

        let mut entries = Vec::with_capacity(n);
        for _ in 0..n {
            let obj_num_tok = header_lexer.next_token()?;
            let offset_tok = header_lexer.next_token()?;
            let obj_num = obj_num_tok.as_i64()? as u32;
            let offset = offset_tok.as_i64()? as usize;
            entries.push((obj_num, offset));
        }

        // Find the requested object
        let idx = index_in_stream as usize;
        if idx >= entries.len() {
            return Err(zpdf_core::Error::InvalidObject(
                0,
                format!("object stream index {idx} out of range (n={n})"),
            ));
        }

        let (_, obj_offset) = entries[idx];
        let data_start = first + obj_offset;

        // Determine end of this object's data
        let data_end = if idx + 1 < entries.len() {
            first + entries[idx + 1].1
        } else {
            decoded.len()
        };

        if data_start >= decoded.len() {
            return Err(zpdf_core::Error::UnexpectedEof(data_start as u64));
        }

        let obj_data = &decoded[data_start..data_end.min(decoded.len())];
        let mut lexer = Lexer::new(obj_data, 0);
        lexer.next_token()
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }
}
