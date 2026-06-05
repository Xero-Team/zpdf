use zpdf_core::{Error, PdfDict, PdfName, PdfObject, Result};

pub fn decode_stream(data: &[u8], dict: &PdfDict) -> Result<Vec<u8>> {
    let filters = match dict.get("Filter") {
        Some(PdfObject::Name(n)) => vec![n.clone()],
        Some(PdfObject::Array(arr)) => arr
            .iter()
            .map(|obj| match obj {
                PdfObject::Name(n) => Ok(n.clone()),
                _ => Err(Error::TypeMismatch {
                    expected: "Name",
                    actual: obj.type_name(),
                }),
            })
            .collect::<Result<Vec<_>>>()?,
        Some(_) => {
            return Err(Error::TypeMismatch {
                expected: "Name or Array",
                actual: "other",
            })
        }
        None => return Ok(data.to_vec()),
    };

    let decode_parms = extract_decode_parms(dict, filters.len());

    let mut result = data.to_vec();
    for (i, filter) in filters.iter().enumerate() {
        let params = decode_parms[i].as_ref();
        result = apply_filter(filter, &result, params)?;
        if let Some(p) = params {
            result = apply_predictor(&result, p)?;
        }
    }
    Ok(result)
}

fn extract_decode_parms(dict: &PdfDict, filter_count: usize) -> Vec<Option<PdfDict>> {
    match dict.get("DecodeParms").or_else(|| dict.get("DP")) {
        Some(PdfObject::Dict(d)) => {
            let mut v = vec![None; filter_count];
            if !v.is_empty() {
                v[0] = Some(d.clone());
            }
            v
        }
        Some(PdfObject::Array(arr)) => arr
            .iter()
            .map(|obj| match obj {
                PdfObject::Dict(d) => Some(d.clone()),
                _ => None,
            })
            .chain(std::iter::repeat(None))
            .take(filter_count)
            .collect(),
        _ => vec![None; filter_count],
    }
}

fn apply_predictor(data: &[u8], params: &PdfDict) -> Result<Vec<u8>> {
    let predictor = params.get_i64("Predictor").unwrap_or(1) as u32;
    if predictor == 1 {
        return Ok(data.to_vec());
    }

    let colors = params.get_i64("Colors").unwrap_or(1).max(1) as usize;
    let bpc = params.get_i64("BitsPerComponent").unwrap_or(8).max(1) as usize;
    let columns = params.get_i64("Columns").unwrap_or(1).max(1) as usize;

    if predictor == 2 {
        decode_tiff_predictor(data, colors, bpc, columns)
    } else if predictor >= 10 {
        decode_png_predictor(data, colors, bpc, columns)
    } else {
        Ok(data.to_vec())
    }
}

fn decode_tiff_predictor(
    data: &[u8],
    colors: usize,
    bpc: usize,
    columns: usize,
) -> Result<Vec<u8>> {
    if bpc != 8 {
        return Ok(data.to_vec());
    }
    let row_bytes = columns * colors;
    let mut output = data.to_vec();
    for row_start in (0..output.len()).step_by(row_bytes) {
        let row_end = (row_start + row_bytes).min(output.len());
        for i in (row_start + colors)..row_end {
            output[i] = output[i].wrapping_add(output[i - colors]);
        }
    }
    Ok(output)
}

