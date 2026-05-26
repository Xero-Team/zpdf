use zpdf_core::Rect;
use zpdf_display_list::*;
use zpdf_font::{FontCache, GlyphOutline, OutlineCommand};
use zpdf_render::{PageRenderInfo, RenderBackend};

pub struct CpuRenderer<'a> {
    pixmap: Option<tiny_skia::Pixmap>,
    scale: f32,
    page_height: f32,
    font_cache: Option<&'a FontCache>,
}

#[derive(Debug, thiserror::Error)]
pub enum CpuRenderError {
    #[error("not initialized")]
    NotInitialized,
    #[error("failed to create pixmap")]
    PixmapCreation,
    #[error("png encode error: {0}")]
    PngEncode(String),
}

impl<'a> CpuRenderer<'a> {
    pub fn new() -> Self {
        Self {
            pixmap: None,
            scale: 1.0,
            page_height: 0.0,
            font_cache: None,
        }
    }

    pub fn with_fonts(mut self, cache: &'a FontCache) -> Self {
        self.font_cache = Some(cache);
        self
    }

    /// Convert PDF Y coordinate (origin bottom-left) to pixel Y (origin top-left).
    fn flip_y(&self, y: f32) -> f32 {
        (self.page_height - y) * self.scale
    }

    fn to_pixel_x(&self, x: f32) -> f32 {
        x * self.scale
    }

    fn build_skia_path(&self, path: &Path) -> Option<tiny_skia::Path> {
        let mut pb = tiny_skia::PathBuilder::new();
        for elem in &path.elements {
            match *elem {
                PathElement::MoveTo(p) => {
                    pb.move_to(self.to_pixel_x(p.x as f32), self.flip_y(p.y as f32));
                }
                PathElement::LineTo(p) => {
                    pb.line_to(self.to_pixel_x(p.x as f32), self.flip_y(p.y as f32));
                }
                PathElement::CurveTo(c1, c2, end) => {
                    pb.cubic_to(
                        self.to_pixel_x(c1.x as f32),
                        self.flip_y(c1.y as f32),
                        self.to_pixel_x(c2.x as f32),
                        self.flip_y(c2.y as f32),
                        self.to_pixel_x(end.x as f32),
                        self.flip_y(end.y as f32),
                    );
                }
                PathElement::Close => {
                    pb.close();
                }
            }
        }
        pb.finish()
    }

