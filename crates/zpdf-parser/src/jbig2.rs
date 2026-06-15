//! JBIG2Decode — ITU-T T.88 bi-level image decoding (embedded PDF profile).
//!
//! Pure Rust, zero deps. Covers the full set of region flavours seen in
//! real-world PDFs: segment headers, the MQ arithmetic decoder (Annex E),
//! generic regions (GB templates 0–3, with/without TPGDON), generic refinement
//! regions (GR templates 0–1, TPGRON), symbol dictionaries (arithmetic and
//! Huffman, with refinement/aggregation), text regions (arithmetic and Huffman,
//! with per-instance refinement), pattern dictionaries and halftone regions,
//! custom and standard Huffman tables (Annex B, tables B.1–B.15), page
//! assembly, and the PDF embedding rules (optional `/JBIG2Globals` stream
//! processed first). MMR-coded regions reuse the sibling CCITT Group 4 decoder.
//! A region that fails to decode renders blank with a warning rather than
//! failing the whole image; corrupt mandatory structures (segment headers,
//! page info) are hard errors.
//!
//! Output follows the PDF sample convention for 1-bpc images: packed MSB-first
//! rows of `ceil(width/8)` bytes with **black = 0** / white = 1. JBIG2's
//! native polarity is 1 = black, so bits are inverted when packing.

use std::collections::HashMap;
use std::rc::Rc;

use tracing::{debug, warn};
use zpdf_core::{Error, PdfDict, PdfObject, Result};

/// Output-size safety cap for the packed page bitmap (matches the sibling
/// CCITT decoder and the ParseLimits::max_stream_bytes default).
const MAX_OUTPUT: usize = 256 * 1024 * 1024;
/// Cap on any single bitmap's pixel count (page, region, or symbol). Internal
/// bitmaps spend one byte per pixel, so this also bounds working memory.
const MAX_BITMAP_PIXELS: usize = 1 << 28;
/// Plausibility bound on any declared width/height/placement (mirrors the
/// CCITT decoder's /Columns bound).
const MAX_DIMENSION: usize = 1 << 20;
/// Cap on the symbols a single dictionary may declare. The MQ decoder never
/// runs out of input (it feeds 1-bits forever), so every decode loop must be
/// bounded by a checked header-declared count, not by data exhaustion.
const MAX_SYMBOLS: usize = 1 << 18;
/// Cap on text-region symbol instances (same rationale as [`MAX_SYMBOLS`]).
const MAX_INSTANCES: usize = 1 << 22;

/// Parsed `/DecodeParms` for a JBIG2Decode filter.
pub struct Jbig2Params {
    /// Decoded bytes of the `/JBIG2Globals` stream, if any. The stream is an
    /// indirect reference the filter layer cannot chase, so the parser inlines
    /// its decoded bytes as a string before the dict reaches this layer.
    pub globals: Option<Vec<u8>>,
}

impl Jbig2Params {
    pub fn from_dict(params: Option<&PdfDict>) -> Self {
        let globals = params.and_then(|p| match p.get("JBIG2Globals") {
            Some(PdfObject::String(s)) => Some(s.0.clone()),
            Some(PdfObject::Stream(st)) => Some(st.data.to_vec()),
            Some(other) => {
                warn!(
                    "JBIG2Decode: /JBIG2Globals is {} (expected an inlined stream); ignored",
                    other.type_name()
                );
                None
            }
            None => None,
        });
        Jbig2Params { globals }
    }
}

/// Decode an embedded JBIG2 stream into packed 1-bpp rows (PDF polarity).
pub fn decode(data: &[u8], params: &Jbig2Params) -> Result<Vec<u8>> {
    let mut dec = Decoder::default();
    if let Some(globals) = &params.globals {
        dec.process_segments(globals)?;
    }
    dec.process_segments(data)?;
    dec.into_packed_output()
}

fn err(msg: impl std::fmt::Display) -> Error {
    Error::StreamDecode(format!("JBIG2Decode: {msg}"))
}

// ---------------------------------------------------------------------------
// Bitmaps — one byte per pixel (0 = white, 1 = black, JBIG2 native polarity).
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Bitmap {
    width: usize,
    height: usize,
    data: Vec<u8>,
}

impl Bitmap {
    fn new(width: usize, height: usize, fill: u8) -> Result<Self> {
        let pixels = width
            .checked_mul(height)
            .filter(|&p| p <= MAX_BITMAP_PIXELS)
            .ok_or_else(|| err(format!("bitmap {width}x{height} exceeds the pixel limit")))?;
        Ok(Self {
            width,
            height,
            data: vec![fill; pixels],
        })
    }

    /// Pixel value with all-white (0) outside the bitmap, as the generic
    /// decoding templates require at the edges.
    #[inline]
    fn get(&self, x: i64, y: i64) -> u8 {
        if x < 0 || y < 0 || x >= self.width as i64 || y >= self.height as i64 {
            0
        } else {
            self.data[y as usize * self.width + x as usize]
        }
    }

    #[inline]
    fn set(&mut self, x: usize, y: usize, v: u8) {
        self.data[y * self.width + x] = v;
    }
}

/// Combine `src` onto `dst` at (x0, y0) with a JBIG2 combination operator
/// (0 = OR, 1 = AND, 2 = XOR, 3 = XNOR, 4 = REPLACE), clipping to `dst`.
fn compose_bitmap(dst: &mut Bitmap, src: &Bitmap, x0: usize, y0: usize, op: u8) {
    for sy in 0..src.height {
        let py = y0 + sy;
        if py >= dst.height {
            break;
        }
        for sx in 0..src.width {
            let px = x0 + sx;
            if px >= dst.width {
                break;
            }
            let s = src.data[sy * src.width + sx];
            let d = &mut dst.data[py * dst.width + px];
            *d = match op {
                0 => *d | s,
                1 => *d & s,
                2 => *d ^ s,
                3 => 1 ^ (*d ^ s),
                _ => s, // REPLACE
            };
        }
    }
}

/// Place a halftone pattern onto a region bitmap at a possibly-negative origin
/// with a combination operator (0 = OR, 1 = AND, 2 = XOR, 3 = XNOR, 4 = REPLACE).
fn place_pattern(dst: &mut Bitmap, pat: &Bitmap, x0: i32, y0: i32, op: u8) {
    for sy in 0..pat.height as i32 {
        let py = y0 + sy;
        if py < 0 || py >= dst.height as i32 {
            continue;
        }
        for sx in 0..pat.width as i32 {
            let px = x0 + sx;
            if px < 0 || px >= dst.width as i32 {
                continue;
            }
            let s = pat.data[sy as usize * pat.width + sx as usize];
            let d = &mut dst.data[py as usize * dst.width + px as usize];
            *d = match op {
                0 => *d | s,
                1 => *d & s,
                2 => *d ^ s,
                3 => 1 ^ (*d ^ s),
                _ => s,
            };
        }
    }
}

/// Draw a symbol bitmap onto a text-region bitmap at a possibly-negative
/// origin with an SBCOMBOP operator (0 = OR, 1 = AND, 2 = XOR, 3 = XNOR).
fn draw_symbol(dst: &mut Bitmap, sym: &Bitmap, x0: i64, y0: i64, op: u8) {
    for sy in 0..sym.height as i64 {
        let py = y0 + sy;
        if py < 0 || py >= dst.height as i64 {
            continue;
        }
        for sx in 0..sym.width as i64 {
            let px = x0 + sx;
            if px < 0 || px >= dst.width as i64 {
                continue;
            }
            let s = sym.data[sy as usize * sym.width + sx as usize];
            let d = &mut dst.data[py as usize * dst.width + px as usize];
            *d = match op {
                0 => *d | s,
                1 => *d & s,
                2 => *d ^ s,
                _ => 1 ^ (*d ^ s),
            };
        }
    }
}

// ---------------------------------------------------------------------------
// MQ arithmetic decoder (T.88 Annex E, software conventions).
// ---------------------------------------------------------------------------

/// (Qe, NMPS, NLPS, SWITCH) — T.88 Table E.1.
#[rustfmt::skip]
const QE_TABLE: [(u16, u8, u8, u8); 47] = [
    (0x5601,  1,  1, 1), (0x3401,  2,  6, 0), (0x1801,  3,  9, 0), (0x0AC1,  4, 12, 0),
    (0x0521,  5, 29, 0), (0x0221, 38, 33, 0), (0x5601,  7,  6, 1), (0x5401,  8, 14, 0),
    (0x4801,  9, 14, 0), (0x3801, 10, 14, 0), (0x3001, 11, 17, 0), (0x2401, 12, 18, 0),
    (0x1C01, 13, 20, 0), (0x1601, 29, 21, 0), (0x5601, 15, 14, 1), (0x5401, 16, 14, 0),
    (0x5101, 17, 15, 0), (0x4801, 18, 16, 0), (0x3801, 19, 17, 0), (0x3401, 20, 18, 0),
    (0x3001, 21, 19, 0), (0x2801, 22, 19, 0), (0x2401, 23, 20, 0), (0x2201, 24, 21, 0),
    (0x1C01, 25, 22, 0), (0x1801, 26, 23, 0), (0x1601, 27, 24, 0), (0x1401, 28, 25, 0),
    (0x1201, 29, 26, 0), (0x1101, 30, 27, 0), (0x0AC1, 31, 28, 0), (0x09C1, 32, 29, 0),
    (0x08A1, 33, 30, 0), (0x0521, 34, 31, 0), (0x0441, 35, 32, 0), (0x02A1, 36, 33, 0),
    (0x0221, 37, 34, 0), (0x0141, 38, 35, 0), (0x0111, 39, 36, 0), (0x0085, 40, 37, 0),
    (0x0049, 41, 38, 0), (0x0025, 42, 39, 0), (0x0015, 43, 40, 0), (0x0009, 44, 41, 0),
    (0x0005, 45, 42, 0), (0x0001, 45, 43, 0), (0x5601, 46, 46, 0),
];

/// MQ decoder. Exhausted input feeds 0xFF bytes per the spec, so decoding
/// never fails mid-stream; the loops above are bounded by declared counts.
/// Per-context adaptive state is packed one byte per context: `index << 1 |
/// mps` (index < 47).
struct MqDecoder<'a> {
    data: &'a [u8],
    bp: usize,
    chigh: u32,
    clow: u32,
    ct: i32,
    a: u32,
}

impl<'a> MqDecoder<'a> {
    fn new(data: &'a [u8]) -> Self {
        // INITDEC (T.88 E.3.5).
        let mut d = MqDecoder {
            data,
            bp: 0,
            chigh: 0,
            clow: 0,
            ct: 0,
            a: 0,
        };
        d.chigh = d.byte_at(0) as u32;
        d.byte_in();
        d.chigh = ((d.chigh << 7) & 0xFFFF) | ((d.clow >> 9) & 0x7F);
        d.clow = (d.clow << 7) & 0xFFFF;
        d.ct -= 7;
        d.a = 0x8000;
        d
    }

    /// Past the end of the data the spec feeds 1-bits (0xFF bytes) forever.
    #[inline]
    fn byte_at(&self, i: usize) -> u8 {
        self.data.get(i).copied().unwrap_or(0xFF)
    }

    /// BYTEIN (T.88 E.3.4) with 0xFF stuffing.
    fn byte_in(&mut self) {
        if self.byte_at(self.bp) == 0xFF {
            if self.byte_at(self.bp + 1) > 0x8F {
                self.clow += 0xFF00;
                self.ct = 8;
            } else {
                self.bp += 1;
                self.clow += (self.byte_at(self.bp) as u32) << 9;
                self.ct = 7;
            }
        } else {
            self.bp += 1;
            self.clow += (self.byte_at(self.bp) as u32) << 8;
            self.ct = 8;
        }
        if self.clow > 0xFFFF {
            self.chigh += self.clow >> 16;
            self.clow &= 0xFFFF;
        }
    }

    /// DECODE (T.88 E.3.2): one bit under the adaptive context `cx[idx]`.
    fn decode(&mut self, cx: &mut [u8], idx: usize) -> u8 {
        let state = cx[idx];
        let mut i = (state >> 1) as usize;
        let mut mps = state & 1;
        let (qe16, nmps, nlps, switch) = QE_TABLE[i];
        let qe = qe16 as u32;

        let d;
        let mut a = self.a.wrapping_sub(qe);
        if self.chigh < qe {
            // LPS interval selected on the code register.
            if a < qe {
                // Conditional exchange: actually the MPS.
                a = qe;
                d = mps;
                i = nmps as usize;
            } else {
                a = qe;
                d = 1 ^ mps;
                if switch == 1 {
                    mps = d;
                }
                i = nlps as usize;
            }
        } else {
            self.chigh -= qe;
            if a & 0x8000 != 0 {
                self.a = a;
                return mps;
            }
            if a < qe {
                // Conditional exchange: actually the LPS.
                d = 1 ^ mps;
                if switch == 1 {
                    mps = d;
                }
                i = nlps as usize;
            } else {
                d = mps;
                i = nmps as usize;
            }
        }

        // RENORMD.
        loop {
            if self.ct == 0 {
                self.byte_in();
            }
            a <<= 1;
            self.chigh = ((self.chigh << 1) & 0xFFFF) | ((self.clow >> 15) & 1);
            self.clow = (self.clow << 1) & 0xFFFF;
            self.ct -= 1;
            if a & 0x8000 != 0 {
                break;
            }
        }
        self.a = a;
        cx[idx] = ((i as u8) << 1) | mps;
        d
    }
}

// ---------------------------------------------------------------------------
// Arithmetic integer decoding (T.88 Annex A).
// ---------------------------------------------------------------------------

/// Size of each IAx context array (PREV is clamped to 9 bits).
const INT_CTX_SIZE: usize = 512;

/// Read `n` bits under the running PREV context (A.2 step 2).
fn read_int_bits(mq: &mut MqDecoder, cx: &mut [u8], prev: &mut usize, n: u32) -> u32 {
    let mut v = 0u32;
    for _ in 0..n {
        let bit = mq.decode(cx, *prev) as u32;
        *prev = if *prev < 256 {
            (*prev << 1) | bit as usize
        } else {
            (((*prev << 1) | bit as usize) & 511) | 256
        };
        v = (v << 1) | bit;
    }
    v
}

/// IAx integer decoding procedure (T.88 A.2). Returns `None` for OOB.
fn decode_int(mq: &mut MqDecoder, cx: &mut [u8]) -> Option<i64> {
    let mut prev = 1usize;
    let sign = read_int_bits(mq, cx, &mut prev, 1);
    let value = if read_int_bits(mq, cx, &mut prev, 1) == 0 {
        read_int_bits(mq, cx, &mut prev, 2) as i64
    } else if read_int_bits(mq, cx, &mut prev, 1) == 0 {
        read_int_bits(mq, cx, &mut prev, 4) as i64 + 4
    } else if read_int_bits(mq, cx, &mut prev, 1) == 0 {
        read_int_bits(mq, cx, &mut prev, 6) as i64 + 20
    } else if read_int_bits(mq, cx, &mut prev, 1) == 0 {
        read_int_bits(mq, cx, &mut prev, 8) as i64 + 84
    } else if read_int_bits(mq, cx, &mut prev, 1) == 0 {
        read_int_bits(mq, cx, &mut prev, 12) as i64 + 340
    } else {
        read_int_bits(mq, cx, &mut prev, 32) as i64 + 4436
    };
    if sign == 1 {
        if value == 0 {
            return None; // OOB
        }
        return Some(-value);
    }
    Some(value)
}

/// IAID decoding procedure (T.88 A.3): a `code_len`-bit symbol ID. The
/// context array must hold `1 << (code_len + 1)` entries.
fn decode_iaid(mq: &mut MqDecoder, cx: &mut [u8], code_len: u32) -> usize {
    let mut prev = 1usize;
    for _ in 0..code_len {
        let bit = mq.decode(cx, prev) as usize;
        prev = (prev << 1) | bit;
    }
    prev - (1usize << code_len)
}

/// `ceil(log2(n))` for n ≥ 1.
fn ceil_log2(n: usize) -> u32 {
    usize::BITS - n.saturating_sub(1).leading_zeros()
}

// ---------------------------------------------------------------------------
// Generic region decoding (T.88 6.2).
// ---------------------------------------------------------------------------

struct GenericParams {
    template: u8,
    tpgdon: bool,
    /// Adaptive template pixels A1..A4 (templates 1–3 use only A1).
    at: [(i8, i8); 4],
}

#[inline]
fn at_pixel(bm: &Bitmap, x: i64, y: i64, at: (i8, i8)) -> usize {
    bm.get(x + at.0 as i64, y + at.1 as i64) as usize
}

/// GB template 0 context — 16 bits, bit layout per T.88 6.2.5.7 (AT pixels
/// A1..A4 occupy bits 4, 10, 11 and 15; nominal AT yields the spec figure).
#[inline]
fn context0(bm: &Bitmap, x: i64, y: i64, at: &[(i8, i8); 4]) -> usize {
    (bm.get(x - 1, y) as usize)
        | (bm.get(x - 2, y) as usize) << 1
        | (bm.get(x - 3, y) as usize) << 2
        | (bm.get(x - 4, y) as usize) << 3
        | at_pixel(bm, x, y, at[0]) << 4
        | (bm.get(x + 2, y - 1) as usize) << 5
        | (bm.get(x + 1, y - 1) as usize) << 6
        | (bm.get(x, y - 1) as usize) << 7
        | (bm.get(x - 1, y - 1) as usize) << 8
        | (bm.get(x - 2, y - 1) as usize) << 9
        | at_pixel(bm, x, y, at[1]) << 10
        | at_pixel(bm, x, y, at[2]) << 11
        | (bm.get(x + 1, y - 2) as usize) << 12
        | (bm.get(x, y - 2) as usize) << 13
        | (bm.get(x - 1, y - 2) as usize) << 14
        | at_pixel(bm, x, y, at[3]) << 15
}

/// GB template 1 context — 13 bits (A1 at bit 3).
#[inline]
fn context1(bm: &Bitmap, x: i64, y: i64, at: &[(i8, i8); 4]) -> usize {
    (bm.get(x - 1, y) as usize)
        | (bm.get(x - 2, y) as usize) << 1
        | (bm.get(x - 3, y) as usize) << 2
        | at_pixel(bm, x, y, at[0]) << 3
        | (bm.get(x + 2, y - 1) as usize) << 4
        | (bm.get(x + 1, y - 1) as usize) << 5
        | (bm.get(x, y - 1) as usize) << 6
        | (bm.get(x - 1, y - 1) as usize) << 7
        | (bm.get(x - 2, y - 1) as usize) << 8
        | (bm.get(x + 2, y - 2) as usize) << 9
        | (bm.get(x + 1, y - 2) as usize) << 10
        | (bm.get(x, y - 2) as usize) << 11
        | (bm.get(x - 1, y - 2) as usize) << 12
}

/// GB template 2 context — 10 bits (A1 at bit 2).
#[inline]
fn context2(bm: &Bitmap, x: i64, y: i64, at: &[(i8, i8); 4]) -> usize {
    (bm.get(x - 1, y) as usize)
        | (bm.get(x - 2, y) as usize) << 1
        | at_pixel(bm, x, y, at[0]) << 2
        | (bm.get(x + 1, y - 1) as usize) << 3
        | (bm.get(x, y - 1) as usize) << 4
        | (bm.get(x - 1, y - 1) as usize) << 5
        | (bm.get(x - 2, y - 1) as usize) << 6
        | (bm.get(x + 1, y - 2) as usize) << 7
        | (bm.get(x, y - 2) as usize) << 8
        | (bm.get(x - 1, y - 2) as usize) << 9
}

/// GB template 3 context — 10 bits, single reference row (A1 at bit 4).
#[inline]
fn context3(bm: &Bitmap, x: i64, y: i64, at: &[(i8, i8); 4]) -> usize {
    (bm.get(x - 1, y) as usize)
        | (bm.get(x - 2, y) as usize) << 1
        | (bm.get(x - 3, y) as usize) << 2
        | (bm.get(x - 4, y) as usize) << 3
        | at_pixel(bm, x, y, at[0]) << 4
        | (bm.get(x + 1, y - 1) as usize) << 5
        | (bm.get(x, y - 1) as usize) << 6
        | (bm.get(x - 1, y - 1) as usize) << 7
        | (bm.get(x - 2, y - 1) as usize) << 8
        | (bm.get(x - 3, y - 1) as usize) << 9
}

/// Context builder for one GB template: (bitmap, x, y, AT pixels) → context.
type ContextFn = fn(&Bitmap, i64, i64, &[(i8, i8); 4]) -> usize;

