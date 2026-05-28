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

    let mut result = data.to_vec();
    for filter in &filters {
        result = apply_filter(filter, &result)?;
    }
    Ok(result)
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
}
