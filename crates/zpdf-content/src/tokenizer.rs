use zpdf_core::{PdfName, PdfObject, PdfString, Result};

/// A token from a PDF content stream: either an operand or an operator.
#[derive(Debug, Clone)]
pub enum ContentToken {
    Operand(PdfObject),
    Operator(String),
}

/// Tokenizer for PDF content streams.
/// Content streams contain sequences of: operand* operator
pub struct ContentTokenizer<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> ContentTokenizer<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn peek(&self) -> Option<u8> {
        self.data.get(self.pos).copied()
    }

    fn is_eof(&self) -> bool {
        self.pos >= self.data.len()
    }

    fn skip_whitespace(&mut self) {
        while let Some(b) = self.peek() {
            if matches!(b, b' ' | b'\t' | b'\r' | b'\n' | b'\x00' | b'\x0c') {
                self.pos += 1;
            } else if b == b'%' {
                self.pos += 1;
                while let Some(c) = self.peek() {
                    self.pos += 1;
                    if c == b'\r' || c == b'\n' {
                        break;
                    }
                }
            } else {
                break;
            }
        }
    }

    pub fn next_token(&mut self) -> Option<ContentToken> {
        self.skip_whitespace();

        if self.is_eof() {
            return None;
        }

        let b = self.peek().unwrap();

        match b {
            b'0'..=b'9' | b'+' | b'-' | b'.' => {
                let obj = self.read_number();
                Some(ContentToken::Operand(obj))
            }
            b'/' => {
                let name = self.read_name();
                Some(ContentToken::Operand(PdfObject::Name(name)))
            }
            b'(' => {
                let s = self.read_literal_string();
                Some(ContentToken::Operand(PdfObject::String(s)))
            }
            b'<' => {
                if self.data.get(self.pos + 1) == Some(&b'<') {
                    Some(ContentToken::Operand(self.read_dict()))
                } else {
                    let s = self.read_hex_string();
                    Some(ContentToken::Operand(PdfObject::String(s)))
                }
            }
            b'[' => {
                let arr = self.read_array();
                Some(ContentToken::Operand(PdfObject::Array(arr)))
            }
            b't' if self.data[self.pos..].starts_with(b"true") => {
                self.pos += 4;
                Some(ContentToken::Operand(PdfObject::Bool(true)))
            }
            b'f' if self.data[self.pos..].starts_with(b"false") => {
                self.pos += 5;
                Some(ContentToken::Operand(PdfObject::Bool(false)))
            }
            b'n' if self.data[self.pos..].starts_with(b"null") => {
                self.pos += 4;
                Some(ContentToken::Operand(PdfObject::Null))
            }
            _ if b.is_ascii_alphabetic() || b == b'\'' || b == b'"' || b == b'*' => {
                let op = self.read_operator();
                Some(ContentToken::Operator(op))
            }
            _ => {
                self.pos += 1;
                None
            }
        }
    }

    fn read_number(&mut self) -> PdfObject {
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
        let s = std::str::from_utf8(&self.data[start..self.pos]).unwrap_or("0");
        if has_dot {
            PdfObject::Real(s.parse().unwrap_or(0.0))
        } else {
            PdfObject::Integer(s.parse().unwrap_or(0))
        }
    }

    fn read_name(&mut self) -> PdfName {
        self.pos += 1; // skip '/'
        let start = self.pos;
        while let Some(b) = self.peek() {
            if is_delimiter(b) || is_whitespace(b) {
                break;
            }
            self.pos += 1;
        }
        let raw = &self.data[start..self.pos];
        PdfName::new(String::from_utf8_lossy(raw).into_owned())
    }

    fn read_literal_string(&mut self) -> PdfString {
        self.pos += 1; // skip '('
        let mut buf = Vec::new();
        let mut depth = 1u32;
        while let Some(b) = self.data.get(self.pos).copied() {
            self.pos += 1;
            match b {
                b'(' => { depth += 1; buf.push(b'('); }
                b')' => {
                    depth -= 1;
                    if depth == 0 { break; }
                    buf.push(b')');
                }
                b'\\' => {
                    if let Some(&esc) = self.data.get(self.pos) {
                        self.pos += 1;
                        match esc {
                            b'n' => buf.push(b'\n'),
                            b'r' => buf.push(b'\r'),
                            b't' => buf.push(b'\t'),
                            b'(' => buf.push(b'('),
                            b')' => buf.push(b')'),
                            b'\\' => buf.push(b'\\'),
                            _ => buf.push(esc),
                        }
                    }
                }
                _ => buf.push(b),
            }
        }
        PdfString::new(buf)
    }

    fn read_hex_string(&mut self) -> PdfString {
        self.pos += 1; // skip '<'
        let mut buf = Vec::new();
        let mut high: Option<u8> = None;
        while let Some(&b) = self.data.get(self.pos) {
            self.pos += 1;
            if b == b'>' { break; }
            if is_whitespace(b) { continue; }
            let nibble = match b {
                b'0'..=b'9' => b - b'0',
                b'a'..=b'f' => b - b'a' + 10,
                b'A'..=b'F' => b - b'A' + 10,
                _ => continue,
            };
            match high {
                None => high = Some(nibble),
                Some(h) => { buf.push((h << 4) | nibble); high = None; }
            }
        }
        if let Some(h) = high { buf.push(h << 4); }
        PdfString::new(buf)
    }

    fn read_array(&mut self) -> Vec<PdfObject> {
        self.pos += 1; // skip '['
        let mut items = Vec::new();
        loop {
            self.skip_whitespace();
            if self.peek() == Some(b']') {
                self.pos += 1;
                break;
            }
            if self.is_eof() { break; }
            if let Some(ContentToken::Operand(obj)) = self.next_token() {
                items.push(obj);
            }
        }
        items
    }

    fn read_dict(&mut self) -> PdfObject {
        self.pos += 2; // skip '<<'
        let mut dict = zpdf_core::PdfDict::new();
        loop {
            self.skip_whitespace();
            if self.data.get(self.pos..self.pos + 2) == Some(b">>") {
                self.pos += 2;
                break;
            }
            if self.is_eof() { break; }
            let key = self.read_name();
            if let Some(ContentToken::Operand(val)) = self.next_token() {
                dict.insert(key, val);
            }
        }
        PdfObject::Dict(dict)
    }

    fn read_operator(&mut self) -> String {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if is_whitespace(b) || is_delimiter(b) {
                break;
            }
            self.pos += 1;
        }
        String::from_utf8_lossy(&self.data[start..self.pos]).into_owned()
    }
}

impl<'a> Iterator for ContentTokenizer<'a> {
    type Item = ContentToken;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_token()
    }
}

fn is_whitespace(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n' | b'\x00' | b'\x0c')
}

fn is_delimiter(b: u8) -> bool {
    matches!(b, b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_simple_content() {
        let data = b"BT /F1 12 Tf (Hello) Tj ET";
        let tokens: Vec<_> = ContentTokenizer::new(data).collect();
        assert!(matches!(&tokens[0], ContentToken::Operator(s) if s == "BT"));
        assert!(matches!(&tokens[1], ContentToken::Operand(PdfObject::Name(n)) if n.as_str() == "F1"));
        assert!(matches!(&tokens[2], ContentToken::Operand(PdfObject::Integer(12))));
        assert!(matches!(&tokens[3], ContentToken::Operator(s) if s == "Tf"));
    }

    #[test]
    fn tokenize_path() {
        let data = b"100 200 m 300 400 l S";
        let tokens: Vec<_> = ContentTokenizer::new(data).collect();
        // 100, 200, m, 300, 400, l, S = 7 tokens
        assert_eq!(tokens.len(), 7);
    }
}
