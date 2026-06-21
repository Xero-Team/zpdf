// Soft-mask apply: pre-multiply a transparency group's resolved layer by the
// per-pixel coverage of an ExtGState /SMask before it is composited. Coverage is
// the mask group's luminosity (over the /BC backdrop baked into the mask layer
// clear) for a luminosity mask, or its alpha for an alpha mask. Drawn as a
// fullscreen quad into a fresh scratch layer (a pass cannot sample its target).
//
// The coverage is sampled at `coord - (dx, dy)` so one mask built for a tiling
// pattern cell can be reused at every cell (the cell CTMs differ only by this
// device-pixel translation); reads outside the built mask take the unpainted
// value. The reduced coverage is then run through the /TR transfer LUT.

struct Page {
    w_px: f32,
    h_px: f32,
    scale: f32,
    page_height: f32,
};
struct MaskU {
    kind: u32,          // 1 = luminosity, 2 = alpha
    dx: i32,            // device-pixel coverage offset: sample at coord - (dx, dy)
    dy: i32,
    pad0: u32,
    backdrop_luma: f32, // coverage where the offset reads outside the built mask
    pad1: f32,
    pad2: f32,
    pad3: f32,
    lut: array<vec4<u32>, 16>, // /TR transfer LUT, 256 bytes packed 4 per u32
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

// Rec.601 luma, matching the CPU oracle (zpdf-render-cpu `rasterize_soft_mask`).
// NB: this is deliberately NOT the (0.3,0.59,0.11) used by the W3C non-separable
// blend modes in composite.wgsl — /SMask luminosity tracks the tiny-skia oracle.
fn lum(c: vec3<f32>) -> f32 {
    return dot(c, vec3<f32>(0.299, 0.587, 0.114));
}

// /TR transfer LUT lookup. Byte `i` is packed at lut[i/16][(i/4)%4], in the
// `8*(i%4)`-th byte of that u32 (little-endian, matching the Rust packing).
fn transfer(i: u32) -> f32 {
    let v = mu.lut[i / 16u];
    let comp = (i / 4u) % 4u;
    var word: u32;
    if (comp == 0u) {
        word = v.x;
    } else if (comp == 1u) {
        word = v.y;
    } else if (comp == 2u) {
        word = v.z;
    } else {
        word = v.w;
    }
    let b = (word >> (8u * (i % 4u))) & 0xFFu;
    return f32(b) / 255.0;
}

@fragment
fn fs_mask(in: VsOut) -> @location(0) vec4<f32> {
    let coord = vec2<i32>(i32(in.clip_pos.x), i32(in.clip_pos.y));
    let g = textureLoad(group_tex, coord, 0); // group (premultiplied), at dest

    // Sample coverage at the destination shifted by the reuse offset. Reads
    // outside the built mask take the unpainted value — the /BC backdrop for a
    // luminosity mask, 0 for an alpha mask — which is what a fresh raster yields
    // there (the CPU oracle fills vacated strips with the same value).
    let src = coord - vec2<i32>(mu.dx, mu.dy);
    let dims = vec2<i32>(textureDimensions(mask_tex));
    var cov: f32;
    if (src.x < 0 || src.y < 0 || src.x >= dims.x || src.y >= dims.y) {
        if (mu.kind == 1u) {
            cov = mu.backdrop_luma;
        } else {
            cov = 0.0;
        }
    } else {
        let m = textureLoad(mask_tex, src, 0); // mask layer (premultiplied)
        if (mu.kind == 1u) {
            // Luminosity: the mask layer is opaque (cleared to /BC), so its
            // straight colour equals its premultiplied colour; coverage = luminosity.
            var rgb = m.rgb;
            if (m.a > 0.0) {
                rgb = m.rgb / m.a;
            }
            cov = lum(rgb);
        } else {
            cov = m.a; // alpha mask
        }
    }

    // Run the coverage through the /TR transfer function (identity LUT when
    // /TR is absent), exactly as the CPU oracle does after reducing coverage.
    // `floor(x + 0.5)` is round-half-away-from-zero for the non-negative clamped
    // coverage, matching Rust `f32::round`; WGSL `round()` is ties-to-even and
    // would pick a different LUT slot on exact x.5 ties.
    let idx = u32(floor(clamp(cov, 0.0, 1.0) * 255.0 + 0.5));
    cov = transfer(idx);

    // Scaling premultiplied RGBA by coverage applies the mask to the group.
    return g * cov;
}