fn decode_png_predictor(data: &[u8], colors: usize, bpc: usize, columns: usize) -> Result<Vec<u8>> {
    let row_bytes = (colors * bpc * columns).div_ceil(8);
    let bpp = (colors * bpc).div_ceil(8); // bytes per pixel for Sub/Paeth
    let stride = 1 + row_bytes; // filter byte + row data

    if !data.len().is_multiple_of(stride) && !data.is_empty() {
        // Try to process what we can
        tracing::debug!(
            "PNG predictor: data length {} not multiple of stride {stride}",
            data.len()
        );
    }

    let num_rows = data.len().div_ceil(stride);
    let mut output = Vec::with_capacity(num_rows * row_bytes);
    let mut prev_row = vec![0u8; row_bytes];

    let mut pos = 0;
    while pos < data.len() {
        if pos >= data.len() {
            break;
        }
        let filter_type = data[pos];
        pos += 1;

        let available = (data.len() - pos).min(row_bytes);
        let cur = &data[pos..pos + available];

        let mut row = vec![0u8; row_bytes];
        row[..available].copy_from_slice(cur);

        match filter_type {
            0 => {} // None
            1 => {
                // Sub
                for i in bpp..row_bytes {
                    row[i] = row[i].wrapping_add(row[i - bpp]);
                }
            }
            2 => {
                // Up
                for i in 0..row_bytes {
                    row[i] = row[i].wrapping_add(prev_row[i]);
                }
            }
            3 => {
                // Average
                for i in 0..row_bytes {
                    let left = if i >= bpp { row[i - bpp] as u16 } else { 0 };
                    let above = prev_row[i] as u16;
                    row[i] = row[i].wrapping_add(((left + above) / 2) as u8);
                }
            }
            4 => {
                // Paeth
                for i in 0..row_bytes {
                    let left = if i >= bpp { row[i - bpp] as i32 } else { 0 };
                    let above = prev_row[i] as i32;
                    let upper_left = if i >= bpp {
                        prev_row[i - bpp] as i32
                    } else {
                        0
                    };
                    row[i] = row[i].wrapping_add(paeth(left, above, upper_left));
                }
            }
            _ => {
                tracing::debug!("PNG predictor: unknown filter type {filter_type}");
            }
        }

        output.extend_from_slice(&row);
        prev_row.copy_from_slice(&row);
        pos += available;
    }

    Ok(output)
}

fn paeth(a: i32, b: i32, c: i32) -> u8 {
    let p = a + b - c;
    let pa = (p - a).abs();
    let pb = (p - b).abs();
    let pc = (p - c).abs();
    if pa <= pb && pa <= pc {
        a as u8
    } else if pb <= pc {
        b as u8
    } else {
        c as u8
    }
}

fn apply_filter(filter: &PdfName, data: &[u8], params: Option<&PdfDict>) -> Result<Vec<u8>> {
    match filter.as_str() {
        "FlateDecode" | "Fl" => decode_flate(data),
        "LZWDecode" | "LZW" => {
            // EarlyChange lives in DecodeParms; default 1 per ISO 32000.
            let early_change = params
                .and_then(|p| p.get_i64("EarlyChange").ok())
                .unwrap_or(1);
            lzw_decode(data, early_change)
        }
        "ASCIIHexDecode" | "AHx" => decode_ascii_hex(data),
        "ASCII85Decode" | "A85" => decode_ascii85(data),
        "RunLengthDecode" | "RL" => decode_run_length(data),
        "DCTDecode" | "DCT" => decode_dct(data),
        other => Err(Error::UnsupportedFilter(other.to_string())),
    }
}

