//! PDF to PowerPoint (.pptx) export with editable content preservation.
//!
//! This crate converts PDF pages into PowerPoint slides, extracting text with
//! positioning, shapes, and images into native OOXML format rather than simply
//! rasterizing pages to images. The goal is to produce editable presentations.

mod ooxml;
mod shape_recognizer;
mod text_grouper;

use image::ImageEncoder;
use zpdf_core::{Rect, Result};
use zpdf_display_list::{DisplayList, RenderCommand};
use zpdf_font::FontCache;
use zpdf_image::ImageCache;

pub use ooxml::PptxWriter;

/// A PowerPoint presentation built from PDF pages.
pub struct PptxPresentation {
    pub slides: Vec<PptxSlide>,
    pub width_emu: i64,  // Width in EMUs (English Metric Units, 914400 per inch)
    pub height_emu: i64, // Height in EMUs
}

/// A single PowerPoint slide corresponding to a PDF page.
pub struct PptxSlide {
    pub page_index: usize,
    pub elements: Vec<SlideElement>,
}

/// An element on a PowerPoint slide.
#[derive(Debug, Clone)]
pub enum SlideElement {
    TextBox {
        text: String,
        x_emu: i64,
        y_emu: i64,
        width_emu: i64,
        height_emu: i64,
        font_family: String,
        font_size_pt: f64,
        color_rgb: (u8, u8, u8),
        bold: bool,
        italic: bool,
    },
    Rectangle {
        x_emu: i64,
        y_emu: i64,
        width_emu: i64,
        height_emu: i64,
        fill_rgb: Option<(u8, u8, u8)>,
        stroke_rgb: Option<(u8, u8, u8)>,
        stroke_width_pt: f64,
    },
    Ellipse {
        x_emu: i64,
        y_emu: i64,
        width_emu: i64,
        height_emu: i64,
        fill_rgb: Option<(u8, u8, u8)>,
        stroke_rgb: Option<(u8, u8, u8)>,
        stroke_width_pt: f64,
    },
    Line {
        x1_emu: i64,
        y1_emu: i64,
        x2_emu: i64,
        y2_emu: i64,
        stroke_rgb: (u8, u8, u8),
        stroke_width_pt: f64,
    },
    FreeformPath {
        path_data: String, // SVG-style path data
        fill_rgb: Option<(u8, u8, u8)>,
        stroke_rgb: Option<(u8, u8, u8)>,
        stroke_width_pt: f64,
    },
    Image {
        x_emu: i64,
        y_emu: i64,
        width_emu: i64,
        height_emu: i64,
        image_data: Vec<u8>, // PNG data
        image_id: String,
    },
}

/// Options for PDF to PowerPoint conversion.
#[derive(Debug, Clone)]
pub struct ConversionOptions {
    /// Minimum text height (in points) to recognize as separate text boxes.
    /// Smaller text might be grouped together.
    pub min_text_box_height: f64,

    /// Maximum horizontal gap (in points) to merge text runs into one box.
    pub text_merge_gap: f64,

    /// Simplify rectangular paths to Rectangle shapes (vs FreeformPath).
    pub recognize_rectangles: bool,

    /// Simplify elliptical paths to Ellipse shapes (vs FreeformPath).
    pub recognize_ellipses: bool,
}

impl Default for ConversionOptions {
    fn default() -> Self {
        Self {
            min_text_box_height: 8.0,
            text_merge_gap: 2.0,
            recognize_rectangles: true,
            recognize_ellipses: true,
        }
    }
}

/// Convert a PDF display list to PowerPoint slide elements.
pub fn display_list_to_slide(
    display_list: &DisplayList,
    page_index: usize,
    font_cache: &FontCache,
    image_cache: &ImageCache,
    options: &ConversionOptions,
) -> Result<PptxSlide> {
    let mut converter = SlideConverter {
        page_rect: display_list.page_rect,
        page_index,
        font_cache,
        image_cache,
        options,
        elements: Vec::new(),
        image_counter: 0,
    };

    converter.convert_commands(&display_list.commands)?;

    Ok(PptxSlide {
        page_index,
        elements: converter.elements,
    })
}

struct SlideConverter<'a> {
    page_rect: Rect,
    #[allow(dead_code)]
    page_index: usize,
    font_cache: &'a FontCache,
    image_cache: &'a ImageCache,
    options: &'a ConversionOptions,
    elements: Vec<SlideElement>,
    image_counter: usize,
}

