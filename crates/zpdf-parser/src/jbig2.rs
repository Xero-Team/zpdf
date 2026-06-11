//! JBIG2Decode — ITU-T T.88 bi-level image decoding (embedded PDF profile).
//!
//! Pure Rust, zero deps. Implements the arithmetic-coded subset that covers
//! real-world PDF usage: segment headers, the MQ arithmetic decoder (Annex E),
//! generic regions (GB templates 0–3, with/without TPGDON), symbol
//! dictionaries and text regions (refinement and aggregation off), page
//! assembly, and the PDF embedding rules (optional `/JBIG2Globals` stream
//! processed first). MMR-coded generic regions reuse the sibling CCITT Group 4
//! decoder. Unsupported region flavours (halftone, refinement, Huffman-coded)
//! are skipped with a warning — the region renders blank rather than failing
//! the whole image; corrupt mandatory structures (segment headers, page info)
//! are hard errors.
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

#[derive(Default)]
struct Decoder {
    page: Option<Bitmap>,
    /// Page height was declared 0xFFFFFFFF (striped): grow rows on demand.
    page_auto_height: bool,
    page_default_pixel: u8,
    /// Exported symbols per symbol-dictionary segment number; `None` records
    /// a dictionary that failed to decode (dependent regions render blank).
    symbol_dicts: HashMap<u32, Option<Vec<Rc<Bitmap>>>>,
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
            // Immediate (lossless) text region.
            6 | 7 => match self.decode_text_region(h, data) {
                Ok((info, bm)) => self.compose_to_page(&info, &bm)?,
                Err(e) => warn!(
                    "JBIG2Decode: text region segment {} failed ({e}); region left blank",
                    h.number
                ),
            },
            // Immediate (lossless) generic region.
            38 | 39 => match self.decode_generic_region_segment(data) {
                Ok((info, bm)) => self.compose_to_page(&info, &bm)?,
                Err(e) => warn!(
                    "JBIG2Decode: generic region segment {} failed ({e}); region left blank",
                    h.number
                ),
            },
            48 => self.process_page_info(data)?,
            50 => self.process_end_of_stripe(data)?,
            // End of page / end of file / profiles / extensions: no action.
            49 | 51 | 52 | 62 => {}
            // Intermediate regions and refinement regions are not supported.
            4 | 36 | 40..=43 => warn!(
                "JBIG2Decode: unsupported region segment type {} ignored (region blank)",
                h.seg_type
            ),
            // Pattern dictionaries and halftone regions are not supported.
            16 | 20 | 22 | 23 => warn!(
                "JBIG2Decode: halftone segment type {} ignored (region blank)",
                h.seg_type
            ),
            // Custom Huffman tables only matter to (unsupported) Huffman coding.
            53 => debug!("JBIG2Decode: Huffman table segment ignored"),
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

