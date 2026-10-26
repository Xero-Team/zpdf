//! Resolve page content against the pending update, not only the original file.
use crate::{invalid_data, IncrementalWriter};
use zpdf_core::{ObjectId, PdfDict, PdfObject, Result};

impl IncrementalWriter {
    /// ISO 32000-1, 7.7.3.3, Table 30: /Contents is a stream or an array
    /// of streams; the array itself may be indirect (7.3.10).
    /// Reject malformed arrays before an editing operation mutates the page.
    pub(crate) fn current_content_refs(&self, page: &PdfDict) -> Result<Vec<ObjectId>> {
        let value = match page.get("Contents") {
            None | Some(PdfObject::Null) => return Ok(Vec::new()),
            Some(PdfObject::Ref(id)) => {
                let object = self.resolve_current(*id)?;
                if matches!(object, PdfObject::Stream(_)) {
                    return Ok(vec![*id]);
                }
                object
            }
            Some(other) => other.clone(),
        };
        let PdfObject::Array(entries) = value else {
            return Err(invalid_data("page Contents must be a stream or an array").into());
        };
        if entries.len() > 65_536 {
            return Err(invalid_data("too many page content streams").into());
        }
        entries
            .into_iter()
            .map(|entry| {
                let PdfObject::Ref(id) = entry else {
                    return Err(
                        invalid_data("page Contents array must contain stream references").into(),
                    );
                };
                if !matches!(self.resolve_current(id)?, PdfObject::Stream(_)) {
                    return Err(invalid_data("page Contents entry is not a stream").into());
                }
                Ok(id)
            })
            .collect()
    }

    pub(crate) fn current_content_bytes(&self, ids: &[ObjectId]) -> Result<Vec<u8>> {
        let limits = self.document().file().limits();
        let mut content = Vec::new();
        for &id in ids {
            let bytes = self.decode_current_content_stream(id, true)?;
            let separator = usize::from(!content.is_empty());
            let extra =
                bytes
                    .len()
                    .checked_add(separator)
                    .ok_or(zpdf_core::Error::StreamSizeLimit(
                        limits.max_decoded_stream_bytes,
                    ))?;
            let total =
                content
                    .len()
                    .checked_add(extra)
                    .ok_or(zpdf_core::Error::StreamSizeLimit(
                        limits.max_decoded_stream_bytes,
                    ))?;
            if total as u64 > limits.max_decoded_stream_bytes {
                return Err(zpdf_core::Error::StreamSizeLimit(
                    limits.max_decoded_stream_bytes,
                ));
            }
            content
                .try_reserve(extra)
                .map_err(|_| invalid_data("page content allocation failed"))?;
            if separator != 0 {
                content.push(b'\n');
            }
            content.extend_from_slice(&bytes);
        }
        Ok(content)
    }

    // resolve_current returns decrypted bytes for original streams and plain
    // bytes for pending ones. In both cases filter metadata must come from the
    // current writer state, including indirect /DP and /JBIG2Globals values.
    fn decode_current_content_stream(&self, id: ObjectId, inline_globals: bool) -> Result<Vec<u8>> {
        let limits = self.document().file().limits();
        let object = self.resolve_current(id)?;
        let stream = object.as_stream()?;
        if stream.data.len() as u64 > limits.max_stream_bytes {
            return Err(zpdf_core::Error::StreamSizeLimit(limits.max_stream_bytes));
        }
        let mut dict = stream.dict.clone();
        let mut remaining = 1024;
        let mut globals_budget = limits.max_decoded_stream_bytes;
        for key in ["Filter", "DecodeParms", "DP"] {
            if let Some(value) = dict.get(key) {
                let value = self.current_filter_value(
                    value,
                    inline_globals && key != "Filter",
                    0,
                    &mut remaining,
                    &mut globals_budget,
                )?;
                dict.insert(zpdf_core::PdfName::new(key), value);
            }
        }
        zpdf_parser::filters::decode_stream_with_limits(&stream.data, &dict, limits)
    }

