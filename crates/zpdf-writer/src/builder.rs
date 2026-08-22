//! Document builder: author new PDFs from scratch with pages, text, and images.
//!
//! [`DocumentBuilder`] creates a PDF without requiring an existing file.
//! Supports pages, standard-14 fonts, and image placement.
//!
//! # Example
//! ```no_run
//! use zpdf_writer::DocumentBuilder;
//!
//! let mut builder = DocumentBuilder::new();
//! let page = builder.add_page(612.0, 792.0);
//! builder.add_text(page, "Hello, PDF!", 50.0, 700.0, "Helvetica", 24.0, (0.0, 0.0, 0.0))?;
//! let pdf_bytes = builder.build()?;
//! std::fs::write("output.pdf", pdf_bytes)?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::collections::{HashMap, HashSet};

use zpdf_core::{ObjectId, PdfDict, PdfName, PdfObject, Result};
use zpdf_document::escape_text;

use crate::metadata::encode_text_string;

/// A handle to a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageHandle(u32);

/// Image data for embedding.
#[derive(Clone)]
pub enum ImageData {
    /// JPEG stream (DCTDecode).
    Jpeg {
        data: Vec<u8>,
        width: u32,
        height: u32,
        components: u8,
    },
    /// Raw RGB pixels (FlateDecode).
    Rgb8 {
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    },
    /// Raw RGBA pixels (RGB + SMask alpha).
    Rgba8 {
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    },
}

/// A structure-element tag for a page item: the role (`/S`) and optional
/// accessibility text (`/Alt`, `/ActualText`). When present, the item's
/// painting operators are wrapped in a marked-content sequence
/// (`/Role << /MCID N >> BDC … EMC`) and a matching `/StructElem` is emitted
/// in the document's `/StructTreeRoot` — producing a Tagged PDF.
#[derive(Debug, Clone)]
pub struct TagSpec {
    /// The structure role (`/S`), e.g. `P`, `H1`, `Figure`.
    pub role: zpdf_document::StructRole,
    /// `/Alt` — an alternate textual description (required for `Figure`).
    pub alt: Option<String>,
    /// `/ActualText` — the exact replacement text for the item.
    pub actual_text: Option<String>,
}

/// Item to place on a page.
enum PageItem {
    Text {
        text: String,
        x: f64,
        y: f64,
        font_name: String,
        size: f64,
        color: (f64, f64, f64),
        tag: Option<TagSpec>,
    },
    Image {
        image: ImageData,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        tag: Option<TagSpec>,
    },
    Path {
        segments: Vec<PathSegment>,
        style: PathStyle,
        tag: Option<TagSpec>,
    },
}

/// One segment of a vector path, in page coordinates.
#[derive(Debug, Clone, Copy)]
pub enum PathSegment {
    /// Begin a new subpath at (x, y).
    MoveTo { x: f64, y: f64 },
    /// Straight line to (x, y).
    LineTo { x: f64, y: f64 },
    /// Cubic Bézier to (x3, y3) with control points (x1, y1), (x2, y2).
    CurveTo {
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        x3: f64,
        y3: f64,
    },
    /// Axis-aligned rectangle (x, y, width, height).
    Rect {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    },
    /// Close the current subpath.
    Close,
}

/// How a path is painted.
#[derive(Debug, Clone, Copy)]
pub struct PathStyle {
    /// Stroke color; `None` disables stroking.
    pub stroke: Option<(f64, f64, f64)>,
    /// Fill color (nonzero winding); `None` disables filling.
    pub fill: Option<(f64, f64, f64)>,
    /// Stroke width in points.
    pub line_width: f64,
}

impl Default for PathStyle {
    fn default() -> Self {
        Self {
            stroke: Some((0.0, 0.0, 0.0)),
            fill: None,
            line_width: 1.0,
        }
    }
}

/// A page being built.
struct PageState {
    width: f64,
    height: f64,
    items: Vec<PageItem>,
}

/// The embedding kind — a simple WinAnsi TrueType font (the original path)
/// or a composite Type0 font (CJK-capable, TrueType- or CFF-flavored).
#[allow(clippy::large_enum_variant)] // WinAnsi carries a 256-entry width table; fonts are few.
enum EmbeddedKind {
    /// Simple-font TrueType with a WinAnsiEncoding byte width table.
    WinAnsi { widths: [u16; 256] },
    /// Composite (Type0) font. The 2-byte code emitted in the content stream
    /// is interpreted per `encoding`; `vertical` selects horizontal vs.
    /// vertical writing mode (Identity-H vs Identity-V, or a predefined CMap's
    /// horizontal vs vertical variant).
    Composite {
        program: FontProgram,
        encoding: CidEncoding,
        vertical: bool,
    },
}

/// The font-program flavor, which decides `/FontFile2` vs `/FontFile3` and
/// the descendant `/Subtype` (`CIDFontType2` vs `CIDFontType0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontProgram {
    /// TrueType outlines (`glyf`/`loca`) — `/FontFile2`, `CIDFontType2`.
    TrueType,
    /// CFF outlines (an OTF `CFF ` table or raw CFF) — `/FontFile3 /OpenType`,
    /// `CIDFontType0`.
    OpenTypeCff,
}

/// The 2-byte CID encoding a composite (Type0) font uses, selecting the Type0
/// `/Encoding` name, the meaning of the 2-byte content-stream code, and the
/// `/CIDToGIDMap` strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CidEncoding {
    /// `Identity-H` (and `Identity-V` when vertical): the 2-byte code is the
    /// GID, and `/CIDToGIDMap /Identity` makes CID = GID. The default for CJK
    /// authoring — any character the font's cmap maps is encodable.
    Identity,
    /// A predefined Adobe Unicode CMap (`UniGB-UCS2`, `UniJIS-UCS2`,
    /// `UniKS-UCS2`, `UniCNS-UCS2`, or their `-V` vertical variants): the
    /// 2-byte code is the Unicode scalar, and `/CIDToGIDMap` is an explicit
    /// table mapping Unicode CIDs to GIDs via the font's cmap. The ordering
    /// names the CJK collection.
    Predefined(PredefinedOrdering),
}

/// The Adobe CJK collection a predefined CMap targets (ISO 32000-1 Annex C).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PredefinedOrdering {
    /// GB / Simplified Chinese — `UniGB-UCS2` / `UniGB-UCS2-V`.
    Gb,
    /// JIS / Japanese — `UniJIS-UCS2` / `UniJIS-UCS2-V`.
    Jis,
    /// KS / Korean — `UniKS-UCS2` / `UniKS-UCS2-V`.
    Ks,
    /// CNS / Traditional Chinese — `UniCNS-UCS2` / `UniCNS-UCS2-V`.
    Cns,
}

impl PredefinedOrdering {
    /// The horizontal CMap name (`/Encoding` on the Type0 font).
    fn cmap_name(self, vertical: bool) -> &'static str {
        match (self, vertical) {
            (PredefinedOrdering::Gb, false) => "UniGB-UCS2-H",
            (PredefinedOrdering::Gb, true) => "UniGB-UCS2-V",
            (PredefinedOrdering::Jis, false) => "UniJIS-UCS2-H",
            (PredefinedOrdering::Jis, true) => "UniJIS-UCS2-V",
            (PredefinedOrdering::Ks, false) => "UniKS-UCS2-H",
            (PredefinedOrdering::Ks, true) => "UniKS-UCS2-V",
            (PredefinedOrdering::Cns, false) => "UniCNS-UCS2-H",
            (PredefinedOrdering::Cns, true) => "UniCNS-UCS2-V",
        }
    }

    /// The `/CIDSystemInfo` ordering string for this collection.
    fn ordering(self) -> &'static str {
        match self {
            PredefinedOrdering::Gb => "GB1",
            PredefinedOrdering::Jis => "Japan1",
            PredefinedOrdering::Ks => "Korea1",
            PredefinedOrdering::Cns => "CNS1",
        }
    }

    /// The `/CIDSystemInfo` supplement for this collection (a stable, common
    /// value; supplements vary by renderer but the writer does not embed the
    /// predefined CMap bytes themselves, only the name, so this is advisory).
    fn supplement(self) -> i64 {
        match self {
            PredefinedOrdering::Gb => 5,
            PredefinedOrdering::Jis => 6,
            PredefinedOrdering::Ks => 2,
            PredefinedOrdering::Cns => 4,
        }
    }
}

/// An embedded font, parsed once at `embed_font` / `embed_composite_font`
/// time. The `kind` selects the emission path; `data` is the raw font bytes.
struct EmbeddedFont {
    /// PostScript-style name used as /BaseFont.
    ps_name: String,
    /// The raw font file (embedded, optionally subset, as FontFile2/3).
    data: Vec<u8>,
    ascent: f64,
    descent: f64,
    cap_height: f64,
    bbox: [f64; 4],
    italic_angle: f64,
    kind: EmbeddedKind,
}

/// A handle to an embedded font, returned by [`DocumentBuilder::embed_font`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EmbeddedFontHandle(u32);

/// Build new PDFs from scratch.
pub struct DocumentBuilder {
    pages: Vec<PageState>,
    embedded_fonts: Vec<EmbeddedFont>,
    /// Catalog `/Lang` (BCP 47), written when the document is tagged.
    lang: Option<String>,
}

impl DocumentBuilder {
    /// Create a new document builder.
    pub fn new() -> Self {
        Self {
            pages: Vec::new(),
            embedded_fonts: Vec::new(),
            lang: None,
        }
    }

    /// Set the catalog `/Lang` (a BCP 47 language tag), emitted when the
    /// document is built. Required for PDF/UA and good practice for Tagged
    /// PDFs (it scopes the document's natural language for screen readers).
    pub fn set_lang(&mut self, lang: impl Into<String>) {
        self.lang = Some(lang.into());
    }

