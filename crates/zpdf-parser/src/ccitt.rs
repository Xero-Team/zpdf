//! CCITTFaxDecode — ITU-T T.4 (Group 3) and T.6 (Group 4) fax decoding.
//!
//! Pure Rust, zero deps. Decodes the bi-level fax stream into packed 1-bit rows
//! (MSB-first, `ceil(columns/8)` bytes per row) following the PDF sample
//! convention controlled by `/BlackIs1`: by default (false) a **black** pixel is
//! sample value 0 and **white** is 1.
//!
//! Supports `/K < 0` (pure 2-D, Group 4), `/K == 0` (pure 1-D, Group 3), and
//! `/K > 0` (mixed 1-D/2-D, Group 3) with `/EncodedByteAlign`. EOL codes are
//! tolerated and skipped.

use zpdf_core::{Error, PdfDict, Result};

/// Parsed `/DecodeParms` for a CCITTFaxDecode filter.
pub struct CcittParams {
    pub k: i64,
    pub columns: usize,
    pub rows: usize, // 0 = unknown → decode until input is exhausted
    pub black_is_1: bool,
    pub byte_align: bool,
}

impl CcittParams {
    pub fn from_dict(params: Option<&PdfDict>) -> Self {
        let g =
            |key: &str, default: i64| params.and_then(|p| p.get_i64(key).ok()).unwrap_or(default);
        let b = |key: &str| {
            params
                .and_then(|p| match p.get(key) {
                    Some(zpdf_core::PdfObject::Bool(v)) => Some(*v),
                    _ => None,
                })
                .unwrap_or(false)
        };
        CcittParams {
            k: g("K", 0),
            columns: g("Columns", 1728).max(1) as usize,
            rows: g("Rows", 0).max(0) as usize,
            black_is_1: b("BlackIs1"),
            byte_align: b("EncodedByteAlign"),
        }
    }
}

/// Decode a CCITTFax stream into packed 1-bpp rows.
pub fn decode(data: &[u8], params: &CcittParams) -> Result<Vec<u8>> {
    let columns = params.columns;
    if columns == 0 || columns > 1 << 20 {
        return Err(Error::StreamDecode(format!(
            "CCITTFaxDecode: implausible /Columns {columns}"
        )));
    }
    let row_bytes = columns.div_ceil(8);

    // Output-size safety cap (matches ParseLimits::max_stream_bytes default and
    // the sibling LZW decoder). A single 1-bit vertical-mode code can emit a
    // full row, so a tiny crafted stream with huge /Columns or /Rows would
    // otherwise expand to gigabytes — bound both the row count and the buffer.
    const MAX_OUTPUT: usize = 256 * 1024 * 1024;
    let row_cap = (MAX_OUTPUT / row_bytes.max(1)).max(1);
    // Honour an explicit /Rows; otherwise bound by the input size (each decoded
    // row consumes at least one bit). Either way, clamp to the output cap.
    let declared = if params.rows > 0 {
        params.rows
    } else {
        data.len().saturating_mul(8).max(1)
    };
    let max_rows = declared.min(row_cap);

    let mut reader = BitReader::new(data);
    let mut out: Vec<u8> = Vec::new();
    // The reference line above row 0 is imaginary all-white: no changing
    // elements, so both b1/b2 lookups fall through to `columns`.
    let mut reference: Vec<usize> = vec![columns, columns];

    let two_d = params.k < 0;

    let mut row_index = 0;
    while row_index < max_rows {
        if reader.is_exhausted() {
            break;
        }

        // Group 3 framing: tolerate an EOL, and (for K>0) read the 1-D/2-D tag.
        let mut line_is_2d = two_d;
        if params.k >= 0 {
            reader.skip_eol();
            if reader.is_exhausted() {
                break;
            }
            if params.k > 0 {
                // 1 → 1-D line, 0 → 2-D line.
                match reader.read_bit() {
                    Some(0) => line_is_2d = true,
                    Some(_) => line_is_2d = false,
                    None => break,
                }
            }
        }

        let changes = if line_is_2d {
            decode_row_2d(&mut reader, &reference, columns)
        } else {
            decode_row_1d(&mut reader, columns)
        };

        let changes = match changes {
            Some(c) => c,
            None => break, // ran out of bits mid-row
        };

        // Defence in depth against a decompression bomb (max_rows already
        // bounds this, but guard the buffer explicitly).
        if out.len().saturating_add(row_bytes) > MAX_OUTPUT {
            break;
        }

        // Pack the row: starts white, flips colour at each changing element.
        let mut row = vec![0u8; row_bytes];
        emit_row(&changes, columns, params.black_is_1, &mut row);
        out.extend_from_slice(&row);

        // This line becomes the reference for the next.
        reference = changes;
        reference.push(columns);
        reference.push(columns);

        row_index += 1;

        if params.byte_align {
            reader.align_to_byte();
        }
    }

    Ok(out)
}

