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

pub use context::{GpuContext, COLOR_FORMAT, STENCIL_FORMAT};

use record::{PageOp, PageRecorder};
use target::PageTarget;
use transform::{quantize_premul, PageMap, PageUniform};
use zpdf_display_list::{Paint, RenderCommand};
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
    #[error("buffer readback failed: {0}")]
    Readback(String),
    #[error("device poll failed: {0}")]
    Poll(String),
    #[error("wgpu error: {0}")]
    Wgpu(String),
}

/// Per-page state, alive between `begin_page` and `end_page`.
struct PageState {
    target: PageTarget,
    clear: wgpu::Color,
    /// Page-units -> device-pixel mapping for tessellation.
    map: PageMap,
    /// Uniform for the pixel->NDC shader step.
    uniform: PageUniform,
    /// Accumulated geometry + ordered op list (fills, strokes, clips).
    recorder: PageRecorder,
}

/// GPU renderer. Borrows font/image caches like `CpuRenderer<'a>`.
pub struct WgpuRenderer<'a> {
    ctx: Option<GpuContext>,
    #[allow(dead_code)] // consumed in M6/M7 (glyphs/images)
    font_cache: Option<&'a zpdf_font::FontCache>,
    #[allow(dead_code)] // consumed in M7 (images)
    image_cache: Option<&'a zpdf_image::ImageCache>,
    page: Option<PageState>,
    /// GPU pass time (nanoseconds) from the most recently completed
    /// `end_page()`, or `None` if timestamp queries are unsupported on this
    /// adapter (see `GpuContext::timestamps_supported`) or no page has
    /// rendered yet. Purely informational (P3.8) — never affects rendering.
    last_gpu_time_ns: Option<u64>,
}

