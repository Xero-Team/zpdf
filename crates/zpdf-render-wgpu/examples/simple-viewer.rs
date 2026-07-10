//! Continuous-scroll PDF viewer (winit 0.30 + wgpu surface).
//!
//! All pages are laid out in one vertical canvas. Pages are rendered on demand into
//! GPU tiles as they enter the viewport (reusing a single headless `GpuContext` so
//! each page render is cheap), cached, and evicted LRU when they scroll far off
//! screen. Visible tiles are blitted at the current scroll offset and zoom.
//!
//! Run:  `cargo run -p zpdf-render-wgpu --example simple-viewer -- <file.pdf>`
//! Keys: wheel scroll · Ctrl+wheel / `+` `-` zoom · PageUp/Down · Home/End · `0` fit · Esc quit.

use std::collections::HashMap;
use std::sync::Arc;

use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use zpdf::{ContentInterpreter, IccCache, ImageCache, PdfDocument, RenderBackend};
use zpdf_render_wgpu::{GpuContext, WgpuRenderer};

/// Resolution each page tile is rasterized at. Zoom scales the blit; this is the
/// crispness ceiling.
const RENDER_DPI: f32 = 150.0;
/// Vertical gap between pages, in tile pixels.
const PAGE_GAP: f32 = 14.0;
/// Max cached page tiles (off-screen ones beyond this are evicted LRU).
const MAX_TILES: usize = 16;
/// Max new tiles rasterized per frame (spreads the cost while scrolling fast).
const MAX_RENDER_PER_FRAME: usize = 3;
/// Vertex-buffer capacity in quads (visible tiles per frame).
const MAX_QUADS: usize = 32;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    pos: [f32; 2],
    uv: [f32; 2],
}

/// One page's position in the vertical canvas, in tile pixels (at RENDER_DPI).
struct PageLayout {
    top: f32,
    w: f32,
    h: f32,
}

/// A rasterized page tile living on the surface device.
struct Tile {
    bind_group: wgpu::BindGroup,
    last_used: u64,
}

struct PageImage {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

/// Rasterize page `idx` to CPU pixels, reusing the headless GPU context in `slot`
/// (created on first use) so we don't rebuild device/pipelines per page.
fn render_page(doc: &PdfDocument, idx: usize, slot: &mut Option<GpuContext>) -> Option<PageImage> {
    let page = doc.page(idx).ok()?;
    let mut fonts = doc.load_page_fonts(&page);
    let content = doc.page_content_bytes(&page).ok()?;
    let mut images = ImageCache::new();
    let mut colors = IccCache::new();
    let doc_intents = doc.output_intents();
    let oi_cmyk = zpdf::output_intent_cmyk_profile(
        doc.file(),
        doc.page_output_intents(&page),
        &doc_intents,
        &mut colors,
    );
    let mut interpreter = ContentInterpreter::new(page.media_box)
        .with_fonts(&mut fonts)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut images)
        .with_colors(&mut colors)
        .with_operand_stack_limit(doc.file().limits().max_operand_stack_depth as usize);
    if let Some(profile) = oi_cmyk {
        interpreter = interpreter.with_output_intent_cmyk(profile);
    }
    let dl = interpreter.interpret(&content);

    let mut renderer = WgpuRenderer::new().with_fonts(&fonts).with_images(&images);
    if let Some(ctx) = slot.take() {
        renderer = renderer.with_context(ctx);
    }
    let result = renderer.render_display_list(&dl, RENDER_DPI / 72.0);
    *slot = renderer.take_context(); // reclaim the context for the next page
    let tex = result.ok()?;
    Some(PageImage {
        width: tex.width,
        height: tex.height,
        data: tex.data,
    })
}

/// GPU resources for presenting tiles to the window.
struct Gfx {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    bgl: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    vbuf: wgpu::Buffer,
    ibuf: wgpu::Buffer,
}