fn context_fn(template: u8) -> ContextFn {
    match template {
        0 => context0,
        1 => context1,
        2 => context2,
        _ => context3,
    }
}

/// TPGDON pseudo-pixel context value per template (T.88 6.2.5.7).
fn tpgd_context(template: u8) -> usize {
    match template {
        0 => 0x9B25,
        1 => 0x0795,
        2 => 0x00E5,
        _ => 0x0195,
    }
}

/// Decode a generic region bitmap (T.88 6.2.5, arithmetic coding). `cx` must
/// hold `1 << 16` contexts and is shared across calls within one segment
/// (symbol dictionaries decode many bitmaps with one context set).
fn decode_generic(
    mq: &mut MqDecoder,
    cx: &mut [u8],
    width: usize,
    height: usize,
    p: &GenericParams,
) -> Result<Bitmap> {
    let mut bm = Bitmap::new(width, height, 0)?;
    let ctx_at = context_fn(p.template);
    let tpgd_cx = tpgd_context(p.template);
    let mut ltp = false;

    for y in 0..height {
        if p.tpgdon {
            // Typical prediction: SLTP toggles LTP; an LTP row repeats the
            // row above (all-white above row 0).
            ltp ^= mq.decode(cx, tpgd_cx) == 1;
            if ltp {
                if y > 0 {
                    let w = bm.width;
                    bm.data.copy_within((y - 1) * w..y * w, y * w);
                }
                continue;
            }
        }
        for x in 0..width {
            let ctx = ctx_at(&bm, x as i64, y as i64, &p.at);
            let pixel = mq.decode(cx, ctx);
            if pixel == 1 {
                bm.set(x, y, 1);
            }
        }
    }
    Ok(bm)
}

// ---------------------------------------------------------------------------
// Generic refinement region decoding (T.88 6.3).
// ---------------------------------------------------------------------------

/// Parameters for the generic refinement decoder (T.88 6.3.2).
struct RefinementParams {
    /// GRTEMPLATE: 0 (13-pixel context, 2 AT pixels) or 1 (10-pixel context).
    template: u8,
    /// TPGRON typical-prediction flag.
    tpgron: bool,
    /// Adaptive template pixels A1 (current-bitmap offset) and A2 (reference
    /// offset); only consulted when `template == 0`.
    at: [(i8, i8); 2],
    /// Reference bitmap offset: the reference pixel for output (x, y) is the
    /// reference bitmap at (x - dx, y - dy).
    dx: i32,
    dy: i32,
}

/// Refinement coding/reference template pixels (T.88 6.3.5.3), matching the
/// canonical bit ordering: the first listed pixel is the most significant bit
/// of the context. Template 0 appends AT1 to the coding set and AT2 to the
/// reference set; template 1 has no adaptive pixels.
///
/// The context is built coding-pixels-first then reference-pixels, folding left
/// so the first pixel ends up in the high bit. This matches the SLTP reused
/// context constants 0x0020 (template 0) and 0x0008 (template 1).
#[inline]
#[allow(clippy::too_many_arguments)]
fn refine_context(
    template: u8,
    cur: &Bitmap,
    refr: &Bitmap,
    x: i64,
    y: i64,
    rx: i64,
    ry: i64,
    at: &[(i8, i8); 2],
) -> usize {
    let mut ctx = 0usize;
    if template == 0 {
        // Coding pixels: (0,-1), (1,-1), (-1,0), then AT1 (nominal (-1,-1)).
        ctx = (ctx << 1) | cur.get(x, y - 1) as usize;
        ctx = (ctx << 1) | cur.get(x + 1, y - 1) as usize;
        ctx = (ctx << 1) | cur.get(x - 1, y) as usize;
        ctx = (ctx << 1) | cur.get(x + at[0].0 as i64, y + at[0].1 as i64) as usize;
        // Reference pixels: (0,-1),(1,-1),(-1,0),(0,0),(1,0),(-1,1),(0,1),(1,1),
        // then AT2 (nominal (-1,-1)).
        ctx = (ctx << 1) | refr.get(rx, ry - 1) as usize;
        ctx = (ctx << 1) | refr.get(rx + 1, ry - 1) as usize;
        ctx = (ctx << 1) | refr.get(rx - 1, ry) as usize;
        ctx = (ctx << 1) | refr.get(rx, ry) as usize;
        ctx = (ctx << 1) | refr.get(rx + 1, ry) as usize;
        ctx = (ctx << 1) | refr.get(rx - 1, ry + 1) as usize;
        ctx = (ctx << 1) | refr.get(rx, ry + 1) as usize;
        ctx = (ctx << 1) | refr.get(rx + 1, ry + 1) as usize;
        ctx = (ctx << 1) | refr.get(rx + at[1].0 as i64, ry + at[1].1 as i64) as usize;
    } else {
        // Coding pixels: (-1,-1),(0,-1),(1,-1),(-1,0).
        ctx = (ctx << 1) | cur.get(x - 1, y - 1) as usize;
        ctx = (ctx << 1) | cur.get(x, y - 1) as usize;
        ctx = (ctx << 1) | cur.get(x + 1, y - 1) as usize;
        ctx = (ctx << 1) | cur.get(x - 1, y) as usize;
        // Reference pixels: (0,-1),(-1,0),(0,0),(1,0),(0,1),(1,1).
        ctx = (ctx << 1) | refr.get(rx, ry - 1) as usize;
        ctx = (ctx << 1) | refr.get(rx - 1, ry) as usize;
        ctx = (ctx << 1) | refr.get(rx, ry) as usize;
        ctx = (ctx << 1) | refr.get(rx + 1, ry) as usize;
        ctx = (ctx << 1) | refr.get(rx, ry + 1) as usize;
        ctx = (ctx << 1) | refr.get(rx + 1, ry + 1) as usize;
    }
    ctx
}

/// Decode a generic refinement region (T.88 6.3.5). `cx` must hold at least
/// `1 << 13` contexts and is shared across calls within one segment.
fn decode_refinement(
    mq: &mut MqDecoder,
    cx: &mut [u8],
    width: usize,
    height: usize,
    refr: &Bitmap,
    p: &RefinementParams,
) -> Result<Bitmap> {
    let mut bm = Bitmap::new(width, height, 0)?;
    let mut ltp = false;
    for y in 0..height as i64 {
        if p.tpgron {
            // SLTP reused context (T.88 6.3.5.6): the pseudo-pixel context for
            // the canonical bit ordering is 0x0020 (template 0) / 0x0008
            // (template 1).
            let sltp_cx = if p.template == 0 { 0x0020 } else { 0x0008 };
            ltp ^= mq.decode(cx, sltp_cx) == 1;
        }
        for x in 0..width as i64 {
            let rx = x - p.dx as i64;
            let ry = y - p.dy as i64;
            if ltp {
                // Typical prediction: a uniform 3x3 reference neighbourhood
                // makes the output pixel equal the reference pixel without
                // coding it (T.88 6.3.5.6).
                let mut sum = 0i32;
                for ddy in -1..=1 {
                    for ddx in -1..=1 {
                        sum += refr.get(rx + ddx, ry + ddy) as i32;
                    }
                }
                if sum == 0 {
                    continue; // all-white neighbourhood → pixel stays 0
                }
                if sum == 9 {
                    bm.set(x as usize, y as usize, 1);
                    continue;
                }
            }
            let ctx = refine_context(p.template, &bm, refr, x, y, rx, ry, &p.at);
            if mq.decode(cx, ctx) == 1 {
                bm.set(x as usize, y as usize, 1);
            }
        }
    }
    Ok(bm)
}

// ---------------------------------------------------------------------------
// Text region core (T.88 6.4) — shared by standalone text regions and the
// symbol-dictionary aggregate case.
// ---------------------------------------------------------------------------

/// Text-region geometry and placement flags, decoupled from the entropy coder
/// so the arithmetic and Huffman paths and the symbol-dict aggregate case all
/// share one placement loop.
struct TextRegionGeom {
    /// SBSTRIPS = 1 << log_strips.
    log_strips: u32,
    strips: i64,
    /// Reference corner: 0 = BL, 1 = TL, 2 = BR, 3 = TR.
    ref_corner: u8,
    transposed: bool,
    comb_op: u8,
    def_pixel: u8,
    ds_offset: i64,
    sb_refine: bool,
    r_template: u8,
    r_at: [(i8, i8); 2],
}

impl TextRegionGeom {
    /// The implicit geometry used when a symbol dictionary aggregates symbols
    /// (T.88 6.5.8.2.2): SBSTRIPS = 1, TOPLEFT, OR, refinement on.
    fn aggregate(r_template: u8, r_at: [(i8, i8); 2]) -> Self {
        Self {
            log_strips: 0,
            strips: 1,
            ref_corner: 1, // TOPLEFT
            transposed: false,
            comb_op: 0, // OR
            def_pixel: 0,
            ds_offset: 0,
            sb_refine: true,
            r_template,
            r_at,
        }
    }
}

/// Mutable arithmetic integer-context bundle for the text-region core.
struct TextArithCtx<'a> {
    iadt: &'a mut [u8],
    iafs: &'a mut [u8],
    iads: &'a mut [u8],
    iait: &'a mut [u8],
    iari: &'a mut [u8],
    iardw: &'a mut [u8],
    iardh: &'a mut [u8],
    iardx: &'a mut [u8],
    iardy: &'a mut [u8],
    iaid: &'a mut [u8],
    cx_gr: &'a mut [u8],
}

/// Place one symbol instance onto the region bitmap, applying per-instance
/// refinement if requested. Returns the advance along the S axis.
#[allow(clippy::too_many_arguments)]
fn place_instance(
    bm: &mut Bitmap,
    sym: &Bitmap,
    refined: Option<Bitmap>,
    t: i64,
    curs: i64,
    geom: &TextRegionGeom,
) -> i64 {
    let placed = refined.as_ref().unwrap_or(sym);
    let (w, hh) = (placed.width as i64, placed.height as i64);
    if !geom.transposed {
        let y0 = t - if geom.ref_corner & 1 == 0 { hh - 1 } else { 0 };
        let x0 = curs;
        draw_symbol(bm, placed, x0, y0, geom.comb_op);
        w - 1
    } else {
        let x0 = t - if geom.ref_corner & 2 != 0 { w - 1 } else { 0 };
        let y0 = curs;
        draw_symbol(bm, placed, x0, y0, geom.comb_op);
        hh - 1
    }
}

/// Arithmetic text-region core (T.88 6.4.5). Decodes `num_instances` placements
/// off the live MQ stream and returns the region bitmap.
#[allow(clippy::too_many_arguments)]
fn decode_text_region_arith(
    mq: &mut MqDecoder,
    width: usize,
    height: usize,
    symbols: &[Rc<Bitmap>],
    num_instances: usize,
    sym_code_len: u32,
    geom: &TextRegionGeom,
    ctx: &mut TextArithCtx,
) -> Result<Bitmap> {
    let mut bm = Bitmap::new(width, height, geom.def_pixel)?;
    let strips = geom.strips;

    let mut stript =
        -decode_int(mq, ctx.iadt).ok_or_else(|| err("unexpected OOB in IADT"))? * strips;
    let mut firsts: i64 = 0;
    let mut inst = 0usize;

    'instances: while inst < num_instances {
        let dt = decode_int(mq, ctx.iadt).ok_or_else(|| err("unexpected OOB in IADT"))?;
        stript += dt * strips;
        let dfs = decode_int(mq, ctx.iafs).ok_or_else(|| err("unexpected OOB in IAFS"))?;
        firsts += dfs;
        let mut curs = firsts;
        let mut first = true;
        loop {
            if !first {
                let Some(ids) = decode_int(mq, ctx.iads) else {
                    break; // OOB ends the strip
                };
                curs += ids + geom.ds_offset;
            }
            first = false;
            if inst >= num_instances {
                break 'instances;
            }

            let curt = if strips == 1 {
                0
            } else {
                decode_int(mq, ctx.iait).ok_or_else(|| err("unexpected OOB in IAIT"))?
            };
            let t = stript + curt;
            let id = decode_iaid(mq, ctx.iaid, sym_code_len);
            let sym = symbols
                .get(id)
                .ok_or_else(|| err(format!("symbol id {id} out of range")))?
                .clone();

            // Per-instance refinement (T.88 6.4.11).
            let refined = if geom.sb_refine {
                let ri = decode_int(mq, ctx.iari).ok_or_else(|| err("unexpected OOB in IARI"))?;
                if ri != 0 {
                    let rdw =
                        decode_int(mq, ctx.iardw).ok_or_else(|| err("unexpected OOB in IARDW"))?;
                    let rdh =
                        decode_int(mq, ctx.iardh).ok_or_else(|| err("unexpected OOB in IARDH"))?;
                    let rdx =
                        decode_int(mq, ctx.iardx).ok_or_else(|| err("unexpected OOB in IARDX"))?;
                    let rdy =
                        decode_int(mq, ctx.iardy).ok_or_else(|| err("unexpected OOB in IARDY"))?;
                    let nw = sym.width as i64 + rdw;
                    let nh = sym.height as i64 + rdh;
                    if nw <= 0 || nh <= 0 || nw > MAX_DIMENSION as i64 || nh > MAX_DIMENSION as i64
                    {
                        return Err(err("implausible refined symbol size"));
                    }
                    // The reference offset (T.88 6.4.11): floor(RDW/2)+RDX,
                    // floor(RDH/2)+RDY.
                    let dx = (rdw >> 1) + rdx;
                    let dy = (rdh >> 1) + rdy;
                    Some(decode_refinement(
                        mq,
                        ctx.cx_gr,
                        nw as usize,
                        nh as usize,
                        &sym,
                        &RefinementParams {
                            template: geom.r_template,
                            tpgron: false,
                            at: geom.r_at,
                            dx: dx as i32,
                            dy: dy as i32,
                        },
                    )?)
                } else {
                    None
                }
            } else {
                None
            };

            curs += place_instance(&mut bm, &sym, refined, t, curs, geom);
            inst += 1;
        }
    }
    Ok(bm)
}

// ---------------------------------------------------------------------------
// Huffman decoding (T.88 Annex B).
// ---------------------------------------------------------------------------

/// MSB-first bit reader over the entropy payload. Past the end it yields 0
/// bits (Huffman streams declare their own counts, so reads stay bounded).
struct BitReader<'a> {
    data: &'a [u8],
    /// Next bit position, counted in bits from the start of `data`.
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    #[inline]
    fn read_bit(&mut self) -> u32 {
        let byte = self.pos >> 3;
        let bit = 7 - (self.pos & 7);
        self.pos += 1;
        match self.data.get(byte) {
            Some(&b) => ((b >> bit) & 1) as u32,
            None => 0,
        }
    }

    /// Read `n` bits MSB-first (n ≤ 32).
    fn read_bits(&mut self, n: u32) -> u32 {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.read_bit();
        }
        v
    }

    /// Skip to the next byte boundary (used at the end of a Huffman region).
    fn byte_align(&mut self) {
        self.pos = self.pos.div_ceil(8) * 8;
    }

    /// Current byte offset (rounded up), used to locate trailing MMR/BMSIZE
    /// payloads inside a Huffman symbol dictionary.
    fn byte_pos(&self) -> usize {
        self.pos.div_ceil(8)
    }
}

/// One Huffman table line (T.88 B.4): a prefix length, a range length and a
/// range low value, plus flags for the lower-range and OOB line kinds.
#[derive(Clone, Copy)]
struct HuffLine {
    prefix_len: u8,
    range_len: u8,
    range_low: i32,
    /// Lower-range line: the magnitude is subtracted from `range_low`.
    is_lower: bool,
    /// Out-of-band line (HUFFDEC returns OOB).
    is_oob: bool,
}

impl HuffLine {
    const fn normal(prefix_len: u8, range_len: u8, range_low: i32) -> Self {
        Self {
            prefix_len,
            range_len,
            range_low,
            is_lower: false,
            is_oob: false,
        }
    }
    const fn lower(prefix_len: u8, range_len: u8, range_low: i32) -> Self {
        Self {
            prefix_len,
            range_len,
            range_low,
            is_lower: true,
            is_oob: false,
        }
    }
    const fn upper(prefix_len: u8, range_len: u8, range_low: i32) -> Self {
        // Upper range line: 32-bit magnitude added to range_low (B.5 reserves
        // this for the highest line; encoded identically to a normal line).
        Self::normal(prefix_len, range_len, range_low)
    }
    const fn oob(prefix_len: u8) -> Self {
        Self {
            prefix_len,
            range_len: 0,
            range_low: 0,
            is_lower: false,
            is_oob: true,
        }
    }
}

/// A built Huffman table: lines paired with assigned prefix codes (T.88 B.3).
struct HuffTable {
    lines: Vec<HuffLine>,
    /// Assigned prefix code per line, parallel to `lines`. Lines with
    /// `prefix_len == 0` are unused and carry an unreachable code.
    codes: Vec<u32>,
}

impl HuffTable {
    /// Assign prefix codes from the lines' prefix lengths (T.88 B.3).
    fn build(lines: Vec<HuffLine>) -> Result<Self> {
        let max_len = lines.iter().map(|l| l.prefix_len).max().unwrap_or(0) as usize;
        if max_len == 0 {
            return Ok(Self {
                codes: vec![0; lines.len()],
                lines,
            });
        }
        if max_len > 32 {
            return Err(err("Huffman prefix length exceeds 32 bits"));
        }
        let mut len_count = vec![0u32; max_len + 1];
        for l in &lines {
            len_count[l.prefix_len as usize] += 1;
        }
        len_count[0] = 0;
        // First code per length (T.88 B.3 step 3).
        let mut first_code = vec![0u32; max_len + 2];
        let mut curcode = 0u32;
        for len in 1..=max_len {
            first_code[len] = curcode;
            curcode = (curcode + len_count[len]) << 1;
        }
        let mut next = first_code.clone();
        let mut codes = vec![0u32; lines.len()];
        for (i, l) in lines.iter().enumerate() {
            let len = l.prefix_len as usize;
            if len > 0 {
                codes[i] = next[len];
                next[len] += 1;
            }
        }
        Ok(Self { lines, codes })
    }

    /// HUFFDEC (T.88 B.2): read a value or OOB. Returns `None` for OOB.
    fn decode(&self, br: &mut BitReader) -> Option<i64> {
        let mut len = 0u32;
        let mut code = 0u32;
        // Bound the prefix scan by the longest assigned length plus slack so a
        // corrupt stream cannot loop forever.
        let max_len = self.lines.iter().map(|l| l.prefix_len).max().unwrap_or(0) as u32;
        while len <= max_len + 1 {
            code = (code << 1) | br.read_bit();
            len += 1;
            for (i, l) in self.lines.iter().enumerate() {
                if l.prefix_len as u32 == len && self.codes[i] == code {
                    if l.is_oob {
                        return None;
                    }
                    if l.range_len == 32 {
                        // (Upper/lower) 32-bit range line.
                        let mag = br.read_bits(32) as i64;
                        return Some(if l.is_lower {
                            l.range_low as i64 - mag
                        } else {
                            l.range_low as i64 + mag
                        });
                    }
                    let mag = br.read_bits(l.range_len as u32) as i64;
                    return Some(if l.is_lower {
                        l.range_low as i64 - mag
                    } else {
                        l.range_low as i64 + mag
                    });
                }
            }
        }
        // No prefix matched (corrupt table or stream): treat as OOB so the
        // caller's bounded loop terminates rather than spinning.
        None
    }
}

