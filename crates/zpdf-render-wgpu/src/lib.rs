//! wgpu GPU rendering backend for zpdf.
//!
//! Implements the same [`RenderBackend`] trait as the tiny-skia CPU renderer.
//! The CPU renderer is the correctness oracle: this backend reproduces its pixels
//! closely enough to pass `zpdf compare` at <1% differing pixels.
//!
//! Status: M1/M2 — headless context, per-page render target, and pixel readback
//! are implemented; `execute` command arms are no-ops until M4 (fills/strokes).

mod batch;
mod blend;
mod context;
mod glyph;
mod glyph_atlas;
mod image;
mod path;
mod pipelines;
mod record;
mod target;
mod timing;
mod transform;

pub use context::{AdapterIdentity, GpuContext, COLOR_FORMAT, STENCIL_FORMAT};

use record::{PageOp, PageRecorder};
use target::PageTarget;
use transform::{quantize_premul, PageMap, PageUniform};
use zpdf_core::ParseLimits;
use zpdf_display_list::{Color, DisplayList, Paint, RenderCommand};
use zpdf_render::{PageRenderInfo, RenderBackend};

/// RGBA8 pixel buffer read back from the GPU. Mirrors `RenderedPage` from the CPU
/// backend (tight, top-left origin, `len == width*height*4`) so the CLI can save
/// either backend's output through a single code path.
pub struct GpuTexture {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum WgpuRenderError {
    #[error("wgpu device not initialized")]
    NotInitialized,
    #[error("no active page (begin_page not called)")]
    NoActivePage,
    #[error("no compatible GPU adapter found")]
    NoAdapter,
    #[error("required GPU capability unavailable: {0}")]
    Unsupported(String),
    #[error("invalid page render info: {0}")]
    InvalidPage(#[from] zpdf_render::PageGeometryError),
    #[error("buffer readback failed: {0}")]
    Readback(String),
    #[error("device poll failed: {0}")]
    Poll(String),
    #[error("wgpu error: {0}")]
    Wgpu(String),
}

/// Per-page state, alive between `begin_page` and `end_page`.
struct PageState {
    width: u32,
    height: u32,
    clear: wgpu::Color,
    /// Page-units -> device-pixel mapping for tessellation.
    map: PageMap,
    /// Uniform for the pixel->NDC shader step.
    uniform: PageUniform,
    /// Accumulated geometry + ordered op list (fills, strokes, clips).
    recorder: PageRecorder,
}

fn estimate_readback_bytes(width: u32, height: u32) -> u64 {
    let row = width as u64 * 4;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as u64;
    let padded_row = row.div_ceil(align) * align;
    padded_row.saturating_mul(height as u64)
}

fn estimate_page_target_bytes(width: u32, height: u32, sample_count: u32) -> u64 {
    let pixels = width as u64 * height as u64;
    let color = pixels.saturating_mul(4).saturating_mul(sample_count as u64);
    let resolve = if sample_count > 1 {
        pixels.saturating_mul(4)
    } else {
        0
    };
    // Use four bytes/sample as a conservative allocation estimate for Stencil8.
    let stencil = pixels.saturating_mul(4).saturating_mul(sample_count as u64);
    let readback = estimate_readback_bytes(width, height);
    color
        .saturating_add(resolve)
        .saturating_add(stencil)
        .saturating_add(readback)
}

/// Unique image textures referenced by a page, including images nested in any
/// soft-mask op list or rasterized dense tiling cell. Preserve first-use order
/// for deterministic upload traces.
fn referenced_image_ids(ops: &[PageOp], tiles: &[record::SyntheticTile]) -> Vec<record::ImageKey> {
    let mut ids = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![ops];
    for tile in tiles {
        let ops = tile.ops();
        if !ops.is_empty() {
            stack.push(ops);
        }
    }
    while let Some(ops) = stack.pop() {
        for op in ops {
            match op {
                PageOp::Image { image_id, .. } if seen.insert(*image_id) => ids.push(*image_id),
                PageOp::PushBlend {
                    mask: Some(mask), ..
                } => stack.push(mask.ops.as_slice()),
                _ => {}
            }
        }
    }
    ids
}

fn unit(v: f32) -> f32 {
    if v.is_finite() {
        v.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// Premultiply and round a straight background colour for `wgpu::LoadOp::Clear`.
///
/// Matches CPU `Pixmap::fill` (tiny-skia): clamp RGB and alpha to `[0.0, 1.0]`,
/// multiply each clamped RGB channel by the clamped alpha in floating point,
/// then round once to an 8-bit UNORM value. Opaque clears (`a == 1.0`) match
/// opaque solid GPU fills of the same colour.
fn quantize_clear_color(bg: &Color) -> wgpu::Color {
    let a = bg.a.clamp(0.0, 1.0);
    let q = |v: f32| ((v * 255.0).round() as u8) as f64 / 255.0;
    wgpu::Color {
        r: q(bg.r.clamp(0.0, 1.0) * a),
        g: q(bg.g.clamp(0.0, 1.0) * a),
        b: q(bg.b.clamp(0.0, 1.0) * a),
        a: q(a),
    }
}

/// Result of a submission-only render — see
/// [`WgpuRenderer::render_display_list_submitted`].
///
/// Carries no pixels, by construction: not paying for the readback is the whole
/// point of that path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Submission {
    /// Host wall time covering record + submit, plus the timing readback when
    /// [`WgpuRenderer::with_gpu_timing`] is enabled.
    pub wall_ns: u64,
    /// GPU pass time, when timestamp queries are enabled *and* the adapter
    /// supports them; `None` otherwise.
    ///
    /// Note the tension: the timer's own readback is a blocking poll, so turning
    /// timing on to populate this field reintroduces exactly the synchronization
    /// the submit-only path exists to avoid. For the honest viewer number, leave
    /// timing off and use `wall_ns` alone.
    pub gpu_pass_ns: Option<u64>,
}

/// GPU renderer. Borrows font/image caches like `CpuRenderer<'a>`.
pub struct WgpuRenderer<'a> {
    ctx: Option<GpuContext>,
    #[allow(dead_code)] // consumed in M6/M7 (glyphs/images)
    font_cache: Option<&'a zpdf_font::FontCache>,
    #[allow(dead_code)] // consumed in M7 (images)
    image_cache: Option<&'a zpdf_image::ImageCache>,
    max_page_pixels: u64,
    max_gpu_texture_bytes: u64,
    /// Content policy for nested Type3 streams and the page geometry budget.
    parse_limits: ParseLimits,
    page: Option<PageState>,
    /// GPU pass time (nanoseconds) from the most recently completed
    /// `end_page()`, or `None` if timestamp queries are unsupported on this
    /// adapter (see `GpuContext::timestamps_supported`) or no page has
    /// rendered yet. Purely informational (P3.8) — never affects rendering.
    last_gpu_time_ns: Option<u64>,
    /// Timestamp readback adds buffers, a map, and a second blocking poll. Keep
    /// it opt-in so normal rendering pays no telemetry cost.
    gpu_timing_enabled: bool,
    /// Shared ceiling on GPU render passes encoded per page across nested
    /// transparency groups, soft masks, and synthetic tiles.
    max_encode_passes: usize,
    #[cfg(test)]
    last_encode_passes: usize,
}

/// Default ceiling on GPU render passes encoded for a single page (ample for
/// complex dense tiling plans and deep blend stacks while bounding retained
/// per-pass bind groups and uniform buffers before submission).
const DEFAULT_MAX_ENCODE_PASSES: usize = 131_072;

/// Shared encoding budget threaded through `end_page`, `composite_into`, and
/// soft-mask / synthetic-tile passes. Caps total encoded render passes and
/// replayed ops so a small recorded page cannot amplify into unbounded GPU
/// passes, bind groups, or uniform buffers before `queue.submit`.
#[derive(Debug, Clone, Copy)]
struct EncodeBudget {
    remaining_passes: usize,
    remaining_ops: usize,
    passes_used: usize,
}

impl EncodeBudget {
    fn new(max_passes: usize, max_ops: usize) -> Self {
        Self {
            remaining_passes: max_passes,
            remaining_ops: max_ops,
            passes_used: 0,
        }
    }

    fn charge_pass(&mut self, ops: usize) -> Result<(), WgpuRenderError> {
        let op_cost = ops.max(1);
        if self.remaining_passes == 0 || self.remaining_ops < op_cost {
            return Err(WgpuRenderError::Unsupported(
                "GPU render pass encoding budget exceeded".into(),
            ));
        }
        self.remaining_passes -= 1;
        self.remaining_ops -= op_cost;
        self.passes_used = self.passes_used.saturating_add(1);
        Ok(())
    }
}

impl<'a> WgpuRenderer<'a> {
    pub fn new() -> Self {
        let limits = ParseLimits::default();
        Self {
            ctx: None,
            font_cache: None,
            image_cache: None,
            max_page_pixels: limits.max_page_pixels,
            max_gpu_texture_bytes: limits.max_gpu_texture_bytes,
            parse_limits: limits,
            page: None,
            last_gpu_time_ns: None,
            gpu_timing_enabled: false,
            max_encode_passes: DEFAULT_MAX_ENCODE_PASSES,
            #[cfg(test)]
            last_encode_passes: 0,
        }
    }

    pub fn with_fonts(mut self, cache: &'a zpdf_font::FontCache) -> Self {
        self.font_cache = Some(cache);
        self
    }

    pub fn with_images(mut self, cache: &'a zpdf_image::ImageCache) -> Self {
        self.image_cache = Some(cache);
        self
    }

    /// Apply render-time security budgets from the source document. Values are
    /// copied, so this does not extend the renderer's borrow lifetime.
    /// `max_page_operators` bounds cumulative Type3 replay work in this render
    /// stage, with a minimum charge of one per child dispatch.
    pub fn with_limits(mut self, limits: &ParseLimits) -> Self {
        self.max_page_pixels = limits.max_page_pixels;
        self.max_gpu_texture_bytes = limits.max_gpu_texture_bytes;
        self.parse_limits = limits.clone();
        self
    }

    /// Reuse an existing context (e.g. the viewer's surface-bound device).
    pub fn with_context(mut self, ctx: GpuContext) -> Self {
        self.ctx = Some(ctx);
        self
    }

    /// Reclaim the context for reuse across renders.
    pub fn take_context(&mut self) -> Option<GpuContext> {
        if self.page.is_some() {
            return None;
        }
        self.ctx.take()
    }

    /// GPU pass time (nanoseconds) for the most recently completed page, if
    /// timestamp queries are supported on this adapter (P3.8). `None` before
    /// any page has rendered, or when the adapter lacks the capability.
    pub fn last_gpu_time_ns(&self) -> Option<u64> {
        self.last_gpu_time_ns
    }

    /// Enable per-page GPU timestamp telemetry. Disabled by default because its
    /// readback introduces an additional synchronization point.
    ///
    /// Consequence for measurement: a run with this **on** is a slightly slower
    /// path than a consumer runs. Report the timing-off run as the headline
    /// number and this one as a labelled diagnostic.
    pub fn with_gpu_timing(mut self, enabled: bool) -> Self {
        self.gpu_timing_enabled = enabled;
        self
    }

    /// Which adapter this renderer uses, initializing the GPU context if it does
    /// not exist yet.
    ///
    /// Record this with any GPU timing. `request_adapter` picks an adapter
    /// silently, and a machine can expose several (including virtual ones), so a
    /// number without its adapter cannot be compared against anything. See
    /// [`AdapterIdentity`](crate::context::AdapterIdentity).
    pub fn adapter_identity(&mut self) -> Result<&AdapterIdentity, WgpuRenderError> {
        if self.ctx.is_none() {
            self.ctx = Some(GpuContext::new_headless()?);
        }
        Ok(self
            .ctx
            .as_ref()
            .expect("context was just created")
            .identity())
    }

    /// Render a page and submit the GPU work **without reading the result back**
    /// — the viewer / GPU-pipeline consumption mode.
    ///
    /// Why this exists: [`RenderBackend::render_display_list`] ends in a readback
    /// (a full-raster copy plus a blocking `device.poll`), and on a fast GPU that
    /// is by far the largest part of the wall time — the measured GPU pass is
    /// ~0.04 ms against a ~30 ms page wall on the development machine. A caller
    /// that presents the texture, or feeds it to a further GPU pass, never pays
    /// it, so quoting the readback path's wall time as "GPU render cost"
    /// overstates the viewer case by orders of magnitude. This path is the honest
    /// number for that mode.
    ///
    /// Rendering itself is identical to the readback path (same recording, same
    /// submission, same GPU work); only the copy into the readback buffer and the
    /// map are skipped. The pixels are dropped when the page ends.
    ///
    /// Measured with [`Self::with_gpu_timing`] **off** for the honest number:
    /// the timer's readback is itself a blocking poll, so enabling it here would
    /// put back the synchronization this path removes.
    pub fn render_display_list_submitted(
        &mut self,
        dl: &DisplayList,
        scale: f32,
    ) -> Result<Submission, WgpuRenderError> {
        let started = zpdf_core::time::Instant::now();
        self.begin_page(&PageRenderInfo {
            page_rect: dl.page_rect,
            scale,
            background: Color::white(),
        })?;
        for cmd in &dl.commands {
            self.execute(cmd)?;
        }
        self.finish_page(false)?;
        Ok(Submission {
            wall_ns: started.elapsed().as_nanos() as u64,
            gpu_pass_ns: self.last_gpu_time_ns,
        })
    }
}

impl<'a> Default for WgpuRenderer<'a> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> RenderBackend for WgpuRenderer<'a> {
    type Target = GpuTexture;
    type Error = WgpuRenderError;

    fn begin_page(&mut self, info: &PageRenderInfo) -> Result<(), Self::Error> {
        // A failed replacement must not leave an older page available to end.
        self.page = None;
        self.last_gpu_time_ns = None;
        let (width, height) = info.raster_dimensions()?;
        let pixels = width as u64 * height as u64;
        if pixels > self.max_page_pixels {
            return Err(WgpuRenderError::Unsupported(format!(
                "page {width}x{height} has {pixels} pixels (max_page_pixels={})",
                self.max_page_pixels
            )));
        }
        let readback_bytes = estimate_readback_bytes(width, height);
        if readback_bytes > self.max_gpu_texture_bytes {
            return Err(WgpuRenderError::Unsupported(format!(
                "page {width}x{height} readback alone needs {readback_bytes} bytes (max_gpu_texture_bytes={})",
                self.max_gpu_texture_bytes
            )));
        }
        if self.ctx.is_none() {
            self.ctx = Some(GpuContext::new_headless()?);
        }
        let ctx = self.ctx.as_ref().unwrap();
        ctx.clear_error();

        let scale = info.scale;
        if width > ctx.max_texture_dim || height > ctx.max_texture_dim {
            return Err(WgpuRenderError::Unsupported(format!(
                "page {width}x{height} exceeds adapter max texture dim {}",
                ctx.max_texture_dim
            )));
        }

        let estimated = estimate_page_target_bytes(width, height, ctx.sample_count);
        if estimated > self.max_gpu_texture_bytes {
            return Err(WgpuRenderError::Unsupported(format!(
                "page {width}x{height} needs about {} MiB of GPU target/readback memory (limit {} MiB)",
                estimated.div_ceil(1024 * 1024),
                self.max_gpu_texture_bytes / (1024 * 1024)
            )));
        }

        // Premultiply and round the background channels so both opaque and
        // translucent clears match CPU `Pixmap::fill` and opaque solid fills.
        let clear = quantize_clear_color(&info.background);

        let map = PageMap::new(info.page_rect, scale);
        let uniform = PageUniform {
            w_px: width as f32,
            h_px: height as f32,
            scale,
            page_height: info.page_rect.height() as f32,
        };

        self.page = Some(PageState {
            width,
            height,
            clear,
            map,
            uniform,
            recorder: PageRecorder::default()
                .with_limits(&self.parse_limits)
                .with_page_rect(info.page_rect)
                .with_opaque_canvas(clear.a >= 1.0),
        });
        Ok(())
    }

    fn execute(&mut self, cmd: &RenderCommand) -> Result<(), Self::Error> {
        let fonts = self.font_cache;
        let images = self.image_cache;
        let Some(page) = self.page.as_mut() else {
            return Err(WgpuRenderError::NoActivePage);
        };
        if let Err(error) = page.recorder.ensure_within_budget() {
            self.page = None;
            return Err(error);
        }
        match cmd {
            RenderCommand::FillPath {
                path,
                rule,
                paint: Paint::Solid(c),
                alpha,
                overprint,
            } => {
                let color = quantize_premul(c, *alpha);
                match overprint {
                    // Overprint composites against the backdrop → offscreen layer.
                    Some(op) if op.active != 0 => {
                        page.recorder.push_overprint(*op);
                        page.recorder.add_fill(path, *rule, color, &page.map);
                        page.recorder.pop_blend();
                    }
                    // active == 0 paints no colorants → nothing to draw.
                    Some(_) => {}
                    None => page.recorder.add_fill(path, *rule, color, &page.map),
                }
            }
            RenderCommand::FillPath {
                path,
                rule,
                paint: Paint::Tiling(tiling),
                alpha,
                overprint,
            } => {
                let id = zpdf_core::Matrix::identity();
                record_tiling_fill(
                    &mut page.recorder,
                    path,
                    *rule,
                    tiling,
                    &id,
                    *alpha,
                    *overprint,
                    fonts,
                    images,
                    &page.map,
                    0,
                );
            }
            RenderCommand::StrokePath {
                path,
                style,
                paint: Paint::Solid(c),
                alpha,
                overprint,
            } => {
                let color = quantize_premul(c, *alpha);
                match overprint {
                    Some(op) if op.active != 0 => {
                        page.recorder.push_overprint(*op);
                        page.recorder.add_stroke(path, style, color, &page.map);
                        page.recorder.pop_blend();
                    }
                    Some(_) => {}
                    None => page.recorder.add_stroke(path, style, color, &page.map),
                }
            }
            RenderCommand::StrokePath {
                path,
                style,
                paint: Paint::Tiling(tiling),
                alpha,
                overprint,
            } => {
                let id = zpdf_core::Matrix::identity();
                record_tiling_stroke(
                    &mut page.recorder,
                    path,
                    style,
                    tiling,
                    &id,
                    *alpha,
                    *overprint,
                    fonts,
                    images,
                    &page.map,
                    0,
                );
            }
            RenderCommand::PushClip { path, rule } => {
                page.recorder.push_clip(path, *rule, &page.map);
            }
            RenderCommand::PushClipStroke { path, style } => {
                page.recorder.push_clip_stroke(path, style, &page.map);
            }
            RenderCommand::PopClip => {
                page.recorder.pop_clip();
            }
            RenderCommand::DrawGlyphRun(run) => {
                if let Some(fonts) = self.font_cache {
                    if let Paint::Tiling(tiling) = &run.paint {
                        let id = zpdf_core::Matrix::identity();
                        record_tiling_glyph_run(
                            &mut page.recorder,
                            run,
                            tiling,
                            &id,
                            fonts,
                            self.image_cache,
                            &page.map,
                            0,
                        );
                    } else {
                        match run.overprint {
                            Some(op) if op.active != 0 => {
                                page.recorder.push_overprint(op);
                                glyph::render_glyph_run(&mut page.recorder, fonts, &page.map, run);
                                page.recorder.pop_blend();
                            }
                            Some(_) => {}
                            None => {
                                glyph::render_glyph_run(&mut page.recorder, fonts, &page.map, run)
                            }
                        }
                    }
                }
            }
            RenderCommand::DrawImage(draw) => {
                if let Some(images) = self.image_cache {
                    if let Some(img) = images.get(draw.image_id) {
                        if !image::is_valid_decoded_image(img) {
                            tracing::warn!(
                                image_id = draw.image_id,
                                width = img.width,
                                height = img.height,
                                bytes = img.data.len(),
                                "skipping malformed decoded image"
                            );
                            return Ok(());
                        }
                        let quad = image::image_quad(
                            img.width as f32,
                            img.height as f32,
                            &draw.transform,
                            &page.map,
                            draw.alpha,
                        );
                        if image::is_safe_quad(&quad) {
                            page.recorder.add_image(quad, draw.image_id);
                        } else {
                            tracing::warn!(
                                image_id = draw.image_id,
                                "skipping out-of-range image transform"
                            );
                        }
                    }
                }
            }
            RenderCommand::DrawInlineImage(draw) => {
                let img = &draw.image;
                if !image::is_valid_rgba_image(img.width, img.height, img.data.len()) {
                    tracing::warn!(
                        width = img.width,
                        height = img.height,
                        bytes = img.data.len(),
                        "skipping malformed inline image"
                    );
                    return Ok(());
                }
                let quad = image::image_quad(
                    img.width as f32,
                    img.height as f32,
                    &draw.transform,
                    &page.map,
                    draw.alpha,
                );
                if image::is_safe_quad(&quad) {
                    page.recorder
                        .add_inline_image(quad, std::sync::Arc::clone(img));
                } else {
                    tracing::warn!("skipping out-of-range inline image transform");
                }
            }
            RenderCommand::PushBlendGroup {
                blend_mode,
                isolated,
                knockout,
                alpha,
                mask,
                ..
            } => {
                // Record the soft mask's geometry into the shared arena (its ops
                // are replayed into an offscreen coverage layer at composite).
                let map = page.map;
                let id = zpdf_core::Matrix::identity();
                let mask_ops = if page.recorder.can_push_blend() {
                    mask.as_ref().map(|m| {
                        build_mask_ops(
                            &mut page.recorder,
                            m,
                            &id,
                            fonts,
                            images.map(RecordImages::Page),
                            &map,
                            0,
                        )
                    })
                } else {
                    None
                };
                if !*isolated && !*knockout {
                    if *blend_mode == zpdf_display_list::BlendMode::Normal
                        && *alpha >= 1.0
                        && mask_ops.is_none()
                    {
                        page.recorder.push_passthrough_blend();
                    } else {
                        page.recorder
                            .push_non_isolated_with_mode(*blend_mode, *alpha, mask_ops);
                    }
                } else {
                    page.recorder.push_blend(*blend_mode, *alpha, mask_ops);
                }
            }
            RenderCommand::PopBlendGroup => {
                page.recorder.pop_blend();
            }
            _ => {}
        }
        let result = page.recorder.ensure_within_budget();
        if result.is_err() {
            // No partial page may be finalized after a failed child or append.
            self.page = None;
        }
        result
    }

