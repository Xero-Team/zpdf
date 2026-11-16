use std::ops::Deref;
use zpdf_core::PdfStreamData;

/// Decoded stream bytes. Unchanged streams share their immutable payload;
/// decoded output owns its Vec without an additional Arc allocation or copy.
///
/// Shared file views may retain the original PDF. `into_vec` detaches them;
/// dropping this value releases its view. Cache budgets are unchanged.
#[derive(Debug)]
pub enum DecodedStreamData {
    Shared(PdfStreamData),
    Owned(Vec<u8>),
}

impl DecodedStreamData {
    /// Move decoded output, or copy only the visible bytes of a shared view.
    pub fn into_vec(self) -> Vec<u8> {
        match self {
            Self::Shared(data) => data.as_slice().to_vec(),
            Self::Owned(data) => data,
        }
    }
}

impl AsRef<[u8]> for DecodedStreamData {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Shared(data) => data.as_slice(),
            Self::Owned(data) => data,
        }
    }
}

impl Deref for DecodedStreamData {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        self.as_ref()
    }
}
