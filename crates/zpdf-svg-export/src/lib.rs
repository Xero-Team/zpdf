//! Vector-faithful SVG export from zpdf display lists.
//!
//! Converts the flat [`DisplayList`] command stream — the same one the CPU and
//! GPU render backends consume — into a standalone SVG document. Semantics
//! mirror the CPU backend command-for-command: solid paints only (patterns and
//! shadings are already rasterized or tiled upstream by the interpreter), text
//! as glyph-outline paths, Type3 glyphs via their interpreted vector content,
//! images as embedded base64 PNG.
//!
//! Coordinates are emitted in PDF points with the page's fixed Y-flip baked in
//! (`svg = (x - x0, y1 - y)`), so the viewBox equals the page box at 72 dpi and
//! no `transform` is needed on the root.
//!
//! Approximations (each logged once per export via `tracing`): overprint
//! composites paint normally, knockout groups composite as normal groups, and
//! soft-mask `/TR` transfer functions are ignored.

use std::borrow::Cow;
use std::fmt::Write as _;

use image::ImageEncoder;
use zpdf_display_list::{
    BlendMode, Color, DisplayList, FillRule, GlyphRun, ImageDraw, LineCap, LineJoin, Paint, Path,
    PathElement, RenderCommand, SoftMask, SoftMaskKind, StrokeStyle,
};
use zpdf_font::{FontCache, LoadedFont, OutlineCommand};
use zpdf_image::{DecodedImage, ImageCache};

/// Options for SVG export.
#[derive(Debug, Clone)]
pub struct SvgOptions {
    /// Opaque background painted under the page content (PDF pages are
    /// conceptually white paper). `None` leaves the canvas transparent.
    pub background: Option<Color>,
    /// Wall-clock budget for one export (`None` disables). Mirrors the CPU
    /// backend's per-page render budget: adversarial display lists (huge
    /// Type3 fan-outs, thousands of pattern-cell images) stop emitting paint
    /// once the budget is spent, and the document stays well-formed.
    pub budget: Option<std::time::Duration>,
}

/// Same default as the CPU backend's `DEFAULT_RENDER_BUDGET`.
const DEFAULT_EXPORT_BUDGET: std::time::Duration = std::time::Duration::from_secs(8);

impl Default for SvgOptions {
    fn default() -> Self {
        Self {
            background: Some(Color::white()),
            budget: Some(DEFAULT_EXPORT_BUDGET),
        }
    }
}

/// Coordinates beyond this are considered corrupt (degenerate CTMs, fuzzer
/// geometry) and the whole path is skipped, mirroring the CPU backend's
/// `finish_within_limits` guard.
const MAX_COORD: f64 = 1.0e8;

/// Soft masks nest through their own display lists; bound the recursion the
/// same way the renderers bound blend-group depth.
const MAX_MASK_DEPTH: u8 = 8;

/// Convert one page's display list to a standalone SVG document.
///
/// `fonts` and `images` must be the same per-page caches the display list was
/// interpreted with (glyph/image ids index into them). Malformed commands are
/// skipped with a `tracing` diagnostic rather than failing the page, matching
/// render-backend behavior.
pub fn display_list_to_svg(
    display_list: &DisplayList,
    fonts: &FontCache,
    images: &ImageCache,
    options: &SvgOptions,
) -> String {
    let rect = display_list.page_rect;
    let (mut width, mut height) = (rect.width(), rect.height());
    if !(width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0) {
        tracing::warn!(?rect, "degenerate page rect; emitting 1x1 SVG");
        width = 1.0;
        height = 1.0;
    }

    let mut writer = SvgWriter {
        out: String::new(),
        x0: rect.x0,
        y1: rect.y1,
        page_width: width,
        page_height: height,
        fonts,
        images,
        next_id: 0,
        clip: None,
        stroke_mask: None,
        deadline: options.budget.map(|b| zpdf_core::time::Instant::now() + b),
        over_budget: false,
        budget_checks: 0,
        warned_overprint: false,
        warned_knockout: false,
        warned_transfer: false,
    };

    writer
        .out
        .push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    writer
        .out
        .push_str("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"");
    push_num(&mut writer.out, width);
    writer.out.push_str("pt\" height=\"");
    push_num(&mut writer.out, height);
    writer.out.push_str("pt\" viewBox=\"0 0 ");
    push_num(&mut writer.out, width);
    writer.out.push(' ');
    push_num(&mut writer.out, height);
    writer.out.push_str("\">\n");

    if let Some(bg) = &options.background {
        writer.out.push_str("<rect width=\"");
        push_num(&mut writer.out, width);
        writer.out.push_str("\" height=\"");
        push_num(&mut writer.out, height);
        writer.out.push_str("\" fill=\"");
        writer.out.push_str(&hex_color(bg));
        writer.out.push_str("\"/>\n");
    }

    writer.emit_commands(&display_list.commands, 0);

    writer.out.push_str("</svg>\n");
    writer.out
}

/// A structural frame opened by a Push* command.
///
/// Clips are NOT emitted as `<g clip-path>` wrappers: in CSS/SVG compositing a
/// `clip-path` (or `mask`) on a group creates an *isolated* group, so a blend
/// mode inside it would composite against an empty backdrop instead of the
/// page — PDF clips don't isolate. Instead the active clip is tracked as a
/// chain of `<clipPath>` defs (intersected via `clip-path` on the `clipPath`
/// element itself) and referenced from every painted element, which keeps
/// blend groups siblings of their true backdrop. Clip frames record the state
/// to restore on the matching pop.
enum Frame {
    Clip {
        prev_clip: Option<u32>,
        prev_mask: Option<u32>,
    },
    Blend {
        open: bool,
    },
}

struct SvgWriter<'a> {
    out: String,
    x0: f64,
    y1: f64,
    page_width: f64,
    page_height: f64,
    fonts: &'a FontCache,
    images: &'a ImageCache,
    next_id: u32,
    /// Innermost active `<clipPath>` def id (already intersected with outer clips).
    clip: Option<u32>,
    /// Innermost active stroke-clip `<mask>` def id (from `PushClipStroke`).
    stroke_mask: Option<u32>,
    /// Wall-clock deadline for the whole export (shared by mask recursion).
    deadline: Option<zpdf_core::time::Instant>,
    /// Latches once `deadline` passes so the clock is read only occasionally.
    over_budget: bool,
    budget_checks: u32,
    warned_overprint: bool,
    warned_knockout: bool,
    warned_transfer: bool,
}

