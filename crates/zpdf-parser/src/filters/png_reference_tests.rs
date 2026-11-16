use super::*;

// Frozen pre-optimization decoder: independent row buffers and history copies.
fn reference(
    data: &[u8],
    colors: usize,
    bpc: usize,
    columns: usize,
    budget: &mut DecodeBudget,
) -> Result<Vec<u8>> {
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
    // Rows lose their filter byte, but a partial last row is padded to a full
    // one, which can outgrow the input.
    budget.reserve(output_size.saturating_sub(data.len()) as u64)?;
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

#[test]
fn png_rows_match_reference_with_partial_rows_and_budgets() {
    for colors in [1usize, 3, 4] {
        for bpc in [1, 2, 4, 8, 16] {
            for columns in [1, 3, 17] {
                let row_bytes = (colors * bpc * columns).div_ceil(8);
                let stride = row_bytes + 1;
                for rows in [0, 1, 2, 5] {
                    for tail in [0, 1, stride - 1] {
                        for filter in [0, 1, 2, 3, 4, 5, 255] {
                            let mut data: Vec<u8> = (0..rows * stride + tail)
                                .map(|i| (i * 67 + 29) as u8)
                                .collect();
                            for start in (0..data.len()).step_by(stride) {
                                data[start] = filter;
                            }
                            let growth = data.len().div_ceil(stride) * row_bytes;
                            let growth = growth.saturating_sub(data.len()) as u64;
                            for limit in [0, growth.saturating_sub(1), growth, u64::MAX] {
                                let mut a = DecodeBudget::new(limit);
                                let mut b = DecodeBudget::new(limit);
                                let actual =
                                    decode_png_predictor(&data, colors, bpc, columns, &mut a)
                                        .map_err(|e| e.to_string());
                                let expected = reference(&data, colors, bpc, columns, &mut b)
                                    .map_err(|e| e.to_string());
                                assert_eq!(
                                    actual, expected,
                                    "{colors}/{bpc}/{columns}/{rows}/{tail}/{filter}/{limit}"
                                );
                                assert_eq!(a.remaining, b.remaining);
                            }
                        }
                    }
                }
            }
        }
    }
}
