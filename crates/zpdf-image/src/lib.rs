use std::collections::HashMap;

use zpdf_core::{Error, PdfDict, PdfObject, Result};

pub type ImageId = u32;

#[derive(Debug)]
pub struct ImageCache {
    images: HashMap<ImageId, DecodedImage>,
    next_id: ImageId,
}

#[derive(Debug, Clone)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    pub has_alpha: bool,
    pub premultiplied: bool,
}

impl ImageCache {
    pub fn new() -> Self {
        Self {
            images: HashMap::new(),
            next_id: 0,
        }
    }

    pub fn get(&self, id: ImageId) -> Option<&DecodedImage> {
        self.images.get(&id)
    }

    pub fn insert(&mut self, image: DecodedImage) -> ImageId {
        let id = self.next_id;
        self.next_id += 1;
        self.images.insert(id, image);
        id
    }
}

impl Default for ImageCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Image colour space pre-resolved by the caller. The interpreter has
/// `PdfFile` access to chase indirect references (ICCBased profiles, Indexed
/// palettes stored in streams); zpdf-image does not, so it consumes this
/// digested form. Pass `None` to [`decode_image_xobject_resolved`] to fall
/// back to inferring the space from the image dictionary alone.
#[derive(Debug, Clone)]
pub enum ResolvedColorSpace {
    Gray,
    Rgb,
    Cmyk,
    /// ICCBased with a compiled profile→sRGB transform built by the caller.
    /// `ncomp` mirrors `transform.components()` for cheap access.
    Icc {
        ncomp: u8,
        transform: std::sync::Arc<zpdf_color::IccTransform>,
    },
    /// `[/Indexed base hival lookup]`: stream samples are palette indices into
    /// `lookup`, which holds `hival + 1` entries of `base.components()` bytes.
    Indexed {
        base: Box<ResolvedColorSpace>,
        hival: u8,
        lookup: Vec<u8>,
    },
}

/// Manual impl because transforms have no structural equality: two `Icc`
/// spaces are equal when they share the same compiled transform (used by the
/// DCT component sniffer and tests, not as a cache key).
impl PartialEq for ResolvedColorSpace {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Gray, Self::Gray) | (Self::Rgb, Self::Rgb) | (Self::Cmyk, Self::Cmyk) => true,
            (
                Self::Icc {
                    ncomp: n1,
                    transform: t1,
                },
                Self::Icc {
                    ncomp: n2,
                    transform: t2,
                },
            ) => n1 == n2 && std::sync::Arc::ptr_eq(t1, t2),
            (
                Self::Indexed {
                    base: b1,
                    hival: h1,
                    lookup: l1,
                },
                Self::Indexed {
                    base: b2,
                    hival: h2,
                    lookup: l2,
                },
            ) => b1 == b2 && h1 == h2 && l1 == l2,
            _ => false,
        }
    }
}

impl ResolvedColorSpace {
    /// Components per pixel in the *image stream* (Indexed samples are single
    /// palette indices regardless of the base space).
    fn components(&self) -> usize {
        match self {
            Self::Gray | Self::Indexed { .. } => 1,
            Self::Rgb => 3,
            Self::Cmyk => 4,
            Self::Icc { ncomp, .. } => (*ncomp).max(1) as usize,
        }
    }
}

/// Map a device/CIE colour-space *name* to its resolved form. Shared with the
/// caller-side resolution helper so the name table lives in one place.
pub fn colorspace_from_name(name: &str) -> Option<ResolvedColorSpace> {
    match name {
        "DeviceGray" | "CalGray" | "G" => Some(ResolvedColorSpace::Gray),
        "DeviceRGB" | "CalRGB" | "RGB" => Some(ResolvedColorSpace::Rgb),
        "DeviceCMYK" | "CMYK" => Some(ResolvedColorSpace::Cmyk),
        _ => None,
    }
}

pub fn decode_image_xobject(decoded_data: &[u8], dict: &PdfDict) -> Result<DecodedImage> {
    decode_image_xobject_resolved(decoded_data, dict, [0, 0, 0], None)
}

/// Decode an image XObject, painting `/ImageMask true` stencils in `fill_rgb`
/// (the current graphics-state fill colour). Non-mask images ignore `fill_rgb`.
pub fn decode_image_xobject_with_fill(
    decoded_data: &[u8],
    dict: &PdfDict,
    fill_rgb: [u8; 3],
) -> Result<DecodedImage> {
    decode_image_xobject_resolved(decoded_data, dict, fill_rgb, None)
}

/// Decode an image XObject into RGBA.
///
/// * `decoded_data` — stream bytes after the parser's filter pipeline.
/// * `fill_rgb` — fill colour painted by `/ImageMask true` stencils.
/// * `colorspace` — pre-resolved colour space from the caller; `None` infers a
///   best-effort space from `/ColorSpace` entries that need no indirect access.
///
/// Honours `/Decode` arrays and `/Mask` colour-key arrays (raw-sample ranges,
/// compared before colour conversion). `/SMask` and `/Mask` stencil *streams*
/// are separate objects the caller must decode and fold in afterwards with
/// [`apply_smask_image`] / [`apply_stencil_mask`].
pub fn decode_image_xobject_resolved(
    decoded_data: &[u8],
    dict: &PdfDict,
    fill_rgb: [u8; 3],
    colorspace: Option<ResolvedColorSpace>,
) -> Result<DecodedImage> {
    let width = dict.get_i64("Width")? as u32;
    let height = dict.get_i64("Height")? as u32;
    let bpc = dict.get_i64("BitsPerComponent").unwrap_or(8) as u8;

    // Bound the RGBA allocation (4 bytes/pixel) against the ParseLimits default
    // so a crafted /Width × /Height cannot OOM — applies to every image path,
    // including the 1-bpp stencil below.
    if (width as u64).saturating_mul(height as u64) > MAX_IMAGE_PIXELS {
        return Err(Error::StreamDecode(format!(
            "image {width}x{height} exceeds the {MAX_IMAGE_PIXELS}-pixel limit"
        )));
    }

    // Stencil mask: 1 bpc, paints `fill_rgb` where the sample selects the page
    // (default /Decode [0 1] → sample 0 paints; [1 0] inverts).
    if is_image_mask(dict) {
        let invert = mask_decode_inverts(dict);
        return decode_image_mask(decoded_data, width, height, fill_rgb, invert);
    }

    // JPEG 2000 first: the codestream carries its own dimensions, colour
    // space, bit depth and alpha, and /ColorSpace + /BitsPerComponent may
    // legally be absent from the dict (spec 7.4.9). A present /ColorSpace
    // overrides the codestream's colour declaration.
    if is_jpx_encoded(dict, decoded_data) {
        return decode_jpx_image(decoded_data, dict, colorspace.as_ref());
    }

    let cs = colorspace.unwrap_or_else(|| infer_colorspace(dict));

    if is_dct_encoded(dict) {
        return decode_dct_image(decoded_data, width, height, &cs, dict);
    }

    if !matches!(bpc, 1 | 2 | 4 | 8 | 16) {
        return Err(Error::StreamDecode(format!(
            "invalid BitsPerComponent {bpc}"
        )));
    }

    let ncomp = cs.components();
    let decode = decode_array(dict, ncomp);
    let color_key = color_key_ranges(dict, ncomp);
    decode_raw_samples(
        decoded_data,
        width,
        height,
        bpc,
        &cs,
        decode.as_deref(),
        color_key.as_deref(),
    )
}

/// Mirrors `ParseLimits::default().max_image_pixels` without depending on it
/// at this layer.
const MAX_IMAGE_PIXELS: u64 = 100_000_000;

/// Fold a decoded `/SMask` (luminosity soft mask) into `image` as alpha. The
/// mask must come through the same full image-decode path as any image (so
/// filters, predictors, sub-byte bpc and /Decode all apply); the decoded gray
/// level is the alpha. A mask whose dimensions differ from the image is
/// bilinearly resampled to the image size. RGB is premultiplied as the alpha
/// folds in, because both backends (tiny-skia `PixmapRef::from_bytes` and the
/// wgpu `Rgba8Unorm` upload) treat the bytes as premultiplied RGBA.
pub fn apply_smask_image(image: &mut DecodedImage, mask: &DecodedImage) {
    // The mask is DeviceGray, so its decoded R channel is the gray level.
    let alpha: Vec<u8> = mask.data.chunks_exact(4).map(|px| px[0]).collect();
    fold_alpha_plane(image, &alpha, mask.width, mask.height);
}