impl<'a> WgpuRenderer<'a> {
    pub fn new() -> Self {
        Self {
            ctx: None,
            font_cache: None,
            image_cache: None,
            page: None,
            last_gpu_time_ns: None,
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

    /// Reuse an existing context (e.g. the viewer's surface-bound device).
    pub fn with_context(mut self, ctx: GpuContext) -> Self {
        self.ctx = Some(ctx);
        self
    }

    /// Reclaim the context for reuse across renders.
    pub fn take_context(&mut self) -> Option<GpuContext> {
        self.ctx.take()
    }

    /// GPU pass time (nanoseconds) for the most recently completed page, if
    /// timestamp queries are supported on this adapter (P3.8). `None` before
    /// any page has rendered, or when the adapter lacks the capability.
    pub fn last_gpu_time_ns(&self) -> Option<u64> {
        self.last_gpu_time_ns
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
        if self.ctx.is_none() {
            self.ctx = Some(GpuContext::new_headless()?);
        }
        let ctx = self.ctx.as_ref().unwrap();
        ctx.clear_error();

        let scale = info.scale;
        // ceil() in f64, exactly like the CPU's `(width() * scale as f64).ceil()`
        // (pdfium semantics: a 595x842pt page at 110 DPI is a 910x1287 raster, so
        // no content is sliced off the right/bottom edges; f32 math would round
        // differently at integer boundaries).
        let width = ((info.page_rect.width() * scale as f64).ceil() as u32).max(1);
        let height = ((info.page_rect.height() * scale as f64).ceil() as u32).max(1);
        if width > ctx.max_texture_dim || height > ctx.max_texture_dim {
            return Err(WgpuRenderError::Unsupported(format!(
                "page {width}x{height} exceeds adapter max texture dim {}",
                ctx.max_texture_dim
            )));
        }

        let target = PageTarget::new(&ctx.device, width, height, ctx.sample_count);

        // Pre-quantize the background channel-wise so the cleared bytes equal the
        // CPU's `(c * 255) as u8` background fill byte-for-byte.
        let bg = &info.background;
        let q = |v: f32| (((v * 255.0) as u8) as f64) / 255.0;
        let clear = wgpu::Color {
            r: q(bg.r),
            g: q(bg.g),
            b: q(bg.b),
            a: q(bg.a),
        };

        let map = PageMap::new(info.page_rect, scale);
        let uniform = PageUniform {
            w_px: width as f32,
            h_px: height as f32,
            scale,
            page_height: info.page_rect.height() as f32,
        };

        self.page = Some(PageState {
            target,
            clear,
            map,
            uniform,
            recorder: PageRecorder::default(),
        });
        Ok(())
    }

    fn execute(&mut self, cmd: &RenderCommand) -> Result<(), Self::Error> {
        let fonts = self.font_cache;
        let images = self.image_cache;
        let Some(page) = self.page.as_mut() else {
            return Ok(());
        };
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
                    match run.overprint {
                        Some(op) if op.active != 0 => {
                            page.recorder.push_overprint(op);
                            glyph::render_glyph_run(&mut page.recorder, fonts, &page.map, run);
                            page.recorder.pop_blend();
                        }
                        Some(_) => {}
                        None => glyph::render_glyph_run(&mut page.recorder, fonts, &page.map, run),
                    }
                }
            }
            RenderCommand::DrawImage(draw) => {
                if let Some(images) = self.image_cache {
                    if let Some(img) = images.get(draw.image_id) {
                        let quad = image::image_quad(
                            img.width as f32,
                            img.height as f32,
                            &draw.transform,
                            &page.map,
                            draw.alpha,
                        );
                        page.recorder.add_image(quad, draw.image_id);
                    }
                }
            }
            RenderCommand::PushBlendGroup {
                blend_mode,
                alpha,
                mask,
                ..
            } => {
                // Record the soft mask's geometry into the shared arena (its ops
                // are replayed into an offscreen coverage layer at composite).
                let map = page.map;
                let mask_ops = mask
                    .as_ref()
                    .map(|m| build_mask_ops(&mut page.recorder, m, fonts, images, &map));
                page.recorder.push_blend(*blend_mode, *alpha, mask_ops);
            }
            RenderCommand::PopBlendGroup => {
                page.recorder.pop_blend();
            }
            _ => {}
        }
        Ok(())
    }

    fn end_page(&mut self) -> Result<Self::Target, Self::Error> {
        use wgpu::util::DeviceExt;

        let ctx = self.ctx.as_ref().ok_or(WgpuRenderError::NotInitialized)?;
        let mut page = self.page.take().ok_or(WgpuRenderError::NoActivePage)?;
        let device = &ctx.device;

        // Fullscreen quad: needed for clip rebuild (ResetStencil) and blend composites.
        let needs_fs = page.recorder.uses_reset() || page.recorder.has_blend_groups();
        // Presence of ResetStencil/PushBlend/PopBlend is unaffected by batching
        // (it only merges runs of Draw/Image/StampClip; it never removes or adds
        // an op of a different kind), so this check is valid whether taken
        // before or after `batch::batch_ops` below.
        let has_blend_groups = page.recorder.has_blend_groups();
        let fs_range = if needs_fs {
            Some(
                page.recorder
                    .append_fullscreen(page.uniform.w_px, page.uniform.h_px),
            )
        } else {
            None
        };

        // Collapse consecutive same-state ops into single draw_indexed calls
        // (P3.7). Must happen before `rec` borrows `page.recorder` below (this
        // takes `page.recorder.ops` by value, leaving an empty Vec in its place
        // — irrelevant, since every remaining use goes through `batched_ops`).
        let batched_ops = batch::batch_ops(std::mem::take(&mut page.recorder.ops));

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
        let rec = &page.recorder;
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

        let mut tex_bgs: std::collections::HashMap<u32, wgpu::BindGroup> =
            std::collections::HashMap::new();
        if let Some(images) = self.image_cache {
            let upload =
                |image_id: u32, tex_bgs: &mut std::collections::HashMap<u32, wgpu::BindGroup>| {
                    if tex_bgs.contains_key(&image_id) {
                        return;
                    }
                    if let Some(img) = images.get(image_id) {
                        if img.width == 0
                            || img.height == 0
                            || img.width > ctx.max_texture_dim
                            || img.height > ctx.max_texture_dim
                        {
                            tracing::warn!(
                                "image {image_id} {}x{} skipped (exceeds limits)",
                                img.width,
                                img.height
                            );
                            return;
                        }
                        tex_bgs.insert(image_id, image::upload_image_bind_group(ctx, img));
                    }
                };
            // Walk every op, descending into soft masks at ANY nesting depth: a
            // mask group may itself contain a masked group whose images are
            // recorded into a sub-mask's `ops` (not the flat page op list), so a
            // one-level scan would silently drop them at replay.
            let mut stack: Vec<&[PageOp]> = vec![batched_ops.as_slice()];
            while let Some(ops) = stack.pop() {
                for op in ops {
                    match op {
                        PageOp::Image { image_id, .. } => upload(*image_id, &mut tex_bgs),
                        // Images inside a soft mask reference the same shared arena.
                        PageOp::PushBlend { mask: Some(m), .. } => stack.push(m.ops.as_slice()),
                        _ => {}
                    }
                }
            }
        }

        let res = ReplayRes {
            pipelines: &ctx.pipelines,
            bind_group: &bind_group,
            solid: buffers.as_ref().map(|(v, i)| (v, i)),
            tex: tex_buffers.as_ref().map(|(v, i)| (v, i)),
            tex_bgs: &tex_bgs,
            glyph: glyph_buffers.as_ref().map(|(v, i)| (v, i)),
            glyph_bg: glyph_bg.as_ref(),
            fs_range,
        };

        let timer = timing::GpuTimer::new(ctx);

        // Pages with transparency groups need the multi-pass offscreen-layer path.
        if has_blend_groups {
            let (texture, gpu_ns) =
                render_layered(ctx, &page.target, page.clear, &batched_ops, &res, &timer)?;
            self.last_gpu_time_ns = gpu_ns;
            return Ok(texture);
        }

        // Single-pass path (no blend groups): one render pass into the page target.
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("zpdf-page"),
        });
        timer.write_begin(&mut encoder);
        {
            let color_att = page.target.color_attachment(page.clear);
            let ds_att = page.target.stencil_attachment();
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
        page.target.record_copy(&mut encoder);
        ctx.queue.submit(Some(encoder.finish()));
        let result = page.target.map_and_strip(device);
        self.last_gpu_time_ns = timer.resolve_ns(device);
        if let Some(msg) = ctx.take_error() {
            return Err(WgpuRenderError::Wgpu(format!("device error: {msg}")));
        }
        result
    }
}