impl Gfx {
    fn new(window: Arc<Window>) -> Self {
        let size = window.inner_size();
        let instance = wgpu::Instance::default();
        let surface = instance.create_surface(window.clone()).expect("surface");
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
        }))
        .expect("adapter");
        // Start from the broadly-compatible downlevel limits, but raise the max
        // texture dimension to what the adapter actually supports — a page tile
        // at RENDER_DPI (e.g. test5 is 2480 px wide) routinely exceeds the
        // downlevel 2048 cap and would otherwise fail texture creation.
        let mut required_limits = wgpu::Limits::downlevel_defaults();
        required_limits.max_texture_dimension_2d = adapter.limits().max_texture_dimension_2d;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("simple-viewer-device"),
            required_features: wgpu::Features::empty(),
            required_limits,
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            memory_hints: wgpu::MemoryHints::MemoryUsage,
            trace: wgpu::Trace::Off,
        }))
        .expect("device");

        let caps = surface.get_capabilities(&adapter);
        // Non-sRGB format: the page tiles hold already-gamma-encoded bytes, so blit 1:1.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("blit"),
            source: wgpu::ShaderSource::Wgsl(BLIT_WGSL.into()),
        });
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("blit-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("blit-layout"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let attrs = wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2];
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("blit"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Vertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &attrs,
                }],
                compilation_options: Default::default(),
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                targets: &[Some(format.into())],
                compilation_options: Default::default(),
            }),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("blit-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let vbuf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("quad-vbuf"),
            size: (std::mem::size_of::<Vertex>() * 4 * MAX_QUADS) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let ibuf = {
            use wgpu::util::DeviceExt;
            device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("quad-ibuf"),
                contents: bytemuck::cast_slice(&[0u16, 1, 2, 0, 2, 3]),
                usage: wgpu::BufferUsages::INDEX,
            })
        };

        Self {
            window,
            surface,
            device,
            queue,
            config,
            pipeline,
            bgl,
            sampler,
            vbuf,
            ibuf,
        }
    }

    fn resize(&mut self, w: u32, h: u32) {
        self.config.width = w.max(1);
        self.config.height = h.max(1);
        self.surface.configure(&self.device, &self.config);
    }

    /// Upload page pixels to a texture + bind group.
    fn upload(&self, img: &PageImage) -> wgpu::BindGroup {
        let size = wgpu::Extent3d {
            width: img.width,
            height: img.height,
            depth_or_array_layers: 1,
        };
        let tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("page-tile"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &img.data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(img.width * 4),
                rows_per_image: Some(img.height),
            },
            size,
        );
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("page-tile-bg"),
            layout: &self.bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        })
    }
}

struct App {
    doc: PdfDocument,
    layout: Vec<PageLayout>,
    doc_height: f32,
    max_w: f32,
    scroll: f32,
    zoom: f32,
    fit_done: bool,
    tiles: HashMap<usize, Tile>,
    ctx: Option<GpuContext>,
    gfx: Option<Gfx>,
    modifiers: ModifiersState,
    frame: u64,
}

impl App {
    fn new(doc: PdfDocument) -> Self {
        // Lay all pages out vertically in tile-pixel space (no rendering yet).
        let s = RENDER_DPI / 72.0;
        let mut layout = Vec::new();
        let mut y = PAGE_GAP;
        let mut max_w = 1.0f32;
        for i in 0..doc.page_count() {
            let (w, h) = doc
                .page(i)
                .map(|p| (p.width() as f32 * s, p.height() as f32 * s))
                .unwrap_or((612.0 * s, 792.0 * s));
            layout.push(PageLayout { top: y, w, h });
            max_w = max_w.max(w);
            y += h + PAGE_GAP;
        }
        Self {
            doc,
            layout,
            doc_height: y,
            max_w,
            scroll: 0.0,
            zoom: 1.0,
            fit_done: false,
            tiles: HashMap::new(),
            ctx: None,
            gfx: None,
            modifiers: ModifiersState::empty(),
            frame: 0,
        }
    }

    fn viewport(&self) -> (f32, f32) {
        let gfx = self.gfx.as_ref().unwrap();
        (gfx.config.width as f32, gfx.config.height as f32)
    }

    fn max_scroll(&self) -> f32 {
        let (_, wh) = self.viewport();
        (self.doc_height - wh / self.zoom).max(0.0)
    }

    fn clamp_scroll(&mut self) {
        self.scroll = self.scroll.clamp(0.0, self.max_scroll());
    }

