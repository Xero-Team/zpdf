// Solid fill / stroke / Type3 shader.
// Vertices arrive in device-pixel space (origin top-left, +Y down); convert to
// clip space here. Color is already premultiplied and integer-quantized on the host.

struct Page {
    w_px: f32,
    h_px: f32,
    scale: f32,
    page_height: f32,
};

@group(0) @binding(0)
var<uniform> page: Page;

struct VsIn {
    @location(0) pos: vec2<f32>,
    @location(1) color: vec4<f32>,
};

struct VsOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) color: vec4<f32>,
};

fn pixel_to_ndc(px: f32, py: f32) -> vec4<f32> {
    // Divide by the integer framebuffer dims (carried in the uniform) so edges stay
    // bit-aligned with the CPU buffer.
    return vec4<f32>(2.0 * px / page.w_px - 1.0, 1.0 - 2.0 * py / page.h_px, 0.0, 1.0);
}

@vertex
fn vs_pixel(in: VsIn) -> VsOut {
    var out: VsOut;
    out.clip_pos = pixel_to_ndc(in.pos.x, in.pos.y);
    out.color = in.color;
    return out;
}

@fragment
fn fs_solid(in: VsOut) -> @location(0) vec4<f32> {
    // Already premultiplied; emit as-is for premultiplied source-over blending.
    return in.color;
}