/// PDF/TIFF variable-width LZW decoder (ISO 32000-1, 7.4.4.2).
///
/// 8-bit input symbols; code width starts at 9 and grows to a max of 12.
/// Code 256 = ClearTable (reset dictionary, width back to 9),
/// code 257 = EOD. Codes 258+ are dictionary strings. `early_change` is the
/// DecodeParms EarlyChange value (default 1); when 1 the code width is increased
/// one code earlier than the natural boundary.
fn lzw_decode(data: &[u8], early_change: i64) -> Result<Vec<u8>> {
    const CLEAR: u32 = 256;
    const EOD: u32 = 257;
    // Output-size safety cap (matches ParseLimits::max_stream_bytes default).
    const MAX_OUTPUT: usize = 256 * 1024 * 1024;

    // EarlyChange is effectively a flag: any nonzero -> 1, explicit 0 -> 0.
    let early: u32 = if early_change == 0 { 0 } else { 1 };

    // Dictionary: index = code, value = decoded byte string. Slots 0..=255 are
    // single bytes; 256/257 are placeholders so the first dynamic code is 258.
    let mut table: Vec<Vec<u8>> = Vec::with_capacity(4096);
    let reset = |t: &mut Vec<Vec<u8>>| {
        t.clear();
        for i in 0..256u32 {
            t.push(vec![i as u8]);
        }
        t.push(Vec::new()); // 256 CLEAR (unused as a string)
        t.push(Vec::new()); // 257 EOD   (unused as a string)
    };
    reset(&mut table);

    let mut width: u32 = 9;
    let mut bit_pos: usize = 0;
    let total_bits = data.len() * 8;

    // MSB-first reader; returns None when fewer than `width` bits remain.
    let read_code = |bit_pos: &mut usize, width: u32| -> Option<u32> {
        if *bit_pos + width as usize > total_bits {
            return None;
        }
        let mut code: u32 = 0;
        for _ in 0..width {
            let byte = data[*bit_pos / 8];
            let bit = (byte >> (7 - (*bit_pos % 8))) & 1;
            code = (code << 1) | bit as u32;
            *bit_pos += 1;
        }
        Some(code)
    };

    let mut out: Vec<u8> = Vec::new();
    let mut prev: Option<u32> = None;

    // Stop when input is exhausted (some streams omit the EOD marker).
    while let Some(code) = read_code(&mut bit_pos, width) {
        if code == EOD {
            break;
        }
        if code == CLEAR {
            reset(&mut table);
            width = 9;
            prev = None;
            continue;
        }

        // Resolve the output string for this code.
        let entry: Vec<u8> = if (code as usize) < table.len() {
            table[code as usize].clone()
        } else if code as usize == table.len() {
            // KwKwK: code refers to the entry we are about to define.
            match prev {
                Some(p) => {
                    let mut e = table[p as usize].clone();
                    e.push(table[p as usize][0]);
                    e
                }
                None => {
                    return Err(Error::StreamDecode(format!(
                        "LZWDecode: code {code} before any literal"
                    )))
                }
            }
        } else {
            return Err(Error::StreamDecode(format!(
                "LZWDecode: invalid code {code} (table size {})",
                table.len()
            )));
        };

        out.extend_from_slice(&entry);
        if out.len() > MAX_OUTPUT {
            return Err(Error::StreamDecode(
                "LZWDecode: output exceeds limit".into(),
            ));
        }

        // Add new dictionary entry = previous string + first byte of this entry.
        // (Skipped for the first code after a clear, when prev is None.)
        if let Some(p) = prev {
            let mut new_entry = table[p as usize].clone();
            new_entry.push(entry[0]);
            table.push(new_entry);
        }
        prev = Some(code);

        // Width growth. After the push above, `table.len()` is the index that
        // will be assigned to the NEXT dictionary entry, which is exactly the
        // value to test against the current width's capacity. EarlyChange (=1)
        // bumps the width one code earlier. Grow when `table.len() + early >=
        // 2^width`. (Validated against weezl/TIFF LZW across the 9->10->11->12
        // and 4096 boundaries; an earlier `+ 1` here desynced real streams.)
        let next_code = table.len() as u32;
        if width < 12 && next_code + early >= (1u32 << width) {
            width += 1;
        }
    }

    Ok(out)
}

fn decode_flate(data: &[u8]) -> Result<Vec<u8>> {
    use flate2::read::ZlibDecoder;
    use std::io::Read;

    let mut decoder = ZlibDecoder::new(data);
    let mut output = Vec::new();
    decoder
        .read_to_end(&mut output)
        .map_err(|e| Error::StreamDecode(format!("FlateDecode: {e}")))?;
    Ok(output)
}

fn decode_ascii_hex(data: &[u8]) -> Result<Vec<u8>> {
    let mut output = Vec::with_capacity(data.len() / 2);
    let mut high: Option<u8> = None;

    for &b in data {
        if b == b'>' {
            break;
        }
        if b.is_ascii_whitespace() {
            continue;
        }
        let nibble = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => {
                return Err(Error::StreamDecode(format!(
                    "ASCIIHexDecode: invalid byte 0x{b:02x}"
                )))
            }
        };

        match high {
            None => high = Some(nibble),
            Some(h) => {
                output.push((h << 4) | nibble);
                high = None;
            }
        }
    }

    if let Some(h) = high {
        output.push(h << 4);
    }

    Ok(output)
}

fn decode_ascii85(data: &[u8]) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut tuple: u32 = 0;
    let mut count = 0;

    let data = if data.ends_with(b"~>") {
        &data[..data.len() - 2]
    } else {
        data
    };

    for &b in data {
        if b.is_ascii_whitespace() {
            continue;
        }

        if b == b'z' && count == 0 {
            output.extend_from_slice(&[0, 0, 0, 0]);
            continue;
        }

        if !(b'!'..=b'u').contains(&b) {
            return Err(Error::StreamDecode(format!(
                "ASCII85Decode: invalid byte 0x{b:02x}"
            )));
        }

        tuple = tuple * 85 + (b - b'!') as u32;
        count += 1;

        if count == 5 {
            output.push((tuple >> 24) as u8);
            output.push((tuple >> 16) as u8);
            output.push((tuple >> 8) as u8);
            output.push(tuple as u8);
            tuple = 0;
            count = 0;
        }
    }

    // Handle remaining bytes
    if count > 1 {
        for _ in count..5 {
            tuple = tuple * 85 + 84; // pad with 'u'
        }
        for i in 0..(count - 1) {
            output.push((tuple >> (24 - i * 8)) as u8);
        }
    }

    Ok(output)
}

