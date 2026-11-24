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
use zpdf_display_list::{
    Color, DisplayList, FillRule, LineJoin, Overprint, Paint, Path, RenderCommand, StrokeStyle,
};
use zpdf_font::FontCache;
use zpdf_image::ImageCache;
use zpdf_render::{PageRenderInfo, RenderBackend};
use zpdf_render_cpu::CpuRenderer;

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
        clip_stack: Vec::new(),
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
    /// Active clip stack replayed when rasterizing `Paint::Tiling` commands.
    clip_stack: Vec<(RenderCommand, Option<Rect>)>,
}

impl<'a> SlideConverter<'a> {
    fn convert_commands(&mut self, commands: &[RenderCommand]) -> Result<()> {
        for cmd in commands {
            match cmd {
                RenderCommand::DrawGlyphRun(glyph_run) => {
                    self.convert_glyph_run(glyph_run)?;
                }
                RenderCommand::FillPath {
                    path,
                    rule,
                    paint,
                    alpha,
                    overprint,
                } => {
                    self.convert_fill_path(path, *rule, paint, *alpha, *overprint)?;
                }
                RenderCommand::StrokePath {
                    path,
                    style,
                    paint,
                    alpha,
                    overprint,
                } => {
                    self.convert_stroke_path(path, style, paint, *alpha, *overprint)?;
                }
                RenderCommand::DrawImage(image_draw) => {
                    self.convert_image(image_draw)?;
                }
                RenderCommand::PushClip { path, .. } => {
                    let bounds = if path.is_empty() {
                        Some(Rect::new(0.0, 0.0, 0.0, 0.0))
                    } else {
                        path_bounds(path)
                    };
                    self.clip_stack.push((cmd.clone(), bounds));
                }
                RenderCommand::PushClipStroke { path, style } => {
                    let bounds = if path.is_empty() {
                        Some(Rect::new(0.0, 0.0, 0.0, 0.0))
                    } else {
                        stroke_bounds(path, style)
                    };
                    self.clip_stack.push((cmd.clone(), bounds));
                }
                RenderCommand::PopClip => {
                    self.clip_stack.pop();
                }
                // Skip blend commands - we're extracting content only
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
        path: &Path,
        rule: FillRule,
        paint: &Paint,
        alpha: f32,
        overprint: Option<Overprint>,
    ) -> Result<()> {
        if let Paint::Tiling(_) = paint {
            if overprint.is_some_and(|op| op.active == 0) {
                return Ok(());
            }
            if let Some(bounds) = path_bounds(path) {
                let cmd = RenderCommand::FillPath {
                    path: path.clone(),
                    rule,
                    paint: paint.clone(),
                    alpha,
                    overprint,
                };
                self.rasterize_pattern_command(&cmd, bounds)?;
            }
            return Ok(());
        }

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
        path: &Path,
        style: &StrokeStyle,
        paint: &Paint,
        alpha: f32,
        overprint: Option<Overprint>,
    ) -> Result<()> {
        if let Paint::Tiling(_) = paint {
            if overprint.is_some_and(|op| op.active == 0) {
                return Ok(());
            }
            if let Some(bounds) = stroke_bounds(path, style) {
                let cmd = RenderCommand::StrokePath {
                    path: path.clone(),
                    style: style.clone(),
                    paint: paint.clone(),
                    alpha,
                    overprint,
                };
                self.rasterize_pattern_command(&cmd, bounds)?;
            }
            return Ok(());
        }

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

    /// Rasterize a clipped `Paint::Tiling` fill or stroke into a transparent
    /// PNG `SlideElement::Image` positioned at its clipped page-space bounds.
    fn rasterize_pattern_command(&mut self, cmd: &RenderCommand, raw_bounds: Rect) -> Result<()> {
        const MAX_PATTERN_DIM: f64 = 2048.0;
        const MAX_PATTERN_PIXELS: f64 = 4_194_304.0;

        let mut bounds = match intersect_rect(raw_bounds, self.page_rect) {
            Some(b) => b,
            None => return Ok(()),
        };
        for (_, clip_bounds) in &self.clip_stack {
            if let Some(cb) = clip_bounds {
                match intersect_rect(bounds, *cb) {
                    Some(b) => bounds = b,
                    None => return Ok(()),
                }
            }
        }
        if !bounds.x0.is_finite()
            || !bounds.y0.is_finite()
            || !bounds.x1.is_finite()
            || !bounds.y1.is_finite()
            || bounds.width() <= 0.0
            || bounds.height() <= 0.0
        {
            return Ok(());
        }

        let mut scale = 2.0_f64;
        let max_dim = (bounds.width() * scale).max(bounds.height() * scale);
        if max_dim > MAX_PATTERN_DIM {
            scale *= MAX_PATTERN_DIM / max_dim;
        }
        let pixels = (bounds.width() * scale) * (bounds.height() * scale);
        if pixels > MAX_PATTERN_PIXELS {
            scale *= (MAX_PATTERN_PIXELS / pixels).sqrt();
        }
        let scale = (scale as f32).max(1e-4);

        let mut renderer = CpuRenderer::new()
            .with_fonts(self.font_cache)
            .with_images(self.image_cache);
        let info = PageRenderInfo {
            page_rect: bounds,
            scale,
            background: Color::rgba(0.0, 0.0, 0.0, 0.0),
        };
        if renderer.begin_page(&info).is_err() {
            return Ok(());
        }
        for (clip_cmd, _) in &self.clip_stack {
            let _ = renderer.execute(clip_cmd);
        }
        let _ = renderer.execute(cmd);
        for _ in &self.clip_stack {
            let _ = renderer.execute(&RenderCommand::PopClip);
        }
        let Ok(mut rendered) = renderer.end_page() else {
            return Ok(());
        };

        // Skip empty / fully transparent pattern outputs.
        if !rendered.data.as_chunks::<4>().0.iter().any(|px| px[3] > 0) {
            return Ok(());
        }

        // Convert tiny-skia premultiplied RGBA8 into straight RGBA8 for PNG.
        for px in rendered.data.as_chunks_mut::<4>().0 {
            let a = px[3] as u32;
            if a > 0 && a < 255 {
                px[0] = ((px[0] as u32 * 255 + a / 2) / a).min(255) as u8;
                px[1] = ((px[1] as u32 * 255 + a / 2) / a).min(255) as u8;
                px[2] = ((px[2] as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }

        let png_data = rgba_to_png(&rendered.data, rendered.width, rendered.height)?;
        self.image_counter += 1;
        let image_id = format!("image{}", self.image_counter);
        let (x_ppt, y_ppt) = self.pdf_to_ppt_coords(bounds.x0, bounds.y1);
        self.elements.push(SlideElement::Image {
            x_emu: pt_to_emu(x_ppt),
            y_emu: pt_to_emu(y_ppt),
            width_emu: pt_to_emu(bounds.width()),
            height_emu: pt_to_emu(bounds.height()),
            image_data: png_data,
            image_id,
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
    rgba_to_png(&image.data, image.width, image.height)
}

fn rgba_to_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let mut png_data = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut png_data);
    encoder
        .write_image(rgba, width, height, image::ExtendedColorType::Rgba8)
        .map_err(|e| zpdf_core::Error::StreamDecode(format!("PNG encode: {e}")))?;
    Ok(png_data)
}

fn path_bounds(path: &Path) -> Option<Rect> {
    let (mut x0, mut y0, mut x1, mut y1) = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    for p in path.points() {
        if !p.x.is_finite() || !p.y.is_finite() {
            return None;
        }
        x0 = x0.min(p.x);
        y0 = y0.min(p.y);
        x1 = x1.max(p.x);
        y1 = y1.max(p.y);
    }
    (x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite())
        .then(|| Rect::new(x0, y0, x1, y1))
}

fn stroke_bounds(path: &Path, style: &StrokeStyle) -> Option<Rect> {
    let b = path_bounds(path)?;
    let reach = match style.join {
        LineJoin::Miter => style.miter_limit.clamp(1.0, 100.0),
        _ => std::f32::consts::SQRT_2,
    };
    let grow =
        (style.width.max(1.0) as f64 * 0.5 * reach.max(std::f32::consts::SQRT_2) as f64).max(0.5);
    Some(Rect::new(
        b.x0 - grow,
        b.y0 - grow,
        b.x1 + grow,
        b.y1 + grow,
    ))
}

fn intersect_rect(a: Rect, b: Rect) -> Option<Rect> {
    let a = a.normalize();
    let b = b.normalize();
    let x0 = a.x0.max(b.x0);
    let y0 = a.y0.max(b.y0);
    let x1 = a.x1.min(b.x1);
    let y1 = a.y1.min(b.y1);
    (x1 > x0 && y1 > y0).then(|| Rect::new(x0, y0, x1, y1))
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
    fn composite_font_mixed_one_and_three_byte_to_unicode_exports_three_byte_code() {
        let make_font = || {
            let mut font = LoadedFont::new_placeholder("MixedThreeByte".into());
            font.font_type = zpdf_font::PdfFontType::Type0CidType2;
            font.to_unicode = Some(ToUnicodeMap::parse(
                b"begincmap 2 begincodespacerange <00> <FF> <000000> <FFFFFF> endcodespacerange \
                  2 beginbfchar <41> <0058> <010203> <0041> endbfchar endcmap",
            ));
            font
        };

        // Explicit source byte length (`code_len = 3` and `code_len = 1`) must
        // match `(raw_code, len)` directly before fixed-width 2-byte recovery
        // splits `<010203>` into `<0102>` and `<03>`.
        let mut fonts = FontCache::new();
        let font_id = fonts.insert("F1".into(), make_font());
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 612.0, 792.0));
        dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
            font_id,
            font_size: 12.0,
            glyphs: vec![
                PositionedGlyph {
                    glyph_id: 7,
                    code_len: 3,
                    code: 0x010203,
                    x: 100.0,
                    y: 100.0,
                    advance: 6.0,
                },
                PositionedGlyph {
                    glyph_id: 8,
                    code_len: 1,
                    code: 0x41,
                    x: 106.0,
                    y: 100.0,
                    advance: 6.0,
                },
            ],
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
        let explicit_text: String = slide
            .elements
            .iter()
            .filter_map(|e| match e {
                SlideElement::TextBox { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(explicit_text, "AX");

        // Inferred-width path (`code_len = 0` via `run_text`) must also emit
        // `"A"` for `<010203>` and `"X"` for `<41>`.
        assert_eq!(run_text(make_font(), 7, 0x010203), "A");
        assert_eq!(run_text(make_font(), 8, 0x41), "X");

        // With `code_len = 0`, no `/ToUnicode`, and `/90ms-RKSJ-H`, `<41>` is a
        // complete 1-byte code and must not be zero-padded to `<0041>` (which
        // would decode `<00>` and drop `<41>`), while `<8141>` still infers
        // 2 bytes and `/UniGB-UCS2-H` still infers 2 bytes for `<0041>`.
        let rksj_font = || {
            let mut font = LoadedFont::new_placeholder("Rksj".into());
            font.font_type = zpdf_font::PdfFontType::Type0CidType2;
            font.cid_cmap = zpdf_font::cmap::CidCMap::predefined("90ms-RKSJ-H");
            font
        };
        assert_eq!(run_text(rksj_font(), 1, 0x41), "A");
        assert_eq!(run_text(rksj_font(), 2, 0x8141), "\u{3001}");

        let mut ucs2_font = LoadedFont::new_placeholder("Ucs2".into());
        ucs2_font.font_type = zpdf_font::PdfFontType::Type0CidType2;
        ucs2_font.cid_cmap = zpdf_font::cmap::CidCMap::predefined("UniGB-UCS2-H");
        assert_eq!(run_text(ucs2_font, 1, 0x41), "A");

        // Fixed-width 1-byte recovery still splits a 2-byte unmapped chunk
        // when no exact `(raw_code, len)` entry exists.
        let mut one_byte_font = LoadedFont::new_placeholder("OneByteFallback".into());
        one_byte_font.font_type = zpdf_font::PdfFontType::Type0CidType2;
        one_byte_font.to_unicode = Some(ToUnicodeMap::parse(
            b"begincmap 1 begincodespacerange <00> <FF> endcodespacerange \
              2 beginbfchar <57> <0057> <69> <4E2D> endbfchar endcmap",
        ));
        let mut fonts = FontCache::new();
        let font_id = fonts.insert("F1".into(), one_byte_font);
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 612.0, 792.0));
        dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
            font_id,
            font_size: 12.0,
            glyphs: vec![PositionedGlyph {
                glyph_id: 1,
                code_len: 2,
                code: 0x5769,
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
        let fixed_text: String = slide
            .elements
            .iter()
            .filter_map(|e| match e {
                SlideElement::TextBox { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(fixed_text, "W中");
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

    #[test]
    fn mixed_width_tounicode_with_both_widths_exports_same_text_in_both_orders() {
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
        assert_ne!(gid_a, gid_b);

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

        let mixed_cmap = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
            /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> def \
            /CMapName /PptxMixedWidth def /CMapType 1 def \
            2 begincodespacerange <20> <7E> <0000> <1FFF> endcodespacerange \
            2 begincidchar <41> 1 <0041> 2 endcidchar \
            endcmap CMapName currentdict /CMap defineresource pop end end";

        let provider = Arc::new(FontProvider::memory_only());
        provider
            .register_with_aliases(PRIMARY_TTF, 0, &["PptxMixedWidthFace"])
            .unwrap();

        for (label, to_unicode) in [
            (
                "short-then-long",
                b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
                  2 begincodespacerange <00> <FF> <0000> <FFFF> endcodespacerange \
                  2 beginbfchar <41> <0041> <0041> <0042> endbfchar \
                  endcmap CMapName currentdict /CMap defineresource pop end end"
                    .as_slice(),
            ),
            (
                "long-then-short",
                b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
                  2 begincodespacerange <00> <FF> <0000> <FFFF> endcodespacerange \
                  2 beginbfchar <0041> <0042> <41> <0041> endbfchar \
                  endcmap CMapName currentdict /CMap defineresource pop end end"
                    .as_slice(),
            ),
        ] {
            let pdf = assemble(&[
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
                b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] \
                  /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
                    .to_vec(),
                stream("", b"0 0 0 rg BT /F1 18 Tf 20 50 Td <410041> Tj ET"),
                b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+PptxMixedWidthFace \
                  /Encoding 9 0 R /DescendantFonts [6 0 R] /ToUnicode 10 0 R >>"
                    .to_vec(),
                b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+PptxMixedWidthFace \
                  /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
                  /CIDToGIDMap /Identity /FontDescriptor 7 0 R /DW 600 >>"
                    .to_vec(),
                b"<< /Type /FontDescriptor /FontName /ABCDEF+PptxMixedWidthFace /Flags 32 /FontFile2 8 0 R >>"
                    .to_vec(),
                stream("", b"corrupt unparseable truetype bytes"),
                stream("", mixed_cmap),
                stream("", to_unicode),
            ]);

            let doc = zpdf_document::PdfDocument::open(pdf)
                .unwrap()
                .with_font_provider(Arc::clone(&provider));
            let page = doc.page(0).unwrap();
            let mut fonts = doc.load_page_fonts(&page);
            let (content, _) = doc.page_content_with_diagnostics(&page).unwrap();
            let mut images = ImageCache::new();
            let mut extracted = Vec::new();
            let interpreter =
                zpdf_content::interpreter::ContentInterpreter::new(page.effective_box())
                    .with_document(doc.file(), &page.resources)
                    .with_fonts(&mut fonts)
                    .with_shared_fonts(&doc)
                    .with_images(&mut images)
                    .with_text_sink(&mut extracted);
            let (display_list, _) = interpreter.interpret_with_stats(&content);

            let extracted_text: String = extracted.iter().map(|s| s.text.as_str()).collect();
            assert_eq!(
                extracted_text, "AB",
                "{label}: text extraction must preserve both <41> and <0041> in one /ToUnicode"
            );

            let runs: Vec<_> = display_list
                .commands
                .iter()
                .filter_map(|cmd| match cmd {
                    RenderCommand::DrawGlyphRun(run) => Some(run),
                    _ => None,
                })
                .collect();
            assert_eq!(runs.len(), 1, "{label}");
            assert_eq!(
                [runs[0].glyphs[0].glyph_id, runs[0].glyphs[1].glyph_id],
                [gid_a, gid_b],
                "{label}: physical glyphs must follow <41>->A and <0041>->B regardless of order"
            );
            assert_eq!(
                [runs[0].glyphs[0].code_len, runs[0].glyphs[1].code_len],
                [1, 2],
                "{label}"
            );

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
                "{label}: PPTX slide export must preserve both <41> and <0041> regardless of declaration order"
            );
        }
    }

    #[test]
    fn unicode_cmap_xml_forbidden_control_char_serializes_valid_slide_xml() {
        use quick_xml::events::Event;
        use std::sync::Arc;
        use zpdf_font::FontProvider;

        const PRIMARY_TTF: &[u8] =
            include_bytes!("../../zpdf-font/tests/fixtures/coverage-primary.ttf");

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
            .register_with_aliases(PRIMARY_TTF, 0, &["PptxXmlControlFace"])
            .unwrap();

        // `/UniGB-UCS2-H` with NO `/ToUnicode` decodes `<0041000B0042>` to
        // `"A\u{000B}B"`. Serializing to PPTX must strip the XML-forbidden
        // vertical tab while keeping a well-formed slide whose `<a:t>` holds
        // both letters.
        let pdf = assemble(&[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] \
              /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
                .to_vec(),
            stream("", b"0 0 0 rg BT /F1 18 Tf 20 50 Td <0041000B0042> Tj ET"),
            b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+PptxXmlControlFace \
              /Encoding /UniGB-UCS2-H /DescendantFonts [6 0 R] >>"
                .to_vec(),
            b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+PptxXmlControlFace \
              /CIDSystemInfo << /Registry (Adobe) /Ordering (GB1) /Supplement 0 >> \
              /CIDToGIDMap /Identity /FontDescriptor 7 0 R /DW 600 >>"
                .to_vec(),
            b"<< /Type /FontDescriptor /FontName /ABCDEF+PptxXmlControlFace /Flags 32 /FontFile2 8 0 R >>"
                .to_vec(),
            stream("", b"corrupt unparseable truetype bytes"),
        ]);

        let doc = zpdf_document::PdfDocument::open(pdf)
            .unwrap()
            .with_font_provider(Arc::clone(&provider));
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

        let slide = display_list_to_slide(
            &display_list,
            0,
            &fonts,
            &images,
            &ConversionOptions::default(),
        )
        .expect("slide export");
        let raw_slide_text: String = slide
            .elements
            .iter()
            .filter_map(|e| match e {
                SlideElement::TextBox { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            raw_slide_text, "A\u{000B}B",
            "test fixture must exercise U+000B decoded via /UniGB-UCS2-H"
        );
        let presentation = PptxPresentation {
            slides: vec![slide],
            width_emu: pt_to_emu(200.0),
            height_emu: pt_to_emu(100.0),
        };
        let bytes = PptxWriter::new(presentation)
            .write_to_bytes()
            .expect("pptx bytes");
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("zip archive");
        let xml = String::from_utf8(zip_entry(&mut archive, "ppt/slides/slide1.xml"))
            .expect("utf-8 slide xml");

        // Enforce XML 1.0 §2.2 Char range across the entire serialized slide.
        for ch in xml.chars() {
            assert!(
                ooxml::is_valid_xml_char(ch),
                "serialized slide XML contains XML 1.0 forbidden character U+{:04X}: {xml}",
                ch as u32
            );
        }

        // Parse the serialized slide with quick-xml and collect `<a:t>` text.
        let mut reader = quick_xml::Reader::from_str(&xml);
        let mut in_text = false;
        let mut parsed_text = String::new();
        loop {
            match reader.read_event().expect("well-formed slide XML") {
                Event::Start(e) => {
                    for attr in e.attributes() {
                        attr.expect("valid attribute");
                    }
                    if e.name().as_ref() == "a:t" {
                        in_text = true;
                    }
                }
                Event::Empty(e) => {
                    for attr in e.attributes() {
                        attr.expect("valid attribute");
                    }
                }
                Event::End(e) => {
                    if e.name().as_ref() == "a:t" {
                        in_text = false;
                    }
                }
                Event::Text(e) => {
                    if in_text {
                        parsed_text.push_str(&e.xml10_content());
                    }
                }
                Event::GeneralRef(e) => {
                    if let Some(ch) = e.resolve_char_ref().expect("valid char ref") {
                        assert!(
                            ooxml::is_valid_xml_char(ch),
                            "serialized slide XML contains forbidden char ref U+{:04X}",
                            ch as u32
                        );
                        if in_text {
                            parsed_text.push(ch);
                        }
                    } else if in_text {
                        let entity = match e.xml10_content().as_ref() {
                            "amp" => "&",
                            "lt" => "<",
                            "gt" => ">",
                            "quot" => "\"",
                            "apos" => "'",
                            other => panic!("unexpected XML entity &{other};"),
                        };
                        parsed_text.push_str(entity);
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }
        assert_eq!(
            parsed_text, "AB",
            "parsed slide XML must retain both 'A' and 'B' without XML-forbidden U+000B"
        );
    }

    #[test]
    fn tiling_pattern_fill_and_stroke_export_as_clipped_slide_images() {
        use std::sync::Arc;
        use zpdf_core::Point;
        use zpdf_display_list::{FillRule, Path, StrokeStyle, TilingPattern};

        // Two-color 20×20 tile: red left half [0..10], blue right half [10..20].
        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        let mut left = Path::new();
        left.rect(Rect::new(0.0, 0.0, 10.0, 20.0));
        cell.push(RenderCommand::FillPath {
            path: left,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        let mut right = Path::new();
        right.rect(Rect::new(10.0, 0.0, 20.0, 20.0));
        cell.push(RenderCommand::FillPath {
            path: right,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        let tiling = Arc::new(TilingPattern {
            commands: Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 20.0, 20.0),
            x_step: 20.0,
            y_step: 20.0,
            matrix: Matrix::identity(),
            tiling_type: 1,
        });

        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 200.0, 200.0));
        // 1) Pattern-filled rectangle [20..100] × [100..160].
        let mut rect_path = Path::new();
        rect_path.rect(Rect::new(20.0, 100.0, 100.0, 160.0));
        dl.push(RenderCommand::FillPath {
            path: rect_path,
            rule: FillRule::NonZero,
            paint: Paint::Tiling(Arc::clone(&tiling)),
            alpha: 1.0,
            overprint: None,
        });
        // 2) Pattern-stroked diagonal line from (20, 20) to (100, 80) with width 16.
        let mut line_path = Path::new();
        line_path.move_to(Point::new(20.0, 20.0));
        line_path.line_to(Point::new(100.0, 80.0));
        dl.push(RenderCommand::StrokePath {
            path: line_path,
            style: StrokeStyle {
                width: 16.0,
                ..StrokeStyle::default()
            },
            paint: Paint::Tiling(tiling),
            alpha: 1.0,
            overprint: None,
        });

        let fonts = FontCache::new();
        let images = ImageCache::new();
        let slide = display_list_to_slide(&dl, 0, &fonts, &images, &ConversionOptions::default())
            .expect("display_list_to_slide");

        assert_eq!(
            slide.elements.len(),
            2,
            "patterned fill and stroke must emit two slide elements"
        );

        let mut decoded_images = Vec::new();
        for elem in &slide.elements {
            match elem.base() {
                SlideElement::Image {
                    width_emu,
                    height_emu,
                    image_data,
                    ..
                } => {
                    assert!(*width_emu > 0 && *height_emu > 0);
                    let img = image::load_from_memory(image_data)
                        .expect("valid PNG")
                        .to_rgba8();
                    decoded_images.push(img);
                }
                other => panic!(
                    "expected SlideElement::Image for tiling pattern fill/stroke, got {other:?}"
                ),
            }
        }

        // Fill image must contain both opaque red and opaque blue pixels from the tile.
        let fill_img = &decoded_images[0];
        let has_red = fill_img
            .pixels()
            .any(|p| p[0] > 200 && p[1] < 40 && p[2] < 40 && p[3] > 200);
        let has_blue = fill_img
            .pixels()
            .any(|p| p[0] < 40 && p[1] < 40 && p[2] > 200 && p[3] > 200);
        assert!(
            has_red && has_blue,
            "patterned fill image must preserve both tile colors"
        );

        // Stroke image must contain both red and blue along the diagonal stroke
        // AND transparent pixels in the off-stroke corners (clipped to stroke outline).
        let stroke_img = &decoded_images[1];
        let stroke_has_red = stroke_img
            .pixels()
            .any(|p| p[0] > 200 && p[1] < 40 && p[2] < 40 && p[3] > 200);
        let stroke_has_blue = stroke_img
            .pixels()
            .any(|p| p[0] < 40 && p[1] < 40 && p[2] > 200 && p[3] > 200);
        let stroke_has_transparent = stroke_img.pixels().any(|p| p[3] == 0);
        assert!(
            stroke_has_red && stroke_has_blue,
            "patterned stroke image must preserve both tile colors instead of defaulting to black"
        );
        assert!(
            stroke_has_transparent,
            "patterned stroke image must stay transparent outside the stroked outline"
        );

        // Ensure the serialized PPTX archive embeds both rendered PNG media entries.
        let bytes = PptxWriter::new(PptxPresentation {
            slides: vec![slide],
            width_emu: pt_to_emu(200.0),
            height_emu: pt_to_emu(200.0),
        })
        .write_to_bytes()
        .expect("pptx bytes");
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("zip");
        assert!(!zip_entry(&mut archive, "ppt/media/image1.png").is_empty());
        assert!(!zip_entry(&mut archive, "ppt/media/image2.png").is_empty());
    }
}
