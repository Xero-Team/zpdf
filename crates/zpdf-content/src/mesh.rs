//! Mesh shading decode + tessellation (ISO 32000-1 §8.7.4.5.5–8): the four
//! mesh shading types, all of which are stream objects whose bytes pack a
//! bit-stream of vertices (types 4/5) or patches (types 6/7).
//!
//! - **Type 4** — free-form Gouraud triangle mesh: per-vertex edge flag drives a
//!   triangle strip.
//! - **Type 5** — lattice-form Gouraud mesh: `/VerticesPerRow` rows triangulated
//!   pairwise.
//! - **Type 6** — Coons patch mesh: 12 boundary control points per patch; the
//!   surface is evaluated directly as `S = SC + SD − SB`.
//! - **Type 7** — tensor-product patch mesh: 16 control points (12 boundary + 4
//!   interior); a bicubic tensor Bézier surface.
//!
//! [`decode_mesh`] returns triangles in **shading space** with per-vertex RGB
//! already resolved (via the caller's `resolve` closure, which applies any
//! `/Function` then the colour space). The interpreter transforms the vertices
//! to page space and wraps them in a [`crate::shading::ShadingDef`]; both
//! backends then consume the rasterized image identically.
//!
//! Byte alignment (the one place reference implementations disagree): per the
//! spec + Ghostscript + SerenityOS, each **vertex** is padded to a byte boundary
//! for types 4/5, and each **patch** for types 6/7. We align accordingly;
//! `pdf.js`/`pdfium` skip some of these and only stay correct on byte-multiple
//! field widths.

/// Shading-space mesh vertex: `(x, y, rgb)` with `rgb` already device-resolved.
type Sv = (f64, f64, [f32; 3]);
/// A shading-space triangle.
type STri = [Sv; 3];
/// A patch's 12 boundary control points + 4 corner colours, retained so the
/// next edge-shared patch can inherit its first edge.
type PatchState = ([(f64, f64); 12], [[f32; 3]; 4]);

/// Global ceiling on emitted triangles — a runaway/adversarial mesh truncates to
/// a partial render rather than exhausting memory (consistent with the
/// project's anti-hang budgets).
pub(crate) const MAX_TRIANGLES: usize = 2_000_000;
/// Safety cap on type-5 vertices read from a single stream.
const MAX_VERTICES: usize = 4_000_000;
/// Fixed subdivision per Coons/tensor patch (each cell → 2 triangles). The
/// `sh`/pattern raster cap (≈768px long side) bounds the visible benefit of a
/// finer grid; adaptive per-patch density is a possible future refinement.
const PATCH_SUBDIV: usize = 12;

/// Parameters parsed from the shading-stream dictionary.
pub(crate) struct MeshParams {
    /// `/BitsPerFlag` (types 4/6/7; unused for 5).
    pub bits_flag: u32,
    /// `/BitsPerCoordinate`.
    pub bits_coord: u32,
    /// `/BitsPerComponent`.
    pub bits_comp: u32,
    /// Colour values stored per vertex: 1 when a `/Function` is present, else the
    /// colour space's component count.
    pub n_color: usize,
    /// `/Decode`: `[xmin xmax ymin ymax c0min c0max …]`, length ≥ `4 + 2*n_color`.
    pub decode: Vec<f64>,
    /// `/VerticesPerRow` (type 5 only).
    pub vertices_per_row: usize,
}

/// MSB-first bit reader over the decoded stream bytes.
struct BitReader<'a> {
    bytes: &'a [u8],
    pos: usize,
    buf: u64,
    n: u32,
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            pos: 0,
            buf: 0,
            n: 0,
        }
    }

    /// Read `bits` (1..=32) MSB-first, or `None` at end of stream.
    fn read(&mut self, bits: u32) -> Option<u32> {
        if bits == 0 || bits > 32 {
            return None;
        }
        while self.n < bits {
            let &b = self.bytes.get(self.pos)?;
            self.pos += 1;
            self.buf = (self.buf << 8) | b as u64;
            self.n += 8;
        }
        self.n -= bits;
        let r = ((self.buf >> self.n) & ((1u64 << bits) - 1)) as u32;
        self.buf &= (1u64 << self.n) - 1;
        Some(r)
    }

    /// Discard the partial byte so the next read starts on a byte boundary.
    fn align(&mut self) {
        self.buf = 0;
        self.n = 0;
    }

    /// No more data is available.
    fn eof(&self) -> bool {
        self.pos >= self.bytes.len() && self.n == 0
    }
}

