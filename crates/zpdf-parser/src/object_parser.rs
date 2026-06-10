use std::sync::Arc;

use zpdf_core::{Error, ObjectId, ParseLimits, PdfDict, PdfObject, PdfStream, Result};

use crate::lexer::Lexer;

pub struct ObjectParser<'a> {
    data: &'a [u8],
    limits: &'a ParseLimits,
}

impl<'a> ObjectParser<'a> {
    pub fn new(data: &'a [u8], limits: &'a ParseLimits) -> Self {
        Self { data, limits }
    }

    /// Parse an indirect object at the given byte offset.
    /// Expected format: `<num> <gen> obj <value> endobj`
    pub fn parse_indirect_at(&self, offset: usize) -> Result<PdfObject> {
        self.parse_indirect_with_id(offset).map(|(_, obj)| obj)
    }

    /// Like [`parse_indirect_at`](Self::parse_indirect_at), but also returns
    /// the `(num, gen)` actually present in the object header. Callers that
    /// arrived here via an xref entry can compare it against the id they asked
    /// for and trigger repair on a mismatch (stale/corrupt offsets are common
    /// in damaged files).
    pub fn parse_indirect_with_id(&self, offset: usize) -> Result<(ObjectId, PdfObject)> {
        let mut lex = Lexer::new(self.data, offset, self.limits);

        let num_tok = lex.next_token()?;
        let gen_tok = lex.next_token()?;
        let id = match (&num_tok, &gen_tok) {
            (PdfObject::Integer(n), PdfObject::Integer(g)) => {
                match (u32::try_from(*n), u16::try_from(*g)) {
                    (Ok(n), Ok(g)) => ObjectId(n, g),
                    _ => {
                        return Err(Error::InvalidObject(
                            offset as u64,
                            format!("object header out of range: {n} {g} obj"),
                        ))
                    }
                }
            }
            _ => {
                return Err(Error::InvalidObject(
                    offset as u64,
                    "object header is not '<int> <int> obj'".into(),
                ))
            }
        };

        lex.skip_whitespace_and_comments();
        self.expect_keyword(&mut lex, b"obj")?;

        let obj = lex.next_token()?;
        // A top-level body may itself be an indirect reference (`N G R`), which
        // the plain tokenizer reads as a bare integer; promote it so resolve()
        // can follow ref-to-ref chains.
        let obj = lex.maybe_resolve_ref(obj)?;

        // Check if this is a stream object
        lex.skip_whitespace_and_comments();
        if let PdfObject::Dict(dict) = &obj {
            if self.starts_with_at(lex.pos(), b"stream") {
                let stream = self.read_stream(dict.clone(), lex.pos())?;
                return Ok((id, PdfObject::Stream(stream)));
            }
        }

        Ok((id, obj))
    }

    fn expect_keyword(&self, lex: &mut Lexer, keyword: &[u8]) -> Result<()> {
        let pos = lex.pos();
        if self.data[pos..].starts_with(keyword) {
            lex.set_pos(pos + keyword.len());
            Ok(())
        } else {
            Err(Error::InvalidObject(
                pos as u64,
                format!(
                    "expected '{}', got '{}'",
                    String::from_utf8_lossy(keyword),
                    String::from_utf8_lossy(
                        &self.data[pos..self.data.len().min(pos + keyword.len())]
                    )
                ),
            ))
        }
    }

    fn starts_with_at(&self, pos: usize, prefix: &[u8]) -> bool {
        self.data.get(pos..).is_some_and(|s| s.starts_with(prefix))
    }

    fn read_stream(&self, dict: PdfDict, keyword_pos: usize) -> Result<PdfStream> {
        let mut pos = keyword_pos + b"stream".len();

        // Skip stream keyword EOL: \r\n or \n (a lone \r is tolerated too).
        if self.data.get(pos) == Some(&b'\r') {
            pos += 1;
        }
        if self.data.get(pos) == Some(&b'\n') {
            pos += 1;
        }

        // Determine the stream's byte length. Trust a direct, non-negative
        // /Length ONLY if `endstream` actually follows it; otherwise (missing,
        // indirect `N G R`, negative, or simply wrong) fall back to scanning for
        // the `endstream` keyword. The low-level parser cannot resolve an
        // indirect /Length, so without this fallback such streams (very common,
        // e.g. Acrobat output) would decode to empty/garbage data.
        let declared = match dict.get("Length") {
            Some(PdfObject::Integer(n)) if *n >= 0 => Some(*n as usize),
            _ => None,
        };

        let end = match declared {
            Some(len)
                if pos
                    .checked_add(len)
                    .is_some_and(|e| self.endstream_follows(e)) =>
            {
                pos + len
            }
            _ => self.scan_for_endstream(pos)?,
        };

        let length = (end - pos) as u64;
        if length > self.limits.max_stream_bytes {
            return Err(Error::StreamSizeLimit(self.limits.max_stream_bytes));
        }

        let stream_data = self.data[pos..end].to_vec();
        Ok(PdfStream {
            dict,
            data: Arc::from(stream_data),
        })
    }

