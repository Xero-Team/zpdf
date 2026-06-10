use zpdf_core::{Error, ObjectId, ParseLimits, PdfName, PdfObject, PdfString, Result};

pub struct Lexer<'a> {
    data: &'a [u8],
    pos: usize,
    limits: &'a ParseLimits,
    depth: u32,
}

impl<'a> Lexer<'a> {
    pub fn new(data: &'a [u8], pos: usize, limits: &'a ParseLimits) -> Self {
        Self {
            data,
            pos,
            limits,
            depth: 0,
        }
    }

    /// Increment container-nesting depth, erroring if it exceeds the limit.
    /// Call once on entry to `read_array`/`read_dict`.
    fn enter_container(&mut self) -> Result<()> {
        self.depth += 1;
        if self.depth > self.limits.max_object_depth {
            return Err(Error::RecursionLimit(self.limits.max_object_depth));
        }
        Ok(())
    }

    fn leave_container(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn set_pos(&mut self, pos: usize) {
        self.pos = pos;
    }

    pub fn is_eof(&self) -> bool {
        self.pos >= self.data.len()
    }

    fn peek(&self) -> Option<u8> {
        self.data.get(self.pos).copied()
    }

    fn advance(&mut self) -> Option<u8> {
        let b = self.data.get(self.pos).copied()?;
        self.pos += 1;
        Some(b)
    }

    pub fn skip_whitespace_and_comments(&mut self) {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\r' | b'\n' | b'\x00' | b'\x0c') => {
                    self.pos += 1;
                }
                Some(b'%') => {
                    self.pos += 1;
                    while let Some(b) = self.peek() {
                        self.pos += 1;
                        if b == b'\r' || b == b'\n' {
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
    }

    pub fn next_token(&mut self) -> Result<PdfObject> {
        self.skip_whitespace_and_comments();

        if self.is_eof() {
            return Err(Error::UnexpectedEof(self.pos as u64));
        }

        match self.peek().unwrap() {
            b'/' => self.read_name(),
            b'(' => self.read_literal_string(),
            b'<' => {
                if self.data.get(self.pos + 1) == Some(&b'<') {
                    self.read_dict()
                } else {
                    self.read_hex_string()
                }
            }
            b'[' => self.read_array(),
            b'+' | b'-' | b'.' | b'0'..=b'9' => self.read_number(),
            b't' | b'f' => self.read_bool_or_keyword(),
            b'n' => self.read_null_or_keyword(),
            _ => Err(Error::InvalidObject(
                self.pos as u64,
                format!("unexpected byte: 0x{:02x}", self.peek().unwrap()),
            )),
        }
    }

    fn read_name(&mut self) -> Result<PdfObject> {
        self.advance(); // skip '/'
        let start = self.pos;
        while let Some(b) = self.peek() {
            if is_delimiter(b) || is_whitespace(b) {
                break;
            }
            self.pos += 1;
        }
        let raw = &self.data[start..self.pos];
        let name = decode_name(raw);
        Ok(PdfObject::Name(PdfName::new(name)))
    }

    fn read_literal_string(&mut self) -> Result<PdfObject> {
        self.advance(); // skip '('
        let mut buf = Vec::new();
        let mut depth = 1u32;
        let max = self.limits.max_string_length as usize;

        while let Some(b) = self.advance() {
            match b {
                b'(' => {
                    depth += 1;
                    buf.push(b'(');
                }
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    buf.push(b')');
                }
                b'\\' => {
                    if let Some(esc) = self.advance() {
                        match esc {
                            b'n' => buf.push(b'\n'),
                            b'r' => buf.push(b'\r'),
                            b't' => buf.push(b'\t'),
                            b'b' => buf.push(0x08),
                            b'f' => buf.push(0x0c),
                            b'(' => buf.push(b'('),
                            b')' => buf.push(b')'),
                            b'\\' => buf.push(b'\\'),
                            b'0'..=b'7' => {
                                let mut octal = (esc - b'0') as u16;
                                for _ in 0..2 {
                                    match self.peek() {
                                        Some(c @ b'0'..=b'7') => {
                                            octal = octal * 8 + (c - b'0') as u16;
                                            self.pos += 1;
                                        }
                                        _ => break,
                                    }
                                }
                                buf.push(octal as u8);
                            }
                            b'\r' => {
                                if self.peek() == Some(b'\n') {
                                    self.pos += 1;
                                }
                            }
                            b'\n' => {}
                            _ => buf.push(esc),
                        }
                    }
                }
                _ => buf.push(b),
            }
            // Each iteration pushes at most one byte, so a single post-match
            // check is sufficient to bound total string growth.
            if buf.len() > max {
                return Err(Error::StringLengthLimit(self.limits.max_string_length));
            }
        }

        Ok(PdfObject::String(PdfString::new(buf)))
    }

    fn read_hex_string(&mut self) -> Result<PdfObject> {
        self.advance(); // skip '<'
        let mut buf = Vec::new();
        let mut high: Option<u8> = None;
        let max = self.limits.max_string_length as usize;

        loop {
            match self.advance() {
                Some(b'>') => break,
                Some(b) if is_whitespace(b) => continue,
                Some(b) => {
                    let nibble = hex_digit(b).ok_or_else(|| {
                        Error::InvalidObject(self.pos as u64 - 1, "invalid hex digit".into())
                    })?;
                    match high {
                        None => high = Some(nibble),
                        Some(h) => {
                            buf.push((h << 4) | nibble);
                            high = None;
                            if buf.len() > max {
                                return Err(Error::StringLengthLimit(
                                    self.limits.max_string_length,
                                ));
                            }
                        }
                    }
                }
                None => return Err(Error::UnexpectedEof(self.pos as u64)),
            }
        }

        if let Some(h) = high {
            buf.push(h << 4);
        }

        Ok(PdfObject::String(PdfString::new(buf)))
    }

    fn read_number(&mut self) -> Result<PdfObject> {
        let start = self.pos;
        let mut has_dot = false;

        if matches!(self.peek(), Some(b'+' | b'-')) {
            self.pos += 1;
        }

        while let Some(b) = self.peek() {
            match b {
                b'0'..=b'9' => self.pos += 1,
                b'.' if !has_dot => {
                    has_dot = true;
                    self.pos += 1;
                }
                _ => break,
            }
        }

        let s = std::str::from_utf8(&self.data[start..self.pos])
            .map_err(|_| Error::InvalidObject(start as u64, "invalid number".into()))?;

        if has_dot {
            let n: f64 = s
                .parse()
                .map_err(|_| Error::InvalidObject(start as u64, format!("bad real: {s}")))?;
            Ok(PdfObject::Real(n))
        } else {
            let n: i64 = s
                .parse()
                .map_err(|_| Error::InvalidObject(start as u64, format!("bad integer: {s}")))?;
            Ok(PdfObject::Integer(n))
        }
    }

    fn read_bool_or_keyword(&mut self) -> Result<PdfObject> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if is_delimiter(b) || is_whitespace(b) {
                break;
            }
            self.pos += 1;
        }
        let word = &self.data[start..self.pos];
        match word {
            b"true" => Ok(PdfObject::Bool(true)),
            b"false" => Ok(PdfObject::Bool(false)),
            _ => Err(Error::InvalidObject(
                start as u64,
                format!("unexpected keyword: {}", String::from_utf8_lossy(word)),
            )),
        }
    }