/// Fold a `/Mask` stencil stream (1 bpc) into `image`: per spec 8.9.6.4 a
/// sample of 1 masks the pixel out (default /Decode [0 1]); `invert` flips the
/// polarity (/Decode [1 0]). The stencil is resampled to the image size when
/// dimensions differ, and RGB is premultiplied as the alpha folds in.
pub fn apply_stencil_mask(
    image: &mut DecodedImage,
    mask_data: &[u8],
    mask_width: u32,
    mask_height: u32,
    invert: bool,
) {
    if (mask_width as u64).saturating_mul(mask_height as u64) > MAX_IMAGE_PIXELS {
        tracing::warn!("/Mask stencil {mask_width}x{mask_height} exceeds the pixel limit, ignored");
        return;
    }
    let row_bytes = (mask_width as usize).div_ceil(8);
    let mut alpha = Vec::with_capacity(mask_width as usize * mask_height as usize);
    for row in 0..mask_height as usize {
        for col in 0..mask_width as usize {
            // Out-of-range (short data) reads as sample 1 → masked out.
            let byte = mask_data
                .get(row * row_bytes + col / 8)
                .copied()
                .unwrap_or(0xFF);
            let sample = (byte >> (7 - (col % 8))) & 1;
            let masked = (sample == 1) != invert;
            alpha.push(if masked { 0 } else { 255 });
        }
    }
    fold_alpha_plane(image, &alpha, mask_width, mask_height);
}

/// An ImageMask's (or `/Mask` stencil's) `/Decode [1 0]` inverts which sample
/// value paints. Default `[0 1]` means sample 0 paints (marks the page).
pub fn mask_decode_inverts(dict: &PdfDict) -> bool {
    match dict.get("Decode") {
        Some(PdfObject::Array(a)) => {
            let first = a.first().and_then(|o| match o {
                PdfObject::Integer(n) => Some(*n),
                PdfObject::Real(r) => Some(*r as i64),
                _ => None,
            });
            first == Some(1)
        }
        _ => false,
    }
}

fn is_image_mask(dict: &PdfDict) -> bool {
    matches!(dict.get("ImageMask"), Some(PdfObject::Bool(true)))
}

