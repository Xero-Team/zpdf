//! Bounded recovery for malformed CAD text fonts (AutoCAD/nanoCAD output).
//!
//! Evidence is gathered while the page is interpreted: there is no pre-scan,
//! so the interpreter's operand, operator and time budgets bound all of it.
//! Each repair has a narrow trigger and validates the replacement:
//!
//! - **Broken subsets.** An embedded TrueType subset may keep the glyph ids it
//!   shows but zero their `loca` lengths. A blank glyph whose own `/ToUnicode`
//!   names visible text is drawn from another font on the page only when that
//!   font maps the same code to the identical Unicode string, draws a glyph for
//!   it and declares the same width. Positions and advances stay the original
//!   font's. No whole-font redirect is made. A font is inspected until its
//!   first 32 distinct glyphs on a page prove healthy.
//! - **Leaked composite text.** A show under a simple font, inheriting it
//!   instead of the intended Type0 font, can carry multi-byte codes. It is
//!   shown with a Type0 font already shown on the page only when the bytes
//!   have that shape: the candidate's CMap decodes every string exactly and
//!   every multi-byte code starts with a non-whitespace byte the simple font
//!   cannot draw. At least half of the codes must have been shown with the
//!   candidate before, and its glyphs must draw every code (blank only where
//!   the code means whitespace).
//! - **Leaked simple text.** A show under a Type0 font whose CMap cannot
//!   decode it even leniently (a fragment shorter than any code, or no code
//!   with a CID and glyph) is shown with a simple font of the same base name
//!   already shown on the page, when that font draws every byte.
//!
//! Leak repairs override the font for one operator only and never become the
//! graphics state's font, and shows under an override are never evidence.
//! Work is capped per page; past the cap, recovery is disabled for the rest
//! of the page.

use std::collections::{HashMap, HashSet};

use zpdf_display_list::PositionedGlyph;
use zpdf_document::page::ResourceDict;
use zpdf_font::cmap::CidCMap;
use zpdf_font::{BlankGlyphTable, DrawingGlyph, FontCache, FontId, LoadedFont, PdfFontType};

use crate::interpreter::next_text_code;

/// Longest show operator, all string operands together, a leak repair judges.
const MAX_OPERATOR_BYTES: usize = 1024;
/// Candidate validations per page. Recovery is disabled once exceeded.
const MAX_PROBES: u64 = 16_384;
/// Fonts per page whose glyphs are classified; later fonts are not inspected.
const MAX_FONTS: usize = 64;
/// Type0 fonts per page whose shown codes are kept as leak evidence.
const MAX_EVIDENCE_FONTS: usize = 16;
/// Remembered replacement decisions per page.
const MAX_DECISIONS: usize = 4096;
/// Distinct glyphs per font and page sampled before a font without broken
/// blanks is no longer inspected on that page.
const SAMPLE_GLYPHS: usize = 32;
/// Width agreement for a replacement glyph, in 1/1000 text-space units.
const WIDTH_TOLERANCE: f64 = 0.5;

#[derive(Default)]
pub(crate) struct FontRepair {
    disabled: bool,
    /// Candidate validations spent on this page.
    pub(crate) probes: u64,
    /// Distinct broken-subset codes drawn from another page font, plus show
    /// operators shown with another font.
    pub(crate) repairs: u64,
    // Pages use few fonts: per-font state is searched linearly, which is
    // cheaper per show than hashing the font id.
    blanks: Vec<(FontId, Option<Box<BlankGlyphs>>)>,
    simple_codes: Vec<(FontId, Box<SimpleCodes>)>,
    /// Type0 fonts shown under their own `Tf`, with the codes they showed.
    evidence: Vec<(FontId, CodeSet)>,
    /// Simple fonts shown under their own `Tf`.
    simple_shown: Vec<FontId>,
    /// Sibling candidates of the current page and Form scope.
    scope: Vec<FontId>,
    /// `scope` matches the Form fonts in effect.
    scope_current: bool,
    /// Sibling decisions, valid for `scope`.
    decisions: HashMap<(FontId, u32, u8), Option<DrawingGlyph>>,
}

