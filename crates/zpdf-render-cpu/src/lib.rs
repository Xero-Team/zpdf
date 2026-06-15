use std::collections::HashMap;

use zpdf_display_list::*;
use zpdf_font::{FontCache, GlyphOutline, OutlineCommand};
use zpdf_image::ImageCache;
use zpdf_render::{PageRenderInfo, RenderBackend};

pub struct CpuRenderer<'a> {
    pixmap: Option<tiny_skia::Pixmap>,
    scale: f32,
    /// Page rect bounds (supports CropBox / nonzero MediaBox origins):
    /// device x = (x - rect_x0) * scale, device y = (rect_y1 - y) * scale.
    rect_x0: f32,
    rect_y0: f32,
    rect_y1: f32,
    font_cache: Option<&'a FontCache>,
    image_cache: Option<&'a ImageCache>,
    clip_stack: Vec<ClipFrame>,
    current_clip: Option<tiny_skia::Mask>,
    /// Cumulative full-raster clip-mask work (Σ width·height over built clip
    /// masks). Bounds the O(clips × raster) cost of the raster-mask clip model:
    /// pathological pages (100k+ tiny W clips on a large raster) would otherwise
    /// allocate/zero gigabytes of mask memory and hang. Past the budget, further
    /// clips are skipped (the page renders complete but slightly over-painted).
    clip_pixel_spent: u64,
    blend_stack: Vec<BlendEntry>,
    /// Wall-clock ceiling per page, an anti-hang backstop for adversarial pages
    /// that emit hundreds of thousands of expensive primitives (strokes, large
    /// fills, glyphs) — too many to render in bounded time yet individually
    /// cheap, so neither the command nor clip-pixel budget catches them. `None`
    /// disables it. Legit pages finish in well under the default and never hit it.
    render_budget: Option<std::time::Duration>,
    /// Absolute deadline for the current page (set in `begin_page`).
    deadline: Option<std::time::Instant>,
    /// Set once the page deadline passes; further draws are skipped so the
    /// command loop drains quickly and the page returns partially rendered.
    over_budget: bool,
    /// Soft-mask coverage planes rasterized at build position (offset 0),
    /// keyed by mask identity. Tiling patterns attach the same mask (modulo
    /// offset) to every painted command of every tile; without this the full
    /// page-sized mask would re-rasterize per command. Cleared per page —
    /// the keys are Arc pointers, only stable while the display list lives.
    soft_mask_planes: HashMap<SoftMaskPlaneKey, Vec<u8>>,
}

/// Identity of a [`SoftMask`] up to its offset: command list and transfer
/// LUT by Arc pointer, plus the scalar parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SoftMaskPlaneKey {
    commands: usize,
    kind: SoftMaskKind,
    backdrop_luma_bits: u32,
    transfer: usize,
}

impl SoftMaskPlaneKey {
    fn new(mask: &SoftMask) -> Self {
        Self {
            commands: std::sync::Arc::as_ptr(&mask.commands) as *const () as usize,
            kind: mask.kind,
            backdrop_luma_bits: mask.backdrop_luma.to_bits(),
            transfer: mask
                .transfer
                .as_ref()
                .map(|lut| std::sync::Arc::as_ptr(lut) as *const () as usize)
                .unwrap_or(0),
        }
    }
}

/// Coverage where the mask group painted nothing — what a fresh raster
/// produces outside the group, used to fill strips vacated by a plane shift.
fn unpainted_value(mask: &SoftMask) -> u8 {
    let v = match mask.kind {
        SoftMaskKind::Luminosity => (mask.backdrop_luma.clamp(0.0, 1.0) * 255.0).round() as u8,
        SoftMaskKind::Alpha => 0,
    };
    match &mask.transfer {
        Some(lut) => lut[v as usize],
        None => v,
    }
}

/// Translate a `w`×`h` coverage plane by whole device pixels, filling vacated
/// areas with `fill`. Offsets at or beyond the plane size yield all-`fill`.
fn shift_plane(base: &[u8], w: u32, h: u32, dx: i64, dy: i64, fill: u8) -> Vec<u8> {
    let (w, h) = (w as usize, h as usize);
    let mut out = vec![fill; w * h];
    if base.len() != w * h {
        return out;
    }
    if dx.unsigned_abs() as usize >= w || dy.unsigned_abs() as usize >= h {
        return out;
    }
    let copy_w = w - dx.unsigned_abs() as usize;
    let copy_h = h - dy.unsigned_abs() as usize;
    let (src_x, dst_x) = if dx >= 0 {
        (0, dx as usize)
    } else {
        ((-dx) as usize, 0)
    };
    let (src_y, dst_y) = if dy >= 0 {
        (0, dy as usize)
    } else {
        ((-dy) as usize, 0)
    };
    for row in 0..copy_h {
        let src = (src_y + row) * w + src_x;
        let dst = (dst_y + row) * w + dst_x;
        out[dst..dst + copy_w].copy_from_slice(&base[src..src + copy_w]);
    }
    out
}

/// Default [`CpuRenderer::render_budget`]. Generous enough that no realistic page
/// approaches it (complex pages render in well under a second) while bounding
/// pathological inputs to a few seconds.
const DEFAULT_RENDER_BUDGET: std::time::Duration = std::time::Duration::from_secs(8);

/// A saved clip state for the clip stack. `Skipped` records a budget-dropped
/// `PushClip` so its matching `PopClip` leaves `current_clip` untouched.
enum ClipFrame {
    Empty,
    Mask(tiny_skia::Mask),
    Skipped,
}

/// Ceiling on cumulative clip-mask pixel-work per page (see `clip_pixel_spent`).
/// ~2 Gpx keeps every realistic clip-heavy page intact while bounding adversarial
/// ones to roughly a second of mask work.
const MAX_CLIP_PIXEL_WORK: u64 = 2_000_000_000;