/// Pack a run-length row (transition positions) into MSB-first 1-bpp samples.
/// `changes` holds ascending transition columns; the row begins white.
fn emit_row(changes: &[usize], columns: usize, black_is_1: bool, row: &mut [u8]) {
    // sample bit for a white / black pixel under the BlackIs1 convention.
    let (white_bit, black_bit) = if black_is_1 { (0u8, 1u8) } else { (1u8, 0u8) };
    let mut color_black = false;
    let mut x = 0usize;
    for &change in changes {
        let end = change.min(columns);
        let bit = if color_black { black_bit } else { white_bit };
        if bit == 1 {
            for col in x..end {
                row[col / 8] |= 0x80 >> (col % 8);
            }
        }
        x = end;
        color_black = !color_black;
        if x >= columns {
            break;
        }
    }
    // Trailing run to the row end keeps the last colour.
    if x < columns {
        let bit = if color_black { black_bit } else { white_bit };
        if bit == 1 {
            for col in x..columns {
                row[col / 8] |= 0x80 >> (col % 8);
            }
        }
    }
}

/// Decode one 1-D (Modified Huffman) coded row → transition columns.
fn decode_row_1d(reader: &mut BitReader, columns: usize) -> Option<Vec<usize>> {
    let mut changes = Vec::new();
    let mut a0 = 0usize;
    let mut white = true;
    while a0 < columns {
        let run = read_run(reader, white)?;
        a0 = (a0 + run).min(columns);
        changes.push(a0);
        white = !white;
    }
    Some(changes)
}

/// Decode one 2-D (READ) coded row against the reference line → transition cols.
fn decode_row_2d(
    reader: &mut BitReader,
    reference: &[usize],
    columns: usize,
) -> Option<Vec<usize>> {
    let mut changes = Vec::new();
    let mut a0: i64 = -1;
    let mut color_white = true; // current coding colour

    while a0 < columns as i64 {
        let (b1, b2) = find_b1_b2(reference, a0, color_white, columns);

        match read_mode(reader)? {
            Mode::Pass => {
                // Run from a0 to b2 stays the current colour; no transition.
                a0 = b2 as i64;
            }
            Mode::Horizontal => {
                let start = if a0 < 0 { 0 } else { a0 as usize };
                let run1 = read_run(reader, color_white)?;
                let run2 = read_run(reader, !color_white)?;
                let a1 = (start + run1).min(columns);
                let a2 = (a1 + run2).min(columns);
                changes.push(a1);
                changes.push(a2);
                a0 = a2 as i64;
                // colour unchanged (two runs return to the same colour)
            }
            Mode::Vertical(delta) => {
                let a1 = (b1 as i64 + delta as i64).clamp(0, columns as i64) as usize;
                changes.push(a1);
                a0 = a1 as i64;
                color_white = !color_white;
            }
        }

        if changes.len() > columns + 2 {
            // Malformed: more transitions than pixels.
            break;
        }
    }
    Some(changes)
}