impl SvgWriter<'_> {
    fn fresh_id(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id
    }

    /// Page space → SVG space: the fixed page Y-flip at scale 1 (points).
    fn map(&self, x: f64, y: f64) -> (f64, f64) {
        (x - self.x0, self.y1 - y)
    }

    /// True once the export's wall-clock budget is spent. Latches; reads the
    /// clock only every few calls (matching the CPU backend's `over_budget`).
    fn check_over_budget(&mut self) -> bool {
        if self.over_budget {
            return true;
        }
        let Some(deadline) = self.deadline else {
            return false;
        };
        self.budget_checks = self.budget_checks.wrapping_add(1);
        if self.budget_checks.is_multiple_of(64) && zpdf_core::time::Instant::now() >= deadline {
            self.over_budget = true;
            tracing::warn!("svg export exceeded time budget; output truncated");
        }
        self.over_budget
    }

    fn emit_commands(&mut self, commands: &[RenderCommand], depth: u8) {
        let mut frames: Vec<Frame> = Vec::new();
        for cmd in commands {
            if self.check_over_budget() {
                break;
            }
            match cmd {
                RenderCommand::FillPath {
                    path,
                    rule,
                    paint,
                    alpha,
                    overprint,
                } => {
                    if overprint.is_some() {
                        self.warn_overprint();
                    }
                    self.emit_fill(path, rule, paint, *alpha);
                }
                RenderCommand::StrokePath {
                    path,
                    style,
                    paint,
                    alpha,
                    overprint,
                } => {
                    if overprint.is_some() {
                        self.warn_overprint();
                    }
                    self.emit_stroke(path, style, paint, *alpha);
                }
                RenderCommand::DrawGlyphRun(run) => self.emit_glyph_run(run),
                RenderCommand::DrawImage(draw) => self.emit_image(draw),
                RenderCommand::PushClip { path, rule } => {
                    let frame = Frame::Clip {
                        prev_clip: self.clip,
                        prev_mask: self.stroke_mask,
                    };
                    self.push_clip(path, rule);
                    frames.push(frame);
                }
                RenderCommand::PushClipStroke { path, style } => {
                    let frame = Frame::Clip {
                        prev_clip: self.clip,
                        prev_mask: self.stroke_mask,
                    };
                    self.push_clip_stroke(path, style);
                    frames.push(frame);
                }
                RenderCommand::PopClip => match frames.last() {
                    Some(Frame::Clip {
                        prev_clip,
                        prev_mask,
                    }) => {
                        self.clip = *prev_clip;
                        self.stroke_mask = *prev_mask;
                        frames.pop();
                    }
                    _ => tracing::warn!("unbalanced PopClip ignored"),
                },
                RenderCommand::PushBlendGroup {
                    blend_mode,
                    isolated,
                    knockout,
                    alpha,
                    mask,
                    ..
                } => {
                    if *knockout {
                        self.warn_knockout();
                    }
                    self.open_blend_group(*blend_mode, *isolated, *alpha, mask.as_ref(), depth);
                    frames.push(Frame::Blend { open: true });
                }
                RenderCommand::PopBlendGroup => match frames.last() {
                    Some(Frame::Blend { open }) => {
                        if *open {
                            self.out.push_str("</g>\n");
                        }
                        frames.pop();
                    }
                    _ => tracing::warn!("unbalanced PopBlendGroup ignored"),
                },
            }
        }
        // A malformed stream can leave frames unclosed; keep the SVG well-formed.
        for frame in frames.drain(..).rev() {
            match frame {
                Frame::Blend { open } => {
                    if open {
                        self.out.push_str("</g>\n");
                    }
                }
                Frame::Clip {
                    prev_clip,
                    prev_mask,
                } => {
                    self.clip = prev_clip;
                    self.stroke_mask = prev_mask;
                }
            }
        }
    }

    /// `clip-path`/`mask` attributes referencing the active clip chain,
    /// appended to every painted element (never to a wrapping `<g>`, which
    /// would create an isolated compositing group — see [`Frame`]).
    fn placement_attrs(&self) -> String {
        let mut s = String::new();
        if let Some(c) = self.clip {
            let _ = write!(s, " clip-path=\"url(#c{c})\"");
        }
        if let Some(m) = self.stroke_mask {
            let _ = write!(s, " mask=\"url(#m{m})\"");
        }
        s
    }

    // -- fills / strokes --

    fn emit_fill(&mut self, path: &Path, rule: &FillRule, paint: &Paint, alpha: f32) {
        let Paint::Solid(color) = paint else {
            tracing::debug!("skipping non-solid fill paint (rasterized upstream)");
            return;
        };
        let opacity = (color.a * alpha).clamp(0.0, 1.0);
        if opacity <= 0.0 {
            return;
        }
        let Some(d) = self.path_data(path) else {
            return;
        };
        self.out.push_str("<path d=\"");
        self.out.push_str(&d);
        self.out.push_str("\" fill=\"");
        self.out.push_str(&hex_color(color));
        self.out.push('"');
        if *rule == FillRule::EvenOdd {
            self.out.push_str(" fill-rule=\"evenodd\"");
        }
        push_opacity(&mut self.out, "fill-opacity", opacity);
        let placement = self.placement_attrs();
        self.out.push_str(&placement);
        self.out.push_str("/>\n");
    }

    fn emit_stroke(&mut self, path: &Path, style: &StrokeStyle, paint: &Paint, alpha: f32) {
        let Paint::Solid(color) = paint else {
            tracing::debug!("skipping non-solid stroke paint (rasterized upstream)");
            return;
        };
        let opacity = (color.a * alpha).clamp(0.0, 1.0);
        if opacity <= 0.0 {
            return;
        }
        let Some(d) = self.path_data(path) else {
            return;
        };
        self.out.push_str("<path d=\"");
        self.out.push_str(&d);
        self.out.push_str("\" fill=\"none\" stroke=\"");
        self.out.push_str(&hex_color(color));
        self.out.push('"');
        push_opacity(&mut self.out, "stroke-opacity", opacity);
        push_stroke_attrs(&mut self.out, style);
        let placement = self.placement_attrs();
        self.out.push_str(&placement);
        self.out.push_str("/>\n");
    }

    // -- clips --

    /// Register a `<clipPath>` def intersected with the enclosing clip (via
    /// `clip-path` on the `clipPath` element itself, SVG 1.1 §14.3.5) and make
    /// it the active clip. Degenerate geometry leaves the outer clip in force,
    /// like the CPU backend's skipped clip frames.
    fn push_clip(&mut self, path: &Path, rule: &FillRule) {
        let Some(d) = self.path_data(path) else {
            return;
        };
        let id = self.fresh_id();
        let _ = write!(self.out, "<clipPath id=\"c{id}\"");
        if let Some(outer) = self.clip {
            let _ = write!(self.out, " clip-path=\"url(#c{outer})\"");
        }
        let _ = write!(self.out, "><path d=\"{d}\"");
        if *rule == FillRule::EvenOdd {
            self.out.push_str(" clip-rule=\"evenodd\"");
        }
        self.out.push_str("/></clipPath>\n");
        self.clip = Some(id);
    }

    /// A stroked clip has no SVG `<clipPath>` equivalent; a luminance mask with
    /// a white stroke is exactly the coverage the CPU backend rasterizes. The
    /// def bakes in the enclosing clip and mask so intersection nests.
    fn push_clip_stroke(&mut self, path: &Path, style: &StrokeStyle) {
        let Some(d) = self.path_data(path) else {
            return;
        };
        let id = self.fresh_id();
        let _ = write!(self.out, "<mask id=\"m{id}\" maskUnits=\"userSpaceOnUse\">");
        if let Some(outer) = self.stroke_mask {
            let _ = write!(self.out, "<g mask=\"url(#m{outer})\">");
        }
        let _ = write!(self.out, "<path d=\"{d}\" fill=\"none\" stroke=\"#ffffff\"");
        push_stroke_attrs(&mut self.out, style);
        if let Some(c) = self.clip {
            let _ = write!(self.out, " clip-path=\"url(#c{c})\"");
        }
        self.out.push_str("/>");
        if self.stroke_mask.is_some() {
            self.out.push_str("</g>");
        }
        self.out.push_str("</mask>\n");
        self.stroke_mask = Some(id);
    }

    // -- transparency groups --

    fn open_blend_group(
        &mut self,
        blend_mode: BlendMode,
        isolated: bool,
        alpha: f32,
        mask: Option<&SoftMask>,
        depth: u8,
    ) {
        let mask_id = mask.and_then(|m| self.emit_soft_mask_def(m, depth));
        self.out.push_str("<g");
        let alpha = if alpha.is_finite() {
            alpha.clamp(0.0, 1.0)
        } else {
            1.0
        };
        if alpha < 1.0 {
            self.out.push_str(" opacity=\"");
            push_num(&mut self.out, alpha as f64);
            self.out.push('"');
        }
        let blend = blend_css(blend_mode);
        if blend.is_some() || isolated {
            self.out.push_str(" style=\"");
            if let Some(name) = blend {
                let _ = write!(self.out, "mix-blend-mode:{name};");
            }
            if isolated {
                self.out.push_str("isolation:isolate;");
            }
            self.out.push('"');
        }
        if let Some(id) = mask_id {
            let _ = write!(self.out, " mask=\"url(#m{id})\"");
        }
        self.out.push_str(">\n");
    }

    /// Emit an ExtGState soft mask as an SVG `<mask>` def and return its id.
    /// SVG masks are luminance-based by default, which is exactly PDF's
    /// luminosity soft mask; alpha masks use `mask-type:alpha`.
    fn emit_soft_mask_def(&mut self, mask: &SoftMask, depth: u8) -> Option<u32> {
        if depth >= MAX_MASK_DEPTH {
            tracing::warn!("soft-mask nesting exceeds depth budget; mask dropped");
            return None;
        }
        if mask.transfer.is_some() {
            self.warn_transfer();
        }
        let id = self.fresh_id();
        let _ = write!(self.out, "<mask id=\"m{id}\" maskUnits=\"userSpaceOnUse\"");
        if mask.kind == SoftMaskKind::Alpha {
            self.out.push_str(" style=\"mask-type:alpha\"");
        }
        self.out.push_str(">\n");
        // Mask content is self-contained (geometry fixed at `gs` time); it must
        // not inherit the page's active clip chain.
        let (saved_clip, saved_mask) = (self.clip.take(), self.stroke_mask.take());
        // /BC backdrop: areas the mask group leaves unpainted read as this
        // luminosity. SVG masks default to black (fully masked), so only a
        // non-zero backdrop needs painting.
        if mask.kind == SoftMaskKind::Luminosity && mask.backdrop_luma > 0.0 {
            let luma = Color::gray(mask.backdrop_luma.clamp(0.0, 1.0));
            self.out.push_str("<rect x=\"0\" y=\"0\" width=\"");
            push_num(&mut self.out, self.page_width);
            self.out.push_str("\" height=\"");
            push_num(&mut self.out, self.page_height);
            self.out.push_str("\" fill=\"");
            self.out.push_str(&hex_color(&luma));
            self.out.push_str("\"/>\n");
        }
        let (dx, dy) = (mask.offset.0 as f64, mask.offset.1 as f64);
        let translated = dx != 0.0 || dy != 0.0;
        if translated {
            // Page-space translation; the Y-flip negates the vertical part.
            self.out.push_str("<g transform=\"translate(");
            push_num(&mut self.out, dx);
            self.out.push(' ');
            push_num(&mut self.out, -dy);
            self.out.push_str(")\">\n");
        }
        self.emit_commands(&mask.commands.commands, depth + 1);
        if translated {
            self.out.push_str("</g>\n");
        }
        self.out.push_str("</mask>\n");
        self.clip = saved_clip;
        self.stroke_mask = saved_mask;
        Some(id)
    }

    // -- text --

    fn emit_glyph_run(&mut self, run: &GlyphRun) {
        let Paint::Solid(color) = &run.paint else {
            tracing::debug!("skipping non-solid text paint (rasterized upstream)");
            return;
        };
        if run.overprint.is_some() {
            self.warn_overprint();
        }
        let opacity = (color.a * run.alpha).clamp(0.0, 1.0);
        if opacity <= 0.0 {
            return;
        }
        let Some(font) = self.fonts.get(run.font_id) else {
            tracing::warn!(font_id = run.font_id, "glyph run references unknown font");
            return;
        };
        if !font.has_font_data() {
            return;
        }
        if font.is_type3() {
            self.emit_type3_run(run, font, color, opacity);
        } else {
            self.emit_outline_run(run, font, color, opacity);
        }
    }

    /// Outline fonts: all glyphs of the run merge into one nonzero-filled path.
    fn emit_outline_run(&mut self, run: &GlyphRun, font: &LoadedFont, color: &Color, opacity: f32) {
        let upem = font.units_per_em;
        if !(upem.is_finite() && upem > 0.0) {
            return;
        }
        let mut d = String::new();
        for glyph in &run.glyphs {
            let Some(outline) = font.glyph_outline(glyph.glyph_id) else {
                continue;
            };
            let off = (glyph.x, glyph.y);
            let mut ok = true;
            let mut part = String::new();
            for cmd in &outline.commands {
                match *cmd {
                    OutlineCommand::MoveTo(x, y) => {
                        ok &= self.push_glyph_point(&mut part, 'M', &[(x, y)], upem, run, off);
                    }
                    OutlineCommand::LineTo(x, y) => {
                        ok &= self.push_glyph_point(&mut part, 'L', &[(x, y)], upem, run, off);
                    }
                    OutlineCommand::QuadTo(x1, y1, x, y) => {
                        ok &= self.push_glyph_point(
                            &mut part,
                            'Q',
                            &[(x1, y1), (x, y)],
                            upem,
                            run,
                            off,
                        );
                    }
                    OutlineCommand::CurveTo(x1, y1, x2, y2, x, y) => {
                        ok &= self.push_glyph_point(
                            &mut part,
                            'C',
                            &[(x1, y1), (x2, y2), (x, y)],
                            upem,
                            run,
                            off,
                        );
                    }
                    OutlineCommand::Close => part.push('Z'),
                }
                if !ok {
                    break;
                }
            }
            if ok {
                d.push_str(&part);
            }
        }
        if d.is_empty() {
            return;
        }
        self.out.push_str("<path d=\"");
        self.out.push_str(&d);
        self.out.push_str("\" fill=\"");
        self.out.push_str(&hex_color(color));
        self.out.push('"');
        push_opacity(&mut self.out, "fill-opacity", opacity);
        let placement = self.placement_attrs();
        self.out.push_str(&placement);
        self.out.push_str("/>\n");
    }

    /// Append one SVG path verb whose points run through the glyph transform.
    /// Returns false (caller drops the glyph) on non-finite/corrupt geometry.
    fn push_glyph_point(
        &self,
        d: &mut String,
        verb: char,
        points: &[(f64, f64)],
        upem: f64,
        run: &GlyphRun,
        off: (f32, f32),
    ) -> bool {
        d.push(verb);
        for &(gx, gy) in points {
            // font units → text space; the shape x carries Th (Tz/100), the
            // offset already includes it (accumulated advance).
            let tx = gx / upem * run.font_size as f64 * run.h_scale as f64 + off.0 as f64;
            let ty = gy / upem * run.font_size as f64 + off.1 as f64;
            let tm = &run.transform;
            let px = tm.a * tx + tm.c * ty + tm.e;
            let py = tm.b * tx + tm.d * ty + tm.f;
            let (sx, sy) = self.map(px, py);
            if !coord_ok(sx) || !coord_ok(sy) {
                return false;
            }
            push_pair(d, sx, sy);
        }
        true
    }

    /// Type3 glyphs: interpret the glyph's content stream (as the CPU backend
    /// does) and emit its fill/stroke paths through the Type3 transform chain.
    fn emit_type3_run(&mut self, run: &GlyphRun, font: &LoadedFont, color: &Color, opacity: f32) {
        use zpdf_content::interpreter::ContentInterpreter;

        for glyph in &run.glyphs {
            if self.check_over_budget() {
                break;
            }
            let Some((stream, font_matrix)) = font.type3_glyph_stream(glyph.glyph_id) else {
                continue;
            };
            let glyph_rect = zpdf_core::Rect::new(0.0, -1000.0, 1000.0, 1000.0);
            let glyph_dl = ContentInterpreter::new(glyph_rect).interpret(stream);

            for cmd in &glyph_dl.commands {
                match cmd {
                    RenderCommand::FillPath { path, rule, .. } => {
                        let Some(d) = self.type3_path_data(path, &font_matrix, run, glyph.x) else {
                            continue;
                        };
                        self.out.push_str("<path d=\"");
                        self.out.push_str(&d);
                        self.out.push_str("\" fill=\"");
                        self.out.push_str(&hex_color(color));
                        self.out.push('"');
                        if *rule == FillRule::EvenOdd {
                            self.out.push_str(" fill-rule=\"evenodd\"");
                        }
                        push_opacity(&mut self.out, "fill-opacity", opacity);
                        let placement = self.placement_attrs();
                        self.out.push_str(&placement);
                        self.out.push_str("/>\n");
                    }
                    RenderCommand::StrokePath { path, style, .. } => {
                        let Some(d) = self.type3_path_data(path, &font_matrix, run, glyph.x) else {
                            continue;
                        };
                        // Stroke width scales from glyph space through the
                        // FontMatrix and font size (page units at scale 1).
                        let width =
                            (style.width as f64 * font_matrix[0].abs() * run.font_size as f64)
                                .max(0.05);
                        self.out.push_str("<path d=\"");
                        self.out.push_str(&d);
                        self.out.push_str("\" fill=\"none\" stroke=\"");
                        self.out.push_str(&hex_color(color));
                        self.out.push_str("\" stroke-width=\"");
                        push_num(&mut self.out, width);
                        self.out.push('"');
                        push_opacity(&mut self.out, "stroke-opacity", opacity);
                        let placement = self.placement_attrs();
                        self.out.push_str(&placement);
                        self.out.push_str("/>\n");
                    }
                    _ => {}
                }
            }
        }
    }

    fn type3_path_data(
        &self,
        path: &Path,
        font_matrix: &[f64; 6],
        run: &GlyphRun,
        glyph_x_offset: f32,
    ) -> Option<String> {
        let map = |gx: f64, gy: f64| -> Option<(f64, f64)> {
            // FontMatrix → text space, scale by size (x also carries Th),
            // advance offset, then Tm/CTM, then the fixed page flip.
            let tx = font_matrix[0] * gx + font_matrix[2] * gy + font_matrix[4];
            let ty = font_matrix[1] * gx + font_matrix[3] * gy + font_matrix[5];
            let tx = tx * run.font_size as f64 * run.h_scale as f64 + glyph_x_offset as f64;
            let ty = ty * run.font_size as f64;
            let tm = &run.transform;
            let px = tm.a * tx + tm.c * ty + tm.e;
            let py = tm.b * tx + tm.d * ty + tm.f;
            let (sx, sy) = self.map(px, py);
            (coord_ok(sx) && coord_ok(sy)).then_some((sx, sy))
        };
        self.path_data_with(path, map)
    }

    // -- images --

    fn emit_image(&mut self, draw: &ImageDraw) {
        let alpha = if draw.alpha.is_finite() {
            draw.alpha.clamp(0.0, 1.0)
        } else {
            0.0
        };
        if alpha <= 0.0 {
            return;
        }
        let Some(image) = self.images.get(draw.image_id) else {
            tracing::warn!(
                image_id = draw.image_id,
                "image draw references unknown image"
            );
            return;
        };
        let Some(png) = encode_png(image) else {
            tracing::warn!(image_id = draw.image_id, "skipping unencodable image");
            return;
        };

        // PDF images occupy the unit square mapped by the CTM, with sample row
        // 0 at the TOP edge (v = 1); composing with the fixed page flip at
        // scale 1 gives, for image pixel (ix, iy):
        //   svg_x = (a/iw)·ix + (-c/ih)·iy + (c + e - x0)
        //   svg_y = (-b/iw)·ix + (d/ih)·iy + (y1 - d - f)
        // — the same affine as the CPU backend's `render_image`.
        let tm = &draw.transform;
        let iw = image.width as f64;
        let ih = image.height as f64;
        let m = [
            tm.a / iw,
            -tm.b / iw,
            -tm.c / ih,
            tm.d / ih,
            tm.c + tm.e - self.x0,
            self.y1 - tm.d - tm.f,
        ];
        if m.iter().any(|v| !v.is_finite() || v.abs() > MAX_COORD) {
            tracing::warn!(
                image_id = draw.image_id,
                "skipping image with corrupt transform"
            );
            return;
        }

        // The image's own `transform` redefines its user space, and `clip-path`
        // on an element resolves in that (post-transform) space — so the active
        // clip must go on a plain wrapper `<g>` instead. A wrapper around a
        // single Normal-blended leaf cannot change compositing (the isolation
        // a clipped group introduces only matters for blend modes crossing it).
        let placement = self.placement_attrs();
        let wrapped = !placement.is_empty();
        if wrapped {
            let _ = write!(self.out, "<g{placement}>");
        }
        let _ = write!(
            self.out,
            "<image width=\"{}\" height=\"{}\" preserveAspectRatio=\"none\" transform=\"matrix(",
            image.width, image.height
        );
        for (i, v) in m.iter().enumerate() {
            if i > 0 {
                self.out.push(' ');
            }
            push_num(&mut self.out, *v);
        }
        self.out.push_str(")\"");
        push_opacity(&mut self.out, "opacity", alpha);
        self.out.push_str(" href=\"data:image/png;base64,");
        push_base64(&mut self.out, &png);
        self.out.push_str("\"/>");
        if wrapped {
            self.out.push_str("</g>");
        }
        self.out.push('\n');
    }

    // -- path serialization --

    fn path_data(&self, path: &Path) -> Option<String> {
        let map = |x: f64, y: f64| -> Option<(f64, f64)> {
            let (sx, sy) = self.map(x, y);
            (coord_ok(sx) && coord_ok(sy)).then_some((sx, sy))
        };
        self.path_data_with(path, map)
    }

    /// Serialize a path through a point-mapping closure. Returns `None` when
    /// the path is empty or any point maps to corrupt geometry (the caller
    /// skips the command, as the raster backends do).
    fn path_data_with(
        &self,
        path: &Path,
        map: impl Fn(f64, f64) -> Option<(f64, f64)>,
    ) -> Option<String> {
        if path.is_empty() {
            return None;
        }
        let mut d = String::new();
        for elem in &path.elements {
            match *elem {
                PathElement::MoveTo(p) => {
                    let (x, y) = map(p.x, p.y)?;
                    d.push('M');
                    push_pair(&mut d, x, y);
                }
                PathElement::LineTo(p) => {
                    let (x, y) = map(p.x, p.y)?;
                    d.push('L');
                    push_pair(&mut d, x, y);
                }
                PathElement::CurveTo(c1, c2, end) => {
                    let (x1, y1) = map(c1.x, c1.y)?;
                    let (x2, y2) = map(c2.x, c2.y)?;
                    let (x, y) = map(end.x, end.y)?;
                    d.push('C');
                    push_pair(&mut d, x1, y1);
                    d.push(' ');
                    push_pair(&mut d, x2, y2);
                    d.push(' ');
                    push_pair(&mut d, x, y);
                }
                PathElement::Close => d.push('Z'),
            }
        }
        (!d.is_empty()).then_some(d)
    }

    // -- one-shot diagnostics --

    fn warn_overprint(&mut self) {
        if !self.warned_overprint {
            self.warned_overprint = true;
            tracing::warn!("overprint has no SVG equivalent; painting normally");
        }
    }

    fn warn_knockout(&mut self) {
        if !self.warned_knockout {
            self.warned_knockout = true;
            tracing::warn!("knockout group has no SVG equivalent; compositing as normal group");
        }
    }

    fn warn_transfer(&mut self) {
        if !self.warned_transfer {
            self.warned_transfer = true;
            tracing::warn!("soft-mask /TR transfer function has no SVG equivalent; ignored");
        }
    }
}

