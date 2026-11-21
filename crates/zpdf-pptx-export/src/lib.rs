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
    /// Rotation/reflection about an element's center. Existing untransformed
    /// element constructors are unchanged; consumers should handle this variant.
    Transformed {
        element: Box<SlideElement>,
        rotation_degrees: f64,
        flip_h: bool,
        flip_v: bool,
    },
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

impl SlideElement {
    pub(crate) fn base(&self) -> &Self {
        let mut element = self;
        while let Self::Transformed { element: inner, .. } = element {
            element = inner;
        }
        element
    }
    pub(crate) fn base_mut(&mut self) -> &mut Self {
        let mut element = self;
        while let Self::Transformed { element: inner, .. } = element {
            element = inner;
        }
        element
    }
}

fn transformed_element(element: SlideElement, matrix: &zpdf_core::Matrix) -> SlideElement {
    let rotation_degrees = (-matrix.b.atan2(matrix.a).to_degrees()).rem_euclid(360.0);
    let flip_v = matrix.determinant() < 0.0;
    if rotation_degrees.abs() < 1e-8 && !flip_v {
        element
    } else {
        SlideElement::Transformed {
            element: Box::new(element),
            rotation_degrees,
            flip_h: false,
            flip_v,
        }
    }
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

        // Extract text from glyphs using the logical PDF font (following any
        // admitted drawing-only companion/fallback face back to its original
        // Type 0 / simple PDF font so `/ToUnicode` and `/Encoding` stay intact).
        let font = self
            .font_cache
            .logical_font(glyph_run.font_id)
            .ok_or_else(|| zpdf_core::Error::StreamDecode("Font not found".into()))?;

        let mut text = String::new();
        let mut min_x = f32::INFINITY;
        let mut max_x = f32::NEG_INFINITY;
        let mut min_y = f32::INFINITY;
        let mut max_y = f32::NEG_INFINITY;

        for glyph in &glyph_run.glyphs {
            // Decode each glyph's packed source bytes and byte width through
            // the logical PDF font's own text decoder (`/ToUnicode`, then
            // composite `/Encoding` CMap or simple encoding / glyph names).
            font.append_decoded_source_code(glyph.code, glyph.code_len, &mut text);

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
        let matrix = &glyph_run.transform;
        if !matrix.is_finite() {
            return Err(zpdf_core::Error::StreamDecode(
                "non-finite PPTX text transform".into(),
            ));
        }
        let scale_x = matrix.a.hypot(matrix.b);
        let scale_y = matrix.c.hypot(matrix.d);
        if scale_x == 0.0 || scale_y == 0.0 {
            return Ok(());
        }
        let center = zpdf_core::Point::new(
            (min_x as f64 + max_x as f64) / 2.0,
            (min_y as f64 + max_y as f64) / 2.0,
        )
        .transform(matrix);
        let (center_x, center_y) = self.pdf_to_ppt_coords(center.x, center.y);
        let width = (max_x - min_x) as f64 * scale_x;
        let height = (max_y - min_y) as f64 * scale_y;
        let (x_ppt, y_ppt) = (center_x - width / 2.0, center_y - height / 2.0);

        // Extract font family name from base_font
        let font_family = font
            .base_font
            .split_once('+')
            .filter(|(prefix, _)| {
                prefix.len() == 6 && prefix.bytes().all(|b| b.is_ascii_uppercase())
            })
            .map(|(_, name)| name)
            .unwrap_or(&font.base_font)
            .to_string();
        let style = font_family.to_ascii_lowercase();

        let element = SlideElement::TextBox {
            text,
            x_emu: pt_to_emu(x_ppt),
            y_emu: pt_to_emu(y_ppt),
            width_emu: pt_to_emu(width),
            height_emu: pt_to_emu(height),
            font_family,
            font_size_pt: glyph_run.font_size as f64 * scale_y,
            color_rgb,
            bold: style.contains("bold") || style.contains("demi"),
            italic: style.contains("italic") || style.contains("oblique"),
        };
        self.elements.push(transformed_element(element, matrix));

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
        if !mat.is_finite() {
            return Err(zpdf_core::Error::StreamDecode(
                "non-finite PPTX image transform".into(),
            ));
        }

        // Image transform: unit square [0,1]×[0,1] → page space
        let width = (mat.a * mat.a + mat.b * mat.b).sqrt();
        let height = (mat.c * mat.c + mat.d * mat.d).sqrt();

        let center = zpdf_core::Point::new(0.5, 0.5).transform(mat);
        let (cx, cy) = self.pdf_to_ppt_coords(center.x, center.y);
        let (x_ppt, y_ppt) = (cx - width / 2.0, cy - height / 2.0);

        // Convert image to PNG
        let png_data = image_to_png(image)?;

        self.image_counter += 1;
        let image_id = format!("image{}", self.image_counter);

        let element = SlideElement::Image {
            x_emu: pt_to_emu(x_ppt),
            y_emu: pt_to_emu(y_ppt),
            width_emu: pt_to_emu(width),
            height_emu: pt_to_emu(height),
            image_data: png_data,
            image_id,
        };
        self.elements.push(transformed_element(element, mat));

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

    fn positioned_slide(transforms: &[Matrix]) -> PptxSlide {
        let mut font = LoadedFont::new_placeholder("ABCDEF+Helvetica-BoldOblique".into());
        font.to_unicode = Some(ToUnicodeMap::parse(
            b"begincmap 1 beginbfchar <41> <0041> endbfchar endcmap",
        ));
        let mut fonts = FontCache::new();
        let font_id = fonts.insert("F1".into(), font);
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 120.0, 80.0));
        for &transform in transforms {
            dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
                font_id,
                font_size: 12.0,
                glyphs: vec![PositionedGlyph {
                    glyph_id: 1,
                    code_len: 1,
                    code: 65,
                    x: 0.0,
                    y: 0.0,
                    advance: 6.0,
                }],
                paint: Paint::Solid(Color::black()),
                alpha: 1.0,
                overprint: None,
                transform,
                h_scale: 1.0,
            }));
        }
        display_list_to_slide(
            &dl,
            0,
            &fonts,
            &ImageCache::new(),
            &ConversionOptions::default(),
        )
        .unwrap()
    }

    #[test]
    fn text_boxes_keep_translation_scale_and_style() {
        let slide = positioned_slide(&[
            Matrix::new(2.0, 0.0, 0.0, 2.0, 20.0, 30.0),
            Matrix::translate(60.0, 20.0),
        ]);
        match &slide.elements[0] {
            SlideElement::TextBox {
                x_emu,
                y_emu,
                width_emu,
                height_emu,
                font_size_pt,
                font_family,
                bold,
                italic,
                ..
            } => {
                assert_eq!(
                    (*x_emu, *y_emu, *width_emu, *height_emu),
                    (254000, 330200, 152400, 304800)
                );
                assert_eq!(*font_size_pt, 24.0);
                assert_eq!(font_family, "Helvetica-BoldOblique");
                assert!(*bold && *italic);
            }
            _ => panic!("unrotated text"),
        }
        assert!(
            matches!(&slide.elements[1],SlideElement::TextBox{x_emu,y_emu,..} if (*x_emu,*y_emu)==(762000,609600))
        );
    }

    #[test]
    fn rotated_and_reflected_text_remains_editable_in_drawingml() {
        let slide = positioned_slide(&[
            Matrix::new(0.0, 1.0, -1.0, 0.0, 40.0, 20.0),
            Matrix::new(1.0, 0.0, 0.0, -1.0, 10.0, 30.0),
        ]);
        let presentation = PptxPresentation {
            slides: vec![slide],
            width_emu: 1524000,
            height_emu: 1016000,
        };
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(
            PptxWriter::new(presentation).write_to_bytes().unwrap(),
        ))
        .unwrap();
        let xml = String::from_utf8(zip_entry(&mut zip, "ppt/slides/slide1.xml")).unwrap();
        assert!(xml.contains("rot=\"16200000\""));
        assert!(xml.contains("flipV=\"1\""));
        assert_eq!(xml.matches("<a:t>A</a:t>").count(), 2);
        assert!(xml.contains("</a:rPr>"));
        assert!(!xml.contains("sz=\"1200\" b=\"1\" i=\"1\"/>"));
    }

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
                code_len: 0,
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
                code_len: 1,
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

    #[test]
    fn recovered_type0_two_byte_code_exports_slide_text_via_logical_font() {
        use std::sync::Arc;
        use zpdf_font::{CidWidths, FontProvider, LoadedFont, PdfFontType};

        const PRIMARY_TTF: &[u8] =
            include_bytes!("../../zpdf-font/tests/fixtures/coverage-primary.ttf");

        let probe = LoadedFont::new_with_data(
            PdfFontType::TrueType,
            "Probe".into(),
            PRIMARY_TTF.to_vec(),
            CidWidths::new(1000.0),
        );
        let gid_a = probe.unicode_glyph('A' as u32).unwrap().0;
        let gid_b = probe.unicode_glyph('B' as u32).unwrap().0;

        // Corrupt only `gid_a` in `PRIMARY_TTF` (invalid `loca` start > end) so
        // the embedded Type 0 face stays primary and lazily admits a simple
        // TrueType descriptor-substitute drawing face for `<0102>` ('A').
        let mut partial_ttf = PRIMARY_TTF.to_vec();
        let num_tables = u16::from_be_bytes([partial_ttf[4], partial_ttf[5]]) as usize;
        let mut loca_off = None;
        for i in 0..num_tables {
            let rec = 12 + i * 16;
            if &partial_ttf[rec..rec + 4] == b"loca" {
                loca_off = Some(u32::from_be_bytes(
                    partial_ttf[rec + 8..rec + 12].try_into().unwrap(),
                ) as usize);
                break;
            }
        }
        let loca_off = loca_off.expect("loca table");
        let entry = loca_off + gid_a as usize * 4;
        partial_ttf[entry..entry + 4].copy_from_slice(&0xFFFF_FF00u32.to_be_bytes());

        let cmap = format!(
            "/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
             /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> def \
             /CMapName /TwoByteRecovered def /CMapType 1 def \
             1 begincodespacerange <0000> <FFFF> endcodespacerange \
             2 begincidchar <0102> {gid_a} <0103> {gid_b} endcidchar \
             endcmap CMapName currentdict /CMap defineresource pop end end"
        );
        let to_unicode = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
            1 begincodespacerange <0000> <FFFF> endcodespacerange \
            2 beginbfchar <0102> <0041> <0103> <0042> endbfchar \
            endcmap CMapName currentdict /CMap defineresource pop end end";

        let stream = |dict: &str, bytes: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", bytes.len()).into_bytes();
            out.extend_from_slice(bytes);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let objects: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] \
              /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
                .to_vec(),
            stream("", b"0 0 0 rg BT /F1 18 Tf 20 50 Td <01020103> Tj ET"),
            b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+PptxRecoveredCid \
              /Encoding 9 0 R /DescendantFonts [6 0 R] /ToUnicode 10 0 R >>"
                .to_vec(),
            b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+PptxRecoveredCid \
              /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
              /CIDToGIDMap /Identity /FontDescriptor 7 0 R /DW 600 >>"
                .to_vec(),
            b"<< /Type /FontDescriptor /FontName /ABCDEF+PptxRecoveredCid /Flags 32 /FontFile2 8 0 R >>"
                .to_vec(),
            stream("", &partial_ttf),
            stream("", cmap.as_bytes()),
            stream("", to_unicode),
        ];
        let mut pdf = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
        let mut offsets = Vec::with_capacity(objects.len());
        for (idx, body) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n", idx + 1).as_bytes());
            pdf.extend_from_slice(body);
            pdf.extend_from_slice(b"\nendobj\n");
        }
        let xref = pdf.len();
        let count = objects.len() + 1;
        pdf.extend_from_slice(format!("xref\n0 {count}\n0000000000 65535 f \n").as_bytes());
        for off in offsets {
            pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!("trailer\n<< /Size {count} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n")
                .as_bytes(),
        );

        let provider = Arc::new(FontProvider::memory_only());
        provider
            .register_with_aliases(PRIMARY_TTF, 0, &["PptxRecoveredCid"])
            .unwrap();
        let doc = zpdf_document::PdfDocument::open(pdf)
            .unwrap()
            .with_font_provider(provider);
        let page = doc.page(0).unwrap();
        let mut fonts = doc.load_page_fonts(&page);
        let (content, _) = doc.page_content_with_diagnostics(&page).unwrap();
        let mut images = ImageCache::new();
        let interpreter = zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
            .with_document(doc.file(), &page.resources)
            .with_fonts(&mut fonts)
            .with_shared_fonts(&doc)
            .with_images(&mut images);
        let (display_list, _) = interpreter.interpret_with_stats(&content);

        let (fid_primary, primary) = fonts.get_by_name("F1").unwrap();
        assert!(!primary.is_substitute, "embedded Type 0 face stays primary");
        let runs: Vec<_> = display_list
            .commands
            .iter()
            .filter_map(|cmd| match cmd {
                RenderCommand::DrawGlyphRun(run) => Some(run),
                _ => None,
            })
            .collect();
        assert_eq!(runs.len(), 2, "recovered <0102> + embedded <0103>");
        assert_ne!(
            runs[0].font_id, fid_primary,
            "<0102> must draw on the admitted descriptor substitute drawing face"
        );
        assert!(matches!(
            fonts.get(runs[0].font_id).unwrap().font_type,
            PdfFontType::TrueType
        ));
        assert_eq!(runs[0].glyphs[0].code, 0x0102);

        let slide = display_list_to_slide(
            &display_list,
            0,
            &fonts,
            &images,
            &ConversionOptions::default(),
        )
        .expect("slide export");
        let slide_text: String = slide
            .elements
            .iter()
            .filter_map(|e| match e {
                SlideElement::TextBox { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            slide_text, "AB",
            "PPTX slide export must recover 'A' for two-byte code <0102> through logical_font"
        );
    }

    #[test]
    fn recovered_type0_unicode_cmap_and_utf16_surrogate_export_slide_text() {
        use std::sync::Arc;
        use zpdf_font::{CidWidths, FontProvider, LoadedFont, PdfFontType};

        const PRIMARY_TTF: &[u8] =
            include_bytes!("../../zpdf-font/tests/fixtures/coverage-primary.ttf");

        let probe = LoadedFont::new_with_data(
            PdfFontType::TrueType,
            "Probe".into(),
            PRIMARY_TTF.to_vec(),
            CidWidths::new(1000.0),
        );
        let gid_a = probe.unicode_glyph('A' as u32).unwrap().0;

        // Corrupt only `gid_a` so the embedded Type 0 font stays primary and
        // lazily admits a drawing-only descriptor substitute for the damaged
        // glyph while keeping 'B' on the embedded face.
        let mut partial_ttf = PRIMARY_TTF.to_vec();
        let num_tables = u16::from_be_bytes([partial_ttf[4], partial_ttf[5]]) as usize;
        let mut loca_off = None;
        for i in 0..num_tables {
            let rec = 12 + i * 16;
            if &partial_ttf[rec..rec + 4] == b"loca" {
                loca_off = Some(u32::from_be_bytes(
                    partial_ttf[rec + 8..rec + 12].try_into().unwrap(),
                ) as usize);
                break;
            }
        }
        let loca_off = loca_off.expect("loca table");
        let entry = loca_off + gid_a as usize * 4;
        partial_ttf[entry..entry + 4].copy_from_slice(&0xFFFF_FF00u32.to_be_bytes());

        let stream = |dict: &str, bytes: &[u8]| -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", bytes.len()).into_bytes();
            out.extend_from_slice(bytes);
            out.extend_from_slice(b"\nendstream");
            out
        };
        let assemble = |objects: &[Vec<u8>]| -> Vec<u8> {
            let mut pdf = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
            let mut offsets = Vec::with_capacity(objects.len());
            for (idx, body) in objects.iter().enumerate() {
                offsets.push(pdf.len());
                pdf.extend_from_slice(format!("{} 0 obj\n", idx + 1).as_bytes());
                pdf.extend_from_slice(body);
                pdf.extend_from_slice(b"\nendobj\n");
            }
            let xref = pdf.len();
            let count = objects.len() + 1;
            pdf.extend_from_slice(format!("xref\n0 {count}\n0000000000 65535 f \n").as_bytes());
            for off in offsets {
                pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
            }
            pdf.extend_from_slice(
                format!("trailer\n<< /Size {count} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n")
                    .as_bytes(),
            );
            pdf
        };

        let provider = Arc::new(FontProvider::memory_only());
        provider
            .register_with_aliases(PRIMARY_TTF, 0, &["PptxUnicodeCmapFace"])
            .unwrap();

        // 1. `/UniGB-UCS2-H` WITHOUT `/ToUnicode`: recovered `<0041>` draws via
        //    descriptor substitute and must export slide text "AB" via the
        //    logical font's CMap decoder.
        let ucs2_pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] \
              /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
                .to_vec(),
            stream("", b"0 0 0 rg BT /F1 18 Tf 20 50 Td <00410042> Tj ET"),
            b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+PptxUnicodeCmapFace \
              /Encoding /UniGB-UCS2-H /DescendantFonts [6 0 R] >>"
                .to_vec(),
            b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+PptxUnicodeCmapFace \
              /CIDSystemInfo << /Registry (Adobe) /Ordering (GB1) /Supplement 0 >> \
              /CIDToGIDMap /Identity /FontDescriptor 7 0 R /DW 600 >>"
                .to_vec(),
            b"<< /Type /FontDescriptor /FontName /ABCDEF+PptxUnicodeCmapFace /Flags 32 /FontFile2 8 0 R >>"
                .to_vec(),
            stream("", &partial_ttf),
        ]);

        // 2. `/UniGB-UTF16-H` with an explicit 4-byte UTF-16 surrogate
        //    `/ToUnicode` mapping `<D840DC00> -> <0041>` ('A'): the display
        //    list must keep the packed source bytes `0xD840DC00` + `code_len=4`
        //    so PPTX export resolves the explicit `/ToUnicode` entry ('A')
        //    instead of dropping it or emitting U+20000, while `<0042>` without
        //    `/ToUnicode` still decodes as 'B' through the CMap.
        let utf16_to_unicode = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
            1 begincodespacerange <0000> <FFFF> endcodespacerange \
            1 beginbfchar <D840DC00> <0041> endbfchar \
            endcmap CMapName currentdict /CMap defineresource pop end end";
        let utf16_pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] \
              /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
                .to_vec(),
            stream("", b"0 0 0 rg BT /F1 18 Tf 20 50 Td <D840DC000042> Tj ET"),
            b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+PptxUnicodeCmapFace \
              /Encoding /UniGB-UTF16-H /DescendantFonts [6 0 R] /ToUnicode 9 0 R >>"
                .to_vec(),
            b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+PptxUnicodeCmapFace \
              /CIDSystemInfo << /Registry (Adobe) /Ordering (GB1) /Supplement 0 >> \
              /CIDToGIDMap /Identity /FontDescriptor 7 0 R /DW 600 >>"
                .to_vec(),
            b"<< /Type /FontDescriptor /FontName /ABCDEF+PptxUnicodeCmapFace /Flags 32 /FontFile2 8 0 R >>"
                .to_vec(),
            stream("", &partial_ttf),
            stream("", utf16_to_unicode),
        ]);

        for (label, pdf, expected_code, expected_len) in [
            ("unigb-ucs2-no-tounicode", ucs2_pdf, 0x0041u32, 2u8),
            (
                "unigb-utf16-surrogate-tounicode",
                utf16_pdf,
                0xD840_DC00u32,
                4u8,
            ),
        ] {
            let doc = zpdf_document::PdfDocument::open(pdf)
                .unwrap()
                .with_font_provider(Arc::clone(&provider));
            let page = doc.page(0).unwrap();
            let mut fonts = doc.load_page_fonts(&page);
            let (content, _) = doc.page_content_with_diagnostics(&page).unwrap();
            let mut images = ImageCache::new();
            let interpreter =
                zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
                    .with_document(doc.file(), &page.resources)
                    .with_fonts(&mut fonts)
                    .with_shared_fonts(&doc)
                    .with_images(&mut images);
            let (display_list, _) = interpreter.interpret_with_stats(&content);

            let (fid_primary, primary) = fonts.get_by_name("F1").unwrap();
            assert!(!primary.is_substitute, "{label}");
            let runs: Vec<_> = display_list
                .commands
                .iter()
                .filter_map(|cmd| match cmd {
                    RenderCommand::DrawGlyphRun(run) => Some(run),
                    _ => None,
                })
                .collect();
            assert_eq!(
                runs.len(),
                2,
                "{label}: recovered first glyph + embedded 'B'"
            );
            assert_ne!(runs[0].font_id, fid_primary, "{label}");
            assert_eq!(runs[0].glyphs[0].code, expected_code, "{label}");
            assert_eq!(runs[0].glyphs[0].code_len, expected_len, "{label}");

            let slide = display_list_to_slide(
                &display_list,
                0,
                &fonts,
                &images,
                &ConversionOptions::default(),
            )
            .expect("slide export");
            let slide_text: String = slide
                .elements
                .iter()
                .filter_map(|e| match e {
                    SlideElement::TextBox { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                slide_text, "AB",
                "{label}: PPTX slide export must decode recovered Type 0 text through logical font decoder"
            );
        }
    }
}
