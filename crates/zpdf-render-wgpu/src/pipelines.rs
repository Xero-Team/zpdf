//! Render pipelines and clip-stencil state.
//!
//! All three pipelines share the solid shader (`vs_pixel` + `fs_solid`), the
//! `SolidVertex` layout, and the page-uniform bind group. They differ only in
//! stencil op and color-write mask:
//! - `solid_fill`  — content; test `stencil == ref`, never write, write color.
//! - `clip_write`  — stamp a clip; where `stencil == ref`, increment; no color.
//! - `clip_reset`  — fullscreen reset; write `0` everywhere; no color.

use crate::context::STENCIL_FORMAT;
use crate::transform::{SolidVertex, TexturedVertex};

pub struct Pipelines {
    /// Bind group 0: the page uniform (pixel->NDC params).
    pub page_bgl: wgpu::BindGroupLayout,
    /// Bind group 1 (textured pipeline): image texture + sampler.
    pub tex_bgl: wgpu::BindGroupLayout,
    /// Shared bilinear / ClampToEdge sampler (matches the CPU backend's
    /// `FilterQuality::Bilinear` image sampling).
    pub sampler: wgpu::Sampler,
    /// Solid fill / stroke / Type3 pipeline.
    pub solid_fill: wgpu::RenderPipeline,
    /// Clip-path stamp (increment stencil within the existing intersection).
    pub clip_write: wgpu::RenderPipeline,
    /// Fullscreen stencil reset to 0 (used when rebuilding on PopClip).
    pub clip_reset: wgpu::RenderPipeline,
    /// Image quad pipeline (samples a texture, premultiplied source-over).
    pub textured: wgpu::RenderPipeline,
    /// Bind group 1 (glyph atlas): R8 coverage texture + sampler.
    pub glyph_bgl: wgpu::BindGroupLayout,
    /// Glyph-atlas quad pipeline (samples coverage, scales the paint color).
    pub glyph: wgpu::RenderPipeline,
    /// Bind group 1 (composite): base texture + group texture + mode uniform.
    pub composite_bgl: wgpu::BindGroupLayout,
    /// Blend-group composite pipeline (fullscreen, reads base+group, 16 modes).
    pub composite: wgpu::RenderPipeline,
    /// Bind group 1 (mask-apply): group texture + mask texture + kind uniform.
    pub mask_apply_bgl: wgpu::BindGroupLayout,
    /// Soft-mask apply pipeline (fullscreen, group × mask coverage).
    pub mask_apply: wgpu::RenderPipeline,
}

impl Pipelines {
    pub fn build(
        device: &wgpu::Device,
        target_format: wgpu::TextureFormat,
        sample_count: u32,
    ) -> Self {
        let shader = device.create_shader_module(wgpu::include_wgsl!("shaders/solid.wgsl"));

        let page_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("zpdf-page-bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("zpdf-solid-layout"),
            bind_group_layouts: &[Some(&page_bgl)],
            immediate_size: 0,
        });

