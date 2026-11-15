use std::fmt;
use std::ops::{Deref, Range};
use std::sync::Arc;

use crate::{Error, Result};

/// Immutable stream bytes, either an independent buffer or a view of shared
/// storage. Cloning shares the view without copying its payload.
///
/// A view keeps its entire backing allocation alive. Use [`Self::into_detached`]
/// when retaining a small stream after closing its source document. Byte access,
/// comparison and Debug expose only the visible range.
///
/// ```
/// use std::sync::Arc;
/// use zpdf_core::PdfStreamData;
///
/// let file: Arc<[u8]> = Arc::from(b"prefixPAYLOADsuffix".as_slice());
/// let data = PdfStreamData::from_shared(file, 6..13)?;
/// assert_eq!(data.as_slice(), b"PAYLOAD");
/// let retained = data.to_detached(); // retains only the payload allocation
/// let legacy: Arc<[u8]> = retained.into_arc();
/// assert_eq!(legacy.as_ref(), b"PAYLOAD");
/// # Ok::<(), zpdf_core::Error>(())
/// ```
#[derive(Clone)]
pub struct PdfStreamData(Repr);

// Owned payloads keep the original Arc directly, without a descriptor allocation.
// Only a shared range needs metadata. The enum fits in the former Arc field on
// our supported targets, avoiding growth of every PdfObject array/dictionary slot.
#[derive(Clone)]
enum Repr {
    Owned(Arc<[u8]>),
    Shared(Arc<Storage>),
}

struct Storage {
    bytes: Arc<[u8]>,
    range: Range<usize>,
}

impl PdfStreamData {
    /// View a checked range without copying its bytes. Empty views never retain
    /// the supplied allocation. Invalid/reversed ranges return an error.
    pub fn from_shared(bytes: Arc<[u8]>, range: Range<usize>) -> Result<Self> {
        if bytes.get(range.clone()).is_none() {
            return Err(Error::InvalidObject(
                range.start as u64,
                "stream byte range is outside its backing storage".into(),
            ));
        }
        if range.is_empty() {
            return Ok(Self::default());
        }
        Ok(Self(Repr::Shared(Arc::new(Storage { bytes, range }))))
    }

    /// Borrow only this stream's visible bytes, without copying or decoding.
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        match &self.0 {
            Repr::Owned(bytes) => bytes,
            Repr::Shared(storage) => &storage.bytes[storage.range.clone()],
        }
    }

    /// The visible byte count. The checked range makes constructing a slice
    /// unnecessary when only its length is needed for budget accounting.
    #[inline]
    pub fn len(&self) -> usize {
        match &self.0 {
            Repr::Owned(bytes) => bytes.len(),
            Repr::Shared(storage) => storage.range.end - storage.range.start,
        }
    }

    /// Whether the visible byte range is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Retain only the visible byte allocation. Partial views copy their bytes;
    /// whole-allocation views reuse their backing because it has no unrelated
    /// bytes, including views made with [`Self::from_shared`]. This does not
    /// guarantee unique ownership or a different allocation identity.
    pub fn into_detached(self) -> Self {
        Self::from(self.into_arc())
    }

    /// An independent copy of this view, sharing storage when already detached.
    pub fn to_detached(&self) -> Self {
        self.clone().into_detached()
    }

    /// Convert to the former stream-data type. Whole allocations are reused;
    /// partial views copy exactly their visible bytes, never the whole file.
    pub fn into_arc(self) -> Arc<[u8]> {
        match self.0 {
            Repr::Owned(bytes) => bytes,
            Repr::Shared(storage) => {
                if storage.range.start == 0 && storage.range.end == storage.bytes.len() {
                    match Arc::try_unwrap(storage) {
                        Ok(storage) => storage.bytes,
                        Err(storage) => Arc::clone(&storage.bytes),
                    }
                } else {
                    Arc::from(&storage.bytes[storage.range.clone()])
                }
            }
        }
    }

    /// Conservative cache charge: visible payload plus view-allocation metadata.
    /// Shared payloads are deliberately charged per entry, preserving the
    /// existing conservative admission policy rather than treating views as free.
    /// This is a logical cache charge, not the complete backing-file size.
    #[inline]
    pub fn cache_bytes(&self) -> u64 {
        let descriptor = match &self.0 {
            Repr::Owned(_) => 0,
            Repr::Shared(_) => std::mem::size_of::<Storage>() as u64,
        };
        (self.len() as u64)
            .saturating_add(descriptor)
            .saturating_add(2 * std::mem::size_of::<usize>() as u64)
    }
}

