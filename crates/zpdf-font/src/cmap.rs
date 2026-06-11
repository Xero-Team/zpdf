//! Parsing of PDF `/ToUnicode` CMap streams into a code -> Unicode-string map.
//!
//! A ToUnicode CMap (PDF 32000-1:2008 §9.10.3) maps character codes used in
//! text-showing operators to Unicode values, so extracted text can be turned
//! into a real string. The relevant CMap constructs are:
//!
//! - `begincodespacerange` / `endcodespacerange`: declares the byte length(s)
//!   of source codes via `<lo> <hi>` pairs.
//! - `beginbfchar` / `endbfchar`: single `<src> <dst>` mappings.
//! - `beginbfrange` / `endbfrange`: `<lo> <hi> <dst>` ranges, where `<dst>` is
//!   either a starting hex string (incremented per code) or an array of hex
//!   strings (one per code).
//!
//! Destination strings are encoded as UTF-16BE, including surrogate pairs.
//!
//! This module is self-contained (std only) and is designed never to panic on
//! malformed input — every byte access is bounds-checked and unparsable tokens
//! are skipped.

use std::collections::HashMap;

/// A parsed `/ToUnicode` CMap: maps a character code to its Unicode string.
#[derive(Debug, Clone, Default)]
pub struct ToUnicodeMap {
    map: HashMap<u32, String>,
    /// Distinct source-code byte lengths declared in codespacerange.
    code_lengths: Vec<u8>,
}

impl ToUnicodeMap {
    /// Parse a decoded ToUnicode CMap.
    ///
    /// `data` is the decoded stream content (the bytes between/inside
    /// `begincmap`..`endcmap`; the surrounding keywords may also be present and
    /// are simply ignored). This is robust to malformed input and never panics.
    pub fn parse(data: &[u8]) -> Self {
        let mut result = ToUnicodeMap::default();
        let tokens = tokenize(data);
        let mut i = 0usize;

        while i < tokens.len() {
            match &tokens[i] {
                Token::Keyword(kw) if kw == b"begincodespacerange" => {
                    i += 1;
                    i = result.parse_codespacerange(&tokens, i);
                }
                Token::Keyword(kw) if kw == b"beginbfchar" => {
                    i += 1;
                    i = result.parse_bfchar(&tokens, i);
                }
                Token::Keyword(kw) if kw == b"beginbfrange" => {
                    i += 1;
                    i = result.parse_bfrange(&tokens, i);
                }
                _ => {
                    i += 1;
                }
            }
        }

        result
    }

    /// Look up the Unicode string mapped to a character code.
    pub fn lookup(&self, code: u32) -> Option<&str> {
        self.map.get(&code).map(|s| s.as_str())
    }

