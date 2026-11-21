//! Page drawing identities for coverage fallback. Logical PDF metrics stay in
//! the original font; admitted drawing fonts expose physical GIDs directly.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use crate::system::SubstituteHints;
use crate::{
    CidWidths, FontCache, FontCharacter, FontId, FontProvider, LoadedFont, PdfFontType,
    RegisteredFontId,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrawingGlyph {
    pub font_id: FontId,
    pub glyph_id: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ResolutionKey {
    font_id: FontId,
    glyph_id: u16,
    code: u32,
    raw_code: u32,
    length: u8,
    byte_limit: u64,
}

#[derive(Default)]
pub(crate) struct ResolutionCache {
    generation: u64,
    decisions: HashMap<ResolutionKey, DrawingGlyph>,
    faces: HashMap<(FontId, RegisteredFontId), FontId>,
}

/// Immutable LoadedFont identities include PDF maps, TTC face and variations.
/// Weak ownership prevents pointer reuse without retaining font byte buffers.
#[derive(Default)]
pub(crate) struct GlyphChecks {
    entries: HashMap<usize, CheckedFont>,
    count: usize,
}

struct CheckedFont {
    _identity: Weak<LoadedFont>,
    glyphs: Vec<(u16, GlyphState)>,
}

/// What a font program holds for one glyph id. A blank glyph is a space unless
/// the code shown with it names visible text (see [`LoadedFont::blank_hides_text`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GlyphState {
    Missing,
    Drawn,
    Blank,
}

impl GlyphState {
    fn usable(self, font: &LoadedFont, code: u32, raw_code: u32, length: u8) -> bool {
        match self {
            GlyphState::Missing => false,
            GlyphState::Drawn => true,
            GlyphState::Blank => !font.blank_hides_text_with_length(code, raw_code, length),
        }
    }
}

impl GlyphChecks {
    fn for_font(&mut self, font: &Arc<LoadedFont>) -> (&mut CheckedFont, &mut usize) {
        if self.count >= 4096 || self.entries.len() >= 1024 {
            self.entries.clear();
            self.count = 0;
        }
        let entry = self
            .entries
            .entry(Arc::as_ptr(font) as usize)
            .or_insert_with(|| CheckedFont {
                _identity: Arc::downgrade(font),
                glyphs: Vec::with_capacity(8),
            });
        (entry, &mut self.count)
    }
}

impl FontProvider {
    fn glyph_state(&self, font: &Arc<LoadedFont>, glyph: u16) -> GlyphState {
        if glyph == 0 && !font.is_type3() {
            return GlyphState::Missing;
        }
        let key = Arc::as_ptr(font) as usize;
        if let Some(Some(state)) = self.read_glyph_checks(key, |cache| {
            let entry = cache.entries.get(&key)?;
            let index = entry
                .glyphs
                .binary_search_by_key(&glyph, |(gid, _)| *gid)
                .ok()?;
            Some(entry.glyphs[index].1)
        }) {
            return state;
        }
        let state = font.glyph_state(glyph);
        self.with_glyph_checks(key, |cache| {
            let (entry, count) = cache.for_font(font);
            match entry.glyphs.binary_search_by_key(&glyph, |(gid, _)| *gid) {
                Ok(_) => {}
                Err(index) => {
                    entry.glyphs.insert(index, (glyph, state));
                    *count += 1;
                }
            }
        });
        state
    }

    #[cfg(test)]
    fn has_mapped_glyph(&self, font: &Arc<LoadedFont>, glyph: u16) -> bool {
        self.glyph_state(font, glyph) != GlyphState::Missing
    }
}

impl LoadedFont {
    /// Whether the glyph exists with a well-formed but empty outline (a space,
    /// an empty `.notdef`), as opposed to one that is missing or unreadable.
    /// Type 1 and Type 3 glyphs are never blank in this sense.
    pub fn is_blank_glyph(&self, glyph_id: u16) -> bool {
        if self.type1.is_some() || self.is_type3() {
            return false;
        }
        self.font_data
            .as_ref()
            .and_then(|data| ttf_parser::Face::parse(data, self.face_index).ok())
            .is_some_and(|mut face| {
                self.apply_variations(&mut face);
                self.drawing_gid_with_face(glyph_id, &face)
                    .is_some_and(|gid| {
                        face.outline_glyph(ttf_parser::GlyphId(gid), &mut OutlineProbe)
                            .is_none()
                            && readable_glyph(&face, gid)
                    })
            })
    }

    /// A mapped empty glyph is present even if it has no drawable outline.
    fn glyph_state(&self, glyph_id: u16) -> GlyphState {
        let mapped = |present: bool| {
            if present {
                GlyphState::Drawn
            } else {
                GlyphState::Missing
            }
        };
        if self.is_type3() {
            return mapped(self.type3_glyph_stream(glyph_id).is_some());
        }
        if glyph_id == 0 {
            return GlyphState::Missing;
        }
        if let Some(type1) = &self.type1 {
            return type1.glyph_state_by_gid(glyph_id);
        }
        parsed_face(self).map_or(GlyphState::Missing, |face| {
            self.face_glyph_state(glyph_id, &face)
        })
    }

    fn face_glyph_state(&self, glyph_id: u16, face: &ttf_parser::Face<'_>) -> GlyphState {
        self.drawing_gid_with_face(glyph_id, face)
            .map_or(GlyphState::Missing, |gid| physical_glyph_state(face, gid))
    }

    /// Whether a blank glyph shown for this code stands in for a missing one:
    /// the embedded program's own `/ToUnicode` names visible text for it.
    /// Broken CAD subsets keep the glyph ids they show but zero their `loca`
    /// entries. Whitespace, controls, format characters and private-use
    /// meanings are never evidence; substitutes are judged by their own cmap.
    #[doc(hidden)]
    pub fn blank_hides_text(&self, code: u32, raw_code: u32) -> bool {
        !self.is_substitute
            && self
                .to_unicode
                .as_ref()
                .and_then(|map| map.lookup(raw_code).or_else(|| map.lookup(code)))
                .is_some_and(is_visible_text)
    }

    /// Source-width-aware [`Self::blank_hides_text`], matching composite
    /// `/ToUnicode` keys by their actual byte length.
    #[doc(hidden)]
    pub fn blank_hides_text_with_length(&self, code: u32, raw_code: u32, length: u8) -> bool {
        !self.is_substitute
            && self
                .lookup_to_unicode(code, raw_code, length)
                .is_some_and(is_visible_text)
    }

    /// Whether every `(glyph id, code, raw code, code length)` draws what the
    /// code means: a glyph with an outline, or a blank glyph only where the
    /// font's own meaning for the code (`/ToUnicode`, else its CMap or
    /// encoding) is whitespace or invisible. An unknown meaning does not
    /// excuse a blank glyph. Parses the program once for the whole batch.
    #[doc(hidden)]
    pub fn glyphs_usable(&self, glyphs: impl IntoIterator<Item = (u16, u32, u32, u8)>) -> bool {
        let face = if self.is_type3() || self.type1.is_some() {
            None
        } else {
            match parsed_face(self) {
                Some(face) => Some(face),
                None => return false,
            }
        };
        glyphs
            .into_iter()
            .all(|(glyph_id, code, raw_code, length)| {
                let state = match &face {
                    Some(face) if glyph_id != 0 => self.face_glyph_state(glyph_id, face),
                    Some(_) => GlyphState::Missing,
                    None => self.glyph_state(glyph_id),
                };
                match state {
                    GlyphState::Missing => false,
                    GlyphState::Drawn => true,
                    GlyphState::Blank => self.blank_is_space(code, raw_code, length),
                }
            })
    }

    /// Whether every `(glyph id, code, raw code, code length)` draws a visible
    /// outline from `self` or represents known whitespace/invisible text in
    /// `meaning_font` (the logical PDF font carrying `/ToUnicode`, CMap, and
    /// `/Encoding`). Used to distinguish complete embedded-font recovery from
    /// missing visible output. Parses `self`'s face at most once per call.
    #[doc(hidden)]
    pub fn fallback_glyphs_complete(
        &self,
        meaning_font: &LoadedFont,
        glyphs: impl IntoIterator<Item = (u16, u32, u32, u8)>,
    ) -> bool {
        let face = if self.is_type3() || self.type1.is_some() {
            None
        } else {
            parsed_face(self)
        };
        let mut cmap_budget = CMAP_PROBE_BUDGET;
        let mut confirmed_blank_gids: crate::HashSet<u16> = crate::HashSet::new();
        glyphs
            .into_iter()
            .all(|(glyph_id, code, raw_code, length)| {
                let state = match &face {
                    Some(face) if glyph_id != 0 => self.face_glyph_state(glyph_id, face),
                    Some(_) => GlyphState::Missing,
                    None if self.is_type3() || self.type1.is_some() => self.glyph_state(glyph_id),
                    None => GlyphState::Missing,
                };
                match state {
                    GlyphState::Drawn => true,
                    GlyphState::Blank => {
                        if meaning_font.code_requires_no_ink(code, raw_code, length) {
                            return true;
                        }
                        // An embedded face that stayed primary can hold genuine
                        // non-visible blank glyphs (e.g. an unmapped space CID
                        // whose own `cmap` maps only to whitespace) alongside
                        // another source code on the same CID that triggered
                        // substitution via `/ToUnicode`.
                        if !self.is_substitute
                            && meaning_font
                                .direct_code_visibility(code, raw_code, length)
                                .is_none()
                        {
                            if let Some(face) = &face {
                                if let Some(gid) = self.drawing_gid_with_face(glyph_id, face) {
                                    if confirmed_blank_gids.contains(&gid) {
                                        return true;
                                    }
                                    if matches!(
                                        glyf_blank_header(face, gid),
                                        GlyfBlankHeader::ZeroLength
                                            | GlyfBlankHeader::ValidNonEmpty
                                    ) && cmap_maps_visible_to_gid(face, gid, &mut cmap_budget)
                                        == Ok(false)
                                    {
                                        confirmed_blank_gids.insert(gid);
                                        return true;
                                    }
                                }
                            } else if let Some(t1) = &self.type1 {
                                if glyph_id != 0
                                    && !t1.is_gid_unreadable(glyph_id)
                                    && t1.is_blank_gid(glyph_id)
                                {
                                    return true;
                                }
                            }
                        }
                        false
                    }
                    GlyphState::Missing => {
                        meaning_font.code_requires_no_ink(code, raw_code, length)
                    }
                }
            })
    }

    /// Whether this code is known to require no visible ink (whitespace,
    /// control, format, or default-ignorable characters). Unlike
    /// [`blank_is_space`], private-use characters (such as icon-font symbols)
    /// are NOT excused when their outline is missing.
    fn code_requires_no_ink(&self, code: u32, raw_code: u32, length: u8) -> bool {
        let text = self.lookup_to_unicode(code, raw_code, length);
        match text {
            Some(text) => !text.is_empty() && text.chars().all(is_no_ink_char),
            None => self
                .fallback_character(code, raw_code, length)
                .is_some_and(|character| is_no_ink_char(character.character)),
        }
    }

    /// Direct evidence from the PDF font's own text mappings (`/ToUnicode`,
    /// CMap, `/Encoding`, Type 1 built-in encoding, or non-symbolic ASCII) for
    /// whether a shown code represents visible text (`Some(true)`), known
    /// no-ink/whitespace (`Some(false)`), or unknown (`None`).
    pub(crate) fn direct_code_visibility(
        &self,
        code: u32,
        raw_code: u32,
        length: u8,
    ) -> Option<bool> {
        if let Some(text) = self.lookup_to_unicode(code, raw_code, length) {
            if !text.is_empty() {
                return Some(is_visible_text(text));
            }
        }
        if matches!(self.font_type, PdfFontType::Type0CidType2) {
            if let Some(decoded) = self
                .cid_cmap
                .as_ref()
                .and_then(|cm| cm.decode_text(code, length))
            {
                let mut buf = String::new();
                decoded.push_to(&mut buf);
                if !buf.is_empty() {
                    return Some(is_visible_text(&buf));
                }
            }
            return None;
        }
        if let Ok(b) = u8::try_from(code) {
            if let Some(name) = self.simple_glyph_name(b) {
                if let Some(s) = crate::glyph_list::glyph_name_to_string(name) {
                    if !s.is_empty() {
                        return Some(is_visible_text(&s));
                    }
                }
            } else if !self.symbolic && (32..=255).contains(&code) && code != 0x7f {
                let ch = char::from_u32(code)?;
                let mut buf = [0u8; 4];
                return Some(is_visible_text(ch.encode_utf8(&mut buf)));
            }
        }
        None
    }

    /// Whether a blank glyph is right for this code: the font's meaning for it is
    /// known and is whitespace or invisible.
    fn blank_is_space(&self, code: u32, raw_code: u32, length: u8) -> bool {
        let text = self
            .to_unicode
            .as_ref()
            .and_then(|map| map.lookup(raw_code).or_else(|| map.lookup(code)));
        match text {
            Some(text) => !is_visible_text(text),
            None => self
                .fallback_character(code, raw_code, length)
                .is_some_and(|character| {
                    let mut buffer = [0; 4];
                    !is_visible_text(character.character.encode_utf8(&mut buffer))
                }),
        }
    }

    /// Whether the glyph has a drawable, non-empty outline.
    #[doc(hidden)]
    pub fn glyph_drawn(&self, glyph_id: u16) -> bool {
        self.glyph_state(glyph_id) == GlyphState::Drawn
    }

    fn fallback_character(&self, code: u32, raw_code: u32, length: u8) -> Option<FontCharacter> {
        let text = self.lookup_to_unicode(code, raw_code, length);
        // A ToUnicode string that is not one character (`fi`) still leaves the
        // font's own identity for the code; it is never cut to its first scalar.
        let character = if let Some(character) = text.and_then(FontCharacter::from_text) {
            character
        } else if matches!(self.font_type, PdfFontType::Type0CidType2) {
            let cmap = self.cid_cmap.as_ref()?;
            cmap.decode_text(code, length)?.font_character()?
        } else if let Some(name) = self.simple_glyph_name(code as u8) {
            FontCharacter::from_text(&crate::glyph_list::glyph_name_to_string(name)?)?
        } else if text.is_none()
            && !self.symbolic
            && self.encoding.is_none()
            && length == 1
            && (32..=255).contains(&code)
        {
            FontCharacter {
                character: char::from_u32(code)?,
                variation_selector: None,
            }
        } else {
            return None;
        };
        // Private-use meanings belong to the original font. Callers may query
        // explicit PUA coverage through the provider, but automatic substitution
        // must not assume another font assigns the same meaning.
        if matches!(character.character as u32, 0xE000..=0xF8FF | 0xF0000..=0xFFFFD | 0x100000..=0x10FFFD)
        {
            return None;
        }
        Some(character)
    }

    fn fallback_hints(&self) -> SubstituteHints {
        let name = crate::system::strip_subset_prefix(&self.base_font).to_ascii_lowercase();
        let face = self
            .font_data
            .as_ref()
            .and_then(|bytes| ttf_parser::Face::parse(bytes, self.face_index).ok());
        SubstituteHints {
            bold: name.contains("bold") || face.as_ref().is_some_and(|face| face.is_bold()),
            italic: name.contains("italic")
                || name.contains("oblique")
                || face.as_ref().is_some_and(|face| face.is_italic()),
            ..Default::default()
        }
    }
}

struct OutlineProbe;
impl ttf_parser::OutlineBuilder for OutlineProbe {
    fn move_to(&mut self, _: f32, _: f32) {}
    fn line_to(&mut self, _: f32, _: f32) {}
    fn quad_to(&mut self, _: f32, _: f32, _: f32, _: f32) {}
    fn curve_to(&mut self, _: f32, _: f32, _: f32, _: f32, _: f32, _: f32) {}
    fn close(&mut self) {}
}

/// Outline failures and valid empty glyphs need distinct fallback decisions.
pub(crate) fn usable_glyph(face: &ttf_parser::Face<'_>, gid: u16) -> bool {
    gid != 0 && readable_glyph(face, gid)
}

pub(crate) fn physical_glyph_state(face: &ttf_parser::Face<'_>, gid: u16) -> GlyphState {
    if gid == 0 || gid >= face.number_of_glyphs() {
        GlyphState::Missing
    } else if face
        .outline_glyph(ttf_parser::GlyphId(gid), &mut OutlineProbe)
        .is_some()
    {
        GlyphState::Drawn
    } else if readable_glyph(face, gid) {
        GlyphState::Blank
    } else {
        GlyphState::Missing
    }
}

/// Characters known to require no visible ink: whitespace, controls, format
/// characters, and default-ignorable code points. Excludes private-use scalars,
/// which icon fonts routinely map to visible symbols.
fn is_no_ink_char(character: char) -> bool {
    character.is_whitespace()
        || character.is_control()
        || matches!(
            character as u32,
            0xAD | 0x34F
                | 0x61C
                | 0x115F..=0x1160
                | 0x17B4..=0x17B5
                | 0x180B..=0x180F
                | 0x200B..=0x200F
                | 0x202A..=0x202E
                | 0x2060..=0x206F
                | 0x2800
                | 0x3164
                | 0xFE00..=0xFE0F
                | 0xFEFF
                | 0xFFA0
                | 0xFFF0..=0xFFF8
                | 0x1BCA0..=0x1BCA3
                | 0x1D173..=0x1D17A
                | 0xE0000..=0xE0FFF
        )
}

fn is_visible_char(character: char) -> bool {
    !is_no_ink_char(character)
        && !matches!(
            character as u32,
            0xE000..=0xF8FF | 0xF0000..=0xFFFFD | 0x100000..=0x10FFFD
        )
}

/// Whether text names something visible: at least one character that is not
/// whitespace, a control or format character, a default-ignorable code point,
/// or private use (whose meaning belongs to one font).
#[doc(hidden)]
pub fn is_visible_text(text: &str) -> bool {
    text.chars().any(is_visible_char)
}

/// The glyph exists and its outline, possibly empty, is well formed.
pub(crate) fn readable_glyph(face: &ttf_parser::Face<'_>, gid: u16) -> bool {
    if gid >= face.number_of_glyphs() {
        return false;
    }
    if face
        .outline_glyph(ttf_parser::GlyphId(gid), &mut OutlineProbe)
        .is_some()
    {
        return true;
    }
    if let Some(cff) = face.tables().cff {
        return matches!(
            cff.outline(ttf_parser::GlyphId(gid), &mut OutlineProbe),
            Err(ttf_parser::CFFError::ZeroBBox)
        );
    }
    if let Some(cff) = face.tables().cff2 {
        return matches!(
            cff.outline(
                face.variation_coordinates(),
                ttf_parser::GlyphId(gid),
                &mut OutlineProbe
            ),
            Err(ttf_parser::CFFError::ZeroBBox)
        );
    }
    let raw = face.raw_face();
    let Some(head) = raw.table(ttf_parser::Tag::from_bytes(b"head")) else {
        return false;
    };
    let Some(loca) = raw.table(ttf_parser::Tag::from_bytes(b"loca")) else {
        return false;
    };
    let Some(glyf) = raw.table(ttf_parser::Tag::from_bytes(b"glyf")) else {
        return false;
    };
    let long = head.get(50..52) == Some(&[0, 1]);
    let offset = |index: usize| -> Option<usize> {
        if long {
            Some(u32::from_be_bytes(loca.get(index * 4..index * 4 + 4)?.try_into().ok()?) as usize)
        } else {
            Some(
                u16::from_be_bytes(loca.get(index * 2..index * 2 + 2)?.try_into().ok()?) as usize
                    * 2,
            )
        }
    };
    let (Some(start), Some(end)) = (offset(gid as usize), offset(gid as usize + 1)) else {
        return false;
    };
    if start > end || end > glyf.len() {
        return false;
    }
    start == end || (end - start >= 10 && glyf.get(start..start + 2) == Some(&[0, 0]))
}

/// Maximum work units spent scanning a font's `cmap` subtables across one
/// `has_usable_embedded_outlines` probe or across one page for a single font.
/// Exhausting the budget stops early and leaves the blank-glyph probe
/// inconclusive rather than stalling page load.
pub(crate) const CMAP_PROBE_BUDGET: usize = 4096;

/// Maximum reverse-`cmap` work units any single unmapped blank glyph may spend
/// from the per-font [`CMAP_PROBE_BUDGET`], so a blank space in a large
/// format-12 `cmap` cannot exhaust recovery for later visible glyphs.
const PER_GLYPH_CMAP_BUDGET: usize = 256;

fn is_visible_scalar(cp: u32) -> bool {
    char::from_u32(cp).is_some_and(is_visible_char)
}

fn spend_budget(budget: &mut usize) -> Result<(), ()> {
    if *budget == 0 {
        return Err(());
    }
    *budget -= 1;
    Ok(())
}

fn read_u16_be(data: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_be_bytes(
        data.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn read_u32_be(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        data.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

/// Whether `[start..=end]` contains at least one visible Unicode scalar. Wide
/// non-visible/invalid blocks are jumped in O(1) so even `[0..=u32::MAX]`
/// completes within a few budget units.
fn range_has_visible_scalar(start: u32, end: u32, budget: &mut usize) -> Result<bool, ()> {
    let mut cp = start;
    let hi = end.min(0x10_FFFF);
    while cp <= hi {
        spend_budget(budget)?;
        cp = match cp {
            0x0000..=0x0020 => 0x0021,
            0x007F..=0x00A0 => 0x00A1,
            0xD800..=0xF8FF => 0xF900,
            0xE0000..=0xE0FFF => 0xE1000,
            0xF0000..=0x10FFFD => 0x10FFFE,
            _ => {
                if is_visible_scalar(cp) {
                    return Ok(true);
                }
                cp + 1
            }
        };
    }
    Ok(false)
}

/// Range-aware, budgeted check for whether any Unicode `cmap` subtable maps a
/// visible scalar to `target_gid`. Returns `Err(())` when `budget` is exhausted
/// (inconclusive), avoiding `ttf-parser`'s unbounded `Subtable::codepoints`
/// range expansion.
fn cmap_maps_visible_to_gid(
    face: &ttf_parser::Face<'_>,
    target_gid: u16,
    budget: &mut usize,
) -> Result<bool, ()> {
    if target_gid == 0 {
        return Ok(false);
    }
    let Some(cmap) = face.raw_face().table(ttf_parser::Tag::from_bytes(b"cmap")) else {
        return Ok(false);
    };
    let Some(num_tables) = read_u16_be(cmap, 2) else {
        return Ok(false);
    };
    for table_idx in 0..usize::from(num_tables) {
        spend_budget(budget)?;
        let rec = 4 + table_idx * 8;
        let (Some(platform_id), Some(encoding_id), Some(offset)) = (
            read_u16_be(cmap, rec),
            read_u16_be(cmap, rec + 2),
            read_u32_be(cmap, rec + 4),
        ) else {
            break;
        };
        let Some(subtable) = cmap.get(offset as usize..) else {
            continue;
        };
        let Some(format) = read_u16_be(subtable, 0) else {
            continue;
        };
        let is_unicode = match platform_id {
            0 => true,
            3 if encoding_id == 1 => true,
            3 if encoding_id == 10 && matches!(format, 12 | 13) => true,
            _ => false,
        };
        if !is_unicode {
            continue;
        }
        match format {
            0 => {
                let Some(glyphs) = subtable.get(6..262) else {
                    continue;
                };
                for (cp, &gid) in glyphs.iter().enumerate() {
                    spend_budget(budget)?;
                    if u16::from(gid) == target_gid && is_visible_scalar(cp as u32) {
                        return Ok(true);
                    }
                }
            }
            4 => {
                let Some(seg_count_x2) = read_u16_be(subtable, 6) else {
                    continue;
                };
                if seg_count_x2 < 2 || seg_count_x2 % 2 != 0 {
                    continue;
                }
                let seg_count = usize::from(seg_count_x2 / 2);
                let end_base = 14usize;
                let start_base = end_base + seg_count * 2 + 2;
                let delta_base = start_base + seg_count * 2;
                let iro_base = delta_base + seg_count * 2;
                if subtable.len() < iro_base + seg_count * 2 {
                    continue;
                }
                for seg in 0..seg_count {
                    spend_budget(budget)?;
                    let (Some(end), Some(start), Some(delta_u16), Some(iro)) = (
                        read_u16_be(subtable, end_base + seg * 2),
                        read_u16_be(subtable, start_base + seg * 2),
                        read_u16_be(subtable, delta_base + seg * 2),
                        read_u16_be(subtable, iro_base + seg * 2),
                    ) else {
                        break;
                    };
                    if start == 0xFFFF && end == 0xFFFF {
                        break;
                    }
                    if start > end {
                        continue;
                    }
                    if iro == 0 {
                        let cp = target_gid.wrapping_sub(delta_u16);
                        if cp >= start && cp <= end && is_visible_scalar(u32::from(cp)) {
                            return Ok(true);
                        }
                    } else if iro != 0xFFFF {
                        let delta_i16 = delta_u16 as i16;
                        let iro_pos = (iro_base + seg * 2) as u16;
                        for cp in start..=end {
                            spend_budget(budget)?;
                            let offset_bytes = (cp - start).wrapping_mul(2);
                            let pos =
                                usize::from(iro_pos.wrapping_add(offset_bytes).wrapping_add(iro));
                            let Some(glyph_val) = read_u16_be(subtable, pos) else {
                                break;
                            };
                            if glyph_val == 0 {
                                continue;
                            }
                            let mapped = u16::try_from((glyph_val as i16).wrapping_add(delta_i16));
                            if mapped == Ok(target_gid) && is_visible_scalar(u32::from(cp)) {
                                return Ok(true);
                            }
                        }
                    }
                }
            }
            6 => {
                let (Some(first_code), Some(entry_count)) =
                    (read_u16_be(subtable, 6), read_u16_be(subtable, 8))
                else {
                    continue;
                };
                let count = usize::from(entry_count).min(subtable.len().saturating_sub(10) / 2);
                for idx in 0..count {
                    spend_budget(budget)?;
                    let Some(gid) = read_u16_be(subtable, 10 + idx * 2) else {
                        break;
                    };
                    if gid == target_gid && is_visible_scalar(u32::from(first_code) + idx as u32) {
                        return Ok(true);
                    }
                }
            }
            10 => {
                let (Some(start_cp), Some(num_chars)) =
                    (read_u32_be(subtable, 12), read_u32_be(subtable, 16))
                else {
                    continue;
                };
                let count = (num_chars as usize).min(subtable.len().saturating_sub(20) / 2);
                for idx in 0..count {
                    spend_budget(budget)?;
                    let Some(gid) = read_u16_be(subtable, 20 + idx * 2) else {
                        break;
                    };
                    if gid == target_gid
                        && start_cp
                            .checked_add(idx as u32)
                            .is_some_and(is_visible_scalar)
                    {
                        return Ok(true);
                    }
                }
            }
            12 => {
                let Some(num_groups) = read_u32_be(subtable, 12) else {
                    continue;
                };
                let count = (num_groups as usize).min(subtable.len().saturating_sub(16) / 12);
                let target = u32::from(target_gid);
                for g in 0..count {
                    spend_budget(budget)?;
                    let base = 16 + g * 12;
                    let (Some(start_cp), Some(end_cp), Some(start_gid)) = (
                        read_u32_be(subtable, base),
                        read_u32_be(subtable, base + 4),
                        read_u32_be(subtable, base + 8),
                    ) else {
                        break;
                    };
                    if start_cp > end_cp {
                        continue;
                    }
                    if let Some(cp) = target
                        .checked_sub(start_gid)
                        .and_then(|delta| start_cp.checked_add(delta))
                        .filter(|&cp| cp <= end_cp)
                    {
                        if is_visible_scalar(cp) {
                            return Ok(true);
                        }
                    }
                }
            }
            13 => {
                let Some(num_groups) = read_u32_be(subtable, 12) else {
                    continue;
                };
                let count = (num_groups as usize).min(subtable.len().saturating_sub(16) / 12);
                let target = u32::from(target_gid);
                for g in 0..count {
                    spend_budget(budget)?;
                    let base = 16 + g * 12;
                    let (Some(start_cp), Some(end_cp), Some(glyph_id)) = (
                        read_u32_be(subtable, base),
                        read_u32_be(subtable, base + 4),
                        read_u32_be(subtable, base + 8),
                    ) else {
                        break;
                    };
                    if start_cp > end_cp || glyph_id != target {
                        continue;
                    }
                    if range_has_visible_scalar(start_cp, end_cp, budget)? {
                        return Ok(true);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(false)
}

/// Structural classification of a `GlyphState::Blank` record in a `glyf` face
/// before consulting any character-to-glyph mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GlyfBlankHeader {
    /// Zero-length `loca` entry (`start == end`) or non-`glyf` face: preserve
    /// existing CAD repair path and never spend `cmap` budget.
    ZeroLength,
    /// Structurally malformed `glyf`/`loca` header or wiped non-zero-padded
    /// contour payload: definitively corrupt regardless of character code.
    Corrupt,
    /// Well-formed nonempty zero-contour header (e.g. 10–12 byte space header
    /// or zero-contour visible letter): check direct PDF character evidence
    /// first, falling back to a bounded reverse `cmap` probe only if needed.
    ValidNonEmpty,
}

pub(crate) fn glyf_blank_header(face: &ttf_parser::Face<'_>, gid: u16) -> GlyfBlankHeader {
    if face.tables().glyf.is_none() {
        return GlyfBlankHeader::ZeroLength;
    }
    let raw = face.raw_face();
    let Some(head) = raw.table(ttf_parser::Tag::from_bytes(b"head")) else {
        return GlyfBlankHeader::Corrupt;
    };
    let Some(loca) = raw.table(ttf_parser::Tag::from_bytes(b"loca")) else {
        return GlyfBlankHeader::Corrupt;
    };
    let Some(glyf) = raw.table(ttf_parser::Tag::from_bytes(b"glyf")) else {
        return GlyfBlankHeader::Corrupt;
    };
    let long = head.get(50..52) == Some(&[0, 1]);
    let offset = |index: usize| -> Option<usize> {
        if long {
            Some(u32::from_be_bytes(loca.get(index * 4..index * 4 + 4)?.try_into().ok()?) as usize)
        } else {
            Some(
                u16::from_be_bytes(loca.get(index * 2..index * 2 + 2)?.try_into().ok()?) as usize
                    * 2,
            )
        }
    };
    let (Some(start), Some(end)) = (offset(gid as usize), offset(gid as usize + 1)) else {
        return GlyfBlankHeader::Corrupt;
    };
    if start > end || end > glyf.len() {
        return GlyfBlankHeader::Corrupt;
    }
    if start == end {
        return GlyfBlankHeader::ZeroLength;
    }
    let len = end - start;
    if len < 10 || glyf.get(start..start + 2) != Some(&[0, 0]) {
        return GlyfBlankHeader::Corrupt;
    }
    let x_min = i16::from_be_bytes([glyf[start + 2], glyf[start + 3]]);
    let y_min = i16::from_be_bytes([glyf[start + 4], glyf[start + 5]]);
    let x_max = i16::from_be_bytes([glyf[start + 6], glyf[start + 7]]);
    let y_max = i16::from_be_bytes([glyf[start + 8], glyf[start + 9]]);
    if x_min > x_max || y_min > y_max {
        return GlyfBlankHeader::Corrupt;
    }
    // OpenType simple glyph description with `numberOfContours == 0`:
    // 10-byte glyph header, optionally followed by `uint16 instructionLength`,
    // `instructionLength` bytes of hinting instructions (e.g. phantom-point
    // hints on a space glyph), and up to 4-byte `loca` zero padding.
    // A wiped contour record has `instructionLength == 0` with extra trailing
    // bytes beyond the 12-byte padded header, or an out-of-bounds length.
    if len == 11 {
        if glyf[start + 10] != 0 {
            return GlyfBlankHeader::Corrupt;
        }
    } else if len >= 12 {
        let instr_len = u16::from_be_bytes([glyf[start + 10], glyf[start + 11]]) as usize;
        let body_len = 12 + instr_len;
        if body_len > len {
            return GlyfBlankHeader::Corrupt;
        }
        let max_pad = if instr_len == 0 {
            0
        } else {
            (4 - (body_len % 4)) % 4
        };
        let trailing = len - body_len;
        if trailing > max_pad || !glyf[start + body_len..end].iter().all(|&byte| byte == 0) {
            return GlyfBlankHeader::Corrupt;
        }
    }
    GlyfBlankHeader::ValidNonEmpty
}

/// Bounded reverse-`cmap` check for a [`GlyfBlankHeader::ValidNonEmpty`] glyph.
/// Caps per-glyph work at [`PER_GLYPH_CMAP_BUDGET`] deducted from `page_budget`
/// and returns `None` when either budget is exhausted (distinct from confirmed
/// `Some(false)`).
pub(crate) fn glyf_blank_cmap_probe(
    face: &ttf_parser::Face<'_>,
    gid: u16,
    page_budget: &mut usize,
) -> Option<bool> {
    let start = (*page_budget).min(PER_GLYPH_CMAP_BUDGET);
    let mut local = start;
    let res = cmap_maps_visible_to_gid(face, gid, &mut local);
    *page_budget = page_budget.saturating_sub(start - local);
    res.ok()
}

/// Whether a `GlyphState::Blank` glyph in a `glyf` face shows evidence of
/// corruption (malformed header, oversized zeroed-out contour payload, or a
/// zero-contour record hiding a visible character in the face's Unicode cmap).
///
/// Zero-length `loca` entries (`start == end`) and valid 10–12 byte zero-contour
/// space headers return `false`. When `cmap_budget` is exhausted, the probe
/// stops early and returns `false` (inconclusive).
pub(crate) fn glyf_blank_is_corrupt(
    face: &ttf_parser::Face<'_>,
    gid: u16,
    cmap_budget: &mut usize,
) -> bool {
    match glyf_blank_header(face, gid) {
        GlyfBlankHeader::ZeroLength => false,
        GlyfBlankHeader::Corrupt => true,
        GlyfBlankHeader::ValidNonEmpty => {
            cmap_maps_visible_to_gid(face, gid, cmap_budget).unwrap_or(false)
        }
    }
}

/// Whether `gid` has a non-zero or malformed `loca` slice in a `glyf` face
/// (as opposed to a valid zero-length `start == end` blank entry).
pub(crate) fn glyf_has_non_empty_record(face: &ttf_parser::Face<'_>, gid: u16) -> bool {
    let raw = face.raw_face();
    let Some(head) = raw.table(ttf_parser::Tag::from_bytes(b"head")) else {
        return true;
    };
    let Some(loca) = raw.table(ttf_parser::Tag::from_bytes(b"loca")) else {
        return true;
    };
    let Some(glyf) = raw.table(ttf_parser::Tag::from_bytes(b"glyf")) else {
        return true;
    };
    let long = head.get(50..52) == Some(&[0, 1]);
    let offset = |index: usize| -> Option<usize> {
        if long {
            Some(u32::from_be_bytes(loca.get(index * 4..index * 4 + 4)?.try_into().ok()?) as usize)
        } else {
            Some(
                u16::from_be_bytes(loca.get(index * 2..index * 2 + 2)?.try_into().ok()?) as usize
                    * 2,
            )
        }
    };
    let (Some(start), Some(end)) = (offset(gid as usize), offset(gid as usize + 1)) else {
        return true;
    };
    start != end || end > glyf.len()
}

/// Constant-time blank-glyph checks read straight from an embedded TrueType
/// program's `loca` table, for per-glyph checks on every shown glyph.
#[doc(hidden)]
pub struct BlankGlyphTable {
    data: Arc<[u8]>,
    loca: std::ops::Range<usize>,
    glyf: std::ops::Range<usize>,
    long: bool,
    glyph_count: u16,
    /// Run glyph ids are physical GIDs (no CID → GID step).
    direct: bool,
}

impl LoadedFont {
    /// The `loca` view of an embedded `glyf` program. `None` for substitutes,
    /// Type 1, Type 3 and CFF outlines.
    #[doc(hidden)]
    pub fn blank_glyph_table(&self) -> Option<BlankGlyphTable> {
        if self.is_substitute || self.type1.is_some() || self.is_type3() {
            return None;
        }
        // Only the table directory is read; no table is parsed.
        let data = self.font_data.as_ref()?;
        let raw = ttf_parser::RawFace::parse(data, self.face_index).ok()?;
        let table = |tag: &[u8; 4]| raw.table(ttf_parser::Tag::from_bytes(tag));
        let range = |tag: &[u8; 4]| {
            let table = table(tag)?;
            let start = (table.as_ptr() as usize).checked_sub(data.as_ptr() as usize)?;
            Some(start..start + table.len())
        };
        let glyph_count = table(b"maxp")?.get(4..6)?;
        Some(BlankGlyphTable {
            loca: range(b"loca")?,
            glyf: range(b"glyf")?,
            long: table(b"head")?.get(50..52)? == [0, 1],
            glyph_count: u16::from_be_bytes([glyph_count[0], glyph_count[1]]),
            direct: self.unicode_coded() || self.cid_to_gid.is_none(),
            data: Arc::clone(data),
        })
    }
}

impl BlankGlyphTable {
    /// Whether `glyph_id`, as carried in `font`'s glyph runs, maps to a real
    /// glyph whose outline is well formed but empty (a zero-length `loca`
    /// entry or zero contours). `font` is the font the table was taken from.
    pub fn is_blank(&self, font: &LoadedFont, glyph_id: u16) -> bool {
        let gid = match &font.cid_to_gid {
            Some(map) if !self.direct => match map.get(&glyph_id) {
                Some(&gid) => gid,
                None => return false,
            },
            _ => glyph_id,
        };
        self.physical_blank(gid)
    }

    fn physical_blank(&self, gid: u16) -> bool {
        if gid == 0 || gid >= self.glyph_count {
            return false;
        }
        let loca = &self.data[self.loca.clone()];
        let glyf = &self.data[self.glyf.clone()];
        let offset = |index: usize| -> Option<usize> {
            if self.long {
                let bytes = loca.get(index * 4..index * 4 + 4)?;
                Some(u32::from_be_bytes(bytes.try_into().ok()?) as usize)
            } else {
                let bytes = loca.get(index * 2..index * 2 + 2)?;
                Some(u16::from_be_bytes(bytes.try_into().ok()?) as usize * 2)
            }
        };
        let (Some(start), Some(end)) = (offset(gid as usize), offset(gid as usize + 1)) else {
            return false;
        };
        start <= end
            && end <= glyf.len()
            && (start == end || (end - start >= 10 && glyf[start..start + 2] == [0, 0]))
    }
}

/// The font's own face at its configured variation instance.
fn parsed_face(font: &LoadedFont) -> Option<ttf_parser::Face<'_>> {
    let mut face = ttf_parser::Face::parse(font.font_data.as_ref()?, font.face_index).ok()?;
    font.apply_variations(&mut face);
    Some(face)
}

/// A substituted font's glyph for a variation sequence must be the sequence's
/// own glyph, which only a parsed face can confirm.
fn needs_variation_check(font: &LoadedFont, code: u32, raw_code: u32, length: u8) -> bool {
    font.is_substitute
        && font
            .fallback_character(code, raw_code, length)
            .is_some_and(|character| character.variation_selector.is_some())
}

impl FontCache {
    /// Validate original glyphs with one parsed face for a text operation.
    /// The provider retains only bounded booleans and weak font identities.
    pub fn original_glyphs_usable(
        &self,
        font_id: FontId,
        glyphs: impl Iterator<Item = (u16, u32, u32, u8)>,
    ) -> bool {
        // Mixed runs already have page decisions. Let that cheaper cache win
        // instead of probing the same original prefix on every text operation.
        if self.resolution.is_some() {
            return false;
        }
        let (Some(provider), Some(font)) = (self.provider.as_ref(), self.fonts.get(&font_id))
        else {
            return false;
        };
        let key = Arc::as_ptr(font) as usize;
        let mut glyphs = glyphs.enumerate();
        // Steady state: every glyph was checked before. Concurrent renders share
        // the read lock, and nothing is parsed or probed while it is held.
        let mut previous = None;
        let pending = provider.read_glyph_checks(key, |checks| {
            let entry = checks.entries.get(&key);
            for (index, source @ (glyph, code, raw_code, length)) in glyphs.by_ref() {
                if index >= 4096 || (glyph == 0 && !font.is_type3()) {
                    return Err(());
                }
                if previous == Some(source) {
                    continue;
                }
                previous = Some(source);
                let known = entry.and_then(|entry| {
                    let index = entry
                        .glyphs
                        .binary_search_by_key(&glyph, |(gid, _)| *gid)
                        .ok()?;
                    Some(entry.glyphs[index].1.usable(font, code, raw_code, length))
                });
                match known {
                    Some(false) => return Err(()),
                    Some(true) if !needs_variation_check(font, code, raw_code, length) => {}
                    _ => return Ok(Some((index, source))),
                }
            }
            Ok(None)
        });
        let first = match pending {
            Some(Err(())) => return false,
            Some(Ok(None)) => return true,
            Some(Ok(Some(first))) => Some(first),
            // Nothing recorded yet (or a poisoned lock): check everything.
            None => None,
        };
        // New glyphs and variation sequences: probe without holding the lock,
        // then record what was learned in one short write.
        let known: Vec<(u16, GlyphState)> = provider
            .read_glyph_checks(key, |checks| {
                checks.entries.get(&key).map(|entry| entry.glyphs.clone())
            })
            .flatten()
            .unwrap_or_default();
        let mut face = None;
        let mut parsed = false;
        let mut learned: Vec<(u16, GlyphState)> = Vec::new();
        let mut usable_all = true;
        let mut previous = None;
        for (index, source @ (glyph, code, raw_code, length)) in first.into_iter().chain(glyphs) {
            if index >= 4096 || (glyph == 0 && !font.is_type3()) {
                usable_all = false;
                break;
            }
            if previous == Some(source) {
                continue;
            }
            previous = Some(source);
            let lookup = |list: &[(u16, GlyphState)]| {
                list.binary_search_by_key(&glyph, |(gid, _)| *gid)
                    .ok()
                    .map(|index| list[index].1)
            };
            let state = match lookup(&known).or_else(|| lookup(&learned)) {
                Some(state) => state,
                None => {
                    let state = if font.is_type3() || font.type1.is_some() {
                        font.glyph_state(glyph)
                    } else {
                        if !parsed {
                            (face, parsed) = (parsed_face(font), true);
                        }
                        face.as_ref().map_or(GlyphState::Missing, |face| {
                            font.face_glyph_state(glyph, face)
                        })
                    };
                    if let Err(index) = learned.binary_search_by_key(&glyph, |(gid, _)| *gid) {
                        learned.insert(index, (glyph, state));
                    }
                    state
                }
            };
            if !state.usable(font, code, raw_code, length) {
                usable_all = false;
                break;
            }
            if needs_variation_check(font, code, raw_code, length) {
                if !parsed {
                    (face, parsed) = (parsed_face(font), true);
                }
                let character = font.fallback_character(code, raw_code, length);
                if face.as_ref().is_none_or(|face| {
                    character.and_then(|character| character.mapped_glyph_index(face))
                        != font.drawing_gid_with_face(glyph, face)
                }) {
                    usable_all = false;
                    break;
                }
            }
        }
        if !learned.is_empty() {
            provider.with_glyph_checks(key, |checks| {
                let (entry, count) = checks.for_font(font);
                for (glyph, state) in learned {
                    if *count >= 4096 {
                        break;
                    }
                    if let Err(index) = entry.glyphs.binary_search_by_key(&glyph, |(gid, _)| *gid) {
                        entry.glyphs.insert(index, (glyph, state));
                        *count += 1;
                    }
                }
            });
        }
        usable_all
    }

    /// Resolve a positioned PDF glyph without changing its logical advance.
    /// `raw_code` retains UTF-16 source bytes when `code` is a decoded scalar.
    /// Returns the original drawing identity if no reliable replacement exists.
    pub fn resolve_drawing_glyph(
        &mut self,
        original: DrawingGlyph,
        code: u32,
        raw_code: u32,
        length: u8,
        max_bytes: u64,
    ) -> DrawingGlyph {
        let Some(provider) = self.provider.as_ref() else {
            return original;
        };
        let generation = provider.generation();
        let key = ResolutionKey {
            font_id: original.font_id,
            glyph_id: original.glyph_id,
            code,
            raw_code,
            length,
            byte_limit: max_bytes,
        };
        if let Some(cache) = self.resolution.as_mut() {
            if cache.generation != generation {
                cache.decisions.clear();
                cache.generation = generation;
            }
            if let Some(&cached) = cache.decisions.get(&key) {
                return cached;
            }
        }
        let Some(font) = self.fonts.get(&original.font_id) else {
            return original;
        };
        if provider
            .glyph_state(font, original.glyph_id)
            .usable(font, code, raw_code, length)
        {
            let character = font
                .is_substitute
                .then(|| font.fallback_character(code, raw_code, length))
                .flatten();
            let needs_variant = character.is_some_and(|ch| ch.variation_selector.is_some())
                && font
                    .font_data
                    .as_ref()
                    .and_then(|bytes| ttf_parser::Face::parse(bytes, font.face_index).ok())
                    .is_none_or(|face| {
                        character.and_then(|ch| ch.mapped_glyph_index(&face))
                            != font.drawing_gid_with_face(original.glyph_id, &face)
                    });
            if !needs_variant {
                if let Some(cache) = self.resolution.as_mut() {
                    if cache.decisions.len() >= 4096 {
                        cache.decisions.clear();
                    }
                    cache.decisions.insert(key, original);
                }
                return original;
            }
        }
        let cache = self.resolution.get_or_insert_with(Default::default);
        if cache.generation != generation {
            cache.decisions.clear();
            cache.generation = generation;
        }
        if let Some(&cached) = cache.decisions.get(&key) {
            return cached;
        }
        // Shared ownership is only needed across slow-path admission. Warm
        // decisions must not increment/decrement an Arc for every glyph.
        let provider = Arc::clone(provider);
        let Some(font) = self.fonts.get(&original.font_id).cloned() else {
            return original;
        };
        let resolved = font
            .fallback_character(code, raw_code, length)
            .and_then(|character| provider.resolve_character(character, font.fallback_hints()))
            .and_then(|selected| {
                let font_id = match self
                    .resolution
                    .as_ref()?
                    .faces
                    .get(&(original.font_id, selected.font_id))
                    .copied()
                {
                    Some(id) => id,
                    None => {
                        let mut loaded = LoadedFont::new_substitute(
                            PdfFontType::TrueType,
                            font.base_font.clone(),
                            selected.font.data,
                            selected.font.face_index,
                            CidWidths::new(1000.0),
                        )?;
                        loaded.variations.clone_from(&font.variations);
                        let id =
                            self.admit_drawing_font(Arc::new(loaded), original.font_id, max_bytes)?;
                        self.resolution
                            .as_mut()?
                            .faces
                            .insert((original.font_id, selected.font_id), id);
                        id
                    }
                };
                Some(DrawingGlyph {
                    font_id,
                    glyph_id: selected.glyph_id,
                })
            })
            .unwrap_or(original);
        let cache = self.resolution.as_mut().expect("resolution cache exists");
        if cache.decisions.len() >= 4096 {
            cache.decisions.clear();
        }
        cache.decisions.insert(key, resolved);
        resolved
    }

    /// If `original` is a substituted font that lacks a drawable outline for
    /// this glyph and an already-loaded embedded original was retained for it,
    /// resolve the glyph from the surviving embedded face.
    #[doc(hidden)]
    pub fn resolve_embedded_companion_glyph(
        &mut self,
        original: DrawingGlyph,
        code: u32,
        raw_code: u32,
        length: u8,
        max_bytes: u64,
    ) -> Option<DrawingGlyph> {
        self.resolve_embedded_companion_glyph_with(
            original,
            code,
            raw_code,
            length,
            max_bytes,
            |_, _| crate::DeferredCompanionLoad::Unavailable,
        )
    }

    /// Like [`Self::resolve_embedded_companion_glyph`], invoking `load_deferred`
    /// only when the primary substitute lacks a drawable outline for this glyph
    /// and the paired companion is still an unloaded resource reference. The
    /// loaded companion is admitted (and charged against `max_bytes`) only if
    /// it actually draws this glyph. Definitive load or budget failures are
    /// remembered per `FontId` so subsequent missing glyphs at the same or
    /// smaller remaining budget do not re-decode the stream.
    #[doc(hidden)]
    pub fn resolve_embedded_companion_glyph_with(
        &mut self,
        original: DrawingGlyph,
        code: u32,
        raw_code: u32,
        length: u8,
        max_bytes: u64,
        load_deferred: impl FnMut(&crate::CompanionSource, u64) -> crate::DeferredCompanionLoad,
    ) -> Option<DrawingGlyph> {
        self.resolve_embedded_companion_glyph_in_run(
            original,
            code,
            raw_code,
            length,
            max_bytes,
            None,
            None,
            None,
            load_deferred,
        )
    }

    /// Per-show companion resolver that reuses the text show's cached
    /// embedded-glyph classification (preserving the per-font `cmap` budget)
    /// and caches a constructed deferred candidate in `run_candidate` across
    /// multiple distinct glyph misses within one text operation without
    /// permanently admitting it until it draws a glyph.
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub fn resolve_embedded_companion_glyph_in_run(
        &mut self,
        original: DrawingGlyph,
        code: u32,
        raw_code: u32,
        length: u8,
        max_bytes: u64,
        original_unreadable: Option<bool>,
        cmap_budget: Option<&mut usize>,
        run_candidate: Option<&mut Option<Arc<LoadedFont>>>,
        mut load_deferred: impl FnMut(&crate::CompanionSource, u64) -> crate::DeferredCompanionLoad,
    ) -> Option<DrawingGlyph> {
        if self.embedded_companions.is_empty() {
            return None;
        }
        let kind = self.embedded_companions.get(&original.font_id)?.kind;
        let primary = self.fonts.get(&original.font_id)?;
        match kind {
            crate::CompanionKind::EmbeddedOriginal => {
                let primary_state = match self.provider.as_ref() {
                    Some(provider) => provider.glyph_state(primary, original.glyph_id),
                    None => primary.glyph_state(original.glyph_id),
                };
                if primary_state == GlyphState::Drawn
                    || (primary_state == GlyphState::Blank
                        && primary.code_requires_no_ink(code, raw_code, length))
                {
                    return None;
                }
            }
            crate::CompanionKind::DescriptorSubstitute { .. } => {
                let is_unreadable = match original_unreadable {
                    Some(known) => known,
                    None => {
                        original.glyph_id != 0
                            && match cmap_budget {
                                Some(budget) => primary.is_shown_embedded_glyph_unreadable(
                                    original.glyph_id,
                                    code,
                                    raw_code,
                                    length,
                                    budget,
                                ),
                                None => {
                                    let mut budget = CMAP_PROBE_BUDGET;
                                    primary.is_shown_embedded_glyph_unreadable(
                                        original.glyph_id,
                                        code,
                                        raw_code,
                                        length,
                                        &mut budget,
                                    )
                                }
                            }
                    }
                };
                if !is_unreadable {
                    return None;
                }
            }
        }

        // Reclaimable standby companions held by other fonts do not count
        // against the budget available to a glyph that actually needs its
        // companion: `admit_drawing_font` reclaims unused standby if needed.
        let miss_key = (code, raw_code, length);
        let remaining = max_bytes.saturating_sub(self.budget.bytes_used());
        let program_count = self.budget.programs.len();
        let current_gen = self.provider.as_ref().map(|p| p.generation());
        if let Some(entry) = self.embedded_companions.get_mut(&original.font_id) {
            if entry.descriptor_spec.is_some() && entry.provider_generation != current_gen {
                entry.failed_budget = None;
                entry.missed_codes.clear();
                entry.provider_generation = current_gen;
            }
        }
        let (companion, existing_id, from_standby, was_deferred) = {
            let entry = self.embedded_companions.get(&original.font_id)?;
            if let Some(id) = entry.admitted {
                (Arc::clone(self.fonts.get(&id)?), Some(id), false, false)
            } else if let Some(comp) = &entry.standby {
                (Arc::clone(comp), None, true, false)
            } else {
                if entry.missed_codes.contains(&miss_key) {
                    return None;
                }
                if let Some(spec) = entry.descriptor_spec.as_deref() {
                    if let Some(cached) = run_candidate.as_deref().and_then(|slot| slot.as_ref()) {
                        (Arc::clone(cached), None, false, true)
                    } else {
                        // Do not short-circuit on `remaining == 0`: a substitute
                        // that shares an already-admitted system program with
                        // zero owned mapping bytes still fits at `max_bytes`.
                        if entry.failed_budget.is_some_and(|limit| remaining <= limit)
                            && entry.failed_program_count == program_count
                        {
                            return None;
                        }
                        let primary = self.fonts.get(&original.font_id)?;
                        match build_descriptor_substitute(primary, spec, self.provider.as_deref()) {
                            Some(sub) => {
                                let loaded = Arc::new(sub);
                                if self.budget.admission_total(&loaded, max_bytes).is_none() {
                                    if let Some(entry) =
                                        self.embedded_companions.get_mut(&original.font_id)
                                    {
                                        entry.record_budget_failure(remaining, program_count);
                                    }
                                    return None;
                                }
                                if let Some(slot) = run_candidate {
                                    *slot = Some(Arc::clone(&loaded));
                                }
                                (loaded, None, false, true)
                            }
                            None => {
                                if let Some(entry) =
                                    self.embedded_companions.get_mut(&original.font_id)
                                {
                                    entry.record_budget_failure(u64::MAX, program_count);
                                }
                                return None;
                            }
                        }
                    }
                } else {
                    match &entry.source {
                        crate::CompanionSource::None => return None,
                        deferred @ (crate::CompanionSource::Object(_)
                        | crate::CompanionSource::FormInlineFont { .. }) => {
                            if let Some(cached) =
                                run_candidate.as_deref().and_then(|slot| slot.as_ref())
                            {
                                (Arc::clone(cached), None, false, true)
                            } else {
                                if remaining == 0
                                    || entry.failed_budget.is_some_and(|limit| remaining <= limit)
                                {
                                    return None;
                                }
                                match load_deferred(deferred, remaining) {
                                    crate::DeferredCompanionLoad::Loaded(loaded) => {
                                        if self.budget.admission_total(&loaded, max_bytes).is_none()
                                        {
                                            // The loaded companion's full cache footprint
                                            // exceeds `remaining` even before checking
                                            // individual glyphs; remember the budget
                                            // failure so later missing glyphs do not
                                            // re-decode the stream repeatedly.
                                            if let Some(entry) =
                                                self.embedded_companions.get_mut(&original.font_id)
                                            {
                                                entry.record_budget_failure(
                                                    remaining,
                                                    program_count,
                                                );
                                            }
                                            return None;
                                        }
                                        if let Some(slot) = run_candidate {
                                            *slot = Some(Arc::clone(&loaded));
                                        }
                                        (loaded, None, false, true)
                                    }
                                    crate::DeferredCompanionLoad::Unavailable => return None,
                                    crate::DeferredCompanionLoad::FailedForBudget => {
                                        if let Some(entry) =
                                            self.embedded_companions.get_mut(&original.font_id)
                                        {
                                            entry.record_budget_failure(remaining, program_count);
                                        }
                                        return None;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        };
        let primary = self.fonts.get(&original.font_id)?;
        let companion_gid = if matches!(primary.font_type, PdfFontType::Type0CidType2)
            && matches!(kind, crate::CompanionKind::DescriptorSubstitute { .. })
        {
            companion.resolve_composite_substitute_gid(primary, code, raw_code, length)
        } else if matches!(companion.font_type, PdfFontType::Type0CidType2) {
            if companion.uses_direct_composite_gids() {
                companion.composite_direct_gid(code, raw_code, length)
            } else {
                let cid = companion
                    .cid_cmap
                    .as_ref()
                    .map(|c| c.code_to_cid(code, length))
                    .unwrap_or(code);
                u16::try_from(cid).ok()
            }
        } else {
            u16::try_from(code)
                .ok()
                .and_then(|c| companion.code_to_gid(c))
        };
        let Some(companion_gid) = companion_gid.filter(|&gid| {
            let state = match self.provider.as_ref() {
                Some(provider) => provider.glyph_state(&companion, gid),
                None => companion.glyph_state(gid),
            };
            state == GlyphState::Drawn
        }) else {
            if was_deferred {
                if let Some(entry) = self.embedded_companions.get_mut(&original.font_id) {
                    if entry.missed_codes.len() < crate::MAX_COMPANION_MISS_CACHE {
                        entry.missed_codes.insert(miss_key);
                    }
                }
            }
            return None;
        };
        let font_id = match existing_id {
            Some(id) => id,
            None => {
                if from_standby {
                    self.budget.admission_total(&companion, max_bytes)?;
                    if let Some(entry) = self.embedded_companions.get_mut(&original.font_id) {
                        entry.standby = None;
                    }
                    self.recompute_standby_bytes();
                }
                // Release this entry's own optional lookup-name override before
                // `admit_drawing_font` runs standby reclamation so admitting
                // this companion does not strip a lower-ID font's lookup name
                // when releasing its own name already frees enough space.
                // Restore the original spec intact if admission fails.
                let released_spec = self
                    .embedded_companions
                    .get_mut(&original.font_id)
                    .and_then(|entry| {
                        let had_extra = entry
                            .descriptor_spec
                            .as_ref()
                            .is_some_and(|spec| spec.extra_bytes() > 0);
                        entry.descriptor_spec.take().map(|spec| (spec, had_extra))
                    });
                if released_spec
                    .as_ref()
                    .is_some_and(|(_, had_extra)| *had_extra)
                {
                    self.recompute_standby_bytes();
                }
                let Some(id) = self.admit_drawing_font(companion, original.font_id, max_bytes)
                else {
                    if let Some((spec, had_extra)) = released_spec {
                        if let Some(entry) = self.embedded_companions.get_mut(&original.font_id) {
                            entry.descriptor_spec = Some(spec);
                        }
                        if had_extra {
                            self.recompute_standby_bytes();
                        }
                    }
                    if was_deferred {
                        // `FontCacheBudget` is non-evicting, so a deferred
                        // companion that exceeded `remaining` cannot fit later
                        // at the same or smaller remaining budget for this
                        // shared-program set either.
                        if let Some(entry) = self.embedded_companions.get_mut(&original.font_id) {
                            entry.record_budget_failure(remaining, program_count);
                        }
                    }
                    return None;
                };
                if let Some(entry) = self.embedded_companions.get_mut(&original.font_id) {
                    entry.admitted = Some(id);
                }
                id
            }
        };
        Some(DrawingGlyph {
            font_id,
            glyph_id: companion_gid,
        })
    }

    fn admit_drawing_font(
        &mut self,
        font: Arc<LoadedFont>,
        original_font_id: FontId,
        max_bytes: u64,
    ) -> Option<FontId> {
        // Drawing faces have no PDF resource name, so hostile resource names
        // cannot collide with an internal alias or redirect an existing run.
        let total = self.budget.admission_total(&font, max_bytes)?;
        let id = self.next_free_id()?;
        self.budget.record(&font, total);
        self.fonts.insert(id, font);
        let root_id = self
            .drawing_font_originals
            .get(&original_font_id)
            .copied()
            .unwrap_or(original_font_id);
        self.drawing_font_originals.insert(id, root_id);
        self.reclaim_standby_for(total, max_bytes);
        Some(id)
    }
}

/// Build a descriptor-based substitute face directly from an embedded primary
/// font's logical mappings and its retained [`DescriptorSubstituteSpec`],
/// allowing on-demand recovery without a `PdfFile` and deferring budget
/// deduplication to `FontCacheBudget::admission_total`.
fn build_descriptor_substitute(
    primary: &LoadedFont,
    spec: &crate::DescriptorSubstituteSpec,
    provider: Option<&FontProvider>,
) -> Option<LoadedFont> {
    let lookup_name = spec.lookup_name.as_deref().unwrap_or(&primary.base_font);
    if matches!(primary.font_type, PdfFontType::Type0CidType2) {
        let can_substitute = primary.cid_cmap.as_ref().is_some_and(|cmap| {
            cmap.codes_are_unicode || cmap.is_legacy() || cmap.collection().is_some()
        }) || primary.to_unicode.is_some();
        if !can_substitute {
            return None;
        }
        let matched = match provider {
            Some(p) => p.find_font(lookup_name, spec.hints, spec.ordering),
            None => crate::system::find_system_font(lookup_name, spec.hints, spec.ordering),
        }?;
        // Descriptor substitutes for Type 0 fonts are drawing-only faces whose
        // physical GIDs are resolved per source code against `primary`'s
        // `/ToUnicode` and `/Encoding` CMap (preserving original CID advances
        // and avoiding hash-order collisions when multiple source codes share
        // one damaged CID).
        let mut sub = LoadedFont::new_substitute(
            PdfFontType::TrueType,
            primary.base_font.clone(),
            matched.data,
            matched.face_index,
            CidWidths::new(1000.0),
        )?;
        sub.variations = primary.variations.clone();
        Some(sub)
    } else {
        let matched = match provider {
            Some(p) => p.find_font(lookup_name, spec.hints, None),
            None => crate::system::find_system_font(lookup_name, spec.hints, None),
        }?;
        let mut cid_widths = primary.cid_widths.clone();
        if cid_widths.is_empty() && !cid_widths.has_authoritative_default() {
            if let Some(metrics) = crate::standard_fonts::lookup(lookup_name) {
                for (code, &w) in metrics.widths.iter().enumerate() {
                    if w > 0 {
                        cid_widths.set(code as u16, w as f64);
                    }
                }
            }
        }
        let mut sub = LoadedFont::new_substitute(
            primary.font_type.clone(),
            primary.base_font.clone(),
            matched.data,
            matched.face_index,
            cid_widths,
        )?;
        sub.encoding = primary.encoding.clone();
        sub.to_unicode = primary.to_unicode.clone();
        sub.symbolic = primary.symbolic;
        sub.variations = primary.variations.clone();
        sub.inherit_type1_encoding_from(primary);
        Some(sub)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check_font() -> Arc<LoadedFont> {
        Arc::new(LoadedFont::new_with_data(
            PdfFontType::TrueType,
            "Checks".into(),
            include_bytes!("../tests/fixtures/coverage-primary.ttf").to_vec(),
            CidWidths::new(1000.0),
        ))
    }

    #[test]
    fn glyph_checks_reuse_immutable_fonts_without_retaining_the_program() {
        let provider = FontProvider::memory_only();
        let font = check_font();
        let data = Arc::downgrade(font.font_data.as_ref().unwrap());
        let id = font.unicode_glyph('A' as u32).unwrap().0;
        assert!(provider.has_mapped_glyph(&font, id));
        assert!(provider.has_mapped_glyph(&font, id));
        let mut entries = 0;
        provider.each_glyph_check_shard(|cache| entries += cache.entries.len());
        assert_eq!(entries, 1);
        drop(font);
        assert!(
            data.upgrade().is_none(),
            "cached checks must not retain font bytes"
        );
    }

    #[test]
    fn changed_maps_and_variation_instances_cannot_reuse_a_glyph_check() {
        let provider = FontProvider::memory_only();
        let mut font = check_font();
        let id = font.unicode_glyph('A' as u32).unwrap().0;
        assert!(provider.has_mapped_glyph(&font, id));
        let previous = Arc::as_ptr(&font);
        assert!(
            Arc::get_mut(&mut font).is_none(),
            "checked font is immutable while its weak identity exists"
        );
        let mut owned = Arc::try_unwrap(font)
            .unwrap_or_else(|_| panic!("cache retained strong font ownership"));
        owned.cid_to_gid = Some(HashMap::new());
        font = Arc::new(owned);
        assert_ne!(
            Arc::as_ptr(&font),
            previous,
            "weak identity prevents allocation reuse"
        );
        assert!(!provider.has_mapped_glyph(&font, id));
        let mut other = check_font();
        Arc::get_mut(&mut other)
            .unwrap()
            .set_variations(Some(900.0), None, None, false);
        assert!(provider.has_mapped_glyph(&other, id));
    }

    #[test]
    fn glyph_checks_bound_dead_font_identities_and_negative_results() {
        let provider = FontProvider::memory_only();
        let font = check_font();
        for glyph in 1..5000 {
            provider.has_mapped_glyph(&font, glyph);
        }
        let mut negative = false;
        provider.each_glyph_check_shard(|checks| {
            assert!(checks.count <= 4096);
            negative |= checks
                .entries
                .values()
                .flat_map(|font| &font.glyphs)
                .any(|(_, state)| *state == GlyphState::Missing);
        });
        assert!(negative);
    }

    #[test]
    fn glyph_checks_bound_empty_font_entries() {
        let provider = Arc::new(FontProvider::memory_only());
        for _ in 0..1100 {
            let mut cache = FontCache::new().with_provider(Arc::clone(&provider));
            let id = cache.insert(
                "Missing".into(),
                LoadedFont::new_placeholder("Missing".into()),
            );
            assert!(!cache.original_glyphs_usable(id, [(0, 65, 65, 1)].into_iter()));
            provider.each_glyph_check_shard(|checks| {
                assert!(checks.entries.len() <= 1024);
                assert_eq!(checks.count, 0);
            });
        }
    }

    #[test]
    fn page_decisions_are_bounded_across_distinct_unrecoverable_codes() {
        let provider = Arc::new(crate::FontProvider::memory_only());
        let mut cache = FontCache::new().with_provider(provider);
        let id = cache.insert("F".into(), LoadedFont::new_placeholder("Missing".into()));
        let original = DrawingGlyph {
            font_id: id,
            glyph_id: 0,
        };
        for code in 0x10000..0x13000 {
            assert_eq!(
                cache.resolve_drawing_glyph(original, code, code, 4, 1024),
                original
            );
            assert!(cache.resolution.as_ref().unwrap().decisions.len() <= 4096);
        }
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.bytes_used(), 0);
    }
}