    fn end_page(&mut self) -> Result<Self::Target, Self::Error> {
        self.finish_page(true)?
            .ok_or_else(|| WgpuRenderError::Wgpu("page render produced no pixels".into()))
    }

    /// The GPU backend deliberately reports **no** [`StageStats`].
    ///
    /// Those buckets describe host-side rasterization — glyph outline extraction,
    /// path fills, glyph raster, image resampling, clip masks — and the GPU does
    /// none of it on the host. Filling them with zeros would read as "this work
    /// is free on the GPU", which is a different and misleading claim. GPU
    /// numbers come from [`WgpuRenderer::last_gpu_time_ns`] for the pass, from
    /// the caller for wall time, and from [`WgpuRenderer::adapter_identity`] for
    /// the configuration they were measured on.
    ///
    /// [`StageStats`]: zpdf_render::StageStats
    fn stage_stats(&self) -> Option<zpdf_render::StageStats> {
        None
    }
}

impl<'a> WgpuRenderer<'a> {
    /// Shared page finalization for both consumption modes.
    ///
    /// `readback == false` submits the page and leaves the pixels on the GPU —
    /// see [`Self::render_display_list_submitted`]. Only the readback copy and
    /// the blocking map are skipped; recording, submission, and the GPU work are
    /// identical, so a submit-only page renders exactly what a readback page
    /// renders.
    fn finish_page(&mut self, readback: bool) -> Result<Option<GpuTexture>, WgpuRenderError> {
        use wgpu::util::DeviceExt;

        let ctx = self.ctx.as_ref().ok_or(WgpuRenderError::NotInitialized)?;
        let mut page = self.page.take().ok_or(WgpuRenderError::NoActivePage)?;
        let device = &ctx.device;
        page.recorder.ensure_within_budget()?;
        page.recorder.close_open_blends();

        // Fullscreen quad: needed for clip rebuild (ResetStencil) and blend composites.
        let needs_fs = page.recorder.uses_reset() || page.recorder.has_blend_groups();
        // Presence of ResetStencil/PushBlend/PopBlend is unaffected by batching
        // (it only merges runs of Draw/Image/StampClip; it never removes or adds
        // an op of a different kind), so this check is valid whether taken
        // before or after `batch::batch_ops` below.
        let has_blend_groups = page.recorder.has_blend_groups();
        let fs_range = if needs_fs {
            page.recorder
                .append_fullscreen(page.uniform.w_px, page.uniform.h_px)
        } else {
            None
        };
        page.recorder.ensure_within_budget()?;

        // Prepare offscreen dense tiling cells (append fullscreen reset quads
        // before creating the shared vertex/index buffers, and batch each tile).
        let mut synthetic_tiles = std::mem::take(&mut page.recorder.synthetic_tiles);
        let mut tile_fs_ranges = Vec::with_capacity(synthetic_tiles.len());
        for tile in &mut synthetic_tiles {
            tile.ops = batch::batch_ops(std::mem::take(&mut tile.ops));
            let tile_fs = if tile.needs_fullscreen() {
                page.recorder
                    .append_fullscreen(tile.width as f32, tile.height as f32)
            } else {
                None
            };
            tile_fs_ranges.push(tile_fs);
        }
        page.recorder.ensure_within_budget()?;

        // Collapse consecutive same-state ops into single draw_indexed calls
        // (P3.7). Must happen before `rec` borrows `page.recorder` below (this
        // takes `page.recorder.ops` by value, leaving an empty Vec in its place
        // — irrelevant, since every remaining use goes through `batched_ops`).
        let batched_ops = batch::batch_ops(std::mem::take(&mut page.recorder.ops));

        // Validate the complete per-page GPU texture/readback working set before
        // allocating any large resource. Single-pass pages own page attachments;
        // layered pages own only readback here and receive the remainder as their
        // dynamically enforced transparency-layer pool budget.
        let rec = &page.recorder;
        let referenced_images = referenced_image_ids(&batched_ops, &synthetic_tiles);
        let lookup_image = |image_id: u32| -> Option<&zpdf_image::DecodedImage> {
            rec.get_type3_image(image_id)
                .or_else(|| self.image_cache.and_then(|images| images.get(image_id)))
        };
        let mut uploadable_images = Vec::new();
        let mut image_texture_bytes = 0u64;
        for image_key in referenced_images {
            let dims = match image_key {
                record::ImageKey::Cache(image_id) => lookup_image(image_id)
                    .filter(|img| image::is_valid_decoded_image(img))
                    .map(|img| (img.width, img.height)),
                record::ImageKey::Inline(idx) => rec
                    .inline_images
                    .get(idx as usize)
                    .filter(|img| image::is_valid_rgba_image(img.width, img.height, img.data.len()))
                    .map(|img| (img.width, img.height)),
            };
            let Some((width, height)) = dims else {
                continue;
            };
            if width > ctx.max_texture_dim || height > ctx.max_texture_dim {
                tracing::warn!("image {image_key:?} {width}x{height} skipped (exceeds limits)");
                continue;
            }
            image_texture_bytes =
                image_texture_bytes.saturating_add(width as u64 * height as u64 * 4);
            uploadable_images.push(image_key);
        }
        let mut tile_texture_bytes = 0u64;
        for tile in &synthetic_tiles {
            if tile.width > ctx.max_texture_dim || tile.height > ctx.max_texture_dim {
                return Err(WgpuRenderError::Unsupported(format!(
                    "tiling pattern cell {}x{} exceeds adapter max texture dim {}",
                    tile.width, tile.height, ctx.max_texture_dim
                )));
            }
            tile_texture_bytes = tile_texture_bytes.saturating_add(blend::estimated_layer_bytes(
                tile.width,
                tile.height,
                ctx.sample_count,
            ));
        }
        let glyph_texture_bytes = rec.glyph_atlas.gpu_texture_bytes();
        let target_bytes = if has_blend_groups {
            estimate_readback_bytes(page.width, page.height)
        } else {
            estimate_page_target_bytes(page.width, page.height, ctx.sample_count)
        };
        let fixed_gpu_bytes = target_bytes
            .saturating_add(image_texture_bytes)
            .saturating_add(tile_texture_bytes)
            .saturating_add(glyph_texture_bytes);
        if fixed_gpu_bytes > self.max_gpu_texture_bytes {
            return Err(WgpuRenderError::Unsupported(format!(
                "page GPU target/readback ({target_bytes} bytes), images ({image_texture_bytes} bytes), tiling cells ({tile_texture_bytes} bytes), and glyph atlas ({glyph_texture_bytes} bytes) exceed max_gpu_texture_bytes={}",
                self.max_gpu_texture_bytes
            )));
        }
        let layer_budget = self.max_gpu_texture_bytes - fixed_gpu_bytes;
        if has_blend_groups
            && blend::estimated_layer_bytes(page.width, page.height, ctx.sample_count)
                > layer_budget
        {
            return Err(WgpuRenderError::Unsupported(format!(
                "page leaves {layer_budget} bytes for transparency layers, less than one layer requires"
            )));
        }

        // Layered rendering supplies its own color/stencil textures and needs
        // only this target's readback buffer. Allocate attachments lazily after
        // the complete budget validation above.
        let target = PageTarget::new(
            device,
            page.width,
            page.height,
            ctx.sample_count,
            !has_blend_groups,
        );

        // Page uniform + bind group (pixel->NDC params).
        let uniform_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("zpdf-page-uniform"),
            contents: bytemuck::bytes_of(&page.uniform),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("zpdf-page-bg"),
            layout: &ctx.pipelines.page_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buf.as_entire_binding(),
            }],
        });

        // One shared arena buffer pair (empty for a blank page).
        let buffers = if rec.indices.is_empty() {
            None
        } else {
            let vbuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("zpdf-vbuf"),
                contents: bytemuck::cast_slice(&rec.vertices),
                usage: wgpu::BufferUsages::VERTEX,
            });
            let ibuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("zpdf-ibuf"),
                contents: bytemuck::cast_slice(&rec.indices),
                usage: wgpu::BufferUsages::INDEX,
            });
            Some((vbuf, ibuf))
        };

        // Image quad buffers (separate vertex format) + per-image texture bind groups.
        let tex_buffers = if rec.tex_indices.is_empty() {
            None
        } else {
            let vbuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("zpdf-tex-vbuf"),
                contents: bytemuck::cast_slice(&rec.tex_vertices),
                usage: wgpu::BufferUsages::VERTEX,
            });
            let ibuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("zpdf-tex-ibuf"),
                contents: bytemuck::cast_slice(&rec.tex_indices),
                usage: wgpu::BufferUsages::INDEX,
            });
            Some((vbuf, ibuf))
        };

        // Glyph-atlas quad buffers (own arena) + the page's single atlas
        // texture bind group — uploaded once here, shared by every Glyph op.
        let glyph_buffers = if rec.glyph_indices.is_empty() {
            None
        } else {
            let vbuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("zpdf-glyph-vbuf"),
                contents: bytemuck::cast_slice(&rec.glyph_vertices),
                usage: wgpu::BufferUsages::VERTEX,
            });
            let ibuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("zpdf-glyph-ibuf"),
                contents: bytemuck::cast_slice(&rec.glyph_indices),
                usage: wgpu::BufferUsages::INDEX,
            });
            Some((vbuf, ibuf))
        };
        let glyph_bg = if rec.glyph_atlas.is_empty() {
            None
        } else {
            Some(glyph_atlas::upload_atlas_bind_group(ctx, &rec.glyph_atlas))
        };

        let mut tex_bgs: std::collections::HashMap<record::ImageKey, wgpu::BindGroup> =
            std::collections::HashMap::new();
        for image_key in uploadable_images {
            match image_key {
                record::ImageKey::Cache(image_id) => {
                    if let Some(img) = lookup_image(image_id) {
                        tex_bgs.insert(image_key, image::upload_image_bind_group(ctx, img));
                    }
                }
                record::ImageKey::Inline(idx) => {
                    if let Some(img) = rec.inline_images.get(idx as usize) {
                        let pixels =
                            image::premultiply_rgba_if_needed(&img.data, img.premultiplied);
                        tex_bgs.insert(
                            image_key,
                            image::upload_rgba_bind_group(ctx, img.width, img.height, &pixels),
                        );
                    }
                }
            }
        }

        let timer = timing::GpuTimer::new(ctx, self.gpu_timing_enabled);
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some(if has_blend_groups {
                "zpdf-layered"
            } else {
                "zpdf-page"
            }),
        });
        timer.write_begin(&mut encoder);

        // Render dense tiling-pattern cells once into repeat-sampled textures
        // before the main page pass. Ordered bottom-up so nested dense tiles
        // are already present in `tile_bgs` when outer tiles sample them.
        let mut tile_bgs: std::collections::HashMap<u32, wgpu::BindGroup> =
            std::collections::HashMap::with_capacity(synthetic_tiles.len());
        let mut tile_outputs: Vec<TileOutput> = Vec::with_capacity(synthetic_tiles.len());
        let mut keep_bgs: Vec<wgpu::BindGroup> = Vec::new();
        let mut keep_bufs: Vec<wgpu::Buffer> = Vec::new();
        let mut remaining_layer_budget = layer_budget;
        let max_encode_ops = (self.parse_limits.max_page_operators as usize)
            .saturating_mul(4)
            .max(65_536);
        let mut encode_budget = EncodeBudget::new(self.max_encode_passes, max_encode_ops);
        for (idx, tile) in synthetic_tiles.iter().enumerate() {
            let tile_uniform = PageUniform {
                w_px: tile.width as f32,
                h_px: tile.height as f32,
                scale: 1.0,
                page_height: tile.height as f32,
            };
            let tile_ubuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("zpdf-tile-uniform"),
                contents: bytemuck::bytes_of(&tile_uniform),
                usage: wgpu::BufferUsages::UNIFORM,
            });
            let tile_page_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("zpdf-tile-page-bg"),
                layout: &ctx.pipelines.page_bgl,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: tile_ubuf.as_entire_binding(),
                }],
            });
            let sampler = if tile.nearest {
                &ctx.pipelines.repeat_nearest_sampler
            } else {
                &ctx.pipelines.repeat_sampler
            };
            let output = {
                let tile_res = ReplayRes {
                    pipelines: &ctx.pipelines,
                    bind_group: &tile_page_bg,
                    solid: buffers.as_ref().map(|(v, i)| (v, i)),
                    tex: tex_buffers.as_ref().map(|(v, i)| (v, i)),
                    tex_bgs: &tex_bgs,
                    tile_bgs: &tile_bgs,
                    tile_outputs: &tile_outputs,
                    glyph: glyph_buffers.as_ref().map(|(v, i)| (v, i)),
                    glyph_bg: glyph_bg.as_ref(),
                    fs_range: tile_fs_ranges[idx],
                };
                if tile.has_blend_groups() {
                    let tile_layer_bytes =
                        blend::estimated_layer_bytes(tile.width, tile.height, ctx.sample_count);
                    let mut pool = blend::LayerPool::new(
                        tile.width,
                        tile.height,
                        ctx.sample_count,
                        tile_layer_bytes.saturating_add(remaining_layer_budget),
                    );
                    let base = pool.acquire(device)?;
                    encode_budget.charge_pass(1)?;
                    {
                        let pass = begin_layer_pass(
                            &mut encoder,
                            pool.get(base),
                            wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                            wgpu::LoadOp::Clear(0),
                        );
                        drop(pass);
                    }
                    let final_layer = composite_into(
                        ctx,
                        &mut encoder,
                        &mut pool,
                        base,
                        &tile.ops,
                        &tile_res,
                        &mut keep_bgs,
                        &mut keep_bufs,
                        &mut encode_budget,
                    )?;
                    let extra_layers = pool.allocated().saturating_sub(1) as u64;
                    remaining_layer_budget = remaining_layer_budget
                        .saturating_sub(extra_layers.saturating_mul(tile_layer_bytes));
                    TileOutput::Pooled(pool, final_layer)
                } else {
                    encode_budget.charge_pass(tile.ops.len())?;
                    let layer =
                        blend::RenderLayer::new(device, tile.width, tile.height, ctx.sample_count);
                    {
                        let mut pass = begin_layer_pass(
                            &mut encoder,
                            &layer,
                            wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                            wgpu::LoadOp::Clear(0),
                        );
                        replay_ops(&mut pass, &tile.ops, &tile_res);
                    }
                    TileOutput::Single(Box::new(layer))
                }
            };
            let tile_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("zpdf-tile-bg"),
                layout: &ctx.pipelines.tex_bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(output.sampleable_view()),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(sampler),
                    },
                ],
            });
            tile_outputs.push(output);
            keep_bgs.push(tile_page_bg);
            keep_bufs.push(tile_ubuf);
            tile_bgs.insert(idx as u32, tile_bg);
        }

        let res = ReplayRes {
            pipelines: &ctx.pipelines,
            bind_group: &bind_group,
            solid: buffers.as_ref().map(|(v, i)| (v, i)),
            tex: tex_buffers.as_ref().map(|(v, i)| (v, i)),
            tex_bgs: &tex_bgs,
            tile_bgs: &tile_bgs,
            tile_outputs: &tile_outputs,
            glyph: glyph_buffers.as_ref().map(|(v, i)| (v, i)),
            glyph_bg: glyph_bg.as_ref(),
            fs_range,
        };

        // Keep any page-level LayerPool alive until after `queue.submit`.
        let mut _page_pool: Option<blend::LayerPool> = None;
        if has_blend_groups {
            let mut pool = blend::LayerPool::new(
                target.width,
                target.height,
                target.sample_count,
                remaining_layer_budget,
            );
            let base = pool.acquire(device)?;
            encode_budget.charge_pass(1)?;
            {
                let pass = begin_layer_pass(
                    &mut encoder,
                    pool.get(base),
                    wgpu::LoadOp::Clear(page.clear),
                    wgpu::LoadOp::Clear(0),
                );
                drop(pass);
            }
            let final_layer = composite_into(
                ctx,
                &mut encoder,
                &mut pool,
                base,
                &batched_ops,
                &res,
                &mut keep_bgs,
                &mut keep_bufs,
                &mut encode_budget,
            )?;
            timer.write_end(&mut encoder);
            timer.record_resolve(&mut encoder);
            if readback {
                target.record_copy_from(&mut encoder, pool.get(final_layer).sampleable_texture());
            }
            _page_pool = Some(pool);
        } else {
            encode_budget.charge_pass(batched_ops.len())?;
            // Single-pass path (no blend groups): one render pass into the page target.
            {
                let color_att = target.color_attachment(page.clear);
                let ds_att = target.stencil_attachment();
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("zpdf-page-pass"),
                    color_attachments: &[Some(color_att)],
                    depth_stencil_attachment: Some(ds_att),
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                replay_ops(&mut pass, &batched_ops, &res);
            }
            timer.write_end(&mut encoder);
            timer.record_resolve(&mut encoder);
            // The copy into the readback buffer is what the readback maps; skipping
            // it is exactly what makes the submit-only path cheap.
            if readback {
                target.record_copy(&mut encoder);
            }
        }
        #[cfg(test)]
        {
            self.last_encode_passes = encode_budget.passes_used;
        }
        ctx.queue.submit(Some(encoder.finish()));
        drop(tile_outputs);
        drop(keep_bgs);
        drop(keep_bufs);
        let result = if readback {
            target.map_and_strip(device).map(Some)
        } else {
            // No readback means no blocking poll — but the queue must still be
            // given the chance to retire completed submissions, or a tight render
            // loop piles up in-flight work until the device reports OOM (observed:
            // a 4 GB-class GPU OOM'd within a few hundred un-synchronized pages).
            // Non-blocking: it drains whatever has finished and returns.
            let _ = ctx.device.poll(wgpu::PollType::Poll);
            Ok(None)
        };
        self.last_gpu_time_ns = timer.resolve_ns(device);
        if let Some(msg) = ctx.take_error() {
            return Err(WgpuRenderError::Wgpu(format!("device error: {msg}")));
        }
        result
    }
}

/// Rendered synthetic tiling output retained until submission so later tiles
/// and the main page can sample it in O(1) without cloning texture views.
enum TileOutput {
    Single(Box<blend::RenderLayer>),
    Pooled(blend::LayerPool, usize),
}

impl TileOutput {
    fn sampleable_view(&self) -> &wgpu::TextureView {
        match self {
            Self::Single(layer) => layer.sampleable_view(),
            Self::Pooled(pool, idx) => pool.get(*idx).sampleable_view(),
        }
    }
}

/// Shared GPU resources for replaying recorded ops into a render pass.
struct ReplayRes<'a> {
    pipelines: &'a pipelines::Pipelines,
    bind_group: &'a wgpu::BindGroup,
    solid: Option<(&'a wgpu::Buffer, &'a wgpu::Buffer)>,
    tex: Option<(&'a wgpu::Buffer, &'a wgpu::Buffer)>,
    tex_bgs: &'a std::collections::HashMap<record::ImageKey, wgpu::BindGroup>,
    tile_bgs: &'a std::collections::HashMap<u32, wgpu::BindGroup>,
    tile_outputs: &'a [TileOutput],
    /// Glyph-atlas quad buffers, present iff any glyph took the atlas path.
    glyph: Option<(&'a wgpu::Buffer, &'a wgpu::Buffer)>,
    /// The page's single atlas texture bind group (no per-op id, unlike `tex_bgs`).
    glyph_bg: Option<&'a wgpu::BindGroup>,
    fs_range: Option<record::MeshRange>,
}

