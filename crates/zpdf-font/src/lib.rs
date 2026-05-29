pub mod cmap;
pub mod encoding;
pub mod glyph_list;
pub mod standard_fonts;

use std::collections::HashMap;
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

/// Per-glyph width from the PDF /W array.
#[derive(Debug, Clone)]
pub struct CidWidths {
    widths: HashMap<u16, f64>,
    default_width: f64,
}

impl CidWidths {
    pub fn new(default_width: f64) -> Self {
        Self {
            widths: HashMap::new(),
            default_width,
        }
    }

    pub fn set(&mut self, cid: u16, width: f64) {
        self.widths.insert(cid, width);
    }

    pub fn get(&self, cid: u16) -> f64 {
        self.widths.get(&cid).copied().unwrap_or(self.default_width)
    }

    /// Explicitly-set width for `cid`, or `None` if it falls back to the default.
    pub fn get_opt(&self, cid: u16) -> Option<f64> {
        self.widths.get(&cid).copied()
    }
}

/// A loaded font with embedded TrueType/CFF data.
pub struct LoadedFont {
    pub font_type: PdfFontType,
    pub base_font: String,
    pub font_data: Option<Arc<[u8]>>,
    pub cid_widths: CidWidths,
    pub units_per_em: f64,
    pub ascent: f64,
    pub descent: f64,
    pub cid_to_gid: Option<HashMap<u16, u16>>,
    /// Simple-font character-code → glyph-name mapping (base encoding + /Differences).
    /// `None` for composite (Type0/CID) fonts and fully symbolic fonts.
    pub encoding: Option<encoding::Encoding>,
    /// /ToUnicode CMap for text extraction (code → Unicode string).
    pub to_unicode: Option<cmap::ToUnicodeMap>,
    /// Symbolic flag from the FontDescriptor (use the font's built-in cmap, not a base encoding).
    pub symbolic: bool,
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

            let cid_to_gid = if was_raw_cff {
                match font_type {
                    PdfFontType::Type0CidType2 => build_cff_cid_to_gid_map(&font_data),
                    PdfFontType::Type1 | PdfFontType::TrueType => {
                        build_cff_encoding_map(&font_data)
                    }
                    _ => None,
                }
            } else {
                None
            };