    /// Embed a TrueType font. The returned handle is used with
    /// [`Self::add_text_embedded`]. The full font file is embedded (no
    /// subsetting yet); text is limited to WinAnsi-encodable characters.
    pub fn embed_font(&mut self, font_bytes: Vec<u8>) -> Result<EmbeddedFontHandle> {
        // Parse in an inner scope so the borrow of `font_bytes` ends before we
        // move it into the stored EmbeddedFont.
        let (ps_name, widths, ascent, descent, cap_height, bbox, italic_angle) = {
            let face = ttf_parser::Face::parse(&font_bytes, 0).map_err(|e| {
                zpdf_core::Error::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("cannot parse font: {e}"),
                ))
            })?;

            let units_per_em = face.units_per_em() as f64;
            if units_per_em <= 0.0 {
                return Err(zpdf_core::Error::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "font has invalid unitsPerEm",
                )));
            }
            let to_milli = 1000.0 / units_per_em;

            let ps_name = face
                .names()
                .into_iter()
                .find(|n| n.name_id == ttf_parser::name_id::POST_SCRIPT_NAME)
                .and_then(|n| n.to_string())
                .or_else(|| {
                    face.names()
                        .into_iter()
                        .find(|n| n.name_id == ttf_parser::name_id::FULL_NAME)
                        .and_then(|n| n.to_string())
                })
                .unwrap_or_else(|| format!("ZPDFEmbedded{}", self.embedded_fonts.len()))
                .replace(' ', "");

            // WinAnsi code → glyph advance, via the font's cmap.
            let mut widths = [0u16; 256];
            for code in 32u16..=255 {
                let Some(ch) = winansi_code_to_char(code as u8) else {
                    continue;
                };
                if let Some(gid) = face.glyph_index(ch) {
                    if let Some(adv) = face.glyph_hor_advance(gid) {
                        widths[code as usize] = (adv as f64 * to_milli).round() as u16;
                    }
                }
            }

            let bbox = face.global_bounding_box();
            (
                ps_name,
                widths,
                face.ascender() as f64 * to_milli,
                face.descender() as f64 * to_milli,
                face.capital_height()
                    .map(|h| h as f64 * to_milli)
                    .unwrap_or(700.0),
                [
                    bbox.x_min as f64 * to_milli,
                    bbox.y_min as f64 * to_milli,
                    bbox.x_max as f64 * to_milli,
                    bbox.y_max as f64 * to_milli,
                ],
                face.italic_angle() as f64,
            )
        };

        self.embedded_fonts.push(EmbeddedFont {
            ps_name,
            data: font_bytes,
            ascent,
            descent,
            cap_height,
            bbox,
            italic_angle,
            kind: EmbeddedKind::WinAnsi { widths },
        });
        Ok(EmbeddedFontHandle((self.embedded_fonts.len() - 1) as u32))
    }

    /// Embed a composite (Type0) font for CJK and other large glyph sets,
    /// using **Identity-H**: the 2-byte code emitted in the content stream is
    /// the glyph ID, and `/CIDToGIDMap /Identity` makes CID = GID. Text is
    /// not limited to WinAnsi — any character the font's cmap can map is
    /// encodable, including tens of thousands of CJK ideographs.
    ///
    /// Both TrueType-flavored (`glyf`, embedded as `/FontFile2` /
    /// `CIDFontType2`) and CFF-flavored (OTF `CFF ` table or raw CFF,
    /// embedded as `/FontFile3 /OpenType` / `CIDFontType0`) fonts are
    /// supported. Raw CFF is wrapped in a minimal OTF container on embed.
    /// Glyphs are subset at `build()` time (sparse glyf / sparse charstrings).
    pub fn embed_composite_font(&mut self, font_bytes: Vec<u8>) -> Result<EmbeddedFontHandle> {
        self.push_composite(font_bytes, CidEncoding::Identity, false)
    }

    /// Embed a composite (Type0) font in **vertical writing mode** — the Type0
    /// `/Encoding` is `Identity-V`, the descendant CIDFont carries `/DW2`/`/W2`
    /// vertical metrics (read from the font's `vhea`/`vmtx` when present, else
    /// the PDF default `[880 −1000]` advance with centred glyph origin), and the
    /// content stream advances glyphs top-to-bottom. The 2-byte code is still the
    /// GID (Identity mapping); only the writing direction changes. Use for
    /// traditional vertical CJK (tategaki) layout.
    pub fn embed_composite_font_vertical(
        &mut self,
        font_bytes: Vec<u8>,
    ) -> Result<EmbeddedFontHandle> {
        self.push_composite(font_bytes, CidEncoding::Identity, true)
    }

    /// Embed a composite (Type0) font using a **predefined Adobe Unicode CMap**
    /// (`UniGB-UCS2`, `UniJIS-UCS2`, `UniKS-UCS2`, or `UniCNS-UCS2`) rather than
    /// Identity-H. The 2-byte code emitted in the content stream is the Unicode
    /// scalar (big-endian), and `/CIDToGIDMap` is an explicit table mapping each
    /// Unicode CID to its GID via the font's cmap. Set `vertical` for the `-V`
    /// (vertical) CMap variant. This is the legacy / non-embedded-CJK-font
    /// convention; for most new CJK authoring, [`Self::embed_composite_font`]
    /// (Identity-H) is simpler and covers every cmap-mappable character.
    pub fn embed_composite_font_predefined(
        &mut self,
        font_bytes: Vec<u8>,
        ordering: PredefinedOrdering,
        vertical: bool,
    ) -> Result<EmbeddedFontHandle> {
        self.push_composite(font_bytes, CidEncoding::Predefined(ordering), vertical)
    }

    /// Shared composite-font embed: parse the font program, extract metrics,
    /// and store it with the given CID encoding and writing mode.
    fn push_composite(
        &mut self,
        font_bytes: Vec<u8>,
        encoding: CidEncoding,
        vertical: bool,
    ) -> Result<EmbeddedFontHandle> {
        let (program, data) = if crate::cff::is_raw_cff(&font_bytes) {
            let otf = crate::cff::wrap_cff_in_otf(&font_bytes);
            if otf.is_empty() {
                return Err(zpdf_core::Error::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "cannot wrap raw CFF font into an OTF container",
                )));
            }
            (FontProgram::OpenTypeCff, otf)
        } else if crate::cff::is_cff_flavored(&font_bytes) {
            (FontProgram::OpenTypeCff, font_bytes)
        } else {
            (FontProgram::TrueType, font_bytes)
        };

        let (ps_name, ascent, descent, cap_height, bbox, italic_angle) = {
            let face = ttf_parser::Face::parse(&data, 0).map_err(|e| {
                zpdf_core::Error::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("cannot parse composite font: {e}"),
                ))
            })?;
            let units_per_em = face.units_per_em() as f64;
            if units_per_em <= 0.0 {
                return Err(zpdf_core::Error::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "composite font has invalid unitsPerEm",
                )));
            }
            let to_milli = 1000.0 / units_per_em;
            let ps_name = face
                .names()
                .into_iter()
                .find(|n| n.name_id == ttf_parser::name_id::POST_SCRIPT_NAME)
                .and_then(|n| n.to_string())
                .or_else(|| {
                    face.names()
                        .into_iter()
                        .find(|n| n.name_id == ttf_parser::name_id::FULL_NAME)
                        .and_then(|n| n.to_string())
                })
                .unwrap_or_else(|| format!("ZPDFComposite{}", self.embedded_fonts.len()))
                .replace(' ', "");
            let bbox = face.global_bounding_box();
            (
                ps_name,
                face.ascender() as f64 * to_milli,
                face.descender() as f64 * to_milli,
                face.capital_height()
                    .map(|h| h as f64 * to_milli)
                    .unwrap_or(700.0),
                [
                    bbox.x_min as f64 * to_milli,
                    bbox.y_min as f64 * to_milli,
                    bbox.x_max as f64 * to_milli,
                    bbox.y_max as f64 * to_milli,
                ],
                face.italic_angle() as f64,
            )
        };

        self.embedded_fonts.push(EmbeddedFont {
            ps_name,
            data,
            ascent,
            descent,
            cap_height,
            bbox,
            italic_angle,
            kind: EmbeddedKind::Composite {
                program,
                encoding,
                vertical,
            },
        });
        Ok(EmbeddedFontHandle((self.embedded_fonts.len() - 1) as u32))
    }

    /// Add text using a previously embedded font.
    #[allow(clippy::too_many_arguments)]
    pub fn add_text_embedded(
        &mut self,
        page: PageHandle,
        text: &str,
        x: f64,
        y: f64,
        font: EmbeddedFontHandle,
        size: f64,
        color: (f64, f64, f64),
    ) -> Result<()> {
        if font.0 as usize >= self.embedded_fonts.len() {
            return Err(zpdf_core::Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "embedded font handle not found",
            )));
        }
        // Embedded fonts are referenced by a reserved name that cannot clash
        // with the standard-14 set.
        let marker = format!("\u{0}EMB{}", font.0);
        if let Some(page_state) = self.pages.get_mut(page.0 as usize) {
            page_state.items.push(PageItem::Text {
                text: text.to_string(),
                x,
                y,
                font_name: marker,
                size,
                color,
                tag: None,
            });
            Ok(())
        } else {
            Err(zpdf_core::Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "page handle not found",
            )))
        }
    }

    /// Add a page with given width and height in points.
    pub fn add_page(&mut self, width: f64, height: f64) -> PageHandle {
        let handle = PageHandle(self.pages.len() as u32);
        self.pages.push(PageState {
            width,
            height,
            items: Vec::new(),
        });
        handle
    }

    /// Add text to a page using a standard-14 font.
    #[allow(clippy::too_many_arguments)]
    pub fn add_text(
        &mut self,
        page: PageHandle,
        text: &str,
        x: f64,
        y: f64,
        font_name: &str,
        size: f64,
        color: (f64, f64, f64),
    ) -> Result<()> {
        // Validate font name
        let normalized = match font_name {
            "Helvetica"
            | "Helvetica-Bold"
            | "Helvetica-Oblique"
            | "Helvetica-BoldOblique"
            | "Times-Roman"
            | "Times-Bold"
            | "Times-Italic"
            | "Times-BoldItalic"
            | "Courier"
            | "Courier-Bold"
            | "Courier-Oblique"
            | "Courier-BoldOblique"
            | "Symbol"
            | "ZapfDingbats" => font_name.to_string(),
            // Aliases
            "Arial" => "Helvetica".to_string(),
            "Arial-Bold" => "Helvetica-Bold".to_string(),
            "Times" => "Times-Roman".to_string(),
            "CourierNew" => "Courier".to_string(),
            _ => {
                return Err(zpdf_core::Error::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("unsupported font: {}", font_name),
                )))
            }
        };

        if let Some(page_state) = self.pages.get_mut(page.0 as usize) {
            page_state.items.push(PageItem::Text {
                text: text.to_string(),
                x,
                y,
                font_name: normalized,
                size,
                color,
                tag: None,
            });
            Ok(())
        } else {
            Err(zpdf_core::Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "page handle not found",
            )))
        }
    }

    /// Add an image to a page.
    pub fn add_image(
        &mut self,
        page: PageHandle,
        image: ImageData,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    ) -> Result<()> {
        if let Some(page_state) = self.pages.get_mut(page.0 as usize) {
            page_state.items.push(PageItem::Image {
                image,
                x,
                y,
                width,
                height,
                tag: None,
            });
            Ok(())
        } else {
            Err(zpdf_core::Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "page handle not found",
            )))
        }
    }

    /// Add a vector path to a page.
    pub fn add_path(
        &mut self,
        page: PageHandle,
        segments: Vec<PathSegment>,
        style: PathStyle,
    ) -> Result<()> {
        let all_finite = segments.iter().all(|seg| match *seg {
            PathSegment::MoveTo { x, y } | PathSegment::LineTo { x, y } => {
                x.is_finite() && y.is_finite()
            }
            PathSegment::CurveTo {
                x1,
                y1,
                x2,
                y2,
                x3,
                y3,
            } => [x1, y1, x2, y2, x3, y3].iter().all(|v| v.is_finite()),
            PathSegment::Rect {
                x,
                y,
                width,
                height,
            } => [x, y, width, height].iter().all(|v| v.is_finite()),
            PathSegment::Close => true,
        });
        if !all_finite || !style.line_width.is_finite() || style.line_width < 0.0 {
            return Err(zpdf_core::Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path coordinates and line width must be finite",
            )));
        }
        if let Some(page_state) = self.pages.get_mut(page.0 as usize) {
            page_state.items.push(PageItem::Path {
                segments,
                style,
                tag: None,
            });
            Ok(())
        } else {
            Err(zpdf_core::Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "page handle not found",
            )))
        }
    }

    /// Add tagged text using a previously embedded font. The text is wrapped
    /// in a marked-content sequence carrying `tag.role` and an MCID, and a
    /// matching `/StructElem` is emitted in the `/StructTreeRoot` at build
    /// time — producing Tagged PDF content.
    #[allow(clippy::too_many_arguments)]
    pub fn add_tagged_text_embedded(
        &mut self,
        page: PageHandle,
        text: &str,
        x: f64,
        y: f64,
        font: EmbeddedFontHandle,
        size: f64,
        color: (f64, f64, f64),
        tag: TagSpec,
    ) -> Result<()> {
        if font.0 as usize >= self.embedded_fonts.len() {
            return Err(zpdf_core::Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "embedded font handle not found",
            )));
        }
        let marker = format!("\u{0}EMB{}", font.0);
        if let Some(page_state) = self.pages.get_mut(page.0 as usize) {
            page_state.items.push(PageItem::Text {
                text: text.to_string(),
                x,
                y,
                font_name: marker,
                size,
                color,
                tag: Some(tag),
            });
            Ok(())
        } else {
            Err(zpdf_core::Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "page handle not found",
            )))
        }
    }

    /// Add tagged text using a standard-14 font. See
    /// [`Self::add_tagged_text_embedded`].
    #[allow(clippy::too_many_arguments)]
    pub fn add_tagged_text(
        &mut self,
        page: PageHandle,
        text: &str,
        x: f64,
        y: f64,
        font_name: &str,
        size: f64,
        color: (f64, f64, f64),
        tag: TagSpec,
    ) -> Result<()> {
        // Reuse the standard-14 validation in add_text by calling it, then
        // patch in the tag.
        self.add_text(page, text, x, y, font_name, size, color)?;
        if let Some(page_state) = self.pages.get_mut(page.0 as usize) {
            if let Some(PageItem::Text { tag: slot, .. }) = page_state.items.last_mut() {
                *slot = Some(tag);
            }
        }
        Ok(())
    }

    /// Add a tagged image. See [`Self::add_tagged_text_embedded`].
    #[allow(clippy::too_many_arguments)]
    pub fn add_tagged_image(
        &mut self,
        page: PageHandle,
        image: ImageData,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        tag: TagSpec,
    ) -> Result<()> {
        self.add_image(page, image, x, y, width, height)?;
        if let Some(page_state) = self.pages.get_mut(page.0 as usize) {
            if let Some(PageItem::Image { tag: slot, .. }) = page_state.items.last_mut() {
                *slot = Some(tag);
            }
        }
        Ok(())
    }

    /// Add a tagged vector path. See [`Self::add_tagged_text_embedded`].
    pub fn add_tagged_path(
        &mut self,
        page: PageHandle,
        segments: Vec<PathSegment>,
        style: PathStyle,
        tag: TagSpec,
    ) -> Result<()> {
        self.add_path(page, segments, style)?;
        if let Some(page_state) = self.pages.get_mut(page.0 as usize) {
            if let Some(PageItem::Path { tag: slot, .. }) = page_state.items.last_mut() {
                *slot = Some(tag);
            }
        }
        Ok(())
    }

    /// Build the PDF and return its bytes.
    pub fn build(&self) -> Result<Vec<u8>> {
        let num_pages = self.pages.len();
        if num_pages == 0 {
            return Err(zpdf_core::Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "no pages added",
            )));
        }

        // Object numbers: 1 = catalog, 2 = pages tree, 3..2+pages = pages, rest = images + contents
        let mut obj_num = 3u32;
        let page_obj_nums: Vec<u32> = (0..num_pages as u32)
            .map(|_| {
                let n = obj_num;
                obj_num += 1;
                n
            })
            .collect();

        // Build page content streams and track image objects
        let mut page_contents = Vec::new();
        let mut image_objects: Vec<(u32, ImageData)> = Vec::new();
        let mut image_counter = 0usize;

        // For composite (Type0/Identity-H) embedded fonts, build a char→GID
        // map for every character used with that font, so build_page_content
        // can emit 2-byte GID codes. Parsed once per font here, reused across
        // pages (the content stream is emitted in the page loop below).
        let composite_glyph_maps = self.build_composite_glyph_maps();

        for (page_idx, page_state) in self.pages.iter().enumerate() {
            let (content_bytes, font_names, image_refs, tagged) = self.build_page_content(
                page_state,
                &mut image_counter,
                &mut obj_num,
                &composite_glyph_maps,
            )?;

            // Record image data for the allocated object numbers.
            let mut ref_iter = image_refs.iter();
            for item in &page_state.items {
                if let PageItem::Image { image, .. } = item {
                    if let Some(&num) = ref_iter.next() {
                        image_objects.push((num, image.clone()));
                    }
                }
            }

            let content_obj = obj_num;
            obj_num += 1;

            page_contents.push((
                page_obj_nums[page_idx],
                content_obj,
                content_bytes,
                font_names,
                image_refs,
                tagged,
            ));
        }

        // Font dicts are emitted as indirect objects (many parsers, including
        // zpdf's own resource loader, only follow /Font entries that are
        // references). Dedup per document by BaseFont name.
        let mut font_obj_by_name: HashMap<String, u32> = HashMap::new();
        for (_, _, _, font_names, _, _) in &page_contents {
            for name in font_names {
                if !font_obj_by_name.contains_key(name) {
                    font_obj_by_name.insert(name.clone(), obj_num);
                    obj_num += 1;
                }
            }
        }

        // Build all objects
        let mut objects = Vec::new();
        let mut streams = Vec::new();

        for (name, num) in &font_obj_by_name {
            // Embedded fonts use the reserved "\0EMB<idx>" marker; everything
            // else is a standard-14 Type1 dict.
            if let Some(idx) = name
                .strip_prefix("\u{0}EMB")
                .and_then(|s| s.parse::<usize>().ok())
            {
                let font = &self.embedded_fonts[idx];

                // Collect the characters shown with this font across all pages.
                let used: HashSet<char> = self
                    .pages
                    .iter()
                    .flat_map(|p| &p.items)
                    .filter_map(|item| match item {
                        PageItem::Text {
                            text, font_name, ..
                        } if font_name == name => Some(text.chars()),
                        _ => None,
                    })
                    .flatten()
                    .collect();

                match &font.kind {
                    EmbeddedKind::WinAnsi { widths } => {
                        // Sparse-glyf subset: unused outlines dropped, metrics
                        // preserved. Fall back to the full file when subsetting
                        // isn't possible.
                        let subset = crate::subset::subset_truetype(&font.data, &used);
                        let file_bytes: &[u8] = subset.as_deref().unwrap_or(&font.data);

                        // FontFile2 stream.
                        let file_num = obj_num;
                        obj_num += 1;
                        let mut file_dict = PdfDict::new();
                        file_dict.insert(
                            PdfName::new("Filter"),
                            PdfObject::Name(PdfName::new("FlateDecode")),
                        );
                        file_dict.insert(
                            PdfName::new("Length1"),
                            PdfObject::Integer(file_bytes.len() as i64),
                        );
                        streams.push((file_num, file_dict, flate_compress(file_bytes)?));

                        // FontDescriptor.
                        let desc_num = obj_num;
                        obj_num += 1;
                        let mut desc = PdfDict::new();
                        desc.insert(
                            PdfName::new("Type"),
                            PdfObject::Name(PdfName::new("FontDescriptor")),
                        );
                        desc.insert(
                            PdfName::new("FontName"),
                            PdfObject::Name(PdfName::new(&font.ps_name)),
                        );
                        // Flags: bit 6 (Nonsymbolic).
                        desc.insert(PdfName::new("Flags"), PdfObject::Integer(32));
                        desc.insert(
                            PdfName::new("FontBBox"),
                            PdfObject::Array(
                                font.bbox.iter().map(|&v| PdfObject::Real(v)).collect(),
                            ),
                        );
                        desc.insert(
                            PdfName::new("ItalicAngle"),
                            PdfObject::Real(font.italic_angle),
                        );
                        desc.insert(PdfName::new("Ascent"), PdfObject::Real(font.ascent));
                        desc.insert(PdfName::new("Descent"), PdfObject::Real(font.descent));
                        desc.insert(PdfName::new("CapHeight"), PdfObject::Real(font.cap_height));
                        desc.insert(PdfName::new("StemV"), PdfObject::Integer(80));
                        desc.insert(
                            PdfName::new("FontFile2"),
                            PdfObject::Ref(ObjectId(file_num, 0)),
                        );
                        objects.push((desc_num, PdfObject::Dict(desc)));

                        // Font dict (simple TrueType, WinAnsi).
                        let mut font_dict = PdfDict::new();
                        font_dict
                            .insert(PdfName::new("Type"), PdfObject::Name(PdfName::new("Font")));
                        font_dict.insert(
                            PdfName::new("Subtype"),
                            PdfObject::Name(PdfName::new("TrueType")),
                        );
                        font_dict.insert(
                            PdfName::new("BaseFont"),
                            PdfObject::Name(PdfName::new(&font.ps_name)),
                        );
                        font_dict.insert(
                            PdfName::new("Encoding"),
                            PdfObject::Name(PdfName::new("WinAnsiEncoding")),
                        );
                        font_dict.insert(PdfName::new("FirstChar"), PdfObject::Integer(32));
                        font_dict.insert(PdfName::new("LastChar"), PdfObject::Integer(255));
                        font_dict.insert(
                            PdfName::new("Widths"),
                            PdfObject::Array(
                                widths[32..=255]
                                    .iter()
                                    .map(|&w| PdfObject::Integer(w as i64))
                                    .collect(),
                            ),
                        );
                        font_dict.insert(
                            PdfName::new("FontDescriptor"),
                            PdfObject::Ref(ObjectId(desc_num, 0)),
                        );
                        objects.push((*num, PdfObject::Dict(font_dict)));
                    }
                    EmbeddedKind::Composite {
                        program,
                        encoding,
                        vertical,
                    } => {
                        let vertical = *vertical;
                        // Type0 composite font. Resolve GIDs and advance widths
                        // for the used characters, build the sparse /W array and
                        // /ToUnicode pairs, and subset. The 2-byte content-stream
                        // code is the GID (Identity) or the Unicode scalar
                        // (Predefined); /W and /W2 are indexed by CID (= code),
                        // and /CIDToGIDMap is /Identity or an explicit table.
                        let face = ttf_parser::Face::parse(&font.data, 0).map_err(|e| {
                            zpdf_core::Error::Io(std::io::Error::new(
                                std::io::ErrorKind::InvalidInput,
                                format!("cannot parse composite font for emission: {e}"),
                            ))
                        })?;
                        let upem = face.units_per_em() as f64;
                        let to_milli = if upem > 0.0 { 1000.0 / upem } else { 1.0 };

                        let mut keep_gids: HashSet<u16> = HashSet::new();
                        keep_gids.insert(0); // .notdef
                                             // /W entries indexed by CID (= the 2-byte code).
                        let mut cid_width_entries: Vec<(u16, u16)> = Vec::new();
                        let mut tounicode_pairs: Vec<(u16, char)> = Vec::new();
                        // For predefined CMaps: explicit CID(=Unicode) → GID map.
                        let mut cid_to_gid: Vec<(u16, u16)> = Vec::new();
                        // Vertical /W2 entries (cid, w1y, vx, vy), built only for
                        // vertical writing mode. w1y is the top-to-bottom advance;
                        // (vx, vy) is the glyph origin offset from the CID origin.
                        let mut w2_entries: Vec<(u16, i64, i64, i64)> = Vec::new();
                        for ch in &used {
                            let gid = face.glyph_index(*ch).map(|g| g.0).unwrap_or(0);
                            keep_gids.insert(gid);
                            let w = face
                                .glyph_hor_advance(ttf_parser::GlyphId(gid))
                                .map(|a| (a as f64 * to_milli).round().max(0.0) as u16)
                                .unwrap_or(1000);
                            let code = match encoding {
                                CidEncoding::Identity => gid,
                                CidEncoding::Predefined(_) => {
                                    // BMP only (see build_composite_glyph_maps).
                                    if *ch as u32 <= 0xFFFF {
                                        *ch as u16
                                    } else {
                                        0
                                    }
                                }
                            };
                            cid_width_entries.push((code, w));
                            tounicode_pairs.push((code, *ch));
                            if matches!(encoding, CidEncoding::Predefined(_)) {
                                cid_to_gid.push((code, gid));
                            }
                            if vertical {
                                // Per-glyph vertical advance from vmtx when
                                // available; else use /DW2's w1y (0 signals
                                // "use /DW2" on the read side, so emit the
                                // default rather than a per-glyph entry).
                                let (w1y, vx, vy) = vertical_glyph_metric(
                                    &face,
                                    ttf_parser::GlyphId(gid),
                                    to_milli,
                                );
                                w2_entries.push((code, w1y, vx, vy));
                            }
                        }
                        cid_width_entries.sort_unstable_by_key(|(c, _)| *c);
                        cid_width_entries.dedup_by_key(|(c, _)| *c);
                        let w_array = build_w_array(&cid_width_entries);

                        // /DW2 default vertical metrics + sparse /W2. The PDF
                        // default is [880 −1000] (vy=880, w1y=−1000); we prefer
                        // the font's vhea ascender/descender when present.
                        let (dw2, w2_array) = if vertical {
                            let dw2 = dw2_default(&face, to_milli);
                            // Only emit per-glyph /W2 entries whose advance
                            // differs from /DW2's w1y (the common case for CJK
                            // punctuation with full-width vs half-width).
                            let w1y_default = dw2[1];
                            let mut entries: Vec<(u16, i64, i64, i64)> = w2_entries
                                .into_iter()
                                .filter(|(_, w1y, _, _)| *w1y != w1y_default)
                                .collect();
                            entries.sort_unstable_by_key(|(c, _, _, _)| *c);
                            entries.dedup_by_key(|(c, _, _, _)| *c);
                            (Some(dw2), build_w2_array(&entries))
                        } else {
                            (None, PdfObject::Array(vec![]))
                        };

                        // Subset (sparse glyf for TrueType, sparse charstrings
                        // for CFF); fall back to the full font.
                        let subset = match program {
                            FontProgram::TrueType => {
                                crate::subset::subset_truetype(&font.data, &used)
                            }
                            FontProgram::OpenTypeCff => {
                                crate::cff::subset_cff(&font.data, &keep_gids)
                            }
                        };
                        let file_bytes: &[u8] = subset.as_deref().unwrap_or(&font.data);

                        // FontFile2 (TrueType) or FontFile3 /OpenType (CFF).
                        let file_num = obj_num;
                        obj_num += 1;
                        let mut file_dict = PdfDict::new();
                        file_dict.insert(
                            PdfName::new("Filter"),
                            PdfObject::Name(PdfName::new("FlateDecode")),
                        );
                        match program {
                            FontProgram::TrueType => {
                                file_dict.insert(
                                    PdfName::new("Length1"),
                                    PdfObject::Integer(file_bytes.len() as i64),
                                );
                            }
                            FontProgram::OpenTypeCff => {
                                file_dict.insert(
                                    PdfName::new("Subtype"),
                                    PdfObject::Name(PdfName::new("OpenType")),
                                );
                            }
                        }
                        streams.push((file_num, file_dict, flate_compress(file_bytes)?));

                        // FontDescriptor.
                        let desc_num = obj_num;
                        obj_num += 1;
                        let mut desc = PdfDict::new();
                        desc.insert(
                            PdfName::new("Type"),
                            PdfObject::Name(PdfName::new("FontDescriptor")),
                        );
                        desc.insert(
                            PdfName::new("FontName"),
                            PdfObject::Name(PdfName::new(&font.ps_name)),
                        );
                        // Flags: bit 3 (Symbolic) — typical for Identity-H CJK.
                        desc.insert(PdfName::new("Flags"), PdfObject::Integer(4));
                        desc.insert(
                            PdfName::new("FontBBox"),
                            PdfObject::Array(
                                font.bbox.iter().map(|&v| PdfObject::Real(v)).collect(),
                            ),
                        );
                        desc.insert(
                            PdfName::new("ItalicAngle"),
                            PdfObject::Real(font.italic_angle),
                        );
                        desc.insert(PdfName::new("Ascent"), PdfObject::Real(font.ascent));
                        desc.insert(PdfName::new("Descent"), PdfObject::Real(font.descent));
                        desc.insert(PdfName::new("CapHeight"), PdfObject::Real(font.cap_height));
                        desc.insert(PdfName::new("StemV"), PdfObject::Integer(80));
                        match program {
                            FontProgram::TrueType => desc.insert(
                                PdfName::new("FontFile2"),
                                PdfObject::Ref(ObjectId(file_num, 0)),
                            ),
                            FontProgram::OpenTypeCff => desc.insert(
                                PdfName::new("FontFile3"),
                                PdfObject::Ref(ObjectId(file_num, 0)),
                            ),
                        }
                        objects.push((desc_num, PdfObject::Dict(desc)));

                        // CIDFont descendant dict.
                        let cid_num = obj_num;
                        obj_num += 1;
                        let mut cid = PdfDict::new();
                        cid.insert(PdfName::new("Type"), PdfObject::Name(PdfName::new("Font")));
                        cid.insert(
                            PdfName::new("Subtype"),
                            PdfObject::Name(PdfName::new(match program {
                                FontProgram::TrueType => "CIDFontType2",
                                FontProgram::OpenTypeCff => "CIDFontType0",
                            })),
                        );
                        cid.insert(
                            PdfName::new("BaseFont"),
                            PdfObject::Name(PdfName::new(&font.ps_name)),
                        );
                        // /CIDSystemInfo: Identity for Identity-H/V, or the
                        // predefined collection's registry/ordering/supplement.
                        let mut sysinfo = PdfDict::new();
                        sysinfo.insert(
                            PdfName::new("Registry"),
                            PdfObject::String(zpdf_core::PdfString(b"Adobe".to_vec())),
                        );
                        let (ordering, supplement) = match encoding {
                            CidEncoding::Identity => ("Identity".to_string(), 0i64),
                            CidEncoding::Predefined(ord) => {
                                (ord.ordering().to_string(), ord.supplement())
                            }
                        };
                        sysinfo.insert(
                            PdfName::new("Ordering"),
                            PdfObject::String(zpdf_core::PdfString(ordering.into_bytes())),
                        );
                        sysinfo.insert(PdfName::new("Supplement"), PdfObject::Integer(supplement));
                        cid.insert(PdfName::new("CIDSystemInfo"), PdfObject::Dict(sysinfo));
                        cid.insert(
                            PdfName::new("FontDescriptor"),
                            PdfObject::Ref(ObjectId(desc_num, 0)),
                        );
                        cid.insert(PdfName::new("DW"), PdfObject::Integer(1000));
                        cid.insert(PdfName::new("W"), w_array);
                        // Vertical metrics on the descendant CIDFont.
                        if let Some(dw2) = &dw2 {
                            cid.insert(
                                PdfName::new("DW2"),
                                PdfObject::Array(
                                    dw2.iter().map(|&v| PdfObject::Integer(v)).collect(),
                                ),
                            );
                            if let PdfObject::Array(ref w2) = w2_array {
                                if !w2.is_empty() {
                                    cid.insert(PdfName::new("W2"), w2_array.clone());
                                }
                            }
                        }
                        // /CIDToGIDMap: /Identity for Identity encodings, or an
                        // explicit table (a stream of 2-byte GID values indexed
                        // by CID) for predefined CMaps where CID = Unicode.
                        match encoding {
                            CidEncoding::Identity => {
                                cid.insert(
                                    PdfName::new("CIDToGIDMap"),
                                    PdfObject::Name(PdfName::new("Identity")),
                                );
                            }
                            CidEncoding::Predefined(_) => {
                                let c2g_num = obj_num;
                                obj_num += 1;
                                let c2g_bytes = build_cid_to_gid_map(&cid_to_gid);
                                let mut c2g_dict = PdfDict::new();
                                c2g_dict.insert(
                                    PdfName::new("Filter"),
                                    PdfObject::Name(PdfName::new("FlateDecode")),
                                );
                                streams.push((c2g_num, c2g_dict, flate_compress(&c2g_bytes)?));
                                cid.insert(
                                    PdfName::new("CIDToGIDMap"),
                                    PdfObject::Ref(ObjectId(c2g_num, 0)),
                                );
                            }
                        }
                        objects.push((cid_num, PdfObject::Dict(cid)));

                        // ToUnicode CMap stream.
                        let tounicode_num = obj_num;
                        obj_num += 1;
                        let tounicode_bytes =
                            crate::tounicode::build_tounicode_cmap(&tounicode_pairs);
                        let mut tu_dict = PdfDict::new();
                        tu_dict.insert(
                            PdfName::new("Filter"),
                            PdfObject::Name(PdfName::new("FlateDecode")),
                        );
                        streams.push((tounicode_num, tu_dict, flate_compress(&tounicode_bytes)?));

                        // Type0 font dict (the font object itself).
                        let mut font_dict = PdfDict::new();
                        font_dict
                            .insert(PdfName::new("Type"), PdfObject::Name(PdfName::new("Font")));
                        font_dict.insert(
                            PdfName::new("Subtype"),
                            PdfObject::Name(PdfName::new("Type0")),
                        );
                        font_dict.insert(
                            PdfName::new("BaseFont"),
                            PdfObject::Name(PdfName::new(&font.ps_name)),
                        );
                        let encoding_name = match encoding {
                            CidEncoding::Identity => {
                                if vertical {
                                    "Identity-V"
                                } else {
                                    "Identity-H"
                                }
                            }
                            CidEncoding::Predefined(ord) => ord.cmap_name(vertical),
                        };
                        font_dict.insert(
                            PdfName::new("Encoding"),
                            PdfObject::Name(PdfName::new(encoding_name)),
                        );
                        font_dict.insert(
                            PdfName::new("DescendantFonts"),
                            PdfObject::Array(vec![PdfObject::Ref(ObjectId(cid_num, 0))]),
                        );
                        font_dict.insert(
                            PdfName::new("ToUnicode"),
                            PdfObject::Ref(ObjectId(tounicode_num, 0)),
                        );
                        objects.push((*num, PdfObject::Dict(font_dict)));
                    }
                }
            } else {
                let mut font_dict = PdfDict::new();
                font_dict.insert(PdfName::new("Type"), PdfObject::Name(PdfName::new("Font")));
                font_dict.insert(
                    PdfName::new("Subtype"),
                    PdfObject::Name(PdfName::new("Type1")),
                );
                font_dict.insert(
                    PdfName::new("BaseFont"),
                    PdfObject::Name(PdfName::new(name)),
                );
                objects.push((*num, PdfObject::Dict(font_dict)));
            }
        }

        // --- Tagged PDF: when any page carries tagged items, emit a
        // /StructTreeRoot + /MarkInfo (and /Lang when set). Each tagged item
        // becomes a /StructElem with /K = its MCID; the root /ParentTree maps
        // each page's /StructParents key to that page's elem-ref array (in
        // MCID order), so the read side can resolve MCID → element.
        let any_tagged = page_contents
            .iter()
            .any(|(_, _, _, _, _, tagged)| !tagged.is_empty());
        let tree_root_num: Option<u32> = if any_tagged {
            // Reserve the StructTreeRoot number first so every /StructElem's
            // /P can point back at it.
            let tree_root_num = obj_num;
            obj_num += 1;

            let mut top_level_elems: Vec<PdfObject> = Vec::new();
            let mut parent_nums: Vec<PdfObject> = Vec::new();
            for (page_idx, (page_num, _, _, _, _, tagged)) in page_contents.iter().enumerate() {
                if tagged.is_empty() {
                    continue;
                }
                let mut elem_refs: Vec<PdfObject> = Vec::with_capacity(tagged.len());
                for (mcid, spec) in tagged {
                    let elem_num = obj_num;
                    obj_num += 1;
                    elem_refs.push(PdfObject::Ref(ObjectId(elem_num, 0)));
                    let mut elem = PdfDict::new();
                    elem.insert(
                        PdfName::new("Type"),
                        PdfObject::Name(PdfName::new("StructElem")),
                    );
                    elem.insert(
                        PdfName::new("S"),
                        PdfObject::Name(PdfName::new(spec.role.as_str())),
                    );
                    elem.insert(
                        PdfName::new("P"),
                        PdfObject::Ref(ObjectId(tree_root_num, 0)),
                    );
                    elem.insert(PdfName::new("Pg"), PdfObject::Ref(ObjectId(*page_num, 0)));
                    elem.insert(PdfName::new("K"), PdfObject::Integer(*mcid as i64));
                    if let Some(alt) = &spec.alt {
                        elem.insert(
                            PdfName::new("Alt"),
                            PdfObject::String(encode_text_string(alt)),
                        );
                    }
                    if let Some(actual) = &spec.actual_text {
                        elem.insert(
                            PdfName::new("ActualText"),
                            PdfObject::String(encode_text_string(actual)),
                        );
                    }
                    objects.push((elem_num, PdfObject::Dict(elem)));
                }
                top_level_elems.extend(elem_refs.iter().cloned());

                // The parent-tree array for this page (elem refs in MCID order).
                let arr_num = obj_num;
                obj_num += 1;
                objects.push((arr_num, PdfObject::Array(elem_refs)));
                parent_nums.push(PdfObject::Integer(page_idx as i64));
                parent_nums.push(PdfObject::Ref(ObjectId(arr_num, 0)));
            }

            // /ParentTree number tree: /Nums [ key0 arr0 key1 arr1 … ].
            let parent_tree_num = obj_num;
            obj_num += 1;
            let mut parent_tree = PdfDict::new();
            parent_tree.insert(PdfName::new("Nums"), PdfObject::Array(parent_nums));
            objects.push((parent_tree_num, PdfObject::Dict(parent_tree)));

            // StructTreeRoot.
            let mut root = PdfDict::new();
            root.insert(
                PdfName::new("Type"),
                PdfObject::Name(PdfName::new("StructTreeRoot")),
            );
            root.insert(PdfName::new("K"), PdfObject::Array(top_level_elems));
            root.insert(
                PdfName::new("ParentTree"),
                PdfObject::Ref(ObjectId(parent_tree_num, 0)),
            );
            root.insert(
                PdfName::new("ParentTreeNextKey"),
                PdfObject::Integer(num_pages as i64),
            );
            objects.push((tree_root_num, PdfObject::Dict(root)));
            Some(tree_root_num)
        } else {
            None
        };

        // Object 1: Catalog
        let mut catalog = PdfDict::new();
        catalog.insert(
            PdfName::new("Type"),
            PdfObject::Name(PdfName::new("Catalog")),
        );
        catalog.insert(PdfName::new("Pages"), PdfObject::Ref(ObjectId(2, 0)));
        if let Some(tree_num) = tree_root_num {
            catalog.insert(
                PdfName::new("StructTreeRoot"),
                PdfObject::Ref(ObjectId(tree_num, 0)),
            );
            let mut mark_info = PdfDict::new();
            mark_info.insert(PdfName::new("Marked"), PdfObject::Bool(true));
            catalog.insert(PdfName::new("MarkInfo"), PdfObject::Dict(mark_info));
        }
        if let Some(lang) = &self.lang {
            catalog.insert(
                PdfName::new("Lang"),
                PdfObject::String(encode_text_string(lang)),
            );
        }
        objects.push((1u32, PdfObject::Dict(catalog)));

        // Object 2: Pages tree
        let mut pages_tree = PdfDict::new();
        pages_tree.insert(PdfName::new("Type"), PdfObject::Name(PdfName::new("Pages")));
        pages_tree.insert(PdfName::new("Count"), PdfObject::Integer(num_pages as i64));
        let kids: Vec<PdfObject> = page_obj_nums
            .iter()
            .map(|&n| PdfObject::Ref(ObjectId(n, 0)))
            .collect();
        pages_tree.insert(PdfName::new("Kids"), PdfObject::Array(kids));
        objects.push((2u32, PdfObject::Dict(pages_tree)));

        // Pages and their content streams
        for (page_num, content_num, content_bytes, font_names, image_refs, tagged) in &page_contents
        {
            let page_num = *page_num;
            let content_num = *content_num;
            let page_state = &self.pages[(page_num - 3) as usize];

            // Page dict
            let mut page = PdfDict::new();
            page.insert(PdfName::new("Type"), PdfObject::Name(PdfName::new("Page")));
            page.insert(PdfName::new("Parent"), PdfObject::Ref(ObjectId(2, 0)));
            page.insert(
                PdfName::new("MediaBox"),
                PdfObject::Array(vec![
                    PdfObject::Integer(0),
                    PdfObject::Integer(0),
                    PdfObject::Real(page_state.width),
                    PdfObject::Real(page_state.height),
                ]),
            );
            page.insert(
                PdfName::new("Contents"),
                PdfObject::Ref(ObjectId(content_num, 0)),
            );
            if !tagged.is_empty() {
                // The page's key into the /ParentTree /Nums is its 0-based index.
                page.insert(
                    PdfName::new("StructParents"),
                    PdfObject::Integer((page_num - 3) as i64),
                );
            }

            // Resources dict
            if !font_names.is_empty() || !image_refs.is_empty() {
                let mut resources = PdfDict::new();

                if !font_names.is_empty() {
                    let mut fonts = PdfDict::new();
                    for (i, font_name) in font_names.iter().enumerate() {
                        let font_num = font_obj_by_name[font_name];
                        fonts.insert(
                            PdfName::new(format!("F{}", i + 1)),
                            PdfObject::Ref(ObjectId(font_num, 0)),
                        );
                    }
                    resources.insert(PdfName::new("Font"), PdfObject::Dict(fonts));
                }

                if !image_refs.is_empty() {
                    let mut xobjects = PdfDict::new();
                    for (i, img_ref) in image_refs.iter().enumerate() {
                        xobjects.insert(
                            PdfName::new(format!("Im{}", i + 1)),
                            PdfObject::Ref(ObjectId(*img_ref, 0)),
                        );
                    }
                    resources.insert(PdfName::new("XObject"), PdfObject::Dict(xobjects));
                }

                page.insert(PdfName::new("Resources"), PdfObject::Dict(resources));
            }

            objects.push((page_num, PdfObject::Dict(page)));

            // Content stream (compressed)
            let mut content_dict = PdfDict::new();
            content_dict.insert(
                PdfName::new("Filter"),
                PdfObject::Name(PdfName::new("FlateDecode")),
            );
            let compressed = flate_compress(content_bytes)?;
            streams.push((content_num, content_dict, compressed));
        }

        // Image XObject streams. RGBA images need an extra SMask object,
        // allocated here (before serialization sizes the xref).
        for (num, image) in &image_objects {
            match image {
                ImageData::Jpeg {
                    data,
                    width,
                    height,
                    components,
                } => {
                    let color_space = match components {
                        1 => "DeviceGray",
                        3 => "DeviceRGB",
                        _ => {
                            return Err(zpdf_core::Error::Io(std::io::Error::new(
                                std::io::ErrorKind::InvalidInput,
                                "JPEG must have 1 or 3 components",
                            )))
                        }
                    };
                    let mut dict = image_xobject_dict(*width, *height, color_space);
                    dict.insert(
                        PdfName::new("Filter"),
                        PdfObject::Name(PdfName::new("DCTDecode")),
                    );
                    streams.push((*num, dict, data.clone()));
                }
                ImageData::Rgb8 {
                    width,
                    height,
                    pixels,
                } => {
                    let expected = (*width as usize)
                        .checked_mul(*height as usize)
                        .and_then(|n| n.checked_mul(3));
                    if expected != Some(pixels.len()) {
                        return Err(zpdf_core::Error::Io(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "RGB buffer size does not match dimensions",
                        )));
                    }
                    let mut dict = image_xobject_dict(*width, *height, "DeviceRGB");
                    dict.insert(
                        PdfName::new("Filter"),
                        PdfObject::Name(PdfName::new("FlateDecode")),
                    );
                    streams.push((*num, dict, flate_compress(pixels)?));
                }
                ImageData::Rgba8 {
                    width,
                    height,
                    pixels,
                } => {
                    let expected = (*width as usize)
                        .checked_mul(*height as usize)
                        .and_then(|n| n.checked_mul(4));
                    if expected != Some(pixels.len()) {
                        return Err(zpdf_core::Error::Io(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "RGBA buffer size does not match dimensions",
                        )));
                    }
                    let mut rgb = Vec::with_capacity(pixels.len() / 4 * 3);
                    let mut alpha = Vec::with_capacity(pixels.len() / 4);
                    for chunk in pixels.as_chunks::<4>().0 {
                        rgb.extend_from_slice(&chunk[..3]);
                        alpha.push(chunk[3]);
                    }
                    let smask_num = obj_num;
                    obj_num += 1;
                    let mut mask_dict = image_xobject_dict(*width, *height, "DeviceGray");
                    mask_dict.insert(
                        PdfName::new("Filter"),
                        PdfObject::Name(PdfName::new("FlateDecode")),
                    );
                    streams.push((smask_num, mask_dict, flate_compress(&alpha)?));

                    let mut dict = image_xobject_dict(*width, *height, "DeviceRGB");
                    dict.insert(
                        PdfName::new("Filter"),
                        PdfObject::Name(PdfName::new("FlateDecode")),
                    );
                    dict.insert(
                        PdfName::new("SMask"),
                        PdfObject::Ref(ObjectId(smask_num, 0)),
                    );
                    streams.push((*num, dict, flate_compress(&rgb)?));
                }
            }
        }

        // Serialize to PDF
        let mut out = Vec::new();
        out.extend_from_slice(b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n");

        let mut offsets = vec![0u64; (obj_num + 1) as usize];

        // Write all objects
        for (num, obj) in &objects {
            offsets[*num as usize] = out.len() as u64;
            crate::serialize::write_object(&mut out, *num, 0, obj).map_err(zpdf_core::Error::Io)?;
        }

        // Write all streams
        for (num, dict, data) in &streams {
            offsets[*num as usize] = out.len() as u64;
            crate::serialize::write_stream(&mut out, *num, 0, dict, data)
                .map_err(zpdf_core::Error::Io)?;
        }

        // Write xref and trailer. `obj_num` is the next unused number, so the
        // table covers objects 0..obj_num-1.
        let xref_pos = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n", obj_num).as_bytes());
        out.extend_from_slice(b"0000000000 65535 f \n");
        for i in 1..obj_num {
            let offset = offsets[i as usize];
            if offset > 9_999_999_999 {
                return Err(zpdf_core::Error::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "xref offset too large",
                )));
            }
            out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }

        let mut trailer = PdfDict::new();
        trailer.insert(PdfName::new("Size"), PdfObject::Integer(obj_num as i64));
        trailer.insert(PdfName::new("Root"), PdfObject::Ref(ObjectId(1, 0)));
        out.extend_from_slice(b"trailer\n");
        crate::serialize::serialize_dict(&mut out, &trailer).map_err(zpdf_core::Error::Io)?;
        out.extend_from_slice(format!("\nstartxref\n{xref_pos}\n%%EOF\n").as_bytes());

        Ok(out)
    }

    /// For each composite (Type0/Identity-H) embedded font, parse the font
    /// once and build a `char → GID` map covering every character used with
    /// that font across all pages. Used by [`build_page_content`] to emit the
    /// 2-byte GID codes that Identity-H addresses. Characters with no glyph
    /// map to GID 0 (`.notdef`), which keeps text positioning stable.
    /// For each composite (Type0) embedded font, parse the font once and build
    /// a `char → 2-byte code` map covering every character used with that font
    /// across all pages. The 2-byte code is what [`build_page_content`] emits:
    /// the GID for `Identity` encodings, or the Unicode scalar (BMP only) for
    /// `Predefined` CMaps. Characters with no glyph (or, for predefined CMaps,
    /// outside the BMP) map to code 0 (`.notdef`), keeping text positioning
    /// stable. Also drives subsetting (the kept GIDs) and `/W` / `/W2` width
    /// arrays. Returns, per font index, the glyph map plus a flag for vertical
    /// writing mode (so the content stream can apply the vertical `Tm` matrix).
    fn build_composite_glyph_maps(&self) -> HashMap<u32, (HashMap<char, u16>, bool)> {
        let mut out: HashMap<u32, (HashMap<char, u16>, bool)> = HashMap::new();
        for (idx, font) in self.embedded_fonts.iter().enumerate() {
            let EmbeddedKind::Composite {
                encoding, vertical, ..
            } = &font.kind
            else {
                continue;
            };
            let marker = format!("\u{0}EMB{}", idx);
            // Collect the characters used with this font on every page.
            let used: HashSet<char> = self
                .pages
                .iter()
                .flat_map(|p| &p.items)
                .filter_map(|item| match item {
                    PageItem::Text {
                        text, font_name, ..
                    } if font_name == &marker => Some(text.chars()),
                    _ => None,
                })
                .flatten()
                .collect();
            if used.is_empty() {
                out.insert(idx as u32, (HashMap::new(), *vertical));
                continue;
            }
            let Ok(face) = ttf_parser::Face::parse(&font.data, 0) else {
                continue;
            };
            let mut map: HashMap<char, u16> = HashMap::with_capacity(used.len());
            for ch in used {
                let code = match encoding {
                    CidEncoding::Identity => face.glyph_index(ch).map(|g| g.0).unwrap_or(0),
                    // Predefined Unicode CMaps address the BMP; supplementary
                    // plane characters cannot be 2-byte-encoded and fall back
                    // to .notdef. (Use Identity-H/V for full-plane coverage.)
                    CidEncoding::Predefined(_) => {
                        if (ch as u32) <= 0xFFFF {
                            ch as u16
                        } else {
                            0
                        }
                    }
                };
                map.insert(ch, code);
            }
            out.insert(idx as u32, (map, *vertical));
        }
        out
    }

    #[allow(clippy::type_complexity)] // flat build tuple; factoring a type alias would obscure it
    fn build_page_content(
        &self,
        page_state: &PageState,
        image_counter: &mut usize,
        next_obj: &mut u32,
        composite_glyph_maps: &HashMap<u32, (HashMap<char, u16>, bool)>,
    ) -> Result<(Vec<u8>, Vec<String>, Vec<u32>, Vec<(i32, TagSpec)>)> {
        let mut ops = Vec::new();
        let mut font_names = Vec::new();
        let mut image_refs = Vec::new();
        let mut used_fonts = HashMap::new();
        let mut used_images = HashMap::new();
        // Per-page marked-content identifiers for tagged items, assigned in
        // item order (0, 1, 2, …). The /StructTreeRoot's /ParentTree maps each
        // page's /StructParents key to an array of /StructElem refs indexed by
        // MCID, so the order here must match the order the struct elements are
        // emitted in build().
        let mut next_mcid: i32 = 0;
        let mut tagged: Vec<(i32, TagSpec)> = Vec::new();

        ops.extend_from_slice(b"BT\n");

        for item in &page_state.items {
            match item {
                PageItem::Text {
                    text,
                    x,
                    y,
                    font_name,
                    size,
                    color,
                    tag,
                } => {
                    // Ensure font is in the resources
                    let font_idx = if let Some(&idx) = used_fonts.get(font_name) {
                        idx
                    } else {
                        let idx = used_fonts.len() + 1;
                        used_fonts.insert(font_name.clone(), idx);
                        font_names.push(font_name.clone());
                        idx
                    };

                    if let Some(spec) = tag {
                        let mcid = next_mcid;
                        next_mcid += 1;
                        tagged.push((mcid, spec.clone()));
                        ops.extend_from_slice(
                            format!("/{} <</MCID {}>> BDC\n", spec.role.as_str(), mcid).as_bytes(),
                        );
                    }

                    // Emit text ops
                    let r = color.0.clamp(0.0, 1.0);
                    let g = color.1.clamp(0.0, 1.0);
                    let b = color.2.clamp(0.0, 1.0);
                    ops.extend_from_slice(format!("{} {} {} rg\n", r, g, b).as_bytes());
                    ops.extend_from_slice(format!("/F{} {} Tf\n", font_idx, size).as_bytes());
                    // Composite (Type0) fonts emit 2-byte codes as a hex string;
                    // simple/standard-14 fonts use a WinAnsi literal string.
                    let composite = font_name
                        .strip_prefix("\u{0}EMB")
                        .and_then(|s| s.parse::<u32>().ok())
                        .and_then(|idx| composite_glyph_maps.get(&idx));
                    if let Some((gmap, vertical)) = composite {
                        // Vertical writing mode (Identity-V / predefined -V):
                        // rotate the text matrix 90° CCW so the baseline runs
                        // top-to-bottom and glyphs advance downward.
                        if *vertical {
                            ops.extend_from_slice(format!("0 1 -1 0 {} {} Tm\n", x, y).as_bytes());
                        } else {
                            ops.extend_from_slice(format!("1 0 0 1 {} {} Tm\n", x, y).as_bytes());
                        }
                        ops.extend_from_slice(b"<");
                        for ch in text.chars() {
                            let code = gmap.get(&ch).copied().unwrap_or(0);
                            ops.extend_from_slice(format!("{code:04X}").as_bytes());
                        }
                        ops.extend_from_slice(b"> Tj\n");
                    } else {
                        ops.extend_from_slice(format!("1 0 0 1 {} {} Tm\n", x, y).as_bytes());
                        ops.extend_from_slice(b"(");
                        escape_text(text, &mut ops);
                        ops.extend_from_slice(b") Tj\n");
                    }

                    if tag.is_some() {
                        ops.extend_from_slice(b"EMC\n");
                    }
                }
                PageItem::Image {
                    image: _,
                    x,
                    y,
                    width,
                    height,
                    tag,
                } => {
                    // End text mode
                    ops.extend_from_slice(b"ET\n");

                    // Get or create image object
                    let image_key = *image_counter;
                    *image_counter += 1;

                    let img_ref = *next_obj;
                    *next_obj += 1;
                    used_images.insert(image_key, img_ref);
                    image_refs.push(img_ref);

                    // Emit image ops (use placeholder; actual image objects added externally)
                    ops.extend_from_slice(b"q\n");
                    if let Some(spec) = tag {
                        let mcid = next_mcid;
                        next_mcid += 1;
                        tagged.push((mcid, spec.clone()));
                        ops.extend_from_slice(
                            format!("/{} <</MCID {}>> BDC\n", spec.role.as_str(), mcid).as_bytes(),
                        );
                    }
                    ops.extend_from_slice(
                        format!("{} 0 0 {} {} {} cm\n", width, height, x, y).as_bytes(),
                    );
                    ops.extend_from_slice(format!("/Im{} Do\n", used_images.len()).as_bytes());
                    if tag.is_some() {
                        ops.extend_from_slice(b"EMC\n");
                    }
                    ops.extend_from_slice(b"Q\n");

                    // Restart text mode
                    ops.extend_from_slice(b"BT\n");
                }
                PageItem::Path {
                    segments,
                    style,
                    tag,
                } => {
                    // Paths are painted outside the text block.
                    ops.extend_from_slice(b"ET\nq\n");
                    if let Some(spec) = tag {
                        let mcid = next_mcid;
                        next_mcid += 1;
                        tagged.push((mcid, spec.clone()));
                        ops.extend_from_slice(
                            format!("/{} <</MCID {}>> BDC\n", spec.role.as_str(), mcid).as_bytes(),
                        );
                    }
                    if let Some((r, g, b)) = style.stroke {
                        ops.extend_from_slice(format!("{} {} {} RG\n", r, g, b).as_bytes());
                        ops.extend_from_slice(format!("{} w\n", style.line_width).as_bytes());
                    }
                    if let Some((r, g, b)) = style.fill {
                        ops.extend_from_slice(format!("{} {} {} rg\n", r, g, b).as_bytes());
                    }
                    for seg in segments {
                        match seg {
                            PathSegment::MoveTo { x, y } => {
                                ops.extend_from_slice(format!("{} {} m\n", x, y).as_bytes());
                            }
                            PathSegment::LineTo { x, y } => {
                                ops.extend_from_slice(format!("{} {} l\n", x, y).as_bytes());
                            }
                            PathSegment::CurveTo {
                                x1,
                                y1,
                                x2,
                                y2,
                                x3,
                                y3,
                            } => {
                                ops.extend_from_slice(
                                    format!("{} {} {} {} {} {} c\n", x1, y1, x2, y2, x3, y3)
                                        .as_bytes(),
                                );
                            }
                            PathSegment::Rect {
                                x,
                                y,
                                width,
                                height,
                            } => {
                                ops.extend_from_slice(
                                    format!("{} {} {} {} re\n", x, y, width, height).as_bytes(),
                                );
                            }
                            PathSegment::Close => ops.extend_from_slice(b"h\n"),
                        }
                    }
                    let paint_op: &[u8] = match (style.fill.is_some(), style.stroke.is_some()) {
                        (true, true) => b"B\n",
                        (true, false) => b"f\n",
                        (false, true) => b"S\n",
                        (false, false) => b"n\n",
                    };
                    ops.extend_from_slice(paint_op);
                    if tag.is_some() {
                        ops.extend_from_slice(b"EMC\n");
                    }
                    ops.extend_from_slice(b"Q\nBT\n");
                }
            }
        }

        ops.extend_from_slice(b"ET\n");

        Ok((ops, font_names, image_refs, tagged))
    }
}