/// Replay a slice of ops into `pass` (fills, strokes, clips, images). Blend ops are
/// skipped here — the layered driver handles them. Sets bind group 0 (page uniform).
fn replay_ops(pass: &mut wgpu::RenderPass, ops: &[PageOp], res: &ReplayRes) {
    pass.set_bind_group(0, Some(res.bind_group), &[]);
    let mut cur = Pipe::None;
    let mut bound = BufSet::None;
    for op in ops {
        match op {
            PageOp::Image {
                range,
                image_id,
                clip_ref,
            } => {
                let (Some((tvb, tib)), Some(bg1)) = (res.tex, res.tex_bgs.get(image_id)) else {
                    continue; // image skipped (too large) or no textured buffers
                };
                if cur != Pipe::Textured {
                    pass.set_pipeline(&res.pipelines.textured);
                    cur = Pipe::Textured;
                }
                if bound != BufSet::Tex {
                    pass.set_vertex_buffer(0, tvb.slice(..));
                    pass.set_index_buffer(tib.slice(..), wgpu::IndexFormat::Uint32);
                    bound = BufSet::Tex;
                }
                pass.set_bind_group(1, Some(bg1), &[]);
                pass.set_stencil_reference(*clip_ref);
                pass.draw_indexed(
                    range.first_index..range.first_index + range.index_count,
                    range.base_vertex,
                    0..1,
                );
            }
            PageOp::Tile {
                range,
                tile_id,
                clip_ref,
            } => {
                let (Some((tvb, tib)), Some(bg1)) = (res.tex, res.tile_bgs.get(tile_id)) else {
                    continue;
                };
                if cur != Pipe::Textured {
                    pass.set_pipeline(&res.pipelines.textured);
                    cur = Pipe::Textured;
                }
                if bound != BufSet::Tex {
                    pass.set_vertex_buffer(0, tvb.slice(..));
                    pass.set_index_buffer(tib.slice(..), wgpu::IndexFormat::Uint32);
                    bound = BufSet::Tex;
                }
                pass.set_bind_group(1, Some(bg1), &[]);
                pass.set_stencil_reference(*clip_ref);
                pass.draw_indexed(
                    range.first_index..range.first_index + range.index_count,
                    range.base_vertex,
                    0..1,
                );
            }
            PageOp::Glyph { range, clip_ref } => {
                let (Some((gvb, gib)), Some(bg1)) = (res.glyph, res.glyph_bg) else {
                    continue; // no glyph took the atlas path this page
                };
                if cur != Pipe::Glyph {
                    pass.set_pipeline(&res.pipelines.glyph);
                    cur = Pipe::Glyph;
                }
                if bound != BufSet::Glyph {
                    pass.set_vertex_buffer(0, gvb.slice(..));
                    pass.set_index_buffer(gib.slice(..), wgpu::IndexFormat::Uint32);
                    bound = BufSet::Glyph;
                }
                pass.set_bind_group(1, Some(bg1), &[]);
                pass.set_stencil_reference(*clip_ref);
                pass.draw_indexed(
                    range.first_index..range.first_index + range.index_count,
                    range.base_vertex,
                    0..1,
                );
            }
            PageOp::Draw { .. } | PageOp::StampClip { .. } | PageOp::ResetStencil => {
                let Some((svb, sib)) = res.solid else {
                    continue;
                };
                let (want, range, sref) = match op {
                    PageOp::Draw { range, clip_ref } => (Pipe::Solid, *range, *clip_ref),
                    PageOp::StampClip { range, ref_value } => (Pipe::ClipWrite, *range, *ref_value),
                    PageOp::ResetStencil => {
                        (Pipe::ClipReset, res.fs_range.expect("fs quad present"), 0)
                    }
                    _ => unreachable!(),
                };
                if cur != want {
                    pass.set_pipeline(match want {
                        Pipe::Solid => &res.pipelines.solid_fill,
                        Pipe::ClipWrite => &res.pipelines.clip_write,
                        Pipe::ClipReset => &res.pipelines.clip_reset,
                        _ => unreachable!(),
                    });
                    cur = want;
                }
                if bound != BufSet::Solid {
                    pass.set_vertex_buffer(0, svb.slice(..));
                    pass.set_index_buffer(sib.slice(..), wgpu::IndexFormat::Uint32);
                    bound = BufSet::Solid;
                }
                pass.set_stencil_reference(sref);
                pass.draw_indexed(
                    range.first_index..range.first_index + range.index_count,
                    range.base_vertex,
                    0..1,
                );
            }
            PageOp::PushBlend { .. } | PageOp::PopBlend | PageOp::AffineTile { .. } => {}
        }
    }
}

/// Image cache source passed into [`record_dl_commands_at`] / [`build_mask_ops`].
/// Page-level masks and tiling cells resolve `DrawImage` against the page's
/// `ImageCache`, whereas Type 3 glyph masks resolve against the glyph's private
/// `ImageCache` and register textures under [`record::Type3ImageKey`] so they
/// never disappear or collide with page `ImageId`s.
#[derive(Clone, Copy)]
pub(crate) enum RecordImages<'a> {
    Page(&'a zpdf_image::ImageCache),
    Type3 {
        font_id: zpdf_font::FontId,
        glyph_id: u16,
        images: &'a zpdf_image::ImageCache,
    },
}

impl<'a> RecordImages<'a> {
    fn page_cache(self) -> Option<&'a zpdf_image::ImageCache> {
        match self {
            Self::Page(cache) => Some(cache),
            Self::Type3 { .. } => None,
        }
    }
}

/// Record a [`zpdf_display_list::DisplayList`] directly into `rec` under an
/// arbitrary affine `ctm` (identity for top-level soft masks; pattern-to-page
/// or pattern-to-tile for tiling cells). Commands — including shared soft-mask
/// `Arc<DisplayList>`s — are streamed directly into the budgeted recorder
/// without cloning or pre-transforming intermediate display lists, and strokes
/// / stroke clips are tessellated in local space before applying `ctm` so
/// anisotropic pattern matrices preserve directional stroke widths.
#[allow(clippy::too_many_arguments)]
fn record_dl_commands_at(
    rec: &mut PageRecorder,
    dl: &zpdf_display_list::DisplayList,
    ctm: &zpdf_core::Matrix,
    fonts: Option<&zpdf_font::FontCache>,
    images: Option<RecordImages<'_>>,
    map: &PageMap,
    depth: u8,
) {
    use zpdf_display_list::Paint;
    let page_images = images.and_then(RecordImages::page_cache);
    for cmd in &dl.commands {
        if !rec.can_record() {
            return;
        }
        match cmd {
            RenderCommand::FillPath {
                path,
                rule,
                paint: Paint::Solid(c),
                alpha,
                overprint,
            } => {
                let color = quantize_premul(c, *alpha);
                match overprint {
                    Some(op) if op.active != 0 => {
                        rec.push_overprint(*op);
                        rec.add_fill_transformed(path, *rule, color, ctm, map);
                        rec.pop_blend();
                    }
                    Some(_) => {}
                    None => rec.add_fill_transformed(path, *rule, color, ctm, map),
                }
            }
            RenderCommand::StrokePath {
                path,
                style,
                paint: Paint::Solid(c),
                alpha,
                overprint,
            } => {
                let color = quantize_premul(c, *alpha);
                match overprint {
                    Some(op) if op.active != 0 => {
                        rec.push_overprint(*op);
                        rec.add_stroke_transformed(path, style, color, ctm, map);
                        rec.pop_blend();
                    }
                    Some(_) => {}
                    None => rec.add_stroke_transformed(path, style, color, ctm, map),
                }
            }
            RenderCommand::DrawGlyphRun(run) => {
                if let Some(f) = fonts {
                    if let Paint::Tiling(tiling) = &run.paint {
                        record_tiling_glyph_run(rec, run, tiling, ctm, f, page_images, map, depth);
                    } else {
                        match run.overprint {
                            Some(op) if op.active != 0 => {
                                rec.push_overprint(op);
                                glyph::render_glyph_run_transformed(rec, f, map, run, ctm);
                                rec.pop_blend();
                            }
                            Some(_) => {}
                            None => glyph::render_glyph_run_transformed(rec, f, map, run, ctm),
                        }
                    }
                }
            }
            RenderCommand::DrawImage(draw) => {
                let (img, type3_key) = match images {
                    Some(RecordImages::Page(cache)) => (cache.get(draw.image_id), None),
                    Some(RecordImages::Type3 {
                        font_id,
                        glyph_id,
                        images: cache,
                    }) => (
                        cache.get(draw.image_id),
                        Some(record::Type3ImageKey {
                            font_id,
                            glyph_id,
                            image_id: draw.image_id,
                            stencil_tint: None,
                        }),
                    ),
                    None => (None, None),
                };
                if let Some(img) = img.filter(|img| image::is_valid_decoded_image(img)) {
                    let transform = if *ctm == zpdf_core::Matrix::identity() {
                        draw.transform
                    } else {
                        ctm.concat(&draw.transform)
                    };
                    let quad = image::image_quad(
                        img.width as f32,
                        img.height as f32,
                        &transform,
                        map,
                        draw.alpha,
                    );
                    if image::is_safe_quad(&quad) {
                        if let Some(key) = type3_key {
                            rec.add_type3_image_cached(key, quad, || img.clone());
                        } else {
                            rec.add_image(quad, draw.image_id);
                        }
                    }
                }
            }
            RenderCommand::DrawInlineImage(draw) => {
                let img = &draw.image;
                if image::is_valid_rgba_image(img.width, img.height, img.data.len()) {
                    let transform = if *ctm == zpdf_core::Matrix::identity() {
                        draw.transform
                    } else {
                        ctm.concat(&draw.transform)
                    };
                    let quad = image::image_quad(
                        img.width as f32,
                        img.height as f32,
                        &transform,
                        map,
                        draw.alpha,
                    );
                    if image::is_safe_quad(&quad) {
                        rec.add_inline_image(quad, std::sync::Arc::clone(img));
                    }
                }
            }
            RenderCommand::PushClip { path, rule } => {
                rec.push_clip_transformed(path, *rule, ctm, map);
            }
            RenderCommand::PushClipStroke { path, style } => {
                rec.push_clip_stroke_transformed(path, style, ctm, map);
            }
            RenderCommand::PopClip => rec.pop_clip(),
            RenderCommand::FillPath {
                path,
                rule,
                paint: Paint::Tiling(tiling),
                alpha,
                overprint,
            } => {
                record_tiling_fill(
                    rec,
                    path,
                    *rule,
                    tiling,
                    ctm,
                    *alpha,
                    *overprint,
                    fonts,
                    page_images,
                    map,
                    depth,
                );
            }
            RenderCommand::StrokePath {
                path,
                style,
                paint: Paint::Tiling(tiling),
                alpha,
                overprint,
            } => {
                record_tiling_stroke(
                    rec,
                    path,
                    style,
                    tiling,
                    ctm,
                    *alpha,
                    *overprint,
                    fonts,
                    page_images,
                    map,
                    depth,
                );
            }
            // Non-solid paints other than tiling are rasterized upstream.
            RenderCommand::FillPath { .. } | RenderCommand::StrokePath { .. } => {}
            // A transparency group nested inside a cell or mask: build its
            // sub-mask by streaming `m.commands` directly under `ctm` without
            // copying the shared `Arc<DisplayList>`.
            RenderCommand::PushBlendGroup {
                blend_mode,
                isolated,
                knockout,
                alpha,
                mask,
                ..
            } => {
                let sub = if rec.can_push_blend() {
                    mask.as_ref()
                        .map(|m| build_mask_ops(rec, m, ctm, fonts, images, map, depth))
                } else {
                    None
                };
                if !*isolated && !*knockout {
                    if *blend_mode == zpdf_display_list::BlendMode::Normal
                        && *alpha >= 1.0
                        && sub.is_none()
                    {
                        rec.push_passthrough_blend();
                    } else {
                        rec.push_non_isolated_with_mode(*blend_mode, *alpha, sub);
                    }
                } else {
                    rec.push_blend(*blend_mode, *alpha, sub);
                }
            }
            RenderCommand::PopBlendGroup => rec.pop_blend(),
        }
    }
}

fn transformed_path_bounds(
    path: &zpdf_display_list::Path,
    ctm: &zpdf_core::Matrix,
) -> Option<zpdf_core::Rect> {
    let (mut x0, mut y0, mut x1, mut y1) = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    let identity = *ctm == zpdf_core::Matrix::identity();
    for p in path.points() {
        let tp = if identity { *p } else { p.transform(ctm) };
        if !tp.x.is_finite() || !tp.y.is_finite() {
            return None;
        }
        x0 = x0.min(tp.x);
        y0 = y0.min(tp.y);
        x1 = x1.max(tp.x);
        y1 = y1.max(tp.y);
    }
    (x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite())
        .then(|| zpdf_core::Rect::new(x0, y0, x1, y1))
}

fn transformed_stroke_bounds(
    path: &zpdf_display_list::Path,
    style: &zpdf_display_list::StrokeStyle,
    ctm: &zpdf_core::Matrix,
) -> Option<zpdf_core::Rect> {
    let b = transformed_path_bounds(path, ctm)?;
    let reach = match style.join {
        zpdf_display_list::LineJoin::Miter => style.miter_limit.clamp(1.0, 100.0),
        _ => std::f32::consts::SQRT_2,
    };
    let ctm_scale = if *ctm == zpdf_core::Matrix::identity() {
        1.0
    } else {
        path::max_linear_scale(ctm, 1.0)
    };
    let scaled_width = if style.width.is_finite() && style.width > 0.0 && ctm_scale.is_finite() {
        (style.width * ctm_scale).max(1.0)
    } else {
        1.0
    };
    let grow = (scaled_width as f64 * 0.5 * reach.max(std::f32::consts::SQRT_2) as f64).max(0.5);
    Some(zpdf_core::Rect::new(
        b.x0 - grow,
        b.y0 - grow,
        b.x1 + grow,
        b.y1 + grow,
    ))
}

/// Record a [`zpdf_display_list::TilingPattern`] on the wgpu backend:
/// streams cell commands into page space per visible tile up to 4,096 tiles,
/// and falls back to repeating GPU-rasterized cell texture(s) when the region
/// exceeds `MAX_GPU_TILES` so dense patterns stay bounded and preserve the page
/// backdrop for non-Normal blend modes (ISO 32000-1 §11.6.7).
#[allow(clippy::too_many_arguments)]
fn record_tiling_pattern(
    rec: &mut PageRecorder,
    tiling: &zpdf_display_list::TilingPattern,
    outer_ctm: &zpdf_core::Matrix,
    mut bounds: zpdf_core::Rect,
    alpha: f32,
    fonts: Option<&zpdf_font::FontCache>,
    images: Option<&zpdf_image::ImageCache>,
    map: &PageMap,
    depth: u8,
) {
    const MAX_GPU_TILES: i64 = 4096;
    const MAX_TILING_DEPTH: u8 = 16;
    if depth >= MAX_TILING_DEPTH || tiling.commands.commands.is_empty() || !rec.can_record() {
        return;
    }
    let pat_to_page = if *outer_ctm == zpdf_core::Matrix::identity() {
        tiling.matrix
    } else {
        outer_ctm.concat(&tiling.matrix)
    };
    if ![
        pat_to_page.a,
        pat_to_page.b,
        pat_to_page.c,
        pat_to_page.d,
        pat_to_page.e,
        pat_to_page.f,
    ]
    .iter()
    .all(|v| v.is_finite())
    {
        return;
    }
    if let Some(page_rect) = rec.page_rect() {
        let p = page_rect.normalize();
        let pad = 1.0 / (map.scale as f64).max(1.0);
        let x0 = bounds.x0.max(p.x0 - pad);
        let y0 = bounds.y0.max(p.y0 - pad);
        let x1 = bounds.x1.min(p.x1 + pad);
        let y1 = bounds.y1.min(p.y1 + pad);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        bounds = zpdf_core::Rect::new(x0, y0, x1, y1);
    }
    let Some(from_page) = pat_to_page.inverse() else {
        return;
    };
    let corners = [
        zpdf_core::Point::new(bounds.x0, bounds.y0),
        zpdf_core::Point::new(bounds.x1, bounds.y0),
        zpdf_core::Point::new(bounds.x0, bounds.y1),
        zpdf_core::Point::new(bounds.x1, bounds.y1),
    ];
    let (mut px0, mut py0) = (f64::INFINITY, f64::INFINITY);
    let (mut px1, mut py1) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
    for c in &corners {
        let p = c.transform(&from_page);
        px0 = px0.min(p.x);
        py0 = py0.min(p.y);
        px1 = px1.max(p.x);
        py1 = py1.max(p.y);
    }
    if ![px0, py0, px1, py1].iter().all(|v| v.is_finite()) {
        return;
    }
    let bbox = tiling.bbox.normalize();
    if ![
        bbox.x0,
        bbox.y0,
        bbox.x1,
        bbox.y1,
        tiling.x_step,
        tiling.y_step,
    ]
    .iter()
    .all(|v| v.is_finite())
        || tiling.x_step == 0.0
        || tiling.y_step == 0.0
    {
        return;
    }
    // Strict positive-area overlap: tiles touching only on a shared boundary
    // line (`lo == cell_hi + i*step` or `hi == cell_lo + i*step`) paint nothing
    // inside `bounds` and must not inflate the tile count against MAX_GPU_TILES.
    let range = |lo: f64, hi: f64, cell_lo: f64, cell_hi: f64, step: f64| -> (i64, i64) {
        let t0 = (lo - cell_hi) / step;
        let t1 = (hi - cell_lo) / step;
        let (a, b) = if t0 <= t1 { (t0, t1) } else { (t1, t0) };
        (
            (a.floor() as i64).saturating_add(1),
            (b.ceil() as i64).saturating_sub(1),
        )
    };
    let (i0, i1) = range(px0, px1, bbox.x0, bbox.x1, tiling.x_step);
    let (j0, j1) = range(py0, py1, bbox.y0, bbox.y1, tiling.y_step);
    let nx = i1.saturating_sub(i0).saturating_add(1);
    let ny = j1.saturating_sub(j0).saturating_add(1);
    if nx <= 0 || ny <= 0 {
        return;
    }
    if nx.saturating_mul(ny) > MAX_GPU_TILES {
        record_tiling_pattern_rasterized(
            rec,
            tiling,
            &pat_to_page,
            bounds,
            from_page,
            alpha,
            fonts,
            images,
            map,
            depth,
        );
        return;
    }

    let opacity = unit(alpha);
    if opacity <= 0.0 {
        return;
    }
    // When `opacity < 1`, apply the object's constant opacity ONCE to the
    // finished pattern. Backdrop-dependent cells (ISO 32000-1 §11.6.7) must
    // preserve the live destination backdrop while evaluating every mark at
    // full outer opacity (`push_non_isolated_opacity`), whereas
    // backdrop-independent cells can render into a clean transparent group.
    let wrap_alpha = opacity < 1.0 && rec.can_push_blend();
    if wrap_alpha {
        if rec.tiling_is_backdrop_dependent(tiling) {
            rec.push_non_isolated_opacity(opacity);
        } else {
            rec.push_blend(zpdf_display_list::BlendMode::Normal, opacity, None);
        }
    }
    'tiles: for j in j0..=j1 {
        for i in i0..=i1 {
            if !rec.can_record() {
                break 'tiles;
            }
            let tile_ctm = pat_to_page.concat(&zpdf_core::Matrix::translate(
                i as f64 * tiling.x_step,
                j as f64 * tiling.y_step,
            ));
            record_dl_commands_at(
                rec,
                &tiling.commands,
                &tile_ctm,
                fonts,
                images.map(RecordImages::Page),
                map,
                depth + 1,
            );
        }
    }
    if wrap_alpha {
        rec.pop_blend();
    }
}