/// Standard Huffman table B.`n` (1-based, T.88 Annex B.5 tables B.1–B.15).
fn standard_huff_table(n: usize) -> HuffTable {
    use HuffLine as L;
    let lines: Vec<HuffLine> = match n {
        1 => vec![
            L::normal(1, 4, 0),
            L::normal(2, 8, 16),
            L::normal(3, 16, 272),
            L::upper(3, 32, 65808),
        ],
        2 => vec![
            L::normal(1, 0, 0),
            L::normal(2, 0, 1),
            L::normal(3, 0, 2),
            L::normal(4, 3, 3),
            L::normal(5, 6, 11),
            L::upper(6, 32, 75),
            L::oob(6),
        ],
        3 => vec![
            L::normal(8, 8, -256),
            L::normal(1, 0, 0),
            L::normal(2, 0, 1),
            L::normal(3, 0, 2),
            L::normal(4, 3, 3),
            L::normal(5, 6, 11),
            L::lower(8, 32, -257),
            L::upper(7, 32, 75),
            L::oob(6),
        ],
        4 => vec![
            L::normal(1, 0, 1),
            L::normal(2, 0, 2),
            L::normal(3, 0, 3),
            L::normal(4, 3, 4),
            L::normal(5, 6, 12),
            L::upper(5, 32, 76),
        ],
        5 => vec![
            L::normal(7, 8, -255),
            L::normal(1, 0, 1),
            L::normal(2, 0, 2),
            L::normal(3, 0, 3),
            L::normal(4, 3, 4),
            L::normal(5, 6, 12),
            L::lower(7, 32, -256),
            L::upper(6, 32, 76),
        ],
        6 => vec![
            L::normal(5, 10, -2048),
            L::normal(4, 9, -1024),
            L::normal(4, 8, -512),
            L::normal(4, 7, -256),
            L::normal(5, 6, -128),
            L::normal(5, 5, -64),
            L::normal(4, 5, -32),
            L::normal(2, 7, 0),
            L::normal(3, 7, 128),
            L::normal(3, 8, 256),
            L::normal(4, 9, 512),
            L::normal(4, 10, 1024),
            L::lower(6, 32, -2049),
            L::upper(6, 32, 2048),
        ],
        7 => vec![
            L::normal(4, 9, -1024),
            L::normal(3, 8, -512),
            L::normal(4, 7, -256),
            L::normal(5, 6, -128),
            L::normal(5, 5, -64),
            L::normal(4, 5, -32),
            L::normal(4, 5, 0),
            L::normal(5, 5, 32),
            L::normal(5, 6, 64),
            L::normal(4, 7, 128),
            L::normal(3, 8, 256),
            L::normal(3, 9, 512),
            L::normal(3, 10, 1024),
            L::lower(5, 32, -1025),
            L::upper(5, 32, 2048),
        ],
        8 => vec![
            L::normal(8, 3, -15),
            L::normal(9, 1, -7),
            L::normal(8, 1, -5),
            L::normal(9, 0, -3),
            L::normal(7, 0, -2),
            L::normal(4, 0, -1),
            L::normal(2, 1, 0),
            L::normal(5, 0, 2),
            L::normal(6, 0, 3),
            L::normal(3, 4, 4),
            L::normal(6, 1, 20),
            L::normal(4, 4, 22),
            L::normal(4, 5, 38),
            L::normal(5, 6, 70),
            L::normal(5, 7, 134),
            L::normal(6, 7, 262),
            L::normal(7, 8, 390),
            L::normal(6, 10, 646),
            L::lower(9, 32, -16),
            L::upper(9, 32, 1670),
            L::oob(2),
        ],
        9 => vec![
            L::normal(8, 4, -31),
            L::normal(9, 2, -15),
            L::normal(8, 2, -11),
            L::normal(9, 1, -7),
            L::normal(7, 1, -5),
            L::normal(4, 1, -3),
            L::normal(3, 1, -1),
            L::normal(3, 1, 1),
            L::normal(5, 1, 3),
            L::normal(6, 1, 5),
            L::normal(3, 5, 7),
            L::normal(6, 2, 39),
            L::normal(4, 5, 43),
            L::normal(4, 6, 75),
            L::normal(5, 7, 139),
            L::normal(5, 8, 267),
            L::normal(6, 8, 523),
            L::normal(7, 9, 779),
            L::normal(6, 11, 1291),
            L::lower(9, 32, -32),
            L::upper(9, 32, 3339),
            L::oob(2),
        ],
        10 => vec![
            L::normal(7, 4, -21),
            L::normal(8, 0, -5),
            L::normal(7, 0, -4),
            L::normal(5, 0, -3),
            L::normal(2, 2, -2),
            L::normal(5, 0, 2),
            L::normal(6, 0, 3),
            L::normal(7, 0, 4),
            L::normal(8, 0, 5),
            L::normal(2, 6, 6),
            L::normal(5, 5, 70),
            L::normal(6, 5, 102),
            L::normal(6, 6, 134),
            L::normal(6, 7, 198),
            L::normal(6, 8, 326),
            L::normal(6, 9, 582),
            L::normal(6, 10, 1094),
            L::normal(7, 11, 2118),
            L::lower(8, 32, -22),
            L::upper(8, 32, 4166),
            L::oob(2),
        ],
        11 => vec![
            L::normal(1, 0, 1),
            L::normal(2, 1, 2),
            L::normal(4, 0, 4),
            L::normal(4, 1, 5),
            L::normal(5, 1, 7),
            L::normal(5, 2, 9),
            L::normal(6, 2, 13),
            L::normal(7, 2, 17),
            L::normal(7, 3, 21),
            L::normal(7, 4, 29),
            L::normal(7, 5, 45),
            L::normal(7, 6, 77),
            L::upper(7, 32, 141),
        ],
        12 => vec![
            L::normal(1, 0, 1),
            L::normal(2, 0, 2),
            L::normal(3, 1, 3),
            L::normal(5, 0, 5),
            L::normal(5, 1, 6),
            L::normal(6, 1, 8),
            L::normal(7, 0, 10),
            L::normal(7, 1, 11),
            L::normal(7, 2, 13),
            L::normal(7, 3, 17),
            L::normal(7, 4, 25),
            L::normal(8, 5, 41),
            L::upper(8, 32, 73),
        ],
        13 => vec![
            L::normal(1, 0, 1),
            L::normal(3, 0, 2),
            L::normal(4, 0, 3),
            L::normal(5, 0, 4),
            L::normal(4, 1, 5),
            L::normal(3, 3, 7),
            L::normal(6, 1, 15),
            L::normal(6, 2, 17),
            L::normal(6, 3, 21),
            L::normal(6, 4, 29),
            L::normal(6, 5, 45),
            L::normal(7, 6, 77),
            L::upper(7, 32, 141),
        ],
        14 => vec![
            L::normal(3, 0, -2),
            L::normal(3, 0, -1),
            L::normal(1, 0, 0),
            L::normal(3, 0, 1),
            L::normal(3, 0, 2),
        ],
        15 => vec![
            L::normal(7, 4, -24),
            L::normal(6, 2, -8),
            L::normal(5, 1, -4),
            L::normal(4, 0, -2),
            L::normal(3, 0, -1),
            L::normal(1, 0, 0),
            L::normal(3, 0, 1),
            L::normal(4, 0, 2),
            L::normal(5, 1, 3),
            L::normal(6, 2, 5),
            L::normal(7, 4, 9),
            L::lower(7, 32, -25),
            L::upper(7, 32, 25),
        ],
        _ => vec![L::normal(1, 0, 0)],
    };
    HuffTable::build(lines).expect("standard Huffman table is well-formed")
}

/// Build a custom Huffman table from a type-53 segment (T.88 B.2 / 7.4.5).
fn parse_custom_huff_table(data: &[u8]) -> Result<HuffTable> {
    let mut r = Reader::new(data);
    let flags = r.u8()?;
    let oob = flags & 1 != 0;
    let prefix_size = (((flags >> 1) & 7) + 1) as u32;
    let range_size = (((flags >> 4) & 7) + 1) as u32;
    let low = r.u32()? as i32;
    let high = r.u32()? as i32;
    if high < low {
        return Err(err("Huffman table HIGH < LOW"));
    }
    let mut br = BitReader::new(r.rest());
    let mut lines = Vec::new();
    let mut cur = low;
    // Normal lines until cur reaches HIGH (T.88 B.2 step 4).
    let mut guard = 0;
    while cur < high {
        let prefix_len = br.read_bits(prefix_size) as u8;
        let range_len = br.read_bits(range_size) as u8;
        lines.push(HuffLine::normal(prefix_len, range_len, cur));
        // Avoid overflow: range_len up to 32.
        cur = cur.saturating_add(1i32.checked_shl(range_len as u32).unwrap_or(i32::MAX));
        guard += 1;
        if guard > MAX_SYMBOLS {
            return Err(err("custom Huffman table has too many lines"));
        }
    }
    // Lower-range line.
    let lower_prefix = br.read_bits(prefix_size) as u8;
    lines.push(HuffLine::lower(lower_prefix, 32, low - 1));
    // Upper-range line.
    let upper_prefix = br.read_bits(prefix_size) as u8;
    lines.push(HuffLine::normal(upper_prefix, 32, high));
    // Optional OOB line.
    if oob {
        let oob_prefix = br.read_bits(prefix_size) as u8;
        lines.push(HuffLine::oob(oob_prefix));
    }
    HuffTable::build(lines)
}

/// A runtime-selectable Huffman table: a standard table or a borrowed custom
/// one resolved from the region's referred type-53 segments.
enum HuffSel<'a> {
    Std(HuffTable),
    Custom(&'a HuffTable),
}

impl HuffSel<'_> {
    fn table(&self) -> &HuffTable {
        match self {
            HuffSel::Std(t) => t,
            HuffSel::Custom(t) => t,
        }
    }
}

/// Read the runtime symbol-ID Huffman table for a Huffman text region
/// (T.88 7.4.4.1.4). First 35 run-code lengths (4 bits each) build a run-code
/// table; that table then decodes `n_syms` code lengths, with run codes 32–34
/// expanding to repeat counts. The result is a Huffman table mapping codes to
/// symbol indices 0..n_syms.
fn read_symbol_id_huff_table(br: &mut BitReader, n_syms: usize) -> Result<HuffTable> {
    // 35 run-code prefix lengths.
    let mut runcode_lines = Vec::with_capacity(35);
    for i in 0..35i32 {
        let len = br.read_bits(4) as u8;
        runcode_lines.push(HuffLine::normal(len, 0, i));
    }
    let runcode_tab = HuffTable::build(runcode_lines)?;

    // Decode n_syms symbol code lengths.
    let mut code_lengths = vec![0u8; n_syms];
    let mut prev_len = 0i64;
    let mut i = 0usize;
    let mut guard = 0usize;
    while i < n_syms {
        guard += 1;
        if guard > n_syms.saturating_add(64).saturating_mul(4) {
            return Err(err("symbol ID Huffman table failed to terminate"));
        }
        let code = runcode_tab
            .decode(br)
            .ok_or_else(|| err("OOB decoding symbol ID run code"))?;
        match code {
            0..=31 => {
                code_lengths[i] = code as u8;
                prev_len = code;
                i += 1;
            }
            32 => {
                // Repeat the previous length 3..=6 times.
                let rep = br.read_bits(2) as usize + 3;
                for _ in 0..rep {
                    if i >= n_syms {
                        break;
                    }
                    code_lengths[i] = prev_len as u8;
                    i += 1;
                }
            }
            33 => {
                // Repeat length 0, 3..=10 times.
                let rep = br.read_bits(3) as usize + 3;
                for _ in 0..rep {
                    if i >= n_syms {
                        break;
                    }
                    code_lengths[i] = 0;
                    i += 1;
                }
            }
            34 => {
                // Repeat length 0, 11..=138 times.
                let rep = br.read_bits(7) as usize + 11;
                for _ in 0..rep {
                    if i >= n_syms {
                        break;
                    }
                    code_lengths[i] = 0;
                    i += 1;
                }
            }
            _ => return Err(err("invalid symbol ID run code")),
        }
    }

    let lines: Vec<HuffLine> = code_lengths
        .iter()
        .enumerate()
        .map(|(idx, &len)| HuffLine::normal(len, 0, idx as i32))
        .collect();
    HuffTable::build(lines)
}

// ---------------------------------------------------------------------------
// Segment headers (T.88 7.2).
// ---------------------------------------------------------------------------

/// Bounds-checked big-endian reader over a segment byte slice.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.remaining() {
            return Err(err("truncated segment data"));
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    /// Everything from the cursor to the end (the entropy-coded payload).
    fn rest(&self) -> &'a [u8] {
        &self.data[self.pos..]
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn i8(&mut self) -> Result<i8> {
        Ok(self.u8()? as i8)
    }

    fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
}

struct SegmentHeader {
    number: u32,
    seg_type: u8,
    referred: Vec<u32>,
    #[allow(dead_code)]
    page: u32,
    /// 0xFFFFFFFF = unknown length (unsupported).
    data_length: u32,
}

fn parse_segment_header(r: &mut Reader) -> Result<SegmentHeader> {
    let number = r.u32()?;
    let flags = r.u8()?;
    let seg_type = flags & 0x3F;
    let page_assoc_4 = flags & 0x40 != 0;

    // Referred-to segment count: short form packs 0..=4 into the top 3 bits;
    // 7 marks the long form (29-bit count + retain-flag bytes).
    let first = r.u8()?;
    let count = match first >> 5 {
        c @ 0..=4 => c as usize,
        7 => {
            let rest = r.take(3)?;
            let count = (((first as u32 & 0x1F) << 24)
                | ((rest[0] as u32) << 16)
                | ((rest[1] as u32) << 8)
                | rest[2] as u32) as usize;
            // ceil((count + 1) / 8) retain-flag bytes follow; parsed, unused.
            r.take((count + 8) / 8)?;
            count
        }
        c => return Err(err(format!("invalid referred-to segment count code {c}"))),
    };

    // Referred segment numbers are sized by this segment's own number.
    let ref_size = if number <= 256 {
        1
    } else if number <= 65536 {
        2
    } else {
        4
    };
    if count.saturating_mul(ref_size) > r.remaining() {
        return Err(err("referred-to segment list exceeds segment data"));
    }
    let mut referred = Vec::with_capacity(count);
    for _ in 0..count {
        referred.push(match ref_size {
            1 => r.u8()? as u32,
            2 => r.u16()? as u32,
            _ => r.u32()?,
        });
    }

    let page = if page_assoc_4 {
        r.u32()?
    } else {
        r.u8()? as u32
    };
    let data_length = r.u32()?;
    Ok(SegmentHeader {
        number,
        seg_type,
        referred,
        page,
        data_length,
    })
}

/// Region segment information field (T.88 7.4.1): geometry plus the external
/// combination operator.
#[derive(Clone)]
struct RegionInfo {
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    op: u8,
}

fn parse_region_info(r: &mut Reader) -> Result<RegionInfo> {
    let width = r.u32()? as usize;
    let height = r.u32()? as usize;
    let x = r.u32()? as usize;
    let y = r.u32()? as usize;
    let op = r.u8()? & 7;
    if width > MAX_DIMENSION || height > MAX_DIMENSION || x > MAX_DIMENSION || y > MAX_DIMENSION {
        return Err(err(format!(
            "implausible region geometry {width}x{height}+{x}+{y}"
        )));
    }
    Ok(RegionInfo {
        width,
        height,
        x,
        y,
        op,
    })
}

// ---------------------------------------------------------------------------
// Segment processing and page assembly.
// ---------------------------------------------------------------------------

/// A decoded pattern dictionary (T.88 6.7): HDPW×HDPH patterns indexed 0..=HDMAX.
struct PatternDict {
    patterns: Vec<Rc<Bitmap>>,
}

#[derive(Default)]
struct Decoder {
    page: Option<Bitmap>,
    /// Page height was declared 0xFFFFFFFF (striped): grow rows on demand.
    page_auto_height: bool,
    page_default_pixel: u8,
    /// Exported symbols per symbol-dictionary segment number; `None` records
    /// a dictionary that failed to decode (dependent regions render blank).
    symbol_dicts: HashMap<u32, Option<Vec<Rc<Bitmap>>>>,
    /// Intermediate region bitmaps keyed by segment number (types 4/20/36/40/42),
    /// with their region geometry, for use as refinement references and for
    /// the (rare) referred-intermediate case.
    regions: HashMap<u32, (RegionInfo, Bitmap)>,
    /// Pattern dictionaries keyed by segment number (type 16).
    pattern_dicts: HashMap<u32, Option<PatternDict>>,
    /// Custom Huffman tables keyed by type-53 segment number.
    huff_tables: HashMap<u32, HuffTable>,
}

impl Decoder {
    /// Parse and process every segment in `data` (globals or page stream).
    fn process_segments(&mut self, data: &[u8]) -> Result<()> {
        let mut r = Reader::new(data);
        // A header is at least 11 bytes; shorter trailers are padding.
        while r.remaining() >= 11 {
            let h = parse_segment_header(&mut r)?;
            if h.data_length == 0xFFFF_FFFF {
                return Err(err(format!(
                    "segment {} has unknown data length (unsupported)",
                    h.number
                )));
            }
            let len = h.data_length as usize;
            let payload = if len <= r.remaining() {
                r.take(len)?
            } else {
                warn!(
                    "JBIG2Decode: segment {} declares {len} data bytes but only {} remain",
                    h.number,
                    r.remaining()
                );
                r.take(r.remaining())?
            };
            self.process_segment(&h, payload)?;
        }
        if r.remaining() > 0 {
            debug!("JBIG2Decode: {} trailing byte(s) ignored", r.remaining());
        }
        Ok(())
    }

    fn process_segment(&mut self, h: &SegmentHeader, data: &[u8]) -> Result<()> {
        match h.seg_type {
            // Symbol dictionary: a failure poisons only dependent regions.
            0 => {
                let dict = match self.decode_symbol_dict(h, data) {
                    Ok(d) => Some(d),
                    Err(e) => {
                        warn!(
                            "JBIG2Decode: symbol dictionary segment {} failed ({e}); \
                             dependent text regions will be blank",
                            h.number
                        );
                        None
                    }
                };
                self.symbol_dicts.insert(h.number, dict);
            }
            // Text region: intermediate (4) is stored; immediate (6/7) composes.
            4 | 6 | 7 => match self.decode_text_region(h, data) {
                Ok((info, bm)) => {
                    if h.seg_type == 4 {
                        self.regions.insert(h.number, (info, bm));
                    } else {
                        self.compose_to_page(&info, &bm)?;
                    }
                }
                Err(e) => warn!(
                    "JBIG2Decode: text region segment {} failed ({e}); region left blank",
                    h.number
                ),
            },
            // Generic region: intermediate (36) stored; immediate (38/39) composes.
            36 | 38 | 39 => match self.decode_generic_region_segment(data) {
                Ok((info, bm)) => {
                    if h.seg_type == 36 {
                        self.regions.insert(h.number, (info, bm));
                    } else {
                        self.compose_to_page(&info, &bm)?;
                    }
                }
                Err(e) => warn!(
                    "JBIG2Decode: generic region segment {} failed ({e}); region left blank",
                    h.number
                ),
            },
            // Generic refinement region: intermediate (40) stored; immediate
            // (42/43) composes. The reference is a referred intermediate region
            // or, by default, the page area at the region's location.
            40 | 42 | 43 => match self.decode_refinement_region_segment(h, data) {
                Ok((info, bm)) => {
                    if h.seg_type == 40 {
                        self.regions.insert(h.number, (info, bm));
                    } else {
                        self.compose_to_page(&info, &bm)?;
                    }
                }
                Err(e) => warn!(
                    "JBIG2Decode: refinement region segment {} failed ({e}); region left blank",
                    h.number
                ),
            },
            // Pattern dictionary: cache by segment number.
            16 => {
                let pd = match self.decode_pattern_dict(data) {
                    Ok(d) => Some(d),
                    Err(e) => {
                        warn!(
                            "JBIG2Decode: pattern dictionary segment {} failed ({e}); \
                             dependent halftone regions will be blank",
                            h.number
                        );
                        None
                    }
                };
                self.pattern_dicts.insert(h.number, pd);
            }
            // Halftone region: intermediate (20) stored; immediate (22/23) composes.
            20 | 22 | 23 => match self.decode_halftone_region(h, data) {
                Ok((info, bm)) => {
                    if h.seg_type == 20 {
                        self.regions.insert(h.number, (info, bm));
                    } else {
                        self.compose_to_page(&info, &bm)?;
                    }
                }
                Err(e) => warn!(
                    "JBIG2Decode: halftone region segment {} failed ({e}); region left blank",
                    h.number
                ),
            },
            48 => self.process_page_info(data)?,
            50 => self.process_end_of_stripe(data)?,
            // End of page / end of file / profiles / extensions: no action.
            49 | 51 | 52 | 62 => {}
            // Custom Huffman table segment (T.88 7.4.5): parse and cache.
            53 => match parse_custom_huff_table(data) {
                Ok(t) => {
                    self.huff_tables.insert(h.number, t);
                }
                Err(e) => warn!(
                    "JBIG2Decode: custom Huffman table segment {} failed ({e}); ignored",
                    h.number
                ),
            },
            other => warn!("JBIG2Decode: unknown segment type {other} ignored"),
        }
        Ok(())
    }

