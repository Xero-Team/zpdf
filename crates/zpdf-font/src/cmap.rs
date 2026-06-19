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

/// The national character encoding of a predefined legacy (byte-encoded) CJK
/// CMap. These map a multi-byte character code, in a national encoding, to a
/// glyph in an Adobe character collection; for a *substituted* (non-embedded)
/// font we instead decode the code to Unicode and resolve the glyph through the
/// system face's Unicode `cmap`. The 2-byte → Unicode tables are baked from the
/// platform codecs (`tools/gen_cjk_tables.py`); 1-byte codes are ASCII (plus
/// the Shift-JIS half-width katakana block).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyEncoding {
    /// GB2312 / EUC-CN — `GBpc-EUC-H/V`, `GB-EUC-H/V` (Adobe-GB1).
    EucCn,
    /// GBK — `GBK-EUC-H/V`, `GBKp-EUC-H/V`, `GBK2K-H/V` (Adobe-GB1).
    Gbk,
    /// Big5 — `B5pc-H/V`, `ETen-B5-H/V`, `HKscs-B5-H/V` (Adobe-CNS1).
    Big5,
    /// Shift-JIS — the `*-RKSJ-H/V` CMaps (Adobe-Japan1).
    ShiftJis,
    /// EUC-JP — `EUC-H/V` (Adobe-Japan1).
    EucJp,
    /// EUC-KR / UHC — `KSC-EUC-H/V`, `KSCms-UHC-H/V`, `KSCpc-EUC-H` (Adobe-Korea1).
    EucKr,
}

impl LegacyEncoding {
    /// Classify a predefined CMap name; `None` if it is not a supported legacy
    /// byte-encoded CMap (the caller then falls back to Identity).
    fn from_cmap_name(name: &str) -> Option<Self> {
        // Japanese Shift-JIS variants all carry the "RKSJ" tag
        // (90ms / 90msp / 90pv / 83pv / Add / Ext / 78 …).
        if name.contains("RKSJ") {
            return Some(LegacyEncoding::ShiftJis);
        }
        if name.starts_with("GBK") {
            return Some(LegacyEncoding::Gbk);
        }
        if name.starts_with("GBpc") || name.starts_with("GB-EUC") {
            return Some(LegacyEncoding::EucCn);
        }
        // B5pc / ETen-B5 / ETenms-B5 / HKscs-B5 / …
        if name.starts_with("B5pc") || name.contains("-B5") {
            return Some(LegacyEncoding::Big5);
        }
        // KSC-EUC / KSCms-UHC / KSCms-UHC-HW / KSCpc-EUC (EUC-KR / UHC). The
        // `KSC-Johab-H/V` CMaps use the unrelated Johab encoding, which we do
        // not bundle a table for — exclude them so they degrade to Identity
        // rather than decode through the wrong (cp949) table into confidently
        // wrong Hangul.
        if name.starts_with("KSC") && !name.contains("Johab") {
            return Some(LegacyEncoding::EucKr);
        }
        if name == "EUC-H" || name == "EUC-V" {
            return Some(LegacyEncoding::EucJp);
        }
        None
    }

    /// Byte-length codespace ranges `(len, lo, hi)` used to segment a show-text
    /// byte string into codes for this encoding.
    fn codespace(self) -> &'static [(u8, u32, u32)] {
        match self {
            // EUC: 1-byte ASCII + 2-byte lead/trail in 0xA1..=0xFE.
            LegacyEncoding::EucCn => &[(1, 0x00, 0x80), (2, 0xA1A1, 0xFEFE)],
            // EUC-JP additionally has SS2 half-width kana (0x8E 0xA1..=0xDF).
            LegacyEncoding::EucJp => &[(1, 0x00, 0x80), (2, 0x8EA1, 0x8EDF), (2, 0xA1A1, 0xFEFE)],
            // GBK / Big5 / UHC: 1-byte ASCII + 2-byte lead 0x81..=0xFE.
            LegacyEncoding::Gbk | LegacyEncoding::Big5 | LegacyEncoding::EucKr => {
                &[(1, 0x00, 0x80), (2, 0x8140, 0xFEFE)]
            }
            // Shift-JIS: 1-byte ASCII + 1-byte half-width kana (0xA1..=0xDF) +
            // 2-byte lead 0x81..=0x9F and 0xE0..=0xFC.
            LegacyEncoding::ShiftJis => &[
                (1, 0x00, 0x80),
                (1, 0xA1, 0xDF),
                (2, 0x8140, 0x9FFC),
                (2, 0xE040, 0xFCFC),
            ],
        }
    }

    /// Decode a single-byte code: ASCII identity for every encoding, plus the
    /// Shift-JIS half-width katakana block (0xA1..=0xDF → U+FF61..=U+FF9F).
    fn decode_single(self, b: u8) -> Option<u32> {
        if self == LegacyEncoding::ShiftJis && (0xA1..=0xDF).contains(&b) {
            return Some(0xFF61 + (b - 0xA1) as u32);
        }
        (b <= 0x7F).then_some(b as u32)
    }

    /// Decode a 2-byte code to a Unicode scalar via the baked table.
    fn decode_double(self, code: u16) -> Option<char> {
        match self {
            LegacyEncoding::EucCn => crate::gb2312::gb2312_to_unicode(code),
            LegacyEncoding::Gbk => crate::gbk::gbk_to_unicode(code),
            LegacyEncoding::Big5 => crate::big5::big5_to_unicode(code),
            LegacyEncoding::ShiftJis => crate::sjis::sjis_to_unicode(code),
            LegacyEncoding::EucJp => crate::eucjp::eucjp_to_unicode(code),
            LegacyEncoding::EucKr => crate::ksc::ksc_to_unicode(code),
        }
    }
}

