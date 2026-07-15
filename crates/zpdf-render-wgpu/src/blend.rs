//! Transparency-group support: an offscreen [`RenderLayer`] per group, composited
//! onto its parent with a blend mode. Used only by the layered render path (pages
//! containing PushBlendGroup); the common single-pass path never allocates these.

use crate::context::{COLOR_FORMAT, STENCIL_FORMAT};
use crate::WgpuRenderError;
use zpdf_display_list::BlendMode;

/// Index matching the `switch` in composite.wgsl and `BlendMode`'s declaration order.
pub fn blend_index(m: BlendMode) -> u32 {
    match m {
        BlendMode::Normal => 0,
        BlendMode::Multiply => 1,
        BlendMode::Screen => 2,
        BlendMode::Overlay => 3,
        BlendMode::Darken => 4,
        BlendMode::Lighten => 5,
        BlendMode::ColorDodge => 6,
        BlendMode::ColorBurn => 7,
        BlendMode::HardLight => 8,
        BlendMode::SoftLight => 9,
        BlendMode::Difference => 10,
        BlendMode::Exclusion => 11,
        BlendMode::Hue => 12,
        BlendMode::Saturation => 13,
        BlendMode::Color => 14,
        BlendMode::Luminosity => 15,
    }
}

/// Pool of full-page offscreen [`RenderLayer`]s with a free-list.
///
/// A page with many transparency groups would otherwise allocate one full-page
/// layer (MSAA color + resolve + stencil — hundreds of MB on a large media box)
/// per group and never free them, OOM-ing the device. The pool reuses a small
/// working set instead: once a group has been composited onto its parent, both
/// the group layer and the now-superseded parent are recycled. Recycled textures
/// stay alive in the pool until it is dropped (after `queue.submit`) and are
/// cleared before reuse, so reusing a just-read layer as a fresh render target
/// within the same command encoder is safe — wgpu inserts the usage barrier.
pub struct LayerPool {
    layers: Vec<RenderLayer>,
    free: Vec<usize>,
    width: u32,
    height: u32,
    sample_count: u32,
    max_layers: usize,
    max_bytes: u64,
}

pub(crate) fn estimated_layer_bytes(width: u32, height: u32, sample_count: u32) -> u64 {
    let pixels = width as u64 * height as u64;
    let color = pixels.saturating_mul(4).saturating_mul(sample_count as u64);
    let resolve = if sample_count > 1 {
        pixels.saturating_mul(4)
    } else {
        0
    };
    // Conservative: Stencil8 may be allocated at a wider hardware granularity.
    let stencil = pixels.saturating_mul(4).saturating_mul(sample_count as u64);
    color.saturating_add(resolve).saturating_add(stencil)
}

impl LayerPool {
    pub fn new(width: u32, height: u32, sample_count: u32, max_bytes: u64) -> Self {
        let per_layer = estimated_layer_bytes(width, height, sample_count).max(1);
        Self {
            layers: Vec::new(),
            free: Vec::new(),
            width,
            height,
            sample_count,
            max_layers: usize::try_from(max_bytes / per_layer).unwrap_or(usize::MAX),
            max_bytes,
        }
    }

    /// Get a layer to render into — a recycled one if available, else a fresh
    /// allocation. The caller must clear/init it before use.
    pub fn acquire(&mut self, device: &wgpu::Device) -> Result<usize, WgpuRenderError> {
        if let Some(i) = self.free.pop() {
            return Ok(i);
        }
        if self.layers.len() >= self.max_layers {
            return Err(WgpuRenderError::Unsupported(format!(
                "transparency layers exceed the {} MiB GPU working-set limit",
                self.max_bytes / (1024 * 1024)
            )));
        }
        self.layers.push(RenderLayer::new(
            device,
            self.width,
            self.height,
            self.sample_count,
        ));
        Ok(self.layers.len() - 1)
    }

    /// Return a layer to the free-list for reuse. Idempotent (ignores double-free).
    pub fn recycle(&mut self, idx: usize) {
        if !self.free.contains(&idx) {
            self.free.push(idx);
        }
    }

    pub fn get(&self, idx: usize) -> &RenderLayer {
        &self.layers[idx]
    }

