//! Transparency-group support: an offscreen [`RenderLayer`] per group, composited
//! onto its parent with a blend mode. Used only by the layered render path (pages
//! containing PushBlendGroup); the common single-pass path never allocates these.

use crate::context::{COLOR_FORMAT, STENCIL_FORMAT};
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