/// Bounded O(1)-tile GPU fallback for dense tiling patterns (> `MAX_GPU_TILES`):
/// records the single fundamental period (including any overlapping neighbor
/// copies) once into a synthetic offscreen tile texture and paints `bounds`
/// with a single repeat-sampled quad. For backdrop-dependent cells (e.g. marks
/// with `/BM /Multiply`), splits top-level blend-mode segments so each segment
/// composites against the live destination backdrop with its own [`BlendMode`].
#[allow(clippy::too_many_arguments)]
fn record_tiling_pattern_rasterized(
    rec: &mut PageRecorder,
    tiling: &zpdf_display_list::TilingPattern,
    pat_to_page: &zpdf_core::Matrix,
    bounds: zpdf_core::Rect,
    from_page: zpdf_core::Matrix,
    alpha: f32,
    fonts: Option<&zpdf_font::FontCache>,
    images: Option<&zpdf_image::ImageCache>,
    map: &PageMap,
    depth: u8,
) {
    const MAX_GPU_TILE_DIM: f64 = 2048.0;
    const MAX_GPU_TILE_PIXELS: f64 = 4_194_304.0;

    let opacity = unit(alpha);
    if opacity <= 0.0 || !rec.can_record() {
        return;
    }
    let bbox = tiling.bbox.normalize();
    let (bx0, by0) = (bbox.x0, bbox.y0);
    let (abs_x, abs_y) = (tiling.x_step.abs(), tiling.y_step.abs());
    if bbox.width() <= 0.0 || bbox.height() <= 0.0 || abs_x <= 0.0 || abs_y <= 0.0 {
        return;
    }

    let s = map.scale as f64;
    let page_to_dev =
        zpdf_core::Matrix::new(s, 0.0, 0.0, -s, -(map.x0 as f64) * s, (map.y1 as f64) * s);
    let pat_to_dev = page_to_dev.concat(pat_to_page);
    if ![
        pat_to_dev.a,
        pat_to_dev.b,
        pat_to_dev.c,
        pat_to_dev.d,
        pat_to_dev.e,
        pat_to_dev.f,
    ]
    .iter()
    .all(|v| v.is_finite())
    {
        return;
    }
    let det = pat_to_dev.a * pat_to_dev.d - pat_to_dev.b * pat_to_dev.c;
    if !det.is_finite() || det.abs() < 1e-12 {
        return;
    }
    let len_x = (pat_to_dev.a * abs_x).hypot(pat_to_dev.b * abs_x);
    let len_y = (pat_to_dev.c * abs_y).hypot(pat_to_dev.d * abs_y);
    if !len_x.is_finite() || !len_y.is_finite() || len_x <= 0.0 || len_y <= 0.0 {
        return;
    }

    let mut tw_f = len_x.round().max(1.0);
    let mut th_f = len_y.round().max(1.0);
    let max_dim = tw_f.max(th_f);
    if max_dim > MAX_GPU_TILE_DIM {
        let k = MAX_GPU_TILE_DIM / max_dim;
        tw_f = (tw_f * k).floor().max(1.0);
        th_f = (th_f * k).floor().max(1.0);
    }
    if tw_f * th_f > MAX_GPU_TILE_PIXELS {
        let k = (MAX_GPU_TILE_PIXELS / (tw_f * th_f)).sqrt();
        tw_f = (tw_f * k).floor().max(1.0);
        th_f = (th_f * k).floor().max(1.0);
    }
    let tile_w = tw_f as u32;
    let tile_h = th_f as u32;

    let tile_to_pat = zpdf_core::Matrix::new(
        abs_x / tile_w as f64,
        0.0,
        0.0,
        -abs_y / tile_h as f64,
        bx0,
        by0 + abs_y,
    );
    let m = pat_to_dev.concat(&tile_to_pat);
    let exact_1to1 = (m.a - 1.0).abs() < 1e-4
        && m.b.abs() < 1e-4
        && m.c.abs() < 1e-4
        && (m.d - 1.0).abs() < 1e-4
        && (m.e - m.e.round()).abs() < 1e-4
        && (m.f - m.f.round()).abs() < 1e-4;

    let sx = tile_w as f64 / abs_x;
    let sy = tile_h as f64 / abs_y;
    if !sx.is_finite() || !sy.is_finite() || sx <= 0.0 || sy <= 0.0 {
        return;
    }

    // Map `bounds`'s four corners into repeating [0, 1]^2 tile UVs, shifting by
    // an integer cell offset in f64 before casting to f32 so distant fills keep
    // exact sub-tile phase precision.
    let corners = [
        zpdf_core::Point::new(bounds.x0, bounds.y1),
        zpdf_core::Point::new(bounds.x1, bounds.y1),
        zpdf_core::Point::new(bounds.x1, bounds.y0),
        zpdf_core::Point::new(bounds.x0, bounds.y0),
    ];
    let mut raw_uvs = [(0.0f64, 0.0f64); 4];
    let (mut u_min, mut v_min) = (f64::INFINITY, f64::INFINITY);
    for (idx, c) in corners.iter().enumerate() {
        let p = c.transform(&from_page);
        let u = (p.x - bx0) / abs_x;
        let v = (by0 + abs_y - p.y) / abs_y;
        if !u.is_finite() || !v.is_finite() {
            return;
        }
        u_min = u_min.min(u);
        v_min = v_min.min(v);
        raw_uvs[idx] = (u, v);
    }
    let u_shift = u_min.floor();
    let v_shift = v_min.floor();
    let build_quad = |quad_alpha: f32| -> Option<[transform::TexturedVertex; 4]> {
        let color = [1.0, 1.0, 1.0, quad_alpha];
        let mut quad = [transform::TexturedVertex {
            pos: [0.0, 0.0],
            uv: [0.0, 0.0],
            color,
        }; 4];
        for (idx, c) in corners.iter().enumerate() {
            let pt = map.pt(*c);
            let u = (raw_uvs[idx].0 - u_shift) as f32;
            let v = (raw_uvs[idx].1 - v_shift) as f32;
            if !u.is_finite() || !v.is_finite() {
                return None;
            }
            quad[idx] = transform::TexturedVertex {
                pos: [pt.x, pt.y],
                uv: [u, v],
                color,
            };
        }
        image::is_safe_quad(&quad).then_some(quad)
    };
    let Some(quad_alpha) = build_quad(opacity) else {
        return;
    };
    let Some(quad_opaque) = build_quad(1.0) else {
        return;
    };

    let overlap = tiling.overlap_copy_ranges();
    let pat_to_tile = zpdf_core::Matrix::new(sx, 0.0, 0.0, sy, -bx0 * sx, -by0 * sy);
    let tile_rect = zpdf_core::Rect::new(0.0, 0.0, tile_w as f64, tile_h as f64);
    let tile_map = PageMap::new(tile_rect, 1.0);

    let get_or_record_tile = |rec: &mut PageRecorder,
                              commands: &zpdf_display_list::DisplayList,
                              segment_index: usize,
                              overlap_shift: Option<(i64, i64)>,
                              copy_blend_mode: zpdf_display_list::BlendMode,
                              seg_bounds: Option<zpdf_core::Rect>|
     -> Option<u32> {
        let combine_copies = overlap_shift.is_none()
            && matches!(
                copy_blend_mode,
                zpdf_display_list::BlendMode::Multiply
                    | zpdf_display_list::BlendMode::Screen
                    | zpdf_display_list::BlendMode::Darken
                    | zpdf_display_list::BlendMode::Lighten
            );
        let key_mode = if combine_copies {
            copy_blend_mode
        } else {
            zpdf_display_list::BlendMode::Normal
        };
        let key = record::SyntheticTileKey::new(
            tiling,
            segment_index,
            overlap_shift,
            key_mode,
            tile_w,
            tile_h,
            exact_1to1,
            rec.blend_depth_total(),
        );
        if let Some(id) = rec.cached_synthetic_tile(&key) {
            return Some(id);
        }
        let ops = rec.record_subops(|r| {
            let saved_rect = r.replace_page_rect(Some(tile_rect));
            let (di_range, dj_range) = match overlap_shift {
                Some((di, dj)) => (di..=di, dj..=dj),
                None => (overlap.di.clone(), overlap.dj.clone()),
            };
            let mut drew_any = false;
            'copies: for dj in dj_range {
                for di in di_range.clone() {
                    if !r.can_record() {
                        break 'copies;
                    }
                    if let Some(b) = seg_bounds {
                        let dx = di as f64 * tiling.x_step;
                        let dy = dj as f64 * tiling.y_step;
                        if b.x1 + dx <= bx0 + 1e-9
                            || b.x0 + dx >= bx0 + abs_x - 1e-9
                            || b.y1 + dy <= by0 + 1e-9
                            || b.y0 + dy >= by0 + abs_y - 1e-9
                        {
                            continue;
                        }
                    }
                    if combine_copies && drew_any && r.can_push_blend() {
                        r.push_blend(copy_blend_mode, 1.0, None);
                    }
                    let copy_ctm = pat_to_tile.concat(&zpdf_core::Matrix::translate(
                        di as f64 * tiling.x_step,
                        dj as f64 * tiling.y_step,
                    ));
                    record_dl_commands_at(
                        r,
                        commands,
                        &copy_ctm,
                        fonts,
                        images.map(RecordImages::Page),
                        &tile_map,
                        depth + 1,
                    );
                    r.pop_all_clips();
                    r.close_open_blends();
                    drew_any = true;
                }
            }
            r.replace_page_rect(saved_rect);
        });
        if ops.is_empty() || !rec.can_record() {
            return None;
        }
        rec.insert_synthetic_tile(
            key,
            record::SyntheticTile {
                width: tile_w,
                height: tile_h,
                nearest: exact_1to1,
                ops,
                _keep_commands: std::sync::Arc::clone(&tiling.commands),
            },
        )
    };

    if rec.tiling_is_backdrop_dependent(tiling) {
        if let Some(steps) = tiling.blend_steps() {
            // Preserve the live backdrop across all steps (segments, nested
            // OpacityGroups, and overlapping neighbor copies) while evaluating
            // at opacity 1.0, then apply the object's constant opacity ONCE
            // (ISO 32000-1 §11.6.7).
            const MAX_DENSE_BLEND_PASSES: usize = 1024;
            let overlap_nx = overlap
                .di
                .end()
                .saturating_sub(*overlap.di.start())
                .saturating_add(1)
                .max(0) as usize;
            let overlap_ny = overlap
                .dj
                .end()
                .saturating_sub(*overlap.dj.start())
                .saturating_add(1)
                .max(0) as usize;
            let overlap_count = overlap_nx.saturating_mul(overlap_ny);

            let wrap_alpha = opacity < 1.0 && rec.can_push_blend();
            if wrap_alpha {
                rec.push_non_isolated_opacity(opacity);
            }
            let seg_quad = if wrap_alpha { quad_opaque } else { quad_alpha };
            let seg_alpha = if wrap_alpha { 1.0 } else { opacity };

            #[allow(clippy::too_many_arguments)]
            fn emit_steps<F>(
                rec: &mut PageRecorder,
                tiling: &zpdf_display_list::TilingPattern,
                steps: &[zpdf_display_list::TilingBlendStep],
                overlap_shift: Option<(i64, i64)>,
                seg_quad: [transform::TexturedVertex; 4],
                quad_opaque: [transform::TexturedVertex; 4],
                seg_alpha: f32,
                remaining_passes: &mut usize,
                get_or_record_tile: &F,
            ) where
                F: Fn(
                    &mut PageRecorder,
                    &zpdf_display_list::DisplayList,
                    usize,
                    Option<(i64, i64)>,
                    zpdf_display_list::BlendMode,
                    Option<zpdf_core::Rect>,
                ) -> Option<u32>,
            {
                for step in steps {
                    if !rec.can_record() {
                        return;
                    }
                    match step {
                        zpdf_display_list::TilingBlendStep::Segment(seg) => {
                            if let Some((di, dj)) = overlap_shift {
                                if !seg.intersects_period(tiling, di, dj) {
                                    continue;
                                }
                            }
                            if rec.is_opaque_backdrop() && seg.is_identity_on_opaque() {
                                continue;
                            }
                            if *remaining_passes == 0 {
                                rec.set_recording_error(
                                    "GPU dense tiling pattern exceeds blend pass budget",
                                );
                                return;
                            }
                            *remaining_passes -= 1;
                            let Some(tile_id) = get_or_record_tile(
                                rec,
                                &seg.commands,
                                seg.segment_id,
                                overlap_shift,
                                if overlap_shift.is_none() {
                                    seg.blend_mode
                                } else {
                                    zpdf_display_list::BlendMode::Normal
                                },
                                seg.bounds,
                            ) else {
                                continue;
                            };
                            if seg.blend_mode == zpdf_display_list::BlendMode::Normal {
                                rec.add_tile(seg_quad, tile_id);
                            } else if rec.can_push_blend() {
                                rec.push_blend(seg.blend_mode, seg_alpha, None);
                                rec.add_tile(quad_opaque, tile_id);
                                rec.pop_blend();
                            } else {
                                rec.add_tile(seg_quad, tile_id);
                            }
                        }
                        zpdf_display_list::TilingBlendStep::OpacityGroup {
                            alpha: inner_alpha,
                            mask: inner_mask,
                            steps: inner_steps,
                        } => {
                            if let Some((di, dj)) = overlap_shift {
                                if inner_mask
                                    .as_ref()
                                    .is_some_and(|m| !m.intersects_period(tiling, di, dj))
                                    || !zpdf_display_list::TilingBlendStep::any_intersects_period(
                                        inner_steps,
                                        tiling,
                                        di,
                                        dj,
                                    )
                                {
                                    continue;
                                }
                            }
                            let a = unit(*inner_alpha);
                            if a <= 0.0 {
                                continue;
                            }
                            if *remaining_passes == 0 {
                                // Check whether any visible inner segment still needs a pass
                                // before recording mask geometry or blend frames.
                                emit_steps(
                                    rec,
                                    tiling,
                                    inner_steps,
                                    overlap_shift,
                                    seg_quad,
                                    quad_opaque,
                                    seg_alpha,
                                    remaining_passes,
                                    get_or_record_tile,
                                );
                                continue;
                            }
                            let mask_ops = match inner_mask {
                                Some(m_seg) => {
                                    let Some(tile_id) = get_or_record_tile(
                                        rec,
                                        &m_seg.commands,
                                        m_seg.segment_id,
                                        overlap_shift,
                                        zpdf_display_list::BlendMode::Normal,
                                        m_seg.bounds,
                                    ) else {
                                        continue;
                                    };
                                    let ops = rec.record_subops(|r| {
                                        r.add_tile(quad_opaque, tile_id);
                                    });
                                    Some(record::MaskOps {
                                        ops,
                                        kind: zpdf_display_list::SoftMaskKind::Alpha,
                                        backdrop_luma: 0.0,
                                        dx: 0,
                                        dy: 0,
                                        transfer: None,
                                    })
                                }
                                None => None,
                            };
                            let wrap_inner =
                                (a < 1.0 || mask_ops.is_some()) && rec.can_push_blend();
                            if wrap_inner {
                                rec.push_non_isolated(a, mask_ops);
                            }
                            emit_steps(
                                rec,
                                tiling,
                                inner_steps,
                                overlap_shift,
                                if wrap_inner { quad_opaque } else { seg_quad },
                                quad_opaque,
                                if wrap_inner { 1.0 } else { seg_alpha },
                                remaining_passes,
                                get_or_record_tile,
                            );
                            if wrap_inner {
                                rec.pop_blend();
                            }
                        }
                    }
                }
            }

            let mut remaining_passes = MAX_DENSE_BLEND_PASSES;
            let separate_copies = if overlap_count > 1 {
                let mut total_passes = 0usize;
                'count: for dj in overlap.dj.clone() {
                    for di in overlap.di.clone() {
                        total_passes = total_passes.saturating_add(
                            zpdf_display_list::TilingBlendStep::count_intersecting_passes(
                                &steps, tiling, di, dj,
                            ),
                        );
                        if total_passes > MAX_DENSE_BLEND_PASSES {
                            break 'count;
                        }
                    }
                }
                if total_passes > MAX_DENSE_BLEND_PASSES {
                    if zpdf_display_list::TilingBlendStep::is_affine_in_backdrop(&steps) {
                        tracing::warn!(
                            total_passes,
                            limit = MAX_DENSE_BLEND_PASSES,
                            "dense GPU tiling blend plan exceeds pass budget; collapsing affine overlapping copies into 3-seed tiles"
                        );
                        // Preserve exact per-copy draw order (all steps of copy 0,
                        // then all steps of copy 1, ...) by evaluating the cell's
                        // overlapping copies once onto tiny transparent (`tile_trans`),
                        // black (`tile0`), and white (`tile1`) tiles, then compositing
                        // the exact premultiplied affine formula in 1 shader pass.
                        const MAX_AFFINE_TILE_PASSES: usize = 16_384;
                        #[allow(clippy::too_many_arguments)]
                        fn record_tile_steps_copy(
                            r: &mut PageRecorder,
                            tiling: &zpdf_display_list::TilingPattern,
                            steps: &[zpdf_display_list::TilingBlendStep],
                            di: i64,
                            dj: i64,
                            copy_ctm: &zpdf_core::Matrix,
                            fonts: Option<&zpdf_font::FontCache>,
                            images: Option<&zpdf_image::ImageCache>,
                            tile_map: &PageMap,
                            depth: u8,
                            seed_is_opaque: bool,
                            remaining: &mut usize,
                            exhausted: &mut bool,
                        ) {
                            for step in steps {
                                if !r.can_record() || *exhausted {
                                    return;
                                }
                                match step {
                                    zpdf_display_list::TilingBlendStep::Segment(seg) => {
                                        if !seg.intersects_period(tiling, di, dj) {
                                            continue;
                                        }
                                        if seed_is_opaque && seg.is_identity_on_opaque() {
                                            continue;
                                        }
                                        if *remaining == 0 {
                                            *exhausted = true;
                                            return;
                                        }
                                        *remaining -= 1;
                                        let wrap = seg.blend_mode
                                            != zpdf_display_list::BlendMode::Normal
                                            && r.can_push_blend();
                                        if wrap {
                                            r.push_blend(seg.blend_mode, 1.0, None);
                                        }
                                        record_dl_commands_at(
                                            r,
                                            &seg.commands,
                                            copy_ctm,
                                            fonts,
                                            images.map(RecordImages::Page),
                                            tile_map,
                                            depth + 1,
                                        );
                                        r.pop_all_clips();
                                        if wrap {
                                            r.pop_blend();
                                        }
                                    }
                                    zpdf_display_list::TilingBlendStep::OpacityGroup {
                                        alpha,
                                        mask,
                                        steps: inner,
                                    } => {
                                        if mask
                                            .as_ref()
                                            .is_some_and(|m| !m.intersects_period(tiling, di, dj))
                                            || !zpdf_display_list::TilingBlendStep::any_intersects_period(
                                                inner, tiling, di, dj,
                                            )
                                        {
                                            continue;
                                        }
                                        let a = unit(*alpha);
                                        if a <= 0.0 {
                                            continue;
                                        }
                                        let mask_ops = mask.as_ref().map(|m_seg| {
                                            let m_ops = r.record_subops(|mr| {
                                                record_dl_commands_at(
                                                    mr,
                                                    &m_seg.commands,
                                                    copy_ctm,
                                                    fonts,
                                                    images.map(RecordImages::Page),
                                                    tile_map,
                                                    depth + 1,
                                                );
                                                mr.pop_all_clips();
                                            });
                                            record::MaskOps {
                                                ops: m_ops,
                                                kind: zpdf_display_list::SoftMaskKind::Alpha,
                                                backdrop_luma: 0.0,
                                                dx: 0,
                                                dy: 0,
                                                transfer: None,
                                            }
                                        });
                                        let wrap_group =
                                            (a < 1.0 || mask_ops.is_some()) && r.can_push_blend();
                                        if wrap_group {
                                            r.push_non_isolated(a, mask_ops);
                                        }
                                        record_tile_steps_copy(
                                            r,
                                            tiling,
                                            inner,
                                            di,
                                            dj,
                                            copy_ctm,
                                            fonts,
                                            images,
                                            tile_map,
                                            depth,
                                            seed_is_opaque,
                                            remaining,
                                            exhausted,
                                        );
                                        if wrap_group {
                                            r.pop_blend();
                                        }
                                    }
                                }
                            }
                        }

                        let build_affine_sample = |rec: &mut PageRecorder,
                                                   seed: Option<[f32; 4]>,
                                                   seg_idx: usize|
                         -> Option<u32> {
                            let key = record::SyntheticTileKey::new(
                                tiling,
                                seg_idx,
                                None,
                                zpdf_display_list::BlendMode::Normal,
                                tile_w,
                                tile_h,
                                exact_1to1,
                                rec.blend_depth_total(),
                            );
                            if let Some(id) = rec.cached_synthetic_tile(&key) {
                                return Some(id);
                            }
                            let mut exhausted = false;
                            let ops = rec.record_subops(|r| {
                                let saved_rect = r.replace_page_rect(Some(tile_rect));
                                let seed_is_opaque = if let Some(seed_color) = seed {
                                    let mut full = zpdf_display_list::Path::new();
                                    full.rect(tile_rect);
                                    r.add_fill(
                                        &full,
                                        zpdf_display_list::FillRule::NonZero,
                                        seed_color,
                                        &tile_map,
                                    );
                                    true
                                } else {
                                    false
                                };
                                let mut rem = MAX_AFFINE_TILE_PASSES;
                                'copies: for dj in overlap.dj.clone() {
                                    for di in overlap.di.clone() {
                                        if !r.can_record() || exhausted {
                                            break 'copies;
                                        }
                                        let copy_ctm =
                                            pat_to_tile.concat(&zpdf_core::Matrix::translate(
                                                di as f64 * tiling.x_step,
                                                dj as f64 * tiling.y_step,
                                            ));
                                        record_tile_steps_copy(
                                            r,
                                            tiling,
                                            &steps,
                                            di,
                                            dj,
                                            &copy_ctm,
                                            fonts,
                                            images,
                                            &tile_map,
                                            depth,
                                            seed_is_opaque,
                                            &mut rem,
                                            &mut exhausted,
                                        );
                                        r.pop_all_clips();
                                        r.close_open_blends();
                                    }
                                }
                                r.replace_page_rect(saved_rect);
                            });
                            if exhausted {
                                rec.set_recording_error(
                                    "GPU dense tiling pattern exceeds affine tile pass budget",
                                );
                                return None;
                            }
                            if !rec.can_record() {
                                return None;
                            }
                            rec.insert_synthetic_tile(
                                key,
                                record::SyntheticTile {
                                    width: tile_w,
                                    height: tile_h,
                                    nearest: exact_1to1,
                                    ops,
                                    _keep_commands: std::sync::Arc::clone(&tiling.commands),
                                },
                            )
                        };

                        let t_trans = build_affine_sample(rec, None, usize::MAX - 2);
                        let t0 =
                            build_affine_sample(rec, Some([0.0, 0.0, 0.0, 1.0]), usize::MAX - 1);
                        let t1 = build_affine_sample(rec, Some([1.0, 1.0, 1.0, 1.0]), usize::MAX);
                        if let (Some(tile_trans), Some(tile0), Some(tile1)) = (t_trans, t0, t1) {
                            rec.add_affine_tile(seg_quad, tile_trans, tile0, tile1, exact_1to1);
                        }
                        if wrap_alpha {
                            rec.pop_blend();
                        }
                        return;
                    }
                    // Non-affine blend modes (e.g. Darken/Lighten) cannot be
                    // interpolated from black/white samples: preserve exact
                    // copy order against the actual backdrop up to a bounded
                    // pass ceiling (explicitly failing on exhaustion).
                    remaining_passes = 16_384;
                    true
                } else {
                    true
                }
            } else {
                false
            };
            if separate_copies {
                'copies: for dj in overlap.dj.clone() {
                    for di in overlap.di.clone() {
                        if !rec.can_record() {
                            break 'copies;
                        }
                        emit_steps(
                            rec,
                            tiling,
                            &steps,
                            Some((di, dj)),
                            seg_quad,
                            quad_opaque,
                            seg_alpha,
                            &mut remaining_passes,
                            &get_or_record_tile,
                        );
                    }
                }
            } else {
                emit_steps(
                    rec,
                    tiling,
                    &steps,
                    None,
                    seg_quad,
                    quad_opaque,
                    seg_alpha,
                    &mut remaining_passes,
                    &get_or_record_tile,
                );
            }
            if wrap_alpha {
                rec.pop_blend();
            }
            return;
        }
    }

    if let Some(tile_id) = get_or_record_tile(
        rec,
        &tiling.commands,
        0,
        None,
        zpdf_display_list::BlendMode::Normal,
        None,
    ) {
        rec.add_tile(quad_alpha, tile_id);
    }
}

/// Apply the linear (2×2) part of `ctm` to a soft-mask reuse displacement
/// vector `(ox, oy)` without translating it.
fn transform_mask_offset(offset: (f32, f32), ctm: &zpdf_core::Matrix) -> (f32, f32) {
    if *ctm == zpdf_core::Matrix::identity() {
        return offset;
    }
    let (ox, oy) = (offset.0 as f64, offset.1 as f64);
    (
        (ctm.a * ox + ctm.c * oy) as f32,
        (ctm.b * ox + ctm.d * oy) as f32,
    )
}

