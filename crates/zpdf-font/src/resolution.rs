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
    fn usable(self, font: &LoadedFont, code: u32, raw_code: u32) -> bool {
        match self {
            GlyphState::Missing => false,
            GlyphState::Drawn => true,
            GlyphState::Blank => !font.blank_hides_text(code, raw_code),
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
            return mapped(type1.glyph_outline_by_gid(glyph_id).is_some());
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
    pub fn blank_hides_text(&self, code: u32, raw_code: u32) -> bool {
        !self.is_substitute
            && self
                .to_unicode
                .as_ref()
                .and_then(|map| map.lookup(raw_code).or_else(|| map.lookup(code)))
                .is_some_and(is_visible_text)
    }

    /// Whether every `(glyph id, code, raw code)` draws a real glyph: mapped,
    /// readable, and blank only where the code is not visible text. Parses the
    /// program once for the whole batch.
    pub fn glyphs_usable(&self, glyphs: impl IntoIterator<Item = (u16, u32, u32)>) -> bool {
        let face = if self.is_type3() || self.type1.is_some() {
            None
        } else {
            match parsed_face(self) {
                Some(face) => Some(face),
                None => return false,
            }
        };
        glyphs.into_iter().all(|(glyph_id, code, raw_code)| {
            let state = match &face {
                Some(face) if glyph_id != 0 => self.face_glyph_state(glyph_id, face),
                Some(_) => GlyphState::Missing,
                None => self.glyph_state(glyph_id),
            };
            state.usable(self, code, raw_code)
        })
    }

    /// Whether the glyph has a drawable, non-empty outline.
    pub fn glyph_drawn(&self, glyph_id: u16) -> bool {
        self.glyph_state(glyph_id) == GlyphState::Drawn
    }

    fn fallback_character(&self, code: u32, raw_code: u32, length: u8) -> Option<FontCharacter> {
        let text = self
            .to_unicode
            .as_ref()
            .and_then(|mapping| mapping.lookup(raw_code).or_else(|| mapping.lookup(code)));
        // A ToUnicode string that is not one character (`fi`) still leaves the
        // font's own identity for the code; it is never cut to its first scalar.
        let character = if let Some(character) = text.and_then(FontCharacter::from_text) {
            character
        } else if matches!(self.font_type, PdfFontType::Type0CidType2) {
            let cmap = self.cid_cmap.as_ref()?;
            cmap.decode_text(code, length)?.font_character()?
        } else if let Some(name) = self
            .encoding
            .as_ref()
            .and_then(|encoding| encoding.glyph_name(code as u8))
        {
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

fn physical_glyph_state(face: &ttf_parser::Face<'_>, gid: u16) -> GlyphState {
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

/// Whether text names something visible: at least one character that is not
/// whitespace, a control or format character, a default-ignorable code point,
/// or private use (whose meaning belongs to one font).
pub fn is_visible_text(text: &str) -> bool {
    text.chars().any(|character| {
        !character.is_whitespace()
            && !character.is_control()
            && !matches!(
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
                    | 0xE000..=0xF8FF
                    | 0xF0000..=0xFFFFD
                    | 0x100000..=0x10FFFD
            )
    })
}

/// The glyph exists and its outline, possibly empty, is well formed.
fn readable_glyph(face: &ttf_parser::Face<'_>, gid: u16) -> bool {
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

/// Constant-time blank-glyph checks read straight from an embedded TrueType
/// program's `loca` table, for per-glyph checks on every shown glyph.
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
                    Some(entry.glyphs[index].1.usable(font, code, raw_code))
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
            if !state.usable(font, code, raw_code) {
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
            .usable(font, code, raw_code)
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
                        let id = self.admit_drawing_font(Arc::new(loaded), max_bytes)?;
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

    fn admit_drawing_font(&mut self, font: Arc<LoadedFont>, max_bytes: u64) -> Option<FontId> {
        // Drawing faces have no PDF resource name, so hostile resource names
        // cannot collide with an internal alias or redirect an existing run.
        let program_key = font.shared_program_key();
        let added = if program_key.is_some_and(|key| self.shared_programs.contains(&key)) {
            0
        } else {
            font.estimated_cache_bytes()
        };
        let total = self.bytes_used.checked_add(added)?;
        if total > max_bytes {
            return None;
        }
        let id = self.next_free_id()?;
        if let Some(key) = program_key {
            self.shared_programs.insert(key);
        }
        self.bytes_used = total;
        self.fonts.insert(id, font);
        Some(id)
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