    fn color_to_paint(color: &Color, alpha: f32) -> tiny_skia::Paint<'static> {
        let mut paint = tiny_skia::Paint::default();
        paint.set_color_rgba8(
            (color.r * 255.0) as u8,
            (color.g * 255.0) as u8,
            (color.b * 255.0) as u8,
            (color.a * alpha * 255.0) as u8,
        );
        paint.anti_alias = true;
        paint
    }

    fn fill_rule_to_skia(rule: &FillRule) -> tiny_skia::FillRule {
        match rule {
            FillRule::NonZero => tiny_skia::FillRule::Winding,
            FillRule::EvenOdd => tiny_skia::FillRule::EvenOdd,
        }
    }

    fn render_fill(
        &mut self,
        path: &Path,
        rule: &FillRule,
        paint_spec: &Paint,
        alpha: f32,
    ) {
        let Some(skia_path) = self.build_skia_path(path) else {
            return;
        };
        let paint = match paint_spec {
            Paint::Solid(c) => Self::color_to_paint(c, alpha),
            _ => return,
        };
        let fill_rule = Self::fill_rule_to_skia(rule);
        if let Some(ref mut pixmap) = self.pixmap {
            pixmap.fill_path(
                &skia_path,
                &paint,
                fill_rule,
                tiny_skia::Transform::identity(),
                None,
            );
        }
    }

    fn render_stroke(
        &mut self,
        path: &Path,
        style: &StrokeStyle,
        paint_spec: &Paint,
        alpha: f32,
    ) {
        let Some(skia_path) = self.build_skia_path(path) else {
            return;
        };
        let paint = match paint_spec {
            Paint::Solid(c) => Self::color_to_paint(c, alpha),
            _ => return,
        };
        let stroke = tiny_skia::Stroke {
            width: style.width * self.scale,
            line_cap: match style.cap {
                LineCap::Butt => tiny_skia::LineCap::Butt,
                LineCap::Round => tiny_skia::LineCap::Round,
                LineCap::Square => tiny_skia::LineCap::Square,
            },
            line_join: match style.join {
                LineJoin::Miter => tiny_skia::LineJoin::Miter,
                LineJoin::Round => tiny_skia::LineJoin::Round,
                LineJoin::Bevel => tiny_skia::LineJoin::Bevel,
            },
            miter_limit: style.miter_limit,
            ..Default::default()
        };
        if let Some(ref mut pixmap) = self.pixmap {
            pixmap.stroke_path(
                &skia_path,
                &paint,
                &stroke,
                tiny_skia::Transform::identity(),
                None,
            );
        }
    }

    fn render_glyph_run(&mut self, run: &GlyphRun) {
        let font_cache = match self.font_cache {
            Some(fc) => fc,
            None => return,
        };
        let font = match font_cache.get(run.font_id) {
            Some(f) => f,
            None => return,
        };
        if !font.has_font_data() {
            return;
        }

        let paint = match &run.paint {
            Paint::Solid(c) => Self::color_to_paint(c, run.alpha),
            _ => return,
        };

        if font.is_type3() {
            self.render_type3_glyphs(run, font, &paint);
        } else {
            self.render_outline_glyphs(run, font, &paint);
        }
    }

    fn render_outline_glyphs(
        &mut self,
        run: &GlyphRun,
        font: &zpdf_font::LoadedFont,
        paint: &tiny_skia::Paint<'_>,
    ) {
        let tm = &run.transform;
        let font_size = run.font_size;
        let upem = font.units_per_em as f32;

        for glyph in &run.glyphs {
            let outline = match font.glyph_outline(glyph.glyph_id) {
                Some(o) => o,
                None => continue,
            };

            // Transform each glyph outline point:
            // glyph_coord (font units) → text space → user space → page space → pixel space
            let skia_path = self.build_outline_transformed_path(
                &outline, upem, font_size, tm, glyph.x,
            );
            if let Some(path) = skia_path {
                if let Some(ref mut pixmap) = self.pixmap {
                    pixmap.fill_path(
                        &path,
                        paint,
                        tiny_skia::FillRule::Winding,
                        tiny_skia::Transform::identity(),
                        None,
                    );
                }
            }
        }
    }

    fn build_outline_transformed_path(
        &self,
        outline: &GlyphOutline,
        upem: f32,
        font_size: f32,
        tm: &zpdf_core::Matrix,
        glyph_x_offset: f32,
    ) -> Option<tiny_skia::Path> {
        let mut pb = tiny_skia::PathBuilder::new();

        for cmd in &outline.commands {
            match *cmd {
                OutlineCommand::MoveTo(x, y) => {
                    let (px, py) = self.outline_to_pixel(
                        x, y, upem, font_size, tm, glyph_x_offset,
                    );
                    pb.move_to(px, py);
                }
                OutlineCommand::LineTo(x, y) => {
                    let (px, py) = self.outline_to_pixel(
                        x, y, upem, font_size, tm, glyph_x_offset,
                    );
                    pb.line_to(px, py);
                }
                OutlineCommand::QuadTo(x1, y1, x, y) => {
                    let (px1, py1) = self.outline_to_pixel(
                        x1, y1, upem, font_size, tm, glyph_x_offset,
                    );
                    let (px, py) = self.outline_to_pixel(
                        x, y, upem, font_size, tm, glyph_x_offset,
                    );
                    pb.quad_to(px1, py1, px, py);
                }
                OutlineCommand::CurveTo(x1, y1, x2, y2, x, y) => {
                    let (px1, py1) = self.outline_to_pixel(
                        x1, y1, upem, font_size, tm, glyph_x_offset,
                    );
                    let (px2, py2) = self.outline_to_pixel(
                        x2, y2, upem, font_size, tm, glyph_x_offset,
                    );
                    let (px, py) = self.outline_to_pixel(
                        x, y, upem, font_size, tm, glyph_x_offset,
                    );
                    pb.cubic_to(px1, py1, px2, py2, px, py);
                }
                OutlineCommand::Close => pb.close(),
            }
        }
        pb.finish()
    }

    fn outline_to_pixel(
        &self,
        gx: f64,
        gy: f64,
        upem: f32,
        font_size: f32,
        tm: &zpdf_core::Matrix,
        glyph_x_offset: f32,
    ) -> (f32, f32) {
        // font units → user space
        let tx = (gx as f32 / upem * font_size + glyph_x_offset) as f64;
        let ty = (gy as f32 / upem * font_size) as f64;

        // user space → page space via combined CTM*Tm
        let page_x = tm.a * tx + tm.c * ty + tm.e;
        let page_y = tm.b * tx + tm.d * ty + tm.f;

        // page space → pixel space
        let ctm_flips_y = tm.d < 0.0 || (tm.d == 0.0 && tm.b != 0.0);
        let px = page_x as f32 * self.scale;
        let py = if ctm_flips_y {
            page_y as f32 * self.scale
        } else {
            (self.page_height - page_y as f32) * self.scale
        };
        (px, py)
    }

    fn render_type3_glyphs(
        &mut self,
        run: &GlyphRun,
        font: &zpdf_font::LoadedFont,
        paint: &tiny_skia::Paint<'_>,
    ) {
        use zpdf_content::interpreter::ContentInterpreter;

        // run.transform = CTM * Tm (already includes all coordinate transforms)
        // The CTM may already flip Y (e.g., Skia PDFs use negative y scale).
        // We must apply the full transform chain: FontMatrix * glyph_coords → text space,
        // then Tm * text_coords → CTM-transformed space, then to pixels.
        let tm = &run.transform;
        let font_size = run.font_size;

        for glyph in &run.glyphs {
            let (stream, font_matrix) = match font.type3_glyph_stream(glyph.glyph_id) {
                Some(s) => s,
                None => continue,
            };

            let glyph_rect = zpdf_core::Rect::new(0.0, -1000.0, 1000.0, 1000.0);
            let glyph_dl = ContentInterpreter::new(glyph_rect).interpret(stream);

            // Build the full transform: glyph_space → page_space → pixel_space
            // FontMatrix transforms glyph coords to text space (typically 0.001 scale)
            // Then the text matrix (tm) transforms text space to page space (already in run.transform)
            // glyph.x is the accumulated horizontal offset in page space from the interpreter

            for cmd in &glyph_dl.commands {
                match cmd {
                    RenderCommand::FillPath { path, rule, .. } => {
                        if let Some(skia_path) = self.build_type3_transformed_path(
                            path,
                            &font_matrix,
                            font_size,
                            tm,
                            glyph.x,
                        ) {
                            if let Some(ref mut pixmap) = self.pixmap {
                                pixmap.fill_path(
                                    &skia_path,
                                    paint,
                                    Self::fill_rule_to_skia(rule),
                                    tiny_skia::Transform::identity(),
                                    None,
                                );
                            }
                        }
                    }
                    RenderCommand::StrokePath { path, style, .. } => {
                        if let Some(skia_path) = self.build_type3_transformed_path(
                            path,
                            &font_matrix,
                            font_size,
                            tm,
                            glyph.x,
                        ) {
                            let stroke = tiny_skia::Stroke {
                                width: (style.width * font_matrix[0].abs() as f32
                                    * font_size
                                    * self.scale)
                                    .max(0.5),
                                ..Default::default()
                            };
                            if let Some(ref mut pixmap) = self.pixmap {
                                pixmap.stroke_path(
                                    &skia_path,
                                    paint,
                                    &stroke,
                                    tiny_skia::Transform::identity(),
                                    None,
                                );
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// Transform a Type3 glyph path through: FontMatrix → text position → CTM → pixels.
    fn build_type3_transformed_path(
        &self,
        path: &Path,
        font_matrix: &[f64; 6],
        font_size: f32,
        tm: &zpdf_core::Matrix,
        glyph_x_offset: f32,
    ) -> Option<tiny_skia::Path> {
        let mut pb = tiny_skia::PathBuilder::new();

        for elem in &path.elements {
            match *elem {
                PathElement::MoveTo(p) => {
                    let (px, py) = self.type3_to_pixel(
                        p.x, p.y, font_matrix, font_size, tm, glyph_x_offset,
                    );
                    pb.move_to(px, py);
                }
                PathElement::LineTo(p) => {
                    let (px, py) = self.type3_to_pixel(
                        p.x, p.y, font_matrix, font_size, tm, glyph_x_offset,
                    );
                    pb.line_to(px, py);
                }
                PathElement::CurveTo(c1, c2, end) => {
                    let (x1, y1) = self.type3_to_pixel(
                        c1.x, c1.y, font_matrix, font_size, tm, glyph_x_offset,
                    );
                    let (x2, y2) = self.type3_to_pixel(
                        c2.x, c2.y, font_matrix, font_size, tm, glyph_x_offset,
                    );
                    let (x, y) = self.type3_to_pixel(
                        end.x, end.y, font_matrix, font_size, tm, glyph_x_offset,
                    );
                    pb.cubic_to(x1, y1, x2, y2, x, y);
                }
                PathElement::Close => pb.close(),
            }
        }
        pb.finish()
    }

    /// Transform a point from Type3 glyph space all the way to pixel space.
    fn type3_to_pixel(
        &self,
        gx: f64,
        gy: f64,
        font_matrix: &[f64; 6],
        font_size: f32,
        tm: &zpdf_core::Matrix,
        glyph_x_offset: f32,
    ) -> (f32, f32) {
        // Step 1: FontMatrix * glyph_coord → text space
        let tx = font_matrix[0] * gx + font_matrix[2] * gy + font_matrix[4];
        let ty = font_matrix[1] * gx + font_matrix[3] * gy + font_matrix[5];

        // Step 2: scale by font_size
        let tx = tx * font_size as f64;
        let ty = ty * font_size as f64;

        // Step 3: add glyph horizontal offset (in page space, pre-CTM)
        let tx = tx + glyph_x_offset as f64;

        // Step 4: apply text matrix (= CTM * Tm) to get page-space coords
        let page_x = tm.a * tx + tm.c * ty + tm.e;
        let page_y = tm.b * tx + tm.d * ty + tm.f;

        // Step 5: page space → pixel space
        // The CTM already handles Y flipping if needed, so we just need
        // to map from PDF page coords (origin bottom-left, Y up) to pixels.
        // But if CTM already flipped Y (like Skia PDFs), page_y is already
        // in top-down order. We detect this by checking if CTM has negative Y scale.
        let ctm_flips_y = tm.d < 0.0 || (tm.d == 0.0 && tm.b != 0.0);

        let px = page_x as f32 * self.scale;
        let py = if ctm_flips_y {
            // CTM already flipped Y into screen coordinates
            page_y as f32 * self.scale
        } else {
            // Standard PDF coords: flip Y
            (self.page_height as f32 - page_y as f32) * self.scale
        };

        (px, py)
    }

    fn build_glyph_path(
        &self,
        outline: &GlyphOutline,
        px: f32,
        py: f32,
        scale: f32,
    ) -> Option<tiny_skia::Path> {
        let mut pb = tiny_skia::PathBuilder::new();

        for cmd in &outline.commands {
            match *cmd {
                OutlineCommand::MoveTo(x, y) => {
                    pb.move_to(
                        px + x as f32 * scale,
                        py - y as f32 * scale, // glyph Y is up, screen Y is down
                    );
                }
                OutlineCommand::LineTo(x, y) => {
                    pb.line_to(px + x as f32 * scale, py - y as f32 * scale);
                }
                OutlineCommand::QuadTo(x1, y1, x, y) => {
                    pb.quad_to(
                        px + x1 as f32 * scale,
                        py - y1 as f32 * scale,
                        px + x as f32 * scale,
                        py - y as f32 * scale,
                    );
                }
                OutlineCommand::CurveTo(x1, y1, x2, y2, x, y) => {
                    pb.cubic_to(
                        px + x1 as f32 * scale,
                        py - y1 as f32 * scale,
                        px + x2 as f32 * scale,
                        py - y2 as f32 * scale,
                        px + x as f32 * scale,
                        py - y as f32 * scale,
                    );
                }
                OutlineCommand::Close => {
                    pb.close();
                }
            }
        }

        pb.finish()
    }
}

impl<'a> Default for CpuRenderer<'a> {
    fn default() -> Self {
        Self::new()
    }
}

/// RGBA pixel buffer output.
pub struct RenderedPage {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl RenderedPage {
    pub fn save_png(&self, path: &str) -> Result<(), CpuRenderError> {
        let img = image::RgbaImage::from_raw(self.width, self.height, self.data.clone())
            .ok_or(CpuRenderError::PixmapCreation)?;
        img.save(path)
            .map_err(|e| CpuRenderError::PngEncode(e.to_string()))
    }
}

impl<'a> RenderBackend for CpuRenderer<'a> {
    type Target = RenderedPage;
    type Error = CpuRenderError;

    fn begin_page(&mut self, info: &PageRenderInfo) -> Result<(), Self::Error> {
        self.scale = info.scale;
        self.page_height = info.page_rect.height() as f32;

        let w = (info.page_rect.width() * info.scale as f64) as u32;
        let h = (info.page_rect.height() * info.scale as f64) as u32;

        let mut pixmap =
            tiny_skia::Pixmap::new(w, h).ok_or(CpuRenderError::PixmapCreation)?;

        let bg = &info.background;
        pixmap.fill(tiny_skia::Color::from_rgba(bg.r, bg.g, bg.b, bg.a).unwrap_or(tiny_skia::Color::WHITE));

        self.pixmap = Some(pixmap);
        Ok(())
    }

    fn execute(&mut self, cmd: &RenderCommand) -> Result<(), Self::Error> {
        match cmd {
            RenderCommand::FillPath {
                path,
                rule,
                paint,
                alpha,
            } => {
                self.render_fill(path, rule, paint, *alpha);
            }
            RenderCommand::StrokePath {
                path,
                style,
                paint,
                alpha,
            } => {
                self.render_stroke(path, style, paint, *alpha);
            }
            RenderCommand::DrawGlyphRun(glyph_run) => {
                self.render_glyph_run(glyph_run);
            }
            RenderCommand::DrawImage(_image_draw) => {
                // Phase 2 later: render images
            }
            RenderCommand::PushClip { .. } | RenderCommand::PopClip => {
                // TODO: stencil clip
            }
            RenderCommand::PushBlendGroup { .. } | RenderCommand::PopBlendGroup => {
                // TODO: transparency groups
            }
        }
        Ok(())
    }

    fn end_page(&mut self) -> Result<Self::Target, Self::Error> {
        let pixmap = self.pixmap.take().ok_or(CpuRenderError::NotInitialized)?;
        Ok(RenderedPage {
            width: pixmap.width(),
            height: pixmap.height(),
            data: pixmap.data().to_vec(),
        })
    }
}