struct BlendEntry {
    pixmap: tiny_skia::Pixmap,
    blend_mode: BlendMode,
    /// Group constant alpha applied at composite time.
    alpha: f32,
    /// Rasterized soft-mask coverage (one byte per pixel), multiplied into the
    /// group before compositing.
    mask: Option<Vec<u8>>,
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
            rect_x0: 0.0,
            rect_y0: 0.0,
            rect_y1: 0.0,
            font_cache: None,
            image_cache: None,
            clip_stack: Vec::new(),
            current_clip: None,
            clip_pixel_spent: 0,
            blend_stack: Vec::new(),
            render_budget: Some(DEFAULT_RENDER_BUDGET),
            deadline: None,
            over_budget: false,
            soft_mask_planes: HashMap::new(),
        }
    }

    /// Override the per-page wall-clock render budget (`None` disables it).
    pub fn with_render_budget(mut self, budget: Option<std::time::Duration>) -> Self {
        self.render_budget = budget;
        self
    }

    pub fn with_fonts(mut self, cache: &'a FontCache) -> Self {
        self.font_cache = Some(cache);
        self
    }

    pub fn with_images(mut self, cache: &'a ImageCache) -> Self {
        self.image_cache = Some(cache);
        self
    }

    /// Convert PDF Y coordinate (origin bottom-left) to pixel Y (origin top-left),
    /// relative to the page rect's top edge.
    fn flip_y(&self, y: f32) -> f32 {
        (self.rect_y1 - y) * self.scale
    }

    fn to_pixel_x(&self, x: f32) -> f32 {
        (x - self.rect_x0) * self.scale
    }

    /// Finish a built path, rejecting it (caller then skips the draw) when its
    /// bounds are non-finite or astronomically larger than the target pixmap.
    /// Such geometry — from a degenerate text/CTM matrix or a corrupt coordinate
    /// — overflows tiny-skia's AA blitter row-offset arithmetic and panics with
    /// an out-of-range slice index instead of being clipped. A generous overscan
    /// (64× the longest pixmap side, min 1e6 px) keeps all legitimately
    /// off-page-but-clipped content while dropping only crash-inducing extents.
    fn finish_within_limits(&self, pb: tiny_skia::PathBuilder) -> Option<tiny_skia::Path> {
        let path = pb.finish()?;
        let b = path.bounds();
        let vals = [b.left(), b.top(), b.right(), b.bottom()];
        if vals.iter().any(|v| !v.is_finite()) {
            tracing::warn!("skipping non-finite path");
            return None;
        }
        let span = self
            .pixmap
            .as_ref()
            .map(|p| p.width().max(p.height()) as f32)
            .unwrap_or(0.0);
        let limit = (span * 64.0).max(1_000_000.0);
        if vals.iter().any(|v| v.abs() > limit) {
            tracing::warn!("skipping out-of-range path bounds={b:?}");
            return None;
        }
        Some(path)
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
        self.finish_within_limits(pb)
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

    fn render_fill(&mut self, path: &Path, rule: &FillRule, paint_spec: &Paint, alpha: f32) {
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
                self.current_clip.as_ref(),
            );
        }
    }

    fn render_stroke(&mut self, path: &Path, style: &StrokeStyle, paint_spec: &Paint, alpha: f32) {
        let Some(skia_path) = self.build_skia_path(path) else {
            return;
        };
        let paint = match paint_spec {
            Paint::Solid(c) => Self::color_to_paint(c, alpha),
            _ => return,
        };
        let stroke = tiny_skia::Stroke {
            // Hairline boost: never let a stroke fall below one device pixel
            // (matches pdfium; keeps thin diagram strokes legible at low DPI,
            // and renders PDF zero-width strokes as 1px hairlines).
            width: (style.width * self.scale).max(1.0),
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
            dash: self.build_stroke_dash(style),
        };
        if let Some(ref mut pixmap) = self.pixmap {
            pixmap.stroke_path(
                &skia_path,
                &paint,
                &stroke,
                tiny_skia::Transform::identity(),
                self.current_clip.as_ref(),
            );
        }
    }

    /// Build a device-space tiny-skia dash from the stroke style. Degenerate
    /// patterns (empty/all-zero/negative entries) return `None` → solid stroke.
    /// PDF allows odd-length dash arrays (the pattern repeats, so `[3]` means
    /// 3 on / 3 off); tiny-skia requires an even count, so odd arrays are doubled.
    fn build_stroke_dash(&self, style: &StrokeStyle) -> Option<tiny_skia::StrokeDash> {
        let dash = style.dash.as_ref()?;
        if zpdf_render::dash::is_degenerate(&dash.array) {
            return None;
        }
        let mut array: Vec<f32> = dash.array.iter().map(|v| v * self.scale).collect();
        if array.len() % 2 == 1 {
            let doubled = array.clone();
            array.extend(doubled);
        }
        tiny_skia::StrokeDash::new(array, dash.phase * self.scale)
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

    fn push_clip(&mut self, path: &Path, rule: &FillRule) {
        let Some(pixmap) = self.pixmap.as_ref() else {
            return;
        };
        let (pw, ph) = (pixmap.width(), pixmap.height());

        // Budget guard: once the page has spent its clip-mask pixel-work, stop
        // building new full-raster masks (which would hang on 100k-clip pages).
        // The matching PopClip pops a `Skipped` frame and the previously-active
        // clip stays in force — strictly tighter than dropping clipping entirely.
        if self.clip_pixel_spent >= MAX_CLIP_PIXEL_WORK {
            self.clip_stack.push(ClipFrame::Skipped);
            return;
        }

        let Some(skia_path) = self.build_skia_path(path) else {
            // A non-finite/out-of-range clip path: keep push/pop balanced.
            self.clip_stack.push(ClipFrame::Skipped);
            return;
        };

        let mut mask = match tiny_skia::Mask::new(pw, ph) {
            Some(m) => m,
            None => {
                self.clip_stack.push(ClipFrame::Skipped);
                return;
            }
        };
        self.clip_pixel_spent += pw as u64 * ph as u64;

        mask.fill_path(
            &skia_path,
            Self::fill_rule_to_skia(rule),
            true, // anti-alias clip edges (fills/strokes are AA'd too)
            tiny_skia::Transform::identity(),
        );

        // Intersect with the current clip, but only across the new clip path's
        // device bbox: outside it the new mask is already 0, so the intersection
        // there is 0 too and need not be touched. This turns the per-clip
        // intersection from O(raster) into O(clip bbox).
        if let Some(ref current) = self.current_clip {
            let b = skia_path.bounds();
            let w = pw as usize;
            let h = ph as usize;
            let x0 = (b.left().floor().max(0.0) as usize).min(w);
            let x1 = (b.right().ceil().max(0.0) as usize).min(w);
            let y0 = (b.top().floor().max(0.0) as usize).min(h);
            let y1 = (b.bottom().ceil().max(0.0) as usize).min(h);
            let current_data = current.data();
            let mask_data = mask.data_mut();
            for y in y0..y1 {
                let row = y * w;
                for x in x0..x1 {
                    let i = row + x;
                    mask_data[i] = ((mask_data[i] as u16 * current_data[i] as u16) / 255) as u8;
                }
            }
        }

        let frame = match self.current_clip.take() {
            Some(old) => ClipFrame::Mask(old),
            None => ClipFrame::Empty,
        };
        self.clip_stack.push(frame);
        self.current_clip = Some(mask);
    }

    fn pop_clip(&mut self) {
        match self.clip_stack.pop() {
            // Budget-skipped push: leave the active clip untouched.
            Some(ClipFrame::Skipped) => {}
            Some(ClipFrame::Mask(prev)) => self.current_clip = Some(prev),
            Some(ClipFrame::Empty) | None => self.current_clip = None,
        }
    }

    fn push_blend_group(&mut self, blend_mode: BlendMode, alpha: f32, mask: Option<&SoftMask>) {
        // Rasterize the soft mask before parking the base pixmap (needs dims).
        let mask_plane = mask.and_then(|m| self.soft_mask_plane(m));

        let pixmap = match self.pixmap.take() {
            Some(p) => p,
            None => return,
        };
        let w = pixmap.width();
        let h = pixmap.height();

        self.blend_stack.push(BlendEntry {
            pixmap,
            blend_mode,
            alpha,
            mask: mask_plane,
        });

        self.pixmap = tiny_skia::Pixmap::new(w, h);
    }

    fn pop_blend_group(&mut self) {
        let entry = match self.blend_stack.pop() {
            Some(e) => e,
            None => return,
        };

        let mut group_pixmap = match self.pixmap.take() {
            Some(p) => p,
            None => {
                self.pixmap = Some(entry.pixmap);
                return;
            }
        };

        // Fold the soft mask into the group: premultiplied RGBA scales
        // uniformly by the per-pixel mask coverage.
        if let Some(plane) = &entry.mask {
            let data = group_pixmap.data_mut();
            for (px, &m) in data.chunks_exact_mut(4).zip(plane.iter()) {
                if m == 255 {
                    continue;
                }
                let m = m as u16;
                px[0] = ((px[0] as u16 * m) / 255) as u8;
                px[1] = ((px[1] as u16 * m) / 255) as u8;
                px[2] = ((px[2] as u16 * m) / 255) as u8;
                px[3] = ((px[3] as u16 * m) / 255) as u8;
            }
        }

        let mut base = entry.pixmap;
        let blend = Self::blend_mode_to_skia(entry.blend_mode);

        let paint = tiny_skia::PixmapPaint {
            blend_mode: blend,
            opacity: entry.alpha.clamp(0.0, 1.0),
            ..Default::default()
        };

        base.draw_pixmap(
            0,
            0,
            group_pixmap.as_ref(),
            &paint,
            tiny_skia::Transform::identity(),
            None,
        );

        self.pixmap = Some(base);
    }

    /// Coverage plane for `mask`, honoring its page-space offset. The base
    /// raster (at build position) is cached per mask identity; offset uses are
    /// derived by shifting it — exact wherever the base raster painted, and
    /// vacated strips take the mask's unpainted value, which is what a fresh
    /// raster yields outside the group too.
    fn soft_mask_plane(&mut self, mask: &SoftMask) -> Option<Vec<u8>> {
        let (w, h) = match self.pixmap.as_ref() {
            Some(p) => (p.width(), p.height()),
            None => return None,
        };

        // Page-space offset → device pixels (device y grows downward).
        let dx = (mask.offset.0 * self.scale).round() as i64;
        let dy = (-mask.offset.1 * self.scale).round() as i64;

        let key = SoftMaskPlaneKey::new(mask);
        if !self.soft_mask_planes.contains_key(&key) {
            let plane = self.rasterize_soft_mask(mask)?;
            self.soft_mask_planes.insert(key, plane);
        }
        let base = self.soft_mask_planes.get(&key)?;

        if dx == 0 && dy == 0 {
            return Some(base.clone());
        }
        Some(shift_plane(base, w, h, dx, dy, unpainted_value(mask)))
    }

    /// Render a soft mask's group commands offscreen (same page geometry as
    /// the current target) and reduce to a per-pixel coverage plane, ignoring
    /// the offset (callers shift the result).
    fn rasterize_soft_mask(&self, mask: &SoftMask) -> Option<Vec<u8>> {
        let (w, h) = match self.pixmap.as_ref() {
            Some(p) => (p.width(), p.height()),
            None => return None,
        };

        let mut target = tiny_skia::Pixmap::new(w, h)?;
        match mask.kind {
            // Luminosity masks composite the group over the /BC backdrop; the
            // result stays opaque, so luminance reads are exact.
            SoftMaskKind::Luminosity => {
                let l = mask.backdrop_luma.clamp(0.0, 1.0);
                target.fill(tiny_skia::Color::from_rgba(l, l, l, 1.0)?);
            }
            // Alpha masks read group coverage; start fully transparent.
            SoftMaskKind::Alpha => {}
        }

        let mut sub = CpuRenderer {
            pixmap: Some(target),
            scale: self.scale,
            rect_x0: self.rect_x0,
            rect_y0: self.rect_y0,
            rect_y1: self.rect_y1,
            font_cache: self.font_cache,
            image_cache: self.image_cache,
            clip_stack: Vec::new(),
            current_clip: None,
            clip_pixel_spent: 0,
            blend_stack: Vec::new(),
            // Share the parent page's deadline so a soft-mask group cannot hang
            // past the budget the top-level render already started counting.
            render_budget: self.render_budget,
            deadline: self.deadline,
            over_budget: self.over_budget,
            soft_mask_planes: HashMap::new(),
        };
        for cmd in &mask.commands.commands {
            let _ = sub.execute(cmd);
        }
        let rendered = sub.pixmap.take()?;

        let mut plane = Vec::with_capacity((w * h) as usize);
        for px in rendered.pixels() {
            let v = match mask.kind {
                SoftMaskKind::Luminosity => {
                    let a = px.alpha();
                    if a == 0 {
                        (mask.backdrop_luma * 255.0).round() as u8
                    } else {
                        let d = px.demultiply();
                        // Rec. 601 luma.
                        (0.299 * d.red() as f32
                            + 0.587 * d.green() as f32
                            + 0.114 * d.blue() as f32)
                            .round()
                            .min(255.0) as u8
                    }
                }
                SoftMaskKind::Alpha => px.alpha(),
            };
            let v = match &mask.transfer {
                Some(lut) => lut[v as usize],
                None => v,
            };
            plane.push(v);
        }
        Some(plane)
    }

    fn blend_mode_to_skia(mode: BlendMode) -> tiny_skia::BlendMode {
        match mode {
            BlendMode::Normal => tiny_skia::BlendMode::SourceOver,
            BlendMode::Multiply => tiny_skia::BlendMode::Multiply,
            BlendMode::Screen => tiny_skia::BlendMode::Screen,
            BlendMode::Overlay => tiny_skia::BlendMode::Overlay,
            BlendMode::Darken => tiny_skia::BlendMode::Darken,
            BlendMode::Lighten => tiny_skia::BlendMode::Lighten,
            BlendMode::ColorDodge => tiny_skia::BlendMode::ColorDodge,
            BlendMode::ColorBurn => tiny_skia::BlendMode::ColorBurn,
            BlendMode::HardLight => tiny_skia::BlendMode::HardLight,
            BlendMode::SoftLight => tiny_skia::BlendMode::SoftLight,
            BlendMode::Difference => tiny_skia::BlendMode::Difference,
            BlendMode::Exclusion => tiny_skia::BlendMode::Exclusion,
            BlendMode::Hue => tiny_skia::BlendMode::Hue,
            BlendMode::Saturation => tiny_skia::BlendMode::Saturation,
            BlendMode::Color => tiny_skia::BlendMode::Color,
            BlendMode::Luminosity => tiny_skia::BlendMode::Luminosity,
        }
    }

    fn render_image(&mut self, draw: &ImageDraw) {
        let image_cache = match self.image_cache {
            Some(c) => c,
            None => return,
        };
        let image = match image_cache.get(draw.image_id) {
            Some(img) => img,
            None => return,
        };
        let pixmap = match self.pixmap.as_mut() {
            Some(p) => p,
            None => return,
        };

        // PDF images occupy the unit square [0,1]×[0,1] in user space, mapped by
        // the CTM. Image sample space has its origin at the TOP-left with y
        // pointing DOWN (PDF spec §8.9.5.2), so sample row 0 maps to the top
        // edge of the unit square (v = 1). The renderer then applies its fixed
        // page Y-flip — the same one used for every path and glyph — so the
        // CTM's own orientation (including a negative `d`, common in scanned
        // PDFs that store the JPEG upside down) is honored as ordinary geometry.
        //
        // Full chain for image pixel (ix, iy):
        //   ux = ix/iw,  uy = 1 - iy/ih              (sample → unit square)
        //   px = a*ux + c*uy + e                     (unit square → user space)
        //   py = b*ux + d*uy + f
        //   screen_x = (px - x0) * s                 (user space → screen px)
        //   screen_y = (y1 - py) * s                 (fixed page flip)
        //
        // Composed into a single affine in image-pixel coordinates:
        //   screen_x = (a*s/iw)*ix + (-c*s/ih)*iy + (c + e - x0)*s
        //   screen_y = (-b*s/iw)*ix + (d*s/ih)*iy + (y1 - d - f)*s
        let tm = &draw.transform;
        let s = self.scale;
        let (a, b, c, d, e, f) = (
            tm.a as f32,
            tm.b as f32,
            tm.c as f32,
            tm.d as f32,
            tm.e as f32,
            tm.f as f32,
        );

        // Device-space footprint of the unit square's axes: how many device
        // pixels one image axis spans (column lengths of the scaled CTM).
        let dev_w = (a * a + b * b).sqrt() * s;
        let dev_h = (c * c + d * d).sqrt() * s;
        let fx = dev_w / image.width as f32;
        let fy = dev_h / image.height as f32;

        // Below ~0.5x per axis, bilinear sampling starts skipping source pixels
        // entirely (thin strokes in downscaled scans break up). Pre-downscale
        // with a box filter (plain average) to the target device scale first.
        let mut downscaled: Option<(Vec<u8>, u32, u32)> = None;
        if fx < 0.5 || fy < 0.5 {
            let tw = ((image.width as f32 * fx.min(1.0)).ceil() as u32).clamp(1, image.width);
            let th = ((image.height as f32 * fy.min(1.0)).ceil() as u32).clamp(1, image.height);
            if tw < image.width || th < image.height {
                let data = box_downscale_rgba(&image.data, image.width, image.height, tw, th);
                downscaled = Some((data, tw, th));
            }
        }
        let (data, w, h) = match &downscaled {
            Some((data, w, h)) => (data.as_slice(), *w, *h),
            None => (image.data.as_slice(), image.width, image.height),
        };
        let src = match tiny_skia::PixmapRef::from_bytes(data, w, h) {
            Some(p) => p,
            None => return,
        };
        let iw = w as f32;
        let ih = h as f32;

        // tiny-skia `Transform::from_row(sx, ky, kx, sy, tx, ty)` maps
        // (x,y) → (sx*x + kx*y + tx, ky*x + sy*y + ty).
        let transform = tiny_skia::Transform::from_row(
            a * s / iw,                 // sx: screen_x per ix
            -b * s / iw,                // ky: screen_y per ix
            -c * s / ih,                // kx: screen_x per iy
            d * s / ih,                 // sy: screen_y per iy
            (c + e - self.rect_x0) * s, // tx
            (self.rect_y1 - d - f) * s, // ty
        );

        let paint = tiny_skia::PixmapPaint {
            opacity: draw.alpha,
            // Bilinear sampling (matches pdfium); nearest leaves blocky upscales
            // and aliased downscales.
            quality: tiny_skia::FilterQuality::Bilinear,
            ..Default::default()
        };

        pixmap.draw_pixmap(0, 0, src, &paint, transform, self.current_clip.as_ref());
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
            let skia_path = self
                .build_outline_transformed_path(&outline, upem, font_size, tm, glyph.x, glyph.y);
            if let Some(path) = skia_path {
                if let Some(ref mut pixmap) = self.pixmap {
                    pixmap.fill_path(
                        &path,
                        paint,
                        tiny_skia::FillRule::Winding,
                        tiny_skia::Transform::identity(),
                        self.current_clip.as_ref(),
                    );
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn build_outline_transformed_path(
        &self,
        outline: &GlyphOutline,
        upem: f32,
        font_size: f32,
        tm: &zpdf_core::Matrix,
        glyph_x_offset: f32,
        glyph_y_offset: f32,
    ) -> Option<tiny_skia::Path> {
        let mut pb = tiny_skia::PathBuilder::new();
        let off = (glyph_x_offset, glyph_y_offset);

        for cmd in &outline.commands {
            match *cmd {
                OutlineCommand::MoveTo(x, y) => {
                    let (px, py) = self.outline_to_pixel(x, y, upem, font_size, tm, off);
                    pb.move_to(px, py);
                }
                OutlineCommand::LineTo(x, y) => {
                    let (px, py) = self.outline_to_pixel(x, y, upem, font_size, tm, off);
                    pb.line_to(px, py);
                }
                OutlineCommand::QuadTo(x1, y1, x, y) => {
                    let (px1, py1) = self.outline_to_pixel(x1, y1, upem, font_size, tm, off);
                    let (px, py) = self.outline_to_pixel(x, y, upem, font_size, tm, off);
                    pb.quad_to(px1, py1, px, py);
                }
                OutlineCommand::CurveTo(x1, y1, x2, y2, x, y) => {
                    let (px1, py1) = self.outline_to_pixel(x1, y1, upem, font_size, tm, off);
                    let (px2, py2) = self.outline_to_pixel(x2, y2, upem, font_size, tm, off);
                    let (px, py) = self.outline_to_pixel(x, y, upem, font_size, tm, off);
                    pb.cubic_to(px1, py1, px2, py2, px, py);
                }
                OutlineCommand::Close => pb.close(),
            }
        }
        self.finish_within_limits(pb)
    }

    fn outline_to_pixel(
        &self,
        gx: f64,
        gy: f64,
        upem: f32,
        font_size: f32,
        tm: &zpdf_core::Matrix,
        glyph_offset: (f32, f32),
    ) -> (f32, f32) {
        // font units → user space
        let tx = (gx as f32 / upem * font_size + glyph_offset.0) as f64;
        let ty = (gy as f32 / upem * font_size + glyph_offset.1) as f64;

        // user space → page space via combined CTM*Tm
        let page_x = tm.a * tx + tm.c * ty + tm.e;
        let page_y = tm.b * tx + tm.d * ty + tm.f;

        // page space → pixel space
        let ctm_flips_y = tm.d < 0.0 || (tm.d == 0.0 && tm.b != 0.0);
        let px = (page_x as f32 - self.rect_x0) * self.scale;
        let py = if ctm_flips_y {
            (page_y as f32 - self.rect_y0) * self.scale
        } else {
            (self.rect_y1 - page_y as f32) * self.scale
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
                                    self.current_clip.as_ref(),
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
                                width: (style.width
                                    * font_matrix[0].abs() as f32
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
                                    self.current_clip.as_ref(),
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
                    let (px, py) =
                        self.type3_to_pixel(p.x, p.y, font_matrix, font_size, tm, glyph_x_offset);
                    pb.move_to(px, py);
                }
                PathElement::LineTo(p) => {
                    let (px, py) =
                        self.type3_to_pixel(p.x, p.y, font_matrix, font_size, tm, glyph_x_offset);
                    pb.line_to(px, py);
                }
                PathElement::CurveTo(c1, c2, end) => {
                    let (x1, y1) =
                        self.type3_to_pixel(c1.x, c1.y, font_matrix, font_size, tm, glyph_x_offset);
                    let (x2, y2) =
                        self.type3_to_pixel(c2.x, c2.y, font_matrix, font_size, tm, glyph_x_offset);
                    let (x, y) = self.type3_to_pixel(
                        end.x,
                        end.y,
                        font_matrix,
                        font_size,
                        tm,
                        glyph_x_offset,
                    );
                    pb.cubic_to(x1, y1, x2, y2, x, y);
                }
                PathElement::Close => pb.close(),
            }
        }
        self.finish_within_limits(pb)
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

        let px = (page_x as f32 - self.rect_x0) * self.scale;
        let py = if ctm_flips_y {
            // CTM already flipped Y into screen coordinates
            (page_y as f32 - self.rect_y0) * self.scale
        } else {
            // Standard PDF coords: flip Y
            (self.rect_y1 - page_y as f32) * self.scale
        };

        (px, py)
    }
}

impl<'a> Default for CpuRenderer<'a> {
    fn default() -> Self {
        Self::new()
    }
}

/// Box-filter (area-average) downscale of a tight RGBA8 buffer. Each target
/// pixel averages its covering source block, so no source pixel is dropped —
/// unlike point/bilinear sampling at strong minification. Channels are averaged
/// independently, which is correct for the premultiplied data tiny-skia consumes.
fn box_downscale_rgba(data: &[u8], sw: u32, sh: u32, tw: u32, th: u32) -> Vec<u8> {
    debug_assert!(tw >= 1 && th >= 1 && tw <= sw && th <= sh);
    let mut out = vec![0u8; tw as usize * th as usize * 4];
    let (sw64, sh64, tw64, th64) = (sw as u64, sh as u64, tw as u64, th as u64);
    for ty in 0..th as u64 {
        let y0 = (ty * sh64 / th64) as u32;
        let y1 = (((ty + 1) * sh64).div_ceil(th64) as u32).clamp(y0 + 1, sh);
        for tx in 0..tw as u64 {
            let x0 = (tx * sw64 / tw64) as u32;
            let x1 = (((tx + 1) * sw64).div_ceil(tw64) as u32).clamp(x0 + 1, sw);
            let mut acc = [0u64; 4];
            for sy in y0..y1 {
                let row = (sy as usize * sw as usize + x0 as usize) * 4;
                for sx in 0..(x1 - x0) as usize {
                    let px = row + sx * 4;
                    acc[0] += data[px] as u64;
                    acc[1] += data[px + 1] as u64;
                    acc[2] += data[px + 2] as u64;
                    acc[3] += data[px + 3] as u64;
                }
            }
            let n = ((x1 - x0) as u64) * ((y1 - y0) as u64);
            let o = (ty as usize * tw as usize + tx as usize) * 4;
            for ch in 0..4 {
                out[o + ch] = ((acc[ch] + n / 2) / n) as u8;
            }
        }
    }
    out
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
        self.rect_x0 = info.page_rect.x0 as f32;
        self.rect_y0 = info.page_rect.y0 as f32;
        self.rect_y1 = info.page_rect.y1 as f32;
        // Fresh clip state + budget for each page (a renderer may be reused).
        self.clip_stack.clear();
        self.current_clip = None;
        self.clip_pixel_spent = 0;
        self.over_budget = false;
        self.deadline = self.render_budget.map(|d| std::time::Instant::now() + d);

        // ceil(), not truncation: a 595x842pt page at 110 DPI is 909.03x1286.6px
        // and must produce a 910x1287 raster (pdfium semantics) so no content is
        // sliced off the right/bottom edges.
        let mut w = ((info.page_rect.width() * info.scale as f64).ceil() as u32).max(1);
        let mut h = ((info.page_rect.height() * info.scale as f64).ceil() as u32).max(1);

        // Clamp the raster to a total-pixel budget. A pathological MediaBox (or
        // a high DPI on a huge page) otherwise demands a multi-gigabyte Pixmap
        // that aborts the process on the allocation alone, or makes per-pixel
        // work (fills, blits, per-clip masks) hang. Scaling BOTH the dimensions
        // and `self.scale` by the same factor keeps geometry consistent, so the
        // output is a smaller-but-complete render instead of a crash/timeout.
        const MAX_PIXELS: u64 = 64 * 1024 * 1024; // 64 Mpx (~8192×8192)
        let pixels = w as u64 * h as u64;
        if pixels > MAX_PIXELS {
            let shrink = (MAX_PIXELS as f64 / pixels as f64).sqrt();
            self.scale *= shrink as f32;
            w = ((info.page_rect.width() * self.scale as f64).ceil() as u32).max(1);
            h = ((info.page_rect.height() * self.scale as f64).ceil() as u32).max(1);
            tracing::warn!(
                "raster {pixels} px exceeds budget {MAX_PIXELS}; clamped to {w}x{h} (scale ×{shrink:.4})"
            );
        }

        let mut pixmap = tiny_skia::Pixmap::new(w, h).ok_or(CpuRenderError::PixmapCreation)?;

        let bg = &info.background;
        pixmap.fill(
            tiny_skia::Color::from_rgba(bg.r, bg.g, bg.b, bg.a).unwrap_or(tiny_skia::Color::WHITE),
        );

        self.pixmap = Some(pixmap);
        // Mask-plane keys are Arc pointers into the previous display list;
        // they must not survive into a new page.
        self.soft_mask_planes.clear();
        Ok(())
    }

    fn execute(&mut self, cmd: &RenderCommand) -> Result<(), Self::Error> {
        // Anti-hang backstop: once the page's wall-clock budget is spent, skip
        // remaining draws so the command loop drains and the page returns
        // partially rendered instead of hanging. Checked before EVERY command:
        // a page may have only a handful of commands that are each individually
        // very expensive (e.g. dozens of large images scaled across a huge
        // raster), so a coarse stride would never sample the clock in time.
        // Instant::now is cheap enough to call per command.
        if self.over_budget {
            return Ok(());
        }
        if let Some(deadline) = self.deadline {
            if std::time::Instant::now() >= deadline {
                self.over_budget = true;
                tracing::warn!("render exceeded time budget; truncating page");
                return Ok(());
            }
        }
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
            RenderCommand::DrawImage(image_draw) => {
                self.render_image(image_draw);
            }
            RenderCommand::PushClip { path, rule } => {
                self.push_clip(path, rule);
            }
            RenderCommand::PopClip => {
                self.pop_clip();
            }
            RenderCommand::PushBlendGroup {
                blend_mode,
                alpha,
                mask,
                ..
            } => {
                self.push_blend_group(*blend_mode, *alpha, mask.as_ref());
            }
            RenderCommand::PopBlendGroup => {
                self.pop_blend_group();
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

#[cfg(test)]
mod tests {
    use super::*;
    use zpdf_core::{Matrix, Point, Rect};
    use zpdf_image::DecodedImage;

    fn px(page: &RenderedPage, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * page.width + x) * 4) as usize;
        [
            page.data[i],
            page.data[i + 1],
            page.data[i + 2],
            page.data[i + 3],
        ]
    }

    fn line_path(x0: f64, y0: f64, x1: f64, y1: f64) -> Path {
        let mut p = Path::new();
        p.move_to(Point::new(x0, y0));
        p.line_to(Point::new(x1, y1));
        p
    }

    fn stroke_cmd(path: Path, style: StrokeStyle) -> RenderCommand {
        RenderCommand::StrokePath {
            path,
            style,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 0.0)),
            alpha: 1.0,
        }
    }

    fn render(dl: &DisplayList, scale: f32) -> RenderedPage {
        CpuRenderer::new()
            .render_display_list(dl, scale)
            .expect("render")
    }

    #[test]
    fn hairline_stroke_clamps_to_one_device_pixel() {
        // 0.05pt stroke at scale 1 → clamped to 1px. Centered at device y=10.5,
        // it covers row 10 fully; without the clamp the row stays ~95% white.
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        let style = StrokeStyle {
            width: 0.05,
            ..Default::default()
        };
        dl.push(stroke_cmd(line_path(2.0, 9.5, 18.0, 9.5), style));
        let page = render(&dl, 1.0);
        let p = px(&page, 10, 10);
        assert!(p[0] < 60, "hairline row should be dark, got {p:?}");
    }

    #[test]
    fn dash_pattern_produces_gaps() {
        // [4 on, 4 off] along y=9.5 (device row 10): on [0,4), [8,12), [16,20).
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        let style = StrokeStyle {
            width: 2.0,
            dash: Some(DashPattern {
                array: vec![4.0, 4.0],
                phase: 0.0,
            }),
            ..Default::default()
        };
        dl.push(stroke_cmd(line_path(0.0, 9.5, 20.0, 9.5), style));
        let page = render(&dl, 1.0);
        let on = px(&page, 2, 10);
        let off = px(&page, 6, 10);
        assert!(on[0] < 60, "dash 'on' segment should be dark, got {on:?}");
        assert!(
            off[0] > 200,
            "dash 'off' gap should stay white, got {off:?}"
        );
    }

    #[test]
    fn odd_dash_array_repeats_like_doubled() {
        // PDF [4] == 4 on / 4 off (tiny-skia needs the doubled even array).
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        let style = StrokeStyle {
            width: 2.0,
            dash: Some(DashPattern {
                array: vec![4.0],
                phase: 0.0,
            }),
            ..Default::default()
        };
        dl.push(stroke_cmd(line_path(0.0, 9.5, 20.0, 9.5), style));
        let page = render(&dl, 1.0);
        assert!(px(&page, 2, 10)[0] < 60);
        assert!(px(&page, 6, 10)[0] > 200);
    }

    #[test]
    fn degenerate_dash_strokes_solid() {
        // All-zero array is invalid → solid stroke, no gaps.
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        let style = StrokeStyle {
            width: 2.0,
            dash: Some(DashPattern {
                array: vec![0.0, 0.0],
                phase: 0.0,
            }),
            ..Default::default()
        };
        dl.push(stroke_cmd(line_path(0.0, 9.5, 20.0, 9.5), style));
        let page = render(&dl, 1.0);
        assert!(px(&page, 6, 10)[0] < 60, "degenerate dash must draw solid");
    }

    #[test]
    fn raster_dims_use_ceil() {
        // 595x842pt (A4) at 110 DPI: 909.03x1286.6 → 910x1287 (pdfium parity).
        let dl = DisplayList::new(Rect::new(0.0, 0.0, 595.0, 842.0));
        let page = render(&dl, 110.0 / 72.0);
        assert_eq!((page.width, page.height), (910, 1287));

        // Exact integer sizes are unchanged by ceil().
        let dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 50.0));
        let page = render(&dl, 2.0);
        assert_eq!((page.width, page.height), (200, 100));
    }

    #[test]
    fn page_rect_origin_offsets_geometry() {
        // CropBox-style rect (100,50)-(120,70): raster is 20x20 and content is
        // positioned relative to the rect origin.
        let mut dl = DisplayList::new(Rect::new(100.0, 50.0, 120.0, 70.0));
        let mut path = Path::new();
        path.rect(Rect::new(105.0, 55.0, 115.0, 65.0));
        dl.push(RenderCommand::FillPath {
            path,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
        });
        let page = render(&dl, 1.0);
        assert_eq!((page.width, page.height), (20, 20));
        let center = px(&page, 10, 10);
        assert!(
            center[0] > 200 && center[1] < 60,
            "center red, got {center:?}"
        );
        let corner = px(&page, 2, 2);
        assert!(
            corner[0] > 200 && corner[1] > 200,
            "corner white, got {corner:?}"
        );
    }

    #[test]
    fn clip_mask_is_antialiased() {
        // Clip to the lower-left triangle, fill black: the diagonal edge passes
        // through pixel (10,10) at 50% coverage — AA must leave it mid-gray.
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        let mut tri = Path::new();
        tri.move_to(Point::new(0.0, 0.0));
        tri.line_to(Point::new(20.0, 0.0));
        tri.line_to(Point::new(0.0, 20.0));
        tri.close();
        dl.push(RenderCommand::PushClip {
            path: tri,
            rule: FillRule::NonZero,
        });
        let mut full = Path::new();
        full.rect(Rect::new(0.0, 0.0, 20.0, 20.0));
        dl.push(RenderCommand::FillPath {
            path: full,
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 0.0)),
            alpha: 1.0,
        });
        dl.push(RenderCommand::PopClip);
        let page = render(&dl, 1.0);
        let edge = px(&page, 10, 10);
        assert!(
            edge[0] > 30 && edge[0] < 225,
            "clip edge should be AA gray, got {edge:?}"
        );
    }

    #[test]
    fn image_upscale_is_bilinear() {
        // 2x2 black/white checker scaled to 20x20: bilinear leaves the center
        // mid-gray; nearest would snap to pure black/white.
        let mut images = ImageCache::new();
        #[rustfmt::skip]
        let data = vec![
            0, 0, 0, 255,        255, 255, 255, 255,
            255, 255, 255, 255,  0, 0, 0, 255,
        ];
        let id = images.insert(DecodedImage {
            width: 2,
            height: 2,
            data,
            has_alpha: false,
            premultiplied: true,
        });
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 20.0, 20.0));
        dl.push(RenderCommand::DrawImage(ImageDraw {
            image_id: id,
            transform: Matrix::new(20.0, 0.0, 0.0, 20.0, 0.0, 0.0),
            alpha: 1.0,
        }));
        let page = CpuRenderer::new()
            .with_images(&images)
            .render_display_list(&dl, 1.0)
            .expect("render");
        let center = px(&page, 10, 10);
        assert!(
            center[0] > 60 && center[0] < 200,
            "center should interpolate to gray, got {center:?}"
        );
    }

    #[test]
    fn strong_minification_box_filters() {
        // 16x16 image of alternating 1px black/white columns drawn into 4x4
        // device pixels (0.25x): the box pre-downscale averages every column to
        // ~50% gray; nearest/bilinear at that ratio would skip columns entirely.
        let mut images = ImageCache::new();
        let mut data = Vec::with_capacity(16 * 16 * 4);
        for _y in 0..16 {
            for x in 0..16u32 {
                let v = if x % 2 == 0 { 0u8 } else { 255u8 };
                data.extend_from_slice(&[v, v, v, 255]);
            }
        }
        let id = images.insert(DecodedImage {
            width: 16,
            height: 16,
            data,
            has_alpha: false,
            premultiplied: true,
        });
        let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 8.0, 8.0));
        dl.push(RenderCommand::DrawImage(ImageDraw {
            image_id: id,
            transform: Matrix::new(4.0, 0.0, 0.0, 4.0, 2.0, 2.0),
            alpha: 1.0,
        }));
        let page = CpuRenderer::new()
            .with_images(&images)
            .render_display_list(&dl, 1.0)
            .expect("render");
        let inside = px(&page, 4, 4);
        assert!(
            inside[0] > 60 && inside[0] < 200,
            "minified stripes should average to gray, got {inside:?}"
        );
    }

    #[test]
    fn box_downscale_averages_blocks() {
        // 4x2 → 2x1: each output pixel averages a 2x2 block.
        let data = vec![
            0, 0, 0, 255, 255, 255, 255, 255, 100, 100, 100, 255, 200, 200, 200, 255, //
            0, 0, 0, 255, 255, 255, 255, 255, 100, 100, 100, 255, 200, 200, 200, 255,
        ];
        let out = box_downscale_rgba(&data, 4, 2, 2, 1);
        assert_eq!(out.len(), 8);
        assert_eq!(out[0], 128); // round((0+255+0+255)/4)
        assert_eq!(out[4], 150); // (100+200+100+200)/4
        assert_eq!(out[3], 255);
    }

    #[test]
    fn shift_plane_right_down() {
        // 3x3 with a marker at (0,0); shift +1,+1 moves it to (1,1).
        let mut base = vec![10u8; 9];
        base[0] = 200;
        let out = shift_plane(&base, 3, 3, 1, 1, 7);
        assert_eq!(out[3 + 1], 200);
        // Vacated top row and left column take the fill value.
        assert_eq!(&out[0..3], &[7, 7, 7]);
        assert_eq!(out[3], 7);
        assert_eq!(out[6], 7);
        // Copied region keeps base values.
        assert_eq!(out[5], 10);
    }

    #[test]
    fn shift_plane_left_up() {
        let mut base = vec![10u8; 9];
        base[2 * 3 + 2] = 200; // marker at (2,2)
        let out = shift_plane(&base, 3, 3, -1, -1, 7);
        assert_eq!(out[3 + 1], 200);
        // Vacated bottom row and right column take the fill value.
        assert_eq!(&out[6..9], &[7, 7, 7]);
        assert_eq!(out[2], 7);
        assert_eq!(out[5], 7);
    }

    #[test]
    fn shift_plane_out_of_range_is_all_fill() {
        let base = vec![10u8; 9];
        assert_eq!(shift_plane(&base, 3, 3, 3, 0, 7), vec![7u8; 9]);
        assert_eq!(shift_plane(&base, 3, 3, 0, -3, 7), vec![7u8; 9]);
    }

    #[test]
    fn unpainted_value_modes() {
        let mask = |kind, backdrop_luma, transfer| SoftMask {
            kind,
            commands: std::sync::Arc::new(DisplayList::new(Rect::new(0.0, 0.0, 1.0, 1.0))),
            offset: (0.0, 0.0),
            backdrop_luma,
            transfer,
        };
        assert_eq!(unpainted_value(&mask(SoftMaskKind::Alpha, 0.5, None)), 0);
        assert_eq!(
            unpainted_value(&mask(SoftMaskKind::Luminosity, 0.5, None)),
            128
        );
        let inverting = std::sync::Arc::new(std::array::from_fn(|i| 255 - i as u8));
        assert_eq!(
            unpainted_value(&mask(SoftMaskKind::Alpha, 0.0, Some(inverting))),
            255
        );
    }
}