    /// Returns true if no code mappings were parsed.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Iterate over all (code, unicode string) mappings.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &str)> {
        self.map.iter().map(|(&code, s)| (code, s.as_str()))
    }

    /// Distinct source-code byte lengths seen in `codespacerange` (e.g. `[2]`).
    ///
    /// Defaults to `[2]` if none were declared, since two-byte codes are by far
    /// the most common case for ToUnicode CMaps (used with Type0 fonts).
    pub fn code_byte_lengths(&self) -> &[u8] {
        if self.code_lengths.is_empty() {
            &[2]
        } else {
            &self.code_lengths
        }
    }

    fn record_code_length(&mut self, len: u8) {
        if len > 0 && !self.code_lengths.contains(&len) {
            self.code_lengths.push(len);
        }
    }

    /// Parse the body of a `codespacerange` block, returning the index just past
    /// `endcodespacerange` (or the end of the token stream).
    fn parse_codespacerange(&mut self, tokens: &[Token], start: usize) -> usize {
        let mut i = start;
        while i < tokens.len() {
            match &tokens[i] {
                Token::Keyword(kw) if kw == b"endcodespacerange" => {
                    return i + 1;
                }
                Token::HexString(lo) => {
                    // Expect a matching <hi>; record the byte length of <lo>.
                    self.record_code_length(lo.len() as u8);
                    i += 1;
                    // Skip the high bound (also a hex string) if present.
                    if let Some(Token::HexString(_)) = tokens.get(i) {
                        i += 1;
                    }
                }
                Token::Keyword(_) => {
                    // Some other keyword — bail out of this block.
                    return i;
                }
                _ => {
                    i += 1;
                }
            }
        }
        i
    }

    /// Parse the body of a `bfchar` block.
    fn parse_bfchar(&mut self, tokens: &[Token], start: usize) -> usize {
        let mut i = start;
        while i < tokens.len() {
            match &tokens[i] {
                Token::Keyword(kw) if kw == b"endbfchar" => {
                    return i + 1;
                }
                Token::HexString(src) => {
                    let code = bytes_to_code(src);
                    i += 1;
                    // The destination follows; it is usually a hex string, but
                    // per spec may be a name (which we ignore).
                    match tokens.get(i) {
                        Some(Token::HexString(dst)) => {
                            if let Some(s) = utf16be_to_string(dst) {
                                self.map.insert(code, s);
                            }
                            i += 1;
                        }
                        Some(Token::Name(_)) => {
                            i += 1; // ignore named destinations
                        }
                        _ => { /* malformed: leave pointer, loop re-examines */ }
                    }
                }
                Token::Keyword(_) => {
                    return i;
                }
                _ => {
                    i += 1;
                }
            }
        }
        i
    }

    /// Parse the body of a `bfrange` block.
    fn parse_bfrange(&mut self, tokens: &[Token], start: usize) -> usize {
        let mut i = start;
        while i < tokens.len() {
            match &tokens[i] {
                Token::Keyword(kw) if kw == b"endbfrange" => {
                    return i + 1;
                }
                Token::HexString(lo) => {
                    let lo_code = bytes_to_code(lo);
                    i += 1;
                    // Expect <hi>.
                    let hi_code = match tokens.get(i) {
                        Some(Token::HexString(hi)) => {
                            let v = bytes_to_code(hi);
                            i += 1;
                            v
                        }
                        _ => {
                            // Malformed triple; abort the block.
                            return i;
                        }
                    };

                    // Destination: either a hex string (start value) or an array.
                    match tokens.get(i) {
                        Some(Token::HexString(dst)) => {
                            self.assign_bfrange_incrementing(lo_code, hi_code, dst);
                            i += 1;
                        }
                        Some(Token::ArrayStart) => {
                            i += 1;
                            let mut code = lo_code;
                            while i < tokens.len() {
                                match &tokens[i] {
                                    Token::ArrayEnd => {
                                        i += 1;
                                        break;
                                    }
                                    Token::HexString(dst) => {
                                        if code <= hi_code {
                                            if let Some(s) = utf16be_to_string(dst) {
                                                self.map.insert(code, s);
                                            }
                                        }
                                        code = code.wrapping_add(1);
                                        i += 1;
                                    }
                                    Token::Name(_) => {
                                        // Named entry inside array: ignore but
                                        // still consume a code slot.
                                        code = code.wrapping_add(1);
                                        i += 1;
                                    }
                                    _ => {
                                        i += 1;
                                    }
                                }
                            }
                        }
                        _ => {
                            // Malformed destination; abort.
                            return i;
                        }
                    }
                }
                Token::Keyword(_) => {
                    return i;
                }
                _ => {
                    i += 1;
                }
            }
        }
        i
    }

    /// For a `<lo> <hi> <dst>` range with a hex-string destination, assign the
    /// destination to `lo`, then increment the *last UTF-16 code unit* by one for
    /// each subsequent code (PDF 32000-1:2008 §9.10.3).
    fn assign_bfrange_incrementing(&mut self, lo: u32, hi: u32, dst: &[u8]) {
        // Work on the destination as UTF-16BE code units.
        let mut units = bytes_to_u16be(dst);
        if units.is_empty() {
            return;
        }
        if lo > hi {
            return;
        }
        // Guard against pathological ranges.
        let count = (hi - lo) as u64 + 1;
        let limit = count.min(0x1_0000) as u32; // at most 65536 codes per range

        for n in 0..limit {
            let code = lo + n;
            if let Some(s) = units_to_string(&units) {
                self.map.insert(code, s);
            }
            // Increment the last code unit, carrying upward if it overflows.
            increment_last_unit(&mut units);
        }
    }
}