            Self {
                font_type,
                base_font,
                font_data: Some(Arc::from(font_data)),
                cid_widths,
                units_per_em,
                ascent,
                descent,
                cid_to_gid,
                encoding: None,
                to_unicode: None,
                symbolic: false,
            }
        } else {
            tracing::debug!(
                "font {base_font}: embedded data not parseable by ttf-parser, using widths only"
            );
            Self {
                font_type,
                base_font,
                font_data: None,
                cid_widths,
                units_per_em: 1000.0,
                ascent: 800.0,
                descent: -200.0,
                cid_to_gid: None,
                encoding: None,
                to_unicode: None,
                symbolic: false,
            }
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
            cid_widths,
            units_per_em: 1000.0,
            ascent: metrics.ascent,
            descent: metrics.descent,
            cid_to_gid: None,
            encoding: None,
            to_unicode: None,
            symbolic: false,
        })
    }

    pub fn new_placeholder(base_font: String) -> Self {
        Self {
            font_type: PdfFontType::Type1,
            base_font,
            font_data: None,
            cid_widths: CidWidths::new(500.0),
            units_per_em: 1000.0,
            ascent: 800.0,
            descent: -200.0,
            cid_to_gid: None,
            encoding: None,
            to_unicode: None,
            symbolic: false,
        }
    }

    /// Get glyph outline for a given glyph ID (or CID for CID fonts).
    pub fn glyph_outline(&self, glyph_id: u16) -> Option<GlyphOutline> {
        let data = self.font_data.as_ref()?;
        let face = ttf_parser::Face::parse(data, 0).ok()?;

        let actual_gid = if let Some(map) = &self.cid_to_gid {
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
            if let Ok(face) = ttf_parser::Face::parse(data, 0) {
                let gid = ttf_parser::GlyphId(glyph_id);
                if let Some(a) = face.glyph_hor_advance(gid) {
                    return a as f64;
                }
            }
        }
        self.cid_widths.get(glyph_id)
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
        let data = self.font_data.as_ref()?;
        let face = ttf_parser::Face::parse(data, 0).ok()?;
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

        // 2. Built-in cmap subtables — common for symbolic embedded fonts.
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
            // (1,0) Macintosh Roman: try the raw code.
            for st in cmap.subtables {
                if st.platform_id == ttf_parser::PlatformId::Macintosh && st.encoding_id == 0 {
                    if let Some(g) = st.glyph_index(code as u32) {
                        return Some(g.0);
                    }
                }
            }
        }

        // 3. Treat the code as Latin-1 and consult the Unicode cmap.
        if let Some(g) = face.glyph_index(code8 as char) {
            return Some(g.0);
        }

        None
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
            if let Ok(face) = ttf_parser::Face::parse(data, 0) {
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
            // Composite font (Identity-H by default → 2-byte codes). Only /ToUnicode
            // can map these; honour its codespace byte-length when it is a single
            // fixed width (e.g. a 1-byte composite CMap), else fall back to 2.
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
        self.font_data.is_some() || self.is_type3()
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
                first_char,
                ..
            } => {
                let idx = char_code.checked_sub(*first_char)? as usize;
                let glyph_name = encoding.get(idx)?;
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
                widths,
                first_char,
                ..
            } => {
                let idx = char_code.checked_sub(*first_char).unwrap_or(0) as usize;
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

fn build_cff_cid_to_gid_map(otf_data: &[u8]) -> Option<HashMap<u16, u16>> {
    // Find the CFF table in the OTF container
    let cff_data = find_cff_table(otf_data)?;
    parse_cff_charset(cff_data)
}

fn build_cff_encoding_map(otf_data: &[u8]) -> Option<HashMap<u16, u16>> {
    let cff_data = find_cff_table(otf_data)?;
    parse_cff_encoding(cff_data)
}

fn parse_cff_encoding(cff: &[u8]) -> Option<HashMap<u16, u16>> {
    if cff.len() < 5 { return None; }
    let header_size = cff[2] as usize;

    // Skip Name INDEX
    let mut pos = header_size;
    pos = skip_cff_index(cff, pos)?;

    // Top DICT INDEX
    let top_dict_index_start = pos;
    if pos + 2 > cff.len() { return None; }
    let count = u16::from_be_bytes([cff[pos], cff[pos + 1]]) as usize;
    if count == 0 { return None; }
    pos += 2;
    let off_size = cff[pos] as usize;
    pos += 1;

    let mut offsets = Vec::with_capacity(count + 1);
    for _ in 0..=count {
        let off = read_cff_offset(cff, pos, off_size)?;
        offsets.push(off);
        pos += off_size;
    }
    let data_start = pos;
    let dict_data = &cff[data_start + offsets[0] - 1..data_start + offsets[1] - 1];

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
        let num_glyphs = count_cff_index_entries(cff, charstrings_offset as usize)?;

        // For Standard Encoding, we need to map through the charset
        // charset maps GlyphId → SID, standard encoding maps charcode → SID
        let charset_offset = parse_top_dict_int(dict_data, 15).unwrap_or(0);
        if charset_offset == 0 {
            // ISOAdobe charset — identity for first 228 glyphs
            return None; // identity works
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
    let enc = encoding_offset as usize;
    if enc >= cff.len() { return None; }
    let format = cff[enc] & 0x7f;
    let mut map = HashMap::new();

    match format {
        0 => {
            // Format 0: nCodes, then array of codes
            if enc + 1 >= cff.len() { return None; }
            let n_codes = cff[enc + 1] as usize;
            for i in 0..n_codes {
                if enc + 2 + i >= cff.len() { break; }
                let code = cff[enc + 2 + i] as u16;
                map.insert(code, (i + 1) as u16);
            }
        }
        1 => {
            // Format 1: nRanges, then [first, nLeft] pairs
            if enc + 1 >= cff.len() { return None; }
            let n_ranges = cff[enc + 1] as usize;
            let mut gid: u16 = 1;
            let mut p = enc + 2;
            for _ in 0..n_ranges {
                if p + 2 > cff.len() { break; }
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

    if map.is_empty() { None } else { Some(map) }
}

fn parse_charset_to_gid_sid(cff: &[u8], charset_offset: usize, num_glyphs: usize, gid_to_sid: &mut HashMap<u16, u16>) {
    if charset_offset >= cff.len() { return; }
    let format = cff[charset_offset];
    let mut p = charset_offset + 1;
    let mut gid: u16 = 1;

    match format {
        0 => {
            while gid < num_glyphs as u16 {
                if p + 2 > cff.len() { break; }
                let sid = u16::from_be_bytes([cff[p], cff[p + 1]]);
                gid_to_sid.insert(gid, sid);
                p += 2;
                gid += 1;
            }
        }
        1 => {
            while gid < num_glyphs as u16 {
                if p + 3 > cff.len() { break; }
                let first = u16::from_be_bytes([cff[p], cff[p + 1]]);
                let n_left = cff[p + 2] as u16;
                for j in 0..=n_left {
                    let Some(sid) = first.checked_add(j) else { break; };
                    gid_to_sid.insert(gid, sid);
                    gid += 1;
                    if gid >= num_glyphs as u16 { break; }
                }
                p += 3;
            }
        }
        2 => {
            while gid < num_glyphs as u16 {
                if p + 4 > cff.len() { break; }
                let first = u16::from_be_bytes([cff[p], cff[p + 1]]);
                let n_left = u16::from_be_bytes([cff[p + 2], cff[p + 3]]);
                for j in 0..=n_left {
                    let Some(sid) = first.checked_add(j) else { break; };
                    gid_to_sid.insert(gid, sid);
                    gid += 1;
                    if gid >= num_glyphs as u16 { break; }
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
    for i in 48..=57 { enc[i] = (i - 48 + 17) as u16; } // 0-9
    enc[58] = 27; // colon
    enc[59] = 28; // semicolon
    enc[60] = 29; // less
    enc[61] = 30; // equal
    enc[62] = 31; // greater
    enc[63] = 32; // question
    enc[64] = 33; // at
    for i in 65..=90 { enc[i] = (i - 65 + 34) as u16; } // A-Z
    enc[91] = 60; // bracketleft
    enc[92] = 61; // backslash
    enc[93] = 62; // bracketright
    enc[94] = 63; // asciicircum
    enc[95] = 64; // underscore
    enc[96] = 65; // quoteleft
    for i in 97..=122 { enc[i] = (i - 97 + 66) as u16; } // a-z
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
    if data.len() < 12 { return None; }
    let num_tables = u16::from_be_bytes([data[4], data[5]]) as usize;
    for i in 0..num_tables {
        let rec = 12 + i * 16;
        if rec + 16 > data.len() { break; }
        if &data[rec..rec + 4] == b"CFF " {
            let offset = u32::from_be_bytes([data[rec + 8], data[rec + 9], data[rec + 10], data[rec + 11]]) as usize;
            let length = u32::from_be_bytes([data[rec + 12], data[rec + 13], data[rec + 14], data[rec + 15]]) as usize;
            if offset + length <= data.len() {
                return Some(&data[offset..offset + length]);
            }
        }
    }
    None
}

fn parse_cff_charset(cff: &[u8]) -> Option<HashMap<u16, u16>> {
    if cff.len() < 5 { return None; }
    let header_size = cff[2] as usize;

    // Skip Name INDEX
    let mut pos = header_size;
    pos = skip_cff_index(cff, pos)?;

    // Top DICT INDEX — we need to read it to find charset offset
    let top_dict_start = pos;
    let top_dict_index_end = skip_cff_index(cff, pos)?;

    // Parse Top DICT INDEX to get the data
    if pos + 2 > cff.len() { return None; }
    let count = u16::from_be_bytes([cff[pos], cff[pos + 1]]) as usize;
    if count == 0 { return None; }
    pos += 2;
    let off_size = cff[pos] as usize;
    pos += 1;

    // Read offsets
    let mut offsets = Vec::with_capacity(count + 1);
    for _ in 0..=count {
        let off = read_cff_offset(cff, pos, off_size)?;
        offsets.push(off);
        pos += off_size;
    }

    let data_start = pos;
    let dict_data = &cff[data_start + offsets[0] - 1..data_start + offsets[1] - 1];

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
    let num_glyphs = count_cff_index_entries(cff, charstrings_offset as usize)?;

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

    let cs = charset_offset as usize;
    if cs >= cff.len() { return None; }

    let format = cff[cs];
    let mut p = cs + 1;
    let mut gid: u16 = 1; // GID 0 is .notdef

    match format {
        0 => {
            // Format 0: array of SID/CIDs
            while gid < num_glyphs as u16 {
                if p + 2 > cff.len() { break; }
                let sid = u16::from_be_bytes([cff[p], cff[p + 1]]);
                map.insert(sid, gid);
                p += 2;
                gid += 1;
            }
        }
        1 => {
            // Format 1: ranges [first_sid, n_left (u8)]
            while gid < num_glyphs as u16 {
                if p + 3 > cff.len() { break; }
                let first = u16::from_be_bytes([cff[p], cff[p + 1]]);
                let n_left = cff[p + 2] as u16;
                for j in 0..=n_left {
                    let Some(sid) = first.checked_add(j) else { break; };
                    map.insert(sid, gid);
                    gid += 1;
                    if gid >= num_glyphs as u16 { break; }
                }
                p += 3;
            }
        }
        2 => {
            // Format 2: ranges [first_sid, n_left (u16)]
            while gid < num_glyphs as u16 {
                if p + 4 > cff.len() { break; }
                let first = u16::from_be_bytes([cff[p], cff[p + 1]]);
                let n_left = u16::from_be_bytes([cff[p + 2], cff[p + 3]]);
                for j in 0..=n_left {
                    let Some(sid) = first.checked_add(j) else { break; };
                    map.insert(sid, gid);
                    gid += 1;
                    if gid >= num_glyphs as u16 { break; }
                }
                p += 4;
            }
        }
        _ => return None,
    }

    Some(map)
}

fn skip_cff_index(cff: &[u8], pos: usize) -> Option<usize> {
    if pos + 2 > cff.len() { return None; }
    let count = u16::from_be_bytes([cff[pos], cff[pos + 1]]) as usize;
    if count == 0 { return Some(pos + 2); }
    let off_size = cff[pos + 2] as usize;
    let offsets_start = pos + 3;
    let last_offset_pos = offsets_start + count * off_size;
    if last_offset_pos + off_size > cff.len() { return None; }
    let last_offset = read_cff_offset(cff, last_offset_pos, off_size)?;
    let data_start = offsets_start + (count + 1) * off_size;
    Some(data_start + last_offset - 1)
}

fn read_cff_offset(cff: &[u8], pos: usize, size: usize) -> Option<usize> {
    if pos + size > cff.len() { return None; }
    let mut val = 0usize;
    for i in 0..size {
        val = (val << 8) | cff[pos + i] as usize;
    }
    Some(val)
}

fn count_cff_index_entries(cff: &[u8], pos: usize) -> Option<usize> {
    if pos + 2 > cff.len() { return None; }
    Some(u16::from_be_bytes([cff[pos], cff[pos + 1]]) as usize)
}

fn parse_top_dict_charset(dict_data: &[u8]) -> Option<usize> {
    parse_top_dict_int(dict_data, 15)
}

fn parse_top_dict_charstrings(dict_data: &[u8]) -> Option<usize> {
    parse_top_dict_int(dict_data, 17)
}

fn parse_top_dict_int(dict_data: &[u8], target_key: u8) -> Option<usize> {
    let mut pos = 0;
    let mut operand_stack: Vec<i64> = Vec::new();

    while pos < dict_data.len() {
        let b0 = dict_data[pos];
        match b0 {
            // Operators
            0..=21 => {
                let key = if b0 == 12 {
                    pos += 1;
                    if pos >= dict_data.len() { break; }
                    256 + dict_data[pos] as u16
                } else {
                    b0 as u16
                };
                if key == target_key as u16 {
                    return operand_stack.last().map(|&v| v as usize);
                }
                operand_stack.clear();
                pos += 1;
            }
            // Integer operands
            28 => {
                if pos + 2 >= dict_data.len() { break; }
                let val = i16::from_be_bytes([dict_data[pos + 1], dict_data[pos + 2]]) as i64;
                operand_stack.push(val);
                pos += 3;
            }
            29 => {
                if pos + 4 >= dict_data.len() { break; }
                let val = i32::from_be_bytes([dict_data[pos + 1], dict_data[pos + 2], dict_data[pos + 3], dict_data[pos + 4]]) as i64;
                operand_stack.push(val);
                pos += 5;
            }
            30 => {
                // Real number — skip nibbles until 0xf
                pos += 1;
                while pos < dict_data.len() {
                    let byte = dict_data[pos];
                    pos += 1;
                    if (byte & 0x0f) == 0x0f || (byte >> 4) == 0x0f { break; }
                }
                operand_stack.push(0); // placeholder
            }
            32..=246 => {
                operand_stack.push(b0 as i64 - 139);
                pos += 1;
            }
            247..=250 => {
                if pos + 1 >= dict_data.len() { break; }
                let val = (b0 as i64 - 247) * 256 + dict_data[pos + 1] as i64 + 108;
                operand_stack.push(val);
                pos += 2;
            }
            251..=254 => {
                if pos + 1 >= dict_data.len() { break; }
                let val = -(b0 as i64 - 251) * 256 - dict_data[pos + 1] as i64 - 108;
                operand_stack.push(val);
                pos += 2;
            }
            _ => { pos += 1; }
        }
    }
    None
}

fn is_raw_cff(data: &[u8]) -> bool {
    data.len() >= 4 && data[0] == 0x01 && data[1] == 0x00
        && data[2] >= 1 && data[2] <= 8
}

fn wrap_cff_in_otf(cff_data: &[u8]) -> Vec<u8> {
    let num_tables: u16 = 5;
    let search_range: u16 = 64;
    let entry_selector: u16 = 2;
    let range_shift: u16 = num_tables * 16 - search_range;

    let header_size = 12 + num_tables as usize * 16;

    fn pad4(n: u32) -> u32 { (n + 3) & !3 }
    fn compute_checksum(data: &[u8]) -> u32 {
        let mut sum: u32 = 0;
        let chunks = data.len() / 4;
        for i in 0..chunks {
            let val = u32::from_be_bytes([data[i * 4], data[i * 4 + 1], data[i * 4 + 2], data[i * 4 + 3]]);
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
    let cff_len = cff_data.len() as u32;

    let head_offset = cff_offset + pad4(cff_len);
    let head_len: u32 = 54;

    let hhea_offset = head_offset + pad4(head_len);
    let hhea_len: u32 = 36;

    let maxp_offset = hhea_offset + pad4(hhea_len);
    let maxp_len: u32 = 6;

    let post_offset = maxp_offset + pad4(maxp_len);
    let post_len: u32 = 32;

    let total_size = post_offset as usize + post_len as usize;
    let mut buf = vec![0u8; total_size];

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

/// Cache of loaded fonts, keyed by FontId.
pub struct FontCache {
    fonts: HashMap<FontId, LoadedFont>,
    name_to_id: HashMap<String, FontId>,
    next_id: FontId,
}

impl FontCache {
    pub fn new() -> Self {
        Self {
            fonts: HashMap::new(),
            name_to_id: HashMap::new(),
            next_id: 0,
        }
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
        if let Some(&existing_id) = self.name_to_id.get(&name) {
            return existing_id;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.name_to_id.insert(name, id);
        self.fonts.insert(id, font);
        id
    }

    pub fn len(&self) -> usize {
        self.fonts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fonts.is_empty()
    }
}

impl Default for FontCache {
    fn default() -> Self {
        Self::new()
    }
}