    /// True if (after optional whitespace) the bytes at `at` begin the
    /// `endstream` keyword. Used to validate a declared /Length before trusting it.
    fn endstream_follows(&self, at: usize) -> bool {
        let mut p = at;
        while let Some(&b) = self.data.get(p) {
            if matches!(b, b' ' | b'\t' | b'\r' | b'\n' | b'\x00' | b'\x0c') {
                p += 1;
            } else {
                break;
            }
        }
        self.data
            .get(p..)
            .is_some_and(|s| s.starts_with(b"endstream"))
    }

    /// Find the stream's data end by scanning for the `endstream` keyword,
    /// stripping the single EOL that precedes it (per spec, not part of the
    /// data). The search is bounded by `max_stream_bytes` so a stream missing
    /// its `endstream` cannot force an unbounded scan.
    fn scan_for_endstream(&self, pos: usize) -> Result<usize> {
        let cap = (self.limits.max_stream_bytes as usize).saturating_add(b"endstream".len() + 2);
        let search_end = pos.saturating_add(cap).min(self.data.len());
        let hay = self
            .data
            .get(pos..search_end)
            .ok_or(Error::UnexpectedEof(pos as u64))?;
        let rel = hay
            .windows(b"endstream".len())
            .position(|w| w == b"endstream")
            .ok_or_else(|| {
                Error::InvalidObject(pos as u64, "stream: no endstream within size limit".into())
            })?;
        let mut end = pos + rel;
        // Strip the EOL immediately before `endstream` (CRLF, LF, or lone CR).
        if end > pos && self.data[end - 1] == b'\n' {
            end -= 1;
            if end > pos && self.data[end - 1] == b'\r' {
                end -= 1;
            }
        } else if end > pos && self.data[end - 1] == b'\r' {
            end -= 1;
        }
        Ok(end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_indirect() {
        let data = b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n";
        let limits = ParseLimits::default();
        let parser = ObjectParser::new(data, &limits);
        let obj = parser.parse_indirect_at(0).unwrap();
        match obj {
            PdfObject::Dict(d) => {
                assert_eq!(d.get_name("Type").unwrap(), "Catalog");
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn parse_stream_object() {
        let content = b"BT /F1 12 Tf (Hello) Tj ET";
        let obj_bytes = format!("5 0 obj\n<< /Length {} >>\nstream\n", content.len());
        let mut data = obj_bytes.into_bytes();
        data.extend_from_slice(content);
        data.extend_from_slice(b"\nendstream\nendobj\n");

        let limits = ParseLimits::default();
        let parser = ObjectParser::new(&data, &limits);
        let obj = parser.parse_indirect_at(0).unwrap();
        match obj {
            PdfObject::Stream(s) => {
                assert_eq!(s.data.as_ref(), content);
                assert_eq!(s.dict.get_i64("Length").unwrap(), content.len() as i64);
            }
            other => panic!("expected Stream, got {other:?}"),
        }
    }

    #[test]
    fn reject_oversized_stream_length() {
        let limits = ParseLimits {
            max_stream_bytes: 16,
            ..Default::default()
        };
        let body = b"0123456789ABCDEFGHIJ"; // 20 bytes > 16
        let obj_bytes = format!("5 0 obj\n<< /Length {} >>\nstream\n", body.len());
        let mut data = obj_bytes.into_bytes();
        data.extend_from_slice(body);
        data.extend_from_slice(b"\nendstream\nendobj\n");
        let parser = ObjectParser::new(&data, &limits);
        let err = parser.parse_indirect_at(0).unwrap_err();
        assert!(matches!(err, Error::StreamSizeLimit(16)), "got {err:?}");
    }

    /// Helper: parse a single stream object and return its decoded data bytes.
    fn stream_data(data: &[u8]) -> Vec<u8> {
        let limits = ParseLimits::default();
        let parser = ObjectParser::new(data, &limits);
        match parser.parse_indirect_at(0).unwrap() {
            PdfObject::Stream(s) => s.data.to_vec(),
            other => panic!("expected Stream, got {other:?}"),
        }
    }

    #[test]
    fn indirect_length_recovers_via_endstream_scan() {
        // `/Length 99 0 R` is an indirect ref the low-level parser cannot
        // resolve; it must fall back to scanning for `endstream`.
        let mut data = b"5 0 obj\n<< /Length 99 0 R >>\nstream\n".to_vec();
        data.extend_from_slice(b"Hello, world!");
        data.extend_from_slice(b"\nendstream\nendobj\n");
        assert_eq!(stream_data(&data), b"Hello, world!");
    }

    #[test]
    fn missing_length_recovers_via_endstream_scan() {
        let mut data = b"5 0 obj\n<< /Type /Whatever >>\nstream\n".to_vec();
        data.extend_from_slice(b"payload bytes");
        data.extend_from_slice(b"\nendstream\nendobj\n");
        assert_eq!(stream_data(&data), b"payload bytes");
    }

    #[test]
    fn wrong_length_recovers_via_endstream_scan() {
        // Declared /Length 3 but the real body is 5 bytes; `endstream` does not
        // follow at +3, so the scan recovers the true extent.
        let mut data = b"5 0 obj\n<< /Length 3 >>\nstream\n".to_vec();
        data.extend_from_slice(b"Hello");
        data.extend_from_slice(b"\nendstream\nendobj\n");
        assert_eq!(stream_data(&data), b"Hello");
    }

    #[test]
    fn negative_length_recovers_via_endstream_scan() {
        let mut data = b"5 0 obj\n<< /Length -1 >>\nstream\n".to_vec();
        data.extend_from_slice(b"abc");
        data.extend_from_slice(b"\nendstream\nendobj\n");
        assert_eq!(stream_data(&data), b"abc");
    }

    #[test]
    fn correct_length_trusted_even_if_data_contains_endstream_bytes() {
        // A correct /Length must be trusted so binary data that happens to
        // contain the bytes "endstream" is not truncated at the wrong place.
        let body: &[u8] = b"AAendstreamBB"; // 13 bytes, literal "endstream" inside
        let mut data = format!("5 0 obj\n<< /Length {} >>\nstream\n", body.len()).into_bytes();
        data.extend_from_slice(body);
        data.extend_from_slice(b"\nendstream\nendobj\n");
        assert_eq!(stream_data(&data), body);
    }

    #[test]
    fn crlf_before_endstream_is_stripped_on_scan() {
        // When scanning, a CRLF preceding `endstream` must not be included.
        let mut data = b"5 0 obj\n<< >>\nstream\n".to_vec();
        data.extend_from_slice(b"data");
        data.extend_from_slice(b"\r\nendstream\nendobj\n");
        assert_eq!(stream_data(&data), b"data");
    }

    #[test]
    fn parse_indirect_with_id_returns_header_id() {
        let data = b"7 2 obj\n<< /Type /Catalog >>\nendobj\n";
        let limits = ParseLimits::default();
        let parser = ObjectParser::new(data, &limits);
        let (id, obj) = parser.parse_indirect_with_id(0).unwrap();
        assert_eq!(id, ObjectId(7, 2));
        assert!(obj.as_dict().is_ok());
    }

    #[test]
    fn parse_indirect_with_id_rejects_non_integer_header() {
        let data = b"/Name 0 obj\n42\nendobj\n";
        let limits = ParseLimits::default();
        let parser = ObjectParser::new(data, &limits);
        assert!(parser.parse_indirect_with_id(0).is_err());
    }

    #[test]
    fn top_level_ref_body_parses_as_ref() {
        // `4 0 obj 5 0 R endobj` — the body is itself an indirect reference.
        let data = b"4 0 obj\n5 0 R\nendobj\n";
        let limits = ParseLimits::default();
        let parser = ObjectParser::new(data, &limits);
        let obj = parser.parse_indirect_at(0).unwrap();
        assert_eq!(obj, PdfObject::Ref(ObjectId(5, 0)));
    }

    #[test]
    fn deeply_nested_value_in_indirect_object_errors() {
        // The recursion guard must fire even when reached via parse_indirect_at.
        let limits = ParseLimits {
            max_object_depth: 4,
            ..Default::default()
        };
        let n = 20usize;
        let mut inner = String::new();
        for _ in 0..n {
            inner.push('[');
        }
        inner.push('1');
        for _ in 0..n {
            inner.push(']');
        }
        let data = format!("1 0 obj\n{inner}\nendobj\n").into_bytes();
        let parser = ObjectParser::new(&data, &limits);
        let err = parser.parse_indirect_at(0).unwrap_err();
        assert!(matches!(err, Error::RecursionLimit(4)), "got {err:?}");
    }
}