// ---------------------------------------------------------------------------
// Code → CID CMaps (composite-font /Encoding)
// ---------------------------------------------------------------------------

/// A code → CID CMap for composite (Type0) fonts: embedded CMap streams plus
/// the predefined Identity and Unicode (UCS-2/UTF-16) families. Legacy
/// byte-encoded predefined CMaps (RKSJ, EUC, Big5, GBK…) are not bundled;
/// callers fall back to Identity with a warning.
#[derive(Debug, Clone, Default)]
pub struct CidCMap {
    /// Codespace ranges as (byte_len, lo, hi).
    codespace: Vec<(u8, u32, u32)>,
    /// cidrange entries as (byte_len, lo, hi, first_cid).
    cid_ranges: Vec<(u8, u32, u32, u32)>,
    /// cidchar singles (keyed by code).
    cid_chars: HashMap<u32, u32>,
    /// Writing mode: 0 horizontal, 1 vertical.
    pub wmode: u8,
    /// Codes are UTF-16BE/UCS-2 Unicode (the predefined UniXX-UCS2/UTF16
    /// families): glyphs resolve via the font's Unicode cmap, not CID → GID.
    pub codes_are_unicode: bool,
    /// CID = code (the Identity family; also the lenient fallback).
    pub identity: bool,
}

impl CidCMap {
    pub fn identity(wmode: u8) -> Self {
        Self {
            wmode,
            identity: true,
            ..Default::default()
        }
    }

    /// Resolve a predefined CMap by name. Identity and the Unicode families
    /// are supported; `None` means the (legacy byte-encoded) CMap is unknown.
    pub fn predefined(name: &str) -> Option<Self> {
        let wmode = if name.ends_with("-V") { 1 } else { 0 };
        if name == "Identity-H" || name == "Identity-V" {
            return Some(Self::identity(wmode));
        }
        let unicode_family = (name.starts_with("UniGB")
            || name.starts_with("UniCNS")
            || name.starts_with("UniJIS")
            || name.starts_with("UniKS")
            || name.starts_with("UniAKR"))
            && (name.contains("UCS2") || name.contains("UTF16"));
        if unicode_family {
            return Some(Self {
                wmode,
                codes_are_unicode: true,
                ..Default::default()
            });
        }
        None
    }

    /// Parse an embedded CMap stream: codespacerange, cidrange, cidchar,
    /// /WMode, and `usecmap` (Identity bases only). Never panics.
    pub fn parse(data: &[u8]) -> Self {
        let mut cmap = CidCMap::default();
        let tokens = tokenize(data);
        let mut i = 0usize;

        while i < tokens.len() {
            match &tokens[i] {
                Token::Keyword(kw) if kw == b"begincodespacerange" => {
                    i += 1;
                    while i + 1 < tokens.len() {
                        match (&tokens[i], &tokens[i + 1]) {
                            (Token::HexString(lo), Token::HexString(hi)) => {
                                let len = lo.len().clamp(1, 4) as u8;
                                cmap.codespace
                                    .push((len, bytes_to_code(lo), bytes_to_code(hi)));
                                i += 2;
                            }
                            _ => break,
                        }
                    }
                }
                Token::Keyword(kw) if kw == b"begincidrange" => {
                    i += 1;
                    while i + 2 < tokens.len() {
                        match (&tokens[i], &tokens[i + 1], &tokens[i + 2]) {
                            (
                                Token::HexString(lo),
                                Token::HexString(hi),
                                Token::Number(cid),
                            ) => {
                                let len = lo.len().clamp(1, 4) as u8;
                                let first = ascii_to_u32(cid);
                                cmap.cid_ranges.push((
                                    len,
                                    bytes_to_code(lo),
                                    bytes_to_code(hi),
                                    first,
                                ));
                                i += 3;
                            }
                            _ => break,
                        }
                    }
                }
                Token::Keyword(kw) if kw == b"begincidchar" => {
                    i += 1;
                    while i + 1 < tokens.len() {
                        match (&tokens[i], &tokens[i + 1]) {
                            (Token::HexString(code), Token::Number(cid)) => {
                                cmap.cid_chars
                                    .insert(bytes_to_code(code), ascii_to_u32(cid));
                                i += 2;
                            }
                            _ => break,
                        }
                    }
                }
                Token::Keyword(kw) if kw == b"usecmap" => {
                    // `/Base-CMap usecmap` (possibly with `findresource` in
                    // between) — honor Identity/Unicode bases, ignore the rest.
                    let base = tokens[..i].iter().rev().take(3).find_map(|t| match t {
                        Token::Name(n) => Self::predefined(&String::from_utf8_lossy(n)),
                        _ => None,
                    });
                    if let Some(base) = base {
                        cmap.identity |= base.identity;
                        cmap.codes_are_unicode |= base.codes_are_unicode;
                    }
                    i += 1;
                }
                Token::Name(n) if n == b"WMode" => {
                    if let Some(Token::Number(v)) = tokens.get(i + 1) {
                        cmap.wmode = (ascii_to_u32(v) == 1) as u8;
                        i += 1;
                    }
                    i += 1;
                }
                _ => {
                    i += 1;
                }
            }
        }

        // A CMap with no mappings at all degrades to Identity (lenient).
        if cmap.cid_ranges.is_empty() && cmap.cid_chars.is_empty() && !cmap.codes_are_unicode {
            cmap.identity = true;
        }
        cmap
    }