/// Fonts a repair may draw from: the page and enclosing Form resources.
pub(crate) struct RepairScope<'s> {
    pub(crate) page: Option<&'s ResourceDict>,
    pub(crate) forms: &'s [HashMap<String, String>],
}

impl FontRepair {
    fn spend(&mut self, probes: u64) -> bool {
        self.probes = self.probes.saturating_add(probes);
        if self.probes > MAX_PROBES {
            if !self.disabled {
                tracing::debug!("CAD font recovery budget exhausted; disabled for this page");
            }
            self.disabled = true;
        }
        !self.disabled
    }

    /// A Form with its own font resources began or ended.
    pub(crate) fn scope_changed(&mut self) {
        self.scope_current = false;
    }

    /// Rebuild the sibling candidates when the Form scope changed: distinct
    /// font programs in resource-name order, at most `MAX_FONTS`. The work
    /// is charged at one probe per 16 names; decisions made for a different
    /// candidate list are dropped.
    fn current_scope(&mut self, cache: &FontCache, scope: &RepairScope<'_>) -> bool {
        if self.scope_current {
            return true;
        }
        let mut names: Vec<&str> = scope
            .page
            .into_iter()
            .flat_map(|resources| resources.fonts.keys())
            .chain(scope.forms.iter().flat_map(|names| names.values()))
            .map(String::as_str)
            .collect();
        if self.disabled || !self.spend(1 + names.len() as u64 / 16) {
            return false;
        }
        names.sort_unstable();
        names.dedup();
        let mut programs = HashSet::new();
        let mut fonts = Vec::new();
        for name in names {
            if let Some((id, font)) = cache.get_by_name(name) {
                if programs.insert(font as *const LoadedFont) {
                    fonts.push(id);
                    if fonts.len() == MAX_FONTS {
                        break;
                    }
                }
            }
        }
        if fonts != self.scope {
            self.decisions.clear();
            self.scope = fonts;
        }
        self.scope_current = true;
        true
    }

    /// Code → glyph lookups for a simple font, shared by `show_text` and the
    /// leak checks so each code is resolved once per page.
    pub(crate) fn simple_codes(&mut self, font_id: FontId) -> Option<&mut SimpleCodes> {
        let index = match self.simple_codes.iter().position(|(id, _)| *id == font_id) {
            Some(index) => index,
            None if self.simple_codes.len() < MAX_FONTS => {
                self.simple_codes.push((font_id, Box::default()));
                self.simple_codes.len() - 1
            }
            None => return None,
        };
        Some(&mut self.simple_codes[index].1)
    }

    /// A simple font shown under its own `Tf`: a sibling for leaked simple
    /// text. Shows under a leak override are not evidence.
    pub(crate) fn note_simple(&mut self, font_id: FontId) {
        if !self.simple_shown.contains(&font_id) && self.simple_shown.len() < MAX_EVIDENCE_FONTS {
            self.simple_shown.push(font_id);
        }
    }

    /// Remember the codes a Type0 font shows under its own `Tf`, the evidence
    /// for leak repairs. Shows under a leak override are not evidence.
    pub(crate) fn evidence(&mut self, font_id: FontId) -> Option<&mut CodeSet> {
        if self.disabled {
            return None;
        }
        let index = match self.evidence.iter().position(|(id, _)| *id == font_id) {
            Some(index) => index,
            None if self.evidence.len() < MAX_EVIDENCE_FONTS => {
                self.evidence.push((font_id, CodeSet::default()));
                self.evidence.len() - 1
            }
            None => return None,
        };
        // Most recently shown last, which breaks evidence ties.
        if index + 1 != self.evidence.len() {
            let entry = self.evidence.remove(index);
            self.evidence.push(entry);
        }
        self.evidence.last_mut().map(|(_, codes)| codes)
    }