    /// Page information segment (T.88 7.4.8).
    fn process_page_info(&mut self, data: &[u8]) -> Result<()> {
        let mut r = Reader::new(data);
        let width = r.u32()? as usize;
        let height = r.u32()?;
        let _xres = r.u32()?;
        let _yres = r.u32()?;
        let flags = r.u8()?;
        // Striping information (2 bytes) is tolerated if absent.
        let _striping = r.u16().unwrap_or(0);

        let auto_height = height == 0xFFFF_FFFF;
        let h = if auto_height { 0 } else { height as usize };
        if width == 0 || width > MAX_DIMENSION || h > MAX_DIMENSION {
            return Err(err(format!("implausible page size {width}x{height}")));
        }
        if width.div_ceil(8).saturating_mul(h) > MAX_OUTPUT {
            return Err(err("page exceeds the output size limit"));
        }
        self.page_default_pixel = (flags >> 2) & 1;
        self.page_auto_height = auto_height;
        self.page = Some(Bitmap::new(width, h, self.page_default_pixel)?);
        Ok(())
    }

    /// End-of-stripe segment: 4-byte Y coordinate of the stripe's last row.
    fn process_end_of_stripe(&mut self, data: &[u8]) -> Result<()> {
        let mut r = Reader::new(data);
        let y = r.u32()? as usize;
        self.grow_page_rows(y.saturating_add(1));
        Ok(())
    }

    /// Grow an auto-height page to at least `rows` rows (clamped to the
    /// safety caps — clamping warns instead of failing the whole image).
    fn grow_page_rows(&mut self, rows: usize) {
        if !self.page_auto_height {
            return;
        }
        let Some(page) = &mut self.page else { return };
        if rows <= page.height {
            return;
        }
        let row_cap = (MAX_BITMAP_PIXELS / page.width.max(1))
            .min(MAX_OUTPUT / page.width.div_ceil(8))
            .min(MAX_DIMENSION);
        let new_h = rows.min(row_cap);
        if new_h < rows {
            warn!("JBIG2Decode: striped page growth clamped to {new_h} rows");
        }
        if new_h > page.height {
            page.data
                .resize(new_h * page.width, self.page_default_pixel);
            page.height = new_h;
        }
    }

    /// Compose a decoded region bitmap onto the page with its combination
    /// operator, creating an implicit page if no page info segment was seen.
    fn compose_to_page(&mut self, info: &RegionInfo, bm: &Bitmap) -> Result<()> {
        if self.page.is_none() {
            warn!("JBIG2Decode: region segment before page information; creating implicit page");
            self.page = Some(Bitmap::new(
                info.x.saturating_add(bm.width),
                info.y.saturating_add(bm.height),
                0,
            )?);
        } else {
            self.grow_page_rows(info.y.saturating_add(bm.height));
        }
        let page = self.page.as_mut().expect("page was just ensured");
        compose_bitmap(page, bm, info.x, info.y, info.op);
        Ok(())
    }

    /// Generic region segment (T.88 7.4.6): MMR or arithmetic coding.
    fn decode_generic_region_segment(&self, data: &[u8]) -> Result<(RegionInfo, Bitmap)> {
        let mut r = Reader::new(data);
        let info = parse_region_info(&mut r)?;
        let flags = r.u8()?;
        let mmr = flags & 1 != 0;
        let template = (flags >> 1) & 3;
        let tpgdon = flags & 8 != 0;

        if mmr {
            // MMR is T.6 (Group 4) with 1 = black, exactly the CCITT decoder's
            // /BlackIs1 true packing; unpack its rows into the native bitmap.
            let params = crate::ccitt::CcittParams {
                k: -1,
                columns: info.width.max(1),
                rows: info.height,
                black_is_1: true,
                byte_align: false,
            };
            let packed = crate::ccitt::decode(r.rest(), &params)?;
            let bm = unpack_rows(&packed, info.width, info.height)?;
            return Ok((info, bm));
        }

        let mut at = [(0i8, 0i8); 4];
        let n_at = if template == 0 { 4 } else { 1 };
        for slot in at.iter_mut().take(n_at) {
            *slot = (r.i8()?, r.i8()?);
        }
        let mut mq = MqDecoder::new(r.rest());
        let mut cx = vec![0u8; 1 << 16];
        let bm = decode_generic(
            &mut mq,
            &mut cx,
            info.width,
            info.height,
            &GenericParams {
                template,
                tpgdon,
                at,
            },
        )?;
        Ok((info, bm))
    }

    /// Generic refinement region segment (T.88 7.4.7). The reference bitmap is
    /// a referred intermediate region (if exactly one is referred) or the
    /// current page contents at the region's location.
    fn decode_refinement_region_segment(
        &self,
        h: &SegmentHeader,
        data: &[u8],
    ) -> Result<(RegionInfo, Bitmap)> {
        let mut r = Reader::new(data);
        let info = parse_region_info(&mut r)?;
        let flags = r.u8()?;
        let template = flags & 1;
        let tpgron = flags & 2 != 0;

        let mut at = [(0i8, 0i8); 2];
        if template == 0 {
            at[0] = (r.i8()?, r.i8()?);
            at[1] = (r.i8()?, r.i8()?);
        }

        // Locate the reference: a referred region segment, else the page.
        let refr: Bitmap =
            if let Some(num) = h.referred.iter().find(|n| self.regions.contains_key(n)) {
                let (_rinfo, rbm) = &self.regions[num];
                rbm.clone()
            } else {
                let page = self
                    .page
                    .as_ref()
                    .ok_or_else(|| err("refinement region with no page reference"))?;
                let mut refr = Bitmap::new(info.width, info.height, 0)?;
                for y in 0..info.height {
                    for x in 0..info.width {
                        let v = page.get((info.x + x) as i64, (info.y + y) as i64);
                        if v != 0 {
                            refr.set(x, y, 1);
                        }
                    }
                }
                refr
            };

        let mut mq = MqDecoder::new(r.rest());
        let mut cx = vec![0u8; 1 << 13];
        let bm = decode_refinement(
            &mut mq,
            &mut cx,
            info.width,
            info.height,
            &refr,
            &RefinementParams {
                template,
                tpgron,
                at,
                dx: 0,
                dy: 0,
            },
        )?;
        // When refining the page in place, the result REPLACES the page area
        // (operator REPLACE), since the reference already held those pixels.
        let mut info = info;
        if !h.referred.iter().any(|n| self.regions.contains_key(n)) {
            info.op = 4; // REPLACE
        }
        Ok((info, bm))
    }

    /// Collect referred custom Huffman tables (type-53 segments) in referral
    /// order, for runtime table selection in regions/dictionaries.
    fn referred_huff_tables(&self, h: &SegmentHeader) -> Vec<&HuffTable> {
        h.referred
            .iter()
            .filter_map(|n| self.huff_tables.get(n))
            .collect()
    }

    /// Symbol dictionary segment (T.88 6.5 / 7.4.3). Supports arithmetic and
    /// Huffman entropy coding, and refinement/aggregate (SDREFAGG) symbols.
    fn decode_symbol_dict(&self, h: &SegmentHeader, data: &[u8]) -> Result<Vec<Rc<Bitmap>>> {
        let mut r = Reader::new(data);
        let flags = r.u16()?;
        let sd_huff = flags & 1 != 0;
        let sd_refagg = flags & 2 != 0;
        let huff_dh_sel = ((flags >> 2) & 3) as usize;
        let huff_dw_sel = ((flags >> 4) & 3) as usize;
        let huff_bm_sel = ((flags >> 6) & 1) as usize;
        let huff_agg_sel = ((flags >> 7) & 1) as usize;
        let ctx_used = flags & 0x100 != 0;
        let template = ((flags >> 10) & 3) as u8;
        let r_template = ((flags >> 12) & 1) as u8;
        if ctx_used {
            return Err(err("imported bitmap coding contexts are unsupported"));
        }

        let mut at = [(0i8, 0i8); 4];
        if !sd_huff {
            let n_at = if template == 0 { 4 } else { 1 };
            for slot in at.iter_mut().take(n_at) {
                *slot = (r.i8()?, r.i8()?);
            }
        }
        // Refinement AT pixels (only when refinement is used and r_template 0).
        let mut r_at = [(0i8, 0i8); 2];
        if sd_refagg && r_template == 0 {
            r_at[0] = (r.i8()?, r.i8()?);
            r_at[1] = (r.i8()?, r.i8()?);
        }

        let num_ex = r.u32()? as usize;
        let num_new = r.u32()? as usize;
        if num_ex > MAX_SYMBOLS || num_new > MAX_SYMBOLS {
            return Err(err(format!(
                "symbol dictionary declares too many symbols ({num_new} new, {num_ex} exported)"
            )));
        }

        // Input symbols: the exported symbols of referred symbol dictionaries,
        // in referral order. Other referred segment types are ignored.
        let mut input: Vec<Rc<Bitmap>> = Vec::new();
        for rs in &h.referred {
            match self.symbol_dicts.get(rs) {
                Some(Some(d)) => input.extend(d.iter().cloned()),
                Some(None) => return Err(err("referred symbol dictionary failed to decode")),
                None => {}
            }
        }
        if input.len() > MAX_SYMBOLS {
            return Err(err("too many input symbols"));
        }

        // Resolve the Huffman tables for SDHUFF (selectors 0/1 = standard,
        // 3 = next custom table in referral order). DH uses B.4/B.5, DW uses
        // B.2/B.3, BMSIZE uses B.1, AGGINST uses B.1.
        let custom = self.referred_huff_tables(h);
        let mut custom_iter = custom.iter();
        let mut next_custom = || -> Result<&HuffTable> {
            custom_iter
                .next()
                .copied()
                .ok_or_else(|| err("SDHUFF references missing custom table"))
        };

        let (dh_tab, dw_tab, bmsize_tab) = if sd_huff {
            let dh = match huff_dh_sel {
                0 => HuffSel::Std(standard_huff_table(4)),
                1 => HuffSel::Std(standard_huff_table(5)),
                _ => HuffSel::Custom(next_custom()?),
            };
            let dw = match huff_dw_sel {
                0 => HuffSel::Std(standard_huff_table(2)),
                1 => HuffSel::Std(standard_huff_table(3)),
                _ => HuffSel::Custom(next_custom()?),
            };
            let bmsize = match huff_bm_sel {
                0 => HuffSel::Std(standard_huff_table(1)),
                _ => HuffSel::Custom(next_custom()?),
            };
            // AGGINST table selection only matters for refinement+aggregation;
            // consume the selector so subsequent custom tables align.
            if sd_refagg {
                let _agg = match huff_agg_sel {
                    0 => HuffSel::Std(standard_huff_table(1)),
                    _ => HuffSel::Custom(next_custom()?),
                };
            }
            (Some(dh), Some(dw), Some(bmsize))
        } else {
            (None, None, None)
        };

        let total_input_plus_new = input.len() + num_new;
        let sym_code_len = ceil_log2(total_input_plus_new.max(1)).max(1);

        // Decode the new symbols, then the export flags, off one live stream.
        let new_syms = if sd_huff {
            self.decode_symbol_dict_huff(
                r.rest(),
                num_new,
                num_ex,
                &input,
                dh_tab.as_ref().unwrap(),
                dw_tab.as_ref().unwrap(),
                bmsize_tab.as_ref().unwrap(),
            )?
        } else {
            self.decode_symbol_dict_arith(
                r.rest(),
                num_new,
                num_ex,
                &input,
                template,
                at,
                sd_refagg,
                r_template,
                r_at,
                sym_code_len,
            )?
        };
        Ok(new_syms)
    }

    /// Arithmetic symbol-dictionary body: decode the new symbols (with optional
    /// refinement/aggregation) then the export flags, all off the live MQ
    /// stream, returning the exported symbols (T.88 6.5.5–6.5.10).
    #[allow(clippy::too_many_arguments)]
    fn decode_symbol_dict_arith(
        &self,
        payload: &[u8],
        num_new: usize,
        num_ex: usize,
        input: &[Rc<Bitmap>],
        template: u8,
        at: [(i8, i8); 4],
        sd_refagg: bool,
        r_template: u8,
        r_at: [(i8, i8); 2],
        sym_code_len: u32,
    ) -> Result<Vec<Rc<Bitmap>>> {
        let mut mq = MqDecoder::new(payload);
        let mut cx_gb = vec![0u8; 1 << 16];
        let mut cx_gr = vec![0u8; 1 << 13];
        let mut iadh = vec![0u8; INT_CTX_SIZE];
        let mut iadw = vec![0u8; INT_CTX_SIZE];
        let mut iaex = vec![0u8; INT_CTX_SIZE];
        let mut iaai = vec![0u8; INT_CTX_SIZE];
        let mut iadt = vec![0u8; INT_CTX_SIZE];
        let mut iafs = vec![0u8; INT_CTX_SIZE];
        let mut iads = vec![0u8; INT_CTX_SIZE];
        let mut iait = vec![0u8; INT_CTX_SIZE];
        let mut iari = vec![0u8; INT_CTX_SIZE];
        let mut iardw = vec![0u8; INT_CTX_SIZE];
        let mut iardh = vec![0u8; INT_CTX_SIZE];
        let mut iardx = vec![0u8; INT_CTX_SIZE];
        let mut iardy = vec![0u8; INT_CTX_SIZE];
        let mut iaid = vec![0u8; 1usize << (sym_code_len + 1)];
        let gp = GenericParams {
            template,
            tpgdon: false,
            at,
        };

        let mut new_syms: Vec<Rc<Bitmap>> = Vec::with_capacity(num_new.min(1024));
        let mut hc_height: i64 = 0;
        let mut total_area: usize = 0;
        while new_syms.len() < num_new {
            let dh = decode_int(&mut mq, &mut iadh).ok_or_else(|| err("unexpected OOB in IADH"))?;
            hc_height += dh;
            if hc_height <= 0 || hc_height > MAX_DIMENSION as i64 {
                return Err(err(format!("implausible symbol height {hc_height}")));
            }
            let mut sym_width: i64 = 0;
            while let Some(dw) = decode_int(&mut mq, &mut iadw) {
                sym_width += dw;
                if sym_width <= 0 || sym_width > MAX_DIMENSION as i64 {
                    return Err(err(format!("implausible symbol width {sym_width}")));
                }
                if new_syms.len() >= num_new {
                    return Err(err("more symbols coded than declared"));
                }
                total_area = total_area.saturating_add((sym_width * hc_height) as usize);
                if total_area > MAX_BITMAP_PIXELS {
                    return Err(err("symbol dictionary exceeds the pixel limit"));
                }
                let bm = if sd_refagg {
                    // REFAGG: IAAI counts the aggregate instances (T.88 6.5.8.2).
                    let n_inst = decode_int(&mut mq, &mut iaai)
                        .ok_or_else(|| err("unexpected OOB in IAAI"))?;
                    let all: Vec<Rc<Bitmap>> =
                        input.iter().chain(new_syms.iter()).cloned().collect();
                    if n_inst == 1 {
                        // Single-instance refinement of an existing symbol.
                        let id = decode_iaid(&mut mq, &mut iaid, sym_code_len);
                        let rdx = decode_int(&mut mq, &mut iardx)
                            .ok_or_else(|| err("unexpected OOB in IARDX"))?;
                        let rdy = decode_int(&mut mq, &mut iardy)
                            .ok_or_else(|| err("unexpected OOB in IARDY"))?;
                        let refr = all.get(id).ok_or_else(|| {
                            err(format!("refinement symbol id {id} out of range"))
                        })?;
                        decode_refinement(
                            &mut mq,
                            &mut cx_gr,
                            sym_width as usize,
                            hc_height as usize,
                            refr,
                            &RefinementParams {
                                template: r_template,
                                tpgron: false,
                                at: r_at,
                                dx: rdx as i32,
                                dy: rdy as i32,
                            },
                        )?
                    } else {
                        // Aggregate: a text region placing n_inst instances.
                        if n_inst <= 0 || n_inst as usize > MAX_INSTANCES {
                            return Err(err("implausible aggregate instance count"));
                        }
                        let mut ctx = TextArithCtx {
                            iadt: &mut iadt,
                            iafs: &mut iafs,
                            iads: &mut iads,
                            iait: &mut iait,
                            iari: &mut iari,
                            iardw: &mut iardw,
                            iardh: &mut iardh,
                            iardx: &mut iardx,
                            iardy: &mut iardy,
                            iaid: &mut iaid,
                            cx_gr: &mut cx_gr,
                        };
                        decode_text_region_arith(
                            &mut mq,
                            sym_width as usize,
                            hc_height as usize,
                            &all,
                            n_inst as usize,
                            sym_code_len,
                            &TextRegionGeom::aggregate(r_template, r_at),
                            &mut ctx,
                        )?
                    }
                } else {
                    decode_generic(
                        &mut mq,
                        &mut cx_gb,
                        sym_width as usize,
                        hc_height as usize,
                        &gp,
                    )?
                };
                new_syms.push(Rc::new(bm));
            }
        }

        // Export flags (T.88 6.5.10): IAEX run lengths over input++new.
        let total = input.len() + new_syms.len();
        let mut exported: Vec<Rc<Bitmap>> = Vec::with_capacity(num_ex.min(1024));
        let mut idx = 0usize;
        let mut exporting = false;
        while idx < total {
            let run =
                decode_int(&mut mq, &mut iaex).ok_or_else(|| err("unexpected OOB in IAEX"))?;
            if run < 0 || idx as i64 + run > total as i64 {
                return Err(err("invalid symbol export run length"));
            }
            if exporting {
                for i in idx..idx + run as usize {
                    exported.push(if i < input.len() {
                        Rc::clone(&input[i])
                    } else {
                        Rc::clone(&new_syms[i - input.len()])
                    });
                }
            }
            idx += run as usize;
            exporting = !exporting;
        }
        if exported.len() != num_ex {
            warn!(
                "JBIG2Decode: symbol dictionary exported {} symbols, declared {num_ex}",
                exported.len()
            );
        }
        Ok(exported)
    }

    /// Huffman symbol-dictionary body (T.88 6.5.8.2.3 / 6.5.9). Height-class
    /// collective bitmaps are MMR-coded (size 0) or fixed at BMSIZE bytes, then
    /// sliced per symbol width. Export flags use the table-B.1 run lengths.
    #[allow(clippy::too_many_arguments)]
    fn decode_symbol_dict_huff(
        &self,
        payload: &[u8],
        num_new: usize,
        num_ex: usize,
        input: &[Rc<Bitmap>],
        dh_tab: &HuffSel,
        dw_tab: &HuffSel,
        bmsize_tab: &HuffSel,
    ) -> Result<Vec<Rc<Bitmap>>> {
        let mut br = BitReader::new(payload);
        let mut new_syms: Vec<Rc<Bitmap>> = Vec::with_capacity(num_new.min(1024));
        let mut hc_height: i64 = 0;
        let mut total_area: usize = 0;
        while new_syms.len() < num_new {
            let dh = dh_tab
                .table()
                .decode(&mut br)
                .ok_or_else(|| err("unexpected OOB in Huffman DH"))?;
            hc_height += dh;
            if hc_height <= 0 || hc_height > MAX_DIMENSION as i64 {
                return Err(err(format!("implausible symbol height {hc_height}")));
            }
            let class_start = new_syms.len();
            let mut widths: Vec<usize> = Vec::new();
            let mut sym_width: i64 = 0;
            let mut totwidth: i64 = 0;
            // OOB from DW ends the height class.
            while let Some(dw) = dw_tab.table().decode(&mut br) {
                sym_width += dw;
                if sym_width <= 0 || sym_width > MAX_DIMENSION as i64 {
                    return Err(err(format!("implausible symbol width {sym_width}")));
                }
                if class_start + widths.len() >= num_new {
                    return Err(err("more symbols coded than declared"));
                }
                totwidth += sym_width;
                widths.push(sym_width as usize);
                total_area = total_area.saturating_add((sym_width * hc_height) as usize);
                if total_area > MAX_BITMAP_PIXELS {
                    return Err(err("symbol dictionary exceeds the pixel limit"));
                }
            }
            if totwidth <= 0 {
                continue;
            }
            // BMSIZE: 0 → MMR collective bitmap consuming the remainder of the
            // height class; >0 → exactly that many bytes (T.88 6.5.9).
            let bmsize = bmsize_tab
                .table()
                .decode(&mut br)
                .ok_or_else(|| err("unexpected OOB in Huffman BMSIZE"))?;
            br.byte_align();
            let cw = totwidth as usize;
            let ch = hc_height as usize;
            let start = br.byte_pos();
            let mmr_data = if bmsize == 0 {
                &payload[start.min(payload.len())..]
            } else {
                let end = start.saturating_add(bmsize as usize).min(payload.len());
                &payload[start.min(payload.len())..end]
            };
            let params = crate::ccitt::CcittParams {
                k: -1,
                columns: cw.max(1),
                rows: ch,
                black_is_1: true,
                byte_align: false,
            };
            let packed = crate::ccitt::decode(mmr_data, &params)?;
            let collective = unpack_rows(&packed, cw, ch)?;
            let consumed = if bmsize == 0 {
                payload.len().saturating_sub(start)
            } else {
                bmsize as usize
            };
            br.pos = (start + consumed) * 8;
            let mut x0 = 0usize;
            for &w in &widths {
                let mut sym = Bitmap::new(w, ch, 0)?;
                for yy in 0..ch {
                    for xx in 0..w {
                        if collective.get((x0 + xx) as i64, yy as i64) != 0 {
                            sym.set(xx, yy, 1);
                        }
                    }
                }
                new_syms.push(Rc::new(sym));
                x0 += w;
            }
        }

        // Export flags via table B.1 run lengths (T.88 6.5.10).
        let ex_tab = standard_huff_table(1);
        let total = input.len() + new_syms.len();
        let mut exported: Vec<Rc<Bitmap>> = Vec::with_capacity(num_ex.min(1024));
        let mut idx = 0usize;
        let mut exporting = false;
        while idx < total {
            let run = ex_tab
                .decode(&mut br)
                .ok_or_else(|| err("unexpected OOB in Huffman EXFLAGS"))?;
            if run < 0 || idx as i64 + run > total as i64 {
                return Err(err("invalid symbol export run length"));
            }
            if exporting {
                for i in idx..idx + run as usize {
                    exported.push(if i < input.len() {
                        Rc::clone(&input[i])
                    } else {
                        Rc::clone(&new_syms[i - input.len()])
                    });
                }
            }
            idx += run as usize;
            exporting = !exporting;
        }
        if exported.len() != num_ex {
            warn!(
                "JBIG2Decode: Huffman symbol dictionary exported {} symbols, declared {num_ex}",
                exported.len()
            );
        }
        Ok(exported)
    }