    /// Split the next code off `bytes` per the codespace. Returns
    /// `(code, byte_len)`; with no codespace declared, codes are 2 bytes
    /// (the Identity/UCS-2 convention).
    pub fn next_code(&self, bytes: &[u8]) -> (u32, usize) {
        if bytes.is_empty() {
            return (0, 1);
        }
        if self.codespace.is_empty() {
            let len = bytes.len().min(2);
            return (bytes_to_code(&bytes[..len]), len);
        }
        // Match the shortest declared range containing the prefix value.
        for want in 1..=4usize {
            if bytes.len() < want {
                break;
            }
            let v = bytes_to_code(&bytes[..want]);
            for &(len, lo, hi) in &self.codespace {
                if len as usize == want && v >= lo && v <= hi {
                    return (v, want);
                }
            }
        }
        // No range matched: consume the shortest declared length.
        let min_len = self
            .codespace
            .iter()
            .map(|&(l, _, _)| l as usize)
            .min()
            .unwrap_or(2)
            .min(bytes.len());
        (bytes_to_code(&bytes[..min_len.max(1)]), min_len.max(1))
    }

    /// Map a code (with its byte length) to a CID.
    pub fn code_to_cid(&self, code: u32, len: u8) -> u32 {
        if let Some(&cid) = self.cid_chars.get(&code) {
            return cid;
        }
        for &(rlen, lo, hi, first) in &self.cid_ranges {
            if rlen == len && code >= lo && code <= hi {
                return first + (code - lo);
            }
        }
        if self.identity {
            code
        } else {
            0
        }
    }
}

/// Parse ASCII decimal bytes to u32 (CID operands in cidrange/cidchar).
fn ascii_to_u32(bytes: &[u8]) -> u32 {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .map(|v| v.max(0.0) as u32)
        .unwrap_or(0)
}

