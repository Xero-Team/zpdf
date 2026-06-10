//! wgpu GPU rendering backend for zpdf.
//!
//! Implements the same [`RenderBackend`] trait as the tiny-skia CPU renderer.
//! The CPU renderer is the correctness oracle: this backend reproduces its pixels
//! closely enough to pass `zpdf compare` at <1% differing pixels.
//!
//! Status: M1/M2 — headless context, per-page render target, and pixel readback
//! are implemented; `execute` command arms are no-ops until M4 (fills/strokes).

mod blend;
mod context;
mod glyph;
mod image;
mod path;
mod pipelines;
mod record;
mod target;
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
}

impl<'a> WgpuRenderer<'a> {
    pub fn new() -> Self {
        Self {
            ctx: None,
            font_cache: None,
            image_cache: None,
            page: None,
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
        let Some(page) = self.page.as_mut() else {
            return Ok(());
        };
        match cmd {
            RenderCommand::FillPath {
                path,
                rule,
                paint: Paint::Solid(c),
                alpha,
            } => {
                let color = quantize_premul(c, *alpha);
                page.recorder.add_fill(path, *rule, color, &page.map);
            }
            RenderCommand::StrokePath {
                path,
                style,
                paint: Paint::Solid(c),
                alpha,
            } => {
                let color = quantize_premul(c, *alpha);
                page.recorder.add_stroke(path, style, color, &page.map);
            }
            RenderCommand::PushClip { path, rule } => {
                page.recorder.push_clip(path, *rule, &page.map);
            }
            RenderCommand::PopClip => {
                page.recorder.pop_clip();
            }
            RenderCommand::DrawGlyphRun(run) => {
                if let Some(fonts) = self.font_cache {
                    glyph::render_glyph_run(&mut page.recorder, fonts, &page.map, run);
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
            RenderCommand::PushBlendGroup { blend_mode, .. } => {
                page.recorder.push_blend(*blend_mode);
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
        let fs_range = if needs_fs {
            Some(
                page.recorder
                    .append_fullscreen(page.uniform.w_px, page.uniform.h_px),
            )
        } else {
            None
        };

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

        let mut tex_bgs: std::collections::HashMap<u32, wgpu::BindGroup> =
            std::collections::HashMap::new();
        if let Some(images) = self.image_cache {
            for op in &rec.ops {
                if let PageOp::Image { image_id, .. } = op {
                    if tex_bgs.contains_key(image_id) {
                        continue;
                    }
                    if let Some(img) = images.get(*image_id) {
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
                            continue;
                        }
                        tex_bgs.insert(*image_id, image::upload_image_bind_group(ctx, img));
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
            fs_range,
        };

        // Pages with transparency groups need the multi-pass offscreen-layer path.
        if rec.has_blend_groups() {
            return render_layered(ctx, &page.target, page.clear, &rec.ops, &res);
        }

        // Single-pass path (no blend groups): one render pass into the page target.
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("zpdf-page"),
        });
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
            replay_ops(&mut pass, &rec.ops, &res);
        }
        page.target.record_copy(&mut encoder);
        ctx.queue.submit(Some(encoder.finish()));
        page.target.map_and_strip(device)
    }
}

/// Shared GPU resources for replaying recorded ops into a render pass.
struct ReplayRes<'a> {
    pipelines: &'a pipelines::Pipelines,
    bind_group: &'a wgpu::BindGroup,
    solid: Option<(&'a wgpu::Buffer, &'a wgpu::Buffer)>,
    tex: Option<(&'a wgpu::Buffer, &'a wgpu::Buffer)>,
    tex_bgs: &'a std::collections::HashMap<u32, wgpu::BindGroup>,
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

/// Multi-pass render for pages with transparency groups: a stack of offscreen
/// layers, each composited onto its parent on PopBlendGroup.
fn render_layered(
    ctx: &GpuContext,
    target: &PageTarget,
    page_clear: wgpu::Color,
    ops: &[PageOp],
    res: &ReplayRes,
) -> Result<GpuTexture, WgpuRenderError> {
    use wgpu::util::DeviceExt;
    let device = &ctx.device;
    let (w, h, sc) = (target.width, target.height, target.sample_count);

    let mut layers: Vec<blend::RenderLayer> = vec![blend::RenderLayer::new(device, w, h, sc)];
    let mut active: Vec<usize> = vec![0];
    let mut blend_info: Vec<(zpdf_display_list::BlendMode, Vec<record::ClipStamp>)> = Vec::new();
    // Composite bind groups + mode buffers kept alive until submit.
    let mut keep_bgs: Vec<wgpu::BindGroup> = Vec::new();
    let mut keep_bufs: Vec<wgpu::Buffer> = Vec::new();

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("zpdf-layered"),
    });

    // Initialize the base layer (page background, no clips).
    {
        let base = begin_layer_pass(
            &mut encoder,
            &layers[0],
            wgpu::LoadOp::Clear(page_clear),
            wgpu::LoadOp::Clear(0),
        );
        drop(base);
    }

    let mut i = 0;
    while i < ops.len() {
        let start = i;
        while i < ops.len() && !matches!(ops[i], PageOp::PushBlend { .. } | PageOp::PopBlend) {
            i += 1;
        }
        if start < i {
            let cur = *active.last().unwrap();
            let mut pass = begin_layer_pass(
                &mut encoder,
                &layers[cur],
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
            PageOp::PushBlend { mode, clips } => {
                let g = layers.len();
                layers.push(blend::RenderLayer::new(device, w, h, sc));
                init_layer(
                    &mut encoder,
                    &layers[g],
                    wgpu::Color::TRANSPARENT,
                    clips,
                    res,
                );
                active.push(g);
                blend_info.push((*mode, clips.clone()));
            }
            PageOp::PopBlend => {
                let group = active.pop().unwrap_or(0);
                let (mode, clips) = blend_info
                    .pop()
                    .unwrap_or((zpdf_display_list::BlendMode::Normal, Vec::new()));
                let parent = *active.last().unwrap_or(&0);
                let scratch = layers.len();
                layers.push(blend::RenderLayer::new(device, w, h, sc));

                // Mode uniform (16-byte aligned) + composite bind group.
                let mode_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("zpdf-blend-mode"),
                    contents: bytemuck::cast_slice(&[blend::blend_index(mode), 0u32, 0, 0]),
                    usage: wgpu::BufferUsages::UNIFORM,
                });
                let comp_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("zpdf-composite-bg"),
                    layout: &ctx.pipelines.composite_bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(
                                layers[parent].sampleable_view(),
                            ),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(
                                layers[group].sampleable_view(),
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
                        &mut encoder,
                        &layers[scratch],
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
                // The scratch becomes the layer at the parent's level.
                active.pop();
                active.push(scratch);
            }
            _ => unreachable!(),
        }
        i += 1;
    }

    let final_layer = *active.first().unwrap_or(&0);
    target.record_copy_from(&mut encoder, layers[final_layer].sampleable_texture());
    ctx.queue.submit(Some(encoder.finish()));
    target.map_and_strip(device)
}

/// Tracks the currently-bound pipeline to skip redundant `set_pipeline` calls.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Pipe {
    None,
    Solid,
    ClipWrite,
    ClipReset,
    Textured,
}

/// Tracks which vertex/index buffer pair is bound (solid arena vs textured arena).
#[derive(PartialEq, Eq, Clone, Copy)]
enum BufSet {
    None,
    Solid,
    Tex,
}
