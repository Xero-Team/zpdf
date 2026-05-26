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
}

/// A loaded font with embedded TrueType data.
pub struct LoadedFont {
    pub font_type: PdfFontType,
    pub base_font: String,
    pub font_data: Option<Arc<[u8]>>,
    pub cid_widths: CidWidths,
    pub units_per_em: f64,
    pub ascent: f64,
    pub descent: f64,
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
        let (units_per_em, ascent, descent) = if let Ok(face) =
            ttf_parser::Face::parse(&font_data, 0)
        {
            (
                face.units_per_em() as f64,
                face.ascender() as f64,
                face.descender() as f64,
            )
        } else {
            (1000.0, 800.0, -200.0)
        };

        Self {
            font_type,
            base_font,
            font_data: Some(Arc::from(font_data)),
            cid_widths,
            units_per_em,
            ascent,
            descent,
        }
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
        }
    }

    /// Get glyph outline for a given glyph ID.
    pub fn glyph_outline(&self, glyph_id: u16) -> Option<GlyphOutline> {
        let data = self.font_data.as_ref()?;
        let face = ttf_parser::Face::parse(data, 0).ok()?;
        let gid = ttf_parser::GlyphId(glyph_id);

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