        let attrs = wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x4];

        let solid_fill = build_pipeline(
            device,
            &layout,
            &shader,
            &attrs,
            sample_count,
            target_format,
            "zpdf-solid-fill",
            content_stencil_state(),
            wgpu::ColorWrites::ALL,
        );
        let clip_write = build_pipeline(
            device,
            &layout,
            &shader,
            &attrs,
            sample_count,
            target_format,
            "zpdf-clip-write",
            clip_write_stencil_state(),
            wgpu::ColorWrites::empty(),
        );
        let clip_reset = build_pipeline(
            device,
            &layout,
            &shader,
            &attrs,
            sample_count,
            target_format,
            "zpdf-clip-reset",
            clip_reset_stencil_state(),
            wgpu::ColorWrites::empty(),
        );

        // --- Textured (image) pipeline: group 0 = page uniform, group 1 = texture. ---
        let tex_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("zpdf-tex-bgl"),
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
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("zpdf-image-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            // Bilinear filtering, matching the CPU backend's
            // `FilterQuality::Bilinear` image sampling (pdfium-quality scaling).
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        let tex_shader = device.create_shader_module(wgpu::include_wgsl!("shaders/textured.wgsl"));
        let tex_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("zpdf-textured-layout"),
            bind_group_layouts: &[Some(&page_bgl), Some(&tex_bgl)],
            immediate_size: 0,
        });
        let tex_attrs = wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32x4];
        let tex_vbl = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<TexturedVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &tex_attrs,
        };
        let textured = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("zpdf-textured"),
            layout: Some(&tex_layout),
            vertex: wgpu::VertexState {
                module: &tex_shader,
                entry_point: Some("vs_textured"),
                buffers: &[tex_vbl],
                compilation_options: Default::default(),
            },
            primitive: wgpu::PrimitiveState {
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: Some(content_stencil_state()),
            multisample: wgpu::MultisampleState {
                count: sample_count,
                ..Default::default()
            },
            fragment: Some(wgpu::FragmentState {
                module: &tex_shader,
                entry_point: Some("fs_image"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            multiview_mask: None,
            cache: None,
        });

        // --- Glyph atlas pipeline: group 0 = page uniform, group 1 = R8
        //     coverage atlas + sampler. Reuses the `TexturedVertex` layout
        //     (pos/uv/color) — a glyph quad is shaped identically to an image
        //     quad, only the fragment stage (coverage x color vs RGBA sample)
        //     and the bound texture's format differ. ---
        let glyph_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("zpdf-glyph-bgl"),
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
        let glyph_shader = device.create_shader_module(wgpu::include_wgsl!("shaders/glyph.wgsl"));
        let glyph_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("zpdf-glyph-layout"),
            bind_group_layouts: &[Some(&page_bgl), Some(&glyph_bgl)],
            immediate_size: 0,
        });
        let glyph_vbl = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<TexturedVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &tex_attrs,
        };
        let glyph = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("zpdf-glyph"),
            layout: Some(&glyph_layout),
            vertex: wgpu::VertexState {
                module: &glyph_shader,
                entry_point: Some("vs_glyph"),
                buffers: &[glyph_vbl],
                compilation_options: Default::default(),
            },
            primitive: wgpu::PrimitiveState {
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: Some(content_stencil_state()),
            multisample: wgpu::MultisampleState {
                count: sample_count,
                ..Default::default()
            },
            fragment: Some(wgpu::FragmentState {
                module: &glyph_shader,
                entry_point: Some("fs_glyph"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            multiview_mask: None,
            cache: None,
        });

        // --- Composite (blend-group) pipeline: group 0 = page uniform,
        //     group 1 = base texture + group texture + mode uniform. ---
        let composite_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("zpdf-composite-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let comp_shader =
            device.create_shader_module(wgpu::include_wgsl!("shaders/composite.wgsl"));
        let comp_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("zpdf-composite-layout"),
            bind_group_layouts: &[Some(&page_bgl), Some(&composite_bgl)],
            immediate_size: 0,
        });
        // Reuse the SolidVertex layout (the composite reads only `pos`).
        let comp_vbl = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<SolidVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &attrs,
        };
        let composite = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("zpdf-composite"),
            layout: Some(&comp_layout),
            vertex: wgpu::VertexState {
                module: &comp_shader,
                entry_point: Some("vs_composite"),
                buffers: &[comp_vbl],
                compilation_options: Default::default(),
            },
            primitive: wgpu::PrimitiveState {
                cull_mode: None,
                ..Default::default()
            },
            // Composite writes the whole layer regardless of stencil; never writes it.
            depth_stencil: Some(composite_stencil_state()),
            multisample: wgpu::MultisampleState {
                count: sample_count,
                ..Default::default()
            },
            fragment: Some(wgpu::FragmentState {
                module: &comp_shader,
                entry_point: Some("fs_composite"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    // The formula already composites the backdrop; write it verbatim.
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            multiview_mask: None,
            cache: None,
        });

        // --- Soft-mask apply pipeline: group 1 = group texture + mask texture +
        //     kind uniform. Same layout shape as composite. ---
        let mask_apply_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("zpdf-mask-apply-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let mask_shader =
            device.create_shader_module(wgpu::include_wgsl!("shaders/mask_apply.wgsl"));
        let mask_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("zpdf-mask-apply-layout"),
            bind_group_layouts: &[Some(&page_bgl), Some(&mask_apply_bgl)],
            immediate_size: 0,
        });
        let mask_vbl = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<SolidVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &attrs,
        };
        let mask_apply = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("zpdf-mask-apply"),
            layout: Some(&mask_layout),
            vertex: wgpu::VertexState {
                module: &mask_shader,
                entry_point: Some("vs_mask"),
                buffers: &[mask_vbl],
                compilation_options: Default::default(),
            },
            primitive: wgpu::PrimitiveState {
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: Some(composite_stencil_state()),
            multisample: wgpu::MultisampleState {
                count: sample_count,
                ..Default::default()
            },
            fragment: Some(wgpu::FragmentState {
                module: &mask_shader,
                entry_point: Some("fs_mask"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            multiview_mask: None,
            cache: None,
        });

        Self {
            page_bgl,
            tex_bgl,
            sampler,
            solid_fill,
            clip_write,
            clip_reset,
            textured,
            glyph_bgl,
            glyph,
            composite_bgl,
            composite,
            mask_apply_bgl,
            mask_apply,
        }
    }
}

