pub mod big5;
pub mod cmap;
pub mod encoding;
pub mod eucjp;
pub mod gb2312;
pub mod gbk;
pub mod glyph_list;
pub mod ksc;
pub mod sjis;
pub mod standard_fonts;
pub mod system;
pub mod type1;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub type FontId = u32;

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
            v_metrics: HashMap::new(),
        }
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
    /// Retained font-program bytes used for cache admission accounting.
    pub fn estimated_cache_bytes(&self) -> u64 {
        if let Some(data) = &self.font_data {
            return data.len() as u64;
        }
        if let Some(font) = &self.type1 {
            return font.estimated_program_bytes();
        }
        match &self.font_type {
            PdfFontType::Type3 { char_procs, .. } => {
                char_procs.values().map(|stream| stream.len() as u64).sum()
            }
            _ => 0,
        }
    }

    fn shared_program_key(&self) -> Option<usize> {
        self.font_data.as_ref().map(|data| data.as_ptr() as usize)
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
        } else if let Some(t1) = type1::Type1Font::parse(&font_data) {
            // Embedded Type 1 (PostScript) program — ttf-parser cannot parse it.
            let units_per_em = t1.units_per_em;
            Self {
                font_type,
                base_font,
                font_data: None,
                face_index: 0,
                is_substitute: false,
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
        if !self.is_substitute || !matches!(self.font_type, PdfFontType::Type0CidType2) {
            return;
        }
        // Unicode-coded CMaps resolve straight to GIDs — no CID table needed.
        if self.unicode_coded() {
            return;
        }
        let Some(tu) = &self.to_unicode else { return };
        let Some(data) = &self.font_data else { return };
        let Ok(face) = ttf_parser::Face::parse(data, self.face_index) else {
            return;
        };
        let mut map = HashMap::new();
        for (code, s) in tu.iter() {
            let Some(ch) = s.chars().next() else { continue };
            // glyph_outline receives CIDs, so key the map by CID — under a
            // non-Identity encoding CMap the code is not the CID.
            let cid = match &self.cid_cmap {
                Some(cm) if !cm.identity => {
                    let len = if code > 0xFF { 2 } else { 1 };
                    let cid = cm.code_to_cid(code, len);
                    if cid == 0 && len == 1 {
                        cm.code_to_cid(code, 2)
                    } else {
                        cid
                    }
                }
                _ => code,
            };
            if cid == 0 || cid > u16::MAX as u32 {
                continue;
            }
            if let Some(g) = face.glyph_index(ch) {
                if g.0 != 0 {
                    map.insert(cid as u16, g.0);
                }
            }
        }
        if map.is_empty() {
            tracing::debug!(
                "substitute {}: no ToUnicode-derived CID mapping; composite glyphs will drop",
                self.base_font
            );
        } else {
            self.cid_to_gid = Some(map);
        }
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

    /// True when the composite-font code values are Unicode and glyph ids in
    /// glyph runs are already real GIDs (no CID → GID mapping applies).
    fn unicode_coded(&self) -> bool {
        self.cid_cmap
            .as_ref()
            .map(|c| c.codes_are_unicode)
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

    /// Downgrade a Unicode-coded CMap to Identity when the font program has
    /// no Unicode cmap table to resolve against (e.g. an embedded CID-keyed
    /// CFF) — codes then pass through as CIDs, which the charset or
    /// /CIDToGIDMap path can still map, instead of every glyph landing on
    /// .notdef. Call after `cid_cmap` is attached.
    pub fn validate_cid_cmap(&mut self) {
        let Some(cm) = &self.cid_cmap else { return };
        if !cm.codes_are_unicode {
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
        let data = self.font_data.as_ref()?;
        let mut face = ttf_parser::Face::parse(data, self.face_index).ok()?;
        self.apply_variations(&mut face);
        let ch = char::from_u32(code)?;
        let gid = face.glyph_index(ch)?;
        let adv = face
            .glyph_hor_advance(gid)
            .map(|a| a as f64 * 1000.0 / self.units_per_em)
            .unwrap_or(500.0);
        Some((gid.0, adv))
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

        let actual_gid = if self.unicode_coded() || self.legacy_substitute() {
            // Unicode-coded (and substituted legacy-CMap) composite fonts
            // already carry real GIDs.
            glyph_id
        } else if let Some(map) = &self.cid_to_gid {
            *map.get(&glyph_id)?
        } else {
            glyph_id
        };
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
        // exercised, not just one. A glyph with no outline at all (e.g. space)
        // returns None too, but at the >0.5 trip point a handful cannot matter.
        cids.sort_unstable();
        let step = (cids.len() / 64).max(1);
        let (mut total, mut fail) = (0u32, 0u32);
        let mut i = 0;
        while i < cids.len() {
            let gid = ttf_parser::GlyphId(map[&cids[i]]);
            let mut builder = OutlineBuilder::new();
            total += 1;
            if face.outline_glyph(gid, &mut builder).is_none() {
                fail += 1;
            }
            i += step;
        }
        fail as f32 / total as f32
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

    /// Map a 1-byte character code to a glyph ID for a *simple* font, using the
    /// font's /Encoding (→ glyph name) and the embedded program's cmap/charset.
    /// Returns `None` if no glyph data is available or no mapping is found
    /// (callers typically fall back to treating the code as the GID).
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

        // 2. The embedded CFF program's built-in encoding (charcode → GID),
        //    for codes the PDF /Encoding doesn't resolve (symbolic fonts, or
        //    subset fonts whose glyph names the /Encoding tables don't know).
        if let Some(map) = &self.builtin_encoding_gids {
            if let Some(&g) = map.get(&code) {
                return Some(g);
            }
        }

        // 3. Built-in cmap subtables — common for symbolic embedded fonts.
        if let Some(cmap) = face.tables().cmap {
            // (3,0) Windows Symbol: try the 0xF000 PUA offset then the raw code.
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
            // (3,1) Windows Unicode at the 0xF000 PUA offset: symbolic fonts
            // often park their glyphs in the PUA but ship only a Unicode
            // subtable (the raw code is retried as Latin-1 in step 4).
            if self.symbolic {
                for st in cmap.subtables {
                    if st.platform_id == ttf_parser::PlatformId::Windows && st.encoding_id == 1 {
                        if let Some(g) = st.glyph_index(0xF000 | code as u32) {
                            return Some(g.0);
                        }
                    }
                }
            }
            // (1,0) Macintosh Roman: try the raw code, then the MacRoman slot
            // of the /Encoding glyph name — the subtable is MacRoman-indexed,
            // so a PDF encoding that differs from MacRoman needs the name detour.
            for st in cmap.subtables {
                if st.platform_id == ttf_parser::PlatformId::Macintosh && st.encoding_id == 0 {
                    if let Some(g) = st.glyph_index(code as u32) {
                        return Some(g.0);
                    }
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
                }
            }
        }

        // 4. Treat the code as Latin-1 and consult the Unicode cmap.
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
        // PDF /Widths are in 1/1000 glyph-space units; rescale to font units.
        if let Some(w) = self.cid_widths.get_opt(code) {
            return w / 1000.0 * self.units_per_em;
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
                    if let Some(s) = self
                        .to_unicode
                        .as_ref()
                        .and_then(|tu| tu.lookup(raw_code).or_else(|| tu.lookup(code)))
                    {
                        out.push_str(s);
                    } else if cm.codes_are_unicode {
                        if let Some(c) = char::from_u32(code) {
                            out.push(c);
                        }
                    } else if cm.is_legacy() {
                        if let Some(c) = cm
                            .decode_to_unicode(code, len as u8)
                            .and_then(char::from_u32)
                        {
                            out.push(c);
                        }
                    }
                }
                return out;
            }
            let width = match &self.to_unicode {
                Some(tu) => match tu.code_byte_lengths() {
                    [n] if *n >= 1 => *n as usize,
                    _ => 2,
                },
                None => 2,
            };
            for chunk in bytes.chunks(width) {
                let mut code = 0u32;
                for &b in chunk {
                    code = (code << 8) | b as u32;
                }
                if let Some(tu) = &self.to_unicode {
                    if let Some(s) = tu.lookup(code) {
                        out.push_str(s);
                    }
                }
            }
        } else {
            for &b in bytes {
                let code = b as u32;
                if let Some(tu) = &self.to_unicode {
                    if let Some(s) = tu.lookup(code) {
                        out.push_str(s);
                        continue;
                    }
                }
                if let Some(enc) = &self.encoding {
                    if let Some(name) = enc.glyph_name(b) {
                        if let Some(s) = glyph_list::glyph_name_to_string(name) {
                            out.push_str(&s);
                            continue;
                        }
                    }
                }
                if b >= 0x20 && b != 0x7f {
                    out.push(b as char);
                }
            }
        }
        out
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

    /// For Type3 fonts: get glyph width from the Widths array.
    pub fn type3_glyph_width(&self, char_code: u16) -> f64 {
        match &self.font_type {
            PdfFontType::Type3 {
                widths, first_char, ..
            } => {
                let idx = char_code.saturating_sub(*first_char) as usize;
                widths.get(idx).copied().unwrap_or(1000.0)
            }
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

/// Page/display-list font store keyed by [`FontId`].
///
/// Entries are intentionally stable for the lifetime of the store: display
/// lists retain only a `FontId`, so evicting a font would invalidate commands
/// that have already been emitted. The capacity argument is therefore an
/// allocation hint, not an eviction limit.
pub struct FontCache {
    fonts: HashMap<FontId, LoadedFont>,
    name_to_id: HashMap<String, FontId>,
    next_id: FontId,
    bytes_used: u64,
    shared_programs: HashSet<usize>,
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
            bytes_used: 0,
            shared_programs: HashSet::with_capacity(capacity),
        }
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
        self.fonts.get(&id)
    }

    pub fn get_by_name(&self, name: &str) -> Option<(FontId, &LoadedFont)> {
        let id = self.name_to_id.get(name)?;
        let font = self.fonts.get(id)?;
        Some((*id, font))
    }

    pub fn insert(&mut self, name: String, font: LoadedFont) -> FontId {
        self.try_insert_with_limit(name, font, u64::MAX)
            .expect("font id space exhausted")
    }

    /// Admit a font without evicting any existing ID. Shared `Arc` font files
    /// (notably multiple TTC faces) are charged once. Returns `None` when the
    /// retained-program byte limit would be exceeded or no ID remains.
    pub fn try_insert_with_limit(
        &mut self,
        name: String,
        font: LoadedFont,
        max_bytes: u64,
    ) -> Option<FontId> {
        if let Some(&existing_id) = self.name_to_id.get(&name) {
            return Some(existing_id);
        }

        let program_key = font.shared_program_key();
        let added_bytes = if program_key.is_some_and(|key| self.shared_programs.contains(&key)) {
            0
        } else {
            font.estimated_cache_bytes()
        };
        let new_total = self.bytes_used.checked_add(added_bytes)?;
        if new_total > max_bytes {
            return None;
        }

        let id = self.next_free_id()?;
        if let Some(key) = program_key {
            self.shared_programs.insert(key);
        }
        self.bytes_used = new_total;
        self.name_to_id.insert(name, id);
        self.fonts.insert(id, font);
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

    /// Total unique font-program bytes retained by this store.
    pub fn bytes_used(&self) -> u64 {
        self.bytes_used
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
        let cmap = build_cmap(&[(1, 0, cmap_format0(&glyph_ids))]);
        let mut font = font_with_cmap(cmap);

        // Without an /Encoding there is no name to detour through.
        assert_eq!(font.code_to_gid(0x95), None);

        font.encoding = Some(encoding::Encoding::from_base(&encoding::WIN_ANSI_ENCODING));
        assert_eq!(font.code_to_gid(0x95), Some(7));
        // Raw-code hits still win where the PDF encoding agrees with MacRoman.
        assert_eq!(font.code_to_gid(0xA5), Some(7));
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
}