#[allow(clippy::too_many_arguments)]
fn record_tiling_fill(
    rec: &mut PageRecorder,
    path: &zpdf_display_list::Path,
    rule: zpdf_display_list::FillRule,
    tiling: &zpdf_display_list::TilingPattern,
    outer_ctm: &zpdf_core::Matrix,
    alpha: f32,
    overprint: Option<zpdf_display_list::Overprint>,
    fonts: Option<&zpdf_font::FontCache>,
    images: Option<&zpdf_image::ImageCache>,
    map: &PageMap,
    depth: u8,
) {
    let Some(bounds) = transformed_path_bounds(path, outer_ctm) else {
        return;
    };
    match overprint {
        Some(op) if op.active != 0 => {
            rec.push_overprint(op);
            rec.push_clip_transformed(path, rule, outer_ctm, map);
            record_tiling_pattern(
                rec, tiling, outer_ctm, bounds, alpha, fonts, images, map, depth,
            );
            rec.pop_clip();
            rec.pop_blend();
        }
        Some(_) => {}
        None => {
            let opacity = unit(alpha);
            if opacity <= 0.0 {
                return;
            }
            // For backdrop-dependent tiling fills, evaluate the full pattern
            // against a copy of the live backdrop and apply BOTH outer shape
            // coverage and constant object opacity ONCE via a non-isolated
            // alpha mask (ISO 32000-1 §11.4.5 / §11.6.7) so anti-aliased edge
            // pixels are never blended multiple times across segments or tiles.
            if rec.tiling_is_backdrop_dependent(tiling) && rec.can_push_blend() {
                let shape_ops = rec.record_subops_clipped(|r| {
                    r.add_fill_transformed(path, rule, [1.0, 1.0, 1.0, 1.0], outer_ctm, map);
                });
                let shape_mask = record::MaskOps {
                    ops: shape_ops,
                    kind: zpdf_display_list::SoftMaskKind::Alpha,
                    backdrop_luma: 0.0,
                    dx: 0,
                    dy: 0,
                    transfer: None,
                };
                rec.with_non_isolated_shape_mask(opacity, shape_mask, |r| {
                    record_tiling_pattern(
                        r, tiling, outer_ctm, bounds, 1.0, fonts, images, map, depth,
                    );
                });
            } else {
                rec.push_clip_transformed(path, rule, outer_ctm, map);
                record_tiling_pattern(
                    rec, tiling, outer_ctm, bounds, alpha, fonts, images, map, depth,
                );
                rec.pop_clip();
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn record_tiling_stroke(
    rec: &mut PageRecorder,
    path: &zpdf_display_list::Path,
    style: &zpdf_display_list::StrokeStyle,
    tiling: &zpdf_display_list::TilingPattern,
    outer_ctm: &zpdf_core::Matrix,
    alpha: f32,
    overprint: Option<zpdf_display_list::Overprint>,
    fonts: Option<&zpdf_font::FontCache>,
    images: Option<&zpdf_image::ImageCache>,
    map: &PageMap,
    depth: u8,
) {
    let Some(bounds) = transformed_stroke_bounds(path, style, outer_ctm) else {
        return;
    };
    match overprint {
        Some(op) if op.active != 0 => {
            rec.push_overprint(op);
            rec.push_clip_stroke_transformed(path, style, outer_ctm, map);
            record_tiling_pattern(
                rec, tiling, outer_ctm, bounds, alpha, fonts, images, map, depth,
            );
            rec.pop_clip();
            rec.pop_blend();
        }
        Some(_) => {}
        None => {
            let opacity = unit(alpha);
            if opacity <= 0.0 {
                return;
            }
            if rec.tiling_is_backdrop_dependent(tiling) && rec.can_push_blend() {
                let shape_ops = rec.record_subops_clipped(|r| {
                    r.add_stroke_transformed(path, style, [1.0, 1.0, 1.0, 1.0], outer_ctm, map);
                });
                let shape_mask = record::MaskOps {
                    ops: shape_ops,
                    kind: zpdf_display_list::SoftMaskKind::Alpha,
                    backdrop_luma: 0.0,
                    dx: 0,
                    dy: 0,
                    transfer: None,
                };
                rec.with_non_isolated_shape_mask(opacity, shape_mask, |r| {
                    record_tiling_pattern(
                        r, tiling, outer_ctm, bounds, 1.0, fonts, images, map, depth,
                    );
                });
            } else {
                rec.push_clip_stroke_transformed(path, style, outer_ctm, map);
                record_tiling_pattern(
                    rec, tiling, outer_ctm, bounds, alpha, fonts, images, map, depth,
                );
                rec.pop_clip();
            }
        }
    }
}

/// Record a `DrawGlyphRun` painted with a [`zpdf_display_list::TilingPattern`]
/// by rasterizing the glyph outlines / atlas / Type 3 shapes into an alpha
/// coverage mask and compositing the tiling pattern through that mask (both on
/// top-level pages and nested inside tiling cells).
#[allow(clippy::too_many_arguments)]
fn record_tiling_glyph_run(
    rec: &mut PageRecorder,
    run: &zpdf_display_list::GlyphRun,
    tiling: &zpdf_display_list::TilingPattern,
    outer_ctm: &zpdf_core::Matrix,
    fonts: &zpdf_font::FontCache,
    images: Option<&zpdf_image::ImageCache>,
    map: &PageMap,
    depth: u8,
) {
    let opacity = unit(run.alpha);
    if opacity <= 0.0 || !rec.can_record() {
        return;
    }
    let paint_shape_ops = |rec: &mut PageRecorder,
                           shape_ops: Vec<record::PageOp>,
                           mask_bounds: Option<zpdf_core::Rect>,
                           eff_opacity: f32| {
        if shape_ops.is_empty() || !rec.can_record() {
            return;
        }
        let bounds = mask_bounds
            .or_else(|| rec.page_rect())
            .unwrap_or_else(|| zpdf_core::Rect::new(0.0, 0.0, 0.0, 0.0));
        if bounds.width() <= 0.0 || bounds.height() <= 0.0 {
            return;
        }
        let shape_mask = record::MaskOps {
            ops: shape_ops,
            kind: zpdf_display_list::SoftMaskKind::Alpha,
            backdrop_luma: 0.0,
            dx: 0,
            dy: 0,
            transfer: None,
        };
        if !rec.can_push_blend() {
            return;
        }
        if rec.tiling_is_backdrop_dependent(tiling) {
            rec.with_non_isolated_shape_mask(eff_opacity, shape_mask, |r| {
                record_tiling_pattern(
                    r,
                    tiling,
                    outer_ctm,
                    bounds,
                    1.0,
                    Some(fonts),
                    images,
                    map,
                    depth,
                );
            });
        } else {
            rec.with_isolated_shape_mask(eff_opacity, shape_mask, |r| {
                record_tiling_pattern(
                    r,
                    tiling,
                    outer_ctm,
                    bounds,
                    1.0,
                    Some(fonts),
                    images,
                    map,
                    depth,
                );
            });
        }
    };
    let is_type3 = fonts.get(run.font_id).is_some_and(|f| f.is_type3());
    let paint_masked = |rec: &mut PageRecorder, eff_opacity: f32| {
        if is_type3 {
            glyph::render_type3_tiling_transformed(
                rec,
                fonts,
                map,
                run,
                outer_ctm,
                eff_opacity,
                |r, ops, bounds| paint_shape_ops(r, ops, bounds, eff_opacity),
            );
            return;
        }
        let mut mask_bounds = None;
        let shape_ops = rec.record_subops_clipped(|r| {
            mask_bounds =
                glyph::render_glyph_run_mask_and_bounds_transformed(r, fonts, map, run, outer_ctm);
        });
        paint_shape_ops(rec, shape_ops, mask_bounds, eff_opacity);
    };
    match run.overprint {
        Some(op) if op.active != 0 => {
            rec.push_overprint(op);
            paint_masked(rec, opacity);
            rec.pop_blend();
        }
        Some(_) => {}
        None => paint_masked(rec, opacity),
    }
}

/// Build the [`record::MaskOps`] for a group's soft mask: stream its group
/// content directly into the shared arena under `ctm` (preserving the shared
/// `Arc<DisplayList>` without pre-recording clones), apply the linear part of
/// `ctm` and page scale to its `offset`, and carry the /TR transfer LUT.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_mask_ops(
    rec: &mut PageRecorder,
    mask: &zpdf_display_list::SoftMask,
    ctm: &zpdf_core::Matrix,
    fonts: Option<&zpdf_font::FontCache>,
    images: Option<RecordImages<'_>>,
    map: &PageMap,
    depth: u8,
) -> record::MaskOps {
    let ops = rec.record_subops(|r| {
        record_dl_commands_at(r, &mask.commands, ctm, fonts, images, map, depth);
    });
    // Transform mask displacement by the linear part of `ctm`, then map to
    // device pixels; device Y grows downward (mirrors CPU `shift_plane`).
    let (off_x, off_y) = transform_mask_offset(mask.offset, ctm);
    let dx = (off_x * map.scale).round() as i32;
    let dy = (-off_y * map.scale).round() as i32;
    record::MaskOps {
        ops,
        kind: mask.kind,
        backdrop_luma: mask.backdrop_luma,
        dx,
        dy,
        transfer: mask.transfer.clone(),
    }
}

/// Stamp clip paths into the current pass's stencil (clip_write pipeline).
fn stamp_clips(pass: &mut wgpu::RenderPass, clips: &[record::ClipStamp], res: &ReplayRes) {
    let Some((svb, sib)) = res.solid else {
        return;
    };
    if clips.is_empty() {
        return;
    }
    pass.set_pipeline(&res.pipelines.clip_write);
    pass.set_vertex_buffer(0, svb.slice(..));
    pass.set_index_buffer(sib.slice(..), wgpu::IndexFormat::Uint32);
    for c in clips {
        pass.set_stencil_reference(c.ref_value);
        pass.draw_indexed(
            c.range.first_index..c.range.first_index + c.range.index_count,
            c.range.base_vertex,
            0..1,
        );
    }
}

fn begin_layer_pass<'e>(
    encoder: &'e mut wgpu::CommandEncoder,
    layer: &'e blend::RenderLayer,
    color_load: wgpu::LoadOp<wgpu::Color>,
    stencil_load: wgpu::LoadOp<u32>,
) -> wgpu::RenderPass<'e> {
    encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("zpdf-layer-pass"),
        color_attachments: &[Some(layer.color_attachment(color_load))],
        depth_stencil_attachment: Some(layer.stencil_attachment(stencil_load)),
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    })
}

/// Initialize a layer: clear color + stencil, then re-stamp any inherited clips.
fn init_layer(
    encoder: &mut wgpu::CommandEncoder,
    layer: &blend::RenderLayer,
    clear: wgpu::Color,
    clips: &[record::ClipStamp],
    res: &ReplayRes,
    budget: &mut EncodeBudget,
) -> Result<(), WgpuRenderError> {
    budget.charge_pass(clips.len().max(1))?;
    let mut pass = begin_layer_pass(
        encoder,
        layer,
        wgpu::LoadOp::Clear(clear),
        wgpu::LoadOp::Clear(0),
    );
    pass.set_bind_group(0, Some(res.bind_group), &[]);
    stamp_clips(&mut pass, clips, res);
    Ok(())
}

/// Initialize a non-isolated layer by copying `parent`'s current resolved
/// pixels into `layer`, clearing stencil to 0, and re-stamping any active clips.
#[allow(clippy::too_many_arguments)]
fn init_layer_from_base(
    ctx: &GpuContext,
    encoder: &mut wgpu::CommandEncoder,
    layer: &blend::RenderLayer,
    parent: &blend::RenderLayer,
    clips: &[record::ClipStamp],
    res: &ReplayRes,
    keep_bgs: &mut Vec<wgpu::BindGroup>,
    budget: &mut EncodeBudget,
) -> Result<(), WgpuRenderError> {
    budget.charge_pass(clips.len().max(1))?;
    let device = &ctx.device;
    let copy_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("zpdf-copy-base-bg"),
        layout: &ctx.pipelines.composite_bgl,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(parent.sampleable_view()),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(parent.sampleable_view()),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: ctx.pipelines.copy_base_uniform.as_entire_binding(),
            },
        ],
    });
    {
        let mut pass = begin_layer_pass(
            encoder,
            layer,
            wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
            wgpu::LoadOp::Clear(0),
        );
        pass.set_bind_group(0, Some(res.bind_group), &[]);
        if let (Some(fs), Some((svb, sib))) = (res.fs_range, res.solid) {
            pass.set_pipeline(&ctx.pipelines.composite);
            pass.set_bind_group(1, Some(&copy_bg), &[]);
            pass.set_vertex_buffer(0, svb.slice(..));
            pass.set_index_buffer(sib.slice(..), wgpu::IndexFormat::Uint32);
            pass.set_stencil_reference(0);
            pass.draw_indexed(
                fs.first_index..fs.first_index + fs.index_count,
                fs.base_vertex,
                0..1,
            );
        }
        stamp_clips(&mut pass, clips, res);
    }
    keep_bgs.push(copy_bg);
    Ok(())
}

/// Composite `src` onto `dst` into a newly acquired scratch layer using the
/// composite shader mode `mode_id`, re-stamping `clips` afterwards.
#[allow(clippy::too_many_arguments)]
fn composite_layer(
    ctx: &GpuContext,
    encoder: &mut wgpu::CommandEncoder,
    pool: &mut blend::LayerPool,
    dst: usize,
    src: usize,
    mode_id: u32,
    alpha: f32,
    op_active: u32,
    cmyk: [f32; 4],
    clips: &[record::ClipStamp],
    res: &ReplayRes,
    keep_bgs: &mut Vec<wgpu::BindGroup>,
    keep_bufs: &mut Vec<wgpu::Buffer>,
    budget: &mut EncodeBudget,
) -> Result<usize, WgpuRenderError> {
    use wgpu::util::DeviceExt;
    budget.charge_pass(clips.len().max(1))?;
    let device = &ctx.device;
    let scratch = pool.acquire(device)?;
    let custom_mode_buf = if op_active == 0
        && (alpha - 1.0).abs() < 1e-6
        && (mode_id as usize) < ctx.pipelines.blend_opaque_uniforms.len()
    {
        None
    } else {
        Some(
            device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("zpdf-blend-mode"),
                contents: bytemuck::cast_slice(&[
                    mode_id,
                    alpha.to_bits(),
                    op_active,
                    0u32,
                    cmyk[0].to_bits(),
                    cmyk[1].to_bits(),
                    cmyk[2].to_bits(),
                    cmyk[3].to_bits(),
                ]),
                usage: wgpu::BufferUsages::UNIFORM,
            }),
        )
    };
    let mode_buf_ref = match custom_mode_buf.as_ref() {
        Some(buf) => buf,
        None => &ctx.pipelines.blend_opaque_uniforms[mode_id as usize],
    };
    let comp_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("zpdf-composite-bg"),
        layout: &ctx.pipelines.composite_bgl,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(pool.get(dst).sampleable_view()),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(pool.get(src).sampleable_view()),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: mode_buf_ref.as_entire_binding(),
            },
        ],
    });
    {
        let mut pass = begin_layer_pass(
            encoder,
            pool.get(scratch),
            wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
            wgpu::LoadOp::Clear(0),
        );
        pass.set_bind_group(0, Some(res.bind_group), &[]);
        if let (Some(fs), Some((svb, sib))) = (res.fs_range, res.solid) {
            pass.set_pipeline(&ctx.pipelines.composite);
            pass.set_bind_group(1, Some(&comp_bg), &[]);
            pass.set_vertex_buffer(0, svb.slice(..));
            pass.set_index_buffer(sib.slice(..), wgpu::IndexFormat::Uint32);
            pass.set_stencil_reference(0);
            pass.draw_indexed(
                fs.first_index..fs.first_index + fs.index_count,
                fs.base_vertex,
                0..1,
            );
        }
        stamp_clips(&mut pass, clips, res);
    }
    keep_bgs.push(comp_bg);
    if let Some(buf) = custom_mode_buf {
        keep_bufs.push(buf);
    }
    Ok(scratch)
}

/// Composite an op list (which may contain `PushBlend`/`PopBlend`) onto `base`,
/// returning the layer index holding the final result. `base` must already be
/// acquired and initialized (cleared, clips stamped); it is never recycled here.
///
/// Shared by the page render (base = the page background) and a soft mask's group
/// (base = the /BC backdrop) so a transparency group nested inside a mask is
/// composited identically to one on the page. Recurses only through
/// [`render_soft_mask_layer`] for a mask's disjoint `mask.ops`; nested
/// non-isolated groups with non-`Normal` outer blend modes track a paired
/// accumulated-alpha layer on the active stack so every child op and soft mask
/// is encoded linearly in one flat scan (never `2^depth` recursive replays),
/// and every encoded pass is charged against the shared `budget`.
///
/// One open transparency/overprint group on the composite stack: its blend
/// mode, group alpha, optional soft mask, inherited clips, optional overprint
/// descriptor, and non-isolated flag.
type BlendFrame<'a> = (
    zpdf_display_list::BlendMode,
    f32,
    Option<&'a record::MaskOps>,
    Vec<record::ClipStamp>,
    Option<zpdf_display_list::Overprint>,
    bool,
);