impl Default for DocumentBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Classify a font file's outline flavor as [`DocumentBuilder::embed_composite_font`]
/// would: CFF (an OTF `CFF ` table or raw CFF) → [`FontProgram::OpenTypeCff`];
/// a TrueType sfnt (`\x00\x01\x00\x00` / `true`) → [`FontProgram::TrueType`].
/// Returns `None` for an unrecognized format. Useful for tests and callers
/// that want to predict the descendant `/Subtype` (`CIDFontType0` vs
/// `CIDFontType2`) before embedding.
pub fn classify_font_program(font: &[u8]) -> Option<FontProgram> {
    if crate::cff::is_cff_flavored(font) {
        Some(FontProgram::OpenTypeCff)
    } else if font.starts_with(&[0x00, 0x01, 0x00, 0x00]) || font.starts_with(b"true") {
        Some(FontProgram::TrueType)
    } else {
        None
    }
}

/// WinAnsiEncoding (CP1252) code → Unicode char. Latin-1 except 0x80–0x9F.
fn winansi_code_to_char(code: u8) -> Option<char> {
    match code {
        0x80 => Some('\u{20AC}'),
        0x82 => Some('\u{201A}'),
        0x83 => Some('\u{0192}'),
        0x84 => Some('\u{201E}'),
        0x85 => Some('\u{2026}'),
        0x86 => Some('\u{2020}'),
        0x87 => Some('\u{2021}'),
        0x88 => Some('\u{02C6}'),
        0x89 => Some('\u{2030}'),
        0x8A => Some('\u{0160}'),
        0x8B => Some('\u{2039}'),
        0x8C => Some('\u{0152}'),
        0x8E => Some('\u{017D}'),
        0x91 => Some('\u{2018}'),
        0x92 => Some('\u{2019}'),
        0x93 => Some('\u{201C}'),
        0x94 => Some('\u{201D}'),
        0x95 => Some('\u{2022}'),
        0x96 => Some('\u{2013}'),
        0x97 => Some('\u{2014}'),
        0x98 => Some('\u{02DC}'),
        0x99 => Some('\u{2122}'),
        0x9A => Some('\u{0161}'),
        0x9B => Some('\u{203A}'),
        0x9C => Some('\u{0153}'),
        0x9E => Some('\u{017E}'),
        0x9F => Some('\u{0178}'),
        0x81 | 0x8D | 0x8F | 0x90 | 0x9D => None,
        _ => Some(code as char),
    }
}