impl<'a> SlideConverter<'a> {
    fn convert_commands(&mut self, commands: &[RenderCommand]) -> Result<()> {
        for cmd in commands {
            match cmd {
                RenderCommand::DrawGlyphRun(glyph_run) => {
                    self.convert_glyph_run(glyph_run)?;
                }
                RenderCommand::FillPath { path, paint, .. } => {
                    self.convert_fill_path(path, paint)?;
                }
                RenderCommand::StrokePath {
                    path, style, paint, ..
                } => {
                    self.convert_stroke_path(path, style, paint)?;
                }
                RenderCommand::DrawImage(image_draw) => {
                    self.convert_image(image_draw)?;
                }
                // Skip clip/blend commands - we're extracting content only
                _ => {}
            }
        }
        Ok(())
    }

    fn convert_glyph_run(&mut self, glyph_run: &zpdf_display_list::GlyphRun) -> Result<()> {
        use zpdf_display_list::Paint;

        // Extract text from glyphs
        let font = self
            .font_cache
            .get(glyph_run.font_id)
            .ok_or_else(|| zpdf_core::Error::StreamDecode("Font not found".into()))?;

        let mut text = String::new();
        let mut min_x = f32::INFINITY;
        let mut max_x = f32::NEG_INFINITY;
        let mut min_y = f32::INFINITY;
        let mut max_y = f32::NEG_INFINITY;

        for glyph in &glyph_run.glyphs {
            // /ToUnicode is keyed by the source character code, not the glyph
            // id. A simple font's one-byte code decodes exactly as in text
            // extraction (/ToUnicode, then the encoding's glyph names); a
            // composite font's code means nothing without /ToUnicode.
            match font.font_type {
                zpdf_font::PdfFontType::Type0CidType2 => {
                    if let Some(unicode) = font
                        .to_unicode
                        .as_ref()
                        .and_then(|to_unicode| to_unicode.lookup(glyph.code))
                    {
                        text.push_str(unicode);
                    }
                }
                _ => {
                    if let Ok(byte) = u8::try_from(glyph.code) {
                        text.push_str(&font.decode_to_string(&[byte]));
                    }
                }
            }

            // Track bounding box
            min_x = min_x.min(glyph.x);
            max_x = max_x.max(glyph.x + glyph.advance);
            min_y = min_y.min(glyph.y);
            max_y = max_y.max(glyph.y + glyph_run.font_size);
        }

        if text.is_empty() {
            // If we couldn't extract text, skip this glyph run
            return Ok(());
        }

        // Extract color from paint
        let color_rgb = match &glyph_run.paint {
            Paint::Solid(c) => (
                (c.r * 255.0) as u8,
                (c.g * 255.0) as u8,
                (c.b * 255.0) as u8,
            ),
            _ => (0, 0, 0), // Default to black for patterns/shadings
        };

        // Convert PDF coordinates (bottom-left origin) to PowerPoint (top-left origin)
        let (x_ppt, y_ppt) = self.pdf_to_ppt_coords(min_x as f64, max_y as f64);
        let width = (max_x - min_x) as f64;
        let height = (max_y - min_y) as f64;

        // Extract font family name from base_font
        let font_family = font.base_font.clone();

        self.elements.push(SlideElement::TextBox {
            text,
            x_emu: pt_to_emu(x_ppt),
            y_emu: pt_to_emu(y_ppt),
            width_emu: pt_to_emu(width),
            height_emu: pt_to_emu(height),
            font_family,
            font_size_pt: glyph_run.font_size as f64,
            color_rgb,
            bold: false, // Font style detection not easily available from LoadedFont
            italic: false,
        });

        Ok(())
    }

