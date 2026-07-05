pub mod annot_appearance;
pub mod annotation;
mod catalog;
pub mod destinations;
pub mod doc_info;
pub mod embedded_files;
pub mod font_loader;
pub mod forms;
pub mod measure;
mod obj_util;
pub mod optional_content;
pub mod outline;
pub mod output_intents;
pub mod page;
pub mod page_labels;
pub mod signature;
pub mod structure;
pub mod xmp;

pub use annotation::Annotation;
pub use catalog::Catalog;
pub use destinations::{DestView, Destination};
pub use doc_info::DocInfo;
pub use embedded_files::{EmbeddedFile, EmbeddedSource};
pub use forms::{AcroForm, FieldKind, FieldValue, FormField};
pub use measure::{GeographicCoordinateSystem, Measure};
pub use optional_content::OcConfig;
pub use outline::OutlineItem;
pub use output_intents::OutputIntent;
pub use page::{PdfPage, ResourceDict};
pub use page_labels::{PageLabelStyle, PageLabels};
pub use signature::{ByteRangeCoverage, DigestStatus, Signature};
pub use structure::{StructElem, StructKid, StructRole, StructTree};
pub use xmp::XmpMetadata;

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use zpdf_core::{Error, ParseLimits, PdfObject, Result};
use zpdf_font::FontCache;
use zpdf_parser::PdfFile;

pub struct PdfDocument {
    file: PdfFile,
    catalog: Catalog,
    /// Lazily-parsed interactive form, shared across page-annotation calls so
    /// the field-tree walk runs at most once per document.
    acro_form: OnceLock<Option<AcroForm>>,
    /// Lazily-flattened named-destination map, shared across page-annotation
    /// calls so resolving link targets never re-walks the name tree per page —
    /// a full-document link scan stays O(pages × links + tree), not O(pages ×
    /// tree).
    named_dests: OnceLock<HashMap<Vec<u8>, PdfObject>>,
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
            named_dests: OnceLock::new(),
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
        annotation::parse_annotations(
            &self.file,
            page,
            &self.catalog,
            self.named_dests(),
            self.acro_form(),
        )
    }

    /// The document's named-destination map, flattened once and cached for the
    /// document's lifetime. Backs link-target resolution so the name tree is
    /// walked at most once, never per page.
    fn named_dests(&self) -> &HashMap<Vec<u8>, PdfObject> {
        self.named_dests
            .get_or_init(|| destinations::collect_named_dests(&self.file))
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

    /// The document outline (bookmarks) from the catalog's `/Outlines`, as a
    /// nested tree of [`OutlineItem`]. Each item's `/Dest` or go-to `/A` is
    /// resolved to a [`Destination`]; URI / remote-go-to targets are captured as
    /// strings. Empty when the document has no outline.
    pub fn outline(&self) -> Vec<OutlineItem> {
        outline::parse_outlines(&self.file, &self.catalog)
    }

    /// Resolve a *named* destination (from a named-destination string/name) to a
    /// [`Destination`]. Tries the `/Names /Dests` name tree and the legacy
    /// `/Root /Dests` dictionary. `None` when the name is unknown.
    pub fn named_destination(&self, name: &[u8]) -> Option<Destination> {
        destinations::resolve_named(&self.file, &self.catalog, name)
    }

    /// Resolve any destination *value* — an explicit `[page /Fit …]` array, a
    /// named-destination name/string, a `<< /D … >>` dictionary, or an indirect
    /// reference to one — to a [`Destination`]. This is what a `/Dest` entry or
    /// a go-to action's `/D` carries; useful for resolving link-annotation
    /// targets. `None` when it does not name a destination.
    pub fn resolve_destination(&self, dest: &PdfObject) -> Option<Destination> {
        destinations::resolve_explicit(&self.file, &self.catalog, dest)
    }

    /// The document information dictionary (`/Info`): title, author, subject,
    /// keywords, creator/producer, and creation/modification dates (raw PDF date
    /// strings). `None` when the document carries no `/Info` or it is empty.
    pub fn info(&self) -> Option<DocInfo> {
        doc_info::parse_info(&self.file)
    }

    /// The document's page labels (`/PageLabels`, ISO 32000-1 §12.4.2): the
    /// number tree mapping page indices to the printed labels a viewer shows and
    /// a user navigates by — e.g. lowercase-roman front matter (`i, ii, …`) then
    /// decimal body (`1, 2, …`), or a prefixed appendix (`A-1, A-2, …`). These
    /// are distinct from the physical 0-based page indices. `None` when the
    /// document declares no page labels. Query a page with [`PageLabels::label`].
    pub fn page_labels(&self) -> Option<PageLabels> {
        page_labels::parse_page_labels(&self.file)
    }

    /// The document's XMP metadata (`/Metadata`, ISO 32000-1 §14.3.2): the common
    /// Dublin Core / XMP / PDF-schema properties (title, authors, description,
    /// keywords, producer, creator tool, dates), read with a bounded scrape (no
    /// XML engine; entity-expansion-safe). `None` when the document carries no
    /// `/Metadata` or none of the recognized properties. PDF 2.0 prefers this
    /// over the `/Info` dictionary ([`PdfDocument::info`]).
    pub fn xmp_metadata(&self) -> Option<XmpMetadata> {
        xmp::parse_xmp(&self.file)
    }

    /// The raw bytes of the catalog's `/Metadata` XMP packet (decoded through the
    /// filter pipeline, respecting `ParseLimits`), for callers that want to parse
    /// the RDF/XML themselves. `None` when the document carries no `/Metadata`.
    pub fn metadata_bytes(&self) -> Option<Vec<u8>> {
        xmp::metadata_bytes(&self.file)
    }

    /// The document's logical structure tree (`/StructTreeRoot`, ISO 32000-1
    /// §14.7–14.8): the Tagged-PDF tree of structure elements (headings,
    /// paragraphs, lists, tables, figures …) with their roles, accessibility
    /// text, and marked-content / object associations. `None` when the document
    /// declares no structure tree. Read-only; runs only when called.
    pub fn struct_tree(&self) -> Option<StructTree> {
        structure::parse_struct_tree(&self.file, &self.catalog)
    }

    /// Whether the document declares Tagged-PDF conformance via the catalog's
    /// `/MarkInfo` dictionary (`/Marked true`). Independent of whether a
    /// [`PdfDocument::struct_tree`] is actually present.
    pub fn is_tagged(&self) -> bool {
        structure::is_tagged(&self.file)
    }

    /// The document's digital signatures (`/Sig` form fields, ISO 32000-1
    /// §12.8). Each [`Signature`] carries the signature dictionary's metadata,
    /// its `/ByteRange` coverage, and a byte-range **integrity** verdict
    /// ([`DigestStatus`]) obtained by recomputing the covered-bytes digest and
    /// comparing it to the digest embedded in the CMS blob. This does *not*
    /// verify the signer's public-key signature or certificate trust — see the
    /// [`signature`] module docs. Empty when the document carries no signatures.
    /// Read-only; runs only when called.
    pub fn signatures(&self) -> Vec<Signature> {
        signature::parse_signatures(&self.file)
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
