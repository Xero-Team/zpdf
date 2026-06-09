# Phase 3 — wgpu GPU Rendering Backend (toward zpdf 0.2.0)

## 1. Overview & goals

Phase 3 makes `crates/zpdf-render-wgpu` a real, second `RenderBackend` alongside the tiny-skia CPU renderer. Today it is a stub: `WgpuRenderer {}` with three `todo!()` methods and a `GpuTexture` that holds nothing.

**What 0.2.0 delivers:**

- A **headless GPU renderer** implementing the exact same `RenderBackend` trait (`begin_page`/`execute`/`end_page`) and handling *all* eight `RenderCommand` variants (FillPath, StrokePath, DrawGlyphRun, DrawImage, PushClip/PopClip, PushBlendGroup/PopBlendGroup).
- A CLI `--backend [cpu|wgpu]` flag so `zpdf render f.pdf -p1 -o gpu.png --backend wgpu` works, threaded into the **existing hand-rolled positional parser** (not clap — see DECISION D8).
- An interactive **winit viewer** example (`cargo run -p zpdf-render-wgpu --example viewer -- f.pdf`) hitting ≥60fps via vsync.
- Pure Rust, zero C/C++ dependencies preserved (wgpu, lyon, pollster, winit, tiny-skia, bytemuck are all pure Rust).

**Headless-first principle.** The renderer's primary path requires **no window and no surface** (`wgpu::Instance::default()` → adapter → device, no display handle). The viewer is a strictly-optional `[dev-dependency]` example that never enters a library or CLI build. This keeps CI deterministic and the dependency surface minimal.

**The acceptance gate.** GPU output must match the CPU reference under the existing `zpdf compare` subcommand at **<1% differing pixels**. `cmd_compare` computes, per pixel, `max |Δ|` over R,G,B only (alpha ignored), counting a pixel as "differing" if that max exceeds `threshold`. The CPU renderer (tiny-skia) is the **correctness oracle**: every transform, fill rule, alpha fold, sampler choice, and color-space decision in the GPU backend exists to reproduce the CPU's exact pixels. This is the single organizing constraint of the entire phase — when a design choice trades fidelity-to-spec against fidelity-to-CPU, **we always follow the CPU**, because that is what `compare` measures.

**Oracle-first construction principle (D7 generalized).** Wherever a fast path risks diverging from the CPU (glyph atlas, batching), we ship the **simple, provably-equivalent path first as the validated baseline**, then layer the optimization behind a flag and promote it only after the corpus passes in both modes. This applies to glyphs (vector-fill baseline before R8 atlas) and to draw batching (Immediate before Batched).

---

## 2. Crate & dependency changes

### 2.1 Root `Cargo.toml` — `[workspace.dependencies]`

`pollster` is **not** currently in the workspace. Add:

```toml
pollster = "0.4"
# winit is NOT added here — it is example-only (see 2.3)
```

`wgpu = "29"`, `lyon = "1"`, `bytemuck` (derive), `tiny-skia`, `zpdf-font`, `zpdf-content`, `zpdf-image`, `image` (png feature) already exist as workspace deps. **Note:** the API in this plan was verified against vendored **wgpu 29.0.0**; the workspace `wgpu = "29"` resolves to the latest 29.x (29.0.3 is API-compatible at patch level). Do not assert the vendored tree is 29.0.3.

### 2.2 `crates/zpdf-render-wgpu/Cargo.toml`

```toml
[dependencies]
zpdf-core.workspace = true
zpdf-display-list.workspace = true
zpdf-render.workspace = true
zpdf-font.workspace = true        # NEW: LoadedFont/GlyphOutline (glyph atlas, Type3)
zpdf-image.workspace = true       # NEW: ImageCache/DecodedImage (image textures)
zpdf-content.workspace = true     # NEW: ContentInterpreter (Type3 char-procs)
wgpu.workspace = true
lyon.workspace = true
bytemuck.workspace = true
tiny-skia.workspace = true        # NEW: pure-Rust glyph-coverage raster (AA parity)
pollster.workspace = true         # NEW: block_on for headless device/readback
tracing.workspace = true
thiserror.workspace = true
# image: see DECISION D1 below — NOT added; PNG encode stays in CLI

[dev-dependencies]
winit = "0.30"                    # viewer example only
pollster.workspace = true
zpdf = { workspace = true, default-features = false, features = ["gpu-render"] }

[[example]]
name = "viewer"
```

### 2.3 Layering justification (the "no parser dep" rule)

CLAUDE.md says *"render backends never depend on the parser."* It does **not** forbid `zpdf-font`/`zpdf-image`/`zpdf-content`. The CPU backend is the proof of intended layering: `zpdf-render-cpu` depends on `{zpdf-core, zpdf-content, zpdf-display-list, zpdf-font, zpdf-image, zpdf-render}` and **not** on `zpdf-parser`/`zpdf-document` directly. We mirror that exact set, so we are compliant by definition. `zpdf-content` transitively pulls in the parser/document, but the *direct* rule holds because the backend names `zpdf-content`, not `zpdf-parser` — identical to the CPU backend today. `tiny-skia` is pure Rust (it backs the CPU renderer); using it to bake a GPU glyph atlas does not breach the no-C-deps constraint.

**`winit` MUST stay in `[dev-dependencies]`.** Placing it in `[dependencies]` would link a windowing stack into the library and break headless CI. Gate-check with `cargo tree -e features --features gpu-render | rg winit` (must be empty).

### 2.4 `crates/zpdf-cli/Cargo.toml` — feature wiring

Currently `zpdf-cli` pulls `zpdf` with default features (enabling `cpu-render`). Switch to explicit features so `cargo build` stays CPU-only and `--features gpu` adds the GPU path:

```toml
[features]
default = ["cpu"]
cpu = ["zpdf/cpu-render"]
gpu = ["zpdf/gpu-render"]

[dependencies]
zpdf = { workspace = true, default-features = false }
image.workspace = true            # DIRECT CLI dep — used by compare/save_rgba; NOT feature-gated
```

**`image` must remain an un-gated direct CLI dependency.** `cmd_compare`, `cmd_text`, and `cmd_render` use `image::open`/`RgbaImage` directly in `main.rs`. Gating it behind `cpu` would break `compare`. The CLI color/image management is upstream of the backend choice.

### 2.5 `crates/zpdf/src/lib.rs` — no edit

Already exposes `pub mod gpu { pub use zpdf_render_wgpu::*; }` behind `#[cfg(feature = "gpu-render")]` (verified, lines 17–20). The glob re-export carries the redefined `WgpuRenderer<'a>`, `GpuTexture`, `GpuContext`, `WgpuRenderError` through unchanged (confirmed: adding `<'a>` and a `data` field is safe because the stub has zero real callers — all methods are `todo!()`).

### 2.6 Resolved contradiction — DECISION D1: PNG encoding lives in the CLI, not the wgpu crate

Two subsystem designs disagreed on whether `save_png` lives in `zpdf-render-wgpu` (requires an `image` dep) or the CLI. **Decision: PNG encode stays in the CLI.** The wgpu crate returns only `GpuTexture { width, height, data: Vec<u8> }` (tight RGBA8). The CLI gains one shared helper `save_rgba(path, w, h, &data)` used by *both* backends, making "saved identically" literal — one encoder, one code path. This keeps `image` out of the wgpu crate's dependency surface. Both `RenderedPage` and `GpuTexture` expose `.width/.height/.data`, so the CLI dispatch arms are symmetric. (`RenderedPage::save_png` stays as-is for back-compat; the new `save_rgba` free function is what both `--backend` arms call.)

---

## 3. Module layout

```
crates/zpdf-render-wgpu/src/
  lib.rs              WgpuRenderer<'a>, GpuTexture{data}, WgpuRenderError, RenderBackend impl,
                      execute() dispatch, with_fonts/with_images/with_context builders
  context.rs          GpuContext: Instance/Adapter/Device/Queue init (headless + surface),
                      MSAA + Stencil8 negotiation, pipeline cache, target_format/sample_count
  target.rs           PageTarget: per-page MSAA color + resolve + Stencil8, padded readback buffer
  transform.rs        PageUniform, vertex POD structs (SolidVertex/TexturedVertex), PageMap (host
                      affine), DrawUniform, vertex buffer layouts, NDC helpers, quantize helpers
  pipelines.rs        Pipelines: solid_fill, textured(image), glyph, clip_write + layouts
  path/
    mod.rs            public surface of the path subsystem
    tessellate.rs     DlPath→lyon::Path (subpath balancing), fill/stroke tessellation
    buffers.rs        PathGeometryArena, DrawBatch, GrowableBuffer pools, quantize helpers
    dash.rs           DASH_ENABLED=false stub (parity: CPU ignores dashes)
  glyph.rs            GlyphRenderer: vector-fill baseline + optional R8 atlas, batching
  atlas.rs            GlyphAtlas (R8Unorm), ShelfPacker, GlyphKey, LRU/per-page reset
  type3.rs            Type3 expansion → ContentInterpreter → path pipeline
  image.rs            TextureCache (LRU), GpuImage, upload_image, build_image_quad, ImageScratch ring
  clip.rs             ClipState, ClipEntry, stencil stamp/test logic, rebuild
  blend.rs            RenderLayer stack, blend-group offscreen targets, composite pass
  batch/              (M8, optional) BatchBuilder coalescing — see 5.9
  shaders/
    solid.wgsl        fill/stroke/Type3: device-pixel vertex → NDC, premultiplied solid fragment
    textured.wgsl     image: device-pixel vertex → NDC, Nearest-sampled premultiplied fragment
    glyph.wgsl        glyph: device-pixel vertex → NDC, R8 coverage × straight color fragment
    clip.wgsl         stencil-only stamp (position, no color)
    composite.wgsl    full-screen blend-group composite, all 16 BlendModes
examples/
  viewer.rs           winit 0.30 ApplicationHandler viewer (pan/zoom/page-flip, ≥60fps)
  common.rs           shared build_page() helper (interpreter + font/image caches)
tests/
  acceptance.rs       compare-based golden harness over tests/corpus/*.pdf
```