    fn convert_fill_path(
        &mut self,
        path: &zpdf_display_list::Path,
        paint: &zpdf_display_list::Paint,
    ) -> Result<()> {
        use zpdf_display_list::Paint;

        let fill_rgb = match paint {
            Paint::Solid(c) => Some((
                (c.r * 255.0) as u8,
                (c.g * 255.0) as u8,
                (c.b * 255.0) as u8,
            )),
            _ => None,
        };

        // Try to recognize simple shapes
        if self.options.recognize_rectangles {
            if let Some(rect) = shape_recognizer::recognize_rectangle(path) {
                let (x_ppt, y_ppt) = self.pdf_to_ppt_coords(rect.x0, rect.y1);
                self.elements.push(SlideElement::Rectangle {
                    x_emu: pt_to_emu(x_ppt),
                    y_emu: pt_to_emu(y_ppt),
                    width_emu: pt_to_emu(rect.width()),
                    height_emu: pt_to_emu(rect.height()),
                    fill_rgb,
                    stroke_rgb: None,
                    stroke_width_pt: 0.0,
                });
                return Ok(());
            }
        }

        if self.options.recognize_ellipses {
            if let Some(rect) = shape_recognizer::recognize_ellipse(path) {
                let (x_ppt, y_ppt) = self.pdf_to_ppt_coords(rect.x0, rect.y1);
                self.elements.push(SlideElement::Ellipse {
                    x_emu: pt_to_emu(x_ppt),
                    y_emu: pt_to_emu(y_ppt),
                    width_emu: pt_to_emu(rect.width()),
                    height_emu: pt_to_emu(rect.height()),
                    fill_rgb,
                    stroke_rgb: None,
                    stroke_width_pt: 0.0,
                });
                return Ok(());
            }
        }

        // Fall back to freeform path
        let path_data = self.path_to_svg(path);
        self.elements.push(SlideElement::FreeformPath {
            path_data,
            fill_rgb,
            stroke_rgb: None,
            stroke_width_pt: 0.0,
        });

        Ok(())
    }

    fn convert_stroke_path(
        &mut self,
        path: &zpdf_display_list::Path,
        style: &zpdf_display_list::StrokeStyle,
        paint: &zpdf_display_list::Paint,
    ) -> Result<()> {
        use zpdf_display_list::Paint;

        let stroke_rgb = match paint {
            Paint::Solid(c) => Some((
                (c.r * 255.0) as u8,
                (c.g * 255.0) as u8,
                (c.b * 255.0) as u8,
            )),
            _ => None,
        };

        // Check if it's a simple line
        if let Some((p1, p2)) = shape_recognizer::recognize_line(path) {
            let (x1_ppt, y1_ppt) = self.pdf_to_ppt_coords(p1.x, p1.y);
            let (x2_ppt, y2_ppt) = self.pdf_to_ppt_coords(p2.x, p2.y);
            self.elements.push(SlideElement::Line {
                x1_emu: pt_to_emu(x1_ppt),
                y1_emu: pt_to_emu(y1_ppt),
                x2_emu: pt_to_emu(x2_ppt),
                y2_emu: pt_to_emu(y2_ppt),
                stroke_rgb: stroke_rgb.unwrap_or((0, 0, 0)),
                stroke_width_pt: style.width as f64,
            });
            return Ok(());
        }

        // Complex stroked path
        let path_data = self.path_to_svg(path);
        self.elements.push(SlideElement::FreeformPath {
            path_data,
            fill_rgb: None,
            stroke_rgb,
            stroke_width_pt: style.width as f64,
        });

        Ok(())
    }

    fn convert_image(&mut self, image_draw: &zpdf_display_list::ImageDraw) -> Result<()> {
        let image = self
            .image_cache
            .get(image_draw.image_id)
            .ok_or_else(|| zpdf_core::Error::StreamDecode("Image not found".into()))?;

        // Extract transform to get position and size
        let mat = &image_draw.transform;
        let x = mat.e;
        let y = mat.f;

        // Image transform: unit square [0,1]×[0,1] → page space
        let width = (mat.a * mat.a + mat.b * mat.b).sqrt();
        let height = (mat.c * mat.c + mat.d * mat.d).sqrt();

        let (x_ppt, y_ppt) = self.pdf_to_ppt_coords(x, y + height);

        // Convert image to PNG
        let png_data = image_to_png(image)?;

        self.image_counter += 1;
        let image_id = format!("image{}", self.image_counter);

        self.elements.push(SlideElement::Image {
            x_emu: pt_to_emu(x_ppt),
            y_emu: pt_to_emu(y_ppt),
            width_emu: pt_to_emu(width),
            height_emu: pt_to_emu(height),
            image_data: png_data,
            image_id,
        });

        Ok(())
    }

    /// Convert PDF coordinates (bottom-left origin, Y+ up) to PowerPoint (top-left origin, Y+ down)
    fn pdf_to_ppt_coords(&self, x: f64, y: f64) -> (f64, f64) {
        let x_ppt = x - self.page_rect.x0;
        let y_ppt = self.page_rect.y1 - y;
        (x_ppt, y_ppt)
    }

