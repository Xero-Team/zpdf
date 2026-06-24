//! The document information dictionary (ISO 32000-1 §14.3.3): the trailer's
//! `/Info` entry, carrying human-authored metadata — title, author, subject,
//! keywords, the producing software, and creation/modification timestamps.
//!
//! This is the classic `pdfinfo`-style surface. (PDF 2.0 deprecates `/Info` in
//! favour of the catalog's XMP `/Metadata` stream for most keys, but `/Info` is
//! still ubiquitous; an XMP reader can be layered on later.) Dates are reported
//! as their raw PDF date strings (`D:YYYYMMDDHHmmSSOHH'mm'`); no date parsing is
//! attempted here.

use zpdf_parser::PdfFile;

use crate::obj_util::{name_value, resolve_dict, text};

/// Metadata from the document information dictionary. Every field is optional —
/// producers populate an arbitrary subset.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocInfo {
    /// `/Title` — the document's title.
    pub title: Option<String>,
    /// `/Author` — the name of the person who created the document.
    pub author: Option<String>,
    /// `/Subject` — the subject of the document.
    pub subject: Option<String>,
    /// `/Keywords` — keywords associated with the document.
    pub keywords: Option<String>,
    /// `/Creator` — the application that created the original document.
    pub creator: Option<String>,
    /// `/Producer` — the application that produced the PDF (often a converter).
    pub producer: Option<String>,
    /// `/CreationDate` — the raw PDF date string the document was created.
    pub creation_date: Option<String>,
    /// `/ModDate` — the raw PDF date string of the most recent modification.
    pub mod_date: Option<String>,
    /// `/Trapped` — `True` / `False` / `Unknown` (whether the document has been
    /// trapped for printing). Carried as the raw name.
    pub trapped: Option<String>,
}

impl DocInfo {
    /// Whether any metadata field is present.
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.author.is_none()
            && self.subject.is_none()
            && self.keywords.is_none()
            && self.creator.is_none()
            && self.producer.is_none()
            && self.creation_date.is_none()
            && self.mod_date.is_none()
            && self.trapped.is_none()
    }
}

/// Parse the trailer's `/Info` dictionary. Returns `None` when the document
/// carries no `/Info`, or it resolves to nothing usable (no populated fields).
pub fn parse_info(file: &PdfFile) -> Option<DocInfo> {
    // /Info SHALL be an indirect reference per spec, but accept a direct dict in
    // the trailer too (lax producers — mirroring the direct-/Encrypt tolerance
    // already in this codebase). resolve_dict handles both shapes.
    let dict = resolve_dict(file, file.trailer.get("Info"))?;

    let info = DocInfo {
        title: text(file, &dict, "Title"),
        author: text(file, &dict, "Author"),
        subject: text(file, &dict, "Subject"),
        keywords: text(file, &dict, "Keywords"),
        creator: text(file, &dict, "Creator"),
        producer: text(file, &dict, "Producer"),
        creation_date: text(file, &dict, "CreationDate"),
        mod_date: text(file, &dict, "ModDate"),
        // /Trapped is a name (/True /False /Unknown); some producers write it as
        // a string, so fall back to a text read.
        trapped: name_value(file, &dict, "Trapped").or_else(|| text(file, &dict, "Trapped")),
    };

    if info.is_empty() {
        None
    } else {
        Some(info)
    }
}

#[cfg(test)]
mod tests {
    use crate::test_util::build_pdf;
    use crate::PdfDocument;

    const PAGES: &str = "<< /Type /Pages /Kids [3 0 R] /Count 1 >>";
    const PAGE: &str = "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>";

