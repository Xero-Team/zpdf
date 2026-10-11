//! Explicit application fonts shared by documents and nested resources.
//!
//! Registration validates the program once and retains immutable shared bytes.
//! Lazy packs decode on first selection. No network access takes place here;
//! callers can use memory-only mode on WASM or without installed fonts.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use zpdf_core::{Error, Result};

use crate::system::{SubstituteHints, SystemFontMatch};
use crate::FontPack;

/// Stable registration identity within one provider. IDs are never recycled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RegisteredFontId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RegisteredFontPackId(pub u32);

/// One Unicode scalar, optionally qualified by a variation selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FontCharacter {
    pub character: char,
    pub variation_selector: Option<char>,
}

impl FontCharacter {
    /// Arbitrary multi-scalar strings require a separate shaping policy.
    pub fn from_text(text: &str) -> Option<Self> {
        let mut chars = text.chars();
        let character = chars.next()?;
        let variation_selector = chars.next();
        if chars.next().is_some()
            || variation_selector.is_some_and(|ch| !is_variation_selector(ch))
            || is_variation_selector(character)
        {
            return None;
        }
        Some(Self {
            character,
            variation_selector,
        })
    }

    pub(crate) fn glyph_index(self, face: &ttf_parser::Face<'_>) -> Option<u16> {
        let gid = self.mapped_glyph_index(face)?;
        crate::resolution::usable_glyph(face, gid).then_some(gid)
    }

    /// Mapping and outline validation are separate operations. PDF font maps
    /// preserve the GID; drawing resolution validates its outline once.
    pub(crate) fn mapped_glyph_index(self, face: &ttf_parser::Face<'_>) -> Option<u16> {
        if is_variation_selector(self.character) {
            return None;
        }
        let gid = match self.variation_selector {
            Some(selector) if is_variation_selector(selector) => {
                face.glyph_variation_index(self.character, selector)?
            }
            Some(_) => return None,
            None => face.glyph_index(self.character)?,
        };
        (gid.0 != 0).then_some(gid.0)
    }
}

fn is_variation_selector(character: char) -> bool {
    matches!(character as u32, 0xFE00..=0xFE0F | 0xE0100..=0xE01EF)
}

/// Misses need only cmap metadata; parse outline tables only for actual hits.
fn registered_character_gid(data: &[u8], face_index: u32, character: FontCharacter) -> Option<u16> {
    if character.variation_selector.is_none() {
        if is_variation_selector(character.character) {
            return None;
        }
        let gid = crate::unicode_lookup::scalar_gid(data, face_index, character.character)?;
        let face = ttf_parser::Face::parse(data, face_index).ok()?;
        crate::resolution::usable_glyph(&face, gid).then_some(gid)
    } else {
        let face = ttf_parser::Face::parse(data, face_index).ok()?;
        character.glyph_index(&face)
    }
}