fn decode_dct(data: &[u8]) -> Result<Vec<u8>> {
    use zune_jpeg::JpegDecoder;
    let mut decoder = JpegDecoder::new(std::io::Cursor::new(data));
    decoder
        .decode()
        .map_err(|e| Error::StreamDecode(format!("DCTDecode: {e}")))
}

fn decode_run_length(data: &[u8]) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut i = 0;

    while i < data.len() {
        let length_byte = data[i];
        i += 1;

        if length_byte == 128 {
            break; // EOD
        } else if length_byte < 128 {
            // Copy next (length_byte + 1) bytes literally
            let count = length_byte as usize + 1;
            if i + count > data.len() {
                return Err(Error::StreamDecode("RunLengthDecode: truncated".into()));
            }
            output.extend_from_slice(&data[i..i + count]);
            i += count;
        } else {
            // Repeat next byte (257 - length_byte) times
            let count = 257 - length_byte as usize;
            if i >= data.len() {
                return Err(Error::StreamDecode("RunLengthDecode: truncated".into()));
            }
            let byte = data[i];
            i += 1;
            output.resize(output.len() + count, byte);
        }
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flate_roundtrip() {
        use flate2::write::ZlibEncoder;
        use flate2::Compression;
        use std::io::Write;

        let original = b"Hello, zpdf! This is a test of FlateDecode.";
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(original).unwrap();
        let compressed = encoder.finish().unwrap();

        let decoded = decode_flate(&compressed).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn ascii_hex() {
        let decoded = decode_ascii_hex(b"48 65 6C 6C 6F>").unwrap();
        assert_eq!(decoded, b"Hello");
    }

    #[test]
    fn ascii85_basic() {
        // "Man " encodes to "9jqo^" in ASCII85
        let decoded = decode_ascii85(b"9jqo^~>").unwrap();
        assert_eq!(decoded, b"Man ");
    }

    #[test]
    fn run_length_literal_and_repeat() {
        // 2 literal bytes [0x41, 0x42], then repeat 0x43 three times, then EOD
        let data = [1, 0x41, 0x42, 254, 0x43, 128];
        let decoded = decode_run_length(&data).unwrap();
        assert_eq!(decoded, vec![0x41, 0x42, 0x43, 0x43, 0x43]);
    }

    #[test]
    fn png_predictor_none() {
        // 2 columns, 1 color, 8 bpc → row_bytes = 2, stride = 3
        // filter=0 (None): [0, 0x41, 0x42]
        let data = [0, 0x41, 0x42];
        let result = decode_png_predictor(&data, 1, 8, 2).unwrap();
        assert_eq!(result, vec![0x41, 0x42]);
    }

    #[test]
    fn png_predictor_sub() {
        // filter=1 (Sub), bpp=1: each byte += left
        // row: [1, 10, 5, 3] → decoded: [10, 15, 18]
        let data = [1, 10, 5, 3];
        let result = decode_png_predictor(&data, 1, 8, 3).unwrap();
        assert_eq!(result, vec![10, 15, 18]);
    }

    #[test]
    fn png_predictor_up() {
        // filter=2 (Up): each byte += above
        // row1: [0, 10, 20] → [10, 20]
        // row2: [2, 5, 3]   → [15, 23]
        let data = [0, 10, 20, 2, 5, 3];
        let result = decode_png_predictor(&data, 1, 8, 2).unwrap();
        assert_eq!(result, vec![10, 20, 15, 23]);
    }

    #[test]
    fn png_predictor_paeth() {
        // filter=4 (Paeth), 1 color 8bpc 3 columns, bpp=1
        // row1: [0, 10, 20, 30]  → None: [10, 20, 30]
        // row2: [4, 5, 7, 3]     → Paeth reconstruction
        //   i=0: paeth(0, 10, 0)=10, 5+10=15
        //   i=1: paeth(15, 20, 10)=20, 7+20=27
        //   i=2: paeth(27, 30, 20)=30, 3+30=33
        let data = [0, 10, 20, 30, 4, 5, 7, 3];
        let result = decode_png_predictor(&data, 1, 8, 3).unwrap();
        assert_eq!(result, vec![10, 20, 30, 15, 27, 33]);
    }

    #[test]
    fn tiff_predictor_basic() {
        // 3 colors (RGB), 8bpc, 2 columns → row = 6 bytes
        // [R0,G0,B0, dR1,dG1,dB1] → [R0,G0,B0, R0+dR1, G0+dG1, B0+dB1]
        let data = [100, 150, 200, 10, 20, 30];
        let result = decode_tiff_predictor(&data, 3, 8, 2).unwrap();
        assert_eq!(result, vec![100, 150, 200, 110, 170, 230]);
    }

    // --- LZWDecode ---

    #[test]
    fn lzw_canonical_vector() {
        // Classic ISO 32000 / Adobe LZW example. 9-bit codes, MSB-first:
        //   256 (Clear), 45 ('-'), 258 (KwKwK -> "--"), 259 ("---"),
        //   65 ('A'), 259 ("---"), 66 ('B'), 257 (EOD)
        let data = [0x80, 0x0B, 0x60, 0x50, 0x22, 0x0C, 0x0C, 0x85, 0x01];
        let decoded = lzw_decode(&data, 1).unwrap();
        assert_eq!(decoded, b"-----A---B");
    }

    #[test]
    fn lzw_via_apply_filter_default_early_change() {
        let data = [0x80, 0x0B, 0x60, 0x50, 0x22, 0x0C, 0x0C, 0x85, 0x01];
        let name = PdfName::new("LZWDecode");
        let out = apply_filter(&name, &data, None).unwrap();
        assert_eq!(out, b"-----A---B");
    }

    #[test]
    fn lzw_stops_at_end_without_eod() {
        // Truncated before EOD; should decode the leading symbols and stop cleanly.
        let data = [0x80, 0x0B, 0x60, 0x50];
        let out = lzw_decode(&data, 1).unwrap();
        assert!(out.starts_with(b"-"));
    }

    #[test]
    fn lzw_empty_input() {
        assert_eq!(lzw_decode(&[], 1).unwrap(), Vec::<u8>::new());
    }

    /// Encode with weezl (an independent, spec-conformant LZW producer) so the
    /// decoder is validated against an EXTERNAL reference rather than its own
    /// paired encoder. weezl's TIFF size-switch == PDF EarlyChange=1; its plain
    /// MSB encoder == EarlyChange=0. Verified: weezl(tiff) output of the canonical
    /// vector decodes to "-----A---B" here.
    fn weezl_encode(data: &[u8], early_change: i64) -> Vec<u8> {
        use weezl::{encode::Encoder, BitOrder};
        let mut enc = if early_change == 0 {
            Encoder::new(BitOrder::Msb, 8)
        } else {
            Encoder::with_tiff_size_switch(BitOrder::Msb, 8)
        };
        enc.encode(data).expect("weezl encode")
    }

    #[test]
    fn lzw_roundtrip_against_weezl() {
        // Cross every width boundary (9->10->11->12) and the 4096 auto-clear,
        // for both EarlyChange settings, against an external reference encoder.
        for ec in [1i64, 0] {
            for &len in &[0usize, 1, 300, 600, 1200, 3000, 5000, 9000] {
                // Mix of low- and high-entropy bytes to grow the dictionary.
                let input: Vec<u8> = (0..len).map(|i| ((i * 7 + i / 11) % 251) as u8).collect();
                let encoded = weezl_encode(&input, ec);
                let decoded = lzw_decode(&encoded, ec).unwrap();
                assert_eq!(decoded, input, "ec={ec} len={len}");
            }
        }
    }

    #[test]
    fn lzw_single_byte_run_roundtrip_against_weezl() {
        // A long single-symbol run exercises the KwKwK path heavily.
        let input = vec![b'A'; 5000];
        for ec in [1i64, 0] {
            let encoded = weezl_encode(&input, ec);
            assert_eq!(lzw_decode(&encoded, ec).unwrap(), input, "ec={ec}");
        }
    }
}
