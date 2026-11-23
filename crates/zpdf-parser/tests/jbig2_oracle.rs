//! Differential oracle tests comparing zpdf's pure-Rust JBIG2 decoder against
//! `hayro-jbig2` (dev-dependency only, `default-features = false`).
//!
//! Replacing zpdf's JBIG2 decoder with `hayro-jbig2` would give up
//! `ParseLimits` budgets and per-region failure isolation (#173). Keeping
//! `hayro-jbig2` as a test-only oracle verifies bit-for-bit decoding parity on
//! valid streams while independently verifying zpdf's resource limits and
//! fault-isolation behavior on adversarial and malformed inputs.
//!
//! Known tracked feature gaps from #153 (not yet supported by zpdf, verified
//! to fail cleanly without panicking below):
//! - Unknown-height (`0xFFFFFFFF` segment data length) generic regions.
//! - Extended 12-AT template (`EXTTEMPLATE`, T.88 Amd. 2).
//! - Symbol-dictionary `SDCTXUSED` / `SDCTXRETAINED` context retention flags.
//! - Skipping (rather than failing the region on) out-of-range symbol IDs.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use flate2::{write::ZlibEncoder, Compression};
use hayro_jbig2::{DecodeSettings, Decoder as HayroDecoder, Globals, Page};
use zpdf_core::{ObjectId, ParseLimits, PdfDict, PdfName, PdfObject, PdfString};
use zpdf_parser::filters::decode_stream_with_limits;
use zpdf_parser::PdfFile;

// ---------------------------------------------------------------------------
// Oracle bridge: polarity, row padding, and bit-for-bit comparison.
// ---------------------------------------------------------------------------

/// Decoded JBIG2 image in canonical form for differential comparison.
///
/// Both decoders are normalized to:
/// - `pixels`: `width * height` bytes, JBIG2 native polarity (`1` = black,
///   `0` = white), row-major.
/// - `packed_pdf`: `height * ceil(width / 8)` bytes, MSB-first PDF 1-bpc
///   sample polarity (`0` = black, `1` = white), with any unused trailing bits
///   past `width` in the final byte of each row set to `0`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OracleImage {
    width: usize,
    height: usize,
    pixels: Vec<u8>,
    packed_pdf: Vec<u8>,
}

/// Collector implementing `hayro_jbig2::Decoder` to capture exact dimensions
/// and JBIG2-native pixel values (`black == true` -> `1`, `false` -> `0`).
struct HayroCollector {
    width: usize,
    height: usize,
    cur_x: usize,
    cur_y: usize,
    pixels: Vec<u8>,
}

impl HayroCollector {
    fn new(width: u32, height: u32) -> Self {
        let width = width as usize;
        let height = height as usize;
        Self {
            width,
            height,
            cur_x: 0,
            cur_y: 0,
            pixels: Vec::with_capacity(width.saturating_mul(height)),
        }
    }

    fn into_oracle_image(self) -> OracleImage {
        assert_eq!(
            self.cur_y, self.height,
            "hayro-jbig2 emitted {} rows, expected {}",
            self.cur_y, self.height
        );
        assert_eq!(self.cur_x, 0, "hayro-jbig2 left partial trailing row");
        assert_eq!(self.pixels.len(), self.width * self.height);
        let packed_pdf = pack_native_to_pdf(&self.pixels, self.width, self.height);
        OracleImage {
            width: self.width,
            height: self.height,
            pixels: self.pixels,
            packed_pdf,
        }
    }
}

impl HayroDecoder for HayroCollector {
    fn push_pixel(&mut self, black: bool) {
        self.pixels.push(u8::from(black));
        self.cur_x += 1;
    }

    fn push_pixel_chunk(&mut self, black: bool, chunk_count: u32) {
        assert_eq!(
            self.cur_x % 8,
            0,
            "hayro-jbig2 emitted unaligned chunk at x={}",
            self.cur_x
        );
        let count = chunk_count as usize * 8;
        self.pixels
            .resize(self.pixels.len() + count, u8::from(black));
        self.cur_x += count;
    }

    fn next_line(&mut self) {
        assert_eq!(
            self.cur_x, self.width,
            "hayro-jbig2 row {} had {} pixels, expected {}",
            self.cur_y, self.cur_x, self.width
        );
        self.cur_x = 0;
        self.cur_y += 1;
    }
}

/// Pack JBIG2 native pixels (`1` = black, `0` = white) into zpdf's PDF 1-bpc
/// output layout (MSB-first, `0` = black, `1` = white, trailing padding = `0`).
fn pack_native_to_pdf(pixels: &[u8], width: usize, height: usize) -> Vec<u8> {
    let row_bytes = width.div_ceil(8);
    let mut out = vec![0u8; row_bytes * height];
    for y in 0..height {
        let row = &pixels[y * width..(y + 1) * width];
        let out_row = &mut out[y * row_bytes..(y + 1) * row_bytes];
        for (x, &px) in row.iter().enumerate() {
            if px == 0 {
                out_row[x / 8] |= 0x80 >> (x % 8);
            }
        }
    }
    out
}

/// Unpack zpdf's PDF 1-bpc packed rows (MSB-first, `0` = black, `1` = white)
/// into JBIG2 native pixels (`1` = black, `0` = white), asserting exact byte
/// length and zero-filled trailing padding bits on every row.
fn unpack_pdf_to_native(
    packed: &[u8],
    width: usize,
    height: usize,
    label: &str,
) -> Result<Vec<u8>, String> {
    let row_bytes = width.div_ceil(8);
    let expected_len = row_bytes * height;
    if packed.len() != expected_len {
        return Err(format!(
            "{label}: zpdf packed byte length {} != expected {} ({width}x{height}, stride {row_bytes})",
            packed.len(),
            expected_len
        ));
    }
    let tail_bits = (row_bytes * 8) - width;
    let tail_mask = if tail_bits == 0 {
        0u8
    } else {
        (1u8 << tail_bits) - 1
    };
    let mut pixels = vec![0u8; width * height];
    for y in 0..height {
        let row = &packed[y * row_bytes..(y + 1) * row_bytes];
        if tail_mask != 0 && (row[row_bytes - 1] & tail_mask) != 0 {
            return Err(format!(
                "{label}: row {y} has non-zero trailing padding bits: last byte {:#04x}, mask {:#04x}",
                row[row_bytes - 1],
                tail_mask
            ));
        }
        for x in 0..width {
            let bit = (row[x / 8] >> (7 - (x % 8))) & 1;
            // PDF 1-bpc: 0 = black -> JBIG2 native 1; 1 = white -> JBIG2 native 0.
            pixels[y * width + x] = 1 - bit;
        }
    }
    Ok(pixels)
}

fn decode_hayro(
    data: &[u8],
    globals: Option<&[u8]>,
    settings: DecodeSettings,
) -> Result<OracleImage, hayro_jbig2::DecodeError> {
    let parsed_globals = match globals {
        Some(g) => Some(Globals::new(g, &settings)?),
        None => None,
    };
    let page = Page::new_embedded(data, parsed_globals.as_ref(), &settings)?;
    let mut collector = HayroCollector::new(page.width(), page.height());
    page.decode(&mut collector)?;
    Ok(collector.into_oracle_image())
}

fn jbig2_stream_dict(globals: Option<&[u8]>) -> PdfDict {
    let mut dict = PdfDict::new();
    dict.insert(
        PdfName::new("Filter"),
        PdfObject::Name(PdfName::new("JBIG2Decode")),
    );
    if let Some(g) = globals {
        let mut parms = PdfDict::new();
        parms.insert(
            PdfName::new("JBIG2Globals"),
            PdfObject::String(PdfString(g.to_vec())),
        );
        dict.insert(PdfName::new("DecodeParms"), PdfObject::Dict(parms));
    }
    dict
}

fn decode_zpdf(
    data: &[u8],
    globals: Option<&[u8]>,
    limits: &ParseLimits,
) -> zpdf_core::Result<Vec<u8>> {
    let dict = jbig2_stream_dict(globals);
    decode_stream_with_limits(data, &dict, limits)
}

fn format_mismatch(label: &str, hayro: &OracleImage, zpdf_pixels: &[u8]) -> String {
    let mut first = None;
    let mut diff_count = 0usize;
    for y in 0..hayro.height {
        for x in 0..hayro.width {
            let idx = y * hayro.width + x;
            if hayro.pixels[idx] != zpdf_pixels[idx] {
                diff_count += 1;
                if first.is_none() {
                    first = Some((x, y, hayro.pixels[idx], zpdf_pixels[idx]));
                }
            }
        }
    }
    let (fx, fy, hp, zp) = first.expect("called on mismatch");
    let mut msg = format!(
        "{label}: {diff_count}/{} pixels differ ({}x{}); first at ({fx},{fy}): hayro={hp} zpdf={zp}",
        hayro.pixels.len(),
        hayro.width,
        hayro.height
    );
    if hayro.width <= 64 && hayro.height <= 32 {
        msg.push_str("\nhayro:\n");
        for y in 0..hayro.height {
            for x in 0..hayro.width {
                msg.push(if hayro.pixels[y * hayro.width + x] != 0 {
                    '#'
                } else {
                    '.'
                });
            }
            msg.push('\n');
        }
        msg.push_str("zpdf:\n");
        for y in 0..hayro.height {
            for x in 0..hayro.width {
                msg.push(if zpdf_pixels[y * hayro.width + x] != 0 {
                    '#'
                } else {
                    '.'
                });
            }
            msg.push('\n');
        }
    }
    msg
}

/// Assert that both `hayro-jbig2` (in strict mode) and `zpdf` decode
/// `(data, globals)` cleanly and agree on dimensions, every pixel, and every
/// packed byte.
fn assert_oracle_match_strict(label: &str, data: &[u8], globals: Option<&[u8]>) -> OracleImage {
    let hayro = decode_hayro(data, globals, DecodeSettings { strict: true })
        .unwrap_or_else(|e| panic!("{label}: hayro-jbig2 (strict) failed: {e:?}"));
    let zpdf_packed = decode_zpdf(data, globals, &ParseLimits::default())
        .unwrap_or_else(|e| panic!("{label}: zpdf failed: {e}"));
    let zpdf_pixels = unpack_pdf_to_native(&zpdf_packed, hayro.width, hayro.height, label)
        .unwrap_or_else(|e| panic!("{e}"));
    if hayro.pixels != zpdf_pixels {
        panic!("{}", format_mismatch(label, &hayro, &zpdf_pixels));
    }
    assert_eq!(
        zpdf_packed, hayro.packed_pdf,
        "{label}: packed PDF bytes differ despite identical pixels"
    );
    hayro
}

/// Like [`assert_oracle_match_strict`], using hayro's default non-strict mode
/// (e.g. for streams with trailing padding or non-strict T.6 EOF markers).
fn assert_oracle_match(label: &str, data: &[u8], globals: Option<&[u8]>) -> OracleImage {
    let hayro = decode_hayro(data, globals, DecodeSettings { strict: false })
        .unwrap_or_else(|e| panic!("{label}: hayro-jbig2 failed: {e:?}"));
    let zpdf_packed = decode_zpdf(data, globals, &ParseLimits::default())
        .unwrap_or_else(|e| panic!("{label}: zpdf failed: {e}"));
    let zpdf_pixels = unpack_pdf_to_native(&zpdf_packed, hayro.width, hayro.height, label)
        .unwrap_or_else(|e| panic!("{e}"));
    if hayro.pixels != zpdf_pixels {
        panic!("{}", format_mismatch(label, &hayro, &zpdf_pixels));
    }
    assert_eq!(
        zpdf_packed, hayro.packed_pdf,
        "{label}: packed PDF bytes differ despite identical pixels"
    );
    hayro
}

// ---------------------------------------------------------------------------
// Synthetic JBIG2 stream builders (T.88 Annex A, B, E, and Section 7).
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Bitmap {
    width: usize,
    height: usize,
    /// JBIG2 native polarity: 0 = white, 1 = black.
    data: Vec<u8>,
}

impl Bitmap {
    fn new(width: usize, height: usize, fill: u8) -> Self {
        Self {
            width,
            height,
            data: vec![fill; width * height],
        }
    }

    fn from_art(rows: &[&str]) -> Self {
        let height = rows.len();
        let width = rows[0].len();
        let mut bm = Self::new(width, height, 0);
        for (y, row) in rows.iter().enumerate() {
            assert_eq!(row.len(), width);
            for (x, ch) in row.chars().enumerate() {
                if ch == '#' {
                    bm.set(x, y, 1);
                }
            }
        }
        bm
    }

    fn get(&self, x: i64, y: i64) -> u8 {
        if x < 0 || y < 0 || x >= self.width as i64 || y >= self.height as i64 {
            0
        } else {
            self.data[y as usize * self.width + x as usize]
        }
    }

    fn set(&mut self, x: usize, y: usize, v: u8) {
        self.data[y * self.width + x] = v;
    }

    /// Pack into MSB-first raw JBIG2 bytes (`1` = black, `0` = white,
    /// zero-padded per row), as used by uncompressed `BMSIZE == 0` Huffman
    /// symbol dictionaries (T.88 6.5.9).
    fn pack_jbig2_rows(&self) -> Vec<u8> {
        let row_bytes = self.width.div_ceil(8);
        let mut out = vec![0u8; row_bytes * self.height];
        for y in 0..self.height {
            for x in 0..self.width {
                if self.get(x as i64, y as i64) != 0 {
                    out[y * row_bytes + x / 8] |= 0x80 >> (x % 8);
                }
            }
        }
        out
    }
}

/// (Qe, NMPS, NLPS, SWITCH) — ITU-T T.88 Table E.1.
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

const INT_CTX_SIZE: usize = 512;

struct MqEncoder {
    a: u32,
    c: u32,
    ct: i32,
    b: u32,
    first: bool,
    out: Vec<u8>,
}