    /// Convert a display-list Path to SVG path data
    fn path_to_svg(&self, path: &zpdf_display_list::Path) -> String {
        use zpdf_display_list::PathElement;

        let mut svg = String::new();
        for elem in path {
            match elem {
                PathElement::MoveTo(p) => {
                    let (x, y) = self.pdf_to_ppt_coords(p.x, p.y);
                    svg.push_str(&format!("M {:.2} {:.2} ", x, y));
                }
                PathElement::LineTo(p) => {
                    let (x, y) = self.pdf_to_ppt_coords(p.x, p.y);
                    svg.push_str(&format!("L {:.2} {:.2} ", x, y));
                }
                PathElement::CurveTo(c1, c2, end) => {
                    let (x1, y1) = self.pdf_to_ppt_coords(c1.x, c1.y);
                    let (x2, y2) = self.pdf_to_ppt_coords(c2.x, c2.y);
                    let (x, y) = self.pdf_to_ppt_coords(end.x, end.y);
                    svg.push_str(&format!(
                        "C {:.2} {:.2} {:.2} {:.2} {:.2} {:.2} ",
                        x1, y1, x2, y2, x, y
                    ));
                }
                PathElement::Close => {
                    svg.push_str("Z ");
                }
            }
        }
        svg
    }
}

/// Convert points to EMUs (English Metric Units).
/// 1 inch = 72 points = 914400 EMUs
fn pt_to_emu(pt: f64) -> i64 {
    (pt * 12700.0) as i64
}

/// Convert an image to PNG format
fn image_to_png(image: &zpdf_image::DecodedImage) -> Result<Vec<u8>> {
    let mut png_data = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut png_data);
    encoder
        .write_image(
            &image.data,
            image.width,
            image.height,
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|e| zpdf_core::Error::StreamDecode(format!("PNG encode: {e}")))?;
    Ok(png_data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pt_to_emu_conversion() {
        assert_eq!(pt_to_emu(72.0), 914400); // 1 inch
        assert_eq!(pt_to_emu(0.0), 0);
    }
}

#[cfg(test)]
mod export_tests {
    use super::*;
    use zpdf_core::Matrix;
    use zpdf_display_list::{Color, DisplayList, GlyphRun, Paint, PositionedGlyph, RenderCommand};
    use zpdf_font::cmap::ToUnicodeMap;
    use zpdf_font::{FontCache, LoadedFont};