/// Map a raw `bits`-wide integer to `[lo, hi]` (image-Decode rule, §8.9.5.2).
/// The divisor is computed in `u64` so the 32-bit case does not overflow.
#[inline]
fn dec(raw: u32, bits: u32, lo: f64, hi: f64) -> f64 {
    let denom = if bits < 32 {
        ((1u64 << bits) - 1) as f64
    } else {
        4_294_967_295.0 // 2^32 − 1
    };
    lo + (raw as f64 / denom) * (hi - lo)
}

/// Reader bound to the params + colour resolver.
struct MeshReader<'a, 'r> {
    r: BitReader<'a>,
    p: &'a MeshParams,
    resolve: &'r mut dyn FnMut(&[f64]) -> [f32; 3],
}

impl MeshReader<'_, '_> {
    fn flag(&mut self) -> Option<u32> {
        Some(self.r.read(self.p.bits_flag)? & 0b11)
    }

    fn point(&mut self) -> Option<(f64, f64)> {
        let xr = self.r.read(self.p.bits_coord)?;
        let yr = self.r.read(self.p.bits_coord)?;
        let d = &self.p.decode;
        Some((
            dec(xr, self.p.bits_coord, d[0], d[1]),
            dec(yr, self.p.bits_coord, d[2], d[3]),
        ))
    }

    fn color(&mut self) -> Option<[f32; 3]> {
        let n = self.p.n_color.min(32);
        let mut comps = [0f64; 32];
        for (i, slot) in comps.iter_mut().enumerate().take(n) {
            let raw = self.r.read(self.p.bits_comp)?;
            *slot = dec(
                raw,
                self.p.bits_comp,
                self.p.decode[4 + 2 * i],
                self.p.decode[5 + 2 * i],
            );
        }
        Some((self.resolve)(&comps[..n]))
    }

    fn vertex(&mut self) -> Option<Sv> {
        let (x, y) = self.point()?;
        let c = self.color()?;
        Some((x, y, c))
    }

    fn align(&mut self) {
        self.r.align();
    }
}

/// Decode a type 4/5/6/7 shading stream into shading-space triangles.
pub(crate) fn decode_mesh(
    shading_type: i64,
    data: &[u8],
    p: &MeshParams,
    resolve: &mut dyn FnMut(&[f64]) -> [f32; 3],
) -> Vec<STri> {
    if p.decode.len() < 4 + 2 * p.n_color || p.bits_coord == 0 || p.bits_comp == 0 {
        return Vec::new();
    }
    let mut mr = MeshReader {
        r: BitReader::new(data),
        p,
        resolve,
    };
    match shading_type {
        4 => decode_type4(&mut mr),
        5 => decode_type5(&mut mr),
        6 => decode_patches(&mut mr, false),
        7 => decode_patches(&mut mr, true),
        _ => Vec::new(),
    }
}

/// Type 4 — free-form Gouraud triangle strip.
fn decode_type4(mr: &mut MeshReader) -> Vec<STri> {
    let mut out = Vec::new();
    // Previous triangle's vertices in stream order (va, vb, vc).
    let mut prev: Option<[Sv; 3]> = None;
    loop {
        if out.len() >= MAX_TRIANGLES || mr.r.eof() {
            break;
        }
        let Some(flag) = mr.flag() else { break };
        match flag {
            0 => {
                // New, unconnected triangle: this vertex + two more whole vertex
                // records (their flags are read and ignored).
                let Some(v0) = mr.vertex() else { break };
                mr.align();
                if mr.flag().is_none() {
                    break;
                }
                let Some(v1) = mr.vertex() else { break };
                mr.align();
                if mr.flag().is_none() {
                    break;
                }
                let Some(v2) = mr.vertex() else { break };
                mr.align();
                out.push([v0, v1, v2]);
                prev = Some([v0, v1, v2]);
            }
            1 | 2 => {
                let Some([va, vb, vc]) = prev else { break }; // first record must be flag 0
                let Some(vd) = mr.vertex() else { break };
                mr.align();
                // f=1 shares side vbc → (vb, vc, vd); f=2 shares side vac → (va, vc, vd).
                let tri = if flag == 1 {
                    [vb, vc, vd]
                } else {
                    [va, vc, vd]
                };
                out.push(tri);
                prev = Some(tri);
            }
            _ => break, // flag out of range → stop
        }
    }
    out
}