    /// Whether a shown glyph is blank although its code names visible text.
    /// Classified once per glyph id and page. A font whose first
    /// `SAMPLE_GLYPHS` distinct glyphs on the page include no such blank is
    /// treated as healthy for the rest of the page, so healthy text pays one
    /// lookup per show; a provider still checks every glyph itself.
    pub(crate) fn scan_blanks(
        &mut self,
        font_id: FontId,
        font: &LoadedFont,
        bytes: &[u8],
        glyphs: &[PositionedGlyph],
    ) -> bool {
        if self.disabled {
            return false;
        }
        let index = match self.blanks.iter().position(|(id, _)| *id == font_id) {
            Some(index) => index,
            None if self.blanks.len() < MAX_FONTS => {
                self.blanks
                    .push((font_id, BlankGlyphs::new(font).map(Box::new)));
                self.blanks.len() - 1
            }
            None => return false,
        };
        let Some(blanks) = &mut self.blanks[index].1 else {
            return false;
        };
        if !blanks.broken && blanks.classified >= SAMPLE_GLYPHS {
            return false;
        }
        // Glyphs with outlines are settled from `loca` alone; only a new blank
        // glyph needs its code decoded and looked up.
        let mut found = false;
        let mut blank = false;
        for glyph in glyphs {
            match blanks.known(glyph.glyph_id) {
                Some(suspicious) => found |= suspicious,
                None if blanks.table.is_blank(font, glyph.glyph_id) => blank = true,
                None => blanks.record(glyph.glyph_id, false),
            }
        }
        if !blank {
            return found;
        }
        let composite = is_composite(font);
        let cmap = font.cid_cmap.as_ref();
        let mut offset = 0;
        for glyph in glyphs {
            let Some((code, _, raw, end)) = next_text_code(cmap, composite, bytes, offset) else {
                break;
            };
            offset = end;
            // Every glyph still unknown is blank (see above).
            if blanks.known(glyph.glyph_id).is_none() {
                let suspicious = font.blank_hides_text(code, raw);
                blanks.record(glyph.glyph_id, suspicious);
                found |= suspicious;
            }
        }
        found
    }

    pub(crate) fn is_suspicious(&self, font_id: FontId, glyph_id: u16) -> bool {
        self.blanks.iter().any(|(id, blanks)| {
            *id == font_id
                && blanks.as_ref().and_then(|blanks| blanks.known(glyph_id)) == Some(true)
        })
    }

    /// A validated glyph from another font for a blank glyph shown for visible
    /// text, or `None` to keep the original drawing.
    pub(crate) fn sibling_glyph(
        &mut self,
        cache: &FontCache,
        scope: &RepairScope<'_>,
        font_id: FontId,
        code: u32,
        length: u8,
        raw_code: u32,
    ) -> Option<DrawingGlyph> {
        if !self.current_scope(cache, scope) {
            return None;
        }
        let key = (font_id, code, length);
        if let Some(decision) = self.decisions.get(&key) {
            return *decision;
        }
        if self.disabled || self.decisions.len() >= MAX_DECISIONS {
            return None;
        }
        let font = cache.get(font_id)?;
        let text = to_unicode(font, code, raw_code)?;
        let original = code_glyph(font, code, length, raw_code)?;
        let mut found = None;
        for index in 0..self.scope.len() {
            let id = self.scope[index];
            if id == font_id {
                continue;
            }
            if !self.spend(1) {
                return None;
            }
            let Some(sibling) = cache.get(id) else {
                continue;
            };
            if std::ptr::eq(sibling, font)
                || sibling.is_substitute
                || !sibling.has_font_data()
                || sibling.is_type3()
                || is_composite(sibling) != is_composite(font)
                || to_unicode(sibling, code, raw_code) != Some(text)
            {
                continue;
            }
            let Some(candidate) = code_glyph(sibling, code, length, raw_code) else {
                continue;
            };
            if (candidate.width - original.width).abs() <= WIDTH_TOLERANCE
                && sibling.glyph_drawn(candidate.glyph_id)
            {
                found = Some(DrawingGlyph {
                    font_id: id,
                    glyph_id: candidate.glyph_id,
                });
                self.repairs += 1;
                break;
            }
        }
        self.decisions.insert(key, found);
        found
    }