    /// Text of every text box on a one-run slide drawn with `font`.
    fn run_text(font: LoadedFont, glyph_id: u16, code: u32) -> String {
        let mut fonts = FontCache::new();
        let font_id = fonts.insert("F1".into(), font);
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 612.0, 792.0));
        dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
            font_id,
            font_size: 12.0,
            glyphs: vec![PositionedGlyph {
                glyph_id,
                code,
                x: 100.0,
                y: 100.0,
                advance: 6.0,
            }],
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
            transform: Matrix::identity(),
            h_scale: 1.0,
        }));
        let slide = display_list_to_slide(
            &dl,
            0,
            &fonts,
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("slide");
        slide
            .elements
            .iter()
            .filter_map(|e| match e {
                SlideElement::TextBox { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn composite_font_text_comes_from_to_unicode_by_code() {
        let font = || {
            let mut font = LoadedFont::new_placeholder("Cid".into());
            font.font_type = zpdf_font::PdfFontType::Type0CidType2;
            font.to_unicode = Some(ToUnicodeMap::parse(
                b"begincmap 1 begincodespacerange <0000> <FFFF> endcodespacerange \
                  1 beginbfchar <0102> <00E9> endbfchar endcmap",
            ));
            font
        };
        // Looking up the glyph id instead of the code finds nothing for the
        // first run and the mapped character for the second; an unmapped code
        // yields no text rather than the code read as a character.
        assert_eq!(run_text(font(), 7, 0x0102), "\u{e9}");
        assert_eq!(run_text(font(), 0x0102, 0x4E00), "");
    }

    #[test]
    fn to_unicode_is_keyed_by_character_code_not_glyph_id() {
        let mut font = LoadedFont::new_placeholder("Simple".into());
        // Map code 0x41 to a non-ASCII character, so falling back to the code
        // itself ("A") cannot pass for a ToUnicode hit.
        font.to_unicode = Some(ToUnicodeMap::parse(
            b"begincmap 1 begincodespacerange <00> <FF> endcodespacerange \
              1 beginbfchar <41> <00E9> endbfchar endcmap",
        ));
        let mut fonts = FontCache::new();
        let font_id = fonts.insert("F1".into(), font);

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 612.0, 792.0));
        dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
            font_id,
            font_size: 12.0,
            glyphs: vec![PositionedGlyph {
                glyph_id: 99,
                code: 0x41,
                x: 100.0,
                y: 100.0,
                advance: 6.0,
            }],
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
            transform: Matrix::identity(),
            h_scale: 1.0,
        }));

        let slide = display_list_to_slide(
            &dl,
            0,
            &fonts,
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .expect("slide");
        let text: String = slide
            .elements
            .iter()
            .filter_map(|e| match e {
                SlideElement::TextBox { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "\u{e9}");
    }

    fn zip_entry(archive: &mut zip::ZipArchive<std::io::Cursor<Vec<u8>>>, name: &str) -> Vec<u8> {
        use std::io::Read;
        let mut entry = archive
            .by_name(name)
            .unwrap_or_else(|_| panic!("{name} missing"));
        let mut out = Vec::new();
        entry.read_to_end(&mut out).unwrap();
        out
    }

    #[test]
    fn image_ids_are_renumbered_across_slides() {
        // Both slides number their picture "image1"; each carries different
        // bytes so a slide pointing at the other's media part is caught.
        let image = |data: &[u8]| PptxSlide {
            page_index: 0,
            elements: vec![SlideElement::Image {
                x_emu: 0,
                y_emu: 0,
                width_emu: 100,
                height_emu: 100,
                image_data: data.to_vec(),
                image_id: "image1".to_string(),
            }],
        };
        let presentation = PptxPresentation {
            slides: vec![image(b"first-slide"), image(b"second-slide")],
            width_emu: 914400,
            height_emu: 914400,
        };
        let bytes = PptxWriter::new(presentation)
            .write_to_bytes()
            .expect("write pptx");
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("zip");
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().unwrap().to_string())
            .collect();
        assert!(
            names.iter().any(|n| n == "ppt/media/image1.png"),
            "{names:?}"
        );
        assert!(
            names.iter().any(|n| n == "ppt/media/image2.png"),
            "{names:?}"
        );
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            unique.len(),
            names.len(),
            "duplicate ZIP members: {names:?}"
        );

        // Each slide's picture resolves, through its own relationship, to the
        // media part holding that slide's bytes.
        for (slide, data) in [(1, &b"first-slide"[..]), (2, &b"second-slide"[..])] {
            let xml = String::from_utf8(zip_entry(
                &mut archive,
                &format!("ppt/slides/slide{slide}.xml"),
            ))
            .unwrap();
            let rels = String::from_utf8(zip_entry(
                &mut archive,
                &format!("ppt/slides/_rels/slide{slide}.xml.rels"),
            ))
            .unwrap();
            let embed = xml
                .split("r:embed=\"")
                .nth(1)
                .and_then(|rest| rest.split('"').next())
                .expect("picture reference");
            let target = rels
                .split(&format!("Id=\"{embed}\""))
                .nth(1)
                .and_then(|rest| rest.split("Target=\"../").nth(1))
                .and_then(|rest| rest.split('"').next())
                .expect("relationship for the picture");
            assert_eq!(zip_entry(&mut archive, &format!("ppt/{target}")), data);
        }
    }

    #[test]
    fn freeform_paths_reach_the_slide_xml() {
        let presentation = PptxPresentation {
            slides: vec![PptxSlide {
                page_index: 0,
                elements: vec![SlideElement::FreeformPath {
                    path_data: "M 10 10 L 30 10 C 35 10 40 15 40 20 Z".to_string(),
                    fill_rgb: None,
                    stroke_rgb: Some((0, 0, 0)),
                    stroke_width_pt: 0.0,
                }],
            }],
            width_emu: 914400,
            height_emu: 914400,
        };
        let bytes = PptxWriter::new(presentation)
            .write_to_bytes()
            .expect("write pptx");
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("zip");
        let xml = String::from_utf8(zip_entry(&mut archive, "ppt/slides/slide1.xml")).unwrap();
        assert!(xml.contains("<a:custGeom>"), "{xml}");
        assert!(xml.contains("<a:cubicBezTo>"), "{xml}");
        // A zero-width PDF stroke is a hairline, not an invisible outline.
        assert!(!xml.contains("<a:ln><a:noFill/></a:ln>"), "{xml}");
        assert!(xml.contains("<a:ln w=\"3175\">"), "{xml}");
    }
}
