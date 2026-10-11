//! Bounded, lazy decoding of application-supplied font programs.
//! No I/O is performed here; acquisition and persistent caching belong to the host.

use flate2::{Decompress, FlushDecompress, Status};
use std::sync::{Arc, OnceLock};
use zpdf_core::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontPackFormat {
    Raw,
    Zlib,
}

#[derive(Debug, Clone, Copy)]
pub struct FontPackLimits {
    /// Retained source bytes; raw TTF/OTF data is not compressed.
    pub max_encoded_bytes: u64,
    /// Decoded program bytes. Decoder scratch space is bounded separately.
    pub max_decoded_bytes: u64,
}

impl Default for FontPackLimits {
    fn default() -> Self {
        Self {
            max_encoded_bytes: 64 * 1024 * 1024,
            max_decoded_bytes: 64 * 1024 * 1024,
        }
    }
}

pub struct FontPack {
    encoded: Arc<[u8]>,
    format: FontPackFormat,
    limits: FontPackLimits,
    coverage: Vec<(u32, u32)>,
    decoded: OnceLock<std::result::Result<Arc<[u8]>, String>>,
}

impl FontPack {
    pub fn new(
        encoded: impl Into<Arc<[u8]>>,
        format: FontPackFormat,
        limits: FontPackLimits,
    ) -> Result<Self> {
        let encoded = encoded.into();
        if encoded.len() as u64 > limits.max_encoded_bytes {
            return Err(Error::StreamSizeLimit(limits.max_encoded_bytes));
        }
        Ok(Self {
            encoded,
            format,
            limits,
            coverage: Vec::new(),
            decoded: OnceLock::new(),
        })
    }

    /// Coverage hints skip irrelevant packs; the decoded cmap remains authoritative.
    pub fn with_coverage(mut self, mut ranges: Vec<(u32, u32)>) -> Result<Self> {
        if ranges.len() > 256
            || ranges
                .iter()
                .any(|&(low, high)| low > high || high > 0x10FFFF)
        {
            return Err(Error::StreamDecode(
                "invalid font pack coverage ranges".into(),
            ));
        }
        ranges.sort_unstable();
        let mut merged: Vec<(u32, u32)> = Vec::new();
        for (low, high) in ranges {
            if let Some(last) = merged
                .last_mut()
                .filter(|last| low <= last.1.saturating_add(1))
            {
                last.1 = last.1.max(high);
            } else {
                merged.push((low, high));
            }
        }
        self.coverage = merged;
        Ok(self)
    }

    pub fn may_cover(&self, character: char) -> bool {
        self.coverage.is_empty()
            || self
                .coverage
                .iter()
                .any(|&(low, high)| (low..=high).contains(&(character as u32)))
    }

    pub fn encoded_len(&self) -> u64 {
        self.encoded.len() as u64
    }

    pub(crate) fn decoded_bound(&self) -> u64 {
        if self.format == FontPackFormat::Raw {
            self.encoded_len()
        } else {
            self.limits.max_decoded_bytes
        }
    }

    pub(crate) fn shares_decoded(&self, bytes: &Arc<[u8]>) -> bool {
        self.decoded
            .get()
            .and_then(|result| result.as_ref().ok())
            .is_some_and(|decoded| Arc::ptr_eq(decoded, bytes))
    }

    pub fn is_decoded(&self) -> bool {
        self.decoded.get().is_some_and(|result| result.is_ok())
    }

    /// Decode once, including failures, and share the resulting immutable bytes.
    pub fn decode(&self) -> Result<Arc<[u8]>> {
        self.decoded
            .get_or_init(|| self.decode_inner().map_err(|error| error.to_string()))
            .as_ref()
            .map(Arc::clone)
            .map_err(|message| Error::StreamDecode(message.clone()))
    }

    fn decode_inner(&self) -> Result<Arc<[u8]>> {
        let limit = self.limits.max_decoded_bytes;
        if self.format == FontPackFormat::Raw {
            if self.encoded.len() as u64 > limit {
                return Err(Error::StreamSizeLimit(limit));
            }
            return Ok(Arc::clone(&self.encoded));
        }
        let mut decoder = Decompress::new(true);
        let mut output = Vec::new();
        let mut buffer = [0u8; 32 * 1024];
        loop {
            let input_before = decoder.total_in();
            let output_before = decoder.total_out();
            let available = limit
                .saturating_sub(output_before)
                .saturating_add(1)
                .min(buffer.len() as u64) as usize;
            let offset = usize::try_from(input_before)
                .map_err(|_| Error::StreamDecode("font pack offset overflow".into()))?;
            let input = self
                .encoded
                .get(offset..)
                .ok_or_else(|| Error::StreamDecode("font pack input overflow".into()))?;
            let status = decoder
                .decompress(input, &mut buffer[..available], FlushDecompress::None)
                .map_err(|error| Error::StreamDecode(format!("invalid zlib font pack: {error}")))?;
            if decoder.total_out() > limit {
                return Err(Error::StreamSizeLimit(limit));
            }
            let produced = (decoder.total_out() - output_before) as usize;
            // Bound growth explicitly instead of letting Vec double past the
            // declared limit. The final Arc conversion may briefly coexist
            // with this allocation; the limit bounds retained program bytes.
            if output.len() + produced > output.capacity() {
                let required = output.len() + produced;
                let capacity = output
                    .capacity()
                    .saturating_mul(2)
                    .max(required)
                    .min(usize::try_from(limit).unwrap_or(usize::MAX));
                output
                    .try_reserve_exact(capacity - output.len())
                    .map_err(|error| {
                        Error::StreamDecode(format!("font pack allocation failed: {error}"))
                    })?;
            }
            output.extend_from_slice(&buffer[..produced]);
            if status == Status::StreamEnd {
                if decoder.total_in() != self.encoded.len() as u64 {
                    return Err(Error::StreamDecode(
                        "trailing data in zlib font pack".into(),
                    ));
                }
                return Ok(Arc::from(output));
            }
            if decoder.total_in() == input_before && decoder.total_out() == output_before {
                return Err(Error::StreamDecode("truncated zlib font pack".into()));
            }
        }
    }
}