/// A code → CID CMap for composite (Type0) fonts: embedded CMap streams plus
/// the predefined Identity, Unicode (UCS-2/UTF-16), and legacy byte-encoded
/// (GB / GBK / Big5 / Shift-JIS / EUC-JP / KSC) families.
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
    /// A predefined legacy byte-encoded CMap (`GBpc-EUC`, `GBK-EUC`, `B5pc`,
    /// `90ms-RKSJ`, `KSC-EUC`, …) and the national encoding it uses. For a
    /// *substituted* (non-embedded) font the code is decoded to Unicode via
    /// [`Self::decode_to_unicode`] and the glyph resolves through the system
    /// face's Unicode cmap; the 1-byte ASCII → CID range stays installed in
    /// `cid_ranges` for Latin advances. `None` for non-legacy CMaps.
    pub legacy: Option<LegacyEncoding>,
}

impl CidCMap {
    pub fn identity(wmode: u8) -> Self {
        Self {
            wmode,
            identity: true,
            ..Default::default()
        }
    }

    /// Build a predefined legacy byte-encoded CMap for `enc`.
    ///
    /// The encoding's codespace segments the show-text bytes; the 1-byte ASCII →
    /// CID range (CID = code − 0x1F, i.e. 0x20 → CID 1 … 0x7E → CID 95) gives
    /// reasonable /W-based Latin advances — the Adobe CJK collections (GB1 /
    /// CNS1 / Japan1 / Korea1) all place the proportional ASCII set at low CIDs
    /// starting near 0x20. CJK codes carry no CID range, so they fall to /DW
    /// (full width); their glyphs come from the substituted face via Unicode.
    fn legacy(enc: LegacyEncoding, wmode: u8) -> Self {
        Self {
            wmode,
            legacy: Some(enc),
            codespace: enc.codespace().to_vec(),
            cid_ranges: vec![(1, 0x20, 0x7E, 1)],
            ..Default::default()
        }
    }

    /// True for a predefined legacy byte-encoded CMap (GB / GBK / Big5 /
    /// Shift-JIS / EUC-JP / KSC).
    pub fn is_legacy(&self) -> bool {
        self.legacy.is_some()
    }

