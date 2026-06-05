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