/// Type 5 — lattice-form Gouraud mesh.
fn decode_type5(mr: &mut MeshReader) -> Vec<STri> {
    let m = mr.p.vertices_per_row;
    if m < 2 {
        return Vec::new();
    }
    let mut verts: Vec<Sv> = Vec::new();
    while !mr.r.eof() && verts.len() < MAX_VERTICES {
        let Some(v) = mr.vertex() else { break };
        mr.align();
        verts.push(v);
    }
    let rows = verts.len() / m; // drop a trailing partial row
    let mut out = Vec::new();
    for i in 0..rows.saturating_sub(1) {
        for j in 0..m - 1 {
            if out.len() + 2 > MAX_TRIANGLES {
                return out;
            }
            let q = i * m + j;
            out.push([verts[q], verts[q + 1], verts[q + m]]);
            out.push([verts[q + 1], verts[q + m], verts[q + m + 1]]);
        }
    }
    out
}

/// Boundary points + corner colours inherited from the previous patch for edge
/// flag 1/2/3: returns `(point indices into prev p1..p12, colour indices into
/// prev c1..c4)`. (ISO 32000-1 §8.7.4.5.7 Table 85.)
fn shared_edge(flag: u32) -> ([usize; 4], [usize; 2]) {
    match flag {
        1 => ([3, 4, 5, 6], [1, 2]),   // prev top edge p4..p7; colours c2,c3
        2 => ([6, 7, 8, 9], [2, 3]),   // prev right edge p7..p10; colours c3,c4
        _ => ([9, 10, 11, 0], [3, 0]), // flag 3: prev bottom p10,p11,p12,p1; colours c4,c1
    }
}

