//! GPU context: instance / adapter / device / queue init for headless rendering.
//!
//! Created once and reused across pages (and, later, viewer frames). Adapter and
//! device requests are async; we block on them with `pollster` so the public API
//! stays synchronous and matches the CPU backend's ergonomics.

use crate::pipelines::Pipelines;
use crate::WgpuRenderError;

/// Color render-target format. **Linear `Rgba8Unorm`, deliberately not sRGB.**
/// tiny-skia (the CPU oracle) blends in stored-gamma byte space; an sRGB target
/// would blend in linear and every antialiased/blended pixel would diverge.
pub const COLOR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Stencil-only format used for clip masks.
pub const STENCIL_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Stencil8;

/// Shared GPU device state.
pub struct GpuContext {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    /// Always [`COLOR_FORMAT`]; carried for pipeline construction.
    pub target_format: wgpu::TextureFormat,
    /// 4 when MSAA-x4 is supported on both color and stencil, else 1.
    pub sample_count: u32,
    /// Adapter's `max_texture_dimension_2d`; pages/images beyond this are rejected.
    pub max_texture_dim: u32,
    /// Built-once render pipelines.
    pub pipelines: Pipelines,
}

impl GpuContext {
    /// Initialize a headless context (no window, no surface). Blocks on the async
    /// adapter/device requests via `pollster`.
    pub fn new_headless() -> Result<Self, WgpuRenderError> {
        let force_fallback = std::env::var("ZPDF_GPU_FORCE_FALLBACK")
            .map(|v| v == "1")
            .unwrap_or(false);
        pollster::block_on(Self::new_headless_async(force_fallback))
    }

    async fn new_headless_async(force_fallback: bool) -> Result<Self, WgpuRenderError> {
        let instance = wgpu::Instance::default();

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: force_fallback,
                compatible_surface: None,
            })
            .await
            .map_err(|_| WgpuRenderError::NoAdapter)?;

        // Raise the texture-dimension cap from the downlevel default (2048) to the
        // adapter maximum so large pages / high-DPI images don't trip a validation
        // error. Other limits stay at downlevel defaults for broad compatibility.
        let adapter_limits = adapter.limits();
        let max_texture_dim = adapter_limits.max_texture_dimension_2d;
        let mut required_limits = wgpu::Limits::downlevel_defaults();
        required_limits.max_texture_dimension_2d = max_texture_dim;

        // Note: 8x/16x MSAA (via TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES) was tried
        // and measured *worse* for dense CJK text — tiny-skia's AA blends in a
        // different space than the GPU's linear box-filter resolve, so finer
        // sampling exposes more midtone mismatches. 4x is the better operating point.
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("zpdf-device"),
                required_features: wgpu::Features::empty(),
                required_limits,
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
                memory_hints: wgpu::MemoryHints::MemoryUsage,
                trace: wgpu::Trace::Off,
            })
            .await
            .map_err(|e| WgpuRenderError::Wgpu(format!("request_device: {e}")))?;

        // Probe required usages so we error cleanly on exotic adapters rather than
        // silently mis-rendering.
        let color_feat = adapter.get_texture_format_features(COLOR_FORMAT);
        if !color_feat
            .allowed_usages
            .contains(wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC)
        {
            return Err(WgpuRenderError::Unsupported(
                "Rgba8Unorm RENDER_ATTACHMENT|COPY_SRC".into(),
            ));
        }
        let stencil_feat = adapter.get_texture_format_features(STENCIL_FORMAT);
        if !stencil_feat
            .allowed_usages
            .contains(wgpu::TextureUsages::RENDER_ATTACHMENT)
        {
            return Err(WgpuRenderError::Unsupported(
                "Stencil8 RENDER_ATTACHMENT".into(),
            ));
        }

        // MSAA requires color *and* stencil to share the sample count. Pick the
        // highest mutually-supported level: finer coverage steps bring the GPU's
        // sampled AA closer to tiny-skia's analytic AA, which matters for dense
        // text (CJK) where glyph-edge pixels dominate the CPU<->GPU diff (R1).
        // 4x is the spec-guaranteed max for Rgba8Unorm and the best AA match to the
        // CPU oracle (see note above). Fall back only if even 4x is unavailable.
        let supports = |flag| color_feat.flags.contains(flag) && stencil_feat.flags.contains(flag);
        let sample_count = if supports(wgpu::TextureFormatFeatureFlags::MULTISAMPLE_X4) {
            4
        } else if supports(wgpu::TextureFormatFeatureFlags::MULTISAMPLE_X2) {
            2
        } else {
            1
        };
        if sample_count < 4 {
            tracing::warn!(sample_count, "MSAA-x4 unavailable on this adapter (worse AA)");
        }

        tracing::debug!(
            adapter = ?adapter.get_info(),
            sample_count,
            max_texture_dim,
            "zpdf wgpu context initialized"
        );

        let pipelines = Pipelines::build(&device, COLOR_FORMAT, sample_count);

        Ok(Self {
            instance,
            adapter,
            device,
            queue,
            target_format: COLOR_FORMAT,
            sample_count,
            max_texture_dim,
            pipelines,
        })
    }
}
