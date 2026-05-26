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

    let rest = &data[pos + prefix.len()..];
    if rest.len() < 3 {
        return Err(Error::NotAPdf);
    }

    let major = rest[0]
        .checked_sub(b'0')
        .filter(|&v| v <= 9)
        .ok_or(Error::NotAPdf)?;

    if rest[1] != b'.' {
        return Err(Error::NotAPdf);
    }

    let minor = rest[2]
        .checked_sub(b'0')
        .filter(|&v| v <= 9)
        .ok_or(Error::NotAPdf)?;

    Ok(PdfHeader { major, minor })
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
}