/// b1 = first changing element on the reference line right of a0 with colour
/// opposite to the current coding colour; b2 = the element after b1.
/// Reference elements alternate colour starting black (index 0 = white→black).
fn find_b1_b2(reference: &[usize], a0: i64, color_white: bool, columns: usize) -> (usize, usize) {
    // Colour of reference changing element i: black when i even, white when odd.
    // We want b1's colour opposite to a0's colour.
    let want_black = color_white; // opposite of white is black
    let mut i = 0;
    while i < reference.len() && (reference[i] as i64) <= a0 {
        i += 1;
    }
    // Fix parity so reference[i] has the wanted colour.
    let is_black = |idx: usize| idx.is_multiple_of(2);
    if i < reference.len() && is_black(i) != want_black {
        i += 1;
    }
    let b1 = reference.get(i).copied().unwrap_or(columns);
    let b2 = reference.get(i + 1).copied().unwrap_or(columns);
    (b1.min(columns), b2.min(columns))
}

/// Read a full run length: zero or more make-up codes (≥64) then one
/// terminating code (0..=63).
fn read_run(reader: &mut BitReader, white: bool) -> Option<usize> {
    let mut total = 0usize;
    loop {
        let table = if white { &WHITE_CODES } else { &BLACK_CODES };
        let run = match_code(reader, table)?;
        total += run as usize;
        if run < 64 {
            return Some(total);
        }
        // make-up code → continue reading; extended make-up (≥1792) is shared.
    }
}

#[derive(Clone, Copy)]
enum Mode {
    Pass,
    Horizontal,
    Vertical(i8),
}

/// Read a 2-D mode code (T.6 Table 1).
fn read_mode(reader: &mut BitReader) -> Option<Mode> {
    // Codes (MSB-first):
    // V0   = 1
    // VR1  = 011   VL1 = 010
    // H    = 001
    // Pass = 0001
    // VR2  = 000011 VL2 = 000010
    // VR3  = 0000011 VL3 = 0000010
    if reader.read_bit()? == 1 {
        return Some(Mode::Vertical(0));
    }
    // 0…
    if reader.read_bit()? == 1 {
        // 01x → VR1 / VL1
        return Some(if reader.read_bit()? == 1 {
            Mode::Vertical(1)
        } else {
            Mode::Vertical(-1)
        });
    }
    // 00…
    if reader.read_bit()? == 1 {
        // 001 → Horizontal
        return Some(Mode::Horizontal);
    }
    // 000…
    if reader.read_bit()? == 1 {
        // 0001 → Pass
        return Some(Mode::Pass);
    }
    // 0000…
    if reader.read_bit()? == 1 {
        // 00001x → VR2 / VL2
        return Some(if reader.read_bit()? == 1 {
            Mode::Vertical(2)
        } else {
            Mode::Vertical(-2)
        });
    }
    // 00000…
    if reader.read_bit()? == 1 {
        // 000001x → VR3 / VL3
        return Some(if reader.read_bit()? == 1 {
            Mode::Vertical(3)
        } else {
            Mode::Vertical(-3)
        });
    }
    // Anything longer (EOL / extensions) is not a valid in-row mode here.
    None
}

/// Match the next bits against a prefix-free code table; returns the run value.
fn match_code(reader: &mut BitReader, table: &[(u8, u16, u16)]) -> Option<u16> {
    let mut code: u16 = 0;
    let mut len: u8 = 0;
    while len < 14 {
        code = (code << 1) | reader.read_bit()? as u16;
        len += 1;
        for &(clen, cbits, run) in table {
            if clen == len && cbits == code {
                return Some(run);
            }
        }
    }
    None
}