---

## 4. Coordinate system — the correctness keystone

**This transform is the single most important piece of the phase. State it once; every subsystem references it.**

The CPU oracle (`zpdf-render-cpu/src/lib.rs`) maps a PDF page-unit point `(x, y)` (origin bottom-left, +Y up) to a device pixel (origin top-left, +Y down):

```
scale       = info.scale                       (= dpi / 72.0)
page_height = info.page_rect.height() as f32   (Rect::height() is .abs())
to_pixel_x(x) = x * scale                       (NO x0 offset — page_rect.x0 ignored)
flip_y(y)     = (page_height - y) * scale
px = x * scale
py = (page_height - y) * scale
```

**Framebuffer dimensions are TRUNCATING casts** (parity-critical — must equal the CPU buffer exactly):

```
w = (page_rect.width()  * scale) as u32        (clamp .max(1) on GPU to avoid zero-size panic)
h = (page_rect.height() * scale) as u32
```

**Pixel → NDC** (wgpu clip space: x right+, y up+; framebuffer sampled top-row-first). Divide by the **integer** `w,h` (as f32), NOT the float product `page_width*scale`, to stay bit-aligned with the CPU buffer at the right/bottom edges:

```
ndc_x = 2 * px / w_px - 1
ndc_y = 1 - 2 * py / h_px
```

### Two vertex paths sharing one pixel→NDC tail

The flip is **conditional** for glyphs/images via the `ctm_flips_y` heuristic and therefore cannot be a single global page-space transform. The host bakes all per-primitive affine into device-pixel positions; the single vertex shader entry `vs_pixel` does only pixel→NDC:

- **`solid_fill` + `clip_write`** (FillPath/StrokePath/PushClip): host tessellates in **device-pixel** space (DECISION D2), baking `scale` + `flip_y`.
- **`textured` (image) + `glyph`**: the host bakes the full per-primitive affine into device-pixel positions (including the `ctm_flips_y` branch); the vertex shader does only pixel→NDC.

The `ctm_flips_y` heuristic, used **identically** in image, outline-glyph, and Type3 paths:

```
ctm_flips_y = tm.d < 0.0 || (tm.d == 0.0 && tm.b != 0.0)
```

### DECISION D2 — tessellate fills/strokes in DEVICE-PIXEL space

**Decision: tessellate fills/strokes/clips in device-pixel space**, baking `scale` + `flip_y` into points at build time, exactly like the CPU's `build_skia_path`. Rationale (the load-bearing parity reason): lyon computes stroke offset geometry (miter joins, round-cap arc flattening) **at tessellation time on the input coordinates**. To match the CPU's device-space stroke under anisotropic CTMs, offsetting must happen post-scale. Tolerance is then a constant device-pixel budget (`0.1` px) and all pipelines unify on `vs_pixel` (pixel→NDC only). The `PageUniform` needs only `w_px, h_px` for pixel→NDC (we still carry `scale`/`page_height` for any future page-space path, but the shipped fill/stroke path uses device-pixel positions).

### PARITY RULE — arithmetic precision (NEW, from completeness critique)

The CPU's `outline_to_pixel`/`type3_to_pixel`/`render_image` do the **font-unit and scale steps in f32, then cast to f64 for the `tm` application** (Matrix components are f64), then the final pixel coords land in f32. The host-side affine bake for glyphs/images/Type3 **must reproduce this exact precision sequence**: compute `tx,ty` font-unit terms in f32, cast to f64, apply the f64 `tm`, then cast the resulting pixel coords back to f32. Doing the whole bake in f32 introduces sub-pixel drift that can shift quads across the AA threshold. This rule sits alongside the truncating-dimension-cast rule as a hard parity invariant.

---

## 5. Subsystem designs (dependency order)

### 5.0 Shared types: `WgpuRenderer`, `GpuTexture`, errors

`WgpuRenderer` becomes **`WgpuRenderer<'a>`** (borrows font/image caches, mirroring `CpuRenderer<'a>`). `GpuTexture` gains `data: Vec<u8>`.

```rust
// lib.rs
pub struct GpuTexture {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,   // tight RGBA8, top-left origin, len == w*h*4, padding stripped
}

#[derive(Debug, thiserror::Error)]
pub enum WgpuRenderError {
    #[error("wgpu device not initialized")] NotInitialized,   // kept from stub
    #[error("no active page")] NoActivePage,
    #[error("no compatible GPU adapter found")] NoAdapter,
    #[error("required GPU feature unavailable: {0}")] Unsupported(String),
    #[error("buffer readback failed: {0}")] Readback(String),
    #[error("device poll failed: {0}")] Poll(String),
    #[error("wgpu error: {0}")] Wgpu(String),                 // kept from stub
}

pub struct WgpuRenderer<'a> {
    ctx: Option<GpuContext>,                   // lazily created (headless) or injected (viewer)
    font_cache: Option<&'a zpdf_font::FontCache>,
    image_cache: Option<&'a zpdf_image::ImageCache>,
    page: Option<PageState>,                   // per-page resources, alive begin..end
}
impl<'a> WgpuRenderer<'a> {
    pub fn new() -> Self { Self { ctx: None, font_cache: None, image_cache: None, page: None } }
    pub fn with_fonts(mut self, c: &'a zpdf_font::FontCache) -> Self { self.font_cache = Some(c); self }
    pub fn with_images(mut self, c: &'a zpdf_image::ImageCache) -> Self { self.image_cache = Some(c); self }
    pub fn with_context(mut self, ctx: GpuContext) -> Self { self.ctx = Some(ctx); self }   // viewer reuse
    pub fn take_context(&mut self) -> Option<GpuContext> { self.ctx.take() }
}
```

`WgpuRenderError` keeps the stub's `NotInitialized` and `Wgpu(String)` variants (additive, compatible). Color-space conversion (DeviceGray/RGB/CMYK/Indexed/Lab) happens **upstream in `zpdf-content`/`zpdf-color`** — the backend only ever sees final RGBA in `Paint::Solid` and RGBA8 in `DecodedImage`. **No GPU-side color management is required.**

### 5.1 GpuContext — headless init, MSAA + Stencil8 negotiation (P3.1)

Created once, reused across pages and viewer frames. Adapter/device requests are **async**; block with `pollster`.

```rust
pub struct GpuContext {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub pipelines: Pipelines,                   // built once
    pub target_format: wgpu::TextureFormat,     // Rgba8Unorm (linear!)
    pub sample_count: u32,                       // 4 or 1
    pub max_texture_dim: u32,                     // adapter limit (Risk R3)
}

impl GpuContext {
    pub fn new_headless() -> Result<Self, WgpuRenderError> {
        pollster::block_on(Self::new_headless_async(false))
    }
    /// `force_fallback` selects the software adapter (lavapipe/WARP) deterministically on CI.
    async fn new_headless_async(force_fallback: bool) -> Result<Self, WgpuRenderError> {
        let instance = wgpu::Instance::default();                       // headless ctor, no surface
        let adapter = instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: force_fallback,                      // CI software-adapter knob (R2)
            compatible_surface: None,
        }).await.map_err(|_| WgpuRenderError::NoAdapter)?;             // v29: returns Result

        // Risk R3: raise the texture-dim limit to the adapter max (downlevel default is only 2048).
        let adapter_limits = adapter.limits();
        let mut limits = wgpu::Limits::downlevel_defaults();
        limits.max_texture_dimension_2d = adapter_limits.max_texture_dimension_2d;

        let (device, queue) = adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("zpdf-device"),
            required_features: wgpu::Features::empty(),
            required_limits: limits,
            experimental_features: wgpu::ExperimentalFeatures::disabled(), // v29 field
            memory_hints: wgpu::MemoryHints::MemoryUsage,
            trace: wgpu::Trace::Off,                                        // v29 field
        }).await.map_err(|e| WgpuRenderError::Wgpu(format!("device: {e}")))?;

        let target_format = wgpu::TextureFormat::Rgba8Unorm;

        // Stencil8 renderability probe (completeness critique): error cleanly, don't silently mis-render.
        let stencil_flags = adapter
            .get_texture_format_features(wgpu::TextureFormat::Stencil8).flags;
        if !stencil_flags.contains(wgpu::TextureFormatFeatureFlags::empty()) { /* always true */ }
        // (Stencil8 RENDER_ATTACHMENT is core in WebGPU; the probe below guards exotic adapters.)
        if !adapter.get_texture_format_features(target_format).allowed_usages
            .contains(wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC) {
            return Err(WgpuRenderError::Unsupported("Rgba8Unorm RENDER_ATTACHMENT|COPY_SRC".into()));
        }

        let color_flags = adapter.get_texture_format_features(target_format).flags;
        let sample_count = if color_flags.contains(wgpu::TextureFormatFeatureFlags::MULTISAMPLE_X4)
            && adapter.get_texture_format_features(wgpu::TextureFormat::Stencil8).flags
                .contains(wgpu::TextureFormatFeatureFlags::MULTISAMPLE_X4) { 4 } else { 1 };

        let pipelines = Pipelines::build(&device, target_format, sample_count);
        Ok(Self { instance, adapter, device, queue, pipelines, target_format, sample_count,
                  max_texture_dim: adapter_limits.max_texture_dimension_2d })
    }
    // new_for_surface(instance, &surface) — viewer; renders offscreen Rgba8Unorm, blits to surface
}
```

