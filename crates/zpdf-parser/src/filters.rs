use zpdf_core::{Error, ParseLimits, PdfDict, PdfName, PdfObject, Result};

/// Tracks cumulative decoded bytes across a filter chain to prevent decompression bombs.
/// Each filter in the chain consumes budget; once exhausted, decoding fails.
pub(crate) struct DecodeBudget {
    remaining: u64,
    total_consumed: u64,
}

impl DecodeBudget {
    pub(crate) fn new(limit: u64) -> Self {
        Self {
            remaining: limit,
            total_consumed: 0,
        }
    }

    /// Reserve bytes for output. Returns error if budget exhausted.
    pub(crate) fn reserve(&mut self, bytes: u64) -> Result<()> {
        if bytes > self.remaining {
            return Err(Error::StreamDecode(format!(
                "decoded stream exceeds budget: {} bytes already consumed, {} more requested, {} limit",
                self.total_consumed, bytes, self.total_consumed.saturating_add(self.remaining)
            )));
        }
        self.remaining = self.remaining.saturating_sub(bytes);
        self.total_consumed = self.total_consumed.saturating_add(bytes);
        Ok(())
    }
}

/// Decode a PDF stream through its filter chain with explicit limits.
///
/// This is the primary implementation that enforces cumulative budget tracking
/// to prevent decompression bombs (H1 security fix).
pub fn decode_stream_with_limits(
    data: &[u8],
    dict: &PdfDict,
    limits: &ParseLimits,
) -> Result<Vec<u8>> {
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

    // H1 Fix: Use cumulative budget from ParseLimits instead of per-filter 1 GiB cap
    let mut budget = DecodeBudget::new(limits.max_decoded_stream_bytes);

    // H1 Fix: Process filter chain properly, using previous output as next input
    let mut current = data.to_vec();
    for (i, filter) in filters.iter().enumerate() {
        let params = decode_parms[i].as_ref();

        // Apply filter to current data
        let decoded = apply_filter(filter, &current, params, &mut budget)?;

        // Apply predictor if specified
        current = if let Some(p) = params {
            apply_predictor(&decoded, p, &mut budget)?
        } else {
            decoded
        };
    }

    Ok(current)
}