    /// Symbol dictionary segment (T.88 6.5 / 7.4.3), arithmetic coding only.
    fn decode_symbol_dict(&self, h: &SegmentHeader, data: &[u8]) -> Result<Vec<Rc<Bitmap>>> {
        let mut r = Reader::new(data);
        let flags = r.u16()?;
        let sd_huff = flags & 1 != 0;
        let sd_refagg = flags & 2 != 0;
        let ctx_used = flags & 0x100 != 0;
        let template = ((flags >> 10) & 3) as u8;
        if sd_huff {
            return Err(err("Huffman-coded symbol dictionary is unsupported"));
        }
        if ctx_used {
            return Err(err("imported bitmap coding contexts are unsupported"));
        }

        let mut at = [(0i8, 0i8); 4];
        let n_at = if template == 0 { 4 } else { 1 };
        for slot in at.iter_mut().take(n_at) {
            *slot = (r.i8()?, r.i8()?);
        }
        if sd_refagg {
            return Err(err("refinement/aggregate symbol coding is unsupported"));
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

        let mut mq = MqDecoder::new(r.rest());
        let mut cx_gb = vec![0u8; 1 << 16];
        let mut iadh = vec![0u8; INT_CTX_SIZE];
        let mut iadw = vec![0u8; INT_CTX_SIZE];
        let mut iaex = vec![0u8; INT_CTX_SIZE];
        let gp = GenericParams {
            template,
            tpgdon: false,
            at,
        };

        // Height-class loop (T.88 6.5.5): IADH grows the class height, IADW
        // deltas walk symbol widths until OOB ends the class.
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
            loop {
                let Some(dw) = decode_int(&mut mq, &mut iadw) else {
                    break; // OOB: height class complete
                };
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
                let bm = decode_generic(
                    &mut mq,
                    &mut cx_gb,
                    sym_width as usize,
                    hc_height as usize,
                    &gp,
                )?;
                new_syms.push(Rc::new(bm));
            }
        }

        // Export flags (T.88 6.5.10): alternating skip/export run lengths over
        // the concatenation of input and new symbols.
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

    /// Text region segment (T.88 6.4 / 7.4.4), arithmetic coding only.
    fn decode_text_region(&self, h: &SegmentHeader, data: &[u8]) -> Result<(RegionInfo, Bitmap)> {
        let mut r = Reader::new(data);
        let info = parse_region_info(&mut r)?;
        let flags = r.u16()?;
        let sb_huff = flags & 1 != 0;
        let sb_refine = flags & 2 != 0;
        let strips = 1i64 << ((flags >> 2) & 3);
        let ref_corner = ((flags >> 4) & 3) as u8; // 0=BL 1=TL 2=BR 3=TR
        let transposed = flags & 0x40 != 0;
        let comb_op = ((flags >> 7) & 3) as u8;
        let def_pixel = ((flags >> 9) & 1) as u8;
        let mut ds_offset = ((flags >> 10) & 0x1F) as i64; // 5-bit signed
        if ds_offset > 15 {
            ds_offset -= 32;
        }
        let rtemplate = (flags >> 15) & 1;

        if sb_huff {
            return Err(err("Huffman-coded text region is unsupported"));
        }
        if sb_refine && rtemplate == 0 {
            r.take(4)?; // refinement AT pixels (refinement itself unsupported)
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

        let mut mq = MqDecoder::new(r.rest());
        let mut iadt = vec![0u8; INT_CTX_SIZE];
        let mut iafs = vec![0u8; INT_CTX_SIZE];
        let mut iads = vec![0u8; INT_CTX_SIZE];
        let mut iait = vec![0u8; INT_CTX_SIZE];
        let mut iari = vec![0u8; INT_CTX_SIZE];
        let mut iaid = vec![0u8; 1usize << (sym_code_len + 1)];

        let mut bm = Bitmap::new(info.width, info.height, def_pixel)?;

        // T.88 6.4.5: STRIPT starts at minus the first IADT value (in strips).
        let mut stript =
            -decode_int(&mut mq, &mut iadt).ok_or_else(|| err("unexpected OOB in IADT"))? * strips;
        let mut firsts: i64 = 0;
        let mut inst = 0usize;

        'instances: while inst < num_instances {
            let dt = decode_int(&mut mq, &mut iadt).ok_or_else(|| err("unexpected OOB in IADT"))?;
            stript += dt * strips;
            let dfs =
                decode_int(&mut mq, &mut iafs).ok_or_else(|| err("unexpected OOB in IAFS"))?;
            firsts += dfs;
            let mut curs = firsts;
            let mut first = true;
            loop {
                if !first {
                    // IADS: OOB terminates the strip.
                    let Some(ids) = decode_int(&mut mq, &mut iads) else {
                        break;
                    };
                    curs += ids + ds_offset;
                }
                first = false;
                if inst >= num_instances {
                    break 'instances;
                }

                let curt = if strips == 1 {
                    0
                } else {
                    decode_int(&mut mq, &mut iait).ok_or_else(|| err("unexpected OOB in IAIT"))?
                };
                let t = stript + curt;
                let id = decode_iaid(&mut mq, &mut iaid, sym_code_len);
                if sb_refine {
                    let ri = decode_int(&mut mq, &mut iari)
                        .ok_or_else(|| err("unexpected OOB in IARI"))?;
                    if ri != 0 {
                        return Err(err("text region symbol refinement is unsupported"));
                    }
                }

                let Some(sym) = symbols.get(id) else {
                    return Err(err(format!("symbol id {id} out of range")));
                };
                let (w, hh) = (sym.width as i64, sym.height as i64);
                if !transposed {
                    // S is x; TOP corners (1, 3) anchor at T, BOTTOM shift up.
                    let y0 = t - if ref_corner & 1 == 0 { hh - 1 } else { 0 };
                    draw_symbol(&mut bm, sym, curs, y0, comb_op);
                    curs += w - 1;
                } else {
                    // S is y; RIGHT corners (2, 3) shift left of T.
                    let x0 = t - if ref_corner & 2 != 0 { w - 1 } else { 0 };
                    draw_symbol(&mut bm, sym, x0, curs, comb_op);
                    curs += hh - 1;
                }
                inst += 1;
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
}