    fn read_null_or_keyword(&mut self) -> Result<PdfObject> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if is_delimiter(b) || is_whitespace(b) {
                break;
            }
            self.pos += 1;
        }
        let word = &self.data[start..self.pos];
        match word {
            b"null" => Ok(PdfObject::Null),
            _ => Err(Error::InvalidObject(
                start as u64,
                format!("unexpected keyword: {}", String::from_utf8_lossy(word)),
            )),
        }
    }

    fn read_array(&mut self) -> Result<PdfObject> {
        self.enter_container()?;
        self.advance(); // skip '['
        let mut items = Vec::new();
        loop {
            self.skip_whitespace_and_comments();
            if self.peek() == Some(b']') {
                self.pos += 1;
                break;
            }
            if self.is_eof() {
                return Err(Error::UnexpectedEof(self.pos as u64));
            }
            let obj = self.next_token()?;
            items.push(self.maybe_resolve_ref(obj)?);
        }
        self.leave_container();
        Ok(PdfObject::Array(items))
    }

    fn read_dict(&mut self) -> Result<PdfObject> {
        self.enter_container()?;
        self.pos += 2; // skip '<<'
        let mut dict = zpdf_core::PdfDict::new();
        loop {
            self.skip_whitespace_and_comments();
            if self.data.get(self.pos..self.pos + 2) == Some(b">>") {
                self.pos += 2;
                break;
            }
            if self.is_eof() {
                return Err(Error::UnexpectedEof(self.pos as u64));
            }
            let key = match self.next_token()? {
                PdfObject::Name(n) => n,
                other => {
                    return Err(Error::InvalidObject(
                        self.pos as u64,
                        format!("dict key must be Name, got {}", other.type_name()),
                    ));
                }
            };
            let value = self.next_token()?;
            let value = self.maybe_resolve_ref(value)?;
            dict.insert(key, value);
        }
        self.leave_container();
        Ok(PdfObject::Dict(dict))
    }

    pub(crate) fn maybe_resolve_ref(&mut self, obj: PdfObject) -> Result<PdfObject> {
        if let PdfObject::Integer(num) = obj {
            let saved = self.pos;
            self.skip_whitespace_and_comments();
            if let Ok(PdfObject::Integer(gen)) = self.read_number_if_available() {
                self.skip_whitespace_and_comments();
                if self.peek() == Some(b'R') {
                    self.pos += 1;
                    return Ok(PdfObject::Ref(ObjectId(num as u32, gen as u16)));
                }
            }
            self.pos = saved;
            Ok(PdfObject::Integer(num))
        } else {
            Ok(obj)
        }
    }

    fn read_number_if_available(&mut self) -> Result<PdfObject> {
        if matches!(self.peek(), Some(b'0'..=b'9' | b'+' | b'-' | b'.')) {
            self.read_number()
        } else {
            Err(Error::InvalidObject(self.pos as u64, "not a number".into()))
        }
    }
}