/// Real glyph identity and shared face selected for drawing or embedding.
#[derive(Debug, Clone)]
pub struct RegisteredGlyph {
    pub font_id: RegisteredFontId,
    pub glyph_id: u16,
    pub font: SystemFontMatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CoverageKey {
    character: FontCharacter,
    bold: bool,
    italic: bool,
}

const MAX_COVERAGE_ENTRIES: usize = 4096;
type CoverageCache = HashMap<CoverageKey, Option<(RegisteredFontId, u16)>>;

struct RegisteredPack {
    pack: Arc<FontPack>,
    face_index: u32,
    aliases: Vec<String>,
    loaded: OnceLock<std::result::Result<RegisteredFontId, String>>,
}

impl RegisteredPack {
    fn load(&self, provider: &FontProvider) -> Result<RegisteredFontId> {
        self.loaded
            .get_or_init(|| {
                let bytes = self.pack.decode().map_err(|error| error.to_string())?;
                let aliases: Vec<&str> = self.aliases.iter().map(String::as_str).collect();
                provider
                    .register_with_aliases(bytes, self.face_index, &aliases)
                    .map_err(|error| error.to_string())
            })
            .as_ref()
            .copied()
            .map_err(|message| Error::StreamDecode(message.clone()))
    }
}

/// Independent admission limits for explicitly supplied application fonts.
#[derive(Debug, Clone, Copy)]
pub struct FontProviderLimits {
    pub max_font_bytes: u64,
    /// Shared TTC programs are charged once, even when several faces are used.
    pub max_total_bytes: u64,
    /// Also bounds the provider's per-face metadata and name index.
    pub max_faces: usize,
}

impl Default for FontProviderLimits {
    fn default() -> Self {
        Self {
            max_font_bytes: 256 * 1024 * 1024,
            max_total_bytes: 256 * 1024 * 1024,
            max_faces: 1024,
        }
    }
}

struct RegisteredFace {
    data: Arc<[u8]>,
    face_index: u32,
    bold: bool,
    italic: bool,
    keys: HashSet<String>,
}

type GlyphCheckSlot = Option<Box<crate::resolution::GlyphChecks>>;

/// Shards of the original-glyph check map. Each keeps the per-map bounds, so
/// the provider retains at most 16 × (4,096 results + 1,024 font identities).
const GLYPH_CHECK_SHARD_BITS: u32 = 4;
const GLYPH_CHECK_SHARDS: usize = 1 << GLYPH_CHECK_SHARD_BITS;

/// One shard per cache line pair, so readers of different shards never share
/// the line their lock state lives on.
#[derive(Default)]
#[repr(align(128))]
struct GlyphCheckShard(RwLock<GlyphCheckSlot>);

/// Aliases accepted per registration call.
const MAX_ALIASES: usize = 16;
/// Name keys one face may accumulate, including style-suffixed copies.
const MAX_FACE_KEYS: usize = 128;
/// A face's own distinct names. With every alias and the styled copies, one
/// registration always fits: (48 + 16) × 2 = 128 keys.
const MAX_FONT_NAMES: usize = MAX_FACE_KEYS / 2 - MAX_ALIASES;

/// Name keys for a face: its own names (at most [`MAX_FONT_NAMES`] distinct)
/// and the caller's aliases, each also under the face's style suffix.
fn face_keys(
    own: impl IntoIterator<Item = String>,
    aliases: &[&str],
    style: &str,
) -> HashSet<String> {
    let mut names = HashSet::new();
    for name in own {
        if names.len() == MAX_FONT_NAMES {
            break;
        }
        let key = normalize_name(&name);
        if !key.is_empty() {
            names.insert(key);
        }
    }
    for alias in aliases {
        let key = normalize_name(alias);
        if !key.is_empty() {
            names.insert(key);
        }
    }
    names
        .into_iter()
        .flat_map(|name| {
            let styled = format!("{name}{style}");
            [name, styled]
        })
        .collect()
}

fn normalize_name(name: &str) -> String {
    name.chars()
        .filter(|ch| ch.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

#[derive(Default)]
struct Registry {
    faces: Vec<RegisteredFace>,
    names: HashMap<String, Vec<usize>>,
    bytes: u64,
    packs: Vec<Arc<RegisteredPack>>,
    pack_names: HashMap<String, Vec<usize>>,
    encoded_bytes: u64,
    reserved_pack_bytes: u64,
    unreserved_bytes: u64,
    coverage: CoverageCache,
}

/// A thread-safe registry with optional installed-font lookup.
///
/// The default permits system substitution after registered candidates. Use
/// [`Self::memory_only`] for deterministic/headless or browser deployments.
/// Documents without an explicit provider retain their existing loading path.
pub struct FontProvider {
    system_fonts: bool,
    limits: FontProviderLimits,
    registry: RwLock<Registry>,
    /// Original-glyph checks have their own locks, sharded by font identity:
    /// concurrent renders read them together, renders of different fonts touch
    /// different cache lines, and none blocks registration or name lookups.
    glyph_checks: Box<[GlyphCheckShard]>,
    generation: AtomicU64,
}

impl Default for FontProvider {
    fn default() -> Self {
        Self::new(true, FontProviderLimits::default())
    }
}

impl FontProvider {
    /// Names are indexed immediately; decoding happens only on selection.
    /// Encoded storage has a separate max_total_bytes budget. Decoded programs
    /// also pass the normal font-program admission limits.
    pub fn register_pack(
        &self,
        pack: Arc<FontPack>,
        face_index: u32,
        aliases: &[&str],
    ) -> Result<RegisteredFontPackId> {
        if aliases.is_empty()
            || aliases.len() > MAX_ALIASES
            || aliases
                .iter()
                .any(|name| name.len() > 1024 || normalize_name(name).is_empty())
        {
            return Err(Error::StreamDecode("invalid font pack aliases".into()));
        }
        let names: Vec<String> = aliases.iter().map(|name| (*name).to_string()).collect();
        let mut registry = self
            .registry
            .write()
            .map_err(|_| Error::StreamDecode("font provider lock is poisoned".into()))?;
        // Hosts rebuild a pack from the same bytes on every call (the wasm
        // binding does); reuse the registered one so a repeat neither reserves
        // decoded capacity again nor retains another encoded copy.
        let pack = registry
            .packs
            .iter()
            .find(|entry| entry.pack.same_source(&pack))
            .map_or(pack, |entry| Arc::clone(&entry.pack));
        if let Some(index) = registry.packs.iter().position(|entry| {
            Arc::ptr_eq(&entry.pack, &pack)
                && entry.face_index == face_index
                && entry.aliases == names
        }) {
            return Ok(RegisteredFontPackId(index as u32));
        }
        if registry.packs.len() >= self.limits.max_faces
            || registry.packs.len() >= u32::MAX as usize
        {
            return Err(Error::StreamDecode("font pack count limit exceeded".into()));
        }
        let shared = registry
            .packs
            .iter()
            .any(|entry| Arc::ptr_eq(&entry.pack, &pack));
        if pack.decoded_bound() > self.limits.max_font_bytes {
            return Err(Error::StreamSizeLimit(self.limits.max_font_bytes));
        }
        let reserved = registry
            .reserved_pack_bytes
            .checked_add(if shared { 0 } else { pack.decoded_bound() })
            .filter(|reserved| {
                reserved
                    .checked_add(registry.unreserved_bytes)
                    .is_some_and(|total| total <= self.limits.max_total_bytes)
            })
            .ok_or(Error::StreamSizeLimit(self.limits.max_total_bytes))?;
        let added = if shared { 0 } else { pack.encoded_len() };
        let bytes = registry
            .encoded_bytes
            .checked_add(added)
            .filter(|bytes| *bytes <= self.limits.max_total_bytes)
            .ok_or(Error::StreamSizeLimit(self.limits.max_total_bytes))?;
        let index = registry.packs.len();
        registry.packs.push(Arc::new(RegisteredPack {
            pack,
            face_index,
            aliases: names,
            loaded: OnceLock::new(),
        }));
        for key in aliases
            .iter()
            .map(|name| normalize_name(name))
            .collect::<HashSet<_>>()
        {
            registry.pack_names.entry(key).or_default().push(index);
        }
        registry.encoded_bytes = bytes;
        registry.reserved_pack_bytes = reserved;
        registry.coverage.clear();
        self.generation.fetch_add(1, Ordering::Release);
        Ok(RegisteredFontPackId(index as u32))
    }

    pub fn load_pack(&self, id: RegisteredFontPackId) -> Result<RegisteredFontId> {
        let pack = self
            .registry
            .read()
            .map_err(|_| Error::StreamDecode("font provider lock is poisoned".into()))?
            .packs
            .get(id.0 as usize)
            .cloned()
            .ok_or_else(|| Error::StreamDecode("unknown font pack identity".into()))?;
        pack.load(self)
    }

    pub fn encoded_bytes_used(&self) -> u64 {
        self.registry
            .read()
            .map_or(0, |registry| registry.encoded_bytes)
    }

    pub fn reserved_pack_bytes(&self) -> u64 {
        self.registry
            .read()
            .map_or(0, |registry| registry.reserved_pack_bytes)
    }

    pub fn new(system_fonts: bool, limits: FontProviderLimits) -> Self {
        Self {
            system_fonts,
            limits,
            registry: RwLock::new(Registry::default()),
            glyph_checks: (0..GLYPH_CHECK_SHARDS)
                .map(|_| GlyphCheckShard::default())
                .collect(),
            generation: AtomicU64::new(0),
        }
    }

    pub fn memory_only() -> Self {
        Self::new(false, FontProviderLimits::default())
    }

    /// Changes after a face, pack or new alias has been admitted successfully.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub fn bytes_used(&self) -> u64 {
        self.registry.read().map_or(0, |registry| registry.bytes)
    }

    pub fn len(&self) -> usize {
        self.registry
            .read()
            .map_or(0, |registry| registry.faces.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn register(
        &self,
        data: impl Into<Arc<[u8]>>,
        face_index: u32,
    ) -> Result<RegisteredFontId> {
        self.register_with_aliases(data, face_index, &[])
    }

    /// Register a TTF/OTF face, optionally binding PDF BaseFont aliases.
    ///
    /// Identical programs share storage. Re-registering the same face returns
    /// its existing identity; newly supplied aliases still advance generation.
    /// Names are bounded independently of the program byte budget.
    pub fn register_with_aliases(
        &self,
        data: impl Into<Arc<[u8]>>,
        face_index: u32,
        aliases: &[&str],
    ) -> Result<RegisteredFontId> {
        let data = data.into();
        if data.len() as u64 > self.limits.max_font_bytes {
            return Err(Error::StreamSizeLimit(self.limits.max_font_bytes));
        }
        if aliases.len() > MAX_ALIASES || aliases.iter().any(|name| name.len() > 1024) {
            return Err(Error::StreamDecode("font alias limit exceeded".into()));
        }
        let face = ttf_parser::Face::parse(&data, face_index)
            .map_err(|error| Error::StreamDecode(format!("invalid registered font: {error}")))?;
        let bold = face.is_bold();
        let italic = face.is_italic();
        let own = face
            .names()
            .into_iter()
            .filter(|name| matches!(name.name_id, 1 | 4 | 6 | 16) && name.name.len() <= 1024)
            .take(64)
            .filter_map(|name| name.to_string());
        let keys = face_keys(own, aliases, crate::system::style_suffix(bold, italic));
        let mut registry = self
            .registry
            .write()
            .map_err(|_| Error::StreamDecode("font provider lock is poisoned".into()))?;
        let existing_program = registry.faces.iter().position(|font| font.data == data);
        let existing_face = registry
            .faces
            .iter()
            .position(|font| font.face_index == face_index && font.data == data);
        let total_names = existing_face.map_or(keys.len(), |index| {
            registry.faces[index].keys.union(&keys).count()
        });
        if total_names > MAX_FACE_KEYS {
            return Err(Error::StreamDecode(
                "registered font name limit exceeded".into(),
            ));
        }
        let index = if let Some(index) = existing_face {
            index
        } else {
            if registry.faces.len() >= self.limits.max_faces
                || registry.faces.len() >= u32::MAX as usize
            {
                return Err(Error::StreamDecode(
                    "font provider face limit exceeded".into(),
                ));
            }
            let added = if existing_program.is_some() {
                0
            } else {
                data.len() as u64
            };
            let bytes = registry
                .bytes
                .checked_add(added)
                .filter(|bytes| *bytes <= self.limits.max_total_bytes)
                .ok_or(Error::StreamSizeLimit(self.limits.max_total_bytes))?;
            let covered = registry
                .packs
                .iter()
                .any(|entry| entry.pack.shares_decoded(&data));
            let unreserved = registry
                .unreserved_bytes
                .checked_add(if covered { 0 } else { added })
                .filter(|bytes| {
                    bytes
                        .checked_add(registry.reserved_pack_bytes)
                        .is_some_and(|total| total <= self.limits.max_total_bytes)
                })
                .ok_or(Error::StreamSizeLimit(self.limits.max_total_bytes))?;
            let data = existing_program
                .map(|index| Arc::clone(&registry.faces[index].data))
                .unwrap_or(data);
            let index = registry.faces.len();
            registry.faces.push(RegisteredFace {
                data,
                face_index,
                bold,
                italic,
                keys: HashSet::new(),
            });
            registry.bytes = bytes;
            registry.unreserved_bytes = unreserved;
            index
        };
        let mut changed = existing_face.is_none();
        for key in keys {
            if registry.faces[index].keys.insert(key.clone()) {
                registry.names.entry(key).or_default().push(index);
                changed = true;
            }
        }
        if changed {
            registry.coverage.clear();
            self.generation.fetch_add(1, Ordering::Release);
        }
        Ok(RegisteredFontId(index as u32))
    }

    /// Resolve a name/style using registered faces, then optional system fonts.
    /// This selects a whole face; per-character coverage resolution is separate.
    pub fn find_font(
        &self,
        base_font: &str,
        hints: SubstituteHints,
        ordering: Option<&str>,
    ) -> Option<SystemFontMatch> {
        {
            let mut registry = self.registry.read().ok()?;
            if !registry.faces.is_empty() || !registry.packs.is_empty() {
                let lower = crate::system::strip_subset_prefix(base_font).to_ascii_lowercase();
                let desired_bold = hints.bold || lower.contains("bold");
                let desired_italic =
                    hints.italic || lower.contains("italic") || lower.contains("oblique");
                let direct = normalize_name(crate::system::strip_subset_prefix(base_font));
                let candidates = std::iter::once(direct).chain(
                    std::iter::once_with(|| {
                        crate::system::font_candidates_with(
                            base_font,
                            hints,
                            ordering,
                            normalize_name,
                        )
                    })
                    .flatten(),
                );
                for key in candidates {
                    if let Some(indices) = registry.names.get(&key) {
                        let index = *indices.iter().min_by_key(|&&index| {
                            let font = &registry.faces[index];
                            u8::from(font.bold != desired_bold)
                                + u8::from(font.italic != desired_italic)
                        })?;
                        let font = &registry.faces[index];
                        return Some(SystemFontMatch {
                            data: Arc::clone(&font.data),
                            face_index: font.face_index,
                        });
                    }
                    if let Some(indices) = registry.pack_names.get(&key) {
                        let packs: Vec<_> = indices
                            .iter()
                            .map(|&index| Arc::clone(&registry.packs[index]))
                            .collect();
                        // Admission needs the write lock; never decode under a read lock.
                        drop(registry);
                        for pack in packs {
                            if let Ok(id) = pack.load(self) {
                                return self.get(id);
                            }
                        }
                        registry = self.registry.read().ok()?;
                    }
                }
            }
        }
        if self.system_fonts {
            crate::system::find_system_font(base_font, hints, ordering)
        } else {
            None
        }
    }

    pub fn get(&self, id: RegisteredFontId) -> Option<SystemFontMatch> {
        let registry = self.registry.read().ok()?;
        let font = registry.faces.get(id.0 as usize)?;
        Some(SystemFontMatch {
            data: Arc::clone(&font.data),
            face_index: font.face_index,
        })
    }

    /// Resolve registered coverage independently of PDF font-name aliases.
    /// Registered faces prefer matching bold/italic metadata, then registration
    /// order. Unloaded packs are tried in registration order after coverage hints.
    /// Empty spaces are valid hits. Variation selectors must be supported.
    /// This per-character API never scans installed fonts or performs network I/O.
    pub fn resolve_character(
        &self,
        character: FontCharacter,
        hints: SubstituteHints,
    ) -> Option<RegisteredGlyph> {
        let key = CoverageKey {
            character,
            bold: hints.bold,
            italic: hints.italic,
        };
        let (generation, packs) = {
            let registry = self.registry.read().ok()?;
            if let Some(cached) = registry.coverage.get(&key) {
                return cached.and_then(|(id, gid)| registered_glyph(&registry, id, gid));
            }
            let generation = self.generation();
            let mut selected = None;
            for score in 0..=2 {
                for (index, font) in registry.faces.iter().enumerate() {
                    if u8::from(font.bold != hints.bold) + u8::from(font.italic != hints.italic)
                        != score
                    {
                        continue;
                    }
                    if let Some(gid) =
                        registered_character_gid(&font.data, font.face_index, character)
                    {
                        selected = registered_glyph(&registry, RegisteredFontId(index as u32), gid);
                        break;
                    }
                }
                if selected.is_some() {
                    break;
                }
            }
            if selected.is_some() {
                drop(registry);
                self.cache_coverage(key, generation, selected.as_ref());
                return selected;
            }
            let packs: Vec<_> = registry
                .packs
                .iter()
                .filter(|entry| entry.pack.may_cover(character.character))
                .cloned()
                .collect();
            (generation, packs)
        };
        for pack in packs {
            let Ok(id) = pack.load(self) else { continue };
            let Some(font) = self.get(id) else { continue };
            if let Some(glyph_id) = registered_character_gid(&font.data, font.face_index, character)
            {
                let selected = RegisteredGlyph {
                    font_id: id,
                    glyph_id,
                    font,
                };
                // Pack admission changes generation. The next query can cache
                // against the fresh registry, without retaining a racing miss.
                self.cache_coverage(key, generation, Some(&selected));
                return Some(selected);
            }
        }
        self.cache_coverage(key, generation, None);
        None
    }

    fn cache_coverage(&self, key: CoverageKey, generation: u64, glyph: Option<&RegisteredGlyph>) {
        if let Ok(mut registry) = self.registry.write() {
            if self.generation() != generation {
                return;
            }
            let coverage = &mut registry.coverage;
            if coverage.len() >= MAX_COVERAGE_ENTRIES {
                coverage.clear();
            }
            coverage.insert(key, glyph.map(|glyph| (glyph.font_id, glyph.glyph_id)));
        }
    }

    fn glyph_check_shard(&self, font_key: usize) -> &RwLock<GlyphCheckSlot> {
        // Fibonacci hashing: allocator addresses share low bits, so take the
        // top bits of a multiplicative hash to spread fonts across shards.
        let hash = (font_key as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        &self.glyph_checks[(hash >> (64 - GLYPH_CHECK_SHARD_BITS)) as usize].0
    }

    /// Original checks allocate only when rendering needs them. They never take
    /// the registry lock, and `operation` must not call back into the provider.
    /// A poisoned check lock only disables reuse; it cannot poison registration.
    pub(crate) fn with_glyph_checks<R>(
        &self,
        font_key: usize,
        operation: impl FnOnce(&mut crate::resolution::GlyphChecks) -> R,
    ) -> Option<R> {
        let mut checks = self.glyph_check_shard(font_key).write().ok()?;
        Some(operation(checks.get_or_insert_with(Default::default)))
    }

    /// Shared read access for already recorded checks; `None` before any.
    pub(crate) fn read_glyph_checks<R>(
        &self,
        font_key: usize,
        operation: impl FnOnce(&crate::resolution::GlyphChecks) -> R,
    ) -> Option<R> {
        self.glyph_check_shard(font_key)
            .read()
            .ok()?
            .as_deref()
            .map(operation)
    }

    #[cfg(test)]
    pub(crate) fn each_glyph_check_shard(
        &self,
        mut operation: impl FnMut(&mut crate::resolution::GlyphChecks),
    ) {
        for shard in self.glyph_checks.iter() {
            if let Some(checks) = shard.0.write().unwrap().as_deref_mut() {
                operation(checks);
            }
        }
    }
}

fn registered_glyph(
    registry: &Registry,
    id: RegisteredFontId,
    glyph_id: u16,
) -> Option<RegisteredGlyph> {
    let font = registry.faces.get(id.0 as usize)?;
    Some(RegisteredGlyph {
        font_id: id,
        glyph_id,
        font: SystemFontMatch {
            data: Arc::clone(&font.data),
            face_index: font.face_index,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negative_coverage_cache_is_bounded_and_registration_clears_it() {
        let provider = FontProvider::memory_only();
        for code in 0x1000..0x4000 {
            let character = FontCharacter {
                character: char::from_u32(code).unwrap(),
                variation_selector: None,
            };
            assert!(provider
                .resolve_character(character, SubstituteHints::default())
                .is_none());
            assert!(provider.registry.read().unwrap().coverage.len() <= MAX_COVERAGE_ENTRIES);
        }
        assert!(!provider.registry.read().unwrap().coverage.is_empty());
        provider
            .register(
                include_bytes!("../tests/fixtures/coverage-primary.ttf").as_slice(),
                0,
            )
            .unwrap();
        assert!(provider.registry.read().unwrap().coverage.is_empty());
    }

    #[test]
    fn name_only_registration_leaves_cache_storage_unallocated() {
        let provider = FontProvider::memory_only();
        let bytes = include_bytes!("../tests/fixtures/coverage-primary.ttf").as_slice();
        provider
            .register_with_aliases(bytes, 0, &["Alias"])
            .unwrap();
        assert!(provider
            .find_font("Alias", SubstituteHints::default(), None)
            .is_some());
        let registry = provider.registry.read().unwrap();
        assert_eq!(registry.coverage.capacity(), 0);
        assert!(provider
            .glyph_checks
            .iter()
            .all(|shard| shard.0.read().unwrap().is_none()));
    }

    #[test]
    fn heavily_localized_styled_faces_keep_every_alias_within_the_key_limit() {
        let own = (0..80).map(|index| format!("Localized Family {index}"));
        let aliases: Vec<String> = (0..MAX_ALIASES)
            .map(|index| format!("Alias{index}"))
            .collect();
        let aliases: Vec<&str> = aliases.iter().map(String::as_str).collect();
        let keys = face_keys(own, &aliases, crate::system::style_suffix(true, true));
        assert!(keys.len() <= MAX_FACE_KEYS, "{} keys", keys.len());
        for alias in &aliases {
            let key = normalize_name(alias);
            assert!(keys.contains(&key) && keys.iter().any(|k| k.starts_with(&key) && k != &key));
        }
    }

    #[test]
    fn glyph_checks_leave_registration_and_lookups_unblocked() {
        let provider = FontProvider::memory_only();
        provider
            .with_glyph_checks(0, |_| {
                assert!(
                    provider.registry.try_read().is_ok(),
                    "a glyph check must not block coverage or name lookups"
                );
                assert!(
                    provider.registry.try_write().is_ok(),
                    "a glyph check must not block registration"
                );
            })
            .unwrap();
    }
}
