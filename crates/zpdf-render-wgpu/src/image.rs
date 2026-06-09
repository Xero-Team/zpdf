//! Image rendering: upload a decoded RGBA image to a texture and build the quad
//! that maps it onto the page, reproducing `CpuRenderer::render_image` exactly.

use zpdf_core::Matrix;
use zpdf_image::DecodedImage;

use crate::context::GpuContext;
use crate::transform::TexturedVertex;

/// Build the four quad corners (device pixels) + UVs for an image, reproducing the
/// affine in `render_image`. Coefficients are computed in f32, matching the CPU.
/// `alpha` is the per-draw opacity (`PixmapPaint::opacity`).
///
/// The image sample → unit-square → CTM → page-flip chain is a single affine that
/// is correct for every CTM (a negative `d` is just ordinary geometry, honored via
/// the fixed page Y-flip). See `CpuRenderer::render_image` for the derivation.
pub fn image_quad(
    iw: f32,
    ih: f32,
    tm: &Matrix,
    scale: f32,
    page_height: f32,
    alpha: f32,
) -> [TexturedVertex; 4] {
    let s = scale;
    let ph = page_height;
    let (a, b, c, d, e, f) = (
        tm.a as f32,
        tm.b as f32,
        tm.c as f32,
        tm.d as f32,
        tm.e as f32,
        tm.f as f32,
    );

    // screen = (t_sx*ix + t_kx*iy + t_tx, t_ky*ix + t_sy*iy + t_ty)
    let (t_sx, t_kx, t_ky, t_sy, t_tx, t_ty) = (
        a * s / iw,
        -c * s / ih,
        -b * s / iw,
        d * s / ih,
        (c + e) * s,
        (ph - d - f) * s,
    );

    let pt = |ix: f32, iy: f32| [t_sx * ix + t_kx * iy + t_tx, t_ky * ix + t_sy * iy + t_ty];
    let color = [1.0, 1.0, 1.0, alpha];
    [
        TexturedVertex { pos: pt(0.0, 0.0), uv: [0.0, 0.0], color },
        TexturedVertex { pos: pt(iw, 0.0), uv: [1.0, 0.0], color },
        TexturedVertex { pos: pt(iw, ih), uv: [1.0, 1.0], color },
        TexturedVertex { pos: pt(0.0, ih), uv: [0.0, 1.0], color },
    ]
}

/// Upload a decoded image to an `Rgba8Unorm` texture and build its bind group
/// (group 1: texture + the shared Nearest sampler). Bytes are uploaded verbatim —
/// no host premultiply (the shader/blend reproduce the CPU's compositing).
pub fn upload_image_bind_group(
    ctx: &GpuContext,
    img: &DecodedImage,
) -> wgpu::BindGroup {
    let size = wgpu::Extent3d {
        width: img.width,
        height: img.height,
        depth_or_array_layers: 1,
    };
    let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("zpdf-image"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    // queue.write_texture accepts an arbitrary row pitch (no 256-byte alignment).
    ctx.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
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
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("zpdf-image-bg"),
        layout: &ctx.pipelines.tex_bgl,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&ctx.pipelines.sampler),
            },
        ],
    })
}