    /// The font to show a leaked operator with, for this operator only.
    /// `strings` yields the operator's string operands; a `TJ` array is judged
    /// as a whole before any element is shown.
    pub(crate) fn leaked_font<'b, I>(
        &mut self,
        cache: &FontCache,
        current: FontId,
        strings: impl Fn() -> I,
    ) -> Option<FontId>
    where
        I: Iterator<Item = &'b [u8]>,
    {
        if self.disabled {
            return None;
        }
        // Without shows of the other kind there is nothing to recover with;
        // decided before looking the font up.
        let composite = self.evidence.iter().any(|(id, _)| *id == current);
        let simple = self.simple_shown.contains(&current);
        if (self.evidence.is_empty() || composite) && self.simple_shown.is_empty()
            || (self.simple_shown.is_empty() || simple) && self.evidence.is_empty()
        {
            return None;
        }
        let font = cache.get(current)?;
        if is_composite(font) {
            self.simple_text_in_composite(cache, font, &strings)
        } else {
            self.composite_text_in_simple(cache, current, font, &strings)
        }
    }

    /// Composite codes shown with a simple font. The bytes must have the
    /// shape such a leak leaves: every multi-byte code of the candidate's own
    /// segmentation starts with a non-whitespace byte the simple font cannot
    /// draw (real simple-font text never has one at every lead position), and
    /// at least half of the codes were shown with the candidate before. CAD
    /// drawings reuse a small set of codes across labels; a coincidental
    /// decoding of other text rarely repeats that many.
    fn composite_text_in_simple<'b, I>(
        &mut self,
        cache: &FontCache,
        current: FontId,
        font: &LoadedFont,
        strings: &impl Fn() -> I,
    ) -> Option<FontId>
    where
        I: Iterator<Item = &'b [u8]>,
    {
        if self.evidence.is_empty() || font.is_type3() || !has_program(font) {
            return None;
        }
        let total: usize = strings().map(<[u8]>::len).sum();
        if total == 0 || total > MAX_OPERATOR_BYTES {
            return None;
        }
        // Bytes the simple font cannot draw that may lead a leaked code.
        // Layout whitespace (tab, line breaks, space) is simple-font text even
        // when the font has no glyph for it.
        let codes = self.simple_codes(current)?;
        let mut lead_bytes = [false; 256];
        let mut any = false;
        for &byte in strings().flatten() {
            if codes.glyph(font, byte).is_none() {
                any = true;
                lead_bytes[byte as usize] = !byte.is_ascii_whitespace() && byte != 0x0B;
            }
        }
        if !any {
            return None;
        }
        let candidates: Vec<FontId> = self.evidence.iter().rev().map(|(id, _)| *id).collect();
        let mut best: Option<(FontId, usize)> = None;
        for id in candidates {
            if !self.spend(1) {
                return None;
            }
            let Some(candidate) = cache.get(id).filter(|font| is_composite(font)) else {
                continue;
            };
            let Some(decoded) = decode_exactly(candidate.cid_cmap.as_ref(), strings()) else {
                continue;
            };
            let lead = |&(_, length, raw): &(u32, u8, u32)| (raw >> (8 * (length - 1))) as u8;
            if !decoded.iter().any(|&(_, length, _)| length > 1)
                || decoded
                    .iter()
                    .any(|code| code.1 > 1 && !lead_bytes[lead(code) as usize])
            {
                continue;
            }
            let set = &self.evidence.iter().find(|(font, _)| *font == id)?.1;
            let seen = decoded
                .iter()
                .filter(|(code, ..)| set.contains(*code))
                .count();
            if seen * 2 < decoded.len() || best.is_some_and(|(_, best)| best >= seen) {
                continue;
            }
            if !self.spend(decoded.len() as u64) {
                return None;
            }
            let glyphs: Option<Vec<(u16, u32, u32, u8)>> = decoded
                .iter()
                .map(|&(code, length, raw)| {
                    let glyph = code_glyph(candidate, code, length, raw)?.glyph_id;
                    Some((glyph, code, raw, length))
                })
                .collect();
            if glyphs.is_some_and(|glyphs| candidate.glyphs_usable(glyphs)) {
                best = Some((id, seen));
            }
        }
        let (id, _) = best?;
        self.repairs += 1;
        Some(id)
    }

    /// Simple bytes shown with a Type0 font: only an operator its CMap cannot
    /// decode even leniently counts (see [`undecodable`]), and the
    /// replacement is a simple font of the same base name shown on the page
    /// that draws every byte.
    fn simple_text_in_composite<'b, I>(
        &mut self,
        cache: &FontCache,
        font: &LoadedFont,
        strings: &impl Fn() -> I,
    ) -> Option<FontId>
    where
        I: Iterator<Item = &'b [u8]>,
    {
        let base = without_subset_prefix(&font.base_font);
        let mut candidates: Vec<FontId> = self
            .simple_shown
            .iter()
            .copied()
            .filter(|&id| {
                cache.get(id).is_some_and(|sibling| {
                    !is_composite(sibling)
                        && !sibling.is_type3()
                        && has_program(sibling)
                        && without_subset_prefix(&sibling.base_font) == base
                })
            })
            .collect();
        if candidates.is_empty() || !undecodable(font, strings()) {
            return None;
        }
        let total: usize = strings().map(<[u8]>::len).sum();
        if total > MAX_OPERATOR_BYTES {
            return None;
        }
        candidates.sort_unstable();
        for id in candidates {
            if !self.spend(1 + total as u64) {
                return None;
            }
            let sibling = cache.get(id)?;
            let codes = self.simple_codes(id)?;
            let glyphs: Option<Vec<(u16, u32, u32, u8)>> = strings()
                .flatten()
                .map(|&byte| Some((codes.glyph(sibling, byte)?, byte as u32, byte as u32, 1)))
                .collect();
            if glyphs.is_some_and(|glyphs| sibling.glyphs_usable(glyphs)) {
                self.repairs += 1;
                return Some(id);
            }
        }
        None
    }
}