/// Increment the last UTF-16 code unit by one, propagating a carry to earlier
/// units on overflow (matching how viewers treat multi-unit bfrange starts).
fn increment_last_unit(units: &mut [u16]) {
    let mut idx = units.len();
    while idx > 0 {
        idx -= 1;
        let (v, carry) = units[idx].overflowing_add(1);
        units[idx] = v;
        if !carry {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Token {
    /// `<...>` — raw decoded bytes from a hex string.
    HexString(Vec<u8>),
    /// `/name` — the name without the leading slash.
    Name(Vec<u8>),
    /// A bareword (keyword) such as `beginbfchar`.
    Keyword(Vec<u8>),
    /// A numeric token (we keep the raw bytes; value rarely needed here).
    Number(Vec<u8>),
    /// `[`
    ArrayStart,
    /// `]`
    ArrayEnd,
}

fn is_pdf_whitespace(b: u8) -> bool {
    matches!(b, b'\0' | b'\t' | b'\n' | 0x0c | b'\r' | b' ')
}

fn is_pdf_delimiter(b: u8) -> bool {
    matches!(
        b,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

/// Convert one ASCII hex digit to its value, or `None` if not a hex digit.
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Tokenize the CMap byte stream into a flat token vector.
fn tokenize(data: &[u8]) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut i = 0usize;
    let n = data.len();

    while i < n {
        let b = data[i];

        if is_pdf_whitespace(b) {
            i += 1;
            continue;
        }

        match b {
            b'%' => {
                // Comment: skip to end of line.
                i += 1;
                while i < n && data[i] != b'\n' && data[i] != b'\r' {
                    i += 1;
                }
            }
            b'<' => {
                // Could be a hex string `<...>` or a dictionary `<<`.
                if i + 1 < n && data[i + 1] == b'<' {
                    // Dictionary open — emit nothing meaningful, skip both.
                    i += 2;
                    continue;
                }
                let (bytes, next) = read_hex_string(data, i + 1);
                tokens.push(Token::HexString(bytes));
                i = next;
            }
            b'>' => {
                // Stray `>` or dictionary close `>>` — just skip.
                if i + 1 < n && data[i + 1] == b'>' {
                    i += 2;
                } else {
                    i += 1;
                }
            }
            b'(' => {
                // Literal string — read and discard (not used by ToUnicode
                // constructs, but must be consumed to stay in sync).
                let next = skip_literal_string(data, i + 1);
                i = next;
            }
            b'[' => {
                tokens.push(Token::ArrayStart);
                i += 1;
            }
            b']' => {
                tokens.push(Token::ArrayEnd);
                i += 1;
            }
            b'/' => {
                let (name, next) = read_name(data, i + 1);
                tokens.push(Token::Name(name));
                i = next;
            }
            b'{' | b'}' => {
                // Procedure delimiters (used by some CMaps) — skip.
                i += 1;
            }
            _ => {
                // Number or bareword keyword.
                let start = i;
                while i < n && !is_pdf_whitespace(data[i]) && !is_pdf_delimiter(data[i]) {
                    i += 1;
                }
                let word = &data[start..i];
                if word.is_empty() {
                    i += 1; // defensive: never get stuck
                } else if is_number(word) {
                    tokens.push(Token::Number(word.to_vec()));
                } else {
                    tokens.push(Token::Keyword(word.to_vec()));
                }
            }
        }
    }

    tokens
}

/// Read a hex string body starting just after the opening `<`.
/// Returns the decoded bytes and the index just past the closing `>`.
fn read_hex_string(data: &[u8], start: usize) -> (Vec<u8>, usize) {
    let n = data.len();
    let mut i = start;
    let mut nibbles: Vec<u8> = Vec::new();

    while i < n {
        let b = data[i];
        if b == b'>' {
            i += 1;
            break;
        }
        if is_pdf_whitespace(b) {
            i += 1;
            continue;
        }
        if let Some(v) = hex_val(b) {
            nibbles.push(v);
        }
        // Non-hex, non-ws chars inside `<...>` are ignored defensively.
        i += 1;
    }

    // Per PDF spec, an odd final nibble is padded with a trailing 0.
    if nibbles.len() % 2 == 1 {
        nibbles.push(0);
    }

    let mut bytes = Vec::with_capacity(nibbles.len() / 2);
    let mut k = 0;
    while k + 1 < nibbles.len() {
        bytes.push((nibbles[k] << 4) | nibbles[k + 1]);
        k += 2;
    }
    // (k == nibbles.len() exactly because length is even.)

    (bytes, i)
}

/// Skip a literal `(...)` string, honoring backslash escapes and nested parens.
/// `start` is just after the opening `(`. Returns index past the closing `)`.
fn skip_literal_string(data: &[u8], start: usize) -> usize {
    let n = data.len();
    let mut i = start;
    let mut depth = 1i32;

    while i < n {
        let b = data[i];
        match b {
            b'\\' => {
                // Skip the escaped character (if any).
                i += 2;
            }
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth -= 1;
                i += 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {
                i += 1;
            }
        }
    }
    i
}

/// Read a name body starting just after the `/`.
/// Returns the name bytes (with `#xx` hex escapes decoded) and the next index.
fn read_name(data: &[u8], start: usize) -> (Vec<u8>, usize) {
    let n = data.len();
    let mut i = start;
    let mut out = Vec::new();

    while i < n {
        let b = data[i];
        if is_pdf_whitespace(b) || is_pdf_delimiter(b) {
            break;
        }
        if b == b'#' && i + 2 < n {
            if let (Some(h), Some(l)) = (hex_val(data[i + 1]), hex_val(data[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(b);
        i += 1;
    }

    (out, i)
}

/// Heuristic: does this bareword look like a PDF number?
fn is_number(word: &[u8]) -> bool {
    let mut seen_digit = false;
    for (idx, &b) in word.iter().enumerate() {
        match b {
            b'0'..=b'9' => seen_digit = true,
            b'+' | b'-' if idx == 0 => {}
            b'.' => {}
            _ => return false,
        }
    }
    seen_digit
}

// ---------------------------------------------------------------------------
// Numeric / Unicode helpers
// ---------------------------------------------------------------------------

/// Interpret raw bytes as a big-endian unsigned integer (source character code).
fn bytes_to_code(bytes: &[u8]) -> u32 {
    let mut code: u32 = 0;
    // Use at most the last 4 bytes to stay within u32.
    let take = bytes.len().min(4);
    let skip = bytes.len() - take;
    for &b in &bytes[skip..] {
        code = (code << 8) | b as u32;
    }
    code
}

/// Reinterpret a byte buffer as a sequence of big-endian u16 code units.
/// A trailing odd byte is treated as the high byte of a final unit.
fn bytes_to_u16be(bytes: &[u8]) -> Vec<u16> {
    let mut units = Vec::with_capacity(bytes.len() / 2 + 1);
    let mut i = 0;
    while i + 1 < bytes.len() {
        units.push(((bytes[i] as u16) << 8) | bytes[i + 1] as u16);
        i += 2;
    }
    if i < bytes.len() {
        units.push((bytes[i] as u16) << 8);
    }
    units
}

/// Decode UTF-16BE bytes (with surrogate pairs) into a `String`.
/// Returns `None` only if the input has no usable code units.
fn utf16be_to_string(bytes: &[u8]) -> Option<String> {
    let units = bytes_to_u16be(bytes);
    units_to_string(&units)
}

/// Decode a slice of UTF-16 code units (with surrogate pairs) into a `String`.
/// Unpaired surrogates are emitted as U+FFFD so we never lose sync or panic.
fn units_to_string(units: &[u16]) -> Option<String> {
    if units.is_empty() {
        return None;
    }
    let mut s = String::with_capacity(units.len());
    let mut i = 0;
    while i < units.len() {
        let u = units[i];
        if (0xD800..=0xDBFF).contains(&u) {
            // High surrogate — needs a following low surrogate.
            if i + 1 < units.len() {
                let lo = units[i + 1];
                if (0xDC00..=0xDFFF).contains(&lo) {
                    let high = (u as u32 - 0xD800) << 10;
                    let low = lo as u32 - 0xDC00;
                    let cp = 0x1_0000 + high + low;
                    if let Some(c) = char::from_u32(cp) {
                        s.push(c);
                    } else {
                        s.push('\u{FFFD}');
                    }
                    i += 2;
                    continue;
                }
            }
            // Unpaired high surrogate.
            s.push('\u{FFFD}');
            i += 1;
        } else if (0xDC00..=0xDFFF).contains(&u) {
            // Unpaired low surrogate.
            s.push('\u{FFFD}');
            i += 1;
        } else {
            s.push(char::from_u32(u as u32).unwrap_or('\u{FFFD}'));
            i += 1;
        }
    }
    Some(s)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bfchar_basic_ascii() {
        let data = b"\
            begincmap\n\
            beginbfchar\n\
            <0041> <0041>\n\
            endbfchar\n\
            endcmap\n";
        let m = ToUnicodeMap::parse(data);
        assert_eq!(m.lookup(0x41), Some("A"));
        assert!(!m.is_empty());
    }

    #[test]
    fn bfrange_incrementing_digits() {
        let data = b"\
            beginbfrange\n\
            <0030> <0039> <0030>\n\
            endbfrange\n";
        let m = ToUnicodeMap::parse(data);
        assert_eq!(m.lookup(0x30), Some("0"));
        assert_eq!(m.lookup(0x35), Some("5"));
        assert_eq!(m.lookup(0x39), Some("9"));
        // Outside the range is unmapped.
        assert_eq!(m.lookup(0x40), None);
    }

    #[test]
    fn bfrange_array_destinations() {
        let data = b"\
            beginbfrange\n\
            <0001> <0002> [<0041> <0042>]\n\
            endbfrange\n";
        let m = ToUnicodeMap::parse(data);
        assert_eq!(m.lookup(1), Some("A"));
        assert_eq!(m.lookup(2), Some("B"));
    }

    #[test]
    fn bfchar_surrogate_pair() {
        // U+1F600 GRINNING FACE = UTF-16BE D83D DE00
        let data = b"\
            beginbfchar\n\
            <0001> <D83DDE00>\n\
            endbfchar\n";
        let m = ToUnicodeMap::parse(data);
        assert_eq!(m.lookup(1), Some("\u{1F600}"));
    }

    #[test]
    fn codespacerange_two_byte() {
        let data = b"\
            begincodespacerange\n\
            <0000> <FFFF>\n\
            endcodespacerange\n";
        let m = ToUnicodeMap::parse(data);
        assert_eq!(m.code_byte_lengths(), &[2u8][..]);
    }

    #[test]
    fn codespacerange_default_when_absent() {
        let data = b"beginbfchar\n<41> <0041>\nendbfchar\n";
        let m = ToUnicodeMap::parse(data);
        // No codespacerange declared -> defaults to [2].
        assert_eq!(m.code_byte_lengths(), &[2u8][..]);
        // One-byte source code.
        assert_eq!(m.lookup(0x41), Some("A"));
    }

    #[test]
    fn codespacerange_mixed_lengths_dedup() {
        let data = b"\
            begincodespacerange\n\
            <00> <80>\n\
            <8140> <9ffc>\n\
            <00> <ff>\n\
            endcodespacerange\n";
        let m = ToUnicodeMap::parse(data);
        let lens = m.code_byte_lengths();
        assert!(lens.contains(&1));
        assert!(lens.contains(&2));
        assert_eq!(lens.len(), 2); // 1-byte length recorded once
    }

    #[test]
    fn bfchar_multi_char_destination() {
        // Map one code to the ligature expansion "ff" (two UTF-16 units).
        let data = b"beginbfchar\n<0001> <00660066>\nendbfchar\n";
        let m = ToUnicodeMap::parse(data);
        assert_eq!(m.lookup(1), Some("ff"));
    }

    #[test]
    fn bfrange_carry_across_units() {
        // Start at U+00FF, range of 2 codes -> 00FF, 0100.
        let data = b"beginbfrange\n<0010> <0011> <00FF>\nendbfrange\n";
        let m = ToUnicodeMap::parse(data);
        assert_eq!(m.lookup(0x10), Some("\u{00FF}"));
        assert_eq!(m.lookup(0x11), Some("\u{0100}"));
    }

    #[test]
    fn multiple_blocks_in_one_stream() {
        let data = b"\
            begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n\
            beginbfchar\n<0041> <0041>\n<0042> <0042>\nendbfchar\n\
            beginbfrange\n<0030> <0032> <0030>\nendbfrange\n";
        let m = ToUnicodeMap::parse(data);
        assert_eq!(m.lookup(0x41), Some("A"));
        assert_eq!(m.lookup(0x42), Some("B"));
        assert_eq!(m.lookup(0x30), Some("0"));
        assert_eq!(m.lookup(0x32), Some("2"));
        assert_eq!(m.code_byte_lengths(), &[2u8][..]);
    }

    #[test]
    fn empty_and_garbage_inputs_never_panic() {
        assert!(ToUnicodeMap::parse(b"").is_empty());
        assert!(ToUnicodeMap::parse(b"   \n\t  ").is_empty());
        assert!(ToUnicodeMap::parse(b"<<<<<>>>>>").is_empty());
        // Truncated block must not panic and must not crash.
        let m = ToUnicodeMap::parse(b"beginbfchar\n<004");
        assert!(m.is_empty() || !m.is_empty());
    }

    #[test]
    fn named_destination_in_bfchar_ignored() {
        let data = b"\
            beginbfchar\n\
            <0001> /bullet\n\
            <0002> <0042>\n\
            endbfchar\n";
        let m = ToUnicodeMap::parse(data);
        assert_eq!(m.lookup(1), None); // name ignored
        assert_eq!(m.lookup(2), Some("B")); // stays in sync afterwards
    }

    #[test]
    fn three_byte_source_code() {
        let data = b"beginbfchar\n<010203> <0041>\nendbfchar\n";
        let m = ToUnicodeMap::parse(data);
        assert_eq!(m.lookup(0x010203), Some("A"));
    }

    // ----- CidCMap -----

    #[test]
    fn predefined_cmaps() {
        let h = CidCMap::predefined("Identity-H").unwrap();
        assert!(h.identity);
        assert_eq!(h.wmode, 0);
        let v = CidCMap::predefined("Identity-V").unwrap();
        assert_eq!(v.wmode, 1);

        let gb = CidCMap::predefined("UniGB-UCS2-H").unwrap();
        assert!(gb.codes_are_unicode);
        let jis_v = CidCMap::predefined("UniJIS-UTF16-V").unwrap();
        assert!(jis_v.codes_are_unicode);
        assert_eq!(jis_v.wmode, 1);

        // Legacy byte-encoded CMaps are not bundled.
        assert!(CidCMap::predefined("90ms-RKSJ-H").is_none());
        assert!(CidCMap::predefined("ETen-B5-H").is_none());
    }

    #[test]
    fn embedded_cmap_cidrange_and_codespace() {
        let data = b"\
            /CIDInit /ProcSet findresource begin\n\
            1 begincodespacerange\n<00> <80>\nendcodespacerange\n\
            1 begincodespacerange\n<8140> <FEFF>\nendcodespacerange\n\
            2 begincidrange\n<00> <7F> 1\n<8140> <817E> 633\nendcidrange\n\
            1 begincidchar\n<80> 9999\nendcidchar\n";
        let m = CidCMap::parse(data);

        // 1-byte code within <00>..<80>.
        assert_eq!(m.next_code(b"\x41\x42"), (0x41, 1));
        assert_eq!(m.code_to_cid(0x41, 1), 0x42); // 1 + (0x41 - 0)
        // 2-byte code within <8140>..<FEFF>.
        assert_eq!(m.next_code(b"\x81\x41"), (0x8141, 2));
        assert_eq!(m.code_to_cid(0x8141, 2), 634);
        // cidchar single.
        assert_eq!(m.code_to_cid(0x80, 1), 9999);
        // Unmapped code in a non-identity CMap → CID 0.
        assert_eq!(m.code_to_cid(0x9000, 2), 0);
    }

    #[test]
    fn embedded_cmap_wmode_and_fallbacks() {
        let m = CidCMap::parse(b"/WMode 1 def\n");
        assert_eq!(m.wmode, 1);
        // No mappings declared → lenient identity.
        assert!(m.identity);
        assert_eq!(m.code_to_cid(0x1234, 2), 0x1234);

        // Garbage never panics; defaults to 2-byte identity segmentation.
        let g = CidCMap::parse(b"<<>> [/borked");
        assert_eq!(g.next_code(b"\x12\x34\x56"), (0x1234, 2));
    }

    #[test]
    fn usecmap_identity_base() {
        let m = CidCMap::parse(b"/Identity-H usecmap\n1 begincidchar\n<0005> 77\nendcidchar\n");
        assert_eq!(m.code_to_cid(5, 2), 77);
        // Codes outside the explicit mappings fall to the identity base.
        assert_eq!(m.code_to_cid(0x300, 2), 0x300);
    }
}