/// Build a CIDFont `/W` array from sorted `(gid, width)` entries: runs of
/// consecutive GIDs become `first [w1 w2 …]` pairs (the compact form the read
/// side parses via `parse_cid_widths`). Single entries are a one-element run.
fn build_w_array(entries: &[(u16, u16)]) -> PdfObject {
    let mut arr: Vec<PdfObject> = Vec::new();
    let mut i = 0;
    while i < entries.len() {
        let first = entries[i].0;
        let mut j = i + 1;
        while j < entries.len() && entries[j].0 == entries[j - 1].0 + 1 {
            j += 1;
        }
        let widths: Vec<PdfObject> = entries[i..j]
            .iter()
            .map(|(_, w)| PdfObject::Integer(*w as i64))
            .collect();
        arr.push(PdfObject::Integer(first as i64));
        arr.push(PdfObject::Array(widths));
        i = j;
    }
    PdfObject::Array(arr)
}

/// Build a CIDFont `/W2` array from sorted `(cid, w1y, vx, vy)` entries. Same
/// run-encoding shape as `/W`, but each width entry is a 3-element array
/// `[w1y vx vy]` (PDF spec Table 117): `w1y` is the top-to-bottom advance,
/// `(vx, vy)` the glyph origin offset from the CID origin.
fn build_w2_array(entries: &[(u16, i64, i64, i64)]) -> PdfObject {
    let mut arr: Vec<PdfObject> = Vec::new();
    let mut i = 0;
    while i < entries.len() {
        let first = entries[i].0;
        let mut j = i + 1;
        while j < entries.len() && entries[j].0 == entries[j - 1].0 + 1 {
            j += 1;
        }
        let metrics: Vec<PdfObject> = entries[i..j]
            .iter()
            .flat_map(|(_, w1y, vx, vy)| {
                [
                    PdfObject::Integer(*w1y),
                    PdfObject::Integer(*vx),
                    PdfObject::Integer(*vy),
                ]
            })
            .collect();
        arr.push(PdfObject::Integer(first as i64));
        arr.push(PdfObject::Array(metrics));
        i = j;
    }
    PdfObject::Array(arr)
}