fn coord_ok(v: f64) -> bool {
    v.is_finite() && v.abs() <= MAX_COORD
}

/// Format a coordinate/length with up to 3 decimals, trailing zeros trimmed.
fn push_num(out: &mut String, v: f64) {
    let r = (v * 1000.0).round() / 1000.0;
    if r == r.trunc() && r.abs() < 1e15 {
        let _ = write!(out, "{}", r as i64);
    } else {
        let mut s = format!("{r:.3}");
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
        out.push_str(&s);
    }
}

fn push_pair(out: &mut String, x: f64, y: f64) {
    push_num(out, x);
    out.push(' ');
    push_num(out, y);
}

fn push_opacity(out: &mut String, attr: &str, opacity: f32) {
    if opacity < 1.0 {
        let _ = write!(out, " {attr}=\"");
        push_num(out, opacity as f64);
        out.push('"');
    }
}

/// Same quantization as the CPU backend's `color_to_paint` (truncating cast).
fn hex_color(c: &Color) -> String {
    format!(
        "#{:02x}{:02x}{:02x}",
        (c.r.clamp(0.0, 1.0) * 255.0) as u8,
        (c.g.clamp(0.0, 1.0) * 255.0) as u8,
        (c.b.clamp(0.0, 1.0) * 255.0) as u8,
    )
}

