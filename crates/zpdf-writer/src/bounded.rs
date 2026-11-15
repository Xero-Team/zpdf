//! Fallible, byte-bounded staging for derived content and private snapshots.

use std::io::{self, Write};

pub(crate) struct BoundedBytes {
    bytes: Vec<u8>,
    limit: u64,
}

impl BoundedBytes {
    pub(crate) fn new(limit: u64) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.bytes.len()
    }
    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.bytes
    }
    pub(crate) fn into_inner(self) -> Vec<u8> {
        self.bytes
    }
}

impl Write for BoundedBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let length = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .filter(|length| *length as u64 <= self.limit)
            .ok_or_else(|| io::Error::other("derived PDF content exceeds byte limit"))?;
        self.bytes
            .try_reserve(length - self.bytes.len())
            .map_err(|_| io::Error::other("derived PDF content allocation failed"))?;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Charge serialized work without retaining another copy of its bytes.
pub(crate) struct ByteBudget(pub(crate) u64);

impl Write for ByteBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len() as u64)
            .ok_or_else(|| io::Error::other("PDF property processing exceeds byte limit"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