/// Default `/DW2` vertical metrics `[vy w1y]` (PDF spec: origin shift `vy`,
/// vertical advance `w1y`), in 1000-unit space. Prefer the font's `vhea`
/// ascender/descender when the table is present (a real vertical metrics
/// table); otherwise the PDF default `[880 −1000]`. The advance is negative
/// because vertical text advances downward in PDF's bottom-left origin.
fn dw2_default(face: &ttf_parser::Face, to_milli: f64) -> [i64; 2] {
    let asc = face.vertical_ascender().map(|v| v as f64).unwrap_or(880.0) * to_milli;
    let desc = face
        .vertical_descender()
        .map(|v| v as f64)
        .unwrap_or(-120.0)
        * to_milli;
    // w1y = -(asc - desc): a full em advance downward. Guard against a
    // non-positive range (degenerate vhea) by falling back to the PDF default.
    let w1y_raw = -(asc - desc);
    let w1y = if w1y_raw.abs() > 1.0 {
        w1y_raw.round() as i64
    } else {
        -1000
    };
    // vy: the vertical origin offset — half the em above the baseline, which
    // places the glyph centred on the vertical advance. Default 880 per spec.
    let vy = if asc.abs() > 1.0 {
        asc.round() as i64
    } else {
        880
    };
    [vy, w1y]
}