fn blend_css(mode: BlendMode) -> Option<&'static str> {
    Some(match mode {
        BlendMode::Normal => return None,
        BlendMode::Multiply => "multiply",
        BlendMode::Screen => "screen",
        BlendMode::Overlay => "overlay",
        BlendMode::Darken => "darken",
        BlendMode::Lighten => "lighten",
        BlendMode::ColorDodge => "color-dodge",
        BlendMode::ColorBurn => "color-burn",
        BlendMode::HardLight => "hard-light",
        BlendMode::SoftLight => "soft-light",
        BlendMode::Difference => "difference",
        BlendMode::Exclusion => "exclusion",
        BlendMode::Hue => "hue",
        BlendMode::Saturation => "saturation",
        BlendMode::Color => "color",
        BlendMode::Luminosity => "luminosity",
    })
}

fn push_stroke_attrs(out: &mut String, style: &StrokeStyle) {
    // PDF width 0 means "thinnest device line"; SVG width 0 renders nothing.
    let width = if style.width.is_finite() && style.width > 0.0 {
        style.width as f64
    } else {
        0.1
    };
    out.push_str(" stroke-width=\"");
    push_num(out, width);
    out.push('"');
    match style.cap {
        LineCap::Butt => {}
        LineCap::Round => out.push_str(" stroke-linecap=\"round\""),
        LineCap::Square => out.push_str(" stroke-linecap=\"square\""),
    }
    match style.join {
        LineJoin::Miter => {
            // SVG's default miterlimit is 4; PDF's is 10, so emit explicitly.
            if style.miter_limit.is_finite() && style.miter_limit >= 1.0 {
                out.push_str(" stroke-miterlimit=\"");
                push_num(out, style.miter_limit as f64);
                out.push('"');
            }
        }
        LineJoin::Round => out.push_str(" stroke-linejoin=\"round\""),
        LineJoin::Bevel => out.push_str(" stroke-linejoin=\"bevel\""),
    }
    if let Some(dash) = &style.dash {
        // SVG natively repeats odd-length dash arrays, matching PDF 8.4.3.6.
        if !zpdf_render::dash::is_degenerate(&dash.array) && dash.phase.is_finite() {
            out.push_str(" stroke-dasharray=\"");
            for (i, v) in dash.array.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                push_num(out, *v as f64);
            }
            out.push('"');
            if dash.phase != 0.0 {
                out.push_str(" stroke-dashoffset=\"");
                push_num(out, dash.phase as f64);
                out.push('"');
            }
        }
    }
}

