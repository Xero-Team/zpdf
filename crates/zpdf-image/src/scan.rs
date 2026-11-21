//! Opaque Gray/RGB scans: convert one source row and box-filter it immediately.
//! This avoids a full-size RGBA intermediate. Other color spaces, external
//! masks and JPX keep their existing paths, including all CMS/mask ordering.
use super::*;

/// A minified scan and its original sample dimensions, for image-cache keys.
pub struct FootprintScan {
    pub image: DecodedImage,
    pub source_width: u32,
    pub source_height: u32,
}

/// Attempt row-wise conversion at the CPU's exact box-filter footprint.
/// `None` requests the ordinary image path: no minification, unsupported color
/// space, masks, or JPX. Input is the same post-filter sample buffer consumed by
/// `decode_image_xobject_resolved_with_limits`; JPEG decoding is not streamed.
pub fn decode_scan_at_footprint(
    data: &[u8],
    dict: &PdfDict,
    colorspace: Option<&ResolvedColorSpace>,
    transform: &zpdf_core::Matrix,
    scale: f32,
    limits: &ParseLimits,
) -> Result<Option<FootprintScan>> {
    if !scale.is_finite()
        || scale <= 0.0
        || is_image_mask(dict)
        || dict.get("SMask").is_some()
        || dict.get("Mask").is_some()
        || is_jpx_encoded(dict, data)
    {
        return Ok(None);
    }
    let inferred;
    let cs = match colorspace {
        Some(cs) => cs,
        None => {
            inferred = infer_colorspace(dict);
            &inferred
        }
    };
    if !matches!(cs, ResolvedColorSpace::Gray | ResolvedColorSpace::Rgb) {
        return Ok(None);
    }
    let dimension = |name| {
        dict.get_i64(name)
            .ok()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|&n| n != 0)
    };
    let (Some(mut width), Some(mut height)) = (dimension("Width"), dimension("Height")) else {
        return Ok(None);
    };
    let dct = is_dct_encoded(dict);
    let mut ncomp = cs.components();
    if dct {
        (width, height, ncomp) = dct_layout(data.len(), width, height, cs)?;
        if !matches!(ncomp, 1 | 3) {
            return Ok(None);
        }
    }
    let Some((tw, th)) = box_filter_size(transform, scale, width, height) else {
        return Ok(None);
    };
    // Match ContentInterpreter::image_footprint: never pre-filter a copy that
    // the renderer would filter again (notably sub-pixel placements).
    if box_filter_size(transform, scale, tw, th).is_some() {
        return Ok(None);
    }
    struct Column {
        start: usize,
        end: usize,
        sum: [u64; 3],
    }
    // Short images can cost more in row/column scratch than in a complete
    // RGBA buffer. Keep the ordinary path unless this actually saves storage.
    let scratch = u64::from(width) * 4 + u64::from(tw) * std::mem::size_of::<Column>() as u64;
    if scratch >= (u64::from(width) * u64::from(height)).saturating_mul(4) {
        return Ok(None);
    }
    if u64::from(width) * u64::from(height) > limits.max_image_pixels {
        let prefix = if dct { "DCT image" } else { "image" };
        return Err(Error::StreamDecode(format!(
            "{prefix} {width}x{height} exceeds the {}-pixel limit",
            limits.max_image_pixels
        )));
    }
    let bpc = if dct { 8 } else { bits_per_component(dict)? };
    let cs = if ncomp == 1 {
        ResolvedColorSpace::Gray
    } else {
        ResolvedColorSpace::Rgb
    };
    let decode = decode_array(dict, ncomp);
    let luts = build_component_luts(bpc, &cs, decode.as_deref());
    let row_bytes = (width as usize)
        .checked_mul(ncomp)
        .and_then(|n| n.checked_mul(bpc as usize))
        .ok_or_else(|| Error::StreamDecode("raw samples: row bytes overflow".into()))?
        .div_ceil(8);
    let required = row_bytes
        .checked_mul(height as usize)
        .ok_or_else(|| Error::StreamDecode("raw samples: data size overflow".into()))?;
    let short = check_sample_data_len(data.len(), row_bytes, required)?;
    let rgba_len = |w: u32, h: u32| {
        (w as usize)
            .checked_mul(h as usize)
            .and_then(|n| n.checked_mul(4))
            .ok_or_else(|| Error::StreamDecode("raw samples: RGBA buffer size overflow".into()))
    };
    let mut row = vec![0; rgba_len(width, 1)?];
    let mut out = vec![0; rgba_len(tw, th)?];
    // Compute covering x intervals once, rather than divide for every row.
    let mut columns: Vec<_> = (0..u64::from(tw))
        .map(|tx| {
            let x0 = tx * u64::from(width) / u64::from(tw);
            let x1 = ((tx + 1) * u64::from(width))
                .div_ceil(u64::from(tw))
                .clamp(x0 + 1, u64::from(width));
            Column {
                start: x0 as usize * 4,
                end: x1 as usize * 4,
                sum: [0; 3],
            }
        })
        .collect();
    let mut last_row = None;
    for ty in 0..u64::from(th) {
        let y0 = ty * u64::from(height) / u64::from(th);
        let y1 = ((ty + 1) * u64::from(height))
            .div_ceil(u64::from(th))
            .clamp(y0 + 1, u64::from(height));
        for column in &mut columns {
            column.sum = [0; 3];
        }
        for sy in y0..y1 {
            // Adjacent destination boxes can overlap at one source row. Reuse
            // its converted pixels while preserving the exact box weights.
            if last_row != Some(sy) {
                let start = sy as usize * row_bytes;
                if bpc == 8 && !short {
                    let samples = &data[start..start + row_bytes];
                    if ncomp == 1 {
                        fill_rgba_8bpc::<1>(samples, &mut row, &luts, |c| [c[0]; 3]);
                    } else {
                        fill_rgba_8bpc::<3>(samples, &mut row, &luts, |c| *c);
                    }
                } else {
                    let mut bit = 0;
                    for px in row.as_chunks_mut::<4>().0 {
                        for c in 0..ncomp {
                            let sample = read_sample(data, start, &mut bit, bpc);
                            let index = if bpc == 16 {
                                (sample >> 8) as usize
                            } else {
                                sample as usize
                            };
                            px[c] = luts[c][index];
                        }
                        if ncomp == 1 {
                            px[1] = px[0];
                            px[2] = px[0];
                        }
                        px[3] = 255;
                    }
                }
                last_row = Some(sy);
            }
            for column in &mut columns {
                for px in row[column.start..column.end].as_chunks::<4>().0 {
                    column.sum[0] += u64::from(px[0]);
                    column.sum[1] += u64::from(px[1]);
                    column.sum[2] += u64::from(px[2]);
                }
            }
        }
        for (tx, column) in columns.iter().enumerate() {
            let n = ((column.end - column.start) / 4) as u64 * (y1 - y0);
            let at = (ty as usize * tw as usize + tx) * 4;
            for c in 0..3 {
                out[at + c] = ((column.sum[c] + n / 2) / n) as u8;
            }
            out[at + 3] = 255;
        }
    }
    Ok(Some(FootprintScan {
        source_width: width,
        source_height: height,
        image: DecodedImage {
            width: tw,
            height: th,
            data: out,
            has_alpha: false,
            is_image_mask: false,
            premultiplied: false,
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_scans_keep_the_lower_memory_full_path() {
        let transform = zpdf_core::Matrix::new(11., 0., 0., 1., 0., 0.);
        for height in [1, 2] {
            let d = dict(37, height, 8, false);
            assert!(decode_scan_at_footprint(
                &vec![0; 37 * height as usize * 3],
                &d,
                None,
                &transform,
                1.,
                &ParseLimits::default()
            )
            .unwrap()
            .is_none());
        }
    }

    fn dict(w: u32, h: u32, bpc: u8, gray: bool) -> PdfDict {
        let mut d = PdfDict::new();
        for (key, value) in [
            ("Width", w as i64),
            ("Height", h as i64),
            ("BitsPerComponent", bpc as i64),
        ] {
            d.insert(zpdf_core::PdfName::new(key), PdfObject::Integer(value));
        }
        d.insert(
            zpdf_core::PdfName::new("ColorSpace"),
            PdfObject::Name(zpdf_core::PdfName::new(if gray {
                "DeviceGray"
            } else {
                "DeviceRGB"
            })),
        );
        d
    }

    #[test]
    fn scan_rows_equal_full_conversion_and_box_filter() {
        let limits = ParseLimits::default();
        for gray in [false, true] {
            for bpc in [1u8, 2, 4, 8, 16] {
                let ncomp = if gray { 1 } else { 3 };
                let row = (37usize * ncomp * bpc as usize).div_ceil(8);
                let bytes: Vec<_> = (0..row * 21).map(|i| (i * 53 + 177) as u8).collect();
                for trim in [0, 1] {
                    let data = &bytes[..bytes.len() - trim];
                    for inverted in [false, true] {
                        let mut d = dict(37, 21, bpc, gray);
                        if inverted {
                            d.insert(
                                zpdf_core::PdfName::new("Decode"),
                                PdfObject::Array(
                                    (0..ncomp)
                                        .flat_map(|_| {
                                            [PdfObject::Integer(1), PdfObject::Integer(0)]
                                        })
                                        .collect(),
                                ),
                            );
                        }
                        let full = decode_image_xobject_resolved_with_limits(
                            data, &d, [0; 3], None, &limits,
                        )
                        .unwrap();
                        for (w, h) in [(11., 8.), (11., 21.), (37., 8.), (1., 1.)] {
                            let transform = zpdf_core::Matrix::new(w, 0., 0., h, 3.25, 1.75);
                            let scan =
                                decode_scan_at_footprint(data, &d, None, &transform, 1., &limits)
                                    .unwrap()
                                    .unwrap();
                            let expected =
                                box_downscale(&full, scan.image.width, scan.image.height);
                            assert_eq!((scan.source_width, scan.source_height), (37, 21));
                            assert_eq!(
                                scan.image.data, expected.data,
                                "{gray}/{bpc}/{trim}/{inverted}/{w}/{h}"
                            );
                            assert_eq!(scan.image.has_alpha, expected.has_alpha);
                            assert_eq!(scan.image.premultiplied, expected.premultiplied);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn scan_dct_geometry_limits_and_fallbacks() {
        let mut d = dict(37, 99, 8, false);
        d.insert(
            zpdf_core::PdfName::new("Filter"),
            PdfObject::Name(zpdf_core::PdfName::new("DCTDecode")),
        );
        let data = vec![71; 37 * 21 * 3];
        let transform = zpdf_core::Matrix::new(11., 0., 0., 8., 0., 0.);
        let mut limits = ParseLimits::default();
        let scan = decode_scan_at_footprint(&data, &d, None, &transform, 1., &limits)
            .unwrap()
            .unwrap();
        let full =
            decode_image_xobject_resolved_with_limits(&data, &d, [0; 3], None, &limits).unwrap();
        assert_eq!(
            (scan.source_width, scan.source_height),
            (full.width, full.height)
        );
        assert_eq!(
            scan.image.data,
            box_downscale(&full, scan.image.width, scan.image.height).data
        );
        limits.max_image_pixels = 1;
        assert!(decode_scan_at_footprint(&data, &d, None, &transform, 1., &limits).is_err());
        limits = ParseLimits::default();
        for key in ["SMask", "Mask"] {
            let mut masked = d.clone();
            masked.insert(
                zpdf_core::PdfName::new(key),
                PdfObject::Ref(zpdf_core::ObjectId(9, 0)),
            );
            assert!(
                decode_scan_at_footprint(&data, &masked, None, &transform, 1., &limits)
                    .unwrap()
                    .is_none()
            );
        }
        assert!(decode_scan_at_footprint(
            &data,
            &d,
            Some(&ResolvedColorSpace::Cmyk),
            &transform,
            1.,
            &limits
        )
        .unwrap()
        .is_none());
        assert!(
            decode_scan_at_footprint(&data, &d, None, &transform, 0., &limits)
                .unwrap()
                .is_none()
        );
        assert!(decode_scan_at_footprint(
            &data,
            &d,
            None,
            &zpdf_core::Matrix::scale(100., 100.),
            1.,
            &limits
        )
        .unwrap()
        .is_none());
    }
}
