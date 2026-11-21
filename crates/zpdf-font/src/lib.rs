mod adobe_cmaps;
pub mod big5;
pub mod cmap;
pub mod encoding;
pub mod eucjp;
pub mod gb2312;
pub mod gbk;
pub mod glyph_list;
pub mod ksc;
pub mod pack;
pub mod provider;
mod resolution;
pub mod sjis;
pub mod standard_fonts;
pub mod system;
pub mod type1;
mod unicode_lookup;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub type FontId = u32;

fn allocation_bytes<T>(capacity: usize) -> u64 {
    (capacity as u64).saturating_mul(std::mem::size_of::<T>() as u64)
}

/// Conservative reserved-table estimate including bucket/control overhead.
fn estimated_map_bytes<K, V>(map: &HashMap<K, V>) -> u64 {
    (map.capacity() as u64)
        .saturating_mul((std::mem::size_of::<(K, V)>() as u64).saturating_add(1))
        .saturating_mul(2)
}

pub use pack::{FontPack, FontPackFormat, FontPackLimits};
pub use provider::{
    FontCharacter, FontProvider, FontProviderLimits, RegisteredFontId, RegisteredFontPackId,
    RegisteredGlyph,
};
pub use resolution::{is_visible_text, BlankGlyphTable, DrawingGlyph};

/// Glyph outline as a series of path commands.
#[derive(Debug, Clone)]
pub struct GlyphOutline {
    pub commands: Vec<OutlineCommand>,
    pub advance_width: f64,
}

#[derive(Debug, Clone, Copy)]
pub enum OutlineCommand {
    MoveTo(f64, f64),
    LineTo(f64, f64),
    QuadTo(f64, f64, f64, f64),
    CurveTo(f64, f64, f64, f64, f64, f64),
    Close,
}

/// Per-glyph horizontal width from the PDF /W array, plus per-CID vertical
/// metrics from /W2 (PDF 9.7.4.3) when present.
#[derive(Debug, Clone)]
pub struct CidWidths {
    widths: HashMap<u16, f64>,
    default_width: f64,
    /// True when `default_width` comes from an explicit PDF `/Widths` +
    /// `/FontDescriptor` (`/MissingWidth`) specification, so a substituted
    /// simple font must honor it for out-of-range codes instead of reading
    /// the substitute face's `hmtx`.
    authoritative_default: bool,
    /// /W2 per-CID vertical metrics: cid → (w1y, vx, vy) in 1/1000 glyph-space
    /// units. `w1y` is the vertical displacement (advance); `(vx, vy)` is the
    /// position vector of the glyph's vertical-mode origin relative to its
    /// horizontal-mode origin. Absent CIDs fall back to /DW2.
    v_metrics: HashMap<u16, (f64, f64, f64)>,
}

impl CidWidths {
    pub fn new(default_width: f64) -> Self {
        Self {
            widths: HashMap::new(),
            default_width: if default_width.is_finite() {
                default_width
            } else {
                1000.0
            },
            authoritative_default: false,
            v_metrics: HashMap::new(),
        }
    }

    pub fn set_authoritative_default(&mut self, authoritative: bool) {
        self.authoritative_default = authoritative;
    }

    pub fn has_authoritative_default(&self) -> bool {
        self.authoritative_default
    }

    pub fn set(&mut self, cid: u16, width: f64) {
        if width.is_finite() {
            self.widths.insert(cid, width);
        }
    }

    pub fn get(&self, cid: u16) -> f64 {
        self.widths.get(&cid).copied().unwrap_or(self.default_width)
    }

    /// Explicitly-set width for `cid`, or `None` if it falls back to the default.
    pub fn get_opt(&self, cid: u16) -> Option<f64> {
        self.widths.get(&cid).copied()
    }

    /// The width used for codes with no explicit entry (`/DW` for CID fonts,
    /// the descriptor's `/MissingWidth` for simple fonts).
    pub fn default_width(&self) -> f64 {
        self.default_width
    }

    /// True when no per-glyph widths were set (every code falls to the default).
    pub fn is_empty(&self) -> bool {
        self.widths.is_empty()
    }

    /// Record a /W2 vertical metric for `cid`: (w1y, vx, vy).
    pub fn set_v(&mut self, cid: u16, w1y: f64, vx: f64, vy: f64) {
        if w1y.is_finite() && vx.is_finite() && vy.is_finite() {
            self.v_metrics.insert(cid, (w1y, vx, vy));
        }
    }

    /// Per-CID /W2 vertical metric (w1y, vx, vy), or `None` to fall back to /DW2.
    pub fn get_v(&self, cid: u16) -> Option<(f64, f64, f64)> {
        self.v_metrics.get(&cid).copied()
    }

    fn estimated_cache_bytes(&self) -> u64 {
        estimated_map_bytes(&self.widths).saturating_add(estimated_map_bytes(&self.v_metrics))
    }
}

/// A loaded font with embedded TrueType/CFF data.
pub struct LoadedFont {
    pub font_type: PdfFontType,
    pub base_font: String,
    pub font_data: Option<Arc<[u8]>>,
    /// Face index within `font_data` (non-zero only for faces from TTC
    /// collections supplied by the system-font fallback).
    pub face_index: u32,
    /// True when `font_data` is a substituted system font rather than the
    /// PDF-embedded program. PDF metrics (/Widths, /W) stay authoritative.
    pub is_substitute: bool,
    /// True when this substituted Type 0 font resolves physical drawing GIDs
    /// per source code (via `/ToUnicode` and `/Encoding` CMap) rather than
    /// routing `glyph_id` through a CID → GID table. Preserves original CID
    /// advances while avoiding hash-order collisions when multiple source codes
    /// share one CID.
    pub direct_composite_gids: bool,
    pub cid_widths: CidWidths,
    pub units_per_em: f64,
    pub ascent: f64,
    pub descent: f64,
    pub cid_to_gid: Option<HashMap<u16, u16>>,
    /// Character-code → GID table from the embedded CFF program's built-in
    /// encoding (simple fonts with raw CFF data only). Consulted as a fallback
    /// in [`code_to_gid`](Self::code_to_gid) — never applied to a resolved GID,
    /// since its keys are character codes, not glyph ids.
    pub builtin_encoding_gids: Option<HashMap<u16, u16>>,
    /// GIDs > 0 whose CFF charset name is literally ".notdef" (SID 0). Quartz
    /// subsets use this for glyphs it could not give a MacRoman-compatible
    /// name; see [`map_unencoded_orphans`](Self::map_unencoded_orphans).
    pub orphan_gids: Vec<u16>,
    /// Simple-font character-code → glyph-name mapping (base encoding + /Differences).
    /// `None` for composite (Type0/CID) fonts and fully symbolic fonts.
    pub encoding: Option<encoding::Encoding>,
    /// /ToUnicode CMap for text extraction (code → Unicode string).
    pub to_unicode: Option<cmap::ToUnicodeMap>,
    /// Symbolic flag from the FontDescriptor (use the font's built-in cmap, not a base encoding).
    pub symbolic: bool,
    /// Parsed embedded Type 1 (PostScript) font program, when the embedded data is
    /// Type 1 rather than sfnt/CFF (ttf-parser cannot handle Type 1).
    pub type1: Option<type1::Type1Font>,
    /// Composite-font code → CID CMap from the Type0 /Encoding (None for
    /// simple fonts; Identity-H behavior when absent on a composite font).
    pub cid_cmap: Option<cmap::CidCMap>,
    /// /DW2 vertical-writing defaults: (origin-shift vy, advance w1y), in
    /// 1/1000 glyph-space units (PDF defaults [880 −1000]).
    pub dw2: (f64, f64),
    /// OpenType variation axis settings `(tag, user-value)` derived from the
    /// FontDescriptor (`/FontWeight`→`wght`, `/FontStretch`→`wdth`,
    /// `/ItalicAngle`→`slnt`, the Italic flag→`ital`). Applied to a *variable*
    /// font program before outlining/measuring so it renders at the intended
    /// instance instead of the default master; a no-op for static fonts
    /// (`set_variation` ignores axes the font lacks). Set via [`set_variations`].
    ///
    /// [`set_variations`]: LoadedFont::set_variations
    pub variations: Vec<([u8; 4], f32)>,
}

#[derive(Debug, Clone)]
pub enum PdfFontType {
    Type0CidType2,
    TrueType,
    Type1,
    /// Type3: glyphs defined by PDF content streams in CharProcs.
    /// Each entry maps a glyph name to the decoded content stream bytes.
    Type3 {
        font_matrix: [f64; 6],
        char_procs: HashMap<String, Arc<[u8]>>,
        encoding: Vec<String>,
        widths: Vec<f64>,
        first_char: u16,
    },
}

impl LoadedFont {
    /// Estimated retained font programs and owned mapping/table storage.
    /// Shared program buffers count once within this font. Cache admission
    /// additionally deduplicates programs and identical `Arc<LoadedFont>`s
    /// across fonts. This is not a total allocator/process-memory measure.
    pub fn estimated_cache_bytes(&self) -> u64 {
        let mut seen = HashSet::new();
        self.shared_programs()
            .fold(self.estimated_owned_cache_bytes(), |sum, data| {
                if seen.insert(data.as_ptr() as usize) {
                    sum.saturating_add(data.len() as u64)
                } else {
                    sum
                }
            })
    }

    fn shared_programs(&self) -> impl Iterator<Item = &Arc<[u8]>> {
        let char_procs = match &self.font_type {
            PdfFontType::Type3 { char_procs, .. } => Some(char_procs.values()),
            _ => None,
        };
        self.font_data
            .iter()
            .chain(char_procs.into_iter().flatten())
    }

    fn estimated_owned_cache_bytes(&self) -> u64 {
        let mut bytes = self.cid_widths.estimated_cache_bytes();
        for map in [&self.cid_to_gid, &self.builtin_encoding_gids]
            .into_iter()
            .flatten()
        {
            bytes = bytes.saturating_add(estimated_map_bytes(map));
        }
        bytes = bytes
            .saturating_add(allocation_bytes::<u16>(self.orphan_gids.capacity()))
            .saturating_add(allocation_bytes::<([u8; 4], f32)>(
                self.variations.capacity(),
            ))
            .saturating_add(
                self.encoding
                    .as_ref()
                    .map_or(0, encoding::Encoding::estimated_cache_bytes),
            )
            .saturating_add(
                self.to_unicode
                    .as_ref()
                    .map_or(0, cmap::ToUnicodeMap::estimated_cache_bytes),
            )
            .saturating_add(
                self.cid_cmap
                    .as_ref()
                    .map_or(0, cmap::CidCMap::estimated_cache_bytes),
            )
            .saturating_add(
                self.type1
                    .as_ref()
                    .map_or(0, type1::Type1Font::estimated_cache_bytes),
            );
        if let PdfFontType::Type3 {
            char_procs,
            encoding,
            widths,
            ..
        } = &self.font_type
        {
            bytes = bytes
                .saturating_add(estimated_map_bytes(char_procs))
                .saturating_add(allocation_bytes::<String>(encoding.capacity()))
                .saturating_add(allocation_bytes::<f64>(widths.capacity()));
            for name in char_procs.keys().chain(encoding.iter()) {
                bytes = bytes.saturating_add(name.capacity() as u64);
            }
        }
        bytes
    }

    pub fn new_with_data(
        font_type: PdfFontType,
        base_font: String,
        font_data: Vec<u8>,
        cid_widths: CidWidths,
    ) -> Self {
        let was_raw_cff = is_raw_cff(&font_data);
        let font_data = if was_raw_cff {
            wrap_cff_in_otf(&font_data)
        } else {
            font_data
        };

        if let Ok(face) = ttf_parser::Face::parse(&font_data, 0) {
            let units_per_em = face.units_per_em() as f64;
            let ascent = face.ascender() as f64;
            let descent = face.descender() as f64;

            // For composite fonts the charset maps CID → GID and glyph_outline
            // applies it to incoming CIDs. For simple fonts the CFF built-in
            // encoding maps character codes → GIDs, which belongs to the
            // code_to_gid resolution chain instead.
            let (cid_to_gid, builtin_encoding_gids, orphan_gids) = if was_raw_cff {
                match font_type {
                    PdfFontType::Type0CidType2 => {
                        (build_cff_cid_to_gid_map(&font_data), None, Vec::new())
                    }
                    PdfFontType::Type1 | PdfFontType::TrueType => (
                        None,
                        build_cff_encoding_map(&font_data),
                        find_cff_table(&font_data)
                            .and_then(parse_cff_orphan_gids)
                            .unwrap_or_default(),
                    ),
                    _ => (None, None, Vec::new()),
                }
            } else if matches!(font_type, PdfFontType::Type0CidType2) {
                // FontFile3 /Subtype /OpenType: an sfnt wrapper whose glyph data
                // is a CID-keyed CFF table. The charset still carries CID → GID,
                // without which CIDs would be misread as GIDs. A non-CID-keyed
                // CFF (no /ROS) is left alone — there the charset holds
                // glyph-name SIDs, not CIDs, and the identity mapping is correct.
                let map = find_cff_table(&font_data)
                    .filter(|cff| cff_is_cid_keyed(cff))
                    .and_then(parse_cff_charset);
                (map, None, Vec::new())
            } else {
                (None, None, Vec::new())
            };

            Self {
                font_type,
                base_font,
                font_data: Some(Arc::from(font_data)),
                face_index: 0,
                is_substitute: false,
                direct_composite_gids: false,
                cid_widths,
                units_per_em,
                ascent,
                descent,
                cid_to_gid,
                builtin_encoding_gids,
                orphan_gids,
                encoding: None,
                to_unicode: None,
                symbolic: false,
                type1: None,
                cid_cmap: None,
                dw2: (880.0, -1000.0),
                variations: Vec::new(),
            }
        } else {
            Self::new_legacy_data(font_type, base_font, font_data, cid_widths)
        }
    }

    /// Keep the legacy-program parser and malformed-font fallback out of the
    /// common sfnt constructor's instruction and stack working set.
    #[cold]
    #[inline(never)]
    fn new_legacy_data(
        font_type: PdfFontType,
        base_font: String,
        font_data: Vec<u8>,
        cid_widths: CidWidths,
    ) -> Self {
        if let Some(t1) = type1::Type1Font::parse(&font_data) {
            // Embedded Type 1 (PostScript) program — ttf-parser cannot parse it.
            let units_per_em = t1.units_per_em;
            Self {
                font_type,
                base_font,
                font_data: None,
                face_index: 0,
                is_substitute: false,
                direct_composite_gids: false,
                cid_widths,
                units_per_em,
                ascent: units_per_em * 0.8,
                descent: -units_per_em * 0.2,
                cid_to_gid: None,
                builtin_encoding_gids: None,
                orphan_gids: Vec::new(),
                encoding: None,
                to_unicode: None,
                symbolic: false,
                type1: Some(t1),
                cid_cmap: None,
                dw2: (880.0, -1000.0),
                variations: Vec::new(),
            }
        } else {
            tracing::debug!(
                "font {base_font}: embedded data not parseable by ttf-parser, using widths only"
            );
            Self {
                font_type,
                base_font,
                font_data: None,
                face_index: 0,
                is_substitute: false,
                direct_composite_gids: false,
                cid_widths,
                units_per_em: 1000.0,
                ascent: 800.0,
                descent: -200.0,
                cid_to_gid: None,
                builtin_encoding_gids: None,
                orphan_gids: Vec::new(),
                encoding: None,
                to_unicode: None,
                symbolic: false,
                type1: None,
                cid_cmap: None,
                dw2: (880.0, -1000.0),
                variations: Vec::new(),
            }
        }
    }

    /// Build a font around a substituted *system* font program (whole file
    /// bytes + face index, from [`system::find_system_font`]). The PDF's own
    /// metrics (`cid_widths`) remain authoritative for advances; the system
    /// face provides outlines, cmap and name tables for glyph resolution.
    pub fn new_substitute(
        font_type: PdfFontType,
        base_font: String,
        data: Arc<[u8]>,
        face_index: u32,
        cid_widths: CidWidths,
    ) -> Option<Self> {
        let face = ttf_parser::Face::parse(&data, face_index).ok()?;
        let units_per_em = face.units_per_em() as f64;
        let ascent = face.ascender() as f64;
        let descent = face.descender() as f64;
        Some(Self {
            font_type,
            base_font,
            font_data: Some(data),
            face_index,
            is_substitute: true,
            direct_composite_gids: false,
            cid_widths,
            units_per_em,
            ascent,
            descent,
            cid_to_gid: None,
            builtin_encoding_gids: None,
            orphan_gids: Vec::new(),
            encoding: None,
            to_unicode: None,
            symbolic: false,
            type1: None,
            cid_cmap: None,
            dw2: (880.0, -1000.0),
            variations: Vec::new(),
        })
    }

    /// For a substituted composite (Type0) font: synthesize the CID → GID
    /// table by routing each /ToUnicode mapping through the system face's
    /// Unicode cmap. Without this, CIDs (which index the *original* embedded
    /// subset) would be misread as GIDs of the substitute. Call after
    /// /ToUnicode is attached.
    pub fn build_substitute_cid_to_gid(&mut self) {
        self.build_substitute_cid_to_gid_from(None);
    }

    /// Like [`Self::build_substitute_cid_to_gid`], borrowing an external
    /// `/ToUnicode` map when provided so callers can probe substitute coverage
    /// without cloning the map into `self.to_unicode` first.
    #[doc(hidden)]
    pub fn build_substitute_cid_to_gid_from(&mut self, to_unicode: Option<&cmap::ToUnicodeMap>) {
        if !self.is_substitute || !matches!(self.font_type, PdfFontType::Type0CidType2) {
            return;
        }
        // Unicode-coded, legacy, and direct per-source-code substitutes resolve
        // straight to physical GIDs — no CID table needed.
        if self.uses_direct_composite_gids() {
            return;
        }
        // An absent or empty mapping must not mean CID == substitute GID.
        // Legacy byte-encoded substitutes bypass this table explicitly.
        self.cid_to_gid = Some(HashMap::new());
        let Some(data) = &self.font_data else { return };
        let Ok(face) = ttf_parser::Face::parse(data, self.face_index) else {
            return;
        };
        let to_unicode = to_unicode.or(self.to_unicode.as_ref());
        let mut map = HashMap::with_capacity(to_unicode.map_or(0, |mapping| mapping.iter().len()));
        let has_collection = self
            .cid_cmap
            .as_ref()
            .and_then(|cm| cm.collection())
            .is_some();
        for (code, len, s) in to_unicode.iter().flat_map(|tu| tu.iter_with_lengths()) {
            // glyph_outline receives CIDs, so key the map by CID — under a
            // non-Identity encoding CMap, or an Identity base with explicit
            // overrides, the code is not the CID.
            let cid = match &self.cid_cmap {
                Some(cm) if !cm.identity || cm.has_explicit_cids() => {
                    cm.source_key_to_cid(code, Some(len))
                }
                _ => {
                    if len != 2
                        && to_unicode.is_some_and(|tu| tu.lookup_with_length(code, 2).is_some())
                    {
                        continue;
                    }
                    code
                }
            };
            if cid == 0 || cid > u16::MAX as u32 {
                continue;
            }
            // Explicit extraction overrides the canonical collection, even
            // when it cannot be represented by one scalar or variation glyph.
            if let Some(gid) =
                FontCharacter::from_text(s).and_then(|ch| ch.mapped_glyph_index(&face))
            {
                map.insert(cid as u16, gid);
            } else if has_collection {
                // Prevent lazy canonical recovery from overriding an explicit
                // ToUnicode miss or a multi-glyph extraction string.
                map.insert(cid as u16, 0);
            } else {
                map.remove(&(cid as u16));
            }
        }
        if map.is_empty() {
            tracing::debug!(
                "substitute {}: no ToUnicode-derived CID mapping; composite glyphs will drop",
                self.base_font
            );
        }
        self.cid_to_gid = Some(map);
    }

    pub fn new_standard(base_font: String) -> Option<Self> {
        let metrics = standard_fonts::lookup(&base_font)?;
        let mut cid_widths = CidWidths::new(500.0);
        for (code, &w) in metrics.widths.iter().enumerate() {
            if w > 0 {
                cid_widths.set(code as u16, w as f64);
            }
        }
        Some(Self {
            font_type: PdfFontType::Type1,
            base_font,
            font_data: None,
            face_index: 0,
            is_substitute: false,
            direct_composite_gids: false,
            cid_widths,
            units_per_em: 1000.0,
            ascent: metrics.ascent,
            descent: metrics.descent,
            cid_to_gid: None,
            builtin_encoding_gids: None,
            orphan_gids: Vec::new(),
            encoding: None,
            to_unicode: None,
            symbolic: false,
            type1: None,
            cid_cmap: None,
            dw2: (880.0, -1000.0),
            variations: Vec::new(),
        })
    }

    pub fn new_placeholder(base_font: String) -> Self {
        Self {
            font_type: PdfFontType::Type1,
            base_font,
            font_data: None,
            face_index: 0,
            is_substitute: false,
            direct_composite_gids: false,
            cid_widths: CidWidths::new(500.0),
            units_per_em: 1000.0,
            ascent: 800.0,
            descent: -200.0,
            cid_to_gid: None,
            builtin_encoding_gids: None,
            orphan_gids: Vec::new(),
            encoding: None,
            to_unicode: None,
            symbolic: false,
            type1: None,
            cid_cmap: None,
            dw2: (880.0, -1000.0),
            variations: Vec::new(),
        }
    }

    /// Return a metrics-only copy of this logical font with its outline program
    /// bytes removed (`font_data`, `type1`, Type 3 `char_procs`, and GID maps).
    ///
    /// Preserves `font_type`, `/W`/`/DW2`/`/Widths`/`/MissingWidth`, `/Encoding`,
    /// `/ToUnicode`, and composite `cid_cmap` when the full program exceeds the
    /// font cache budget.
    pub fn without_font_program(&self) -> Self {
        let font_type = match &self.font_type {
            PdfFontType::Type3 {
                font_matrix,
                encoding,
                widths,
                first_char,
                ..
            } => PdfFontType::Type3 {
                font_matrix: *font_matrix,
                char_procs: HashMap::new(),
                encoding: encoding.clone(),
                widths: widths.clone(),
                first_char: *first_char,
            },
            other => other.clone(),
        };
        Self {
            font_type,
            base_font: self.base_font.clone(),
            font_data: None,
            face_index: 0,
            is_substitute: self.is_substitute,
            direct_composite_gids: self.direct_composite_gids,
            cid_widths: self.cid_widths.clone(),
            units_per_em: self.units_per_em,
            ascent: self.ascent,
            descent: self.descent,
            cid_to_gid: None,
            builtin_encoding_gids: None,
            orphan_gids: Vec::new(),
            encoding: self.encoding.clone(),
            to_unicode: self.to_unicode.clone(),
            symbolic: self.symbolic,
            type1: None,
            cid_cmap: self.cid_cmap.clone(),
            dw2: self.dw2,
            variations: self.variations.clone(),
        }
    }

    /// True when the composite-font code values are Unicode and glyph ids in
    /// glyph runs are already real GIDs (no CID → GID mapping applies).
    pub fn unicode_coded(&self) -> bool {
        self.cid_cmap
            .as_ref()
            .map(|c| c.codes_are_unicode && (!c.is_authoritative() || self.is_substitute))
            .unwrap_or(false)
    }

    /// A *substituted* font under a legacy byte-encoded CMap (GB / GBK / Big5 /
    /// Shift-JIS / EUC-JP / KSC): `show_text` already resolved the incoming
    /// `glyph_id` to a real system-face GID (via code → Unicode), so
    /// `glyph_outline` must pass it straight through rather than route it as a
    /// CID. Embedded legacy-CMap fonts keep the normal CID path.
    pub fn legacy_substitute(&self) -> bool {
        self.is_substitute
            && self
                .cid_cmap
                .as_ref()
                .map(|c| c.is_legacy())
                .unwrap_or(false)
    }