/// MSB-first bit reader over a byte slice.
struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit: 0 }
    }

    #[inline]
    fn read_bit(&mut self) -> Option<u8> {
        let byte = self.bit / 8;
        if byte >= self.data.len() {
            return None;
        }
        let shift = 7 - (self.bit % 8);
        self.bit += 1;
        Some((self.data[byte] >> shift) & 1)
    }

    fn align_to_byte(&mut self) {
        if !self.bit.is_multiple_of(8) {
            self.bit = (self.bit / 8 + 1) * 8;
        }
    }

    fn is_exhausted(&self) -> bool {
        self.bit / 8 >= self.data.len()
    }

    /// Skip a single EOL code (000000000001) if present at the cursor, without
    /// consuming anything otherwise.
    fn skip_eol(&mut self) {
        let save = self.bit;
        // Up to a fill of zero bits then 000000000001.
        let mut zeros = 0;
        loop {
            match self.read_bit() {
                Some(0) => {
                    zeros += 1;
                    if zeros > 64 {
                        self.bit = save;
                        return;
                    }
                }
                Some(_) => {
                    if zeros >= 11 {
                        return; // consumed the EOL (11+ zeros then a 1)
                    }
                    self.bit = save;
                    return;
                }
                None => {
                    self.bit = save;
                    return;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ITU-T T.4 run-length code tables: (bit_length, code, run_length).
// White and black terminating + make-up codes, plus the shared extended
// make-up codes (1792..=2560).
// ---------------------------------------------------------------------------

#[rustfmt::skip]
const EXT_MAKEUP: [(u8, u16, u16); 13] = [
    (11, 0b00000001000, 1792), (11, 0b00000001100, 1856), (11, 0b00000001101, 1920),
    (12, 0b000000010010, 1984), (12, 0b000000010011, 2048), (12, 0b000000010100, 2112),
    (12, 0b000000010101, 2176), (12, 0b000000010110, 2240), (12, 0b000000010111, 2304),
    (12, 0b000000011100, 2368), (12, 0b000000011101, 2432), (12, 0b000000011110, 2496),
    (12, 0b000000011111, 2560),
];

#[rustfmt::skip]
const WHITE_CODES: [(u8, u16, u16); 64 + 27 + 13] = [
    // Terminating codes 0..=63
    (8,0b00110101,0),(6,0b000111,1),(4,0b0111,2),(4,0b1000,3),(4,0b1011,4),(4,0b1100,5),
    (4,0b1110,6),(4,0b1111,7),(5,0b10011,8),(5,0b10100,9),(5,0b00111,10),(5,0b01000,11),
    (6,0b001000,12),(6,0b000011,13),(6,0b110100,14),(6,0b110101,15),(6,0b101010,16),(6,0b101011,17),
    (7,0b0100111,18),(7,0b0001100,19),(7,0b0001000,20),(7,0b0010111,21),(7,0b0000011,22),(7,0b0000100,23),
    (7,0b0101000,24),(7,0b0101011,25),(7,0b0010011,26),(7,0b0100100,27),(7,0b0011000,28),(8,0b00000010,29),
    (8,0b00000011,30),(8,0b00011010,31),(8,0b00011011,32),(8,0b00010010,33),(8,0b00010011,34),(8,0b00010100,35),
    (8,0b00010101,36),(8,0b00010110,37),(8,0b00010111,38),(8,0b00101000,39),(8,0b00101001,40),(8,0b00101010,41),
    (8,0b00101011,42),(8,0b00101100,43),(8,0b00101101,44),(8,0b00000100,45),(8,0b00000101,46),(8,0b00001010,47),
    (8,0b00001011,48),(8,0b01010010,49),(8,0b01010011,50),(8,0b01010100,51),(8,0b01010101,52),(8,0b00100100,53),
    (8,0b00100101,54),(8,0b01011000,55),(8,0b01011001,56),(8,0b01011010,57),(8,0b01011011,58),(8,0b01001010,59),
    (8,0b01001011,60),(8,0b00110010,61),(8,0b00110011,62),(8,0b00110100,63),
    // Make-up codes 64..=1728
    (5,0b11011,64),(5,0b10010,128),(6,0b010111,192),(7,0b0110111,256),(8,0b00110110,320),(8,0b00110111,384),
    (8,0b01100100,448),(8,0b01100101,512),(8,0b01101000,576),(8,0b01100111,640),(9,0b011001100,704),(9,0b011001101,768),
    (9,0b011010010,832),(9,0b011010011,896),(9,0b011010100,960),(9,0b011010101,1024),(9,0b011010110,1088),(9,0b011010111,1152),
    (9,0b011011000,1216),(9,0b011011001,1280),(9,0b011011010,1344),(9,0b011011011,1408),(9,0b010011000,1472),(9,0b010011001,1536),
    (9,0b010011010,1600),(6,0b011000,1664),(9,0b010011011,1728),
    // Shared extended make-up 1792..=2560
    EXT_MAKEUP[0],EXT_MAKEUP[1],EXT_MAKEUP[2],EXT_MAKEUP[3],EXT_MAKEUP[4],EXT_MAKEUP[5],EXT_MAKEUP[6],
    EXT_MAKEUP[7],EXT_MAKEUP[8],EXT_MAKEUP[9],EXT_MAKEUP[10],EXT_MAKEUP[11],EXT_MAKEUP[12],
];

#[rustfmt::skip]
const BLACK_CODES: [(u8, u16, u16); 64 + 27 + 13] = [
    // Terminating codes 0..=63
    (10,0b0000110111,0),(3,0b010,1),(2,0b11,2),(2,0b10,3),(3,0b011,4),(4,0b0011,5),
    (4,0b0010,6),(5,0b00011,7),(6,0b000101,8),(6,0b000100,9),(7,0b0000100,10),(7,0b0000101,11),
    (7,0b0000111,12),(8,0b00000100,13),(8,0b00000111,14),(9,0b000011000,15),(10,0b0000010111,16),(10,0b0000011000,17),
    (10,0b0000001000,18),(11,0b00001100111,19),(11,0b00001101000,20),(11,0b00001101100,21),(11,0b00000110111,22),(11,0b00000101000,23),
    (11,0b00000010111,24),(11,0b00000011000,25),(12,0b000011001010,26),(12,0b000011001011,27),(12,0b000011001100,28),(12,0b000011001101,29),
    (12,0b000001101000,30),(12,0b000001101001,31),(12,0b000001101010,32),(12,0b000001101011,33),(12,0b000011010010,34),(12,0b000011010011,35),
    (12,0b000011010100,36),(12,0b000011010101,37),(12,0b000011010110,38),(12,0b000011010111,39),(12,0b000001101100,40),(12,0b000001101101,41),
    (12,0b000011011010,42),(12,0b000011011011,43),(12,0b000001010100,44),(12,0b000001010101,45),(12,0b000001010110,46),(12,0b000001010111,47),
    (12,0b000001100100,48),(12,0b000001100101,49),(12,0b000001010010,50),(12,0b000001010011,51),(12,0b000000100100,52),(12,0b000000110111,53),
    (12,0b000000111000,54),(12,0b000000100111,55),(12,0b000000101000,56),(12,0b000001011000,57),(12,0b000001011001,58),(12,0b000000101011,59),
    (12,0b000000101100,60),(12,0b000001011010,61),(12,0b000001100110,62),(12,0b000001100111,63),
    // Make-up codes 64..=1728
    (10,0b0000001111,64),(12,0b000011001000,128),(12,0b000011001001,192),(12,0b000001011011,256),(12,0b000000110011,320),(12,0b000000110100,384),
    (12,0b000000110101,448),(13,0b0000001101100,512),(13,0b0000001101101,576),(13,0b0000001001010,640),(13,0b0000001001011,704),(13,0b0000001001100,768),
    (13,0b0000001001101,832),(13,0b0000001110010,896),(13,0b0000001110011,960),(13,0b0000001110100,1024),(13,0b0000001110101,1088),(13,0b0000001110110,1152),
    (13,0b0000001110111,1216),(13,0b0000001010010,1280),(13,0b0000001010011,1344),(13,0b0000001010100,1408),(13,0b0000001010101,1472),(13,0b0000001011010,1536),
    (13,0b0000001011011,1600),(13,0b0000001100100,1664),(13,0b0000001100101,1728),
    // Shared extended make-up 1792..=2560
    EXT_MAKEUP[0],EXT_MAKEUP[1],EXT_MAKEUP[2],EXT_MAKEUP[3],EXT_MAKEUP[4],EXT_MAKEUP[5],EXT_MAKEUP[6],
    EXT_MAKEUP[7],EXT_MAKEUP[8],EXT_MAKEUP[9],EXT_MAKEUP[10],EXT_MAKEUP[11],EXT_MAKEUP[12],
];

#[cfg(test)]
mod tests {
    use super::*;

    /// A single all-white 8-pixel row, 1-D coded: white run 8 (10011) then black
    /// run 0 (0000110111).
    #[test]
    fn one_d_simple_row() {
        let bits = "100110000110111";
        let data = bits_to_bytes(bits);
        let p = CcittParams {
            k: 0,
            columns: 8,
            rows: 1,
            black_is_1: false,
            byte_align: false,
        };
        let out = decode(&data, &p).unwrap();
        // 8 white pixels, BlackIs1 false → white = sample 1 → 0xFF
        assert_eq!(out, vec![0xFF]);
    }

    /// 1-D row: 3 white, 2 black, 3 white (columns = 8).
    #[test]
    fn one_d_mixed_row() {
        // white 3 = 1000, black 2 = 11, white 3 = 1000
        let bits = "1000111000";
        let data = bits_to_bytes(bits);
        let p = CcittParams {
            k: 0,
            columns: 8,
            rows: 1,
            black_is_1: false,
            byte_align: false,
        };
        let out = decode(&data, &p).unwrap();
        // pixels WWWBBWWW, white=1 black=0 → 1110 0111 = 0xE7
        assert_eq!(out, vec![0b11100111]);
    }

    /// Same row with BlackIs1 = true inverts the sample bits.
    #[test]
    fn black_is_1_inverts() {
        let bits = "1000111000";
        let data = bits_to_bytes(bits);
        let p = CcittParams {
            k: 0,
            columns: 8,
            rows: 1,
            black_is_1: true,
            byte_align: false,
        };
        let out = decode(&data, &p).unwrap();
        // WWWBBWWW, white=0 black=1 → 0001 1000 = 0x18
        assert_eq!(out, vec![0b00011000]);
    }

    /// A crafted stream that would expand without bound (huge declared /Rows,
    /// 1 bit per row) must terminate, bounded by the available input bits —
    /// the decompression-bomb guard.
    #[test]
    fn rows_bounded_by_input() {
        // 4 bytes = 32 bits; under K=-1 each 0xFF bit is a V0 row at columns=8.
        let data = vec![0xFFu8; 4];
        let p = CcittParams {
            k: -1,
            columns: 8,
            rows: 1_000_000_000,
            black_is_1: false,
            byte_align: false,
        };
        let out = decode(&data, &p).unwrap();
        assert!(
            out.len() <= 32,
            "output must stay bounded by input bits, got {}",
            out.len()
        );
    }

    fn bits_to_bytes(bits: &str) -> Vec<u8> {
        let mut out = Vec::new();
        let mut cur = 0u8;
        let mut n = 0;
        for ch in bits.chars() {
            cur = (cur << 1) | if ch == '1' { 1 } else { 0 };
            n += 1;
            if n == 8 {
                out.push(cur);
                cur = 0;
                n = 0;
            }
        }
        if n > 0 {
            out.push(cur << (8 - n));
        }
        out
    }
}