/// Per-glyph vertical metric `(w1y, vx, vy)` for `gid` from the font's `vmtx`
/// table when present, in 1000-unit space. `w1y` is the vertical advance
/// (negative, downward); `(vx, vy)` is the origin offset — `vx` centres the
/// glyph horizontally (half the advance width), `vy` is the ascender. When the
/// font has no vertical metrics, returns `(0, 0, 0)` so the caller can emit
/// only entries that differ from `/DW2` (and let `/DW2` cover the rest).
fn vertical_glyph_metric(
    face: &ttf_parser::Face,
    gid: ttf_parser::GlyphId,
    to_milli: f64,
) -> (i64, i64, i64) {
    // ttf-parser exposes vertical advance via glyph_hor_advance only for the
    // horizontal case; vertical advance comes from vmtx, which the Face reads
    // when a vhea table is present. Fall back to /DW2 signalling (0) otherwise.
    let Some(v_adv) = face.vertical_ascender() else {
        return (0, 0, 0);
    };
    // Per-glyph vertical advance: ttf-parser does not expose glyph_vert_advance
    // directly in 0.25; approximate with the font-wide vertical em (the common
    // case for CJK, where vmtx advances are uniform full-em). Centroid origin:
    // vx = half the horizontal advance (centres the glyph), vy = ascender.
    let h_adv = face
        .glyph_hor_advance(gid)
        .map(|a| a as f64 * to_milli)
        .unwrap_or(1000.0);
    let upem = face.units_per_em() as f64;
    let to_milli_v = if upem > 0.0 { 1000.0 / upem } else { 1.0 };
    let w1y = -((v_adv as f64) * to_milli_v).round() as i64;
    let vx = (h_adv / 2.0).round() as i64;
    let vy = ((v_adv as f64) * to_milli_v).round() as i64;
    (w1y, vx, vy)
}

