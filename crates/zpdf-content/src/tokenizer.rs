use zpdf_core::{PdfName, PdfObject, PdfString};

/// A token from a PDF content stream: either an operand or an operator.
#[derive(Debug, Clone)]
pub enum ContentToken {
    Operand(PdfObject),
    Operator(String),
    /// An inline image (`BI … ID <binary> EI`) with its raw parameter dict
    /// (abbreviated keys/values not yet normalized) and the raw sample bytes.
    InlineImage {
        dict: zpdf_core::PdfDict,
        data: Vec<u8>,
    },
}

/// Maximum array/dict nesting depth in a content stream. Real content is shallow;
/// this only exists to bound recursion so adversarial deeply-nested input cannot
/// overflow the native stack.
const MAX_CONTENT_DEPTH: u32 = 200;

/// Tokenizer for PDF content streams.
/// Content streams contain sequences of: operand* operator
pub struct ContentTokenizer<'a> {
    data: &'a [u8],
    pos: usize,
    /// Current array/dict nesting depth (see `MAX_CONTENT_DEPTH`).
    depth: u32,
}

impl<'a> ContentTokenizer<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            depth: 0,
        }
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
                if op == "BI" {
                    return Some(self.read_inline_image());
                }
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
                    if let Some(&esc) = self.data.get(self.pos) {
                        self.pos += 1;
                        match esc {
                            b'n' => buf.push(b'\n'),
                            b'r' => buf.push(b'\r'),
                            b't' => buf.push(b'\t'),
                            b'b' => buf.push(0x08),
                            b'f' => buf.push(0x0c),
                            b'(' => buf.push(b'('),
                            b')' => buf.push(b')'),
                            b'\\' => buf.push(b'\\'),
                            // Line continuation: a backslash before an EOL is elided.
                            b'\n' => {}
                            b'\r' => {
                                if self.data.get(self.pos) == Some(&b'\n') {
                                    self.pos += 1;
                                }
                            }
                            // Octal escape: \ddd (1-3 octal digits).
                            b'0'..=b'7' => {
                                let mut val = (esc - b'0') as u16;
                                for _ in 0..2 {
                                    match self.data.get(self.pos) {
                                        Some(&d @ b'0'..=b'7') => {
                                            val = val * 8 + (d - b'0') as u16;
                                            self.pos += 1;
                                        }
                                        _ => break,
                                    }
                                }
                                buf.push(val as u8);
                            }
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
            if b == b'>' {
                break;
            }
            if is_whitespace(b) {
                continue;
            }
            let nibble = match b {
                b'0'..=b'9' => b - b'0',
                b'a'..=b'f' => b - b'a' + 10,
                b'A'..=b'F' => b - b'A' + 10,
                _ => continue,
            };
            match high {
                None => high = Some(nibble),
                Some(h) => {
                    buf.push((h << 4) | nibble);
                    high = None;
                }
            }
        }
        if let Some(h) = high {
            buf.push(h << 4);
        }
        PdfString::new(buf)
    }

    fn read_array(&mut self) -> Vec<PdfObject> {
        self.pos += 1; // skip '['
        self.depth += 1;
        // Bail out (without recursing) on pathologically deep nesting. `pos` has
        // already advanced past '[', so forward progress is guaranteed.
        if self.depth > MAX_CONTENT_DEPTH {
            self.depth -= 1;
            return Vec::new();
        }
        let mut items = Vec::new();
        loop {
            self.skip_whitespace();
            if self.peek() == Some(b']') {
                self.pos += 1;
                break;
            }
            if self.is_eof() {
                break;
            }
            if let Some(ContentToken::Operand(obj)) = self.next_token() {
                items.push(obj);
            }
        }
        self.depth -= 1;
        items
    }

    fn read_dict(&mut self) -> PdfObject {
        self.pos += 2; // skip '<<'
        self.depth += 1;
        if self.depth > MAX_CONTENT_DEPTH {
            self.depth -= 1;
            return PdfObject::Dict(zpdf_core::PdfDict::new());
        }
        let mut dict = zpdf_core::PdfDict::new();
        loop {
            self.skip_whitespace();
            if self.data.get(self.pos..self.pos + 2) == Some(b">>") {
                self.pos += 2;
                break;
            }
            if self.is_eof() {
                break;
            }
            let key = self.read_name();
            if let Some(ContentToken::Operand(val)) = self.next_token() {
                dict.insert(key, val);
            }
        }
        self.depth -= 1;
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

    /// Parse the body of an inline image after the `BI` operator: the parameter
    /// pairs up to `ID`, then the raw sample bytes up to `EI`.
    fn read_inline_image(&mut self) -> ContentToken {
        let mut dict = zpdf_core::PdfDict::new();

        // Parameter key/value pairs, terminated by the `ID` operator.
        loop {
            self.skip_whitespace();
            if self.is_eof() {
                return ContentToken::InlineImage {
                    dict,
                    data: Vec::new(),
                };
            }
            if self.peek() == Some(b'/') {
                let key = self.read_name();
                self.skip_whitespace();
                if let Some(ContentToken::Operand(val)) = self.next_token() {
                    dict.insert(key, val);
                }
            } else {
                let op = self.read_operator();
                if op == "ID" {
                    break;
                }
                if op.is_empty() {
                    self.pos += 1; // guard against stalling on stray bytes
                } else {
                    // Malformed parameters (no `ID`): recover by skipping past the
                    // `EI` terminator so the binary section is never re-tokenized.
                    let end = self.scan_for_ei(self.pos);
                    self.pos = end;
                    self.skip_whitespace();
                    if self.data[self.pos..].starts_with(b"EI") {
                        self.pos += 2;
                    }
                    return ContentToken::InlineImage {
                        dict,
                        data: Vec::new(),
                    };
                }
            }
        }

        // A single EOL separates `ID` from the binary data; accept CRLF as one unit.
        if self.data[self.pos..].starts_with(b"\r\n") {
            self.pos += 2;
        } else if matches!(self.peek(), Some(b) if is_whitespace(b)) {
            self.pos += 1;
        }

        let data_start = self.pos;
        let data_end = self.inline_image_data_end(&dict, data_start);
        let data = self.data[data_start..data_end.min(self.data.len())].to_vec();
        self.pos = data_end.min(self.data.len());

        // Consume the trailing `EI` marker.
        self.skip_whitespace();
        if self.data[self.pos..].starts_with(b"EI") {
            self.pos += 2;
        }

        ContentToken::InlineImage { dict, data }
    }

    /// Determine where the inline-image sample data ends. For uncompressed data
    /// the exact byte length is computed from W·H·components·bpc (robust against
    /// `EI` byte collisions); otherwise we scan for a whitespace-delimited `EI`.
    fn inline_image_data_end(&self, dict: &zpdf_core::PdfDict, start: usize) -> usize {
        let has_filter = dict.get("Filter").or_else(|| dict.get("F")).is_some();
        if !has_filter {
            let w = dict.get_i64("Width").or_else(|_| dict.get_i64("W"));
            let h = dict.get_i64("Height").or_else(|_| dict.get_i64("H"));
            if let (Ok(w), Ok(h), Some(comps)) = (w, h, inline_image_components(dict)) {
                let is_mask = matches!(
                    dict.get("ImageMask").or_else(|| dict.get("IM")),
                    Some(PdfObject::Bool(true))
                );
                // Image masks are always 1 bit per component regardless of /BPC.
                let bpc = if is_mask {
                    1usize
                } else {
                    dict.get_i64("BitsPerComponent")
                        .or_else(|_| dict.get_i64("BPC"))
                        .unwrap_or(8)
                        .clamp(1, 16) as usize
                };
                // Checked arithmetic: dimensions are attacker-controlled and must
                // not overflow (debug panic / release wrap).
                let len = (w.max(0) as usize)
                    .checked_mul(comps)
                    .and_then(|x| x.checked_mul(bpc))
                    .map(|bits| bits.saturating_add(7) / 8)
                    .and_then(|row| row.checked_mul(h.max(0) as usize));
                if let Some(len) = len {
                    if let Some(end) = start.checked_add(len) {
                        if end <= self.data.len() {
                            return end;
                        }
                    }
                }
            }
        }
        self.scan_for_ei(start)
    }

    fn scan_for_ei(&self, start: usize) -> usize {
        let data = self.data;
        let mut i = start;
        while i + 2 <= data.len() {
            if &data[i..i + 2] == b"EI" {
                let prev_ws = i == start || is_whitespace(data[i - 1]);
                let next_ok =
                    i + 2 >= data.len() || is_whitespace(data[i + 2]) || is_delimiter(data[i + 2]);
                if prev_ws && next_ok {
                    let mut end = i;
                    if end > start && is_whitespace(data[end - 1]) {
                        end -= 1;
                    }
                    return end;
                }
            }
            i += 1;
        }
        data.len()
    }
}

