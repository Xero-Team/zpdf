// Blend-group composite: read the backdrop (base) and source (group) resolved
// textures per-texel, apply the W3C/PDF blend formula, and write the premultiplied
// result. Drawn as a fullscreen quad into a fresh scratch layer (the pass cannot
// sample its own target). Mode index matches zpdf_display_list::BlendMode order.

struct Page {
    w_px: f32,
    h_px: f32,
    scale: f32,
    page_height: f32,
};
struct ModeU {
    id: u32,
};

@group(0) @binding(0) var<uniform> page: Page;
@group(1) @binding(0) var base_tex: texture_2d<f32>;
@group(1) @binding(1) var group_tex: texture_2d<f32>;
@group(1) @binding(2) var<uniform> mode: ModeU;

struct VsIn {
    @location(0) pos: vec2<f32>,
    @location(1) color: vec4<f32>,
};
struct VsOut {
    @builtin(position) clip_pos: vec4<f32>,
};

@vertex
fn vs_composite(in: VsIn) -> VsOut {
    var out: VsOut;
    out.clip_pos = vec4<f32>(2.0 * in.pos.x / page.w_px - 1.0, 1.0 - 2.0 * in.pos.y / page.h_px, 0.0, 1.0);
    return out;
}

// --- separable blend functions (operate per channel) ---
fn b_hardlight(cb: f32, cs: f32) -> f32 {
    if (cs <= 0.5) {
        return cb * 2.0 * cs;                 // multiply(cb, 2cs)
    }
    let s = 2.0 * cs - 1.0;
    return cb + s - cb * s;                   // screen(cb, 2cs-1)
}
fn b_softlight(cb: f32, cs: f32) -> f32 {
    if (cs <= 0.5) {
        return cb - (1.0 - 2.0 * cs) * cb * (1.0 - cb);
    }
    var d: f32;
    if (cb <= 0.25) {
        d = ((16.0 * cb - 12.0) * cb + 4.0) * cb;
    } else {
        d = sqrt(cb);
    }
    return cb + (2.0 * cs - 1.0) * (d - cb);
}
fn b_colordodge(cb: f32, cs: f32) -> f32 {
    if (cb <= 0.0) { return 0.0; }
    if (cs >= 1.0) { return 1.0; }
    return min(1.0, cb / (1.0 - cs));
}
fn b_colorburn(cb: f32, cs: f32) -> f32 {
    if (cb >= 1.0) { return 1.0; }
    if (cs <= 0.0) { return 0.0; }
    return 1.0 - min(1.0, (1.0 - cb) / cs);
}
fn separable(cb: vec3<f32>, cs: vec3<f32>, m: u32) -> vec3<f32> {
    switch (m) {
        case 1u: { return cb * cs; }                                  // Multiply
        case 2u: { return cb + cs - cb * cs; }                        // Screen
        case 3u: { return vec3(b_hardlight(cs.r, cb.r), b_hardlight(cs.g, cb.g), b_hardlight(cs.b, cb.b)); } // Overlay = HardLight(cs,cb)
        case 4u: { return min(cb, cs); }                              // Darken
        case 5u: { return max(cb, cs); }                              // Lighten
        case 6u: { return vec3(b_colordodge(cb.r, cs.r), b_colordodge(cb.g, cs.g), b_colordodge(cb.b, cs.b)); } // ColorDodge
        case 7u: { return vec3(b_colorburn(cb.r, cs.r), b_colorburn(cb.g, cs.g), b_colorburn(cb.b, cs.b)); }    // ColorBurn
        case 8u: { return vec3(b_hardlight(cb.r, cs.r), b_hardlight(cb.g, cs.g), b_hardlight(cb.b, cs.b)); }    // HardLight
        case 9u: { return vec3(b_softlight(cb.r, cs.r), b_softlight(cb.g, cs.g), b_softlight(cb.b, cs.b)); }    // SoftLight
        case 10u: { return abs(cb - cs); }                            // Difference
        case 11u: { return cb + cs - 2.0 * cb * cs; }                 // Exclusion
        default: { return cs; }                                       // Normal (0)
    }
}

// --- non-separable helpers (W3C) ---
fn lum(c: vec3<f32>) -> f32 {
    return dot(c, vec3<f32>(0.3, 0.59, 0.11));
}
fn clip_color(c: vec3<f32>) -> vec3<f32> {
    let l = lum(c);
    let n = min(min(c.r, c.g), c.b);
    let x = max(max(c.r, c.g), c.b);
    var r = c;
    if (n < 0.0) {
        r = l + (c - l) * l / (l - n);
    }
    if (x > 1.0) {
        r = l + (r - l) * (1.0 - l) / (x - l);
    }
    return r;
}
fn set_lum(c: vec3<f32>, l: f32) -> vec3<f32> {
    return clip_color(c + (l - lum(c)));
}
fn sat(c: vec3<f32>) -> f32 {
    return max(max(c.r, c.g), c.b) - min(min(c.r, c.g), c.b);
}
// Set saturation of c to s, preserving relative ordering (W3C SetSat).
fn set_sat(c: vec3<f32>, s: f32) -> vec3<f32> {
    let mn = min(min(c.r, c.g), c.b);
    let mx = max(max(c.r, c.g), c.b);
    if (mx > mn) {
        return (c - mn) * s / (mx - mn);
    }
    return vec3<f32>(0.0);
}
fn non_separable(cb: vec3<f32>, cs: vec3<f32>, m: u32) -> vec3<f32> {
    switch (m) {
        case 12u: { return set_lum(set_sat(cs, sat(cb)), lum(cb)); }   // Hue
        case 13u: { return set_lum(set_sat(cb, sat(cs)), lum(cb)); }   // Saturation
        case 14u: { return set_lum(cs, lum(cb)); }                     // Color
        case 15u: { return set_lum(cb, lum(cs)); }                     // Luminosity
        default: { return cs; }
    }
}

@fragment
fn fs_composite(in: VsOut) -> @location(0) vec4<f32> {
    let coord = vec2<i32>(i32(in.clip_pos.x), i32(in.clip_pos.y));
    let s = textureLoad(group_tex, coord, 0); // source (premultiplied)
    let d = textureLoad(base_tex, coord, 0);  // backdrop (premultiplied)

    let as_ = s.a;
    let ab = d.a;
    var cs = vec3<f32>(0.0);
    if (as_ > 0.0) { cs = s.rgb / as_; }
    var cb = vec3<f32>(0.0);
    if (ab > 0.0) { cb = d.rgb / ab; }

    var blended: vec3<f32>;
    if (mode.id >= 12u) {
        blended = non_separable(cb, cs, mode.id);
    } else {
        blended = separable(cb, cs, mode.id);
    }

    // W3C: Co = αs(1-αb)Cs + αs·αb·B(Cb,Cs) + (1-αs)·αb·Cb   (premultiplied)
    let co = as_ * (1.0 - ab) * cs + as_ * ab * blended + (1.0 - as_) * ab * cb;
    let ao = as_ + ab * (1.0 - as_);
    return vec4<f32>(co, ao);
}