    fn current_filter_value(
        &self,
        value: &PdfObject,
        inline_globals: bool,
        depth: u32,
        remaining: &mut usize,
        globals_budget: &mut u64,
    ) -> Result<PdfObject> {
        let max_depth = self.document().file().limits().max_object_depth.min(32);
        if depth > max_depth || *remaining == 0 {
            return Err(zpdf_core::Error::RecursionLimit(max_depth));
        }
        *remaining -= 1;
        match value {
            PdfObject::Ref(id) => self.current_filter_value(
                &self.resolve_current(*id)?,
                inline_globals,
                depth + 1,
                remaining,
                globals_budget,
            ),
            PdfObject::Array(values) => Ok(PdfObject::Array(
                values
                    .iter()
                    .map(|value| {
                        self.current_filter_value(
                            value,
                            inline_globals,
                            depth + 1,
                            remaining,
                            globals_budget,
                        )
                    })
                    .collect::<Result<_>>()?,
            )),
            PdfObject::Dict(dict) => {
                let mut dict = dict.clone();
                if let Some(PdfObject::Ref(id)) = dict.get("JBIG2Globals") {
                    if !inline_globals {
                        return Err(
                            invalid_data("nested JBIG2Globals decoding is unsupported").into()
                        );
                    }
                    // Never inline globals of globals: malformed reference
                    // cycles must fail without recursion or partial edits.
                    let bytes = self.decode_current_content_stream(*id, false)?;
                    *globals_budget = globals_budget.checked_sub(bytes.len() as u64).ok_or(
                        zpdf_core::Error::StreamSizeLimit(
                            self.document().file().limits().max_decoded_stream_bytes,
                        ),
                    )?;
                    dict.insert(
                        zpdf_core::PdfName::new("JBIG2Globals"),
                        PdfObject::String(zpdf_core::PdfString(bytes)),
                    );
                }
                Ok(PdfObject::Dict(dict))
            }
            other => Ok(other.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use zpdf_core::{ParseLimits, PdfName};

    fn writer(limits: ParseLimits) -> IncrementalWriter {
        let mut builder = crate::DocumentBuilder::new();
        builder.add_page(100.0, 100.0);
        let bytes = builder.build().unwrap();
        let mut writer = IncrementalWriter::new(bytes.clone()).unwrap();
        writer.doc = zpdf_document::PdfDocument::open_with_limits(bytes, limits).unwrap();
        writer
    }
    fn save(writer: &IncrementalWriter) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        writer.write(&mut out).unwrap();
        out.into_inner()
    }
    fn object(writer: &mut IncrementalWriter, object: PdfObject) -> ObjectId {
        let (n, g) = writer.try_add_object(&object).unwrap();
        ObjectId(n, g as u16)
    }
    fn stream(writer: &mut IncrementalWriter, dict: &PdfDict, bytes: &[u8]) -> ObjectId {
        let (n, g) = writer.try_add_stream(dict, bytes).unwrap();
        ObjectId(n, g as u16)
    }
    fn flate(bytes: &[u8]) -> Vec<u8> {
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }
    fn predictor(enabled: bool) -> PdfDict {
        let mut dict = PdfDict::new();
        dict.insert(
            PdfName::new("Predictor"),
            PdfObject::Integer(if enabled { 12 } else { 1 }),
        );
        dict.insert(PdfName::new("Columns"), PdfObject::Integer(3));
        dict
    }

    #[test]
    fn pending_streams_resolve_decode_parameter_aliases_and_arrays() {
        for key in ["DecodeParms", "DP"] {
            let mut writer = writer(Default::default());
            let filter = object(&mut writer, PdfObject::Name(PdfName::new("FlateDecode")));
            let filters = object(&mut writer, PdfObject::Array(vec![PdfObject::Ref(filter)]));
            let params = object(&mut writer, PdfObject::Dict(predictor(true)));
            let params = object(&mut writer, PdfObject::Array(vec![PdfObject::Ref(params)]));
            let mut dict = PdfDict::new();
            dict.insert(PdfName::new("Filter"), PdfObject::Ref(filters));
            dict.insert(PdfName::new(key), PdfObject::Ref(params));
            let id = stream(&mut writer, &dict, &flate(b"\x02q Q"));
            assert_eq!(
                writer.current_content_bytes(&[id]).unwrap(),
                b"q Q",
                "{key}"
            );
        }
    }

    #[test]
    fn original_streams_use_pending_filter_and_predictor_metadata() {
        let mut source = writer(Default::default());
        let params = object(&mut source, PdfObject::Dict(predictor(false)));
        let filter = object(&mut source, PdfObject::Name(PdfName::new("ASCIIHexDecode")));
        let mut dict = PdfDict::new();
        dict.insert(PdfName::new("Filter"), PdfObject::Ref(filter));
        dict.insert(PdfName::new("DP"), PdfObject::Ref(params));
        let id = stream(&mut source, &dict, &flate(b"\x02q Q"));
        let mut writer = IncrementalWriter::new(save(&source)).unwrap();
        writer.overwrite_object(filter, PdfObject::Name(PdfName::new("FlateDecode")));
        writer.overwrite_object(params, PdfObject::Dict(predictor(true)));
        assert_eq!(writer.current_content_bytes(&[id]).unwrap(), b"q Q");
    }

    #[test]
    fn content_limits_charge_encoded_bytes_and_only_between_stream_separators() {
        let mut encoded = writer(ParseLimits {
            max_stream_bytes: 2,
            ..Default::default()
        });
        let mut dict = PdfDict::new();
        dict.insert(
            PdfName::new("Filter"),
            PdfObject::Name(PdfName::new("ASCIIHexDecode")),
        );
        let id = stream(&mut encoded, &dict, b"71>");
        assert!(matches!(
            encoded.current_content_bytes(&[id]),
            Err(zpdf_core::Error::StreamSizeLimit(2))
        ));
        let mut decoded = writer(ParseLimits {
            max_decoded_stream_bytes: 3,
            ..Default::default()
        });
        let id = stream(&mut decoded, &PdfDict::new(), b"q Q");
        assert_eq!(decoded.current_content_bytes(&[id]).unwrap(), b"q Q");
        let a = stream(&mut decoded, &PdfDict::new(), b"q");
        let b = stream(&mut decoded, &PdfDict::new(), b"Q");
        assert_eq!(decoded.current_content_bytes(&[a, b]).unwrap(), b"q\nQ");
    }

    #[test]
    fn current_jbig2_globals_are_decoded_and_cycles_are_bounded() {
        // Same synthetic 8x2 MMR segments as the parser's globals regression.
        let globals = [
            &[0, 0, 0, 0, 0x30, 0, 1, 0, 0, 0, 19][..],
            &[0, 0, 0, 8, 0, 0, 0, 2][..],
            &[0; 8][..],
            &[0, 0, 0][..],
        ]
        .concat();
        let image = [
            &[0, 0, 0, 1, 0x26, 0, 1, 0, 0, 0, 20][..],
            &[0, 0, 0, 8, 0, 0, 0, 2][..],
            &[0, 0, 0, 0, 0, 0, 0, 0, 0][..],
            &[1, 0x31, 0xF8][..],
        ]
        .concat();
        let mut writer = writer(Default::default());
        let mut dict = PdfDict::new();
        dict.insert(
            PdfName::new("Filter"),
            PdfObject::Name(PdfName::new("FlateDecode")),
        );
        let globals_id = stream(&mut writer, &dict, &flate(&globals));
        let mut params = PdfDict::new();
        params.insert(PdfName::new("JBIG2Globals"), PdfObject::Ref(globals_id));
        let params = object(&mut writer, PdfObject::Dict(params));
        dict.insert(
            PdfName::new("Filter"),
            PdfObject::Name(PdfName::new("JBIG2Decode")),
        );
        dict.insert(PdfName::new("DP"), PdfObject::Ref(params));
        let id = stream(&mut writer, &dict, &image);
        assert_eq!(writer.current_content_bytes(&[id]).unwrap(), [0xE7, 0xE7]);
        writer.overwrite_object(params, PdfObject::Ref(params));
        assert!(matches!(
            writer.current_content_bytes(&[id]),
            Err(zpdf_core::Error::RecursionLimit(_))
        ));
    }
}
