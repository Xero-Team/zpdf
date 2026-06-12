use zpdf_core::{Error, Result};

#[derive(Debug, Clone, Copy)]
pub struct PdfHeader {
    pub major: u8,
    pub minor: u8,
}

pub fn parse_header(data: &[u8]) -> Result<PdfHeader> {
    let prefix = b"%PDF-";
    let pos = data
        .windows(prefix.len())
        .position(|w| w == prefix)
        .ok_or(Error::NotAPdf)?;

    // A malformed version after the magic (e.g. `%PDF-a.4`) doesn't make the
    // body unparseable — treat it as PDF 1.7 like other robust readers do.
    let rest = &data[pos + prefix.len()..];
    if rest.len() >= 3 {
        let digit = |b: u8| b.checked_sub(b'0').filter(|&v| v <= 9);
        if let (Some(major), b'.', Some(minor)) = (digit(rest[0]), rest[1], digit(rest[2])) {
            return Ok(PdfHeader { major, minor });
        }
    }

    let shown = String::from_utf8_lossy(&rest[..rest.len().min(8)]);
    tracing::warn!("malformed PDF header version {shown:?}; assuming PDF 1.7");
    Ok(PdfHeader { major: 1, minor: 7 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_header() {
        let data = b"%PDF-1.7\n";
        let h = parse_header(data).unwrap();
        assert_eq!(h.major, 1);
        assert_eq!(h.minor, 7);
    }

    #[test]
    fn pdf_2_0() {
        let data = b"%PDF-2.0\n";
        let h = parse_header(data).unwrap();
        assert_eq!(h.major, 2);
        assert_eq!(h.minor, 0);
    }

    #[test]
    fn garbage_before_header() {
        let data = b"\xef\xbb\xbf%PDF-1.4\n";
        let h = parse_header(data).unwrap();
        assert_eq!(h.major, 1);
        assert_eq!(h.minor, 4);
    }

    #[test]
    fn not_a_pdf() {
        assert!(parse_header(b"not a pdf").is_err());
    }

    #[test]
    fn malformed_version_defaults_to_1_7() {
        // veraPDF corpus 6.1.2 file-header test: bad version digit. The body
        // is a normal PDF, so the header must not reject the whole file.
        let h = parse_header(b"%PDF-a.4\n").unwrap();
        assert_eq!((h.major, h.minor), (1, 7));
    }

    #[test]
    fn truncated_version_defaults_to_1_7() {
        let h = parse_header(b"%PDF-").unwrap();
        assert_eq!((h.major, h.minor), (1, 7));
    }
}