/// Serialize an explicit `/CIDToGIDMap` table for a predefined-CMap font where
/// CID = Unicode scalar. The map is a binary table of 2-byte big-endian GID
/// values indexed by CID; only CIDs in `entries` are populated, with GID 0 for
/// any gap (the read side reads `gid = u16::from_be_bytes(table[2*cid..])`).
/// We emit a dense table from 0..=max_cid so the indexing is a simple offset.
fn build_cid_to_gid_map(entries: &[(u16, u16)]) -> Vec<u8> {
    let max_cid = entries.iter().map(|(c, _)| *c).max().unwrap_or(0);
    let mut table = vec![0u8; (max_cid as usize + 1) * 2];
    for &(cid, gid) in entries {
        let off = cid as usize * 2;
        table[off] = (gid >> 8) as u8;
        table[off + 1] = (gid & 0xff) as u8;
    }
    table
}

fn image_xobject_dict(width: u32, height: u32, color_space: &str) -> PdfDict {
    let mut dict = PdfDict::new();
    dict.insert(
        PdfName::new("Type"),
        PdfObject::Name(PdfName::new("XObject")),
    );
    dict.insert(
        PdfName::new("Subtype"),
        PdfObject::Name(PdfName::new("Image")),
    );
    dict.insert(PdfName::new("Width"), PdfObject::Integer(width as i64));
    dict.insert(PdfName::new("Height"), PdfObject::Integer(height as i64));
    dict.insert(
        PdfName::new("ColorSpace"),
        PdfObject::Name(PdfName::new(color_space)),
    );
    dict.insert(PdfName::new("BitsPerComponent"), PdfObject::Integer(8));
    dict
}

