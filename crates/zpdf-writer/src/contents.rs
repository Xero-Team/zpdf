//! Resolve page content against the pending update, not only the original file.
use crate::{invalid_data, IncrementalWriter};
use zpdf_core::{ObjectId, PdfDict, PdfObject, Result};

impl IncrementalWriter {
    /// /Contents may be a stream or an indirect array of stream references.
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
            let bytes = if self.pending.contains_key(&id.0) {
                let object = self.resolve_current(id)?;
                let stream = object.as_stream()?;
                let mut dict = stream.dict.clone();
                for key in ["Filter", "DecodeParms"] {
                    if let Some(value) = dict.get(key).cloned() {
                        let mut resolved = match value {
                            PdfObject::Ref(reference) => self.resolve_current(reference)?,
                            value => value,
                        };
                        if let PdfObject::Array(values) = &mut resolved {
                            for value in values {
                                if let PdfObject::Ref(reference) = value {
                                    *value = self.resolve_current(*reference)?;
                                }
                            }
                        }
                        dict.insert(zpdf_core::PdfName::new(key), resolved);
                    }
                }
                zpdf_parser::filters::decode_stream_with_limits(&stream.data, &dict, limits)?
            } else {
                self.document().file().resolve_stream_data(id)?
            };
            let extra = bytes
                .len()
                .checked_add(1)
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
            content.extend_from_slice(&bytes);
            content.push(b'\n');
        }
        Ok(content)
    }
}