fn is_whitespace(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n' | b'\x00' | b'\x0c')
}

fn is_delimiter(b: u8) -> bool {
    matches!(
        b,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn decode_name(raw: &[u8]) -> String {
    let mut result = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'#' && i + 2 < raw.len() {
            if let (Some(h), Some(l)) = (hex_digit(raw[i + 1]), hex_digit(raw[i + 2])) {
                result.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        result.push(raw[i]);
        i += 1;
    }
    String::from_utf8_lossy(&result).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lim() -> ParseLimits {
        ParseLimits::default()
    }

    #[test]
    fn lex_name() {
        let l = lim();
        let mut lex = Lexer::new(b"/Type", 0, &l);
        let obj = lex.next_token().unwrap();
        assert_eq!(obj, PdfObject::Name(PdfName::new("Type")));
    }

    #[test]
    fn lex_name_with_hex_escape() {
        let l = lim();
        let mut lex = Lexer::new(b"/A#20B", 0, &l);
        let obj = lex.next_token().unwrap();
        assert_eq!(obj, PdfObject::Name(PdfName::new("A B")));
    }

    #[test]
    fn lex_integer() {
        let l = lim();
        let mut lex = Lexer::new(b"42 ", 0, &l);
        assert_eq!(lex.next_token().unwrap(), PdfObject::Integer(42));
    }

    #[test]
    fn lex_negative_real() {
        let l = lim();
        let mut lex = Lexer::new(b"-3.5 ", 0, &l);
        match lex.next_token().unwrap() {
            PdfObject::Real(n) => assert!((n - (-3.5)).abs() < 1e-10),
            other => panic!("expected Real, got {other:?}"),
        }
    }

    #[test]
    fn lex_literal_string() {
        let l = lim();
        let mut lex = Lexer::new(b"(hello world)", 0, &l);
        let obj = lex.next_token().unwrap();
        assert_eq!(
            obj,
            PdfObject::String(PdfString::new(b"hello world".to_vec()))
        );
    }

    #[test]
    fn lex_literal_string_nested_parens() {
        let l = lim();
        let mut lex = Lexer::new(b"(a (b) c)", 0, &l);
        let obj = lex.next_token().unwrap();
        assert_eq!(obj, PdfObject::String(PdfString::new(b"a (b) c".to_vec())));
    }

    #[test]
    fn lex_hex_string() {
        let l = lim();
        let mut lex = Lexer::new(b"<48656C6C6F>", 0, &l);
        let obj = lex.next_token().unwrap();
        assert_eq!(obj, PdfObject::String(PdfString::new(b"Hello".to_vec())));
    }

    #[test]
    fn lex_array() {
        let l = lim();
        let mut lex = Lexer::new(b"[1 2 3]", 0, &l);
        let obj = lex.next_token().unwrap();
        assert_eq!(
            obj,
            PdfObject::Array(vec![
                PdfObject::Integer(1),
                PdfObject::Integer(2),
                PdfObject::Integer(3),
            ])
        );
    }

    #[test]
    fn lex_dict() {
        let l = lim();
        let mut lex = Lexer::new(b"<< /Type /Page /Count 5 >>", 0, &l);
        let obj = lex.next_token().unwrap();
        match obj {
            PdfObject::Dict(d) => {
                assert_eq!(d.get_name("Type").unwrap(), "Page");
                assert_eq!(d.get_i64("Count").unwrap(), 5);
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn lex_bool_and_null() {
        let l = lim();
        let mut lex = Lexer::new(b"true", 0, &l);
        assert_eq!(lex.next_token().unwrap(), PdfObject::Bool(true));

        let mut lex = Lexer::new(b"false", 0, &l);
        assert_eq!(lex.next_token().unwrap(), PdfObject::Bool(false));

        let mut lex = Lexer::new(b"null", 0, &l);
        assert_eq!(lex.next_token().unwrap(), PdfObject::Null);
    }

    #[test]
    fn lex_indirect_ref_in_array() {
        let l = lim();
        let mut lex = Lexer::new(b"[12 0 R]", 0, &l);
        let obj = lex.next_token().unwrap();
        assert_eq!(obj, PdfObject::Array(vec![PdfObject::Ref(ObjectId(12, 0))]));
    }

    #[test]
    fn skip_comments() {
        let l = lim();
        let mut lex = Lexer::new(b"% comment\n42 ", 0, &l);
        assert_eq!(lex.next_token().unwrap(), PdfObject::Integer(42));
    }

    #[test]
    fn reject_deeply_nested_array() {
        let mut l = lim();
        l.max_object_depth = 10;
        let depth = 50usize;
        let mut data = vec![b'['; depth];
        data.extend(std::iter::repeat_n(b']', depth));
        let mut lex = Lexer::new(&data, 0, &l);
        let err = lex.next_token().unwrap_err();
        assert!(matches!(err, Error::RecursionLimit(10)), "got {err:?}");
    }

    #[test]
    fn reject_deeply_nested_dict() {
        let mut l = lim();
        l.max_object_depth = 5;
        let n = 20usize;
        let mut s = String::new();
        for _ in 0..n {
            s.push_str("<< /a ");
        }
        s.push('1');
        for _ in 0..n {
            s.push_str(" >>");
        }
        let data = s.into_bytes();
        let mut lex = Lexer::new(&data, 0, &l);
        let err = lex.next_token().unwrap_err();
        assert!(matches!(err, Error::RecursionLimit(5)), "got {err:?}");
    }

    #[test]
    fn nested_within_limit_ok() {
        let l = lim(); // depth limit 100
        let data = b"[[[[[1]]]]]"; // depth 5
        let mut lex = Lexer::new(data, 0, &l);
        assert!(lex.next_token().is_ok());
    }

    #[test]
    fn reject_oversized_literal_string() {
        let mut l = lim();
        l.max_string_length = 8;
        let mut data = vec![b'('];
        data.extend(std::iter::repeat_n(b'a', 100));
        data.push(b')');
        let mut lex = Lexer::new(&data, 0, &l);
        let err = lex.next_token().unwrap_err();
        assert!(matches!(err, Error::StringLengthLimit(8)), "got {err:?}");
    }

    #[test]
    fn reject_oversized_hex_string() {
        let mut l = lim();
        l.max_string_length = 4;
        // 20 hex digits => 10 raw bytes > 4
        let mut data = vec![b'<'];
        data.extend(std::iter::repeat_n(b'4', 20));
        data.push(b'>');
        let mut lex = Lexer::new(&data, 0, &l);
        let err = lex.next_token().unwrap_err();
        assert!(matches!(err, Error::StringLengthLimit(4)), "got {err:?}");
    }

    #[test]
    fn small_string_within_limit_ok() {
        let l = lim(); // 65536
        let mut lex = Lexer::new(b"(hello)", 0, &l);
        assert_eq!(
            lex.next_token().unwrap(),
            PdfObject::String(PdfString::new(b"hello".to_vec()))
        );
    }
}
