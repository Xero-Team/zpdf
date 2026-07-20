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

use std::collections::HashMap;

use zpdf_core::{ObjectId, PdfDict, PdfName, PdfObject, Result};
use zpdf_document::escape_text;

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

/// Item to place on a page.
enum PageItem {
    Text {
        text: String,
        x: f64,
        y: f64,
        font_name: String,
        size: f64,
        color: (f64, f64, f64),
    },
    Image {
        image: ImageData,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    },
    Path {
        segments: Vec<PathSegment>,
        style: PathStyle,
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

/// An embedded TrueType font, parsed once at `embed_font` time.
struct EmbeddedFont {
    /// PostScript-style name used as /BaseFont.
    ps_name: String,
    /// The raw font file (embedded verbatim as FontFile2).
    data: Vec<u8>,
    /// WinAnsi code → advance width in 1/1000 em units.
    widths: [u16; 256],
    ascent: f64,
    descent: f64,
    cap_height: f64,
    bbox: [f64; 4],
    italic_angle: f64,
}

/// A handle to an embedded font, returned by [`DocumentBuilder::embed_font`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EmbeddedFontHandle(u32);

/// Build new PDFs from scratch.
pub struct DocumentBuilder {
    pages: Vec<PageState>,
    embedded_fonts: Vec<EmbeddedFont>,
}

impl DocumentBuilder {
    /// Create a new document builder.
    pub fn new() -> Self {
        Self {
            pages: Vec::new(),
            embedded_fonts: Vec::new(),
        }
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
            widths,
            ascent,
            descent,
            cap_height,
            bbox,
            italic_angle,
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
            page_state.items.push(PageItem::Path { segments, style });
            Ok(())
        } else {
            Err(zpdf_core::Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "page handle not found",
            )))
        }
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

        for (page_idx, page_state) in self.pages.iter().enumerate() {
            let (content_bytes, font_names, image_refs) =
                self.build_page_content(page_state, &mut image_counter, &mut obj_num)?;

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
            ));
        }

        // Font dicts are emitted as indirect objects (many parsers, including
        // zpdf's own resource loader, only follow /Font entries that are
        // references). Dedup per document by BaseFont name.
        let mut font_obj_by_name: HashMap<String, u32> = HashMap::new();
        for (_, _, _, font_names, _) in &page_contents {
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

                // Subset to the characters actually shown with this font
                // (sparse-glyf: unused outlines dropped, metrics preserved).
                // Fall back to the full file when subsetting isn't possible.
                let used: std::collections::HashSet<char> = self
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
                    PdfObject::Array(font.bbox.iter().map(|&v| PdfObject::Real(v)).collect()),
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
                font_dict.insert(PdfName::new("Type"), PdfObject::Name(PdfName::new("Font")));
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
                        font.widths[32..=255]
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

        // Object 1: Catalog
        let mut catalog = PdfDict::new();
        catalog.insert(
            PdfName::new("Type"),
            PdfObject::Name(PdfName::new("Catalog")),
        );
        catalog.insert(PdfName::new("Pages"), PdfObject::Ref(ObjectId(2, 0)));
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
        for (page_num, content_num, content_bytes, font_names, image_refs) in page_contents {
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
            let compressed = flate_compress(&content_bytes)?;
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
                    for chunk in pixels.chunks_exact(4) {
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

    fn build_page_content(
        &self,
        page_state: &PageState,
        image_counter: &mut usize,
        next_obj: &mut u32,
    ) -> Result<(Vec<u8>, Vec<String>, Vec<u32>)> {
        let mut ops = Vec::new();
        let mut font_names = Vec::new();
        let mut image_refs = Vec::new();
        let mut used_fonts = HashMap::new();
        let mut used_images = HashMap::new();

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

                    // Emit text ops
                    let r = color.0.clamp(0.0, 1.0);
                    let g = color.1.clamp(0.0, 1.0);
                    let b = color.2.clamp(0.0, 1.0);
                    ops.extend_from_slice(format!("{} {} {} rg\n", r, g, b).as_bytes());
                    ops.extend_from_slice(format!("/F{} {} Tf\n", font_idx, size).as_bytes());
                    ops.extend_from_slice(format!("1 0 0 1 {} {} Tm\n", x, y).as_bytes());
                    ops.extend_from_slice(b"(");
                    escape_text(text, &mut ops);
                    ops.extend_from_slice(b") Tj\n");
                }
                PageItem::Image {
                    image: _,
                    x,
                    y,
                    width,
                    height,
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
                    ops.extend_from_slice(
                        format!("{} 0 0 {} {} {} cm\n", width, height, x, y).as_bytes(),
                    );
                    ops.extend_from_slice(format!("/Im{} Do\n", used_images.len()).as_bytes());
                    ops.extend_from_slice(b"Q\n");

                    // Restart text mode
                    ops.extend_from_slice(b"BT\n");
                }
                PageItem::Path { segments, style } => {
                    // Paths are painted outside the text block.
                    ops.extend_from_slice(b"ET\nq\n");
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
                    ops.extend_from_slice(b"Q\nBT\n");
                }
            }
        }

        ops.extend_from_slice(b"ET\n");

        Ok((ops, font_names, image_refs))
    }
}

impl Default for DocumentBuilder {
    fn default() -> Self {
        Self::new()
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
