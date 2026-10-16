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

use crate::adobe_cmaps::{self, PredefinedMap, SourceEncoding};
pub use crate::adobe_cmaps::{CMapUnicode, CharacterCollection};
use std::collections::HashMap;

const MAX_CMAP_ENTRIES: usize = 65_536;
const MAX_CMAP_TOKENS: usize = MAX_CMAP_ENTRIES * 4 + 1024;
const MAX_CMAP_HEX_BYTES: usize = 512;
const MAX_CMAP_WORD_BYTES: usize = 256;

/// A parsed `/ToUnicode` CMap: maps a character code to its Unicode string.
#[derive(Debug, Clone, Default)]
pub struct ToUnicodeMap {
    map: HashMap<u32, String>,
    /// Distinct source-code byte lengths declared in codespacerange.
    code_lengths: Vec<u8>,
    /// Byte lengths of source keys written with leading zero bytes (`<0041>`),
    /// which the numeric key alone would read as shorter codes.
    padded_lengths: HashMap<u32, u8>,
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
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (u32, &str)> {
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

    /// Source byte length of a mapped key written with leading zero bytes:
    /// `<0041>` is a two-byte code although its value fits in one byte.
    pub fn padded_length(&self, code: u32) -> Option<u8> {
        self.padded_lengths.get(&code).copied()
    }

    fn record_code_length(&mut self, len: u8) {
        if (1..=4).contains(&len) && !self.code_lengths.contains(&len) {
            self.code_lengths.push(len);
        }
    }

    fn insert_mapping(&mut self, code: u32, len: u8, value: String) -> bool {
        if self.map.len() < MAX_CMAP_ENTRIES || self.map.contains_key(&code) {
            self.map.insert(code, value);
            let width = (4 - code.leading_zeros() / 8).max(1) as u8;
            if len > width {
                self.padded_lengths.insert(code, len);
            } else if !self.padded_lengths.is_empty() {
                self.padded_lengths.remove(&code);
            }
            true
        } else {
            false
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
                    i += 1;
                    // Record only a valid, matching one-to-four-byte range.
                    if let Some(Token::HexString(hi)) = tokens.get(i) {
                        if lo.len() == hi.len() && (1..=4).contains(&lo.len()) {
                            self.record_code_length(lo.len() as u8);
                        }
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
                    if !(1..=4).contains(&src.len()) {
                        i += 1;
                        if matches!(tokens.get(i), Some(Token::HexString(_) | Token::Name(_))) {
                            i += 1;
                        }
                        continue;
                    }
                    let code = bytes_to_code(src);
                    i += 1;
                    // The destination follows; it is usually a hex string, but
                    // per spec may be a name (which we ignore).
                    match tokens.get(i) {
                        Some(Token::HexString(dst)) => {
                            if let Some(s) = utf16be_to_string(dst) {
                                self.insert_mapping(code, src.len() as u8, s);
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
                    if !(1..=4).contains(&lo.len()) {
                        return i + 1;
                    }
                    let lo_code = bytes_to_code(lo);
                    i += 1;
                    // Expect <hi>.
                    let hi_code = match tokens.get(i) {
                        Some(Token::HexString(hi)) => {
                            if hi.len() != lo.len() {
                                return i + 1;
                            }
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
                            self.assign_bfrange_incrementing(lo_code, hi_code, lo.len() as u8, dst);
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
                                                self.insert_mapping(code, lo.len() as u8, s);
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
    fn assign_bfrange_incrementing(&mut self, lo: u32, hi: u32, len: u8, dst: &[u8]) {
        if dst.len() > MAX_CMAP_HEX_BYTES {
            return;
        }
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
                if !self.insert_mapping(code, len, s) {
                    break;
                }
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
    /// cidrange entries as (byte_len, lo, hi, first_cid, declaration).
    cid_ranges: Vec<(u8, u32, u32, u32, usize)>,
    /// Length-sensitive cidchar singles and their declaration order.
    cid_chars: HashMap<(u8, u32), (u32, usize)>,
    predefined: Option<&'static PredefinedMap>,
    collection: Option<CharacterCollection>,
    /// Writing mode: 0 horizontal, 1 vertical.
    pub wmode: u8,
    /// `wmode` came from a `usecmap` base, not from an explicit /WMode.
    wmode_inherited: bool,
    /// Source codes are UTF-16BE/UCS-2 Unicode. Authoritative predefined maps
    /// still resolve them to collection CIDs for embedded glyphs and metrics.
    pub codes_are_unicode: bool,
    /// CID = code (the Identity family; also the lenient fallback).
    pub identity: bool,
    /// The Unicode CMap uses UTF-16 rather than UCS-2, so a valid surrogate
    /// pair is one four-byte character code.
    utf16: bool,
    /// A predefined legacy byte-encoded CMap (`GBpc-EUC`, `GBK-EUC`, `B5pc`,
    /// `90ms-RKSJ`, `KSC-EUC`, …) and the national encoding it uses. For a
    /// *substituted* (non-embedded) font the code is decoded to Unicode via
    /// [`Self::decode_to_unicode`] and the glyph resolves through the system
    /// face's Unicode cmap. Named maps use pinned Adobe mappings; this field
    /// remains available for manually configured legacy decoder compatibility.
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

    /// True for a predefined legacy byte-encoded CMap (GB / GBK / Big5 /
    /// Shift-JIS / EUC-JP / KSC).
    pub fn is_legacy(&self) -> bool {
        self.legacy.is_some()
            || self
                .predefined
                .is_some_and(|map| map.encoding == SourceEncoding::Legacy)
    }

    /// Resolve a predefined CMap by name. Identity, the Unicode families, and
    /// the legacy byte-encoded CJK families are supported; `None` means the
    /// CMap is unknown (caller falls back to Identity with a warning).
    pub fn predefined(name: &str) -> Option<Self> {
        let wmode = if name.ends_with("-V") { 1 } else { 0 };
        if name == "Identity-H" || name == "Identity-V" {
            return Some(Self::identity(wmode));
        }
        let map = adobe_cmaps::predefined(name)?;
        Some(Self {
            wmode: map.wmode,
            codes_are_unicode: map.encoding != SourceEncoding::Legacy,
            utf16: map.encoding == SourceEncoding::Utf16,
            legacy: (map.encoding == SourceEncoding::Legacy)
                .then(|| LegacyEncoding::from_cmap_name(name))
                .flatten(),
            predefined: Some(map),
            collection: Some(map.collection),
            ..Default::default()
        })
    }

    /// True when code-to-CID semantics come from a pinned Adobe resource.
    pub fn is_authoritative(&self) -> bool {
        self.predefined.is_some()
    }

    pub fn collection(&self) -> Option<CharacterCollection> {
        self.collection
    }

    /// Attach CIDSystemInfo for Identity/custom CMaps without overriding a
    /// named CMap's authoritative collection.
    pub fn set_collection(&mut self, ordering: &str) {
        if self.collection.is_none() {
            self.collection = CharacterCollection::from_ordering(ordering);
        }
    }

    pub fn decode_text(&self, code: u32, len: u8) -> Option<CMapUnicode> {
        if let Some(cid) = self.explicit_cid(code, len) {
            return self.collection?.unicode(cid);
        }
        if let Some(map) = self.predefined {
            return map
                .lookup(code, len)
                .or_else(|| map.source_unicode(code, len));
        }
        if self.codes_are_unicode {
            return char::from_u32(code).map(CMapUnicode::Scalar);
        }
        if let Some(collection) = self.collection {
            return collection.unicode(self.code_to_cid(code, len));
        }
        let encoding = self.legacy?;
        let character = if len == 1 {
            encoding.decode_single(code as u8).and_then(char::from_u32)
        } else if len == 2 {
            encoding.decode_double(code as u16)
        } else {
            None
        };
        character.map(CMapUnicode::Scalar)
    }

    /// Decode a code to one Unicode scalar. Use `decode_text` when complete
    /// canonical strings, including variation selectors, are required.
    pub fn decode_to_unicode(&self, code: u32, len: u8) -> Option<u32> {
        if self.cid_chars.is_empty() && self.cid_ranges.is_empty() {
            if let Some(map) = self.predefined {
                return map
                    .lookup_scalar(code, len)
                    .or_else(|| map.source_unicode(code, len)?.scalar());
            }
        }
        self.decode_text(code, len)?.scalar()
    }

    /// Parse an embedded CMap stream: codespacerange, cidrange, cidchar,
    /// /WMode, and `usecmap` with named Adobe or Identity bases. Never panics.
    pub fn parse(data: &[u8]) -> Self {
        let mut cmap = CidCMap::default();
        let mut explicit_wmode = false;
        let mut declaration = 0;
        let tokens = tokenize(data);
        let mut i = 0usize;

        while i < tokens.len() {
            match &tokens[i] {
                Token::Keyword(kw) if kw == b"begincodespacerange" => {
                    i += 1;
                    while i + 1 < tokens.len() {
                        match (&tokens[i], &tokens[i + 1]) {
                            (Token::HexString(lo), Token::HexString(hi)) => {
                                if cmap.codespace.len() < MAX_CMAP_ENTRIES
                                    && lo.len() == hi.len()
                                    && (1..=4).contains(&lo.len())
                                {
                                    let low = bytes_to_code(lo);
                                    let high = bytes_to_code(hi);
                                    if low <= high {
                                        cmap.codespace.push((lo.len() as u8, low, high));
                                    }
                                }
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
                                if cmap.cid_ranges.len() < MAX_CMAP_ENTRIES
                                    && lo.len() == hi.len()
                                    && (1..=4).contains(&lo.len())
                                {
                                    let low = bytes_to_code(lo);
                                    let high = bytes_to_code(hi);
                                    if low <= high {
                                        cmap.cid_ranges.push((
                                            lo.len() as u8,
                                            low,
                                            high,
                                            ascii_to_u32(cid),
                                            declaration,
                                        ));
                                        declaration += 1;
                                    }
                                }
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
                                if (1..=4).contains(&code.len())
                                    && (cmap.cid_chars.len() < MAX_CMAP_ENTRIES
                                        || cmap
                                            .cid_chars
                                            .contains_key(&(code.len() as u8, bytes_to_code(code))))
                                {
                                    cmap.cid_chars.insert(
                                        (code.len() as u8, bytes_to_code(code)),
                                        (ascii_to_u32(cid), declaration),
                                    );
                                    declaration += 1;
                                }
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
                        for &range in base.codespaces() {
                            if !cmap.codespace.contains(&range) {
                                cmap.codespace.push(range);
                            }
                        }
                        cmap.identity |= base.identity;
                        cmap.codes_are_unicode |= base.codes_are_unicode;
                        cmap.utf16 |= base.utf16;
                        cmap.predefined = base.predefined;
                        cmap.collection = base.collection;
                        cmap.legacy = base.legacy;
                        if !explicit_wmode {
                            cmap.wmode = base.wmode;
                            cmap.wmode_inherited = true;
                        }
                    }
                    i += 1;
                }
                Token::Name(n) if n == b"WMode" => {
                    if let Some(Token::Number(v)) = tokens.get(i + 1) {
                        cmap.wmode = (ascii_to_u32(v) == 1) as u8;
                        cmap.wmode_inherited = false;
                        explicit_wmode = true;
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
        if cmap.cid_ranges.is_empty()
            && cmap.cid_chars.is_empty()
            && !cmap.codes_are_unicode
            && cmap.predefined.is_none()
        {
            cmap.identity = true;
        }
        cmap
    }

    /// Apply an embedded CMap stream dictionary's /WMode (PDF 32000-1 Table
    /// 120). Vertical always applies; horizontal replaces only a mode
    /// inherited through `usecmap`, never one the CMap program declares.
    pub fn apply_stream_wmode(&mut self, wmode: i64) {
        match wmode {
            1 => self.wmode = 1,
            0 if self.wmode_inherited => self.wmode = 0,
            _ => {}
        }
    }

    /// Split the next code off `bytes` per the codespace. Returns
    /// `(code, byte_len)`; with no codespace declared, codes are 2 bytes
    /// (the Identity/UCS-2 convention).
    pub fn next_code(&self, bytes: &[u8]) -> (u32, usize) {
        if bytes.is_empty() {
            return (0, 1);
        }
        if self.codes_are_unicode && self.utf16 {
            if let Some(pair) = utf16_pair(bytes) {
                return pair;
            }
            let len = bytes.len().min(2);
            return (bytes_to_code(&bytes[..len]), len);
        }
        let spaces = self.codespaces();
        if spaces.is_empty() {
            let len = bytes.len().min(2);
            return (bytes_to_code(&bytes[..len]), len);
        }
        if let Some(code) = codespace_code(spaces, bytes) {
            return code;
        }
        // No range matched: consume the shortest declared length.
        let min_len = spaces
            .iter()
            .map(|&(l, _, _)| l as usize)
            .min()
            .unwrap_or(2)
            .min(bytes.len());
        (bytes_to_code(&bytes[..min_len.max(1)]), min_len.max(1))
    }

    /// [`Self::next_code`] without its lenient recovery: `None` unless the
    /// next code is complete and lies in a declared codespace range (two-byte
    /// code units for Identity and UTF-16 maps).
    pub fn exact_code(&self, bytes: &[u8]) -> Option<(u32, usize)> {
        if self.codes_are_unicode && self.utf16 {
            let unit = bytes.get(..2)?;
            return Some(utf16_pair(bytes).unwrap_or((bytes_to_code(unit), 2)));
        }
        let spaces = self.codespaces();
        if spaces.is_empty() {
            return (bytes.len() >= 2).then(|| (bytes_to_code(&bytes[..2]), 2));
        }
        codespace_code(spaces, bytes)
    }

    /// Byte length of the shortest declared code. A code [`Self::next_code`]
    /// returns with fewer bytes is a fragment left at the end of a string.
    #[doc(hidden)]
    pub fn shortest_code_length(&self) -> usize {
        if self.codes_are_unicode && self.utf16 {
            return 2;
        }
        self.codespaces()
            .iter()
            .map(|&(length, _, _)| length as usize)
            .min()
            .unwrap_or(2)
    }

    /// Whether `bytes` segment completely into in-codespace codes, so that a
    /// string can be judged as text for this CMap rather than recovered.
    pub fn decodes_exactly(&self, bytes: &[u8]) -> bool {
        // UTF-16 accepts every complete code unit; one full-range codespace
        // accepts every complete code. Only the length can then fail.
        if self.codes_are_unicode && self.utf16 {
            return bytes.len().is_multiple_of(2);
        }
        match self.codespaces() {
            [] => return bytes.len().is_multiple_of(2),
            &[(length @ 1..=4, 0, high)] if high == u32::MAX >> (32 - 8 * length as u32) => {
                return bytes.len().is_multiple_of(length as usize);
            }
            _ => {}
        }
        let mut rest = bytes;
        while !rest.is_empty() {
            match self.exact_code(rest) {
                Some((_, length)) => rest = &rest[length..],
                None => return false,
            }
        }
        true
    }

    /// Map a code (with its byte length) to a CID.
    pub fn code_to_cid(&self, code: u32, len: u8) -> u32 {
        if let Some(cid) = self.explicit_cid(code, len) {
            return cid;
        }
        if let Some(map) = self.predefined {
            return map.lookup_cid(code, len).map_or(0, u32::from);
        }
        if self.identity {
            code
        } else {
            0
        }
    }

    /// Resolve a packed source key from ToUnicode, using this CMap's declared
    /// code lengths instead of assuming that every non-ASCII key is two bytes.
    pub fn raw_code_to_cid(&self, raw: u32) -> u32 {
        self.source_key_to_cid(raw, None)
    }

    /// [`Self::raw_code_to_cid`] for a key whose source byte length is known
    /// (see [`ToUnicodeMap::padded_length`]). The declared length is tried
    /// first; inference from the value remains the lenient fallback.
    pub fn source_key_to_cid(&self, raw: u32, length: Option<u8>) -> u32 {
        let declared = length.map_or(0, |length| self.sized_code_to_cid(raw, length as usize));
        if declared != 0 {
            return declared;
        }
        (1..=4)
            .map(|length| self.sized_code_to_cid(raw, length))
            .find(|&cid| cid != 0)
            .unwrap_or(0)
    }

    /// CID of `raw` read as exactly `length` source bytes, or 0 when that
    /// length cannot hold the value or the codespace segments it differently.
    fn sized_code_to_cid(&self, raw: u32, length: usize) -> u32 {
        if !(1..=4).contains(&length) || (length < 4 && raw >= (1u32 << (length * 8))) {
            return 0;
        }
        let bytes = raw.to_be_bytes();
        let (code, consumed) = self.next_code(&bytes[4 - length..]);
        if consumed != length {
            return 0;
        }
        self.code_to_cid(code, length as u8)
    }

    fn codespaces(&self) -> &[(u8, u32, u32)] {
        if self.codespace.is_empty() {
            self.predefined.map_or(&[], |map| map.codespaces)
        } else {
            &self.codespace
        }
    }

    /// Whether `cidchar`/`cidrange` entries can override the base mapping,
    /// e.g. an embedded CMap that `usecmap`s Identity and remaps some codes.
    pub(crate) fn has_explicit_cids(&self) -> bool {
        !self.cid_chars.is_empty() || !self.cid_ranges.is_empty()
    }

    fn explicit_cid(&self, code: u32, len: u8) -> Option<u32> {
        let code = if self.utf16 && len == 4 && (0x10000..=0x10FFFF).contains(&code) {
            let scalar = code - 0x10000;
            ((0xD800 + (scalar >> 10)) << 16) | (0xDC00 + (scalar & 1023))
        } else {
            code
        };
        let single = self.cid_chars.get(&(len, code));
        for &(rlen, lo, hi, first, declaration) in self.cid_ranges.iter().rev() {
            if single.is_some_and(|&(_, order)| order > declaration) {
                break;
            }
            if rlen == len && code >= lo && code <= hi {
                return Some(first.checked_add(code - lo).unwrap_or(0));
            }
        }
        single.map(|&(cid, _)| cid)
    }
}

/// Parse ASCII decimal bytes to u32 (CID operands in cidrange/cidchar).
fn ascii_to_u32(bytes: &[u8]) -> u32 {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
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
        if tokens.len() >= MAX_CMAP_TOKENS {
            break;
        }
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
                } else if word.len() > MAX_CMAP_WORD_BYTES {
                    continue;
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
    let mut bytes = Vec::new();
    let mut high_nibble = None;
    let mut too_long = false;

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
            if let Some(high) = high_nibble.take() {
                if bytes.len() < MAX_CMAP_HEX_BYTES {
                    bytes.push((high << 4) | v);
                } else {
                    too_long = true;
                }
            } else {
                high_nibble = Some(v);
            }
        }
        // Non-hex, non-ws chars inside `<...>` are ignored defensively.
        i += 1;
    }

    // Per PDF spec, an odd final nibble is padded with a trailing 0.
    if let Some(high) = high_nibble.filter(|_| !too_long) {
        bytes.push(high << 4);
    }

    if too_long {
        bytes.clear();
    }

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
    let mut too_long = false;

    while i < n {
        let b = data[i];
        if is_pdf_whitespace(b) || is_pdf_delimiter(b) {
            break;
        }
        if b == b'#' && i + 2 < n {
            if let (Some(h), Some(l)) = (hex_val(data[i + 1]), hex_val(data[i + 2])) {
                if out.len() < MAX_CMAP_WORD_BYTES {
                    out.push((h << 4) | l);
                } else {
                    too_long = true;
                }
                i += 3;
                continue;
            }
        }
        if out.len() < MAX_CMAP_WORD_BYTES {
            out.push(b);
        } else {
            too_long = true;
        }
        i += 1;
    }

    if too_long {
        out.clear();
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

/// A UTF-16 surrogate pair at the start of `bytes`, as one scalar code.
fn utf16_pair(bytes: &[u8]) -> Option<(u32, usize)> {
    let high = u16::from_be_bytes([*bytes.first()?, *bytes.get(1)?]);
    let low = u16::from_be_bytes([*bytes.get(2)?, *bytes.get(3)?]);
    ((0xD800..=0xDBFF).contains(&high) && (0xDC00..=0xDFFF).contains(&low)).then(|| {
        let scalar = 0x1_0000 + ((high as u32 - 0xD800) << 10) + (low as u32 - 0xDC00);
        (scalar, 4)
    })
}

/// The shortest declared codespace range containing the prefix value.
fn codespace_code(spaces: &[(u8, u32, u32)], bytes: &[u8]) -> Option<(u32, usize)> {
    for want in 1..=4usize.min(bytes.len()) {
        let v = bytes_to_code(&bytes[..want]);
        for &(len, lo, hi) in spaces {
            if len as usize == want && v >= lo && v <= hi {
                return Some((v, want));
            }
        }
    }
    None
}

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
    let mut units = Vec::with_capacity(bytes.len().div_ceil(2));
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
    fn mapping_count_is_bounded() {
        let mut map = ToUnicodeMap::default();
        for code in 0..(MAX_CMAP_ENTRIES as u32 + 10) {
            map.insert_mapping(code, 4, "A".to_string());
        }
        assert_eq!(map.map.len(), MAX_CMAP_ENTRIES);
        assert_eq!(map.padded_lengths.len(), MAX_CMAP_ENTRIES);
        assert!(map.lookup(MAX_CMAP_ENTRIES as u32).is_none());
    }

    #[test]
    fn overflowing_cid_range_is_invalid_not_a_panic() {
        let cmap = CidCMap {
            cid_ranges: vec![(1, 0, 1, u32::MAX, 0)],
            ..CidCMap::default()
        };
        assert_eq!(cmap.code_to_cid(1, 1), 0);
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
        // U+20000 encoded as UTF-16BE D840 DC00 is one scalar/code, not two
        // unpaired surrogate glyphs.
        assert_eq!(jis_v.next_code(&[0xD8, 0x40, 0xDC, 0x00]), (0x20000, 4));
        assert_eq!(gb.next_code(&[0xD8, 0x40, 0xDC, 0x00]), (0xD840, 2));

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
    fn exact_codes_follow_mixed_codespaces_without_lenient_recovery() {
        let m =
            CidCMap::parse(b"2 begincodespacerange\n<00> <7F>\n<8140> <FEFF>\nendcodespacerange\n");
        // Odd lengths are valid text when one-byte ranges exist.
        assert!(m.decodes_exactly(b"\x41\x81\x40"));
        assert_eq!(m.exact_code(b"\x81\x40\x41"), Some((0x8140, 2)));
        // A partial trailing code or a byte outside every range is not text,
        // although `next_code` still consumes it leniently.
        assert!(!m.decodes_exactly(b"\x41\x81"));
        assert_eq!(m.exact_code(b"\x80\x41"), None);
        assert_eq!(m.next_code(b"\x80\x41"), (0x80, 1));

        let identity = CidCMap::identity(0);
        assert!(identity.decodes_exactly(b"\x00\x13\x01\x16"));
        assert!(!identity.decodes_exactly(b"\x31"));
        assert_eq!(identity.exact_code(b"\x31"), None);
        assert_eq!(identity.next_code(b"\x31"), (0x31, 1));

        let utf16 = CidCMap::predefined("UniGB-UTF16-H").unwrap();
        assert_eq!(utf16.exact_code(b"\xD8\x40\xDC\x00"), Some((0x20000, 4)));
        assert!(utf16.decodes_exactly(b"\x4E\x2D\xD8\x40\xDC\x00"));
        assert!(!utf16.decodes_exactly(b"\x4E\x2D\x41"));
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