    /// True when this Type 0 font resolves physical drawing GIDs directly per
    /// source code in `show_text` (Unicode CMap, legacy substitute, or a
    /// loader-selected primary Type 0 substitute) while keeping PDF advances on
    /// the original CID.
    pub fn uses_direct_composite_gids(&self) -> bool {
        self.unicode_coded()
            || self.legacy_substitute()
            || (self.is_substitute
                && self.direct_composite_gids
                && matches!(self.font_type, PdfFontType::Type0CidType2))
    }

    /// Opt this substituted Type 0 font into per-source-code physical drawing
    /// GID resolution instead of synthesizing a CID-keyed `cid_to_gid` map.
    #[doc(hidden)]
    pub fn enable_direct_composite_gids(&mut self) {
        if self.is_substitute && matches!(self.font_type, PdfFontType::Type0CidType2) {
            self.direct_composite_gids = true;
        }
    }

    /// Whether this direct composite substitute can resolve at least one
    /// non-`.notdef` glyph via a Unicode/legacy CMap, canonical CJK collection,
    /// or `/ToUnicode` entry mapping a non-zero CID to a glyph in this face.
    #[doc(hidden)]
    pub fn has_direct_composite_mapping(&self, to_unicode: Option<&cmap::ToUnicodeMap>) -> bool {
        if !self.is_substitute || !matches!(self.font_type, PdfFontType::Type0CidType2) {
            return false;
        }
        if self.unicode_coded()
            || self.legacy_substitute()
            || self
                .cid_cmap
                .as_ref()
                .and_then(|c| c.collection())
                .is_some()
        {
            return true;
        }
        let Some(data) = &self.font_data else {
            return false;
        };
        let Ok(face) = ttf_parser::Face::parse(data, self.face_index) else {
            return false;
        };
        let to_unicode = to_unicode.or(self.to_unicode.as_ref());
        to_unicode
            .iter()
            .flat_map(|tu| tu.iter_with_lengths())
            .any(|(code, len, s)| {
                let cid = match &self.cid_cmap {
                    Some(cm) if !cm.identity || cm.has_explicit_cids() => {
                        cm.source_key_to_cid(code, Some(len))
                    }
                    _ => code,
                };
                cid != 0
                    && cid <= u16::MAX as u32
                    && FontCharacter::from_text(s)
                        .and_then(|ch| ch.mapped_glyph_index(&face))
                        .is_some_and(|gid| gid != 0)
            })
    }

    /// Downgrade a Unicode-coded CMap to Identity when the font program has
    /// no Unicode cmap table to resolve against (e.g. an embedded CID-keyed
    /// CFF) — codes then pass through as CIDs, which the charset or
    /// /CIDToGIDMap path can still map, instead of every glyph landing on
    /// .notdef. Call after `cid_cmap` is attached.
    pub fn validate_cid_cmap(&mut self) {
        let Some(cm) = &self.cid_cmap else { return };
        if !cm.codes_are_unicode || cm.is_authoritative() || self.font_data.is_none() {
            return;
        }
        let has_unicode_cmap = self
            .font_data
            .as_deref()
            .and_then(|d| ttf_parser::Face::parse(d, self.face_index).ok())
            .and_then(|f| f.tables().cmap)
            .map(|c| c.subtables.into_iter().any(|s| s.is_unicode()))
            .unwrap_or(false);
        if !has_unicode_cmap {
            tracing::warn!(
                "font {}: Unicode CMap but no Unicode cmap table; treating codes as CIDs",
                self.base_font
            );
            let wmode = cm.wmode;
            self.cid_cmap = Some(cmap::CidCMap::identity(wmode));
        }
    }

    /// Resolve a Unicode code point to (GID, advance) for Unicode-coded
    /// composite fonts. The advance is normalized to 1/1000 text-space units
    /// (the composite-font convention).
    pub fn unicode_glyph(&self, code: u32) -> Option<(u16, f64)> {
        unicode_lookup::lookup::<true>(self, code)
    }

    /// Look up `/ToUnicode` for a segmented PDF character code `(code, raw_code,
    /// length)`. For composite fonts with an encoding CMap, match the entry's
    /// actual source byte width in both directions (`<41>` only matches 1-byte
    /// codes and `<0041>` only matches 2-byte codes), while allowing UTF-16
    /// surrogate pairs (`code != raw_code`) to fall back to a scalar key.
    #[doc(hidden)]
    pub fn lookup_to_unicode(&self, code: u32, raw_code: u32, length: u8) -> Option<&str> {
        let map = self.to_unicode.as_ref()?;
        if matches!(self.font_type, PdfFontType::Type0CidType2) && self.cid_cmap.is_some() {
            map.lookup_with_length(raw_code, length)
                .or_else(|| (code != raw_code).then(|| map.lookup(code)).flatten())
        } else {
            map.lookup_with_length(raw_code, length)
                .or_else(|| map.lookup(raw_code))
                .or_else(|| (code != raw_code).then(|| map.lookup(code)).flatten())
        }
    }

    /// Resolve a Unicode/legacy substitute code to a physical GID. ToUnicode
    /// has priority, including raw UTF-16 source keys and variation sequences.
    /// When it names no single character (`fi`) or one this face lacks, the
    /// CMap's own Unicode for the code still selects the glyph. PDF advances
    /// must be obtained separately from the original CID.
    pub fn composite_unicode_gid(&self, code: u32, raw_code: u32, length: u8) -> Option<u16> {
        self.lookup_to_unicode(code, raw_code, length)
            .and_then(FontCharacter::from_text)
            .and_then(|character| self.character_gid(character))
            .or_else(|| {
                let character = self
                    .cid_cmap
                    .as_ref()?
                    .decode_text(code, length)?
                    .font_character()?;
                self.character_gid(character)
            })
    }

    /// Resolve a composite PDF source code to a physical GID on this direct
    /// composite substitute using its own `/ToUnicode` and `/Encoding` CMap.
    /// PDF advances must be obtained separately from the original CID.
    #[doc(hidden)]
    pub fn composite_direct_gid(&self, code: u32, raw_code: u32, length: u8) -> Option<u16> {
        self.resolve_composite_substitute_gid(self, code, raw_code, length)
    }

    /// Resolve a composite PDF source code to a physical GID in this direct
    /// drawing substitute using `primary`'s `/ToUnicode` and `/Encoding` CMap.
    ///
    /// Resolving per source code avoids collapsing multiple source codes that
    /// share a damaged CID into a single `cid_to_gid` slot, while leaving all
    /// PDF advances on `primary`'s original CID.
    pub(crate) fn resolve_composite_substitute_gid(
        &self,
        primary: &LoadedFont,
        code: u32,
        raw_code: u32,
        length: u8,
    ) -> Option<u16> {
        if let Some(text) = primary.lookup_to_unicode(code, raw_code, length) {
            if let Some(gid) =
                FontCharacter::from_text(text).and_then(|character| self.character_gid(character))
            {
                return Some(gid);
            }
            // Match `composite_unicode_gid` for Unicode/legacy CMaps (where the
            // CMap's own Unicode can still recover a multi-glyph or unmapped
            // `/ToUnicode`), while matching `build_substitute_cid_to_gid_from`
            // for canonical CJK collections (never letting the collection
            // override an explicit `/ToUnicode` miss).
            let fallback_to_cmap = primary
                .cid_cmap
                .as_ref()
                .is_some_and(|cm| cm.codes_are_unicode || cm.is_legacy());
            if !fallback_to_cmap {
                return None;
            }
        }
        let character = primary
            .cid_cmap
            .as_ref()?
            .decode_text(code, length)?
            .font_character()?;
        self.character_gid(character)
    }

    fn character_gid(&self, character: FontCharacter) -> Option<u16> {
        if character.variation_selector.is_none() {
            return unicode_lookup::lookup::<false>(self, character.character as u32)
                .map(|(gid, _)| gid)
                .filter(|&gid| gid != 0);
        }
        let data = self.font_data.as_ref()?;
        let face = ttf_parser::Face::parse(data, self.face_index).ok()?;
        character.mapped_glyph_index(&face)
    }

    /// Configure OpenType variation axes from FontDescriptor values, so a
    /// *variable* embedded (or substituted) font program renders at the intended
    /// instance rather than the default master. Each axis is requested only when
    /// the corresponding descriptor entry is present and finite; `set_variation`
    /// then ignores any the font program does not actually carry, so this is a
    /// no-op for ordinary static fonts. `width_pct` is the `/FontStretch` value
    /// already mapped to a percentage; `italic_angle` is `/ItalicAngle` (degrees,
    /// counter-clockwise — the same convention as the `slnt` axis).
    ///
    /// Policy: the descriptor selectors drive the instance. When a selector
    /// equals the font's default axis value — the well-formed case, since
    /// `/FontWeight` derives from OS/2 `usWeightClass`, which tracks the default
    /// master — `set_variation` normalizes to the default coordinate and changes
    /// nothing. A selector that *contradicts* the default master (rare, malformed
    /// metadata) is honored as written, re-instancing to the requested value.
    pub fn set_variations(
        &mut self,
        weight: Option<f64>,
        width_pct: Option<f64>,
        italic_angle: Option<f64>,
        italic: bool,
    ) {
        let mut v: Vec<([u8; 4], f32)> = Vec::new();
        let mut push = |tag: &[u8; 4], value: f64| {
            let value = value as f32;
            if value.is_finite() {
                v.push((*tag, value));
            }
        };
        if let Some(w) = weight.filter(|w| *w > 0.0) {
            push(b"wght", w);
        }
        if let Some(wd) = width_pct.filter(|wd| *wd > 0.0) {
            push(b"wdth", wd);
        }
        if let Some(a) = italic_angle.filter(|a| *a != 0.0) {
            push(b"slnt", a);
        }
        if italic {
            v.push((*b"ital", 1.0));
        }
        self.variations = v;
    }

    /// Apply the descriptor-derived variation axes to a freshly parsed face.
    /// No-op unless the font is variable and carries the requested axes.
    fn apply_variations(&self, face: &mut ttf_parser::Face) {
        for (tag, value) in &self.variations {
            let _ = face.set_variation(ttf_parser::Tag::from_bytes(tag), *value);
        }
    }

    /// Get glyph outline for a given glyph ID (or CID for CID fonts).
    pub fn glyph_outline(&self, glyph_id: u16) -> Option<GlyphOutline> {
        if let Some(t1) = &self.type1 {
            return t1.glyph_outline_by_gid(glyph_id);
        }
        let data = self.font_data.as_ref()?;
        let mut face = ttf_parser::Face::parse(data, self.face_index).ok()?;
        self.apply_variations(&mut face);

        let actual_gid = self.drawing_gid_with_face(glyph_id, &face)?;
        let gid = ttf_parser::GlyphId(actual_gid);

        let mut builder = OutlineBuilder::new();
        face.outline_glyph(gid, &mut builder)?;

        let advance = face
            .glyph_hor_advance(gid)
            .map(|a| a as f64)
            .unwrap_or_else(|| self.cid_widths.get(glyph_id));

        Some(GlyphOutline {
            commands: builder.commands,
            advance_width: advance,
        })
    }

    pub(crate) fn drawing_gid_with_face(
        &self,
        glyph_id: u16,
        face: &ttf_parser::Face<'_>,
    ) -> Option<u16> {
        if self.unicode_coded() || self.legacy_substitute() {
            return Some(glyph_id);
        }
        if self.is_substitute && self.direct_composite_gids {
            return (glyph_id != 0).then_some(glyph_id);
        }
        let Some(map) = &self.cid_to_gid else {
            return Some(glyph_id);
        };
        if let Some(&gid) = map.get(&glyph_id) {
            return (gid != 0).then_some(gid);
        }
        self.collection_substitute_gid(glyph_id, face)
    }

    /// Resolve only CIDs actually drawn. Scanning all 65,536 canonical CIDs
    /// at every font load disproportionately penalizes small substitute pages.
    #[inline(never)]
    fn collection_substitute_gid(&self, cid: u16, face: &ttf_parser::Face<'_>) -> Option<u16> {
        if !self.is_substitute || cid == 0 {
            return None;
        }
        self.cid_cmap
            .as_ref()?
            .collection()?
            .unicode(cid as u32)?
            .font_character()?
            .mapped_glyph_index(face)
    }

    /// Fraction of sampled, mapped CIDs whose embedded outline fails to resolve.
    ///
    /// Some embedded CID-keyed CFF subsets are defective: their per-FD Private
    /// DICTs are unparseable, stranding the local subroutines, so `ttf-parser`
    /// (like FreeType) returns no outline for every glyph that calls a local
    /// subr — often the majority. The font loader uses a high rate to switch to
    /// a system substitute rather than render mostly-blank text. Returns 0.0 for
    /// anything that is not an embedded composite font with a CID→GID map.
    pub fn embedded_outline_failure_rate(&self) -> f32 {
        if self.is_substitute || !matches!(self.font_type, PdfFontType::Type0CidType2) {
            return 0.0;
        }
        let Some(map) = &self.cid_to_gid else {
            return 0.0;
        };
        let Some(data) = &self.font_data else {
            return 0.0;
        };
        let Ok(face) = ttf_parser::Face::parse(data, self.face_index) else {
            return 0.0;
        };
        let mut cids: Vec<u16> = map.keys().copied().collect();
        if cids.is_empty() {
            return 0.0;
        }
        // Sample evenly across the (sorted) CID range so several CFF FDs are
        // exercised, not just one. Valid blank glyphs (spaces, zero-length
        // `loca` entries) are excluded; only genuinely missing/unreadable
        // outlines count as failures.
        cids.sort_unstable();
        let step = (cids.len() / 64).max(1);
        let (mut total, mut fail) = (0u32, 0u32);
        let mut i = 0;
        while i < cids.len() {
            let gid = map[&cids[i]];
            total += 1;
            if resolution::physical_glyph_state(&face, gid) == resolution::GlyphState::Missing {
                fail += 1;
            }
            i += step;
        }
        fail as f32 / total as f32
    }

    /// Whether an embedded font program parsed into usable outline tables.
    ///
    /// Returns `false` when neither `ttf-parser` nor the Type 1 parser accepted
    /// the bytes, when an sfnt/OTF program carries no parseable outline table
    /// (`glyf`, `CFF `, or `CFF2`), or when more than half of non-blank sampled
    /// glyphs fail to outline due to corrupt glyph data. Valid empty glyphs
    /// (such as space, valid zero-contour space records, or zero-length CAD
    /// `loca` entries) are treated as usable.
    pub fn has_usable_embedded_outlines(&self) -> bool {
        self.probe_embedded_outlines() != EmbeddedOutlineProbe::Unusable
    }

    /// Classify an embedded font program as completely healthy, partially
    /// corrupt (usable as primary face, but with at least one sampled
    /// unreadable glyph), or unusable (majority/all sampled glyphs unreadable).
    #[doc(hidden)]
    pub fn probe_embedded_outlines(&self) -> EmbeddedOutlineProbe {
        if self.is_type3() || self.is_substitute {
            return EmbeddedOutlineProbe::Healthy;
        }
        if let Some(t1) = &self.type1 {
            return t1.probe_outlines();
        }
        let Some(data) = &self.font_data else {
            return EmbeddedOutlineProbe::Unusable;
        };
        let Ok(mut face) = ttf_parser::Face::parse(data, self.face_index) else {
            return EmbeddedOutlineProbe::Unusable;
        };
        self.apply_variations(&mut face);
        if face.tables().glyf.is_none()
            && face.tables().cff.is_none()
            && face.tables().cff2.is_none()
        {
            return EmbeddedOutlineProbe::Unusable;
        }

        let (mut drawn, mut unreadable) = (0u32, 0u32);
        let mut cmap_budget = resolution::CMAP_PROBE_BUDGET;
        let probe = |gid: u16, drawn: &mut u32, unreadable: &mut u32, cmap_budget: &mut usize| {
            match resolution::physical_glyph_state(&face, gid) {
                resolution::GlyphState::Drawn => *drawn += 1,
                resolution::GlyphState::Missing => *unreadable += 1,
                resolution::GlyphState::Blank => {
                    if resolution::glyf_blank_is_corrupt(&face, gid, cmap_budget) {
                        *unreadable += 1;
                    }
                }
            }
        };

        let mapped_gids: Vec<u16> = self
            .cid_to_gid
            .as_ref()
            .map(|map| {
                let mut gids: Vec<u16> = map.values().copied().filter(|&g| g != 0).collect();
                gids.sort_unstable();
                gids.dedup();
                gids
            })
            .unwrap_or_default();

        if !mapped_gids.is_empty() {
            let step = (mapped_gids.len() / 64).max(1);
            let mut i = 0;
            while i < mapped_gids.len() {
                probe(
                    mapped_gids[i],
                    &mut drawn,
                    &mut unreadable,
                    &mut cmap_budget,
                );
                i += step;
            }
        } else {
            let num_glyphs = face
                .tables()
                .cff
                .as_ref()
                .map(|cff| cff.number_of_glyphs())
                .unwrap_or_else(|| face.number_of_glyphs());
            if num_glyphs <= 1 {
                let mut builder = OutlineBuilder::new();
                if face
                    .outline_glyph(ttf_parser::GlyphId(0), &mut builder)
                    .is_some()
                    || resolution::readable_glyph(&face, 0)
                {
                    return EmbeddedOutlineProbe::Healthy;
                }
                return EmbeddedOutlineProbe::Unusable;
            }
            let count = (num_glyphs as usize) - 1;
            let step = (count / 64).max(1);
            let mut gid = 1usize;
            while gid < num_glyphs as usize {
                probe(gid as u16, &mut drawn, &mut unreadable, &mut cmap_budget);
                gid += step;
            }
            if drawn == 0 && unreadable == 0 && face.tables().glyf.is_some() {
                let mut found = 0usize;
                for g in 1..num_glyphs {
                    if resolution::glyf_has_non_empty_record(&face, g) {
                        probe(g, &mut drawn, &mut unreadable, &mut cmap_budget);
                        found += 1;
                        if found >= 64 {
                            break;
                        }
                    }
                }
            }
        }

        if unreadable > 0 && unreadable > drawn {
            EmbeddedOutlineProbe::Unusable
        } else if unreadable > 0 {
            EmbeddedOutlineProbe::PartiallyCorrupt
        } else {
            EmbeddedOutlineProbe::Healthy
        }
    }

    /// Classify a batch of glyph IDs in an embedded font as having a corrupt /
    /// unreadable embedded outline record (`true`) vs drawn, valid blank, or
    /// unmapped (`false`), applying the same bounded `glyf` blank-record check
    /// used by [`Self::probe_embedded_outlines`]. Parses the embedded font
    /// program at most once per batch.
    #[doc(hidden)]
    pub fn classify_embedded_glyphs_unreadable(
        &self,
        glyph_ids: &[u16],
        cmap_budget: &mut usize,
        mut record: impl FnMut(u16, bool),
    ) {
        if self.is_substitute || self.is_type3() || !self.has_font_data() {
            for &gid in glyph_ids {
                record(gid, false);
            }
            return;
        }
        if let Some(t1) = &self.type1 {
            for &gid in glyph_ids {
                record(gid, gid != 0 && t1.is_gid_unreadable(gid));
            }
            return;
        }
        let Some(data) = &self.font_data else {
            for &gid in glyph_ids {
                record(gid, false);
            }
            return;
        };
        let Ok(mut face) = ttf_parser::Face::parse(data, self.face_index) else {
            for &gid in glyph_ids {
                record(gid, gid != 0);
            }
            return;
        };
        self.apply_variations(&mut face);
        let num_glyphs = face.number_of_glyphs();
        // When the font has an explicit GID mapping (`/CIDToGIDMap`, CFF
        // charset, Unicode cmap, or simple-font `code_to_gid`), a resolved
        // nonzero GID outside `num_glyphs` is an unreadable out-of-range
        // mapping. Absent mappings (`None`) and `.notdef` (0) stay distinct.
        let explicit_gid_map = self.cid_to_gid.is_some()
            || !matches!(self.font_type, PdfFontType::Type0CidType2)
            || self.unicode_coded();
        for &glyph_id in glyph_ids {
            let unreadable = if glyph_id == 0 {
                false
            } else {
                match self.drawing_gid_with_face(glyph_id, &face) {
                    Some(gid) if gid != 0 && (gid < num_glyphs || explicit_gid_map) => {
                        match resolution::physical_glyph_state(&face, gid) {
                            resolution::GlyphState::Drawn => false,
                            resolution::GlyphState::Missing => true,
                            resolution::GlyphState::Blank => {
                                match resolution::glyf_blank_header(&face, gid) {
                                    resolution::GlyfBlankHeader::ZeroLength => false,
                                    resolution::GlyfBlankHeader::Corrupt => true,
                                    resolution::GlyfBlankHeader::ValidNonEmpty => {
                                        resolution::glyf_blank_cmap_probe(&face, gid, cmap_budget)
                                            .unwrap_or(false)
                                    }
                                }
                            }
                        }
                    }
                    _ => false,
                }
            };
            record(glyph_id, unreadable);
        }
    }

