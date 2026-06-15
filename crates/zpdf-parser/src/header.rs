use zpdf_core::{Error, Result};

#[derive(Debug, Clone, Copy)]
pub struct PdfHeader {
    pub major: u8,
    pub minor: u8,
}

/// Version assumed when a `%PDF` marker is present but the version digits that
/// should follow it are missing or malformed. 1.4 is a safe lower bound: it
/// predates object/xref streams, so nothing is silently disabled, yet every
/// reader treats it as a normal modern document.
const DEFAULT_VERSION: PdfHeader = PdfHeader { major: 1, minor: 4 };

/// Locate and parse the `%PDF` header. Real-world corpora are full of files
/// whose version field is garbage (`%PDF-1.)`, `%PDF-0000000`, `%PDF-/Si3`),
/// missing entirely (`%PDF-\n2 0 obj`), or written without the conventional
/// hyphen (`%PDF/DA2`). Matching mainstream readers, we accept any file that
/// contains the literal `%PDF` and fall back to [`DEFAULT_VERSION`] whenever the
/// trailing version cannot be read. `Err(NotAPdf)` is reserved for files with no
/// `%PDF` marker at all — the caller then tries object-scan recovery, which can
/// still open a headerless fragment that begins directly with `N G obj`.
pub fn parse_header(data: &[u8]) -> Result<PdfHeader> {
    let marker = b"%PDF";
    let pos = data
        .windows(marker.len())
        .position(|w| w == marker)
        .ok_or(Error::NotAPdf)?;

    // The version conventionally follows as "-M.m"; tolerate a missing hyphen.
    let rest = &data[pos + marker.len()..];
    let rest = rest.strip_prefix(b"-").unwrap_or(rest);
    Ok(parse_version(rest).unwrap_or(DEFAULT_VERSION))
}

/// Best-effort `M.m` parse from the bytes following the `%PDF[-]` marker.
/// Returns `None` (caller defaults) if the major digit, the `.`, or the minor
/// digit is absent or out of range.
fn parse_version(rest: &[u8]) -> Option<PdfHeader> {
    let major = rest.first()?.checked_sub(b'0').filter(|&v| v <= 9)?;
    if rest.get(1).copied()? != b'.' {
        return None;
    }
    let minor = rest.get(2)?.checked_sub(b'0').filter(|&v| v <= 9)?;
    Some(PdfHeader { major, minor })
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
    fn marker_without_hyphen_defaults_version() {
        // `%PDF/DA2 ...` — real Ghostscript output; accept it as 1.4.
        let h = parse_header(b"%PDF/DA2 \x1d\n").unwrap();
        assert_eq!((h.major, h.minor), (1, 4));
    }

    #[test]
    fn malformed_version_defaults() {
        for bytes in [
            &b"%PDF-1.)"[..],
            &b"%PDF-0000000"[..],
            &b"%PDF-/Si3/De"[..],
            &b"%PDF-1e66666"[..],
            &b"%PDF-{<~00~"[..],
            &b"%PDF-\n2 0 obj"[..],
        ] {
            let h = parse_header(bytes).expect("marker present => header parses");
            assert_eq!((h.major, h.minor), (1, 4), "input {bytes:?}");
        }
    }

    #[test]
    fn no_marker_at_all_is_err() {
        // A headerless object fragment has no `%PDF`; the parser rejects it here
        // and the caller falls back to object-scan recovery.
        assert!(parse_header(b"1 0 obj<</Type/Catalog>>endobj").is_err());
    }
}
