// Exact 3-seed affine tiling pattern composite shader:
// Evaluates the premultiplied affine backdrop map induced by any sequence of
// Normal / Multiply / Screen / OpacityGroup steps across arbitrary target alpha:
//   out_a   = a_t + (1 - a_t) * b.a
//   out_rgb = (1 - b.a) * t.rgb + b.a * k + clamp(f1 - k, 0, 1 - k) * b.rgb
// where:
//   trans_tex = cell evaluated over transparent (0, 0, 0, 0) -> (t.rgb, a_t)
//   tile0_tex = cell evaluated over opaque black (0, 0, 0, 1) -> (k, 1)
//   tile1_tex = cell evaluated over opaque white (1, 1, 1, 1) -> (f1, 1)
// Unpainted gaps have t = (0, 0, 0, 0), k = (0, 0, 0), f1 = (1, 1, 1) -> exact identity.

struct Page {
    w_px: f32,
    h_px: f32,
    scale: f32,
    page_height: f32,
};

@group(0) @binding(0) var<uniform> page: Page;
@group(1) @binding(0) var base_tex: texture_2d<f32>;
@group(1) @binding(1) var trans_tex: texture_2d<f32>;
@group(1) @binding(2) var tile0_tex: texture_2d<f32>;
@group(1) @binding(3) var tile1_tex: texture_2d<f32>;
@group(1) @binding(4) var samp: sampler;

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
fn vs_affine_tile(in: VsIn) -> VsOut {
    var out: VsOut;
    out.clip_pos = vec4<f32>(2.0 * in.pos.x / page.w_px - 1.0, 1.0 - 2.0 * in.pos.y / page.h_px, 0.0, 1.0);
    out.uv = in.uv;
    out.color = in.color;
    return out;
}

@fragment
fn fs_affine_tile(in: VsOut) -> @location(0) vec4<f32> {
    let coord = vec2<i32>(i32(in.clip_pos.x), i32(in.clip_pos.y));
    let b = textureLoad(base_tex, coord, 0);
    let t = textureSample(trans_tex, samp, in.uv);
    let k = textureSample(tile0_tex, samp, in.uv).rgb;
    let f1 = textureSample(tile1_tex, samp, in.uv).rgb;
    let m = clamp(f1 - k, vec3<f32>(0.0), vec3<f32>(1.0) - k);
    let out_a = clamp(t.a + (1.0 - t.a) * b.a, 0.0, 1.0);
    let out_rgb = clamp(
        (1.0 - b.a) * t.rgb + b.a * k + m * b.rgb,
        vec3<f32>(0.0),
        vec3<f32>(out_a),
    );
    let w = clamp(in.color.a, 0.0, 1.0);
    return mix(b, vec4<f32>(out_rgb, out_a), w);
}
