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
/// Aggregate number of objects retained inside array/dictionary operands. This
/// closes the gap left by an operand-count limit: one operand can otherwise be
/// an array containing millions of heap-allocated objects.
const MAX_RETAINED_OBJECTS: usize = 100_000;
/// Names and operators are identifiers, not bulk data. Bound their owned copy
/// while still scanning the full token so malformed input always advances.
const MAX_CONTENT_NAME_BYTES: usize = 64 * 1024;
const MAX_CONTENT_OPERATOR_BYTES: usize = 256;
const MAX_CONTENT_STRING_BYTES: usize = 16 * 1024 * 1024;

/// Tokenizer for PDF content streams.
/// Content streams contain sequences of: operand* operator
pub struct ContentTokenizer<'a> {
    data: &'a [u8],
    pos: usize,
    /// Current array/dict nesting depth (see `MAX_CONTENT_DEPTH`).
    depth: u32,
    /// Objects retained by nested arrays/dicts over this tokenizer's lifetime.
    retained_objects: usize,
}

impl<'a> ContentTokenizer<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            depth: 0,
            retained_objects: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.data.get(self.pos).copied()
    }

    fn is_eof(&self) -> bool {
        self.pos >= self.data.len()
    }

    fn starts_keyword(&self, keyword: &[u8]) -> bool {
        let Some(rest) = self.data.get(self.pos..) else {
            return false;
        };
        rest.starts_with(keyword)
            && rest
                .get(keyword.len())
                .is_none_or(|&b| is_whitespace(b) || is_delimiter(b))
    }

    fn retain_slot(&mut self) -> bool {
        if self.retained_objects >= MAX_RETAINED_OBJECTS {
            return false;
        }
        self.retained_objects += 1;
        true
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
            b't' if self.starts_keyword(b"true") => {
                self.pos += 4;
                Some(ContentToken::Operand(PdfObject::Bool(true)))
            }
            b'f' if self.starts_keyword(b"false") => {
                self.pos += 5;
                Some(ContentToken::Operand(PdfObject::Bool(false)))
            }
            b'n' if self.starts_keyword(b"null") => {
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
            PdfObject::Real(
                s.parse::<f64>()
                    .ok()
                    .filter(|n| n.is_finite())
                    .unwrap_or(0.0),
            )
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
        PdfName::new(decode_name(&raw[..raw.len().min(MAX_CONTENT_NAME_BYTES)]))
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
                    push_string_byte(&mut buf, b'(');
                }
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    push_string_byte(&mut buf, b')');
                }
                b'\\' => {
                    if let Some(&esc) = self.data.get(self.pos) {
                        self.pos += 1;
                        match esc {
                            b'n' => push_string_byte(&mut buf, b'\n'),
                            b'r' => push_string_byte(&mut buf, b'\r'),
                            b't' => push_string_byte(&mut buf, b'\t'),
                            b'b' => push_string_byte(&mut buf, 0x08),
                            b'f' => push_string_byte(&mut buf, 0x0c),
                            b'(' => push_string_byte(&mut buf, b'('),
                            b')' => push_string_byte(&mut buf, b')'),
                            b'\\' => push_string_byte(&mut buf, b'\\'),
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
                                push_string_byte(&mut buf, val as u8);
                            }
                            _ => push_string_byte(&mut buf, esc),
                        }
                    }
                }
                _ => push_string_byte(&mut buf, b),
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
                    push_string_byte(&mut buf, (h << 4) | nibble);
                    high = None;
                }
            }
        }
        if let Some(h) = high {
            push_string_byte(&mut buf, h << 4);
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
                if self.retain_slot() {
                    items.push(obj);
                }
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
            if self.peek() != Some(b'/') {
                // A dictionary key must be a name. Consume one malformed token
                // and keep looking instead of manufacturing a key from it.
                let before = self.pos;
                let _ = self.next_token();
                if self.pos == before {
                    self.pos += 1;
                }
                continue;
            }
            let key = self.read_name();
            if let Some(ContentToken::Operand(val)) = self.next_token() {
                if self.retain_slot() {
                    dict.insert(key, val);
                }
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
        let end = start
            .saturating_add(MAX_CONTENT_OPERATOR_BYTES)
            .min(self.pos);
        String::from_utf8_lossy(&self.data[start..end]).into_owned()
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
                    if self.retain_slot() {
                        dict.insert(key, val);
                    }
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
        loop {
            let before = self.pos;
            if let Some(token) = self.next_token() {
                return Some(token);
            }
            if self.is_eof() {
                return None;
            }
            if self.pos == before {
                self.pos += 1;
            }
        }
    }
}

fn decode_name(raw: &[u8]) -> String {
    let mut decoded = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'#' && i + 2 < raw.len() {
            if let (Some(high), Some(low)) = (hex_digit(raw[i + 1]), hex_digit(raw[i + 2])) {
                decoded.push((high << 4) | low);
                i += 3;
                continue;
            }
        }
        decoded.push(raw[i]);
        i += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn push_string_byte(buffer: &mut Vec<u8>, byte: u8) {
    if buffer.len() < MAX_CONTENT_STRING_BYTES {
        buffer.push(byte);
    }
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
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

    #[test]
    fn iterator_recovers_after_stray_bytes() {
        let tokens: Vec<_> = ContentTokenizer::new(b"q @ Q").collect();
        assert_eq!(tokens.len(), 2);
        assert!(matches!(&tokens[0], ContentToken::Operator(op) if op == "q"));
        assert!(matches!(&tokens[1], ContentToken::Operator(op) if op == "Q"));
    }

    #[test]
    fn keywords_require_token_boundaries() {
        let tokens: Vec<_> = ContentTokenizer::new(b"truecolor falsehood nullify").collect();
        assert!(tokens
            .iter()
            .all(|t| matches!(t, ContentToken::Operator(_))));
    }

    #[test]
    fn name_hex_escapes_are_decoded() {
        let tokens: Vec<_> = ContentTokenizer::new(b"/F#31 12 Tf").collect();
        assert!(matches!(
            &tokens[0],
            ContentToken::Operand(PdfObject::Name(name)) if name.as_str() == "F1"
        ));
    }

    #[test]
    fn overflowing_real_degrades_to_zero() {
        let data = format!("{}.0", "9".repeat(400));
        let token = ContentTokenizer::new(data.as_bytes()).next().unwrap();
        assert!(matches!(token, ContentToken::Operand(PdfObject::Real(0.0))));
    }

    #[test]
    fn nested_operand_retention_is_bounded() {
        let mut data = Vec::with_capacity((MAX_RETAINED_OBJECTS + 10) * 2 + 2);
        data.push(b'[');
        for _ in 0..MAX_RETAINED_OBJECTS + 10 {
            data.extend_from_slice(b"0 ");
        }
        data.push(b']');
        let token = ContentTokenizer::new(&data).next().unwrap();
        let ContentToken::Operand(PdfObject::Array(items)) = token else {
            panic!("expected array operand");
        };
        assert_eq!(items.len(), MAX_RETAINED_OBJECTS);
    }
}