/// Components per sample for an inline image, from its (raw) dict; `None` when the
/// colour space is an array/unknown (caller falls back to scanning).
fn inline_image_components(dict: &zpdf_core::PdfDict) -> Option<usize> {
    if matches!(
        dict.get("ImageMask").or_else(|| dict.get("IM")),
        Some(PdfObject::Bool(true))
    ) {
        return Some(1);
    }
    match dict.get("ColorSpace").or_else(|| dict.get("CS")) {
        Some(PdfObject::Name(n)) => Some(match n.as_str() {
            "DeviceRGB" | "RGB" | "CalRGB" => 3,
            "DeviceCMYK" | "CMYK" => 4,
            "DeviceGray" | "G" | "CalGray" | "Indexed" | "I" => 1,
            _ => return None,
        }),
        // Array color space, e.g. [/Indexed base hival lut] — one index per sample.
        Some(PdfObject::Array(arr)) => match arr.first() {
            Some(PdfObject::Name(n)) if matches!(n.as_str(), "Indexed" | "I") => Some(1),
            _ => None,
        },
        _ => None,
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
    matches!(
        b,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deeply_nested_array_does_not_overflow() {
        // 100k unbalanced '[' would blow the native stack with unguarded
        // recursion; the depth cap must keep it bounded and terminating.
        let data = vec![b'['; 100_000];
        let mut tk = ContentTokenizer::new(&data);
        let mut tokens = 0u32;
        while tk.next_token().is_some() {
            tokens += 1;
            if tokens > 200_000 {
                break; // safety: must terminate well before this
            }
        }
        // Reaching here without a stack overflow is the assertion.
    }

    #[test]
    fn deeply_nested_dict_does_not_overflow() {
        let mut data = Vec::new();
        for _ in 0..100_000 {
            data.extend_from_slice(b"<<");
        }
        let mut tk = ContentTokenizer::new(&data);
        let _ = tk.next_token();
    }

    #[test]
    fn tokenize_simple_content() {
        let data = b"BT /F1 12 Tf (Hello) Tj ET";
        let tokens: Vec<_> = ContentTokenizer::new(data).collect();
        assert!(matches!(&tokens[0], ContentToken::Operator(s) if s == "BT"));
        assert!(
            matches!(&tokens[1], ContentToken::Operand(PdfObject::Name(n)) if n.as_str() == "F1")
        );
        assert!(matches!(
            &tokens[2],
            ContentToken::Operand(PdfObject::Integer(12))
        ));
        assert!(matches!(&tokens[3], ContentToken::Operator(s) if s == "Tf"));
    }

    #[test]
    fn literal_string_octal_and_continuation() {
        // \101 = octal 0o101 = 'A'; \\ -> backslash; backslash-newline elided.
        let data = b"(\\101\\\\B\\\n C) Tj";
        let tokens: Vec<_> = ContentTokenizer::new(data).collect();
        match &tokens[0] {
            ContentToken::Operand(PdfObject::String(s)) => {
                assert_eq!(s.0, b"A\\B C");
            }
            other => panic!("expected string, got {other:?}"),
        }
    }

    #[test]
    fn tokenize_inline_image() {
        // 2x2, 1-component (gray), 8bpc, uncompressed => exactly 4 data bytes.
        let data = b"q BI /W 2 /H 2 /CS /G /BPC 8 ID \x00\xFF\xFF\x00 EI Q";
        let tokens: Vec<_> = ContentTokenizer::new(data).collect();
        assert_eq!(tokens.len(), 3, "expected q, InlineImage, Q");
        match &tokens[1] {
            ContentToken::InlineImage { dict, data } => {
                assert_eq!(dict.get_i64("W").unwrap(), 2);
                assert_eq!(dict.get_i64("H").unwrap(), 2);
                assert_eq!(data.as_slice(), &[0x00, 0xFF, 0xFF, 0x00]);
            }
            other => panic!("expected inline image, got {other:?}"),
        }
        assert!(matches!(&tokens[2], ContentToken::Operator(s) if s == "Q"));
    }

    #[test]
    fn inline_image_mask_defaults_bpc_1() {
        // 8x1 image mask, no /BPC => 1 bit per sample => (8+7)/8 = 1 data byte.
        let data = b"BI /W 8 /H 1 /IM true ID \xAA EI";
        let tokens: Vec<_> = ContentTokenizer::new(data).collect();
        match &tokens[0] {
            ContentToken::InlineImage { data, .. } => assert_eq!(data.as_slice(), &[0xAA]),
            other => panic!("expected inline image, got {other:?}"),
        }
    }

    #[test]
    fn inline_image_crlf_after_id() {
        // ID terminated by CRLF: the leading \n must not bleed into the data.
        let mut data = b"BI /W 2 /H 1 /CS /G /BPC 8 ID\r\n".to_vec();
        data.extend_from_slice(&[0x11, 0x22]);
        data.extend_from_slice(b"\nEI");
        let tokens: Vec<_> = ContentTokenizer::new(&data).collect();
        match &tokens[0] {
            ContentToken::InlineImage { data, .. } => assert_eq!(data.as_slice(), &[0x11, 0x22]),
            other => panic!("expected inline image, got {other:?}"),
        }
    }

    #[test]
    fn tokenize_path() {
        let data = b"100 200 m 300 400 l S";
        let tokens: Vec<_> = ContentTokenizer::new(data).collect();
        // 100, 200, m, 300, 400, l, S = 7 tokens
        assert_eq!(tokens.len(), 7);
    }
}