impl MqEncoder {
    fn new() -> Self {
        Self {
            a: 0x8000,
            c: 0,
            ct: 12,
            b: 0,
            first: true,
            out: Vec::new(),
        }
    }

    fn push_b(&mut self) {
        if !self.first {
            self.out.push(self.b as u8);
        }
        self.first = false;
    }

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
            self.b += 1;
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

    fn encode(&mut self, cx: &mut [u8], idx: usize, d: u8) {
        let state = cx[idx];
        let i = (state >> 1) as usize;
        let mps = state & 1;
        let (qe16, nmps, nlps, switch) = QE_TABLE[i];
        let qe = qe16 as u32;
        if d == mps {
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

    fn finish(mut self) -> Vec<u8> {
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
        self.out.push(0xFF);
        self.out.push(0xAC);
        self.out
    }
}

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

fn encode_int(enc: &mut MqEncoder, cx: &mut [u8], value: Option<i64>) {
    let mut prev = 1usize;
    let (sign, mag) = match value {
        None => (1u32, 0i64),
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

fn encode_iaid(enc: &mut MqEncoder, cx: &mut [u8], code_len: u32, id: usize) {
    let mut prev = 1usize;
    for k in (0..code_len).rev() {
        let bit = ((id >> k) & 1) as u8;
        enc.encode(cx, prev, bit);
        prev = (prev << 1) | bit as usize;
    }
}

fn ceil_log2(n: usize) -> u32 {
    usize::BITS - n.saturating_sub(1).leading_zeros()
}

fn at_px(bm: &Bitmap, x: i64, y: i64, at: (i8, i8)) -> usize {
    bm.get(x + at.0 as i64, y + at.1 as i64) as usize
}

fn generic_context(template: u8, bm: &Bitmap, x: i64, y: i64, at: &[(i8, i8); 4]) -> usize {
    match template {
        0 => {
            (bm.get(x - 1, y) as usize)
                | (bm.get(x - 2, y) as usize) << 1
                | (bm.get(x - 3, y) as usize) << 2
                | (bm.get(x - 4, y) as usize) << 3
                | at_px(bm, x, y, at[0]) << 4
                | (bm.get(x + 2, y - 1) as usize) << 5
                | (bm.get(x + 1, y - 1) as usize) << 6
                | (bm.get(x, y - 1) as usize) << 7
                | (bm.get(x - 1, y - 1) as usize) << 8
                | (bm.get(x - 2, y - 1) as usize) << 9
                | at_px(bm, x, y, at[1]) << 10
                | at_px(bm, x, y, at[2]) << 11
                | (bm.get(x + 1, y - 2) as usize) << 12
                | (bm.get(x, y - 2) as usize) << 13
                | (bm.get(x - 1, y - 2) as usize) << 14
                | at_px(bm, x, y, at[3]) << 15
        }
        1 => {
            (bm.get(x - 1, y) as usize)
                | (bm.get(x - 2, y) as usize) << 1
                | (bm.get(x - 3, y) as usize) << 2
                | at_px(bm, x, y, at[0]) << 3
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
        2 => {
            (bm.get(x - 1, y) as usize)
                | (bm.get(x - 2, y) as usize) << 1
                | at_px(bm, x, y, at[0]) << 2
                | (bm.get(x + 1, y - 1) as usize) << 3
                | (bm.get(x, y - 1) as usize) << 4
                | (bm.get(x - 1, y - 1) as usize) << 5
                | (bm.get(x - 2, y - 1) as usize) << 6
                | (bm.get(x + 1, y - 2) as usize) << 7
                | (bm.get(x, y - 2) as usize) << 8
                | (bm.get(x - 1, y - 2) as usize) << 9
        }
        _ => {
            (bm.get(x - 1, y) as usize)
                | (bm.get(x - 2, y) as usize) << 1
                | (bm.get(x - 3, y) as usize) << 2
                | (bm.get(x - 4, y) as usize) << 3
                | at_px(bm, x, y, at[0]) << 4
                | (bm.get(x + 1, y - 1) as usize) << 5
                | (bm.get(x, y - 1) as usize) << 6
                | (bm.get(x - 1, y - 1) as usize) << 7
                | (bm.get(x - 2, y - 1) as usize) << 8
                | (bm.get(x - 3, y - 1) as usize) << 9
        }
    }
}

fn tpgd_context(template: u8) -> usize {
    match template {
        0 => 0x9B25,
        1 => 0x0795,
        2 => 0x00E5,
        _ => 0x0195,
    }
}

fn nominal_at(template: u8) -> [(i8, i8); 4] {
    match template {
        0 => [(3, -1), (-3, -1), (2, -2), (-2, -2)],
        1 => [(3, -1), (0, 0), (0, 0), (0, 0)],
        2 => [(2, -1), (0, 0), (0, 0), (0, 0)],
        _ => [(2, -1), (0, 0), (0, 0), (0, 0)],
    }
}

fn encode_generic(
    enc: &mut MqEncoder,
    cx: &mut [u8],
    bm: &Bitmap,
    template: u8,
    tpgdon: bool,
    at: &[(i8, i8); 4],
) {
    encode_generic_with_skip(enc, cx, bm, template, tpgdon, at, None);
}

fn encode_generic_with_skip(
    enc: &mut MqEncoder,
    cx: &mut [u8],
    bm: &Bitmap,
    template: u8,
    tpgdon: bool,
    at: &[(i8, i8); 4],
    skip: Option<&Bitmap>,
) {
    let tpgd_cx = tpgd_context(template);
    let mut ltp = false;
    for y in 0..bm.height {
        if tpgdon {
            let row = &bm.data[y * bm.width..(y + 1) * bm.width];
            let typical = if y == 0 {
                row.iter().all(|&px| px == 0)
            } else {
                row == &bm.data[(y - 1) * bm.width..y * bm.width]
            };
            enc.encode(cx, tpgd_cx, u8::from(typical != ltp));
            ltp = typical;
            if ltp {
                continue;
            }
        }
        for x in 0..bm.width {
            if skip.is_some_and(|s| s.get(x as i64, y as i64) != 0) {
                continue;
            }
            let ctx = generic_context(template, bm, x as i64, y as i64, at);
            enc.encode(cx, ctx, bm.get(x as i64, y as i64));
        }
    }
}

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
        ctx = (ctx << 1) | cur.get(x, y - 1) as usize;
        ctx = (ctx << 1) | cur.get(x + 1, y - 1) as usize;
        ctx = (ctx << 1) | cur.get(x - 1, y) as usize;
        ctx = (ctx << 1) | cur.get(x + at[0].0 as i64, y + at[0].1 as i64) as usize;
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
        ctx = (ctx << 1) | cur.get(x - 1, y - 1) as usize;
        ctx = (ctx << 1) | cur.get(x, y - 1) as usize;
        ctx = (ctx << 1) | cur.get(x + 1, y - 1) as usize;
        ctx = (ctx << 1) | cur.get(x - 1, y) as usize;
        ctx = (ctx << 1) | refr.get(rx, ry - 1) as usize;
        ctx = (ctx << 1) | refr.get(rx - 1, ry) as usize;
        ctx = (ctx << 1) | refr.get(rx, ry) as usize;
        ctx = (ctx << 1) | refr.get(rx + 1, ry) as usize;
        ctx = (ctx << 1) | refr.get(rx, ry + 1) as usize;
        ctx = (ctx << 1) | refr.get(rx + 1, ry + 1) as usize;
    }
    ctx
}

#[allow(clippy::too_many_arguments)]
fn encode_refinement(
    enc: &mut MqEncoder,
    cx: &mut [u8],
    bm: &Bitmap,
    refr: &Bitmap,
    template: u8,
    tpgron: bool,
    at: &[(i8, i8); 2],
    dx: i32,
    dy: i32,
) {
    let sltp_cx = if template == 0 { 0x0020 } else { 0x0008 };
    let mut ltp = false;
    for y in 0..bm.height as i64 {
        if tpgron {
            let mut row_predictable = true;
            for x in 0..bm.width as i64 {
                let rx = x - dx as i64;
                let ry = y - dy as i64;
                let mut sum = 0i32;
                for ddy in -1..=1 {
                    for ddx in -1..=1 {
                        sum += refr.get(rx + ddx, ry + ddy) as i32;
                    }
                }
                if (sum == 0 && bm.get(x, y) != 0) || (sum == 9 && bm.get(x, y) != 1) {
                    row_predictable = false;
                    break;
                }
            }
            enc.encode(cx, sltp_cx, u8::from(row_predictable != ltp));
            ltp = row_predictable;
        }
        for x in 0..bm.width as i64 {
            let rx = x - dx as i64;
            let ry = y - dy as i64;
            if ltp {
                let mut sum = 0i32;
                for ddy in -1..=1 {
                    for ddx in -1..=1 {
                        sum += refr.get(rx + ddx, ry + ddy) as i32;
                    }
                }
                if sum == 0 || sum == 9 {
                    continue;
                }
            }
            let ctx = refine_context(template, bm, refr, x, y, rx, ry, at);
            enc.encode(cx, ctx, bm.get(x, y));
        }
    }
}

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

    fn write_bytes(&mut self, bytes: &[u8]) {
        self.byte_align();
        self.out.extend_from_slice(bytes);
    }

    fn finish(mut self) -> Vec<u8> {
        self.byte_align();
        self.out
    }
}

#[derive(Clone, Copy)]
struct HuffLine {
    prefix_len: u8,
    range_len: u8,
    range_low: i32,
    is_lower: bool,
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

fn huff_encode(bw: &mut BitWriter, lines: &[HuffLine], value: Option<i64>) {
    let max_len = lines.iter().map(|l| l.prefix_len).max().unwrap_or(0) as usize;
    let mut len_count = vec![0u64; max_len + 1];
    for l in lines {
        len_count[l.prefix_len as usize] += 1;
    }
    len_count[0] = 0;
    let mut next = vec![0u64; max_len + 1];
    let mut curcode = 0u64;
    for len in 1..=max_len {
        next[len] = curcode;
        curcode = (curcode + len_count[len]) << 1;
    }
    let codes: Vec<u64> = lines
        .iter()
        .map(|l| {
            let len = l.prefix_len as usize;
            if len == 0 {
                0
            } else {
                let c = next[len];
                next[len] += 1;
                c
            }
        })
        .collect();

    for (i, l) in lines.iter().enumerate() {
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
            bw.write_bits(codes[i] as u32, l.prefix_len as u32);
            if l.is_oob {
                return;
            }
            let mag = if l.is_lower {
                l.range_low as i64 - value.unwrap()
            } else {
                value.unwrap() - l.range_low as i64
            };
            bw.write_bits(mag as u32, l.range_len as u32);
            return;
        }
    }
    panic!("no Huffman line matches {value:?}");
}

fn std_huff_lines(n: usize) -> Vec<HuffLine> {
    use HuffLine as L;
    match n {
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
        4 => vec![
            L::normal(1, 0, 1),
            L::normal(2, 0, 2),
            L::normal(3, 0, 3),
            L::normal(4, 3, 4),
            L::normal(5, 6, 12),
            L::upper(5, 32, 76),
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
        14 => vec![
            L::normal(3, 0, -2),
            L::normal(3, 0, -1),
            L::normal(1, 0, 0),
            L::normal(3, 0, 1),
            L::normal(3, 0, 2),
        ],
        _ => unreachable!("unused standard table {n}"),
    }
}

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

// Page information flags (T.88 7.4.8.5) and striping flag (T.88 7.4.8.6).
const PAGE_EVENTUALLY_LOSSLESS: u8 = 1 << 0;
const PAGE_MIGHT_CONTAIN_REFINEMENTS: u8 = 1 << 1;
const PAGE_DEFAULT_BLACK: u8 = 1 << 2;
const PAGE_REQUIRES_AUX_BUFFERS: u8 = 1 << 5;
const PAGE_COMB_OP_OVERRIDDEN: u8 = 1 << 6;
const PAGE_STRIPED: u16 = 1 << 15;

fn page_info_payload_striped(width: u32, height: u32, flags: u8, striping: u16) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&width.to_be_bytes());
    p.extend_from_slice(&height.to_be_bytes());
    p.extend_from_slice(&[0; 8]);
    p.push(flags);
    p.extend_from_slice(&striping.to_be_bytes());
    p
}

fn page_info_payload(width: u32, height: u32, flags: u8) -> Vec<u8> {
    page_info_payload_striped(width, height, flags, 0)
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

fn generic_region_payload_at(
    bm: &Bitmap,
    x: u32,
    y: u32,
    op: u8,
    template: u8,
    tpgdon: bool,
    at: [(i8, i8); 4],
) -> Vec<u8> {
    let mut p = region_info(bm.width as u32, bm.height as u32, x, y, op);
    p.push((u8::from(tpgdon) << 3) | (template << 1));
    let n_at = if template == 0 { 4 } else { 1 };
    for &(ax, ay) in at.iter().take(n_at) {
        p.push(ax as u8);
        p.push(ay as u8);
    }
    let mut enc = MqEncoder::new();
    let mut cx = vec![0u8; 1 << 16];
    encode_generic(&mut enc, &mut cx, bm, template, tpgdon, &at);
    p.extend_from_slice(&enc.finish());
    p
}

fn generic_region_payload(
    bm: &Bitmap,
    x: u32,
    y: u32,
    op: u8,
    template: u8,
    tpgdon: bool,
) -> Vec<u8> {
    generic_region_payload_at(bm, x, y, op, template, tpgdon, nominal_at(template))
}

fn symbol_dict_payload(symbols: &[&Bitmap], template: u8) -> Vec<u8> {
    let at = nominal_at(template);
    let mut p = Vec::new();
    let flags: u16 = (template as u16) << 10;
    p.extend_from_slice(&flags.to_be_bytes());
    let n_at = if template == 0 { 4 } else { 1 };
    for &(ax, ay) in at.iter().take(n_at) {
        p.push(ax as u8);
        p.push(ay as u8);
    }
    p.extend_from_slice(&(symbols.len() as u32).to_be_bytes());
    p.extend_from_slice(&(symbols.len() as u32).to_be_bytes());

    let mut enc = MqEncoder::new();
    let mut cx_gb = vec![0u8; 1 << 16];
    let mut iadh = vec![0u8; INT_CTX_SIZE];
    let mut iadw = vec![0u8; INT_CTX_SIZE];
    let mut iaex = vec![0u8; INT_CTX_SIZE];

    let mut prev_h = 0i64;
    let mut i = 0;
    while i < symbols.len() {
        let h = symbols[i].height as i64;
        assert!(h > prev_h, "height classes must be strictly increasing");
        encode_int(&mut enc, &mut iadh, Some(h - prev_h));
        prev_h = h;
        let mut prev_w = 0i64;
        while i < symbols.len() && symbols[i].height as i64 == h {
            let sym = symbols[i];
            encode_int(&mut enc, &mut iadw, Some(sym.width as i64 - prev_w));
            prev_w = sym.width as i64;
            encode_generic(&mut enc, &mut cx_gb, sym, template, false, &at);
            i += 1;
        }
        encode_int(&mut enc, &mut iadw, None);
    }
    encode_int(&mut enc, &mut iaex, Some(0));
    encode_int(&mut enc, &mut iaex, Some(symbols.len() as i64));
    p.extend_from_slice(&enc.finish());
    p
}

type TextStripItem = (i64, i64, usize);
type TextStrip = (i64, i64, Vec<TextStripItem>);

#[allow(clippy::too_many_arguments)]
fn text_region_arith_payload(
    w: u32,
    h: u32,
    x: u32,
    y: u32,
    ext_op: u8,
    num_syms: usize,
    log_strips: u32,
    ref_corner: u8,
    transposed: bool,
    sb_comb_op: u8,
    def_pixel: u8,
    ds_offset: i8,
    strips: &[TextStrip],
) -> Vec<u8> {
    let total_instances: usize = strips.iter().map(|(_, _, s)| s.len()).sum();
    let mut p = region_info(w, h, x, y, ext_op);
    let ds_bits = ((ds_offset as i32) & 0x1F) as u16;
    let flags: u16 = ((log_strips as u16) << 2)
        | ((ref_corner as u16) << 4)
        | (u16::from(transposed) << 6)
        | ((sb_comb_op as u16) << 7)
        | ((def_pixel as u16) << 9)
        | (ds_bits << 10);
    p.extend_from_slice(&flags.to_be_bytes());
    p.extend_from_slice(&(total_instances as u32).to_be_bytes());

    // T.88 7.4.3.1 / A.3: arithmetic IAID uses ceil(log2(SBNUMSYMS)) bits
    // (0 bits when SBNUMSYMS == 1).
    let code_len = ceil_log2(num_syms);
    let mut enc = MqEncoder::new();
    let mut iadt = vec![0u8; INT_CTX_SIZE];
    let mut iafs = vec![0u8; INT_CTX_SIZE];
    let mut iads = vec![0u8; INT_CTX_SIZE];
    let mut iait = vec![0u8; INT_CTX_SIZE];
    let mut iaid = vec![0u8; 1usize << (code_len + 1)];

    encode_int(&mut enc, &mut iadt, Some(0));
    for (dt, dfs, items) in strips {
        encode_int(&mut enc, &mut iadt, Some(*dt));
        encode_int(&mut enc, &mut iafs, Some(*dfs));
        for (idx, &(ids, curt, sym_id)) in items.iter().enumerate() {
            if idx > 0 {
                encode_int(&mut enc, &mut iads, Some(ids));
            }
            if log_strips > 0 {
                encode_int(&mut enc, &mut iait, Some(curt));
            }
            encode_iaid(&mut enc, &mut iaid, code_len, sym_id);
        }
        encode_int(&mut enc, &mut iads, None);
    }
    p.extend_from_slice(&enc.finish());
    p
}

fn pattern_dict_payload(patterns: &[&Bitmap], template: u8) -> Vec<u8> {
    let hdpw = patterns[0].width;
    let hdph = patterns[0].height;
    let mut pd = vec![template << 1, hdpw as u8, hdph as u8];
    pd.extend_from_slice(&(patterns.len() as u32 - 1).to_be_bytes());
    let mut collective = Bitmap::new(hdpw * patterns.len(), hdph, 0);
    for (i, pat) in patterns.iter().enumerate() {
        for yy in 0..hdph {
            for xx in 0..hdpw {
                if pat.get(xx as i64, yy as i64) != 0 {
                    collective.set(i * hdpw + xx, yy, 1);
                }
            }
        }
    }
    let at = [(-(hdpw as i64) as i8, 0i8), (-3, -1), (2, -2), (-2, -2)];
    let mut enc = MqEncoder::new();
    let mut cx = vec![0u8; 1 << 16];
    encode_generic(&mut enc, &mut cx, &collective, template, false, &at);
    pd.extend_from_slice(&enc.finish());
    pd
}

// ---------------------------------------------------------------------------
// 1. Polarity, bit-order, and row-padding sanity checks
// ---------------------------------------------------------------------------

#[test]
fn oracle_polarity_and_non_byte_aligned_widths() {
    // Asymmetric bit patterns across byte-aligned and non-byte-aligned widths
    // on both white (default=0) and black (default=1) page backgrounds.
    // Verifies neither polarity inversion nor trailing-bit padding masks diffs.
    for width in [1usize, 3, 7, 8, 9, 13, 15, 16, 17, 31, 32, 33] {
        for default_pixel in [0u8, 1u8] {
            let height = 5usize;
            let mut bm = Bitmap::new(width, height, 0);
            for y in 0..height {
                for x in 0..width {
                    if (x * 5 + y * 3 + usize::from(default_pixel)) % 7 < 3 {
                        bm.set(x, y, 1);
                    }
                }
            }
            let page_flags = (default_pixel << 2) | PAGE_COMB_OP_OVERRIDDEN;
            let mut stream = segment(
                0,
                48,
                &[],
                1,
                &page_info_payload(width as u32, height as u32, page_flags),
            );
            stream.extend_from_slice(&segment(
                1,
                38,
                &[],
                1,
                &generic_region_payload(&bm, 0, 0, 4, 0, false),
            ));
            let label = format!("width={width} default_pixel={default_pixel}");
            let img = assert_oracle_match_strict(&label, &stream, None);
            assert_eq!(img.pixels, bm.data, "{label}: expected exact input bitmap");
        }
    }
}

#[test]
fn oracle_page_default_pixel_and_explicit_pdf_polarity() {
    for default_pixel in [0u8, 1u8] {
        let stream = segment(0, 48, &[], 1, &page_info_payload(11, 4, default_pixel << 2));
        let img = assert_oracle_match_strict(
            &format!("blank default_pixel={default_pixel}"),
            &stream,
            None,
        );
        assert!(img.pixels.iter().all(|&p| p == default_pixel));
        let expected_row = if default_pixel == 0 {
            [0xFFu8, 0xE0u8] // 11 white (1) bits + 5 zero pad bits
        } else {
            [0x00u8, 0x00u8] // 11 black (0) bits + 5 zero pad bits
        };
        assert_eq!(img.packed_pdf, expected_row.repeat(4));
    }

    // Verify a single asymmetric byte (`#.#..##.` -> JBIG2 10100110 -> PDF 01011001 = 0x59)
    // so an accidental double inversion in the oracle harness is impossible.
    let asym = Bitmap::from_art(&["#.#..##."]);
    let mut stream = segment(0, 48, &[], 1, &page_info_payload(8, 1, 0));
    stream.extend_from_slice(&segment(
        1,
        38,
        &[],
        1,
        &generic_region_payload(&asym, 0, 0, 0, 0, false),
    ));
    let img = assert_oracle_match_strict("asymmetric polarity check", &stream, None);
    assert_eq!(img.packed_pdf, vec![0x59]);
}

// ---------------------------------------------------------------------------
// 2. Generic regions: templates 0-3, TPGDON, custom AT, MMR, sparse, striped
// ---------------------------------------------------------------------------

#[test]
fn oracle_generic_regions_all_templates_tpgdon_and_custom_at() {
    let bm = Bitmap::from_art(&[
        "...................",
        "..#####....####....",
        "..#####....####....",
        "..#...#...#....#...",
        "..#####...#....#...",
        "..#...#...#....#...",
        "..#####....####....",
        "...................",
        "...................",
        "#.#.#.#.#.#.#.#.#.#",
    ]);

    for template in 0..4u8 {
        for tpgdon in [false, true] {
            for seg_type in [38u8, 39u8] {
                let page_flags = if seg_type == 39 {
                    PAGE_EVENTUALLY_LOSSLESS
                } else {
                    0
                };
                let mut stream = segment(
                    0,
                    48,
                    &[],
                    1,
                    &page_info_payload(bm.width as u32, bm.height as u32, page_flags),
                );
                stream.extend_from_slice(&segment(
                    1,
                    seg_type,
                    &[],
                    1,
                    &generic_region_payload(&bm, 0, 0, 0, template, tpgdon),
                ));
                let img = assert_oracle_match_strict(
                    &format!("generic template={template} tpgdon={tpgdon} seg={seg_type}"),
                    &stream,
                    None,
                );
                assert_eq!(img.pixels, bm.data);
            }
        }
    }

    // Non-nominal valid AT positions for template 0 and templates 1..=3.
    let custom_t0 = [(-1, -1), (1, -2), (-4, 0), (1, -1)];
    let mut stream = segment(
        0,
        48,
        &[],
        1,
        &page_info_payload(bm.width as u32, bm.height as u32, 0),
    );
    stream.extend_from_slice(&segment(
        1,
        38,
        &[],
        1,
        &generic_region_payload_at(&bm, 0, 0, 0, 0, true, custom_t0),
    ));
    let img = assert_oracle_match_strict("generic custom AT template 0", &stream, None);
    assert_eq!(img.pixels, bm.data);

    for template in 1..4u8 {
        let custom_at = [(-2, -1), (0, 0), (0, 0), (0, 0)];
        let mut stream = segment(
            0,
            48,
            &[],
            1,
            &page_info_payload(bm.width as u32, bm.height as u32, 0),
        );
        stream.extend_from_slice(&segment(
            1,
            38,
            &[],
            1,
            &generic_region_payload_at(&bm, 0, 0, 0, template, false, custom_at),
        ));
        let img = assert_oracle_match_strict(
            &format!("generic custom AT template {template}"),
            &stream,
            None,
        );
        assert_eq!(img.pixels, bm.data);
    }
}

#[test]
fn oracle_mmr_generic_region() {
    // Two rows "WWWBBWWW" (8x2) hand-coded in T.6: 0x31 0xF8 (plus optional EOFB).
    let mut payload = region_info(8, 2, 0, 0, 0);
    payload.push(0x01); // MMR = 1
    payload.extend_from_slice(&[0x31, 0xF8]);
    let mut stream = segment(0, 48, &[], 1, &page_info_payload(8, 2, 0));
    stream.extend_from_slice(&segment(1, 38, &[], 1, &payload));
    let img = assert_oracle_match("mmr 8x2", &stream, None);
    assert_eq!(img.packed_pdf, vec![0xE7, 0xE7]);

    // Offset MMR region onto a wider non-byte-aligned page (13x4) with XOR on
    // a black default background.
    let mut offset_payload = region_info(8, 2, 3, 1, 2); // XOR at (3, 1)
    offset_payload.push(0x01);
    offset_payload.extend_from_slice(&[0x31, 0xF8]);
    let mut stream = segment(
        0,
        48,
        &[],
        1,
        &page_info_payload(13, 4, PAGE_DEFAULT_BLACK | PAGE_COMB_OP_OVERRIDDEN),
    );
    stream.extend_from_slice(&segment(1, 38, &[], 1, &offset_payload));
    assert_oracle_match("mmr offset xor on black bg", &stream, None);
}

#[test]
fn oracle_multi_region_combination_operators_and_clipping() {
    let base = Bitmap::from_art(&[
        "....####....",
        "....####....",
        "..########..",
        "..########..",
        "....####....",
        "....####....",
    ]);
    let overlay = Bitmap::from_art(&["##..##..", ".####...", "..##..##", "####...."]);

    for default_pixel in [0u8, 1u8] {
        for op in 0..=4u8 {
            let page_flags = (default_pixel << 2) | PAGE_COMB_OP_OVERRIDDEN;
            let mut stream = segment(0, 48, &[], 1, &page_info_payload(12, 6, page_flags));
            stream.extend_from_slice(&segment(
                1,
                38,
                &[],
                1,
                &generic_region_payload(&base, 0, 0, 4, 0, false),
            ));
            // Compose overlay at (6, 3) so it both overlaps `base` and clips
            // past the right (x=12) and bottom (y=6) page edges.
            stream.extend_from_slice(&segment(
                2,
                38,
                &[],
                1,
                &generic_region_payload(&overlay, 6, 3, op, 1, false),
            ));
            assert_oracle_match_strict(
                &format!("multi-region default={default_pixel} op={op}"),
                &stream,
                None,
            );
        }
    }
}

#[test]
fn oracle_striped_auto_height_page_with_end_of_stripe() {
    let top = Bitmap::from_art(&["#..#..#.", ".##..##."]);
    let bottom = Bitmap::from_art(&["########", "........", "..####.."]);
    // T.88 7.4.8.6: mark the page striped with maximum stripe size 3 rows.
    let mut stream = segment(
        0,
        48,
        &[],
        1,
        &page_info_payload_striped(8, 0xFFFF_FFFF, 0, PAGE_STRIPED | 3),
    );
    stream.extend_from_slice(&segment(
        1,
        38,
        &[],
        1,
        &generic_region_payload(&top, 0, 0, 0, 0, false),
    ));
    stream.extend_from_slice(&segment(2, 50, &[], 1, &1u32.to_be_bytes()));
    stream.extend_from_slice(&segment(
        3,
        38,
        &[],
        1,
        &generic_region_payload(&bottom, 0, 2, 0, 2, true),
    ));
    stream.extend_from_slice(&segment(4, 50, &[], 1, &4u32.to_be_bytes()));
    let img = assert_oracle_match_strict("striped auto-height page", &stream, None);
    assert_eq!((img.width, img.height), (8, 5));
}

#[test]
fn oracle_sparse_large_generic_region() {
    let (w, h) = (513usize, 320usize);
    let mut bm = Bitmap::new(w, h, 0);
    for y in 120..150 {
        for x in 200..260 {
            if (x + y) % 3 == 0 {
                bm.set(x, y, 1);
            }
        }
    }
    let mut stream = segment(0, 48, &[], 1, &page_info_payload(w as u32, h as u32, 0));
    stream.extend_from_slice(&segment(
        1,
        38,
        &[],
        1,
        &generic_region_payload(&bm, 0, 0, 0, 0, true),
    ));
    let img = assert_oracle_match_strict("sparse 513x320 region", &stream, None);
    assert_eq!(img.pixels, bm.data);
}

// ---------------------------------------------------------------------------
// 3. Generic refinement regions: templates 0 & 1, TPGRON, custom AT, offsets
// ---------------------------------------------------------------------------

#[test]
fn oracle_refinement_regions_templates_tpgron_and_intermediate() {
    let initial = Bitmap::from_art(&[
        "..........",
        ".######...",
        ".######...",
        ".######...",
        ".######...",
        ".######...",
        "..........",
    ]);
    let target = Bitmap::from_art(&[
        "..####....",
        ".#....#...",
        ".#.##.#...",
        ".#....#...",
        ".######...",
        "...##.....",
        "..........",
    ]);
    let nominal_at = [(-1i8, -1i8), (-1i8, -1i8)];

    for template in 0..2u8 {
        for tpgron in [false, true] {
            // (a) Refine page in place (immediate refinement region, op=REPLACE).
            let mut ref_payload = region_info(initial.width as u32, initial.height as u32, 0, 0, 4);
            ref_payload.push(template | (u8::from(tpgron) << 1));
            if template == 0 {
                for &(ax, ay) in &nominal_at {
                    ref_payload.push(ax as u8);
                    ref_payload.push(ay as u8);
                }
            }
            let mut enc = MqEncoder::new();
            let mut cx = vec![0u8; 1 << 13];
            encode_refinement(
                &mut enc,
                &mut cx,
                &target,
                &initial,
                template,
                tpgron,
                &nominal_at,
                0,
                0,
            );
            ref_payload.extend_from_slice(&enc.finish());

            let mut stream = segment(
                0,
                48,
                &[],
                1,
                &page_info_payload(
                    initial.width as u32,
                    initial.height as u32,
                    PAGE_MIGHT_CONTAIN_REFINEMENTS | PAGE_COMB_OP_OVERRIDDEN,
                ),
            );
            stream.extend_from_slice(&segment(
                1,
                38,
                &[],
                1,
                &generic_region_payload(&initial, 0, 0, 0, 0, false),
            ));
            stream.extend_from_slice(&segment(2, 42, &[], 1, &ref_payload));
            let img = assert_oracle_match_strict(
                &format!("refine-page template={template} tpgron={tpgron}"),
                &stream,
                None,
            );
            assert_eq!(img.pixels, target.data);

            // (b) Refine a referred intermediate generic region (seg 36 -> seg 42).
            // T.88 7.4.7.5: the referred intermediate region and refinement
            // region share dimensions, location, and external operator.
            let mut stream = segment(
                0,
                48,
                &[],
                1,
                &page_info_payload(
                    initial.width as u32,
                    initial.height as u32,
                    PAGE_MIGHT_CONTAIN_REFINEMENTS
                        | PAGE_REQUIRES_AUX_BUFFERS
                        | PAGE_COMB_OP_OVERRIDDEN,
                ),
            );
            stream.extend_from_slice(&segment(
                1,
                36, // intermediate generic region
                &[],
                1,
                &generic_region_payload(&initial, 0, 0, 4, 0, false),
            ));
            stream.extend_from_slice(&segment(2, 42, &[1], 1, &ref_payload));
            let img = assert_oracle_match_strict(
                &format!("refine-intermediate template={template} tpgron={tpgron}"),
                &stream,
                None,
            );
            assert_eq!(img.pixels, target.data);
        }
    }

    // (c) Referred intermediate region and immediate refinement region at a
    // non-zero page location (3, 2) with matching dimensions, origin, and
    // external XOR operator per T.88 7.4.7.5 / Table 38 (dx=0, dy=0), using
    // custom template-0 AT pixels onto a pre-populated page.
    let custom_at = [(-2i8, 0i8), (1i8, -1i8)];
    let sub_initial = Bitmap::from_art(&[".###.", ".###.", ".###.", "....."]);
    let sub_target = Bitmap::from_art(&["#####", "#.#.#", "###.#", "..###"]);
    let (reg_x, reg_y) = (3u32, 2u32);
    let mut ref_payload = region_info(
        sub_target.width as u32,
        sub_target.height as u32,
        reg_x,
        reg_y,
        2, // XOR onto page
    );
    ref_payload.push(0x00); // template 0, TPGRON = 0
    for &(ax, ay) in &custom_at {
        ref_payload.push(ax as u8);
        ref_payload.push(ay as u8);
    }
    let mut enc = MqEncoder::new();
    let mut cx = vec![0u8; 1 << 13];
    encode_refinement(
        &mut enc,
        &mut cx,
        &sub_target,
        &sub_initial,
        0,
        false,
        &custom_at,
        0,
        0,
    );
    ref_payload.extend_from_slice(&enc.finish());

    let mut stream = segment(
        0,
        48,
        &[],
        1,
        &page_info_payload(
            12,
            9,
            PAGE_MIGHT_CONTAIN_REFINEMENTS | PAGE_REQUIRES_AUX_BUFFERS | PAGE_COMB_OP_OVERRIDDEN,
        ),
    );
    stream.extend_from_slice(&segment(
        1,
        36,
        &[],
        1,
        &generic_region_payload(&sub_initial, reg_x, reg_y, 2, 0, false),
    ));
    // Paint background ink on the page so XOR composition is observable.
    stream.extend_from_slice(&segment(
        2,
        38,
        &[],
        1,
        &generic_region_payload(&target, 1, 1, 0, 0, false),
    ));
    stream.extend_from_slice(&segment(3, 42, &[1], 1, &ref_payload));
    assert_oracle_match_strict(
        "refine-intermediate matching origin (3,2) + custom AT + XOR",
        &stream,
        None,
    );
}

// ---------------------------------------------------------------------------
// 4. Symbol dictionaries & text regions (arithmetic, REFAGG, Huffman, globals)
// ---------------------------------------------------------------------------

#[test]
fn oracle_symbol_dict_and_text_region_arith_variants() {
    let sym0 = Bitmap::from_art(&["#.#", ".#.", "#.#"]);
    let sym1 = Bitmap::from_art(&["..#", ".#.", "#.."]);
    let sym2 = Bitmap::from_art(&["####", "#..#", "#..#", "####"]);

    for template in 0..4u8 {
        let dict = symbol_dict_payload(&[&sym0, &sym1, &sym2], template);
        let text = text_region_arith_payload(
            20,
            12,
            0,
            0,
            0,
            3,
            1, // SBSTRIPS = 2
            1, // TOPLEFT
            false,
            0, // OR
            0,
            1, // DSOFFSET = +1
            &[
                (0, 1, vec![(0, 0, 0), (1, 1, 1), (1, 0, 2)]),
                (3, 0, vec![(0, 1, 2), (2, 0, 0)]),
            ],
        );
        let mut stream = segment(0, 48, &[], 1, &page_info_payload(20, 12, 0));
        stream.extend_from_slice(&segment(1, 0, &[], 1, &dict));
        stream.extend_from_slice(&segment(2, 6, &[1], 1, &text));
        assert_oracle_match_strict(&format!("symdict+text template={template}"), &stream, None);
    }

    // Single-symbol dictionary (SBNUMSYMS == 1 -> SBSYMCODELEN == 0 per T.88 A.3).
    let single_dict = symbol_dict_payload(&[&sym0], 0);
    let single_text = text_region_arith_payload(
        12,
        5,
        0,
        0,
        0,
        1,
        0,
        1,
        false,
        0,
        0,
        0,
        &[(1, 1, vec![(0, 0, 0), (2, 0, 0), (1, 0, 0)])],
    );
    let mut stream = segment(0, 48, &[], 1, &page_info_payload(12, 5, 0));
    stream.extend_from_slice(&segment(1, 0, &[], 1, &single_dict));
    stream.extend_from_slice(&segment(2, 6, &[1], 1, &single_text));
    assert_oracle_match_strict(
        "single-symbol arithmetic text region (code_len=0)",
        &stream,
        None,
    );
}

#[test]
fn oracle_text_region_ref_corners_transposed_and_comb_ops() {
    let sym0 = Bitmap::from_art(&["###", "#..", "###", "..#"]);
    let sym1 = Bitmap::from_art(&[".#.", "###", ".#."]);
    let dict = symbol_dict_payload(&[&sym1, &sym0], 0);

    for ref_corner in 0..4u8 {
        for transposed in [false, true] {
            for sb_comb_op in 0..4u8 {
                let text = text_region_arith_payload(
                    24,
                    24,
                    0,
                    0,
                    0,
                    2,
                    0, // SBSTRIPS = 1
                    ref_corner,
                    transposed,
                    sb_comb_op,
                    u8::from(sb_comb_op == 1 || sb_comb_op == 3),
                    0,
                    &[(6, 6, vec![(0, 0, 0), (2, 0, 1), (1, 0, 0)])],
                );
                let mut stream = segment(0, 48, &[], 1, &page_info_payload(24, 24, 0));
                stream.extend_from_slice(&segment(1, 0, &[], 1, &dict));
                stream.extend_from_slice(&segment(2, 6, &[1], 1, &text));
                assert_oracle_match_strict(
                    &format!(
                        "text ref_corner={ref_corner} transposed={transposed} op={sb_comb_op}"
                    ),
                    &stream,
                    None,
                );
            }
        }
    }
}

#[test]
fn oracle_symbol_dict_refagg_and_text_per_instance_refinement() {
    let base = Bitmap::from_art(&["####", "#..#", "#..#", "####"]);
    let t1 = Bitmap::from_art(&["####", "#..#", "#.##", "####"]);
    let t2 = Bitmap::from_art(&["####", "##.#", "#.##", "####"]);
    let at_r = [(-1i8, -1i8), (-1i8, -1i8)];

    let mut sd = Vec::new();
    sd.extend_from_slice(&(0x0002u16 | (1 << 12)).to_be_bytes());
    for &(ax, ay) in &nominal_at(0) {
        sd.push(ax as u8);
        sd.push(ay as u8);
    }
    sd.extend_from_slice(&3u32.to_be_bytes());
    sd.extend_from_slice(&3u32.to_be_bytes());

    let code_len = ceil_log2(4);
    let mut enc = MqEncoder::new();
    let mut cx_gr = vec![0u8; 1 << 13];
    let mut iadh = vec![0u8; INT_CTX_SIZE];
    let mut iadw = vec![0u8; INT_CTX_SIZE];
    let mut iaex = vec![0u8; INT_CTX_SIZE];
    let mut iaai = vec![0u8; INT_CTX_SIZE];
    let mut iardx = vec![0u8; INT_CTX_SIZE];
    let mut iardy = vec![0u8; INT_CTX_SIZE];
    let mut iadt = vec![0u8; INT_CTX_SIZE];
    let mut iafs = vec![0u8; INT_CTX_SIZE];
    let mut iads = vec![0u8; INT_CTX_SIZE];
    let mut iari = vec![0u8; INT_CTX_SIZE];
    let mut iaid = vec![0u8; 1usize << (code_len + 1)];

    encode_int(&mut enc, &mut iadh, Some(4));
    encode_int(&mut enc, &mut iadw, Some(4));
    encode_int(&mut enc, &mut iaai, Some(1));
    encode_iaid(&mut enc, &mut iaid, code_len, 0);
    encode_int(&mut enc, &mut iardx, Some(0));
    encode_int(&mut enc, &mut iardy, Some(0));
    encode_refinement(&mut enc, &mut cx_gr, &t1, &base, 1, false, &at_r, 0, 0);

    encode_int(&mut enc, &mut iadw, Some(0));
    encode_int(&mut enc, &mut iaai, Some(1));
    encode_iaid(&mut enc, &mut iaid, code_len, 1);
    encode_int(&mut enc, &mut iardx, Some(0));
    encode_int(&mut enc, &mut iardy, Some(0));
    encode_refinement(&mut enc, &mut cx_gr, &t2, &t1, 1, false, &at_r, 0, 0);

    encode_int(&mut enc, &mut iadw, Some(4));
    encode_int(&mut enc, &mut iaai, Some(2));
    encode_int(&mut enc, &mut iadt, Some(0));
    encode_int(&mut enc, &mut iadt, Some(0));
    encode_int(&mut enc, &mut iafs, Some(0));
    encode_iaid(&mut enc, &mut iaid, code_len, 0);
    encode_int(&mut enc, &mut iari, Some(0));
    encode_int(&mut enc, &mut iads, Some(1));
    encode_iaid(&mut enc, &mut iaid, code_len, 2);
    encode_int(&mut enc, &mut iari, Some(0));
    encode_int(&mut enc, &mut iads, None);
    encode_int(&mut enc, &mut iadw, None);
    encode_int(&mut enc, &mut iaex, Some(1));
    encode_int(&mut enc, &mut iaex, Some(3));
    sd.extend_from_slice(&enc.finish());

    let refined_5x5 = Bitmap::from_art(&["#####", "##.#.", "#.##.", "####.", "....#"]);
    let mut tr = region_info(16, 6, 0, 0, 0);
    let tr_flags: u16 = (1 << 4) | 0x0002 | (1 << 15);
    tr.extend_from_slice(&tr_flags.to_be_bytes());
    tr.extend_from_slice(&2u32.to_be_bytes());

    let tr_code_len = ceil_log2(3);
    let mut enc2 = MqEncoder::new();
    let mut iadt = vec![0u8; INT_CTX_SIZE];
    let mut iafs = vec![0u8; INT_CTX_SIZE];
    let mut iads = vec![0u8; INT_CTX_SIZE];
    let mut iari = vec![0u8; INT_CTX_SIZE];
    let mut iardw = vec![0u8; INT_CTX_SIZE];
    let mut iardh = vec![0u8; INT_CTX_SIZE];
    let mut iardx = vec![0u8; INT_CTX_SIZE];
    let mut iardy = vec![0u8; INT_CTX_SIZE];
    let mut iaid = vec![0u8; 1usize << (tr_code_len + 1)];
    let mut cx_gr = vec![0u8; 1 << 13];

    encode_int(&mut enc2, &mut iadt, Some(0));
    encode_int(&mut enc2, &mut iadt, Some(0));
    encode_int(&mut enc2, &mut iafs, Some(0));
    encode_iaid(&mut enc2, &mut iaid, tr_code_len, 1);
    encode_int(&mut enc2, &mut iari, Some(1));
    encode_int(&mut enc2, &mut iardw, Some(1));
    encode_int(&mut enc2, &mut iardh, Some(1));
    encode_int(&mut enc2, &mut iardx, Some(0));
    encode_int(&mut enc2, &mut iardy, Some(0));
    encode_refinement(
        &mut enc2,
        &mut cx_gr,
        &refined_5x5,
        &t2,
        1,
        false,
        &at_r,
        0,
        0,
    );
    encode_int(&mut enc2, &mut iads, Some(2));
    encode_iaid(&mut enc2, &mut iaid, tr_code_len, 2);
    encode_int(&mut enc2, &mut iari, Some(0));
    encode_int(&mut enc2, &mut iads, None);
    tr.extend_from_slice(&enc2.finish());

    let mut globals = segment(1, 0, &[], 0, &symbol_dict_payload(&[&base], 0));
    globals.extend_from_slice(&segment(2, 0, &[1], 0, &sd));
    let mut page = segment(
        3,
        48,
        &[],
        1,
        &page_info_payload(16, 6, PAGE_MIGHT_CONTAIN_REFINEMENTS),
    );
    page.extend_from_slice(&segment(4, 6, &[2], 1, &tr));

    assert_oracle_match_strict(
        "refagg globals + per-instance text refinement",
        &page,
        Some(&globals),
    );
}

#[test]
fn oracle_huffman_symbol_dict_mmr_and_uncompressed_and_huffman_text_refinement() {
    let dh_lines = std_huff_lines(4);
    let dw_lines = std_huff_lines(2);
    let b1_lines = std_huff_lines(1);
    let dt_lines = std_huff_lines(11);
    let ds_lines = std_huff_lines(8);
    let rd_lines = std_huff_lines(14);

    // (a) Huffman symbol dictionary with BMSIZE > 0 (MMR collective bitmap).
    let mmr = [0x31u8, 0xF8]; // two rows of "...##..." (8x2)
    let mut sd_mmr = Vec::new();
    sd_mmr.extend_from_slice(&0x0001u16.to_be_bytes());
    sd_mmr.extend_from_slice(&1u32.to_be_bytes());
    sd_mmr.extend_from_slice(&1u32.to_be_bytes());
    let mut bw = BitWriter::new();
    huff_encode(&mut bw, &dh_lines, Some(2));
    huff_encode(&mut bw, &dw_lines, Some(8));
    huff_encode(&mut bw, &dw_lines, None);
    huff_encode(&mut bw, &b1_lines, Some(mmr.len() as i64));
    bw.write_bytes(&mmr);
    huff_encode(&mut bw, &b1_lines, Some(0));
    huff_encode(&mut bw, &b1_lines, Some(1));
    sd_mmr.extend_from_slice(&bw.finish());

    // (b) Huffman symbol dictionary with BMSIZE == 0 (uncompressed collective
    // bitmap, T.88 6.5.9) containing 2 symbols of non-byte-aligned total width
    // (3 + 4 = 7 pixels wide, height 3).
    let collective_raw = Bitmap::from_art(&["#.#####", ".#.#..#", "#.###.#"]);
    let mut sd_raw = Vec::new();
    sd_raw.extend_from_slice(&0x0001u16.to_be_bytes());
    sd_raw.extend_from_slice(&2u32.to_be_bytes());
    sd_raw.extend_from_slice(&2u32.to_be_bytes());
    let mut bw_raw = BitWriter::new();
    huff_encode(&mut bw_raw, &dh_lines, Some(3));
    huff_encode(&mut bw_raw, &dw_lines, Some(3)); // sym 0 width = 3
    huff_encode(&mut bw_raw, &dw_lines, Some(1)); // sym 1 width = 4
    huff_encode(&mut bw_raw, &dw_lines, None);
    huff_encode(&mut bw_raw, &b1_lines, Some(0)); // BMSIZE = 0 -> uncompressed!
    bw_raw.write_bytes(&collective_raw.pack_jbig2_rows());
    huff_encode(&mut bw_raw, &b1_lines, Some(0));
    huff_encode(&mut bw_raw, &b1_lines, Some(2));
    sd_raw.extend_from_slice(&bw_raw.finish());

    // Custom Huffman table (segment 53) for SBHUFFFS: values 0..8, prefix=2, range=1.
    let mut custom_tab = Vec::new();
    custom_tab.push((3u8 << 1) | (2u8 << 4)); // oob=0, prefix_size=4, range_size=3
    custom_tab.extend_from_slice(&0i32.to_be_bytes());
    custom_tab.extend_from_slice(&8i32.to_be_bytes());
    let mut cbw = BitWriter::new();
    for _ in 0..4 {
        cbw.write_bits(2, 4);
        cbw.write_bits(1, 3);
    }
    cbw.write_bits(0, 4); // lower unused
    cbw.write_bits(0, 4); // upper unused
    custom_tab.extend_from_slice(&cbw.finish());
    let custom_lines = vec![
        HuffLine::normal(2, 1, 0),
        HuffLine::normal(2, 1, 2),
        HuffLine::normal(2, 1, 4),
        HuffLine::normal(2, 1, 6),
    ];

    // Huffman text region referencing BOTH dicts (3 symbols total) and the
    // custom FS table, with SBREFINE=1 and TWO refined instances back-to-back
    // (exercises T.88 7.4.3.1.7 alignment, RSIZE slicing, and cx_gr reuse).
    let sym_a = Bitmap::from_art(&["#.#", ".#.", "#.#"]);
    let ref_a = Bitmap::from_art(&["###", ".#.", "#.#"]);
    let sym_b = Bitmap::from_art(&["####", "#..#", "##.#"]);
    let ref_b = Bitmap::from_art(&["####", "####", "##.#"]);
    let at_r = [(-1i8, -1i8), (-1i8, -1i8)];

    let mut cx_gr = vec![0u8; 1 << 13];
    let mut enc_r1 = MqEncoder::new();
    encode_refinement(
        &mut enc_r1,
        &mut cx_gr,
        &ref_a,
        &sym_a,
        1,
        false,
        &at_r,
        0,
        0,
    );
    let r1_bytes = enc_r1.finish();
    let mut enc_r2 = MqEncoder::new();
    encode_refinement(
        &mut enc_r2,
        &mut cx_gr,
        &ref_b,
        &sym_b,
        1,
        false,
        &at_r,
        0,
        0,
    );
    let r2_bytes = enc_r2.finish();

    let mut tr = region_info(16, 8, 0, 0, 0);
    let tr_flags: u16 = 0x0001 | 0x0002 | (1 << 4) | (1 << 15); // SBHUFF, SBREFINE, TOPLEFT, SBRTEMPLATE=1
    tr.extend_from_slice(&tr_flags.to_be_bytes());
    let huff_flags: u16 = 3; // SBHUFFFS = custom, rest standard (0)
    tr.extend_from_slice(&huff_flags.to_be_bytes());
    tr.extend_from_slice(&3u32.to_be_bytes()); // 3 instances

    let mut tbw = BitWriter::new();
    // 35 run-code prefix lengths: run code 2 (sym code len = 2) has prefix_len = 1.
    for rc in 0..35 {
        tbw.write_bits(if rc == 2 { 1 } else { 0 }, 4);
    }
    // 3 symbols, each code length 2 (00, 01, 10) -> three '0' bits in runcode table.
    tbw.write_bit(0);
    tbw.write_bit(0);
    tbw.write_bit(0);
    // T.88 7.4.3.1.7 step 6: byte-align after symbol ID Huffman table!
    tbw.byte_align();

    huff_encode(&mut tbw, &dt_lines, Some(1)); // initial STRIPT = -1
                                               // Strip 1 (y = 0): sym 0 (MMR 8x2) unrefined at x = 1.
    huff_encode(&mut tbw, &dt_lines, Some(1));
    huff_encode(&mut tbw, &custom_lines, Some(1));
    tbw.write_bits(0b00, 2); // sym 0
    tbw.write_bit(0); // RI = 0
    huff_encode(&mut tbw, &ds_lines, None);
    // Strip 2 (y = 3): sym 1 refined -> ref_a at x = 2, then sym 2 refined -> ref_b at DS = 2.
    huff_encode(&mut tbw, &dt_lines, Some(3));
    huff_encode(&mut tbw, &custom_lines, Some(1)); // firsts = 1 + 1 = 2
    tbw.write_bits(0b01, 2); // sym 1
    tbw.write_bit(1); // RI = 1
    huff_encode(&mut tbw, &rd_lines, Some(0)); // RDW
    huff_encode(&mut tbw, &rd_lines, Some(0)); // RDH
    huff_encode(&mut tbw, &rd_lines, Some(0)); // RDX
    huff_encode(&mut tbw, &rd_lines, Some(0)); // RDY
    huff_encode(&mut tbw, &b1_lines, Some(r1_bytes.len() as i64)); // RSIZE
    tbw.write_bytes(&r1_bytes);
    huff_encode(&mut tbw, &ds_lines, Some(2));
    tbw.write_bits(0b10, 2); // sym 2
    tbw.write_bit(1); // RI = 1
    huff_encode(&mut tbw, &rd_lines, Some(0));
    huff_encode(&mut tbw, &rd_lines, Some(0));
    huff_encode(&mut tbw, &rd_lines, Some(0));
    huff_encode(&mut tbw, &rd_lines, Some(0));
    huff_encode(&mut tbw, &b1_lines, Some(r2_bytes.len() as i64));
    tbw.write_bytes(&r2_bytes);
    huff_encode(&mut tbw, &ds_lines, None);
    tr.extend_from_slice(&tbw.finish());

    let mut globals = segment(1, 0, &[], 0, &sd_mmr);
    globals.extend_from_slice(&segment(2, 0, &[], 0, &sd_raw));
    globals.extend_from_slice(&segment(3, 53, &[], 0, &custom_tab));
    let mut page = segment(
        4,
        48,
        &[],
        1,
        &page_info_payload(16, 8, PAGE_MIGHT_CONTAIN_REFINEMENTS),
    );
    page.extend_from_slice(&segment(5, 6, &[1, 2, 3], 1, &tr));

    assert_oracle_match(
        "huffman symdict (MMR + BMSIZE=0 raw) + custom table + huffman text with 2 refined instances",
        &page,
        Some(&globals),
    );
}

// ---------------------------------------------------------------------------
// 5. Pattern dictionaries & halftone regions (arithmetic, ENABLE_SKIP, tilt, MMR)
// ---------------------------------------------------------------------------

#[test]
fn oracle_pattern_dict_and_halftone_regions() {
    let p0 = Bitmap::from_art(&["..", ".."]);
    let p1 = Bitmap::from_art(&["#.", ".."]);
    let p2 = Bitmap::from_art(&["#.", ".#"]);
    let p3 = Bitmap::from_art(&["##", "##"]);
    let pd = pattern_dict_payload(&[&p0, &p1, &p2, &p3], 0);

    let gray = [[1u32, 2, 3], [0, 3, 1], [2, 1, 0]];
    let (hgw, hgh) = (3usize, 3usize);
    let (reg_w, reg_h) = (8i64, 8i64);

    for enable_skip in [false, true] {
        for comb_op in 0..=4u8 {
            let def_pixel = u8::from(comb_op == 1 || comb_op == 3);
            let hgx: i32 = if enable_skip { -(2 << 8) } else { 1 << 8 };
            let hgy: i32 = 1 << 8;
            let hrx: u16 = 2 << 8;
            let hry: u16 = if enable_skip { 1 << 7 } else { 0 }; // tilted grid when skip=true

            let mut ht = region_info(reg_w as u32, reg_h as u32, 0, 0, 0);
            let flags = (u8::from(enable_skip) << 3) | (comb_op << 4) | (def_pixel << 7);
            ht.push(flags);
            ht.extend_from_slice(&(hgw as u32).to_be_bytes());
            ht.extend_from_slice(&(hgh as u32).to_be_bytes());
            ht.extend_from_slice(&(hgx as u32).to_be_bytes());
            ht.extend_from_slice(&(hgy as u32).to_be_bytes());
            ht.extend_from_slice(&hrx.to_be_bytes());
            ht.extend_from_slice(&hry.to_be_bytes());

            // Compute T.88 6.6.5.1 HSKIP bitmap so encoder skips the exact same cells.
            let skip = if enable_skip {
                let mut s = Bitmap::new(hgw, hgh, 0);
                for m in 0..hgh {
                    for n in 0..hgw {
                        let x =
                            (hgx as i64 + (m as i64) * (hry as i64) + (n as i64) * (hrx as i64))
                                >> 8;
                        let y = (hgy as i64 + (m as i64) * (hrx as i64)
                            - (n as i64) * (hry as i64))
                            >> 8;
                        if x + 2 <= 0 || x >= reg_w || y + 2 <= 0 || y >= reg_h {
                            s.set(n, m, 1);
                        }
                    }
                }
                Some(s)
            } else {
                None
            };

            let mut enc = MqEncoder::new();
            let mut cx = vec![0u8; 1 << 16];
            let at = [(3i8, -1i8), (-3, -1), (2, -2), (-2, -2)];
            for j in (0..2usize).rev() {
                let mut plane = Bitmap::new(hgw, hgh, 0);
                for (m, row) in gray.iter().enumerate() {
                    for (n, &v) in row.iter().enumerate() {
                        if skip
                            .as_ref()
                            .is_some_and(|s| s.get(n as i64, m as i64) != 0)
                        {
                            continue;
                        }
                        let g = v ^ (v >> 1);
                        if (g >> j) & 1 == 1 {
                            plane.set(n, m, 1);
                        }
                    }
                }
                encode_generic_with_skip(&mut enc, &mut cx, &plane, 0, false, &at, skip.as_ref());
            }
            ht.extend_from_slice(&enc.finish());

            let globals = segment(1, 16, &[], 0, &pd);
            let mut page = segment(
                2,
                48,
                &[],
                1,
                &page_info_payload(reg_w as u32, reg_h as u32, 0),
            );
            page.extend_from_slice(&segment(3, 22, &[1], 1, &ht));
            assert_oracle_match_strict(
                &format!("halftone skip={enable_skip} op={comb_op}"),
                &page,
                Some(&globals),
            );
        }
    }

    // MMR pattern dictionary (HDMMR = 1): two 4x2 patterns packed side-by-side
    // into 8x2 MMR "WWWBBWWW" (0x31 0xF8) -> pattern 0 = "...#", pattern 1 = "#...".
    let mut pd_mmr = vec![0x01u8, 4, 2]; // HDMMR = 1, HDPW = 4, HDPH = 2
    pd_mmr.extend_from_slice(&1u32.to_be_bytes()); // GRAYMAX = 1 (2 patterns)
    pd_mmr.extend_from_slice(&[0x31, 0xF8]);

    // Single-plane MMR halftone region: 8x2 grid using the same MMR data.
    let mut ht_mmr = region_info(32, 4, 0, 0, 0);
    ht_mmr.push(0x01); // HMMR = 1, OR
    ht_mmr.extend_from_slice(&8u32.to_be_bytes()); // HGW = 8
    ht_mmr.extend_from_slice(&2u32.to_be_bytes()); // HGH = 2
    ht_mmr.extend_from_slice(&0u32.to_be_bytes()); // HGX
    ht_mmr.extend_from_slice(&0u32.to_be_bytes()); // HGY
    ht_mmr.extend_from_slice(&(4u16 << 8).to_be_bytes()); // HRX = 4.0
    ht_mmr.extend_from_slice(&0u16.to_be_bytes()); // HRY = 0
    ht_mmr.extend_from_slice(&[0x31, 0xF8]);

    let globals = segment(1, 16, &[], 0, &pd_mmr);
    let mut page = segment(2, 48, &[], 1, &page_info_payload(32, 4, 0));
    page.extend_from_slice(&segment(3, 22, &[1], 1, &ht_mmr));
    assert_oracle_match(
        "mmr pattern dict + single-plane mmr halftone",
        &page,
        Some(&globals),
    );

    // Clipped arithmetic halftone grid with ENABLE_SKIP = 1 whose full
    // `grid_cells * hbpp` (256 * 256 * 2 = 131_072) exceeds `mq_iteration_budget`
    // for the tiny MQ payload, while its 4 coded (unskipped) cells fit easily.
    // Must decode identically to hayro-jbig2 rather than failing blank.
    let (big_hgw, big_hgh) = (256usize, 256usize);
    let mut clipped_ht = region_info(4, 4, 0, 0, 0);
    clipped_ht.push(0x08); // MMR=0, template=0, ENABLE_SKIP=1, OR, def_pixel=0
    clipped_ht.extend_from_slice(&(big_hgw as u32).to_be_bytes());
    clipped_ht.extend_from_slice(&(big_hgh as u32).to_be_bytes());
    clipped_ht.extend_from_slice(&0u32.to_be_bytes()); // HGX = 0
    clipped_ht.extend_from_slice(&0u32.to_be_bytes()); // HGY = 0
    clipped_ht.extend_from_slice(&(2u16 << 8).to_be_bytes()); // HRX = 2.0
    clipped_ht.extend_from_slice(&0u16.to_be_bytes()); // HRY = 0

    let mut skip = Bitmap::new(big_hgw, big_hgh, 1);
    for m in 0..2 {
        for n in 0..2 {
            skip.set(n, m, 0);
        }
    }
    let visible_gray = [[1u32, 2], [3, 0]];
    let mut enc = MqEncoder::new();
    let mut cx = vec![0u8; 1 << 16];
    let at = [(3i8, -1i8), (-3, -1), (2, -2), (-2, -2)];
    for j in (0..2usize).rev() {
        let mut plane = Bitmap::new(big_hgw, big_hgh, 0);
        for (m, row) in visible_gray.iter().enumerate() {
            for (n, &v) in row.iter().enumerate() {
                let g = v ^ (v >> 1);
                if (g >> j) & 1 == 1 {
                    plane.set(n, m, 1);
                }
            }
        }
        encode_generic_with_skip(&mut enc, &mut cx, &plane, 0, false, &at, Some(&skip));
    }
    let mq_bytes = enc.finish();
    assert!(
        mq_bytes.len() * 8 * 1024 < big_hgw * big_hgh * 2,
        "fixture must exceed naive grid_cells * hbpp MQ budget ({} bytes)",
        mq_bytes.len()
    );
    clipped_ht.extend_from_slice(&mq_bytes);

    let globals = segment(1, 16, &[], 0, &pd);
    let mut page = segment(2, 48, &[], 1, &page_info_payload(4, 4, 0));
    page.extend_from_slice(&segment(3, 22, &[1], 1, &clipped_ht));
    let img = assert_oracle_match_strict(
        "clipped halftone grid coded-cells MQ budget",
        &page,
        Some(&globals),
    );
    // Non-blank verification: cells (0,0)=p1, (1,0)=p2, (0,1)=p3, (1,1)=p0.
    let expected = Bitmap::from_art(&["#.#.", "...#", "##..", "##.."]);
    assert_eq!(img.pixels, expected.data);

    // Under a tight shared pixel-work allowance (`max_image_pixels = 262_144`,
    // `region_work_limit = 524_288`), one clipped 256x256 halftone (262,144
    // traversal work) still renders and matches hayro-jbig2 bit-for-bit, while
    // repeated clipped halftones stop on cumulative pixel-work exhaustion.
    let tight_limits = ParseLimits {
        max_image_pixels: (big_hgw * big_hgh * 4) as u64,
        ..ParseLimits::default()
    };
    let z_one = decode_zpdf(&page, Some(&globals), &tight_limits)
        .expect("one clipped halftone fits the tight shared pixel-work allowance");
    assert_eq!(z_one, img.packed_pdf);

    let mut repeated_clipped = page.clone();
    repeated_clipped.extend_from_slice(&segment(4, 22, &[1], 1, &clipped_ht));
    repeated_clipped.extend_from_slice(&segment(5, 22, &[1], 1, &clipped_ht));
    assert!(
        decode_hayro(
            &repeated_clipped,
            Some(&globals),
            DecodeSettings { strict: false },
        )
        .is_ok(),
        "hayro-jbig2 decodes repeated clipped halftones unconditionally"
    );
    let err = decode_zpdf(&repeated_clipped, Some(&globals), &tight_limits)
        .expect_err("zpdf must stop repeated clipped halftone full-grid scans");
    assert!(
        err.to_string().contains("pixel-work budget"),
        "expected cumulative pixel-work budget error, got {err}"
    );
}

// ---------------------------------------------------------------------------
// 6. Full PDF container round-trip with indirect (raw & Flate) /JBIG2Globals
// ---------------------------------------------------------------------------

#[test]
fn oracle_pdf_container_with_indirect_jbig2_globals() {
    let sym = Bitmap::from_art(&["#.#", ".#.", "#.#"]);
    let globals = segment(1, 0, &[], 0, &symbol_dict_payload(&[&sym], 0));
    let mut page = segment(2, 48, &[], 1, &page_info_payload(11, 5, 0));
    page.extend_from_slice(&segment(
        3,
        6,
        &[1],
        1,
        &text_region_arith_payload(
            11,
            5,
            0,
            0,
            0,
            1,
            0,
            1,
            false,
            0,
            0,
            0,
            &[(1, 1, vec![(0, 0, 0), (3, 0, 0)])],
        ),
    ));

    let mut zenc = ZlibEncoder::new(Vec::new(), Compression::default());
    zenc.write_all(&globals).unwrap();
    let flate_globals = zenc.finish().unwrap();

    for (label, globals_filter, globals_bytes) in [
        ("raw-globals", "", globals.as_slice()),
        (
            "flate-globals",
            "/Filter /FlateDecode",
            flate_globals.as_slice(),
        ),
    ] {
        let make_stream = |entries: &str, bytes: &[u8]| {
            let mut obj = format!("<< /Length {} {entries} >>\nstream\n", bytes.len()).into_bytes();
            obj.extend_from_slice(bytes);
            obj.extend_from_slice(b"\nendstream");
            obj
        };
        let objects = [
            (1, b"<< /Type /Catalog >>".to_vec()),
            (
                2,
                make_stream(
                    "/Type /XObject /Subtype /Image /Width 11 /Height 5 /ColorSpace /DeviceGray /BitsPerComponent 1 /Filter /JBIG2Decode /DecodeParms << /JBIG2Globals 3 0 R >>",
                    &page,
                ),
            ),
            (3, make_stream(globals_filter, globals_bytes)),
        ];
        let mut pdf = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::new();
        for (num, body) in objects {
            offsets.push((num, pdf.len()));
            pdf.extend_from_slice(format!("{num} 0 obj\n").as_bytes());
            pdf.extend_from_slice(&body);
            pdf.extend_from_slice(b"\nendobj\n");
        }
        let xref = pdf.len();
        pdf.extend_from_slice(b"xref\n0 4\n0000000000 65535 f \n");
        for (_, off) in offsets {
            pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!("trailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
        );

        let file = PdfFile::parse(pdf).expect("parse synthetic PDF");
        let zpdf_packed = file
            .resolve_stream_data(ObjectId(2, 0))
            .expect("resolve JBIG2 stream with indirect globals");
        let hayro = decode_hayro(&page, Some(&globals), DecodeSettings { strict: true })
            .expect("hayro decode");
        let zpdf_pixels =
            unpack_pdf_to_native(&zpdf_packed, hayro.width, hayro.height, label).expect("unpack");
        assert_eq!(zpdf_pixels, hayro.pixels, "{label}: pixel mismatch");
        assert_eq!(zpdf_packed, hayro.packed_pdf, "{label}: packed mismatch");
    }
}

// ---------------------------------------------------------------------------
// 7. Checked-in JBIG2 fuzz seeds & real-world PDF corpus oracle tests
// ---------------------------------------------------------------------------

#[test]
fn oracle_checked_in_jbig2_fuzz_seeds() {
    let seeds_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/seeds/filters");
    let mut found = 0usize;
    for entry in std::fs::read_dir(&seeds_dir).expect("read fuzz/seeds/filters") {
        let path = entry.expect("dir entry").path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with("jbig2") {
            continue;
        }
        let raw = std::fs::read(&path).unwrap_or_else(|e| panic!("read {name}: {e}"));
        assert!(
            raw.len() > 2,
            "fuzz seed {name} must have a 2-byte filter header + JBIG2 payload"
        );
        // `fuzz/fuzz_targets/filters.rs` selects FILTERS[sel % 15]; index 12 is "JBIG2Decode".
        assert_eq!(
            raw[0] % 15,
            12,
            "fuzz seed {name} byte 0 must select JBIG2Decode (index 12)"
        );
        let payload = &raw[2..];
        assert_oracle_match(&format!("fuzz-seed:{name}"), payload, None);
        found += 1;
    }
    assert!(
        found >= 2,
        "expected checked-in JBIG2 fuzz seeds under fuzz/seeds/filters/"
    );
}

fn resolve_dict_entry(file: &PdfFile, obj: &PdfObject) -> Option<PdfObject> {
    match obj {
        PdfObject::Ref(r) => file.resolve(*r).ok(),
        other => Some(other.clone()),
    }
}

fn extract_jbig2_inputs(file: &PdfFile, id: ObjectId) -> Option<(Vec<u8>, Option<Vec<u8>>)> {
    let PdfObject::Stream(stream) = file.resolve(id).ok()? else {
        return None;
    };
    let filter_obj = resolve_dict_entry(file, stream.dict.get("Filter")?)?;
    let filters: Vec<String> = match filter_obj {
        PdfObject::Name(n) => vec![n.as_str().to_string()],
        PdfObject::Array(arr) => arr
            .iter()
            .filter_map(|e| match resolve_dict_entry(file, e)? {
                PdfObject::Name(n) => Some(n.as_str().to_string()),
                _ => None,
            })
            .collect(),
        _ => return None,
    };
    let jbig2_idx = filters.iter().position(|f| f == "JBIG2Decode")?;

    // Resolve positional /DecodeParms (falling back to /DP only when
    // /DecodeParms is absent, matching zpdf_parser::filters).
    let parms_raw = stream
        .dict
        .get("DecodeParms")
        .or_else(|| stream.dict.get("DP"))
        .and_then(|o| resolve_dict_entry(file, o));
    let mut resolved_parms: Vec<Option<PdfDict>> = vec![None; filters.len()];
    match parms_raw {
        Some(PdfObject::Dict(d)) if !resolved_parms.is_empty() => {
            resolved_parms[0] = Some(d);
        }
        Some(PdfObject::Array(arr)) => {
            for (slot, entry) in resolved_parms.iter_mut().zip(arr.iter()) {
                if let Some(PdfObject::Dict(d)) = resolve_dict_entry(file, entry) {
                    *slot = Some(d);
                }
            }
        }
        _ => {}
    }

    let raw_jbig2 = if jbig2_idx == 0 {
        stream.data.to_vec()
    } else {
        let mut prefix_dict = PdfDict::new();
        prefix_dict.insert(
            PdfName::new("Filter"),
            PdfObject::Array(
                filters[..jbig2_idx]
                    .iter()
                    .map(|f| PdfObject::Name(PdfName::new(f)))
                    .collect(),
            ),
        );
        if resolved_parms[..jbig2_idx].iter().any(Option::is_some) {
            prefix_dict.insert(
                PdfName::new("DecodeParms"),
                PdfObject::Array(
                    resolved_parms[..jbig2_idx]
                        .iter()
                        .map(|p| match p {
                            Some(d) => PdfObject::Dict(d.clone()),
                            None => PdfObject::Null,
                        })
                        .collect(),
                ),
            );
        }
        decode_stream_with_limits(&stream.data, &prefix_dict, &ParseLimits::default()).ok()?
    };

    let jbig2_parms = resolved_parms.get(jbig2_idx).and_then(|p| p.as_ref());
    let globals = jbig2_parms.and_then(|d| match d.get("JBIG2Globals") {
        Some(PdfObject::Ref(r)) => file.resolve_stream_data(*r).ok(),
        Some(PdfObject::String(s)) => Some(s.0.clone()),
        Some(PdfObject::Stream(st)) => Some(st.data.to_vec()),
        _ => None,
    });

    Some((raw_jbig2, globals))
}

#[test]
fn oracle_extract_jbig2_inputs_preserves_prefix_decode_parms() {
    // Build a valid PDF-embedded JBIG2 stream + globals (no EndOfPage/EndOfFile
    // per ISO 32000-1 §7.4.7), encode the JBIG2 bytes with PNG Up predictor
    // (Predictor=12) + FlateDecode, and wrap in a PDF with
    // `/Filter [/FlateDecode /JBIG2Decode]` and indirect positional
    // `/DecodeParms [4 0 R 5 0 R]`.
    let sym = Bitmap::from_art(&["#.#", ".#.", "#.#"]);
    let globals = segment(1, 0, &[], 0, &symbol_dict_payload(&[&sym], 0));
    let mut page = segment(2, 48, &[], 1, &page_info_payload(8, 3, 0));
    page.extend_from_slice(&segment(
        3,
        6,
        &[1],
        1,
        &text_region_arith_payload(
            8,
            3,
            0,
            0,
            0,
            1,
            0,
            1,
            false,
            0,
            0,
            0,
            &[(0, 2, vec![(0, 0, 0)])],
        ),
    ));
    // Choose a column count dividing `page.len()` cleanly (or 1 -> exact length).
    let cols = [8usize, 4, 2, 1]
        .into_iter()
        .find(|&c| page.len().is_multiple_of(c))
        .unwrap();
    let mut predicted = Vec::with_capacity(page.len() + page.len() / cols);
    let mut prev = vec![0u8; cols];
    for row in page.chunks_exact(cols) {
        predicted.push(2u8); // PNG Up filter
        for (i, &b) in row.iter().enumerate() {
            predicted.push(b.wrapping_sub(prev[i]));
        }
        prev.copy_from_slice(row);
    }
    let mut zenc = ZlibEncoder::new(Vec::new(), Compression::default());
    zenc.write_all(&predicted).unwrap();
    let flate_predicted = zenc.finish().unwrap();

    let make_stream = |entries: &str, bytes: &[u8]| {
        let mut obj = format!("<< /Length {} {entries} >>\nstream\n", bytes.len()).into_bytes();
        obj.extend_from_slice(bytes);
        obj.extend_from_slice(b"\nendstream");
        obj
    };
    let objects = [
        (1, b"<< /Type /Catalog >>".to_vec()),
        (
            2,
            make_stream(
                "/Type /XObject /Subtype /Image /Width 8 /Height 3 /ColorSpace /DeviceGray /BitsPerComponent 1 /Filter [/FlateDecode /JBIG2Decode] /DecodeParms [4 0 R 5 0 R]",
                &flate_predicted,
            ),
        ),
        (3, make_stream("", &globals)),
        (4, format!("<< /Predictor 12 /Columns {cols} >>").into_bytes()),
        (5, b"<< /JBIG2Globals 3 0 R >>".to_vec()),
    ];
    let mut pdf = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (num, body) in objects {
        offsets.push((num, pdf.len()));
        pdf.extend_from_slice(format!("{num} 0 obj\n").as_bytes());
        pdf.extend_from_slice(&body);
        pdf.extend_from_slice(b"\nendobj\n");
    }
    let xref = pdf.len();
    pdf.extend_from_slice(b"xref\n0 6\n0000000000 65535 f \n");
    for (_, off) in offsets {
        pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!("trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
    );

    let file = PdfFile::parse(pdf).expect("parse chained-filter PDF");
    let (extracted_page, extracted_globals) =
        extract_jbig2_inputs(&file, ObjectId(2, 0)).expect("extract chained JBIG2 stream");
    assert_eq!(
        extracted_page, page,
        "prefix predictor DecodeParms must be applied"
    );
    assert_eq!(extracted_globals.as_deref(), Some(globals.as_slice()));

    let img = assert_oracle_match_strict(
        "chained Flate+PNG-Up + JBIG2Decode",
        &extracted_page,
        extracted_globals.as_deref(),
    );
    let direct = file
        .resolve_stream_data(ObjectId(2, 0))
        .expect("PdfFile chained decode");
    assert_eq!(direct, img.packed_pdf);
}

#[test]
fn oracle_checked_in_corpus_jbig2_pdfs() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let pdfs = [
        "tests/failed/batch1/PDFBOX/PDFBOX-3948-0.pdf",
        "tests/failed/batch2/GHOSTSCRIPT/GHOSTSCRIPT-693513-4.pdf",
        "tests/failed/batch2/GHOSTSCRIPT/GHOSTSCRIPT-697866-0.pdf",
        "tests/failed/batch2/GHOSTSCRIPT/GHOSTSCRIPT-698998-0.pdf",
        "tests/failed/batch2/GHOSTSCRIPT/GHOSTSCRIPT-699083-1.pdf",
        "tests/failed/batch2/GHOSTSCRIPT/GHOSTSCRIPT-699142-0.pdf",
        "tests/failed/batch2/GHOSTSCRIPT/GHOSTSCRIPT-699673-0.pdf",
        "tests/failed/batch2/GHOSTSCRIPT/GHOSTSCRIPT-699694-0.pdf",
        "tests/failed/batch5/PDFIUM/PDFIUM-1219-0.pdf",
        "tests/failed/batch5/poppler/poppler-101525-0.pdf",
        "tests/failed/batch5/poppler/poppler-102854-0.pdf",
        "tests/failed/batch5/poppler/poppler-28806-0.gz-0.pdf",
        "tests/failed/batch5/sumatrapdf/sumatrapdf-1431-0.pdf",
        "tests/failed/batch5/sumatrapdf/sumatrapdf-1432-0.pdf",
    ];

    let mut exact_matches = 0usize;
    let mut zpdf_isolated_only = 0usize;
    let mut both_rejected = 0usize;
    let mut divergences = Vec::new();

    for rel in pdfs {
        let path: PathBuf = repo_root.join(rel);
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {rel}: {e}"));
        let Ok(file) = PdfFile::parse(bytes) else {
            continue;
        };
        let ids: BTreeSet<ObjectId> = file.xref.object_ids().collect();
        for id in ids {
            let Some((raw_jbig2, globals)) = extract_jbig2_inputs(&file, id) else {
                continue;
            };
            let hayro = decode_hayro(
                &raw_jbig2,
                globals.as_deref(),
                DecodeSettings { strict: false },
            );
            let zpdf = decode_zpdf(&raw_jbig2, globals.as_deref(), &ParseLimits::default());
            match (hayro, zpdf) {
                (Ok(h), Ok(z)) => {
                    match unpack_pdf_to_native(&z, h.width, h.height, &format!("{rel} {id}")) {
                        Ok(zp) if zp == h.pixels && z == h.packed_pdf => {
                            exact_matches += 1;
                            eprintln!(
                                "[MATCH] {rel} {id}: {}x{} ({} bytes)",
                                h.width,
                                h.height,
                                z.len()
                            );
                        }
                        Ok(zp) => {
                            divergences.push(format_mismatch(&format!("{rel} {id}"), &h, &zp))
                        }
                        Err(e) => divergences.push(e),
                    }
                }
                (Ok(h), Err(ze)) => divergences.push(format!(
                    "[HAYRO-OK/ZPDF-ERR] {rel} {id}: hayro={}x{}, zpdf={ze}",
                    h.width, h.height
                )),
                (Err(he), Ok(z)) => {
                    // Malformed real-world streams (e.g. GHOSTSCRIPT-693513-4 and
                    // GHOSTSCRIPT-699694-0) where hayro aborts the page while zpdf
                    // isolates the broken segment/dictionary.
                    zpdf_isolated_only += 1;
                    eprintln!(
                        "[ZPDF-ISOLATED] {rel} {id}: hayro={he:?}, zpdf={} bytes",
                        z.len()
                    );
                }
                (Err(he), Err(ze)) => {
                    both_rejected += 1;
                    eprintln!("[BOTH-ERR] {rel} {id}: hayro={he:?}, zpdf={ze}");
                }
            }
        }
    }

    assert_eq!(
        exact_matches, 2,
        "expected both full-page scanned streams in PDFBOX-3948-0.pdf to match bit-for-bit"
    );
    assert_eq!(
        zpdf_isolated_only, 2,
        "expected GHOSTSCRIPT-693513-4 and GHOSTSCRIPT-699694-0 to isolate errors in zpdf"
    );
    assert_eq!(
        both_rejected, 1,
        "expected poppler-28806-0 to be rejected cleanly by both decoders"
    );
    assert!(
        divergences.is_empty(),
        "found {} corpus divergences:\n{}",
        divergences.len(),
        divergences.join("\n")
    );
}

// ---------------------------------------------------------------------------
// 8. Negative / adversarial tests: ParseLimits, budgets, isolation, #153 gaps
// ---------------------------------------------------------------------------

#[test]
fn zpdf_preserves_parse_limits_budgets_and_per_region_isolation() {
    let valid_bm = Bitmap::from_art(&["........", ".######.", ".#....#.", ".######.", "........"]);

    // (a) Per-region failure isolation: a broken symbol dictionary + dependent
    // text region followed by a valid generic region on the same page.
    // hayro-jbig2 fails the whole page on the bad dictionary; zpdf isolates
    // the bad dictionary/text region and decodes the valid generic region.
    let mut bad_dict = Vec::new();
    bad_dict.extend_from_slice(&0x0100u16.to_be_bytes()); // SDCTXUSED=1 (unsupported) + truncated
    bad_dict.extend_from_slice(&[0xFF; 4]);
    let mut isolated = segment(0, 48, &[], 1, &page_info_payload(8, 5, 0));
    isolated.extend_from_slice(&segment(1, 0, &[], 1, &bad_dict));
    isolated.extend_from_slice(&segment(
        2,
        6,
        &[1],
        1,
        &text_region_arith_payload(
            8,
            5,
            0,
            0,
            0,
            1,
            0,
            1,
            false,
            0,
            0,
            0,
            &[(0, 0, vec![(0, 0, 0)])],
        ),
    ));
    isolated.extend_from_slice(&segment(
        3,
        38,
        &[],
        1,
        &generic_region_payload(&valid_bm, 0, 0, 0, 0, false),
    ));

    assert!(
        decode_hayro(&isolated, None, DecodeSettings { strict: false }).is_err(),
        "hayro-jbig2 fails the whole page on a bad symbol dictionary"
    );
    let zpdf_isolated = decode_zpdf(&isolated, None, &ParseLimits::default())
        .expect("zpdf isolates the broken dictionary and preserves the valid generic region");
    let zpdf_pixels = unpack_pdf_to_native(&zpdf_isolated, 8, 5, "isolated").unwrap();
    assert_eq!(zpdf_pixels, valid_bm.data);

    // (b) ParseLimits::max_image_pixels enforcement: hayro-jbig2 decodes a
    // 100x100 page unconditionally, while zpdf enforces max_image_pixels.
    let big_page = segment(0, 48, &[], 1, &page_info_payload(100, 100, 0));
    assert!(decode_hayro(&big_page, None, DecodeSettings { strict: false }).is_ok());
    let tight_pixels = ParseLimits {
        max_image_pixels: 9_999,
        ..ParseLimits::default()
    };
    assert!(
        decode_zpdf(&big_page, None, &tight_pixels).is_err(),
        "zpdf must reject a page exceeding ParseLimits::max_image_pixels"
    );

    // (c) ParseLimits::max_decoded_stream_bytes enforcement.
    let tight_bytes = ParseLimits {
        max_decoded_stream_bytes: 100,
        ..ParseLimits::default()
    };
    assert!(
        decode_zpdf(&big_page, None, &tight_bytes).is_err(),
        "zpdf must reject output exceeding ParseLimits::max_decoded_stream_bytes"
    );

    // (d) Cumulative generic-region work budget across segments: 3 repeated
    // 8x8 generic regions (3 * (64 + 8) = 216 work > 2 * 64 = 128 budget).
    // hayro-jbig2 has no caller-configurable work cap and decodes all three;
    // zpdf stops when the cumulative pixel-work budget is exceeded.
    let small_bm = Bitmap::new(8, 8, 0);
    let region = generic_region_payload(&small_bm, 0, 0, 0, 0, false);
    let mut repeated = segment(0, 48, &[], 1, &page_info_payload(8, 8, 0));
    repeated.extend_from_slice(&segment(1, 38, &[], 1, &region));
    repeated.extend_from_slice(&segment(2, 38, &[], 1, &region));
    repeated.extend_from_slice(&segment(3, 38, &[], 1, &region));
    assert!(decode_hayro(&repeated, None, DecodeSettings { strict: false }).is_ok());
    let work_limits = ParseLimits {
        max_image_pixels: 64,
        ..ParseLimits::default()
    };
    assert!(
        decode_zpdf(&repeated, None, &work_limits).is_err(),
        "zpdf must reject cumulative region work exceeding 2 * max_image_pixels"
    );

    // (e) Oversized positive BMSIZE in a Huffman symbol dictionary (extending
    // past the segment payload, including a value large enough to overflow a
    // 32-bit/WASM bit offset): hayro-jbig2 rejects the page, while zpdf rejects
    // the dictionary cleanly and isolates the failure from valid page regions.
    let mut bad_huff_sd = Vec::new();
    bad_huff_sd.extend_from_slice(&0x0001u16.to_be_bytes());
    bad_huff_sd.extend_from_slice(&1u32.to_be_bytes());
    bad_huff_sd.extend_from_slice(&1u32.to_be_bytes());
    let mut bw = BitWriter::new();
    huff_encode(&mut bw, &std_huff_lines(4), Some(2));
    huff_encode(&mut bw, &std_huff_lines(2), Some(8));
    huff_encode(&mut bw, &std_huff_lines(2), None);
    huff_encode(&mut bw, &std_huff_lines(1), Some(0x2000_0000));
    bw.write_bytes(&[0x31, 0xF8]);
    bad_huff_sd.extend_from_slice(&bw.finish());
    let mut oversized_bmsize = segment(0, 48, &[], 1, &page_info_payload(8, 5, 0));
    oversized_bmsize.extend_from_slice(&segment(1, 0, &[], 1, &bad_huff_sd));
    oversized_bmsize.extend_from_slice(&segment(
        2,
        38,
        &[],
        1,
        &generic_region_payload(&valid_bm, 0, 0, 0, 0, false),
    ));
    assert!(
        decode_hayro(&oversized_bmsize, None, DecodeSettings { strict: false }).is_err(),
        "hayro-jbig2 must reject truncated BMSIZE collective bitmap"
    );
    let zpdf_bmsize = decode_zpdf(&oversized_bmsize, None, &ParseLimits::default())
        .expect("zpdf isolates truncated BMSIZE dictionary and preserves valid region");
    assert_eq!(
        unpack_pdf_to_native(&zpdf_bmsize, 8, 5, "oversized-bmsize").unwrap(),
        valid_bm.data
    );

    // (f) Invalid RSIZE values in a Huffman text region with SBREFINE = 1:
    // both positive RSIZE extending past the segment payload (small overrun
    // and 32-bit/WASM-overflowing 0x2000_0000) and a negative RSIZE decoded
    // from a custom type-53 Huffman table must reject the text region cleanly
    // while isolating the failure so a subsequent valid generic region renders.
    let solid_sym = Bitmap::from_art(&["###", "###", "###"]);
    let alt_sym = Bitmap::from_art(&["#.#", ".#.", "#.#"]);
    let rsize_sd = symbol_dict_payload(&[&solid_sym, &alt_sym], 0);
    let mut rsize_cx = vec![0u8; 1 << 13];
    let mut rsize_enc = MqEncoder::new();
    encode_refinement(
        &mut rsize_enc,
        &mut rsize_cx,
        &solid_sym,
        &alt_sym,
        1,
        false,
        &[(-1, -1), (-1, -1)],
        0,
        0,
    );
    let valid_r_bytes = rsize_enc.finish();
    assert!(
        valid_r_bytes.len() <= 23,
        "valid 3x3 refinement payload must fit in custom table [-8, 24)"
    );

    // Custom type-53 table for SBHUFFRSIZE: LOW = -8, HIGH = 24, four complete
    // 2-bit-prefix lines with range_len = 3 covering [-8,-1], [0,7], [8,15],
    // [16,23] (supports both negative RSIZE and the valid control length).
    let mut custom_rsize_tab = Vec::new();
    custom_rsize_tab.push((3u8 << 1) | (2u8 << 4)); // oob=0, prefix_size=4, range_size=3
    custom_rsize_tab.extend_from_slice(&(-8i32).to_be_bytes());
    custom_rsize_tab.extend_from_slice(&24i32.to_be_bytes());
    let mut cbw = BitWriter::new();
    for _ in 0..4 {
        cbw.write_bits(2, 4);
        cbw.write_bits(3, 3);
    }
    cbw.write_bits(0, 4); // lower unused
    cbw.write_bits(0, 4); // upper unused
    custom_rsize_tab.extend_from_slice(&cbw.finish());
    let custom_rsize_lines = vec![
        HuffLine::normal(2, 3, -8),
        HuffLine::normal(2, 3, 0),
        HuffLine::normal(2, 3, 8),
        HuffLine::normal(2, 3, 16),
    ];

    let fs_lines = std_huff_lines(6);
    let ds_lines = std_huff_lines(8);
    let dt_lines = std_huff_lines(11);
    let rd_lines = std_huff_lines(14);
    let b1_lines = std_huff_lines(1);

    // Builds an 8x5 Huffman text region (SBHUFF=1, SBREFINE=1, TOPLEFT,
    // SBRTEMPLATE=1) with 2 instances: instance 0 places `solid_sym` unrefined
    // at (0, 0) (which would paint (0, 0) black, unlike `valid_bm`), and
    // instance 1 refines `alt_sym` with the supplied RSIZE table/value/tail.
    let build_huff_rsize_tr = |use_custom_rsize: bool, rsize_val: i64, refine_tail: &[u8]| {
        let mut tr = region_info(8, 5, 0, 0, 0);
        let tr_flags: u16 = 0x0001 | 0x0002 | (1 << 4) | (1 << 15);
        tr.extend_from_slice(&tr_flags.to_be_bytes());
        let huff_flags: u16 = if use_custom_rsize { 1 << 14 } else { 0 };
        tr.extend_from_slice(&huff_flags.to_be_bytes());
        tr.extend_from_slice(&2u32.to_be_bytes()); // SBNUMINSTANCES = 2

        let mut tbw = BitWriter::new();
        for rc in 0..35 {
            tbw.write_bits(if rc == 1 { 1 } else { 0 }, 4);
        }
        tbw.write_bit(0); // sym 0: code len 1 (code 0)
        tbw.write_bit(0); // sym 1: code len 1 (code 1)
        tbw.byte_align();

        huff_encode(&mut tbw, &dt_lines, Some(1)); // initial STRIPT = -1
        huff_encode(&mut tbw, &dt_lines, Some(1)); // strip y = -1 + 1 = 0
        huff_encode(&mut tbw, &fs_lines, Some(0)); // x = 0
        tbw.write_bit(0); // sym 0 (solid_sym)
        tbw.write_bit(0); // RI = 0 (unrefined)
        huff_encode(&mut tbw, &ds_lines, Some(1)); // DS = 1
        tbw.write_bit(1); // sym 1 (alt_sym)
        tbw.write_bit(1); // RI = 1 (refined)
        huff_encode(&mut tbw, &rd_lines, Some(0)); // RDW
        huff_encode(&mut tbw, &rd_lines, Some(0)); // RDH
        huff_encode(&mut tbw, &rd_lines, Some(0)); // RDX
        huff_encode(&mut tbw, &rd_lines, Some(0)); // RDY
        let rsize_table = if use_custom_rsize {
            &custom_rsize_lines
        } else {
            &b1_lines
        };
        huff_encode(&mut tbw, rsize_table, Some(rsize_val));
        tbw.write_bytes(refine_tail);
        huff_encode(&mut tbw, &ds_lines, None); // OOB ends strip
        tr.extend_from_slice(&tbw.finish());
        tr
    };

    let build_rsize_page = |use_custom_rsize: bool, tr: &[u8], with_valid_generic: bool| {
        let mut page = segment(
            0,
            48,
            &[],
            1,
            &page_info_payload(8, 5, PAGE_MIGHT_CONTAIN_REFINEMENTS),
        );
        page.extend_from_slice(&segment(1, 0, &[], 1, &rsize_sd));
        let mut refs = vec![1u32];
        if use_custom_rsize {
            page.extend_from_slice(&segment(2, 53, &[], 1, &custom_rsize_tab));
            refs.push(2);
        }
        page.extend_from_slice(&segment(3, 6, &refs, 1, tr));
        if with_valid_generic {
            page.extend_from_slice(&segment(
                4,
                38,
                &[],
                1,
                &generic_region_payload(&valid_bm, 0, 0, 0, 0, false),
            ));
        }
        page
    };

    // Control: both standard B.1 and custom RSIZE tables succeed with the
    // exact refinement payload length and paint (0, 0) black, proving the
    // malformed cases below reach the RSIZE validation path itself.
    for use_custom in [false, true] {
        let valid_tr = build_huff_rsize_tr(use_custom, valid_r_bytes.len() as i64, &valid_r_bytes);
        let control = build_rsize_page(use_custom, &valid_tr, false);
        let img = assert_oracle_match(
            &format!("huffman text RSIZE control custom={use_custom}"),
            &control,
            None,
        );
        assert_eq!(img.pixels[0], 1, "control text region must paint (0, 0)");
    }

    // Malformed RSIZE cases (identical to the passing controls above except for
    // the decoded RSIZE value): positive overrun past segment end, 32-bit/WASM
    // bit-offset overflow candidate, and negative RSIZE from the custom table.
    let overrun_rsize = valid_r_bytes.len() as i64 + 16;
    let bad_rsize_cases: &[(&str, bool, i64)] = &[
        ("rsize past segment end", false, overrun_rsize),
        ("rsize 32-bit bit-offset overflow", false, 0x2000_0000),
        ("negative custom-table rsize", true, -2),
    ];
    let blank_8x5 = vec![0u8; 8 * 5];
    for &(label, use_custom, bad_rsize) in bad_rsize_cases {
        let bad_tr = build_huff_rsize_tr(use_custom, bad_rsize, &valid_r_bytes);

        let tr_only = build_rsize_page(use_custom, &bad_tr, false);
        assert!(
            std::panic::catch_unwind(|| {
                decode_hayro(&tr_only, None, DecodeSettings { strict: false })
            })
            .is_ok(),
            "{label}: hayro-jbig2 must not panic"
        );
        let z_only = decode_zpdf(&tr_only, None, &ParseLimits::default())
            .unwrap_or_else(|e| panic!("{label}: zpdf must isolate bad text region, got {e}"));
        assert_eq!(
            unpack_pdf_to_native(&z_only, 8, 5, label).unwrap(),
            blank_8x5,
            "{label}: bad Huffman text region must be rejected blank"
        );

        let with_generic = build_rsize_page(use_custom, &bad_tr, true);
        let z_isolated =
            decode_zpdf(&with_generic, None, &ParseLimits::default()).unwrap_or_else(|e| {
                panic!("{label}: zpdf must preserve subsequent valid generic region, got {e}")
            });
        assert_eq!(
            unpack_pdf_to_native(&z_isolated, 8, 5, label).unwrap(),
            valid_bm.data,
            "{label}: subsequent valid generic region must render untouched"
        );
    }

    // (g) Documented tracked gap (#153): unknown segment data length 0xFFFFFFFF
    // on an immediate generic region fails cleanly in zpdf (no panic).
    let mut unknown_len = segment(0, 48, &[], 1, &page_info_payload(8, 2, 0));
    let mut bad_seg = segment(1, 38, &[], 1, &[]);
    let n = bad_seg.len();
    bad_seg[n - 4..].copy_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
    unknown_len.extend_from_slice(&bad_seg);
    assert!(
        decode_zpdf(&unknown_len, None, &ParseLimits::default()).is_err(),
        "unknown segment data length (0xFFFFFFFF) is tracked in #153 and must return Err"
    );

    // (h) Malformed/adversarial inputs never panic in either decoder.
    let malformed_cases: &[&[u8]] = &[
        &[],
        &[0x00, 0x01, 0x02],
        &[0xFF; 32],
        // Minimized fuzz regression (type-53 custom Huffman table with LOW=i32::MIN).
        &[
            0x00, 0xFF, 0xFC, 0x05, 0x35, 0x05, 0xE1, 0xF1, 0x36, 0x00, 0x00, 0x00, 0x80, 0x00,
            0x00, 0x00, 0x80, 0x00, 0x00, 0xFF, 0x00, 0x43, 0xFF, 0xFF, 0xFF, 0x00, 0x07, 0xE8,
            0x45,
        ],
    ];
    for (idx, payload) in malformed_cases.iter().enumerate() {
        let z_res =
            std::panic::catch_unwind(|| decode_zpdf(payload, None, &ParseLimits::default()));
        assert!(z_res.is_ok(), "zpdf panicked on malformed case {idx}");
        let h_res = std::panic::catch_unwind(|| {
            decode_hayro(payload, None, DecodeSettings { strict: false })
        });
        assert!(
            h_res.is_ok(),
            "hayro-jbig2 panicked on malformed case {idx}"
        );
    }
}