    /// Resolve a predefined CMap by name. Identity, the Unicode families, and
    /// the legacy byte-encoded CJK families are supported; `None` means the
    /// CMap is unknown (caller falls back to Identity with a warning).
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
        if let Some(enc) = LegacyEncoding::from_cmap_name(name) {
            return Some(Self::legacy(enc, wmode));
        }
        None
    }

    /// Decode a code (with its byte length) to a Unicode scalar for a legacy
    /// byte-encoded CMap: 1-byte codes are ASCII (plus Shift-JIS half-width
    /// kana), 2-byte codes go through the encoding's baked table. `None` for
    /// non-legacy CMaps or undefined codes.
    pub fn decode_to_unicode(&self, code: u32, len: u8) -> Option<u32> {
        let enc = self.legacy?;
        if len == 1 {
            return enc.decode_single(code as u8);
        }
        enc.decode_double(code as u16).map(|c| c as u32)
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
                            (Token::HexString(lo), Token::HexString(hi), Token::Number(cid)) => {
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

        // Legacy byte-encoded CMaps now classify to their national encoding.
        assert_eq!(
            CidCMap::predefined("90ms-RKSJ-H").unwrap().legacy,
            Some(LegacyEncoding::ShiftJis)
        );
        assert_eq!(
            CidCMap::predefined("ETen-B5-H").unwrap().legacy,
            Some(LegacyEncoding::Big5)
        );
        // An unknown CMap is still unresolved.
        assert!(CidCMap::predefined("Bogus-CMap-H").is_none());
    }

    #[test]
    fn legacy_cmap_name_classification() {
        use LegacyEncoding::*;
        let cases = [
            ("GBpc-EUC-H", EucCn),
            ("GB-EUC-V", EucCn),
            ("GBK-EUC-H", Gbk),
            ("GBKp-EUC-H", Gbk),
            ("GBK2K-V", Gbk),
            ("B5pc-H", Big5),
            ("ETen-B5-V", Big5),
            ("ETenms-B5-H", Big5),
            ("HKscs-B5-H", Big5),
            ("90ms-RKSJ-H", ShiftJis),
            ("90msp-RKSJ-V", ShiftJis),
            ("90pv-RKSJ-H", ShiftJis),
            ("Add-RKSJ-H", ShiftJis),
            ("Ext-RKSJ-V", ShiftJis),
            ("EUC-H", EucJp),
            ("EUC-V", EucJp),
            ("KSC-EUC-H", EucKr),
            ("KSCms-UHC-V", EucKr),
            ("KSCms-UHC-HW-H", EucKr),
            ("KSCpc-EUC-H", EucKr),
        ];
        for (name, want) in cases {
            assert_eq!(
                CidCMap::predefined(name).and_then(|c| c.legacy),
                Some(want),
                "CMap {name}"
            );
        }
        // Unicode and Identity families are not legacy.
        assert!(!CidCMap::predefined("UniGB-UCS2-H").unwrap().is_legacy());
        assert!(!CidCMap::predefined("Identity-H").unwrap().is_legacy());

        // KSC-Johab uses the unrelated Johab encoding (no table); it must not be
        // mis-routed to the EUC-KR/cp949 table — degrade to Identity instead.
        assert!(CidCMap::predefined("KSC-Johab-H").is_none());
        assert!(CidCMap::predefined("KSC-Johab-V").is_none());
    }

    #[test]
    fn gbpc_euc_decodes_and_splits() {
        let cm = CidCMap::predefined("GBpc-EUC-H").unwrap();
        assert!(cm.is_legacy());
        assert_eq!(cm.legacy, Some(LegacyEncoding::EucCn));
        assert_eq!(cm.wmode, 0);
        assert_eq!(CidCMap::predefined("GBpc-EUC-V").unwrap().wmode, 1);

        // next_code: 1-byte ASCII, then a 2-byte EUC-CN code.
        let bytes = b"\x4D\xCF\xC2"; // 'M' then 0xCFC2 (下)
        assert_eq!(cm.next_code(&bytes[..]), (0x4D, 1));
        assert_eq!(cm.next_code(&bytes[1..]), (0xCFC2, 2));

        // decode_to_unicode: ASCII identity + GB2312 lookup.
        assert_eq!(cm.decode_to_unicode(0x4D, 1), Some(0x4D));
        assert_eq!(cm.decode_to_unicode(0xCFC2, 2), Some(0x4E0B)); // 下
                                                                   // 1-byte ASCII → Adobe-GB1 CID (CID = code − 0x1F): 'M' → 46.
        assert_eq!(cm.code_to_cid(0x4D, 1), 46);
    }

    #[test]
    fn legacy_segmentation_and_decode() {
        // Big5 (B5pc): 一 = 0xA440; ASCII stays single-byte.
        let b5 = CidCMap::predefined("B5pc-H").unwrap();
        assert_eq!(b5.next_code(b"\xA4\x40A"), (0xA440, 2));
        assert_eq!(b5.next_code(b"A\xA4\x40"), (b'A' as u32, 1));
        assert_eq!(b5.decode_to_unicode(0xA440, 2), Some(0x4E00));

        // GBK: 一 = 0xD2BB.
        let gbk = CidCMap::predefined("GBK-EUC-H").unwrap();
        assert_eq!(gbk.next_code(b"\xD2\xBB"), (0xD2BB, 2));
        assert_eq!(gbk.decode_to_unicode(0xD2BB, 2), Some(0x4E00));

        // Shift-JIS: 2-byte 亜 = 0x889F; 1-byte half-width katakana ｱ = 0xB1.
        let sj = CidCMap::predefined("90ms-RKSJ-H").unwrap();
        assert_eq!(sj.next_code(b"\x88\x9F"), (0x889F, 2));
        assert_eq!(sj.decode_to_unicode(0x889F, 2), Some(0x4E9C));
        assert_eq!(sj.next_code(b"\xB1"), (0xB1, 1));
        assert_eq!(sj.decode_to_unicode(0xB1, 1), Some(0xFF71));
        assert_eq!(sj.next_code(b"A\x88\x9F"), (b'A' as u32, 1));

        // EUC-KR / UHC: 가 = 0xB0A1.
        let ksc = CidCMap::predefined("KSC-EUC-H").unwrap();
        assert_eq!(ksc.next_code(b"\xB0\xA1"), (0xB0A1, 2));
        assert_eq!(ksc.decode_to_unicode(0xB0A1, 2), Some(0xAC00));

        // EUC-JP: 亜 = 0xB0A1; SS2 half-width kana 0x8EA1 → U+FF61.
        let euc = CidCMap::predefined("EUC-H").unwrap();
        assert_eq!(euc.next_code(b"\xB0\xA1"), (0xB0A1, 2));
        assert_eq!(euc.decode_to_unicode(0xB0A1, 2), Some(0x4E9C));
        assert_eq!(euc.next_code(b"\x8E\xA1"), (0x8EA1, 2));
        assert_eq!(euc.decode_to_unicode(0x8EA1, 2), Some(0xFF61));

        // The ASCII → CID range gives Latin advances for every legacy CMap.
        assert_eq!(b5.code_to_cid(b'M' as u32, 1), 46); // 0x4D − 0x1F
                                                        // Undefined / unmapped codes never panic.
        assert_eq!(b5.decode_to_unicode(0x8181, 2), None);
        assert_eq!(sj.decode_to_unicode(0xFF, 1), None);
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