/// PNG-encode a decoded RGBA image, un-premultiplying alpha first when needed
/// (PNG stores straight alpha; `DecodedImage` is premultiplied once a soft
/// mask or stencil has been folded in).
fn encode_png(image: &DecodedImage) -> Option<Vec<u8>> {
    let expected = (image.width as usize)
        .checked_mul(image.height as usize)?
        .checked_mul(4)?;
    if image.width == 0 || image.height == 0 || image.data.len() != expected {
        return None;
    }
    let data: Cow<'_, [u8]> = if image.premultiplied {
        let mut d = image.data.clone();
        unpremultiply(&mut d);
        Cow::Owned(d)
    } else {
        Cow::Borrowed(&image.data)
    };
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(
            &data,
            image.width,
            image.height,
            image::ExtendedColorType::Rgba8,
        )
        .ok()?;
    Some(png)
}

fn unpremultiply(data: &mut [u8]) {
    for px in data.as_chunks_mut::<4>().0 {
        let a = px[3] as u32;
        if a == 0 {
            px[0] = 0;
            px[1] = 0;
            px[2] = 0;
        } else if a < 255 {
            for c in &mut px[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
}

fn push_base64(out: &mut String, data: &[u8]) {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for chunk in data.chunks(3) {
        let b1 = chunk[0] as u32;
        let b2 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b3 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b1 << 16) | (b2 << 8) | b3;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zpdf_core::{Matrix, Point, Rect};
    use zpdf_display_list::{DashPattern, ImageDraw, PositionedGlyph};

    fn empty_caches() -> (FontCache, ImageCache) {
        (FontCache::new(), ImageCache::new())
    }

    fn page() -> DisplayList {
        DisplayList::new(Rect::new(0.0, 0.0, 100.0, 200.0))
    }

    fn to_svg(dl: &DisplayList) -> String {
        let (fonts, images) = empty_caches();
        display_list_to_svg(dl, &fonts, &images, &SvgOptions::default())
    }

    fn rect_path(x0: f64, y0: f64, x1: f64, y1: f64) -> Path {
        let mut p = Path::new();
        p.rect(Rect::new(x0, y0, x1, y1));
        p
    }

    #[test]
    fn fill_is_y_flipped_into_viewbox() {
        let mut dl = page();
        dl.push(RenderCommand::FillPath {
            path: rect_path(10.0, 20.0, 30.0, 40.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        let svg = to_svg(&dl);
        assert!(svg.contains("viewBox=\"0 0 100 200\""), "{svg}");
        // (10,20) bottom-left → svg y = 200-20 = 180; (30,40) → y = 160.
        assert!(svg.contains("M10 180L30 180L30 160L10 160Z"), "{svg}");
        assert!(svg.contains("fill=\"#ff0000\""), "{svg}");
        // NonZero is SVG's default; no fill-rule attribute.
        assert!(!svg.contains("fill-rule"), "{svg}");
    }

    #[test]
    fn even_odd_and_alpha_are_emitted() {
        let mut dl = page();
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 10.0, 10.0),
            rule: FillRule::EvenOdd,
            paint: Paint::Solid(Color::rgba(0.0, 0.0, 0.0, 1.0)),
            alpha: 0.5,
            overprint: None,
        });
        let svg = to_svg(&dl);
        assert!(svg.contains("fill-rule=\"evenodd\""), "{svg}");
        assert!(svg.contains("fill-opacity=\"0.5\""), "{svg}");
    }

    #[test]
    fn stroke_attributes_round_trip() {
        let mut dl = page();
        dl.push(RenderCommand::StrokePath {
            path: rect_path(0.0, 0.0, 50.0, 50.0),
            style: StrokeStyle {
                width: 2.5,
                cap: LineCap::Round,
                join: LineJoin::Bevel,
                miter_limit: 10.0,
                dash: Some(DashPattern {
                    array: vec![3.0],
                    phase: 1.5,
                }),
            },
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        let svg = to_svg(&dl);
        assert!(svg.contains("fill=\"none\""), "{svg}");
        assert!(svg.contains("stroke=\"#0000ff\""), "{svg}");
        assert!(svg.contains("stroke-width=\"2.5\""), "{svg}");
        assert!(svg.contains("stroke-linecap=\"round\""), "{svg}");
        assert!(svg.contains("stroke-linejoin=\"bevel\""), "{svg}");
        assert!(svg.contains("stroke-dasharray=\"3\""), "{svg}");
        assert!(svg.contains("stroke-dashoffset=\"1.5\""), "{svg}");
    }

    #[test]
    fn degenerate_dash_is_dropped() {
        let mut dl = page();
        dl.push(RenderCommand::StrokePath {
            path: rect_path(0.0, 0.0, 50.0, 50.0),
            style: StrokeStyle {
                dash: Some(DashPattern {
                    array: vec![0.0, 0.0],
                    phase: 0.0,
                }),
                ..StrokeStyle::default()
            },
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
        });
        let svg = to_svg(&dl);
        assert!(!svg.contains("stroke-dasharray"), "{svg}");
        // PDF miter default 10 must be explicit (SVG default is 4).
        assert!(svg.contains("stroke-miterlimit=\"10\""), "{svg}");
    }

    #[test]
    fn zero_width_stroke_gets_hairline() {
        let mut dl = page();
        dl.push(RenderCommand::StrokePath {
            path: rect_path(0.0, 0.0, 50.0, 50.0),
            style: StrokeStyle {
                width: 0.0,
                ..StrokeStyle::default()
            },
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
        });
        assert!(to_svg(&dl).contains("stroke-width=\"0.1\""));
    }

    #[test]
    fn clips_nest_and_balance() {
        let mut dl = page();
        dl.push(RenderCommand::PushClip {
            path: rect_path(0.0, 0.0, 50.0, 50.0),
            rule: FillRule::EvenOdd,
        });
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 10.0, 10.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PopClip);
        // A fill after the pop must NOT carry the clip reference.
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 5.0, 5.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        let svg = to_svg(&dl);
        assert!(svg.contains("<clipPath id=\"c1\">"), "{svg}");
        assert!(svg.contains("clip-rule=\"evenodd\""), "{svg}");
        // Clips are element attributes, never `<g>` wrappers (a `clip-path` on
        // a group isolates blending — PDF clips must not).
        assert!(!svg.contains("<g clip-path"), "{svg}");
        assert!(
            svg.contains("fill=\"#000000\" clip-path=\"url(#c1)\""),
            "{svg}"
        );
        assert!(!svg.contains("fill=\"#ffffff\" clip-path"), "{svg}");
    }

    #[test]
    fn nested_clips_intersect_via_clippath_chain() {
        let mut dl = page();
        dl.push(RenderCommand::PushClip {
            path: rect_path(0.0, 0.0, 80.0, 80.0),
            rule: FillRule::NonZero,
        });
        dl.push(RenderCommand::PushClip {
            path: rect_path(10.0, 10.0, 60.0, 60.0),
            rule: FillRule::NonZero,
        });
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 10.0, 10.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PopClip);
        dl.push(RenderCommand::PopClip);
        let svg = to_svg(&dl);
        // Inner def carries the outer def: intersection without wrappers.
        assert!(
            svg.contains("<clipPath id=\"c2\" clip-path=\"url(#c1)\">"),
            "{svg}"
        );
        assert!(svg.contains("clip-path=\"url(#c2)\"/>"), "{svg}");
    }

    #[test]
    fn unbalanced_stream_still_produces_wellformed_groups() {
        let mut dl = page();
        dl.push(RenderCommand::PopClip); // stray pop: ignored
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Screen,
            isolated: false,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
            alpha: 1.0,
            mask: None,
        }); // never popped: auto-closed
        let svg = to_svg(&dl);
        assert_eq!(svg.matches("<g ").count(), svg.matches("</g>").count());
    }

    #[test]
    fn skipped_clip_keeps_pop_balanced_and_outer_clip_in_force() {
        let mut dl = page();
        dl.push(RenderCommand::PushClip {
            path: rect_path(0.0, 0.0, 50.0, 50.0),
            rule: FillRule::NonZero,
        });
        let mut bad = Path::new();
        bad.move_to(Point::new(f64::NAN, 0.0));
        bad.line_to(Point::new(10.0, 10.0));
        dl.push(RenderCommand::PushClip {
            path: bad,
            rule: FillRule::NonZero,
        });
        // Under a skipped inner clip, the OUTER clip must stay in force —
        // strictly tighter than dropping clipping, like the CPU backend.
        dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 10.0, 10.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PopClip);
        dl.push(RenderCommand::PopClip);
        let svg = to_svg(&dl);
        assert!(svg.contains("clip-path=\"url(#c1)\"/>"), "{svg}");
        assert!(!svg.contains("id=\"c2\""), "{svg}");
    }

    #[test]
    fn blend_group_maps_to_css() {
        let mut dl = page();
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Multiply,
            isolated: true,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 100.0, 200.0),
            alpha: 0.75,
            mask: None,
        });
        dl.push(RenderCommand::PopBlendGroup);
        let svg = to_svg(&dl);
        assert!(svg.contains("opacity=\"0.75\""), "{svg}");
        assert!(svg.contains("mix-blend-mode:multiply"), "{svg}");
        assert!(svg.contains("isolation:isolate"), "{svg}");
    }

    #[test]
    fn luminosity_soft_mask_emits_mask_def() {
        let mut mask_dl = DisplayList::new(Rect::new(0.0, 0.0, 100.0, 200.0));
        mask_dl.push(RenderCommand::FillPath {
            path: rect_path(0.0, 0.0, 100.0, 100.0),
            rule: FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        let mut dl = page();
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: BlendMode::Normal,
            isolated: false,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 100.0, 200.0),
            alpha: 1.0,
            mask: Some(SoftMask {
                kind: SoftMaskKind::Luminosity,
                commands: std::sync::Arc::new(mask_dl),
                offset: (5.0, 7.0),
                backdrop_luma: 0.25,
                transfer: None,
            }),
        });
        dl.push(RenderCommand::PopBlendGroup);
        let svg = to_svg(&dl);
        assert!(
            svg.contains("<mask id=\"m1\" maskUnits=\"userSpaceOnUse\">"),
            "{svg}"
        );
        assert!(svg.contains("mask=\"url(#m1)\""), "{svg}");
        // Backdrop luma 0.25 → gray(0.25) = #3f3f3f (truncating cast).
        assert!(svg.contains("fill=\"#3f3f3f\""), "{svg}");
        // Page-space offset (5,7) → svg translate(5 -7).
        assert!(svg.contains("translate(5 -7)"), "{svg}");
        assert!(!svg.contains("mask-type:alpha"), "{svg}");
    }

    #[test]
    fn image_matrix_and_data_uri() {
        let mut dl = page();
        let (fonts, mut images) = empty_caches();
        let id = images.insert(DecodedImage {
            width: 2,
            height: 2,
            data: vec![255; 16],
            has_alpha: false,
            premultiplied: false,
        });
        // CTM: unit square scaled 40×60, placed at (10, 20).
        dl.push(RenderCommand::DrawImage(ImageDraw {
            image_id: id,
            transform: Matrix {
                a: 40.0,
                b: 0.0,
                c: 0.0,
                d: 60.0,
                e: 10.0,
                f: 20.0,
            },
            alpha: 1.0,
        }));
        let svg = display_list_to_svg(&dl, &fonts, &images, &SvgOptions::default());
        // svg_x = (40/2)ix + 10, svg_y = (60/2)iy + (200-60-20) = 30iy + 120.
        assert!(svg.contains("matrix(20 0 0 30 10 120)"), "{svg}");
        assert!(svg.contains("href=\"data:image/png;base64,"), "{svg}");
        assert!(svg.contains("preserveAspectRatio=\"none\""), "{svg}");
    }

    #[test]
    fn missing_font_and_image_are_skipped_without_panic() {
        let mut dl = page();
        dl.push(RenderCommand::DrawGlyphRun(GlyphRun {
            font_id: 42,
            font_size: 12.0,
            glyphs: vec![PositionedGlyph {
                glyph_id: 1,
                x: 0.0,
                y: 0.0,
                advance: 6.0,
            }],
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
            transform: Matrix::identity(),
            h_scale: 1.0,
        }));
        dl.push(RenderCommand::DrawImage(ImageDraw {
            image_id: 7,
            transform: Matrix::identity(),
            alpha: 1.0,
        }));
        let svg = to_svg(&dl);
        assert!(!svg.contains("<image"), "{svg}");
        assert!(svg.ends_with("</svg>\n"), "{svg}");
    }

    #[test]
    fn background_is_optional() {
        let dl = page();
        let (fonts, images) = empty_caches();
        let svg = display_list_to_svg(
            &dl,
            &fonts,
            &images,
            &SvgOptions {
                background: None,
                ..SvgOptions::default()
            },
        );
        assert!(!svg.contains("<rect"), "{svg}");
        let with_bg = display_list_to_svg(&dl, &fonts, &images, &SvgOptions::default());
        assert!(with_bg.contains("<rect width=\"100\" height=\"200\" fill=\"#ffffff\"/>"));
    }

    #[test]
    fn spent_budget_truncates_but_stays_wellformed() {
        let mut dl = page();
        for _ in 0..1000 {
            dl.push(RenderCommand::FillPath {
                path: rect_path(0.0, 0.0, 10.0, 10.0),
                rule: FillRule::NonZero,
                paint: Paint::Solid(Color::black()),
                alpha: 1.0,
                overprint: None,
            });
        }
        let (fonts, images) = empty_caches();
        let svg = display_list_to_svg(
            &dl,
            &fonts,
            &images,
            &SvgOptions {
                budget: Some(std::time::Duration::ZERO),
                ..SvgOptions::default()
            },
        );
        // The clock is sampled every 64 commands, so at most a handful of
        // paints land before truncation — and the document still closes.
        assert!(svg.matches("<path").count() < 100, "{svg}");
        assert!(svg.ends_with("</svg>\n"), "{svg}");
    }

    #[test]
    fn base64_is_standard() {
        let mut s = String::new();
        push_base64(&mut s, b"Man");
        assert_eq!(s, "TWFu");
        s.clear();
        push_base64(&mut s, b"Ma");
        assert_eq!(s, "TWE=");
        s.clear();
        push_base64(&mut s, b"M");
        assert_eq!(s, "TQ==");
    }

    #[test]
    fn unpremultiply_inverts_multiply() {
        // 50% alpha premultiplied mid-gray: (64, 64, 64, 128) → ~(128, 128, 128).
        let mut px = vec![64u8, 64, 64, 128, 0, 0, 0, 0];
        unpremultiply(&mut px);
        assert!((px[0] as i32 - 128).abs() <= 1, "{px:?}");
        assert_eq!(&px[4..], &[0, 0, 0, 0]);
    }

    #[test]
    fn numbers_are_trimmed() {
        let mut s = String::new();
        push_num(&mut s, 1.0);
        s.push(' ');
        push_num(&mut s, 1.25);
        s.push(' ');
        push_num(&mut s, 1.2344);
        s.push(' ');
        push_num(&mut s, -0.5);
        assert_eq!(s, "1 1.25 1.234 -0.5");
    }
}