/// Shared GPU resources for replaying recorded ops into a render pass.
struct ReplayRes<'a> {
    pipelines: &'a pipelines::Pipelines,
    bind_group: &'a wgpu::BindGroup,
    solid: Option<(&'a wgpu::Buffer, &'a wgpu::Buffer)>,
    tex: Option<(&'a wgpu::Buffer, &'a wgpu::Buffer)>,
    tex_bgs: &'a std::collections::HashMap<u32, wgpu::BindGroup>,
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
            PageOp::PushBlend { .. } | PageOp::PopBlend => {}
        }
    }
}

/// Record a soft mask's [`zpdf_display_list::DisplayList`] into `rec` (geometry
/// into the shared arena, ops collected separately by the caller). A transparency
/// group nested inside the mask is recorded as a `PushBlend`/`PopBlend` pair (with
/// its own sub-mask), so the mask group is composited through the same layered
/// path as the page instead of being dropped. As on the page path, the group's
/// `isolated`/`knockout` flags are not carried into [`record::PageOp::PushBlend`],
/// so a nested knockout / non-isolated group is approximated as isolated source-
/// over — the one residual vs the CPU oracle's full recursive sub-renderer.
fn record_dl_commands(
    rec: &mut PageRecorder,
    dl: &zpdf_display_list::DisplayList,
    fonts: Option<&zpdf_font::FontCache>,
    images: Option<&zpdf_image::ImageCache>,
    map: &PageMap,
) {
    use zpdf_display_list::Paint;
    for cmd in &dl.commands {
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
                        rec.add_fill(path, *rule, color, map);
                        rec.pop_blend();
                    }
                    Some(_) => {}
                    None => rec.add_fill(path, *rule, color, map),
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
                        rec.add_stroke(path, style, color, map);
                        rec.pop_blend();
                    }
                    Some(_) => {}
                    None => rec.add_stroke(path, style, color, map),
                }
            }
            RenderCommand::DrawGlyphRun(run) => {
                if let Some(f) = fonts {
                    match run.overprint {
                        Some(op) if op.active != 0 => {
                            rec.push_overprint(op);
                            glyph::render_glyph_run(rec, f, map, run);
                            rec.pop_blend();
                        }
                        Some(_) => {}
                        None => glyph::render_glyph_run(rec, f, map, run),
                    }
                }
            }
            RenderCommand::DrawImage(draw) => {
                if let Some(img) = images.and_then(|c| c.get(draw.image_id)) {
                    let quad = image::image_quad(
                        img.width as f32,
                        img.height as f32,
                        &draw.transform,
                        map,
                        draw.alpha,
                    );
                    rec.add_image(quad, draw.image_id);
                }
            }
            RenderCommand::PushClip { path, rule } => rec.push_clip(path, *rule, map),
            RenderCommand::PushClipStroke { path, style } => rec.push_clip_stroke(path, style, map),
            RenderCommand::PopClip => rec.pop_clip(),
            // Pattern fills (non-solid paint) are already expanded to clip+fill
            // by the interpreter, so a non-solid paint here is a rare no-op.
            RenderCommand::FillPath { .. } | RenderCommand::StrokePath { .. } => {}
            // A transparency group nested inside the mask: build its sub-mask and
            // record a blend group, recursively (sub-masks compose to any depth).
            RenderCommand::PushBlendGroup {
                blend_mode,
                alpha,
                mask,
                ..
            } => {
                let sub = mask
                    .as_ref()
                    .map(|m| build_mask_ops(rec, m, fonts, images, map));
                rec.push_blend(*blend_mode, *alpha, sub);
            }
            RenderCommand::PopBlendGroup => rec.pop_blend(),
        }
    }
}