    /// Text region segment (T.88 6.4 / 7.4.4): arithmetic or Huffman coding,
    /// with optional per-instance symbol refinement.
    fn decode_text_region(&self, h: &SegmentHeader, data: &[u8]) -> Result<(RegionInfo, Bitmap)> {
        let mut r = Reader::new(data);
        let info = parse_region_info(&mut r)?;
        let flags = r.u16()?;
        let sb_huff = flags & 1 != 0;
        let sb_refine = flags & 2 != 0;
        let log_strips = ((flags >> 2) & 3) as u32;
        let strips = 1i64 << log_strips;
        let ref_corner = ((flags >> 4) & 3) as u8; // 0=BL 1=TL 2=BR 3=TR
        let transposed = flags & 0x40 != 0;
        let comb_op = ((flags >> 7) & 3) as u8;
        let def_pixel = ((flags >> 9) & 1) as u8;
        let mut ds_offset = ((flags >> 10) & 0x1F) as i64; // 5-bit signed
        if ds_offset > 15 {
            ds_offset -= 32;
        }
        let r_template = ((flags >> 15) & 1) as u8;

        // Huffman flags (T.88 7.4.4.1.2): per-field table selectors.
        let huff_flags = if sb_huff { r.u16()? } else { 0 };

        let mut r_at = [(0i8, 0i8); 2];
        if sb_refine && r_template == 0 {
            r_at[0] = (r.i8()?, r.i8()?);
            r_at[1] = (r.i8()?, r.i8()?);
        }
        let num_instances = r.u32()? as usize;
        if num_instances > MAX_INSTANCES {
            return Err(err(format!(
                "text region declares too many instances ({num_instances})"
            )));
        }

        // Gather symbols from the referred symbol dictionaries.
        let mut symbols: Vec<Rc<Bitmap>> = Vec::new();
        for rs in &h.referred {
            match self.symbol_dicts.get(rs) {
                Some(Some(d)) => symbols.extend(d.iter().cloned()),
                Some(None) => return Err(err("referred symbol dictionary failed to decode")),
                None => {}
            }
        }
        if symbols.is_empty() {
            return Err(err("text region refers to no usable symbols"));
        }
        if symbols.len() > MAX_SYMBOLS {
            // Also bounds the IAID context allocation below.
            return Err(err("too many symbols for text region"));
        }
        // T.88 (as amended) and shipping encoders use max(1, ceil(log2 n)).
        let sym_code_len = ceil_log2(symbols.len()).max(1);

        let geom = TextRegionGeom {
            log_strips,
            strips,
            ref_corner,
            transposed,
            comb_op,
            def_pixel,
            ds_offset,
            sb_refine,
            r_template,
            r_at,
        };

        if sb_huff {
            return self.decode_text_region_huff(
                r.rest(),
                &info,
                &geom,
                huff_flags,
                h,
                &symbols,
                num_instances,
            );
        }

        let mut mq = MqDecoder::new(r.rest());
        let mut iadt = vec![0u8; INT_CTX_SIZE];
        let mut iafs = vec![0u8; INT_CTX_SIZE];
        let mut iads = vec![0u8; INT_CTX_SIZE];
        let mut iait = vec![0u8; INT_CTX_SIZE];
        let mut iari = vec![0u8; INT_CTX_SIZE];
        let mut iardw = vec![0u8; INT_CTX_SIZE];
        let mut iardh = vec![0u8; INT_CTX_SIZE];
        let mut iardx = vec![0u8; INT_CTX_SIZE];
        let mut iardy = vec![0u8; INT_CTX_SIZE];
        let mut iaid = vec![0u8; 1usize << (sym_code_len + 1)];
        let mut cx_gr = vec![0u8; 1 << 13];
        let mut ctx = TextArithCtx {
            iadt: &mut iadt,
            iafs: &mut iafs,
            iads: &mut iads,
            iait: &mut iait,
            iari: &mut iari,
            iardw: &mut iardw,
            iardh: &mut iardh,
            iardx: &mut iardx,
            iardy: &mut iardy,
            iaid: &mut iaid,
            cx_gr: &mut cx_gr,
        };
        let bm = decode_text_region_arith(
            &mut mq,
            info.width,
            info.height,
            &symbols,
            num_instances,
            sym_code_len,
            &geom,
            &mut ctx,
        )?;
        Ok((info, bm))
    }

    /// Huffman text-region body (T.88 6.4.5 with Huffman entropy). Resolves the
    /// FS/DS/DT/RDW/RDH/RDX/RDY tables and the runtime symbol-ID table, then
    /// runs the placement loop with optional refinement.
    #[allow(clippy::too_many_arguments)]
    fn decode_text_region_huff(
        &self,
        payload: &[u8],
        info: &RegionInfo,
        geom: &TextRegionGeom,
        huff_flags: u16,
        h: &SegmentHeader,
        symbols: &[Rc<Bitmap>],
        num_instances: usize,
    ) -> Result<(RegionInfo, Bitmap)> {
        let custom = self.referred_huff_tables(h);
        let mut custom_iter = custom.iter();
        let mut next_custom = || -> Result<&HuffTable> {
            custom_iter
                .next()
                .copied()
                .ok_or_else(|| err("SBHUFF references missing custom table"))
        };

        // Field-table selectors (T.88 7.4.4.1.2 Table 32).
        let fs_sel = huff_flags & 3;
        let ds_sel = (huff_flags >> 2) & 3;
        let dt_sel = (huff_flags >> 4) & 3;
        let rdw_sel = (huff_flags >> 6) & 3;
        let rdh_sel = (huff_flags >> 8) & 3;
        let rdx_sel = (huff_flags >> 10) & 3;
        let rdy_sel = (huff_flags >> 12) & 3;
        let rsize_sel = (huff_flags >> 14) & 1;

        let fs_tab = match fs_sel {
            0 => HuffSel::Std(standard_huff_table(6)),
            1 => HuffSel::Std(standard_huff_table(7)),
            _ => HuffSel::Custom(next_custom()?),
        };
        let ds_tab = match ds_sel {
            0 => HuffSel::Std(standard_huff_table(8)),
            1 => HuffSel::Std(standard_huff_table(9)),
            2 => HuffSel::Std(standard_huff_table(10)),
            _ => HuffSel::Custom(next_custom()?),
        };
        let dt_tab = match dt_sel {
            0 => HuffSel::Std(standard_huff_table(11)),
            1 => HuffSel::Std(standard_huff_table(12)),
            2 => HuffSel::Std(standard_huff_table(13)),
            _ => HuffSel::Custom(next_custom()?),
        };
        let rdw_tab = match rdw_sel {
            0 => HuffSel::Std(standard_huff_table(14)),
            1 => HuffSel::Std(standard_huff_table(15)),
            _ => HuffSel::Custom(next_custom()?),
        };
        let rdh_tab = match rdh_sel {
            0 => HuffSel::Std(standard_huff_table(14)),
            1 => HuffSel::Std(standard_huff_table(15)),
            _ => HuffSel::Custom(next_custom()?),
        };
        let rdx_tab = match rdx_sel {
            0 => HuffSel::Std(standard_huff_table(14)),
            1 => HuffSel::Std(standard_huff_table(15)),
            _ => HuffSel::Custom(next_custom()?),
        };
        let rdy_tab = match rdy_sel {
            0 => HuffSel::Std(standard_huff_table(14)),
            1 => HuffSel::Std(standard_huff_table(15)),
            _ => HuffSel::Custom(next_custom()?),
        };
        let rsize_tab = match rsize_sel {
            0 => HuffSel::Std(standard_huff_table(1)),
            _ => HuffSel::Custom(next_custom()?),
        };

        let mut br = BitReader::new(payload);
        // Symbol-ID Huffman table (T.88 7.4.4.1.4): 35 run-code lengths, then
        // the per-symbol code lengths decoded with those run codes.
        let id_tab = read_symbol_id_huff_table(&mut br, symbols.len())?;

        let mut bm = Bitmap::new(info.width, info.height, geom.def_pixel)?;
        let strips = geom.strips;

        let mut stript = -dt_tab
            .table()
            .decode(&mut br)
            .ok_or_else(|| err("unexpected OOB in Huffman DT"))?
            * strips;
        let mut firsts: i64 = 0;
        let mut inst = 0usize;

        'instances: while inst < num_instances {
            let dt = dt_tab
                .table()
                .decode(&mut br)
                .ok_or_else(|| err("unexpected OOB in Huffman DT"))?;
            stript += dt * strips;
            let dfs = fs_tab
                .table()
                .decode(&mut br)
                .ok_or_else(|| err("unexpected OOB in Huffman FS"))?;
            firsts += dfs;
            let mut curs = firsts;
            let mut first = true;
            loop {
                if !first {
                    let Some(ids) = ds_tab.table().decode(&mut br) else {
                        break; // OOB ends the strip
                    };
                    curs += ids + geom.ds_offset;
                }
                first = false;
                if inst >= num_instances {
                    break 'instances;
                }
                let curt = if strips == 1 {
                    0
                } else {
                    br.read_bits(geom.log_strips) as i64
                };
                let t = stript + curt;
                let id = id_tab
                    .decode(&mut br)
                    .ok_or_else(|| err("unexpected OOB in symbol ID"))?
                    as usize;
                let sym = symbols
                    .get(id)
                    .ok_or_else(|| err(format!("symbol id {id} out of range")))?
                    .clone();

                let refined = if geom.sb_refine {
                    let ri = br.read_bit();
                    if ri != 0 {
                        let rdw = rdw_tab
                            .table()
                            .decode(&mut br)
                            .ok_or_else(|| err("unexpected OOB in Huffman RDW"))?;
                        let rdh = rdh_tab
                            .table()
                            .decode(&mut br)
                            .ok_or_else(|| err("unexpected OOB in Huffman RDH"))?;
                        let rdx = rdx_tab
                            .table()
                            .decode(&mut br)
                            .ok_or_else(|| err("unexpected OOB in Huffman RDX"))?;
                        let rdy = rdy_tab
                            .table()
                            .decode(&mut br)
                            .ok_or_else(|| err("unexpected OOB in Huffman RDY"))?;
                        let _rsize = rsize_tab
                            .table()
                            .decode(&mut br)
                            .ok_or_else(|| err("unexpected OOB in Huffman RSIZE"))?;
                        br.byte_align();
                        let nw = sym.width as i64 + rdw;
                        let nh = sym.height as i64 + rdh;
                        if nw <= 0
                            || nh <= 0
                            || nw > MAX_DIMENSION as i64
                            || nh > MAX_DIMENSION as i64
                        {
                            return Err(err("implausible refined symbol size"));
                        }
                        let dx = (rdw >> 1) + rdx;
                        let dy = (rdh >> 1) + rdy;
                        let mut mq = MqDecoder::new(&payload[br.byte_pos().min(payload.len())..]);
                        let mut cx = vec![0u8; 1 << 13];
                        let refined = decode_refinement(
                            &mut mq,
                            &mut cx,
                            nw as usize,
                            nh as usize,
                            &sym,
                            &RefinementParams {
                                template: geom.r_template,
                                tpgron: false,
                                at: geom.r_at,
                                dx: dx as i32,
                                dy: dy as i32,
                            },
                        )?;
                        // Advance past the refinement MQ data using RSIZE.
                        br.pos = (br.byte_pos() + _rsize.max(0) as usize) * 8;
                        Some(refined)
                    } else {
                        None
                    }
                } else {
                    None
                };

                curs += place_instance(&mut bm, &sym, refined, t, curs, geom);
                inst += 1;
            }
        }
        Ok((info.clone(), bm))
    }

    /// Pattern dictionary segment (T.88 6.7 / 7.4.4). Decodes one wide generic
    /// region holding HDMAX+1 patterns side by side and slices it into the
    /// individual pattern bitmaps.
    fn decode_pattern_dict(&self, data: &[u8]) -> Result<PatternDict> {
        let mut r = Reader::new(data);
        let flags = r.u8()?;
        let mmr = flags & 1 != 0;
        let template = (flags >> 1) & 3;
        let hdpw = r.u8()? as usize;
        let hdph = r.u8()? as usize;
        let gray_max = r.u32()? as usize; // HDMAX (largest pattern index)
        if hdpw == 0 || hdph == 0 || hdpw > MAX_DIMENSION || hdph > MAX_DIMENSION {
            return Err(err(format!("implausible pattern size {hdpw}x{hdph}")));
        }
        let n_patterns = gray_max
            .checked_add(1)
            .filter(|&n| n <= MAX_SYMBOLS)
            .ok_or_else(|| err("pattern dictionary declares too many patterns"))?;
        let collective_w = hdpw
            .checked_mul(n_patterns)
            .filter(|&w| w <= MAX_DIMENSION)
            .ok_or_else(|| err("pattern dictionary collective bitmap too wide"))?;
        if collective_w
            .checked_mul(hdph)
            .map(|p| p > MAX_BITMAP_PIXELS)
            .unwrap_or(true)
        {
            return Err(err("pattern dictionary exceeds the pixel limit"));
        }

        // Decode the collective bitmap. The AT pixels are fixed (T.88 6.7.5):
        // A1 = (-HDPW, 0), A2 = (-3, -1), A3 = (2, -2), A4 = (-2, -2).
        let collective = if mmr {
            let params = crate::ccitt::CcittParams {
                k: -1,
                columns: collective_w.max(1),
                rows: hdph,
                black_is_1: true,
                byte_align: false,
            };
            let packed = crate::ccitt::decode(r.rest(), &params)?;
            unpack_rows(&packed, collective_w, hdph)?
        } else {
            let at = [
                (-(hdpw as i64).min(127) as i8, 0i8),
                (-3, -1),
                (2, -2),
                (-2, -2),
            ];
            let mut mq = MqDecoder::new(r.rest());
            let mut cx = vec![0u8; 1 << 16];
            decode_generic(
                &mut mq,
                &mut cx,
                collective_w,
                hdph,
                &GenericParams {
                    template,
                    tpgdon: false,
                    at,
                },
            )?
        };

        // Slice into HDMAX+1 patterns (T.88 6.7.5 step 4).
        let mut patterns = Vec::with_capacity(n_patterns);
        for idx in 0..n_patterns {
            let x0 = idx * hdpw;
            let mut p = Bitmap::new(hdpw, hdph, 0)?;
            for yy in 0..hdph {
                for xx in 0..hdpw {
                    if collective.get((x0 + xx) as i64, yy as i64) != 0 {
                        p.set(xx, yy, 1);
                    }
                }
            }
            patterns.push(Rc::new(p));
        }
        Ok(PatternDict { patterns })
    }

    /// Halftone region segment (T.88 6.6 / 7.4.5). Decodes the grayscale image
    /// (HBPP Gray-coded bitplanes) and tiles the referred pattern dictionary
    /// across the HGW×HGH grid.
    fn decode_halftone_region(
        &self,
        h: &SegmentHeader,
        data: &[u8],
    ) -> Result<(RegionInfo, Bitmap)> {
        let mut r = Reader::new(data);
        let info = parse_region_info(&mut r)?;
        let flags = r.u8()?;
        let mmr = flags & 1 != 0;
        let template = (flags >> 1) & 3;
        let enable_skip = flags & 8 != 0;
        let comb_op = (flags >> 4) & 7;
        let def_pixel = (flags >> 7) & 1;
        let hgw = r.u32()? as usize;
        let hgh = r.u32()? as usize;
        let hgx = r.u32()? as i32;
        let hgy = r.u32()? as i32;
        let hrx = r.u16()? as i32;
        let hry = r.u16()? as i32;

        if hgw == 0 || hgh == 0 || hgw > MAX_DIMENSION || hgh > MAX_DIMENSION {
            return Err(err(format!("implausible halftone grid {hgw}x{hgh}")));
        }
        let grid_cells = hgw
            .checked_mul(hgh)
            .filter(|&n| n <= MAX_BITMAP_PIXELS)
            .ok_or_else(|| err("halftone grid exceeds the pixel limit"))?;

        // Find the referred pattern dictionary.
        let pd = h
            .referred
            .iter()
            .find_map(|n| self.pattern_dicts.get(n))
            .ok_or_else(|| err("halftone region refers to no pattern dictionary"))?
            .as_ref()
            .ok_or_else(|| err("referred pattern dictionary failed to decode"))?;
        if pd.patterns.is_empty() {
            return Err(err("empty pattern dictionary"));
        }
        let pat_w = pd.patterns[0].width;
        let pat_h = pd.patterns[0].height;
        // HBPP = ceil(log2(number of patterns)) (T.88 C.5).
        let hbpp = ceil_log2(pd.patterns.len()).max(1) as usize;
        if hbpp > 31 {
            return Err(err("implausible halftone bit depth"));
        }

        let mut bm = Bitmap::new(info.width, info.height, def_pixel)?;

        // Optional skip bitmap (T.88 6.6.5.1): a cell is skipped when its
        // pattern would fall entirely outside the region.
        let skip = if enable_skip {
            let mut s = Bitmap::new(hgw, hgh, 0)?;
            for m in 0..hgh as i32 {
                for n in 0..hgw as i32 {
                    let x = (hgx + m * hry + n * hrx) >> 8;
                    let y = (hgy + m * hrx - n * hry) >> 8;
                    if x + (pat_w as i32) <= 0
                        || x >= info.width as i32
                        || y + (pat_h as i32) <= 0
                        || y >= info.height as i32
                    {
                        s.set(n as usize, m as usize, 1);
                    }
                }
            }
            Some(s)
        } else {
            None
        };

        // Decode the grayscale image: HBPP bitplanes, MSB first, Gray-coded
        // (T.88 Annex C.5). Each plane is a generic region of size HGW×HGH.
        let mut gray = vec![0u32; grid_cells];
        // The grayscale generic regions use template AT pixels per T.88 C.2:
        // A1 = (template<=1 ? 3 : 2, -1), then (-3,-1), (2,-2), (-2,-2).
        let at = [
            (if template <= 1 { 3 } else { 2 }, -1),
            (-3, -1),
            (2, -2),
            (-2, -2),
        ];
        let mut mq = if mmr {
            None
        } else {
            Some(MqDecoder::new(r.rest()))
        };
        let mut cx = if mmr { Vec::new() } else { vec![0u8; 1 << 16] };
        // MMR planes are concatenated; the CCITT decoder consumes one full
        // image per call but does not report bytes consumed, so multi-plane
        // MMR halftones can only be tracked for the common single-plane case.
        let mmr_data = r.rest();
        // `bit[j+1]` accumulator for the Gray decode, per cell.
        let mut prev_bit = vec![0u8; grid_cells];
        for j in (0..hbpp).rev() {
            let plane = if let Some(mqd) = mq.as_mut() {
                decode_generic(
                    mqd,
                    &mut cx,
                    hgw,
                    hgh,
                    &GenericParams {
                        template,
                        tpgdon: false,
                        at,
                    },
                )?
            } else {
                let params = crate::ccitt::CcittParams {
                    k: -1,
                    columns: hgw.max(1),
                    rows: hgh,
                    black_is_1: true,
                    byte_align: false,
                };
                let packed = crate::ccitt::decode(mmr_data, &params)?;
                unpack_rows(&packed, hgw, hgh)?
            };
            // Gray-code combine: the j-th gray bit is plane XOR the (j+1)-th bit.
            for i in 0..grid_cells {
                let py = (i / hgw) as i64;
                let px = (i % hgw) as i64;
                let bit = plane.get(px, py) ^ prev_bit[i];
                gray[i] |= (bit as u32) << j;
                prev_bit[i] = bit;
            }
        }

        // Place patterns on the grid (T.88 6.6.5.2).
        let n_pat = pd.patterns.len();
        for m in 0..hgh {
            for n in 0..hgw {
                if let Some(s) = &skip {
                    if s.get(n as i64, m as i64) != 0 {
                        continue;
                    }
                }
                let gi = gray[m * hgw + n] as usize;
                let idx = gi.min(n_pat - 1);
                let pat = &pd.patterns[idx];
                let x = (hgx + (m as i32) * hry + (n as i32) * hrx) >> 8;
                let y = (hgy + (m as i32) * hrx - (n as i32) * hry) >> 8;
                place_pattern(&mut bm, pat, x, y, comb_op);
            }
        }
        Ok((info, bm))
    }

    /// Final page bitmap → packed MSB-first rows, inverted to PDF polarity
    /// (JBIG2 1 = black becomes the PDF 1-bpc convention black = 0).
    fn into_packed_output(self) -> Result<Vec<u8>> {
        let Some(bm) = self.page else {
            return Err(err("stream contains no page"));
        };
        let row_bytes = bm.width.div_ceil(8);
        let size = row_bytes
            .checked_mul(bm.height)
            .filter(|&s| s <= MAX_OUTPUT)
            .ok_or_else(|| err("page exceeds the output size limit"))?;
        let mut out = vec![0u8; size];
        for y in 0..bm.height {
            let row = &bm.data[y * bm.width..(y + 1) * bm.width];
            let out_row = &mut out[y * row_bytes..(y + 1) * row_bytes];
            for (x, &px) in row.iter().enumerate() {
                if px == 0 {
                    out_row[x / 8] |= 0x80 >> (x % 8);
                }
            }
        }
        Ok(out)
    }
}

