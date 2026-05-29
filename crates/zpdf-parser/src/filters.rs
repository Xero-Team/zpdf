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
        result = apply_filter(filter, &result)?;
        if let Some(params) = &decode_parms[i] {
            result = apply_predictor(&result, params)?;
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

fn decode_png_predictor(
    data: &[u8],
    colors: usize,
    bpc: usize,
    columns: usize,
) -> Result<Vec<u8>> {
    let row_bytes = (colors * bpc * columns + 7) / 8;
    let bpp = (colors * bpc + 7) / 8; // bytes per pixel for Sub/Paeth
    let stride = 1 + row_bytes; // filter byte + row data

    if data.len() % stride != 0 && !data.is_empty() {
        // Try to process what we can
        tracing::debug!(
            "PNG predictor: data length {} not multiple of stride {stride}",
            data.len()
        );
    }

    let num_rows = (data.len() + stride - 1) / stride;
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
                    let upper_left = if i >= bpp { prev_row[i - bpp] as i32 } else { 0 };
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

fn apply_filter(filter: &PdfName, data: &[u8]) -> Result<Vec<u8>> {
    match filter.as_str() {
        "FlateDecode" | "Fl" => decode_flate(data),
        "ASCIIHexDecode" | "AHx" => decode_ascii_hex(data),
        "ASCII85Decode" | "A85" => decode_ascii85(data),
        "RunLengthDecode" | "RL" => decode_run_length(data),
        "DCTDecode" | "DCT" => decode_dct(data),
        other => Err(Error::UnsupportedFilter(other.to_string())),
    }
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
}