/// Build the [`record::MaskOps`] for a group's soft mask: record its group content
/// into the shared arena, pre-scale its page-space `offset` to device pixels, and
/// carry the /TR transfer LUT. Nested groups inside the mask are recorded too, so
/// the mask is always representable (no fall back to rendering the group unmasked).
fn build_mask_ops(
    rec: &mut PageRecorder,
    mask: &zpdf_display_list::SoftMask,
    fonts: Option<&zpdf_font::FontCache>,
    images: Option<&zpdf_image::ImageCache>,
    map: &PageMap,
) -> record::MaskOps {
    let ops = rec.record_subops(|r| {
        record_dl_commands(r, &mask.commands, fonts, images, map);
    });
    // Page-space offset → device pixels; device Y grows downward (mirrors the
    // CPU `shift_plane` deltas), so the Y component is negated.
    let dx = (mask.offset.0 * map.scale).round() as i32;
    let dy = (-mask.offset.1 * map.scale).round() as i32;
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
) {
    let mut pass = begin_layer_pass(
        encoder,
        layer,
        wgpu::LoadOp::Clear(clear),
        wgpu::LoadOp::Clear(0),
    );
    pass.set_bind_group(0, Some(res.bind_group), &[]);
    stamp_clips(&mut pass, clips, res);
}