    /// Classify shown embedded `(glyph_id, code, raw_code, length)` tuples in
    /// one batched face parse, consulting direct PDF text evidence before
    /// spending `cmap_budget` on reverse scans and keeping an exhausted probe
    /// distinct from a confirmed readable result.
    #[doc(hidden)]
    pub fn classify_embedded_glyphs_with_codes(
        &self,
        items: &[(u16, u32, u32, u8)],
        cmap_budget: &mut usize,
        mut record: impl FnMut(u16, EmbeddedGlyphHealthOutcome),
    ) {
        if self.is_substitute || self.is_type3() {
            for &(gid, _, _, _) in items {
                record(gid, EmbeddedGlyphHealthOutcome::Readable);
            }
            return;
        }
        if let Some(t1) = &self.type1 {
            for &(gid, code, raw_code, _) in items {
                let outcome = if gid == 0 {
                    EmbeddedGlyphHealthOutcome::Readable
                } else if t1.is_gid_unreadable(gid) {
                    EmbeddedGlyphHealthOutcome::Unreadable
                } else if self.to_unicode.is_some() && t1.is_blank_gid(gid) {
                    EmbeddedGlyphHealthOutcome::CodeDependent {
                        unreadable: self.blank_hides_text(code, raw_code),
                    }
                } else {
                    EmbeddedGlyphHealthOutcome::Readable
                };
                record(gid, outcome);
            }
            return;
        }
        let Some(data) = &self.font_data else {
            for &(gid, _, _, _) in items {
                record(gid, EmbeddedGlyphHealthOutcome::Readable);
            }
            return;
        };
        let Ok(mut face) = ttf_parser::Face::parse(data, self.face_index) else {
            for &(gid, _, _, _) in items {
                let outcome = if gid != 0 {
                    EmbeddedGlyphHealthOutcome::Unreadable
                } else {
                    EmbeddedGlyphHealthOutcome::Readable
                };
                record(gid, outcome);
            }
            return;
        };
        self.apply_variations(&mut face);
        let num_glyphs = face.number_of_glyphs();
        let explicit_gid_map = self.cid_to_gid.is_some()
            || !matches!(self.font_type, PdfFontType::Type0CidType2)
            || self.unicode_coded();
        for &(glyph_id, code, raw_code, length) in items {
            let outcome = if glyph_id == 0 {
                EmbeddedGlyphHealthOutcome::Readable
            } else {
                match self.drawing_gid_with_face(glyph_id, &face) {
                    Some(gid) if gid != 0 && (gid < num_glyphs || explicit_gid_map) => {
                        match resolution::physical_glyph_state(&face, gid) {
                            resolution::GlyphState::Drawn => EmbeddedGlyphHealthOutcome::Readable,
                            resolution::GlyphState::Missing => {
                                EmbeddedGlyphHealthOutcome::Unreadable
                            }
                            resolution::GlyphState::Blank => {
                                match resolution::glyf_blank_header(&face, gid) {
                                    resolution::GlyfBlankHeader::ZeroLength => {
                                        EmbeddedGlyphHealthOutcome::Readable
                                    }
                                    resolution::GlyfBlankHeader::Corrupt => {
                                        EmbeddedGlyphHealthOutcome::Unreadable
                                    }
                                    resolution::GlyfBlankHeader::ValidNonEmpty => {
                                        if let Some(visible) =
                                            self.direct_code_visibility(code, raw_code, length)
                                        {
                                            EmbeddedGlyphHealthOutcome::CodeDependent {
                                                unreadable: visible,
                                            }
                                        } else {
                                            match resolution::glyf_blank_cmap_probe(
                                                &face,
                                                gid,
                                                cmap_budget,
                                            ) {
                                                Some(unreadable) => {
                                                    EmbeddedGlyphHealthOutcome::CodeDependentFallback {
                                                        unreadable,
                                                    }
                                                }
                                                None => EmbeddedGlyphHealthOutcome::Exhausted,
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    _ => EmbeddedGlyphHealthOutcome::Readable,
                }
            };
            record(glyph_id, outcome);
        }
    }

    /// Evaluate a previously cached code-dependent blank glyph (Type 1 custom
    /// blank with `/ToUnicode` or TrueType valid-header nonempty zero-contour
    /// record) without reparsing the font face or spending `cmap_budget`.
    #[doc(hidden)]
    pub fn code_dependent_blank_is_unreadable(
        &self,
        code: u32,
        raw_code: u32,
        length: u8,
    ) -> Option<bool> {
        if self.type1.is_some() {
            return Some(self.blank_hides_text(code, raw_code));
        }
        self.direct_code_visibility(code, raw_code, length)
    }

    /// Whether a single shown `(glyph_id, code, raw_code, length)` in this
    /// embedded font is unreadable, using the same code-aware classification
    /// as [`Self::classify_embedded_glyphs_with_codes`] (so cache misses after
    /// `MAX_TRACKED_FONTS` still honor `/ToUnicode` and direct code evidence).
    #[doc(hidden)]
    pub fn is_shown_embedded_glyph_unreadable(
        &self,
        glyph_id: u16,
        code: u32,
        raw_code: u32,
        length: u8,
        cmap_budget: &mut usize,
    ) -> bool {
        let mut unreadable = false;
        self.classify_embedded_glyphs_with_codes(
            &[(glyph_id, code, raw_code, length)],
            cmap_budget,
            |_, out| {
                unreadable = matches!(
                    out,
                    EmbeddedGlyphHealthOutcome::Unreadable
                        | EmbeddedGlyphHealthOutcome::CodeDependent { unreadable: true }
                        | EmbeddedGlyphHealthOutcome::CodeDependentFallback { unreadable: true }
                );
            },
        );
        unreadable
    }

    /// Whether `glyph_id` resolves in this embedded font to a physical glyph
    /// whose outline record is corrupt/unreadable (including damaged
    /// zero-contour `glyf` records), charging any blank-record `cmap` scan
    /// against `cmap_budget`.
    #[doc(hidden)]
    pub fn is_embedded_glyph_unreadable_with_budget(
        &self,
        glyph_id: u16,
        cmap_budget: &mut usize,
    ) -> bool {
        let mut unreadable = false;
        self.classify_embedded_glyphs_unreadable(&[glyph_id], cmap_budget, |_, u| unreadable = u);
        unreadable
    }

    /// Whether `glyph_id` resolves in this embedded font to a physical glyph
    /// whose outline record is corrupt/unreadable (including damaged
    /// zero-contour `glyf` records).
    #[doc(hidden)]
    pub fn is_embedded_glyph_unreadable(&self, glyph_id: u16) -> bool {
        let mut cmap_budget = resolution::CMAP_PROBE_BUDGET;
        self.is_embedded_glyph_unreadable_with_budget(glyph_id, &mut cmap_budget)
    }

    /// Get glyph advance width.
    ///
    /// Returns width in the font's native unit system:
    /// - Type3: glyph units (typically 1000-based, use with font_matrix)
    /// - CID/Type0: 1/1000 of text space (use as width/1000 * font_size)
    /// - TrueType: font units (use as width/units_per_em * font_size)
    pub fn glyph_advance(&self, glyph_id: u16) -> f64 {
        if self.is_type3() {
            return self.type3_glyph_width(glyph_id);
        }
        // For CID fonts, always prefer the PDF /W array — it's authoritative
        if matches!(self.font_type, PdfFontType::Type0CidType2) {
            return self.cid_widths.get(glyph_id);
        }
        // For simple TrueType, try the font program's hmtx table
        if let Some(data) = &self.font_data {
            if let Ok(mut face) = ttf_parser::Face::parse(data, self.face_index) {
                self.apply_variations(&mut face);
                let gid = ttf_parser::GlyphId(glyph_id);
                if let Some(a) = face.glyph_hor_advance(gid) {
                    return a as f64;
                }
            }
        }
        self.cid_widths.get(glyph_id)
    }

    /// Per-CID vertical metric (w1y, vx, vy) from the font's /W2 array, or
    /// `None` when this CID has no explicit entry (caller uses /DW2).
    pub fn cid_v_metric(&self, cid: u16) -> Option<(f64, f64, f64)> {
        self.cid_widths.get_v(cid)
    }

    /// The denominator for converting glyph_advance() to user-space units.
    /// Usage: advance_user = glyph_advance() / advance_divisor() * font_size
    pub fn advance_divisor(&self) -> f64 {
        match &self.font_type {
            PdfFontType::Type0CidType2 => 1000.0,
            PdfFontType::Type3 { .. } => self.units_per_em,
            _ => self.units_per_em,
        }
    }

    /// Effective glyph name for a 1-byte simple-font character code, following
    /// the same PDF `/Encoding` → Type 1 built-in `/Encoding` precedence as
    /// [`Self::code_to_gid`].
    pub(crate) fn simple_glyph_name(&self, code: u8) -> Option<&str> {
        if let Some(t1) = &self.type1 {
            if let Some(enc) = &self.encoding {
                if let Some(name) = enc.glyph_name(code) {
                    if t1.gid_for_name(name).is_some()
                        || glyph_list::glyph_name_to_string(name).is_some()
                    {
                        return Some(name);
                    }
                }
            }
            if let Some(name) = t1.builtin_name(code) {
                return Some(name);
            }
        }
        self.encoding.as_ref().and_then(|enc| enc.glyph_name(code))
    }

    /// Copy an embedded Type 1 font's effective code → glyph-name mapping
    /// (PDF `/Encoding` overlaid onto the Type 1 program's built-in `/Encoding`)
    /// onto a substitute face that carries no `Type1Font` program.
    #[doc(hidden)]
    pub fn inherit_type1_encoding_from(&mut self, primary: &LoadedFont) {
        if primary.type1.is_none() {
            return;
        }
        let mut merged = encoding::Encoding::empty();
        let mut any = false;
        for code in 0u8..=255 {
            if let Some(name) = primary.simple_glyph_name(code) {
                if name != ".notdef" {
                    merged.apply_difference(code, name);
                    any = true;
                }
            }
        }
        if any {
            self.encoding = Some(merged);
        }
    }

    /// Map a 1-byte character code to a glyph ID for a *simple* font, using the
    /// font's /Encoding (→ glyph name) and the embedded program's cmap/charset.
    /// Returns `None` if no glyph data is available or no mapping is found.
    /// Character codes are not a valid fallback namespace for sfnt glyph IDs.
    pub fn code_to_gid(&self, code: u16) -> Option<u16> {
        // Embedded Type 1: resolve via the PDF /Encoding (or the font's built-in
        // encoding) to a glyph name, then to the synthetic Type 1 glyph id.
        if let Some(t1) = &self.type1 {
            let code8 = code as u8;
            if let Some(enc) = &self.encoding {
                if let Some(name) = enc.glyph_name(code8) {
                    if let Some(g) = t1.gid_for_name(name) {
                        return Some(g);
                    }
                    // A missing named character must not become the built-in
                    // glyph of another letter; a name without character meaning
                    // (`/g3`) still falls back to the program's own encoding.
                    if glyph_list::glyph_name_to_char(name).is_some() {
                        return None;
                    }
                }
            }
            if let Some(name) = t1.builtin_name(code8) {
                if let Some(g) = t1.gid_for_name(name) {
                    return Some(g);
                }
            }
            return None;
        }

        let data = self.font_data.as_ref()?;
        let face = ttf_parser::Face::parse(data, self.face_index).ok()?;
        let code8 = code as u8;

        // 1. /Encoding → glyph name → (by-name lookup, else name→Unicode→cmap).
        if let Some(enc) = &self.encoding {
            if let Some(name) = enc.glyph_name(code8) {
                if let Some(g) = face.glyph_index_by_name(name) {
                    return Some(g.0);
                }
                if let Some(ch) = glyph_list::glyph_name_to_char(name) {
                    if let Some(g) = face.glyph_index(ch) {
                        return Some(g.0);
                    }
                }
            }
        }

        // 1b. For a substituted simple font whose /Encoding did not resolve the
        //     code (custom subset glyph names, or absent on a symbolic subset),
        //     consult /ToUnicode against the substitute face's Unicode cmap
        //     before guessing from raw-byte Symbol/MacRoman/Latin-1 cmap slots.
        if self.is_substitute {
            if let Some(gid) = self
                .to_unicode
                .as_ref()
                .and_then(|map| {
                    map.lookup_with_length(code as u32, 1)
                        .or_else(|| map.lookup(code as u32))
                })
                .and_then(FontCharacter::from_text)
                .and_then(|ch| ch.mapped_glyph_index(&face))
            {
                return Some(gid);
            }
        }

        // 2. The embedded CFF program's built-in encoding (charcode → GID),
        //    for codes the PDF /Encoding doesn't resolve (symbolic fonts, or
        //    subset fonts whose glyph names the /Encoding tables don't know).
        if let Some(map) = &self.builtin_encoding_gids {
            if let Some(&g) = map.get(&code) {
                return Some(g);
            }
        }

        // 3. Built-in cmap subtables — common for symbolic embedded fonts.
        //    When a substitute carries an explicit encoding (including a Type 1
        //    program's built-in encoding), never fall through to raw-byte
        //    MacRoman/PUA/Latin-1 slots that could map a remapped byte (e.g.
        //    65 -> `/B`) onto an unrelated glyph (`/A`), while preserving
        //    Windows Symbol / PUA `0xF000 | code` lookup for codes whose
        //    declared glyph name matches their canonical Symbol slot.
        let substitute_has_explicit_encoding = self.is_substitute && self.encoding.is_some();
        let canonical_symbol_code = self.encoding.as_ref().is_some_and(|enc| {
            enc.is_canonical_symbol_code(code8) || {
                let base = self.base_font.rsplit('+').next().unwrap_or(&self.base_font);
                let table = if base.contains("ZapfDingbats") || base.contains("Dingbats") {
                    Some(&encoding::ZAPF_DINGBATS_ENCODING)
                } else if base.contains("Symbol") {
                    Some(&encoding::SYMBOL_ENCODING)
                } else {
                    None
                };
                table.is_some_and(|t| {
                    let expected = t[code8 as usize];
                    expected.is_some() && enc.glyph_name(code8) == expected
                })
            }
        });
        let allow_symbol_pua = !substitute_has_explicit_encoding || canonical_symbol_code;
        if let Some(cmap) = face.tables().cmap {
            // (3,0) Windows Symbol: try the 0xF000 PUA offset then the raw code.
            if allow_symbol_pua {
                for st in cmap.subtables {
                    if st.platform_id == ttf_parser::PlatformId::Windows && st.encoding_id == 0 {
                        if let Some(g) = st
                            .glyph_index(0xF000 | code as u32)
                            .or_else(|| st.glyph_index(code as u32))
                        {
                            return Some(g.0);
                        }
                    }
                }
            }
            // (3,1) Windows Unicode at the 0xF000 PUA offset: symbolic fonts
            // often park their glyphs in the PUA but ship only a Unicode
            // subtable (the raw code is retried as Latin-1 in step 4).
            if self.symbolic && allow_symbol_pua {
                for st in cmap.subtables {
                    if st.platform_id == ttf_parser::PlatformId::Windows && st.encoding_id == 1 {
                        if let Some(g) = st.glyph_index(0xF000 | code as u32) {
                            return Some(g.0);
                        }
                    }
                }
            }
            // (1,0) Macintosh Roman: an explicit PDF encoding must resolve its
            // glyph name through the MacRoman slot, not reuse an unrelated byte.
            for st in cmap.subtables {
                if st.platform_id == ttf_parser::PlatformId::Macintosh && st.encoding_id == 0 {
                    if let Some(mac) = self
                        .encoding
                        .as_ref()
                        .and_then(|e| e.glyph_name(code8))
                        .and_then(mac_roman_code_for_name)
                    {
                        if let Some(g) = st.glyph_index(mac) {
                            return Some(g.0);
                        }
                    }
                    if !substitute_has_explicit_encoding
                        && (self.encoding.is_none() || self.symbolic)
                    {
                        if let Some(g) = st.glyph_index(code as u32) {
                            return Some(g.0);
                        }
                    }
                }
            }
        }

        // An embedded TrueType program without any cmap addresses its glyphs by
        // character code, which producers of such subsets rely on. A substitute
        // face's glyph ids are unrelated to PDF codes, so it never does this.
        if face.tables().cmap.is_none() && face.tables().glyf.is_some() && !self.is_substitute {
            return (code < face.number_of_glyphs()).then_some(code);
        }

        // 4. Only an unspecified encoding permits the Latin-1 fallback. A
        // declared missing glyph must not turn into a different encoded letter.
        if self.encoding.is_some() && (!self.symbolic || self.is_substitute) {
            return None;
        }
        if let Some(g) = face.glyph_index(code8 as char) {
            return Some(g.0);
        }

        None
    }

    /// Recover glyphs that no encoding can reach in Quartz (macOS) subsets.
    ///
    /// Quartz names each subset glyph after the MacRoman slot it re-encoded the
    /// text to, and names glyphs with no MacRoman-compatible slot literally
    /// ".notdef" (charset SID 0), addressing them by their original Type 1 code
    /// — e.g. the CMSY minus at code 0. Pair the /Widths-declared codes that no
    /// resolution step maps with those orphan GIDs, both in ascending order.
    /// Call after `encoding` and `cid_widths` are attached.
    pub fn map_unencoded_orphans(&mut self) {
        if self.orphan_gids.is_empty() {
            return;
        }
        let unmapped: Vec<u16> = (0..=255u16)
            .filter(|&c| self.cid_widths.get_opt(c).is_some_and(|w| w > 0.0))
            .filter(|&c| self.code_to_gid(c).is_none())
            .collect();
        if unmapped.is_empty() {
            return;
        }
        let orphans = self.orphan_gids.clone();
        let map = self.builtin_encoding_gids.get_or_insert_with(HashMap::new);
        for (&code, &gid) in unmapped.iter().zip(&orphans) {
            map.entry(code).or_insert(gid);
        }
    }

    /// Advance for a simple-font glyph, in font units consistent with
    /// [`advance_divisor`](Self::advance_divisor). Prefers the PDF /Widths entry
    /// (keyed by character *code*, authoritative per the spec) over the font's hmtx.
    pub fn simple_glyph_advance(&self, code: u16, gid: u16) -> f64 {
        if let PdfFontType::Type3 { font_matrix, .. } = &self.font_type {
            // Type 3 /Widths are expressed in glyph space, so apply the
            // font matrix before converting them to the renderer's normalized
            // font-unit scale. Simple-font /Widths are already in 1/1000
            // text-space units and must not take this path.
            // /Widths advance along the horizontal glyph-space axis. The
            // y-component is a shear/rotation component, not extra horizontal
            // advance, so do not use the transformed axis length here.
            let scale = font_matrix[0];
            return self.type3_glyph_width(code) * scale * self.units_per_em;
        }
        // PDF /Widths are in 1/1000 glyph-space units; rescale to font units.
        if let Some(w) = self.cid_widths.get_opt(code) {
            return w / 1000.0 * self.units_per_em;
        }
        // When a substituted simple font supplied PDF /Widths with a descriptor,
        // codes outside /Widths advance by /MissingWidth rather than the
        // substitute face's unrelated hmtx.
        if self.is_substitute && self.cid_widths.has_authoritative_default() {
            return self.cid_widths.default_width() / 1000.0 * self.units_per_em;
        }
        if let Some(data) = &self.font_data {
            if let Ok(mut face) = ttf_parser::Face::parse(data, self.face_index) {
                // Match the varied outline: a variable font's hmtx advance must
                // be read at the same instance `glyph_outline` renders.
                self.apply_variations(&mut face);
                if let Some(a) = face.glyph_hor_advance(ttf_parser::GlyphId(gid)) {
                    return a as f64;
                }
            }
        }
        self.cid_widths.get(code) / 1000.0 * self.units_per_em
    }

    /// Decode a show-text byte string to Unicode for text extraction.
    /// Uses /ToUnicode when present, else /Encoding + the Adobe Glyph List.
    pub fn decode_to_string(&self, bytes: &[u8]) -> String {
        let mut out = String::new();
        if matches!(self.font_type, PdfFontType::Type0CidType2) {
            // Composite font: segment codes through the /Encoding CMap when
            // present (variable-length codespaces), else fall back to the
            // /ToUnicode codespace width or the 2-byte Identity convention.
            if let Some(cm) = &self.cid_cmap {
                let mut i = 0usize;
                while i < bytes.len() {
                    let start = i;
                    let (code, len) = cm.next_code(&bytes[i..]);
                    i += len.max(1);
                    let raw_code = bytes[start..i.min(bytes.len())]
                        .iter()
                        .fold(0u32, |value, &byte| (value << 8) | byte as u32);
                    self.append_decoded_code(code, len as u8, raw_code, &mut out);
                }
            } else if let Some(map) = &self.to_unicode {
                map.append_fixed_width(bytes, &mut out);
            }
        } else {
            for &b in bytes {
                self.append_decoded_code(b as u32, 1, b as u32, &mut out);
            }
        }
        out
    }

    /// Append the Unicode for one already-segmented PDF character code.
    ///
    /// `code` and `len` are the encoding CMap's decoded code and source byte
    /// length. `raw_code` is the big-endian value of those source bytes (it
    /// differs from `code` for UTF-16 surrogate pairs). Sharing this decoding
    /// with the interpreter keeps a ligature's full Unicode string attached to
    /// the glyph that actually drew it. Unmapped codes may append nothing.
    pub fn append_decoded_code(&self, code: u32, len: u8, raw_code: u32, out: &mut String) {
        if let Some(s) = self.lookup_to_unicode(code, raw_code, len) {
            out.push_str(s);
            return;
        }
        if matches!(self.font_type, PdfFontType::Type0CidType2) {
            if let Some(value) = self
                .cid_cmap
                .as_ref()
                .and_then(|cm| cm.decode_text(code, len))
            {
                value.push_to(out);
            }
        } else if let Ok(b) = u8::try_from(code) {
            if let Some(s) = self
                .simple_glyph_name(b)
                .and_then(glyph_list::glyph_name_to_string)
            {
                out.push_str(&s);
            } else if b >= 0x20 && b != 0x7f {
                out.push(b as char);
            }
        }
    }

    /// Append the Unicode text for one PDF character code represented by its
    /// packed big-endian source bytes `raw_code` and byte width `code_len`
    /// (1–4, or 0 when unspecified by a synthetic caller).
    ///
    /// For composite fonts with an encoding CMap, re-segments the exact source
    /// bytes through `cid_cmap.next_code` so UTF-16 surrogate pairs recover
    /// both their decoded scalar and packed raw bytes for `/ToUnicode` and
    /// CMap decoding.
    pub fn append_decoded_source_code(&self, raw_code: u32, code_len: u8, out: &mut String) {
        if matches!(self.font_type, PdfFontType::Type0CidType2) {
            let len = if (1..=4).contains(&code_len) {
                code_len
            } else {
                self.to_unicode
                    .as_ref()
                    .and_then(|map| map.source_length(raw_code))
                    .unwrap_or_else(|| ((4 - raw_code.leading_zeros() / 8) as u8).max(2))
            };
            let be = raw_code.to_be_bytes();
            let slice = &be[4 - usize::from(len)..];
            if let Some(cm) = &self.cid_cmap {
                let (code, seg_len) = cm.next_code(slice);
                self.append_decoded_code(code, seg_len as u8, raw_code, out);
            } else if let Some(map) = &self.to_unicode {
                if let Some(text) = map.lookup_with_length(raw_code, len) {
                    out.push_str(text);
                } else {
                    map.append_fixed_width(slice, out);
                }
            }
        } else if let Ok(byte) = u8::try_from(raw_code) {
            self.append_decoded_code(byte as u32, 1, byte as u32, out);
        }
    }

    /// Whether this code decodes to Unicode text through `/ToUnicode`, CMap,
    /// `/Encoding`, Type 1 built-in encoding, or non-symbolic single-byte
    /// fallback.
    #[doc(hidden)]
    pub fn can_decode_code(&self, code: u32, len: u8, raw_code: u32) -> bool {
        if self
            .lookup_to_unicode(code, raw_code, len)
            .is_some_and(|s| !s.is_empty())
        {
            return true;
        }
        if matches!(self.font_type, PdfFontType::Type0CidType2) {
            return self
                .cid_cmap
                .as_ref()
                .and_then(|cm| cm.decode_text(code, len))
                .is_some();
        }
        if let Ok(b) = u8::try_from(code) {
            if let Some(name) = self.simple_glyph_name(b) {
                return glyph_list::glyph_name_to_string(name).is_some();
            }
            return !self.symbolic && b >= 0x20 && b != 0x7f;
        }
        false
    }

    pub fn has_font_data(&self) -> bool {
        self.font_data.is_some() || self.is_type3() || self.type1.is_some()
    }

    pub fn is_type3(&self) -> bool {
        matches!(self.font_type, PdfFontType::Type3 { .. })
    }

    /// For Type3 fonts: get the content stream bytes for a given char code.
    pub fn type3_glyph_stream(&self, char_code: u16) -> Option<(&[u8], [f64; 6])> {
        match &self.font_type {
            PdfFontType::Type3 {
                font_matrix,
                char_procs,
                encoding,
                ..
            } => {
                // `encoding` is indexed by absolute character code (the
                // /Differences codes), unlike /Widths which is /FirstChar-based.
                let glyph_name = encoding.get(char_code as usize)?;
                if glyph_name == "g0" || glyph_name.is_empty() {
                    return None;
                }
                let stream = char_procs.get(glyph_name)?;
                Some((stream, *font_matrix))
            }
            _ => None,
        }
    }

    /// For Type3 fonts: get glyph width from the Widths array. Codes outside
    /// `/FirstChar` .. `/FirstChar + len` take the font's default width, on
    /// either side of the range (a code below `/FirstChar` must not borrow the
    /// first entry).
    pub fn type3_glyph_width(&self, char_code: u16) -> f64 {
        match &self.font_type {
            PdfFontType::Type3 {
                widths, first_char, ..
            } => char_code
                .checked_sub(*first_char)
                .and_then(|idx| widths.get(idx as usize).copied())
                .unwrap_or_else(|| self.cid_widths.default_width()),
            _ => 1000.0,
        }
    }
}

struct OutlineBuilder {
    commands: Vec<OutlineCommand>,
}

impl OutlineBuilder {
    fn new() -> Self {
        Self {
            commands: Vec::new(),
        }
    }
}

impl ttf_parser::OutlineBuilder for OutlineBuilder {
    fn move_to(&mut self, x: f32, y: f32) {
        self.commands
            .push(OutlineCommand::MoveTo(x as f64, y as f64));
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.commands
            .push(OutlineCommand::LineTo(x as f64, y as f64));
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.commands.push(OutlineCommand::QuadTo(
            x1 as f64, y1 as f64, x as f64, y as f64,
        ));
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.commands.push(OutlineCommand::CurveTo(
            x1 as f64, y1 as f64, x2 as f64, y2 as f64, x as f64, y as f64,
        ));
    }

    fn close(&mut self) {
        self.commands.push(OutlineCommand::Close);
    }
}

/// MacRoman character code for a glyph name (inverse of MAC_ROMAN_ENCODING).
fn mac_roman_code_for_name(name: &str) -> Option<u32> {
    encoding::MAC_ROMAN_ENCODING
        .iter()
        .position(|slot| *slot == Some(name))
        .map(|i| i as u32)
}

fn build_cff_cid_to_gid_map(otf_data: &[u8]) -> Option<HashMap<u16, u16>> {
    // Find the CFF table in the OTF container
    let cff_data = find_cff_table(otf_data)?;
    parse_cff_charset(cff_data)
}

/// CFF Top DICT operator 12 30 (/ROS) — present iff the font is CID-keyed.
const CFF_OP_ROS: u16 = 256 + 30;

/// Whether a CFF table is CID-keyed (its Top DICT carries a /ROS operator).
fn cff_is_cid_keyed(cff: &[u8]) -> bool {
    cff_top_dict(cff).is_some_and(|dict| parse_top_dict_int(dict, CFF_OP_ROS).is_some())
}

/// Slice of the first Top DICT in a CFF table.
fn cff_top_dict(cff: &[u8]) -> Option<&[u8]> {
    if cff.len() < 5 {
        return None;
    }
    let header_size = cff[2] as usize;
    // Skip the Name INDEX, then read the first Top DICT INDEX entry.
    let pos = skip_cff_index(cff, header_size)?;
    cff_index_entry(cff, pos, 0)
}

fn build_cff_encoding_map(otf_data: &[u8]) -> Option<HashMap<u16, u16>> {
    let cff_data = find_cff_table(otf_data)?;
    parse_cff_encoding(cff_data)
}

fn parse_cff_encoding(cff: &[u8]) -> Option<HashMap<u16, u16>> {
    if cff.len() < 5 {
        return None;
    }
    let header_size = cff[2] as usize;

    // Skip Name INDEX
    let mut pos = header_size;
    pos = skip_cff_index(cff, pos)?;

    // Top DICT INDEX
    let top_dict_index_start = pos;
    let dict_data = cff_index_entry(cff, top_dict_index_start, 0)?;

    // Get encoding offset from Top DICT (key 16)
    let encoding_offset = parse_top_dict_int(dict_data, 16).unwrap_or(0);

    if encoding_offset <= 1 {
        // 0 = Standard Encoding, 1 = Expert Encoding
        // For standard encoding, char code maps roughly to GlyphId
        // Build a simple identity-ish mapping
        let top_dict_end = skip_cff_index(cff, top_dict_index_start)?;
        let string_end = skip_cff_index(cff, top_dict_end)?;
        let _global_subr_end = skip_cff_index(cff, string_end)?;
        let charstrings_offset = parse_top_dict_charstrings(dict_data)?;
        let num_glyphs = count_cff_index_entries(cff, charstrings_offset)?;

        // For Standard Encoding, we need to map through the charset
        // charset maps GlyphId → SID, standard encoding maps charcode → SID
        let charset_offset = parse_top_dict_int(dict_data, 15).unwrap_or(0);
        if charset_offset <= 2 {
            // Predefined charset (0 ISOAdobe / 1 Expert / 2 ExpertSubset),
            // not an offset. ISOAdobe is identity for the first 228 glyphs.
            return None;
        }

        // Build GlyphId → SID from charset
        let mut gid_to_sid: HashMap<u16, u16> = HashMap::new();
        gid_to_sid.insert(0, 0);
        parse_charset_to_gid_sid(cff, charset_offset, num_glyphs, &mut gid_to_sid);

        // Build SID → GlyphId (inverse)
        let mut sid_to_gid: HashMap<u16, u16> = HashMap::new();
        for (&gid, &sid) in &gid_to_sid {
            sid_to_gid.insert(sid, gid);
        }

        // Standard encoding: charcode → SID
        let std_enc = standard_encoding();
        let mut map = HashMap::new();
        for (charcode, sid) in std_enc.iter().enumerate() {
            if *sid != 0 {
                if let Some(&gid) = sid_to_gid.get(sid) {
                    map.insert(charcode as u16, gid);
                }
            }
        }
        return if map.is_empty() { None } else { Some(map) };
    }

    // Custom encoding
    let enc = encoding_offset;
    if enc >= cff.len() {
        return None;
    }
    let format = cff[enc] & 0x7f;
    let mut map = HashMap::new();

    match format {
        0 => {
            // Format 0: nCodes, then array of codes
            if enc + 1 >= cff.len() {
                return None;
            }
            let n_codes = cff[enc + 1] as usize;
            for i in 0..n_codes {
                if enc + 2 + i >= cff.len() {
                    break;
                }
                let code = cff[enc + 2 + i] as u16;
                map.insert(code, (i + 1) as u16);
            }
        }
        1 => {
            // Format 1: nRanges, then [first, nLeft] pairs
            if enc + 1 >= cff.len() {
                return None;
            }
            let n_ranges = cff[enc + 1] as usize;
            let mut gid: u16 = 1;
            let mut p = enc + 2;
            for _ in 0..n_ranges {
                if p + 2 > cff.len() {
                    break;
                }
                let first = cff[p] as u16;
                let n_left = cff[p + 1] as u16;
                for j in 0..=n_left {
                    map.insert(first + j, gid);
                    gid += 1;
                }
                p += 2;
            }
        }
        _ => return None,
    }

    if map.is_empty() {
        None
    } else {
        Some(map)
    }
}

/// GIDs > 0 mapped to SID 0 (".notdef") by the charset — real glyphs that a
/// (Quartz) subsetter declined to name. Predefined charsets have none.
fn parse_cff_orphan_gids(cff: &[u8]) -> Option<Vec<u16>> {
    if cff.len() < 5 {
        return None;
    }
    let header_size = cff[2] as usize;

    // Skip Name INDEX, then read the first Top DICT.
    let pos = skip_cff_index(cff, header_size)?;
    let dict_data = cff_index_entry(cff, pos, 0)?;

    let charset_offset = parse_top_dict_int(dict_data, 15).unwrap_or(0);
    // 0/1/2 are predefined charsets (ISOAdobe/Expert/ExpertSubset), not
    // offsets — and predefined charsets never contain orphans.
    if charset_offset <= 2 {
        return None;
    }
    let charstrings_offset = parse_top_dict_charstrings(dict_data)?;
    let num_glyphs = count_cff_index_entries(cff, charstrings_offset)?;

    let mut gid_to_sid = HashMap::new();
    parse_charset_to_gid_sid(cff, charset_offset, num_glyphs, &mut gid_to_sid);

    let mut orphans: Vec<u16> = gid_to_sid
        .iter()
        .filter(|&(&gid, &sid)| gid != 0 && sid == 0)
        .map(|(&gid, _)| gid)
        .collect();
    orphans.sort_unstable();
    if orphans.is_empty() {
        None
    } else {
        Some(orphans)
    }
}

fn parse_charset_to_gid_sid(
    cff: &[u8],
    charset_offset: usize,
    num_glyphs: usize,
    gid_to_sid: &mut HashMap<u16, u16>,
) {
    if charset_offset >= cff.len() {
        return;
    }
    let format = cff[charset_offset];
    let mut p = charset_offset + 1;
    let mut gid: u16 = 1;

    match format {
        0 => {
            while gid < num_glyphs as u16 {
                if p + 2 > cff.len() {
                    break;
                }
                let sid = u16::from_be_bytes([cff[p], cff[p + 1]]);
                gid_to_sid.insert(gid, sid);
                p += 2;
                gid += 1;
            }
        }
        1 => {
            while gid < num_glyphs as u16 {
                if p + 3 > cff.len() {
                    break;
                }
                let first = u16::from_be_bytes([cff[p], cff[p + 1]]);
                let n_left = cff[p + 2] as u16;
                for j in 0..=n_left {
                    let Some(sid) = first.checked_add(j) else {
                        break;
                    };
                    gid_to_sid.insert(gid, sid);
                    gid += 1;
                    if gid >= num_glyphs as u16 {
                        break;
                    }
                }
                p += 3;
            }
        }
        2 => {
            while gid < num_glyphs as u16 {
                if p + 4 > cff.len() {
                    break;
                }
                let first = u16::from_be_bytes([cff[p], cff[p + 1]]);
                let n_left = u16::from_be_bytes([cff[p + 2], cff[p + 3]]);
                for j in 0..=n_left {
                    let Some(sid) = first.checked_add(j) else {
                        break;
                    };
                    gid_to_sid.insert(gid, sid);
                    gid += 1;
                    if gid >= num_glyphs as u16 {
                        break;
                    }
                }
                p += 4;
            }
        }
        _ => {}
    }
}

fn standard_encoding() -> [u16; 256] {
    let mut enc = [0u16; 256];
    // Standard Encoding per CFF spec Appendix B
    // charcode → SID for the 149 standard-encoded characters
    enc[32] = 1; // space
    enc[33] = 2; // exclam
    enc[34] = 3; // quotedbl
    enc[35] = 4; // numbersign
    enc[36] = 5; // dollar
    enc[37] = 6; // percent
    enc[38] = 7; // ampersand
    enc[39] = 8; // quoteright
    enc[40] = 9; // parenleft
    enc[41] = 10; // parenright
    enc[42] = 11; // asterisk
    enc[43] = 12; // plus
    enc[44] = 13; // comma
    enc[45] = 14; // hyphen
    enc[46] = 15; // period
    enc[47] = 16; // slash
    for (offset, slot) in enc[48..=57].iter_mut().enumerate() {
        *slot = (offset + 17) as u16;
    } // 0-9
    enc[58] = 27; // colon
    enc[59] = 28; // semicolon
    enc[60] = 29; // less
    enc[61] = 30; // equal
    enc[62] = 31; // greater
    enc[63] = 32; // question
    enc[64] = 33; // at
    for (offset, slot) in enc[65..=90].iter_mut().enumerate() {
        *slot = (offset + 34) as u16;
    } // A-Z
    enc[91] = 60; // bracketleft
    enc[92] = 61; // backslash
    enc[93] = 62; // bracketright
    enc[94] = 63; // asciicircum
    enc[95] = 64; // underscore
    enc[96] = 65; // quoteleft
    for (offset, slot) in enc[97..=122].iter_mut().enumerate() {
        *slot = (offset + 66) as u16;
    } // a-z
    enc[123] = 92; // braceleft
    enc[124] = 93; // bar
    enc[125] = 94; // braceright
    enc[126] = 95; // asciitilde
    enc[161] = 96; // exclamdown
    enc[162] = 97; // cent
    enc[163] = 98; // sterling
    enc[164] = 99; // fraction
    enc[165] = 100; // yen
    enc[166] = 101; // florin
    enc[167] = 102; // section
    enc[168] = 103; // currency
    enc[169] = 104; // quotesingle
    enc[170] = 105; // quotedblleft
    enc[171] = 106; // guillemotleft
    enc[172] = 107; // guilsinglleft
    enc[173] = 108; // guilsinglright
    enc[174] = 109; // fi
    enc[175] = 110; // fl
    enc[177] = 111; // endash
    enc[178] = 112; // dagger
    enc[179] = 113; // daggerdbl
    enc[180] = 114; // periodcentered
    enc[182] = 115; // paragraph
    enc[183] = 116; // bullet
    enc[184] = 117; // quotesinglbase
    enc[185] = 118; // quotedblbase
    enc[186] = 119; // quotedblright
    enc[187] = 120; // guillemotright
    enc[188] = 121; // ellipsis
    enc[189] = 122; // perthousand
    enc[191] = 123; // questiondown
    enc[193] = 124; // grave
    enc[194] = 125; // acute
    enc[195] = 126; // circumflex
    enc[196] = 127; // tilde
    enc[197] = 128; // macron
    enc[198] = 129; // breve
    enc[199] = 130; // dotaccent
    enc[200] = 131; // dieresis
    enc[202] = 132; // ring
    enc[203] = 133; // cedilla
    enc[205] = 134; // hungarumlaut
    enc[206] = 135; // ogonek
    enc[207] = 136; // caron
    enc[208] = 137; // emdash
    enc[225] = 138; // AE
    enc[227] = 139; // ordfeminine
    enc[232] = 140; // Lslash
    enc[233] = 141; // Oslash
    enc[234] = 142; // OE
    enc[235] = 143; // ordmasculine
    enc[241] = 144; // ae
    enc[245] = 145; // dotlessi
    enc[248] = 146; // lslash
    enc[249] = 147; // oslash
    enc[250] = 148; // oe
    enc[251] = 149; // germandbls
    enc
}

fn find_cff_table(data: &[u8]) -> Option<&[u8]> {
    if data.len() < 12 {
        return None;
    }
    let num_tables = u16::from_be_bytes([data[4], data[5]]) as usize;
    for i in 0..num_tables {
        let rec = 12 + i * 16;
        if rec + 16 > data.len() {
            break;
        }
        if &data[rec..rec + 4] == b"CFF " {
            let offset =
                u32::from_be_bytes([data[rec + 8], data[rec + 9], data[rec + 10], data[rec + 11]])
                    as usize;
            let length = u32::from_be_bytes([
                data[rec + 12],
                data[rec + 13],
                data[rec + 14],
                data[rec + 15],
            ]) as usize;
            if let Some(table) = offset
                .checked_add(length)
                .and_then(|end| data.get(offset..end))
            {
                return Some(table);
            }
        }
    }
    None
}

fn parse_cff_charset(cff: &[u8]) -> Option<HashMap<u16, u16>> {
    if cff.len() < 5 {
        return None;
    }
    let header_size = cff[2] as usize;

    // Skip Name INDEX
    let mut pos = header_size;
    pos = skip_cff_index(cff, pos)?;

    // Top DICT INDEX — we need to read it to find charset offset
    let top_dict_index_end = skip_cff_index(cff, pos)?;

    // Parse the first Top DICT entry through the bounds-checked INDEX helper.
    let dict_data = cff_index_entry(cff, pos, 0)?;

    // Parse Top DICT to find charset offset (key 15)
    let charset_offset = parse_top_dict_charset(dict_data)?;

    // Skip String INDEX
    pos = top_dict_index_end;
    pos = skip_cff_index(cff, pos)?;

    // Skip Global Subr INDEX
    let _global_subr_end = skip_cff_index(cff, pos)?;

    // Now count charstrings to know how many glyphs
    // Find CharStrings INDEX — its offset is in Top DICT (key 17)
    let charstrings_offset = parse_top_dict_charstrings(dict_data)?;
    let num_glyphs = count_cff_index_entries(cff, charstrings_offset)?;

    // Parse charset at the given offset
    let mut map = HashMap::new();
    map.insert(0u16, 0u16); // .notdef always at GID 0

    if charset_offset == 0 {
        // Predefined charset — identity mapping
        for gid in 0..num_glyphs as u16 {
            map.insert(gid, gid);
        }
        return Some(map);
    }
    if charset_offset <= 2 {
        // Predefined Expert/ExpertSubset charsets, not offsets — no mapping.
        return None;
    }

    let cs = charset_offset;
    if cs >= cff.len() {
        return None;
    }

    let format = cff[cs];
    let mut p = cs + 1;
    let mut gid: u16 = 1; // GID 0 is .notdef

    match format {
        0 => {
            // Format 0: array of SID/CIDs
            while gid < num_glyphs as u16 {
                if p + 2 > cff.len() {
                    break;
                }
                let sid = u16::from_be_bytes([cff[p], cff[p + 1]]);
                map.insert(sid, gid);
                p += 2;
                gid += 1;
            }
        }
        1 => {
            // Format 1: ranges [first_sid, n_left (u8)]
            while gid < num_glyphs as u16 {
                if p + 3 > cff.len() {
                    break;
                }
                let first = u16::from_be_bytes([cff[p], cff[p + 1]]);
                let n_left = cff[p + 2] as u16;
                for j in 0..=n_left {
                    let Some(sid) = first.checked_add(j) else {
                        break;
                    };
                    map.insert(sid, gid);
                    gid += 1;
                    if gid >= num_glyphs as u16 {
                        break;
                    }
                }
                p += 3;
            }
        }
        2 => {
            // Format 2: ranges [first_sid, n_left (u16)]
            while gid < num_glyphs as u16 {
                if p + 4 > cff.len() {
                    break;
                }
                let first = u16::from_be_bytes([cff[p], cff[p + 1]]);
                let n_left = u16::from_be_bytes([cff[p + 2], cff[p + 3]]);
                for j in 0..=n_left {
                    let Some(sid) = first.checked_add(j) else {
                        break;
                    };
                    map.insert(sid, gid);
                    gid += 1;
                    if gid >= num_glyphs as u16 {
                        break;
                    }
                }
                p += 4;
            }
        }
        _ => return None,
    }

    Some(map)
}

fn skip_cff_index(cff: &[u8], pos: usize) -> Option<usize> {
    let count_bytes = cff.get(pos..pos.checked_add(2)?)?;
    let count = u16::from_be_bytes([count_bytes[0], count_bytes[1]]) as usize;
    if count == 0 {
        return pos.checked_add(2);
    }
    let off_size = *cff.get(pos.checked_add(2)?)? as usize;
    if !(1..=4).contains(&off_size) {
        return None;
    }
    let offsets_start = pos.checked_add(3)?;
    let last_offset_pos = offsets_start.checked_add(count.checked_mul(off_size)?)?;
    let last_offset = read_cff_offset(cff, last_offset_pos, off_size)?;
    let last_offset = last_offset.checked_sub(1)?;
    let data_start = offsets_start.checked_add((count + 1).checked_mul(off_size)?)?;
    let end = data_start.checked_add(last_offset)?;
    (end <= cff.len()).then_some(end)
}

fn read_cff_offset(cff: &[u8], pos: usize, size: usize) -> Option<usize> {
    if !(1..=4).contains(&size) {
        return None;
    }
    let bytes = cff.get(pos..pos.checked_add(size)?)?;
    let mut val = 0usize;
    for &byte in bytes {
        val = (val << 8) | byte as usize;
    }
    Some(val)
}

fn cff_index_entry(cff: &[u8], pos: usize, entry: usize) -> Option<&[u8]> {
    let count_bytes = cff.get(pos..pos.checked_add(2)?)?;
    let count = u16::from_be_bytes([count_bytes[0], count_bytes[1]]) as usize;
    if entry >= count {
        return None;
    }
    let off_size = *cff.get(pos.checked_add(2)?)? as usize;
    if !(1..=4).contains(&off_size) {
        return None;
    }
    let offsets_start = pos.checked_add(3)?;
    let start_pos = offsets_start.checked_add(entry.checked_mul(off_size)?)?;
    let end_pos = start_pos.checked_add(off_size)?;
    let start = read_cff_offset(cff, start_pos, off_size)?.checked_sub(1)?;
    let end = read_cff_offset(cff, end_pos, off_size)?.checked_sub(1)?;
    if end < start {
        return None;
    }
    let data_start = offsets_start.checked_add((count + 1).checked_mul(off_size)?)?;
    let range_start = data_start.checked_add(start)?;
    let range_end = data_start.checked_add(end)?;
    cff.get(range_start..range_end)
}

fn count_cff_index_entries(cff: &[u8], pos: usize) -> Option<usize> {
    let bytes = cff.get(pos..pos.checked_add(2)?)?;
    Some(u16::from_be_bytes([bytes[0], bytes[1]]) as usize)
}

fn parse_top_dict_charset(dict_data: &[u8]) -> Option<usize> {
    parse_top_dict_int(dict_data, 15)
}

fn parse_top_dict_charstrings(dict_data: &[u8]) -> Option<usize> {
    parse_top_dict_int(dict_data, 17)
}

/// Last integer operand of `target_key` in a CFF Top DICT. Two-byte operators
/// (12 x) are addressed as `256 + x` (e.g. [`CFF_OP_ROS`]).
fn parse_top_dict_int(dict_data: &[u8], target_key: u16) -> Option<usize> {
    let mut pos = 0;
    let mut operand_stack: Vec<i64> = Vec::new();

    while pos < dict_data.len() {
        let b0 = dict_data[pos];
        match b0 {
            // Operators
            0..=21 => {
                let key = if b0 == 12 {
                    pos += 1;
                    if pos >= dict_data.len() {
                        break;
                    }
                    256 + dict_data[pos] as u16
                } else {
                    b0 as u16
                };
                if key == target_key {
                    return operand_stack
                        .last()
                        .and_then(|&value| usize::try_from(value).ok());
                }
                operand_stack.clear();
                pos += 1;
            }
            // Integer operands
            28 => {
                if pos + 2 >= dict_data.len() {
                    break;
                }
                let val = i16::from_be_bytes([dict_data[pos + 1], dict_data[pos + 2]]) as i64;
                operand_stack.push(val);
                pos += 3;
            }
            29 => {
                if pos + 4 >= dict_data.len() {
                    break;
                }
                let val = i32::from_be_bytes([
                    dict_data[pos + 1],
                    dict_data[pos + 2],
                    dict_data[pos + 3],
                    dict_data[pos + 4],
                ]) as i64;
                operand_stack.push(val);
                pos += 5;
            }
            30 => {
                // Real number — skip nibbles until 0xf
                pos += 1;
                while pos < dict_data.len() {
                    let byte = dict_data[pos];
                    pos += 1;
                    if (byte & 0x0f) == 0x0f || (byte >> 4) == 0x0f {
                        break;
                    }
                }
                operand_stack.push(0); // placeholder
            }
            32..=246 => {
                operand_stack.push(b0 as i64 - 139);
                pos += 1;
            }
            247..=250 => {
                if pos + 1 >= dict_data.len() {
                    break;
                }
                let val = (b0 as i64 - 247) * 256 + dict_data[pos + 1] as i64 + 108;
                operand_stack.push(val);
                pos += 2;
            }
            251..=254 => {
                if pos + 1 >= dict_data.len() {
                    break;
                }
                let val = -(b0 as i64 - 251) * 256 - dict_data[pos + 1] as i64 - 108;
                operand_stack.push(val);
                pos += 2;
            }
            _ => {
                pos += 1;
            }
        }
        if operand_stack.len() > 48 {
            return None;
        }
    }
    None
}

fn is_raw_cff(data: &[u8]) -> bool {
    data.len() >= 4 && data[0] == 0x01 && data[1] == 0x00 && data[2] >= 1 && data[2] <= 8
}

fn wrap_cff_in_otf(cff_data: &[u8]) -> Vec<u8> {
    let num_tables: u16 = 5;
    let search_range: u16 = 64;
    let entry_selector: u16 = 2;
    let range_shift: u16 = num_tables * 16 - search_range;

    let header_size = 12 + num_tables as usize * 16;

    fn pad4(n: u32) -> Option<u32> {
        Some(n.checked_add(3)? & !3)
    }
    fn compute_checksum(data: &[u8]) -> u32 {
        let mut sum: u32 = 0;
        let chunks = data.len() / 4;
        for i in 0..chunks {
            let val = u32::from_be_bytes([
                data[i * 4],
                data[i * 4 + 1],
                data[i * 4 + 2],
                data[i * 4 + 3],
            ]);
            sum = sum.wrapping_add(val);
        }
        let remainder = data.len() % 4;
        if remainder > 0 {
            let mut last = [0u8; 4];
            last[..remainder].copy_from_slice(&data[chunks * 4..]);
            sum = sum.wrapping_add(u32::from_be_bytes(last));
        }
        sum
    }

    let cff_offset = header_size as u32;
    let Ok(cff_len) = u32::try_from(cff_data.len()) else {
        return Vec::new();
    };

    let Some(cff_padded) = pad4(cff_len) else {
        return Vec::new();
    };
    let Some(head_offset) = cff_offset.checked_add(cff_padded) else {
        return Vec::new();
    };
    let head_len: u32 = 54;

    let Some(head_padded) = pad4(head_len) else {
        return Vec::new();
    };
    let Some(hhea_offset) = head_offset.checked_add(head_padded) else {
        return Vec::new();
    };
    let hhea_len: u32 = 36;

    let Some(hhea_padded) = pad4(hhea_len) else {
        return Vec::new();
    };
    let Some(maxp_offset) = hhea_offset.checked_add(hhea_padded) else {
        return Vec::new();
    };
    let maxp_len: u32 = 6;

    let Some(maxp_padded) = pad4(maxp_len) else {
        return Vec::new();
    };
    let Some(post_offset) = maxp_offset.checked_add(maxp_padded) else {
        return Vec::new();
    };
    let post_len: u32 = 32;

    let Some(total_size) = (post_offset as usize).checked_add(post_len as usize) else {
        return Vec::new();
    };
    let mut buf = Vec::new();
    if buf.try_reserve_exact(total_size).is_err() {
        return Vec::new();
    }
    buf.resize(total_size, 0);

    // OTF header
    buf[0..4].copy_from_slice(b"OTTO");
    buf[4..6].copy_from_slice(&num_tables.to_be_bytes());
    buf[6..8].copy_from_slice(&search_range.to_be_bytes());
    buf[8..10].copy_from_slice(&entry_selector.to_be_bytes());
    buf[10..12].copy_from_slice(&range_shift.to_be_bytes());

    // Build table data first so we can compute checksums

    // head table
    let mut head_data = vec![0u8; head_len as usize];
    head_data[0..4].copy_from_slice(&0x00010000u32.to_be_bytes()); // version
    head_data[12..16].copy_from_slice(&0x5F0F3CF5u32.to_be_bytes()); // magicNumber
    head_data[16..18].copy_from_slice(&0x000Bu16.to_be_bytes()); // flags
    head_data[18..20].copy_from_slice(&1000u16.to_be_bytes()); // unitsPerEm
    head_data[50..52].copy_from_slice(&0u16.to_be_bytes()); // indexToLocFormat

    // hhea table
    let mut hhea_data = vec![0u8; hhea_len as usize];
    hhea_data[0..4].copy_from_slice(&0x00010000u32.to_be_bytes()); // version
    hhea_data[4..6].copy_from_slice(&800i16.to_be_bytes()); // ascender
    hhea_data[6..8].copy_from_slice(&(-200i16).to_be_bytes()); // descender
    hhea_data[34..36].copy_from_slice(&65535u16.to_be_bytes()); // numberOfHMetrics

    // maxp table
    let mut maxp_data = vec![0u8; maxp_len as usize];
    maxp_data[0..4].copy_from_slice(&0x00005000u32.to_be_bytes()); // version 0.5
    maxp_data[4..6].copy_from_slice(&65535u16.to_be_bytes()); // numGlyphs

    // post table (format 3.0 — no glyph names)
    let mut post_data = vec![0u8; post_len as usize];
    post_data[0..4].copy_from_slice(&0x00030000u32.to_be_bytes()); // format 3.0

    // Table records (sorted alphabetically: CFF, head, hhea, maxp, post)
    let mut rec_off = 12;
    for (tag, toff, tlen, tdata) in [
        (b"CFF ", cff_offset, cff_len, cff_data as &[u8]),
        (b"head", head_offset, head_len, &head_data as &[u8]),
        (b"hhea", hhea_offset, hhea_len, &hhea_data),
        (b"maxp", maxp_offset, maxp_len, &maxp_data),
        (b"post", post_offset, post_len, &post_data),
    ] {
        buf[rec_off..rec_off + 4].copy_from_slice(tag);
        let cs = compute_checksum(tdata);
        buf[rec_off + 4..rec_off + 8].copy_from_slice(&cs.to_be_bytes());
        buf[rec_off + 8..rec_off + 12].copy_from_slice(&toff.to_be_bytes());
        buf[rec_off + 12..rec_off + 16].copy_from_slice(&tlen.to_be_bytes());
        rec_off += 16;
    }

    // Write table data
    buf[cff_offset as usize..cff_offset as usize + cff_data.len()].copy_from_slice(cff_data);
    buf[head_offset as usize..head_offset as usize + head_data.len()].copy_from_slice(&head_data);
    buf[hhea_offset as usize..hhea_offset as usize + hhea_data.len()].copy_from_slice(&hhea_data);
    buf[maxp_offset as usize..maxp_offset as usize + maxp_data.len()].copy_from_slice(&maxp_data);
    buf[post_offset as usize..post_offset as usize + post_data.len()].copy_from_slice(&post_data);

    buf
}

/// Shared admission accounting for the page and document font stores.
/// Owners must retain admitted fonts until this accounting is dropped or
/// rebuilt; removing fonts without rebuilding would leave stale identities.
#[doc(hidden)]
#[derive(Default)]
pub struct FontCacheBudget {
    bytes: u64,
    fonts: HashSet<usize>,
    programs: HashSet<usize>,
    specs: HashSet<usize>,
}

impl FontCacheBudget {
    fn admission_total(&self, font: &Arc<LoadedFont>, max_bytes: u64) -> Option<u64> {
        let mut added = 0u64;
        if !self.fonts.contains(&(Arc::as_ptr(font) as usize)) {
            added = font.estimated_owned_cache_bytes();
            let mut seen = HashSet::new();
            for data in font.shared_programs() {
                let key = data.as_ptr() as usize;
                if !self.programs.contains(&key) && seen.insert(key) {
                    added = added.checked_add(data.len() as u64)?;
                }
            }
        }
        self.bytes
            .checked_add(added)
            .filter(|&total| total <= max_bytes)
    }

    fn record(&mut self, font: &Arc<LoadedFont>, total: u64) {
        self.fonts.insert(Arc::as_ptr(font) as usize);
        self.programs
            .extend(font.shared_programs().map(|data| data.as_ptr() as usize));
        self.bytes = total;
    }

    pub fn try_admit(&mut self, font: &Arc<LoadedFont>, max_bytes: u64) -> bool {
        let Some(total) = self.admission_total(font, max_bytes) else {
            return false;
        };
        self.record(font, total);
        true
    }

    /// Atomically admit `fonts` together with optional descriptor metadata
    /// `spec` only when the combined incremental bytes fit within `max_bytes`.
    /// Leaves the budget untouched if any component would exceed the limit.
    pub fn try_admit_with_spec(
        &mut self,
        fonts: &[&Arc<LoadedFont>],
        spec: Option<&Arc<DescriptorSubstituteSpec>>,
        max_bytes: u64,
    ) -> bool {
        let mut added = 0u64;
        let mut seen_fonts = HashSet::new();
        let mut seen_programs = HashSet::new();
        for &font in fonts {
            let font_key = Arc::as_ptr(font) as usize;
            if !self.fonts.contains(&font_key) && seen_fonts.insert(font_key) {
                let Some(next) = added.checked_add(font.estimated_owned_cache_bytes()) else {
                    return false;
                };
                added = next;
                for data in font.shared_programs() {
                    let key = data.as_ptr() as usize;
                    if !self.programs.contains(&key) && seen_programs.insert(key) {
                        let Some(next) = added.checked_add(data.len() as u64) else {
                            return false;
                        };
                        added = next;
                    }
                }
            }
        }
        let mut spec_key = None;
        if let Some(spec) = spec {
            let extra = spec.extra_bytes();
            if extra > 0 {
                let key = Arc::as_ptr(spec) as usize;
                if !self.specs.contains(&key) {
                    let Some(next) = added.checked_add(extra) else {
                        return false;
                    };
                    added = next;
                    spec_key = Some(key);
                }
            }
        }
        let Some(total) = self.bytes.checked_add(added).filter(|&t| t <= max_bytes) else {
            return false;
        };
        for &font in fonts {
            self.record(font, total);
        }
        if let Some(key) = spec_key {
            self.specs.insert(key);
        }
        self.bytes = total;
        true
    }

    /// Atomically admit two fonts together when both fit within `max_bytes`,
    /// leaving the budget untouched if the pair would exceed the limit.
    pub fn try_admit_pair(
        &mut self,
        first: &Arc<LoadedFont>,
        second: &Arc<LoadedFont>,
        max_bytes: u64,
    ) -> bool {
        self.try_admit_with_spec(&[first, second], None, max_bytes)
    }

    pub fn bytes_used(&self) -> u64 {
        self.bytes
    }
}

/// Per-font `cmap` scan budget shared across bounded blank-record probes.
#[doc(hidden)]
pub const CMAP_PROBE_BUDGET: usize = resolution::CMAP_PROBE_BUDGET;

/// Classification outcome for one shown glyph in an embedded font program.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddedGlyphHealthOutcome {
    /// Confirmed readable (drawn outline, zero-length `loca` blank handled by
    /// CAD repair, or confirmed non-visible blank). Safe to cache by GID.
    Readable,
    /// Physically unreadable outline record (missing, malformed header, failed
    /// or empty visible Type 1 charstring, or out-of-range GID). Safe to cache
    /// by GID.
    Unreadable,
    /// Physically blank record whose readability depends on the shown PDF code
    /// (Type 1 blank with `/ToUnicode` or TrueType valid-header nonempty
    /// zero-contour record resolved via direct PDF evidence). Must not be
    /// cached as readable or unreadable by GID alone.
    CodeDependent { unreadable: bool },
    /// Nonempty zero-contour blank record whose reverse `cmap` probe confirmed
    /// whether the font's own `cmap` maps a visible scalar (`unreadable: true`)
    /// or only non-visible/absent scalars (`unreadable: false`) to this GID.
    /// Cached only as a fallback for shown codes without direct PDF evidence,
    /// so another source code sharing the same GID with an explicit `/ToUnicode`
    /// mapping (either visible text or whitespace) still wins.
    CodeDependentFallback { unreadable: bool },
    /// Reverse `cmap` probe exhausted its work budget before finding a mapping.
    /// Distinct from confirmed `Readable` and must not be cached.
    Exhausted,
}

/// Result of probing an embedded font program's glyph outlines at load time.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddedOutlineProbe {
    /// No sampled glyph failed to outline.
    Healthy,
    /// Majority of sampled glyphs drew cleanly, but at least one sampled glyph
    /// was unreadable; keep the embedded face as primary and allow per-glyph
    /// descriptor substitution for unreadable shown glyphs.
    PartiallyCorrupt,
    /// Program failed to parse or majority of sampled glyphs were unreadable;
    /// switch primary face to a substitute and retain the embedded face as a
    /// companion for surviving glyphs.
    Unusable,
}

/// Descriptor metadata retained for an embedded primary font so a
/// descriptor-based substitute can be resolved lazily on demand — including
/// when interpreting from a standalone [`FontCache`] without a `PdfFile`.
#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct DescriptorSubstituteSpec {
    /// Override font name from `/FontDescriptor /FontName` only when it differs
    /// from `primary.base_font` (e.g. when `/BaseFont` was absent). Shared via
    /// `Arc<DescriptorSubstituteSpec>` across aliases and pages and charged
    /// once in [`FontCacheBudget`].
    pub lookup_name: Option<Arc<str>>,
    pub hints: system::SubstituteHints,
    /// Recognized CJK CID ordering (`"GB1"`, `"CNS1"`, `"Japan1"`, `"Korea1"`,
    /// `"KR"`), never retaining arbitrary unbounded PDF strings.
    pub ordering: Option<&'static str>,
    /// Object ID of the embedded `/FontFile*` stream for lazy fallback
    /// diagnostics when an unreadable shown glyph triggers fallback.
    pub font_file_id: Option<zpdf_core::ObjectId>,
}

impl DescriptorSubstituteSpec {
    pub(crate) fn extra_bytes(&self) -> u64 {
        self.lookup_name
            .as_ref()
            .map_or(0, |name| name.len() as u64)
    }

    /// Copy without the optional `lookup_name` override so zero-byte descriptor
    /// metadata (hints, ordering, stream object ID) can still be retained when
    /// an oversized `/FontDescriptor /FontName` does not fit within budget.
    pub fn without_lookup_name(&self) -> Self {
        Self {
            lookup_name: None,
            hints: self.hints,
            ordering: self.ordering,
            font_file_id: self.font_file_id,
        }
    }
}

/// Direction of a paired fallback companion in [`FontCache`].
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompanionKind {
    /// Primary is a substitute face; companion is the partially readable
    /// embedded original used to draw surviving embedded glyphs.
    EmbeddedOriginal,
    /// Primary is an embedded face; companion is a descriptor-based substitute
    /// used only when a shown embedded glyph's outline record proves
    /// unreadable.
    DescriptorSubstitute {
        /// Object ID of the embedded `/FontFile*` stream for lazy fallback
        /// diagnostics when an unreadable shown glyph triggers fallback.
        font_file_id: Option<zpdf_core::ObjectId>,
    },
}

/// Maximum distinct missed `(code, raw_code, length)` tuples remembered per
/// logical font on a page so repeated shows of an uncovered code do not
/// repeatedly reload and re-parse a deferred companion.
pub(crate) const MAX_COMPANION_MISS_CACHE: usize = 512;

/// Reload source for a fallback companion paired with a [`FontId`] in
/// [`FontCache`]. Stores only lightweight references (never cloned PDF font
/// dictionaries or uncharged font streams).
#[doc(hidden)]
#[derive(Clone)]
pub enum CompanionSource {
    /// Indirect font dictionary object ID in the source PDF.
    Object(zpdf_core::ObjectId),
    /// Inline font dictionary inside an indirect Form XObject's `/Resources /Font`.
    FormInlineFont {
        form_id: zpdf_core::ObjectId,
        font_name: String,
    },
    /// No external reload source (e.g. direct `FontCache` caller or inline Form).
    None,
}

/// Outcome of attempting to load a deferred companion on demand.
#[doc(hidden)]
pub enum DeferredCompanionLoad {
    /// Companion loaded cleanly and can be probed or admitted.
    Loaded(Arc<LoadedFont>),
    /// No loader/document was available for this call (transient; do not mark
    /// the deferred source as failed).
    Unavailable,
    /// Definitive load or budget failure at the supplied remaining byte budget
    /// (e.g. stream exceeded remaining budget or failed to parse). Because
    /// [`FontCacheBudget`] is non-evicting, retrying at the same or smaller
    /// remaining budget cannot succeed.
    FailedForBudget,
}

pub(crate) struct CompanionEntry {
    pub(crate) kind: CompanionKind,
    pub(crate) source: CompanionSource,
    /// Lightweight descriptor metadata for lazy descriptor-based substitution
    /// of unreadable embedded glyphs without requiring a `PdfFile`.
    pub(crate) descriptor_spec: Option<Arc<DescriptorSubstituteSpec>>,
    /// Optional in-memory standby copy of the companion face, strictly
    /// charged in `FontCache::standby_bytes` so `bytes_used() <= max_bytes`
    /// always holds, and reclaimed whenever a primary font, Form font, or
    /// drawn companion needs the space.
    pub(crate) standby: Option<Arc<LoadedFont>>,
    /// Permanent drawing `FontId` admitted into `self.fonts` once a glyph
    /// actually draws from this companion.
    pub(crate) admitted: Option<FontId>,
    /// Highest remaining page-budget allowance for which loading or admitting
    /// this deferred companion already failed definitively for
    /// `failed_program_count`.
    pub(crate) failed_budget: Option<u64>,
    /// Number of unique shared programs in `self.budget.programs` when
    /// `failed_budget` was recorded; if another font later admits a shared
    /// program, a companion sharing it may now fit in incremental bytes.
    pub(crate) failed_program_count: usize,
    /// `FontProvider::generation()` observed for `descriptor_spec` caches so
    /// newly registered faces clear stale failure/miss entries.
    pub(crate) provider_generation: Option<u64>,
    /// Bounded set of `(code, raw_code, length)` misses observed on this page
    /// so repeated shows of a code neither face draws do not re-decode the
    /// deferred companion, while leaving other codes free to load it.
    pub(crate) missed_codes: HashSet<(u32, u32, u8)>,
}

impl CompanionEntry {
    /// Record a definitive budget failure for `program_count`. When the shared
    /// program set changes, replace any stale watermark from the old count so
    /// a lower incremental requirement can still succeed later; within the same
    /// count retain the highest failed remaining allowance.
    pub(crate) fn record_budget_failure(&mut self, remaining: u64, program_count: usize) {
        if self.failed_program_count != program_count {
            self.failed_budget = Some(remaining);
            self.failed_program_count = program_count;
        } else {
            self.failed_budget = Some(
                self.failed_budget
                    .map_or(remaining, |prev| prev.max(remaining)),
            );
        }
    }
}

/// Page/display-list font store keyed by [`FontId`].
///
/// Entries are intentionally stable for the lifetime of the store: display
/// lists retain only a `FontId`, so evicting a font would invalidate commands
/// that have already been emitted. The capacity argument is therefore an
/// allocation hint, not an eviction limit.
pub struct FontCache {
    fonts: HashMap<FontId, Arc<LoadedFont>>,
    name_to_id: HashMap<String, FontId>,
    next_id: FontId,
    budget: FontCacheBudget,
    /// Bytes currently charged to reclaimable unadmitted `standby` companions
    /// and optional descriptor lookup-name overrides. Included in
    /// [`Self::bytes_used`], and reclaimed before rejecting any primary, Form,
    /// or drawn-companion admission.
    standby_bytes: u64,
    /// Unique `Arc<DescriptorSubstituteSpec>` pointers currently charged in
    /// `standby_bytes`, allowing O(1) accounting across shared resource aliases.
    standby_specs: HashSet<usize>,
    /// Whether any companion entry currently retains an in-memory `standby`
    /// face (whose shared programs might deduplicate with later primary loads).
    has_standby_companions: bool,
    provider: Option<Arc<FontProvider>>,
    resolution: Option<Box<resolution::ResolutionCache>>,
    /// Per-font diagnostics for embedded font programs that failed to decode or
    /// parse and fell back to substitution or metrics-only mode. Empty maps
    /// allocate nothing, so normal pages pay zero overhead.
    embedded_fallbacks: HashMap<FontId, zpdf_core::ProcessingDiagnostics>,
    /// Partially-readable embedded originals paired with a substituted
    /// `FontId`. Empty maps allocate nothing.
    embedded_companions: HashMap<FontId, CompanionEntry>,
    /// Maps each admitted drawing-only companion/fallback `FontId` back to the
    /// logical PDF `FontId` whose `/ToUnicode`, `/Encoding`, `cid_cmap`, and
    /// `font_type` govern text decoding (e.g. for PPTX export). Empty maps
    /// allocate nothing.
    drawing_font_originals: HashMap<FontId, FontId>,
}

impl FontCache {
    /// Create a new font store preallocated for 256 fonts.
    pub fn new() -> Self {
        Self::with_preallocated_capacity(256)
    }

    /// Create a new font store with a specific allocation hint.
    pub fn with_preallocated_capacity(capacity: usize) -> Self {
        Self {
            fonts: HashMap::with_capacity(capacity),
            name_to_id: HashMap::with_capacity(capacity),
            next_id: 0,
            budget: FontCacheBudget::default(),
            standby_bytes: 0,
            standby_specs: HashSet::new(),
            has_standby_companions: false,
            provider: None,
            resolution: None,
            embedded_fallbacks: HashMap::new(),
            embedded_companions: HashMap::new(),
            drawing_font_originals: HashMap::new(),
        }
    }

    fn standby_extra_bytes(
        &self,
        candidate_standby: Option<&Arc<LoadedFont>>,
        candidate_spec: Option<&Arc<DescriptorSubstituteSpec>>,
    ) -> Option<u64> {
        if self.embedded_companions.is_empty()
            && candidate_standby.is_none()
            && candidate_spec.is_none()
        {
            return Some(0);
        }
        let mut added = 0u64;
        let mut seen_fonts = HashSet::new();
        let mut seen_programs = HashSet::new();
        let standby_iter = self
            .embedded_companions
            .values()
            .filter_map(|entry| entry.standby.as_ref());
        for font in standby_iter.chain(candidate_standby) {
            let font_key = Arc::as_ptr(font) as usize;
            if !self.budget.fonts.contains(&font_key) && seen_fonts.insert(font_key) {
                added = added.checked_add(font.estimated_owned_cache_bytes())?;
                for data in font.shared_programs() {
                    let key = data.as_ptr() as usize;
                    if !self.budget.programs.contains(&key) && seen_programs.insert(key) {
                        added = added.checked_add(data.len() as u64)?;
                    }
                }
            }
        }
        let mut seen_specs = HashSet::new();
        let spec_iter = self.embedded_companions.values().filter_map(|entry| {
            entry
                .admitted
                .is_none()
                .then_some(entry.descriptor_spec.as_ref())
                .flatten()
        });
        for spec in spec_iter.chain(candidate_spec) {
            let extra = spec.extra_bytes();
            if extra > 0 && seen_specs.insert(Arc::as_ptr(spec) as usize) {
                added = added.checked_add(extra)?;
            }
        }
        Some(added)
    }

    pub(crate) fn recompute_standby_bytes(&mut self) {
        self.standby_specs.clear();
        self.has_standby_companions = false;
        if self.embedded_companions.is_empty() {
            self.standby_bytes = 0;
            return;
        }
        let mut added = 0u64;
        let mut seen_fonts = HashSet::new();
        let mut seen_programs = HashSet::new();
        for entry in self.embedded_companions.values() {
            if let Some(font) = entry.standby.as_ref() {
                self.has_standby_companions = true;
                let font_key = Arc::as_ptr(font) as usize;
                if !self.budget.fonts.contains(&font_key) && seen_fonts.insert(font_key) {
                    added = added.saturating_add(font.estimated_owned_cache_bytes());
                    for data in font.shared_programs() {
                        let key = data.as_ptr() as usize;
                        if !self.budget.programs.contains(&key) && seen_programs.insert(key) {
                            added = added.saturating_add(data.len() as u64);
                        }
                    }
                }
            }
            if entry.admitted.is_none() {
                if let Some(spec) = entry.descriptor_spec.as_ref() {
                    let extra = spec.extra_bytes();
                    if extra > 0 && self.standby_specs.insert(Arc::as_ptr(spec) as usize) {
                        added = added.saturating_add(extra);
                    }
                }
            }
        }
        self.standby_bytes = added;
    }

    /// Reclaim optional descriptor lookup-name overrides and unadmitted standby
    /// companions until `permanent_total + self.standby_bytes <= max_bytes`, so
    /// primary font programs, Form fonts, and drawn companions always take
    /// priority over optional standby metadata regardless of resource order.
    pub(crate) fn reclaim_standby_for(&mut self, permanent_total: u64, max_bytes: u64) {
        if self.standby_bytes == 0 {
            return;
        }
        // Fast path: when no standby companion faces could have deduplicated
        // against a newly added program and total bytes already fit, avoid
        // scanning `embedded_companions` (keeps loading N aliases O(N)).
        if !self.has_standby_companions
            && permanent_total.saturating_add(self.standby_bytes) <= max_bytes
        {
            return;
        }
        self.recompute_standby_bytes();
        if permanent_total.saturating_add(self.standby_bytes) <= max_bytes {
            return;
        }
        // Reclaim optional `/FontDescriptor /FontName` lookup-name overrides
        // first (grouping aliases that share one `Arc<DescriptorSubstituteSpec>`
        // so stripping frees the shared bytes immediately in O(N) total work),
        // keeping zero-byte hints/ordering/ID intact.
        let mut spec_groups: HashMap<usize, (FontId, u64, Vec<FontId>)> = HashMap::new();
        for (&id, entry) in &self.embedded_companions {
            if entry.admitted.is_none() {
                if let Some(spec) = entry.descriptor_spec.as_ref() {
                    let extra = spec.extra_bytes();
                    if extra > 0 {
                        let ptr = Arc::as_ptr(spec) as usize;
                        spec_groups
                            .entry(ptr)
                            .and_modify(|(min_id, _, ids)| {
                                *min_id = (*min_id).min(id);
                                ids.push(id);
                            })
                            .or_insert_with(|| (id, extra, vec![id]));
                    }
                }
            }
        }
        let mut ordered_groups: Vec<(FontId, usize, u64, Vec<FontId>)> = spec_groups
            .into_iter()
            .map(|(ptr, (min_id, extra, ids))| (min_id, ptr, extra, ids))
            .collect();
        ordered_groups.sort_unstable_by_key(|&(min_id, _, _, _)| min_id);
        for (_, ptr, extra, ids) in ordered_groups {
            let mut stripped_replacement: Option<Arc<DescriptorSubstituteSpec>> = None;
            for id in ids {
                if let Some(entry) = self.embedded_companions.get_mut(&id) {
                    if let Some(spec) = entry.descriptor_spec.as_ref() {
                        let repl = stripped_replacement
                            .get_or_insert_with(|| Arc::new(spec.without_lookup_name()));
                        entry.descriptor_spec = Some(Arc::clone(repl));
                    }
                }
            }
            if self.standby_specs.remove(&ptr) {
                self.standby_bytes = self.standby_bytes.saturating_sub(extra);
            }
            if permanent_total.saturating_add(self.standby_bytes) <= max_bytes {
                return;
            }
        }
        let mut standby_ids: Vec<FontId> = self
            .embedded_companions
            .iter()
            .filter_map(|(&id, entry)| entry.standby.is_some().then_some(id))
            .collect();
        standby_ids.sort_unstable();
        for id in standby_ids {
            if let Some(entry) = self.embedded_companions.get_mut(&id) {
                entry.standby = None;
            }
            self.recompute_standby_bytes();
            if permanent_total.saturating_add(self.standby_bytes) <= max_bytes {
                break;
            }
        }
    }

    /// Pair a substituted font `id` with an optional reload `source` and an
    /// optional in-memory `standby` embedded original within `max_bytes`.
    ///
    /// When `standby` fits within `max_bytes` alongside existing permanent and
    /// standby fonts, it is retained as a budget-charged, reclaimable standby
    /// copy (so callers invoking `ContentInterpreter::with_fonts` without a
    /// `PdfFile` can still recover surviving embedded glyphs). When `standby`
    /// exceeds `max_bytes`, it is dropped immediately and only the lightweight
    /// `source` reference (if any) is recorded. Returns `true` when `standby`
    /// was retained.
    #[doc(hidden)]
    pub fn record_companion(
        &mut self,
        id: FontId,
        kind: CompanionKind,
        source: CompanionSource,
        standby: Option<Arc<LoadedFont>>,
        max_bytes: u64,
    ) -> bool {
        if !self.fonts.contains_key(&id) {
            return false;
        }
        if let Some(existing) = self.embedded_companions.get(&id) {
            return existing.admitted.is_some() || existing.standby.is_some();
        }
        let retained_standby = match standby {
            Some(comp) => match self
                .standby_extra_bytes(Some(&comp), None)
                .and_then(|sb| self.budget.bytes_used().checked_add(sb))
            {
                Some(total) if total <= max_bytes => {
                    self.standby_bytes = total - self.budget.bytes_used();
                    self.has_standby_companions = true;
                    Some(comp)
                }
                _ => None,
            },
            None => None,
        };
        let retained = retained_standby.is_some();
        if retained
            || !matches!(source, CompanionSource::None)
            || matches!(kind, CompanionKind::DescriptorSubstitute { .. })
        {
            self.embedded_companions.insert(
                id,
                CompanionEntry {
                    kind,
                    source,
                    descriptor_spec: None,
                    standby: retained_standby,
                    admitted: None,
                    failed_budget: None,
                    failed_program_count: 0,
                    provider_generation: self.provider.as_ref().map(|p| p.generation()),
                    missed_codes: HashSet::new(),
                },
            );
        }
        retained
    }

    /// Record lightweight descriptor metadata for an embedded primary font so
    /// unreadable shown glyphs can lazily resolve a descriptor-based substitute
    /// without retaining uncharged font bytes or needing a `PdfFile`.
    /// Any optional `lookup_name` override is shared by `Arc` and charged
    /// incrementally in reclaimable standby bytes so primary font programs
    /// always take priority over optional descriptor names and loading many
    /// shared resource aliases stays O(N).
    #[doc(hidden)]
    pub fn record_descriptor_substitute(
        &mut self,
        id: FontId,
        spec: Arc<DescriptorSubstituteSpec>,
        max_bytes: u64,
    ) {
        if !self.fonts.contains_key(&id) || self.embedded_companions.contains_key(&id) {
            return;
        }
        let extra = spec.extra_bytes();
        let spec = if extra == 0 {
            spec
        } else {
            let key = Arc::as_ptr(&spec) as usize;
            if self.standby_specs.contains(&key) {
                spec
            } else {
                match self
                    .budget
                    .bytes_used()
                    .checked_add(self.standby_bytes)
                    .and_then(|used| used.checked_add(extra))
                {
                    Some(total) if total <= max_bytes => {
                        self.standby_specs.insert(key);
                        self.standby_bytes += extra;
                        spec
                    }
                    _ => Arc::new(spec.without_lookup_name()),
                }
            }
        };
        let font_file_id = spec.font_file_id;
        self.embedded_companions.insert(
            id,
            CompanionEntry {
                kind: CompanionKind::DescriptorSubstitute { font_file_id },
                source: CompanionSource::None,
                descriptor_spec: Some(spec),
                standby: None,
                admitted: None,
                failed_budget: None,
                failed_program_count: 0,
                provider_generation: self.provider.as_ref().map(|p| p.generation()),
                missed_codes: HashSet::new(),
            },
        );
    }

    /// Pair a substituted font `id` with an indirect PDF font dictionary whose
    /// partially readable embedded original can be loaded on demand if an
    /// uncovered glyph actually needs it. Retains no font program bytes and
    /// charges zero cache budget until used.
    #[doc(hidden)]
    pub fn record_deferred_companion_ref(&mut self, id: FontId, font_ref: zpdf_core::ObjectId) {
        let _ = self.record_companion(
            id,
            CompanionKind::EmbeddedOriginal,
            CompanionSource::Object(font_ref),
            None,
            u64::MAX,
        );
    }

    /// Pair a substituted font `id` with an already-loaded embedded original
    /// when `max_bytes` allows, charging the page cache budget before retaining
    /// the `Arc`. Returns `false` and drops `embedded` if `id` is absent or
    /// retaining `embedded` would exceed `max_bytes`.
    #[doc(hidden)]
    pub fn record_embedded_companion(
        &mut self,
        id: FontId,
        embedded: Arc<LoadedFont>,
        max_bytes: u64,
    ) -> bool {
        self.record_companion(
            id,
            CompanionKind::EmbeddedOriginal,
            CompanionSource::None,
            Some(embedded),
            max_bytes,
        )
    }

    /// Companion kind registered for `id`, if any.
    #[doc(hidden)]
    pub fn companion_kind(&self, id: FontId) -> Option<CompanionKind> {
        self.embedded_companions.get(&id).map(|entry| entry.kind)
    }

    /// Whether `id` has a paired embedded-original companion available (either
    /// deferred, standby, or already admitted) for uncovered substitute glyphs.
    #[doc(hidden)]
    pub fn has_embedded_companion(&self, id: FontId) -> bool {
        self.embedded_companions
            .get(&id)
            .is_some_and(|entry| matches!(entry.kind, CompanionKind::EmbeddedOriginal))
    }

    /// Whether `id` has a paired embedded-original companion that can still be
    /// probed within `max_bytes` (admitted, standby, or an unfailed deferred
    /// source). Descriptor-substitute companions on healthy embedded fonts are
    /// excluded so normal text shows stay on the ordinary fast path unless a
    /// shown glyph actually proves unreadable.
    #[doc(hidden)]
    pub fn has_embedded_companion_for_budget(&self, id: FontId, max_bytes: u64) -> bool {
        if self.embedded_companions.is_empty() {
            return false;
        }
        let Some(entry) = self.embedded_companions.get(&id) else {
            return false;
        };
        if !matches!(entry.kind, CompanionKind::EmbeddedOriginal) {
            return false;
        }
        self.companion_entry_probeable(entry, max_bytes)
    }

    /// Whether `id` has a paired descriptor-substitute companion that can still
    /// be probed within `max_bytes` for an unreadable shown embedded glyph.
    #[doc(hidden)]
    pub fn has_descriptor_substitute_for_budget(&self, id: FontId, max_bytes: u64) -> bool {
        if self.embedded_companions.is_empty() {
            return false;
        }
        let Some(entry) = self.embedded_companions.get(&id) else {
            return false;
        };
        if !matches!(entry.kind, CompanionKind::DescriptorSubstitute { .. }) {
            return false;
        }
        self.companion_entry_probeable(entry, max_bytes)
    }

    fn companion_entry_probeable(&self, entry: &CompanionEntry, max_bytes: u64) -> bool {
        if entry.admitted.is_some() || entry.standby.is_some() {
            return true;
        }
        if entry.descriptor_spec.is_none() && matches!(entry.source, CompanionSource::None) {
            return false;
        }
        let remaining = max_bytes.saturating_sub(self.budget.bytes_used());
        if entry.descriptor_spec.is_some() {
            let current_gen = self.provider.as_ref().map(|p| p.generation());
            if entry.provider_generation != current_gen {
                return true;
            }
            if entry.failed_program_count != self.budget.programs.len() {
                return true;
            }
            // A descriptor substitute sharing an already-admitted program with
            // zero owned mapping bytes can still fit when `remaining == 0`;
            // let `admission_total` decide unless it already failed here.
            return !entry.failed_budget.is_some_and(|limit| remaining <= limit);
        }
        if remaining == 0 {
            return false;
        }
        !entry.failed_budget.is_some_and(|limit| remaining <= limit)
    }

    /// Whether `id` currently retains an in-memory embedded companion program
    /// in this page cache (either admitted or in reclaimable standby).
    #[doc(hidden)]
    pub fn has_retained_embedded_companion(&self, id: FontId) -> bool {
        self.embedded_companions
            .get(&id)
            .is_some_and(|entry| entry.admitted.is_some() || entry.standby.is_some())
    }

    /// Whether `id`'s embedded companion has been permanently admitted as a
    /// drawing face for glyphs that actually used it.
    #[doc(hidden)]
    pub fn has_admitted_embedded_companion(&self, id: FontId) -> bool {
        self.embedded_companions
            .get(&id)
            .is_some_and(|entry| entry.admitted.is_some())
    }

    /// Admit `font` under `name` within `max_bytes`, preserving logical PDF
    /// metrics/mappings and attaching diagnostics if the program exceeds the
    /// cache budget.
    ///
    /// When the full font program exceeds `max_bytes`, this records a
    /// `StreamSizeLimit` resource-limit diagnostic, retries admission with
    /// [`LoadedFont::without_font_program`] (keeping composite segmentation,
    /// `/W`, `/DW2`, `/Widths`, `/Encoding`, and `/ToUnicode`), and falls back
    /// to a zero-byte placeholder only if even the logical font does not fit.
    /// Returns `Err(diagnostics)` only when no entry (not even a placeholder)
    /// could be admitted.
    pub fn admit_with_diagnostics(
        &mut self,
        name: String,
        font: Arc<LoadedFont>,
        mut diagnostics: zpdf_core::ProcessingDiagnostics,
        object_id: Option<zpdf_core::ObjectId>,
        max_bytes: u64,
    ) -> Result<FontId, zpdf_core::ProcessingDiagnostics> {
        if let Some(fid) =
            self.try_insert_shared_with_limit(name.clone(), Arc::clone(&font), max_bytes)
        {
            if !diagnostics.is_empty() {
                self.record_embedded_fallback(fid, diagnostics);
            }
            return Ok(fid);
        }
        let limit_err = zpdf_core::Error::StreamSizeLimit(max_bytes);
        if let Some(id) = object_id {
            diagnostics.record_object_failure(id, &limit_err);
        } else {
            diagnostics.record_decode_failure(&limit_err);
        }
        let metrics_only = font.without_font_program();
        let fid = self
            .try_insert_with_limit(name.clone(), metrics_only, max_bytes)
            .or_else(|| {
                self.try_insert_with_limit(
                    name,
                    LoadedFont::new_placeholder(font.base_font.clone()),
                    max_bytes,
                )
            });
        match fid {
            Some(fid) => {
                self.record_embedded_fallback(fid, diagnostics);
                Ok(fid)
            }
            None => Err(diagnostics),
        }
    }

    /// Record fallback diagnostics associated with `id` when its embedded font
    /// program failed to decode or parse.
    pub fn record_embedded_fallback(
        &mut self,
        id: FontId,
        diagnostics: zpdf_core::ProcessingDiagnostics,
    ) {
        if diagnostics.is_empty() {
            return;
        }
        self.embedded_fallbacks
            .entry(id)
            .and_modify(|existing| existing.merge(diagnostics))
            .or_insert(diagnostics);
    }

    /// Diagnostics recorded for `id` when its embedded font program failed and
    /// fell back, or `None` for valid embedded and normal non-embedded fonts.
    pub fn embedded_fallback(&self, id: FontId) -> Option<zpdf_core::ProcessingDiagnostics> {
        if self.embedded_fallbacks.is_empty() {
            return None;
        }
        self.embedded_fallbacks.get(&id).copied()
    }

    /// Aggregate diagnostics across all fonts in this cache whose embedded
    /// programs failed to decode or parse.
    pub fn diagnostics(&self) -> zpdf_core::ProcessingDiagnostics {
        let mut all = zpdf_core::ProcessingDiagnostics::default();
        if !self.embedded_fallbacks.is_empty() {
            let mut ids: Vec<FontId> = self.embedded_fallbacks.keys().copied().collect();
            ids.sort_unstable();
            for id in ids {
                if let Some(&diag) = self.embedded_fallbacks.get(&id) {
                    all.merge(diag);
                }
            }
        }
        all
    }

    /// Carry the document's provider into nested Form XObject font loading.
    pub fn with_provider(mut self, provider: Arc<FontProvider>) -> Self {
        self.provider = Some(provider);
        self.resolution = None;
        self
    }

    pub fn provider(&self) -> Option<&Arc<FontProvider>> {
        self.provider.as_ref()
    }

    /// Compatibility shim for the former capacity-limited cache constructor.
    ///
    /// Evicting entries at this threshold would invalidate display-list font
    /// IDs, so `capacity` is now only a preallocation hint.
    #[deprecated(
        note = "old hard-limit semantics could not safely be preserved; use FontCache::with_preallocated_capacity"
    )]
    pub fn with_capacity(capacity: usize) -> Self {
        Self::with_preallocated_capacity(capacity)
    }

    pub fn get(&self, id: FontId) -> Option<&LoadedFont> {
        self.fonts.get(&id).map(|font| font.as_ref())
    }

    /// Return the logical PDF font whose text mappings (`/ToUnicode`,
    /// `/Encoding`, `cid_cmap`, `font_type`, and `base_font`) govern `id`,
    /// following any admitted drawing-only companion/fallback face back to its
    /// original PDF font.
    pub fn logical_font(&self, id: FontId) -> Option<&LoadedFont> {
        let logical_id = self.drawing_font_originals.get(&id).copied().unwrap_or(id);
        self.fonts.get(&logical_id).map(|font| font.as_ref())
    }

    pub fn get_by_name(&self, name: &str) -> Option<(FontId, &LoadedFont)> {
        let id = self.name_to_id.get(name)?;
        let font = self.fonts.get(id)?;
        Some((*id, font.as_ref()))
    }

    pub fn insert(&mut self, name: String, font: LoadedFont) -> FontId {
        self.try_insert_with_limit(name, font, u64::MAX)
            .expect("font id space exhausted")
    }

    /// Admit a font another cache may already hold, without copying it.
    ///
    /// A document's pages routinely reference the same font objects, so a
    /// document-level cache hands the same parsed font to each page's cache.
    /// Cloning a `LoadedFont` instead would deep-copy its maps (`cid_to_gid`,
    /// `to_unicode`) — for a CJK font that costs about what re-parsing does,
    /// which is the whole thing this exists to avoid.
    pub fn insert_shared(&mut self, name: String, font: Arc<LoadedFont>) -> FontId {
        self.try_insert_shared_with_limit(name, font, u64::MAX)
            .expect("font id space exhausted")
    }

    /// Admit a font without evicting any existing ID. Shared `Arc` font files
    /// (notably multiple TTC faces) are charged once; independently owned
    /// mappings count for each font. Aliases of one `Arc<LoadedFont>` count
    /// once. Returns `None` when the parsed-font budget or ID space is exhausted.
    pub fn try_insert_with_limit(
        &mut self,
        name: String,
        font: LoadedFont,
        max_bytes: u64,
    ) -> Option<FontId> {
        self.try_insert_shared_with_limit(name, Arc::new(font), max_bytes)
    }

    /// [`Self::try_insert_with_limit`] for a font that is already shared.
    pub fn try_insert_shared_with_limit(
        &mut self,
        name: String,
        font: Arc<LoadedFont>,
        max_bytes: u64,
    ) -> Option<FontId> {
        if let Some(&existing_id) = self.name_to_id.get(&name) {
            return Some(existing_id);
        }

        let prev_total = self.budget.bytes_used();
        let prev_programs = self.budget.programs.len();
        let new_total = self.budget.admission_total(&font, max_bytes)?;
        let id = self.next_free_id()?;
        self.budget.record(&font, new_total);
        self.name_to_id.insert(name, id);
        self.fonts.insert(id, font);
        if new_total != prev_total
            || self.budget.programs.len() != prev_programs
            || new_total.saturating_add(self.standby_bytes) > max_bytes
        {
            self.reclaim_standby_for(new_total, max_bytes);
        }
        Some(id)
    }

    fn next_free_id(&mut self) -> Option<FontId> {
        let start = self.next_id;
        loop {
            let candidate = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            if !self.fonts.contains_key(&candidate) {
                return Some(candidate);
            }
            if self.next_id == start {
                return None;
            }
        }
    }

    pub fn len(&self) -> usize {
        self.fonts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fonts.is_empty()
    }

    /// Estimated retained programs and mappings, accounting for shared ownership
    /// and any retained reclaimable standby companions.
    pub fn bytes_used(&self) -> u64 {
        self.budget.bytes_used().saturating_add(self.standby_bytes)
    }
}

impl Default for FontCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- synthetic sfnt / cmap fixtures -----

    /// Assemble an sfnt from (tag, data) pairs. Tags must be pre-sorted —
    /// ttf-parser binary-searches the table directory.
    fn build_sfnt(tables: &[([u8; 4], Vec<u8>)]) -> Vec<u8> {
        let header_size = 12 + tables.len() * 16;
        let mut offsets = Vec::new();
        let mut pos = header_size;
        for (_, data) in tables {
            offsets.push(pos);
            pos += (data.len() + 3) & !3;
        }
        let mut buf = vec![0u8; pos];
        buf[0..4].copy_from_slice(&0x00010000u32.to_be_bytes());
        buf[4..6].copy_from_slice(&(tables.len() as u16).to_be_bytes());
        for (i, (tag, data)) in tables.iter().enumerate() {
            let rec = 12 + i * 16;
            buf[rec..rec + 4].copy_from_slice(tag);
            buf[rec + 8..rec + 12].copy_from_slice(&(offsets[i] as u32).to_be_bytes());
            buf[rec + 12..rec + 16].copy_from_slice(&(data.len() as u32).to_be_bytes());
            buf[offsets[i]..offsets[i] + data.len()].copy_from_slice(data);
        }
        buf
    }

    fn head_table() -> Vec<u8> {
        let mut d = vec![0u8; 54];
        d[0..4].copy_from_slice(&0x00010000u32.to_be_bytes());
        d[12..16].copy_from_slice(&0x5F0F3CF5u32.to_be_bytes()); // magic
        d[16..18].copy_from_slice(&0x000Bu16.to_be_bytes()); // flags
        d[18..20].copy_from_slice(&1000u16.to_be_bytes()); // unitsPerEm
        d
    }

    fn hhea_table() -> Vec<u8> {
        let mut d = vec![0u8; 36];
        d[0..4].copy_from_slice(&0x00010000u32.to_be_bytes());
        d[4..6].copy_from_slice(&800i16.to_be_bytes());
        d[6..8].copy_from_slice(&(-200i16).to_be_bytes());
        d
    }

    fn maxp_table(num_glyphs: u16) -> Vec<u8> {
        let mut d = vec![0u8; 6];
        d[0..4].copy_from_slice(&0x00005000u32.to_be_bytes()); // version 0.5
        d[4..6].copy_from_slice(&num_glyphs.to_be_bytes());
        d
    }

    fn build_cmap(subtables: &[(u16, u16, Vec<u8>)]) -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(&0u16.to_be_bytes());
        d.extend_from_slice(&(subtables.len() as u16).to_be_bytes());
        let mut offset = 4 + subtables.len() * 8;
        for (plat, enc, data) in subtables {
            d.extend_from_slice(&plat.to_be_bytes());
            d.extend_from_slice(&enc.to_be_bytes());
            d.extend_from_slice(&(offset as u32).to_be_bytes());
            offset += data.len();
        }
        for (_, _, data) in subtables {
            d.extend_from_slice(data);
        }
        d
    }