impl Default for PdfStreamData {
    fn default() -> Self {
        Self::from(Arc::<[u8]>::from([]))
    }
}

impl From<Arc<[u8]>> for PdfStreamData {
    #[inline]
    fn from(bytes: Arc<[u8]>) -> Self {
        Self(Repr::Owned(bytes))
    }
}

impl From<Vec<u8>> for PdfStreamData {
    #[inline]
    fn from(bytes: Vec<u8>) -> Self {
        Self::from(Arc::<[u8]>::from(bytes))
    }
}

impl From<&[u8]> for PdfStreamData {
    #[inline]
    fn from(bytes: &[u8]) -> Self {
        Self::from(Arc::<[u8]>::from(bytes))
    }
}

impl Deref for PdfStreamData {
    type Target = [u8];
    #[inline]
    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl AsRef<[u8]> for PdfStreamData {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl PartialEq for PdfStreamData {
    fn eq(&self, other: &Self) -> bool {
        let same = match (&self.0, &other.0) {
            (Repr::Owned(a), Repr::Owned(b)) => Arc::ptr_eq(a, b),
            (Repr::Shared(a), Repr::Shared(b)) => Arc::ptr_eq(a, b),
            _ => false,
        };
        same || self.as_slice() == other.as_slice()
    }
}
impl Eq for PdfStreamData {}

impl fmt::Debug for PdfStreamData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_slice().fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_handle_stays_within_the_former_arc_field_size() {
        assert!(std::mem::size_of::<PdfStreamData>() <= std::mem::size_of::<Arc<[u8]>>());
    }

    #[test]
    fn views_share_bytes_but_only_expose_their_range() {
        let bytes: Arc<[u8]> = Arc::from(&b"secretABCprivate"[..]);
        let view = PdfStreamData::from_shared(Arc::clone(&bytes), 6..9).unwrap();
        assert_eq!(view.as_slice(), b"ABC");
        assert_eq!(view.as_ptr(), bytes[6..].as_ptr());
        assert_eq!(view.clone().as_ptr(), view.as_ptr());
        assert_eq!(format!("{view:?}"), "[65, 66, 67]");
        assert_eq!(view, PdfStreamData::from(b"ABC".as_slice()));
        assert_eq!(view.clone().into_arc().as_ref(), b"ABC");
    }

    #[test]
    fn detached_data_releases_the_source_and_preserves_bytes() {
        let bytes: Arc<[u8]> = Arc::from(&b"prefixABCtail"[..]);
        let weak = Arc::downgrade(&bytes);
        let view = PdfStreamData::from_shared(bytes, 6..9).unwrap();
        let clone = view.clone();
        let detached = view.into_detached();
        assert!(weak.upgrade().is_some());
        drop(clone);
        assert!(weak.upgrade().is_none());
        assert_eq!(detached.as_slice(), b"ABC");
    }

    #[test]
    fn whole_allocations_convert_without_copying() {
        let bytes: Arc<[u8]> = Arc::from(&b"ABC"[..]);
        let data = PdfStreamData::from(Arc::clone(&bytes));
        assert!(Arc::ptr_eq(&data.clone().into_arc(), &bytes));
        assert!(Arc::ptr_eq(&data.into_detached().into_arc(), &bytes));
        let whole = PdfStreamData::from_shared(Arc::clone(&bytes), 0..bytes.len()).unwrap();
        assert!(Arc::ptr_eq(&whole.to_detached().into_arc(), &bytes));
        assert!(Arc::ptr_eq(&whole.into_detached().into_arc(), &bytes));
    }

    #[test]
    fn empty_and_invalid_ranges_do_not_keep_the_source() {
        for range in [0..0, 3..3] {
            let bytes: Arc<[u8]> = Arc::from(&b"ABC"[..]);
            let weak = Arc::downgrade(&bytes);
            let view = PdfStreamData::from_shared(bytes, range).unwrap();
            assert!(view.is_empty());
            assert!(weak.upgrade().is_none());
        }
        for range in [Range { start: 3, end: 2 }, 0..4, usize::MAX..usize::MAX] {
            let bytes: Arc<[u8]> = Arc::from(&b"ABC"[..]);
            assert!(PdfStreamData::from_shared(bytes, range).is_err());
        }
    }
}