#[allow(clippy::too_many_arguments)]
fn composite_into(
    ctx: &GpuContext,
    encoder: &mut wgpu::CommandEncoder,
    pool: &mut blend::LayerPool,
    base: usize,
    ops: &[PageOp],
    res: &ReplayRes,
    keep_bgs: &mut Vec<wgpu::BindGroup>,
    keep_bufs: &mut Vec<wgpu::Buffer>,
    budget: &mut EncodeBudget,
) -> Result<usize, WgpuRenderError> {
    const OVERPRINT_MODE: u32 = 100;
    const LERP_ALPHA_MODE: u32 = 102;
    let device = &ctx.device;
    // Each stack entry holds `(color_layer, alpha_layer, clear_c, clear_a)`.
    // `clear_c` / `clear_a` defer an unclipped transparent `LoadOp::Clear` so
    // the first draw pass clears and renders in one pass instead of two.
    let mut active: Vec<(usize, Option<usize>, bool, bool)> = vec![(base, None, false, false)];
    let mut blend_info: Vec<BlendFrame> = Vec::new();

    let flush_clear = |encoder: &mut wgpu::CommandEncoder,
                       layer: &blend::RenderLayer,
                       flag: &mut bool,
                       budget: &mut EncodeBudget|
     -> Result<(), WgpuRenderError> {
        if *flag {
            budget.charge_pass(1)?;
            let pass = begin_layer_pass(
                encoder,
                layer,
                wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                wgpu::LoadOp::Clear(0),
            );
            drop(pass);
            *flag = false;
        }
        Ok(())
    };

    let mut i = 0;
    while i < ops.len() {
        let start = i;
        while i < ops.len()
            && !matches!(
                ops[i],
                PageOp::PushBlend { .. } | PageOp::PopBlend | PageOp::AffineTile { .. }
            )
        {
            i += 1;
        }
        if start < i {
            let chunk = &ops[start..i];
            let (cur_c, cur_a, clear_c, clear_a) = active.last_mut().unwrap();
            budget.charge_pass(chunk.len())?;
            {
                let (c_load, s_load) = if *clear_c {
                    *clear_c = false;
                    (
                        wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        wgpu::LoadOp::Clear(0),
                    )
                } else {
                    (wgpu::LoadOp::Load, wgpu::LoadOp::Load)
                };
                let mut pass = begin_layer_pass(encoder, pool.get(*cur_c), c_load, s_load);
                replay_ops(&mut pass, chunk, res);
            }
            if let Some(a) = *cur_a {
                budget.charge_pass(chunk.len())?;
                let (c_load, s_load) = if *clear_a {
                    *clear_a = false;
                    (
                        wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        wgpu::LoadOp::Clear(0),
                    )
                } else {
                    (wgpu::LoadOp::Load, wgpu::LoadOp::Load)
                };
                let mut pass = begin_layer_pass(encoder, pool.get(a), c_load, s_load);
                replay_ops(&mut pass, chunk, res);
            }
        }
        if i >= ops.len() {
            break;
        }
        match &ops[i] {
            PageOp::AffineTile {
                range,
                clip_ref,
                clips,
                tile_trans,
                tile0,
                tile1,
                nearest,
            } => {
                let (cur_c, cur_a, clear_c, clear_a) = active.last_mut().unwrap();
                flush_clear(encoder, pool.get(*cur_c), clear_c, budget)?;
                let old_c = *cur_c;
                let cur_a_id = *cur_a;
                let scratch = pool.acquire(device)?;
                init_layer_from_base(
                    ctx,
                    encoder,
                    pool.get(scratch),
                    pool.get(old_c),
                    clips,
                    res,
                    keep_bgs,
                    budget,
                )?;
                if let (Some((tvb, tib)), Some(v_trans), Some(v0), Some(v1)) = (
                    res.tex,
                    res.tile_outputs
                        .get(*tile_trans as usize)
                        .map(TileOutput::sampleable_view),
                    res.tile_outputs
                        .get(*tile0 as usize)
                        .map(TileOutput::sampleable_view),
                    res.tile_outputs
                        .get(*tile1 as usize)
                        .map(TileOutput::sampleable_view),
                ) {
                    budget.charge_pass(1)?;
                    let sampler = if *nearest {
                        &ctx.pipelines.repeat_nearest_sampler
                    } else {
                        &ctx.pipelines.repeat_sampler
                    };
                    let affine_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("zpdf-affine-tile-bg"),
                        layout: &ctx.pipelines.affine_tile_bgl,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: wgpu::BindingResource::TextureView(
                                    pool.get(old_c).sampleable_view(),
                                ),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: wgpu::BindingResource::TextureView(v_trans),
                            },
                            wgpu::BindGroupEntry {
                                binding: 2,
                                resource: wgpu::BindingResource::TextureView(v0),
                            },
                            wgpu::BindGroupEntry {
                                binding: 3,
                                resource: wgpu::BindingResource::TextureView(v1),
                            },
                            wgpu::BindGroupEntry {
                                binding: 4,
                                resource: wgpu::BindingResource::Sampler(sampler),
                            },
                        ],
                    });
                    {
                        let mut pass = begin_layer_pass(
                            encoder,
                            pool.get(scratch),
                            wgpu::LoadOp::Load,
                            wgpu::LoadOp::Load,
                        );
                        pass.set_bind_group(0, Some(res.bind_group), &[]);
                        pass.set_pipeline(&ctx.pipelines.affine_tile);
                        pass.set_bind_group(1, Some(&affine_bg), &[]);
                        pass.set_vertex_buffer(0, tvb.slice(..));
                        pass.set_index_buffer(tib.slice(..), wgpu::IndexFormat::Uint32);
                        pass.set_stencil_reference(*clip_ref);
                        pass.draw_indexed(
                            range.first_index..range.first_index + range.index_count,
                            range.base_vertex,
                            0..1,
                        );
                    }
                    keep_bgs.push(affine_bg);
                }
                if let Some(a) = cur_a_id {
                    budget.charge_pass(1)?;
                    let (c_load, s_load) = if *clear_a {
                        *clear_a = false;
                        (
                            wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                            wgpu::LoadOp::Clear(0),
                        )
                    } else {
                        (wgpu::LoadOp::Load, wgpu::LoadOp::Load)
                    };
                    let mut pass = begin_layer_pass(encoder, pool.get(a), c_load, s_load);
                    let alpha_tile_op = [PageOp::Tile {
                        range: *range,
                        tile_id: *tile_trans,
                        clip_ref: *clip_ref,
                    }];
                    replay_ops(&mut pass, &alpha_tile_op, res);
                }
                *cur_c = scratch;
                if old_c != base {
                    pool.recycle(old_c);
                }
            }
            PageOp::PushBlend {
                mode,
                alpha,
                clips,
                mask,
                overprint,
                non_isolated,
                unclipped_group,
            } => {
                if let Some((parent_c, parent_a, p_clear_c, p_clear_a)) = active.last_mut() {
                    if *non_isolated {
                        flush_clear(encoder, pool.get(*parent_c), p_clear_c, budget)?;
                        if let Some(pa) = *parent_a {
                            flush_clear(encoder, pool.get(pa), p_clear_a, budget)?;
                        }
                    }
                }
                let (parent_c, parent_a, _, _) =
                    *active.last().unwrap_or(&(base, None, false, false));
                let group_clips: &[record::ClipStamp] = if *unclipped_group { &[] } else { clips };
                let g_c = pool.acquire(device)?;
                let (g_a, clear_c, clear_a) = if *non_isolated {
                    init_layer_from_base(
                        ctx,
                        encoder,
                        pool.get(g_c),
                        pool.get(parent_c),
                        group_clips,
                        res,
                        keep_bgs,
                        budget,
                    )?;
                    if *mode != zpdf_display_list::BlendMode::Normal && overprint.is_none() {
                        // Start this group's own accumulated-alpha surface from
                        // transparent so ISO 32000-1 §11.4.5 backdrop removal
                        // reads exact α_g at PopBlend without replaying inner ops.
                        let a = pool.acquire(device)?;
                        if group_clips.is_empty() {
                            (Some(a), false, true)
                        } else {
                            init_layer(
                                encoder,
                                pool.get(a),
                                wgpu::Color::TRANSPARENT,
                                group_clips,
                                res,
                                budget,
                            )?;
                            (Some(a), false, false)
                        }
                    } else if let Some(pa) = parent_a {
                        // Non-isolated Normal group inside a non-isolated
                        // non-Normal ancestor: mirror the ancestor's alpha
                        // layer so `mix(pa, ga, w)` yields exact Porter-Duff
                        // alpha `α_0 + w * α_K * (1 - α_0)` at PopBlend.
                        let a = pool.acquire(device)?;
                        init_layer_from_base(
                            ctx,
                            encoder,
                            pool.get(a),
                            pool.get(pa),
                            group_clips,
                            res,
                            keep_bgs,
                            budget,
                        )?;
                        (Some(a), false, false)
                    } else {
                        (None, false, false)
                    }
                } else if group_clips.is_empty() {
                    // Defer the transparent clear to the first draw pass inside this group.
                    (None, true, false)
                } else {
                    // Isolated (or overprint) group starts from transparent, so
                    // its own color layer already carries exact α_g (`g_a = None`).
                    init_layer(
                        encoder,
                        pool.get(g_c),
                        wgpu::Color::TRANSPARENT,
                        group_clips,
                        res,
                        budget,
                    )?;
                    (None, false, false)
                };
                active.push((g_c, g_a, clear_c, clear_a));
                blend_info.push((
                    *mode,
                    *alpha,
                    mask.as_ref(),
                    clips.clone(),
                    *overprint,
                    *non_isolated,
                ));
            }
            PageOp::PopBlend => {
                if active.len() <= 1 {
                    i += 1;
                    continue;
                }
                let (group_c, group_a, mut g_clear_c, mut g_clear_a) = active.pop().unwrap();
                flush_clear(encoder, pool.get(group_c), &mut g_clear_c, budget)?;
                if let Some(ga) = group_a {
                    flush_clear(encoder, pool.get(ga), &mut g_clear_a, budget)?;
                }
                let (mode, alpha, mask, clips, overprint, non_isolated) =
                    blend_info.pop().unwrap_or((
                        zpdf_display_list::BlendMode::Normal,
                        1.0,
                        None,
                        Vec::new(),
                        None,
                        false,
                    ));
                if let Some((pc, pa, p_clear_c, p_clear_a)) = active.last_mut() {
                    flush_clear(encoder, pool.get(*pc), p_clear_c, budget)?;
                    if let Some(pa_id) = *pa {
                        flush_clear(encoder, pool.get(pa_id), p_clear_a, budget)?;
                    }
                }
                let (parent_c, parent_a, _, _) =
                    *active.last().unwrap_or(&(base, None, false, false));

                if non_isolated
                    && mode != zpdf_display_list::BlendMode::Normal
                    && overprint.is_none()
                {
                    // ISO 32000-1 §11.4.5 / §11.6.7 backdrop removal using the
                    // paired `group_a` layer computed in the same flat pass.
                    let g_removed = if let Some(g_a) = group_a {
                        remove_non_isolated_backdrop(
                            ctx, encoder, pool, group_c, g_a, parent_c, res, keep_bgs, keep_bufs,
                            budget,
                        )?
                    } else {
                        group_c
                    };
                    let g_final = if let Some(m) = mask {
                        apply_soft_mask(
                            ctx, encoder, pool, g_removed, m, None, res, keep_bgs, keep_bufs,
                            budget,
                        )?
                    } else {
                        g_removed
                    };
                    let new_parent_c = composite_layer(
                        ctx,
                        encoder,
                        pool,
                        parent_c,
                        g_final,
                        blend::blend_index(mode),
                        unit(alpha),
                        0,
                        [0.0; 4],
                        &clips,
                        res,
                        keep_bgs,
                        keep_bufs,
                        budget,
                    )?;
                    // Reuse `g_final` (whose alpha IS `α_g * mask`) to update
                    // any enclosing non-isolated ancestor's alpha layer in one
                    // Normal composite pass — zero child replay.
                    let new_parent_a = if let Some(pa) = parent_a {
                        let new_pa = composite_layer(
                            ctx,
                            encoder,
                            pool,
                            pa,
                            g_final,
                            blend::blend_index(zpdf_display_list::BlendMode::Normal),
                            unit(alpha),
                            0,
                            [0.0; 4],
                            &clips,
                            res,
                            keep_bgs,
                            keep_bufs,
                            budget,
                        )?;
                        pool.recycle(pa);
                        Some(new_pa)
                    } else {
                        None
                    };
                    pool.recycle(g_final);
                    active.pop();
                    if parent_c != base {
                        pool.recycle(parent_c);
                    }
                    active.push((new_parent_c, new_parent_a, false, false));
                    i += 1;
                    continue;
                }

                if non_isolated {
                    if let Some(m) = mask {
                        // Render the soft mask ONCE and apply it to both
                        // `group_c` and (when active) `group_a`.
                        let (mask_layer, mask_base) = render_soft_mask_layer(
                            ctx, encoder, pool, m, res, keep_bgs, keep_bufs, budget,
                        )?;
                        let new_parent_c = apply_rendered_soft_mask(
                            ctx,
                            encoder,
                            pool,
                            group_c,
                            mask_layer,
                            m,
                            Some((parent_c, unit(alpha), &clips)),
                            res,
                            keep_bgs,
                            keep_bufs,
                            budget,
                        )?;
                        let new_parent_a = match (parent_a, group_a) {
                            (Some(pa), Some(ga)) => {
                                let new_pa = apply_rendered_soft_mask(
                                    ctx,
                                    encoder,
                                    pool,
                                    ga,
                                    mask_layer,
                                    m,
                                    Some((pa, unit(alpha), &clips)),
                                    res,
                                    keep_bgs,
                                    keep_bufs,
                                    budget,
                                )?;
                                pool.recycle(pa);
                                Some(new_pa)
                            }
                            (pa, ga) => {
                                if let Some(ga) = ga {
                                    pool.recycle(ga);
                                }
                                pa
                            }
                        };
                        pool.recycle(mask_layer);
                        if mask_base != mask_layer {
                            pool.recycle(mask_base);
                        }
                        active.pop();
                        if parent_c != base {
                            pool.recycle(parent_c);
                        }
                        active.push((new_parent_c, new_parent_a, false, false));
                        i += 1;
                        continue;
                    }

                    let new_parent_c = composite_layer(
                        ctx,
                        encoder,
                        pool,
                        parent_c,
                        group_c,
                        LERP_ALPHA_MODE,
                        unit(alpha),
                        0,
                        [0.0; 4],
                        &clips,
                        res,
                        keep_bgs,
                        keep_bufs,
                        budget,
                    )?;
                    pool.recycle(group_c);
                    let new_parent_a = match (parent_a, group_a) {
                        (Some(pa), Some(ga)) => {
                            let new_pa = composite_layer(
                                ctx,
                                encoder,
                                pool,
                                pa,
                                ga,
                                LERP_ALPHA_MODE,
                                unit(alpha),
                                0,
                                [0.0; 4],
                                &clips,
                                res,
                                keep_bgs,
                                keep_bufs,
                                budget,
                            )?;
                            pool.recycle(ga);
                            pool.recycle(pa);
                            Some(new_pa)
                        }
                        (pa, ga) => {
                            if let Some(ga) = ga {
                                pool.recycle(ga);
                            }
                            pa
                        }
                    };
                    active.pop();
                    if parent_c != base {
                        pool.recycle(parent_c);
                    }
                    active.push((new_parent_c, new_parent_a, false, false));
                    i += 1;
                    continue;
                }

                // Isolated or overprint group (`group_a` is `None`).
                if let Some(ga) = group_a {
                    pool.recycle(ga);
                }
                let group = if let Some(m) = mask {
                    apply_soft_mask(
                        ctx, encoder, pool, group_c, m, None, res, keep_bgs, keep_bufs, budget,
                    )?
                } else {
                    group_c
                };
                let (id, comp_alpha, op_active, cmyk) = match overprint {
                    Some(op) => (OVERPRINT_MODE, 1.0f32, op.active as u32, op.cmyk.map(unit)),
                    None => (blend::blend_index(mode), unit(alpha), 0u32, [0.0; 4]),
                };
                let new_parent_c = composite_layer(
                    ctx, encoder, pool, parent_c, group, id, comp_alpha, op_active, cmyk, &clips,
                    res, keep_bgs, keep_bufs, budget,
                )?;
                let new_parent_a = if let Some(pa) = parent_a {
                    let new_pa = composite_layer(
                        ctx,
                        encoder,
                        pool,
                        pa,
                        group,
                        blend::blend_index(zpdf_display_list::BlendMode::Normal),
                        comp_alpha,
                        0,
                        [0.0; 4],
                        &clips,
                        res,
                        keep_bgs,
                        keep_bufs,
                        budget,
                    )?;
                    pool.recycle(pa);
                    Some(new_pa)
                } else {
                    None
                };
                pool.recycle(group);
                active.pop();
                if parent_c != base {
                    pool.recycle(parent_c);
                }
                active.push((new_parent_c, new_parent_a, false, false));
            }
            _ => unreachable!(),
        }
        i += 1;
    }

    while active.len() > 1 {
        if let Some((c, a, _, _)) = active.pop() {
            if c != base {
                pool.recycle(c);
            }
            if let Some(a) = a {
                pool.recycle(a);
            }
        }
    }
    Ok(active.first().map_or(base, |&(c, _, _, _)| c))
}

/// Uniform for the mask-apply pass. Mirrors `MaskU` in `mask_apply.wgsl`: the
/// coverage kind, the device-pixel sampling offset, the /BC backdrop luminosity
/// (used where the offset reads outside the built mask), and the /TR transfer LUT
/// packed 4 bytes per `u32` (`array<vec4<u32>, 16>` on the shader side).
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct MaskUniform {
    kind: u32,
    dx: i32,
    dy: i32,
    non_isolated: u32,
    backdrop_luma: f32,
    group_alpha: f32,
    _pad2: f32,
    _pad3: f32,
    lut: [u32; 64],
}

/// Pack a /TR transfer LUT (identity when absent) into 256 bytes, 4 per `u32`.
fn pack_transfer_lut(transfer: Option<&std::sync::Arc<[u8; 256]>>) -> [u32; 64] {
    let mut lut = [0u32; 64];
    for (i, slot) in lut.iter_mut().enumerate() {
        let mut word = 0u32;
        for byte in 0..4 {
            let v = match transfer {
                Some(t) => t[i * 4 + byte],
                None => (i * 4 + byte) as u8, // identity
            };
            word |= (v as u32) << (8 * byte);
        }
        *slot = word;
    }
    lut
}

/// ISO 32000-1 §11.4.5 non-isolated group backdrop removal: given `g_color`
/// evaluated over `parent` and `g_alpha` evaluated over transparent, write the
/// backdrop-removed premultiplied group result `(c_g * α_g, α_g)` into a fresh
/// layer using `mask_apply` (`non_isolated = 2u`), and recycle `g_color` and
/// `g_alpha`.
#[allow(clippy::too_many_arguments)]
fn remove_non_isolated_backdrop(
    ctx: &GpuContext,
    encoder: &mut wgpu::CommandEncoder,
    pool: &mut blend::LayerPool,
    g_color: usize,
    g_alpha: usize,
    parent: usize,
    res: &ReplayRes,
    keep_bgs: &mut Vec<wgpu::BindGroup>,
    keep_bufs: &mut Vec<wgpu::Buffer>,
    budget: &mut EncodeBudget,
) -> Result<usize, WgpuRenderError> {
    use wgpu::util::DeviceExt;
    budget.charge_pass(1)?;
    let device = &ctx.device;
    let out = pool.acquire(device)?;
    let mask_u = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("zpdf-backdrop-remove-uniform"),
        contents: bytemuck::bytes_of(&MaskUniform {
            kind: 2,
            dx: 0,
            dy: 0,
            non_isolated: 2,
            backdrop_luma: 0.0,
            group_alpha: 1.0,
            _pad2: 0.0,
            _pad3: 0.0,
            lut: pack_transfer_lut(None),
        }),
        usage: wgpu::BufferUsages::UNIFORM,
    });
    let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("zpdf-backdrop-remove-bg"),
        layout: &ctx.pipelines.mask_apply_bgl,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(pool.get(g_color).sampleable_view()),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(pool.get(g_alpha).sampleable_view()),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: mask_u.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: wgpu::BindingResource::TextureView(pool.get(parent).sampleable_view()),
            },
        ],
    });
    {
        let mut pass = begin_layer_pass(
            encoder,
            pool.get(out),
            wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
            wgpu::LoadOp::Clear(0),
        );
        pass.set_bind_group(0, Some(res.bind_group), &[]);
        if let (Some(fs), Some((svb, sib))) = (res.fs_range, res.solid) {
            pass.set_pipeline(&ctx.pipelines.mask_apply);
            pass.set_bind_group(1, Some(&bg), &[]);
            pass.set_vertex_buffer(0, svb.slice(..));
            pass.set_index_buffer(sib.slice(..), wgpu::IndexFormat::Uint32);
            pass.set_stencil_reference(0);
            pass.draw_indexed(
                fs.first_index..fs.first_index + fs.index_count,
                fs.base_vertex,
                0..1,
            );
        }
    }
    keep_bgs.push(bg);
    keep_bufs.push(mask_u);
    pool.recycle(g_color);
    pool.recycle(g_alpha);
    Ok(out)
}

/// Composite a soft mask's group into its own layer, returning
/// `(mask_layer, mask_base)` so the caller can apply the rendered coverage once
/// or twice (for paired color/alpha layers) before recycling both.
#[allow(clippy::too_many_arguments)]
fn render_soft_mask_layer(
    ctx: &GpuContext,
    encoder: &mut wgpu::CommandEncoder,
    pool: &mut blend::LayerPool,
    mask: &record::MaskOps,
    res: &ReplayRes,
    keep_bgs: &mut Vec<wgpu::BindGroup>,
    keep_bufs: &mut Vec<wgpu::Buffer>,
    budget: &mut EncodeBudget,
) -> Result<(usize, usize), WgpuRenderError> {
    use zpdf_display_list::SoftMaskKind;
    let device = &ctx.device;
    let mask_base = pool.acquire(device)?;
    let backdrop = quantize_clear_color(&Color::gray(unit(mask.backdrop_luma)));
    let clear = match mask.kind {
        SoftMaskKind::Luminosity => backdrop,
        SoftMaskKind::Alpha => wgpu::Color::TRANSPARENT,
    };
    budget.charge_pass(1)?;
    {
        let pass = begin_layer_pass(
            encoder,
            pool.get(mask_base),
            wgpu::LoadOp::Clear(clear),
            wgpu::LoadOp::Clear(0),
        );
        drop(pass);
    }
    let mask_layer = composite_into(
        ctx, encoder, pool, mask_base, &mask.ops, res, keep_bgs, keep_bufs, budget,
    )?;
    Ok((mask_layer, mask_base))
}

/// Apply an already-rendered `mask_layer` to `group`, recycling `group` (but
/// NOT `mask_layer`, which the caller recycles after all uses complete).
#[allow(clippy::too_many_arguments)]
fn apply_rendered_soft_mask(
    ctx: &GpuContext,
    encoder: &mut wgpu::CommandEncoder,
    pool: &mut blend::LayerPool,
    group: usize,
    mask_layer: usize,
    mask: &record::MaskOps,
    non_isolated: Option<(usize, f32, &[record::ClipStamp])>,
    res: &ReplayRes,
    keep_bgs: &mut Vec<wgpu::BindGroup>,
    keep_bufs: &mut Vec<wgpu::Buffer>,
    budget: &mut EncodeBudget,
) -> Result<usize, WgpuRenderError> {
    use wgpu::util::DeviceExt;
    use zpdf_display_list::SoftMaskKind;
    let device = &ctx.device;
    let backdrop = quantize_clear_color(&Color::gray(unit(mask.backdrop_luma)));
    let (ni_flag, group_alpha, base_layer, restamp_clips) = match non_isolated {
        Some((parent, ga, clips)) => (1u32, ga, parent, Some(clips)),
        None => (0u32, 1.0f32, group, None),
    };
    budget.charge_pass(restamp_clips.map_or(1, |c| c.len().max(1)))?;
    let masked = pool.acquire(device)?;
    let kind_id: u32 = match mask.kind {
        SoftMaskKind::Luminosity => 1,
        SoftMaskKind::Alpha => 2,
    };
    let mask_u = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("zpdf-mask-uniform"),
        contents: bytemuck::bytes_of(&MaskUniform {
            kind: kind_id,
            dx: mask.dx,
            dy: mask.dy,
            non_isolated: ni_flag,
            backdrop_luma: backdrop.r as f32,
            group_alpha,
            _pad2: 0.0,
            _pad3: 0.0,
            lut: pack_transfer_lut(mask.transfer.as_ref()),
        }),
        usage: wgpu::BufferUsages::UNIFORM,
    });
    let mask_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("zpdf-mask-apply-bg"),
        layout: &ctx.pipelines.mask_apply_bgl,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(pool.get(group).sampleable_view()),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(
                    pool.get(mask_layer).sampleable_view(),
                ),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: mask_u.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: wgpu::BindingResource::TextureView(
                    pool.get(base_layer).sampleable_view(),
                ),
            },
        ],
    });
    {
        let mut pass = begin_layer_pass(
            encoder,
            pool.get(masked),
            wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
            wgpu::LoadOp::Clear(0),
        );
        pass.set_bind_group(0, Some(res.bind_group), &[]);
        if let (Some(fs), Some((svb, sib))) = (res.fs_range, res.solid) {
            pass.set_pipeline(&ctx.pipelines.mask_apply);
            pass.set_bind_group(1, Some(&mask_bg), &[]);
            pass.set_vertex_buffer(0, svb.slice(..));
            pass.set_index_buffer(sib.slice(..), wgpu::IndexFormat::Uint32);
            pass.set_stencil_reference(0);
            pass.draw_indexed(
                fs.first_index..fs.first_index + fs.index_count,
                fs.base_vertex,
                0..1,
            );
        }
        if let Some(clips) = restamp_clips {
            stamp_clips(&mut pass, clips, res);
        }
    }
    keep_bgs.push(mask_bg);
    keep_bufs.push(mask_u);
    pool.recycle(group);
    Ok(masked)
}

/// Apply a group's soft mask: composite the mask group into its own layer, then a
/// fullscreen pass pre-multiplies the group layer by the mask's per-pixel coverage
/// (luminosity over the /BC backdrop, or the mask group's alpha), honoring the
/// tiling-pattern reuse offset and the /TR transfer function. Returns the index of
/// the masked layer that takes the group's place as the composite source. New bind
/// groups/buffers are parked in `keep_*` so they outlive the encoder submission.
#[allow(clippy::too_many_arguments)]
fn apply_soft_mask(
    ctx: &GpuContext,
    encoder: &mut wgpu::CommandEncoder,
    pool: &mut blend::LayerPool,
    group: usize,
    mask: &record::MaskOps,
    non_isolated: Option<(usize, f32, &[record::ClipStamp])>,
    res: &ReplayRes,
    keep_bgs: &mut Vec<wgpu::BindGroup>,
    keep_bufs: &mut Vec<wgpu::Buffer>,
    budget: &mut EncodeBudget,
) -> Result<usize, WgpuRenderError> {
    let (mask_layer, mask_base) =
        render_soft_mask_layer(ctx, encoder, pool, mask, res, keep_bgs, keep_bufs, budget)?;
    let masked = apply_rendered_soft_mask(
        ctx,
        encoder,
        pool,
        group,
        mask_layer,
        mask,
        non_isolated,
        res,
        keep_bgs,
        keep_bufs,
        budget,
    )?;
    pool.recycle(mask_layer);
    if mask_base != mask_layer {
        pool.recycle(mask_base);
    }
    Ok(masked)
}