/// Unpack CCITT-style packed rows (MSB-first, 1 = black) into a bitmap.
/// Short input leaves the remaining rows white, matching the lenient CCITT
/// decoder, which stops at input exhaustion.
fn unpack_rows(packed: &[u8], width: usize, height: usize) -> Result<Bitmap> {
    let mut bm = Bitmap::new(width, height, 0)?;
    let row_bytes = width.div_ceil(8);
    for y in 0..height {
        let Some(row) = packed.get(y * row_bytes..(y + 1) * row_bytes) else {
            break;
        };
        for x in 0..width {
            if (row[x / 8] >> (7 - x % 8)) & 1 == 1 {
                bm.set(x, y, 1);
            }
        }
    }
    Ok(bm)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Test-only MQ *encoder* (T.88 E.3.2–E.3.8, software conventions). Used to
    // hand-build JBIG2 fixtures; validated against the spec's Annex H.2
    // conformance vector below, so the decoder is checked against an external
    // reference rather than only its own mirror image.
    // -----------------------------------------------------------------------

    struct MqEncoder {
        a: u32,
        c: u32,
        ct: i32,
        /// Byte at the (conceptual) BP pointer; the first one is fictitious
        /// (INITENC starts BP one before the output) and is never emitted.
        b: u32,
        first: bool,
        out: Vec<u8>,
    }

    impl MqEncoder {
        fn new() -> Self {
            // INITENC: A = 0x8000, C = 0, CT = 12 (the fictitious B is not 0xFF).
            MqEncoder {
                a: 0x8000,
                c: 0,
                ct: 12,
                b: 0,
                first: true,
                out: Vec::new(),
            }
        }

        /// Advance BP: flush the current B (skipping the fictitious first one).
        fn push_b(&mut self) {
            if !self.first {
                self.out.push(self.b as u8);
            }
            self.first = false;
        }

        /// BYTEOUT (T.88 E.3.7) with carry propagation and 0xFF stuffing.
        fn byte_out(&mut self) {
            if self.b == 0xFF {
                self.push_b();
                self.b = self.c >> 20;
                self.c &= 0xF_FFFF;
                self.ct = 7;
            } else if self.c < 0x800_0000 {
                self.push_b();
                self.b = self.c >> 19;
                self.c &= 0x7_FFFF;
                self.ct = 8;
            } else {
                self.b += 1; // carry into the previous byte
                if self.b == 0xFF {
                    self.c &= 0x7FF_FFFF;
                    self.push_b();
                    self.b = self.c >> 20;
                    self.c &= 0xF_FFFF;
                    self.ct = 7;
                } else {
                    self.push_b();
                    self.b = self.c >> 19;
                    self.c &= 0x7_FFFF;
                    self.ct = 8;
                }
            }
        }

        /// ENCODE: one bit `d` under the adaptive context `cx[idx]`.
        fn encode(&mut self, cx: &mut [u8], idx: usize, d: u8) {
            let state = cx[idx];
            let i = (state >> 1) as usize;
            let mps = state & 1;
            let (qe16, nmps, nlps, switch) = QE_TABLE[i];
            let qe = qe16 as u32;
            if d == mps {
                // CODEMPS
                self.a -= qe;
                if self.a & 0x8000 == 0 {
                    if self.a < qe {
                        self.a = qe;
                    } else {
                        self.c += qe;
                    }
                    cx[idx] = (nmps << 1) | mps;
                    self.renorm();
                } else {
                    self.c += qe;
                }
            } else {
                // CODELPS
                self.a -= qe;
                if self.a < qe {
                    self.c += qe;
                } else {
                    self.a = qe;
                }
                let mps = if switch == 1 { 1 - mps } else { mps };
                cx[idx] = (nlps << 1) | mps;
                self.renorm();
            }
        }

        /// RENORME.
        fn renorm(&mut self) {
            loop {
                self.a <<= 1;
                self.c <<= 1;
                self.ct -= 1;
                if self.ct == 0 {
                    self.byte_out();
                }
                if self.a & 0x8000 != 0 {
                    break;
                }
            }
        }

        /// FLUSH (SETBITS + two byteouts), returning the code stream.
        fn finish(mut self) -> Vec<u8> {
            // SETBITS: set as many trailing C bits to 1 as possible without
            // leaving the [C, C + A) interval.
            let tempc = self.c + self.a;
            self.c |= 0xFFFF;
            if self.c >= tempc {
                self.c -= 0x8000;
            }
            self.c <<= self.ct;
            self.byte_out();
            self.c <<= self.ct;
            self.byte_out();
            self.push_b();
            // The spec terminates the code stream with the 0xFF 0xAC marker.
            self.out.push(0xFF);
            self.out.push(0xAC);
            self.out
        }
    }

    /// Encoder mirror of [`read_int_bits`].
    fn encode_int_bits(enc: &mut MqEncoder, cx: &mut [u8], prev: &mut usize, v: u32, n: u32) {
        for k in (0..n).rev() {
            let bit = ((v >> k) & 1) as u8;
            enc.encode(cx, *prev, bit);
            *prev = if *prev < 256 {
                (*prev << 1) | bit as usize
            } else {
                (((*prev << 1) | bit as usize) & 511) | 256
            };
        }
    }

    /// Encoder mirror of [`decode_int`] (`None` encodes OOB).
    fn encode_int(enc: &mut MqEncoder, cx: &mut [u8], value: Option<i64>) {
        let mut prev = 1usize;
        let (sign, mag) = match value {
            None => (1u32, 0i64), // OOB = "negative zero"
            Some(v) if v < 0 => (1, -v),
            Some(v) => (0, v),
        };
        encode_int_bits(enc, cx, &mut prev, sign, 1);
        if mag < 4 {
            encode_int_bits(enc, cx, &mut prev, 0b0, 1);
            encode_int_bits(enc, cx, &mut prev, mag as u32, 2);
        } else if mag < 20 {
            encode_int_bits(enc, cx, &mut prev, 0b10, 2);
            encode_int_bits(enc, cx, &mut prev, (mag - 4) as u32, 4);
        } else if mag < 84 {
            encode_int_bits(enc, cx, &mut prev, 0b110, 3);
            encode_int_bits(enc, cx, &mut prev, (mag - 20) as u32, 6);
        } else if mag < 340 {
            encode_int_bits(enc, cx, &mut prev, 0b1110, 4);
            encode_int_bits(enc, cx, &mut prev, (mag - 84) as u32, 8);
        } else if mag < 4436 {
            encode_int_bits(enc, cx, &mut prev, 0b11110, 5);
            encode_int_bits(enc, cx, &mut prev, (mag - 340) as u32, 12);
        } else {
            encode_int_bits(enc, cx, &mut prev, 0b11111, 5);
            encode_int_bits(enc, cx, &mut prev, (mag - 4436) as u32, 32);
        }
    }

    /// Encoder mirror of [`decode_iaid`].
    fn encode_iaid(enc: &mut MqEncoder, cx: &mut [u8], code_len: u32, id: usize) {
        let mut prev = 1usize;
        for k in (0..code_len).rev() {
            let bit = ((id >> k) & 1) as u8;
            enc.encode(cx, prev, bit);
            prev = (prev << 1) | bit as usize;
        }
    }

    /// Encoder mirror of [`decode_generic`]. Valid AT pixels reference only
    /// already-coded pixels, so encoding straight off the full bitmap matches
    /// the decoder's progressive reconstruction.
    fn encode_generic(enc: &mut MqEncoder, cx: &mut [u8], bm: &Bitmap, p: &GenericParams) {
        let ctx_at = context_fn(p.template);
        let tpgd_cx = tpgd_context(p.template);
        let row_eq_prev = |y: usize| {
            let row = &bm.data[y * bm.width..(y + 1) * bm.width];
            if y == 0 {
                row.iter().all(|&px| px == 0)
            } else {
                row == &bm.data[(y - 1) * bm.width..y * bm.width]
            }
        };
        let mut ltp = false;
        for y in 0..bm.height {
            if p.tpgdon {
                let typical = row_eq_prev(y);
                enc.encode(cx, tpgd_cx, (typical != ltp) as u8); // SLTP
                ltp = typical;
                if ltp {
                    continue;
                }
            }
            for x in 0..bm.width {
                let ctx = ctx_at(bm, x as i64, y as i64, &p.at);
                enc.encode(cx, ctx, bm.get(x as i64, y as i64));
            }
        }
    }

    /// Encoder mirror of [`decode_refinement`]: encodes `bm` as a refinement of
    /// `refr`. With TPGRON off (used by the tests) the loop matches the decoder
    /// pixel-for-pixel.
    fn encode_refinement(
        enc: &mut MqEncoder,
        cx: &mut [u8],
        bm: &Bitmap,
        refr: &Bitmap,
        p: &RefinementParams,
    ) {
        assert!(!p.tpgron, "test encoder only covers TPGRON off");
        for y in 0..bm.height as i64 {
            for x in 0..bm.width as i64 {
                let rx = x - p.dx as i64;
                let ry = y - p.dy as i64;
                let ctx = refine_context(p.template, bm, refr, x, y, rx, ry, &p.at);
                enc.encode(cx, ctx, bm.get(x, y));
            }
        }
    }

    // -----------------------------------------------------------------------
    // Test-only Huffman bit writer and table encoder.
    // -----------------------------------------------------------------------

    /// MSB-first bit writer, the encode mirror of [`BitReader`].
    struct BitWriter {
        out: Vec<u8>,
        cur: u8,
        nbits: u32,
    }

    impl BitWriter {
        fn new() -> Self {
            Self {
                out: Vec::new(),
                cur: 0,
                nbits: 0,
            }
        }
        fn write_bit(&mut self, bit: u32) {
            self.cur = (self.cur << 1) | (bit & 1) as u8;
            self.nbits += 1;
            if self.nbits == 8 {
                self.out.push(self.cur);
                self.cur = 0;
                self.nbits = 0;
            }
        }
        fn write_bits(&mut self, v: u32, n: u32) {
            for k in (0..n).rev() {
                self.write_bit((v >> k) & 1);
            }
        }
        fn byte_align(&mut self) {
            if self.nbits > 0 {
                self.cur <<= 8 - self.nbits;
                self.out.push(self.cur);
                self.cur = 0;
                self.nbits = 0;
            }
        }
        fn finish(mut self) -> Vec<u8> {
            self.byte_align();
            self.out
        }
    }

    /// Encode `value` (or OOB) with a Huffman table by finding the matching
    /// line and emitting its prefix code plus the range magnitude.
    fn huff_encode(bw: &mut BitWriter, tab: &HuffTable, value: Option<i64>) {
        for (i, l) in tab.lines.iter().enumerate() {
            let matches = match value {
                None => l.is_oob,
                Some(v) => {
                    if l.is_oob {
                        false
                    } else if l.range_len == 32 {
                        if l.is_lower {
                            v <= l.range_low as i64
                        } else {
                            v >= l.range_low as i64
                        }
                    } else {
                        let span = 1i64 << l.range_len;
                        if l.is_lower {
                            v <= l.range_low as i64 && v > l.range_low as i64 - span
                        } else {
                            v >= l.range_low as i64 && v < l.range_low as i64 + span
                        }
                    }
                }
            };
            if matches && l.prefix_len > 0 {
                bw.write_bits(tab.codes[i], l.prefix_len as u32);
                if l.is_oob {
                    return;
                }
                let mag = if l.is_lower {
                    l.range_low as i64 - value.unwrap()
                } else {
                    value.unwrap() - l.range_low as i64
                };
                let bits = if l.range_len == 32 {
                    32
                } else {
                    l.range_len as u32
                };
                bw.write_bits(mag as u32, bits);
                return;
            }
        }
        panic!("no Huffman line matches {value:?}");
    }

    // -----------------------------------------------------------------------
    // Fixture-building helpers.
    // -----------------------------------------------------------------------

    /// Nominal AT pixel positions per GB template.
    fn nominal_at(template: u8) -> [(i8, i8); 4] {
        match template {
            0 => [(3, -1), (-3, -1), (2, -2), (-2, -2)],
            1 | 2 => [
                (if template == 1 { 3 } else { 2 }, -1),
                (0, 0),
                (0, 0),
                (0, 0),
            ],
            _ => [(2, -1), (0, 0), (0, 0), (0, 0)],
        }
    }

    /// Bitmap from ASCII art: '#' = black (JBIG2 native 1).
    fn bitmap_from(rows: &[&str]) -> Bitmap {
        let mut bm = Bitmap::new(rows[0].len(), rows.len(), 0).unwrap();
        for (y, row) in rows.iter().enumerate() {
            for (x, c) in row.chars().enumerate() {
                if c == '#' {
                    bm.set(x, y, 1);
                }
            }
        }
        bm
    }

    /// Expected packed PDF-polarity rows: '#' (black) = 0, anything else = 1.
    fn packed_from(rows: &[&str]) -> Vec<u8> {
        let row_bytes = rows[0].len().div_ceil(8);
        let mut out = vec![0u8; row_bytes * rows.len()];
        for (y, row) in rows.iter().enumerate() {
            for (x, c) in row.chars().enumerate() {
                if c != '#' {
                    out[y * row_bytes + x / 8] |= 0x80 >> (x % 8);
                }
            }
        }
        out
    }

    /// Wrap a payload in a segment header (short forms only: number ≤ 256,
    /// ≤ 4 referred segments, 1-byte page association).
    fn segment(number: u32, seg_type: u8, referred: &[u32], page: u8, payload: &[u8]) -> Vec<u8> {
        assert!(number <= 256 && referred.len() <= 4);
        let mut out = Vec::new();
        out.extend_from_slice(&number.to_be_bytes());
        out.push(seg_type);
        out.push((referred.len() as u8) << 5);
        for &r in referred {
            out.push(r as u8);
        }
        out.push(page);
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn page_info_payload(width: u32, height: u32, flags: u8) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&width.to_be_bytes());
        p.extend_from_slice(&height.to_be_bytes());
        p.extend_from_slice(&[0; 8]); // X/Y resolution: unspecified
        p.push(flags);
        p.extend_from_slice(&[0, 0]); // striping information
        p
    }

    fn region_info(w: u32, h: u32, x: u32, y: u32, op: u8) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&w.to_be_bytes());
        p.extend_from_slice(&h.to_be_bytes());
        p.extend_from_slice(&x.to_be_bytes());
        p.extend_from_slice(&y.to_be_bytes());
        p.push(op);
        p
    }

    /// Arithmetic generic region segment payload.
    fn generic_region_payload(bm: &Bitmap, template: u8, tpgdon: bool) -> Vec<u8> {
        let at = nominal_at(template);
        let mut p = region_info(bm.width as u32, bm.height as u32, 0, 0, 0);
        p.push(((tpgdon as u8) << 3) | (template << 1)); // MMR = 0
        let n_at = if template == 0 { 4 } else { 1 };
        for &(ax, ay) in at.iter().take(n_at) {
            p.push(ax as u8);
            p.push(ay as u8);
        }
        let mut enc = MqEncoder::new();
        let mut cx = vec![0u8; 1 << 16];
        encode_generic(
            &mut enc,
            &mut cx,
            bm,
            &GenericParams {
                template,
                tpgdon,
                at,
            },
        );
        p.extend_from_slice(&enc.finish());
        p
    }

    /// Symbol dictionary payload: all `symbols` (equal heights, one height
    /// class) coded with GB template 0 and exported.
    fn symbol_dict_payload(symbols: &[&Bitmap]) -> Vec<u8> {
        let at = nominal_at(0);
        let gp = GenericParams {
            template: 0,
            tpgdon: false,
            at,
        };
        let mut p = Vec::new();
        p.extend_from_slice(&0u16.to_be_bytes()); // flags: arithmetic, template 0
        for &(ax, ay) in at.iter() {
            p.push(ax as u8);
            p.push(ay as u8);
        }
        p.extend_from_slice(&(symbols.len() as u32).to_be_bytes()); // SDNUMEXSYMS
        p.extend_from_slice(&(symbols.len() as u32).to_be_bytes()); // SDNUMNEWSYMS

        let mut enc = MqEncoder::new();
        let mut cx_gb = vec![0u8; 1 << 16];
        let mut iadh = vec![0u8; INT_CTX_SIZE];
        let mut iadw = vec![0u8; INT_CTX_SIZE];
        let mut iaex = vec![0u8; INT_CTX_SIZE];

        let height = symbols[0].height as i64;
        encode_int(&mut enc, &mut iadh, Some(height)); // HCDH from 0
        let mut width = 0i64;
        for sym in symbols {
            assert_eq!(sym.height as i64, height, "one height class only");
            encode_int(&mut enc, &mut iadw, Some(sym.width as i64 - width));
            width = sym.width as i64;
            encode_generic(&mut enc, &mut cx_gb, sym, &gp);
        }
        encode_int(&mut enc, &mut iadw, None); // OOB closes the height class
        encode_int(&mut enc, &mut iaex, Some(0)); // run of 0 not exported …
        encode_int(&mut enc, &mut iaex, Some(symbols.len() as i64)); // … then all
        p.extend_from_slice(&enc.finish());
        p
    }

    /// Text region payload placing `placements` = (symbol id, ds-delta before
    /// the instance, dt) on one strip (SBSTRIPS = 1, TOPLEFT, OR).
    fn text_region_payload(
        w: u32,
        h: u32,
        num_syms: usize,
        placements: &[(usize, i64)],
    ) -> Vec<u8> {
        let mut p = region_info(w, h, 0, 0, 0);
        p.extend_from_slice(&(1u16 << 4).to_be_bytes()); // flags: REFCORNER = TOPLEFT
        p.extend_from_slice(&(placements.len() as u32).to_be_bytes());

        let code_len = ceil_log2(num_syms).max(1);
        let mut enc = MqEncoder::new();
        let mut iadt = vec![0u8; INT_CTX_SIZE];
        let mut iafs = vec![0u8; INT_CTX_SIZE];
        let mut iads = vec![0u8; INT_CTX_SIZE];
        let mut iaid = vec![0u8; 1usize << (code_len + 1)];

        encode_int(&mut enc, &mut iadt, Some(0)); // initial STRIPT
        encode_int(&mut enc, &mut iadt, Some(0)); // strip DT
        for (i, &(id, ds)) in placements.iter().enumerate() {
            if i == 0 {
                encode_int(&mut enc, &mut iafs, Some(ds)); // first S
            } else {
                encode_int(&mut enc, &mut iads, Some(ds)); // IDS
            }
            encode_iaid(&mut enc, &mut iaid, code_len, id);
        }
        encode_int(&mut enc, &mut iads, None); // OOB ends the strip
        p.extend_from_slice(&enc.finish());
        p
    }

    // -----------------------------------------------------------------------
    // MQ coder conformance and round trips.
    // -----------------------------------------------------------------------

    /// T.88 Annex H.2 arithmetic coder test sequence: 256 bits in …
    const H2_INPUT: [u8; 32] = [
        0x00, 0x02, 0x00, 0x51, 0x00, 0x00, 0x00, 0xC0, 0x03, 0x52, 0x87, 0x2A, 0xAA, 0xAA, 0xAA,
        0xAA, 0x82, 0xC0, 0x20, 0x00, 0xFC, 0xD7, 0x9E, 0xF6, 0xBF, 0x7F, 0xED, 0x90, 0x4F, 0x46,
        0xA3, 0xBF,
    ];
    /// … and the expected code stream out (single context, initial state 0/0).
    const H2_ENCODED: [u8; 30] = [
        0x84, 0xC7, 0x3B, 0xFC, 0xE1, 0xA1, 0x43, 0x04, 0x02, 0x20, 0x00, 0x00, 0x41, 0x0D, 0xBB,
        0x86, 0xF4, 0x31, 0x7F, 0xFF, 0x88, 0xFF, 0x37, 0x47, 0x1A, 0xDB, 0x6A, 0xDF, 0xFF, 0xAC,
    ];

    #[test]
    fn mq_decoder_t88_h2_vector() {
        let mut dec = MqDecoder::new(&H2_ENCODED);
        let mut cx = vec![0u8; 1];
        let mut out = vec![0u8; 32];
        for i in 0..256 {
            let bit = dec.decode(&mut cx, 0);
            out[i / 8] |= bit << (7 - i % 8);
        }
        assert_eq!(out, H2_INPUT);
    }

    #[test]
    fn mq_encoder_t88_h2_vector() {
        let mut enc = MqEncoder::new();
        let mut cx = vec![0u8; 1];
        for i in 0..256 {
            enc.encode(&mut cx, 0, (H2_INPUT[i / 8] >> (7 - i % 8)) & 1);
        }
        assert_eq!(
            format!("{:02X?}", enc.finish()),
            format!("{:02X?}", H2_ENCODED)
        );
    }

    #[test]
    fn mq_round_trip_random_bits() {
        // Deterministic pseudo-random bits across several contexts.
        let mut state = 0x2545_F491u64;
        let bits: Vec<u8> = (0..4096)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                ((state >> 41) & 1) as u8
            })
            .collect();
        let mut enc = MqEncoder::new();
        let mut cx = vec![0u8; 5];
        for (i, &b) in bits.iter().enumerate() {
            enc.encode(&mut cx, i % 5, b);
        }
        let data = enc.finish();
        let mut dec = MqDecoder::new(&data);
        let mut cx = vec![0u8; 5];
        for (i, &b) in bits.iter().enumerate() {
            assert_eq!(dec.decode(&mut cx, i % 5), b, "bit {i}");
        }
    }

    #[test]
    fn integer_decoding_round_trip_all_buckets() {
        let values: Vec<Option<i64>> = vec![
            Some(0),
            Some(1),
            Some(3),
            Some(4),
            Some(19),
            Some(20),
            Some(83),
            Some(84),
            Some(339),
            Some(340),
            Some(4435),
            Some(4436),
            Some(100_000),
            Some(1 << 30),
            Some(-1),
            Some(-4),
            Some(-20),
            Some(-84),
            Some(-340),
            Some(-4436),
            Some(-99_999),
            None,
            Some(7),
            None,
            Some(-2),
        ];
        let mut enc = MqEncoder::new();
        let mut cx = vec![0u8; INT_CTX_SIZE];
        for v in &values {
            encode_int(&mut enc, &mut cx, *v);
        }
        let data = enc.finish();
        let mut dec = MqDecoder::new(&data);
        let mut cx = vec![0u8; INT_CTX_SIZE];
        for v in &values {
            assert_eq!(decode_int(&mut dec, &mut cx), *v);
        }
    }

    #[test]
    fn iaid_round_trip() {
        let ids = [0usize, 1, 2, 5, 7, 6, 3, 4];
        let code_len = 3;
        let mut enc = MqEncoder::new();
        let mut cx = vec![0u8; 1 << (code_len + 1)];
        for &id in &ids {
            encode_iaid(&mut enc, &mut cx, code_len as u32, id);
        }
        let data = enc.finish();
        let mut dec = MqDecoder::new(&data);
        let mut cx = vec![0u8; 1 << (code_len + 1)];
        for &id in &ids {
            assert_eq!(decode_iaid(&mut dec, &mut cx, code_len as u32), id);
        }
    }

    // -----------------------------------------------------------------------
    // Generic region decoding.
    // -----------------------------------------------------------------------

    fn test_pattern() -> Bitmap {
        bitmap_from(&[
            "................",
            ".####....####...",
            ".#...#..#....#..",
            ".####...#....#..",
            ".#...#..#....#..",
            ".####....####...",
            "................",
            "#.#.#.#.#.#.#.#.",
        ])
    }

    #[test]
    fn generic_region_round_trip_all_templates() {
        let bm = test_pattern();
        for template in 0..4u8 {
            let p = GenericParams {
                template,
                tpgdon: false,
                at: nominal_at(template),
            };
            let mut enc = MqEncoder::new();
            let mut cx = vec![0u8; 1 << 16];
            encode_generic(&mut enc, &mut cx, &bm, &p);
            let data = enc.finish();
            let mut dec = MqDecoder::new(&data);
            let mut cx = vec![0u8; 1 << 16];
            let out = decode_generic(&mut dec, &mut cx, bm.width, bm.height, &p).unwrap();
            assert_eq!(out.data, bm.data, "template {template}");
        }
    }

    #[test]
    fn generic_region_round_trip_tpgdon() {
        // Repeated rows exercise the typical-prediction (LTP) path.
        let bm = bitmap_from(&[
            "........", "..####..", "..####..", "..####..", "........", "........",
        ]);
        for template in 0..4u8 {
            let p = GenericParams {
                template,
                tpgdon: true,
                at: nominal_at(template),
            };
            let mut enc = MqEncoder::new();
            let mut cx = vec![0u8; 1 << 16];
            encode_generic(&mut enc, &mut cx, &bm, &p);
            let data = enc.finish();
            let mut dec = MqDecoder::new(&data);
            let mut cx = vec![0u8; 1 << 16];
            let out = decode_generic(&mut dec, &mut cx, bm.width, bm.height, &p).unwrap();
            assert_eq!(out.data, bm.data, "template {template}");
        }
    }

    #[test]
    fn generic_region_round_trip_custom_at() {
        // Non-nominal (but valid: above/left) AT pixels.
        let bm = test_pattern();
        let p = GenericParams {
            template: 0,
            tpgdon: false,
            at: [(-1, -1), (1, -2), (-4, 0), (1, -1)],
        };
        let mut enc = MqEncoder::new();
        let mut cx = vec![0u8; 1 << 16];
        encode_generic(&mut enc, &mut cx, &bm, &p);
        let data = enc.finish();
        let mut dec = MqDecoder::new(&data);
        let mut cx = vec![0u8; 1 << 16];
        let out = decode_generic(&mut dec, &mut cx, bm.width, bm.height, &p).unwrap();
        assert_eq!(out.data, bm.data);
    }

    // -----------------------------------------------------------------------
    // Segment header parsing.
    // -----------------------------------------------------------------------

    #[test]
    fn segment_header_short_form() {
        // Number 2, type 6, referred [1], 1-byte page association, length 7.
        let bytes = [0, 0, 0, 2, 0x06, 0x20, 0x01, 0x01, 0, 0, 0, 7];
        let h = parse_segment_header(&mut Reader::new(&bytes)).unwrap();
        assert_eq!(h.number, 2);
        assert_eq!(h.seg_type, 6);
        assert_eq!(h.referred, vec![1]);
        assert_eq!(h.page, 1);
        assert_eq!(h.data_length, 7);
    }

    #[test]
    fn segment_header_long_form_wide_numbers() {
        // Segment number 70000 (> 65536 → 4-byte referred numbers), 5 referred
        // segments (> 4 → long count form + 1 retain byte), 4-byte page assoc.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&70000u32.to_be_bytes());
        bytes.push(0x40); // type 0, 4-byte page association
        bytes.extend_from_slice(&0xE000_0005u32.to_be_bytes()); // long form, count 5
        bytes.push(0x00); // retain flags: ceil(6/8) = 1 byte
        for r in [1u32, 2, 3, 65537, 69999] {
            bytes.extend_from_slice(&r.to_be_bytes());
        }
        bytes.extend_from_slice(&9u32.to_be_bytes()); // page
        bytes.extend_from_slice(&42u32.to_be_bytes()); // data length
        let h = parse_segment_header(&mut Reader::new(&bytes)).unwrap();
        assert_eq!(h.number, 70000);
        assert_eq!(h.referred, vec![1, 2, 3, 65537, 69999]);
        assert_eq!(h.page, 9);
        assert_eq!(h.data_length, 42);
    }

    #[test]
    fn segment_header_rejects_bad_count_code() {
        // Count code 5 in the top 3 bits is reserved.
        let bytes = [0, 0, 0, 1, 0x30, 0xA0, 0x01, 0, 0, 0, 0];
        assert!(parse_segment_header(&mut Reader::new(&bytes)).is_err());
    }

    #[test]
    fn segment_header_truncated_is_error() {
        let bytes = [0, 0, 0, 1, 0x30];
        assert!(parse_segment_header(&mut Reader::new(&bytes)).is_err());
        // And via the full decode path (header shorter than the 11-byte
        // minimum is treated as trailing padding → "no page" error).
        let params = Jbig2Params { globals: None };
        assert!(decode(&bytes, &params).is_err());
    }

    #[test]
    fn referred_list_longer_than_data_is_error() {
        // Long form claiming 2^20 referred segments with only a few bytes left.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1u32.to_be_bytes());
        bytes.push(0x00);
        bytes.extend_from_slice(&(0xE000_0000u32 | (1 << 20)).to_be_bytes());
        bytes.extend_from_slice(&[0u8; 64]);
        assert!(parse_segment_header(&mut Reader::new(&bytes)).is_err());
    }

    // -----------------------------------------------------------------------
    // End-to-end embedded streams.
    // -----------------------------------------------------------------------

    #[test]
    fn end_to_end_generic_region() {
        let art = [
            "................",
            ".####....####...",
            ".#...#..#....#..",
            ".####...#....#..",
            ".#...#..#....#..",
            ".####....####...",
            "................",
            "#.#.#.#.#.#.#.#.",
        ];
        let bm = bitmap_from(&art);
        for (template, tpgdon) in [(0u8, false), (0, true), (2, false), (3, true)] {
            let mut stream = segment(0, 48, &[], 1, &page_info_payload(16, 8, 0));
            stream.extend_from_slice(&segment(
                1,
                38, // immediate generic region
                &[],
                1,
                &generic_region_payload(&bm, template, tpgdon),
            ));
            let out = decode(&stream, &Jbig2Params { globals: None }).unwrap();
            assert_eq!(
                out,
                packed_from(&art),
                "template {template} tpgdon {tpgdon}"
            );
        }
    }

    #[test]
    fn end_to_end_mmr_generic_region() {
        // Two identical rows "WWWBBWWW" hand-coded in T.6: H(001) W3(1000)
        // B2(11) V0(1) for row 0, then V0 V0 V0 for row 1 → 0x31 0xF8.
        let mut payload = region_info(8, 2, 0, 0, 0);
        payload.push(0x01); // generic flags: MMR
        payload.extend_from_slice(&[0x31, 0xF8]);
        let mut stream = segment(0, 48, &[], 1, &page_info_payload(8, 2, 0));
        stream.extend_from_slice(&segment(1, 38, &[], 1, &payload));
        let out = decode(&stream, &Jbig2Params { globals: None }).unwrap();
        assert_eq!(out, vec![0xE7, 0xE7]); // WWWBBWWW, white = 1
    }

    #[test]
    fn end_to_end_symbol_dict_and_text_region() {
        let sym0 = bitmap_from(&["#.#", ".#.", "#.#"]);
        let sym1 = bitmap_from(&["..#", ".#.", "#.."]);
        // TOPLEFT placement on one strip: sym0 at S=0, then IDS=2 advances S
        // from 2 (= 0 + w-1) to 4 for sym1.
        let mut stream = segment(0, 48, &[], 1, &page_info_payload(10, 4, 0));
        stream.extend_from_slice(&segment(
            1,
            0,
            &[],
            1,
            &symbol_dict_payload(&[&sym0, &sym1]),
        ));
        stream.extend_from_slice(&segment(
            2,
            6, // immediate text region
            &[1],
            1,
            &text_region_payload(10, 4, 2, &[(0, 0), (1, 2)]),
        ));
        let out = decode(&stream, &Jbig2Params { globals: None }).unwrap();
        let expected = packed_from(&["#.#...#...", ".#...#....", "#.#.#.....", ".........."]);
        assert_eq!(out, expected);
    }

    #[test]
    fn globals_stream_supplies_dictionary() {
        // PDF embedding rule: page info + symbol dictionary live in the
        // /JBIG2Globals stream; the image stream holds only the text region.
        let sym = bitmap_from(&["##", "##"]);
        let mut globals = segment(0, 48, &[], 1, &page_info_payload(4, 2, 0));
        globals.extend_from_slice(&segment(1, 0, &[], 1, &symbol_dict_payload(&[&sym])));
        let stream = segment(2, 6, &[1], 1, &text_region_payload(4, 2, 1, &[(0, 1)]));
        let out = decode(
            &stream,
            &Jbig2Params {
                globals: Some(globals),
            },
        )
        .unwrap();
        assert_eq!(out, packed_from(&[".##.", ".##."]));
    }

    #[test]
    fn page_default_pixel_and_packing_polarity() {
        // A page with no regions: default pixel 0 = white = PDF sample 1.
        let stream = segment(0, 48, &[], 1, &page_info_payload(10, 3, 0));
        let out = decode(&stream, &Jbig2Params { globals: None }).unwrap();
        assert_eq!(out, vec![0xFF, 0xC0, 0xFF, 0xC0, 0xFF, 0xC0]);

        // Default pixel 1 (page flags bit 2) = black = PDF sample 0.
        let stream = segment(0, 48, &[], 1, &page_info_payload(10, 3, 0x04));
        let out = decode(&stream, &Jbig2Params { globals: None }).unwrap();
        assert_eq!(out, vec![0u8; 6]);
    }

    #[test]
    fn unsupported_segments_leave_page_blank() {
        // A Huffman symbol dictionary (flags bit 0) fails decode; the text
        // region depending on it is skipped with a warning, not an error.
        let mut dict_payload = vec![0x00, 0x01]; // SDHUFF = 1
        dict_payload.extend_from_slice(&[0u8; 16]);
        let mut stream = segment(0, 48, &[], 1, &page_info_payload(8, 2, 0));
        stream.extend_from_slice(&segment(1, 0, &[], 1, &dict_payload));
        stream.extend_from_slice(&segment(
            2,
            6,
            &[1],
            1,
            &text_region_payload(8, 2, 1, &[(0, 0)]),
        ));
        // A halftone region segment is skipped wholesale.
        stream.extend_from_slice(&segment(3, 22, &[], 1, &[0u8; 20]));
        let out = decode(&stream, &Jbig2Params { globals: None }).unwrap();
        assert_eq!(out, vec![0xFF, 0xFF]); // page stays white
    }

    #[test]
    fn missing_page_info_is_error_but_unknown_length_too() {
        // Empty stream: nothing decoded → no page.
        assert!(decode(&[], &Jbig2Params { globals: None }).is_err());
        // Unknown segment data length (0xFFFFFFFF) is unsupported.
        let mut stream = segment(0, 48, &[], 1, &page_info_payload(8, 2, 0));
        let mut bad = segment(1, 38, &[], 1, &[]);
        let n = bad.len();
        bad[n - 4..].copy_from_slice(&[0xFF; 4]);
        stream.extend_from_slice(&bad);
        assert!(decode(&stream, &Jbig2Params { globals: None }).is_err());
    }

    #[test]
    fn implausible_page_size_is_error() {
        let stream = segment(0, 48, &[], 1, &page_info_payload(u32::MAX - 1, 2, 0));
        assert!(decode(&stream, &Jbig2Params { globals: None }).is_err());
    }

    #[test]
    fn oversized_region_is_skipped_not_fatal() {
        // Region pixel-limit breach fails that region (blank), not the image.
        let mut payload = region_info(1 << 19, 1 << 19, 0, 0, 0);
        payload.push(0x00); // arithmetic, template 0
        payload.extend_from_slice(&[3, 0xFF, 0xFD, 0xFF, 2, 0xFE, 0xFE, 0xFE]); // AT
        let mut stream = segment(0, 48, &[], 1, &page_info_payload(8, 1, 0));
        stream.extend_from_slice(&segment(1, 38, &[], 1, &payload));
        let out = decode(&stream, &Jbig2Params { globals: None }).unwrap();
        assert_eq!(out, vec![0xFF]);
    }

    #[test]
    fn region_composition_clips_and_offsets() {
        // A 4x1 black bar composed at (6, 1) on an 8x3 page: clipped at x = 8.
        let bar = bitmap_from(&["####"]);
        let mut payload = region_info(4, 1, 6, 1, 0);
        payload.push(0x00);
        let at = nominal_at(0);
        for &(ax, ay) in at.iter() {
            payload.push(ax as u8);
            payload.push(ay as u8);
        }
        let mut enc = MqEncoder::new();
        let mut cx = vec![0u8; 1 << 16];
        encode_generic(
            &mut enc,
            &mut cx,
            &bar,
            &GenericParams {
                template: 0,
                tpgdon: false,
                at,
            },
        );
        payload.extend_from_slice(&enc.finish());
        let mut stream = segment(0, 48, &[], 1, &page_info_payload(8, 3, 0));
        stream.extend_from_slice(&segment(1, 38, &[], 1, &payload));
        let out = decode(&stream, &Jbig2Params { globals: None }).unwrap();
        assert_eq!(out, packed_from(&["........", "......##", "........"]));
    }

    // -----------------------------------------------------------------------
    // Generic refinement region (step 1).
    // -----------------------------------------------------------------------

    /// Nominal refinement AT pixels per GRTEMPLATE (T.88 6.3.5.3): A1 = (-1,-1)
    /// in the current bitmap, A2 = (-1,-1) in the reference.
    fn nominal_refine_at() -> [(i8, i8); 2] {
        [(-1, -1), (-1, -1)]
    }

    #[test]
    fn refinement_round_trip_both_templates() {
        // Reference is a coarse blob; the refined bitmap tweaks a few pixels.
        let refr = bitmap_from(&[
            "........", ".#####..", ".#####..", ".#####..", ".#####..", "........",
        ]);
        let target = bitmap_from(&[
            "..####..", ".#...#..", ".#...#..", ".#...#..", ".#####..", "...#....",
        ]);
        for template in 0..2u8 {
            let p = RefinementParams {
                template,
                tpgron: false,
                at: nominal_refine_at(),
                dx: 0,
                dy: 0,
            };
            let mut enc = MqEncoder::new();
            let mut cx = vec![0u8; 1 << 13];
            encode_refinement(&mut enc, &mut cx, &target, &refr, &p);
            let data = enc.finish();
            let mut dec = MqDecoder::new(&data);
            let mut cx = vec![0u8; 1 << 13];
            let out = decode_refinement(&mut dec, &mut cx, target.width, target.height, &refr, &p)
                .unwrap();
            assert_eq!(out.data, target.data, "template {template}");
        }
    }

    #[test]
    fn refinement_round_trip_with_offset() {
        // A non-zero reference offset (dx, dy) exercises the rx = x - dx path
        // shared by symbol-dict and text-region refinement.
        let refr = bitmap_from(&[
            "........", "..####..", "..#..#..", "..#..#..", "..####..", "........",
        ]);
        let target = bitmap_from(&[
            "........", "..####..", "..#.##..", "..#..#..", "..####..", "........",
        ]);
        for (dx, dy) in [(1, 0), (-1, 1), (0, -1)] {
            for template in 0..2u8 {
                let p = RefinementParams {
                    template,
                    tpgron: false,
                    at: nominal_refine_at(),
                    dx,
                    dy,
                };
                let mut enc = MqEncoder::new();
                let mut cx = vec![0u8; 1 << 13];
                encode_refinement(&mut enc, &mut cx, &target, &refr, &p);
                let data = enc.finish();
                let mut dec = MqDecoder::new(&data);
                let mut cx = vec![0u8; 1 << 13];
                let out =
                    decode_refinement(&mut dec, &mut cx, target.width, target.height, &refr, &p)
                        .unwrap();
                assert_eq!(
                    out.data, target.data,
                    "template {template} offset ({dx},{dy})"
                );
            }
        }
    }

    #[test]
    fn refinement_identity_reproduces_reference() {
        // Refining a bitmap against itself must reproduce it exactly.
        let refr = test_pattern();
        let p = RefinementParams {
            template: 1,
            tpgron: false,
            at: nominal_refine_at(),
            dx: 0,
            dy: 0,
        };
        let mut enc = MqEncoder::new();
        let mut cx = vec![0u8; 1 << 13];
        encode_refinement(&mut enc, &mut cx, &refr, &refr, &p);
        let data = enc.finish();
        let mut dec = MqDecoder::new(&data);
        let mut cx = vec![0u8; 1 << 13];
        let out = decode_refinement(&mut dec, &mut cx, refr.width, refr.height, &refr, &p).unwrap();
        assert_eq!(out.data, refr.data);
    }

    /// Immediate generic refinement region segment (type 42) refining the page.
    #[test]
    fn end_to_end_refinement_region_over_page() {
        // Page starts with a generic region (a 8x6 box); a refinement region
        // then refines that page area into a slightly different shape.
        let initial = bitmap_from(&[
            "........", ".#####..", ".#...#..", ".#...#..", ".#####..", "........",
        ]);
        let refined = bitmap_from(&[
            "..###...", ".#...#..", ".#...#..", ".#...#..", ".#####..", "...#....",
        ]);
        let mut stream = segment(0, 48, &[], 1, &page_info_payload(8, 6, 0));
        stream.extend_from_slice(&segment(
            1,
            38,
            &[],
            1,
            &generic_region_payload(&initial, 0, false),
        ));
        // Refinement region payload: region info, flags (template 1), then MQ.
        let mut payload = region_info(8, 6, 0, 0, 4); // op REPLACE on the page
        payload.push(0x01); // GRTEMPLATE 1, TPGRON off
        let p = RefinementParams {
            template: 1,
            tpgron: false,
            at: nominal_refine_at(),
            dx: 0,
            dy: 0,
        };
        let mut enc = MqEncoder::new();
        let mut cx = vec![0u8; 1 << 13];
        encode_refinement(&mut enc, &mut cx, &refined, &initial, &p);
        payload.extend_from_slice(&enc.finish());
        stream.extend_from_slice(&segment(2, 42, &[], 1, &payload));
        let out = decode(&stream, &Jbig2Params { globals: None }).unwrap();
        // The refinement region REPLACEs the page area with the refined art.
        assert_eq!(
            out,
            packed_from(&["..###...", ".#...#..", ".#...#..", ".#...#..", ".#####..", "...#....",])
        );
    }

    // -----------------------------------------------------------------------
    // Symbol dictionary refinement / aggregation (step 2).
    // -----------------------------------------------------------------------

    #[test]
    fn symbol_dict_refagg_refines_input_symbol() {
        // Build a refagg dictionary by hand so the refinement bytes line up
        // with the chosen reference. One input symbol; the new symbol is its
        // refinement.
        let input = bitmap_from(&["####", "#..#", "#..#", "####"]);
        let target = bitmap_from(&["####", "#..#", "#.##", "####"]);

        // flags: SDREFAGG=1, SDRTEMPLATE=1.
        let mut p = Vec::new();
        let flags: u16 = 0x0002 | (1 << 12);
        p.extend_from_slice(&flags.to_be_bytes());
        for &(ax, ay) in nominal_at(0).iter() {
            p.push(ax as u8);
            p.push(ay as u8);
        }
        p.extend_from_slice(&1u32.to_be_bytes()); // SDNUMEXSYMS
        p.extend_from_slice(&1u32.to_be_bytes()); // SDNUMNEWSYMS

        let code_len = ceil_log2(2).max(1); // input(1) + new(1) = 2 → 1 bit
        let mut enc = MqEncoder::new();
        let mut cx_gr = vec![0u8; 1 << 13];
        let mut iadh = vec![0u8; INT_CTX_SIZE];
        let mut iadw = vec![0u8; INT_CTX_SIZE];
        let mut iaex = vec![0u8; INT_CTX_SIZE];
        let mut iaai = vec![0u8; INT_CTX_SIZE];
        let mut iardx = vec![0u8; INT_CTX_SIZE];
        let mut iardy = vec![0u8; INT_CTX_SIZE];
        let mut iaid = vec![0u8; 1usize << (code_len + 1)];

        encode_int(&mut enc, &mut iadh, Some(target.height as i64));
        encode_int(&mut enc, &mut iadw, Some(target.width as i64));
        encode_int(&mut enc, &mut iaai, Some(1)); // one aggregate instance
        encode_iaid(&mut enc, &mut iaid, code_len, 0); // refine input symbol 0
        encode_int(&mut enc, &mut iardx, Some(0));
        encode_int(&mut enc, &mut iardy, Some(0));
        encode_refinement(
            &mut enc,
            &mut cx_gr,
            &target,
            &input,
            &RefinementParams {
                template: 1,
                tpgron: false,
                at: nominal_refine_at(),
                dx: 0,
                dy: 0,
            },
        );
        encode_int(&mut enc, &mut iadw, None); // OOB ends the height class
        encode_int(&mut enc, &mut iaex, Some(1)); // skip the input symbol
        encode_int(&mut enc, &mut iaex, Some(1)); // export the new symbol
        p.extend_from_slice(&enc.finish());

        // Compose: globals carry the input symbol dictionary; page stream has
        // the refagg dictionary (referring to the input) and a text region.
        let input_dict = symbol_dict_payload(&[&input]);
        let mut stream = segment(0, 48, &[], 1, &page_info_payload(8, 4, 0));
        stream.extend_from_slice(&segment(1, 0, &[], 1, &input_dict));
        stream.extend_from_slice(&segment(2, 0, &[1], 1, &p));
        stream.extend_from_slice(&segment(
            3,
            6,
            &[2],
            1,
            &text_region_payload(8, 4, 1, &[(0, 0)]),
        ));
        let out = decode(&stream, &Jbig2Params { globals: None }).unwrap();
        assert_eq!(
            out,
            packed_from(&["####....", "#..#....", "#.##....", "####...."])
        );
    }

    // -----------------------------------------------------------------------
    // Text region per-instance refinement (step 3).
    // -----------------------------------------------------------------------

    #[test]
    fn text_region_per_instance_refinement() {
        // One symbol placed once, refined in place into a different bitmap.
        let sym = bitmap_from(&["####", "#..#", "#..#", "####"]);
        let refined = bitmap_from(&["####", "##.#", "#..#", "####"]);

        let dict = symbol_dict_payload(&[&sym]);

        // Text region with SBREFINE = 1, SBRTEMPLATE = 1.
        let mut p = region_info(4, 4, 0, 0, 0);
        let flags: u16 = (1 << 4) /* TOPLEFT */ | 0x0002 /* SBREFINE */ | (1 << 15) /* SBRTEMPLATE */;
        p.extend_from_slice(&flags.to_be_bytes());
        // SBRTEMPLATE = 1 → no refinement AT pixels in the header.
        p.extend_from_slice(&1u32.to_be_bytes()); // SBNUMINSTANCES

        let code_len = ceil_log2(1).max(1);
        let mut enc = MqEncoder::new();
        let mut iadt = vec![0u8; INT_CTX_SIZE];
        let mut iafs = vec![0u8; INT_CTX_SIZE];
        let mut iads = vec![0u8; INT_CTX_SIZE];
        let mut iari = vec![0u8; INT_CTX_SIZE];
        let mut iardw = vec![0u8; INT_CTX_SIZE];
        let mut iardh = vec![0u8; INT_CTX_SIZE];
        let mut iardx = vec![0u8; INT_CTX_SIZE];
        let mut iardy = vec![0u8; INT_CTX_SIZE];
        let mut iaid = vec![0u8; 1usize << (code_len + 1)];
        let mut cx_gr = vec![0u8; 1 << 13];

        encode_int(&mut enc, &mut iadt, Some(0)); // STRIPT
        encode_int(&mut enc, &mut iadt, Some(0)); // strip DT
        encode_int(&mut enc, &mut iafs, Some(0)); // first S
        encode_iaid(&mut enc, &mut iaid, code_len, 0);
        encode_int(&mut enc, &mut iari, Some(1)); // RI != 0 → refine
        encode_int(&mut enc, &mut iardw, Some(0)); // same width
        encode_int(&mut enc, &mut iardh, Some(0)); // same height
        encode_int(&mut enc, &mut iardx, Some(0));
        encode_int(&mut enc, &mut iardy, Some(0));
        // dx = floor(0/2)+0 = 0, dy = 0.
        encode_refinement(
            &mut enc,
            &mut cx_gr,
            &refined,
            &sym,
            &RefinementParams {
                template: 1,
                tpgron: false,
                at: nominal_refine_at(),
                dx: 0,
                dy: 0,
            },
        );
        encode_int(&mut enc, &mut iads, None); // OOB ends the strip
        p.extend_from_slice(&enc.finish());

        let mut stream = segment(0, 48, &[], 1, &page_info_payload(4, 4, 0));
        stream.extend_from_slice(&segment(1, 0, &[], 1, &dict));
        stream.extend_from_slice(&segment(2, 6, &[1], 1, &p));
        let out = decode(&stream, &Jbig2Params { globals: None }).unwrap();
        assert_eq!(out, packed_from(&["####", "##.#", "#..#", "####"]));
    }

    // -----------------------------------------------------------------------
    // Huffman decoding (step 4).
    // -----------------------------------------------------------------------

    #[test]
    fn standard_huff_tables_round_trip() {
        // Encode and decode representative values through every standard table.
        for n in 1..=15usize {
            let tab = standard_huff_table(n);
            // Pick values inside the table's coverage from each line's low.
            let samples: Vec<Option<i64>> = tab
                .lines
                .iter()
                .filter(|l| l.prefix_len > 0)
                .map(|l| {
                    if l.is_oob {
                        None
                    } else {
                        Some(l.range_low as i64)
                    }
                })
                .collect();
            let mut bw = BitWriter::new();
            for v in &samples {
                huff_encode(&mut bw, &tab, *v);
            }
            let data = bw.finish();
            let mut br = BitReader::new(&data);
            for v in &samples {
                assert_eq!(tab.decode(&mut br), *v, "table B.{n} value {v:?}");
            }
        }
    }

    #[test]
    fn custom_huff_table_round_trip() {
        // Build a type-53 custom table segment, parse it, and round-trip values.
        // Flags: OOB present, prefix size 4 bits, range size 3 bits.
        let oob = 1u8;
        let prefix_size = 4u8;
        let range_size = 3u8;
        let flags = oob | ((prefix_size - 1) << 1) | ((range_size - 1) << 4);
        let low = 0i32;
        let high = 8i32;
        let mut seg = Vec::new();
        seg.push(flags);
        seg.extend_from_slice(&(low as u32).to_be_bytes());
        seg.extend_from_slice(&(high as u32).to_be_bytes());
        // Code lengths via a bit writer: lines for cur=0..high in steps of
        // 2^range_len. With range_len = 1, values 0..8 → 8 normal lines.
        let mut bw = BitWriter::new();
        // 8 normal lines, each prefix_len=4, range_len=1 (covers 2 values).
        // cur advances by 2 each line → 4 lines reach high=8.
        for _ in 0..4 {
            bw.write_bits(4, prefix_size as u32); // prefix length 4
            bw.write_bits(1, range_size as u32); // range length 1
        }
        bw.write_bits(4, prefix_size as u32); // lower-range prefix length
        bw.write_bits(4, prefix_size as u32); // upper-range prefix length
        bw.write_bits(4, prefix_size as u32); // OOB prefix length
        seg.extend_from_slice(&bw.finish());

        let tab = parse_custom_huff_table(&seg).unwrap();
        let samples = [Some(0i64), Some(1), Some(4), Some(7), None];
        let mut wb = BitWriter::new();
        for v in &samples {
            huff_encode(&mut wb, &tab, *v);
        }
        let data = wb.finish();
        let mut br = BitReader::new(&data);
        for v in &samples {
            assert_eq!(tab.decode(&mut br), *v, "custom table value {v:?}");
        }
    }

    /// Huffman symbol-dictionary payload with a hand-coded MMR collective
    /// bitmap. The height class holds the supplied symbols (equal height); the
    /// caller provides the matching MMR bytes for their concatenation.
    fn huff_symbol_dict_payload(widths: &[usize], height: usize, mmr: &[u8]) -> Vec<u8> {
        // flags: SDHUFF=1; DH=B.4(0), DW=B.2(0), BMSIZE=B.1(0), AGG=0.
        let flags: u16 = 0x0001;
        let mut p = Vec::new();
        p.extend_from_slice(&flags.to_be_bytes());
        p.extend_from_slice(&(widths.len() as u32).to_be_bytes()); // SDNUMEXSYMS
        p.extend_from_slice(&(widths.len() as u32).to_be_bytes()); // SDNUMNEWSYMS

        let dh_tab = standard_huff_table(4);
        let dw_tab = standard_huff_table(2);
        let bm_tab = standard_huff_table(1);

        let mut bw = BitWriter::new();
        huff_encode(&mut bw, &dh_tab, Some(height as i64)); // HCHEIGHT delta from 0
        let mut prev_w = 0i64;
        for &w in widths {
            huff_encode(&mut bw, &dw_tab, Some(w as i64 - prev_w));
            prev_w = w as i64;
        }
        huff_encode(&mut bw, &dw_tab, None); // OOB ends the height class
                                             // BMSIZE: use the explicit byte count so the decoder knows the span.
        huff_encode(&mut bw, &bm_tab, Some(mmr.len() as i64));
        bw.byte_align();
        p.extend_from_slice(&bw.finish());
        p.extend_from_slice(mmr);

        // Export flags (table B.1) on the same bit stream, byte-aligned after
        // the collective bitmap: skip 0, export all.
        let mut bw2 = BitWriter::new();
        huff_encode(&mut bw2, &bm_tab, Some(0));
        huff_encode(&mut bw2, &bm_tab, Some(widths.len() as i64));
        p.extend_from_slice(&bw2.finish());
        p
    }

    #[test]
    fn huff_symbol_dict_and_text_region() {
        // Reuse the known T.6 fixture: two rows "WWWBBWWW" → 0x31 0xF8. As a
        // collective bitmap that is a single 8x2 symbol with black at cols 3-4.
        // We export it and place it via an arithmetic text region.
        let mmr = [0x31u8, 0xF8];
        let dict = huff_symbol_dict_payload(&[8], 2, &mmr);
        let mut stream = segment(0, 48, &[], 1, &page_info_payload(8, 2, 0));
        stream.extend_from_slice(&segment(1, 0, &[], 1, &dict));
        stream.extend_from_slice(&segment(
            2,
            6,
            &[1],
            1,
            &text_region_payload(8, 2, 1, &[(0, 0)]),
        ));
        let out = decode(&stream, &Jbig2Params { globals: None }).unwrap();
        // The symbol is WWWBBWWW on both rows (black at cols 3-4).
        assert_eq!(out, packed_from(&["...##...", "...##..."]));
    }

    #[test]
    fn pattern_dict_and_halftone_round_trip() {
        // 2x2 patterns, 4 of them (HDMAX=3 → 2 bitplanes). Build a pattern
        // dictionary and a halftone region selecting patterns by gray value.
        let p0 = bitmap_from(&["..", ".."]); // gray 0 (white)
        let p1 = bitmap_from(&["#.", ".."]); // gray 1
        let p2 = bitmap_from(&["#.", ".#"]); // gray 2
        let p3 = bitmap_from(&["##", "##"]); // gray 3 (black)
        let patterns = [&p0, &p1, &p2, &p3];

        // Pattern dictionary payload (arithmetic, template 0).
        let hdpw = 2u8;
        let hdph = 2u8;
        let hdmax = 3u32;
        let mut pd = Vec::new();
        pd.push(0x00); // flags: MMR=0, template 0
        pd.push(hdpw);
        pd.push(hdph);
        pd.extend_from_slice(&hdmax.to_be_bytes());
        // Collective bitmap: patterns side by side (8x2).
        let mut collective = Bitmap::new((hdpw as usize) * 4, hdph as usize, 0).unwrap();
        for (i, pat) in patterns.iter().enumerate() {
            for yy in 0..hdph as usize {
                for xx in 0..hdpw as usize {
                    if pat.get(xx as i64, yy as i64) != 0 {
                        collective.set(i * hdpw as usize + xx, yy, 1);
                    }
                }
            }
        }
        let at = [(-(hdpw as i64) as i8, 0i8), (-3, -1), (2, -2), (-2, -2)];
        let mut enc = MqEncoder::new();
        let mut cx = vec![0u8; 1 << 16];
        encode_generic(
            &mut enc,
            &mut cx,
            &collective,
            &GenericParams {
                template: 0,
                tpgdon: false,
                at,
            },
        );
        pd.extend_from_slice(&enc.finish());

        // Halftone region: 2x2 grid, HRX=HRY*256 so each cell is at (n*2, m*2).
        // Gray values: top-left=1, top-right=2, bottom-left=0, bottom-right=3.
        let gray = [[1u32, 2], [0, 3]];
        let hgw = 2usize;
        let hgh = 2usize;
        let mut ht = region_info(4, 4, 0, 0, 0);
        ht.push(0x00); // flags: MMR=0, template 0, no skip, OR, def_pixel 0
        ht.extend_from_slice(&(hgw as u32).to_be_bytes());
        ht.extend_from_slice(&(hgh as u32).to_be_bytes());
        ht.extend_from_slice(&0u32.to_be_bytes()); // HGX
        ht.extend_from_slice(&0u32.to_be_bytes()); // HGY
        ht.extend_from_slice(&((2u16) << 8).to_be_bytes()); // HRX = 2.0 (fixed 8.8)
        ht.extend_from_slice(&0u16.to_be_bytes()); // HRY = 0
                                                   // Encode the grayscale image as 2 Gray-coded bitplanes (MSB first).
        let hbpp = 2usize;
        let mut enc2 = MqEncoder::new();
        let mut cx2 = vec![0u8; 1 << 16];
        let at2 = [(3i8, -1i8), (-3, -1), (2, -2), (-2, -2)];
        // Build planes from gray via Gray code: g = v ^ (v>>1).
        for j in (0..hbpp).rev() {
            let mut plane = Bitmap::new(hgw, hgh, 0).unwrap();
            for (m, row) in gray.iter().enumerate() {
                for (n, &v) in row.iter().enumerate() {
                    let g = v ^ (v >> 1);
                    if (g >> j) & 1 == 1 {
                        plane.set(n, m, 1);
                    }
                }
            }
            encode_generic(
                &mut enc2,
                &mut cx2,
                &plane,
                &GenericParams {
                    template: 0,
                    tpgdon: false,
                    at: at2,
                },
            );
        }
        ht.extend_from_slice(&enc2.finish());

        let mut stream = segment(0, 48, &[], 1, &page_info_payload(4, 4, 0));
        stream.extend_from_slice(&segment(1, 16, &[], 1, &pd)); // pattern dict
        stream.extend_from_slice(&segment(2, 22, &[1], 1, &ht)); // halftone region
        let out = decode(&stream, &Jbig2Params { globals: None }).unwrap();
        // Expected page: cell (m,n) draws pattern[gray[m][n]] at (n*2, m*2).
        // gray = [[1,2],[0,3]] → patterns p1,p2 (top), p0,p3 (bottom).
        let expected = packed_from(&[
            "#.#.", // p1 row0 (#.) | p2 row0 (#.)
            "...#", // p1 row1 (..) | p2 row1 (.#)
            "..##", // p0 row0 (..) | p3 row0 (##)
            "..##", // p0 row1 (..) | p3 row1 (##)
        ]);
        assert_eq!(out, expected);
    }
}