/// Build a stencil: paint `fill` (opaque) where the 1-bpp sample selects the
/// page, transparent elsewhere. `invert` flips the paint polarity (`/Decode
/// [1 0]`). Output alpha is 0 or 255, so the bytes are valid premultiplied
/// RGBA for the rasterizer.
fn decode_image_mask(
    data: &[u8],
    width: u32,
    height: u32,
    fill: [u8; 3],
    invert: bool,
) -> Result<DecodedImage> {
    let row_bytes = (width as usize).div_ceil(8);
    let mut rgba = Vec::with_capacity((width as usize) * (height as usize) * 4);
    for row in 0..height as usize {
        for col in 0..width as usize {
            let byte_idx = row * row_bytes + col / 8;
            // Out-of-range (short data) reads as sample 1 → unpainted.
            let sample = if byte_idx < data.len() {
                (data[byte_idx] >> (7 - (col % 8))) & 1
            } else {
                1
            };
            let paint = if invert { sample == 1 } else { sample == 0 };
            if paint {
                rgba.extend_from_slice(&[fill[0], fill[1], fill[2], 255]);
            } else {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
    Ok(DecodedImage {
        width,
        height,
        data: rgba,
        has_alpha: true,
        premultiplied: true,
    })
}

/// Best-effort colour-space inference from the image dict alone, used when the
/// caller supplies no [`ResolvedColorSpace`]. Cannot chase indirect refs, so
/// ICCBased guesses RGB (unless an inline profile stream exposes `/N`) and
/// Indexed only resolves for fully inline `[/Indexed name hival string]`.
fn infer_colorspace(dict: &PdfDict) -> ResolvedColorSpace {
    match dict.get("ColorSpace") {
        Some(PdfObject::Name(n)) => {
            colorspace_from_name(n.as_str()).unwrap_or(ResolvedColorSpace::Gray)
        }
        Some(PdfObject::Array(arr)) => infer_colorspace_array(arr),
        // No /ColorSpace at all: RGB is the least-bad guess for 3-ish data.
        _ => ResolvedColorSpace::Rgb,
    }
}

fn infer_colorspace_array(arr: &[PdfObject]) -> ResolvedColorSpace {
    let head = match arr.first() {
        Some(PdfObject::Name(n)) => n.as_str(),
        _ => return ResolvedColorSpace::Gray,
    };
    if let Some(cs) = colorspace_from_name(head) {
        return cs;
    }
    match head {
        "ICCBased" => {
            // /N lives in the profile stream's dict; only an inline stream is
            // visible here. Most ICC image profiles are RGB, so default there.
            if let Some(PdfObject::Stream(s)) = arr.get(1) {
                match s.dict.get_i64("N") {
                    Ok(1) => return ResolvedColorSpace::Gray,
                    Ok(4) => return ResolvedColorSpace::Cmyk,
                    _ => {}
                }
            }
            ResolvedColorSpace::Rgb
        }
        "Indexed" | "I" => {
            let base = arr.get(1).and_then(|o| match o {
                PdfObject::Name(n) => colorspace_from_name(n.as_str()),
                _ => None,
            });
            let hival = arr.get(2).and_then(|o| o.as_f64().ok());
            let lookup = arr.get(3).and_then(|o| match o {
                PdfObject::String(s) => Some(s.as_bytes().to_vec()),
                _ => None,
            });
            match (base, hival, lookup) {
                (Some(base), Some(h), Some(lookup)) if (0.0..=255.0).contains(&h) => {
                    ResolvedColorSpace::Indexed {
                        base: Box::new(base),
                        hival: h as u8,
                        lookup,
                    }
                }
                _ => {
                    tracing::warn!("unresolvable /Indexed colour space, treating samples as gray");
                    ResolvedColorSpace::Gray
                }
            }
        }
        other => {
            tracing::warn!("unsupported colour space {other}, treating as gray");
            ResolvedColorSpace::Gray
        }
    }
}

/// JP2 signature box — the first 12 bytes of a `.jp2` container file.
const JP2_SIGNATURE: &[u8] = b"\x00\x00\x00\x0C\x6A\x50\x20\x20\x0D\x0A\x87\x0A";
/// Raw JPEG 2000 codestream — SOC marker immediately followed by SIZ.
const J2K_SOC_SIZ: &[u8] = b"\xFF\x4F\xFF\x51";

/// A JPX image is detected by `/Filter` naming `JPXDecode` (the parser's
/// filter chain passes the codestream bytes through untouched), or — for
/// robustness against broken producers — by the post-filter data starting
/// with the JP2 signature box or a bare SOC+SIZ codestream.
fn is_jpx_encoded(dict: &PdfDict, data: &[u8]) -> bool {
    let filter_says = match dict.get("Filter") {
        Some(PdfObject::Name(n)) => n.as_str() == "JPXDecode",
        Some(PdfObject::Array(arr)) => arr
            .iter()
            .any(|o| matches!(o, PdfObject::Name(n) if n.as_str() == "JPXDecode")),
        _ => false,
    };
    filter_says || data.starts_with(JP2_SIGNATURE) || data.starts_with(J2K_SOC_SIZ)
}

/// Decode a JPEG 2000 (JPXDecode) image to RGBA via hayro-jpeg2000.
///
/// The codestream is authoritative for dimensions, colour space and alpha;
/// the PDF dict only contributes `/SMaskInData` (spec 7.4.9: 0/absent =
/// ignore any codestream soft mask, 1 = alpha is unpremultiplied, 2 = colour
/// channels are already premultiplied). `/Decode` is only meaningful for
/// Indexed JPX images per spec table 89 and is not applied here.
fn decode_jpx_image(
    data: &[u8],
    dict: &PdfDict,
    colorspace: Option<&ResolvedColorSpace>,
) -> Result<DecodedImage> {
    use hayro_jpeg2000::{ColorSpace as JpxColorSpace, DecodeSettings, Image};
    use std::panic::{catch_unwind, AssertUnwindSafe};

    // hayro-jpeg2000 is a third-party decoder that panics (slice-index OOB) on
    // some malformed/unusual JPX codestreams; input pre-validation cannot cover
    // all of its internal invariants. Catch the unwind and surface a clean Err
    // so a single bad image is skipped rather than aborting the whole render.
    let image = catch_unwind(AssertUnwindSafe(|| {
        Image::new(data, &DecodeSettings::default())
    }))
    .map_err(|_| {
        Error::StreamDecode("JPXDecode: decoder panicked parsing codestream header".into())
    })?
    .map_err(|e| Error::StreamDecode(format!("JPXDecode: {e}")))?;

    // Re-check the pixel limit against the *codestream* header dims, which
    // are authoritative and may differ from the dict's /Width × /Height.
    let (width, height) = (image.width(), image.height());
    let pixel_count = (width as u64).saturating_mul(height as u64);
    if pixel_count == 0 || pixel_count > MAX_IMAGE_PIXELS {
        return Err(Error::StreamDecode(format!(
            "JPX codestream {width}x{height} exceeds the {MAX_IMAGE_PIXELS}-pixel limit"
        )));
    }
    let dict_w = dict.get_i64("Width").unwrap_or(width as i64);
    let dict_h = dict.get_i64("Height").unwrap_or(height as i64);
    if (dict_w, dict_h) != (width as i64, height as i64) {
        tracing::warn!(
            "JPX codestream is {width}x{height} but the dict says {dict_w}x{dict_h}; \
             using the codestream dimensions"
        );
    }

    // Colour channels per pixel (alpha excluded). An ICC profile embedded in
    // the JP2 codestream is captured here and compiled below (unless a
    // PDF-level /ColorSpace overrides it per spec 7.4.9).
    let supported_ncolor = |n: u8| -> Result<usize> {
        match n {
            1 => Ok(1),
            3 => Ok(3),
            4 => Ok(4),
            n => Err(Error::StreamDecode(format!(
                "JPX image with unsupported channel count {n}"
            ))),
        }
    };
    let mut codestream_icc: Option<Vec<u8>> = None;
    let ncolor = match image.color_space() {
        JpxColorSpace::Gray => 1usize,
        JpxColorSpace::RGB => 3,
        JpxColorSpace::CMYK => 4,
        JpxColorSpace::Icc {
            profile,
            num_channels,
        } => {
            let n = supported_ncolor(*num_channels)?;
            codestream_icc = Some(profile.clone());
            n
        }
        JpxColorSpace::Unknown { num_channels } => supported_ncolor(*num_channels)?,
    };
    let has_alpha = image.has_alpha();
    let channels = ncolor + usize::from(has_alpha);

    let samples = catch_unwind(AssertUnwindSafe(|| image.decode()))
        .map_err(|_| Error::StreamDecode("JPXDecode: decoder panicked decoding codestream".into()))?
        .map_err(|e| Error::StreamDecode(format!("JPXDecode: {e}")))?;
    if samples.len() < pixel_count as usize * channels {
        return Err(Error::StreamDecode(format!(
            "JPX decoded data too short: {} bytes for {width}x{height}x{channels}",
            samples.len()
        )));
    }

    let smask_in_data = dict.get_i64("SMaskInData").unwrap_or(0);
    let use_alpha = has_alpha && smask_in_data != 0;
    if has_alpha && !use_alpha {
        tracing::warn!("JPX codestream has an alpha channel but /SMaskInData is 0/absent; ignoring it per spec");
    }
    if dict.get("Decode").is_some() {
        tracing::debug!("/Decode on a JPXDecode image is ignored (non-Indexed)");
    }

    // A present /ColorSpace overrides the codestream's colour declaration
    // (7.4.9) when its component count matches the colour channel count —
    // required for Indexed palettes and ICC-managed spaces.
    let override_cs = colorspace.filter(|cs| {
        let n = cs.components();
        if n == ncolor {
            true
        } else {
            tracing::warn!(
                "JPX /ColorSpace expects {n} components but the codestream has {ncolor}; \
                 using the codestream interpretation"
            );
            false
        }
    });

    // With no overriding PDF /ColorSpace, honour an ICC profile embedded in the
    // JP2 codestream by compiling it (media-relative colorimetric — the
    // graphics-state rendering intent is not threaded into image decoding).
    let codestream_cs = match (override_cs, &codestream_icc) {
        (None, Some(profile)) => {
            match zpdf_color::IccTransform::from_profile_bytes(
                profile,
                zpdf_color::RenderIntent::default(),
            ) {
                Ok(t) if t.components() == ncolor => Some(ResolvedColorSpace::Icc {
                    ncomp: ncolor as u8,
                    transform: std::sync::Arc::new(t),
                }),
                Ok(t) => {
                    tracing::warn!(
                        "JPX codestream ICC has {} components but {ncolor} colour channels; \
                         using channel-count fallback",
                        t.components()
                    );
                    None
                }
                Err(e) => {
                    tracing::warn!(
                        "JPX codestream ICC profile rejected: {e}; using channel-count fallback"
                    );
                    None
                }
            }
        }
        _ => None,
    };
    let effective_cs = override_cs.or(codestream_cs.as_ref());

    let mut rgba = Vec::with_capacity(pixel_count as usize * 4);
    for px in samples.chunks_exact(channels).take(pixel_count as usize) {
        let [mut r, mut g, mut b] = match effective_cs {
            Some(cs) => {
                let mut comps = [0u8; 4];
                comps[..ncolor].copy_from_slice(&px[..ncolor]);
                if let ResolvedColorSpace::Indexed { hival, .. } = cs {
                    comps[0] = comps[0].min(*hival);
                }
                components_to_rgb(cs, &comps)
            }
            None => match ncolor {
                1 => [px[0]; 3],
                3 => [px[0], px[1], px[2]],
                _ => cmyk_to_rgb(&[px[0], px[1], px[2], px[3]]),
            },
        };
        let a = if use_alpha { px[ncolor] } else { 255 };
        // /SMaskInData 2 means the codestream colour channels are already
        // premultiplied; 1 means we premultiply here (both render backends
        // treat the RGBA bytes as premultiplied).
        if use_alpha && smask_in_data != 2 {
            r = mul255(r, a);
            g = mul255(g, a);
            b = mul255(b, a);
        }
        rgba.extend_from_slice(&[r, g, b, a]);
    }

    Ok(DecodedImage {
        width,
        height,
        data: rgba,
        has_alpha: use_alpha,
        premultiplied: use_alpha,
    })
}

fn is_dct_encoded(dict: &PdfDict) -> bool {
    match dict.get("Filter") {
        Some(PdfObject::Name(n)) => matches!(n.as_str(), "DCTDecode" | "DCT"),
        Some(PdfObject::Array(arr)) => arr
            .iter()
            .any(|o| matches!(o, PdfObject::Name(n) if matches!(n.as_str(), "DCTDecode" | "DCT"))),
        _ => false,
    }
}

fn decode_dct_image(
    decoded_data: &[u8],
    width: u32,
    height: u32,
    cs: &ResolvedColorSpace,
    dict: &PdfDict,
) -> Result<DecodedImage> {
    // DCTDecode was already run by the parser's filter pipeline (zune-jpeg with
    // default options), which converts YCbCr/CMYK/YCCK to RGB — including the
    // Adobe APP14 transform and CMYK inversion — so the data is normally
    // 3 bytes/pixel regardless of the PDF colour space. Sniff the component
    // count from the data length to stay robust if that ever changes. An ICC
    // space whose component count matches the sniffed one is kept, so its
    // transform applies to the JPEG's output samples.
    let pixel_count = (width as usize) * (height as usize);
    let cs_is_cmyk =
        *cs == ResolvedColorSpace::Cmyk || matches!(cs, ResolvedColorSpace::Icc { ncomp: 4, .. });
    let (sniffed, ncomp) = if decoded_data.len() >= pixel_count * 4 && cs_is_cmyk {
        (cs.clone(), 4)
    } else if decoded_data.len() >= pixel_count * 3 {
        match cs {
            ResolvedColorSpace::Icc { ncomp: 3, .. } => (cs.clone(), 3),
            _ => (ResolvedColorSpace::Rgb, 3),
        }
    } else if decoded_data.len() >= pixel_count {
        match cs {
            ResolvedColorSpace::Icc { ncomp: 1, .. } => (cs.clone(), 1),
            _ => (ResolvedColorSpace::Gray, 1),
        }
    } else {
        return Err(Error::StreamDecode(format!(
            "DCT decoded data too short: {} bytes for {}x{} image",
            decoded_data.len(),
            width,
            height
        )));
    };

    // /Decode and colour-key /Mask entries apply to the JPEG's output
    // components; only honour them when their length matches what the JPEG
    // actually produced (a 4-component CMYK /Decode on zune's RGB output would
    // double-invert). The mismatching case is logged inside the extractors.
    let decode = decode_array(dict, ncomp);
    let color_key = color_key_ranges(dict, ncomp);
    decode_raw_samples(
        decoded_data,
        width,
        height,
        8,
        &sniffed,
        decode.as_deref(),
        color_key.as_deref(),
    )
}

/// `/Decode` as `[min, max]` pairs per component, if present with the
/// expected length.
fn decode_array(dict: &PdfDict, ncomp: usize) -> Option<Vec<f32>> {
    let arr = match dict.get("Decode") {
        Some(PdfObject::Array(a)) => a,
        _ => return None,
    };
    let vals: Vec<f32> = arr
        .iter()
        .filter_map(|o| o.as_f64().ok().map(|v| v as f32))
        .collect();
    if vals.len() != ncomp * 2 {
        tracing::warn!(
            "/Decode has {} numbers, expected {} — ignoring",
            vals.len(),
            ncomp * 2
        );
        return None;
    }
    Some(vals)
}

/// `/Mask [min₁ max₁ …]` colour-key ranges, compared against RAW sample values
/// (before /Decode is applied), per spec 8.9.6.4.
fn color_key_ranges(dict: &PdfDict, ncomp: usize) -> Option<Vec<(u16, u16)>> {
    let arr = match dict.get("Mask") {
        Some(PdfObject::Array(a)) => a,
        _ => return None,
    };
    let vals: Vec<u16> = arr
        .iter()
        .filter_map(|o| o.as_f64().ok().map(|v| v.clamp(0.0, 65535.0) as u16))
        .collect();
    if vals.len() != ncomp * 2 {
        tracing::warn!(
            "colour-key /Mask has {} numbers, expected {} — ignoring",
            vals.len(),
            ncomp * 2
        );
        return None;
    }
    Some(vals.chunks_exact(2).map(|p| (p[0], p[1])).collect())
}

/// Decode packed raw samples (post-filter) into RGBA: unpack 1/2/4/8/16-bpc
/// components (rows are byte-aligned per spec), apply colour-key masking on
/// the raw values, map through /Decode, then convert to RGB.
fn decode_raw_samples(
    data: &[u8],
    width: u32,
    height: u32,
    bpc: u8,
    cs: &ResolvedColorSpace,
    decode: Option<&[f32]>,
    color_key: Option<&[(u16, u16)]>,
) -> Result<DecodedImage> {
    if let ResolvedColorSpace::Icc { transform, .. } = cs {
        return decode_raw_samples_icc(data, width, height, bpc, cs, transform, decode, color_key);
    }

    let ncomp = cs.components();
    let pixel_count = (width as usize) * (height as usize);
    let luts = build_component_luts(bpc, cs, decode);

    let row_bytes = (width as usize * ncomp * bpc as usize).div_ceil(8);
    let mut rgba = Vec::with_capacity(pixel_count * 4);
    let mut any_masked = false;

    for row in 0..height as usize {
        let row_start = row * row_bytes;
        let mut bit = 0usize;
        for _col in 0..width {
            let mut raw = [0u16; 4];
            for r in raw.iter_mut().take(ncomp) {
                *r = read_sample(data, row_start, &mut bit, bpc);
            }
            let masked = color_key.is_some_and(|ranges| {
                ranges
                    .iter()
                    .zip(&raw)
                    .all(|(&(lo, hi), &v)| lo <= v && v <= hi)
            });
            if masked {
                any_masked = true;
                rgba.extend_from_slice(&[0, 0, 0, 0]);
                continue;
            }
            let mut comps = [0u8; 4];
            for c in 0..ncomp {
                let idx = if bpc == 16 {
                    (raw[c] >> 8) as usize
                } else {
                    raw[c] as usize
                };
                comps[c] = luts[c][idx];
            }
            let [r, g, b] = components_to_rgb(cs, &comps);
            rgba.extend_from_slice(&[r, g, b, 255]);
        }
    }

    Ok(DecodedImage {
        width,
        height,
        data: rgba,
        has_alpha: any_masked,
        // Colour-key holes are transparent black, which is valid premultiplied
        // RGBA — the backends treat the bytes as premultiplied.
        premultiplied: any_masked,
    })
}

/// ICC variant of [`decode_raw_samples`]: unpack and /Decode-map the whole
/// image into a component buffer first, run ONE buffer-level CMS transform
/// (far cheaper than per-pixel calls), then interleave RGBA. Colour-key
/// masking still applies to the raw samples; masked pixels' transformed RGB
/// is simply discarded.
#[allow(clippy::too_many_arguments)]
fn decode_raw_samples_icc(
    data: &[u8],
    width: u32,
    height: u32,
    bpc: u8,
    cs: &ResolvedColorSpace,
    transform: &zpdf_color::IccTransform,
    decode: Option<&[f32]>,
    color_key: Option<&[(u16, u16)]>,
) -> Result<DecodedImage> {
    let ncomp = cs.components();
    let pixel_count = (width as usize) * (height as usize);
    let luts = build_component_luts(bpc, cs, decode);
    let row_bytes = (width as usize * ncomp * bpc as usize).div_ceil(8);

    let mut comps = Vec::with_capacity(pixel_count * ncomp);
    let mut masked = vec![false; if color_key.is_some() { pixel_count } else { 0 }];
    let mut any_masked = false;

    for row in 0..height as usize {
        let row_start = row * row_bytes;
        let mut bit = 0usize;
        for col in 0..width as usize {
            let mut raw = [0u16; 4];
            for r in raw.iter_mut().take(ncomp) {
                *r = read_sample(data, row_start, &mut bit, bpc);
            }
            if let Some(ranges) = color_key {
                if ranges
                    .iter()
                    .zip(&raw)
                    .all(|(&(lo, hi), &v)| lo <= v && v <= hi)
                {
                    masked[row * width as usize + col] = true;
                    any_masked = true;
                }
            }
            for c in 0..ncomp {
                let idx = if bpc == 16 {
                    (raw[c] >> 8) as usize
                } else {
                    raw[c] as usize
                };
                comps.push(luts[c][idx]);
            }
        }
    }

    let mut rgb = vec![0u8; pixel_count * 3];
    transform.slice_to_rgb(&comps, &mut rgb)?;

    let mut rgba = Vec::with_capacity(pixel_count * 4);
    for (i, px) in rgb.chunks_exact(3).enumerate() {
        if any_masked && masked[i] {
            rgba.extend_from_slice(&[0, 0, 0, 0]);
        } else {
            rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
        }
    }

    Ok(DecodedImage {
        width,
        height,
        data: rgba,
        has_alpha: any_masked,
        // Colour-key holes are transparent black, which is valid premultiplied
        // RGBA — the backends treat the bytes as premultiplied.
        premultiplied: any_masked,
    })
}

/// Per-component lookup tables mapping a raw sample (or its high byte for
/// 16 bpc) to its decoded 8-bit value. Defaults follow /Decode `[0 1]` per
/// component, so 1/2/4-bpc values scale to 0..=255 (DeviceGray sample 0 is
/// black) and 16 bpc keeps the high byte. For Indexed spaces the table yields
/// the palette *index* (default /Decode `[0 2^bpc − 1]`), clamped to `hival`.
fn build_component_luts(bpc: u8, cs: &ResolvedColorSpace, decode: Option<&[f32]>) -> Vec<Vec<u8>> {
    let ncomp = cs.components();
    let lut_max: usize = if bpc == 16 { 255 } else { (1usize << bpc) - 1 };
    let maxf = lut_max as f32;
    (0..ncomp)
        .map(|c| {
            let (dmin, dmax) = match cs {
                ResolvedColorSpace::Indexed { .. } => {
                    // /Decode is expressed in index units for Indexed spaces.
                    let full = if bpc == 16 { 65535.0 } else { maxf };
                    decode.map_or((0.0, full), |d| (d[c * 2], d[c * 2 + 1]))
                }
                _ => decode.map_or((0.0, 1.0), |d| (d[c * 2], d[c * 2 + 1])),
            };
            (0..=lut_max)
                .map(|raw| {
                    let v = dmin + (raw as f32 / maxf) * (dmax - dmin);
                    match cs {
                        ResolvedColorSpace::Indexed { hival, .. } => {
                            v.round().clamp(0.0, *hival as f32) as u8
                        }
                        _ => (v.clamp(0.0, 1.0) * 255.0).round() as u8,
                    }
                })
                .collect()
        })
        .collect()
}

/// Read one `bpc`-bit sample at bit offset `*bit` within the row starting at
/// byte `row_start`, advancing the cursor. Short data reads as 0 (black).
fn read_sample(data: &[u8], row_start: usize, bit: &mut usize, bpc: u8) -> u16 {
    let offset = *bit;
    *bit += bpc as usize;
    let byte_idx = row_start + offset / 8;
    match bpc {
        16 => {
            let hi = data.get(byte_idx).copied().unwrap_or(0) as u16;
            let lo = data.get(byte_idx + 1).copied().unwrap_or(0) as u16;
            (hi << 8) | lo
        }
        8 => data.get(byte_idx).copied().unwrap_or(0) as u16,
        // 1/2/4 bpc divide a byte evenly, so a sample never spans bytes.
        _ => {
            let byte = data.get(byte_idx).copied().unwrap_or(0);
            let shift = 8 - bpc as usize - (offset % 8);
            ((byte >> shift) & ((1u8 << bpc) - 1)) as u16
        }
    }
}

/// Convert one pixel's decoded components to RGB. For Indexed spaces,
/// `comps[0]` is the (already clamped) palette index.
fn components_to_rgb(cs: &ResolvedColorSpace, comps: &[u8; 4]) -> [u8; 3] {
    match cs {
        ResolvedColorSpace::Gray => [comps[0]; 3],
        ResolvedColorSpace::Rgb => [comps[0], comps[1], comps[2]],
        ResolvedColorSpace::Cmyk => cmyk_to_rgb(comps),
        // Whole images take the buffer-level path in decode_raw_samples_icc;
        // this per-pixel call serves Indexed bases built without the
        // interpreter's palette pre-baking.
        ResolvedColorSpace::Icc { transform, .. } => transform.comps8_to_rgb8(comps),
        ResolvedColorSpace::Indexed { base, lookup, .. } => {
            let n = base.components();
            let off = comps[0] as usize * n;
            let mut bc = [0u8; 4];
            for (i, b) in bc.iter_mut().enumerate().take(n) {
                // A short palette reads as 0 rather than failing the image.
                *b = lookup.get(off + i).copied().unwrap_or(0);
            }
            // Recursion terminates: `base` is a strictly smaller tree (and an
            // Indexed base of Indexed is illegal per spec 8.6.6.3 anyway).
            components_to_rgb(base, &bc)
        }
    }
}

/// Convert an 8-bit DeviceCMYK pixel to 8-bit sRGB via the shared Adobe
/// polynomial (the non-ICC path; ICC-tagged CMYK is converted upstream through
/// the moxcms transform). Single source of truth in [`zpdf_color::cmyk_to_rgb`].
fn cmyk_to_rgb(comps: &[u8; 4]) -> [u8; 3] {
    let (r, g, b) = zpdf_color::cmyk_to_rgb(
        comps[0] as f64 / 255.0,
        comps[1] as f64 / 255.0,
        comps[2] as f64 / 255.0,
        comps[3] as f64 / 255.0,
    );
    [
        (r * 255.0).round() as u8,
        (g * 255.0).round() as u8,
        (b * 255.0).round() as u8,
    ]
}

/// Fold an 8-bit alpha plane into `image`, bilinearly resampling when the
/// dimensions differ. Multiplies into any existing alpha and scales RGB by the
/// same factor, keeping the buffer premultiplied (what both render backends
/// expect of the bytes).
fn fold_alpha_plane(image: &mut DecodedImage, alpha: &[u8], aw: u32, ah: u32) {
    if image.width == 0 || image.height == 0 || aw == 0 || ah == 0 {
        return;
    }
    let resampled;
    let plane: &[u8] = if (aw, ah) == (image.width, image.height) {
        alpha
    } else {
        resampled = resample_bilinear(alpha, aw, ah, image.width, image.height);
        &resampled
    };
    for (px, &a) in image.data.chunks_exact_mut(4).zip(plane) {
        for ch in px.iter_mut() {
            *ch = mul255(*ch, a);
        }
    }
    image.has_alpha = true;
    image.premultiplied = true;
}

/// `round(v * a / 255)` without going through floats.
#[inline]
fn mul255(v: u8, a: u8) -> u8 {
    ((v as u32 * a as u32 + 127) / 255) as u8
}

/// Bilinear resample of a single-channel plane (fits mask planes onto the
/// image they soften). Missing source data reads as opaque (255).
fn resample_bilinear(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    let (sw_u, sh_u) = (sw as usize, sh as usize);
    let sample =
        |x: usize, y: usize| -> f32 { src.get(y * sw_u + x).copied().unwrap_or(255) as f32 };
    let mut out = Vec::with_capacity(dw as usize * dh as usize);
    for dy in 0..dh {
        let fy = ((dy as f32 + 0.5) * sh as f32 / dh as f32 - 0.5).clamp(0.0, (sh - 1) as f32);
        let y0 = fy.floor() as usize;
        let y1 = (y0 + 1).min(sh_u - 1);
        let ty = fy - y0 as f32;
        for dx in 0..dw {
            let fx = ((dx as f32 + 0.5) * sw as f32 / dw as f32 - 0.5).clamp(0.0, (sw - 1) as f32);
            let x0 = fx.floor() as usize;
            let x1 = (x0 + 1).min(sw_u - 1);
            let tx = fx - x0 as f32;
            let top = sample(x0, y0) * (1.0 - tx) + sample(x1, y0) * tx;
            let bot = sample(x0, y1) * (1.0 - tx) + sample(x1, y1) * tx;
            out.push((top * (1.0 - ty) + bot * ty).round() as u8);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use zpdf_core::PdfName;

    fn image_dict(width: i64, height: i64, bpc: i64, cs_name: Option<&str>) -> PdfDict {
        let mut d = PdfDict::new();
        d.insert(PdfName::new("Width"), PdfObject::Integer(width));
        d.insert(PdfName::new("Height"), PdfObject::Integer(height));
        d.insert(PdfName::new("BitsPerComponent"), PdfObject::Integer(bpc));
        if let Some(n) = cs_name {
            d.insert(PdfName::new("ColorSpace"), PdfObject::Name(PdfName::new(n)));
        }
        d
    }

    fn int_array(vals: &[i64]) -> PdfObject {
        PdfObject::Array(vals.iter().map(|&v| PdfObject::Integer(v)).collect())
    }

    fn pixel(img: &DecodedImage, i: usize) -> &[u8] {
        &img.data[i * 4..i * 4 + 4]
    }

    #[test]
    fn rgb8_to_rgba() {
        let samples = vec![255, 0, 0, 0, 255, 0, 0, 0, 255, 128, 128, 128];
        let dict = image_dict(2, 2, 8, Some("DeviceRGB"));
        let img = decode_image_xobject(&samples, &dict).unwrap();
        assert_eq!(img.width, 2);
        assert_eq!(img.height, 2);
        assert_eq!(img.data.len(), 16);
        assert_eq!(pixel(&img, 0), &[255, 0, 0, 255]); // red
        assert_eq!(pixel(&img, 1), &[0, 255, 0, 255]); // green
    }

    #[test]
    fn gray8_to_rgba() {
        let samples = vec![0, 128, 255, 64];
        let dict = image_dict(2, 2, 8, Some("DeviceGray"));
        let img = decode_image_xobject(&samples, &dict).unwrap();
        assert_eq!(pixel(&img, 0), &[0, 0, 0, 255]);
        assert_eq!(pixel(&img, 1), &[128, 128, 128, 255]);
    }

    #[test]
    fn cmyk8_to_rgba() {
        // Pure black in CMYK: C=0, M=0, Y=0, K=255. The Adobe DeviceCMYK
        // polynomial renders 100% K as a dark near-black, not pure (0,0,0).
        let samples = vec![0, 0, 0, 255];
        let dict = image_dict(1, 1, 8, Some("DeviceCMYK"));
        let img = decode_image_xobject(&samples, &dict).unwrap();
        assert_eq!(pixel(&img, 0), &[44, 46, 53, 255]);
    }

    // ---- JPX signature sniffing (decode itself is tested in tests/jpx.rs) ----

    #[test]
    fn jpx_sniffing_filter_name_and_array() {
        let mut d = image_dict(1, 1, 8, None);
        d.insert(
            PdfName::new("Filter"),
            PdfObject::Name(PdfName::new("JPXDecode")),
        );
        assert!(is_jpx_encoded(&d, b""));

        // Filter chains keep the name in an array (e.g. [/FlateDecode /JPXDecode]).
        let mut d = image_dict(1, 1, 8, None);
        d.insert(
            PdfName::new("Filter"),
            PdfObject::Array(vec![
                PdfObject::Name(PdfName::new("FlateDecode")),
                PdfObject::Name(PdfName::new("JPXDecode")),
            ]),
        );
        assert!(is_jpx_encoded(&d, b""));

        let d = image_dict(1, 1, 8, None);
        assert!(!is_jpx_encoded(&d, b"plain sample bytes"));
    }

    #[test]
    fn jpx_sniffing_magic_bytes() {
        let d = image_dict(1, 1, 8, None);
        assert!(is_jpx_encoded(
            &d,
            b"\x00\x00\x00\x0C\x6A\x50\x20\x20\x0D\x0A\x87\x0Arest"
        ));
        assert!(is_jpx_encoded(&d, b"\xFF\x4F\xFF\x51rest"));
        // A truncated signature box must not match.
        assert!(!is_jpx_encoded(&d, b"\x00\x00\x00\x0C\x6A\x50\x20\x20"));
        assert!(!is_jpx_encoded(&d, b"\xFF\x4F\xFF\x52"));
    }

    // ---- item 1: bitonal polarity + 2/4/16 bpc expansion ----

    #[test]
    fn gray1_polarity_zero_is_black() {
        // 2x2 image: bits 1,0 / 0,1 — DeviceGray sample 0 = black, 1 = white.
        let samples = vec![0b1000_0000, 0b0100_0000];
        let dict = image_dict(2, 2, 1, Some("DeviceGray"));
        let img = decode_image_xobject(&samples, &dict).unwrap();
        assert_eq!(pixel(&img, 0), &[255, 255, 255, 255]); // bit 1 → white
        assert_eq!(pixel(&img, 1), &[0, 0, 0, 255]); // bit 0 → black
        assert_eq!(pixel(&img, 2), &[0, 0, 0, 255]);
        assert_eq!(pixel(&img, 3), &[255, 255, 255, 255]);
    }

    #[test]
    fn gray2_scales_to_255() {
        // 4x1 image, samples 0,1,2,3 packed into one byte.
        let samples = vec![0b00_01_10_11];
        let dict = image_dict(4, 1, 2, Some("DeviceGray"));
        let img = decode_image_xobject(&samples, &dict).unwrap();
        assert_eq!(pixel(&img, 0)[0], 0);
        assert_eq!(pixel(&img, 1)[0], 85);
        assert_eq!(pixel(&img, 2)[0], 170);
        assert_eq!(pixel(&img, 3)[0], 255);
    }

    #[test]
    fn gray4_scales_to_255() {
        // 2x1 image, samples 0x3 and 0xF.
        let samples = vec![0x3F];
        let dict = image_dict(2, 1, 4, Some("DeviceGray"));
        let img = decode_image_xobject(&samples, &dict).unwrap();
        assert_eq!(pixel(&img, 0)[0], 51); // 3 * 17
        assert_eq!(pixel(&img, 1)[0], 255); // 15 * 17
    }

    #[test]
    fn gray16_takes_high_byte() {
        let samples = vec![0x12, 0x34, 0xFF, 0xFF];
        let dict = image_dict(2, 1, 16, Some("DeviceGray"));
        let img = decode_image_xobject(&samples, &dict).unwrap();
        assert_eq!(pixel(&img, 0)[0], 0x12);
        assert_eq!(pixel(&img, 1)[0], 0xFF);
    }

    #[test]
    fn sub_byte_rows_are_byte_aligned() {
        // 3x2 at 4 bpc gray: each row is ceil(3*4/8) = 2 bytes.
        let samples = vec![0x0F, 0x00, 0xF0, 0xF0];
        let dict = image_dict(3, 2, 4, Some("DeviceGray"));
        let img = decode_image_xobject(&samples, &dict).unwrap();
        assert_eq!(pixel(&img, 0)[0], 0); // row 0: 0x0, 0xF, 0x0
        assert_eq!(pixel(&img, 1)[0], 255);
        assert_eq!(pixel(&img, 2)[0], 0);
        assert_eq!(pixel(&img, 3)[0], 255); // row 1: 0xF, 0x0, 0xF
        assert_eq!(pixel(&img, 4)[0], 0);
        assert_eq!(pixel(&img, 5)[0], 255);
    }

    // ---- item 2: /Decode arrays ----

    #[test]
    fn decode_array_inverts_gray() {
        let samples = vec![0, 255];
        let mut dict = image_dict(2, 1, 8, Some("DeviceGray"));
        dict.insert(PdfName::new("Decode"), int_array(&[1, 0]));
        let img = decode_image_xobject(&samples, &dict).unwrap();
        assert_eq!(pixel(&img, 0)[0], 255);
        assert_eq!(pixel(&img, 1)[0], 0);
    }

    #[test]
    fn decode_array_inverts_rgb() {
        let samples = vec![255, 0, 0];
        let mut dict = image_dict(1, 1, 8, Some("DeviceRGB"));
        dict.insert(PdfName::new("Decode"), int_array(&[1, 0, 1, 0, 1, 0]));
        let img = decode_image_xobject(&samples, &dict).unwrap();
        assert_eq!(pixel(&img, 0), &[0, 255, 255, 255]);
    }

    #[test]
    fn decode_array_inverts_cmyk() {
        // Raw 255,255,255,0 with [1 0 ×4] decodes to C=M=Y=0, K=1 → near-black
        // (the Adobe DeviceCMYK polynomial maps 100% K to a dark gray).
        let samples = vec![255, 255, 255, 0];
        let mut dict = image_dict(1, 1, 8, Some("DeviceCMYK"));
        dict.insert(PdfName::new("Decode"), int_array(&[1, 0, 1, 0, 1, 0, 1, 0]));
        let img = decode_image_xobject(&samples, &dict).unwrap();
        assert_eq!(pixel(&img, 0), &[44, 46, 53, 255]);
    }

    #[test]
    fn decode_array_partial_range_gray() {
        // /Decode [0.5 1]: raw 0 → 0.5 → 128, raw 255 → 1.0 → 255.
        let samples = vec![0, 255];
        let mut dict = image_dict(2, 1, 8, Some("DeviceGray"));
        dict.insert(
            PdfName::new("Decode"),
            PdfObject::Array(vec![PdfObject::Real(0.5), PdfObject::Integer(1)]),
        );
        let img = decode_image_xobject(&samples, &dict).unwrap();
        assert_eq!(pixel(&img, 0)[0], 128);
        assert_eq!(pixel(&img, 1)[0], 255);
    }

    #[test]
    fn decode_array_wrong_length_ignored() {
        let samples = vec![0];
        let mut dict = image_dict(1, 1, 8, Some("DeviceGray"));
        dict.insert(PdfName::new("Decode"), int_array(&[1, 0, 1, 0]));
        let img = decode_image_xobject(&samples, &dict).unwrap();
        assert_eq!(pixel(&img, 0)[0], 0); // not inverted
    }

    #[test]
    fn decode_array_remaps_indexed() {
        // 1-bpc indexed with /Decode [1 0]: raw 0 → index 1, raw 1 → index 0.
        let cs = ResolvedColorSpace::Indexed {
            base: Box::new(ResolvedColorSpace::Rgb),
            hival: 1,
            lookup: vec![255, 0, 0, 0, 0, 255],
        };
        let mut dict = image_dict(2, 1, 1, None);
        dict.insert(PdfName::new("Decode"), int_array(&[1, 0]));
        let samples = vec![0b0100_0000]; // raw 0, 1
        let img = decode_image_xobject_resolved(&samples, &dict, [0, 0, 0], Some(cs)).unwrap();
        assert_eq!(pixel(&img, 0), &[0, 0, 255, 255]); // raw 0 → idx 1 → blue
        assert_eq!(pixel(&img, 1), &[255, 0, 0, 255]); // raw 1 → idx 0 → red
    }

    // ---- item 5: resolved colour spaces / Indexed palettes ----

    #[test]
    fn indexed_palette_lookup_rgb_base() {
        let cs = ResolvedColorSpace::Indexed {
            base: Box::new(ResolvedColorSpace::Rgb),
            hival: 2,
            lookup: vec![255, 0, 0, 0, 255, 0, 0, 0, 255],
        };
        // 4x1 at 2 bpc: indices 0,1,2,3 (3 clamps to hival 2).
        let samples = vec![0b00_01_10_11];
        let dict = image_dict(4, 1, 2, None);
        let img = decode_image_xobject_resolved(&samples, &dict, [0, 0, 0], Some(cs)).unwrap();
        assert_eq!(pixel(&img, 0), &[255, 0, 0, 255]);
        assert_eq!(pixel(&img, 1), &[0, 255, 0, 255]);
        assert_eq!(pixel(&img, 2), &[0, 0, 255, 255]);
        assert_eq!(pixel(&img, 3), &[0, 0, 255, 255]); // clamped
    }

    #[test]
    fn indexed_palette_cmyk_base() {
        let cs = ResolvedColorSpace::Indexed {
            base: Box::new(ResolvedColorSpace::Cmyk),
            hival: 0,
            lookup: vec![0, 0, 0, 255], // K=1 → near-black via the CMYK polynomial
        };
        let samples = vec![0u8];
        let dict = image_dict(1, 1, 8, None);
        let img = decode_image_xobject_resolved(&samples, &dict, [0, 0, 0], Some(cs)).unwrap();
        assert_eq!(pixel(&img, 0), &[44, 46, 53, 255]);
    }

    #[test]
    fn inline_indexed_colorspace_is_inferred() {
        // Fully inline [/Indexed /DeviceRGB 1 (…)] with no caller resolution.
        let mut dict = image_dict(2, 1, 1, None);
        dict.insert(
            PdfName::new("ColorSpace"),
            PdfObject::Array(vec![
                PdfObject::Name(PdfName::new("Indexed")),
                PdfObject::Name(PdfName::new("DeviceRGB")),
                PdfObject::Integer(1),
                PdfObject::String(zpdf_core::PdfString::new(vec![255, 0, 0, 0, 0, 255])),
            ]),
        );
        let samples = vec![0b0100_0000];
        let img = decode_image_xobject(&samples, &dict).unwrap();
        assert_eq!(pixel(&img, 0), &[255, 0, 0, 255]);
        assert_eq!(pixel(&img, 1), &[0, 0, 255, 255]);
    }

    #[test]
    fn resolved_cmyk_overrides_inferred_rgb() {
        // An ICCBased /N 4 image resolved by the caller decodes as CMYK even
        // though the dict alone would have guessed RGB.
        let samples = vec![0, 0, 0, 255];
        let dict = image_dict(1, 1, 8, None);
        let img = decode_image_xobject_resolved(
            &samples,
            &dict,
            [0, 0, 0],
            Some(ResolvedColorSpace::Cmyk),
        )
        .unwrap();
        // K=1 → dark near-black via the Adobe DeviceCMYK polynomial.
        assert_eq!(pixel(&img, 0), &[44, 46, 53, 255]);
    }

    // ---- item 3: /SMask ----

    #[test]
    fn smask_premultiplies_rgb() {
        let dict = image_dict(2, 1, 8, Some("DeviceRGB"));
        let mut img = decode_image_xobject(&[255, 0, 0, 0, 255, 0], &dict).unwrap();
        let mask_dict = image_dict(2, 1, 8, Some("DeviceGray"));
        let mask = decode_image_xobject(&[128, 0], &mask_dict).unwrap();
        apply_smask_image(&mut img, &mask);
        assert!(img.has_alpha);
        assert!(img.premultiplied);
        assert_eq!(pixel(&img, 0), &[128, 0, 0, 128]); // premultiplied red
        assert_eq!(pixel(&img, 1), &[0, 0, 0, 0]); // fully transparent, no bleed
    }

    #[test]
    fn smask_honors_decode_and_bpc() {
        // 1-bpc mask with /Decode [1 0]: raw 1 → gray 0 (transparent).
        let dict = image_dict(2, 1, 8, Some("DeviceRGB"));
        let mut img = decode_image_xobject(&[255, 255, 255, 255, 255, 255], &dict).unwrap();
        let mut mask_dict = image_dict(2, 1, 1, Some("DeviceGray"));
        mask_dict.insert(PdfName::new("Decode"), int_array(&[1, 0]));
        let mask = decode_image_xobject(&[0b1000_0000], &mask_dict).unwrap();
        apply_smask_image(&mut img, &mask);
        assert_eq!(pixel(&img, 0)[3], 0);
        assert_eq!(pixel(&img, 1)[3], 255);
    }

    #[test]
    fn smask_resampled_to_image_size() {
        // 1x1 mask stretched over a 2x2 image: constant alpha everywhere.
        let dict = image_dict(2, 2, 8, Some("DeviceGray"));
        let mut img = decode_image_xobject(&[255, 255, 255, 255], &dict).unwrap();
        let mask_dict = image_dict(1, 1, 8, Some("DeviceGray"));
        let mask = decode_image_xobject(&[100], &mask_dict).unwrap();
        apply_smask_image(&mut img, &mask);
        for i in 0..4 {
            assert_eq!(pixel(&img, i)[3], 100);
        }
    }

    #[test]
    fn smask_bilinear_interpolates() {
        // 2x1 mask [0, 255] over a 4x1 image: edge pixels keep the endpoint
        // values and interior pixels land strictly between them.
        let dict = image_dict(4, 1, 8, Some("DeviceGray"));
        let mut img = decode_image_xobject(&[255; 4], &dict).unwrap();
        let mask_dict = image_dict(2, 1, 8, Some("DeviceGray"));
        let mask = decode_image_xobject(&[0, 255], &mask_dict).unwrap();
        apply_smask_image(&mut img, &mask);
        let a: Vec<u8> = (0..4).map(|i| pixel(&img, i)[3]).collect();
        assert_eq!(a[0], 0);
        assert_eq!(a[3], 255);
        assert!(a[0] < a[1] && a[1] < a[2] && a[2] < a[3], "alpha {a:?}");
    }

    // ---- item 4: /Mask ----

    #[test]
    fn stencil_mask_one_masks_out() {
        let dict = image_dict(2, 1, 8, Some("DeviceRGB"));
        let mut img = decode_image_xobject(&[255, 0, 0, 0, 255, 0], &dict).unwrap();
        apply_stencil_mask(&mut img, &[0b1000_0000], 2, 1, false);
        assert_eq!(pixel(&img, 0), &[0, 0, 0, 0]); // sample 1 → masked out
        assert_eq!(pixel(&img, 1), &[0, 255, 0, 255]); // sample 0 → painted
    }

    #[test]
    fn stencil_mask_inverted_polarity() {
        let dict = image_dict(2, 1, 8, Some("DeviceRGB"));
        let mut img = decode_image_xobject(&[255, 0, 0, 0, 255, 0], &dict).unwrap();
        apply_stencil_mask(&mut img, &[0b1000_0000], 2, 1, true);
        assert_eq!(pixel(&img, 0), &[255, 0, 0, 255]);
        assert_eq!(pixel(&img, 1), &[0, 0, 0, 0]);
    }

    #[test]
    fn color_key_mask_raw_samples_before_decode() {
        // RGB pixels: pure green is keyed out, red survives. /Decode [1 0 …]
        // proves ranges are compared on RAW samples (post-decode green would
        // be magenta and no longer match the key).
        let samples = vec![255, 0, 0, 0, 255, 0];
        let mut dict = image_dict(2, 1, 8, Some("DeviceRGB"));
        dict.insert(PdfName::new("Mask"), int_array(&[0, 0, 255, 255, 0, 0]));
        dict.insert(PdfName::new("Decode"), int_array(&[1, 0, 1, 0, 1, 0]));
        let img = decode_image_xobject(&samples, &dict).unwrap();
        assert!(img.has_alpha);
        assert!(img.premultiplied);
        assert_eq!(pixel(&img, 0), &[0, 255, 255, 255]); // red, inverted by /Decode
        assert_eq!(pixel(&img, 1), &[0, 0, 0, 0]); // keyed out
    }

    #[test]
    fn color_key_mask_indexed() {
        let cs = ResolvedColorSpace::Indexed {
            base: Box::new(ResolvedColorSpace::Rgb),
            hival: 1,
            lookup: vec![255, 0, 0, 0, 0, 255],
        };
        let mut dict = image_dict(2, 1, 1, None);
        dict.insert(PdfName::new("Mask"), int_array(&[1, 1])); // key out index 1
        let samples = vec![0b0100_0000];
        let img = decode_image_xobject_resolved(&samples, &dict, [0, 0, 0], Some(cs)).unwrap();
        assert_eq!(pixel(&img, 0), &[255, 0, 0, 255]);
        assert_eq!(pixel(&img, 1), &[0, 0, 0, 0]);
    }

    // ---- item 6: CMYK JPEG (Adobe APP14, inverted storage) ----

    #[test]
    fn cmyk_jpeg_decodes_to_cyan() {
        // 8x8 pure-cyan CMYK JPEG written by Pillow (Adobe APP14, transform 0,
        // inverted samples). The parser's filter pipeline runs zune-jpeg with
        // default options; replicate that here and feed the output through the
        // DCT image path.
        let jpeg = include_bytes!("testdata/cmyk_cyan_8x8.jpg");
        let mut decoder = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(&jpeg[..]));
        let decoded = decoder.decode().expect("zune-jpeg decode");

        let mut dict = image_dict(8, 8, 8, Some("DeviceCMYK"));
        dict.insert(
            PdfName::new("Filter"),
            PdfObject::Name(PdfName::new("DCTDecode")),
        );
        let img = decode_image_xobject_resolved(
            &decoded,
            &dict,
            [0, 0, 0],
            Some(ResolvedColorSpace::Cmyk),
        )
        .unwrap();
        // Cyan ≈ (0, 255, 255), with JPEG-loss tolerance.
        let px = pixel(&img, 0);
        assert!(px[0] < 60, "R too high: {px:?}");
        assert!(px[1] > 180, "G too low: {px:?}");
        assert!(px[2] > 180, "B too low: {px:?}");
        assert_eq!(px[3], 255);
    }

    // ---- ICCBased colour spaces (buffer-level CMS transforms) ----

    fn icc_cs(profile: &[u8]) -> ResolvedColorSpace {
        let t = zpdf_color::IccTransform::from_profile_bytes(
            profile,
            zpdf_color::RenderIntent::default(),
        )
        .unwrap();
        ResolvedColorSpace::Icc {
            ncomp: t.components() as u8,
            transform: std::sync::Arc::new(t),
        }
    }

    #[test]
    fn icc_gray_image_applies_tone_curve() {
        // Linear gray re-encoded into sRGB: 128 → ≈188 (the old /N fallback
        // would have left 128 untouched).
        let cs = icc_cs(include_bytes!("testdata/gray_linear.icc"));
        let dict = image_dict(3, 1, 8, None);
        let img =
            decode_image_xobject_resolved(&[0, 128, 255], &dict, [0, 0, 0], Some(cs)).unwrap();
        assert_eq!(pixel(&img, 0), &[0, 0, 0, 255]);
        assert!(
            (pixel(&img, 1)[0] as i16 - 188).abs() <= 2,
            "{:?}",
            pixel(&img, 1)
        );
        assert_eq!(pixel(&img, 2), &[255, 255, 255, 255]);
    }

    #[test]
    fn icc_cmyk_image_converts_through_lut() {
        let cs = icc_cs(include_bytes!("testdata/cmyk_lut.icc"));
        // white, K-black, cyan
        let samples = vec![0u8, 0, 0, 0, 0, 0, 0, 255, 255, 0, 0, 0];
        let dict = image_dict(3, 1, 8, None);
        let img = decode_image_xobject_resolved(&samples, &dict, [0, 0, 0], Some(cs)).unwrap();
        let w = pixel(&img, 0);
        let k = pixel(&img, 1);
        let c = pixel(&img, 2);
        assert!(w[0] > 220 && w[1] > 220 && w[2] > 220, "white {w:?}");
        assert!(k[0] < 30 && k[1] < 30 && k[2] < 30, "black {k:?}");
        assert!(c[0] < 60 && c[1] > 180 && c[2] > 180, "cyan {c:?}");
    }

    #[test]
    fn icc_image_honors_decode_and_color_key() {
        // /Decode [1 0] inverts the gray samples before the CMS transform;
        // the colour key still matches RAW values.
        let cs = icc_cs(include_bytes!("testdata/gray_linear.icc"));
        let mut dict = image_dict(2, 1, 8, None);
        dict.insert(PdfName::new("Decode"), int_array(&[1, 0]));
        dict.insert(PdfName::new("Mask"), int_array(&[0, 0])); // key out raw 0
        let img = decode_image_xobject_resolved(&[0, 255], &dict, [0, 0, 0], Some(cs)).unwrap();
        assert!(img.has_alpha);
        assert_eq!(pixel(&img, 0), &[0, 0, 0, 0]); // raw 0 keyed out
        assert_eq!(pixel(&img, 1), &[0, 0, 0, 255]); // raw 255 → decoded 0 → black
    }

    #[test]
    fn icc_cmyk_jpeg_applies_transform() {
        // Same CMYK JPEG as cmyk_jpeg_decodes_to_cyan, but resolved through
        // the LUT profile: the DCT sniffer must keep the ICC space.
        let jpeg = include_bytes!("testdata/cmyk_cyan_8x8.jpg");
        let mut decoder = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(&jpeg[..]));
        let decoded = decoder.decode().expect("zune-jpeg decode");

        let mut dict = image_dict(8, 8, 8, Some("DeviceCMYK"));
        dict.insert(
            PdfName::new("Filter"),
            PdfObject::Name(PdfName::new("DCTDecode")),
        );
        let cs = icc_cs(include_bytes!("testdata/cmyk_lut.icc"));
        let img = decode_image_xobject_resolved(&decoded, &dict, [0, 0, 0], Some(cs)).unwrap();
        let px = pixel(&img, 0);
        assert!(px[0] < 60, "R too high: {px:?}");
        assert!(px[1] > 180, "G too low: {px:?}");
        assert!(px[2] > 180, "B too low: {px:?}");
    }

    // ---- ImageMask stencil (pre-existing behaviour) ----

    #[test]
    fn image_mask_paints_fill() {
        let mut dict = image_dict(2, 1, 1, None);
        dict.insert(PdfName::new("ImageMask"), PdfObject::Bool(true));
        let img = decode_image_xobject_with_fill(&[0b0100_0000], &dict, [10, 20, 30]).unwrap();
        assert_eq!(pixel(&img, 0), &[10, 20, 30, 255]); // sample 0 paints
        assert_eq!(pixel(&img, 1), &[0, 0, 0, 0]);
        assert!(img.premultiplied);
    }
}