/// Whether a Type0 font's lenient decoding, as `show_text` performs it,
/// genuinely fails for an operator's strings: one ends in a fragment shorter
/// than any declared code, or no code at all has a CID with a glyph in the
/// font's CID → GID map. One code outside the font's collection among
/// drawable ones is a missing character, not leaked text, and codes outside
/// the declared codespace that still decode are not evidence either.
fn undecodable<'b>(font: &LoadedFont, strings: impl Iterator<Item = &'b [u8]>) -> bool {
    let cmap = font.cid_cmap.as_ref();
    let shortest = cmap.map_or(2, CidCMap::shortest_code_length);
    // Unicode-coded and legacy substitutes draw by code, not by CID.
    let direct = font.unicode_coded() || font.legacy_substitute();
    let (mut codes, mut drawable) = (0usize, false);
    for bytes in strings {
        let mut rest = bytes;
        while !rest.is_empty() {
            let (code, length) = match cmap {
                Some(cmap) => cmap.next_code(rest),
                None => {
                    let length = rest.len().min(2);
                    let code = rest[..length]
                        .iter()
                        .fold(0u32, |value, &byte| (value << 8) | byte as u32);
                    (code, length)
                }
            };
            if length < shortest {
                return true;
            }
            codes += 1;
            drawable = drawable || direct || {
                let cid = cmap.map_or(code, |cmap| cmap.code_to_cid(code, length as u8));
                u16::try_from(cid)
                    .ok()
                    .filter(|&cid| cid != 0)
                    .is_some_and(|cid| {
                        font.cid_to_gid
                            .as_ref()
                            .is_none_or(|map| map.get(&cid).is_some_and(|&gid| gid != 0))
                    })
            };
            rest = &rest[length.max(1)..];
        }
    }
    codes > 0 && !drawable
}

/// Per-page simple-font code → glyph id table (0 = not looked up yet).
pub(crate) struct SimpleCodes([u32; 256]);

