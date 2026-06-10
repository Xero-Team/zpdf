mod catalog;
pub mod font_loader;
pub mod page;

pub use catalog::Catalog;
pub use page::{PdfPage, ResourceDict};

use std::sync::Arc;
use zpdf_core::{ParseLimits, Result};
use zpdf_font::FontCache;
use zpdf_parser::PdfFile;

pub struct PdfDocument {
    file: PdfFile,
    catalog: Catalog,
}

impl PdfDocument {
    pub fn open(data: impl Into<Arc<[u8]>>) -> Result<Self> {
        Self::open_with_limits(data, ParseLimits::default())
    }

    pub fn open_with_limits(data: impl Into<Arc<[u8]>>, limits: ParseLimits) -> Result<Self> {
        let file = PdfFile::parse_with_limits(data, limits)?;
        let catalog = Catalog::from_trailer(&file)?;
        Ok(Self { file, catalog })
    }

    pub fn page_count(&self) -> usize {
        self.catalog.page_count
    }

    pub fn page(&self, index: usize) -> Result<PdfPage> {
        self.catalog.get_page(&self.file, index)
    }

    pub fn file(&self) -> &PdfFile {
        &self.file
    }

    pub fn version(&self) -> (u8, u8) {
        (self.file.header.major, self.file.header.minor)
    }

    /// Get decoded content stream bytes for a page.
    pub fn page_content_bytes(&self, page: &PdfPage) -> Result<Vec<u8>> {
        let mut all_bytes = Vec::new();
        for &content_id in &page.contents {
            match self.file.resolve_stream_data(content_id) {
                Ok(bytes) => {
                    if !all_bytes.is_empty() {
                        all_bytes.push(b'\n');
                    }
                    all_bytes.extend_from_slice(&bytes);
                }
                Err(e) => {
                    tracing::warn!("failed to decode content stream {content_id}: {e}");
                }
            }
        }
        Ok(all_bytes)
    }

    /// Load all fonts referenced by a page.
    pub fn load_page_fonts(&self, page: &PdfPage) -> FontCache {
        font_loader::load_page_fonts(self.file(), page)
    }
}

#[cfg(test)]
pub(crate) mod test_util {
    /// Build a synthetic PDF from numbered object bodies (index `i` becomes
    /// object `i + 1`), with a correct xref table and a trailer whose /Root is
    /// object 1. Offsets are computed, so bodies can be edited freely.
    pub fn build_pdf(objects: &[&str]) -> Vec<u8> {
        let mut buf = Vec::from(&b"%PDF-1.7\n"[..]);
        let mut offsets = Vec::with_capacity(objects.len());
        for (i, body) in objects.iter().enumerate() {
            offsets.push(buf.len());
            buf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
        }
        let xref_off = buf.len();
        buf.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for off in &offsets {
            buf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        buf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_off}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        buf
    }
}
