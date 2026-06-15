// Soft-mask apply: pre-multiply a transparency group's resolved layer by the
// per-pixel coverage of an ExtGState /SMask before it is composited. Coverage is
// the mask group's luminosity (over the /BC backdrop baked into the mask layer
// clear) for a luminosity mask, or its alpha for an alpha mask. Drawn as a
// fullscreen quad into a fresh scratch layer (a pass cannot sample its target).

struct Page {
    w_px: f32,
    h_px: f32,
    scale: f32,
    page_height: f32,
};
struct MaskU {
    kind: u32, // 1 = luminosity, 2 = alpha
};

@group(0) @binding(0) var<uniform> page: Page;
@group(1) @binding(0) var group_tex: texture_2d<f32>;
@group(1) @binding(1) var mask_tex: texture_2d<f32>;
@group(1) @binding(2) var<uniform> mu: MaskU;

struct VsIn {
    @location(0) pos: vec2<f32>,
    @location(1) color: vec4<f32>,
};
struct VsOut {
    @builtin(position) clip_pos: vec4<f32>,
};

@vertex
fn vs_mask(in: VsIn) -> VsOut {
    var out: VsOut;
    out.clip_pos = vec4<f32>(2.0 * in.pos.x / page.w_px - 1.0, 1.0 - 2.0 * in.pos.y / page.h_px, 0.0, 1.0);
    return out;
}

fn lum(c: vec3<f32>) -> f32 {
    return dot(c, vec3<f32>(0.3, 0.59, 0.11));
}

@fragment
fn fs_mask(in: VsOut) -> @location(0) vec4<f32> {
    let coord = vec2<i32>(i32(in.clip_pos.x), i32(in.clip_pos.y));
    let g = textureLoad(group_tex, coord, 0); // group (premultiplied)
    let m = textureLoad(mask_tex, coord, 0);  // mask layer (premultiplied)

    var cov: f32;
    if (mu.kind == 1u) {
        // Luminosity: the mask layer is opaque (cleared to /BC), so its straight
        // colour equals its premultiplied colour; coverage = luminosity.
        var rgb = m.rgb;
        if (m.a > 0.0) {
            rgb = m.rgb / m.a;
        }
        cov = clamp(lum(rgb), 0.0, 1.0);
    } else {
        cov = m.a; // alpha mask
    }

    // Scaling premultiplied RGBA by coverage applies the mask to the group.
    return g * cov;
}