**v29 gotchas baked in:** `DeviceDescriptor` requires `experimental_features` + `trace` + `memory_hints`; `request_adapter`/`request_device` are async returning `Result`; `RequestAdapterOptions` has `force_fallback_adapter` (exposed for CI). On adapters lacking `MULTISAMPLE_X4` on *both* color and Stencil8, `sample_count` falls back to 1 (correct, worse AA; `warn!`). **MSAA requires color and stencil to share `sample_count`** — both negotiated together here. The `request_adapter` `map_err` uses `|_|` to avoid an unused-variable warning.

**Risk R3 (texture size).** `required_limits.max_texture_dimension_2d` is raised to the adapter max so large pages/images at high DPI don't trip the 2048 downlevel cap. In `begin_page`/image upload, any dimension still exceeding `max_texture_dim` triggers a clean `warn!` + skip (image) or an error (page) — the CPU path has no such limit, so this is a documented, adapter-dependent divergence. **M1 decision:** raise limits (above) rather than clamp DPI.

**Risk R2 (CI adapter).** `--backend wgpu` never silently falls back to CPU (that masks GPU breakage); it errors cleanly via `NoAdapter`. CI must expose a software Vulkan adapter (lavapipe) or WARP; the harness sets `force_fallback_adapter: true` when `ZPDF_GPU_FORCE_FALLBACK=1`. **Open question resolved in M1 smoke test:** confirm which software adapter the runner exposes.

### 5.2 PageTarget — render-to-texture + MSAA + readback (P3.1)

One per page, rebuilt in `begin_page`. Owns MSAA color + resolve + Stencil8 + the padded staging buffer.

**Format invariants (the parity foundation):**
- Color format = **`Rgba8Unorm` (linear, NOT sRGB, NOT Bgra)**. tiny-skia blends in stored gamma byte-space; an sRGB target blends in linear and every blended/AA pixel diverges. Bgra swaps R/B.
- **Clear color conversion (FIX from critique):** `info.background` is `zpdf_core::Color` with **f32** fields; `wgpu::Color` is **f64**. Build it with channel-wise **CPU pre-quantization** so the cleared bytes equal the CPU's `(c*255) as u8` background fill byte-for-byte:
  ```rust
  let q = |v: f32| (((v * 255.0) as u8) as f64) / 255.0;
  let clear = wgpu::Color { r: q(bg.r), g: q(bg.g), b: q(bg.b), a: q(bg.a) };
  ```
- Stencil = **`Stencil8`**, created at the **same `sample_count` as color** (4 under MSAA; color and depth/stencil attachments must share sample_count).
- Final readback: **premultiplied RGB written as-is, NO demultiply** (matches the CPU PNG-boundary quirk: `pixmap.data()` returns premultiplied bytes that `image::RgbaImage::from_raw` writes verbatim, confirmed at `lib.rs:701/775`). Background is opaque ⇒ visible framebuffer is opaque ⇒ premul==straight ⇒ matches. **Invariant:** this parity argument holds only for an **opaque background**; the CLI's `render_display_list` hardcodes `Color::white()`, so it always holds there. Direct `begin_page` callers must pass an opaque background.

```rust
pub const COLOR_FORMAT:   wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
pub const STENCIL_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Stencil8;

pub struct PageTarget {
    pub width: u32, pub height: u32, pub sample_count: u32,
    pub color_msaa: wgpu::Texture, pub color_msaa_view: wgpu::TextureView,
    pub resolve: Option<wgpu::Texture>, pub resolve_view: Option<wgpu::TextureView>,
    pub stencil: wgpu::Texture, pub stencil_view: wgpu::TextureView,
    pub readback: wgpu::Buffer,
    pub padded_bytes_per_row: u32, pub unpadded_bytes_per_row: u32,
}
```

When `sample_count == 1`, skip the resolve texture; `color_msaa` itself carries `COPY_SRC` and is the readback source. When `sample_count > 1`, the resolve texture (1-sample, `RENDER_ATTACHMENT | COPY_SRC`) is the readback source.

**Render pass wiring (v29 required fields — FIX from critique, these have no defaults):**

```rust
let color_att = wgpu::RenderPassColorAttachment {
    view: &target.color_msaa_view,
    depth_slice: None,                                   // v29 REQUIRED field
    resolve_target: target.resolve_view.as_ref(),        // Some(..) under MSAA, None at 1x
    ops: wgpu::Operations {
        load: wgpu::LoadOp::Clear(clear),                // pre-quantized background
        store: if target.sample_count > 1 { wgpu::StoreOp::Discard } // MSAA: discard, resolve is source
               else { wgpu::StoreOp::Store },
    },
};
let depth_stencil_att = wgpu::RenderPassDepthStencilAttachment {
    view: &target.stencil_view,
    depth_ops: None,                                     // Stencil8-only: MUST be None
    stencil_ops: Some(wgpu::Operations {
        load: wgpu::LoadOp::Clear(0),
        store: wgpu::StoreOp::Store,
    }),
};
let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
    label: Some("page-pass"),
    color_attachments: &[Some(color_att)],
    depth_stencil_attachment: Some(depth_stencil_att),
    timestamp_writes: None,
    occlusion_query_set: None,
    multiview_mask: None,                                // v29 REQUIRED field
});
```

**Readback (end_page), grounded in `render_to_texture/mod.rs`:**

```rust
let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;                 // 256
let padded = (width * 4).div_ceil(align) * align;
// v29 names: TexelCopyTextureInfo / TexelCopyBufferInfo / TexelCopyBufferLayout
encoder.copy_texture_to_buffer(
    wgpu::TexelCopyTextureInfo { texture: src, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
    wgpu::TexelCopyBufferInfo { buffer: &readback,
        layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(padded), rows_per_image: Some(height) } },
    wgpu::Extent3d { width, height, depth_or_array_layers: 1 });
queue.submit(Some(encoder.finish()));

let slice = readback.slice(..);
// Capture the map Result (don't swallow it); device.poll blocks until the callback fires.
let map_result = std::sync::Arc::new(std::sync::Mutex::new(None));
let mr = map_result.clone();
slice.map_async(wgpu::MapMode::Read, move |r| { *mr.lock().unwrap() = Some(r); });
device.poll(wgpu::PollType::wait_indefinitely())
    .map_err(|e| WgpuRenderError::Poll(format!("{e}")))?;       // v29: poll returns Result
match map_result.lock().unwrap().take() {
    Some(Ok(())) => {}
    Some(Err(e)) => return Err(WgpuRenderError::Readback(format!("{e}"))),
    None => return Err(WgpuRenderError::Readback("map callback did not fire".into())),
}
let view = slice.get_mapped_range()                              // v29: returns Result — FIX
    .map_err(|e| WgpuRenderError::Readback(e.to_string()))?;
// strip padding row-by-row → tight w*h*4 Vec<u8>
let mut out = Vec::with_capacity((width * height * 4) as usize);
for row in 0..height {
    let start = (row * padded) as usize;
    out.extend_from_slice(&view[start..start + (width * 4) as usize]);
}
drop(view);
readback.unmap();                                                // matches the vendored example
```

**FIX from critiques:** `get_mapped_range()` returns `Result` in v29 — mapped to `WgpuRenderError::Readback`. `device.poll()` returns `Result` — mapped to `WgpuRenderError::Poll`. The map callback's `Result` is captured (not discarded) so a failed mapping surfaces as an error rather than garbage bytes. `unmap()` is called after copying.

### 5.3 PageUniform + vertex types (transform.rs)

```rust
#[repr(C)] #[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
pub struct PageUniform { pub w_px: f32, pub h_px: f32, pub scale: f32, pub page_height: f32 } // 16B, uniform-aligned

#[repr(C)] #[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
pub struct SolidVertex { pub pos: [f32; 2], pub color: [f32; 4] }      // device px; premultiplied color, 24B

#[repr(C)] #[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
pub struct TexturedVertex { pub pos: [f32; 2], pub uv: [f32; 2], pub color: [f32; 4] } // 32B
// image: color = [1,1,1, draw.alpha].  glyph: color = STRAIGHT rgb + straight alpha (see 5.5/5.6)
```

`PageUniform` is exactly 16 bytes → satisfies uniform-buffer 16-byte alignment. Vertex structs are vertex-buffer-bound (no uniform alignment constraint) and contiguous (no padding) → `Pod` is sound. Vertex attribute layouts use the verified macro form:
```rust
wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x4]            // SolidVertex
wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32x4] // TexturedVertex
```

**Color quantization + premultiply (DECISION D3, REVISED per contract critique).** The CPU passes **straight 8-bit** color to tiny-skia (`set_color_rgba8((c.r*255) as u8, …, (c.a*alpha*255) as u8)`, `color_to_paint` lines 93–103); tiny-skia premultiplies **internally** in integer fixed-point during compositing. To match that single integer premultiply (and avoid a double-quantization rounding chain), do the premultiply on the host with tiny-skia's exact integer formula, not f32 `c*a`:

```rust
fn quantize_premul(c: &Color, alpha: f32) -> [f32; 4] {
    // Quantize straight color exactly as tiny-skia receives it:
    let cr = (c.r * 255.0) as u8;
    let cg = (c.g * 255.0) as u8;
    let cb = (c.b * 255.0) as u8;
    let ca = (c.a * alpha * 255.0) as u8;                 // final alpha = color.a * cmd.alpha
    // tiny-skia integer premultiply: (channel * alpha + 127) / 255  (rounded), per channel.
    let premul = |ch: u8| -> f32 {
        let p = ((ch as u32 * ca as u32 + 127) / 255) as u8;
        p as f32 / 255.0
    };
    [premul(cr), premul(cg), premul(cb), ca as f32 / 255.0]   // PREMULTIPLIED, integer-rounded
}
```

For **opaque** draws (`ca == 255`) this reduces to the straight quantized color (premul==straight), so the common case is exact. For **translucent** draws it reproduces tiny-skia's `(c*a+127)/255` integer premultiply, eliminating the LSB drift the f32 path would introduce. Color rides in the vertex stream (not a per-draw uniform) so different-colored fills/glyphs batch together. Indices are **`u32`** everywhere (`IndexFormat::Uint32`) — `u16`'s 65535 cap overflows on complex glyph/path meshes. A **`translucent.pdf`** corpus case (50% overlapping fills) gates this path.

> Note on glyphs/images: the *vertex* color for glyphs is **straight** (un-premultiplied), because the glyph fragment multiplies by coverage and premultiplies in-shader (5.5). Images carry `[1,1,1,alpha]` and premultiply the texel in-shader. Only fills/strokes/Type3 use the premultiplied `quantize_premul` output directly.

### 5.4 Path tessellation (P3.2/P3.3) — fill, stroke, dash

`DlPath → lyon::path::Path` with **subpath balancing** (lyon panics on unbalanced subpaths). Map each point through the device-pixel `PageMap` (D2). Track an `open` flag, inject `end(false)` before each new `MoveTo` and at end-of-stream:

```rust
fn build_lyon_path(dl: &Path, m: &PageMap) -> Option<LyonPath> {
    if dl.elements.is_empty() { return None; }       // EMPTY PATH → None → no draw (mirrors CPU build_skia_path)
    let mut b = LyonPath::builder(); let mut open = false;
    for el in &dl.elements { match *el {
        PathElement::MoveTo(p) => { if open { b.end(false); } b.begin(m.pt(p)); open = true; }
        PathElement::LineTo(p) => { if !open { b.begin(m.pt(p)); open = true; } else { b.line_to(m.pt(p)); } }
        PathElement::CurveTo(c1,c2,e) => { if !open { b.begin(m.pt(c1)); open = true; } b.cubic_bezier_to(m.pt(c1),m.pt(c2),m.pt(e)); }
        PathElement::Close => { if open { b.end(true); open = false; } }
    }}
    if open { b.end(false); }
    Some(b.build())
}
```

**Fill:** `FillOptions::tolerance(0.1).with_fill_rule(NonZero|EvenOdd)` — lyon supports **both** rules (EvenOdd is its default); `NonZero` maps from the CPU's `Winding`. `with_intersections(true)` (default) — PDF fills self-intersect.

**Stroke (REVISED per completeness critique — clamps reconciled to the oracle):** `StrokeOptions` with `line_width = style.width * scale` (device px), caps Butt/Round/Square 1:1, joins Miter/Round/Bevel 1:1.
- **miter_limit:** the CPU passes `style.miter_limit` straight to tiny-skia with no clamp; lyon **asserts ≥1.0**. PDF miter limits are ≥1 by spec, but to avoid a panic on a malformed corpus file we clamp **only when below 1.0** and emit `tracing::debug!` — documented as an oracle divergence that cannot occur for valid PDFs. A unit test asserts no corpus file triggers the clamp.
- **0/sub-1px width:** the CPU passes `style.width * scale` straight to tiny-skia (no `MIN_STROKE_PX` clamp). To match, **do not force 1.0**. Reproduce tiny-skia's behavior: tiny-skia treats width 0 as producing no stroke geometry — so if `line_width <= 0`, **skip the draw** (matching tiny-skia's empty result) rather than synthesizing a hairline. For `0 < width < 1` we pass the true sub-pixel width to lyon; a **`thin_strokes.pdf`** corpus case (0-width and 0.3px strokes) gates that this stays under the threshold-16 budget. (lyon does emit a thin mesh at sub-pixel widths; if a corpus case shows divergence we revisit, but the default is pass-through, not clamp.)

Both `StrokeVertex::position()` and `FillVertex::position()` return the final offset device-pixel vertex — feed straight in.

**DECISION D4 — dashes are IGNORED.** The CPU drops `StrokeStyle.dash`. For parity the GPU **must also stroke solid** — implementing dashing would *diverge* from the oracle. `dash.rs` is a documented `DASH_ENABLED=false` stub; emit `tracing::debug!` (not `warn!`) when a dash is present.

**Primitive state:** `cull_mode: None` always — lyon emits both windings; `ctm_flips_y`/negative determinants flip winding; culling would drop triangles.

### 5.5 Pipelines + WGSL (P3.2)

Three content pipelines (**`solid_fill`**, **`textured`** for images, **`glyph`**) plus **`clip_write`** (stencil stamp). Fill-rule and stroke are tessellator concerns, so they share `solid_fill`.

All use **`PREMULTIPLIED_ALPHA_BLENDING`** (`src + dst*(1-src.a)`), matching tiny-skia's premultiplied source-over. Fragments emit premultiplied color. `MultisampleState { count: sample_count, .. }`.

**v29 pipeline/layout API (FIX from critique):**
- **`immediate_size: 0` belongs on `PipelineLayoutDescriptor`, NOT `RenderPipelineDescriptor`.** The pipeline descriptor has exactly: `label, layout, vertex, primitive, depth_stencil, multisample, fragment, multiview_mask: None, cache: None`. There is no `push_constant_ranges` and no `immediate_size` on it.
- `PipelineLayoutDescriptor { label, bind_group_layouts, immediate_size: 0 }`.
- **`bind_group_layouts` entries are wrapped in `Some(..)`:** `bind_group_layouts: &[Some(&page_bgl), Some(&tex_bgl)]` (type is `&[Option<&BindGroupLayout>]`). A naive `&[&layout]` will not compile.
- `set_bind_group(index, Some(&bind_group), &[])` (the bind-group arg is `Option`).
- Vertex state: `compilation_options: Default::default()`, `entry_point: Some("vs_pixel")`.

```rust
let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
    label: Some("solid-layout"),
    bind_group_layouts: &[Some(&page_bgl)],
    immediate_size: 0,                                   // v29: lives HERE
});
let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
    label: Some("solid_fill"),
    layout: Some(&layout),
    vertex: wgpu::VertexState { module: &shader, entry_point: Some("vs_pixel"),
        buffers: &[solid_vbl], compilation_options: Default::default() },
    primitive: wgpu::PrimitiveState { cull_mode: None, ..Default::default() },
    depth_stencil: Some(stencil_state()),               // DECISION D5
    multisample: wgpu::MultisampleState { count: sample_count, ..Default::default() },
    fragment: Some(wgpu::FragmentState { module: &shader, entry_point: Some("fs_solid"),
        targets: &[Some(color_target)], compilation_options: Default::default() }),
    multiview_mask: None,                                // v29 REQUIRED
    cache: None,                                         // v29 REQUIRED
});
```

**DECISION D5 — content pipelines test the clip stencil with a single ref-driven variant.** Every content pipeline carries:
```rust
fn stencil_state() -> wgpu::DepthStencilState {
    let face = wgpu::StencilFaceState {
        compare: wgpu::CompareFunction::Equal,
        fail_op: wgpu::StencilOperation::Keep,
        depth_fail_op: wgpu::StencilOperation::Keep,
        pass_op: wgpu::StencilOperation::Keep,
    };
    wgpu::DepthStencilState {
        format: wgpu::TextureFormat::Stencil8,
        depth_write_enabled: false,
        depth_compare: wgpu::CompareFunction::Always,
        stencil: wgpu::StencilState { front: face, back: face,
            read_mask: 0xff, write_mask: 0x00 },
        bias: wgpu::DepthBiasState::default(),
    }
}
```
Before each content draw, `set_stencil_reference(clip_depth)`. Unclipped ⇒ ref 0, stencil cleared to 0 ⇒ `Equal 0` passes everywhere ⇒ draws everywhere. This unifies clipped/unclipped into one pipeline.

Sampler choices (parity-critical):
- **Image sampler = Nearest** + `ClampToEdge` (tiny-skia `PixmapPaint::default()` is `FilterQuality::Nearest`). Texture `sample_type: Float { filterable: false }`, `SamplerBindingType::NonFiltering`.
- **Glyph atlas sampler = Linear** (axis-aligned baked-bucket interpolation; only used on the atlas fast path, never for rotated glyphs — see 5.6).

**WGSL essence** (one `pixel_to_ndc` helper shared by all; FIX: glyph fragment never divides by alpha):

```wgsl
fn pixel_to_ndc(px: f32, py: f32) -> vec4<f32> {
    return vec4(2.0*px/page.w_px - 1.0, 1.0 - 2.0*py/page.h_px, 0.0, 1.0);
}
// fs_solid: color is already premultiplied & integer-quantized → return color;
// fs_glyph: straight rgb + straight alpha in vertex; cov from R8 atlas:
//   let cov = textureSample(atlas, samp, in.uv).r;
//   let a = in.color.a * cov;
//   return vec4(in.color.rgb * a, a);        // premultiply with coverage; NEVER divide by color.a
// fs_image: texel treated per 5.7 (premultiply straight-alpha texels in-shader):
//   var t = textureSample(img, smp, in.uv);
//   t = vec4(t.rgb * t.a, t.a);              // straight→premultiplied (no-op for opaque a==1)
//   return t * in.color.a;                   // draw.alpha (PixmapPaint.opacity)
```