    /// cmap format 4 subtable mapping the single code point `code` → `gid`.
    fn cmap_format4_single(code: u16, gid: u16) -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(&4u16.to_be_bytes()); // format
        d.extend_from_slice(&32u16.to_be_bytes()); // length
        d.extend_from_slice(&0u16.to_be_bytes()); // language
        d.extend_from_slice(&4u16.to_be_bytes()); // segCountX2
        d.extend_from_slice(&4u16.to_be_bytes()); // searchRange
        d.extend_from_slice(&1u16.to_be_bytes()); // entrySelector
        d.extend_from_slice(&0u16.to_be_bytes()); // rangeShift
        d.extend_from_slice(&code.to_be_bytes()); // endCode[0]
        d.extend_from_slice(&0xFFFFu16.to_be_bytes()); // endCode[1]
        d.extend_from_slice(&0u16.to_be_bytes()); // reservedPad
        d.extend_from_slice(&code.to_be_bytes()); // startCode[0]
        d.extend_from_slice(&0xFFFFu16.to_be_bytes()); // startCode[1]
        d.extend_from_slice(&gid.wrapping_sub(code).to_be_bytes()); // idDelta[0]
        d.extend_from_slice(&1u16.to_be_bytes()); // idDelta[1] (0xFFFF → 0)
        d.extend_from_slice(&0u16.to_be_bytes()); // idRangeOffset[0]
        d.extend_from_slice(&0u16.to_be_bytes()); // idRangeOffset[1]
        d
    }

    /// cmap format 0 (byte-encoded, MacRoman-indexed) subtable.
    fn cmap_format0(glyph_ids: &[u8; 256]) -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(&0u16.to_be_bytes()); // format
        d.extend_from_slice(&262u16.to_be_bytes()); // length
        d.extend_from_slice(&0u16.to_be_bytes()); // language
        d.extend_from_slice(glyph_ids);
        d
    }

    fn font_with_cmap(cmap: Vec<u8>) -> LoadedFont {
        let data = build_sfnt(&[
            (*b"cmap", cmap),
            (*b"head", head_table()),
            (*b"hhea", hhea_table()),
            (*b"maxp", maxp_table(16)),
        ]);
        let font = LoadedFont::new_with_data(
            PdfFontType::TrueType,
            "Test".into(),
            data,
            CidWidths::new(500.0),
        );
        assert!(font.font_data.is_some(), "synthetic sfnt must parse");
        font
    }

    // ----- code_to_gid cmap fallbacks -----

    #[test]
    fn embedded_truetype_without_cmap_uses_codes_as_glyph_ids() {
        let mut head = head_table();
        head[50..52].copy_from_slice(&0u16.to_be_bytes()); // short loca
        let data = build_sfnt(&[
            (*b"glyf", vec![0; 4]),
            (*b"head", head),
            (*b"hhea", hhea_table()),
            (*b"loca", vec![0; 2 * 17]),
            (*b"maxp", maxp_table(16)),
        ]);
        let mut font = LoadedFont::new_with_data(
            PdfFontType::TrueType,
            "NoCmap".into(),
            data,
            CidWidths::new(500.0),
        );
        font.encoding = Some(crate::encoding::Encoding::win_ansi());
        assert_eq!(font.code_to_gid(5), Some(5));
        assert_eq!(font.code_to_gid(16), None, "beyond numGlyphs");
        font.is_substitute = true;
        assert_eq!(font.code_to_gid(5), None, "substitute GIDs are not codes");
    }

    #[test]
    fn symbolic_f000_retry_on_windows_unicode_subtable() {
        // Only a (3,1) subtable, glyph parked at PUA 0xF041.
        let cmap = build_cmap(&[(3, 1, cmap_format4_single(0xF041, 5))]);
        let mut font = font_with_cmap(cmap);

        // Non-symbolic: 0x41 is not in the subtable, no PUA retry.
        assert_eq!(font.code_to_gid(0x41), None);

        font.symbolic = true;
        assert_eq!(font.code_to_gid(0x41), Some(5));
    }

    #[test]
    fn glyph_name_resolves_via_macroman_slot_on_mac_subtable() {
        // WinAnsi maps 0x95 → "bullet"; MacRoman slots bullet at 0xA5, which
        // is where the (1,0) subtable indexes it.
        let mut glyph_ids = [0u8; 256];
        glyph_ids[0xA5] = 7;
        glyph_ids[0xB4] = 9; // MacRoman yen, distinct from bullet.
        let cmap = build_cmap(&[(1, 0, cmap_format0(&glyph_ids))]);
        let mut font = font_with_cmap(cmap);

        // Without an /Encoding there is no name to detour through.
        assert_eq!(font.code_to_gid(0x95), None);

        font.encoding = Some(encoding::Encoding::from_base(&encoding::WIN_ANSI_ENCODING));
        assert_eq!(font.code_to_gid(0x95), Some(7));
        // WinAnsi 0xA5 means yen; reusing raw MacRoman 0xA5 would draw bullet.
        assert_eq!(font.code_to_gid(0xA5), Some(9));
    }

    #[test]
    fn mac_roman_inverse_lookup() {
        assert_eq!(mac_roman_code_for_name("bullet"), Some(0xA5));
        assert_eq!(mac_roman_code_for_name("A"), Some(0x41));
        assert_eq!(mac_roman_code_for_name("nosuchglyphname"), None);
    }

    #[test]
    fn utf16_cmap_keeps_raw_code_for_to_unicode_lookup() {
        let mut font = LoadedFont::new_placeholder("Test".into());
        font.font_type = PdfFontType::Type0CidType2;
        font.cid_cmap = cmap::CidCMap::predefined("UniJIS-UTF16-H");
        font.to_unicode = Some(cmap::ToUnicodeMap::parse(
            b"beginbfchar <D840DC00> <0041> endbfchar",
        ));
        assert_eq!(font.decode_to_string(&[0xD8, 0x40, 0xDC, 0x00]), "A");
    }

    // ----- OTF-wrapped CID-keyed CFF -----

    /// Minimal CFF table: Name INDEX ("T"), Top DICT (optionally with /ROS),
    /// empty String/GSubr INDEXes, format-0 charset (GID 1 → CID 5,
    /// GID 2 → CID 9), and a 3-entry empty CharStrings INDEX.
    fn build_test_cff(cid_keyed: bool) -> Vec<u8> {
        let dict_len: usize = 12 + if cid_keyed { 9 } else { 0 };
        let charset_off = 19 + dict_len;
        let charstrings_off = charset_off + 5;

        let mut cff = vec![1, 0, 4, 4]; // header
        cff.extend_from_slice(&[0x00, 0x01, 0x01, 0x01, 0x02, b'T']); // Name INDEX
                                                                      // Top DICT INDEX: one entry of dict_len bytes.
        cff.extend_from_slice(&[0x00, 0x01, 0x01, 0x01, dict_len as u8 + 1]);
        if cid_keyed {
            cff.extend_from_slice(&[28, 0x01, 0x87]); // SID 391 (registry)
            cff.extend_from_slice(&[28, 0x01, 0x88]); // SID 392 (ordering)
            cff.push(139); // supplement 0
            cff.extend_from_slice(&[12, 30]); // ROS
        }
        cff.push(29);
        cff.extend_from_slice(&(charset_off as i32).to_be_bytes());
        cff.push(15); // charset
        cff.push(29);
        cff.extend_from_slice(&(charstrings_off as i32).to_be_bytes());
        cff.push(17); // CharStrings
        cff.extend_from_slice(&[0x00, 0x00]); // String INDEX (empty)
        cff.extend_from_slice(&[0x00, 0x00]); // Global Subr INDEX (empty)
        cff.extend_from_slice(&[0, 0x00, 0x05, 0x00, 0x09]); // charset fmt 0
        cff.extend_from_slice(&[0x00, 0x03, 0x01, 0x01, 0x01, 0x01, 0x01]); // CharStrings
        assert_eq!(cff.len(), charstrings_off + 7);
        cff
    }

    #[test]
    fn cff_cid_keyed_detection() {
        assert!(cff_is_cid_keyed(&build_test_cff(true)));
        assert!(!cff_is_cid_keyed(&build_test_cff(false)));
    }

    #[test]
    fn otf_wrapped_cid_keyed_cff_builds_cid_to_gid() {
        // FontFile3 /Subtype /OpenType: data arrives as an sfnt, not raw CFF,
        // so the charset CID → GID map must still be built from the CFF table.
        let otf = wrap_cff_in_otf(&build_test_cff(true));
        assert!(!is_raw_cff(&otf));
        let font = LoadedFont::new_with_data(
            PdfFontType::Type0CidType2,
            "Test".into(),
            otf,
            CidWidths::new(1000.0),
        );
        let map = font.cid_to_gid.expect("charset-derived map");
        assert_eq!(map.get(&5), Some(&1));
        assert_eq!(map.get(&9), Some(&2));
    }

    #[test]
    fn otf_wrapped_plain_cff_keeps_identity_mapping() {
        // No /ROS: the charset holds glyph-name SIDs, not CIDs — no remap.
        let otf = wrap_cff_in_otf(&build_test_cff(false));
        let font = LoadedFont::new_with_data(
            PdfFontType::Type0CidType2,
            "Test".into(),
            otf,
            CidWidths::new(1000.0),
        );
        assert!(font.cid_to_gid.is_none());
    }

    #[test]
    fn malformed_cff_indexes_never_panic() {
        for len in 0..48 {
            for fill in [0u8, 1, 4, 0xFF] {
                let data = vec![fill; len];
                let result = std::panic::catch_unwind(|| {
                    let _ = cff_top_dict(&data);
                    let _ = parse_cff_encoding(&data);
                    let _ = parse_cff_orphan_gids(&data);
                    let _ = parse_cff_charset(&data);
                    for pos in 0..=data.len() {
                        let _ = skip_cff_index(&data, pos);
                    }
                });
                assert!(result.is_ok(), "panicked for len={len}, fill={fill:#x}");
            }
        }
    }

    #[test]
    fn type3_simple_glyph_advance_uses_horizontal_font_matrix_component() {
        let font = LoadedFont::new_with_data(
            PdfFontType::Type3 {
                font_matrix: [0.002, 0.003, 0.0, 0.002, 0.0, 0.0],
                char_procs: HashMap::new(),
                encoding: vec!["g0".into(), "g1".into()],
                widths: vec![600.0, 700.0],
                first_char: 32,
            },
            "Type3Test".into(),
            Vec::new(),
            CidWidths::new(1000.0),
        );

        // `simple_glyph_advance` returns units compatible with
        // `advance_divisor`, so the 0.002 FontMatrix is normalized back to
        // the font's 1000-unit advance scale here. The content interpreter
        // applies the divisor when converting this value to text space.
        assert!((font.simple_glyph_advance(33, 1) - 1400.0).abs() < 1e-9);
    }

    /// `/Widths` starts at `/FirstChar`: a code below it has no entry and
    /// takes the default width, exactly like a code past the end, rather than
    /// the first entry's width.
    #[test]
    fn type3_width_outside_first_char_range_uses_default() {
        let font = LoadedFont::new_with_data(
            PdfFontType::Type3 {
                font_matrix: [0.001, 0.0, 0.0, 0.001, 0.0, 0.0],
                char_procs: HashMap::new(),
                encoding: Vec::new(),
                widths: vec![600.0, 700.0],
                first_char: 32,
            },
            "Type3Test".into(),
            Vec::new(),
            CidWidths::new(250.0),
        );
        assert_eq!(font.type3_glyph_width(32), 600.0);
        assert_eq!(font.type3_glyph_width(33), 700.0);
        assert_eq!(font.type3_glyph_width(31), 250.0, "below /FirstChar");
        assert_eq!(font.type3_glyph_width(0), 250.0, "below /FirstChar");
        assert_eq!(font.type3_glyph_width(34), 250.0, "past the end");
    }

    #[test]
    fn font_ids_remain_valid_beyond_capacity_hint() {
        let mut cache = FontCache::with_preallocated_capacity(1);
        let first = cache.insert("A".into(), LoadedFont::new_placeholder("A".into()));
        let second = cache.insert("B".into(), LoadedFont::new_placeholder("B".into()));
        assert!(cache.get(first).is_some());
        assert!(cache.get(second).is_some());
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn font_ids_wrap_without_overwriting_live_entries() {
        let mut cache = FontCache::new();
        cache.next_id = u32::MAX;
        let last = cache.insert("A".into(), LoadedFont::new_placeholder("A".into()));
        let zero = cache.insert("B".into(), LoadedFont::new_placeholder("B".into()));
        assert_eq!(last, u32::MAX);
        assert_eq!(zero, 0);
        assert!(cache.get(last).is_some() && cache.get(zero).is_some());
    }

    #[test]
    fn mapping_storage_is_charged_per_font_but_not_per_alias() {
        let data: Arc<[u8]> = Arc::from(vec![0u8; 8]);
        let make_font = || {
            let mut font = LoadedFont::new_placeholder("Mapping".into());
            font.font_data = Some(Arc::clone(&data));
            font.to_unicode = Some(cmap::ToUnicodeMap::parse(
                b"1 beginbfchar <41> <0041> endbfchar",
            ));
            Arc::new(font)
        };
        let first = make_font();
        let first_cost = first.estimated_cache_bytes();
        assert!(first_cost > data.len() as u64);
        let second = make_font();
        let total = first_cost + second.estimated_cache_bytes() - data.len() as u64;
        let mut cache = FontCache::new();
        assert_eq!(
            cache.try_insert_shared_with_limit("A".into(), Arc::clone(&first), first_cost - 1),
            None
        );
        assert_eq!(
            cache.try_insert_shared_with_limit("A".into(), Arc::clone(&first), first_cost),
            Some(0)
        );
        assert_eq!(
            cache.try_insert_shared_with_limit("Alias".into(), first, first_cost),
            Some(1)
        );
        assert_eq!(cache.bytes_used(), first_cost);
        assert_eq!(
            cache.try_insert_shared_with_limit("B".into(), Arc::clone(&second), total - 1),
            None
        );
        assert_eq!(
            cache.try_insert_shared_with_limit("B".into(), second, total),
            Some(2)
        );
        assert_eq!(cache.bytes_used(), total);

        let mut mapping_only = LoadedFont::new_placeholder("NoProgram".into());
        mapping_only.to_unicode = Some(cmap::ToUnicodeMap::parse(
            b"1 beginbfchar <41> <0041> endbfchar",
        ));
        let cost = mapping_only.estimated_cache_bytes();
        assert!(cost > 0);
        let mut cache = FontCache::new();
        assert_eq!(
            cache.try_insert_with_limit("F".into(), mapping_only, cost - 1),
            None
        );
    }

    #[test]
    fn type3_shared_charprocs_count_program_bytes_once() {
        let data: Arc<[u8]> = Arc::from(&b"q Q"[..]);
        let mut font = LoadedFont::new_placeholder("Type3".into());
        font.font_type = PdfFontType::Type3 {
            font_matrix: [0.001, 0.0, 0.0, 0.001, 0.0, 0.0],
            char_procs: HashMap::from([("A".into(), Arc::clone(&data)), ("B".into(), data)]),
            encoding: vec!["A".into(), "B".into()],
            widths: vec![500.0, 500.0],
            first_char: 0,
        };
        assert_eq!(
            font.estimated_cache_bytes(),
            font.estimated_owned_cache_bytes() + 3
        );
    }

    #[test]
    fn font_cache_budget_is_non_evicting_and_deduplicates_shared_programs() {
        let data: Arc<[u8]> = Arc::from(vec![0u8; 8]);
        let mut first = LoadedFont::new_placeholder("A".into());
        first.font_data = Some(data.clone());
        let mut second = LoadedFont::new_placeholder("B".into());
        second.font_data = Some(data);
        let mut third = LoadedFont::new_placeholder("C".into());
        third.font_data = Some(Arc::from(vec![0u8; 1]));

        let mut cache = FontCache::new();
        assert_eq!(cache.try_insert_with_limit("A".into(), first, 8), Some(0));
        assert_eq!(cache.try_insert_with_limit("B".into(), second, 8), Some(1));
        assert_eq!(cache.try_insert_with_limit("C".into(), third, 8), None);
        assert_eq!(cache.bytes_used(), 8);
        assert!(cache.get(0).is_some() && cache.get(1).is_some());
        assert_eq!(
            cache.try_insert_with_limit("D".into(), LoadedFont::new_placeholder("D".into()), 8),
            Some(2)
        );
    }

    /// Build a minimal TrueType sfnt with custom per-glyph `glyf` records and an
    /// optional Unicode format-4 + MacRoman format-0 `cmap`.
    fn build_glyf_sfnt(
        glyph_records: &[Vec<u8>],
        cmap_entries: &[(u16, u16)],
        mac_roman: bool,
    ) -> Vec<u8> {
        let num_glyphs = glyph_records.len() as u16;
        let mut head = head_table();
        head[50..52].copy_from_slice(&1u16.to_be_bytes()); // long loca

        let mut glyf = Vec::new();
        let mut loca = Vec::with_capacity((glyph_records.len() + 1) * 4);
        for rec in glyph_records {
            loca.extend_from_slice(&(glyf.len() as u32).to_be_bytes());
            glyf.extend_from_slice(rec);
            while !glyf.len().is_multiple_of(4) {
                glyf.push(0);
            }
        }
        loca.extend_from_slice(&(glyf.len() as u32).to_be_bytes());

        let mut tables: Vec<([u8; 4], Vec<u8>)> = Vec::new();
        if !cmap_entries.is_empty() || mac_roman {
            // Build cmap with optional (1,0) MacRoman format 0 and (3,1) Unicode format 4.
            let mut subtables: Vec<(u16, u16, Vec<u8>)> = Vec::new();
            if mac_roman {
                let mut f0 = vec![0u8; 262];
                f0[0..2].copy_from_slice(&0u16.to_be_bytes());
                f0[2..4].copy_from_slice(&262u16.to_be_bytes());
                for &(cp, gid) in cmap_entries {
                    if cp < 256 {
                        f0[6 + cp as usize] = gid as u8;
                    }
                }
                subtables.push((1, 0, f0));
            }
            if !cmap_entries.is_empty() {
                let mut sorted = cmap_entries.to_vec();
                sorted.sort_by_key(|&(cp, _)| cp);
                let seg_count = sorted.len() + 1;
                let seg_count_x2 = (seg_count * 2) as u16;
                let mut f4 = Vec::new();
                f4.extend_from_slice(&4u16.to_be_bytes()); // format
                let len_pos = f4.len();
                f4.extend_from_slice(&0u16.to_be_bytes()); // length placeholder
                f4.extend_from_slice(&0u16.to_be_bytes()); // language
                f4.extend_from_slice(&seg_count_x2.to_be_bytes());
                let search_range = 2 * (1u16 << (seg_count as u16).ilog2());
                f4.extend_from_slice(&search_range.to_be_bytes());
                f4.extend_from_slice(&((seg_count as u16).ilog2() as u16).to_be_bytes());
                f4.extend_from_slice(&(seg_count_x2 - search_range).to_be_bytes());
                for &(cp, _) in &sorted {
                    f4.extend_from_slice(&cp.to_be_bytes());
                }
                f4.extend_from_slice(&0xFFFFu16.to_be_bytes());
                f4.extend_from_slice(&0u16.to_be_bytes()); // reservedPad
                for &(cp, _) in &sorted {
                    f4.extend_from_slice(&cp.to_be_bytes());
                }
                f4.extend_from_slice(&0xFFFFu16.to_be_bytes());
                for &(cp, gid) in &sorted {
                    let delta = gid.wrapping_sub(cp) as i16;
                    f4.extend_from_slice(&delta.to_be_bytes());
                }
                f4.extend_from_slice(&1i16.to_be_bytes());
                for _ in 0..seg_count {
                    f4.extend_from_slice(&0u16.to_be_bytes());
                }
                let f4_len = f4.len() as u16;
                f4[len_pos..len_pos + 2].copy_from_slice(&f4_len.to_be_bytes());
                subtables.push((3, 1, f4));
            }
            let header_len = 4 + subtables.len() * 8;
            let mut cmap = Vec::new();
            cmap.extend_from_slice(&0u16.to_be_bytes());
            cmap.extend_from_slice(&(subtables.len() as u16).to_be_bytes());
            let mut off = header_len as u32;
            for (pid, eid, data) in &subtables {
                cmap.extend_from_slice(&pid.to_be_bytes());
                cmap.extend_from_slice(&eid.to_be_bytes());
                cmap.extend_from_slice(&off.to_be_bytes());
                off += data.len() as u32;
            }
            for (_, _, data) in subtables {
                cmap.extend_from_slice(&data);
            }
            tables.push((*b"cmap", cmap));
        }
        tables.push((*b"glyf", glyf));
        tables.push((*b"head", head));
        tables.push((*b"hhea", hhea_table()));
        tables.push((*b"loca", loca));
        tables.push((*b"maxp", maxp_table(num_glyphs)));
        build_sfnt(&tables)
    }

    fn simple_rect_glyf() -> Vec<u8> {
        let mut g = Vec::new();
        g.extend_from_slice(&1i16.to_be_bytes()); // numberOfContours = 1
        g.extend_from_slice(&0i16.to_be_bytes()); // xMin
        g.extend_from_slice(&0i16.to_be_bytes()); // yMin
        g.extend_from_slice(&500i16.to_be_bytes()); // xMax
        g.extend_from_slice(&700i16.to_be_bytes()); // yMax
        g.extend_from_slice(&3u16.to_be_bytes()); // endPtsOfContours[0] = 3
        g.extend_from_slice(&0u16.to_be_bytes()); // instructionLength = 0
        g.extend_from_slice(&[1, 1, 1, 1]); // 4 on-curve points, 16-bit deltas
        for dx in [0i16, 500, 0, -500] {
            g.extend_from_slice(&dx.to_be_bytes());
        }
        for dy in [0i16, 0, 700, 0] {
            g.extend_from_slice(&dy.to_be_bytes());
        }
        g
    }

    #[test]
    fn substitute_consults_tounicode_before_macroman_cmap_slots() {
        let sfnt = build_glyf_sfnt(
            &[vec![], simple_rect_glyf(), simple_rect_glyf()],
            &[(0x41, 1), (0x42, 2)],
            true,
        );
        let mut font = LoadedFont::new_substitute(
            PdfFontType::TrueType,
            "SubMac".into(),
            Arc::from(sfnt),
            0,
            CidWidths::new(500.0),
        )
        .expect("substitute");
        font.symbolic = true;
        font.encoding = None;
        font.to_unicode = Some(cmap::ToUnicodeMap::parse(
            b"1 beginbfchar <41> <0042> endbfchar",
        ));

        // 0x41 maps to 'B' (GID 2) via /ToUnicode, NOT 'A' (GID 1) via MacRoman cmap.
        assert_eq!(font.code_to_gid(0x41), Some(2));
        // Codes absent from /ToUnicode still fall back to raw cmap slots.
        assert_eq!(font.code_to_gid(0x42), Some(2));
    }

    #[test]
    fn zero_contour_space_glyph_is_usable_while_zeroed_visible_glyf_is_rejected() {
        // Valid 10-byte zero-contour header (numberOfContours = 0, bbox = 0).
        let zero_contour_10 = vec![0u8; 10];

        // 1. Space-only /CIDToGIDMap mapping GID 1 with a 10-byte zero-contour header.
        let space_cid_sfnt = build_glyf_sfnt(&[vec![], zero_contour_10.clone()], &[], false);
        let mut cid_font = LoadedFont::new_with_data(
            PdfFontType::Type0CidType2,
            "SpaceOnlyCID".into(),
            space_cid_sfnt,
            CidWidths::new(500.0),
        );
        cid_font.cid_to_gid = Some(HashMap::from([(1u16, 1u16)]));
        assert!(
            cid_font.has_usable_embedded_outlines(),
            "valid 10-byte zero-contour space glyph must be accepted"
        );

        // 2. Simple space-only subset mapping U+0020 -> GID 1 (10-byte zero-contour header).
        let space_simple_sfnt =
            build_glyf_sfnt(&[vec![], zero_contour_10.clone()], &[(0x20, 1)], false);
        let space_simple = LoadedFont::new_with_data(
            PdfFontType::TrueType,
            "SpaceOnlySimple".into(),
            space_simple_sfnt,
            CidWidths::new(500.0),
        );
        assert!(space_simple.has_usable_embedded_outlines());

        // 3. Zero-contour 10-byte header mapped to visible 'A' in cmap -> rejected.
        let hidden_a_sfnt = build_glyf_sfnt(&[vec![], zero_contour_10], &[(0x41, 1)], false);
        let hidden_a = LoadedFont::new_with_data(
            PdfFontType::TrueType,
            "HiddenVisibleA".into(),
            hidden_a_sfnt,
            CidWidths::new(500.0),
        );
        assert!(!hidden_a.has_usable_embedded_outlines());

        // 4. Wiped 32-byte all-zero glyf payload -> rejected even without cmap.
        let wiped_sfnt = build_glyf_sfnt(&[vec![], vec![0u8; 32]], &[], false);
        let mut wiped = LoadedFont::new_with_data(
            PdfFontType::Type0CidType2,
            "WipedGlyf".into(),
            wiped_sfnt,
            CidWidths::new(500.0),
        );
        wiped.cid_to_gid = Some(HashMap::from([(1u16, 1u16)]));
        assert!(!wiped.has_usable_embedded_outlines());

        // 5. Valid zero-contour space with phantom-point hinting instructions
        //    (10-byte header + instructionLength=6 + 6 instruction bytes,
        //    4-byte aligned to 20 bytes) -> accepted.
        let mut hinted_space = vec![0u8; 10];
        hinted_space.extend_from_slice(&6u16.to_be_bytes());
        hinted_space.extend_from_slice(&[0xB0, 0x00, 0xB0, 0x01, 0x20, 0x00]);
        let hinted_sfnt = build_glyf_sfnt(&[vec![], hinted_space], &[], false);
        let mut hinted = LoadedFont::new_with_data(
            PdfFontType::Type0CidType2,
            "HintedSpaceCID".into(),
            hinted_sfnt,
            CidWidths::new(500.0),
        );
        hinted.cid_to_gid = Some(HashMap::from([(1u16, 1u16)]));
        assert!(
            hinted.has_usable_embedded_outlines(),
            "zero-contour space glyph with valid hinting instructions must be accepted"
        );

        // 6. Zero-contour glyph with truncated instructionLength past slice bounds -> rejected.
        let mut truncated_instr = vec![0u8; 10];
        truncated_instr.extend_from_slice(&40u16.to_be_bytes());
        truncated_instr.extend_from_slice(&[0xB0, 0x00, 0x00, 0x00]);
        let truncated_sfnt = build_glyf_sfnt(&[vec![], truncated_instr], &[], false);
        let mut truncated = LoadedFont::new_with_data(
            PdfFontType::Type0CidType2,
            "TruncatedInstrCID".into(),
            truncated_sfnt,
            CidWidths::new(500.0),
        );
        truncated.cid_to_gid = Some(HashMap::from([(1u16, 1u16)]));
        assert!(!truncated.has_usable_embedded_outlines());
    }

    #[test]
    fn embedded_outline_failure_rate_excludes_valid_blank_glyphs() {
        // 1 drawn glyph (GID 1) + 2 valid zero-length loca blank glyphs (GIDs 2, 3).
        let sfnt = build_glyf_sfnt(&[vec![], simple_rect_glyf(), vec![], vec![]], &[], false);
        let mut font = LoadedFont::new_with_data(
            PdfFontType::Type0CidType2,
            "ValidCJKWithBlanks".into(),
            sfnt,
            CidWidths::new(1000.0),
        );
        font.cid_to_gid = Some(HashMap::from([(1, 1), (2, 2), (3, 3)]));
        assert!(font.has_usable_embedded_outlines());
        assert_eq!(font.embedded_outline_failure_rate(), 0.0);
    }

    #[test]
    fn cmap_blank_probe_is_range_aware_and_strictly_bounded() {
        let build_with_cmap_groups = |format: u16, groups: &[(u32, u32, u32)]| -> Vec<u8> {
            let mut head = head_table();
            head[50..52].copy_from_slice(&1u16.to_be_bytes()); // long loca
            let zero_contour = vec![0u8; 10];
            let mut glyf = Vec::new();
            let mut loca = Vec::new();
            // GID 0 = empty, GID 1 = 10-byte zero-contour header
            loca.extend_from_slice(&0u32.to_be_bytes());
            loca.extend_from_slice(&0u32.to_be_bytes());
            glyf.extend_from_slice(&zero_contour);
            glyf.extend_from_slice(&[0, 0]); // 4-byte align
            loca.extend_from_slice(&(glyf.len() as u32).to_be_bytes());

            let subtable_len = (16 + groups.len() * 12) as u32;
            let mut subtable = Vec::with_capacity(subtable_len as usize);
            subtable.extend_from_slice(&format.to_be_bytes());
            subtable.extend_from_slice(&0u16.to_be_bytes()); // reserved
            subtable.extend_from_slice(&subtable_len.to_be_bytes());
            subtable.extend_from_slice(&0u32.to_be_bytes()); // language
            subtable.extend_from_slice(&(groups.len() as u32).to_be_bytes());
            for &(s, e, g) in groups {
                subtable.extend_from_slice(&s.to_be_bytes());
                subtable.extend_from_slice(&e.to_be_bytes());
                subtable.extend_from_slice(&g.to_be_bytes());
            }
            let mut cmap = Vec::new();
            cmap.extend_from_slice(&0u16.to_be_bytes()); // version
            cmap.extend_from_slice(&1u16.to_be_bytes()); // numTables
            cmap.extend_from_slice(&3u16.to_be_bytes()); // Windows
            cmap.extend_from_slice(&10u16.to_be_bytes()); // Unicode full repertoire
            cmap.extend_from_slice(&12u32.to_be_bytes()); // offset
            cmap.extend_from_slice(&subtable);

            build_sfnt(&[
                (*b"cmap", cmap),
                (*b"glyf", glyf),
                (*b"head", head),
                (*b"hhea", hhea_table()),
                (*b"loca", loca),
                (*b"maxp", maxp_table(2)),
            ])
        };

        // 1. Adversarial Format 12 spanning the full u32 space: 0x41 -> GID 1
        //    must finish in O(1) without expanding 4.29B codepoints, and reject.
        let huge_f12 = build_with_cmap_groups(12, &[(0x41, u32::MAX, 1)]);
        let font_f12 = LoadedFont::new_with_data(
            PdfFontType::TrueType,
            "HugeFormat12".into(),
            huge_f12,
            CidWidths::new(500.0),
        );
        assert!(!font_f12.has_usable_embedded_outlines());

        // 2. Adversarial Format 13 mapping [0..=u32::MAX] -> GID 1 must finish
        //    in O(1) and detect visible scalars in the range.
        let huge_f13 = build_with_cmap_groups(13, &[(0, u32::MAX, 1)]);
        let font_f13 = LoadedFont::new_with_data(
            PdfFontType::TrueType,
            "HugeFormat13".into(),
            huge_f13,
            CidWidths::new(500.0),
        );
        assert!(!font_f13.has_usable_embedded_outlines());

        // 3. Adversarial Format 13 covering only surrogates/PUA (non-visible)
        //    leaves the zero-contour blank glyph accepted in O(1).
        let pua_f13 = build_with_cmap_groups(13, &[(0xD800, 0xF8FF, 1)]);
        let font_pua = LoadedFont::new_with_data(
            PdfFontType::TrueType,
            "PuaFormat13".into(),
            pua_f13,
            CidWidths::new(500.0),
        );
        assert!(font_pua.has_usable_embedded_outlines());

        // 4. Oversized group count past CMAP_PROBE_BUDGET stops early and
        //    leaves the probe inconclusive rather than stalling font load.
        let mut many_groups: Vec<(u32, u32, u32)> = (0..resolution::CMAP_PROBE_BUDGET as u32 + 128)
            .map(|i| (0x1000 + i, 0x1000 + i, 2))
            .collect();
        many_groups.push((0x0041, 0x0041, 1));
        let huge_groups = build_with_cmap_groups(12, &many_groups);
        let font_budget = LoadedFont::new_with_data(
            PdfFontType::TrueType,
            "BudgetExhaustedCmap".into(),
            huge_groups,
            CidWidths::new(500.0),
        );
        assert!(
            font_budget.has_usable_embedded_outlines(),
            "exhausted cmap probe budget must leave the blank check inconclusive"
        );
    }

    #[test]
    fn embedded_companion_is_charged_against_page_budget_on_retention() {
        let primary_bytes = include_bytes!("../tests/fixtures/coverage-primary.ttf");
        let primary = Arc::new(
            LoadedFont::new_substitute(
                PdfFontType::TrueType,
                "PrimarySub".into(),
                Arc::from(primary_bytes.as_slice()),
                0,
                CidWidths::new(1000.0),
            )
            .unwrap(),
        );
        let companion = Arc::new(LoadedFont::new_with_data(
            PdfFontType::TrueType,
            "EmbeddedOriginal".into(),
            primary_bytes.to_vec(),
            CidWidths::new(1000.0),
        ));
        let primary_cost = primary.estimated_cache_bytes();
        let companion_cost = companion.estimated_cache_bytes();
        let combined_cost = primary_cost + companion_cost;

        // 1. Budget fits primary, rejects unused companion: no strong Arc retained.
        let mut tight = FontCache::new();
        let fid = tight
            .try_insert_shared_with_limit("F1".into(), Arc::clone(&primary), combined_cost - 1)
            .expect("primary fits");
        let weak_font = Arc::downgrade(&companion);
        let weak_data = Arc::downgrade(companion.font_data.as_ref().unwrap());
        assert!(!tight.record_embedded_companion(fid, Arc::clone(&companion), combined_cost - 1));
        assert!(!tight.has_embedded_companion(fid));
        assert_eq!(tight.bytes_used(), primary_cost);
        drop(companion);
        assert!(
            weak_font.upgrade().is_none() && weak_data.upgrade().is_none(),
            "rejected companion must not be retained by FontCache"
        );

        // 2. Budget fits both: companion is charged immediately upon retention,
        //    and lazy drawing admission does not double-charge it.
        let companion = Arc::new(LoadedFont::new_with_data(
            PdfFontType::TrueType,
            "EmbeddedOriginal".into(),
            primary_bytes.to_vec(),
            CidWidths::new(1000.0),
        ));
        let mut ample = FontCache::new();
        let fid = ample
            .try_insert_shared_with_limit("F1".into(), primary, combined_cost)
            .expect("primary fits");
        assert!(ample.record_embedded_companion(fid, companion, combined_cost));
        assert!(ample.has_embedded_companion(fid));
        assert_eq!(ample.bytes_used(), combined_cost);

        let resolved = ample
            .resolve_embedded_companion_glyph(
                DrawingGlyph {
                    font_id: fid,
                    glyph_id: 0,
                },
                b'A' as u32,
                b'A' as u32,
                1,
                combined_cost,
            )
            .expect("uncovered glyph resolves from retained companion");
        assert_ne!(resolved.font_id, fid);
        assert_eq!(
            ample.bytes_used(),
            combined_cost,
            "drawing admission must not double-charge an already-budgeted companion"
        );
    }

    #[test]
    fn deferred_companion_remembers_budget_failures_without_poisoning_glyph_misses() {
        let primary_bytes = include_bytes!("../tests/fixtures/coverage-primary.ttf");
        let primary = Arc::new(
            LoadedFont::new_substitute(
                PdfFontType::TrueType,
                "PrimarySub".into(),
                Arc::from(primary_bytes.as_slice()),
                0,
                CidWidths::new(1000.0),
            )
            .unwrap(),
        );
        let companion = Arc::new(LoadedFont::new_with_data(
            PdfFontType::TrueType,
            "EmbeddedOriginal".into(),
            primary_bytes.to_vec(),
            CidWidths::new(1000.0),
        ));
        let primary_cost = primary.estimated_cache_bytes();
        let companion_cost = companion.estimated_cache_bytes();
        let tight = primary_cost + companion_cost / 2;
        let ample = primary_cost + companion_cost;

        // 1. Definitive FailedForBudget: never retried at same or smaller budget
        //    across distinct missing glyphs, but retries if budget is raised.
        let mut cache = FontCache::new();
        let fid = cache
            .try_insert_shared_with_limit("F1".into(), Arc::clone(&primary), tight)
            .unwrap();
        cache.record_deferred_companion_ref(fid, zpdf_core::ObjectId(10, 0));
        let mut load_calls = 0usize;
        for ch in *b"ABC" {
            let res = cache.resolve_embedded_companion_glyph_with(
                DrawingGlyph {
                    font_id: fid,
                    glyph_id: 0,
                },
                ch as u32,
                ch as u32,
                1,
                tight,
                |_, _| {
                    load_calls += 1;
                    DeferredCompanionLoad::FailedForBudget
                },
            );
            assert!(res.is_none());
        }
        assert_eq!(
            load_calls, 1,
            "definitive load/budget failure must not retry subsequent missing glyphs"
        );
        assert!(!cache.has_embedded_companion_for_budget(fid, tight));
        // Smaller budget also short-circuits without calling load_deferred.
        assert!(cache
            .resolve_embedded_companion_glyph_with(
                DrawingGlyph {
                    font_id: fid,
                    glyph_id: 0,
                },
                b'D' as u32,
                b'D' as u32,
                1,
                tight - 1,
                |_, _| {
                    load_calls += 1;
                    DeferredCompanionLoad::FailedForBudget
                },
            )
            .is_none());
        assert_eq!(load_calls, 1);
        // Raising max_bytes past the failed watermark allows a retry that succeeds.
        assert!(cache.has_embedded_companion_for_budget(fid, ample));
        let resolved = cache.resolve_embedded_companion_glyph_with(
            DrawingGlyph {
                font_id: fid,
                glyph_id: 0,
            },
            b'A' as u32,
            b'A' as u32,
            1,
            ample,
            |_, _| {
                load_calls += 1;
                DeferredCompanionLoad::Loaded(Arc::clone(&companion))
            },
        );
        assert!(resolved.is_some());
        assert_eq!(load_calls, 2);

        // 2. Post-load admission failure (Loaded exceeds max_bytes) also records
        //    the failed watermark and stops retrying at the same budget.
        let mut cache_admit = FontCache::new();
        let fid_admit = cache_admit
            .try_insert_shared_with_limit("F1".into(), Arc::clone(&primary), ample - 1)
            .unwrap();
        cache_admit.record_deferred_companion_ref(fid_admit, zpdf_core::ObjectId(11, 0));
        let mut admit_calls = 0usize;
        for ch in *b"AB" {
            assert!(cache_admit
                .resolve_embedded_companion_glyph_with(
                    DrawingGlyph {
                        font_id: fid_admit,
                        glyph_id: 0,
                    },
                    ch as u32,
                    ch as u32,
                    1,
                    ample - 1,
                    |_, _| {
                        admit_calls += 1;
                        DeferredCompanionLoad::Loaded(Arc::clone(&companion))
                    },
                )
                .is_none());
        }
        assert_eq!(admit_calls, 1);

        // 3. Glyph-specific miss and transient Unavailable do NOT poison the budget.
        let mut cache_miss = FontCache::new();
        let fid_miss = cache_miss
            .try_insert_shared_with_limit("F1".into(), primary, ample)
            .unwrap();
        cache_miss.record_deferred_companion_ref(fid_miss, zpdf_core::ObjectId(12, 0));
        // Transient Unavailable leaves companion retryable at the same budget.
        assert!(cache_miss
            .resolve_embedded_companion_glyph(
                DrawingGlyph {
                    font_id: fid_miss,
                    glyph_id: 0,
                },
                b'A' as u32,
                b'A' as u32,
                1,
                ample,
            )
            .is_none());
        assert!(cache_miss.has_embedded_companion_for_budget(fid_miss, ample));
        // Glyph-specific miss (code 0x1F600 absent from coverage-primary.ttf)
        // must not admit the companion OR poison subsequent glyphs at the same
        // budget, AND repeated misses for the same code must hit the bounded
        // page-local miss cache without reloading the deferred companion again.
        let mut miss_load_calls = 0usize;
        for _ in 0..5 {
            assert!(cache_miss
                .resolve_embedded_companion_glyph_with(
                    DrawingGlyph {
                        font_id: fid_miss,
                        glyph_id: 0,
                    },
                    0x1F600,
                    0x1F600,
                    2,
                    ample,
                    |_, _| {
                        miss_load_calls += 1;
                        DeferredCompanionLoad::Loaded(Arc::clone(&companion))
                    },
                )
                .is_none());
        }
        assert_eq!(
            miss_load_calls, 1,
            "repeated misses for the same code must not reload the deferred companion"
        );
        assert!(!cache_miss.has_retained_embedded_companion(fid_miss));
        assert_eq!(cache_miss.bytes_used(), primary_cost);
        assert!(cache_miss.has_embedded_companion_for_budget(fid_miss, ample));
        // Subsequent drawable glyph 'A' still succeeds and admits the companion.
        let ok = cache_miss.resolve_embedded_companion_glyph_with(
            DrawingGlyph {
                font_id: fid_miss,
                glyph_id: 0,
            },
            b'A' as u32,
            b'A' as u32,
            1,
            ample,
            |_, _| {
                miss_load_calls += 1;
                DeferredCompanionLoad::Loaded(Arc::clone(&companion))
            },
        );
        assert!(ok.is_some());
        assert_eq!(miss_load_calls, 2);
        assert!(cache_miss.has_retained_embedded_companion(fid_miss));
        assert_eq!(cache_miss.bytes_used(), ample);
    }

    #[test]
    fn descriptor_substitute_reuses_glyph_classification_and_run_candidate_across_misses() {
        let primary_bytes = include_bytes!("../tests/fixtures/coverage-primary.ttf");
        let bold_bytes = include_bytes!("../tests/fixtures/coverage-bold.ttf");
        // BOLD_TTF contains only 'A' (not 'B' or 'C').
        let provider = Arc::new(FontProvider::memory_only());
        provider
            .register_with_aliases(bold_bytes.as_slice(), 0, &["ReuseRunCandidate"])
            .unwrap();

        let mut primary = LoadedFont::new_with_data(
            PdfFontType::TrueType,
            "ABCDEF+ReuseRunCandidate".into(),
            primary_bytes.to_vec(),
            CidWidths::new(1000.0),
        );
        primary.encoding = Some(encoding::Encoding::win_ansi());
        let gid_a = primary.code_to_gid(b'A' as u16).unwrap();
        let gid_b = primary.code_to_gid(b'B' as u16).unwrap();
        let gid_c = primary.code_to_gid(b'C' as u16).unwrap();
        let primary = Arc::new(primary);

        let mut cache = FontCache::new().with_provider(provider);
        let max_bytes = 4 * 1024 * 1024;
        let fid = cache
            .try_insert_shared_with_limit("F1".into(), primary, max_bytes)
            .unwrap();
        cache.record_descriptor_substitute(
            fid,
            Arc::new(DescriptorSubstituteSpec {
                lookup_name: None,
                hints: system::SubstituteHints::default(),
                ordering: None,
                font_file_id: None,
            }),
            max_bytes,
        );

        let mut run_candidate: Option<Arc<LoadedFont>> = None;

        // 1. Passing cached `original_unreadable = Some(false)` short-circuits
        //    before building or admitting any substitute.
        assert!(cache
            .resolve_embedded_companion_glyph_in_run(
                DrawingGlyph {
                    font_id: fid,
                    glyph_id: gid_a,
                },
                b'A' as u32,
                b'A' as u32,
                1,
                max_bytes,
                Some(false),
                None,
                Some(&mut run_candidate),
                |_, _| DeferredCompanionLoad::Unavailable,
            )
            .is_none());
        assert!(run_candidate.is_none());
        assert!(!cache.has_admitted_embedded_companion(fid));

        // 2. First distinct miss ('B', absent from BOLD_TTF) builds the
        //    descriptor substitute once into `run_candidate` without admitting.
        assert!(cache
            .resolve_embedded_companion_glyph_in_run(
                DrawingGlyph {
                    font_id: fid,
                    glyph_id: gid_b,
                },
                b'B' as u32,
                b'B' as u32,
                1,
                max_bytes,
                Some(true),
                None,
                Some(&mut run_candidate),
                |_, _| DeferredCompanionLoad::Unavailable,
            )
            .is_none());
        assert!(!cache.has_admitted_embedded_companion(fid));
        let candidate_ptr = Arc::as_ptr(
            run_candidate
                .as_ref()
                .expect("descriptor substitute cached for current run"),
        );

        // 3. Second distinct miss ('C', also absent from BOLD_TTF) reuses the
        //    exact same `Arc<LoadedFont>` without rebuilding or admitting.
        assert!(cache
            .resolve_embedded_companion_glyph_in_run(
                DrawingGlyph {
                    font_id: fid,
                    glyph_id: gid_c,
                },
                b'C' as u32,
                b'C' as u32,
                1,
                max_bytes,
                Some(true),
                None,
                Some(&mut run_candidate),
                |_, _| DeferredCompanionLoad::Unavailable,
            )
            .is_none());
        assert!(!cache.has_admitted_embedded_companion(fid));
        assert_eq!(
            Arc::as_ptr(run_candidate.as_ref().unwrap()),
            candidate_ptr,
            "second distinct miss must reuse the same constructed substitute"
        );

        // 4. First covered glyph ('A') permanently admits that exact candidate.
        let drawn = cache
            .resolve_embedded_companion_glyph_in_run(
                DrawingGlyph {
                    font_id: fid,
                    glyph_id: gid_a,
                },
                b'A' as u32,
                b'A' as u32,
                1,
                max_bytes,
                Some(true),
                None,
                Some(&mut run_candidate),
                |_, _| DeferredCompanionLoad::Unavailable,
            )
            .expect("covered glyph 'A' must draw from reused substitute");
        assert!(cache.has_admitted_embedded_companion(fid));
        assert_eq!(
            cache.get(drawn.font_id).unwrap() as *const LoadedFont,
            candidate_ptr,
            "admitted drawing font must be the reused run candidate"
        );
    }

    #[test]
    fn descriptor_budget_watermark_resets_when_shared_program_set_changes() {
        let primary_bytes = include_bytes!("../tests/fixtures/coverage-primary.ttf");
        // Register PRIMARY_TTF (~18 KB) under two distinct names sharing one Arc<[u8]>.
        let provider = Arc::new(FontProvider::memory_only());
        provider
            .register_with_aliases(primary_bytes.as_slice(), 0, &["WatermarkF1", "WatermarkF2"])
            .unwrap();

        let make_primary = |name: &str| {
            let mut f = LoadedFont::new_with_data(
                PdfFontType::TrueType,
                name.into(),
                primary_bytes.to_vec(),
                CidWidths::new(1000.0),
            );
            f.encoding = Some(encoding::Encoding::win_ansi());
            Arc::new(f)
        };
        let f1 = make_primary("ABCDEF+WatermarkF1");
        let f2 = make_primary("GHIJKL+WatermarkF2");
        let gid_a = f1.code_to_gid(b'A' as u16).unwrap();

        // Measure the exact full and incremental (shared-program) costs of a
        // descriptor substitute for f1/f2.
        let mut probe_cache = FontCache::new().with_provider(Arc::clone(&provider));
        let pf1 = probe_cache
            .try_insert_shared_with_limit("F1".into(), Arc::clone(&f1), u64::MAX)
            .unwrap();
        probe_cache.record_descriptor_substitute(
            pf1,
            Arc::new(DescriptorSubstituteSpec {
                lookup_name: None,
                hints: system::SubstituteHints::default(),
                ordering: None,
                font_file_id: None,
            }),
            u64::MAX,
        );
        let sub_drawn = probe_cache
            .resolve_embedded_companion_glyph_in_run(
                DrawingGlyph {
                    font_id: pf1,
                    glyph_id: gid_a,
                },
                b'A' as u32,
                b'A' as u32,
                1,
                u64::MAX,
                Some(true),
                None,
                None,
                |_, _| DeferredCompanionLoad::Unavailable,
            )
            .unwrap();
        let sub_full_cost = probe_cache
            .get(sub_drawn.font_id)
            .unwrap()
            .estimated_cache_bytes();
        let sub_owned_cost = sub_full_cost - primary_bytes.len() as u64;
        assert!(sub_owned_cost > 0 && sub_owned_cost + 4096 < sub_full_cost);

        let mut cache = FontCache::new().with_provider(provider);
        let fid1 = cache
            .try_insert_shared_with_limit("F1".into(), f1, u64::MAX)
            .unwrap();
        let fid2 = cache
            .try_insert_shared_with_limit("F2".into(), f2, u64::MAX)
            .unwrap();
        for fid in [fid1, fid2] {
            cache.record_descriptor_substitute(
                fid,
                Arc::new(DescriptorSubstituteSpec {
                    lookup_name: None,
                    hints: system::SubstituteHints::default(),
                    ordering: None,
                    font_file_id: None,
                }),
                u64::MAX,
            );
        }
        let base_used = cache.bytes_used();

        // 1. F1 needs `sub_full_cost` (program not yet admitted), fails with
        //    `sub_owned_cost + 2048` free (< `sub_full_cost`).
        let lim1 = base_used + sub_owned_cost + 2048;
        assert!(cache
            .resolve_embedded_companion_glyph_in_run(
                DrawingGlyph {
                    font_id: fid1,
                    glyph_id: gid_a,
                },
                b'A' as u32,
                b'A' as u32,
                1,
                lim1,
                Some(true),
                None,
                None,
                |_, _| DeferredCompanionLoad::Unavailable,
            )
            .is_none());

        // 2. F2 admits the shared PRIMARY_TTF program (program count grows!).
        assert!(cache
            .resolve_embedded_companion_glyph_in_run(
                DrawingGlyph {
                    font_id: fid2,
                    glyph_id: gid_a,
                },
                b'A' as u32,
                b'A' as u32,
                1,
                u64::MAX,
                Some(true),
                None,
                None,
                |_, _| DeferredCompanionLoad::Unavailable,
            )
            .is_some());
        let after_f2 = cache.bytes_used();

        // 3. F1 now needs only `sub_owned_cost`, fails at `sub_owned_cost - 1`
        //    free. Because `program_count` changed, the watermark must reset to
        //    `sub_owned_cost - 1` instead of retaining the older larger value!
        assert!(cache
            .resolve_embedded_companion_glyph_in_run(
                DrawingGlyph {
                    font_id: fid1,
                    glyph_id: gid_a,
                },
                b'A' as u32,
                b'A' as u32,
                1,
                after_f2 + sub_owned_cost - 1,
                Some(true),
                None,
                None,
                |_, _| DeferredCompanionLoad::Unavailable,
            )
            .is_none());

        // 4. Raising the budget to `after_f2 + sub_owned_cost` (still LESS free
        //    space than step 1's `sub_owned_cost + 2048`!) MUST now succeed!
        assert!(
            cache.has_descriptor_substitute_for_budget(fid1, after_f2 + sub_owned_cost),
            "reset watermark must allow retry once incremental cost fits"
        );
        assert!(
            cache
                .resolve_embedded_companion_glyph_in_run(
                    DrawingGlyph {
                        font_id: fid1,
                        glyph_id: gid_a,
                    },
                    b'A' as u32,
                    b'A' as u32,
                    1,
                    after_f2 + sub_owned_cost,
                    Some(true),
                    None,
                    None,
                    |_, _| DeferredCompanionLoad::Unavailable,
                )
                .is_some(),
            "F1 must admit at incremental `sub_owned_cost` after program-count watermark reset"
        );
    }
}