impl Default for SimpleCodes {
    fn default() -> Self {
        Self([0; 256])
    }
}

impl SimpleCodes {
    pub(crate) fn glyph(&mut self, font: &LoadedFont, byte: u8) -> Option<u16> {
        let slot = &mut self.0[byte as usize];
        if *slot == 0 {
            *slot = font
                .code_to_gid(byte as u16)
                .map_or(1, |gid| gid as u32 + 2);
        }
        (*slot >= 2).then(|| (*slot - 2) as u16)
    }
}

/// Two-byte codes seen under one Type0 font.
/// Grows only as far as the largest code seen.
#[derive(Default)]
pub(crate) struct CodeSet(Vec<u64>);

impl CodeSet {
    pub(crate) fn insert(&mut self, code: u32) {
        if let Ok(code) = u16::try_from(code) {
            set_bit(&mut self.0, code);
        }
    }

    fn contains(&self, code: u32) -> bool {
        u16::try_from(code).is_ok_and(|code| bit(&self.0, code))
    }
}

/// Blank-glyph classification for one embedded font: whether a shown blank
/// hides visible text, memoized per glyph id for the page.
struct BlankGlyphs {
    table: BlankGlyphTable,
    known: Vec<u64>,
    suspicious: Vec<u64>,
    /// Distinct glyph ids classified on this page.
    classified: usize,
    /// Some shown blank glyph hides visible text.
    broken: bool,
}

impl BlankGlyphs {
    /// Only embedded TrueType outlines with a `/ToUnicode` qualify; a
    /// substitute, Type 1, Type 3 or CFF program is never inspected.
    fn new(font: &LoadedFont) -> Option<Self> {
        font.to_unicode.as_ref()?;
        Some(Self {
            table: font.blank_glyph_table()?,
            known: Vec::new(),
            suspicious: Vec::new(),
            classified: 0,
            broken: false,
        })
    }

    fn known(&self, glyph_id: u16) -> Option<bool> {
        bit(&self.known, glyph_id).then(|| bit(&self.suspicious, glyph_id))
    }

    fn record(&mut self, glyph_id: u16, suspicious: bool) {
        set_bit(&mut self.known, glyph_id);
        self.classified += 1;
        if suspicious {
            set_bit(&mut self.suspicious, glyph_id);
            self.broken = true;
        }
    }
}

fn bit(bits: &[u64], index: u16) -> bool {
    bits.get(index as usize >> 6)
        .is_some_and(|word| word & (1 << (index & 63)) != 0)
}

fn set_bit(bits: &mut Vec<u64>, index: u16) {
    let word = index as usize >> 6;
    if bits.len() <= word {
        bits.resize(word + 1, 0);
    }
    bits[word] |= 1 << (index & 63);
}

/// A font's drawing glyph id and logical width (1/1000 text space) for a code.
struct CodeGlyph {
    glyph_id: u16,
    width: f64,
}

/// Mirrors `show_text`: the glyph id a font's run carries for this code and
/// the width it advances by. `None` when the font segments the code otherwise
/// or has no glyph for it.
fn code_glyph(font: &LoadedFont, code: u32, length: u8, raw_code: u32) -> Option<CodeGlyph> {
    if is_composite(font) {
        let bytes = raw_code.to_be_bytes();
        let source = bytes.get(4usize.checked_sub(length as usize)?..)?;
        let exact = match &font.cid_cmap {
            Some(cmap) => cmap.exact_code(source),
            None => (source.len() >= 2)
                .then(|| (u32::from(u16::from_be_bytes([source[0], source[1]])), 2)),
        };
        if exact != Some((code, length as usize)) {
            return None;
        }
        let cid = font
            .cid_cmap
            .as_ref()
            .map_or(code, |cmap| cmap.code_to_cid(code, length));
        let cid = u16::try_from(cid).ok()?;
        let glyph_id = if font.unicode_coded() || font.legacy_substitute() {
            font.composite_unicode_gid(code, raw_code, length)?
        } else {
            cid
        };
        Some(CodeGlyph {
            glyph_id,
            width: font.glyph_advance(cid),
        })
    } else {
        if length != 1 {
            return None;
        }
        let glyph_id = font.code_to_gid(code as u16)?;
        let divisor = font.advance_divisor();
        (divisor > 0.0).then(|| CodeGlyph {
            glyph_id,
            width: font.simple_glyph_advance(code as u16, glyph_id) * 1000.0 / divisor,
        })
    }
}