### 5.6 Glyphs: vector-fill baseline + optional R8 atlas + Type3 (P3.4)

**DECISION D6 (REVISED per completeness + contract critiques) — the vector-fill path is the oracle baseline; the R8 atlas is an axis-aligned-only optimization gated by measurement.**

The CPU renders each glyph as a **fresh vector path** filled with `anti_alias=true` at the **exact fractional device position** (`outline_to_pixel` bakes `glyph.x` and the full `tm` into f32 pixel coords, no rounding). Reproducing that exactly is the correctness requirement.

**Baseline (ship first, M6a):** route every outline glyph through the **same `solid_fill` vector pipeline as Type3/fills** — tessellate the transformed glyph outline with lyon (`FillRule::Winding`) at the exact device-pixel position, no bucket rounding, no bitmap. This is correct-by-construction relative to the CPU's per-glyph `fill_path` (same coordinates, same winding rule; only the AA kernel differs, lyon+MSAA vs tiny-skia analytic — that residual is what the threshold budgets). Measure `text_latin.pdf` compare on this path **before** building any atlas.

**Optimization (M6b, gated): R8 coverage atlas for AXIS-ALIGNED glyphs only.** Only when (a) the vector baseline is too slow on dense text AND (b) the atlas measurably *improves* (not worsens) the compare delta, enable an R8Unorm coverage atlas baked with tiny-skia (`FillRule::Winding`, `anti_alias=true`, opaque-white fill ⇒ alpha plane == coverage). **The atlas is used only when `tm.b == 0 && tm.c == 0` (no rotation/shear).** For axis-aligned glyphs the baked bucket + Linear-sampled quad reproduces tiny-skia AA closely; for **rotated/sheared glyphs the upright-bake-then-rotate-bitmap approach does NOT match the CPU's direct rotated-outline rasterization**, so those **always** take the vector-fill baseline. Oversized glyphs (>256px bucket) also take the vector path.

```rust
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct GlyphKey { font_id: u32, glyph_id: u16, px_size: u32 }   // atlas path only; axis-aligned

fn glyph_device_em(font_size: f32, tm: &Matrix, scale: f32) -> f32 {
    let lin = ((tm.a*tm.d - tm.b*tm.c).abs() as f32).sqrt();    // rotation/shear-invariant
    font_size * lin * scale
}
// px_size = round(device_em).clamp(1, 256);  atlas used iff tm.b==0 && tm.c==0 && device_em<=256
```

Atlas = 2048×2048 R8Unorm, shelf packer, 1px gutter, **LRU** (per-page reset for the CLI; persistent drop-and-rebuild for the viewer). Upload with **256-aligned `bytes_per_row`** (R8 = 1 byte/px so padding bites at every width).

**Quad/affine generation reproduces `outline_to_pixel` exactly** — only `glyph.x` is used (never `glyph.y`); `glyph.x` is user-space, added **before** `tm`. **Per the precision parity rule (§4):** compute `tx,ty` in f32, cast to f64, apply f64 `tm`, then cast pixel coords to f32:

```
tx = gx/upem*font_size + glyph.x;   ty = gy/upem*font_size              // f32
page_x = (tm.a as f64)*(tx as f64) + (tm.c as f64)*(ty as f64) + tm.e;  // f64 tm
page_y = (tm.b as f64)*(tx as f64) + (tm.d as f64)*(ty as f64) + tm.f;
flips = ctm_flips_y;
px = (page_x*scale) as f32;
py = (if flips { page_y*scale } else { (page_height - page_y)*scale }) as f32;
```

For the atlas fast path the atlas stores the upright glyph and the 4 device-pixel quad corners are computed from the glyph bbox under the same affine (axis-aligned, so no shear). For the vector path the full outline is transformed directly.

**Type3 glyphs** (`type3.rs`): `type3_glyph_stream` returns content-stream bytes + a `font_matrix`. Interpret via `ContentInterpreter::new(Rect::new(0.0,-1000.0,1000.0,1000.0)).interpret(stream)` → sub-DisplayList; route **only** its `FillPath`/`StrokePath` (everything else — including any nested image — ignored, mirroring the CPU `_ => {}` arm at `render_type3_glyphs` lines 543–592) through `solid_fill` with the `type3_to_pixel` affine baked on the host (same f32→f64→f32 precision rule). Paint = the **outer run's color** (d0/d1 take text fill color). Stroke width = `(style.width * fm[0].abs() * font_size * scale).max(0.5)` (matches CPU). This forces the `zpdf-content` dep. The `text_type3.pdf` corpus case uses **path-only char-procs** (no images) so coverage is honest. **Defer-able:** if Type3 is cut from the first pass, no-op `DrawGlyphRun` when `font.is_type3()` and drop `zpdf-content` until then (flag: compare diverges on Type3 docs).

Dispatch guards (mirror CPU exactly — a single missing guard = extra/missing pixels = compare failure): `font_cache` None → skip the whole run; `font_cache.get(font_id)` None → skip; `!has_font_data()` → skip; `glyph_outline()` None → skip that glyph; non-`Solid` paint → skip; final alpha = `color.a * run.alpha` (when 0, the glyph contributes nothing — skip).

### 5.7 Image textures (P3.5)

`TextureCache` keyed by stable `image_id` (no quantization — images upload at native resolution, the quad scales). LRU eviction (per-page clear for CLI; budget-driven for viewer).

**Upload:** `Rgba8Unorm`, `TEXTURE_BINDING | COPY_DST`, **256-aligned `bytes_per_row`** (direct path when `width*4` already aligned, else staging copy). **Bytes uploaded verbatim — no host premultiply.**

**Premultiply semantics (CLARIFIED per critiques R6 + completeness):** `DecodedImage` carries `premultiplied: bool` and `has_alpha: bool`. The CPU's `render_image` (lib.rs:325) calls `PixmapRef::from_bytes` on the straight RGBA and **tiny-skia premultiplies internally before compositing**. To match that:
- **The fragment shader premultiplies straight texels** (`t.rgb *= t.a`) for images where `premultiplied == false` — this matches tiny-skia's from_bytes→premultiply→composite for translucent (SMask) images. (Earlier draft wording "treat texels as already premultiplied" was wrong for straight-alpha SMasks and is corrected here.)
- For **opaque** images (`has_alpha == false`, `a == 1`) premultiply is a no-op, so the common case is exact regardless.
- If `zpdf-image` ever emits `premultiplied == true`, skip the in-shader premultiply for those. **M7 must first confirm**, by inspecting `apply_smask`, that SMask images arrive as `premultiplied: false` straight RGBA with `has_alpha: true` — the `image_alpha.pdf` corpus case must exercise a **straight-alpha SMask** to gate this exact path.

**Quad transform reproduces `render_image()` exactly** — copy both `ctm_flips_y` branches verbatim, with the §4 f32→f64→f32 precision rule. The unit-square Y-flip lives in the affine (`uy = 1 - iy/ih`); UVs are plain `(ix/iw, iy/ih)` (no UV flip). With `s=scale`, `ph=page_height`, `iw/ih` image dims:

```
flips = tm.d < 0 || (tm.d == 0 && tm.b != 0)
if flips:  (t_sx,t_kx,t_ky,t_sy,t_tx,t_ty) = (a*s/iw, -c*s/ih,  b*s/iw, -d*s/ih, (c+e)*s, (d+f)*s)
else:                                       = (a*s/iw, -c*s/ih, -b*s/iw,  d*s/ih, (c+e)*s, (ph-d-f)*s)
screen = (t_sx*ix + t_kx*iy + t_tx, t_ky*ix + t_sy*iy + t_ty);  then pixel→NDC
```

`draw.alpha` = `PixmapPaint.opacity`: multiply the premultiplied texel by `alpha` in-shader. **Image draws are stencil-tested** via `set_stencil_reference(clip_depth)` (the CPU `render_image` passes `current_clip`, lib.rs:409 — images honor the clip). A **stenciled-image-under-clip** corpus case validates this.

**CRITICAL ImageScratch pitfall:** a single reused vertex buffer rewritten per image draw inside one pass produces last-write-wins (all images draw the last quad). **Mandatory: a ring buffer** — `queue.write_buffer(&ring, off, verts)` at a fresh offset per draw, `set_vertex_buffer(0, ring.slice(off..off+128))`, reset cursor per page. (Or per-draw `create_buffer_init` for the CLI, since images are rare.) Static index buffer `[0,1,2,0,2,3]`. Covered by `image_rgb.pdf` containing **≥2 images** (Risk R7).

### 5.8 Clip (stencil) + blend groups (offscreen) (P3.6)

**Clip via stencil — an EXACT reproduction of the CPU clip mask (strengthened per contract critique).** The CPU clip (`push_clip`, lines 191–229) rasterizes an 8-bit alpha Mask with `anti_alias=false` (hard-edged, binary 0/255) and intersects multiplicatively (`(m*c)/255`), which keeps nested clips binary (`255*255/255=255` else 0). The stencil `Equal == clip_depth` test is therefore an **exact** reproduction of the clip mask itself — not merely "close." The only residual is MSAA antialiasing the clipped *content* edge at the clip boundary (genuinely sub-pixel, within budget). **The R8 coverage-mask fallback is therefore unnecessary for correctness and is dropped from M5 scope** unless a specific `clip.pdf` case empirically fails.