#[cfg(test)]
mod resource_tests {
    use super::*;
    use zpdf_core::Rect;
    use zpdf_display_list::Color;

    #[test]
    fn default_gpu_budget_rejects_multi_gigabyte_targets() {
        let limit = ParseLimits::default().max_gpu_texture_bytes;
        assert!(estimate_page_target_bytes(2_000, 2_000, 4) < limit);
        assert!(estimate_page_target_bytes(16_384, 16_384, 4) > limit);
        assert_eq!(
            estimate_page_target_bytes(u32::MAX, u32::MAX, u32::MAX),
            u64::MAX
        );
    }

    #[test]
    fn with_limits_copies_renderer_budgets() {
        let limits = ParseLimits {
            max_page_pixels: 123,
            max_gpu_texture_bytes: 456,
            max_blend_group_depth: 7,
            max_page_operators: 8,
            max_operand_stack_depth: 9,
            max_gpu_geometry_bytes: 10,
            ..ParseLimits::default()
        };
        let renderer = WgpuRenderer::new().with_limits(&limits);
        assert_eq!(renderer.max_page_pixels, 123);
        assert_eq!(renderer.max_gpu_texture_bytes, 456);
        assert_eq!(renderer.parse_limits.max_blend_group_depth, 7);
        assert_eq!(renderer.parse_limits.max_page_operators, 8);
        assert_eq!(renderer.parse_limits.max_operand_stack_depth, 9);
        assert_eq!(renderer.parse_limits.max_gpu_geometry_bytes, 10);
    }

    #[test]
    fn failed_geometry_admission_discards_the_active_page_without_a_gpu() {
        let limits = ParseLimits {
            max_gpu_geometry_bytes: 0,
            ..ParseLimits::default()
        };
        let mut renderer = WgpuRenderer::new().with_limits(&limits);
        // Recording itself does not require a device. Install only its small
        // page state to exercise the public execute/error-cleanup path.
        renderer.page = Some(PageState {
            width: 1,
            height: 1,
            clear: wgpu::Color {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 1.0,
            },
            map: PageMap::new(Rect::new(0.0, 0.0, 1.0, 1.0), 1.0),
            uniform: PageUniform {
                w_px: 1.0,
                h_px: 1.0,
                scale: 1.0,
                page_height: 1.0,
            },
            recorder: PageRecorder::default().with_limits(&limits),
        });
        let mut path = zpdf_display_list::Path::new();
        path.move_to(zpdf_core::Point::new(0.0, 0.0));
        path.line_to(zpdf_core::Point::new(1.0, 0.0));
        path.line_to(zpdf_core::Point::new(0.0, 1.0));
        path.close();
        let error = renderer
            .execute(&RenderCommand::FillPath {
                path,
                rule: zpdf_display_list::FillRule::NonZero,
                paint: Paint::Solid(Color::black()),
                alpha: 1.0,
                overprint: None,
            })
            .unwrap_err();
        assert!(matches!(error, WgpuRenderError::Unsupported(_)));
        assert!(renderer.page.is_none());
        assert!(renderer.ctx.is_none());
        assert!(matches!(
            renderer.execute(&RenderCommand::PopClip),
            Err(WgpuRenderError::NoActivePage)
        ));
    }

    #[test]
    fn page_pixel_limit_rejects_before_creating_a_gpu_context() {
        let limits = ParseLimits {
            max_page_pixels: 3,
            ..ParseLimits::default()
        };
        let mut renderer = WgpuRenderer::new().with_limits(&limits);
        let err = renderer
            .begin_page(&PageRenderInfo {
                page_rect: Rect::new(0.0, 0.0, 2.0, 2.0),
                scale: 1.0,
                background: Color::white(),
            })
            .unwrap_err();
        assert!(matches!(err, WgpuRenderError::Unsupported(_)));
        assert!(renderer.ctx.is_none());
        assert!(renderer.page.is_none());
    }

    #[test]
    fn gpu_byte_limit_rejects_before_creating_a_gpu_context() {
        let limits = ParseLimits {
            max_gpu_texture_bytes: 0,
            ..ParseLimits::default()
        };
        let mut renderer = WgpuRenderer::new().with_limits(&limits);
        let err = renderer
            .begin_page(&PageRenderInfo {
                page_rect: Rect::new(0.0, 0.0, 1.0, 1.0),
                scale: 1.0,
                background: Color::white(),
            })
            .unwrap_err();
        assert!(matches!(err, WgpuRenderError::Unsupported(_)));
        assert!(renderer.ctx.is_none());
        assert!(renderer.page.is_none());
    }

    #[test]
    fn quantize_clear_color_premultiplies_and_rounds() {
        let expect = |r: u8, g: u8, b: u8, a: u8| wgpu::Color {
            r: r as f64 / 255.0,
            g: g as f64 / 255.0,
            b: b as f64 / 255.0,
            a: a as f64 / 255.0,
        };
        assert_eq!(
            quantize_clear_color(&Color::gray(0.5)),
            expect(128, 128, 128, 255)
        );
        assert_eq!(
            quantize_clear_color(&Color::gray(0.7)),
            expect(179, 179, 179, 255)
        );
        assert_eq!(
            quantize_clear_color(&Color::rgba(0.1, 0.0, 0.0, 254.0 / 255.0)),
            expect(25, 0, 0, 254)
        );
        assert_eq!(
            quantize_clear_color(&Color::rgba(1.0, 0.5, 0.0, 0.5)),
            expect(128, 64, 0, 128)
        );
        assert_eq!(
            quantize_clear_color(&Color::rgba(1.0, 0.5, 0.25, 0.0)),
            expect(0, 0, 0, 0)
        );
        // Clamp RGB and alpha independently before premultiplying.
        assert_eq!(
            quantize_clear_color(&Color::rgba(2.0, -1.0, 0.5, 0.5)),
            expect(128, 0, 64, 128)
        );
        assert_eq!(
            quantize_clear_color(&Color::rgba(0.5, 0.5, 0.5, 2.0)),
            expect(128, 128, 128, 255)
        );
        assert_eq!(
            quantize_clear_color(&Color::rgba(f32::NAN, 0.5, f32::INFINITY, 1.0)),
            expect(0, 128, 255, 255)
        );
    }