/// Composite an op list (which may contain `PushBlend`/`PopBlend`) onto `base`,
/// returning the layer index holding the final result. `base` must already be
/// acquired and initialized (cleared, clips stamped); it is never recycled here.
///
/// Shared by the page render (base = the page background) and a soft mask's group
/// (base = the /BC backdrop) so a transparency group nested inside a mask is
/// composited identically to one on the page. Recurses through [`apply_soft_mask`]
/// for sub-masks; `pool`, `encoder`, and the `keep_*` lifetimes are threaded
/// through so every layer/bind-group outlives the single submit.
/// One open transparency/overprint group on the composite stack: its blend
/// mode, group alpha, optional soft mask, inherited clips, and optional
/// overprint descriptor (mutually exclusive with a real blend/mask in practice).
type BlendFrame<'a> = (
    zpdf_display_list::BlendMode,
    f32,
    Option<&'a record::MaskOps>,
    Vec<record::ClipStamp>,
    Option<zpdf_display_list::Overprint>,
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
) -> usize {
    use wgpu::util::DeviceExt;
    let device = &ctx.device;
    let mut active: Vec<usize> = vec![base];
    let mut blend_info: Vec<BlendFrame> = Vec::new();

    let mut i = 0;
    while i < ops.len() {
        let start = i;
        while i < ops.len() && !matches!(ops[i], PageOp::PushBlend { .. } | PageOp::PopBlend) {
            i += 1;
        }
        if start < i {
            let cur = *active.last().unwrap();
            let mut pass = begin_layer_pass(
                encoder,
                pool.get(cur),
                wgpu::LoadOp::Load,
                wgpu::LoadOp::Load,
            );
            replay_ops(&mut pass, &ops[start..i], res);
            drop(pass);
        }
        if i >= ops.len() {
            break;
        }
        match &ops[i] {
            PageOp::PushBlend {
                mode,
                alpha,
                clips,
                mask,
                overprint,
            } => {
                let g = pool.acquire(device);
                init_layer(encoder, pool.get(g), wgpu::Color::TRANSPARENT, clips, res);
                active.push(g);
                blend_info.push((*mode, *alpha, mask.as_ref(), clips.clone(), *overprint));
            }
            PageOp::PopBlend => {
                let group = active.pop().unwrap_or(base);
                let (mode, alpha, mask, clips, overprint) = blend_info.pop().unwrap_or((
                    zpdf_display_list::BlendMode::Normal,
                    1.0,
                    None,
                    Vec::new(),
                    None,
                ));
                let parent = *active.last().unwrap_or(&base);

                // A soft mask pre-multiplies the group layer by per-pixel
                // coverage: render the mask into its own layer, then an
                // apply-mask pass writes (group × coverage) into a fresh layer
                // that takes the group's place as the composite source. The
                // original group + coverage layers are recycled inside.
                let group = if let Some(m) = mask {
                    apply_soft_mask(ctx, encoder, pool, group, m, res, keep_bgs, keep_bufs)
                } else {
                    group
                };

                let scratch = pool.acquire(device);

                // Composite uniform (ModeU, 32 bytes): blend id + group alpha, or
                // the overprint sentinel + source colorants/mask when overprinting.
                const OVERPRINT_MODE: u32 = 100;
                let (id, alpha_bits, op_active, cmyk) = match overprint {
                    Some(op) => (OVERPRINT_MODE, 1.0f32.to_bits(), op.active as u32, op.cmyk),
                    None => (
                        blend::blend_index(mode),
                        alpha.clamp(0.0, 1.0).to_bits(),
                        0u32,
                        [0.0; 4],
                    ),
                };
                let mode_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("zpdf-blend-mode"),
                    contents: bytemuck::cast_slice(&[
                        id,
                        alpha_bits,
                        op_active,
                        0u32,
                        cmyk[0].to_bits(),
                        cmyk[1].to_bits(),
                        cmyk[2].to_bits(),
                        cmyk[3].to_bits(),
                    ]),
                    usage: wgpu::BufferUsages::UNIFORM,
                });
                let comp_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("zpdf-composite-bg"),
                    layout: &ctx.pipelines.composite_bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(
                                pool.get(parent).sampleable_view(),
                            ),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(
                                pool.get(group).sampleable_view(),
                            ),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: mode_buf.as_entire_binding(),
                        },
                    ],
                });

                // Composite pass into the scratch layer, then re-stamp the parent clips.
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
                    stamp_clips(&mut pass, &clips, res);
                }

                keep_bgs.push(comp_bg);
                keep_bufs.push(mode_buf);

                // The group and the now-superseded parent are dead: their reads
                // are recorded, so recycle them for reuse (textures live until
                // submit). The scratch takes the parent's slot in the stack.
                pool.recycle(group);
                active.pop();
                if parent != base {
                    pool.recycle(parent);
                }
                active.push(scratch);
            }
            _ => unreachable!(),
        }
        i += 1;
    }

    *active.first().unwrap_or(&base)
}