Maintain `clip_depth = entries.len()`. The stencil value at a pixel = how many active clip paths cover it; content draws where `stencil == clip_depth`.

- **`clip_write` pipeline:** color writes masked off (`ColorWrites::empty()` on the target / fragment with no color output), `compare: Equal` against ref `n-1`, `pass_op: IncrementClamp`, `write_mask: 0xff`. Pushing clip #n stamps where `stencil == n-1` → increments to `n` (intersection accumulates). **Tessellate the clip path with the matching lyon fill rule** so `IncrementClamp` counts each clip exactly once.
- **PushClip:** flush current draws, stamp into the top layer's stencil at ref `clip_depth`, `clip_depth += 1`.
- **PopClip:** `clip_depth -= 1`, **clear stencil + re-stamp all remaining entries** (decrement-specific-region is unsafe at shared boundaries; PDFs nest clips shallowly so rebuild is cheap). `ClipState` keeps tessellated geometry per entry for this. **Unbalanced Pop → no-op** (CPU does the same).

**Clip ⇄ blend-group stack interaction (SPECIFIED per completeness critique).** Clip entries form a **single global stack, independent of the blend-layer stack** (mirrors the CPU, where `current_clip` is a renderer field persisting across blend push/pop):
- Each new `RenderLayer` (on PushBlendGroup) **re-stamps the CURRENT clip entries** into its fresh stencil at allocation (clips established outside the group still constrain draws inside).
- A `PushClip` **inside** a group stamps into that group's current-layer stencil and increments the global `clip_depth`; its `PopClip` rebuilds normally on that layer.
- On `PopBlendGroup` the **clip stack is unchanged**; the composite-into-scratch step re-stamps the then-current clip set into the scratch layer's stencil before compositing.
- A **clip-inside-blend-group** corpus case validates this interaction.

**Blend groups via offscreen RenderLayer stack.** Each layer = its own MSAA color + resolve + Stencil8 (all sharing `sample_count`).

- **PushBlendGroup:** flush + end current pass; allocate a new `RenderLayer`, clear color to **transparent black** `(0,0,0,0)` (matches `Pixmap::new`), clear stencil to 0, **re-stamp all active clip entries** into the new layer's stencil. `isolated`/`knockout`/`bounds` are **ignored** (CPU ignores them).
- **PopBlendGroup:** flush + end pass; composite the popped layer onto the base. Because a pass can't sample its own target, composite **into a fresh scratch layer** sampling both group + base via `textureLoad` (exact per-texel, no filtering), then swap the scratch in as the new base. Recycle layers via a free-list.

**16 blend modes in `composite.wgsl`.** 12 separable (Normal, Multiply, Screen, Overlay, Darken, Lighten, ColorDodge, ColorBurn, HardLight, SoftLight, Difference, Exclusion) + 4 non-separable (Hue, Saturation, Color, Luminosity via PDF SetLum/SetSat/ClipColor). Work in premultiplied space (unpremul → blend → repremul), matching tiny-skia. Composite formula: `Co = (1-ab)*Cs + ab*B(Cb,Cs)` then source-over. **v0.1 fallback (Risk R4):** the 4 non-separable modes may map to Normal with a `warn!` if their helpers don't land in time (rare in real docs).

**Premultiplied-alpha unifying contract (ties everything):** every layer stores premultiplied RGBA; all content fragments output premultiplied; all blends are premultiplied; readback writes premultiplied RGB **without demultiply**. Base layer clears to the (premultiplied, opaque) background. Over opaque white, final pixels are opaque ⇒ premul==straight ⇒ matches CPU.

**Limits/safety:** Stencil8 caps clip depth at 255 (clamp + `warn!` beyond — PDFs never nest that deep). Cap blend nesting (~64) per the ParseLimits ethos; on overflow `warn!` + no-op. `end_page` auto-pops any dangling blend groups before readback. Unbalanced Pop → no-op.

### 5.9 Draw-call batching (P3.7) — OPTIONAL, layered over a correct immediate path

**DECISION D7 — batching is a pure performance optimization, default OFF until validated; `Immediate` mode is the oracle.** Ship `BatchMode::Immediate` (one draw per command, correct, validated baseline) as default; `BatchMode::Batched` (adjacency-coalescing) behind a `with_batch_mode` builder. Flip the default to `Batched` only after the corpus passes in both modes.

**The batching safety rule:** draws may be coalesced only within a maximal run of **adjacent same-state commands** (same `BatchKey { pipeline, texture, clip_ref }`, color rides in vertices). **No global sort** — source-over is non-commutative and clip state is positional. Coalesce by appending re-based-index geometry into shared `GrowableBuffer` pools (reused across pages), **preserving per-primitive append order == command order**, one `queue.write_buffer` per pool per page, one `draw_indexed` per batch. **Hard flush** at every BatchKey change, PushClip/PopClip, PushBlendGroup/PopBlendGroup, and `end_page`. `Pattern`/`Shading` paint produce no geometry (no flush).

**Mode-equivalence test (WEAKENED per completeness critique):** because batching preserves command/append order within a batch, the rasterization sequence is identical, so we assert **`compare(immediate, batched) == 0 differing pixels at threshold 0`** rather than a brittle "byte-identical" claim (MSAA resolve + FP tessellation can differ at the last LSB even with identical order). Since batching is OFF by default and optional, this gate is informational, not release-blocking.

### 5.10 CLI dispatch (P3.8) — DECISION D8: thread into the existing hand-rolled parser

**The CLI is NOT clap.** `main()` dispatches on `args[1]`; `cmd_render()` manually walks args matching `-p`/`-o`/`--dpi` in a `while` loop with a `_ => {}` arm (line 269) that **silently ignores unknown flags**. There is no `Backend` enum and no save helper. **Plan:**

1. Add a `let mut backend = "cpu".to_string();` and a parser arm to the existing loop:
   ```rust
   "--backend" => { i += 1; backend = args.get(i).cloned()
       .unwrap_or_else(|| { eprintln!("--backend requires a value"); std::process::exit(2); }); }
   ```
2. **Validate the value explicitly** (the silent-ignore `_ => {}` arm means a typo would otherwise render CPU silently — guard against that):
   ```rust
   match backend.as_str() {
       "cpu" => { /* cpu arm */ }
       #[cfg(feature = "gpu")]
       "wgpu" => { /* gpu arm */ }
       #[cfg(not(feature = "gpu"))]
       "wgpu" => { eprintln!("--backend wgpu requires building with --features gpu"); std::process::exit(1); }
       other => { eprintln!("unknown --backend '{other}' (expected cpu|wgpu)"); std::process::exit(2); }
   }
   ```
3. Build the `DisplayList` **once** (interpreter + font/image caches are backend-agnostic), then branch only at render. Add a free `fn save_rgba(path: &Path, w: u32, h: u32, data: &[u8]) -> ...` in `main.rs`, used by **both** arms (`GpuTexture` has no `save_png`; `RenderedPage` keeps its own but the new arms route through `save_rgba` for a single encoder path):

```rust
let scale = dpi / 72.0;
match backend.as_str() {
    "cpu" => {
        let mut r = zpdf::cpu::CpuRenderer::new().with_fonts(&font_cache).with_images(&image_cache);
        let page = r.render_display_list(&display_list, scale)?;
        save_rgba(&output, page.width, page.height, &page.data)?;     // shared helper (D1)
    }
    #[cfg(feature = "gpu")]
    "wgpu" => {
        let mut r = zpdf::gpu::WgpuRenderer::new().with_fonts(&font_cache).with_images(&image_cache);
        let tex = r.render_display_list(&display_list, scale)?;
        save_rgba(&output, tex.width, tex.height, &tex.data)?;
    }
    _ => unreachable!("validated above"),
}
```

`render_display_list` defaults `background` to `Color::white()` ⇒ GPU clear = opaque white, matching CPU. The other five subcommands (`info`, `dump`, `text`, `compare`, `debug-stream`) are **untouched** by `--backend`.

### 5.11 winit viewer (P3.8) — example only

winit 0.30 `ApplicationHandler` + `EventLoop::run_app`. Window created in `resumed` (kept in an `Arc` for the surface). `GpuContext::new_for_surface` renders pages to an **offscreen Rgba8Unorm tile** (reusing `render_page_to_texture` — same geometry as `end_page` but **no CPU readback**), then a textured-quad **blit pass** draws the tile to the surface (Linear sampler; surface may be `Bgra8UnormSrgb`, wgpu converts on store — the viewer is not under the compare gate).

**Re-raster policy:** the tile is re-rendered only when **page index** or **zoom quantum** (`round(zoom*8)`) changes; pan only moves the quad uniform (no re-raster). Controls: scroll/`+`/`-` zoom, drag pan, PageUp/Down/arrows flip, `0` reset, Esc quit. `PresentMode::Fifo` caps to vsync (~60fps); title bar shows fps via CPU `Instant` dt. Surface acquire is the v29 `CurrentSurfaceTexture` enum (handle `Outdated`/`Lost` → reconfigure + skip); present via `queue.present(frame)`.

**60fps methodology:** the ≥60fps claim is measured on a text-heavy A4 page at 150 DPI. The worst case is **continuous zoom** (re-raster every quantum), not pan; M9 reports fps for both pan-only and continuous-zoom-drag so the re-raster cost is visible. Viewer is out of the compare gate, so this is a perf observation, not a hard acceptance.

---

## 6. Implementation sequencing

Each milestone is independently testable. **Critical path is M1 → M2 → M3 → M4** (the end-to-end pixel pipeline; ~7 days yields `--backend wgpu` fills/strokes at <1%). M5–M8 add command coverage and partly parallelize once M3's seams (encoder, target views, page uniform, stencil ref, render-pass field set) are fixed.