    #[test]
    fn tiling_overprint_wraps_active_and_skips_inactive() {
        let mut cell = zpdf_display_list::DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0));
        let mut sq = zpdf_display_list::Path::new();
        sq.rect(Rect::new(0.0, 0.0, 5.0, 5.0));
        cell.push(RenderCommand::FillPath {
            path: sq,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
        });
        let tiling = zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 10.0, 10.0),
            x_step: 10.0,
            y_step: 10.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        };
        let mut fill_rect = zpdf_display_list::Path::new();
        fill_rect.rect(Rect::new(0.0, 0.0, 20.0, 20.0));
        let map = PageMap::new(Rect::new(0.0, 0.0, 20.0, 20.0), 1.0);

        // Active overprint must push/pop an overprint blend around the replay.
        let mut rec_active = PageRecorder::default();
        let active_op = zpdf_display_list::Overprint {
            cmyk: [0.0, 0.0, 0.0, 1.0],
            active: zpdf_display_list::Overprint::K,
        };
        let id = zpdf_core::Matrix::identity();
        record_tiling_fill(
            &mut rec_active,
            &fill_rect,
            zpdf_display_list::FillRule::NonZero,
            &tiling,
            &id,
            1.0,
            Some(active_op),
            None,
            None,
            &map,
            0,
        );
        assert!(rec_active.ops.iter().any(|op| matches!(
            op,
            PageOp::PushBlend {
                overprint: Some(p),
                ..
            } if *p == active_op
        )));
        assert!(rec_active
            .ops
            .iter()
            .any(|op| matches!(op, PageOp::PopBlend)));

        // Inactive overprint (active == 0) paints no colorants -> no-op.
        let mut rec_inactive = PageRecorder::default();
        let inactive_op = zpdf_display_list::Overprint {
            cmyk: [0.0; 4],
            active: 0,
        };
        record_tiling_fill(
            &mut rec_inactive,
            &fill_rect,
            zpdf_display_list::FillRule::NonZero,
            &tiling,
            &id,
            1.0,
            Some(inactive_op),
            None,
            None,
            &map,
            0,
        );
        assert!(rec_inactive.ops.is_empty());
    }

    #[test]
    fn tiling_pattern_strict_overlap_excludes_boundary_only_tiles() {
        let mut cell = zpdf_display_list::DisplayList::new(Rect::new(0.0, 0.0, 1.0, 1.0));
        let mut sq = zpdf_display_list::Path::new();
        sq.rect(Rect::new(0.0, 0.0, 1.0, 1.0));
        cell.push(RenderCommand::FillPath {
            path: sq,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        let tiling = zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 1.0, 1.0),
            x_step: 1.0,
            y_step: 1.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        };
        // 63×63 = 3,969 strictly overlapping tiles (<= MAX_GPU_TILES = 4,096).
        // Inclusive floor..=ceil would count 65×65 = 4,225 and drop the pattern.
        let mut rec = PageRecorder::default();
        let map = PageMap::new(Rect::new(0.0, 0.0, 63.0, 63.0), 1.0);
        let id = zpdf_core::Matrix::identity();
        record_tiling_pattern(
            &mut rec,
            &tiling,
            &id,
            Rect::new(0.0, 0.0, 63.0, 63.0),
            1.0,
            None,
            None,
            &map,
            0,
        );
        assert!(
            !rec.ops.is_empty(),
            "63x63 strictly overlapping tiles must not be rejected by MAX_GPU_TILES"
        );
        assert!(
            rec.synthetic_tiles.is_empty(),
            "tiles within MAX_GPU_TILES should stay on the vector replay path"
        );
    }

    #[test]
    fn tiling_pattern_above_cap_records_repeating_synthetic_tile() {
        let mut cell = zpdf_display_list::DisplayList::new(Rect::new(0.0, 0.0, 2.0, 2.0));
        let mut left = zpdf_display_list::Path::new();
        left.rect(Rect::new(0.0, 0.0, 1.0, 2.0));
        cell.push(RenderCommand::FillPath {
            path: left,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        let mut right = zpdf_display_list::Path::new();
        right.rect(Rect::new(1.0, 0.0, 2.0, 2.0));
        cell.push(RenderCommand::FillPath {
            path: right,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        let tiling = zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 2.0, 2.0),
            x_step: 2.0,
            y_step: 2.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        };
        // 65×65 = 4,225 visible tiles (> MAX_GPU_TILES = 4,096).
        let mut rec = PageRecorder::default();
        let map = PageMap::new(Rect::new(0.0, 0.0, 130.0, 130.0), 2.0);
        let id = zpdf_core::Matrix::identity();
        record_tiling_pattern(
            &mut rec,
            &tiling,
            &id,
            Rect::new(0.0, 0.0, 130.0, 130.0),
            1.0,
            None,
            None,
            &map,
            0,
        );
        assert_eq!(
            rec.synthetic_tiles.len(),
            1,
            "65x65 dense pattern must record one rasterized cell texture"
        );
        assert_eq!(
            (rec.synthetic_tiles[0].width, rec.synthetic_tiles[0].height),
            (4, 4)
        );
        assert!(
            rec.ops
                .iter()
                .any(|op| matches!(op, PageOp::Tile { tile_id: 0, .. })),
            "dense pattern must emit a repeating PageOp::Tile draw instead of returning blank"
        );
        // A second fill with the same dense pattern must reuse the cached synthetic tile.
        record_tiling_pattern(
            &mut rec,
            &tiling,
            &id,
            Rect::new(0.0, 0.0, 130.0, 130.0),
            0.5,
            None,
            None,
            &map,
            0,
        );
        assert_eq!(
            rec.synthetic_tiles.len(),
            1,
            "repeated dense fills should reuse the cached synthetic tile"
        );
        assert_eq!(
            rec.ops
                .iter()
                .filter(|op| matches!(op, PageOp::Tile { tile_id: 0, .. }))
                .count(),
            2
        );
    }

    #[test]
    fn build_mask_ops_applies_linear_ctm_to_soft_mask_offset_without_cloning() {
        let mut mask_dl = zpdf_display_list::DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0));
        let mut sq = zpdf_display_list::Path::new();
        sq.rect(Rect::new(0.0, 0.0, 5.0, 5.0));
        mask_dl.push(RenderCommand::FillPath {
            path: sq,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        let mask_arc = std::sync::Arc::new(mask_dl);
        let mask = zpdf_display_list::SoftMask {
            kind: zpdf_display_list::SoftMaskKind::Luminosity,
            commands: std::sync::Arc::clone(&mask_arc),
            offset: (2.0, 3.0),
            backdrop_luma: 0.0,
            transfer: None,
        };
        let ctm = zpdf_core::Matrix::new(0.0, 2.0, -3.0, 0.0, 100.0, 200.0);
        let mut rec = PageRecorder::default();
        let map = PageMap::new(Rect::new(0.0, 0.0, 400.0, 400.0), 1.0);
        let ops = build_mask_ops(&mut rec, &mask, &ctm, None, None, &map, 0);
        // Linear part of ctm applied to (2, 3): (0*2 + -3*3, 2*2 + 0*3) = (-9, 4).
        // Device Y flips sign: (dx, dy) = (-9, -4).
        assert_eq!((ops.dx, ops.dy), (-9, -4));
        assert!(!ops.ops.is_empty());
        assert_eq!(
            std::sync::Arc::strong_count(&mask_arc),
            2,
            "streaming mask recording must not retain transformed DisplayList clones"
        );
    }

    #[test]
    fn tiling_cell_shared_soft_mask_streams_under_geometry_budget_without_cloning() {
        // Build one shared soft-mask Arc<DisplayList> containing 32 filled rects,
        // referenced by 50 marks inside a single tiling cell.
        let mut mask_dl = zpdf_display_list::DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0));
        for k in 0..32 {
            let mut r = zpdf_display_list::Path::new();
            r.rect(Rect::new(k as f64 * 0.25, 0.0, 0.2, 10.0));
            mask_dl.push(RenderCommand::FillPath {
                path: r,
                rule: zpdf_display_list::FillRule::NonZero,
                paint: Paint::Solid(Color::white()),
                alpha: 1.0,
                overprint: None,
            });
        }
        let shared_mask = std::sync::Arc::new(mask_dl);
        let mut cell = zpdf_display_list::DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0));
        for _ in 0..50 {
            cell.push(RenderCommand::PushBlendGroup {
                blend_mode: zpdf_display_list::BlendMode::Normal,
                isolated: true,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
                alpha: 1.0,
                mask: Some(zpdf_display_list::SoftMask {
                    kind: zpdf_display_list::SoftMaskKind::Luminosity,
                    commands: std::sync::Arc::clone(&shared_mask),
                    offset: (0.0, 0.0),
                    backdrop_luma: 0.0,
                    transfer: None,
                }),
            });
            let mut r = zpdf_display_list::Path::new();
            r.rect(Rect::new(0.0, 0.0, 10.0, 10.0));
            cell.push(RenderCommand::FillPath {
                path: r,
                rule: zpdf_display_list::FillRule::NonZero,
                paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
                alpha: 1.0,
                overprint: None,
            });
            cell.push(RenderCommand::PopBlendGroup);
        }
        let tiling = zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 10.0, 10.0),
            x_step: 10.0,
            y_step: 10.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        };
        // Tight geometry budget allows only a handful of rectangles before latching an error.
        let limits = ParseLimits {
            max_gpu_geometry_bytes: 4_096,
            ..ParseLimits::default()
        };
        let mut rec = PageRecorder::default().with_limits(&limits);
        let map = PageMap::new(Rect::new(0.0, 0.0, 10.0, 10.0), 1.0);
        let id = zpdf_core::Matrix::identity();
        record_tiling_pattern(
            &mut rec,
            &tiling,
            &id,
            Rect::new(0.0, 0.0, 10.0, 10.0),
            1.0,
            None,
            None,
            &map,
            0,
        );
        assert!(
            rec.ensure_within_budget().is_err(),
            "tight max_gpu_geometry_bytes must stop streaming immediately"
        );
        // Only the local `shared_mask` plus the 50 original cell entries hold the Arc.
        assert_eq!(std::sync::Arc::strong_count(&shared_mask), 51);
        // Geometry stopped promptly near the 4 KiB budget rather than recording all 50*32 rects.
        assert!(
            rec.vertices.len() < 200,
            "recorder should abort promptly at budget, got {} vertices",
            rec.vertices.len()
        );
    }

    #[test]
    fn dense_backdrop_dependent_tiling_emits_blend_composite_op() {
        let mut cell = zpdf_display_list::DisplayList::new(Rect::new(0.0, 0.0, 2.0, 2.0));
        cell.push(RenderCommand::PushBlendGroup {
            blend_mode: zpdf_display_list::BlendMode::Multiply,
            isolated: true,
            knockout: false,
            bounds: Rect::new(0.0, 0.0, 2.0, 2.0),
            alpha: 1.0,
            mask: None,
        });
        let mut sq = zpdf_display_list::Path::new();
        sq.rect(Rect::new(0.0, 0.0, 2.0, 2.0));
        cell.push(RenderCommand::FillPath {
            path: sq,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        cell.push(RenderCommand::PopBlendGroup);
        let tiling = zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 2.0, 2.0),
            x_step: 2.0,
            y_step: 2.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        };
        // 65×65 = 4,225 tiles (> MAX_GPU_TILES) -> dense synthetic tile path.
        let mut rec = PageRecorder::default();
        let map = PageMap::new(Rect::new(0.0, 0.0, 130.0, 130.0), 1.0);
        let id = zpdf_core::Matrix::identity();
        record_tiling_pattern(
            &mut rec,
            &tiling,
            &id,
            Rect::new(0.0, 0.0, 130.0, 130.0),
            1.0,
            None,
            None,
            &map,
            0,
        );
        assert_eq!(rec.synthetic_tiles.len(), 1);
        assert!(
            rec.ops.iter().any(|op| matches!(
                op,
                PageOp::PushBlend {
                    mode: zpdf_display_list::BlendMode::Multiply,
                    ..
                }
            )),
            "dense Multiply cell must composite its tile against the page backdrop with Multiply"
        );
        assert!(rec
            .ops
            .iter()
            .any(|op| matches!(op, PageOp::Tile { tile_id: 0, .. })));
        assert!(rec.ops.iter().any(|op| matches!(op, PageOp::PopBlend)));
    }

    #[test]
    fn translucent_dense_overlapping_multiply_wraps_once_in_non_isolated_opacity() {
        let mut cell = zpdf_display_list::DisplayList::new(Rect::new(0.0, 0.0, 2.0, 2.0));
        for mode in [
            zpdf_display_list::BlendMode::Multiply,
            zpdf_display_list::BlendMode::Darken,
        ] {
            cell.push(RenderCommand::PushBlendGroup {
                blend_mode: mode,
                isolated: true,
                knockout: false,
                bounds: Rect::new(0.0, 0.0, 2.0, 2.0),
                alpha: 1.0,
                mask: None,
            });
            let mut sq = zpdf_display_list::Path::new();
            sq.rect(Rect::new(0.0, 0.0, 2.0, 2.0));
            cell.push(RenderCommand::FillPath {
                path: sq,
                rule: zpdf_display_list::FillRule::NonZero,
                paint: Paint::Solid(Color::black()),
                alpha: 1.0,
                overprint: None,
            });
            cell.push(RenderCommand::PopBlendGroup);
        }
        let tiling = zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 2.0, 2.0),
            x_step: 2.0,
            y_step: 2.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        };
        let mut rec = PageRecorder::default();
        let map = PageMap::new(Rect::new(0.0, 0.0, 130.0, 130.0), 1.0);
        let id = zpdf_core::Matrix::identity();
        record_tiling_pattern(
            &mut rec,
            &tiling,
            &id,
            Rect::new(0.0, 0.0, 130.0, 130.0),
            0.5,
            None,
            None,
            &map,
            0,
        );
        // Outer wrapper applies 0.5 opacity ONCE non-isolated; both inner Multiply
        // segments composite at full opacity 1.0 against the preserved backdrop.
        assert!(matches!(
            rec.ops.first(),
            Some(PageOp::PushBlend {
                non_isolated: true,
                alpha,
                ..
            }) if (*alpha - 0.5).abs() < 1e-6
        ));
        let seg_alphas: Vec<f32> =
            rec.ops
                .iter()
                .filter_map(|op| match op {
                    PageOp::PushBlend {
                        mode:
                            zpdf_display_list::BlendMode::Multiply
                            | zpdf_display_list::BlendMode::Darken,
                        alpha,
                        non_isolated: false,
                        ..
                    } => Some(*alpha),
                    _ => None,
                })
                .collect();
        assert_eq!(seg_alphas, vec![1.0, 1.0]);
    }

    #[test]
    fn encode_budget_enforces_pass_and_op_limits() {
        let mut b = EncodeBudget::new(2, 5);
        assert!(b.charge_pass(2).is_ok());
        assert_eq!(b.passes_used, 1);
        assert!(b.charge_pass(3).is_ok());
        assert_eq!(b.passes_used, 2);
        assert!(matches!(
            b.charge_pass(1),
            Err(WgpuRenderError::Unsupported(_))
        ));

        let mut op_limited = EncodeBudget::new(10, 4);
        assert!(op_limited.charge_pass(3).is_ok());
        assert!(matches!(
            op_limited.charge_pass(2),
            Err(WgpuRenderError::Unsupported(_))
        ));
    }

    #[test]
    fn nested_non_isolated_groups_encode_linearly_and_enforce_encode_budget() {
        // 15 nested non-isolated Screen groups plus 1 inner Multiply group
        // (= the full default max_blend_group_depth = 16): recursive
        // color+alpha replay would encode 2^15 = 32,768 child subtrees; paired
        // alpha-layer tracking encodes linearly (< 160 passes total) and still
        // obeys `max_encode_passes`.
        let page_rect = Rect::new(0.0, 0.0, 32.0, 32.0);
        let mut dl = DisplayList::new(page_rect);
        let mut full = zpdf_display_list::Path::new();
        full.rect(page_rect);
        dl.push(RenderCommand::FillPath {
            path: full.clone(),
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        const DEPTH: usize = 15;
        for _ in 0..DEPTH {
            dl.push(RenderCommand::PushBlendGroup {
                blend_mode: zpdf_display_list::BlendMode::Screen,
                isolated: false,
                knockout: false,
                bounds: page_rect,
                alpha: 1.0,
                mask: None,
            });
        }
        // White Multiply fill inside the innermost non-isolated group evaluates
        // against the blue backdrop -> blue, screened 16 times onto blue -> blue.
        dl.push(RenderCommand::PushBlendGroup {
            blend_mode: zpdf_display_list::BlendMode::Multiply,
            isolated: true,
            knockout: false,
            bounds: page_rect,
            alpha: 1.0,
            mask: None,
        });
        dl.push(RenderCommand::FillPath {
            path: full,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::white()),
            alpha: 1.0,
            overprint: None,
        });
        dl.push(RenderCommand::PopBlendGroup);
        for _ in 0..DEPTH {
            dl.push(RenderCommand::PopBlendGroup);
        }

        let mut renderer = WgpuRenderer::new();
        let tex = match renderer.render_display_list(&dl, 1.0) {
            Ok(t) => t,
            Err(WgpuRenderError::NoAdapter) => return,
            Err(e) => panic!("unexpected GPU error: {e}"),
        };
        assert!(
            renderer.last_encode_passes > 0 && renderer.last_encode_passes < 160,
            "16 nested non-isolated groups must encode linearly (< 160 passes), got {}",
            renderer.last_encode_passes
        );
        let off = ((16 * tex.width + 16) * 4) as usize;
        let px = &tex.data[off..off + 4];
        assert!(
            px[0] < 10 && px[1] < 10 && px[2] > 245 && px[3] > 245,
            "16 nested non-isolated Screen groups over blue with white Multiply must stay blue, got {px:?}"
        );

        // Tightening the shared pass budget must return a clean render error
        // from `end_page` rather than submitting unbounded passes.
        renderer.max_encode_passes = 8;
        let err = renderer
            .render_display_list(&dl, 1.0)
            .err()
            .expect("expected encoding-budget error");
        assert!(
            matches!(err, WgpuRenderError::Unsupported(_)),
            "expected Unsupported budget error, got {err:?}"
        );
    }

    #[test]
    fn dense_non_affine_copy_boundary_exhaustion_sets_recording_error() {
        // 128 overlapping copies × 128 non-affine Darken segments = 16,384
        // (exact pass budget) must succeed; adding a 129th overlapping copy
        // must hit copy 129 in `emit_steps` and latch a recording error rather
        // than returning a silent partial recording.
        let run_case = |copies: usize| -> Result<(), WgpuRenderError> {
            let bbox_w = (copies * 2) as f64;
            let mut cell = DisplayList::new(Rect::new(0.0, 0.0, bbox_w, 2.0));
            let mut span = zpdf_display_list::Path::new();
            span.rect(Rect::new(0.0, 0.0, bbox_w, 2.0));
            for _ in 0..128 {
                cell.push(RenderCommand::PushBlendGroup {
                    blend_mode: zpdf_display_list::BlendMode::Darken,
                    isolated: true,
                    knockout: false,
                    bounds: Rect::new(0.0, 0.0, bbox_w, 2.0),
                    alpha: 1.0,
                    mask: None,
                });
                cell.push(RenderCommand::FillPath {
                    path: span.clone(),
                    rule: zpdf_display_list::FillRule::NonZero,
                    paint: Paint::Solid(Color::gray(0.5)),
                    alpha: 1.0,
                    overprint: None,
                });
                cell.push(RenderCommand::PopBlendGroup);
            }
            let tiling = std::sync::Arc::new(zpdf_display_list::TilingPattern {
                commands: std::sync::Arc::new(cell),
                bbox: Rect::new(0.0, 0.0, bbox_w, 2.0),
                x_step: 2.0,
                y_step: 2.0,
                matrix: zpdf_core::Matrix::identity(),
                tiling_type: 1,
            });
            // 140×140 with 2×2 step = 4,900 > 4,096 tiles -> rasterized path.
            let page_rect = Rect::new(0.0, 0.0, 140.0, 140.0);
            let map = PageMap::new(page_rect, 1.0);
            let limits = zpdf_core::ParseLimits {
                max_gpu_texture_bytes: 256 * 1024 * 1024,
                ..zpdf_core::ParseLimits::default()
            };
            let mut rec = PageRecorder::default()
                .with_limits(&limits)
                .with_page_rect(page_rect)
                .with_opaque_canvas(true);
            record_tiling_pattern_rasterized(
                &mut rec,
                &tiling,
                &zpdf_core::Matrix::identity(),
                page_rect,
                zpdf_core::Matrix::identity(),
                1.0,
                None,
                None,
                &map,
                0,
            );
            rec.ensure_within_budget()
        };

        assert!(
            run_case(128).is_ok(),
            "exact 16,384 non-affine passes (128×128) must succeed without false exhaustion"
        );
        let err = run_case(129).expect_err("129th copy past 16,384 passes must report exhaustion");
        assert!(
            matches!(err, WgpuRenderError::Unsupported(_)),
            "expected Unsupported recording-budget error, got {err:?}"
        );
    }

    #[test]
    fn patterned_glyph_run_honors_expired_recording_deadline() {
        // A long patterned glyph run must immediately observe an expired
        // PageRecorder deadline without pre-parsing glyph outlines for all
        // occurrences.
        let ttf = include_bytes!("../../zpdf-font/tests/fixtures/var.ttf");
        let font = zpdf_font::LoadedFont::new_with_data(
            zpdf_font::PdfFontType::TrueType,
            "ZpdfSans".into(),
            ttf.to_vec(),
            zpdf_font::CidWidths::new(500.0),
        );
        let mut fonts = zpdf_font::FontCache::new();
        let font_id = fonts.insert("F1".into(), font);

        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 10.0, 10.0));
        let mut sq = zpdf_display_list::Path::new();
        sq.rect(Rect::new(0.0, 0.0, 10.0, 10.0));
        cell.push(RenderCommand::FillPath {
            path: sq,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::black()),
            alpha: 1.0,
            overprint: None,
        });
        let tiling = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 10.0, 10.0),
            x_step: 10.0,
            y_step: 10.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });

        let glyphs = vec![
            zpdf_display_list::PositionedGlyph {
                glyph_id: 1,
                x: 0.0,
                y: 0.0,
                code: b'A' as u32,
                code_len: 1,
                advance: 12.0,
            };
            50_000
        ];
        let run = zpdf_display_list::GlyphRun {
            font_id,
            font_size: 24.0,
            transform: zpdf_core::Matrix::translate(10.0, 10.0),
            glyphs,
            paint: Paint::Tiling(std::sync::Arc::clone(&tiling)),
            alpha: 1.0,
            h_scale: 1.0,
            overprint: None,
        };

        let page_rect = Rect::new(0.0, 0.0, 200.0, 200.0);
        let map = PageMap::new(page_rect, 1.0);
        let mut rec = PageRecorder::default()
            .with_limits(&zpdf_core::ParseLimits::default())
            .with_page_rect(page_rect)
            .with_deadline(zpdf_core::time::Instant::now());
        let start = zpdf_core::time::Instant::now();
        record_tiling_glyph_run(
            &mut rec,
            &run,
            &tiling,
            &zpdf_core::Matrix::identity(),
            &fonts,
            None,
            &map,
            0,
        );
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "expired deadline must bail out immediately without walking 50,000 glyphs, took {elapsed:?}"
        );
        assert!(rec.ops.is_empty(), "expired run must not emit ops");
        assert!(
            rec.ensure_within_budget().is_err(),
            "expired run must latch budget error"
        );
    }

    #[test]
    fn type3_bitmap_glyph_records_inline_image_mask_in_tiling_coverage_pass() {
        let glyph_stream = b"BI /W 8 /H 8 /BPC 1 /IM true ID \x00\x00\x00\x00\xff\xff\xff\xff EI";
        let font = zpdf_font::LoadedFont::new_with_data(
            zpdf_font::PdfFontType::Type3 {
                font_matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
                char_procs: std::collections::HashMap::from([(
                    "A".into(),
                    std::sync::Arc::<[u8]>::from(&glyph_stream[..]),
                )]),
                encoding: vec!["A".into()],
                widths: vec![1.0],
                first_char: 0,
            },
            "Type3BitmapTest".into(),
            Vec::new(),
            zpdf_font::CidWidths::new(1.0),
        );
        let mut fonts = zpdf_font::FontCache::new();
        let font_id = fonts.insert("F1".into(), font);

        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 4.0, 4.0));
        let mut sq = zpdf_display_list::Path::new();
        sq.rect(Rect::new(0.0, 0.0, 4.0, 4.0));
        cell.push(RenderCommand::FillPath {
            path: sq,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(Color::rgb(1.0, 0.0, 0.0)),
            alpha: 1.0,
            overprint: None,
        });
        let tiling = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 4.0, 4.0),
            x_step: 4.0,
            y_step: 4.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });

        let run = zpdf_display_list::GlyphRun {
            font_id,
            font_size: 12.0,
            transform: zpdf_core::Matrix::translate(2.0, 2.0),
            glyphs: vec![zpdf_display_list::PositionedGlyph {
                glyph_id: 0,
                x: 0.0,
                y: 0.0,
                code: b'A' as u32,
                code_len: 1,
                advance: 12.0,
            }],
            paint: Paint::Tiling(std::sync::Arc::clone(&tiling)),
            alpha: 1.0,
            h_scale: 1.0,
            overprint: None,
        };

        let page_rect = Rect::new(0.0, 0.0, 20.0, 20.0);
        let map = PageMap::new(page_rect, 1.0);
        let mut rec = PageRecorder::default()
            .with_limits(&zpdf_core::ParseLimits::default())
            .with_page_rect(page_rect);
        record_tiling_glyph_run(
            &mut rec,
            &run,
            &tiling,
            &zpdf_core::Matrix::identity(),
            &fonts,
            None,
            &map,
            0,
        );
        assert!(rec.ensure_within_budget().is_ok());
        assert_eq!(
            rec.type3_images.len(),
            1,
            "bitmap Type 3 glyph must preserve its decoded inline image mask"
        );
        let img = &rec.type3_images[0];
        assert_eq!((img.width, img.height), (8, 8));
        // Top half painted white (alpha=255), bottom half transparent (alpha=0).
        assert_eq!(&img.data[0..4], &[255, 255, 255, 255]);
        assert_eq!(&img.data[img.data.len() - 4..], &[0, 0, 0, 0]);
        let ref_ids = referenced_image_ids(&rec.ops, &rec.synthetic_tiles);
        assert!(
            ref_ids.contains(&record::ImageKey::Cache(record::TYPE3_IMAGE_ID_BASE)),
            "mask ops must reference the recorded Type 3 stencil image id, got {ref_ids:?}"
        );
    }

    #[test]
    fn type3_repeated_glyph_reuses_single_texture_within_budget() {
        // 256×256 1-bit `/IM true` bitmap glyph = 256 KiB RGBA texture.
        // Nine occurrences would exceed a 2 MiB `max_gpu_texture_bytes` cap if
        // each occurrence pushed a duplicate texture; reusing one texture ID
        // lets all nine record cleanly while preserving independent placements.
        let mut glyph_stream = b"BI /W 256 /H 256 /BPC 1 /IM true ID ".to_vec();
        glyph_stream.extend(std::iter::repeat_n(0x00u8, 256 * 32));
        glyph_stream.extend_from_slice(b" EI");
        let font = zpdf_font::LoadedFont::new_with_data(
            zpdf_font::PdfFontType::Type3 {
                font_matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
                char_procs: std::collections::HashMap::from([(
                    "A".into(),
                    std::sync::Arc::<[u8]>::from(glyph_stream),
                )]),
                encoding: vec!["A".into()],
                widths: vec![1.0],
                first_char: 0,
            },
            "Type3Repeated256".into(),
            Vec::new(),
            zpdf_font::CidWidths::new(1.0),
        );
        let mut fonts = zpdf_font::FontCache::new();
        let font_id = fonts.insert("F1".into(), font);

        let glyphs = (0..9)
            .map(|i| zpdf_display_list::PositionedGlyph {
                glyph_id: 0,
                x: i as f32 * 10.0,
                y: 0.0,
                code: b'A' as u32,
                code_len: 1,
                advance: 10.0,
            })
            .collect();
        let run = zpdf_display_list::GlyphRun {
            font_id,
            font_size: 8.0,
            transform: zpdf_core::Matrix::translate(2.0, 2.0),
            glyphs,
            paint: Paint::Solid(zpdf_display_list::Color::rgb(1.0, 0.0, 0.0)),
            alpha: 0.5,
            h_scale: 1.0,
            overprint: None,
        };

        let limits = zpdf_core::ParseLimits {
            max_gpu_texture_bytes: 2 * 1024 * 1024, // 2 MiB (holds at most 8 × 256 KiB)
            ..Default::default()
        };
        let page_rect = Rect::new(0.0, 0.0, 120.0, 20.0);
        let map = PageMap::new(page_rect, 1.0);
        let mut rec = PageRecorder::default()
            .with_limits(&limits)
            .with_page_rect(page_rect);
        glyph::render_glyph_run(&mut rec, &fonts, &map, &run);
        assert!(
            rec.ensure_within_budget().is_ok(),
            "9 occurrences reusing 1 texture must fit 2 MiB texture quota"
        );
        assert_eq!(
            rec.type3_images.len(),
            1,
            "all 9 occurrences must share a single Type 3 texture entry"
        );
        let image_ops: Vec<record::ImageKey> = rec
            .ops
            .iter()
            .filter_map(|op| match op {
                record::PageOp::Image { image_id, .. } => Some(*image_id),
                _ => None,
            })
            .collect();
        assert_eq!(image_ops.len(), 9);
        assert!(
            image_ops
                .iter()
                .all(|&id| id == record::ImageKey::Cache(record::TYPE3_IMAGE_ID_BASE)),
            "all 9 quads must reference the same cached texture ID"
        );
        // Verify distinct quad X positions and preserved per-quad opacity (128/255 ≈ 0.5).
        assert!(
            (rec.tex_vertices[4].pos[0] - rec.tex_vertices[0].pos[0] - 10.0).abs() < 0.01,
            "second occurrence must stay translated by 10pt"
        );
        assert!(
            (rec.tex_vertices[0].color[3] - 128.0 / 255.0).abs() < 1e-3,
            "expected quantized 0.5 opacity on quad, got {}",
            rec.tex_vertices[0].color[3]
        );
        // A second run with the same RGB tint at a different opacity (0.25)
        // must still reuse the same cached texture ID with its own quad alpha.
        let mut run2 = run;
        run2.glyphs.truncate(1);
        run2.alpha = 0.25;
        glyph::render_glyph_run(&mut rec, &fonts, &map, &run2);
        assert_eq!(
            rec.type3_images.len(),
            1,
            "same stencil tint at a different opacity must reuse the same texture"
        );
        let last_v = rec.tex_vertices.last().unwrap();
        assert!(
            (last_v.color[3] - 64.0 / 255.0).abs() < 1e-3,
            "second run quad must carry its own 0.25 opacity, got {}",
            last_v.color[3]
        );
    }

    #[test]
    fn type3_d0_colored_image_under_tiling_paint_keeps_rgb_image_direct() {
        let d0_stream =
            b"1 0 d0 BI /W 2 /H 2 /CS /RGB /BPC 8 ID \xff\x00\x00\xff\x00\x00\xff\x00\x00\xff\x00\x00 EI";
        let font = zpdf_font::LoadedFont::new_with_data(
            zpdf_font::PdfFontType::Type3 {
                font_matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
                char_procs: std::collections::HashMap::from([(
                    "A".into(),
                    std::sync::Arc::<[u8]>::from(&d0_stream[..]),
                )]),
                encoding: vec!["A".into()],
                widths: vec![1.0],
                first_char: 0,
            },
            "Type3ColoredD0Tiling".into(),
            Vec::new(),
            zpdf_font::CidWidths::new(1.0),
        );
        let mut fonts = zpdf_font::FontCache::new();
        let font_id = fonts.insert("F1".into(), font);

        let mut cell = DisplayList::new(Rect::new(0.0, 0.0, 4.0, 4.0));
        let mut r = zpdf_display_list::Path::new();
        r.rect(Rect::new(0.0, 0.0, 4.0, 4.0));
        cell.push(RenderCommand::FillPath {
            path: r,
            rule: zpdf_display_list::FillRule::NonZero,
            paint: Paint::Solid(zpdf_display_list::Color::rgb(0.0, 0.0, 1.0)),
            alpha: 1.0,
            overprint: None,
        });
        let tiling = std::sync::Arc::new(zpdf_display_list::TilingPattern {
            commands: std::sync::Arc::new(cell),
            bbox: Rect::new(0.0, 0.0, 4.0, 4.0),
            x_step: 4.0,
            y_step: 4.0,
            matrix: zpdf_core::Matrix::identity(),
            tiling_type: 1,
        });

        let run = zpdf_display_list::GlyphRun {
            font_id,
            font_size: 12.0,
            transform: zpdf_core::Matrix::translate(2.0, 2.0),
            glyphs: vec![zpdf_display_list::PositionedGlyph {
                glyph_id: 0,
                x: 0.0,
                y: 0.0,
                code: b'A' as u32,
                code_len: 1,
                advance: 12.0,
            }],
            paint: Paint::Tiling(std::sync::Arc::clone(&tiling)),
            alpha: 1.0,
            h_scale: 1.0,
            overprint: None,
        };

        let page_rect = Rect::new(0.0, 0.0, 20.0, 20.0);
        let map = PageMap::new(page_rect, 1.0);
        let mut rec = PageRecorder::default()
            .with_limits(&zpdf_core::ParseLimits::default())
            .with_page_rect(page_rect);
        record_tiling_glyph_run(
            &mut rec,
            &run,
            &tiling,
            &zpdf_core::Matrix::identity(),
            &fonts,
            None,
            &map,
            0,
        );
        assert!(rec.ensure_within_budget().is_ok());
        assert_eq!(rec.type3_images.len(), 1);
        assert_eq!(
            &rec.type3_images[0].data[0..4],
            &[255, 0, 0, 255],
            "d0 colored RGB image must preserve intrinsic red pixels"
        );
        assert!(
            !rec.ops
                .iter()
                .any(|op| matches!(op, record::PageOp::PushBlend { .. })),
            "pure d0 colored image must draw directly without masking through tiling pattern"
        );
        assert!(
            rec.ops.iter().any(|op| matches!(
                op,
                record::PageOp::Image { image_id, .. }
                    if *image_id == record::ImageKey::Cache(record::TYPE3_IMAGE_ID_BASE)
            )),
            "d0 colored image must emit a direct PageOp::Image"
        );
    }

    #[test]
    fn inline_image_straight_alpha_matches_premultiplied_on_gpu() {
        let straight = [128u8, 0, 0, 128];
        let premul = [64u8, 0, 0, 128];
        assert_eq!(
            image::premultiply_rgba_if_needed(&straight, false).as_ref(),
            &premul
        );
        assert_eq!(
            image::premultiply_rgba_if_needed(&premul, true).as_ref(),
            &premul
        );

        let make_dl = |data: Vec<u8>, premultiplied: bool| {
            let mut dl = DisplayList::new(Rect::new(0.0, 0.0, 8.0, 8.0));
            dl.push(RenderCommand::DrawInlineImage(
                zpdf_display_list::InlineImageDraw {
                    image: std::sync::Arc::new(zpdf_display_list::InlineImage {
                        width: 1,
                        height: 1,
                        data,
                        has_alpha: true,
                        is_image_mask: false,
                        premultiplied,
                    }),
                    transform: zpdf_core::Matrix::new(8.0, 0.0, 0.0, 8.0, 0.0, 0.0),
                    alpha: 1.0,
                },
            ));
            dl
        };
        let ctx = match GpuContext::new_headless() {
            Ok(ctx) => ctx,
            Err(WgpuRenderError::NoAdapter) => {
                // Headless environment without a compatible GPU adapter: helper assertion above still ran.
                return;
            }
            Err(err) => panic!("unexpected GPU adapter initialization error: {err}"),
        };
        let mut renderer = WgpuRenderer::new().with_context(ctx);
        let straight_page = renderer
            .render_display_list(&make_dl(straight.to_vec(), false), 1.0)
            .expect("render straight-alpha inline image");
        let premul_page = renderer
            .render_display_list(&make_dl(premul.to_vec(), true), 1.0)
            .expect("render premultiplied inline image");
        let px_idx = ((4 * straight_page.width + 4) * 4) as usize;
        assert_eq!(
            &straight_page.data[px_idx..px_idx + 4],
            &premul_page.data[px_idx..px_idx + 4],
            "straight-alpha and premultiplied inline images must composite identically on white"
        );
        assert!(
            straight_page.data[px_idx] <= 195 && straight_page.data[px_idx] >= 187,
            "straight [128,0,0,128] on white should composite near [191,127,127], got {:?}",
            &straight_page.data[px_idx..px_idx + 4]
        );
    }
}

/// Tracks the currently-bound pipeline to skip redundant `set_pipeline` calls.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Pipe {
    None,
    Solid,
    ClipWrite,
    ClipReset,
    Textured,
    Glyph,
}

/// Tracks which vertex/index buffer pair is bound (solid arena vs textured vs glyph arena).
#[derive(PartialEq, Eq, Clone, Copy)]
enum BufSet {
    None,
    Solid,
    Tex,
    Glyph,
}