fn flate_compress(data: &[u8]) -> Result<Vec<u8>> {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    use std::io::Write;
    enc.write_all(data).map_err(zpdf_core::Error::Io)?;
    enc.finish().map_err(zpdf_core::Error::Io)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes_contain(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn minimal_pdf() {
        let mut builder = DocumentBuilder::new();
        let _page = builder.add_page(612.0, 792.0);
        let pdf = builder.build().unwrap();
        assert!(pdf.starts_with(b"%PDF-1.7"));
        assert!(pdf.ends_with(b"%%EOF\n"));
        assert!(pdf.len() > 200);
    }

    #[test]
    fn with_text() {
        let mut builder = DocumentBuilder::new();
        let page = builder.add_page(612.0, 792.0);
        builder
            .add_text(
                page,
                "Hello, PDF!",
                50.0,
                700.0,
                "Helvetica",
                24.0,
                (0.0, 0.0, 0.0),
            )
            .unwrap();
        let pdf = builder.build().unwrap();
        assert!(bytes_contain(&pdf, b"Helvetica"));
    }

    #[test]
    fn multi_page() {
        let mut builder = DocumentBuilder::new();
        builder.add_page(612.0, 792.0);
        builder.add_page(612.0, 792.0);
        builder.add_page(400.0, 600.0);
        let pdf = builder.build().unwrap();
        assert!(bytes_contain(&pdf, b"/Count 3"));
    }

    #[test]
    fn no_pages_error() {
        let builder = DocumentBuilder::new();
        assert!(builder.build().is_err());
    }

    #[test]
    fn invalid_page_handle() {
        let mut builder = DocumentBuilder::new();
        let page = PageHandle(999);
        let result = builder.add_text(
            page,
            "test",
            50.0,
            700.0,
            "Helvetica",
            12.0,
            (0.0, 0.0, 0.0),
        );
        assert!(result.is_err());
    }

    #[test]
    fn font_alias_normalization() {
        let mut builder = DocumentBuilder::new();
        let page = builder.add_page(612.0, 792.0);
        // Use alias "Arial" which should normalize to "Helvetica"
        builder
            .add_text(page, "test", 50.0, 700.0, "Arial", 12.0, (0.0, 0.0, 0.0))
            .unwrap();
        let pdf = builder.build().unwrap();
        // The alias should be normalized internally
        assert!(bytes_contain(&pdf, b"Helvetica"));
    }
}