| Milestone | Roadmap | Deliverable | Independent test |
|---|---|---|---|
| **M1 — Context + deps** | P3.1 | `pollster` in workspace; crate deps added; `GpuContext::new_headless` (instance/adapter/device/queue, **raised texture-dim limit**, MSAA+Stencil8 probe, `force_fallback` knob). `WgpuRenderer<'a>` + builders. `GpuTexture{data}`. | `cargo build -p zpdf-render-wgpu --features ...`; headless context inits on CI (resolve R2 software-adapter question). |
| **M2 — Target + readback** | P3.1 | `PageTarget` (MSAA color + resolve + Stencil8 sharing sample_count + padded readback). `begin_page` (truncated dims, **pre-quantized f64 clear**, render pass with `depth_slice:None`/`multiview_mask:None`/`depth_ops:None`). `end_page` (copy→map-with-captured-Result→poll(Result)→`get_mapped_range`(Result)→strip→unmap→`GpuTexture`). All `execute` arms no-op. | Render blank page → all-white PNG of exact CPU dims; `zpdf compare` ~0%. Validates dims/format/clear/readback before any geometry. |
| **M3 — CLI + transform + WGSL scaffold** | P3.8/P3.2 | `--backend [cpu\|wgpu]` arm in the hand-rolled parser + explicit value validation + `gpu` CLI feature + `save_rgba` helper. `PageUniform`, vertex structs, `vs_pixel`/`pixel_to_ndc`, `fs_solid`. Pipeline-layout (`immediate_size:0`, `Some(&bgl)`) + pipeline (`multiview_mask:None`,`cache:None`) scaffolding. | `zpdf render f.pdf --backend wgpu` produces the M2 blank PNG via CLI; compare ~0%; typo'd backend errors (exit 2). |
| **M4 — Fills + strokes** | P3.2/P3.3 | `path/` (lyon tessellation, device-pixel D2, `u32` indices, miter ≥1-only clamp, width pass-through + skip-on-0, dash-ignore). `solid_fill` pipeline + D5 stencil state. `FillPath`/`StrokePath` arms + empty-path/non-Solid guards. | `rect_fills`, `curves`, `strokes`, `thin_strokes`, `translucent` → compare <1% (MSAA 4x). |
| **M5 — Clip** | P3.6a | `clip.rs` (stencil stamp/test, rebuild-on-pop, **R8 fallback dropped**), `clip_write` pipeline (`depth_ops:None`, matching fill rule), content-pipeline stencil test (D5), `clip_depth` plumbing. | `clip.pdf` (nested), `image_under_clip.pdf` → compare <1%. |
| **M6a — Glyphs (vector baseline) + Type3** | P3.4 | Outline glyphs through `solid_fill` vector path (exact `outline_to_pixel`, f32→f64→f32), `type3.rs`, full dispatch guards. | `text_latin`, `text_type3` (path-only procs), `text_rotated` → compare <1% (baseline, correct-by-construction). |
| **M6b — Glyph atlas (optional)** | P3.4 | `atlas.rs` (R8 shelf LRU), axis-aligned-only atlas path gated on a measured speed+fidelity win; rotated/oversized stay on vector baseline. | Atlas-on vs baseline compare delta ≤ baseline on `text_latin`; `text_rotated` still uses vector path. |
| **M7 — Images** | P3.5 | `image.rs` (`TextureCache` LRU, verbatim upload + **in-shader premultiply for straight-alpha**, `render_image` affine both branches, Nearest sampler, **ring** `ImageScratch`, clip-tested). Confirm `apply_smask` output shape. | `image_rgb` (≥2 images), `image_alpha` (straight-alpha smask), `image_under_clip` → compare <1%. |
| **M8 — Blend groups + batching** | P3.6b/P3.7 | `blend.rs` (RenderLayer stack, clip⇄blend interaction, composite.wgsl 16 modes, premultiplied contract), `batch/` (Immediate default + Batched). | `blend.pdf` (Normal+Multiply), `clip_in_blend.pdf` → compare <1%; mode-equivalence (0 diff @ threshold 0). |
| **M9 — Viewer + acceptance harness** | P3.8 | `examples/viewer.rs` (+ `common.rs`), `tests/corpus/*.pdf`, `tests/acceptance.rs`. Gate-check `winit` absent from lib builds; pan vs zoom fps report. | Full corpus <1%; `cargo run --example viewer` ≥60fps; `cargo tree` gate. |

**Parallelization:** after M3 fixes the seams, M4 (path) and M6b's atlas baker (tiny-skia, pure CPU) can be built concurrently; M7 (images) is independent of M5/M6. M5 (clip) must land before M6/M7 *complete* (they consume `clip_depth`), but their geometry develops against `clip_depth=0`. M8's batching is strictly last. The viewer (M9) only needs M2's `render_page_to_texture` seam.

---

## 7. Testing & acceptance