/// Types 6 (Coons) and 7 (tensor) — patch mesh. `tensor` selects 16-point
/// patches with full bicubic evaluation; otherwise 12-point Coons patches.
fn decode_patches(mr: &mut MeshReader, tensor: bool) -> Vec<STri> {
    let mut out = Vec::new();
    let mut prev: Option<PatchState> = None;
    loop {
        if out.len() >= MAX_TRIANGLES {
            break;
        }
        // Each patch begins on a byte boundary (discard the previous patch's pad).
        mr.align();
        if mr.r.eof() {
            break;
        }
        let Some(flag) = mr.flag() else { break };
        if flag > 3 {
            break;
        }
        let mut bp = [(0.0, 0.0); 12];
        let mut cols = [[0.0f32; 3]; 4];
        let mut interior = [(0.0, 0.0); 4];

        let start = if flag == 0 {
            0
        } else {
            let Some((pp, pc)) = prev else { break }; // first patch with flag≠0 → error
            let (pi, ci) = shared_edge(flag);
            for k in 0..4 {
                bp[k] = pp[pi[k]];
            }
            cols[0] = pc[ci[0]];
            cols[1] = pc[ci[1]];
            4
        };

        // New boundary control points: all 12 for flag 0, else p5..p12.
        let mut ok = true;
        for slot in bp.iter_mut().skip(start) {
            match mr.point() {
                Some(pt) => *slot = pt,
                None => {
                    ok = false;
                    break;
                }
            }
        }
        // Tensor patches read the 4 interior points (last, after the boundary).
        if ok && tensor {
            for slot in interior.iter_mut() {
                match mr.point() {
                    Some(pt) => *slot = pt,
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
        }
        // New colours: all 4 for flag 0, else c3,c4.
        let cstart = if flag == 0 { 0 } else { 2 };
        if ok {
            for slot in cols.iter_mut().skip(cstart) {
                match mr.color() {
                    Some(c) => *slot = c,
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
        }
        if !ok {
            break; // truncated patch → drop it
        }

        tessellate(
            &mut out,
            &bp,
            if tensor { Some(&interior) } else { None },
            &cols,
        );
        prev = Some((bp, cols));
    }
    out
}

/// Cubic Bézier point at `t` over four control points.
#[inline]
fn bez(b: [(f64, f64); 4], t: f64) -> (f64, f64) {
    let m = 1.0 - t;
    let (w0, w1, w2, w3) = (m * m * m, 3.0 * t * m * m, 3.0 * t * t * m, t * t * t);
    (
        w0 * b[0].0 + w1 * b[1].0 + w2 * b[2].0 + w3 * b[3].0,
        w0 * b[0].1 + w1 * b[1].1 + w2 * b[2].1 + w3 * b[3].1,
    )
}

#[inline]
fn lerp_pt(a: (f64, f64), b: (f64, f64), t: f64) -> (f64, f64) {
    (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t)
}

/// Bernstein cubic basis at `t`.
#[inline]
fn bern(t: f64) -> [f64; 4] {
    let m = 1.0 - t;
    [m * m * m, 3.0 * t * m * m, 3.0 * t * t * m, t * t * t]
}

/// Coons surface `S(u,v) = SC + SD − SB` from the 12 boundary points (1-based
/// `p1..p12` = `bp[0..11]`). Corners: `p1`@(0,0), `p4`@(0,1), `p7`@(1,1),
/// `p10`@(1,0); `u` runs along the top/bottom edges, `v` along the sides.
fn coons_surface(bp: &[(f64, f64); 12], u: f64, v: f64) -> (f64, f64) {
    let c1 = [bp[0], bp[11], bp[10], bp[9]]; // bottom (v=0), u:0→1
    let c2 = [bp[3], bp[4], bp[5], bp[6]]; // top (v=1), u:0→1
    let d1 = [bp[0], bp[1], bp[2], bp[3]]; // left (u=0), v:0→1
    let d2 = [bp[9], bp[8], bp[7], bp[6]]; // right (u=1), v:0→1
    let sc = lerp_pt(bez(c1, u), bez(c2, u), v);
    let sd = lerp_pt(bez(d1, v), bez(d2, v), u);
    let (p1, p4, p7, p10) = (bp[0], bp[3], bp[6], bp[9]);
    let sb = (
        (1.0 - u) * (1.0 - v) * p1.0 + (1.0 - u) * v * p4.0 + u * v * p7.0 + u * (1.0 - v) * p10.0,
        (1.0 - u) * (1.0 - v) * p1.1 + (1.0 - u) * v * p4.1 + u * v * p7.1 + u * (1.0 - v) * p10.1,
    );
    (sc.0 + sd.0 - sb.0, sc.1 + sd.1 - sb.1)
}

/// Tensor-product bicubic surface from the 12 boundary + 4 interior points.
/// Grid `g[iu][jv]` (`iu` along `u`, `jv` along `v`); interior `p13,p14,p15,p16`
/// land at `g[1][1], g[1][2], g[2][2], g[2][1]` respectively — each pole at its
/// `(u=iu/3, v=jv/3)` cell per the ISO §8.7.4.5.8 spatial grid.
fn tensor_surface(bp: &[(f64, f64); 12], ip: &[(f64, f64); 4], u: f64, v: f64) -> (f64, f64) {
    let mut g = [[(0.0, 0.0); 4]; 4];
    // Left column (u=0), v:0→1 = p1,p2,p3,p4.
    g[0] = [bp[0], bp[1], bp[2], bp[3]];
    // Right column (u=1), v:0→1 = p10,p9,p8,p7.
    g[3] = [bp[9], bp[8], bp[7], bp[6]];
    // Bottom row (v=0), u:0→1 = p1,p12,p11,p10 (corners already set).
    g[1][0] = bp[11];
    g[2][0] = bp[10];
    // Top row (v=1), u:0→1 = p4,p5,p6,p7.
    g[1][3] = bp[4];
    g[2][3] = bp[5];
    // Interior: p13,p14,p15,p16.
    g[1][1] = ip[0];
    g[1][2] = ip[1];
    g[2][2] = ip[2];
    g[2][1] = ip[3];

    let bu = bern(u);
    let bv = bern(v);
    let (mut x, mut y) = (0.0, 0.0);
    for (iu, col) in g.iter().enumerate() {
        for (jv, pt) in col.iter().enumerate() {
            let w = bu[iu] * bv[jv];
            x += w * pt.0;
            y += w * pt.1;
        }
    }
    (x, y)
}

/// Bilinear blend of the four corner colours over `(u,v)`.
#[inline]
fn bilin(c: &[[f32; 3]; 4], u: f64, v: f64) -> [f32; 3] {
    let (u, v) = (u as f32, v as f32);
    let w = [
        (1.0 - u) * (1.0 - v), // c1 @ (0,0)
        (1.0 - u) * v,         // c2 @ (0,1)
        u * v,                 // c3 @ (1,1)
        u * (1.0 - v),         // c4 @ (1,0)
    ];
    let mut out = [0.0f32; 3];
    for k in 0..3 {
        out[k] = w[0] * c[0][k] + w[1] * c[1][k] + w[2] * c[2][k] + w[3] * c[3][k];
    }
    out
}

/// Tessellate one patch into `PATCH_SUBDIV²·2` triangles.
fn tessellate(
    out: &mut Vec<STri>,
    bp: &[(f64, f64); 12],
    interior: Option<&[(f64, f64); 4]>,
    cols: &[[f32; 3]; 4],
) {
    let n = PATCH_SUBDIV;
    let eval = |u: f64, v: f64| match interior {
        Some(ip) => tensor_surface(bp, ip, u, v),
        None => coons_surface(bp, u, v),
    };
    // Sample an (n+1)×(n+1) lattice of (position, colour).
    let mut lat: Vec<Sv> = Vec::with_capacity((n + 1) * (n + 1));
    for iv in 0..=n {
        for iu in 0..=n {
            let u = iu as f64 / n as f64;
            let v = iv as f64 / n as f64;
            let (x, y) = eval(u, v);
            lat.push((x, y, bilin(cols, u, v)));
        }
    }
    let idx = |iu: usize, iv: usize| iv * (n + 1) + iu;
    for iv in 0..n {
        for iu in 0..n {
            if out.len() + 2 > MAX_TRIANGLES {
                return;
            }
            let a = lat[idx(iu, iv)];
            let b = lat[idx(iu + 1, iv)];
            let c = lat[idx(iu, iv + 1)];
            let d = lat[idx(iu + 1, iv + 1)];
            out.push([a, b, c]);
            out.push([b, d, c]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Treat decoded components as straight RGB (DeviceRGB) or replicated gray.
    fn rgb_resolver() -> impl FnMut(&[f64]) -> [f32; 3] {
        |c: &[f64]| {
            if c.len() >= 3 {
                [c[0] as f32, c[1] as f32, c[2] as f32]
            } else {
                [c[0] as f32; 3]
            }
        }
    }

    fn rgb_params(bits_flag: u32) -> MeshParams {
        MeshParams {
            bits_flag,
            bits_coord: 8,
            bits_comp: 8,
            n_color: 3,
            decode: vec![0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0],
            vertices_per_row: 0,
        }
    }

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }
    fn approx3(a: [f32; 3], b: [f32; 3]) -> bool {
        (0..3).all(|i| (a[i] - b[i]).abs() < 1e-4)
    }

    // TV1 — Type 4 single triangle (byte-aligned, 6 bytes/vertex).
    #[test]
    fn tv1_type4_single_triangle() {
        let data = [
            0x00, 0x00, 0x00, 0xFF, 0x00, 0x00, // V0 flag0 (0,0) red
            0x00, 0xFF, 0x00, 0x00, 0xFF, 0x00, // V1 flag0 (1,0) green
            0x00, 0x00, 0xFF, 0x00, 0x00, 0xFF, // V2 flag0 (0,1) blue
        ];
        let p = rgb_params(8);
        let tris = decode_mesh(4, &data, &p, &mut rgb_resolver());
        assert_eq!(tris.len(), 1);
        let t = tris[0];
        assert!(approx(t[0].0, 0.0) && approx(t[0].1, 0.0) && approx3(t[0].2, [1.0, 0.0, 0.0]));
        assert!(approx(t[1].0, 1.0) && approx(t[1].1, 0.0) && approx3(t[1].2, [0.0, 1.0, 0.0]));
        assert!(approx(t[2].0, 0.0) && approx(t[2].1, 1.0) && approx3(t[2].2, [0.0, 0.0, 1.0]));
    }

    // TV1b — Type 4 flag=1 continuation reuses (vb, vc).
    #[test]
    fn tv1b_type4_flag1_continuation() {
        let data = [
            0x00, 0x00, 0x00, 0xFF, 0x00, 0x00, // V0
            0x00, 0xFF, 0x00, 0x00, 0xFF, 0x00, // V1
            0x00, 0x00, 0xFF, 0x00, 0x00, 0xFF, // V2
            0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, // V3 flag1 (1,1) white
        ];
        let p = rgb_params(8);
        let tris = decode_mesh(4, &data, &p, &mut rgb_resolver());
        assert_eq!(tris.len(), 2);
        // New triangle = (V1, V2, V3).
        let t = tris[1];
        assert!(approx(t[0].0, 1.0) && approx(t[0].1, 0.0)); // V1 green
        assert!(approx(t[1].0, 0.0) && approx(t[1].1, 1.0)); // V2 blue
        assert!(approx(t[2].0, 1.0) && approx(t[2].1, 1.0) && approx3(t[2].2, [1.0, 1.0, 1.0]));
    }

    // TV2 — Type 5 lattice 2×2 → two triangles.
    #[test]
    fn tv2_type5_lattice() {
        let data = [
            0x00, 0x00, 0xFF, 0x00, 0x00, // V0 (0,0) red
            0xFF, 0x00, 0x00, 0xFF, 0x00, // V1 (1,0) green
            0x00, 0xFF, 0x00, 0x00, 0xFF, // V2 (0,1) blue
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, // V3 (1,1) white
        ];
        let mut p = rgb_params(0);
        p.vertices_per_row = 2;
        let tris = decode_mesh(5, &data, &p, &mut rgb_resolver());
        assert_eq!(tris.len(), 2);
        // Triangle A = (V0,V1,V2), B = (V1,V2,V3).
        assert!(approx(tris[0][0].0, 0.0) && approx(tris[0][0].1, 0.0));
        assert!(approx(tris[1][2].0, 1.0) && approx(tris[1][2].1, 1.0));
    }

    // TV3 — Type 6 single Coons patch (planar unit square). Center → mid-gray.
    #[test]
    fn tv3_type6_coons_planar() {
        // 12 boundary points along the unit-square border (u8 0/85/170/255).
        let pts: [(u8, u8); 12] = [
            (0, 0),
            (0, 85),
            (0, 170),
            (0, 255), // left p1..p4
            (85, 255),
            (170, 255),
            (255, 255), // top p5..p7
            (255, 170),
            (255, 85),
            (255, 0), // right p8..p10
            (170, 0),
            (85, 0), // bottom p11,p12
        ];
        let cols: [[u8; 3]; 4] = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 255]];
        let mut data = vec![0u8]; // flag 0
        for (x, y) in pts {
            data.push(x);
            data.push(y);
        }
        for c in cols {
            data.extend_from_slice(&c);
        }
        let p = rgb_params(8);
        let tris = decode_mesh(6, &data, &p, &mut rgb_resolver());
        assert_eq!(tris.len(), PATCH_SUBDIV * PATCH_SUBDIV * 2);
        // Reconstruct the surface directly to check the patch center.
        let bp: [(f64, f64); 12] =
            std::array::from_fn(|i| (pts[i].0 as f64 / 255.0, pts[i].1 as f64 / 255.0));
        let mid = coons_surface(&bp, 0.5, 0.5);
        assert!(approx(mid.0, 0.5) && approx(mid.1, 0.5), "center {mid:?}");
        let cc = [
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 1.0],
            [1.0, 1.0, 1.0],
        ];
        let mc = bilin(&cc, 0.5, 0.5);
        assert!(approx3(mc, [0.5, 0.5, 0.5]), "center color {mc:?}");
    }

    // TV4 — edge-flag shared-edge inheritance table.
    #[test]
    fn tv4_shared_edge_table() {
        assert_eq!(shared_edge(1), ([3, 4, 5, 6], [1, 2]));
        assert_eq!(shared_edge(2), ([6, 7, 8, 9], [2, 3]));
        assert_eq!(shared_edge(3), ([9, 10, 11, 0], [3, 0]));
    }

    // TV5 — per-vertex byte padding with non-byte-aligned field widths.
    // Vertex = flag(2)+x(4)+y(4)+gray(4) = 14 bits → padded to 16 (2 bytes).
    #[test]
    fn tv5_bit_reader_alignment() {
        // flag=0 (00), x=15 (1111), y=0 (0000), gray=8 (1000) → 0011_1100 0010_0000
        let data = [0x3C, 0x20, 0xAB]; // 3rd byte must be untouched after align
        let mut r = BitReader::new(&data);
        assert_eq!(r.read(2), Some(0)); // flag
        let x = dec(r.read(4).unwrap(), 4, 0.0, 1.0);
        let y = dec(r.read(4).unwrap(), 4, 0.0, 1.0);
        let g = dec(r.read(4).unwrap(), 4, 0.0, 1.0);
        assert!(approx(x, 1.0), "x={x}");
        assert!(approx(y, 0.0), "y={y}");
        assert!(approx(g, 8.0 / 15.0), "g={g}");
        r.align(); // discard the 2 pad bits → next read starts at byte 2
        assert_eq!(r.pos, 2, "align must land on the next byte boundary");
        assert_eq!(r.read(8), Some(0xAB));
    }

    // Tensor patch with non-planar interior must equal Coons at the boundary but
    // differ in the interior — and the interior point must move S the right way.
    #[test]
    fn type7_interior_points_used() {
        // Flat unit-square boundary (same as TV3 points).
        let pts: [(f64, f64); 12] = [
            (0.0, 0.0),
            (0.0, 1.0 / 3.0),
            (0.0, 2.0 / 3.0),
            (0.0, 1.0),
            (1.0 / 3.0, 1.0),
            (2.0 / 3.0, 1.0),
            (1.0, 1.0),
            (1.0, 2.0 / 3.0),
            (1.0, 1.0 / 3.0),
            (1.0, 0.0),
            (2.0 / 3.0, 0.0),
            (1.0 / 3.0, 0.0),
        ];
        // Interior at their natural bilinear positions → tensor == Coons (planar).
        let nat: [(f64, f64); 4] = [
            (1.0 / 3.0, 1.0 / 3.0), // p13 @ g[1][1] = (u=1/3, v=1/3)
            (1.0 / 3.0, 2.0 / 3.0), // p14 @ g[1][2] = (u=1/3, v=2/3)
            (2.0 / 3.0, 2.0 / 3.0), // p15 @ g[2][2] = (u=2/3, v=2/3)
            (2.0 / 3.0, 1.0 / 3.0), // p16 @ g[2][1] = (u=2/3, v=1/3)
        ];
        let s = tensor_surface(&pts, &nat, 0.5, 0.5);
        assert!(
            approx(s.0, 0.5) && approx(s.1, 0.5),
            "natural interior planar: {s:?}"
        );
        // Push p13 (nearest the (0,0) corner) in +x; the center must shift +x.
        let mut moved = nat;
        moved[0].0 += 0.3;
        let s2 = tensor_surface(&pts, &moved, 0.5, 0.5);
        assert!(
            s2.0 > s.0 + 0.01,
            "moving p13 +x must move center +x: {s2:?}"
        );
    }
}