    /// Number of distinct textures actually allocated (peak working set).
    #[allow(dead_code)]
    pub fn allocated(&self) -> usize {
        self.layers.len()
    }
}

/// An offscreen render target for a transparency group (or the page base / a
/// composite scratch). MSAA color is stored (so multiple passes can Load it across
/// nested-group boundaries) and resolved into a sampleable single-sample texture.
pub struct RenderLayer {
    color: wgpu::Texture,
    color_view: wgpu::TextureView,
    resolve: Option<wgpu::Texture>,
    resolve_view: Option<wgpu::TextureView>,
    #[allow(dead_code)]
    stencil: wgpu::Texture,
    stencil_view: wgpu::TextureView,
}

impl RenderLayer {
    pub fn new(device: &wgpu::Device, width: u32, height: u32, sample_count: u32) -> Self {
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let mk = |label, sc, format, usage| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size,
                mip_level_count: 1,
                sample_count: sc,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            })
        };

        // When single-sampled, the color texture is itself the sampleable/copy source.
        let color_usage = if sample_count > 1 {
            wgpu::TextureUsages::RENDER_ATTACHMENT
        } else {
            wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC
        };
        let color = mk("zpdf-layer-color", sample_count, COLOR_FORMAT, color_usage);
        let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());

        let (resolve, resolve_view) = if sample_count > 1 {
            let r = mk(
                "zpdf-layer-resolve",
                1,
                COLOR_FORMAT,
                wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_SRC,
            );
            let v = r.create_view(&wgpu::TextureViewDescriptor::default());
            (Some(r), Some(v))
        } else {
            (None, None)
        };

        let stencil = mk(
            "zpdf-layer-stencil",
            sample_count,
            STENCIL_FORMAT,
            wgpu::TextureUsages::RENDER_ATTACHMENT,
        );
        let stencil_view = stencil.create_view(&wgpu::TextureViewDescriptor::default());

        Self {
            color,
            color_view,
            resolve,
            resolve_view,
            stencil,
            stencil_view,
        }
    }

    /// The single-sample texture holding the resolved image (for compositing input
    /// or final readback).
    pub fn sampleable_view(&self) -> &wgpu::TextureView {
        self.resolve_view.as_ref().unwrap_or(&self.color_view)
    }

    pub fn sampleable_texture(&self) -> &wgpu::Texture {
        self.resolve.as_ref().unwrap_or(&self.color)
    }

    /// Color attachment that always resolves and stores MSAA (so a later pass can
    /// Load it). `load` clears (first pass) or loads (resume) the layer.
    pub fn color_attachment(
        &self,
        load: wgpu::LoadOp<wgpu::Color>,
    ) -> wgpu::RenderPassColorAttachment<'_> {
        wgpu::RenderPassColorAttachment {
            view: &self.color_view,
            depth_slice: None,
            resolve_target: self.resolve_view.as_ref(),
            ops: wgpu::Operations {
                load,
                store: wgpu::StoreOp::Store,
            },
        }
    }

    pub fn stencil_attachment(
        &self,
        load: wgpu::LoadOp<u32>,
    ) -> wgpu::RenderPassDepthStencilAttachment<'_> {
        wgpu::RenderPassDepthStencilAttachment {
            view: &self.stencil_view,
            depth_ops: None,
            stencil_ops: Some(wgpu::Operations {
                load,
                store: wgpu::StoreOp::Store,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_budget_scales_with_msaa_and_page_area() {
        assert_eq!(estimated_layer_bytes(100, 100, 1), 80_000);
        assert!(estimated_layer_bytes(100, 100, 4) > estimated_layer_bytes(100, 100, 1));
        assert!(estimated_layer_bytes(16_384, 16_384, 4) > 512 * 1024 * 1024);
    }

    #[test]
    fn layer_pool_uses_the_supplied_byte_budget() {
        let per_layer = estimated_layer_bytes(100, 100, 1);
        let pool = LayerPool::new(100, 100, 1, per_layer * 2 + per_layer / 2);
        assert_eq!(pool.max_layers, 2);
        assert_eq!(pool.max_bytes, per_layer * 2 + per_layer / 2);
    }
}