### 7.1 Unit tests
- **Subpath balancing** (`build_lyon_path`): MoveTo/LineTo/CurveTo/Close, implicit close, trailing-open flush, empty path → `None` (no draw).
- **Guard enumeration:** empty path → no draw; missing font → skip; missing image → skip; non-Solid paint → skip; final-alpha-0 → skip. One test per guard asserts zero geometry emitted (parity with the CPU's silent no-ops).
- **`quantize_premul`** reproduces straight `(c*255) as u8` for opaque AND tiny-skia integer premultiply `(ch*a+127)/255` for translucent.
- **Stroke clamps:** assert no corpus file triggers the miter `<1.0` clamp; assert `width==0` skips the draw (no geometry).
- **`BatchKey` equality/inequality**; **mode-equivalence** (Immediate vs Batched → 0 diff @ threshold 0).
- **Precision rule:** a glyph/image affine bake unit test asserts f32→f64→f32 ordering produces the same pixel coords as the CPU's `outline_to_pixel`/`render_image` for a known transform.
- **Shader load:** `include_wgsl!` validation runs at `create_shader_module` (compile-time WGSL gate).
- **Headless smoke:** `GpuContext::new_headless()` + a 1-draw render works with no surface under `pollster::block_on`.

### 7.2 Golden compare harness (`tests/acceptance.rs`, `cargo test --features gpu`)

For each corpus PDF: render `--backend cpu` and `--backend wgpu` at 150 DPI, then `zpdf compare cpu.png wgpu.png --threshold 16 --out diff.png`. Parse the printed `(P%)`; **assert `P < 1.0` per file.** `DIMENSION MISMATCH` exits 2 — catch dimension bugs first.

### 7.3 Corpus (`tests/corpus/*.pdf`, deterministic, synthesized — never web-pulled)

Each is a single page isolating one feature so a regression localizes:
- `rect_fills` (Fill NonZero/EvenOdd), `curves` (cubic fills/thin strokes), `strokes` (all caps/joins/miter), **`thin_strokes`** (0-width + 0.3px — gates the no-clamp stroke decision), **`translucent`** (50% alpha overlapping fills — gates D3 premultiply rounding).
- `text_latin` (TrueType/Type1 outline), `text_type3` (Type3, **path-only char-procs**), **`text_rotated`** (tm.b/c ≠ 0 — gates the atlas-vs-vector glyph decision).
- `image_rgb` (opaque RGB, **≥2 images** — gates the ImageScratch ring), `image_alpha` (**straight-alpha SMask** — gates in-shader premultiply), **`image_under_clip`** (image honors clip stencil).
- `clip` (nested), **`clip_in_blend`** (clip pushed inside a blend group), `blend` (Normal+Multiply).
- **`offset_mediabox`** (non-zero MediaBox origin — gates that the GPU ignores `page_rect.x0` exactly like the CPU's `to_pixel_x`).

### 7.4 Threshold rationale & expected deltas

`--threshold 16` (the CLI default), **not 0**: GPU↔CPU AA edge pixels legitimately differ; `16/255 ≈ 6%`/channel tolerates sub-pixel AA disagreement while still catching real errors (wrong color, missing primitive, flipped Y blow past 16 across huge pixel counts).

**Known accepted divergences and mitigations (built into the design):**
- **Fill/stroke/glyph edge AA** (analytic vs MSAA): MSAA 4x baseline. Glyphs are correct-by-construction via the **vector-fill baseline** (exact CPU coordinates); the R8 atlas only ships if it does not worsen the delta on axis-aligned text.
- **Clip edges:** stencil `Equal` is an **exact** reproduction of the CPU's binary (anti_alias=false) clip mask; only content-edge MSAA is sub-pixel.
- **Image sampling:** Nearest is mandatory (Linear = large diff, caught by `image_rgb`).
- **Premultiplied boundary:** write premultiplied RGB un-demultiplied on readback; over opaque white, premul==straight, passes. Straight-alpha images premultiply in-shader to match tiny-skia.
- **Dimension truncation:** GPU uses the identical truncating cast `(width*scale) as u32` (clamp `.max(1)`).
- **Color management:** none on the GPU — all color-space conversion is upstream in `zpdf-content`/`zpdf-color`; the backend sees final RGBA only.

**Triage workflow** when a file exceeds 1%: open the `diff.png` heatmap. Red *outline* = AA/edge (acceptable if <1%); red *solid region* = missing/mis-positioned primitive (bug); red *full-image wash* = format/gamma/flip bug (Bgra vs Rgba, sRGB, or Y-flip — check those first).

### 7.5 Build gates
`cargo build` (no features) → no wgpu/winit. `cargo build --features gpu-render` → no winit (`cargo tree -e features | rg winit` empty). `cargo build --example viewer` (in `zpdf-render-wgpu`) → winit present. `cargo clippy --workspace --features gpu-render`.

---

## 8. Risks & open questions (ranked)

1. **R1 — Small/rotated-text AA misses <1% (highest).** Thin and rotated glyphs are the dominant pixel-diff risk. *Mitigation (revised):* the **vector-fill baseline** renders glyphs at the CPU's exact coordinates and is correct-by-construction; rotated glyphs *always* use it. The R8 atlas is an axis-aligned-only optimization that ships only if measured not to worsen the delta. Escalation: subpixel-X bucket on the atlas; MSAA 4x→8x. Documented escape hatch (not preferred): raise `--threshold` for the GPU run.
2. **R2 — No GPU adapter on CI.** `request_adapter` errs on headless runners without a GPU/software rasterizer. *Mitigation:* CI provides a software adapter (lavapipe/llvmpipe Vulkan, or WARP on Windows); the harness sets `force_fallback_adapter` via `ZPDF_GPU_FORCE_FALLBACK=1`. **Never silently fall back to CPU** for `--backend wgpu` — error cleanly (`NoAdapter`). **Open question:** which software adapter the runner exposes — verify in M1.
3. **R3 — Texture size > adapter `max_texture_dimension_2d`.** Large pages/images at high DPI exceed limits → validation error; the CPU path has no such limit. *Mitigation:* `required_limits.max_texture_dimension_2d = adapter.limits().max_texture_dimension_2d` at init; beyond the adapter max, `warn!` + skip (image) or error (page). **Decided in M1:** raise limits, do not clamp DPI.
4. **R4 — Non-separable blend modes (Hue/Sat/Color/Luminosity).** SetLum/SetSat/ClipColor must match tiny-skia. *Mitigation:* documented v0.1 Normal-fallback with `warn!` (rare in real docs); ship exact helpers in M8 if time allows.
5. **R5 — Clip-edge fidelity.** *Resolved (downgraded):* CPU clip is binary (anti_alias=false) + multiplicative-binary intersect, so the stencil `Equal` test is an **exact** reproduction. R8 coverage-mask fallback **dropped from scope** unless `clip.pdf` empirically fails.
6. **R6 — Straight-alpha SMask divergence.** *Resolved:* the shader **premultiplies straight texels** (`rgb*=a`) to match tiny-skia's from_bytes→premultiply→composite; opaque images are a no-op. M7 confirms `apply_smask` emits `premultiplied:false`/`has_alpha:true`; `image_alpha.pdf` gates it. Re-evaluate if `zpdf-image` ever emits `premultiplied:true`.
7. **R7 — `ImageScratch` last-write-wins.** *Mitigation:* mandatory ring buffer (5.7); never ship the single-slot version. Covered by `image_rgb.pdf` with ≥2 images.
8. **R8 — Type3 scope.** Forces `zpdf-content` + a recursive interpret. *Decision:* include in M6a (mirrors CPU); defer-able with a no-op + `warn!` if schedule slips.
9. **R9 — Stencil8/format capability on exotic adapters.** *Mitigation:* M1 probes `Rgba8Unorm` `RENDER_ATTACHMENT|COPY_SRC` and Stencil8 `MULTISAMPLE_X4`; on failure, error cleanly (`Unsupported`) or drop to sample_count 1, never silently mis-render.

---

## 9. Effort estimate

| Milestone | Scope | Estimate |
|---|---|---|
| **M1** — Context + deps | Cargo edits, headless init (v29 fields, raised limits, fallback knob, format probes), MSAA+Stencil8 negotiation, renderer `<'a>`/builders, `GpuTexture{data}` | **2 days** |
| **M2** — Target + readback | PageTarget (shared sample_count), render-pass field set, pre-quantized clear, captured-Result readback, blank-page parity | **1.5 days** |
| **M3** — CLI + transform + WGSL scaffold | hand-rolled `--backend` arm + validation, features, `save_rgba`, page uniform, vertex structs, pipeline-layout/pipeline v29 fields, pixel→NDC | **1.5 days** |
| **M4** — Fills + strokes | lyon tessellation, `solid_fill` pipeline, reconciled clamps, dash-ignore, translucent/thin corpus | **2.5–3 days** |
| **M5** — Clip | stencil stamp/test/rebuild (R8 fallback dropped), content stencil test wiring | **1.5–2 days** |
| **M6a** — Glyphs (vector) + Type3 | vector-fill baseline (exact `outline_to_pixel`, f32→f64→f32), Type3 expansion, guards | **3 days** |
| **M6b** — Glyph atlas (optional) | R8 atlas + shelf packer + LRU, axis-aligned gate, measured promote | **2–3 days** |
| **M7** — Images | TextureCache, verbatim upload + in-shader premultiply, affine (both branches), Nearest, ring buffer, clip-test, `apply_smask` confirm | **2.5 days** |
| **M8** — Blend + batching | RenderLayer stack, clip⇄blend interaction, 16-mode composite WGSL, premultiplied contract, BatchBuilder | **4 days** |
| **M9** — Viewer + acceptance | winit ApplicationHandler, tile cache, blit, corpus (14 PDFs), harness, gate-checks, fps report | **3 days** |
| **Integration/triage buffer** | cross-subsystem parity debugging via heatmaps, MSAA tuning | **3–4 days** |
| **Total** | | **≈ 27–32 working days** (~5.5–6.5 weeks for one engineer) |

**Critical-path subset to a demonstrable end-to-end render** (M1→M2→M3→M4): **~7 days** yields `--backend wgpu` rendering fills/strokes at <1%. Text (M6a vector baseline) and the full corpus gate land by ~day 18; the optional atlas, blend/batching, and viewer polish fill the remainder.

---

## Addressed critiques

**Blockers/majors (all folded in):**

- **wgpu-correctness [major] `immediate_size` on wrong descriptor:** moved to `PipelineLayoutDescriptor`; `RenderPipelineDescriptor` now lists exactly `label, layout, vertex, primitive, depth_stencil, multisample, fragment, multiview_mask:None, cache:None`; dropped the "replaces push_constant_ranges" claim (§5.5).
- **wgpu-correctness [major] `get_mapped_range()` consumed as value:** now `get_mapped_range().map_err(Readback)?`; the map callback's `Result` is captured (not discarded) and `device.poll()`'s `Result` mapped to `Poll`; `unmap()` added (§5.2).
- **wgpu-correctness [major] Color f32→f64 clear with no conversion:** `wgpu::Color` built channel-wise with CPU pre-quantization `((c*255) as u8) as f64/255` for byte-exact background parity (§5.2).
- **contract-correctness [major] D3 premultiply double-quantization:** revised `quantize_premul` to reproduce tiny-skia's integer premultiply `(ch*a+127)/255`; opaque path unchanged (exact); added `translucent.pdf` corpus gate (§5.3, §7.3).
- **completeness [major] CLI is not clap:** DECISION D8 threads `--backend` into the existing hand-rolled positional parser with explicit value validation (typo errors exit 2, not silent CPU), adds the free `save_rgba` helper, notes the other five subcommands are untouched (§5.10, §2.4).
- **completeness [major] stroke clamps diverge from oracle:** removed `MIN_STROKE_PX`; width-0 now **skips** (matches tiny-skia empty result), sub-pixel widths pass through; miter clamps only `<1.0` with a test asserting no corpus file hits it; gated by `thin_strokes.pdf` (§5.4, §7.1, §7.3).
- **completeness [major] glyph atlas vs exact CPU placement:** DECISION D6 revised — **vector-fill baseline is the oracle** (M6a, exact coordinates, correct-by-construction); R8 atlas demoted to an axis-aligned-only, measurement-gated optimization (M6b); rotated/sheared glyphs always use the vector path; added `text_rotated.pdf` (§5.6, §6, §7.3, R1).

**Minors addressed where cheap:** glyph fragment NaN (straight color + cov premultiply, never divide by alpha, §5.5); `request_adapter` `map_err(|_|)` unused-var (§5.1); `bind_group_layouts: &[Some(&_)]` + `set_bind_group(Some(..))` (§5.5); render-pass `depth_slice:None`/`multiview_mask:None`/`depth_ops:None`/MSAA resolve `StoreOp::Discard` (§5.2); Stencil8 created at color `sample_count` (§5.2/§5.8); map-callback Result capture (§5.2); rotated-glyph atlas caveat → vector fallback (§5.6, R1); straight-alpha image premultiply-in-shader, R6 wording reconciled (§5.7, R6); f32→f64→f32 precision parity rule (§4, §5.6, §5.7); clip⇄blend-group stack interaction specified + `clip_in_blend.pdf` (§5.8, §7.3); mode-equivalence weakened to "0 diff @ threshold 0" (§5.9); clip R8 fallback dropped as unnecessary (stencil is exact, §5.8, R5); `force_fallback_adapter` for CI (§5.1, R2); Stencil8/format capability probe (§5.1, R9); guard enumeration test (§7.1); `offset_mediabox.pdf` for x0-ignored (§7.3); `image_under_clip.pdf` (§7.3); opaque-background readback invariant (§5.2); upstream color-management assertion (§5.0); wgpu version note 29.0.0-verified/29.0.3-compatible (§2.1).

**Rejected/no-op:** the `GpuTexture`/`WgpuRenderer<'a>` redefinition was flagged only to confirm safety — no change needed (stub has zero real callers; glob re-export carries the new types). The `bytemuck` alignment note required no change (`PageUniform` is 16B; vertex structs need no uniform alignment).