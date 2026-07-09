//! Document information (`/Info`) editing.
//!
//! [`IncrementalWriter::set_info`] rewrites the info dictionary in an
//! incremental update: an existing indirect `/Info` is overwritten in place
//! (the trailer keeps pointing at it); a direct or absent `/Info` is replaced
//! by a fresh object the new trailer points at.

use std::time::{SystemTime, UNIX_EPOCH};

use zpdf_core::{ObjectId, PdfDict, PdfName, PdfObject, PdfString, Result};

use crate::IncrementalWriter;

/// A partial update of the `/Info` dictionary.
///
/// Each field is three-state: `None` keeps the existing entry, `Some(None)`
/// deletes it, `Some(Some(v))` sets it. `/ModDate` is always refreshed.
#[derive(Debug, Default, Clone)]
pub struct InfoUpdate {
    pub title: Option<Option<String>>,
    pub author: Option<Option<String>>,
    pub subject: Option<Option<String>>,
    pub keywords: Option<Option<String>>,
    pub creator: Option<Option<String>>,
    pub producer: Option<Option<String>>,
}

impl InfoUpdate {
    /// True when the update carries no changes at all.
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.author.is_none()
            && self.subject.is_none()
            && self.keywords.is_none()
            && self.creator.is_none()
            && self.producer.is_none()
    }
}

impl IncrementalWriter {
    /// Apply an [`InfoUpdate`] to the document information dictionary.
    /// `/ModDate` is set to the current UTC time.
    pub fn set_info(&mut self, update: &InfoUpdate) -> Result<()> {
        // Resolve the current /Info: indirect ref (common), direct dict, or absent.
        let info_entry = self.doc.file().trailer.get("Info").cloned();
        let (existing_id, mut dict) = match &info_entry {
            Some(PdfObject::Ref(r)) => {
                let dict = self
                    .resolve_current(*r)
                    .ok()
                    .and_then(|o| o.as_dict().ok().cloned())
                    .unwrap_or_default();
                (Some(*r), dict)
            }
            Some(PdfObject::Dict(d)) => (None, d.clone()),
            _ => (None, PdfDict::new()),
        };

        for (key, field) in [
            ("Title", &update.title),
            ("Author", &update.author),
            ("Subject", &update.subject),
            ("Keywords", &update.keywords),
            ("Creator", &update.creator),
            ("Producer", &update.producer),
        ] {
            match field {
                None => {}
                Some(None) => {
                    dict.0.remove(&PdfName::new(key));
                }
                Some(Some(v)) => {
                    dict.insert(PdfName::new(key), PdfObject::String(encode_text_string(v)));
                }
            }
        }
        dict.insert(
            PdfName::new("ModDate"),
            PdfObject::String(PdfString::new(pdf_date_now().into_bytes())),
        );

        match existing_id {
            Some(id) => self.overwrite_object(id, PdfObject::Dict(dict)),
            None => {
                let (num, gen) = self.add_object(&PdfObject::Dict(dict));
                self.set_info_ref(ObjectId(num, gen as u16));
            }
        }
        Ok(())
    }
}

/// Encode a PDF text string (ISO 32000-1 §7.9.2.2): plain bytes when every
/// char fits PDFDocEncoding's Latin-1 range, else UTF-16BE with a `FE FF` BOM.
pub(crate) fn encode_text_string(s: &str) -> PdfString {
    if s.chars().all(|c| (c as u32) < 0x100) {
        PdfString::new(s.chars().map(|c| c as u8).collect())
    } else {
        let mut bytes = vec![0xFE, 0xFF];
        for unit in s.encode_utf16() {
            bytes.extend_from_slice(&unit.to_be_bytes());
        }
        PdfString::new(bytes)
    }
}

/// The current UTC time as a PDF date string (`D:YYYYMMDDHHMMSSZ`).
fn pdf_date_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (year, month, day) = civil_from_days(days);
    format!("D:{year:04}{month:02}{day:02}{h:02}{m:02}{s:02}Z")
}

/// Days-since-epoch → (year, month, day), Howard Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1)); // 2024-01-01
        assert_eq!(civil_from_days(11_016), (2000, 2, 29)); // leap day
    }

    #[test]
    fn date_format_shape() {
        let d = pdf_date_now();
        assert!(d.starts_with("D:"));
        assert!(d.ends_with('Z'));
        assert_eq!(d.len(), 2 + 14 + 1);
    }

    #[test]
    fn text_string_encoding() {
        assert_eq!(encode_text_string("Hello").as_bytes(), b"Hello");
        assert_eq!(encode_text_string("caf\u{e9}").as_bytes(), b"caf\xe9");
        let utf16 = encode_text_string("\u{4e2d}\u{6587}");
        assert_eq!(&utf16.as_bytes()[..2], &[0xFE, 0xFF]);
        assert_eq!(&utf16.as_bytes()[2..], &[0x4E, 0x2D, 0x65, 0x87]);
    }
}