    /// Zoom around the vertical center of the viewport.
    fn set_zoom(&mut self, z: f32) {
        let (_, wh) = self.viewport();
        let center = self.scroll + (wh / self.zoom) * 0.5;
        self.zoom = z.clamp(0.1, 8.0);
        self.scroll = center - (wh / self.zoom) * 0.5;
        self.clamp_scroll();
    }

    /// Fit the widest page to ~95% of the window width.
    fn fit_width(&mut self) {
        let (ww, _) = self.viewport();
        self.zoom = (ww * 0.95 / self.max_w).clamp(0.1, 8.0);
        self.clamp_scroll();
    }

    fn request_redraw(&self) {
        if let Some(gfx) = self.gfx.as_ref() {
            gfx.window.request_redraw();
        }
    }

    fn redraw(&mut self) {
        self.frame += 1;
        if self.gfx.is_none() {
            return;
        }
        if !self.fit_done {
            self.fit_width();
            self.fit_done = true;
        }
        self.clamp_scroll();

        let (ww, wh) = self.viewport();
        let zoom = self.zoom;
        let scroll = self.scroll;
        let view_top = scroll;
        let view_bot = scroll + wh / zoom;

        // Visible pages = bands intersecting the viewport.
        let visible: Vec<usize> = self
            .layout
            .iter()
            .enumerate()
            .filter(|(_, p)| p.top + p.h >= view_top && p.top <= view_bot)
            .map(|(i, _)| i)
            .collect();

        // `gfx` borrows only self.gfx; the tile/ctx/doc fields stay independently usable.
        let gfx = self.gfx.as_ref().unwrap();

        // Rasterize missing visible tiles (capped per frame).
        let mut rendered = 0usize;
        let mut pending = false;
        for &i in &visible {
            if self.tiles.contains_key(&i) {
                continue;
            }
            if rendered >= MAX_RENDER_PER_FRAME {
                pending = true;
                break;
            }
            rendered += 1;
            if let Some(img) = render_page(&self.doc, i, &mut self.ctx) {
                let bind_group = gfx.upload(&img);
                self.tiles.insert(
                    i,
                    Tile {
                        bind_group,
                        last_used: self.frame,
                    },
                );
            }
        }

        // Touch visible tiles, then evict LRU off-screen ones over the cap.
        for &i in &visible {
            if let Some(t) = self.tiles.get_mut(&i) {
                t.last_used = self.frame;
            }
        }
        while self.tiles.len() > MAX_TILES {
            let victim = self
                .tiles
                .iter()
                .filter(|(k, _)| !visible.contains(k))
                .min_by_key(|(_, t)| t.last_used)
                .map(|(k, _)| *k);
            match victim {
                Some(v) => {
                    self.tiles.remove(&v);
                }
                None => break,
            }
        }

        // Build quads (NDC) for visible tiles that are ready.
        let ndc = |x: f32, y: f32| [2.0 * x / ww - 1.0, 1.0 - 2.0 * y / wh];
        let mut verts: Vec<Vertex> = Vec::new();
        let mut draws: Vec<&wgpu::BindGroup> = Vec::new();
        for &i in &visible {
            if draws.len() >= MAX_QUADS {
                break;
            }
            let Some(tile) = self.tiles.get(&i) else {
                continue;
            };
            let p = &self.layout[i];
            let sx = ww * 0.5 - p.w * zoom * 0.5;
            let sy = (p.top - scroll) * zoom;
            let (sw, sh) = (p.w * zoom, p.h * zoom);
            verts.extend_from_slice(&[
                Vertex {
                    pos: ndc(sx, sy),
                    uv: [0.0, 0.0],
                },
                Vertex {
                    pos: ndc(sx + sw, sy),
                    uv: [1.0, 0.0],
                },
                Vertex {
                    pos: ndc(sx + sw, sy + sh),
                    uv: [1.0, 1.0],
                },
                Vertex {
                    pos: ndc(sx, sy + sh),
                    uv: [0.0, 1.0],
                },
            ]);
            draws.push(&tile.bind_group);
        }

        // Present.
        let frame = match gfx.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f)
            | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                gfx.surface.configure(&gfx.device, &gfx.config);
                self.request_redraw();
                return;
            }
            _ => return,
        };
        if !verts.is_empty() {
            gfx.queue
                .write_buffer(&gfx.vbuf, 0, bytemuck::cast_slice(&verts));
        }
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut enc = gfx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("blit-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.12,
                            g: 0.12,
                            b: 0.14,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&gfx.pipeline);
            pass.set_index_buffer(gfx.ibuf.slice(..), wgpu::IndexFormat::Uint16);
            pass.set_vertex_buffer(0, gfx.vbuf.slice(..));
            for (k, bg) in draws.iter().enumerate() {
                pass.set_bind_group(0, Some(*bg), &[]);
                pass.draw_indexed(0..6, (k * 4) as i32, 0..1);
            }
        }
        gfx.queue.submit(Some(enc.finish()));
        frame.present();

        // Title: which page is at the viewport top, total, zoom.
        let cur = self
            .layout
            .iter()
            .position(|p| p.top + p.h >= view_top)
            .unwrap_or(0)
            + 1;
        gfx.window.set_title(&format!(
            "zpdf simple viewer — page {}/{}  ({:.0}%)",
            cur,
            self.layout.len(),
            zoom * 100.0
        ));

        if pending {
            self.request_redraw(); // more tiles to rasterize next frame
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gfx.is_some() {
            return;
        }
        let window = Arc::new(
            event_loop
                .create_window(Window::default_attributes().with_title("zpdf simple viewer"))
                .expect("window"),
        );
        self.gfx = Some(Gfx::new(window));
        self.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::Resized(size) => {
                if let Some(gfx) = self.gfx.as_mut() {
                    gfx.resize(size.width, size.height);
                }
                self.request_redraw();
            }
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::MouseWheel { delta, .. } => {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32 / 40.0,
                };
                if self.modifiers.control_key() {
                    self.set_zoom(self.zoom * (1.0 + dy * 0.1));
                } else {
                    // Scroll ~90 screen px per wheel notch, in doc space.
                    self.scroll -= dy * 90.0 / self.zoom;
                    self.clamp_scroll();
                }
                self.request_redraw();
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let (_, wh) = self.viewport();
                let page = wh / self.zoom * 0.9;
                match event.logical_key.as_ref() {
                    Key::Named(NamedKey::Escape) => event_loop.exit(),
                    Key::Named(NamedKey::PageDown) | Key::Named(NamedKey::Space) => {
                        self.scroll += page;
                    }
                    Key::Named(NamedKey::PageUp) => self.scroll -= page,
                    Key::Named(NamedKey::ArrowDown) => self.scroll += 90.0 / self.zoom,
                    Key::Named(NamedKey::ArrowUp) => self.scroll -= 90.0 / self.zoom,
                    Key::Named(NamedKey::Home) => self.scroll = 0.0,
                    Key::Named(NamedKey::End) => self.scroll = self.max_scroll(),
                    Key::Character("+") | Key::Character("=") => self.set_zoom(self.zoom * 1.25),
                    Key::Character("-") => self.set_zoom(self.zoom / 1.25),
                    Key::Character("0") => self.fit_width(),
                    _ => {}
                }
                self.clamp_scroll();
                self.request_redraw();
            }
            _ => {}
        }
    }
}

const BLIT_WGSL: &str = r#"
struct VsOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };
@vertex fn vs(@location(0) pos: vec2<f32>, @location(1) uv: vec2<f32>) -> VsOut {
    var o: VsOut; o.pos = vec4<f32>(pos, 0.0, 1.0); o.uv = uv; return o;
}
@group(0) @binding(0) var t: texture_2d<f32>;
@group(0) @binding(1) var s: sampler;
@fragment fn fs(i: VsOut) -> @location(0) vec4<f32> { return textureSample(t, s, i.uv); }
"#;

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: simple-viewer <file.pdf>");
        std::process::exit(1);
    });
    let data = std::fs::read(&path).expect("read pdf");
    let doc = PdfDocument::open(data).expect("open pdf");
    let event_loop = EventLoop::new().expect("event loop");
    let mut app = App::new(doc);
    event_loop.run_app(&mut app).expect("run");
}
