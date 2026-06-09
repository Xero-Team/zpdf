// Image shader. Quad vertices arrive in device-pixel space with UVs; the texture
// holds the decoder's straight RGBA, which the CPU oracle feeds to tiny-skia as
// premultiplied (no premultiply). We match that: sample and scale by draw.alpha.

struct Page {
    w_px: f32,
    h_px: f32,
    scale: f32,
    page_height: f32,
};

@group(0) @binding(0) var<uniform> page: Page;
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;

struct VsIn {
    @location(0) pos: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec4<f32>,
};

struct VsOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
};

@vertex
fn vs_textured(in: VsIn) -> VsOut {
    var out: VsOut;
    out.clip_pos = vec4<f32>(2.0 * in.pos.x / page.w_px - 1.0, 1.0 - 2.0 * in.pos.y / page.h_px, 0.0, 1.0);
    out.uv = in.uv;
    out.color = in.color;
    return out;
}

@fragment
fn fs_image(in: VsOut) -> @location(0) vec4<f32> {
    let t = textureSample(tex, samp, in.uv);
    // Texel treated as premultiplied (matches tiny-skia from_bytes on straight
    // decoder RGBA); scale by the per-draw opacity carried in color.a.
    return t * in.color.a;
}
