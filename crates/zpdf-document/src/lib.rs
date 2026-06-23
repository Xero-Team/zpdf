pub mod annot_appearance;
pub mod annotation;
mod catalog;
pub mod embedded_files;
pub mod font_loader;
pub mod forms;
pub mod optional_content;
pub mod output_intents;
pub mod page;

pub use annotation::Annotation;
pub use catalog::Catalog;
pub use embedded_files::{EmbeddedFile, EmbeddedSource};
pub use forms::{AcroForm, FieldKind, FieldValue, FormField};
pub use optional_content::OcConfig;
pub use output_intents::OutputIntent;
pub use page::{PdfPage, ResourceDict};

use std::sync::{Arc, OnceLock};
use zpdf_core::{Error, ParseLimits, Result};
use zpdf_font::FontCache;
use zpdf_parser::PdfFile;

pub struct PdfDocument {
    file: PdfFile,
    catalog: Catalog,
    /// Lazily-parsed interactive form, shared across page-annotation calls so
    /// the field-tree walk runs at most once per document.
    acro_form: OnceLock<Option<AcroForm>>,
}

impl PdfDocument {
    pub fn open(data: impl Into<Arc<[u8]>>) -> Result<Self> {
        Self::open_with_limits(data, ParseLimits::default())
    }

    pub fn open_with_limits(data: impl Into<Arc<[u8]>>, limits: ParseLimits) -> Result<Self> {
        Self::open_with_password_and_limits(data, b"", limits)
    }

    /// Open an encrypted document with a user or owner password. Returns
    /// [`zpdf_core::Error::WrongPassword`] when the password authenticates as
    /// neither. (A non-encrypted document opens regardless of the password.)
    pub fn open_with_password(data: impl Into<Arc<[u8]>>, password: &[u8]) -> Result<Self> {
        Self::open_with_password_and_limits(data, password, ParseLimits::default())
    }

    pub fn open_with_password_and_limits(
        data: impl Into<Arc<[u8]>>,
        password: &[u8],
        limits: ParseLimits,
    ) -> Result<Self> {
        let file = PdfFile::parse_with_password_and_limits(data, password, limits)?;
        let catalog = Catalog::from_trailer(&file)?;
        Ok(Self {
            file,
            catalog,
            acro_form: OnceLock::new(),
        })
    }

    /// True when the document is encrypted (carries an `/Encrypt` dictionary).
    pub fn is_encrypted(&self) -> bool {
        self.file.is_encrypted()
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

    /// Parse a page's annotations into renderable form (/Rect, /F, the
    /// /AS-selected appearance stream, /OC membership). Widget annotations for
    /// interactive-form fields gain a generated appearance when the producer
    /// left none (or set /NeedAppearances).
    pub fn page_annotations(&self, page: &PdfPage) -> Vec<Annotation> {
        annotation::parse_annotations(&self.file, page, self.acro_form())
    }

    /// The document's interactive form (`/AcroForm`), if any. Parsed once and
    /// cached for the lifetime of the document.
    pub fn acro_form(&self) -> Option<&AcroForm> {
        self.acro_form
            .get_or_init(|| AcroForm::parse(&self.file))
            .as_ref()
    }

    /// The document's default optional-content configuration, if any.
    pub fn oc_config(&self) -> Option<OcConfig> {
        optional_content::parse_oc_config(&self.file)
    }

    /// The document-level output intents (catalog `/OutputIntents`). Empty when
    /// the document declares none. Page-level intents (PDF 2.0) are carried on
    /// the page and read via [`PdfDocument::page_output_intents`].
    pub fn output_intents(&self) -> Vec<OutputIntent> {
        output_intents::parse_output_intents(&self.file)
    }

    /// PDF 2.0 page-level `/OutputIntents`, which override the document-level
    /// intents for that page. Empty for pre-2.0 / most documents.
    pub fn page_output_intents<'a>(&self, page: &'a PdfPage) -> &'a [OutputIntent] {
        &page.output_intents
    }

    /// The document's embedded files — file streams registered in the catalog's
    /// `/Names /EmbeddedFiles` name tree (a viewer's "attachments"). Empty when
    /// the document carries none. Pull a file's bytes with
    /// [`PdfDocument::embedded_file_bytes`].
    pub fn embedded_files(&self) -> Vec<EmbeddedFile> {
        embedded_files::parse_embedded_files(&self.file)
    }

    /// Catalog-level associated files (`/Root /AF`, PDF 2.0). Each carries an
    /// `/AFRelationship`. Per PDF 2.0 these are also listed by
    /// [`PdfDocument::embedded_files`]; the two lists usually overlap.
    pub fn associated_files(&self) -> Vec<EmbeddedFile> {
        embedded_files::parse_associated_files(&self.file)
    }

    /// Page-level associated files (`/Page /AF`, PDF 2.0) for one page. `/AF` is
    /// not inheritable, so only the leaf page dictionary is consulted.
    pub fn page_associated_files(&self, page: &PdfPage) -> Vec<EmbeddedFile> {
        match self
            .file
            .resolve(page.id)
            .ok()
            .and_then(|o| o.as_dict().ok().cloned())
        {
            Some(dict) => embedded_files::parse_page_associated_files(&self.file, &dict),
            None => Vec::new(),
        }
    }

    /// Decode and return the bytes of an embedded file. Routes through the
    /// parser's filter pipeline, so it respects `ParseLimits` (max stream size).
    /// Errors if the file specification carries no embedded stream
    /// ([`EmbeddedFile::is_embedded`] is `false`).
    pub fn embedded_file_bytes(&self, file: &EmbeddedFile) -> Result<Vec<u8>> {
        match file.stream {
            Some(id) => self.file.resolve_stream_data(id),
            // An external file specification has nothing to extract; report the
            // absent /EF as a missing key rather than a fake object-corruption
            // error, so a caller can distinguish it from a decode failure.
            None => Err(Error::MissingKey("EF".into())),
        }
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
