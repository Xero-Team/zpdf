//! Per-page render target: MSAA color + optional resolve + Stencil8, plus the
//! padded staging buffer used to read pixels back to the CPU for PNG output.

use crate::context::{COLOR_FORMAT, STENCIL_FORMAT};
use crate::{GpuTexture, WgpuRenderError};

/// GPU resources for one page render. Rebuilt each `begin_page`.
pub struct PageTarget {
    pub width: u32,
    pub height: u32,
    pub sample_count: u32,

    /// Multisampled (or single-sample, when `sample_count == 1`) color attachment.
    pub color_msaa: wgpu::Texture,
    pub color_msaa_view: wgpu::TextureView,

    /// Single-sample resolve target; `Some` only under MSAA. Readback source when present.
    pub resolve: Option<wgpu::Texture>,
    pub resolve_view: Option<wgpu::TextureView>,

    /// Stencil attachment for clip masks (same sample count as color).
    /// The texture is held to keep it alive and for clip-rebuild in M5.
    #[allow(dead_code)]
    pub stencil: wgpu::Texture,
    pub stencil_view: wgpu::TextureView,

    /// Staging buffer for texture->CPU readback (rows padded to 256 bytes).
    pub readback: wgpu::Buffer,
    pub padded_bytes_per_row: u32,
    pub unpadded_bytes_per_row: u32,
}

impl PageTarget {
    pub fn new(device: &wgpu::Device, width: u32, height: u32, sample_count: u32) -> Self {
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };

        // When single-sampled, the color texture is itself the readback source and
        // needs COPY_SRC. Under MSAA it is render-only; the resolve carries COPY_SRC.
        let color_usage = if sample_count > 1 {
            wgpu::TextureUsages::RENDER_ATTACHMENT
        } else {
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC
        };
        let color_msaa = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("zpdf-color"),
            size,
            mip_level_count: 1,
            sample_count,
            dimension: wgpu::TextureDimension::D2,
            format: COLOR_FORMAT,
            usage: color_usage,
            view_formats: &[],
        });
        let color_msaa_view = color_msaa.create_view(&wgpu::TextureViewDescriptor::default());

        let (resolve, resolve_view) = if sample_count > 1 {
            let resolve = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("zpdf-resolve"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: COLOR_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let view = resolve.create_view(&wgpu::TextureViewDescriptor::default());
            (Some(resolve), Some(view))
        } else {
            (None, None)
        };

        let stencil = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("zpdf-stencil"),
            size,
            mip_level_count: 1,
            sample_count,
            dimension: wgpu::TextureDimension::D2,
            format: STENCIL_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let stencil_view = stencil.create_view(&wgpu::TextureViewDescriptor::default());

        let unpadded_bytes_per_row = width * 4;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded_bytes_per_row = unpadded_bytes_per_row.div_ceil(align) * align;
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("zpdf-readback"),
            size: padded_bytes_per_row as u64 * height as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        Self {
            width,
            height,
            sample_count,
            color_msaa,
            color_msaa_view,
            resolve,
            resolve_view,
            stencil,
            stencil_view,
            readback,
            padded_bytes_per_row,
            unpadded_bytes_per_row,
        }
    }

    /// The texture that holds the final single-sample image (resolve under MSAA,
    /// else the color texture itself).
    fn readback_source(&self) -> &wgpu::Texture {
        self.resolve.as_ref().unwrap_or(&self.color_msaa)
    }

    /// Color attachment for the page pass: clears to `clear`, resolves under MSAA.
    pub fn color_attachment(&self, clear: wgpu::Color) -> wgpu::RenderPassColorAttachment<'_> {
        wgpu::RenderPassColorAttachment {
            view: &self.color_msaa_view,
            depth_slice: None,
            resolve_target: self.resolve_view.as_ref(),
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(clear),
                // Under MSAA we only need the resolved image.
                store: if self.sample_count > 1 {
                    wgpu::StoreOp::Discard
                } else {
                    wgpu::StoreOp::Store
                },
            },
        }
    }

    /// Depth-stencil attachment: Stencil8 only (no depth aspect), cleared to 0.
    pub fn stencil_attachment(&self) -> wgpu::RenderPassDepthStencilAttachment<'_> {
        wgpu::RenderPassDepthStencilAttachment {
            view: &self.stencil_view,
            depth_ops: None,
            stencil_ops: Some(wgpu::Operations {
                load: wgpu::LoadOp::Clear(0),
                store: wgpu::StoreOp::Store,
            }),
        }
    }

    /// Record the texture->staging-buffer copy into `encoder` (the readback source
    /// is the resolve texture under MSAA, else the color texture).
    pub fn record_copy(&self, encoder: &mut wgpu::CommandEncoder) {
        self.record_copy_from(encoder, self.readback_source());
    }

    /// Record a copy from an arbitrary single-sample `src` (e.g. a blend layer's
    /// resolved texture) into the readback buffer. `src` must be `width x height`.
    pub fn record_copy_from(&self, encoder: &mut wgpu::CommandEncoder, src: &wgpu::Texture) {
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: src,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &self.readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(self.padded_bytes_per_row),
                    rows_per_image: Some(self.height),
                },
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
    }

    /// Map the staging buffer (after the copy has been submitted), strip per-row
    /// padding, and return a tight RGBA8 `GpuTexture`.
    pub fn map_and_strip(&self, device: &wgpu::Device) -> Result<GpuTexture, WgpuRenderError> {
        // Map the staging buffer and block until the copy completes.
        let slice = self.readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| WgpuRenderError::Poll(format!("{e}")))?;
        match rx.try_recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(WgpuRenderError::Readback(format!("map: {e}"))),
            Err(_) => {
                return Err(WgpuRenderError::Readback(
                    "map callback did not fire".into(),
                ))
            }
        }

        // wgpu 29.0.3: `get_mapped_range` returns the view directly (infallible here —
        // the map status was already checked above via the callback Result).
        let view = slice.get_mapped_range();

        // Strip per-row padding into a tight w*h*4 buffer.
        let row = self.unpadded_bytes_per_row as usize;
        let padded = self.padded_bytes_per_row as usize;
        let mut data = Vec::with_capacity(row * self.height as usize);
        for r in 0..self.height as usize {
            let start = r * padded;
            data.extend_from_slice(&view[start..start + row]);
        }
        drop(view);
        self.readback.unmap();

        Ok(GpuTexture {
            width: self.width,
            height: self.height,
            data,
        })
    }
}