/// Every string split into in-codespace codes, as `(code, length, raw)`.
fn decode_exactly<'b>(
    cmap: Option<&CidCMap>,
    strings: impl Iterator<Item = &'b [u8]>,
) -> Option<Vec<(u32, u8, u32)>> {
    let mut codes = Vec::new();
    for bytes in strings {
        let mut rest = bytes;
        while !rest.is_empty() {
            let (code, length) = match cmap {
                Some(cmap) => cmap.exact_code(rest)?,
                None if rest.len() >= 2 => (u32::from(u16::from_be_bytes([rest[0], rest[1]])), 2),
                None => return None,
            };
            let raw = rest[..length]
                .iter()
                .fold(0u32, |value, &byte| (value << 8) | byte as u32);
            codes.push((code, length as u8, raw));
            rest = &rest[length..];
        }
    }
    Some(codes)
}

fn to_unicode(font: &LoadedFont, code: u32, raw_code: u32) -> Option<&str> {
    let map = font.to_unicode.as_ref()?;
    map.lookup(raw_code).or_else(|| map.lookup(code))
}

fn is_composite(font: &LoadedFont) -> bool {
    matches!(font.font_type, PdfFontType::Type0CidType2)
}

fn has_program(font: &LoadedFont) -> bool {
    font.has_font_data() || font.type1.is_some()
}

/// `ABCDEF+Name` → `Name` (PDF 32000-1 §9.6.4 subset tags).
fn without_subset_prefix(name: &str) -> &str {
    match name.split_once('+') {
        Some((tag, rest)) if tag.len() == 6 && tag.bytes().all(|b| b.is_ascii_uppercase()) => rest,
        _ => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_decoding_keeps_raw_bytes_and_rejects_partial_codes() {
        let cmap =
            CidCMap::parse(b"2 begincodespacerange <00> <7F> <8000> <FFFF> endcodespacerange");
        let strings = [b"\x41\x81\x40".as_slice(), b"\x42".as_slice()];
        assert_eq!(
            decode_exactly(Some(&cmap), strings.into_iter()),
            Some(vec![(0x41, 1, 0x41), (0x8140, 2, 0x8140), (0x42, 1, 0x42)])
        );
        assert_eq!(
            decode_exactly(Some(&cmap), [b"\x41\x81".as_slice()].into_iter()),
            None
        );
        assert_eq!(
            decode_exactly(None, [b"\x00\x13\x00".as_slice()].into_iter()),
            None
        );
    }

    #[test]
    fn evidence_tracks_recency_and_two_byte_codes() {
        let mut repair = FontRepair::default();
        repair.evidence(1).unwrap().insert(0x13);
        repair.evidence(2).unwrap().insert(0x1_0000);
        repair.evidence(1).unwrap();
        assert_eq!(
            repair
                .evidence
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
            [2, 1]
        );
        assert!(repair.evidence[1].1.contains(0x13));
        assert!(!repair.evidence[0].1.contains(0x1_0000));
        for id in 3..40 {
            repair.evidence(id);
        }
        assert_eq!(repair.evidence.len(), MAX_EVIDENCE_FONTS);
    }

    #[test]
    fn subset_tags_are_six_uppercase_letters() {
        assert_eq!(without_subset_prefix("ABCDEF+Arial"), "Arial");
        assert_eq!(without_subset_prefix("ABCDE+Arial"), "ABCDE+Arial");
        assert_eq!(without_subset_prefix("abcdef+Arial"), "abcdef+Arial");
        assert_eq!(without_subset_prefix("Arial"), "Arial");
    }
}