    /// Build a PDF whose trailer references an `/Info` object. The standard
    /// `build_pdf` helper writes a fixed trailer, so this variant adds `/Info`.
    fn build_with_info(objects: &[&str], info_obj: u32) -> Vec<u8> {
        let mut buf = Vec::from(&b"%PDF-1.7\n"[..]);
        let mut offsets = Vec::new();
        for (i, body) in objects.iter().enumerate() {
            offsets.push(buf.len());
            buf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
        }
        let xref = buf.len();
        buf.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for off in &offsets {
            buf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        buf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R /Info {info_obj} 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        buf
    }

    #[test]
    fn no_info_dict_is_none() {
        let doc = PdfDocument::open(build_pdf(&[
            "<< /Type /Catalog /Pages 2 0 R >>",
            PAGES,
            PAGE,
        ]))
        .expect("open");
        assert!(doc.info().is_none());
    }

    #[test]
    fn all_fields_parsed() {
        let doc = PdfDocument::open(build_with_info(
            &[
                "<< /Type /Catalog /Pages 2 0 R >>",
                PAGES,
                PAGE,
                "<< /Title (Annual Report) /Author (Jane Doe) /Subject (Finance) \
                 /Keywords (q4, revenue) /Creator (LibreOffice) /Producer (zpdf) \
                 /CreationDate (D:20240101120000Z) /ModDate (D:20240115093000Z) \
                 /Trapped /False >>",
            ],
            4,
        ))
        .expect("open");
        let info = doc.info().expect("info");
        assert_eq!(info.title.as_deref(), Some("Annual Report"));
        assert_eq!(info.author.as_deref(), Some("Jane Doe"));
        assert_eq!(info.subject.as_deref(), Some("Finance"));
        assert_eq!(info.keywords.as_deref(), Some("q4, revenue"));
        assert_eq!(info.creator.as_deref(), Some("LibreOffice"));
        assert_eq!(info.producer.as_deref(), Some("zpdf"));
        assert_eq!(info.creation_date.as_deref(), Some("D:20240101120000Z"));
        assert_eq!(info.mod_date.as_deref(), Some("D:20240115093000Z"));
        assert_eq!(info.trapped.as_deref(), Some("False"));
    }

    #[test]
    fn partial_fields_and_utf16_title() {
        // /Title <FEFF0048 0069> = "Hi"; only a couple of fields present.
        let doc = PdfDocument::open(build_with_info(
            &[
                "<< /Type /Catalog /Pages 2 0 R >>",
                PAGES,
                PAGE,
                "<< /Title <FEFF00480069> /Producer (zpdf) >>",
            ],
            4,
        ))
        .expect("open");
        let info = doc.info().expect("info");
        assert_eq!(info.title.as_deref(), Some("Hi"));
        assert_eq!(info.producer.as_deref(), Some("zpdf"));
        assert!(info.author.is_none());
    }

    #[test]
    fn direct_info_dict_in_trailer_is_read() {
        // Some lax producers inline /Info as a direct dictionary in the trailer
        // rather than as an indirect reference; we should still read it (parity
        // with the direct-/Encrypt tolerance elsewhere in the codebase).
        let objects = ["<< /Type /Catalog /Pages 2 0 R >>", PAGES, PAGE];
        let mut buf = Vec::from(&b"%PDF-1.7\n"[..]);
        let mut offsets = Vec::new();
        for (i, body) in objects.iter().enumerate() {
            offsets.push(buf.len());
            buf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
        }
        let xref = buf.len();
        buf.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for off in &offsets {
            buf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        buf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R /Info << /Title (Direct) /Producer (zpdf) >> >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        let doc = PdfDocument::open(buf).expect("open");
        let info = doc.info().expect("a direct /Info dict should be read");
        assert_eq!(info.title.as_deref(), Some("Direct"));
        assert_eq!(info.producer.as_deref(), Some("zpdf"));
    }

    #[test]
    fn trapped_as_string_uses_text_fallback() {
        // /Trapped is a name (/True /False /Unknown), but some producers write
        // it as a string; the name_value-then-text fallback must catch it.
        let doc = PdfDocument::open(build_with_info(
            &[
                "<< /Type /Catalog /Pages 2 0 R >>",
                PAGES,
                PAGE,
                "<< /Trapped (Unknown) /Producer (zpdf) >>",
            ],
            4,
        ))
        .expect("open");
        let info = doc.info().expect("info");
        assert_eq!(info.trapped.as_deref(), Some("Unknown"));
    }

    #[test]
    fn empty_info_dict_is_none() {
        let doc = PdfDocument::open(build_with_info(
            &["<< /Type /Catalog /Pages 2 0 R >>", PAGES, PAGE, "<< >>"],
            4,
        ))
        .expect("open");
        assert!(
            doc.info().is_none(),
            "an /Info with no fields reads as None"
        );
    }
}