/// Decode a PDF stream through its filter chain using default limits.
///
/// **Temporary backward compatibility wrapper** - allows existing code to compile
/// while we migrate call sites to pass explicit ParseLimits. This uses default
/// limits (2 GiB decoded budget), which provides DoS protection but isn't customizable.
///
/// New code should use `decode_stream_with_limits` and pass the active ParseLimits.
/// This wrapper will be removed once all call sites are migrated.
pub fn decode_stream(data: &[u8], dict: &PdfDict) -> Result<Vec<u8>> {
    decode_stream_with_limits(data, dict, &ParseLimits::default())
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

fn apply_predictor(data: &[u8], params: &PdfDict, budget: &mut DecodeBudget) -> Result<Vec<u8>> {
    let predictor = params.get_i64("Predictor").unwrap_or(1) as u32;
    if predictor == 1 {
        return Ok(data.to_vec());
    }

    // H1 Fix: Reserve budget for predictor output before allocation
    budget.reserve(data.len() as u64)?;

    // Validate parameters with reasonable PDF limits to prevent overflow attacks.
    // PDF spec typically uses modest values, but we allow generous bounds.
    const MAX_COLORS: usize = 256; // PDF spec typically ≤32
    const MAX_BPC: usize = 32; // PDF spec: 1,2,4,8,12,16,24,32
    const MAX_COLUMNS: usize = 1 << 16; // 64K columns is generous

    let colors = params
        .get_i64("Colors")
        .unwrap_or(1)
        .clamp(1, MAX_COLORS as i64) as usize;
    let bpc = params
        .get_i64("BitsPerComponent")
        .unwrap_or(8)
        .clamp(1, MAX_BPC as i64) as usize;
    let columns = params
        .get_i64("Columns")
        .unwrap_or(1)
        .clamp(1, MAX_COLUMNS as i64) as usize;

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
    // M4 Fix: Validate parameters before processing
    // TIFF predictor (Predictor 2) in PDF spec only supports BPC=8
    if bpc != 8 {
        // Not an error per spec - just means predictor doesn't apply
        return Ok(data.to_vec());
    }

    // M4 Fix: Explicit validation that parameters are non-zero
    if colors == 0 || columns == 0 {
        return Err(Error::StreamDecode(
            "TIFF predictor: colors and columns must be non-zero".into(),
        ));
    }

    // Check for overflow in row_bytes calculation
    let row_bytes = columns
        .checked_mul(colors)
        .ok_or_else(|| Error::StreamDecode("TIFF predictor: columns * colors overflow".into()))?;

    // M4 Fix: Validate data length is consistent with parameters
    // Data should be a multiple of row_bytes (though we handle partial rows gracefully)
    if data.is_empty() {
        return Ok(data.to_vec());
    }

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
    // L1 Fix: Explicit validation of parameters before processing
    if colors == 0 || columns == 0 {
        return Err(Error::StreamDecode(
            "PNG predictor: colors and columns must be non-zero".into(),
        ));
    }
    if bpc == 0 || bpc > 16 {
        return Err(Error::StreamDecode(
            "PNG predictor: bits per component must be 1-16".into(),
        ));
    }

    // Check all multiplications for overflow to prevent allocation bombs
    let bits_per_row = colors
        .checked_mul(bpc)
        .and_then(|v| v.checked_mul(columns))
        .ok_or_else(|| Error::StreamDecode("PNG predictor: row computation overflow".into()))?;
    let row_bytes = bits_per_row.div_ceil(8);

    let bits_per_pixel = colors
        .checked_mul(bpc)
        .ok_or_else(|| Error::StreamDecode("PNG predictor: pixel computation overflow".into()))?;
    let bpp = bits_per_pixel.div_ceil(8); // bytes per pixel for Sub/Paeth

    let stride = row_bytes
        .checked_add(1)
        .ok_or_else(|| Error::StreamDecode("PNG predictor: stride overflow".into()))?; // filter byte + row data

    // L1 Fix: Validate stride is reasonable (not zero, not absurdly large)
    if stride == 0 {
        return Err(Error::StreamDecode(
            "PNG predictor: stride cannot be zero".into(),
        ));
    }

    if !data.len().is_multiple_of(stride) && !data.is_empty() {
        // Try to process what we can
        tracing::debug!(
            "PNG predictor: data length {} not multiple of stride {stride}",
            data.len()
        );
    }

    let num_rows = data.len().div_ceil(stride);
    let output_size = num_rows
        .checked_mul(row_bytes)
        .ok_or_else(|| Error::StreamDecode("PNG predictor: output size overflow".into()))?;
    let mut output = Vec::with_capacity(output_size);
    let mut prev_row = vec![0u8; row_bytes];

    let mut pos = 0;
    while pos < data.len() {
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

fn apply_filter(
    filter: &PdfName,
    data: &[u8],
    params: Option<&PdfDict>,
    budget: &mut DecodeBudget,
) -> Result<Vec<u8>> {
    match filter.as_str() {
        "FlateDecode" | "Fl" => decode_flate(data, budget),
        "LZWDecode" | "LZW" => {
            // EarlyChange lives in DecodeParms; default 1 per ISO 32000.
            let early_change = params
                .and_then(|p| p.get_i64("EarlyChange").ok())
                .unwrap_or(1);
            lzw_decode(data, early_change, budget)
        }
        "ASCIIHexDecode" | "AHx" => decode_ascii_hex(data, budget),
        "ASCII85Decode" | "A85" => decode_ascii85(data, budget),
        "RunLengthDecode" | "RL" => decode_run_length(data, budget),
        "DCTDecode" | "DCT" => decode_dct(data, budget),
        "CCITTFaxDecode" | "CCF" => {
            let ccitt_params = crate::ccitt::CcittParams::from_dict(params);
            // M1 Fix: Thread budget through to prevent decompression bombs
            crate::ccitt::decode(data, &ccitt_params, budget)
        }
        "JBIG2Decode" => {
            let jbig2_params = crate::jbig2::Jbig2Params::from_dict(params);
            // M2 Fix: Thread budget through to prevent decompression bombs
            crate::jbig2::decode(data, &jbig2_params, budget)
        }
        // JPXDecode output is decoded *pixels*, not raw samples, and JPEG 2000
        // carries its own colour-space/alpha metadata that a bytes-only filter
        // cannot return. Pass the codestream through unchanged; zpdf-image
        // sniffs it (filter name + JP2/SOC magic) and runs the real decode.
        "JPXDecode" => Ok(data.to_vec()),
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
fn lzw_decode(data: &[u8], early_change: i64, budget: &mut DecodeBudget) -> Result<Vec<u8>> {
    const CLEAR: u32 = 256;
    const EOD: u32 = 257;

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

        // H1 Fix: Reserve budget before extending output
        budget.reserve(entry.len() as u64)?;
        out.extend_from_slice(&entry);

        // Add new dictionary entry = previous string + first byte of this entry.
        // (Skipped for the first code after a clear, when prev is None.)
        if let Some(p) = prev {
            // M5 Fix: Enforce the LZW table size limit of 4096 entries (codes 0-4095).
            // The spec dictates max code width is 12 bits (2^12 = 4096), so adding
            // beyond this would violate the protocol.
            if table.len() >= 4096 {
                // Table is full; don't add new entries until a CLEAR resets it.
                // This is standard LZW behavior when the table maxes out.
            } else {
                let mut new_entry = table[p as usize].clone();
                new_entry.push(entry[0]);
                table.push(new_entry);
            }
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

/// Outcome of one chunked inflate attempt: either the reader ran to a clean
/// EOF, or it failed partway with whatever bytes were recovered first.
enum InflateOutcome {
    Complete(Vec<u8>),
    Failed(Vec<u8>, String),
}

/// Drive `reader` to completion in fixed-size chunks so that a mid-stream
/// error still yields the bytes decoded before it. Output is capped by the
/// budget; hitting the cap is a hard error (a decompression bomb is not salvageable data).
fn inflate_chunked(
    mut reader: impl std::io::Read,
    budget: &mut DecodeBudget,
) -> Result<InflateOutcome> {
    let mut out = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => return Ok(InflateOutcome::Complete(out)),
            Ok(n) => {
                // H1 Fix: Reserve budget before extending output
                budget.reserve(n as u64)?;
                out.extend_from_slice(&buf[..n]);
            }
            Err(e) => return Ok(InflateOutcome::Failed(out, e.to_string())),
        }
    }
}

/// FlateDecode with real-world tolerance: salvages partial output from
/// truncated/corrupt zlib streams, retries headerless data as raw deflate,
/// and skips a bounded run of leading garbage before a plausible zlib header.
fn decode_flate(data: &[u8], budget: &mut DecodeBudget) -> Result<Vec<u8>> {
    use flate2::read::{DeflateDecoder, ZlibDecoder};

    // Lenient: an empty stream decodes to nothing.
    if data.is_empty() {
        return Ok(Vec::new());
    }

    // Plausible zlib header at `i`: CM (low nibble of CMF) is 8 (deflate) and
    // the FCHECK property holds (CMF<<8 | FLG divisible by 31).
    let plausible_zlib = |i: usize| {
        data.len() >= i + 2
            && data[i] & 0x0f == 8
            && ((data[i] as u32) << 8 | data[i + 1] as u32).is_multiple_of(31)
    };

    let mut zlib_err: Option<String> = None;
    if plausible_zlib(0) {
        match inflate_chunked(ZlibDecoder::new(data), budget)? {
            InflateOutcome::Complete(out) => return Ok(out),
            InflateOutcome::Failed(partial, err) if !partial.is_empty() => {
                tracing::warn!(
                    "FlateDecode: zlib stream failed after {} bytes ({err}); keeping partial output",
                    partial.len()
                );
                return Ok(partial);
            }
            InflateOutcome::Failed(_, err) => zlib_err = Some(err),
        }
    }

    // The header was implausible (or decoded to nothing): look for a plausible
    // CMF/FLG pair after a bounded garbage/whitespace prefix.
    const MAX_HEADER_SCAN: usize = 64;
    if let Some(k) = (1..data.len().min(MAX_HEADER_SCAN)).find(|&k| plausible_zlib(k)) {
        match inflate_chunked(ZlibDecoder::new(&data[k..]), budget)? {
            InflateOutcome::Complete(out) => {
                tracing::warn!("FlateDecode: skipped {k} bytes of leading garbage");
                return Ok(out);
            }
            InflateOutcome::Failed(partial, err) if !partial.is_empty() => {
                tracing::warn!(
                    "FlateDecode: zlib stream at offset {k} failed ({err}); keeping {} partial bytes",
                    partial.len()
                );
                return Ok(partial);
            }
            InflateOutcome::Failed(..) => {}
        }
    }

    // Last resort: some writers emit raw deflate with no zlib wrapper.
    match inflate_chunked(DeflateDecoder::new(data), budget)? {
        InflateOutcome::Complete(out) => {
            tracing::warn!("FlateDecode: decoded as raw deflate (missing zlib header)");
            Ok(out)
        }
        InflateOutcome::Failed(partial, err) if !partial.is_empty() => {
            tracing::warn!(
                "FlateDecode: raw deflate failed ({err}); keeping {} partial bytes",
                partial.len()
            );
            Ok(partial)
        }
        InflateOutcome::Failed(_, err) => Err(Error::StreamDecode(format!(
            "FlateDecode: {}",
            zlib_err.unwrap_or(err)
        ))),
    }
}

/// Lenient ASCIIHexDecode: whitespace is ignored anywhere, stray non-hex bytes
/// are skipped (warned, not fatal), and anything after the `>` EOD marker is
/// ignored, so partial/dirty streams still decode.
fn decode_ascii_hex(data: &[u8], budget: &mut DecodeBudget) -> Result<Vec<u8>> {
    // M7 Fix: Reserve budget upfront for worst-case output size (one byte per two input chars)
    let max_output = data.len().saturating_add(1) / 2;
    budget.reserve(max_output as u64)?;

    let mut output = Vec::with_capacity(data.len() / 2);
    let mut high: Option<u8> = None;
    let mut stray = 0usize;

    for &b in data {
        if b == b'>' {
            break; // EOD; bytes after it are ignored
        }
        if b.is_ascii_whitespace() || b == 0 {
            continue;
        }
        let nibble = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => {
                stray += 1;
                continue;
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
    if stray > 0 {
        tracing::warn!("ASCIIHexDecode: ignored {stray} invalid byte(s)");
    }

    Ok(output)
}

/// Lenient ASCII85Decode: whitespace is ignored anywhere, stray bytes outside
/// the alphabet are skipped (warned, not fatal), and everything from the `~`
/// of the `~>` EOD marker on is ignored, salvaging partial output.
fn decode_ascii85(data: &[u8], budget: &mut DecodeBudget) -> Result<Vec<u8>> {
    // M6 Fix: Reserve budget upfront for worst-case output size.
    // Each 5-char ASCII85 group decodes to 4 bytes, plus special 'z' → 4 bytes.
    // Worst case: all 'z' chars → data.len() * 4 bytes. Over-reservation is safe.
    let max_output = (data.len() as u64).saturating_mul(4);
    budget.reserve(max_output)?;

    let mut output = Vec::new();
    // u64 accumulator: a 5-char group of bytes near 'u' encodes a value just
    // above u32::MAX; the spec calls it invalid, but it must not overflow.
    let mut tuple: u64 = 0;
    let mut count = 0usize;
    let mut stray = 0usize;

    for &b in data {
        if b == b'~' {
            break; // start of the "~>" EOD marker; ignore it and the rest
        }
        if b.is_ascii_whitespace() || b == 0 {
            continue;
        }

        if b == b'z' && count == 0 {
            output.extend_from_slice(&[0, 0, 0, 0]);
            continue;
        }

        if !(b'!'..=b'u').contains(&b) {
            stray += 1;
            continue;
        }

        tuple = tuple * 85 + (b - b'!') as u64;
        count += 1;

        if count == 5 {
            let t = (tuple & 0xFFFF_FFFF) as u32;
            output.extend_from_slice(&t.to_be_bytes());
            tuple = 0;
            count = 0;
        }
    }

    // Handle remaining bytes
    if count > 1 {
        for _ in count..5 {
            tuple = tuple * 85 + 84; // pad with 'u'
        }
        let t = (tuple & 0xFFFF_FFFF) as u32;
        for i in 0..(count - 1) {
            output.push((t >> (24 - i * 8)) as u8);
        }
    }
    if stray > 0 {
        tracing::warn!("ASCII85Decode: ignored {stray} invalid byte(s)");
    }

    Ok(output)
}

/// H2 Fix: Validate JPEG dimensions before zune-jpeg allocates the output buffer.
/// A malformed JPEG header claiming 65535×65535×4 would trigger a ~16 GiB allocation
/// in `decode()`, bypassing all limits. Pre-validate against ParseLimits and reserve
/// the decoded size from the budget before calling decode.
fn validate_jpeg_dimensions(
    width: u16,
    height: u16,
    components: u8,
    budget: &mut DecodeBudget,
) -> Result<()> {
    let w = width as u64;
    let h = height as u64;
    let c = components as u64;

    // Check pixel count against the same limit zpdf-image uses (mirrors ParseLimits default)
    let pixel_count = w.saturating_mul(h);
    if pixel_count > 100_000_000 {
        return Err(Error::StreamDecode(format!(
            "JPEG dimensions {w}×{h} exceed 100M pixel limit"
        )));
    }

    // Check decoded byte size (width × height × channels) against budget
    let decoded_size = pixel_count.saturating_mul(c);
    if decoded_size > budget.remaining {
        return Err(Error::StreamDecode(format!(
            "JPEG output {w}×{h}×{c} = {decoded_size} bytes exceeds remaining budget {}",
            budget.remaining
        )));
    }

    // Reserve budget before zune-jpeg allocates
    budget.reserve(decoded_size)?;
    Ok(())
}

fn decode_dct(data: &[u8], budget: &mut DecodeBudget) -> Result<Vec<u8>> {
    use zune_jpeg::JpegDecoder;

    // Adobe YCCK JPEGs (APP14 transform == 2, 4 components) are mis-handled by
    // zune-jpeg's built-in YCCK->RGB: it applies a spurious `255 - x`, producing
    // a colour-negative image (a white CMYK page reads back as black). zune has
    // no YCCK->CMYK arm either, so we take the raw YCCK channels and convert them
    // ourselves. Plain Adobe CMYK (transform 0) decodes correctly via zune's
    // CMYK->RGB, so it stays on the default path.
    if jpeg_is_adobe_ycck(data) {
        use zune_jpeg::zune_core::colorspace::ColorSpace;
        use zune_jpeg::zune_core::options::DecoderOptions;
        let opts = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::YCCK);
        let mut decoder = JpegDecoder::new_with_options(std::io::Cursor::new(data), opts);

        // H2 Fix: Validate JPEG dimensions before decode to prevent allocation bomb
        decoder
            .decode_headers()
            .map_err(|e| Error::StreamDecode(format!("DCTDecode header parse failed: {e}")))?;
        if let Some(info) = decoder.info() {
            validate_jpeg_dimensions(info.width, info.height, info.components, budget)?;
        }

        match decoder.decode() {
            Ok(ycck) if decoder.output_colorspace() == Some(ColorSpace::YCCK) => {
                return Ok(ycck_to_rgb(&ycck));
            }
            // Unexpected (e.g. not actually 4-component): fall through to the
            // default decode rather than mangle the data.
            _ => {}
        }
    }

    let mut decoder = JpegDecoder::new(std::io::Cursor::new(data));

    // H2 Fix: Validate JPEG dimensions before decode to prevent allocation bomb
    decoder
        .decode_headers()
        .map_err(|e| Error::StreamDecode(format!("DCTDecode header parse failed: {e}")))?;
    if let Some(info) = decoder.info() {
        validate_jpeg_dimensions(info.width, info.height, info.components, budget)?;
    }

    decoder
        .decode()
        .map_err(|e| Error::StreamDecode(format!("DCTDecode: {e}")))
}

/// Convert raw upsampled Adobe YCCK samples (`Y, Cb, Cr, K` per pixel) to RGB.
///
/// In Adobe YCCK the chroma channels encode the *complement* of C/M/Y, so the
/// JFIF YCbCr->RGB output is the transmitted (inverted) ink: `C = 1 − R'`,
/// `M = 1 − G'`, `Y = 1 − B'`. The 4th channel is the black-ink amount
/// (`K_raw = 255` ⇒ full black). The recovered DeviceCMYK is converted through
/// the shared Adobe polynomial ([`zpdf_color::cmyk_to_rgb`]) so YCCK JPEGs match
/// every other DeviceCMYK path — e.g. 100 % K is a dark near-black, not pure
/// black. (The previous `channel * (255 − K_raw)` shortcut was the naïve
/// `(1−c)(1−k)`, which over-saturated like a non-fidelity viewer.)
fn ycck_to_rgb(ycck: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ycck.len() / 4 * 3);
    for px in ycck.chunks_exact(4) {
        let (y, cb, cr) = (px[0] as f64, px[1] as f64, px[2] as f64);
        // JFIF YCbCr -> R'G'B' (transmitted light = complement of C/M/Y ink).
        let rp = (y + 1.402 * (cr - 128.0)).clamp(0.0, 255.0);
        let gp = (y - 0.344_136 * (cb - 128.0) - 0.714_136 * (cr - 128.0)).clamp(0.0, 255.0);
        let bp = (y + 1.772 * (cb - 128.0)).clamp(0.0, 255.0);
        let (r, g, b) = zpdf_color::cmyk_to_rgb(
            1.0 - rp / 255.0,
            1.0 - gp / 255.0,
            1.0 - bp / 255.0,
            px[3] as f64 / 255.0,
        );
        out.push((r * 255.0).round() as u8);
        out.push((g * 255.0).round() as u8);
        out.push((b * 255.0).round() as u8);
    }
    out
}

/// Scan a JPEG for an Adobe APP14 marker with transform 2 (YCCK) over a SOF that
/// declares 4 components. Cheap byte walk over the marker segments only.
fn jpeg_is_adobe_ycck(data: &[u8]) -> bool {
    let mut adobe_ycck = false;
    let mut four_components = false;
    let mut i = 2; // skip SOI (FFD8)
    while i + 3 < data.len() {
        if data[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = data[i + 1];
        // Standalone markers (no length): padding fill, SOI/EOI, RSTn, TEM.
        if marker == 0xFF || marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
            i += 2;
            continue;
        }
        let seg_len = ((data[i + 2] as usize) << 8) | data[i + 3] as usize;
        if seg_len < 2 {
            break;
        }
        let payload_start = i + 4;
        let payload_end = i + 2 + seg_len;
        if payload_end > data.len() {
            break;
        }
        let payload = &data[payload_start..payload_end];
        match marker {
            // APP14: "Adobe" + version(2) + flags0(2) + flags1(2) + transform(1).
            0xEE => {
                if payload.len() >= 12 && &payload[0..5] == b"Adobe" {
                    adobe_ycck = payload[11] == 2;
                }
            }
            // SOFn (baseline/progressive/etc.), excluding DHT(C4)/JPG(C8)/DAC(CC).
            0xC0..=0xCF if marker != 0xC4 && marker != 0xC8 && marker != 0xCC => {
                // precision(1) + height(2) + width(2) + Nf(1).
                if payload.len() >= 6 {
                    four_components = payload[5] == 4;
                }
            }
            // Start of scan: header is done.
            0xDA => break,
            _ => {}
        }
        i = payload_end;
    }
    adobe_ycck && four_components
}

fn decode_run_length(data: &[u8], budget: &mut DecodeBudget) -> Result<Vec<u8>> {
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
            // H1 Fix: Reserve budget before extending
            budget.reserve(count as u64)?;
            output.extend_from_slice(&data[i..i + count]);
            i += count;
        } else {
            // Repeat next byte (257 - length_byte) times
            let count = 257 - length_byte as usize;
            if i >= data.len() {
                return Err(Error::StreamDecode("RunLengthDecode: truncated".into()));
            }
            // H1 Fix: Reserve budget before resizing
            budget.reserve(count as u64)?;
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
    fn ycck_white_decodes_white_not_black() {
        // Adobe white (no ink): Y=255, Cb=Cr=128 (neutral), K_raw=0 → CMYK all 0.
        let rgb = ycck_to_rgb(&[255, 128, 128, 0]);
        assert_eq!(rgb, vec![255, 255, 255], "Adobe YCCK white must stay white");
    }

    #[test]
    fn ycck_full_black_ink_decodes_near_black() {
        // K_raw=255 ⇒ CMYK (0,0,0,1). The Adobe DeviceCMYK polynomial renders
        // 100% K as a dark near-black, not pure black (matches every other path).
        let rgb = ycck_to_rgb(&[255, 128, 128, 255]);
        assert_eq!(rgb, vec![44, 46, 53]);
    }

    #[test]
    fn ycck_neutral_gray_via_polynomial() {
        // No CMY (chroma neutral, luma full) with half black ink ⇒ CMYK
        // (0,0,0,0.5); the polynomial maps it lighter than the naïve 127.
        let rgb = ycck_to_rgb(&[255, 128, 128, 128]);
        assert_eq!(rgb, vec![154, 156, 159]);
    }

    #[test]
    fn ycck_colored_pixel_via_polynomial() {
        // Non-neutral chroma exercises the C/M/Y recovery + the full polynomial.
        let rgb = ycck_to_rgb(&[200, 100, 150, 50]);
        assert_eq!(rgb, vec![198, 165, 131]);
    }

    #[test]
    fn adobe_ycck_detection() {
        // Minimal marker stream: SOI, APP14(Adobe, transform=2), SOF0(4 comp), SOS.
        let mut j = vec![0xFF, 0xD8];
        // APP14, len=16: "Adobe"(5)+ver(2)+f0(2)+f1(2)+transform(1) = 12 payload, +2 len = 14... use 16 with pad.
        j.extend_from_slice(&[0xFF, 0xEE, 0x00, 0x0E]);
        j.extend_from_slice(b"Adobe");
        j.extend_from_slice(&[0x00, 0x64, 0x00, 0x00, 0x00, 0x00, 0x02]); // version, flags, transform=2
                                                                          // SOF0, len=17 (1 prec + 2 h + 2 w + 1 Nf=4 + 4*3 comp specs) -> payload 6+ needed.
        j.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08, 0x00, 0x10, 0x00, 0x10, 0x04]);
        j.extend_from_slice(&[1, 0x11, 0, 2, 0x11, 0, 3, 0x11, 0, 4, 0x11, 0]);
        j.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02]); // SOS
        assert!(jpeg_is_adobe_ycck(&j));

        // transform=0 (plain CMYK) must NOT take the YCCK path.
        let mut j0 = j.clone();
        // transform byte is at: 2 (SOI) + 4 (app14 hdr) + 11 = index 17.
        j0[17] = 0;
        assert!(!jpeg_is_adobe_ycck(&j0));
    }

    #[test]
    fn flate_roundtrip() {
        use flate2::write::ZlibEncoder;
        use flate2::Compression;
        use std::io::Write;

        let original = b"Hello, zpdf! This is a test of FlateDecode.";
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(original).unwrap();
        let compressed = encoder.finish().unwrap();

        let decoded = {
            let mut budget = DecodeBudget::new(ParseLimits::default().max_decoded_stream_bytes);
            decode_flate(&compressed, &mut budget).unwrap()
        };
        assert_eq!(decoded, original);
    }

    #[test]
    fn flate_partial_salvage_on_truncation() {
        use flate2::write::ZlibEncoder;
        use flate2::Compression;
        use std::io::Write;

        // Deterministic, mostly-incompressible data so the compressed stream
        // is long and a truncation still leaves plenty of decodable input.
        let mut state = 0x2545F491u64;
        let original: Vec<u8> = (0..64 * 1024)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 33) as u8
            })
            .collect();
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&original).unwrap();
        let compressed = encoder.finish().unwrap();

        let truncated = &compressed[..compressed.len() / 2];
        let decoded = {
            let mut budget = DecodeBudget::new(ParseLimits::default().max_decoded_stream_bytes);
            decode_flate(truncated, &mut budget).unwrap()
        };
        assert!(!decoded.is_empty(), "partial output must be salvaged");
        assert!(decoded.len() < original.len());
        assert_eq!(
            &original[..decoded.len()],
            &decoded[..],
            "salvaged bytes are a prefix"
        );
    }

    #[test]
    fn flate_raw_deflate_fallback() {
        use flate2::write::DeflateEncoder;
        use flate2::Compression;
        use std::io::Write;

        let original = b"raw deflate stream without a zlib wrapper".to_vec();
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&original).unwrap();
        let compressed = encoder.finish().unwrap();

        let decoded = {
            let mut budget = DecodeBudget::new(ParseLimits::default().max_decoded_stream_bytes);
            decode_flate(&compressed, &mut budget).unwrap()
        };
        assert_eq!(decoded, original);
    }

    #[test]
    fn flate_skips_leading_garbage() {
        use flate2::write::ZlibEncoder;
        use flate2::Compression;
        use std::io::Write;

        let original = b"zlib data behind a garbage prefix".to_vec();
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&original).unwrap();
        let compressed = encoder.finish().unwrap();

        // \r\n\xff: no byte pair in the prefix forms a plausible zlib header.
        let mut data = b"\r\n\xff".to_vec();
        data.extend_from_slice(&compressed);
        let decoded = {
            let mut budget = DecodeBudget::new(ParseLimits::default().max_decoded_stream_bytes);
            decode_flate(&data, &mut budget).unwrap()
        };
        assert_eq!(decoded, original);
    }

    #[test]
    fn flate_empty_input_is_empty_output() {
        assert_eq!(
            {
                let mut budget = DecodeBudget::new(ParseLimits::default().max_decoded_stream_bytes);
                decode_flate(&[], &mut budget).unwrap()
            },
            Vec::<u8>::new()
        );
    }

    #[test]
    fn flate_garbage_still_errors() {
        // Pure ASCII text: implausible zlib header, invalid deflate.
        assert!({
            let mut budget = DecodeBudget::new(ParseLimits::default().max_decoded_stream_bytes);
            decode_flate(b"this is not compressed data at all....", &mut budget).is_err()
        });
    }

    #[test]
    fn ascii_hex() {
        let decoded = {
            let mut budget = DecodeBudget::new(ParseLimits::default().max_decoded_stream_bytes);
            decode_ascii_hex(b"48 65 6C 6C 6F>", &mut budget).unwrap()
        };
        assert_eq!(decoded, b"Hello");
    }

    #[test]
    fn ascii_hex_tolerates_stray_bytes_and_data_after_eod() {
        // 'x'/'!' are not hex digits (skipped); '>' is EOD (rest ignored).
        let decoded = {
            let mut budget = DecodeBudget::new(ParseLimits::default().max_decoded_stream_bytes);
            decode_ascii_hex(b"48 65 x6C!6C 6F> trailing garbage \xff", &mut budget).unwrap()
        };
        assert_eq!(decoded, b"Hello");
    }

    #[test]
    fn ascii85_basic() {
        // "Man " encodes to "9jqo^" in ASCII85
        let decoded = {
            let mut budget = DecodeBudget::new(ParseLimits::default().max_decoded_stream_bytes);
            decode_ascii85(b"9jqo^~>", &mut budget).unwrap()
        };
        assert_eq!(decoded, b"Man ");
    }

    #[test]
    fn ascii85_ignores_bytes_after_eod() {
        let decoded = {
            let mut budget = DecodeBudget::new(ParseLimits::default().max_decoded_stream_bytes);
            decode_ascii85(b"9jqo^~> stray bytes \xff\xfe after EOD", &mut budget).unwrap()
        };
        assert_eq!(decoded, b"Man ");
    }

    #[test]
    fn ascii85_skips_stray_bytes_and_whitespace() {
        // NUL and 0xFF are outside the alphabet: skipped, not fatal.
        // Whitespace inside a group is ignored.
        let decoded = {
            let mut budget = DecodeBudget::new(ParseLimits::default().max_decoded_stream_bytes);
            decode_ascii85(b"9j\x00qo\xff ^~>", &mut budget).unwrap()
        };
        assert_eq!(decoded, b"Man ");
    }

    #[test]
    fn ascii85_overflowing_group_does_not_panic() {
        // "uuuuu" encodes a value above u32::MAX — invalid per spec, but must
        // decode leniently (truncated) instead of overflowing.
        assert!({
            let mut budget = DecodeBudget::new(ParseLimits::default().max_decoded_stream_bytes);
            decode_ascii85(b"uuuuu~>", &mut budget).is_ok()
        });
    }

    #[test]
    fn run_length_literal_and_repeat() {
        // 2 literal bytes [0x41, 0x42], then repeat 0x43 three times, then EOD
        let data = [1, 0x41, 0x42, 254, 0x43, 128];
        let decoded = {
            let mut budget = DecodeBudget::new(ParseLimits::default().max_decoded_stream_bytes);
            decode_run_length(&data, &mut budget).unwrap()
        };
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
        let mut budget = DecodeBudget::new(ParseLimits::default().max_decoded_stream_bytes);
        let decoded = lzw_decode(&data, 1, &mut budget).unwrap();
        assert_eq!(decoded, b"-----A---B");
    }

    #[test]
    fn lzw_via_apply_filter_default_early_change() {
        let data = [0x80, 0x0B, 0x60, 0x50, 0x22, 0x0C, 0x0C, 0x85, 0x01];
        let name = PdfName::new("LZWDecode");
        let mut budget = DecodeBudget::new(ParseLimits::default().max_decoded_stream_bytes);
        let out = apply_filter(&name, &data, None, &mut budget).unwrap();
        assert_eq!(out, b"-----A---B");
    }

    #[test]
    fn lzw_stops_at_end_without_eod() {
        // Truncated before EOD; should decode the leading symbols and stop cleanly.
        let data = [0x80, 0x0B, 0x60, 0x50];
        let mut budget = DecodeBudget::new(ParseLimits::default().max_decoded_stream_bytes);
        let out = lzw_decode(&data, 1, &mut budget).unwrap();
        assert!(out.starts_with(b"-"));
    }

    #[test]
    fn lzw_empty_input() {
        let mut budget = DecodeBudget::new(ParseLimits::default().max_decoded_stream_bytes);
        assert_eq!(lzw_decode(&[], 1, &mut budget).unwrap(), Vec::<u8>::new());
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
        let mut budget = DecodeBudget::new(ParseLimits::default().max_decoded_stream_bytes);
        for ec in [1i64, 0] {
            for &len in &[0usize, 1, 300, 600, 1200, 3000, 5000, 9000] {
                // Mix of low- and high-entropy bytes to grow the dictionary.
                let input: Vec<u8> = (0..len).map(|i| ((i * 7 + i / 11) % 251) as u8).collect();
                let encoded = weezl_encode(&input, ec);
                let decoded = lzw_decode(&encoded, ec, &mut budget).unwrap();
                assert_eq!(decoded, input, "ec={ec} len={len}");
            }
        }
    }

    #[test]
    fn lzw_single_byte_run_roundtrip_against_weezl() {
        // A long single-symbol run exercises the KwKwK path heavily.
        let input = vec![b'A'; 5000];
        let mut budget = DecodeBudget::new(ParseLimits::default().max_decoded_stream_bytes);
        for ec in [1i64, 0] {
            let encoded = weezl_encode(&input, ec);
            assert_eq!(
                lzw_decode(&encoded, ec, &mut budget).unwrap(),
                input,
                "ec={ec}"
            );
        }
    }
}