/// Composite pipeline stencil state: always pass, never write — the fullscreen
/// composite covers the whole layer irrespective of clips (clips are re-stamped
/// afterward for subsequent draws).
fn composite_stencil_state() -> wgpu::DepthStencilState {
    let face = wgpu::StencilFaceState {
        compare: wgpu::CompareFunction::Always,
        fail_op: wgpu::StencilOperation::Keep,
        depth_fail_op: wgpu::StencilOperation::Keep,
        pass_op: wgpu::StencilOperation::Keep,
    };
    base_stencil_state(wgpu::StencilState {
        front: face,
        back: face,
        read_mask: 0xff,
        write_mask: 0x00,
    })
}

#[allow(clippy::too_many_arguments)]
fn build_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    shader: &wgpu::ShaderModule,
    attrs: &[wgpu::VertexAttribute],
    sample_count: u32,
    target_format: wgpu::TextureFormat,
    label: &str,
    depth_stencil: wgpu::DepthStencilState,
    color_writes: wgpu::ColorWrites,
) -> wgpu::RenderPipeline {
    let vbl = wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<SolidVertex>() as u64,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: attrs,
    };
    // Color writes only matter for content; clip pipelines mask all channels.
    let blend = if color_writes.is_empty() {
        None
    } else {
        Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING)
    };
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_pixel"),
            buffers: &[vbl],
            compilation_options: Default::default(),
        },
        primitive: wgpu::PrimitiveState {
            // lyon emits both windings; flipped CTMs invert them, so never cull.
            cull_mode: None,
            ..Default::default()
        },
        depth_stencil: Some(depth_stencil),
        multisample: wgpu::MultisampleState {
            count: sample_count,
            ..Default::default()
        },
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fs_solid"),
            targets: &[Some(wgpu::ColorTargetState {
                format: target_format,
                blend,
                write_mask: color_writes,
            })],
            compilation_options: Default::default(),
        }),
        multiview_mask: None,
        cache: None,
    })
}

fn base_stencil_state(stencil: wgpu::StencilState) -> wgpu::DepthStencilState {
    wgpu::DepthStencilState {
        format: STENCIL_FORMAT,
        // Stencil8 has no depth aspect: depth fields are None (wgpu 29.0.3).
        depth_write_enabled: None,
        depth_compare: None,
        stencil,
        bias: wgpu::DepthBiasState::default(),
    }
}

/// Content pipelines: pass where `stencil == reference` (active clip depth),
/// never modify the stencil. Reference 0 against a 0-cleared stencil passes
/// everywhere (unclipped).
fn content_stencil_state() -> wgpu::DepthStencilState {
    let face = wgpu::StencilFaceState {
        compare: wgpu::CompareFunction::Equal,
        fail_op: wgpu::StencilOperation::Keep,
        depth_fail_op: wgpu::StencilOperation::Keep,
        pass_op: wgpu::StencilOperation::Keep,
    };
    base_stencil_state(wgpu::StencilState {
        front: face,
        back: face,
        read_mask: 0xff,
        write_mask: 0x00,
    })
}

/// Clip stamp: increment where `stencil == reference` (n-1), accumulating the
/// intersection of nested clips. Clamps at 255.
fn clip_write_stencil_state() -> wgpu::DepthStencilState {
    let face = wgpu::StencilFaceState {
        compare: wgpu::CompareFunction::Equal,
        fail_op: wgpu::StencilOperation::Keep,
        depth_fail_op: wgpu::StencilOperation::Keep,
        pass_op: wgpu::StencilOperation::IncrementClamp,
    };
    base_stencil_state(wgpu::StencilState {
        front: face,
        back: face,
        read_mask: 0xff,
        write_mask: 0xff,
    })
}

/// Fullscreen reset: write the reference (0) everywhere, clearing the stencil
/// before re-stamping the remaining clips on pop.
fn clip_reset_stencil_state() -> wgpu::DepthStencilState {
    let face = wgpu::StencilFaceState {
        compare: wgpu::CompareFunction::Always,
        fail_op: wgpu::StencilOperation::Keep,
        depth_fail_op: wgpu::StencilOperation::Keep,
        pass_op: wgpu::StencilOperation::Replace,
    };
    base_stencil_state(wgpu::StencilState {
        front: face,
        back: face,
        read_mask: 0xff,
        write_mask: 0xff,
    })
}
