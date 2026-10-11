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
    glyphs: Vec<(u16, bool)>,
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
    fn has_mapped_glyph(&self, font: &Arc<LoadedFont>, glyph: u16) -> bool {
        if glyph == 0 && !font.is_type3() {
            return false;
        }
        let key = Arc::as_ptr(font) as usize;
        if let Some(Some(usable)) = self.with_glyph_checks(|cache| {
            let entry = cache.entries.get(&key)?;
            let index = entry
                .glyphs
                .binary_search_by_key(&glyph, |(gid, _)| *gid)
                .ok()?;
            Some(entry.glyphs[index].1)
        }) {
            return usable;
        }
        let usable = font.has_mapped_glyph(glyph);
        self.with_glyph_checks(|cache| {
            let (entry, count) = cache.for_font(font);
            match entry.glyphs.binary_search_by_key(&glyph, |(gid, _)| *gid) {
                Ok(_) => {}
                Err(index) => {
                    entry.glyphs.insert(index, (glyph, usable));
                    *count += 1;
                }
            }
        });
        usable
    }
}

impl LoadedFont {
    /// A mapped empty glyph is present even if it has no drawable outline.
    fn has_mapped_glyph(&self, glyph_id: u16) -> bool {
        if self.is_type3() {
            return self.type3_glyph_stream(glyph_id).is_some();
        }
        if glyph_id == 0 {
            return false;
        }
        if let Some(type1) = &self.type1 {
            return type1.glyph_outline_by_gid(glyph_id).is_some();
        }
        self.font_data
            .as_ref()
            .and_then(|data| ttf_parser::Face::parse(data, self.face_index).ok())
            .is_some_and(|mut face| {
                self.apply_variations(&mut face);
                self.drawing_gid_with_face(glyph_id, &face)
                    .is_some_and(|gid| usable_glyph(&face, gid))
            })
    }

    fn fallback_character(&self, code: u32, raw_code: u32, length: u8) -> Option<FontCharacter> {
        let character = if let Some(text) = self
            .to_unicode
            .as_ref()
            .and_then(|mapping| mapping.lookup(raw_code).or_else(|| mapping.lookup(code)))
        {
            FontCharacter::from_text(text)?
        } else if matches!(self.font_type, PdfFontType::Type0CidType2) {
            let cmap = self.cid_cmap.as_ref()?;
            cmap.decode_text(code, length)?.font_character()?
        } else if let Some(name) = self
            .encoding
            .as_ref()
            .and_then(|encoding| encoding.glyph_name(code as u8))
        {
            FontCharacter::from_text(&crate::glyph_list::glyph_name_to_string(name)?)?
        } else if !self.symbolic
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
    if gid == 0 || gid >= face.number_of_glyphs() {
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
        provider
            .with_glyph_checks(|cache| {
                let (entry, count) = cache.for_font(font);
                let mut face = None;
                let mut parsed = false;
                let mut previous = None;
                for (index, (glyph, code, raw_code, length)) in glyphs.enumerate() {
                    if index >= 4096 || (glyph == 0 && !font.is_type3()) {
                        return false;
                    }
                    let source = (glyph, code, raw_code, length);
                    if previous == Some(source) {
                        continue;
                    }
                    previous = Some(source);
                    let position = entry.glyphs.binary_search_by_key(&glyph, |(gid, _)| *gid);
                    let known = position.ok().map(|index| entry.glyphs[index].1);
                    let usable = if let Some(usable) = known {
                        usable
                    } else if font.is_type3() || font.type1.is_some() {
                        font.has_mapped_glyph(glyph)
                    } else {
                        if !parsed {
                            face = font.font_data.as_ref().and_then(|data| {
                                ttf_parser::Face::parse(data, font.face_index).ok()
                            });
                            if let Some(face) = face.as_mut() {
                                font.apply_variations(face);
                            }
                            parsed = true;
                        }
                        face.as_ref().is_some_and(|face| {
                            font.drawing_gid_with_face(glyph, face)
                                .is_some_and(|gid| usable_glyph(face, gid))
                        })
                    };
                    if let Err(index) = position {
                        if *count >= 4096 {
                            return false;
                        }
                        entry.glyphs.insert(index, (glyph, usable));
                        *count += 1;
                    }
                    if !usable {
                        return false;
                    }
                    if font.is_substitute {
                        if let Some(character) = font
                            .fallback_character(code, raw_code, length)
                            .filter(|ch| ch.variation_selector.is_some())
                        {
                            if !parsed {
                                face = font.font_data.as_ref().and_then(|data| {
                                    ttf_parser::Face::parse(data, font.face_index).ok()
                                });
                                if let Some(face) = face.as_mut() {
                                    font.apply_variations(face);
                                }
                                parsed = true;
                            }
                            if face.as_ref().is_none_or(|face| {
                                character.mapped_glyph_index(face)
                                    != font.drawing_gid_with_face(glyph, face)
                            }) {
                                return false;
                            }
                        }
                    }
                }
                true
            })
            .unwrap_or(false)
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
        if provider.has_mapped_glyph(font, original.glyph_id) {
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
        assert_eq!(
            provider
                .with_glyph_checks(|cache| cache.entries.len())
                .unwrap(),
            1
        );
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
        provider
            .with_glyph_checks(|checks| {
                assert!(checks.count <= 4096);
                assert!(checks
                    .entries
                    .values()
                    .flat_map(|font| &font.glyphs)
                    .any(|(_, usable)| !usable));
            })
            .unwrap();
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
            provider
                .with_glyph_checks(|checks| {
                    assert!(checks.entries.len() <= 1024);
                    assert_eq!(checks.count, 0);
                })
                .unwrap();
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
