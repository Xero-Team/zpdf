use std::sync::Arc;

use zpdf_core::{Error, ParseLimits, PdfDict, PdfObject, PdfStream, Result};

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
        let mut lex = Lexer::new(self.data, offset);

        let _obj_num = lex.next_token()?;
        let _gen_num = lex.next_token()?;

        lex.skip_whitespace_and_comments();
        self.expect_keyword(&mut lex, b"obj")?;

        let obj = lex.next_token()?;

        // Check if this is a stream object
        lex.skip_whitespace_and_comments();
        if let PdfObject::Dict(dict) = &obj {
            if self.starts_with_at(lex.pos(), b"stream") {
                let stream = self.read_stream(dict.clone(), lex.pos())?;
                return Ok(PdfObject::Stream(stream));
            }
        }

        Ok(obj)
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

        // Skip stream keyword EOL: \r\n or \n
        if self.data.get(pos) == Some(&b'\r') {
            pos += 1;
        }
        if self.data.get(pos) == Some(&b'\n') {
            pos += 1;
        }

        let length = dict.get_i64("Length").unwrap_or(0) as usize;

        if length as u64 > self.limits.max_stream_bytes {
            return Err(Error::StreamSizeLimit(self.limits.max_stream_bytes));
        }

        let end = pos + length;
        if end > self.data.len() {
            return Err(Error::UnexpectedEof(end as u64));
        }

        let stream_data = self.data[pos..end].to_vec();
        Ok(PdfStream {
            dict,
            data: Arc::from(stream_data),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zpdf_core::PdfName;

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
        let obj_bytes = format!(
            "5 0 obj\n<< /Length {} >>\nstream\n",
            content.len()
        );
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
}