/// Multi-pass render for pages with transparency groups: a stack of offscreen
/// layers, each composited onto its parent on PopBlendGroup. Layers come from a
/// recycling [`blend::LayerPool`] so a page with hundreds of groups reuses a
/// small working set instead of allocating (and never freeing) one full-page
/// layer per group.
fn render_layered(
    ctx: &GpuContext,
    target: &PageTarget,
    page_clear: wgpu::Color,
    ops: &[PageOp],
    res: &ReplayRes,
    timer: &timing::GpuTimer,
) -> Result<(GpuTexture, Option<u64>), WgpuRenderError> {
    let device = &ctx.device;
    let (w, h, sc) = (target.width, target.height, target.sample_count);

    let mut pool = blend::LayerPool::new(w, h, sc);
    let base = pool.acquire(device);
    // Composite bind groups + mode buffers kept alive until submit.
    let mut keep_bgs: Vec<wgpu::BindGroup> = Vec::new();
    let mut keep_bufs: Vec<wgpu::Buffer> = Vec::new();

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("zpdf-layered"),
    });
    timer.write_begin(&mut encoder);

    // Initialize the base layer (page background, no clips).
    {
        let pass = begin_layer_pass(
            &mut encoder,
            pool.get(base),
            wgpu::LoadOp::Clear(page_clear),
            wgpu::LoadOp::Clear(0),
        );
        drop(pass);
    }

    let final_layer = composite_into(
        ctx,
        &mut encoder,
        &mut pool,
        base,
        ops,
        res,
        &mut keep_bgs,
        &mut keep_bufs,
    );
    timer.write_end(&mut encoder);
    timer.record_resolve(&mut encoder);
    target.record_copy_from(&mut encoder, pool.get(final_layer).sampleable_texture());
    ctx.queue.submit(Some(encoder.finish()));
    let result = target.map_and_strip(device);
    let gpu_ns = timer.resolve_ns(device);
    if let Some(msg) = ctx.take_error() {
        return Err(WgpuRenderError::Wgpu(format!("device error: {msg}")));
    }
    result.map(|tex| (tex, gpu_ns))
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
    _pad0: u32,
    backdrop_luma: f32,
    _pad1: [f32; 3],
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
    res: &ReplayRes,
    keep_bgs: &mut Vec<wgpu::BindGroup>,
    keep_bufs: &mut Vec<wgpu::Buffer>,
) -> usize {
    use wgpu::util::DeviceExt;
    use zpdf_display_list::SoftMaskKind;
    let device = &ctx.device;

    // 1. Composite the mask group into its own layer. Luminosity masks start from
    //    the /BC backdrop luminosity (opaque); alpha masks from transparent. The
    //    group may itself contain nested transparency groups, so it goes through
    //    the layered compositor rather than a single replay pass.
    let mask_base = pool.acquire(device);
    let clear = match mask.kind {
        SoftMaskKind::Luminosity => {
            let l = mask.backdrop_luma.clamp(0.0, 1.0) as f64;
            wgpu::Color {
                r: l,
                g: l,
                b: l,
                a: 1.0,
            }
        }
        SoftMaskKind::Alpha => wgpu::Color::TRANSPARENT,
    };
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
        ctx, encoder, pool, mask_base, &mask.ops, res, keep_bgs, keep_bufs,
    );

    // 2. Pre-multiply the group layer by the mask coverage into a fresh layer.
    //    The apply pass samples coverage at `coord − (dx, dy)` (tiling-pattern
    //    reuse offset), reduces to luminosity/alpha, and runs it through the /TR
    //    transfer LUT before scaling the group.
    let masked = pool.acquire(device);
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
            _pad0: 0,
            backdrop_luma: mask.backdrop_luma.clamp(0.0, 1.0),
            _pad1: [0.0; 3],
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
    }
    keep_bgs.push(mask_bg);
    keep_bufs.push(mask_u);

    // The original group and the mask's layers have been consumed into `masked`
    // (their reads are recorded); recycle them for reuse. `mask_base` differs from
    // `mask_layer` only when the mask group itself contained nested groups (the
    // composite produced a fresh scratch); recycle it too so deep nesting doesn't
    // leak a layer per level.
    pool.recycle(group);
    pool.recycle(mask_layer);
    if mask_base != mask_layer {
        pool.recycle(mask_base);
    }
    masked
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
